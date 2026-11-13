//! Motor de audio: decodifica con symphonia (vía rodio) para que FLAC, MP3,
//! WAV y AAC suenen igual en Linux, Windows y macOS sin depender de codecs
//! del sistema. Incluye el "escaneo" automático: brinca a puntos repartidos
//! por todo el track, como cuando uno da clics en la línea de tiempo.

use crate::config::ScanConfig;
use rand::Rng;
use crate::decoder::AudioSource;
use rodio::{OutputStream, OutputStreamHandle, Sink};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Scan {
    active: bool,
    points: Vec<f64>,
    index: usize,
    segment: f64,
    repeat: bool,
    seg_started: Option<Instant>,
    n_points: u32,
    duration: f64,
}

impl Scan {
    fn new(duration: Option<f64>, cfg: &ScanConfig, active: bool) -> Scan {
        let mut s = Scan {
            active: false,
            segment: cfg.segment_secs,
            repeat: cfg.repeat,
            n_points: cfg.points,
            duration: duration.unwrap_or(0.0),
            ..Default::default()
        };
        s.generate();
        s.active = active && !s.points.is_empty();
        s
    }

    /// Divide el track en N secciones y elige un punto al azar dentro de cada
    /// una, en orden: aleatorio pero siempre cubre intro, desarrollo y final.
    fn generate(&mut self) {
        self.points.clear();
        self.index = 0;
        self.seg_started = None;
        let usable = self.duration - self.segment - 0.5;
        if usable <= self.segment {
            return;
        }
        let max_points = (usable / self.segment).floor().max(1.0) as u32;
        let n = self.n_points.clamp(1, max_points);
        let section = usable / n as f64;
        let mut rng = rand::thread_rng();
        for i in 0..n {
            let start = i as f64 * section;
            let jitter = (section - self.segment).max(0.0);
            let offset = if jitter > 0.0 { rng.gen_range(0.0..jitter) } else { 0.0 };
            self.points.push(start + offset);
        }
    }
}

#[derive(Default)]
struct Inner {
    sink: Option<Arc<Sink>>,
    path: Option<PathBuf>,
    duration: Option<f64>,
    volume: f32,
    scan: Scan,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlayerStatus {
    pub loaded: bool,
    pub playing: bool,
    pub ended: bool,
    pub position: f64,
    pub duration: Option<f64>,
    pub scan_active: bool,
    pub scan_points: Vec<f64>,
    pub scan_index: usize,
    pub scan_segment: f64,
}

pub struct Player {
    handle: Option<OutputStreamHandle>,
    init_error: Option<String>,
    inner: Mutex<Inner>,
}

impl Player {
    pub fn new(volume: f32) -> Arc<Player> {
        let (handle, init_error) = match open_output() {
            Ok(h) => (Some(h), None),
            Err(e) => (None, Some(e)),
        };
        let player = Arc::new(Player {
            handle,
            init_error,
            inner: Mutex::new(Inner { volume, ..Default::default() }),
        });
        // Reloj del escaneo automático.
        let weak = Arc::downgrade(&player);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(20));
            match weak.upgrade() {
                Some(p) => p.tick(),
                None => break,
            }
        });
        player
    }

    fn new_sink(&self, path: &Path) -> Result<(Arc<Sink>, Option<f64>), String> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| self.init_error.clone().unwrap_or_else(|| "Sin salida de audio".into()))?;
        let decoder = AudioSource::open(path)?;
        let duration = decoder.duration_secs();
        let sink = Sink::try_new(handle).map_err(|e| e.to_string())?;
        sink.pause();
        sink.append(decoder);
        Ok((Arc::new(sink), duration))
    }

    /// Carga un track. `known_duration` viene de los metadatos (más confiable
    /// que la del decodificador en MP3 VBR).
    pub fn load(
        &self,
        path: &Path,
        known_duration: Option<f64>,
        scan_cfg: &ScanConfig,
    ) -> Result<Option<f64>, String> {
        self.stop();
        let (sink, dec_duration) = self.new_sink(path)?;
        let duration = known_duration.or(dec_duration);
        let mut scan = Scan::new(duration, scan_cfg, scan_cfg.enabled);

        let volume = self.inner.lock().unwrap().volume;
        sink.set_volume(volume);
        // Si hay escaneo, brinca al primer punto antes de que suene nada.
        if scan.active {
            let first = scan.points[0];
            let _ = sink.try_seek(Duration::from_secs_f64(first));
            scan.index = 1;
            scan.seg_started = Some(Instant::now());
        }
        sink.play();

        let mut g = self.inner.lock().unwrap();
        g.sink = Some(sink);
        g.path = Some(path.to_path_buf());
        g.duration = duration;
        g.scan = scan;
        Ok(duration)
    }

    /// Detiene y suelta el archivo (necesario antes de moverlo, sobre todo en Windows).
    pub fn stop(&self) {
        let mut g = self.inner.lock().unwrap();
        if let Some(s) = g.sink.take() {
            s.stop();
        }
        g.path = None;
        g.duration = None;
        g.scan = Scan::default();
    }

    /// Si el track ya terminó, rodio soltó la fuente: hay que reabrirlo.
    fn ensure_sink(&self) -> Option<Arc<Sink>> {
        let (sink, path, volume) = {
            let g = self.inner.lock().unwrap();
            (g.sink.clone(), g.path.clone(), g.volume)
        };
        match sink {
            Some(s) if !s.empty() => Some(s),
            _ => {
                let path = path?;
                let (s, _) = self.new_sink(&path).ok()?;
                s.set_volume(volume);
                s.play();
                self.inner.lock().unwrap().sink = Some(s.clone());
                Some(s)
            }
        }
    }

    pub fn toggle_play(&self) -> bool {
        let Some(sink) = self.ensure_sink() else { return false };
        if sink.is_paused() {
            sink.play();
            // Al reanudar, el punto actual del escaneo vuelve a durar completo.
            let mut g = self.inner.lock().unwrap();
            if g.scan.active {
                g.scan.seg_started = Some(Instant::now());
            }
            true
        } else {
            sink.pause();
            false
        }
    }

    /// `manual = true` cuando lo hace el usuario: apaga el escaneo para este track.
    pub fn seek(&self, position: f64, manual: bool) {
        let duration = {
            let mut g = self.inner.lock().unwrap();
            if manual {
                g.scan.active = false;
            }
            g.duration
        };
        let Some(sink) = self.ensure_sink() else { return };
        let max = duration.map(|d| (d - 0.25).max(0.0)).unwrap_or(f64::MAX);
        let pos = position.clamp(0.0, max);
        let _ = sink.try_seek(Duration::from_secs_f64(pos));
    }

    pub fn set_volume(&self, volume: f32) {
        let mut g = self.inner.lock().unwrap();
        g.volume = volume.clamp(0.0, 1.0);
        if let Some(s) = &g.sink {
            s.set_volume(g.volume);
        }
    }

    /// Prende/apaga el escaneo solo para el track actual.
    pub fn set_scan(&self, active: bool, cfg: &ScanConfig) {
        let mut g = self.inner.lock().unwrap();
        if active {
            let duration = g.duration;
            let mut scan = Scan::new(duration, cfg, true);
            // Continúa desde el punto más cercano adelante de la posición actual.
            let pos = g.sink.as_ref().map(|s| s.get_pos().as_secs_f64()).unwrap_or(0.0);
            scan.index = scan.points.iter().position(|p| *p > pos).unwrap_or(scan.points.len());
            scan.seg_started = None;
            g.scan = scan;
        } else {
            g.scan.active = false;
        }
    }

    pub fn status(&self) -> PlayerStatus {
        let g = self.inner.lock().unwrap();
        let Some(sink) = &g.sink else {
            return PlayerStatus { loaded: g.path.is_some(), ..Default::default() };
        };
        let ended = sink.empty();
        PlayerStatus {
            loaded: true,
            playing: !sink.is_paused() && !ended,
            ended: ended && !g.scan.active,
            position: if ended { g.duration.unwrap_or(0.0) } else { sink.get_pos().as_secs_f64() },
            duration: g.duration,
            scan_active: g.scan.active,
            scan_points: g.scan.points.clone(),
            scan_index: g.scan.index,
            scan_segment: g.scan.segment,
        }
    }

    fn tick(&self) {
        let sink = {
            let mut g = self.inner.lock().unwrap();
            let Some(sink) = g.sink.clone() else { return };
            if !g.scan.active || sink.is_paused() {
                return;
            }
            let seg = Duration::from_secs_f64(g.scan.segment);
            if let Some(t) = g.scan.seg_started {
                if t.elapsed() < seg && !sink.empty() {
                    return;
                }
            }
            if g.scan.index >= g.scan.points.len() {
                if g.scan.repeat {
                    g.scan.generate();
                } else {
                    g.scan.active = false;
                    return;
                }
                if g.scan.points.is_empty() {
                    g.scan.active = false;
                    return;
                }
            }
            let target = g.scan.points[g.scan.index];
            g.scan.index += 1;
            g.scan.seg_started = Some(Instant::now());
            (sink, target)
        };
        let (sink, target) = sink;
        let sink = if sink.empty() { self.ensure_sink() } else { Some(sink) };
        if let Some(s) = sink {
            let _ = s.try_seek(Duration::from_secs_f64(target));
        }
    }
}

/// El `OutputStream` de rodio no es `Send`, así que vive en su propio hilo
/// para siempre y solo compartimos su handle.
fn open_output() -> Result<OutputStreamHandle, String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || match OutputStream::try_default() {
        Ok((stream, handle)) => {
            let _ = tx.send(Ok(handle));
            let _keep_alive = stream;
            loop {
                thread::park();
            }
        }
        Err(e) => {
            let _ = tx.send(Err(format!("No hay salida de audio: {e}")));
        }
    });
    rx.recv().map_err(|e| e.to_string())?
}
