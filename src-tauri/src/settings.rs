use std::{
    fs::{self, File, OpenOptions},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

const SETTINGS_FILE: &str = "settings.json";
const LOG_FILE: &str = "cmf-tray.log";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub last_device: Option<String>,
    pub autostart: bool,
    pub manual_com_port: Option<String>,
    pub debug: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            last_device: None,
            autostart: false,
            manual_com_port: None,
            debug: false,
        }
    }
}

pub fn app_data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_default()
        .join("cmf-tray")
}

pub fn load() -> Settings {
    let path = app_data_dir().join(SETTINGS_FILE);
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

pub fn save(settings: &Settings) {
    let dir = app_data_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let text = match serde_json::to_string_pretty(settings) {
        Ok(text) => text,
        Err(_) => return,
    };
    let _ = fs::write(dir.join(SETTINGS_FILE), text);
}

/// File logging is opt-in via `--debug` or `CMF_TRAY_DEBUG=1` so the idle app
/// never pays for a logger it isn't using.
pub fn init_logging(enabled: bool) {
    if !enabled {
        return;
    }
    let dir = app_data_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let file: File = match OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG_FILE))
    {
        Ok(file) => file,
        Err(error) => {
            eprintln!("cmf-tray: cannot open log file: {error}");
            return;
        }
    };
    let writer = move || file.try_clone().expect("clone log file handle");
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(false)
        .with_level(true)
        .init();
}
