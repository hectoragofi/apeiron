//! infinite-canvas: your desktop as an unbounded plane you pan and zoom with
//! touchpad gestures. See `docs/PLAN.md` for the architecture.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Declared first so every module below can use it.
macro_rules! log {
    ($($arg:tt)*) => { $crate::safety::log(format_args!($($arg)*)) };
}

mod autostart;
mod engine;
mod icons;
mod layout;
mod safety;
mod settings;
mod shared;
mod tray;
mod ui;

use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;

use settings::Settings;
use shared::Shared;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("rescue") => {
            safety::rescue_cli();
            return;
        }
        Some("--watchdog") => {
            safety::run_watchdog(args.get(2).and_then(|p| p.parse().ok()).unwrap_or(0));
            return;
        }
        _ => {}
    }

    safety::start_log();
    safety::install();

    // Held for the life of the process.
    let _instance = unsafe { CreateMutexW(None, false, w!("Local\\infinite-canvas")) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        log!("already running");
        return;
    }
    safety::spawn_watchdog();

    let settings = Settings::load();
    log!("infinite-canvas {} starting", env!("CARGO_PKG_VERSION"));
    autostart::sync(&settings);
    let (shared, ui_rx) = Shared::new(settings);
    let engine = match engine::spawn(shared.clone()) {
        Ok(e) => e,
        Err(e) => {
            log!("engine failed to start: {e}");
            return;
        }
    };

    ui::run(shared, engine, ui_rx);
    tray::remove();

    let n = engine::wins::rescue_all();
    log!("bye ({n} windows brought back on screen)");
}
