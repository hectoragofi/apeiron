//! Updates from the GitHub releases. A background check (soon after start,
//! then every few hours) downloads a newer `apeiron.exe` next to this one.
//! It takes this exe's place at the next start, or right away through the
//! tray's "Restart to update".
//!
//! Downloads go through `curl.exe`, which ships with Windows 10 and 11, so
//! there's no HTTP stack in the binary. Each release also carries
//! `apeiron.exe.sha256`; a download that doesn't match it (or a release
//! without it) is thrown away.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::shared::{Shared, UiEvent};

const LATEST: &str = "https://api.github.com/repos/hectoragofi/apeiron/releases/latest";
const ASSET: &str = "apeiron.exe";
const HASH_ASSET: &str = "apeiron.exe.sha256";
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
    let Some(Release { version, url, hash_url }) = release(&json) else {
        return Err("release has no apeiron.exe".into());
    };
    if !newer(&version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let Some(hash_url) = hash_url else { return Err(format!("release {version} has no {HASH_ASSET}; not updating")) };
    let hash_file = curl(&[&hash_url])?;
    let Some(want) = parse_hash_file(&String::from_utf8_lossy(&hash_file), ASSET) else {
        return Err(format!("{HASH_ASSET} of {version} is unreadable; not updating"));
    };
    let (Some(part), Some(new)) = (path("apeiron.download.exe"), path("apeiron.new.exe")) else {
        return Err("no exe path".into());
    };
    let part_s = part.to_string_lossy().into_owned();
    curl(&["-o", &part_s, &url])?;
    let bytes = std::fs::read(&part).unwrap_or_default();
    if (bytes.len() as u64) < MIN_SIZE || !bytes.starts_with(b"MZ") {
        let _ = std::fs::remove_file(&part);
        return Err("download isn't a Windows program".into());
    }
    if !matches_hash(&bytes, &want) {
        let _ = std::fs::remove_file(&part);
        return Err(format!("download of {version} doesn't match its {HASH_ASSET}; thrown away"));
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

#[derive(Debug, PartialEq)]
struct Release {
    /// No leading v.
    version: String,
    url: String,
    hash_url: Option<String>,
}

/// A release's version and the download links of its exe and the exe's hash.
fn release(json: &serde_json::Value) -> Option<Release> {
    let version = json["tag_name"].as_str()?.trim_start_matches('v').to_string();
    let assets = json["assets"].as_array()?;
    let link = |name: &str| {
        assets.iter().find(|a| a["name"] == name).and_then(|a| a["browser_download_url"].as_str()).map(String::from)
    };
    Some(Release { version, url: link(ASSET)?, hash_url: link(HASH_ASSET) })
}

/// The SHA-256 for `name` in a `sha256sum` listing (`<hex>  <name>`, or
/// `<hex> *<name>` in binary mode).
fn parse_hash_file(text: &str, name: &str) -> Option<[u8; 32]> {
    text.lines().find_map(|line| {
        let (hex, file) = line.trim().split_once(char::is_whitespace)?;
        if file.trim().trim_start_matches('*') != name || hex.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(out)
    })
}

fn matches_hash(bytes: &[u8], want: &[u8; 32]) -> bool {
    Sha256::digest(bytes).as_slice() == want.as_slice()
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
        let r = release(&json).unwrap();
        assert_eq!((r.version.as_str(), r.url.as_str(), r.hash_url), ("0.3.0", "https://x/apeiron.exe", None));
        assert_eq!(release(&serde_json::json!({ "tag_name": "v1", "assets": [] })), None);
        let json = serde_json::json!({
            "tag_name": "v0.4.0",
            "assets": [
                { "name": "apeiron.exe", "browser_download_url": "https://x/apeiron.exe" },
                { "name": "apeiron.exe.sha256", "browser_download_url": "https://x/apeiron.exe.sha256" }
            ]
        });
        assert_eq!(release(&json).unwrap().hash_url.as_deref(), Some("https://x/apeiron.exe.sha256"));
    }

    /// SHA-256 of "abc".
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn hash_file_is_parsed() {
        let want = parse_hash_file(&format!("{ABC}  apeiron.exe\n"), "apeiron.exe").unwrap();
        assert_eq!((want[0], want[31]), (0xba, 0xad));
        assert_eq!(parse_hash_file(&format!("{}  apeiron.exe", ABC.to_uppercase()), "apeiron.exe"), Some(want));
        assert_eq!(parse_hash_file(&format!("{ABC} *apeiron.exe\r\n"), "apeiron.exe"), Some(want));
        assert_eq!(parse_hash_file(&format!("{ABC}  other.exe"), "apeiron.exe"), None);
        assert_eq!(parse_hash_file(&format!("{}  apeiron.exe", &ABC[..62]), "apeiron.exe"), None);
        assert_eq!(parse_hash_file("<html>Not Found</html>", "apeiron.exe"), None);
        assert_eq!(parse_hash_file("", "apeiron.exe"), None);
    }

    #[test]
    fn download_must_match_its_hash() {
        let want = parse_hash_file(&format!("{ABC}  apeiron.exe"), "apeiron.exe").unwrap();
        assert!(matches_hash(b"abc", &want));
        assert!(!matches_hash(b"abd", &want));
        assert!(!matches_hash(b"", &want));
    }
}
