//! Window icons for labels and search results.
//!
//! An icon comes from the window (`WM_GETICON`), its class (`GCLP_HICON`) or,
//! failing both, the first icon in its executable. It's rendered once at
//! 32x32 into a BGRA image and cached per HWND on the GPUI thread; images of
//! windows that have gone away are dropped from GPUI's sprite atlas.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{RenderImage, Window};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::shared::WinInfo;

const SIZE: i32 = 32;
/// A window that had no icon is asked again after this long; new windows
/// often set theirs a moment after they appear.
const RETRY: Duration = Duration::from_secs(3);
/// Never wait long on a busy app; a hung one is skipped outright.
const SEND_TIMEOUT_MS: u32 = 40;

struct Entry {
    image: Option<Arc<RenderImage>>,
    fetched: Instant,
}

thread_local! {
    static CACHE: RefCell<HashMap<isize, Entry>> = RefCell::new(HashMap::new());
}

/// The window's icon, fetched on first use.
pub fn icon_for(hwnd: isize) -> Option<Arc<RenderImage>> {
    let cached = CACHE.with(|c| {
        let cache = c.borrow();
        let e = cache.get(&hwnd)?;
        (e.image.is_some() || e.fetched.elapsed() < RETRY).then(|| e.image.clone())
    });
    if let Some(image) = cached {
        return image;
    }
    // No borrow is held here: asking another app for its icon can pump
    // messages into our own windows.
    let image = fetch(HWND(hwnd as *mut _)).map(Arc::new);
    CACHE.with(|c| {
        c.borrow_mut().insert(hwnd, Entry { image: image.clone(), fetched: Instant::now() });
    });
    image
}

/// Forgets the icons of windows that are no longer on the canvas and frees
/// their atlas space in `window`.
pub fn prune(live: &[WinInfo], window: &mut Window) {
    let gone: Vec<Arc<RenderImage>> = CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        let dead: Vec<isize> = cache.keys().copied().filter(|id| !live.iter().any(|w| w.id == *id)).collect();
        dead.into_iter().filter_map(|id| cache.remove(&id)?.image).collect()
    });
    for image in gone {
        let _ = window.drop_image(image);
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
