use serde::{Deserialize, Serialize};
use std::{fs, io, path::PathBuf};

/// Store the user persisted app preferences.
/// Right now this is just the folder that should be scanned for music.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub music_folder: Option<String>,
}

/// Returning the path to the app JSON config file in the user home directory.
fn config_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".songbird").join("config.json")
}

/// Loads the saved configuration if it exists.
/// If the file is missing or invalid, we fall back to defaults.
pub fn load_config() -> AppConfig {
    let path = config_path();
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Persists the app configuration to disk.
pub fn save_config(config: &AppConfig) -> io::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(config).expect("config serializes");
    fs::write(path, raw)
}
