//! Todo lo que toca el sistema de archivos: armar la cola de tracks, leer
//! metadatos y mover archivos (con sus .lrc) sin sobrescribir nunca nada.

use crate::config::Config;
use lofty::prelude::*;
use serde::Serialize;
use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub const AUDIO_EXTS: &[&str] = &["mp3", "flac", "wav", "aac", "m4a"];
/// Archivos que viajan junto con el track (mismo nombre, otra extensión).
const SIDECAR_EXTS: &[&str] = &["lrc", "LRC"];

pub fn is_audio(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

/// Orden natural ("2. x" antes que "10. x"), primero por carpeta y luego por
/// nombre, para que al vaciar una subcarpeta se pase sola a la siguiente.
fn natural_cmp(a: &Path, b: &Path) -> Ordering {
    let pa = a.parent().map(|p| p.to_string_lossy()).unwrap_or_default();
    let pb = b.parent().map(|p| p.to_string_lossy()).unwrap_or_default();
    let na = a.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    let nb = b.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    natord::compare_ignore_case(&pa, &pb).then_with(|| natord::compare_ignore_case(&na, &nb))
}

/// Todos los audios bajo `dir`, omitiendo carpetas ocultas y las de `exclude`.
fn collect(dir: &Path, recursive: bool, exclude: &[PathBuf]) -> Vec<PathBuf> {
    let walker = WalkDir::new(dir).follow_links(false).max_depth(if recursive { usize::MAX } else { 1 });
    let mut files: Vec<PathBuf> = walker
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || (!is_hidden(e.file_name()) && !exclude.iter().any(|x| e.path() == x.as_path()))
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && is_audio(e.path()))
        .map(|e| e.into_path())
        .collect();
    files.sort_by(|a, b| natural_cmp(a, b));
    files
}

/// Arma la cola para un archivo recién abierto.
/// - Dentro de una raíz: todos los audios de la raíz que aún no están en un destino.
/// - Dentro de un destino: los audios de ese destino (para reacomodar).
/// - Fuera de toda raíz: los audios de su misma carpeta.
pub fn build_queue(cfg: &Config, file: &Path) -> (Option<usize>, Vec<PathBuf>) {
    let root_idx = cfg.root_for(file);
    let mut queue = match root_idx {
        Some(i) => {
            let root = &cfg.roots[i];
            let dests: Vec<PathBuf> = root.destinations.iter().map(|d| PathBuf::from(&d.path)).collect();
            match dests.iter().find(|d| file.starts_with(d)) {
                Some(d) => collect(d, true, &[]),
                None => collect(Path::new(&root.path), true, &dests),
            }
        }
        None => file.parent().map(|p| collect(p, false, &[])).unwrap_or_default(),
    };
    if !queue.iter().any(|p| p == file) {
        queue.push(file.to_path_buf());
        queue.sort_by(|a, b| natural_cmp(a, b));
    }
    (root_idx, queue)
}

#[derive(Serialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TrackMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub format: String,
    pub bitrate: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub channels: Option<u8>,
    pub duration: Option<f64>,
}

pub fn read_meta(path: &Path) -> TrackMeta {
    let format = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_uppercase())
        .unwrap_or_default();
    let mut meta = TrackMeta { format, ..Default::default() };
    let Ok(tagged) = lofty::read_from_path(path) else { return meta };
    let props = tagged.properties();
    let d = props.duration().as_secs_f64();
    meta.duration = (d > 0.0).then_some(d);
    meta.bitrate = props.audio_bitrate().or(props.overall_bitrate());
    meta.sample_rate = props.sample_rate();
    meta.bit_depth = props.bit_depth();
    meta.channels = props.channels();
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        let clean = |v: Option<std::borrow::Cow<'_, str>>| {
            v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        };
        meta.title = clean(tag.title());
        meta.artist = clean(tag.artist());
        meta.album = clean(tag.album());
        meta.genre = clean(tag.genre());
    }
    meta
}

/// `dir/nombre.ext`, o `dir/nombre (2).ext`, `(3)`... si ya existe.
pub fn unique_target(dir: &Path, file_name: &str) -> PathBuf {
    let first = dir.join(file_name);
    if !first.exists() {
        return first;
    }
    let p = Path::new(file_name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|c| !c.exists())
        .unwrap()
}

/// Renombra; si origen y destino están en discos distintos, copia y borra.
fn move_path(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => {
            fs::copy(from, to)?;
            fs::remove_file(from)
        }
        Err(e) => Err(e),
    }
}

fn is_cross_device(e: &io::Error) -> bool {
    #[cfg(unix)]
    const CODE: i32 = 18; // EXDEV
    #[cfg(windows)]
    const CODE: i32 = 17; // ERROR_NOT_SAME_DEVICE
    #[cfg(not(any(unix, windows)))]
    const CODE: i32 = -1;
    e.raw_os_error() == Some(CODE)
}

#[derive(Clone, Debug)]
pub struct MoveRecord {
    /// Pares (origen, destino): el track primero y luego sus .lrc.
    pub files: Vec<(PathBuf, PathBuf)>,
    pub queue_index: usize,
}

/// Mueve `src` a `target` junto con sus archivos acompañantes.
pub fn move_track(src: &Path, target: &Path) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let dir = target.parent().ok_or("Destino inválido")?;
    fs::create_dir_all(dir).map_err(|e| format!("No se pudo crear {}: {e}", dir.display()))?;
    if target.exists() {
        return Err(format!("Ya existe {}", target.display()));
    }
    move_path(src, target).map_err(|e| format!("No se pudo mover: {e}"))?;
    let mut moved = vec![(src.to_path_buf(), target.to_path_buf())];

    let (Some(src_dir), Some(src_stem), Some(dst_stem)) =
        (src.parent(), src.file_stem(), target.file_stem())
    else {
        return Ok(moved);
    };
    for ext in SIDECAR_EXTS {
        let from = src_dir.join(format!("{}.{ext}", src_stem.to_string_lossy()));
        let to = dir.join(format!("{}.{ext}", dst_stem.to_string_lossy()));
        // Nunca sobrescribir: si ya hay uno con ese nombre, se queda donde está.
        if from.is_file() && !to.exists() && move_path(&from, &to).is_ok() {
            moved.push((from, to));
        }
    }
    Ok(moved)
}

/// Regresa un movimiento a su lugar original.
pub fn undo_move(rec: &MoveRecord) -> Result<(), String> {
    for (orig, now) in &rec.files {
        if orig.exists() {
            return Err(format!("Ya hay otro archivo en {}", orig.display()));
        }
        if let Some(dir) = orig.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        move_path(now, orig).map_err(|e| format!("No se pudo regresar: {e}"))?;
    }
    Ok(())
}

/// Borra `dir` y sus padres mientras estén vacíos, sin salirse de `root`
/// ni tocar la raíz o los destinos. `remove_dir` falla si hay cualquier cosa
/// adentro, así que nunca borra archivos.
pub fn prune_empty_dirs(dir: &Path, root: &Path, keep: &[PathBuf]) {
    let mut cur = dir.to_path_buf();
    while cur.starts_with(root) && cur != root && !keep.iter().any(|k| k == &cur) {
        if fs::remove_dir(&cur).is_err() {
            break;
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => break,
        }
    }
}
