//! User settings, stored as JSON next to the saved layout.
//!
//! The settings window edits them live: it writes the shared copy, saves the
//! file and tells the engine, which reads the shared copy on every use.

use std::path::{Path, PathBuf};
use std::sync::Once;

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
    pub background: Background,
    /// How far the wallpaper scrolls per pixel the canvas pans; below 1 it
    /// reads as distant scenery.
    pub parallax: f64,
    /// Dots on the canvas when the background is the dot grid.
    pub show_grid: bool,
    pub show_labels: bool,
    pub show_minimap: bool,
    pub remember_layout: bool,
    pub autostart: bool,
    /// Space left between windows, in canvas pixels.
    pub gap: f64,
    /// Four-finger swipes and taps, and a three-finger tap. Four fingers are
    /// only Apeiron's when Windows' own four-finger gestures are off.
    pub four_up: GestureAction,
    pub four_down: GestureAction,
    pub four_left: GestureAction,
    pub four_right: GestureAction,
    pub four_tap: GestureAction,
    pub three_tap: GestureAction,
    /// In the overview, the window under the pointer gets the keyboard.
    pub hover_focus: bool,
    /// Landing back on 1:1 frames the window it lands on, whole.
    pub land_framed: bool,
    /// A flick stops on the next window in its direction.
    pub magnetic_flick: bool,
    /// A pinch that ends near a window's (or group's) fit zoom snaps to it.
    pub zoom_fit: bool,
    /// Zoomed-out pans come to rest centred on a group of windows.
    pub overview_snap: bool,
    /// Dragged windows line up with their neighbours' edges.
    pub window_snap: bool,
    /// Download new releases in the background.
    pub auto_update: bool,
    /// Global shortcuts, written like "Ctrl+Alt+Z" (see `hotkey`); empty
    /// means unbound.
    pub key_overview: String,
    pub key_find: String,
    pub key_tidy: String,
    pub key_quit: String,
    /// Alt+Tab opens Apeiron's own window switcher instead of Windows'.
    pub alt_tab: bool,
}

/// What a custom gesture does.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GestureAction {
    #[default]
    Nothing,
    /// Open the overview (no-op if it's open).
    Overview,
    ToggleOverview,
    /// Leave the overview, back to the windows.
    Back,
    Tidy,
    Search,
    NextDesktop,
    PrevDesktop,
    TaskView,
    ShowDesktop,
}

impl GestureAction {
    pub const ALL: [GestureAction; 10] = [
        GestureAction::Nothing,
        GestureAction::Overview,
        GestureAction::ToggleOverview,
        GestureAction::Back,
        GestureAction::Tidy,
        GestureAction::Search,
        GestureAction::NextDesktop,
        GestureAction::PrevDesktop,
        GestureAction::TaskView,
        GestureAction::ShowDesktop,
    ];

    pub fn label(self) -> &'static str {
        match self {
            GestureAction::Nothing => "Nothing",
            GestureAction::Overview => "Open overview",
            GestureAction::ToggleOverview => "Toggle overview",
            GestureAction::Back => "Back to windows",
            GestureAction::Tidy => "Tidy up",
            GestureAction::Search => "Find a window",
            GestureAction::NextDesktop => "Next desktop",
            GestureAction::PrevDesktop => "Previous desktop",
            GestureAction::TaskView => "Task View",
            GestureAction::ShowDesktop => "Show desktop",
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            pan_speed: 1.0,
            pinch_speed: 1.0,
            flick: 0.25,
            two_finger_canvas: true,
            three_finger: true,
            background: Background::Wallpaper,
            parallax: 0.35,
            show_grid: true,
            show_labels: true,
            show_minimap: true,
            remember_layout: true,
            autostart: false,
            gap: 40.0,
            four_up: GestureAction::Overview,
            four_down: GestureAction::Back,
            four_left: GestureAction::NextDesktop,
            four_right: GestureAction::PrevDesktop,
            four_tap: GestureAction::Nothing,
            three_tap: GestureAction::Nothing,
            hover_focus: true,
            land_framed: true,
            magnetic_flick: true,
            zoom_fit: true,
            overview_snap: true,
            window_snap: true,
            auto_update: true,
            key_overview: "Ctrl+Alt+Z".into(),
            key_find: "Ctrl+Alt+F".into(),
            key_tidy: "Ctrl+Alt+T".into(),
            key_quit: "Ctrl+Alt+Q".into(),
            alt_tab: true,
        }
    }
}

/// What the canvas is painted with behind the windows.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Background {
    /// The desktop wallpaper, mirror-tiled into one endless scene. Falls
    /// back to the dot grid when there's no picture to show.
    #[default]
    Wallpaper,
    DotGrid,
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
pub const PARALLAX: Limits = Limits { min: 0.0, max: 1.0, step: 0.05 };

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

/// `%APPDATA%\Apeiron`, created on demand.
pub fn data_dir() -> PathBuf {
    static MIGRATED: Once = Once::new();
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join("Apeiron");
    MIGRATED.call_once(|| migrate(&base.join(LEGACY_DIR), &dir));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The data folder's name before the app was called Apeiron.
const LEGACY_DIR: &str = "infinite-canvas";

/// Carries settings, the saved layout and the parked-window record over
/// from the old data folder, once: only when the new folder doesn't exist.
fn migrate(old: &Path, new: &Path) {
    if new.exists() || !old.is_dir() || std::fs::create_dir_all(new).is_err() {
        return;
    }
    for name in ["settings.json", "layout.json", "parked.txt"] {
        let from = old.join(name);
        if from.is_file() {
            let _ = std::fs::copy(&from, new.join(name));
        }
    }
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
            parallax: PARALLAX.clamp(self.parallax, d.parallax),
            key_overview: crate::hotkey::normalized(&self.key_overview),
            key_find: crate::hotkey::normalized(&self.key_find),
            key_tidy: crate::hotkey::normalized(&self.key_tidy),
            key_quit: crate::hotkey::normalized(&self.key_quit),
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
            background: Background::DotGrid,
            parallax: 0.5,
            show_grid: false,
            show_labels: true,
            show_minimap: false,
            remember_layout: false,
            autostart: true,
            gap: 60.0,
            four_up: GestureAction::Tidy,
            four_down: GestureAction::Nothing,
            four_left: GestureAction::PrevDesktop,
            four_right: GestureAction::NextDesktop,
            four_tap: GestureAction::Search,
            three_tap: GestureAction::ToggleOverview,
            hover_focus: false,
            land_framed: true,
            magnetic_flick: false,
            zoom_fit: true,
            overview_snap: false,
            window_snap: true,
            auto_update: false,
            key_overview: "Shift+Win+O".into(),
            key_find: String::new(),
            key_tidy: "Ctrl+Alt+T".into(),
            key_quit: "Ctrl+Alt+F12".into(),
            alt_tab: false,
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
        let s = Settings::from_json(r#"{ "pan_speed": -4, "flick": 9, "gap": 1e9, "parallax": 3 }"#);
        assert_eq!((s.pan_speed, s.flick, s.gap, s.parallax), (PAN_SPEED.min, FLICK.max, GAP.max, PARALLAX.max));
    }

    #[test]
    fn background_is_stored_by_name() {
        assert!(Settings::default().to_json().contains(r#""background": "wallpaper""#));
        let s = Settings::from_json(r#"{ "background": "dot_grid" }"#);
        assert_eq!(s.background, Background::DotGrid);
    }

    #[test]
    fn migrates_the_old_data_folder_once() {
        let root = std::env::temp_dir().join(format!("apeiron-test-migrate-{}", std::process::id()));
        let (old, new) = (root.join("old"), root.join("new"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("settings.json"), "{}").unwrap();
        std::fs::write(old.join("log.txt"), "x").unwrap();
        migrate(&old, &new);
        assert!(new.join("settings.json").is_file());
        assert!(!new.join("log.txt").exists());
        // An existing new folder is never overwritten.
        std::fs::write(old.join("layout.json"), "{}").unwrap();
        migrate(&old, &new);
        assert!(!new.join("layout.json").exists());
        let _ = std::fs::remove_dir_all(root);
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
