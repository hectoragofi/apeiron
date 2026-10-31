//! Updates from the GitHub releases. A background check (soon after start,
//! then every few hours) downloads a newer `apeiron.exe` next to this one.
//! It takes this exe's place at the next start, or right away through the
//! tray's "Restart to update".
//!
//! Downloads go through `curl.exe`, which ships with Windows 10 and 11, so
//! there's no HTTP stack in the binary.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::shared::{Shared, UiEvent};

const LATEST: &str = "https://api.github.com/repos/hectoragofi/apeiron/releases/latest";
const ASSET: &str = "apeiron.exe";
const FIRST_CHECK: Duration = Duration::from_secs(30);
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// Anything smaller than this isn't a real build (an error page, a cut-off
/// download).
const MIN_SIZE: u64 = 1 << 20;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Idle,
    Checking,
    UpToDate,
    /// Downloaded and waiting; the version it brings.
    Ready(String),
    Failed,
}

static STATUS: Mutex<Status> = Mutex::new(Status::Idle);
/// Passed to the new process when restarting into an update.
pub const RESTARTED: &str = "--updated";

static RESTART: AtomicBool = AtomicBool::new(false);

pub fn status() -> Status {
    STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set_status(shared: &Shared, s: Status) {
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = s;
    shared.emit(UiEvent::Update);
}

/// This exe's path, read once at start: after a swap the running file has
/// been renamed, but this is still where the app lives.
fn exe() -> Option<&'static PathBuf> {
    static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    EXE.get_or_init(|| std::env::current_exe().ok()).as_ref()
}

fn path(name: &str) -> Option<PathBuf> {
    Some(exe()?.with_file_name(name))
}

/// At start: if an update is waiting, move it into this exe's place and
/// start it instead. True when it did (this process should just exit).
pub fn apply_pending() -> bool {
    let (Some(new), Some(old)) = (path("apeiron.new.exe"), path("apeiron.old.exe")) else { return false };
    // Left over from the last swap; still locked if that process lives on.
    let _ = std::fs::remove_file(&old);
    if !new.exists() {
        return false;
    }
    match swap() {
        Ok(()) => {
            log!("update: installed; restarting into it");
            relaunch()
        }
        Err(e) => {
            log!("update: couldn't install the download: {e}");
            false
        }
    }
}

/// Puts `apeiron.new.exe` where this exe is. A running exe can't be
/// overwritten but can be renamed, so it steps aside as `apeiron.old.exe`.
fn swap() -> std::io::Result<()> {
    let exe = exe().ok_or_else(|| std::io::Error::other("no exe path"))?;
    let (new, old) = (exe.with_file_name("apeiron.new.exe"), exe.with_file_name("apeiron.old.exe"));
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old)?;
    if let Err(e) = std::fs::rename(&new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(e);
    }
    Ok(())
}

/// Starts the exe at this one's path (the new build after a swap).
fn relaunch() -> bool {
    let Some(exe) = exe() else { return false };
    let mut args: Vec<String> = std::env::args().skip(1).filter(|a| a != RESTARTED).collect();
    args.push(RESTARTED.into());
    match Command::new(exe).args(args).spawn() {
        Ok(_) => true,
        Err(e) => {
            log!("update: couldn't start {}: {e}", exe.display());
            false
        }
    }
}

/// From the tray, with an update downloaded: install it now and restart
/// once the normal quit has put every window back.
pub fn restart_now() -> bool {
    match swap() {
        Ok(()) => {
            RESTART.store(true, Ordering::SeqCst);
            true
        }
        Err(e) => {
            log!("update: couldn't install the download: {e}");
            false
        }
    }
}

/// Called last thing on the way out.
pub fn finish() {
    if RESTART.load(Ordering::SeqCst) {
        relaunch();
    }
}

/// Checks in the background now and then, while that's turned on.
pub fn spawn_checker(shared: Shared) {
    let spawned = std::thread::Builder::new().name("update".into()).spawn(move || {
        std::thread::sleep(FIRST_CHECK);
        loop {
            if shared.settings().auto_update {
                check(&shared);
            }
            std::thread::sleep(EVERY);
        }
    });
    if let Err(e) = spawned {
        log!("update: couldn't start the checker: {e}");
    }
}

/// "Check for updates" in the tray.
pub fn check_now(shared: &Shared) {
    let shared = shared.clone();
    let _ = std::thread::Builder::new().name("update-now".into()).spawn(move || check(&shared));
}

fn check(shared: &Shared) {
    if matches!(status(), Status::Checking | Status::Ready(_)) {
        return;
    }
    set_status(shared, Status::Checking);
    let s = match find_and_fetch() {
        Ok(Some(v)) => {
            log!("update: {v} downloaded");
            Status::Ready(v)
        }
        Ok(None) => Status::UpToDate,
        Err(e) => {
            log!("update: {e}");
            Status::Failed
        }
    };
    set_status(shared, s);
}

/// The newest release, downloaded if it's newer than this build.
fn find_and_fetch() -> Result<Option<String>, String> {
    let body = curl(&["-H", "Accept: application/vnd.github+json", LATEST])?;
    let json: serde_json::Value = serde_json::from_slice(&body).map_err(|e| format!("bad release info: {e}"))?;
    let Some((version, url)) = release(&json) else { return Err("release has no apeiron.exe".into()) };
    if !newer(&version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let (Some(part), Some(new)) = (path("apeiron.download.exe"), path("apeiron.new.exe")) else {
        return Err("no exe path".into());
    };
    let part_s = part.to_string_lossy().into_owned();
    curl(&["-o", &part_s, &url])?;
    let ok = std::fs::read(&part).is_ok_and(|b| b.len() as u64 >= MIN_SIZE && b.starts_with(b"MZ"));
    if !ok {
        let _ = std::fs::remove_file(&part);
        return Err("download isn't a Windows program".into());
    }
    std::fs::rename(&part, &new).map_err(|e| format!("couldn't keep the download: {e}"))?;
    Ok(Some(version))
}

fn curl(args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("curl.exe")
        .args(["-sfL", "--max-time", "300", "-A", concat!("apeiron/", env!("CARGO_PKG_VERSION"))])
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("couldn't run curl: {e}"))?;
    if !out.status.success() {
        return Err(format!("curl failed ({})", out.status));
    }
    Ok(out.stdout)
}

/// The version (no leading v) and exe download link of a release.
fn release(json: &serde_json::Value) -> Option<(String, String)> {
    let tag = json["tag_name"].as_str()?.trim_start_matches('v').to_string();
    let url = json["assets"].as_array()?.iter().find(|a| a["name"] == ASSET)?["browser_download_url"].as_str()?;
    Some((tag, url.to_string()))
}

/// Whether version `a` comes after `b` (dotted numbers; anything after a
/// `-` is ignored).
fn newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split('-').next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect()
    };
    parse(a) > parse(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("0.3.0", "0.2.0"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("1.0", "0.99.0"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("0.2.0-rc1", "0.2.0"));
    }

    #[test]
    fn release_finds_the_exe() {
        let json = serde_json::json!({
            "tag_name": "v0.3.0",
            "assets": [
                { "name": "notes.txt", "browser_download_url": "https://x/notes.txt" },
                { "name": "apeiron.exe", "browser_download_url": "https://x/apeiron.exe" }
            ]
        });
        assert_eq!(release(&json), Some(("0.3.0".into(), "https://x/apeiron.exe".into())));
        assert_eq!(release(&serde_json::json!({ "tag_name": "v1", "assets": [] })), None);
    }
}
