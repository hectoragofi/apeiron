//! Window icons for labels and search results.
//!
//! An icon comes from the window (`WM_GETICON`), its class (`GCLP_HICON`) or,
//! failing both, the first icon in its executable. Asking another app (or
//! the disk) can take a while, so a worker thread fetches icons and renders
//! each once at 32x32 into a BGRA image; drawing code only ever reads the
//! cache and shows a placeholder until the icon arrives. Images of windows
//! that have gone away are dropped from the sprite atlas of every GPUI window
//! that draws icons (each has its own).
//!
//! Installed apps (the search's launcher) get their icon from the shell,
//! by the name the shell launches them by, on the same worker.

use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use gpui::{RenderImage, Window};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::UI::Shell::{
    ExtractIconExW, IShellItem, SHCreateItemFromParsingName, SHGetFileInfoW, SHGetIDListFromObject, SHFILEINFOW,
    SHGFI_ICON, SHGFI_LARGEICON, SHGFI_PIDL,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::shared::WinInfo;

const SIZE: i32 = 32;
/// A window that had no icon is asked again after this long; new windows
/// often set theirs a moment after they appear.
const RETRY: Duration = Duration::from_secs(3);
/// Never wait long on a busy app; a hung one is skipped outright.
const SEND_TIMEOUT_MS: u32 = 40;

enum Entry {
    /// Queued for the worker.
    Pending,
    Done { image: Option<Arc<RenderImage>>, fetched: Instant },
}

enum Job {
    Window(isize),
    /// An app, by its launch target (see `AppInfo`).
    App(String),
}

struct Icons {
    cache: Mutex<HashMap<isize, Entry>>,
    /// Apps' icons; few, and kept for the whole run.
    apps: Mutex<HashMap<String, Entry>>,
    worker: Sender<Job>,
}

static ICONS: OnceLock<Icons> = OnceLock::new();

/// The GPUI windows that draw icons.
#[derive(Clone, Copy)]
pub enum Atlas {
    Overlay,
    Desktop,
    Map,
}

const ATLASES: [Atlas; 3] = [Atlas::Overlay, Atlas::Desktop, Atlas::Map];

/// Per atlas, the images it has yet to free. A hidden window doesn't render,
/// so its share waits here until it next does.
static GONE: [Mutex<Vec<Arc<RenderImage>>>; ATLASES.len()] = [const { Mutex::new(Vec::new()) }; ATLASES.len()];

fn bury(image: Arc<RenderImage>) {
    for atlas in ATLASES {
        GONE[atlas as usize].lock().unwrap_or_else(|e| e.into_inner()).push(image.clone());
    }
}

fn take_gone(atlas: Atlas) -> Vec<Arc<RenderImage>> {
    std::mem::take(&mut *GONE[atlas as usize].lock().unwrap_or_else(|e| e.into_inner()))
}

/// Frees what `prune` let go of from `window`'s atlas. Call while rendering it.
pub fn free_gone(atlas: Atlas, window: &mut Window) {
    for image in take_gone(atlas) {
        let _ = window.drop_image(image);
    }
}

/// Starts the worker; `fetched` is called (on the worker) whenever an icon
/// arrives, so the UI can redraw.
pub fn start(fetched: impl Fn() + Send + 'static) {
    let (tx, rx) = mpsc::channel::<Job>();
    let spawned = std::thread::Builder::new().name("icons".into()).spawn(move || {
        // The shell's icon lookups are COM.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        for job in rx {
            let Some(icons) = ICONS.get() else { continue };
            match job {
                Job::Window(hwnd) => {
                    let image = fetch(HWND(hwnd as *mut _)).map(Arc::new);
                    cache(icons).insert(hwnd, Entry::Done { image, fetched: Instant::now() });
                }
                Job::App(target) => {
                    let image = app_icon(&target).map(Arc::new);
                    lock(&icons.apps).insert(target, Entry::Done { image, fetched: Instant::now() });
                }
            }
            fetched();
        }
    });
    if let Err(e) = spawned {
        log!("warning: no icon worker, labels show initials: {e}");
        return;
    }
    let _ = ICONS.set(Icons { cache: Mutex::new(HashMap::new()), apps: Mutex::new(HashMap::new()), worker: tx });
}

fn cache(icons: &Icons) -> MutexGuard<'_, HashMap<isize, Entry>> {
    lock(&icons.cache)
}

fn lock<K, V>(m: &Mutex<HashMap<K, V>>) -> MutexGuard<'_, HashMap<K, V>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// An installed app's icon if it's been fetched; otherwise asks the worker
/// for it and returns None for now. An app with no icon isn't asked again.
pub fn app_icon_for(target: &str) -> Option<Arc<RenderImage>> {
    let icons = ICONS.get()?;
    let mut apps = lock(&icons.apps);
    match apps.get(target) {
        Some(Entry::Done { image, .. }) => return image.clone(),
        Some(Entry::Pending) => return None,
        None => {}
    }
    if icons.worker.send(Job::App(target.to_string())).is_ok() {
        apps.insert(target.to_string(), Entry::Pending);
    }
    None
}

/// The icon the shell shows for `target` (a `shell:AppsFolder` name or a
/// shortcut's path).
fn app_icon(target: &str) -> Option<RenderImage> {
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(&windows::core::HSTRING::from(target), None).ok()?;
        let pidl = SHGetIDListFromObject(&item).ok()?;
        let mut info = SHFILEINFOW::default();
        let ok = SHGetFileInfoW(
            PCWSTR(pidl as *const u16),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_PIDL | SHGFI_ICON | SHGFI_LARGEICON,
        );
        CoTaskMemFree(Some(pidl as *const _));
        if ok == 0 || info.hIcon.is_invalid() {
            return None;
        }
        let image = render(info.hIcon);
        let _ = DestroyIcon(info.hIcon);
        image
    }
}

/// The window's icon if it's been fetched; otherwise asks the worker for it
/// and returns None for now.
pub fn icon_for(hwnd: isize) -> Option<Arc<RenderImage>> {
    let icons = ICONS.get()?;
    let mut cache = cache(icons);
    match cache.get(&hwnd) {
        Some(Entry::Pending) => return None,
        Some(Entry::Done { image: Some(image), .. }) => return Some(image.clone()),
        Some(Entry::Done { image: None, fetched }) if fetched.elapsed() < RETRY => return None,
        _ => {}
    }
    if icons.worker.send(Job::Window(hwnd)).is_ok() {
        cache.insert(hwnd, Entry::Pending);
    }
    None
}

/// Forgets the icons of windows that are no longer around, leaving each
/// atlas to free them (see `free_gone`).
pub fn prune(live: &[&WinInfo]) {
    let Some(icons) = ICONS.get() else { return };
    let mut cache = cache(icons);
    let dead: Vec<isize> = cache.keys().copied().filter(|id| !live.iter().any(|w| w.id == *id)).collect();
    for id in dead {
        if let Some(Entry::Done { image: Some(image), .. }) = cache.remove(&id) {
            bury(image);
        }
    }
}

fn fetch(h: HWND) -> Option<RenderImage> {
    if let Some(icon) = window_icon(h) {
        if let Some(image) = render(icon) {
            return Some(image);
        }
    }
    let icon = exe_icon(h)?;
    let image = render(icon);
    unsafe {
        let _ = DestroyIcon(icon);
    }
    image
}

/// The icon the window shows in its title bar or the taskbar. Owned by the
/// window, so never destroyed here.
fn window_icon(h: HWND) -> Option<HICON> {
    for kind in [ICON_BIG, ICON_SMALL2, ICON_SMALL] {
        let mut icon = 0usize;
        let ok = unsafe {
            SendMessageTimeoutW(
                h,
                WM_GETICON,
                WPARAM(kind as usize),
                LPARAM(0),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                SEND_TIMEOUT_MS,
                Some(&mut icon),
            )
        };
        if ok.0 == 0 {
            // Busy or hung: don't wait on it again for the other sizes.
            break;
        }
        if icon != 0 {
            return Some(HICON(icon as *mut _));
        }
    }
    [GCLP_HICON, GCLP_HICONSM]
        .into_iter()
        .map(|index| unsafe { GetClassLongPtrW(h, index) })
        .find(|&icon| icon != 0)
        .map(|icon| HICON(icon as *mut _))
}

/// The first icon in the window's executable; the caller destroys it.
fn exe_icon(h: HWND) -> Option<HICON> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32 - 1;
        let ok = QueryFullProcessImageNameW(proc, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(proc);
        if !ok {
            return None;
        }
        buf[len as usize] = 0;
        let mut icon = HICON::default();
        let n = ExtractIconExW(PCWSTR(buf.as_ptr()), 0, Some(&mut icon), None, 1);
        (n > 0 && !icon.is_invalid()).then_some(icon)
    }
}

/// Draws the icon at SIZE x SIZE into a BGRA image with straight alpha.
///
/// The icon is drawn twice, over black and over white: the difference
/// between the two gives the alpha, which works the same for alpha-blended
/// icons and old masked ones.
fn render(icon: HICON) -> Option<RenderImage> {
    let len = (SIZE * SIZE * 4) as usize;
    let mut black = vec![0u8; len];
    let mut white = vec![0u8; len];
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: SIZE,
                // Negative height: top-down rows.
                biHeight: -SIZE,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(dc);
            return None;
        };
        let old = SelectObject(dc, bitmap.into());
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u8, len);
        let mut drawn = true;
        for (fill, out) in [(0u8, &mut black), (255u8, &mut white)] {
            pixels.fill(fill);
            drawn &= DrawIconEx(dc, 0, 0, icon, SIZE, SIZE, 0, None, DI_NORMAL).is_ok();
            let _ = GdiFlush();
            out.copy_from_slice(pixels);
        }
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        if !drawn {
            return None;
        }
    }

    let mut out = vec![0u8; len];
    let mut any = false;
    for ((o, b), w) in out.as_chunks_mut::<4>().0.iter_mut().zip(black.as_chunks::<4>().0).zip(white.as_chunks::<4>().0) {
        // Over black a pixel is c*a; over white it's c*a + (1 - a).
        let spread: u32 = b[..3].iter().zip(&w[..3]).map(|(&b, &w)| w.saturating_sub(b) as u32).sum();
        let a = 255 - (spread / 3).min(255);
        if a == 0 {
            continue;
        }
        any = true;
        for (o, &b) in o[..3].iter_mut().zip(&b[..3]) {
            *o = (b as u32 * 255 / a).min(255) as u8;
        }
        o[3] = a as u8;
    }
    if !any {
        return None;
    }
    let image = image::RgbaImage::from_raw(SIZE as u32, SIZE as u32, out)?;
    Some(RenderImage::new(vec![image::Frame::new(image)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_atlas_frees_a_gone_image_once() {
        bury(Arc::new(RenderImage::new(Vec::new())));
        for atlas in ATLASES {
            assert_eq!(take_gone(atlas).len(), 1);
            assert!(take_gone(atlas).is_empty());
        }
    }
}
