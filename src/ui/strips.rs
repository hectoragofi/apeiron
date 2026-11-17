//! Tab strips while idle: each group's strip is a small window of its own,
//! kept just above its group's window in the z-order, so it reads as part
//! of it and other windows cover it as they'd cover the window. (The
//! canvas desktop is under every window; in tabs-only mode it's not there
//! at all.) A window with no room above it (maximized) keeps its strip
//! out of sight, so it never covers the app's own title bar (Terminal,
//! browsers, Discord draw tabs there): resting the mouse on the top edge of
//! the screen over that window brings it down, centred in the title bar,
//! always in the same spot, until the mouse leaves it.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use gpui::*;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
};
use windows::Win32::Graphics::Gdi::{
    CreateRoundRectRgn, GetMonitorInfoW, MonitorFromPoint, SetWindowRgn, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::labels::{self, STRIP_GAP, STRIP_H};
use super::motion::{fade, NORMAL};
use crate::shared::{EngineHandle, Mode, Shared, Snapshot};

/// Widest a tab gets in a strip inside a title bar, and the room left
/// either side of it for the app's own title bar controls, logical px.
const INSET_TAB_W: f64 = 190.0;
const INSET_SIDES: f64 = 160.0;
/// Tucked strips: how long the mouse rests on the top edge before one comes
/// down (a quick flick up to the app's own tabs doesn't bring it), how long
/// it stays once the mouse leaves, how far off it the mouse may stray
/// (logical px), and how often the mouse is looked at.
const REVEAL_AFTER: Duration = Duration::from_millis(250);
const HIDE_AFTER: Duration = Duration::from_millis(350);
const REVEAL_SLACK: f64 = 12.0;
const HOVER_TICK: Duration = Duration::from_millis(30);
/// Coming down: fade in while dropping the last few px into place, eased
/// out; going away is quicker and eased in. Frames are paced at ~144 Hz.
const SHOW_IN: Duration = Duration::from_millis(220);
const SHOW_OUT: Duration = Duration::from_millis(150);
const DROP: f64 = 10.0;
const ANIM_TICK: Duration = Duration::from_millis(7);

/// The group (by active window) whose tucked strip is coming down
/// (`show`) or going away, since `at`, from `from` of the way shown.
#[derive(Clone, Copy)]
struct Reveal {
    id: isize,
    show: bool,
    at: Instant,
    from: f64,
}

impl Reveal {
    /// How far shown, 0..=1, eased.
    fn amount(&self) -> f64 {
        let d = if self.show { SHOW_IN } else { SHOW_OUT };
        let t = (self.at.elapsed().as_secs_f64() / d.as_secs_f64()).min(1.0);
        if self.show {
            let e = 1.0 - (1.0 - t).powi(3);
            self.from + (1.0 - self.from) * e
        } else {
            self.from * (1.0 - t * t * t)
        }
    }

    fn moving(&self) -> bool {
        self.at.elapsed() < if self.show { SHOW_IN } else { SHOW_OUT }
    }

    fn to(self, show: bool) -> Reveal {
        Reveal { id: self.id, show, at: Instant::now(), from: self.amount() }
    }
}

/// How far group `id`'s tucked strip is shown, if it's tucked and moving
/// or out.
fn reveal_of(id: isize) -> Option<f64> {
    REVEAL.lock().unwrap_or_else(|e| e.into_inner()).filter(|r| r.id == id).map(|r| r.amount())
}

/// A tucked strip: the top-edge band that brings it down, and where it
/// shows, screen engine pixels.
struct Tucked {
    id: isize,
    band: (f64, f64, f64, f64),
    strip: (f64, f64, f64, f64),
}

/// The tucked strips as last placed, and the group (by active window) whose
/// strip the mouse has brought down.
static TUCKED: Mutex<Vec<Tucked>> = Mutex::new(Vec::new());
static REVEAL: Mutex<Option<Reveal>> = Mutex::new(None);

pub struct StripView {
    shared: Shared,
    engine: EngineHandle,
    slot: usize,
    /// Where the window sits on screen, engine (physical) pixels.
    origin: Rc<Cell<(f64, f64)>>,
}

impl Render for StripView {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let snap = self.shared.snap();
        let root = div().size_full();
        let Some(group) = snap.tabs.get(self.slot) else { return root };
        let scale = window.scale_factor();
        let width = f32::from(window.viewport_size().width);
        let bar = labels::strip_bar(&snap, group, width, &self.engine, scale, self.origin.get()).size_full();
        // A tucked strip fades with its drop, frame by frame.
        if let Some(a) = reveal_of(group.active) {
            return root.child(bar.opacity(a as f32));
        }
        root.child(fade(bar, ("stripwin", labels::group_key(group)), NORMAL, 1.))
    }
}

/// One strip window, and where it was last put.
pub struct Strip {
    pub handle: WindowHandle<StripView>,
    pub hwnd: isize,
    origin: Rc<Cell<(f64, f64)>>,
    /// Rect (this thread's coordinates) and the window it was put above.
    placed: Option<(i32, i32, i32, i32)>,
    /// How far the client area sits into the window, from the last full place.
    inset: (i32, i32),
    shown: bool,
}

pub fn open(cx: &mut App, shared: &Shared, engine: &EngineHandle, slot: usize) -> Option<Strip> {
    let bounds = Bounds::new(point(px(0.), px(0.)), size(px(200.), px(STRIP_H)));
    let origin = Rc::new(Cell::new((0.0, 0.0)));
    let (shared, engine, o) = (shared.clone(), engine.clone(), origin.clone());
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                kind: WindowKind::PopUp,
                focus: false,
                show: false,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                window_background: WindowBackgroundAppearance::Blurred,
                ..Default::default()
            },
            move |_, cx| cx.new(|_| StripView { shared, engine, slot, origin: o }),
        )
        .map_err(|e| log!("couldn't open a tab strip window: {e}"))
        .ok()?;
    let hwnd = handle
        .update(cx, |_, window, cx| {
            window.on_window_should_close(cx, |_, _| false);
            super::hwnd_of(window)
        })
        .ok()??;
    restyle(HWND(hwnd as *mut _));
    Some(Strip { handle, hwnd, origin, placed: None, inset: (0, 0), shown: false })
}

/// Borderless, never activating, not in Alt+Tab, hidden.
pub(super) fn restyle(hwnd: HWND) {
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_STYLE, WS_POPUP.0 as isize);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (ex | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) as isize);
        let square = DWMWCP_DONOTROUND;
        let none = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &square as *const _ as *const _, 4);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &none as *const _ as *const _, 4);
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

/// How many strip windows the snapshot needs (0 while the overlay is up).
pub fn wanted(snap: &Snapshot) -> usize {
    if snap.overlay_visible || snap.paused || snap.mode != Mode::Idle {
        0
    } else {
        snap.tabs.len()
    }
}

/// Puts each strip above its group's window, hides the spare ones. Runs
/// outside any GPUI update (moving a window re-enters GPUI).
pub fn place_all(strips: &mut [Strip], snap: &Snapshot) {
    let want = wanted(snap);
    let (sw, _) = super::screen_size();
    let k = sw as f64 / snap.screen_w.max(1.0);
    let s = super::primary_dpi() as f64 / 96.0;
    let mut tucked = Vec::new();
    for (i, strip) in strips.iter_mut().enumerate() {
        let hwnd = HWND(strip.hwnd as *mut _);
        let rect = if i < want { strip_rect(snap, i, s, &mut tucked) } else { None };
        let Some((x, y, w, h)) = rect else {
            if strip.shown {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                strip.shown = false;
            }
            continue;
        };
        strip.origin.set((x, y));
        let group = &snap.tabs[i];
        let active = HWND(group.active as *mut _);
        // Just above its window: under whatever is above that.
        let above = unsafe { GetWindow(active, GW_HWNDPREV) }.ok();
        let r = ((x * k).round() as i32, (y * k).round() as i32, (w * k).round() as i32, (h * k).round() as i32);
        unsafe {
            let same_size = strip.placed.is_some_and(|p| (p.2, p.3) == (r.2, r.3));
            if strip.placed != Some(r) && same_size && strip.shown {
                // Only moved (a tucked strip dropping in): no refit, same region.
                let (l, t) = strip.inset;
                let flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
                let _ = SetWindowPos(hwnd, None, r.0 - l, r.1 - t, 0, 0, flags);
                strip.placed = Some(r);
            } else if strip.placed != Some(r) {
                let order = match above {
                    Some(a) if a != hwnd => a,
                    _ => HWND_TOP,
                };
                let (l, t) = super::place(hwnd, order, r.0, r.1, r.2, r.3);
                let rgn = CreateRoundRectRgn(l, t, l + r.2 + 1, t + r.3 + 1, r.3, r.3);
                let _ = SetWindowRgn(hwnd, Some(rgn), true);
                strip.placed = Some(r);
                strip.inset = (l, t);
            } else if let Some(a) = above.filter(|&a| a != hwnd) {
                let _ = SetWindowPos(hwnd, Some(a), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
            if !strip.shown {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                strip.shown = true;
            }
        }
    }
    // Same lock order as the hover thread: tucked, then reveal.
    let mut zones = TUCKED.lock().unwrap_or_else(|e| e.into_inner());
    let mut r = REVEAL.lock().unwrap_or_else(|e| e.into_inner());
    if r.is_some_and(|v| (!v.show && !v.moving()) || !tucked.iter().any(|z: &Tucked| z.id == v.id)) {
        *r = None;
    }
    *zones = tucked;
}

/// Group `i`'s strip on screen `(x, y, w, h)`, engine pixels: floating
/// above its window, or (no room above) tucked away and noted in `tucked`,
/// shown centred in its title bar only while the mouse brings it, dropping
/// into place as it comes.
fn strip_rect(snap: &Snapshot, i: usize, s: f64, tucked: &mut Vec<Tucked>) -> Option<(f64, f64, f64, f64)> {
    let g = &snap.tabs[i];
    if !snap.windows.iter().any(|w| w.id == g.active) {
        return None;
    }
    match spot_for(snap, g.spot, s)? {
        Place::Above(r) => Some(r),
        Place::Inset { l, t, w, top } => {
            // In the visible part of the title bar (a maximized window's
            // frame hangs off the top of the screen).
            let strip = centred(l, t.max(top), w, g.tabs.len(), s);
            // The cursor stops at the screen's top edge; a few pixels of it
            // over this window bring the strip.
            tucked.push(Tucked { id: g.active, band: (l, top - 2.0 * s, w, 4.0 * s), strip });
            let a = reveal_of(g.active).filter(|&a| a > 0.001)?;
            let (x, y, w, h) = strip;
            Some((x, y - DROP * s * (1.0 - a), w, h))
        }
    }
}

enum Place {
    Above((f64, f64, f64, f64)),
    /// Window at `(l, t, w)` on screen, its monitor's work area from `top`.
    Inset { l: f64, t: f64, w: f64, top: f64 },
}

/// Watches the mouse for tucked strips on a thread of its own: resting on
/// the top edge over a tucked group brings its strip, leaving the strip
/// puts it away again. Asks for a redraw when that changes.
pub fn watch_hover(shared: Shared) {
    let spawned = std::thread::Builder::new().name("strip-hover".into()).spawn(move || {
        let mut candidate: Option<(isize, Instant)> = None;
        let mut leaving: Option<Instant> = None;
        let mut animating = false;
        loop {
            // Frame-paced while a strip moves, so every frame gets placed.
            std::thread::sleep(if animating { ANIM_TICK } else { HOVER_TICK });
            if animating {
                animating = REVEAL.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|r| r.moving());
                // One more after the end lands it exactly.
                shared.redraw();
            }
            let zones = TUCKED.lock().unwrap_or_else(|e| e.into_inner());
            let mut reveal = REVEAL.lock().unwrap_or_else(|e| e.into_inner());
            if zones.is_empty() {
                candidate = None;
                continue;
            }
            let mut p = POINT::default();
            if unsafe { GetPhysicalCursorPos(&mut p) }.is_err() {
                continue;
            }
            let p = (p.x as f64, p.y as f64);
            // A held button is a drag (the app's own tabs, a window): leave it be.
            let held = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0;
            let slack = REVEAL_SLACK * super::primary_dpi() as f64 / 96.0;
            let near = |id: isize| {
                zones.iter().find(|z| z.id == id).is_some_and(|z| {
                    let (x, y, w, h) = z.strip;
                    inside(p, (x - slack, y - slack, w + 2.0 * slack, h + 2.0 * slack)) || inside(p, z.band)
                })
            };
            let changed = match *reveal {
                Some(r) if r.show => {
                    if near(r.id) {
                        leaving = None;
                        false
                    } else if leaving.get_or_insert_with(Instant::now).elapsed() >= HIDE_AFTER {
                        *reveal = Some(r.to(false));
                        leaving = None;
                        true
                    } else {
                        false
                    }
                }
                // Going away, and the mouse came back onto it: back it comes.
                Some(r) if near(r.id) && r.amount() > 0.0 => {
                    *reveal = Some(r.to(true));
                    leaving = None;
                    true
                }
                _ => match zones.iter().find(|z| !held && inside(p, z.band)) {
                    Some(z) => match candidate {
                        Some((id, since)) if id == z.id && since.elapsed() >= REVEAL_AFTER => {
                            *reveal = Some(match *reveal {
                                Some(r) if r.id == id => r.to(true),
                                _ => Reveal { id, show: true, at: Instant::now(), from: 0.0 },
                            });
                            candidate = None;
                            leaving = None;
                            true
                        }
                        Some((id, _)) if id == z.id => false,
                        _ => {
                            candidate = Some((z.id, Instant::now()));
                            false
                        }
                    },
                    None => {
                        candidate = None;
                        false
                    }
                },
            };
            drop((zones, reveal));
            if changed {
                animating = true;
                shared.redraw();
            }
        }
    });
    if let Err(e) = spawned {
        log!("couldn't start the tab strip hover watch: {e}");
    }
}

fn inside(p: (f64, f64), r: (f64, f64, f64, f64)) -> bool {
    p.0 >= r.0 && p.0 < r.0 + r.2 && p.1 >= r.1 && p.1 < r.1 + r.3
}

/// Where a strip for the canvas rect `spot` goes: above it on screen, or
/// (no room above) inside the title bar of the window at `(l, t, w)`.
fn spot_for(snap: &Snapshot, spot: (f64, f64, f64, f64), s: f64) -> Option<Place> {
    let (x, y, w, _) = spot;
    let (l, t, w) = ((x - snap.cam_x) * snap.zoom, (y - snap.cam_y) * snap.zoom, w * snap.zoom);
    let (h, gap) = (STRIP_H as f64 * s, STRIP_GAP as f64 * s);
    if w < 60.0 * s {
        return None;
    }
    let top = work_top((l + w / 2.0, t));
    if t - gap - h >= top {
        return Some(Place::Above((l, t - gap - h, w, h)));
    }
    Some(Place::Inset { l, t, w, top })
}

/// A strip of `n` tabs for the canvas rect `spot`, on screen; centred in
/// the title bar when it can't go above (for the short-lived drop hint).
fn rect_for(snap: &Snapshot, spot: (f64, f64, f64, f64), n: usize, s: f64) -> Option<(f64, f64, f64, f64)> {
    match spot_for(snap, spot, s)? {
        Place::Above(r) => Some(r),
        Place::Inset { l, t, w, .. } => Some(centred(l, t, w, n, s)),
    }
}

fn centred(l: f64, t: f64, w: f64, n: usize, s: f64) -> (f64, f64, f64, f64) {
    let n = n.max(1) as f64;
    let room = (w - 2.0 * INSET_SIDES * s).max(w * 0.5);
    let sw = (n * INSET_TAB_W * s).min(room).max((120.0 * s).min(w));
    (l + (w - sw) / 2.0, t + 3.0 * s, sw, STRIP_H as f64 * s)
}

/// The top of the work area of the monitor at `p` (engine pixels; on a
/// DPI-virtualized thread close enough for this test).
fn work_top(p: (f64, f64)) -> f64 {
    unsafe {
        let m = MonitorFromPoint(POINT { x: p.0 as i32, y: p.1.max(0.0) as i32 }, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(m, &mut info).as_bool() {
            let RECT { top, .. } = info.rcWork;
            top as f64
        } else {
            0.0
        }
    }
}

/// "Add as tab", over the title bar a window being dragged would join.
pub struct HintView {
    shared: Shared,
}

impl Render for HintView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let snap = self.shared.snap();
        let root = div().size_full();
        let Some(id) = snap.tab_drop else { return root };
        root.child(fade(labels::add_tab_capsule().size_full(), ("hint", id as u64), super::motion::FAST, 1.))
    }
}

pub struct Hint {
    pub handle: WindowHandle<HintView>,
    pub hwnd: isize,
    placed: Option<(i32, i32, i32, i32)>,
    shown: bool,
}

pub fn open_hint(cx: &mut App, shared: &Shared) -> Option<Hint> {
    let bounds = Bounds::new(point(px(0.), px(0.)), size(px(200.), px(STRIP_H)));
    let shared = shared.clone();
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                kind: WindowKind::PopUp,
                focus: false,
                show: false,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                window_background: WindowBackgroundAppearance::Blurred,
                ..Default::default()
            },
            move |_, cx| cx.new(|_| HintView { shared }),
        )
        .map_err(|e| log!("couldn't open the tab hint window: {e}"))
        .ok()?;
    let hwnd = handle
        .update(cx, |_, window, cx| {
            window.on_window_should_close(cx, |_, _| false);
            super::hwnd_of(window)
        })
        .ok()??;
    let h = HWND(hwnd as *mut _);
    restyle(h);
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
        // Above the window being dragged, and never in the way of the mouse.
        SetWindowLongPtrW(h, GWL_EXSTYLE, (ex | WS_EX_TOPMOST.0 | WS_EX_TRANSPARENT.0 | WS_EX_LAYERED.0) as isize);
        let _ = SetLayeredWindowAttributes(h, Default::default(), 255, LWA_ALPHA);
    }
    Some(Hint { handle, hwnd, placed: None, shown: false })
}

/// Whether the hint is wanted: a real window dragged onto a title bar.
pub fn hint_wanted(snap: &Snapshot) -> bool {
    snap.tab_drop.is_some() && !snap.overlay_visible
}

pub fn place_hint(hint: &mut Hint, snap: &Snapshot) {
    let hwnd = HWND(hint.hwnd as *mut _);
    let (sw, _) = super::screen_size();
    let k = sw as f64 / snap.screen_w.max(1.0);
    let s = super::primary_dpi() as f64 / 96.0;
    let rect = snap.tab_drop.filter(|_| hint_wanted(snap)).and_then(|id| {
        let w = snap.windows.iter().find(|w| w.id == id)?;
        let (spot, n) = match snap.tabs.iter().find(|g| g.active == id) {
            Some(g) => (g.spot, g.tabs.len() + 1),
            None => ((w.x, w.y, w.w, w.h), 2),
        };
        rect_for(snap, spot, n, s)
    });
    unsafe {
        let Some((x, y, w, h)) = rect else {
            if hint.shown {
                let _ = ShowWindow(hwnd, SW_HIDE);
                hint.shown = false;
            }
            return;
        };
        let r = ((x * k).round() as i32, (y * k).round() as i32, (w * k).round() as i32, (h * k).round() as i32);
        if hint.placed != Some(r) {
            let (l, t) = super::place(hwnd, HWND_TOPMOST, r.0, r.1, r.2, r.3);
            let rgn = CreateRoundRectRgn(l, t, l + r.2 + 1, t + r.3 + 1, r.3, r.3);
            let _ = SetWindowRgn(hwnd, Some(rgn), true);
            hint.placed = Some(r);
        }
        if !hint.shown {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            hint.shown = true;
        }
    }
}
