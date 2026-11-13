mod audio;
mod config;
mod decoder;
mod library;

use audio::{Player, PlayerStatus};
use config::{folder_name, Config};
use library::{MoveRecord, TrackMeta};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};

const WAVEFORM_BUCKETS: usize = 1200;

/// Cola de trabajo: los tracks que faltan por clasificar.
#[derive(Default)]
struct Session {
    root: Option<usize>,
    queue: Vec<PathBuf>,
    index: usize,
    current: Option<PathBuf>,
    meta: TrackMeta,
    error: Option<String>,
}

struct AppState {
    app: AppHandle,
    player: Arc<Player>,
    /// Cambia con cada track cargado; el cálculo de onda viejo se cancela.
    wave_gen: Arc<AtomicU64>,
    config: Mutex<Config>,
    config_path: PathBuf,
    session: Mutex<Session>,
    undo: Mutex<Vec<MoveRecord>>,
    pending_open: Mutex<Option<String>>,
}

impl AppState {
    fn save_config(&self, cfg: &Config) -> Result<(), String> {
        cfg.save(&self.config_path)
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DestView {
    label: String,
    path: String,
    /// El track ya está en esta carpeta (el botón no aplica).
    is_current: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RootView {
    path: String,
    name: String,
    destinations: Vec<DestView>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct TrackView {
    path: String,
    file_name: String,
    /// Carpeta del track relativa a la raíz (p. ej. "House").
    folder: String,
    meta: TrackMeta,
    root: Option<RootView>,
    queue_index: usize,
    queue_len: usize,
    remaining_in_folder: usize,
    error: Option<String>,
    undo_available: bool,
}

#[derive(Serialize, Clone)]
struct WaveformEvent {
    path: String,
    rms: Vec<u8>,
    peak: Vec<u8>,
}

/// Calcula la forma de onda en segundo plano y la manda a la interfaz.
fn spawn_waveform(state: &AppState, path: PathBuf) {
    let gen = state.wave_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let counter = state.wave_gen.clone();
    let app = state.app.clone();
    std::thread::spawn(move || {
        let cancelled = || counter.load(Ordering::SeqCst) != gen;
        if let Some(w) = decoder::compute_waveform(&path, WAVEFORM_BUCKETS, cancelled) {
            if counter.load(Ordering::SeqCst) == gen {
                let _ = app.emit(
                    "waveform",
                    WaveformEvent { path: path.to_string_lossy().into_owned(), rms: w.rms, peak: w.peak },
                );
            }
        }
    });
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum MoveOutcome {
    Moved { to: String, label: String, next: Option<TrackView> },
    Conflict { existing: String },
}

fn view(state: &AppState, s: &Session) -> Option<TrackView> {
    let path = s.current.as_ref()?;
    let cfg = state.config.lock().unwrap();
    let root = s.root.and_then(|i| cfg.roots.get(i));
    let parent = path.parent().unwrap_or(Path::new(""));
    let folder = match root {
        Some(r) => match parent.strip_prefix(&r.path) {
            Ok(rel) if rel.as_os_str().is_empty() => folder_name(Path::new(&r.path)),
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => folder_name(parent),
        },
        None => parent.to_string_lossy().into_owned(),
    };
    Some(TrackView {
        path: path.to_string_lossy().into_owned(),
        file_name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        folder,
        meta: s.meta.clone(),
        root: root.map(|r| RootView {
            path: r.path.clone(),
            name: folder_name(Path::new(&r.path)),
            destinations: r
                .destinations
                .iter()
                .map(|d| DestView {
                    label: d.label.clone(),
                    path: d.path.clone(),
                    is_current: parent == Path::new(&d.path),
                })
                .collect(),
        }),
        queue_index: s.index,
        queue_len: s.queue.len(),
        remaining_in_folder: s.queue.iter().filter(|p| p.parent() == Some(parent)).count(),
        error: s.error.clone(),
        undo_available: !state.undo.lock().unwrap().is_empty(),
    })
}

/// Carga el track en `s.index`, saltando los que ya no existan.
fn play_index(state: &AppState, s: &mut Session) -> Option<TrackView> {
    loop {
        if s.queue.is_empty() {
            state.player.stop();
            s.current = None;
            return None;
        }
        s.index %= s.queue.len();
        let path = s.queue[s.index].clone();
        if !path.is_file() {
            let i = s.index;
            s.queue.remove(i);
            continue;
        }
        let meta = library::read_meta(&path);
        let scan_cfg = state.config.lock().unwrap().scan.clone();
        let mut meta = meta;
        match state.player.load(&path, meta.duration, &scan_cfg) {
            Ok(d) => {
                meta.duration = d;
                s.error = None;
            }
            Err(e) => s.error = Some(e),
        }
        s.meta = meta;
        if s.error.is_none() {
            spawn_waveform(state, path.clone());
        }
        s.current = Some(path);
        return view(state, s);
    }
}

#[tauri::command(async)]
fn startup_file(state: State<AppState>) -> Option<String> {
    state.pending_open.lock().unwrap().take()
}

#[tauri::command(async)]
fn open_file(path: String, state: State<AppState>) -> Result<Option<TrackView>, String> {
    let file = PathBuf::from(&path);
    if !file.is_file() {
        return Err(format!("No existe: {path}"));
    }
    let (root, queue) = {
        let cfg = state.config.lock().unwrap();
        library::build_queue(&cfg, &file)
    };
    let mut s = state.session.lock().unwrap();
    s.index = queue.iter().position(|p| p == &file).unwrap_or(0);
    s.queue = queue;
    s.root = root;
    Ok(play_index(&state, &mut s))
}

#[tauri::command(async)]
fn current_track(state: State<AppState>) -> Option<TrackView> {
    let s = state.session.lock().unwrap();
    view(&state, &s)
}

#[tauri::command(async)]
fn next_track(state: State<AppState>) -> Option<TrackView> {
    let mut s = state.session.lock().unwrap();
    if s.queue.is_empty() {
        return None;
    }
    s.index = (s.index + 1) % s.queue.len();
    play_index(&state, &mut s)
}

#[tauri::command(async)]
fn prev_track(state: State<AppState>) -> Option<TrackView> {
    let mut s = state.session.lock().unwrap();
    if s.queue.is_empty() {
        return None;
    }
    s.index = (s.index + s.queue.len() - 1) % s.queue.len();
    play_index(&state, &mut s)
}

#[tauri::command(async)]
fn move_current(dest: usize, rename: bool, state: State<AppState>) -> Result<MoveOutcome, String> {
    let mut s = state.session.lock().unwrap();
    let src = s.current.clone().ok_or("No hay track cargado")?;
    let (root_path, destination, keep, remove_empty) = {
        let cfg = state.config.lock().unwrap();
        let root = s.root.and_then(|i| cfg.roots.get(i)).ok_or("Esta carpeta no está configurada")?;
        let d = root.destinations.get(dest).ok_or("Ese botón no existe")?.clone();
        let mut keep: Vec<PathBuf> = root.destinations.iter().map(|d| PathBuf::from(&d.path)).collect();
        keep.push(PathBuf::from(&root.path));
        (PathBuf::from(&root.path), d, keep, cfg.remove_empty_folders)
    };
    let dest_dir = PathBuf::from(&destination.path);
    if src.parent() == Some(dest_dir.as_path()) {
        return Err(format!("Ya está en \"{}\"", destination.label));
    }
    let file_name = src.file_name().ok_or("Nombre inválido")?.to_string_lossy().into_owned();
    let direct = dest_dir.join(&file_name);
    // Revisar el conflicto ANTES de detener: si hay duplicado, que siga sonando.
    if direct.exists() && !rename {
        return Ok(MoveOutcome::Conflict { existing: direct.to_string_lossy().into_owned() });
    }
    let target = if rename { library::unique_target(&dest_dir, &file_name) } else { direct };

    // Detener y soltar el archivo antes de moverlo.
    state.player.stop();
    let files = match library::move_track(&src, &target) {
        Ok(f) => f,
        Err(e) => {
            // No se movió: que vuelva a sonar.
            play_index(&state, &mut s);
            return Err(e);
        }
    };
    let queue_index = s.index;
    state.undo.lock().unwrap().push(MoveRecord { files, queue_index });
    if remove_empty {
        if let Some(dir) = src.parent() {
            library::prune_empty_dirs(dir, &root_path, &keep);
        }
    }
    let idx = s.index;
    if idx < s.queue.len() {
        s.queue.remove(idx);
    }
    let next = play_index(&state, &mut s);
    Ok(MoveOutcome::Moved {
        to: target.to_string_lossy().into_owned(),
        label: destination.label,
        next,
    })
}

#[tauri::command(async)]
fn undo_move(state: State<AppState>) -> Result<Option<TrackView>, String> {
    let mut s = state.session.lock().unwrap();
    let rec = state.undo.lock().unwrap().pop().ok_or("No hay nada que deshacer")?;
    // Si el que suena es justo el que vamos a regresar, soltarlo primero.
    state.player.stop();
    if let Err(e) = library::undo_move(&rec) {
        state.undo.lock().unwrap().push(rec);
        play_index(&state, &mut s);
        return Err(e);
    }
    let original = rec.files[0].0.clone();
    s.queue.retain(|p| p != &rec.files[0].1);
    let idx = rec.queue_index.min(s.queue.len());
    s.queue.insert(idx, original);
    s.index = idx;
    Ok(play_index(&state, &mut s))
}

/// Portada como data URL (se pide aparte para no hacer pesada la carga del track).
#[tauri::command(async)]
fn cover_art(path: String) -> Option<String> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let (mime, data) = library::read_cover(Path::new(&path))?;
    Some(format!("data:{mime};base64,{}", STANDARD.encode(data)))
}

#[tauri::command(async)]
fn toggle_play(state: State<AppState>) -> bool {
    state.player.toggle_play()
}

#[tauri::command(async)]
fn seek(position: f64, manual: bool, state: State<AppState>) {
    state.player.seek(position, manual);
}

#[tauri::command(async)]
fn set_autoscan(enabled: bool, state: State<AppState>) {
    let cfg = state.config.lock().unwrap().scan.clone();
    state.player.set_scan(enabled, &cfg);
}

#[tauri::command(async)]
fn set_volume(volume: f32, state: State<AppState>) -> Result<(), String> {
    state.player.set_volume(volume);
    let mut cfg = state.config.lock().unwrap();
    cfg.volume = volume.clamp(0.0, 1.0);
    state.save_config(&cfg)
}

#[tauri::command(async)]
fn status(state: State<AppState>) -> PlayerStatus {
    state.player.status()
}

#[tauri::command(async)]
fn get_config(state: State<AppState>) -> Config {
    state.config.lock().unwrap().clone()
}

/// Guarda la config y recalcula la cola del track actual (los botones pueden cambiar).
#[tauri::command(async)]
fn save_config(config: Config, state: State<AppState>) -> Result<Option<TrackView>, String> {
    let config = config.normalized();
    state.save_config(&config)?;
    *state.config.lock().unwrap() = config;
    let mut s = state.session.lock().unwrap();
    if let Some(cur) = s.current.clone() {
        let (root, queue) = library::build_queue(&state.config.lock().unwrap(), &cur);
        s.index = queue.iter().position(|p| p == &cur).unwrap_or(0);
        s.queue = queue;
        s.root = root;
    }
    Ok(view(&state, &s))
}

/// Crea `parent/name` (p. ej. "Las que suenan") y devuelve la ruta.
#[tauri::command(async)]
fn create_subfolder(parent: String, name: String) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.contains(['/', '\\']) || name == "." || name == ".." {
        return Err("Nombre de carpeta inválido".into());
    }
    let dir = Path::new(&parent).join(name);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.to_string_lossy().into_owned())
}

/// Primer argumento que sea un archivo existente (el doble clic del explorador).
fn file_from_args<I: IntoIterator<Item = String>>(args: I) -> Option<String> {
    args.into_iter()
        .skip(1)
        .map(|a| a.strip_prefix("file://").map(percent_decode).unwrap_or(a))
        .find(|a| Path::new(a).is_file())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Debe ir primero: si ya hay una ventana abierta, el doble clic manda
        // el archivo a esa ventana en lugar de abrir otra.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if let Some(path) = file_from_args(argv) {
                let _ = app.emit("open-file", path);
            }
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let config_path = app.path().app_config_dir()?.join("config.json");
            let config = Config::load(&config_path).normalized();
            let player = Player::new(config.volume);
            app.manage(AppState {
                app: app.handle().clone(),
                player,
                wave_gen: Arc::new(AtomicU64::new(0)),
                config: Mutex::new(config),
                config_path,
                session: Mutex::new(Session::default()),
                undo: Mutex::new(Vec::new()),
                pending_open: Mutex::new(file_from_args(std::env::args())),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            startup_file,
            open_file,
            current_track,
            next_track,
            prev_track,
            move_current,
            undo_move,
            toggle_play,
            cover_art,
            seek,
            set_autoscan,
            set_volume,
            status,
            get_config,
            save_config,
            create_subfolder,
        ])
        .build(tauri::generate_context!())
        .expect("error al iniciar Osky")
        .run(|_app, _event| {
            // macOS entrega los archivos abiertos desde Finder como evento.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Opened { urls } = &_event {
                if let Some(p) = urls.iter().find_map(|u| u.to_file_path().ok()) {
                    let _ = _app.emit("open-file", p.to_string_lossy().into_owned());
                }
            }
        });
}
