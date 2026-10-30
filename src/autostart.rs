//! Run-at-login via `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

use windows_registry::CURRENT_USER;

use crate::settings::Settings;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "infinite-canvas";

/// Writes or deletes our Run value so it matches `settings.autostart`.
/// Called at startup (the exe may have moved) and when the toggle changes.
pub fn sync(settings: &Settings) {
    let key = match CURRENT_USER.create(RUN_KEY) {
        Ok(k) => k,
        Err(e) => {
            log!("autostart: couldn't open the Run key: {}", e.message());
            return;
        }
    };
    let current = key.get_string(VALUE).ok();
    if !settings.autostart {
        if current.is_some() {
            match key.remove_value(VALUE) {
                Ok(()) => log!("autostart off"),
                Err(e) => log!("autostart: couldn't remove the Run value: {}", e.message()),
            }
        }
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            log!("autostart: couldn't find our own exe: {e}");
            return;
        }
    };
    let command = format!("\"{}\"", exe.display());
    if current.as_deref() != Some(command.as_str()) {
        match key.set_string(VALUE, &command) {
            Ok(()) => log!("autostart on: {command}"),
            Err(e) => log!("autostart: couldn't write the Run value: {}", e.message()),
        }
    }
}
