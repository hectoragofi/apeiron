//! Tab bars while idle: each group's bar is a small window of its own,
//! sitting flush on top of what's drawn of its group's window and exactly as
//! wide, kept just above that window in the z-order, so it reads as part of
//! it and other windows cover it as they'd cover the window. (The canvas
//! desktop is under every window; in tabs-only mode it's not there at all.)
//!
//! The bar isn't made an owned window of its group's window, though Windows
//! would then keep it above that window and minimize it along: an owner in
//! another process attaches the two threads' input queues, so one hung app
//! would stall the input of this thread (every canvas window) and, through
//! it, of every other app with tabs, and the app finds our window made its
//! active one. So it's restacked here instead, on every focus change and
//! every move of its window (`UiEvent::TabsMoved`), reading where the window
//! is right now rather than where the last snapshot saw it.
//!
//! A maximized group's window is left alone, filling its screen: its bar
//! tucks away, a small handle at the top edge showing it's there, and comes
//! down over the top of the app while the mouse rests on the handle (or it's
//! clicked), until the mouse leaves it (macOS's full screen toolbar).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use gpui::*;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateEllipticRgn, CreateRectRgn, CreateRoundRectRgn, DeleteObject, GetMonitorInfoW, MonitorFromRect,
    SetWindowRgn, HRGN, MONITORINFO, MONITOR_DEFAULTTONEAREST, RGN_AND, RGN_DIFF, RGN_OR,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::labels::{self, BAR_RADIUS, STRIP_H};
use super::motion::{fade, NORMAL};
use crate::shared::{EngineHandle, Mode, Shared, Snapshot};

/// The handle a tucked bar leaves at the top of the screen, logical px, and
/// how wide the band along the top edge that brings the bar is.
const HANDLE_W: f64 = 54.0;
const HANDLE_H: f64 = 5.0;
const BAND_W: f64 = 360.0;
/// Its bar's corners, coming down as a floating capsule.
const OPEN_RADIUS: f64 = 10.0;
/// Resting this long on the band brings the bar (a flick up to the app's
/// own tabs doesn't); it goes this long after the mouse leaves it, which
/// may stray this far off it (logical px). The mouse is looked at this often.
const REVEAL_AFTER: Duration = Duration::from_millis(120);
const HIDE_AFTER: Duration = Duration::from_millis(300);
const REVEAL_SLACK: f64 = 14.0;
const HOVER_TICK: Duration = Duration::from_millis(30);
/// Coming down: fades in while sliding the last few logical px into place,
/// animated by GPUI inside the bar's own window (the window itself doesn't
/// move). Going away is immediate.
const SHOW_IN: Duration = Duration::from_millis(180);
const SHOW_OUT: Duration = Duration::from_millis(0);
const DROP: f32 = 8.0;

/// The group (by shown window) whose tucked bar is coming down (`show`) or
/// going away, since `at`, from `from` of the way shown.
#[derive(Clone, Copy)]
struct Reveal {
    id: isize,
    show: bool,
    at: Instant,
    from: f64,
    /// Which coming-down this is (restarts its animation).
    n: u64,
}

/// How many times a tucked bar has come down.
static SHOWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Reveal {
    fn new(id: isize) -> Reveal {
        let n = SHOWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Reveal { id, show: true, at: Instant::now(), from: 0.0, n }
    }
}

impl Reveal {
    /// How far shown, 0..=1, eased.
    fn amount(&self) -> f64 {
        let d = if self.show { SHOW_IN } else { SHOW_OUT };
        let t = (self.at.elapsed().as_secs_f64() / d.as_secs_f64()).min(1.0);
        if self.show {
            self.from + (1.0 - self.from) * (1.0 - (1.0 - t).powi(3))
        } else {
            self.from * (1.0 - t * t * t)
        }
    }

    fn moving(&self) -> bool {
        self.at.elapsed() < if self.show { SHOW_IN } else { SHOW_OUT }
    }

    fn to(self, show: bool) -> Reveal {
        Reveal { id: self.id, show, at: Instant::now(), from: self.amount(), n: self.n }
    }
}

/// A tucked bar: the band along the top edge that brings it, and where it
/// comes down, screen pixels.
struct Tucked {
    id: isize,
    band: (f64, f64, f64, f64),
    strip: (f64, f64, f64, f64),
}

/// The tucked bars as last placed, and the one the mouse brought down.
static TUCKED: Mutex<Vec<Tucked>> = Mutex::new(Vec::new());
static REVEAL: Mutex<Option<Reveal>> = Mutex::new(None);

/// Which coming-down of group `id`'s tucked bar is out, if it is (groups by
/// `labels::group_key`, which a tab switch doesn't change).
fn revealed(id: isize) -> Option<u64> {
    REVEAL.lock().unwrap_or_else(|e| e.into_inner()).filter(|r| r.id == id && r.show).map(|r| r.n)
}

/// The handle was clicked: down it comes now.
fn reveal_now(id: isize) {
    let mut r = REVEAL.lock().unwrap_or_else(|e| e.into_inner());
    if !r.is_some_and(|v| v.id == id && v.show) {
        *r = Some(Reveal::new(id));
    }
}

fn inside(p: (f64, f64), r: (f64, f64, f64, f64)) -> bool {
    p.0 >= r.0 && p.0 < r.0 + r.2 && p.1 >= r.1 && p.1 < r.1 + r.3
}

/// Watches the mouse for tucked bars on a thread of its own: resting on the
/// band over a handle brings its bar, leaving the bar puts it away again.
/// Asks for a redraw when that changes, and every frame while it moves.
pub fn watch_hover(shared: Shared) {
    let spawned = std::thread::Builder::new().name("strip-hover".into()).spawn(move || {
        let mut candidate: Option<(isize, Instant)> = None;
        let mut leaving: Option<Instant> = None;
        loop {
            std::thread::sleep(HOVER_TICK);
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
                    inside(p, (x - slack, y - 2.0 * slack, w + 2.0 * slack, h + 3.0 * slack)) || inside(p, z.band)
                })
            };
            let changed = match *reveal {
                Some(r) if r.show => {
                    if near(r.id) || (held && r.amount() > 0.5) {
                        leaving = None;
                        false
                    } else if leaving.get_or_insert_with(Instant::now).elapsed() >= HIDE_AFTER {
                        *reveal = None;
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
                            *reveal = Some(Reveal::new(id));
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
                shared.redraw();
            }
        }
    });
    if let Err(e) = spawned {
        log!("couldn't start the tab bar hover watch: {e}");
    }
}

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
        let Some(group) = snap.tabs.get(self.slot) else { return div().size_full() };
        let scale = window.scale_factor();
        let width = f32::from(window.viewport_size().width);
        let id = labels::group_key(group) as isize;
        if tucks(group) {
            // Come down: the bar fades in, sliding the last few px into place.
            if let Some(n) = revealed(id) {
                let bar = labels::strip_bar(&snap, group, width, &self.engine, scale, self.origin.get())
                    .absolute()
                    .left_0()
                    .top_0()
                    .size_full();
                let anim = Animation::new(SHOW_IN).with_easing(ease_out_quint());
                let bar = bar.with_animation(("reveal", super::motion::key(id, n)), anim, |el, t| {
                    el.opacity(t).top(px(-DROP * (1. - t)))
                });
                return div().size_full().relative().child(bar);
            }
            // Tucked: the handle (all that shows of this window), which also
            // brings the bar when clicked.
            let color = if labels::light_theme() { rgba(0x0000004d) } else { rgba(0xffffff4d) };
            let shared = self.shared.clone();
            let pill = div()
                .absolute()
                .top_0()
                .left(px(((width - HANDLE_W as f32) / 2.).max(0.)))
                .w(px(HANDLE_W as f32))
                .h(px(HANDLE_H as f32))
                .rounded_full()
                .bg(color);
            return div().size_full().child(pill).on_mouse_down(MouseButton::Left, move |_, _, _| {
                reveal_now(id);
                shared.redraw();
            });
        }
        // The bar's colour reaches below it, into the window's top corners.
        let root = div().size_full().bg(labels::bar_bg());
        let bar = labels::strip_bar(&snap, group, width, &self.engine, scale, self.origin.get());
        root.child(fade(bar, ("stripwin", labels::group_key(group)), NORMAL, 1.))
    }
}

/// One bar window, and where it was last put.
pub struct Strip {
    pub handle: WindowHandle<StripView>,
    pub hwnd: isize,
    origin: Rc<Cell<(f64, f64)>>,
    /// Rect of the bar and its corner fill (this thread's coordinates).
    placed: Option<(i32, i32, i32, i32)>,
    /// How far the client area sits into the window, from the last full place.
    inset: (i32, i32),
    /// What its region was last cut for.
    kind: Kind,
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
                ..Default::default()
            },
            move |_, cx| cx.new(|_| StripView { shared, engine, slot, origin: o }),
        )
        .map_err(|e| log!("couldn't open a tab bar window: {e}"))
        .ok()?;
    let hwnd = handle
        .update(cx, |_, window, cx| {
            window.on_window_should_close(cx, |_, _| false);
            super::hwnd_of(window)
        })
        .ok()??;
    restyle(HWND(hwnd as *mut _));
    Some(Strip { handle, hwnd, origin, placed: None, inset: (0, 0), kind: Kind::Attached, shown: false })
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

/// How many bar windows the snapshot needs (0 while the overlay is up).
pub fn wanted(snap: &Snapshot) -> usize {
    if snap.overlay_visible || snap.paused || snap.mode != Mode::Idle {
        0
    } else {
        snap.tabs.len()
    }
}

/// A bar on screen, this thread's pixels: `fill` more rows below it fill
/// the window's rounded top corners (0 when it isn't on the window).
#[derive(Clone, Copy, PartialEq, Debug)]
struct Bar {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    radius: i32,
    fill: i32,
    kind: Kind,
}

/// On top of its window, a maximized one's handle, or its bar come down.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Attached,
    /// Only a pill this big shows, centred at the top of the bar's window
    /// (which stays the bar's size, so it never has to be resized).
    Handle { w: i32, h: i32 },
    Open,
}

/// Whether group `g`'s bar tucks away: its window is maximized.
fn tucks(g: &crate::shared::TabsSnap) -> bool {
    g.split.is_none() && unsafe { IsZoomed(HWND(g.active as *mut _)) }.as_bool()
}

/// Puts each bar on top of its group's window, hides the spare ones.
/// Returns the bars that changed size (they need drawing again). Runs
/// outside any GPUI update (moving a window re-enters GPUI).
pub fn place_all(strips: &mut [Strip], snap: &Snapshot) -> Vec<isize> {
    let want = wanted(snap);
    let (sw, _) = super::screen_size();
    let k = sw as f64 / snap.screen_w.max(1.0);
    let mut resized = Vec::new();
    let mut tucked = Vec::new();
    for (i, strip) in strips.iter_mut().enumerate() {
        let hwnd = HWND(strip.hwnd as *mut _);
        let bar = if i < want { group_bar(snap, i, &mut tucked) } else { None };
        let Some(b) = bar else {
            if strip.shown {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                strip.shown = false;
            }
            continue;
        };
        strip.origin.set((b.x as f64 / k, b.y as f64 / k));
        let active = HWND(snap.tabs[i].active as *mut _);
        let r = (b.x, b.y, b.w, b.h + b.fill);
        unsafe {
            // Just above its window: under whatever is above that.
            let above = GetWindow(active, GW_HWNDPREV).ok();
            let order = match above {
                Some(a) if a == hwnd => GetWindow(hwnd, GW_HWNDPREV).unwrap_or(HWND_TOP),
                Some(a) => a,
                None => HWND_TOP,
            };
            let same_size = strip.placed.is_some_and(|p| (p.2, p.3) == (r.2, r.3)) && strip.kind == b.kind;
            if strip.placed != Some(r) && same_size {
                // Only moved (its window dragged): no refit, same region.
                let (l, t) = strip.inset;
                let _ = SetWindowPos(hwnd, Some(order), r.0 - l, r.1 - t, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
                strip.placed = Some(r);
            } else if strip.placed != Some(r) {
                let (l, t) = super::place(hwnd, order, r.0, r.1, r.2, r.3);
                let rgn = match b.kind {
                    Kind::Attached => bar_region(l, t, b),
                    Kind::Handle { w, h } => {
                        let x = l + (b.w - w) / 2;
                        CreateRoundRectRgn(x, t, x + w + 1, t + h + 1, h, h)
                    }
                    Kind::Open => CreateRoundRectRgn(l, t, l + b.w + 1, t + b.h + 1, 2 * b.radius, 2 * b.radius),
                };
                let _ = SetWindowRgn(hwnd, Some(rgn), true);
                strip.placed = Some(r);
                strip.kind = b.kind;
                strip.inset = (l, t);
                resized.push(strip.hwnd);
            } else if above != Some(hwnd) {
                let _ = SetWindowPos(hwnd, Some(order), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
            if !strip.shown {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                strip.shown = true;
            }
        }
    }
    let mut r = REVEAL.lock().unwrap_or_else(|e| e.into_inner());
    if r.is_some_and(|v| (!v.show && !v.moving()) || !tucked.iter().any(|z: &Tucked| z.id == v.id)) {
        *r = None;
    }
    drop(r);
    *TUCKED.lock().unwrap_or_else(|e| e.into_inner()) = tucked;
    resized
}

/// Group `i`'s bar: on top of its shown window (both, when split) as the
/// window is right now.
fn group_bar(snap: &Snapshot, i: usize, tucked: &mut Vec<Tucked>) -> Option<Bar> {
    let g = &snap.tabs[i];
    if !snap.windows.iter().any(|w| w.id == g.active) {
        return None;
    }
    let mut f = live_frame(g.active)?;
    if let Some(o) = g.split.and_then(live_frame) {
        f = RECT { left: f.left.min(o.left), top: f.top.min(o.top), right: f.right.max(o.right), bottom: f.bottom.max(o.bottom) };
    }
    let s = dpi_scale(g.active);
    if tucks(g) {
        return Some(tucked_bar(f, s, labels::group_key(g) as isize, g.tabs.len(), tucked));
    }
    bar_on(f, s, true)
}

/// A maximized group's bar: its handle at the top of the window, or (while
/// the mouse brings it) the bar come down over the window, centred.
fn tucked_bar(f: RECT, s: f64, id: isize, n: usize, tucked: &mut Vec<Tucked>) -> Bar {
    let (l, top, w) = (f.left as f64, f.top as f64, (f.right - f.left) as f64);
    let strip = labels::tucked_bar_rect(l, top, w, n, s);
    let cx = l + w / 2.0;
    let band = (cx - BAND_W * s / 2.0, top - 4.0 * s, BAND_W * s, 10.0 * s);
    tucked.push(Tucked { id, band, strip });
    match revealed(id) {
        Some(_) => Bar {
            x: strip.0 as i32,
            y: strip.1 as i32,
            w: strip.2 as i32,
            h: strip.3 as i32,
            radius: (OPEN_RADIUS * s).round() as i32,
            fill: 0,
            kind: Kind::Open,
        },
        None => {
            let (hw, hh) = ((HANDLE_W * s).round() as i32, (HANDLE_H * s).round() as i32);
            Bar {
                x: strip.0 as i32,
                y: (top + 3.0 * s).round() as i32,
                w: strip.2 as i32,
                h: strip.3 as i32,
                radius: hh / 2,
                fill: 0,
                kind: Kind::Handle { w: hw, h: hh },
            }
        }
    }
}

/// What's drawn of window `id` on screen (its frame without the invisible
/// borders), if it's showing.
fn live_frame(id: isize) -> Option<RECT> {
    let h = HWND(id as *mut _);
    unsafe {
        if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
            return None;
        }
        let mut f = RECT::default();
        let size = std::mem::size_of::<RECT>() as u32;
        if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut f as *mut RECT as *mut _, size).is_err() {
            GetWindowRect(h, &mut f).ok()?;
        }
        Some(f)
    }
}

fn dpi_scale(id: isize) -> f64 {
    match unsafe { GetDpiForWindow(HWND(id as *mut _)) } {
        0 => super::primary_dpi() as f64 / 96.0,
        dpi => dpi as f64 / 96.0,
    }
}

/// The bar for a window drawn at `f`: right on top of it, unless its top is
/// on a monitor with no room above it (the engine makes room as soon as it
/// can), then at the top of that monitor's work area, over the window.
/// `fill`: reach into the window's rounded corners.
fn bar_on(f: RECT, s: f64, fill: bool) -> Option<Bar> {
    bar_at(f, s, work_top(&f), fill)
}

/// `bar_on`, the monitor's work area starting at `top`.
fn bar_at(f: RECT, s: f64, top: i32, fill: bool) -> Option<Bar> {
    let (w, h) = (f.right - f.left, (STRIP_H as f64 * s).round() as i32);
    if (w as f64) < 60.0 * s {
        return None;
    }
    let y = if f.top >= top { (f.top - h).max(top) } else { f.top - h };
    let radius = (BAR_RADIUS as f64 * s).round() as i32;
    let fill = if fill && y + h == f.top { radius } else { 0 };
    Some(Bar { x: f.left, y, w, h, radius, fill, kind: Kind::Attached })
}

/// The top of the work area of the monitor showing `r`.
fn work_top(r: &RECT) -> i32 {
    unsafe {
        let m = MonitorFromRect(r, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(m, &mut info).as_bool() {
            info.rcWork.top
        } else {
            0
        }
    }
}

/// The bar's shape, its client area at `l`,`t` in its window: rounded at
/// the top, square below, plus the bits outside the window's rounded top
/// corners when it sits on one, so bar and window meet with no gap.
fn bar_region(l: i32, t: i32, b: Bar) -> HRGN {
    let (w, h, r) = (b.w, b.h, b.radius);
    unsafe {
        let rgn = CreateRoundRectRgn(l, t, l + w + 1, t + h + 2 * r + 1, 2 * r, 2 * r);
        let cut = CreateRectRgn(l, t, l + w, t + h);
        let _ = CombineRgn(Some(rgn), Some(rgn), Some(cut), RGN_AND);
        let _ = DeleteObject(cut.into());
        if b.fill > 0 {
            let y = t + h;
            for (x0, ex) in [(l, l), (l + w - r, l + w - 2 * r)] {
                let corner = CreateRectRgn(x0, y, x0 + r, y + r);
                let round = CreateEllipticRgn(ex, y, ex + 2 * r + 1, y + 2 * r + 1);
                let _ = CombineRgn(Some(corner), Some(corner), Some(round), RGN_DIFF);
                let _ = CombineRgn(Some(rgn), Some(rgn), Some(corner), RGN_OR);
                let _ = DeleteObject(round.into());
                let _ = DeleteObject(corner.into());
            }
        }
        rgn
    }
}

/// "Add as tab", where the bar of the window a dragged one would join goes.
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
    placed: Option<Bar>,
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
    let bar = snap.tab_drop.filter(|_| hint_wanted(snap)).and_then(|id| {
        // On a group, over its bar (both halves when split).
        let g = snap.tabs.iter().find(|g| g.active == id);
        let mut f = live_frame(id)?;
        if let Some(o) = g.and_then(|g| g.split).and_then(live_frame) {
            f = RECT { left: f.left.min(o.left), top: f.top.min(o.top), right: f.right.max(o.right), bottom: f.bottom };
        }
        bar_on(f, dpi_scale(id), false)
    });
    unsafe {
        let Some(b) = bar else {
            if hint.shown {
                let _ = ShowWindow(hwnd, SW_HIDE);
                hint.shown = false;
            }
            return;
        };
        if hint.placed != Some(b) {
            let (l, t) = super::place(hwnd, HWND_TOPMOST, b.x, b.y, b.w, b.h);
            let _ = SetWindowRgn(hwnd, Some(bar_region(l, t, b)), true);
            hint.placed = Some(b);
        }
        if !hint.shown {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            hint.shown = true;
        }
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's own `test` attribute would shadow the standard one.
    use super::{bar_at, Bar, RECT};

    fn frame(l: i32, t: i32, w: i32, h: i32) -> RECT {
        RECT { left: l, top: t, right: l + w, bottom: t + h }
    }

    #[test]
    fn bar_sits_right_on_top_of_its_window() {
        // 125%: 40 px tall, as wide as the window, its corners filled 10 px.
        let b = bar_at(frame(100, 300, 800, 600), 1.25, 0, true).unwrap();
        assert_eq!(b, Bar { x: 100, y: 260, w: 800, h: 40, radius: 10, fill: 10, kind: super::Kind::Attached });
        // Filling the screen below the bar: the bar is at the very top.
        let b = bar_at(frame(0, 40, 1920, 980), 1.25, 0, true).unwrap();
        assert_eq!((b.y, b.fill), (0, 10));
    }

    #[test]
    fn bar_without_room_stays_on_its_monitor() {
        // Snapped to the top: over the window, not off the screen.
        let b = bar_at(frame(0, 0, 960, 1000), 1.25, 0, true).unwrap();
        assert_eq!((b.y, b.fill), (0, 0));
        // A window hanging off the top (the canvas put it there) keeps its bar on it.
        let b = bar_at(frame(0, -200, 960, 1000), 1.25, 0, true).unwrap();
        assert_eq!(b.y, -240);
        // Too narrow for tabs.
        assert!(bar_at(frame(0, 300, 50, 400), 1.0, 0, true).is_none());
    }
}
