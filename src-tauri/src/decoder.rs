//! Decodificador propio sobre symphonia, conectado a rodio como `Source`.
//!
//! El `Decoder` de rodio 0.20 no informa el tamaño del archivo a symphonia,
//! y sin eso el lector FLAC no puede hacer seek (`Unseekable`); además entra en
//! pánico con algunos M4A. Abriendo el archivo directamente con symphonia
//! tenemos seek en todos los formatos y errores en lugar de pánicos.

use rodio::source::SeekError;
use rodio::Source;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;
use symphonia::core::audio::{SampleBuffer, SignalSpec};
use symphonia::core::codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};

pub struct AudioSource {
    path: PathBuf,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    spec: SignalSpec,
    buf: Vec<f32>,
    pos: usize,
    /// Segundo en el que empieza el bloque decodificado actual.
    buf_time: f64,
    duration: Option<Duration>,
}

type Opened = (Box<dyn FormatReader>, Box<dyn Decoder>, u32, CodecParameters);

fn open_format(path: &Path) -> Result<Opened, String> {
    let file = File::open(path).map_err(|e| format!("No se pudo abrir: {e}"))?;
    // `File` le da a symphonia el tamaño del archivo: necesario para el seek en FLAC.
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let opts = FormatOptions { enable_gapless: true, ..Default::default() };
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &opts, &MetadataOptions::default())
        .map_err(|e| format!("Formato no soportado: {e}"))?;
    let format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("El archivo no tiene pista de audio")?;
    let track_id = track.id;
    let params = track.codec_params.clone();
    let decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| format!("Codec no soportado: {e}"))?;
    Ok((format, decoder, track_id, params))
}

fn measure_duration(path: &Path, tb: TimeBase) -> Option<Duration> {
    let (mut format, _, track_id, _) = open_format(path).ok()?;
    let mut end = 0u64;
    loop {
        match format.next_packet() {
            Ok(p) if p.track_id() == track_id => end = end.max(p.ts() + p.dur()),
            Ok(_) | Err(SymError::ResetRequired) => {}
            Err(_) => break,
        }
    }
    let t = tb.calc_time(end);
    let secs = t.seconds as f64 + t.frac;
    (secs > 0.0).then(|| Duration::from_secs_f64(secs))
}

impl AudioSource {
    pub fn open(path: &Path) -> Result<AudioSource, String> {
        let (format, decoder, track_id, params) = open_format(path)?;
        let time_base = params
            .time_base
            .or_else(|| params.sample_rate.map(|r| TimeBase::new(1, r)));
        let duration = match (params.n_frames, time_base) {
            (Some(n), Some(tb)) if n > 0 => {
                let t = tb.calc_time(n);
                Some(Duration::from_secs_f64(t.seconds as f64 + t.frac))
            }
            // Ej. MP4 fragmentado (descargas de Tidal): sin duración en el
            // encabezado. Se mide recorriendo los paquetes sin decodificarlos.
            _ => time_base.and_then(|tb| measure_duration(path, tb)),
        };
        let mut src = AudioSource {
            path: path.to_path_buf(),
            format,
            decoder,
            track_id,
            time_base,
            spec: SignalSpec::new(params.sample_rate.unwrap_or(44_100), Default::default()),
            buf: Vec::new(),
            pos: 0,
            buf_time: 0.0,
            duration,
        };
        // Decodificar el primer bloque para conocer canales y frecuencia reales.
        if !src.refill() {
            return Err("No se pudo decodificar el audio".into());
        }
        Ok(src)
    }

    fn ts_secs(&self, ts: u64) -> f64 {
        match self.time_base {
            Some(tb) => {
                let t = tb.calc_time(ts);
                t.seconds as f64 + t.frac
            }
            None => ts as f64 / self.spec.rate.max(1) as f64,
        }
    }

    /// Respaldo cuando el formato no soporta seek: reabre si hay que ir hacia
    /// atrás y luego descarta paquetes (sin decodificarlos) hasta llegar.
    fn skip_to(&mut self, target: f64) -> bool {
        if target < self.buf_time {
            let Ok((format, decoder, track_id, _)) = open_format(&self.path) else { return false };
            self.format = format;
            self.decoder = decoder;
            self.track_id = track_id;
        }
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::ResetRequired) => continue,
                Err(_) => return false,
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let end = self.ts_secs(packet.ts() + packet.dur());
            if end >= target {
                self.decoder.reset();
                return true;
            }
        }
    }

    pub fn duration_secs(&self) -> Option<f64> {
        self.duration.map(|d| d.as_secs_f64())
    }

    /// Decodifica el siguiente paquete al búfer. `false` = fin del archivo.
    fn refill(&mut self) -> bool {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(_) => return false,
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let packet_time = self.ts_secs(packet.ts());
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    if decoded.frames() == 0 {
                        continue;
                    }
                    self.spec = *decoded.spec();
                    let mut sb = SampleBuffer::<f32>::new(decoded.capacity() as u64, self.spec);
                    sb.copy_interleaved_ref(decoded);
                    self.buf.clear();
                    self.buf.extend_from_slice(sb.samples());
                    self.pos = 0;
                    self.buf_time = packet_time;
                    return true;
                }
                // Un paquete dañado no debe parar el track: se salta.
                Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => continue,
                Err(_) => return false,
            }
        }
    }
}

impl Iterator for AudioSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        // pos == len solo ocurre cuando ya no hay más audio.
        let s = *self.buf.get(self.pos)?;
        self.pos += 1;
        if self.pos >= self.buf.len() {
            // Rellenar en cuanto se acaba el bloque, así `current_frame_len`
            // solo vale 0 cuando el track realmente terminó.
            self.refill();
        }
        Some(s)
    }
}

impl Source for AudioSource {
    fn current_frame_len(&self) -> Option<usize> {
        // Lo que queda del bloque actual (después puede cambiar el formato);
        // 0 significa fin del track.
        Some(self.buf.len().saturating_sub(self.pos))
    }

    fn channels(&self) -> u16 {
        self.spec.channels.count().max(1) as u16
    }

    fn sample_rate(&self) -> u32 {
        self.spec.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.duration
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        let target = pos.as_secs_f64();
        let to = SeekTo::Time { time: Time::from(target), track_id: Some(self.track_id) };
        match self.format.seek(SeekMode::Coarse, to) {
            Ok(_) => self.decoder.reset(),
            Err(e) => {
                if !self.skip_to(target) {
                    return Err(SeekError::NotSupported { underlying_source: leak_name(e) });
                }
            }
        }
        self.buf.clear();
        self.pos = 0;
        // Rellenar ya, para que canales y frecuencia sigan siendo válidos.
        if !self.refill() {
            self.buf.clear();
        }
        Ok(())
    }
}

/// `SeekError::NotSupported` de rodio pide un `&'static str`.
fn leak_name(e: SymError) -> &'static str {
    match e {
        SymError::SeekError(_) => "seek no soportado por este archivo",
        SymError::IoError(_) => "error de lectura durante el seek",
        _ => "error durante el seek",
    }
}

/// Forma de onda resumida en `buckets` columnas: RMS y pico de cada una,
/// normalizados a 0–255. Decodifica el archivo completo, así que se llama en
/// un hilo aparte; `cancelled` permite abandonar si ya se cambió de track.
pub struct Waveform {
    pub rms: Vec<u8>,
    pub peak: Vec<u8>,
}

pub fn compute_waveform(path: &Path, buckets: usize, cancelled: impl Fn() -> bool) -> Option<Waveform> {
    let (mut format, mut decoder, track_id, _) = open_format(path).ok()?;
    // Por paquete: (cuadros, suma de cuadrados, pico). Luego se reparte en columnas.
    let mut packets: Vec<(u64, f64, f32)> = Vec::new();
    let mut sb: Option<SampleBuffer<f32>> = None;
    let mut count = 0usize;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(_) => break,
        };
        if packet.track_id() != track_id {
            continue;
        }
        count += 1;
        if count % 64 == 0 && cancelled() {
            return None;
        }
        let Ok(decoded) = decoder.decode(&packet) else { continue };
        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }
        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        let buf = match &mut sb {
            Some(b) if b.capacity() >= decoded.capacity() * channels => b,
            _ => sb.insert(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec)),
        };
        buf.copy_interleaved_ref(decoded);
        let mut sum = 0f64;
        let mut peak = 0f32;
        for frame in buf.samples().chunks_exact(channels) {
            // Mezcla a mono para que la onda refleje lo que se oye.
            let v = frame.iter().sum::<f32>() / channels as f32;
            sum += (v * v) as f64;
            peak = peak.max(v.abs());
        }
        packets.push((frames as u64, sum, peak));
    }
    let total: u64 = packets.iter().map(|p| p.0).sum();
    if total == 0 || buckets == 0 {
        return None;
    }
    let mut sums = vec![0f64; buckets];
    let mut counts = vec![0u64; buckets];
    let mut peaks = vec![0f32; buckets];
    let mut start = 0u64;
    for (frames, sum, peak) in packets {
        let b = ((start as u128 * buckets as u128) / total as u128) as usize;
        let b = b.min(buckets - 1);
        sums[b] += sum;
        counts[b] += frames;
        peaks[b] = peaks[b].max(peak);
        start += frames;
    }
    // Columnas sin paquete propio (tracks muy cortos): copiar la anterior.
    for i in 1..buckets {
        if counts[i] == 0 {
            sums[i] = sums[i - 1];
            counts[i] = counts[i - 1];
            peaks[i] = peaks[i - 1];
        }
    }
    let rms: Vec<f32> = (0..buckets)
        .map(|i| if counts[i] > 0 { (sums[i] / counts[i] as f64).sqrt() as f32 } else { 0.0 })
        .collect();
    let max_rms = rms.iter().cloned().fold(0f32, f32::max).max(1e-6);
    let max_peak = peaks.iter().cloned().fold(0f32, f32::max).max(1e-6);
    Some(Waveform {
        rms: rms.iter().map(|v| ((v / max_rms) * 255.0).round() as u8).collect(),
        peak: peaks.iter().map(|v| ((v / max_peak) * 255.0).round() as u8).collect(),
    })
}
