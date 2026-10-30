//! Keeping the user's windows reachable no matter how we exit, plus the log
//! file. The app only ever moves windows, so the failure mode to guard
//! against is windows stranded off-screen.

use std::fmt::Arguments;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Console::{AttachConsole, SetConsoleCtrlHandler, ATTACH_PARENT_PROCESS};
use windows::Win32::System::Threading::{
    OpenMutexW, OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

use crate::engine::wins;
use crate::settings::data_dir;

/// Held by the running instance for its whole life.
pub const INSTANCE_MUTEX: PCWSTR = w!("Local\\infinite-canvas");

/// Log files past this size are started afresh.
const MAX_LOG_BYTES: u64 = 512 * 1024;

static START: OnceLock<Instant> = OnceLock::new();

/// Appends a line to `%APPDATA%\infinite-canvas\log.txt` (and stdout in debug).
pub fn log(args: Arguments) {
    let t = START.get_or_init(Instant::now).elapsed().as_secs_f64();
    let line = format!("[{t:8.2}] {args}");
    println!("{line}");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(data_dir().join("log.txt")) {
        let _ = writeln!(f, "{line}");
    }
}

pub fn start_log() {
    START.get_or_init(Instant::now);
    let path = data_dir().join("log.txt");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = std::fs::remove_file(path);
    }
}

/// Process-wide setup that has to happen before any window exists.
pub fn install() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = SetConsoleCtrlHandler(Some(on_console_close), true);
    }
    std::panic::set_hook(Box::new(|info| {
        log!("panic: {info}");
        wins::rescue_all();
    }));
}

/// `infinite-canvas rescue`: pull stranded windows back on screen.
pub fn rescue_cli() {
    // The release build has no console of its own; borrow the parent's.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
    let n = wins::rescue_all();
    log!("rescue: moved {n} windows back on screen");
}

/// Closing the console kills the process without unwinding; put any parked
/// windows back first.
unsafe extern "system" fn on_console_close(_ctrl: u32) -> BOOL {
    wins::rescue_all();
    false.into()
}

/// A detached copy of ourselves that waits for this process to exit (however
/// it exits) and then brings stranded windows back on screen.
pub fn spawn_watchdog() {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let Ok(exe) = std::env::current_exe() else { return };
    let spawned = std::process::Command::new(exe)
        .args(["--watchdog", &std::process::id().to_string()])
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn();
    if let Err(e) = spawned {
        log!("couldn't start watchdog: {e}");
    }
}

/// Rescue passes after the process exits, in seconds since. Moves we sent
/// to a busy app land whenever it gets to them, so look again for a while.
const WATCH_PASSES: [u64; 4] = [0, 1, 3, 10];

pub fn run_watchdog(pid: u32) {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else { return };
        WaitForSingleObject(h, INFINITE);
        let _ = CloseHandle(h);
    }
    let mut waited = 0;
    for at in WATCH_PASSES {
        std::thread::sleep(Duration::from_secs(at - waited));
        waited = at;
        // A new instance places windows on its own canvas; leave them be.
        if instance_running() {
            log!("watchdog: a new instance is running; stopping");
            return;
        }
        let n = wins::rescue_all();
        log!("watchdog: process {pid} exited {at} s ago, rescued {n} windows");
    }
    wins::clear_parked();
}

/// Whether some instance holds the single-instance mutex.
fn instance_running() -> bool {
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, INSTANCE_MUTEX) } {
        Ok(h) => {
            unsafe {
                let _ = CloseHandle(h);
            }
            true
        }
        Err(_) => false,
    }
}
