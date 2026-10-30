//! The settings window, opened from the tray.

use gpui::App;

use crate::shared::{EngineHandle, Shared};

// FEATURE B: a normal GPUI window (single instance; focus the existing one if
// it is open) with Gestures / Canvas / General sections, toggles and steppers
// that write `shared.settings` + settings.json and send Command::SettingsChanged,
// a hotkey reference, and "Tidy up now" / "Reset saved layout" buttons.
pub fn open(_cx: &mut App, _shared: &Shared, _engine: &EngineHandle) {
    log!("settings window is not implemented yet");
}
