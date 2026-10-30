//! Run-at-login via `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

use crate::settings::Settings;

// FEATURE B: write or delete the Run value (name "infinite-canvas", data = the
// quoted path of the current exe) with windows-registry so it matches
// `settings.autostart`. Called once at startup and whenever the toggle changes.
pub fn sync(_settings: &Settings) {}
