//! Finding the top-level windows that live on the canvas, and pulling
//! stranded windows back on screen.

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Visible, app-like top-level windows, topmost first.
pub fn app_windows(include_maximized: bool) -> Vec<HWND> {
    let mut all: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut _ as isize));
    }
    all.into_iter()
        .filter(|&h| is_app_window(h, include_maximized))
        .collect()
}

unsafe extern "system" fn collect(h: HWND, lp: LPARAM) -> BOOL {
    let v = &mut *(lp.0 as *mut Vec<HWND>);
    v.push(h);
    true.into()
}

fn is_app_window(h: HWND, include_maximized: bool) -> bool {
    unsafe {
        if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
            return false;
        }
        if !include_maximized && IsZoomed(h).as_bool() {
            return false;
        }
        if GetWindow(h, GW_OWNER).map(|o| !o.is_invalid()).unwrap_or(false) {
            return false;
        }
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
        if ex & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) != 0 {
            return false;
        }
        if GetWindowTextLengthW(h) == 0 {
            return false;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(
            h,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == GetCurrentProcessId() {
            return false;
        }
        let mut buf = [0u16; 64];
        let n = GetClassNameW(h, &mut buf) as usize;
        let class = String::from_utf16_lossy(&buf[..n]);
        !matches!(
            class.as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        )
    }
}

pub fn window_rect(h: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r).ok()? };
    Some(r)
}

/// Moves every app window that sits entirely outside the virtual screen
/// back onto the primary monitor. Stateless, so the watchdog can run it
/// after the canvas process has died.
pub fn rescue_all() -> usize {
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let mut n = 0;
    for h in app_windows(true) {
        let Some(r) = window_rect(h) else { continue };
        let visible = r.right > vx + 40 && r.left < vx + vw - 40 && r.bottom > vy + 40 && r.top < vy + vh - 40;
        if visible {
            continue;
        }
        let off = 40 + 32 * (n as i32 % 12);
        unsafe {
            let _ = SetWindowPos(h, None, off, off, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS);
        }
        n += 1;
    }
    n
}
