//! Finding the top-level windows that live on the canvas, describing them,
//! and pulling stranded windows back on screen.

use std::collections::HashSet;
use std::path::PathBuf;

use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HDC, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::settings::data_dir;

/// Visible, app-like top-level windows, topmost first.
pub fn app_windows(include_maximized: bool) -> Vec<HWND> {
    top_level()
        .into_iter()
        .filter(|&h| is_app_window(h) && (include_maximized || !unsafe { IsZoomed(h) }.as_bool()))
        .collect()
}

/// Minimized app windows, topmost first. They aren't on the canvas.
pub fn minimized_windows() -> Vec<HWND> {
    top_level()
        .into_iter()
        .filter(|&h| unsafe { IsWindowVisible(h).as_bool() && IsIconic(h).as_bool() } && !is_cloaked(h) && is_app_like(h))
        .collect()
}

fn top_level() -> Vec<HWND> {
    let mut all: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut _ as isize));
    }
    all
}

unsafe extern "system" fn collect(h: HWND, lp: LPARAM) -> BOOL {
    let v = &mut *(lp.0 as *mut Vec<HWND>);
    v.push(h);
    true.into()
}

/// On screen right now: visible, not minimized, on this virtual desktop.
fn is_app_window(h: HWND) -> bool {
    let shown = unsafe { IsWindowVisible(h).as_bool() && !IsIconic(h).as_bool() };
    shown && !is_cloaked(h) && is_app_like(h)
}

/// An app's main window (whatever its current state), not a tool window,
/// popup, shell window or one of ours.
fn is_app_like(h: HWND) -> bool {
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
        // Owned windows are dialogs and popups, except (as in Alt+Tab) one
        // marked as an app window, or one whose owner is a hidden helper:
        // some toolkits give every main window one. Topmost ones are
        // launchers and pop-ups.
        if let Ok(owner) = GetWindow(h, GW_OWNER) {
            let popup = IsWindowVisible(owner).as_bool() || ex & WS_EX_TOPMOST.0 != 0;
            if !owner.is_invalid() && ex & WS_EX_APPWINDOW.0 == 0 && popup {
                return false;
            }
        }
        if ex & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) != 0 {
            return false;
        }
        if GetWindowTextLengthW(h) == 0 {
            return false;
        }
        if is_ours(h) {
            return false;
        }
        !matches!(
            class_name(h).as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        )
    }
}

fn is_ours(h: HWND) -> bool {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(h, Some(&mut pid));
        pid == GetCurrentProcessId()
    }
}

fn is_cloaked(h: HWND) -> bool {
    let mut cloaked = 0u32;
    unsafe {
        let _ = DwmGetWindowAttribute(
            h,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        );
    }
    cloaked != 0
}

/// The app has stopped answering messages; anything synchronous would hang.
pub fn is_hung(h: HWND) -> bool {
    unsafe { IsHungAppWindow(h).as_bool() }
}

pub fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(h, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

pub fn title(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(h, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

/// Lowercase executable file name of the window's process; empty when the
/// process can't be opened (elevated apps).
pub fn exe_name(h: HWND) -> String {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        let Ok(proc) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(proc, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(proc);
        if !ok {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\').next().unwrap_or("").to_lowercase()
    }
}

/// Turns a maximized window into a normal one that fills the same work
/// area, so it can move around the canvas without visibly changing size.
/// Both steps are queued to the app's thread, so a busy app never stalls
/// the engine; returns the rect the window is on its way to.
pub fn unmaximize(h: HWND) -> Option<RECT> {
    unsafe {
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
            return None;
        }
        let work = mi.rcWork;
        if !ShowWindowAsync(h, SW_SHOWNOACTIVATE).as_bool() {
            return None;
        }
        let (w, hgt) = (work.right - work.left, work.bottom - work.top);
        let flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
        let _ = SetWindowPos(h, None, work.left, work.top, w, hgt, flags);
        Some(work)
    }
}

/// Restores a minimized window (queued to its thread). If it would come back
/// somewhere the user can't reach, it's also queued a move onto the screen.
pub fn restore(h: HWND) {
    unsafe {
        let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        let stranded = GetWindowPlacement(h, &mut wp).is_ok()
            && !wp.flags.contains(WPF_RESTORETOMAXIMIZED)
            && !reachable(h, &workspace_to_screen(wp.rcNormalPosition));
        let _ = ShowWindowAsync(h, SW_RESTORE);
        if stranded {
            send_home(h, 0, false);
        }
    }
}

/// Placement rects are relative to the primary work area.
fn workspace_to_screen(r: RECT) -> RECT {
    let work = primary_work();
    RECT { left: r.left + work.left, top: r.top + work.top, right: r.right + work.left, bottom: r.bottom + work.top }
}

/// How far the window rect `r` reaches past what's drawn on each side
/// (left, top, right, bottom): the invisible resize borders.
pub fn frame_inset(h: HWND, r: &RECT) -> (f64, f64, f64, f64) {
    let mut f = RECT::default();
    let ok = unsafe {
        DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut f as *mut RECT as *mut _, std::mem::size_of::<RECT>() as u32)
    }
    .is_ok();
    let d = |v: i32| (v.clamp(0, 32)) as f64;
    if !ok || f.right <= f.left {
        return (0.0, 0.0, 0.0, 0.0);
    }
    (d(f.left - r.left), d(f.top - r.top), d(r.right - f.right), d(r.bottom - f.bottom))
}

/// Whether Windows acts on four-finger swipes itself (Settings > Bluetooth
/// & devices > Touchpad > Four-finger gestures; on unless set to Nothing).
/// Read at each swipe, so changing it needs no restart.
pub fn os_four_finger() -> bool {
    !windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\PrecisionTouchPad")
        .and_then(|k| k.get_u32("FourFingerSlideEnabled"))
        .is_ok_and(|v| v == 0)
}

/// Switches to the next virtual desktop to the right (or left), as
/// Ctrl+Win+Right (Left) does.
pub fn switch_desktop(right: bool) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_LEFT, VK_LWIN, VK_RIGHT};
    send_keys(&[VK_CONTROL, VK_LWIN], if right { VK_RIGHT } else { VK_LEFT });
}

/// Presses `key` with the `mods` held, as if typed.
pub fn send_keys(mods: &[windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY], key: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY) {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let input = |vk: VIRTUAL_KEY, up: bool| {
        let mut flags = if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
        // Arrows are extended keys; without the flag they're the keypad's.
        if matches!(vk, VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN) {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, dwFlags: flags, ..Default::default() } } }
    };
    let mut inputs: Vec<INPUT> = mods.iter().map(|&m| input(m, false)).collect();
    inputs.push(input(key, false));
    inputs.push(input(key, true));
    inputs.extend(mods.iter().rev().map(|&m| input(m, true)));
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        log!("couldn't send keys ({sent} of {} sent)", inputs.len());
    }
}

pub fn window_rect(h: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r).ok()? };
    Some(r)
}

/// Where the window's client area starts inside its window rect.
pub fn client_offset(h: HWND) -> (i32, i32) {
    let mut origin = POINT::default();
    match window_rect(h) {
        Some(r) if unsafe { ClientToScreen(h, &mut origin) }.as_bool() => (origin.x - r.left, origin.y - r.top),
        _ => (0, 0),
    }
}

// ---- reachability and rescue ------------------------------------------------

/// Title bar height and the caption buttons' width at 96 DPI: a window can
/// be dragged back by the part of its title bar left of the buttons.
const TITLE_H: i32 = 30;
const BUTTONS_W: i32 = 140;
/// How much of that grab strip must be on a monitor, at 96 DPI.
const GRAB_W: i32 = 100;
const GRAB_H: i32 = 8;

/// Each monitor's full rect and work area, physical pixels.
fn monitors() -> Vec<(RECT, RECT)> {
    unsafe extern "system" fn add(m: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        let v = &mut *(lp.0 as *mut Vec<(RECT, RECT)>);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(m, &mut mi).as_bool() {
            v.push((mi.rcMonitor, mi.rcWork));
        }
        true.into()
    }
    let mut v: Vec<(RECT, RECT)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(add), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

pub fn primary_work() -> RECT {
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe {
        let m = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        if GetMonitorInfoW(m, &mut mi).as_bool() {
            return mi.rcWork;
        }
        RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) }
    }
}

/// Whether window `h` at `r` could be dragged back with the mouse: enough
/// of its title bar (left of the caption buttons) lies on some monitor's
/// work area, or it covers a whole monitor (fullscreen apps).
pub fn reachable(h: HWND, r: &RECT) -> bool {
    reachable_on(r, dpi_of(h), &monitors())
}

fn dpi_of(h: HWND) -> f64 {
    match unsafe { GetDpiForWindow(h) } {
        0 => 1.0,
        dpi => dpi as f64 / 96.0,
    }
}

fn reachable_on(r: &RECT, scale: f64, mons: &[(RECT, RECT)]) -> bool {
    let s = |v: i32| (v as f64 * scale).round() as i32;
    let grab_right = (r.right - s(BUTTONS_W)).max(r.left + s(GRAB_W));
    let strip = RECT { left: r.left, top: r.top, right: grab_right.min(r.right), bottom: r.top + s(TITLE_H) };
    let need_w = s(GRAB_W).min(strip.right - strip.left);
    mons.iter().any(|(mon, work)| {
        let covers = r.left <= mon.left && r.top <= mon.top && r.right >= mon.right && r.bottom >= mon.bottom;
        let w = strip.right.min(work.right) - strip.left.max(work.left);
        let h = strip.bottom.min(work.bottom) - strip.top.max(work.top);
        covers || (w >= need_w && h >= s(GRAB_H).min(strip.bottom - strip.top))
    })
}

/// Where the `n`th rescued window goes: cascaded from the top-left of the
/// primary work area.
fn rescue_spot(n: usize) -> (i32, i32) {
    let work = primary_work();
    let off = 40 + 32 * (n as i32 % 12);
    (work.left + off, work.top + off)
}

/// Moves `h` to the `n`th rescue spot. Unless `wait`, the move is queued to
/// the app, so it lands after any move already queued there.
pub fn send_home(h: HWND, n: usize, wait: bool) {
    let (x, y) = rescue_spot(n);
    let mut flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
    if !wait {
        flags |= SWP_ASYNCWINDOWPOS;
    }
    unsafe {
        let _ = SetWindowPos(h, None, x, y, 0, 0, flags);
    }
}

/// Brings every window the user couldn't reach back onto the primary
/// monitor: on-screen app windows, minimized and cloaked ones (judged by
/// where they'd restore to), and anything we recorded parking off-screen,
/// even if it's hidden now. Stateless apart from that record, so the
/// watchdog and the `rescue` CLI can run it after we're gone.
pub fn rescue_all() -> usize {
    let parked = load_parked();
    let mons = monitors();
    let mut n = 0;
    for h in top_level() {
        if is_ours(h) {
            continue;
        }
        let recorded = parked.contains(&(h.0 as isize));
        let candidate = recorded
            || (unsafe { IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() } && is_app_like(h));
        if candidate && rescue_one(h, n, &mons) {
            n += 1;
        }
    }
    n
}

fn rescue_one(h: HWND, n: usize, mons: &[(RECT, RECT)]) -> bool {
    let scale = dpi_of(h);
    unsafe {
        if IsIconic(h).as_bool() {
            // Judge (and fix) where it will restore to. Placement is in
            // workspace coordinates, relative to the primary work area.
            let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
            if GetWindowPlacement(h, &mut wp).is_err() {
                return false;
            }
            let work = primary_work();
            let r = wp.rcNormalPosition;
            if reachable_on(&workspace_to_screen(r), scale, mons) || is_hung(h) {
                return false;
            }
            let (x, y) = rescue_spot(n);
            let (x, y) = (x - work.left, y - work.top);
            wp.rcNormalPosition = RECT { left: x, top: y, right: x + r.right - r.left, bottom: y + r.bottom - r.top };
            wp.showCmd = SW_SHOWMINNOACTIVE.0 as u32;
            return SetWindowPlacement(h, &wp).is_ok();
        }
        let Some(r) = window_rect(h) else { return false };
        if reachable_on(&r, scale, mons) {
            return false;
        }
        // No SWP_SHOWWINDOW: a hidden window stays hidden, just reachable.
        send_home(h, n, false);
        true
    }
}

// ---- the parked record ----------------------------------------------------------

fn parked_path() -> PathBuf {
    data_dir().join("parked.txt")
}

/// HWNDs we last left somewhere unreachable, one per line.
fn load_parked() -> HashSet<isize> {
    std::fs::read_to_string(parked_path())
        .map(|s| s.lines().filter_map(|l| l.trim().parse().ok()).collect())
        .unwrap_or_default()
}

/// Records the windows we've parked off-screen, so a rescue finds them even
/// once they're hidden (apps that close to the tray).
pub fn save_parked(parked: &HashSet<isize>) {
    let path = parked_path();
    let live: Vec<String> = parked
        .iter()
        .filter(|&&id| unsafe { IsWindow(Some(HWND(id as *mut _))).as_bool() })
        .map(|id| id.to_string())
        .collect();
    let result = if live.is_empty() {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    } else {
        std::fs::write(&path, live.join("\n"))
    };
    if let Err(e) = result {
        log!("couldn't update {}: {e}", path.display());
    }
}

pub fn clear_parked() {
    save_parked(&HashSet::new());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(l: i32, t: i32, w: i32, h: i32) -> RECT {
        RECT { left: l, top: t, right: l + w, bottom: t + h }
    }

    /// 1920x1080 primary with a taskbar, plus a monitor above-right of it.
    fn mons() -> Vec<(RECT, RECT)> {
        vec![
            (rect(0, 0, 1920, 1080), rect(0, 0, 1920, 1032)),
            (rect(1920, -1080, 1920, 1080), rect(1920, -1080, 1920, 1080)),
        ]
    }

    #[test]
    fn on_screen_windows_are_reachable() {
        assert!(reachable_on(&rect(100, 100, 800, 600), 1.0, &mons()));
        // Maximized: the invisible borders hang off every edge.
        assert!(reachable_on(&rect(-8, -8, 1936, 1056), 1.0, &mons()));
        // Fullscreen covers the taskbar too.
        assert!(reachable_on(&rect(0, 0, 1920, 1080), 1.0, &mons()));
        assert!(reachable_on(&rect(2000, -900, 800, 600), 1.0, &mons()));
    }

    #[test]
    fn title_bar_off_screen_is_unreachable() {
        // Client area peeks in from the top, caption above the screen.
        assert!(!reachable_on(&rect(200, -560, 800, 600), 1.0, &mons()));
        // Off the left edge with only the caption buttons showing.
        assert!(!reachable_on(&rect(-700, 100, 800, 600), 1.0, &mons()));
        // In the bounding box of both monitors but on neither.
        assert!(!reachable_on(&rect(2000, 200, 800, 600), 1.0, &mons()));
        assert!(!reachable_on(&rect(-12000, 300, 800, 600), 1.0, &mons()));
    }

    #[test]
    fn grab_strip_scales_with_dpi() {
        // 150 px of title bar showing: enough at 100%, not at 200%.
        let r = rect(1920 - 150, 100, 800, 600);
        assert!(reachable_on(&r, 1.0, &mons()[..1]));
        assert!(!reachable_on(&r, 2.0, &mons()[..1]));
    }
}
