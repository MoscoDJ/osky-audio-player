use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Una carpeta destino: un botón "Mover a ...".
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Destination {
    pub label: String,
    pub path: String,
}

/// Una carpeta raíz (p. ej. ".../Warmup Electronico"). Cualquier track dentro
/// de ella (a cualquier profundidad) muestra sus botones de destino.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Root {
    pub path: String,
    pub destinations: Vec<Destination>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct ScanConfig {
    /// El escaneo automático arranca solo al abrir cada track.
    pub enabled: bool,
    /// Segundos que suena cada punto antes de brincar al siguiente.
    pub segment_secs: f64,
    /// En cuántos puntos se reparte el track.
    pub points: u32,
    /// Al terminar una pasada, vuelve a empezar con puntos nuevos.
    pub repeat: bool,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            segment_secs: 1.0,
            points: 12,
            repeat: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    pub roots: Vec<Root>,
    pub scan: ScanConfig,
    pub volume: f32,
    /// Borra la subcarpeta de origen cuando queda completamente vacía.
    pub remove_empty_folders: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            scan: ScanConfig::default(),
            volume: 0.8,
            remove_empty_folders: true,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Config {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        // Escritura atómica: si algo falla a medias no se pierde la config.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json).map_err(|e| e.to_string())?;
        fs::rename(&tmp, path).map_err(|e| e.to_string())
    }

    /// Limpia entradas vacías, rutas con "/" final y valores fuera de rango.
    pub fn normalized(mut self) -> Config {
        for root in &mut self.roots {
            root.path = clean_path(&root.path);
            root.destinations.retain(|d| !d.path.trim().is_empty());
            for d in &mut root.destinations {
                d.path = clean_path(&d.path);
                if d.label.trim().is_empty() {
                    d.label = folder_name(Path::new(&d.path));
                }
            }
        }
        self.roots.retain(|r| !r.path.is_empty());
        self.scan.segment_secs = self.scan.segment_secs.clamp(0.25, 30.0);
        self.scan.points = self.scan.points.clamp(1, 100);
        self.volume = self.volume.clamp(0.0, 1.0);
        self
    }

    /// Índice de la raíz que contiene `file` (la más específica gana).
    /// También cuenta si el archivo está dentro de uno de sus destinos.
    pub fn root_for(&self, file: &Path) -> Option<usize> {
        self.roots
            .iter()
            .enumerate()
            .filter_map(|(i, r)| {
                let rp = Path::new(&r.path);
                if file.starts_with(rp) {
                    return Some((i, rp.components().count()));
                }
                r.destinations
                    .iter()
                    .find(|d| file.starts_with(&d.path))
                    .map(|d| (i, Path::new(&d.path).components().count()))
            })
            .max_by_key(|(_, depth)| *depth)
            .map(|(i, _)| i)
    }
}

pub fn clean_path(p: &str) -> String {
    let p = p.trim();
    if p.is_empty() {
        return String::new();
    }
    PathBuf::from(p).components().collect::<PathBuf>().to_string_lossy().into_owned()
}

pub fn folder_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}
