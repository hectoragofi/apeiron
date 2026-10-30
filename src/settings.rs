//! User settings, stored as JSON next to the saved layout.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// FEATURE B: the Settings window (ui/settings_window.rs) edits these live;
// anything it adds must keep a serde default so old files still load.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub pan_speed: f64,
    pub pinch_speed: f64,
    /// Seconds of release velocity a flick carries.
    pub flick: f64,
    /// Two fingers over empty canvas pan and zoom it.
    pub two_finger_canvas: bool,
    /// Three fingers pan and zoom from anywhere.
    pub three_finger: bool,
    pub show_grid: bool,
    pub show_labels: bool,
    pub show_minimap: bool,
    pub remember_layout: bool,
    pub autostart: bool,
    /// Space left between windows, in canvas pixels.
    pub gap: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            pan_speed: 1.0,
            pinch_speed: 1.0,
            flick: 0.25,
            two_finger_canvas: true,
            three_finger: true,
            show_grid: true,
            show_labels: true,
            show_minimap: true,
            remember_layout: true,
            autostart: false,
            gap: 40.0,
        }
    }
}

/// `%APPDATA%\infinite-canvas`, created on demand.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join("infinite-canvas");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn path() -> PathBuf {
    data_dir().join("settings.json")
}

impl Settings {
    pub fn load() -> Settings {
        std::fs::read_to_string(path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    #[allow(dead_code)]
    pub fn save(&self) {
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path(), json) {
                    log!("couldn't save settings: {e}");
                }
            }
            Err(e) => log!("couldn't serialize settings: {e}"),
        }
    }
}
