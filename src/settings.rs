//! User settings, stored as JSON next to the saved layout.
//!
//! The settings window edits them live: it writes the shared copy, saves the
//! file and tells the engine, which reads the shared copy on every use.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Anything added here must keep a serde default so old files still load.
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

/// The range a numeric setting may take and how far one stepper click moves it.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub min: f64,
    pub max: f64,
    pub step: f64,
}

pub const PAN_SPEED: Limits = Limits { min: 0.25, max: 3.0, step: 0.25 };
pub const PINCH_SPEED: Limits = Limits { min: 0.25, max: 3.0, step: 0.25 };
pub const FLICK: Limits = Limits { min: 0.0, max: 1.0, step: 0.05 };
pub const GAP: Limits = Limits { min: 0.0, max: 200.0, step: 10.0 };

impl Limits {
    pub fn clamp(&self, v: f64, default: f64) -> f64 {
        if v.is_finite() {
            v.clamp(self.min, self.max)
        } else {
            default
        }
    }

    /// `v` moved `dir` steps, landing on a whole step and staying in range.
    pub fn step(&self, v: f64, dir: f64) -> f64 {
        let n = ((v - self.min) / self.step).round() + dir;
        // Rounded to hundredths so the JSON doesn't fill up with 0.30000000000000004.
        let v = ((self.min + n * self.step) * 100.0).round() / 100.0;
        v.clamp(self.min, self.max)
    }

    pub fn at_min(&self, v: f64) -> bool {
        v <= self.min + 1e-9
    }

    pub fn at_max(&self, v: f64) -> bool {
        v >= self.max - 1e-9
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
        std::fs::read_to_string(path()).map(|s| Settings::from_json(&s)).unwrap_or_default()
    }

    pub fn save(&self) {
        let path = path();
        // Written beside and renamed over, so a crash mid-write can't leave
        // half a file behind.
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, self.to_json()).and_then(|_| std::fs::rename(&tmp, &path)) {
            log!("couldn't save settings: {e}");
        }
    }

    /// Unreadable files fall back to the defaults; out-of-range values are
    /// pulled back into range.
    pub fn from_json(s: &str) -> Settings {
        serde_json::from_str::<Settings>(s).map(Settings::sanitized).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    pub fn sanitized(self) -> Settings {
        let d = Settings::default();
        Settings {
            pan_speed: PAN_SPEED.clamp(self.pan_speed, d.pan_speed),
            pinch_speed: PINCH_SPEED.clamp(self.pinch_speed, d.pinch_speed),
            flick: FLICK.clamp(self.flick, d.flick),
            gap: GAP.clamp(self.gap, d.gap),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let s = Settings {
            pan_speed: 1.5,
            pinch_speed: 0.75,
            flick: 0.4,
            two_finger_canvas: false,
            three_finger: true,
            show_grid: false,
            show_labels: true,
            show_minimap: false,
            remember_layout: false,
            autostart: true,
            gap: 60.0,
        };
        assert_eq!(Settings::from_json(&s.to_json()), s);
        assert_eq!(Settings::from_json(&Settings::default().to_json()), Settings::default());
    }

    #[test]
    fn missing_fields_take_defaults() {
        let s = Settings::from_json(r#"{ "pan_speed": 2.0, "unknown": 1 }"#);
        assert_eq!(s, Settings { pan_speed: 2.0, ..Settings::default() });
    }

    #[test]
    fn bad_input_is_repaired() {
        assert_eq!(Settings::from_json("not json"), Settings::default());
        let s = Settings::from_json(r#"{ "pan_speed": -4, "flick": 9, "gap": 1e9 }"#);
        assert_eq!((s.pan_speed, s.flick, s.gap), (PAN_SPEED.min, FLICK.max, GAP.max));
    }

    #[test]
    fn steps_land_on_whole_steps() {
        assert_eq!(FLICK.step(0.25, 1.0), 0.3);
        assert_eq!(FLICK.step(0.0, -1.0), 0.0);
        assert_eq!(PAN_SPEED.step(1.1, 1.0), 1.25);
        assert_eq!(GAP.step(200.0, 1.0), 200.0);
        assert!(GAP.at_max(GAP.step(190.0, 1.0)));
    }
}
