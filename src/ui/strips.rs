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

use std::cell::Cell;
use std::rc::Rc;

use gpui::*;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateEllipticRgn, CreateRectRgn, CreateRoundRectRgn, DeleteObject, GetMonitorInfoW, MonitorFromRect,
    SetWindowRgn, HRGN, MONITORINFO, MONITOR_DEFAULTTONEAREST, RGN_AND, RGN_DIFF, RGN_OR,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::labels::{self, BAR_RADIUS, STRIP_H};
use super::motion::{fade, NORMAL};
use crate::shared::{EngineHandle, Mode, Shared, Snapshot};

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
        // The bar's colour reaches below it, into the window's top corners.
        let root = div().size_full().bg(labels::bar_bg());
        let Some(group) = snap.tabs.get(self.slot) else { return root };
        let scale = window.scale_factor();
        let width = f32::from(window.viewport_size().width);
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
}

/// Puts each bar on top of its group's window, hides the spare ones.
/// Returns the bars that changed size (they need drawing again). Runs
/// outside any GPUI update (moving a window re-enters GPUI).
pub fn place_all(strips: &mut [Strip], snap: &Snapshot) -> Vec<isize> {
    let want = wanted(snap);
    let (sw, _) = super::screen_size();
    let k = sw as f64 / snap.screen_w.max(1.0);
    let mut resized = Vec::new();
    for (i, strip) in strips.iter_mut().enumerate() {
        let hwnd = HWND(strip.hwnd as *mut _);
        let bar = if i < want { group_bar(snap, i) } else { None };
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
            let same_size = strip.placed.is_some_and(|p| (p.2, p.3) == (r.2, r.3));
            if strip.placed != Some(r) && same_size {
                // Only moved (its window dragged): no refit, same region.
                let (l, t) = strip.inset;
                let _ = SetWindowPos(hwnd, Some(order), r.0 - l, r.1 - t, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
                strip.placed = Some(r);
            } else if strip.placed != Some(r) {
                let (l, t) = super::place(hwnd, order, r.0, r.1, r.2, r.3);
                let _ = SetWindowRgn(hwnd, Some(bar_region(l, t, b)), true);
                strip.placed = Some(r);
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
    resized
}

/// Group `i`'s bar: on top of its shown window (both, when split) as the
/// window is right now.
fn group_bar(snap: &Snapshot, i: usize) -> Option<Bar> {
    let g = &snap.tabs[i];
    if !snap.windows.iter().any(|w| w.id == g.active) {
        return None;
    }
    let mut f = live_frame(g.active)?;
    if let Some(o) = g.split.and_then(live_frame) {
        f = RECT { left: f.left.min(o.left), top: f.top.min(o.top), right: f.right.max(o.right), bottom: f.bottom.max(o.bottom) };
    }
    bar_on(f, dpi_scale(g.active), true)
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
    Some(Bar { x: f.left, y, w, h, radius, fill })
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
        assert_eq!(b, Bar { x: 100, y: 260, w: 800, h: 40, radius: 10, fill: 10 });
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
