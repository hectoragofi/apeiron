//! Tab strips while idle: each group's strip is a small window of its own,
//! kept just above its group's window in the z-order, so it reads as part
//! of it and other windows cover it as they'd cover the window. (The
//! canvas desktop is under every window; in tabs-only mode it's not there
//! at all.) A window with no room above it (maximized) gets the strip
//! inside its title bar, in the widest stretch the app itself leaves empty
//! (asked with `WM_NCHITTEST`), so it never covers the app's own tabs or
//! buttons (Terminal, browsers, Discord draw theirs there).

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::*;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
};
use windows::Win32::Graphics::Gdi::{
    CreateRoundRectRgn, GetMonitorInfoW, MonitorFromPoint, ScreenToClient, SetWindowRgn, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::labels::{self, STRIP_GAP, STRIP_H};
use super::motion::{fade, NORMAL};
use crate::shared::{EngineHandle, Mode, Shared, Snapshot};

/// Widest a tab gets in a strip inside a title bar, and the room left
/// either side of it for the app's own title bar controls, logical px.
const INSET_TAB_W: f64 = 190.0;
const INSET_SIDES: f64 = 160.0;
/// Narrowest an icons-only tab gets, the strip's own padding, and the gap
/// kept to the app's controls either side, logical px.
const INSET_ICON_W: f64 = 32.0;
const INSET_PAD: f64 = 6.0;
const INSET_MARGIN: f64 = 8.0;
/// Title bar probing: spacing of the hit tests (logical px), how long one
/// may take, and how often an unchanged title bar is asked again (an app
/// grows its own tabs without moving).
const PROBE_STEP: f64 = 10.0;
const PROBE_TIMEOUT_MS: u32 = 20;
const PROBE_EVERY: Duration = Duration::from_millis(750);

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
    shown: bool,
    /// Last look at the free room in its window's title bar.
    probe: Option<Probe>,
}

/// What the title bar of window `key.0` at `(left, top, width)` had free.
struct Probe {
    key: (isize, i32, i32, i32),
    at: Instant,
    free: Free,
}

#[derive(Clone, Copy)]
enum Free {
    /// The app didn't answer: fall back to centring.
    Unknown,
    /// The widest empty stretch, screen x from..to (engine pixels).
    Span(f64, f64),
    /// No empty stretch at all.
    None,
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
    Some(Strip { handle, hwnd, origin, placed: None, shown: false, probe: None })
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
    for (i, strip) in strips.iter_mut().enumerate() {
        let hwnd = HWND(strip.hwnd as *mut _);
        let rect = if i < want { strip_rect(snap, i, s, &mut strip.probe) } else { None };
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
            if strip.placed != Some(r) {
                let order = match above {
                    Some(a) if a != hwnd => a,
                    _ => HWND_TOP,
                };
                let (l, t) = super::place(hwnd, order, r.0, r.1, r.2, r.3);
                let rgn = CreateRoundRectRgn(l, t, l + r.2 + 1, t + r.3 + 1, r.3, r.3);
                let _ = SetWindowRgn(hwnd, Some(rgn), true);
                strip.placed = Some(r);
            } else if let Some(a) = above.filter(|&a| a != hwnd) {
                let _ = SetWindowPos(hwnd, Some(a), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
            if !strip.shown {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                strip.shown = true;
            }
        }
    }
}

/// Group `i`'s strip on screen `(x, y, w, h)`, engine pixels: floating
/// above its window, or inside its title bar when there's no room above,
/// where the app leaves it empty.
fn strip_rect(snap: &Snapshot, i: usize, s: f64, probe: &mut Option<Probe>) -> Option<(f64, f64, f64, f64)> {
    let g = &snap.tabs[i];
    if !snap.windows.iter().any(|w| w.id == g.active) {
        return None;
    }
    let (l, t, w) = match spot_for(snap, g.spot, s)? {
        Place::Above(r) => {
            *probe = None;
            return Some(r);
        }
        Place::Inset { l, t, w } => (l, t, w),
    };
    let (h, n) = (STRIP_H as f64 * s, g.tabs.len().max(1) as f64);
    let y = t + 3.0 * s;
    let key = (g.active, l.round() as i32, t.round() as i32, w.round() as i32);
    let fresh = probe.as_ref().is_some_and(|p| p.key == key && p.at.elapsed() < PROBE_EVERY);
    if !fresh {
        let free = free_span(HWND(g.active as *mut _), l, w, y + h / 2.0, s);
        *probe = Some(Probe { key, at: Instant::now(), free });
    }
    match probe.as_ref().map_or(Free::Unknown, |p| p.free) {
        Free::Unknown => Some(centred(l, t, w, g.tabs.len(), s)),
        Free::None => None,
        Free::Span(a, b) => {
            let margin = INSET_MARGIN * s;
            let room = b - a - 2.0 * margin;
            if room < (n * INSET_ICON_W + INSET_PAD) * s {
                return None;
            }
            // Against the right end: apps grow their own tabs from the
            // left, and the caption buttons sit right.
            let sw = (n * INSET_TAB_W * s).min(room);
            Some((b - margin - sw, y, sw, h))
        }
    }
}

enum Place {
    Above((f64, f64, f64, f64)),
    Inset { l: f64, t: f64, w: f64 },
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
    if t - gap - h >= work_top((l + w / 2.0, t)) {
        return Some(Place::Above((l, t - gap - h, w, h)));
    }
    Some(Place::Inset { l, t, w })
}

/// A strip of `n` tabs for the canvas rect `spot`, on screen; centred in
/// the title bar when it can't go above (for the short-lived drop hint).
fn rect_for(snap: &Snapshot, spot: (f64, f64, f64, f64), n: usize, s: f64) -> Option<(f64, f64, f64, f64)> {
    match spot_for(snap, spot, s)? {
        Place::Above(r) => Some(r),
        Place::Inset { l, t, w } => Some(centred(l, t, w, n, s)),
    }
}

fn centred(l: f64, t: f64, w: f64, n: usize, s: f64) -> (f64, f64, f64, f64) {
    let n = n.max(1) as f64;
    let room = (w - 2.0 * INSET_SIDES * s).max(w * 0.5);
    let sw = (n * INSET_TAB_W * s).min(room).max((120.0 * s).min(w));
    (l + (w - sw) / 2.0, t + 3.0 * s, sw, STRIP_H as f64 * s)
}

/// The widest run of plain title bar (`HTCAPTION`: empty, draggable) along
/// screen row `y` of `hwnd` between `l` and `l + w`, engine pixels.
fn free_span(hwnd: HWND, l: f64, w: f64, y: f64, s: f64) -> Free {
    let step = (PROBE_STEP * s).max(4.0);
    let (mut best, mut run): (Option<(f64, f64)>, Option<f64>) = (None, None);
    let mut answered = true;
    // Engine pixels are physical: hit test as a per-monitor aware thread.
    let old = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let mut x = l + step / 2.0;
    while x < l + w {
        let Some(ht) = nc_hit(hwnd, x, y) else {
            answered = false;
            break;
        };
        if ht == HTCAPTION {
            run.get_or_insert(x);
        } else if let Some(a) = run.take() {
            best = wider(best, (a - step / 2.0, x - step / 2.0));
        }
        x += step;
    }
    if let Some(a) = run {
        best = wider(best, (a - step / 2.0, l + w));
    }
    unsafe {
        if !old.is_invalid() {
            SetThreadDpiAwarenessContext(old);
        }
    }
    match (answered, best) {
        (false, _) => Free::Unknown,
        (true, Some((a, b))) => Free::Span(a, b),
        (true, None) => Free::None,
    }
}

fn wider(best: Option<(f64, f64)>, r: (f64, f64)) -> Option<(f64, f64)> {
    match best {
        Some(b) if b.1 - b.0 >= r.1 - r.0 => Some(b),
        _ => Some(r),
    }
}

/// `WM_NCHITTEST` at screen point `(x, y)`, asked of the deepest visible
/// child of `h` there (Terminal's tabs are a child over a frame that says
/// caption everywhere); None if the app doesn't answer in time.
fn nc_hit(mut h: HWND, x: f64, y: f64) -> Option<u32> {
    let (x, y) = (x.round() as i32, y.round() as i32);
    for _ in 0..16 {
        let mut p = POINT { x, y };
        let c = unsafe {
            if !ScreenToClient(h, &mut p).as_bool() {
                break;
            }
            ChildWindowFromPointEx(h, p, CWP_SKIPINVISIBLE | CWP_SKIPTRANSPARENT)
        };
        if c.is_invalid() || c == h {
            break;
        }
        h = c;
    }
    let lp = (x as i16 as u16 as u32) | ((y as i16 as u16 as u32) << 16);
    let mut out = 0usize;
    let ok = unsafe {
        SendMessageTimeoutW(
            h,
            WM_NCHITTEST,
            WPARAM(0),
            LPARAM(lp as i32 as isize),
            SMTO_ABORTIFHUNG,
            PROBE_TIMEOUT_MS,
            Some(&mut out),
        )
    };
    (ok.0 != 0).then_some(out as i16 as i32 as u32)
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
