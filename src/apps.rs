//! Installed apps, for launching from the search panel.
//!
//! The list is the Start menu's "All apps" (`shell:AppsFolder`), which holds
//! both the Start-menu shortcuts (the user's and everyone's) and Store apps,
//! each under a parsing name the shell can launch. Reading it takes the shell
//! a moment, so a worker thread does it at startup and again whenever the
//! search opens on a list older than `STALE`; the search only ever reads
//! the cached copy. If the shell can't list it, the Start menu folders'
//! shortcuts are read instead.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows::core::{HSTRING, PWSTR};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{
    IEnumShellItems, IShellItem, SHGetKnownFolderItem, ShellExecuteW, BHID_EnumItems, FOLDERID_AppsFolder,
    KF_FLAG_DEFAULT, SIGDN, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::shared::AppInfo;

/// Apps installed or removed show up within this long of opening the search.
const STALE: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Catalog {
    apps: Arc<Vec<AppInfo>>,
    loaded: Option<Instant>,
    loading: bool,
}

static CATALOG: LazyLock<Mutex<Catalog>> = LazyLock::new(Mutex::default);

fn catalog() -> MutexGuard<'static, Catalog> {
    CATALOG.lock().unwrap_or_else(|e| e.into_inner())
}

/// The apps as last read (empty until the first read finishes).
pub fn all() -> Arc<Vec<AppInfo>> {
    catalog().apps.clone()
}

/// Reads the list again on a worker thread if it's never been read or has
/// gone stale; `done` runs once a new list is in.
pub fn refresh(done: impl FnOnce() + Send + 'static) {
    {
        let mut c = catalog();
        if c.loading || c.loaded.is_some_and(|t| t.elapsed() < STALE) {
            return;
        }
        c.loading = true;
    }
    let spawned = std::thread::Builder::new().name("apps".into()).spawn(move || {
        let started = Instant::now();
        let apps = read();
        log!("apps: {} found in {} ms", apps.len(), started.elapsed().as_millis());
        {
            let mut c = catalog();
            c.apps = Arc::new(apps);
            c.loaded = Some(Instant::now());
            c.loading = false;
        }
        done();
    });
    if let Err(e) = spawned {
        log!("warning: couldn't read the installed apps: {e}");
        catalog().loading = false;
    }
}

/// Starts an app. Launching can wait on the shell, so it happens on a
/// thread of its own.
pub fn launch(target: String) {
    let spawned = std::thread::Builder::new().name("launch".into()).spawn(move || unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let r = ShellExecuteW(None, &HSTRING::from("open"), &HSTRING::from(target.as_str()), None, None, SW_SHOWNORMAL);
        // Anything above 32 is success.
        if r.0 as isize <= 32 {
            log!("couldn't launch {target} ({})", r.0 as isize);
        }
        if com {
            CoUninitialize();
        }
    });
    if let Err(e) = spawned {
        log!("couldn't launch an app: {e}");
    }
}

fn read() -> Vec<AppInfo> {
    let listed = unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let apps = apps_folder();
        if com {
            CoUninitialize();
        }
        apps
    };
    let mut apps = match listed {
        Ok(v) if !v.is_empty() => v,
        other => {
            if let Err(e) = other {
                log!("apps folder unreadable ({}); reading the Start menu instead", e.message());
            }
            start_menu()
        }
    };
    tidy_list(&mut apps)
}

/// Sorted by name, one entry per name (the same app is often listed for
/// both the user and everyone).
fn tidy_list(apps: &mut Vec<AppInfo>) -> Vec<AppInfo> {
    apps.retain(|a| !a.name.trim().is_empty());
    apps.sort_by_cached_key(|a| a.name.to_lowercase());
    apps.dedup_by(|a, b| a.name.eq_ignore_ascii_case(&b.name));
    std::mem::take(apps)
}

unsafe fn apps_folder() -> windows::core::Result<Vec<AppInfo>> {
    let folder: IShellItem = SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)?;
    let items: IEnumShellItems = folder.BindToHandler(None, &BHID_EnumItems)?;
    let mut apps = Vec::new();
    loop {
        let mut got = [None];
        let mut n = 0u32;
        if items.Next(&mut got, Some(&mut n)).is_err() || n == 0 {
            break;
        }
        let Some(item) = got[0].take() else { break };
        let (Some(name), Some(id)) = (display_name(&item, SIGDN_NORMALDISPLAY), display_name(&item, SIGDN_PARENTRELATIVEPARSING))
        else {
            continue;
        };
        apps.push(AppInfo { name, target: format!("shell:AppsFolder\\{id}") });
    }
    Ok(apps)
}

unsafe fn display_name(item: &IShellItem, kind: SIGDN) -> Option<String> {
    let p: PWSTR = item.GetDisplayName(kind).ok()?;
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as *const _));
    s.filter(|s| !s.is_empty())
}

/// The shortcuts in the user's and everyone's Start menu folders.
fn start_menu() -> Vec<AppInfo> {
    let roots = [("APPDATA", r"Microsoft\Windows\Start Menu\Programs"), ("ProgramData", r"Microsoft\Windows\Start Menu\Programs")];
    let mut apps = Vec::new();
    for (var, sub) in roots {
        if let Some(base) = std::env::var_os(var) {
            shortcuts(&PathBuf::from(base).join(sub), 0, &mut apps);
        }
    }
    apps
}

fn shortcuts(dir: &Path, depth: usize, out: &mut Vec<AppInfo>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            if depth < 4 {
                shortcuts(&path, depth + 1, out);
            }
        } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")) {
            let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            out.push(AppInfo { name, target: path.to_string_lossy().into_owned() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str) -> AppInfo {
        AppInfo { name: name.into(), target: format!("shell:AppsFolder\\{name}") }
    }

    #[test]
    fn list_is_sorted_and_deduplicated() {
        let mut v = vec![app("Paint"), app("calculator"), app("  "), app("Calculator"), app("Notepad")];
        let names: Vec<String> = tidy_list(&mut v).into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["calculator", "Notepad", "Paint"]);
    }
}
