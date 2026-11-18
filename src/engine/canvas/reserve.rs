//! The tab strip of a maximized tab group: the top of its monitor is
//! reserved for it the way the taskbar reserves its edge (an app bar), so
//! Windows itself fits every maximized window below it. Apps that insist on
//! filling the work area when maximized (Chromium ones: Chrome, Edge,
//! Discord, Spotify, VS Code) then fill it below the bar too, and nothing
//! has to be fought over.
//!
//! A strip is held only while a maximized tab group is the window in
//! front, so other maximized windows fill their monitor as ever; the space
//! is given back as soon as something else comes to the front, and on quit.
//! (The taskbar, the desktop or Apeiron's own windows coming to the front
//! change nothing: they're passing through.)

use std::cell::RefCell;

use windows::core::w;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::Shell::{SHAppBarMessage, ABE_TOP, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::Canvas;
use crate::shared::RESERVED_STRIPS;

/// App bar notifications (ignored: the strip is checked again on every
/// publish anyway).
const STRIP_MSG: u32 = WM_APP + 0x61;

/// A strip held on a monitor: the hidden window it's registered with, and
/// its top and height, physical pixels.
struct Strip {
    monitor: isize,
    hwnd: HWND,
    top: i32,
    h: i32,
}

thread_local! {
    static STRIPS: RefCell<Vec<Strip>> = const { RefCell::new(Vec::new()) };
    /// Strips a killed Apeiron left held have been given back.
    static CLEANED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The windows strips are registered with, saved so the next start can give
/// back any a killed Apeiron left held (Windows keeps the space otherwise).
fn record_path() -> std::path::PathBuf {
    crate::settings::data_dir().join("strips.txt")
}

fn save_record(strips: &[Strip]) {
    let text: String = strips.iter().map(|s| format!("{}\n", s.hwnd.0 as isize)).collect();
    let _ = std::fs::write(record_path(), text);
}

/// Gives back strips held by a previous run, once.
fn clean_up_last_run() {
    if CLEANED.replace(true) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(record_path()) else { return };
    for id in text.lines().filter_map(|l| l.trim().parse::<isize>().ok()) {
        let mut abd = APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, hWnd: HWND(id as *mut _), ..Default::default() };
        unsafe {
            SHAppBarMessage(ABM_REMOVE, &mut abd);
        }
    }
    let _ = std::fs::remove_file(record_path());
}

impl Canvas {
    /// Holds a strip on each monitor with a maximized tab group's window,
    /// and gives back the rest. Left alone while the overlay is up (windows
    /// are being moved about then).
    pub(super) fn reserve_strips(&self) {
        clean_up_last_run();
        if self.shown {
            return;
        }
        let fg = unsafe { GetForegroundWindow() };
        if passing_through(fg) {
            return;
        }
        let front = [fg.0 as isize, crate::engine::wins::root_owner(fg).0 as isize];
        let mut want: Vec<(isize, i32)> = Vec::new();
        if !self.quitting {
            for g in self.tabs.iter().filter(|g| g.split.is_none()) {
                if self.index(g.active).is_none() || !front.contains(&g.active) {
                    continue;
                }
                let h = HWND(g.active as *mut _);
                unsafe {
                    if !IsZoomed(h).as_bool() || IsIconic(h).as_bool() || !IsWindowVisible(h).as_bool() {
                        continue;
                    }
                    let m = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST).0 as isize;
                    if !want.iter().any(|w| w.0 == m) {
                        want.push((m, super::tabs::bar_height(h) as i32));
                    }
                }
            }
        }
        let changed = STRIPS.with(|s| {
            let mut s = s.borrow_mut();
            let before = s.len();
            s.retain(|st| {
                let keep = want.iter().any(|&(m, h)| m == st.monitor && h == st.h);
                if !keep {
                    release(st);
                }
                keep
            });
            let mut changed = s.len() != before;
            for &(m, h) in &want {
                if !s.iter().any(|st| st.monitor == m) {
                    if let Some(st) = hold(m, h) {
                        log!("tabs: holding a tab strip at the top of a monitor");
                        s.push(st);
                        changed = true;
                    }
                }
            }
            if changed {
                save_record(&s);
                *RESERVED_STRIPS.lock().unwrap_or_else(|e| e.into_inner()) = s.iter().map(|st| (st.monitor, st.top)).collect();
            }
            changed
        });
        if changed {
            log!("tabs: tab strips now held on {} monitor(s)", want.len());
        }
    }

    /// Gives every strip back (on quit).
    pub(super) fn release_strips(&self) {
        STRIPS.with(|s| s.borrow_mut().drain(..).for_each(|st| release(&st)));
        let _ = std::fs::remove_file(record_path());
        RESERVED_STRIPS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// Whether the window in front leaves the strips as they are: none, one of
/// Apeiron's own (a tab bar clicked, the overlay, a menu), the taskbar or
/// the desktop.
fn passing_through(fg: HWND) -> bool {
    if fg.0.is_null() {
        return true;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(fg, Some(&mut pid)) };
    if pid == std::process::id() {
        return true;
    }
    matches!(
        crate::engine::wins::class_name(fg).as_str(),
        "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "Progman" | "WorkerW" | "NotifyIconOverflowWindow" | "TopLevelWindowForOverflowXamlIsland" | "XamlExplorerHostIslandWindow"
    )
}

/// Registers a strip `h` tall at the top of monitor `m` (under the taskbar,
/// if that's at the top).
fn hold(m: isize, h: i32) -> Option<Strip> {
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe {
        if !GetMonitorInfoW(HMONITOR(m as *mut _), &mut mi).as_bool() {
            return None;
        }
        let ex = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        let hwnd = CreateWindowExW(ex, w!("STATIC"), w!("Apeiron tab strip"), WS_POPUP, 0, 0, 0, 0, None, None, None, None).ok()?;
        let mut abd = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: hwnd,
            uCallbackMessage: STRIP_MSG,
            ..Default::default()
        };
        if SHAppBarMessage(ABM_NEW, &mut abd) == 0 {
            let _ = DestroyWindow(hwnd);
            return None;
        }
        let r = mi.rcMonitor;
        abd.uEdge = ABE_TOP;
        abd.rc = RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + h };
        SHAppBarMessage(ABM_QUERYPOS, &mut abd);
        abd.rc.bottom = abd.rc.top + h;
        SHAppBarMessage(ABM_SETPOS, &mut abd);
        let rc = abd.rc;
        let _ = SetWindowPos(hwnd, None, rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top, SWP_NOACTIVATE | SWP_NOZORDER);
        Some(Strip { monitor: m, hwnd, top: rc.top, h })
    }
}

fn release(st: &Strip) {
    let mut abd = APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, hWnd: st.hwnd, ..Default::default() };
    unsafe {
        SHAppBarMessage(ABM_REMOVE, &mut abd);
        let _ = DestroyWindow(st.hwnd);
    }
}
