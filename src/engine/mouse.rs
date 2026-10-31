//! The mouse, from any app: hold the middle button and move to pan the
//! canvas, scroll while holding it to zoom (like Blender). A middle click
//! that never moved, the side buttons and the screen's corners can each be
//! bound to an action in settings.
//!
//! A low-level mouse hook on a thread of its own sees every event first. It
//! only reads atomics and posts to the engine, so it always answers in time
//! (Windows drops hooks that are slow).

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU8, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::WM_APP_MOUSE;
use crate::settings::{GestureAction, Settings};

/// `WM_APP_MOUSE` codes, in the low byte of `wparam`; `lparam` packs the
/// point (x low, y high, signed 16 bits each).
pub const PAN_START: usize = 1;
pub const PAN_MOVE: usize = 2;
pub const PAN_END: usize = 3;
/// Wheel turned while the middle button is held; the delta (120 a notch,
/// positive away from the user) is in `wparam`'s high 16 bits.
pub const PAN_ZOOM: usize = 4;
pub const MIDDLE_CLICK: usize = 5;
pub const BACK: usize = 6;
pub const FORWARD: usize = 7;
/// Corners: top-left, top-right, bottom-left, bottom-right.
pub const CORNER: usize = 8;

/// How far (physical pixels) the cursor moves with the middle button down
/// before it's a pan rather than a click.
const SLOP: i32 = 6;
/// How close to a corner counts as in it, and how far out it re-arms.
const CORNER_IN: i32 = 2;
const CORNER_OUT: i32 = 40;

static ENGINE: AtomicIsize = AtomicIsize::new(0);
/// Not paused.
static ENABLED: AtomicBool = AtomicBool::new(false);
static MIDDLE_PAN: AtomicBool = AtomicBool::new(false);
static MIDDLE_CLICK_BOUND: AtomicBool = AtomicBool::new(false);
static BACK_BOUND: AtomicBool = AtomicBool::new(false);
static FORWARD_BOUND: AtomicBool = AtomicBool::new(false);
/// Bit per bound corner, in `CORNER` order.
static CORNERS: AtomicU8 = AtomicU8::new(0);

/// Middle button state: 0 up, 1 down (not moved yet), 2 panning.
static MIDDLE: AtomicU8 = AtomicU8::new(0);
static DOWN_X: AtomicI32 = AtomicI32::new(0);
static DOWN_Y: AtomicI32 = AtomicI32::new(0);
/// The corner the cursor is in (+1), or 0; it fires once per visit.
static IN_CORNER: AtomicU8 = AtomicU8::new(0);

/// Called whenever settings or pause change.
pub fn configure(s: &Settings, enabled: bool) {
    let bound = |a: GestureAction| a != GestureAction::Nothing;
    ENABLED.store(enabled, Ordering::Release);
    MIDDLE_PAN.store(s.middle_pan, Ordering::Release);
    MIDDLE_CLICK_BOUND.store(bound(s.middle_click), Ordering::Release);
    BACK_BOUND.store(bound(s.mouse_back), Ordering::Release);
    FORWARD_BOUND.store(bound(s.mouse_forward), Ordering::Release);
    let corners = [s.corner_tl, s.corner_tr, s.corner_bl, s.corner_br];
    let mask = corners.iter().enumerate().filter(|(_, a)| bound(**a)).fold(0u8, |m, (i, _)| m | 1 << i);
    CORNERS.store(mask, Ordering::Release);
}

/// The action a `WM_APP_MOUSE` button or corner code is bound to.
pub fn action(s: &Settings, code: usize, corner: usize) -> GestureAction {
    match code {
        MIDDLE_CLICK => s.middle_click,
        BACK => s.mouse_back,
        FORWARD => s.mouse_forward,
        CORNER => [s.corner_tl, s.corner_tr, s.corner_bl, s.corner_br].get(corner).copied().unwrap_or_default(),
        _ => GestureAction::Nothing,
    }
}

pub fn unpack(lp: LPARAM) -> (f64, f64) {
    let v = lp.0 as u32;
    ((v & 0xffff) as u16 as i16 as f64, (v >> 16) as u16 as i16 as f64)
}

fn pack(p: POINT) -> LPARAM {
    LPARAM(((p.x as i16 as u16 as u32) | ((p.y as i16 as u16 as u32) << 16)) as isize)
}

/// Starts the hook thread; it posts to `engine`.
pub fn spawn_hook(engine: HWND) {
    ENGINE.store(engine.0 as isize, Ordering::Release);
    let started = std::thread::Builder::new().name("mouse".into()).spawn(|| unsafe {
        let hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(on_mouse), None, 0) {
            Ok(h) => h,
            Err(e) => {
                log!("warning: no mouse hook (middle-drag pan, hot corners): {}", e.message());
                return;
            }
        };
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
    });
    if let Err(e) = started {
        log!("warning: no mouse thread: {e}");
    }
}

fn post(wp: usize, lp: LPARAM) {
    let hwnd = HWND(ENGINE.load(Ordering::Acquire) as *mut _);
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_APP_MOUSE, WPARAM(wp), lp);
    }
}

unsafe extern "system" fn on_mouse(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && ENABLED.load(Ordering::Acquire) {
        let m = &*(lp.0 as *const MSLLHOOKSTRUCT);
        if m.flags & LLMHF_INJECTED == 0 && handle(m, wp.0 as u32) {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

/// True swallows the event.
fn handle(m: &MSLLHOOKSTRUCT, msg: u32) -> bool {
    let p = m.pt;
    let middle = MIDDLE.load(Ordering::Acquire);
    let middle_on = MIDDLE_PAN.load(Ordering::Acquire) || MIDDLE_CLICK_BOUND.load(Ordering::Acquire);
    match msg {
        WM_MBUTTONDOWN if middle_on => {
            MIDDLE.store(1, Ordering::Release);
            DOWN_X.store(p.x, Ordering::Release);
            DOWN_Y.store(p.y, Ordering::Release);
            true
        }
        WM_MBUTTONUP if middle != 0 => {
            MIDDLE.store(0, Ordering::Release);
            if middle == 2 {
                post(PAN_END, pack(p));
            } else if MIDDLE_CLICK_BOUND.load(Ordering::Acquire) {
                post(MIDDLE_CLICK, pack(p));
            } else {
                // Nothing bound to a click: hand it to the app after all.
                replay_middle_click();
            }
            true
        }
        WM_MOUSEMOVE => {
            if middle == 1 && MIDDLE_PAN.load(Ordering::Acquire) {
                let (dx, dy) = (p.x - DOWN_X.load(Ordering::Acquire), p.y - DOWN_Y.load(Ordering::Acquire));
                if dx.abs() > SLOP || dy.abs() > SLOP {
                    MIDDLE.store(2, Ordering::Release);
                    post(PAN_START, pack(POINT { x: DOWN_X.load(Ordering::Acquire), y: DOWN_Y.load(Ordering::Acquire) }));
                    post(PAN_MOVE, pack(p));
                }
            } else if middle == 2 {
                post(PAN_MOVE, pack(p));
            } else if middle == 0 {
                hot_corner(p);
            }
            false
        }
        WM_MOUSEWHEEL if middle != 0 && MIDDLE_PAN.load(Ordering::Acquire) => {
            if middle == 1 {
                MIDDLE.store(2, Ordering::Release);
                post(PAN_START, pack(p));
            }
            let delta = (m.mouseData >> 16) as u16 as usize;
            post(PAN_ZOOM | delta << 16, pack(p));
            true
        }
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let (bound, code) = match (m.mouseData >> 16) as u16 {
                XBUTTON1 => (BACK_BOUND.load(Ordering::Acquire), BACK),
                XBUTTON2 => (FORWARD_BOUND.load(Ordering::Acquire), FORWARD),
                _ => return false,
            };
            if bound && msg == WM_XBUTTONDOWN {
                post(code, pack(p));
            }
            bound
        }
        _ => false,
    }
}

/// Fires a corner once each time the cursor pushes into it (not while a
/// button is held: that's a drag).
fn hot_corner(p: POINT) {
    let mask = CORNERS.load(Ordering::Acquire);
    if mask == 0 {
        return;
    }
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let near = |v: i32, edge: i32, d: i32| (v - edge).abs() <= d;
    let corner_at = |d: i32| -> Option<usize> {
        let left = near(p.x, 0, d);
        let right = near(p.x, w - 1, d);
        let top = near(p.y, 0, d);
        let bottom = near(p.y, h - 1, d);
        match (left, right, top, bottom) {
            (true, _, true, _) => Some(0),
            (_, true, true, _) => Some(1),
            (true, _, _, true) => Some(2),
            (_, true, _, true) => Some(3),
            _ => None,
        }
    };
    let inside = IN_CORNER.load(Ordering::Acquire);
    if inside != 0 {
        // Re-arms once the cursor has left the corner properly.
        if corner_at(CORNER_OUT) != Some(inside as usize - 1) {
            IN_CORNER.store(0, Ordering::Release);
        }
        return;
    }
    let Some(c) = corner_at(CORNER_IN) else { return };
    let held = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 || GetAsyncKeyState(VK_RBUTTON.0 as i32) < 0 };
    if mask & (1 << c) == 0 || held {
        return;
    }
    IN_CORNER.store(c as u8 + 1, Ordering::Release);
    post(CORNER | c << 8, pack(p));
}

fn replay_middle_click() {
    let ev = |flags| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dwFlags: flags, ..Default::default() } },
    };
    unsafe {
        SendInput(&[ev(MOUSEEVENTF_MIDDLEDOWN), ev(MOUSEEVENTF_MIDDLEUP)], std::mem::size_of::<INPUT>() as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_round_trip_signed() {
        for (x, y) in [(0, 0), (1919, 1079), (-1920, -5), (3839, -1080)] {
            assert_eq!(unpack(pack(POINT { x, y })), (x as f64, y as f64));
        }
    }
}
