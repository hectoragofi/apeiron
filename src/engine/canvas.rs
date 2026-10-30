//! The canvas: every app window has a position on an unbounded plane and the
//! screen is a camera onto it.
//!
//! Real windows can't be scaled and repaint late when moved, so all motion
//! happens on a fullscreen overlay that shows live DWM thumbnails. At zoom 1
//! the thumbnails line up pixel-for-pixel with the real windows, so a
//! gesture looks like the windows themselves gliding. When motion stops at
//! zoom 1 the real windows are moved to match and the overlay disappears.
//!
//! This module only models and moves things; the GPUI thread draws the grid,
//! labels and so on from the `Snapshot` published by [`Canvas::flush`].

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::touchpad::Contact;
use super::{tidy, wins, HOTKEY_ESC};
use crate::layout::{Layout, WinKey};
use crate::settings::Settings;
use crate::shared::{Command, Mode, SearchState, Shared, Snapshot, UiEvent, WinInfo};
use crate::ui::{labels, search};

pub const MIN_ZOOM: f64 = 0.04;
pub const MAX_ZOOM: f64 = 1.0;
/// A gesture that ends this close to zoom 1 snaps back to real windows.
const SNAP: f64 = 0.15;
/// Finger travel (px) before three fingers count as a gesture, not a tap.
const START_SLOP: f64 = 6.0;
const ANIM_SECS: f64 = 0.32;
/// Pinch must change finger spread by this much (log ratio) before zoom
/// engages, so a plain pan never wobbles the zoom.
const PINCH_LOCK: f64 = 0.12;
/// Wheel (two-finger scroll / pinch) gestures end after this much quiet.
const WHEEL_IDLE_MS: u32 = 140;
/// Windows also turns touchpad motion into coarse, laggy wheel events; those
/// are ignored for this long after the last precision-touchpad frame.
const WHEEL_QUIET_SECS: f64 = 1.5;
/// How long the overlay stays up after real windows are moved, so they have
/// time to land before it disappears.
const HIDE_DELAY_MS: u32 = 70;
const SAVE_DELAY_MS: u32 = 1000;
/// Thumbnail opacity of windows that don't match the search.
const DIMMED: u8 = 70;
/// Space kept around a search result the camera frames, physical pixels.
const FRAME_MARGIN: f64 = 48.0;

pub const TIMER_ANIM: usize = 1;
pub const TIMER_HIDE: usize = 2;
pub const TIMER_WHEEL: usize = 3;
pub const TIMER_SAVE: usize = 4;

struct Win {
    hwnd: HWND,
    title: String,
    exe: String,
    class: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    /// Where we last put the real window; if it's still there, our canvas
    /// position is authoritative (the window may be clamped off-screen).
    placed: Option<RECT>,
    thumb: isize,
}

impl Win {
    fn id(&self) -> isize {
        self.hwnd.0 as isize
    }

    fn key(&self) -> WinKey {
        WinKey { exe: self.exe.clone(), class: self.class.clone(), title: self.title.clone() }
    }
}

/// A window gliding between canvas positions: (id, from, to).
type WinMove = (isize, (f64, f64), (f64, f64));

struct Anim {
    from: (f64, f64, f64),
    to: (f64, f64, f64),
    start: Instant,
    /// Hand control back to real windows when done.
    commit: bool,
    focus: Option<HWND>,
    /// Windows that move on the canvas during the animation (tidy).
    moves: Vec<WinMove>,
}

struct Drag {
    target: Option<usize>,
    last: (f64, f64),
    moved: f64,
}

#[derive(Default)]
struct Gesture {
    prev: Option<(usize, f64, f64, f64)>,
    active: bool,
    travel: f64,
    last: Option<Instant>,
    vel: (f64, f64),
    /// Finger spread when the gesture started; zoom stays locked until the
    /// spread departs from it by PINCH_LOCK.
    spread0: f64,
    zooming: bool,
}

pub struct Canvas {
    /// The engine's own message window (timers and hotkeys hang off it).
    hwnd: HWND,
    overlay: HWND,
    /// Fullscreen window under every app that draws the canvas itself.
    bg: HWND,
    attached: bool,
    shared: Shared,
    layout: Layout,
    /// Zoomed-out and browsing; stays up until a window is picked or Esc.
    overview: bool,
    search: Option<SearchState>,
    sw: f64,
    sh: f64,
    cam_x: f64,
    cam_y: f64,
    zoom: f64,
    wins: Vec<Win>,
    shown: bool,
    anim: Option<Anim>,
    drag: Option<Drag>,
    gesture: Gesture,
    pending_focus: Option<HWND>,
    focused: Option<isize>,
    hovered: Option<isize>,
    paused: bool,
    quitting: bool,
    esc_registered: bool,
    /// Last precision-touchpad frame.
    last_touch: Option<Instant>,
    /// Camera when the overlay came up; Esc returns here.
    origin: (f64, f64),
    /// State changed since the last publish.
    dirty: bool,
}

impl Canvas {
    pub fn new(hwnd: HWND, shared: Shared) -> Self {
        let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN) as f64, GetSystemMetrics(SM_CYSCREEN) as f64) };
        Canvas {
            hwnd,
            overlay: HWND::default(),
            bg: HWND::default(),
            attached: false,
            shared,
            layout: Layout::load(),
            overview: false,
            search: None,
            sw,
            sh,
            cam_x: 0.0,
            cam_y: 0.0,
            zoom: 1.0,
            wins: Vec::new(),
            shown: false,
            anim: None,
            drag: None,
            gesture: Gesture::default(),
            pending_focus: None,
            focused: None,
            hovered: None,
            paused: false,
            quitting: false,
            esc_registered: false,
            last_touch: None,
            origin: (0.0, 0.0),
            dirty: true,
        }
    }

    fn cfg(&self) -> Settings {
        self.shared.settings()
    }

    // ---- UI commands -------------------------------------------------------

    pub fn command(&mut self, cmd: Command) {
        if self.quitting {
            return;
        }
        if !self.attached && !matches!(cmd, Command::Attach { .. } | Command::Quit) {
            return;
        }
        match cmd {
            Command::Attach { overlay, bg } => self.attach(overlay, bg),
            Command::MouseDown { x, y } => self.mouse_down((x, y)),
            Command::MouseMove { x, y } => self.mouse_move((x, y)),
            Command::MouseUp => self.mouse_up(),
            Command::Wheel { dx, dy, pinch, x, y } => self.wheel(dx, dy, pinch, (x, y)),
            Command::ToggleOverview => self.toggle_overview(),
            Command::LeaveOverview => self.leave_overview(),
            Command::Tidy => self.tidy(),
            Command::FocusWindow(id) => self.focus_window(id),
            Command::FlyTo { x, y } => self.fly_to(x, y),
            Command::OpenSearch => self.open_search(),
            Command::SearchQuery(q) => self.search_query(q),
            Command::SearchMove(d) => self.search_move(d),
            Command::SearchAccept => self.search_accept(),
            Command::CloseSearch => self.close_search(),
            Command::SettingsChanged => self.dirty = true,
            Command::SetPaused(p) => self.set_paused(p),
            Command::ResetLayout => {
                self.stop_timer(TIMER_SAVE);
                self.layout.reset();
                self.layout.save();
                log!("saved layout cleared");
            }
            Command::Quit => self.quit(),
        }
    }

    fn attach(&mut self, overlay: isize, bg: isize) {
        self.overlay = HWND(overlay as *mut _);
        self.bg = HWND(bg as *mut _);
        self.attached = !self.overlay.0.is_null() && !self.bg.0.is_null();
        self.restore_layout();
        self.dirty = true;
        log!("canvas attached (screen {}x{})", self.sw, self.sh);
    }

    /// Esc: closes search first, then leaves the overview.
    pub fn escape(&mut self) {
        if self.search.is_some() {
            self.close_search();
        } else {
            self.leave_overview();
        }
    }

    pub fn on_timer(&mut self, id: usize) {
        match id {
            TIMER_ANIM => self.tick(),
            TIMER_HIDE => self.hide_overlay(),
            TIMER_WHEEL => self.wheel_idle(),
            TIMER_SAVE => {
                self.stop_timer(TIMER_SAVE);
                // Mid-gesture the camera is in flux; the next commit re-arms.
                if !self.shown {
                    self.save_layout();
                }
            }
            _ => {}
        }
    }

    // ---- touchpad ----------------------------------------------------------

    pub fn on_fingers(&mut self, fingers: &[Contact]) {
        if !self.attached || self.quitting {
            return;
        }
        let n = fingers.len();
        let now = Instant::now();
        self.last_touch = Some(now);
        let cfg = self.cfg();
        // Three fingers drive the canvas from anywhere; two only over the
        // canvas itself, so scrolling inside apps is left alone. Four or more
        // belong to Windows (desktop switching etc.).
        let wanted = match n {
            3 => cfg.three_finger,
            2 => cfg.two_finger_canvas && self.cursor_on_canvas(),
            _ => false,
        };
        if !wanted || self.paused {
            if self.gesture.active {
                self.gesture_end();
            }
            self.gesture = Gesture::default();
            return;
        }
        let cx = fingers.iter().map(|c| c.x).sum::<f64>() / n as f64;
        let cy = fingers.iter().map(|c| c.y).sum::<f64>() / n as f64;
        let spread = fingers.iter().map(|c| (c.x - cx).hypot(c.y - cy)).sum::<f64>() / n as f64;

        // A finger joining or leaving shifts the centroid; re-anchor instead of jumping.
        if let Some((_, px, py, ps)) = self.gesture.prev.filter(|p| p.0 == n) {
            let (dx, dy) = ((cx - px) * cfg.pan_speed, (cy - py) * cfg.pan_speed);
            if !self.gesture.active {
                self.gesture.travel += dx.hypot(dy) + (spread - ps).abs();
                if self.gesture.travel > START_SLOP {
                    self.gesture.active = true;
                    self.gesture.spread0 = ps;
                    self.begin();
                }
            }
            if self.gesture.active {
                self.pan(dx, dy);
                if !self.gesture.zooming && self.gesture.spread0 > 1.0 {
                    self.gesture.zooming = (spread / self.gesture.spread0).ln().abs() > PINCH_LOCK;
                }
                if self.gesture.zooming && ps > 1.0 {
                    self.zoom_about((spread / ps).powf(cfg.pinch_speed), cursor());
                }
                if let Some(t) = self.gesture.last {
                    let dt = now.duration_since(t).as_secs_f64().max(0.004);
                    let v = &mut self.gesture.vel;
                    v.0 = v.0 * 0.6 + dx / dt * 0.4;
                    v.1 = v.1 * 0.6 + dy / dt * 0.4;
                }
                self.gesture.last = Some(now);
                self.update_view();
            }
        }
        self.gesture.prev = Some((n, cx, cy, spread));
    }

    fn gesture_end(&mut self) {
        let stale = self.gesture.last.map(|t| t.elapsed().as_secs_f64() > 0.08).unwrap_or(true);
        let (vx, vy) = if stale { (0.0, 0.0) } else { self.gesture.vel };
        self.settle(vx, vy);
    }

    /// Motion has stopped: glide out the flick, then hand back to real
    /// windows if we're near 1:1, otherwise stay zoomed out.
    fn settle(&mut self, vx: f64, vy: f64) {
        let flick = self.cfg().flick;
        let tx = self.cam_x - vx * flick / self.zoom;
        let ty = self.cam_y - vy * flick / self.zoom;
        if self.overview {
            self.animate_to(tx, ty, self.zoom, false, None);
        } else if (self.zoom - 1.0).abs() < SNAP {
            // Keep the point under the cursor fixed while snapping to 1:1.
            let p = cursor();
            let (wx, wy) = (tx + p.0 / self.zoom, ty + p.1 / self.zoom);
            self.animate_to(wx - p.0, wy - p.1, 1.0, true, None);
        } else {
            self.animate_to(tx, ty, self.zoom, false, None);
            self.enter_overview();
        }
    }

    // ---- mouse on the overlay ----------------------------------------------

    fn mouse_down(&mut self, p: (f64, f64)) {
        if !self.shown {
            return;
        }
        self.cancel_anim();
        let target = self.hit(p);
        self.drag = Some(Drag { target, last: p, moved: 0.0 });
    }

    fn mouse_move(&mut self, p: (f64, f64)) {
        let Some(d) = self.drag.as_mut() else {
            self.set_hover(p);
            return;
        };
        let (dx, dy) = (p.0 - d.last.0, p.1 - d.last.1);
        d.last = p;
        d.moved += dx.abs() + dy.abs();
        match d.target {
            Some(i) => {
                let w = &mut self.wins[i];
                w.x += dx / self.zoom;
                w.y += dy / self.zoom;
            }
            None => self.pan(dx, dy),
        }
        self.update_view();
    }

    fn mouse_up(&mut self) {
        let Some(d) = self.drag.take() else { return };
        if d.moved < 4.0 {
            if let Some(i) = d.target {
                self.focus(i);
            }
        }
    }

    fn set_hover(&mut self, p: (f64, f64)) {
        let hovered = if self.shown { self.hit(p).map(|i| self.wins[i].id()) } else { None };
        if hovered != self.hovered {
            self.hovered = hovered;
            self.dirty = true;
        }
    }

    /// Mouse-wheel input from the overlay; pinch comes with Ctrl held.
    fn wheel(&mut self, dx: f64, dy: f64, pinch: bool, p: (f64, f64)) {
        if self.paused || self.last_touch.is_some_and(|t| t.elapsed().as_secs_f64() < WHEEL_QUIET_SECS) {
            return;
        }
        self.cancel_anim();
        if !self.shown {
            self.begin();
        }
        if !self.overview {
            unsafe {
                SetTimer(Some(self.hwnd), TIMER_WHEEL, WHEEL_IDLE_MS, None);
            }
        }
        if pinch {
            self.zoom_about((dy / 60.0 * 0.12).exp(), p);
        } else {
            self.pan(dx, dy);
        }
        self.update_view();
    }

    fn wheel_idle(&mut self) {
        self.stop_timer(TIMER_WHEEL);
        if self.shown && self.anim.is_none() && self.drag.is_none() {
            self.settle(0.0, 0.0);
        }
    }

    // ---- hotkeys and commands ------------------------------------------------

    /// Zoom out to fit every window, or back in if already zoomed out.
    fn toggle_overview(&mut self) {
        if self.shown {
            self.leave_overview();
            return;
        }
        self.begin();
        let Some(b) = self.bounds() else { return };
        let (cx, cy, z) = self.fit(b);
        self.animate_to(cx, cy, z, false, None);
        self.enter_overview();
    }

    /// Camera and zoom that show the canvas rect `b` with a margin.
    fn fit(&self, (x0, y0, x1, y1): (f64, f64, f64, f64)) -> (f64, f64, f64) {
        let margin = 80.0;
        let z = ((self.sw - margin * 2.0) / (x1 - x0))
            .min((self.sh - margin * 2.0) / (y1 - y0))
            .clamp(MIN_ZOOM, 0.85);
        ((x0 + x1) / 2.0 - self.sw / 2.0 / z, (y0 + y1) / 2.0 - self.sh / 2.0 / z, z)
    }

    /// Zoom back to 1:1 where the overlay first came up.
    fn leave_overview(&mut self) {
        if !self.shown {
            return;
        }
        let (x, y) = self.origin;
        self.animate_to(x, y, 1.0, true, None);
    }

    /// Outside the overview (e.g. the background's minimap) this brings the
    /// overlay up and lands back on real windows at 1:1.
    fn fly_to(&mut self, x: f64, y: f64) {
        if !self.shown {
            self.begin();
        }
        if !self.shown {
            return;
        }
        let commit = !self.overview;
        let z = if commit { 1.0 } else { self.zoom };
        self.animate_to(x - self.sw / 2.0 / z, y - self.sh / 2.0 / z, z, commit, None);
    }

    fn focus_window(&mut self, id: isize) {
        if !self.shown {
            self.begin();
        }
        if let Some(i) = self.wins.iter().position(|w| w.id() == id) {
            self.focus(i);
        }
    }

    fn set_paused(&mut self, paused: bool) {
        if paused == self.paused {
            return;
        }
        self.paused = paused;
        self.gesture = Gesture::default();
        if paused {
            self.leave_overview();
        }
        unsafe {
            let _ = ShowWindowAsync(self.bg, if paused { SW_HIDE } else { SW_SHOWNOACTIVATE });
        }
        self.dirty = true;
    }

    /// Lay every window out in neat rows, keeping their reading order. Each
    /// one glides to its new spot while the camera frames the result.
    fn tidy(&mut self) {
        if !self.shown {
            self.begin();
        }
        let Some((x0, y0, x1, y1)) = self.bounds().filter(|_| self.shown) else { return };
        self.drag = None;
        let rects: Vec<_> = self.wins.iter().map(|w| (w.x, w.y, w.w, w.h)).collect();
        let order = tidy::reading_order(&rects);
        let cfg = self.cfg();
        let gap = cfg.gap.round();
        // Labels sit above windows at a fixed screen size, so each row needs
        // room for one in canvas pixels at the zoom that frames the result.
        // That zoom depends on the room; a few passes settle it.
        let label = if cfg.show_labels { (labels::LABEL_ROOM as f64 + 6.0) * dpi_scale() } else { 0.0 };
        let arrange = |room: f64| {
            let padded: Vec<(f64, f64)> = order.iter().map(|&i| (self.wins[i].w, self.wins[i].h + room)).collect();
            let spots = tidy::arrange(&padded, gap);
            let extent = spots
                .iter()
                .zip(&padded)
                .fold((0.0_f64, 0.0_f64), |(w, h), (p, s)| (w.max(p.0 + s.0), h.max(p.1 + s.1)));
            (spots, extent)
        };
        let mut room = 0.0;
        for _ in 0..3 {
            let (w, h) = arrange(room).1;
            room = (label / self.fit((0.0, 0.0, w, h)).2).ceil();
        }
        let (spots, (w, h)) = arrange(room);
        // Centred where the windows were, on whole pixels.
        let ox = ((x0 + x1 - w) / 2.0).round();
        let oy = ((y0 + y1 - h) / 2.0).round();
        let moves: Vec<WinMove> = order
            .iter()
            .zip(&spots)
            .map(|(&i, &(x, y))| (self.wins[i].id(), (self.wins[i].x, self.wins[i].y), (ox + x, oy + y + room)))
            .collect();
        // Esc returns to 1:1 where the overlay came up; carry that spot along
        // with the focused (or topmost) window that was on screen there, so it
        // doesn't land on empty canvas. With none on screen, centre one.
        let (gx, gy) = self.origin;
        let seen = |w: &&Win| w.x < gx + self.sw && w.y < gy + self.sh && w.x + w.w > gx && w.y + w.h > gy;
        let focused_first = |w: &&Win| Some(w.id()) != self.focused;
        let on_screen = self.wins.iter().filter(seen).min_by_key(focused_first);
        if let Some(w) = on_screen.or_else(|| self.wins.iter().min_by_key(focused_first)) {
            if let Some(&(_, from, to)) = moves.iter().find(|m| m.0 == w.id()) {
                self.origin = match on_screen {
                    Some(_) => (gx + to.0 - from.0, gy + to.1 - from.1),
                    None => self.centre_on((to.0, to.1, w.w, w.h)),
                };
            }
        }
        let (cx, cy, z) = self.fit((ox, oy, ox + w, oy + h));
        self.animate_to(cx, cy, z, false, None);
        if let Some(a) = self.anim.as_mut() {
            a.moves = moves;
        }
        self.enter_overview();
        log!("tidy: arranged {} windows", spots.len());
    }

    // ---- search --------------------------------------------------------------

    fn open_search(&mut self) {
        let fresh = !self.shown;
        self.begin();
        if !self.shown {
            return;
        }
        if !self.overview {
            self.enter_overview();
        }
        if self.search.is_none() {
            self.search = Some(self.ranked(String::new()));
        }
        // Zoom out to everything, fitted below the search panel.
        if let Some(b) = self.bounds().filter(|_| fresh) {
            let (l, t, r, bottom) = self.search_area();
            let z = ((r - l) / (b.2 - b.0)).min((bottom - t) / (b.3 - b.1)).clamp(MIN_ZOOM, 0.85);
            let x = (b.0 + b.2) / 2.0 - (l + r) / 2.0 / z;
            let y = (b.1 + b.3) / 2.0 - (t + bottom) / 2.0 / z;
            self.animate_to(x, y, z, false, None);
        }
        self.shared.emit(UiEvent::FocusSearch);
        self.update_view();
    }

    fn ranked(&self, query: String) -> SearchState {
        let results = search::rank(&query, &self.infos());
        SearchState { query, results, selected: 0 }
    }

    fn search_query(&mut self, query: String) {
        if self.search.is_some() {
            self.search = Some(self.ranked(query));
            self.frame_selected();
            self.update_view();
        }
    }

    fn search_move(&mut self, delta: i32) {
        let Some(s) = self.search.as_mut() else { return };
        let n = s.results.len() as i32;
        if n > 0 {
            s.selected = (s.selected as i32 + delta).rem_euclid(n) as usize;
        }
        self.frame_selected();
        self.dirty = true;
    }

    /// Glide the camera so the selected result is in view below the search
    /// panel: a nudge if it's partly visible, centred if it's out of sight,
    /// zoomed out if it doesn't fit.
    fn frame_selected(&mut self) {
        let Some(s) = self.search.as_ref() else { return };
        let Some(w) = s.results.get(s.selected).and_then(|id| self.wins.iter().find(|w| w.id() == *id)) else {
            return;
        };
        // Aim from where a running glide is heading; never cut a tidy short.
        let (cx, cy, z0) = match &self.anim {
            Some(a) if !a.moves.is_empty() || a.commit => return,
            Some(a) => a.to,
            None => (self.cam_x, self.cam_y, self.zoom),
        };
        let (l, t, r, b) = self.search_area();
        let z = z0.min((r - l) / w.w).min((b - t) / w.h).max(MIN_ZOOM);
        let (sx, sy) = ((w.x - cx) * z, (w.y - cy) * z);
        let (ex, ey) = (sx + w.w * z, sy + w.h * z);
        let hidden = ex <= l || sx >= r || ey <= t || sy >= b;
        let (tx, ty) = if z < z0 || hidden {
            (w.x + w.w / 2.0 - (l + r) / 2.0 / z, w.y + w.h / 2.0 - (t + b) / 2.0 / z)
        } else {
            let nudge = |lo: f64, hi: f64, min: f64, max: f64| {
                if lo < min {
                    lo - min
                } else if hi > max {
                    hi - max
                } else {
                    0.0
                }
            };
            (cx + nudge(sx, ex, l, r) / z, cy + nudge(sy, ey, t, b) / z)
        };
        if (tx - cx).abs() > 0.5 || (ty - cy).abs() > 0.5 || z != z0 {
            self.animate_to(tx, ty, z, false, None);
        }
    }

    /// The part of the screen below the search panel, inset by FRAME_MARGIN.
    fn search_area(&self) -> (f64, f64, f64, f64) {
        let empty = SearchState::default();
        let panel = search::panel_rect(self.search.as_ref().unwrap_or(&empty), self.sw, dpi_scale());
        (FRAME_MARGIN, panel.3 + FRAME_MARGIN, self.sw - FRAME_MARGIN, self.sh - FRAME_MARGIN)
    }

    fn search_accept(&mut self) {
        let Some(s) = self.search.take() else { return };
        if let Some(&id) = s.results.get(s.selected) {
            self.focus_window(id);
        }
        self.update_view();
    }

    fn close_search(&mut self) {
        if self.search.take().is_some() {
            self.update_view();
        }
    }

    // ---- timers --------------------------------------------------------------

    fn tick(&mut self) {
        let Some(a) = self.anim.as_ref() else {
            self.stop_timer(TIMER_ANIM);
            return;
        };
        let t = (a.start.elapsed().as_secs_f64() / ANIM_SECS).min(1.0);
        let e = 1.0 - (1.0 - t).powi(3);
        let lerp = |a: f64, b: f64| a + (b - a) * e;
        self.cam_x = lerp(a.from.0, a.to.0);
        self.cam_y = lerp(a.from.1, a.to.1);
        // Interpolate zoom in log space so it feels even across scales.
        self.zoom = (a.from.2.ln() + (a.to.2.ln() - a.from.2.ln()) * e).exp();
        for &(id, from, to) in &a.moves {
            if let Some(w) = self.wins.iter_mut().find(|w| w.id() == id) {
                w.x = lerp(from.0, to.0);
                w.y = lerp(from.1, to.1);
            }
        }
        self.update_view();
        if t >= 1.0 {
            if let Some(a) = self.anim.take() {
                self.stop_timer(TIMER_ANIM);
                if a.commit {
                    self.commit(a.focus);
                }
            }
        }
    }

    /// Real windows have been moved; drop the overlay.
    fn hide_overlay(&mut self) {
        self.stop_timer(TIMER_HIDE);
        unsafe {
            let _ = ShowWindowAsync(self.overlay, SW_HIDE);
        }
        self.unregister_esc();
        self.release_thumbnails();
        self.shown = false;
        self.hovered = None;
        self.dirty = true;
        if let Some(h) = self.pending_focus.take() {
            unsafe {
                let _ = SetForegroundWindow(h);
            }
            self.focused = Some(h.0 as isize);
        }
    }

    // ---- core ----------------------------------------------------------------

    /// Show the overlay with thumbnails standing in for the real windows.
    fn begin(&mut self) {
        self.stop_timer(TIMER_HIDE);
        self.pending_focus = None;
        if self.shown || !self.attached {
            return;
        }
        self.sync();
        self.origin = (self.cam_x, self.cam_y);
        let fg = unsafe { GetForegroundWindow() }.0 as isize;
        self.focused = self.wins.iter().find(|w| w.id() == fg).map(Win::id);
        self.shown = true;
        // Publish before the overlay appears so its first frame is current.
        self.publish();
        unsafe {
            let _ = SetWindowPos(
                self.overlay,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_SHOWWINDOW | SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
            );
        }
        // Bottom-most first so later (higher) windows draw on top.
        for w in self.wins.iter_mut().rev() {
            w.thumb = unsafe { DwmRegisterThumbnail(self.overlay, w.hwnd) }.unwrap_or(0);
        }
        self.update_view();
    }

    fn enter_overview(&mut self) {
        self.overview = true;
        if !self.esc_registered {
            self.esc_registered =
                unsafe { RegisterHotKey(Some(self.hwnd), HOTKEY_ESC, MOD_NOREPEAT, VK_ESCAPE.0 as u32) }.is_ok();
        }
        self.dirty = true;
    }

    fn unregister_esc(&mut self) {
        if self.esc_registered {
            unsafe {
                let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_ESC);
            }
            self.esc_registered = false;
        }
    }

    fn release_thumbnails(&mut self) {
        for w in &mut self.wins {
            if w.thumb != 0 {
                unsafe {
                    let _ = DwmUnregisterThumbnail(w.thumb);
                }
                w.thumb = 0;
            }
        }
    }

    /// Refresh the window list from reality. Only valid at zoom 1 with the
    /// overlay hidden, when real windows mirror the canvas.
    fn sync(&mut self) {
        let cfg = self.cfg();
        let mut next: Vec<Win> = Vec::new();
        let mut fresh = Vec::new();
        let mut new = Vec::new();
        for h in wins::app_windows(true) {
            let maximized = unsafe { IsZoomed(h).as_bool() };
            if maximized {
                wins::unmaximize(h);
            }
            let Some(r) = wins::window_rect(h) else { continue };
            let (w, hgt) = ((r.right - r.left) as f64, (r.bottom - r.top) as f64);
            let known = self.wins.iter().find(|o| o.hwnd == h);
            let (exe, class) = match known {
                Some(o) => (o.exe.clone(), o.class.clone()),
                None => (wins::exe_name(h), wins::class_name(h)),
            };
            let title = wins::title(h);
            let (x, y, placed) = match known {
                Some(o) if o.placed == Some(r) => (o.x, o.y, o.placed),
                // New, or moved/resized by the user since we placed it.
                _ => (self.cam_x + r.left as f64, self.cam_y + r.top as f64, None),
            };
            fresh.push(known.is_none() || maximized);
            new.push(known.is_none());
            next.push(Win { hwnd: h, title, exe, class, x, y, w, h: hgt, placed, thumb: 0 });
        }

        // A new window that was here last time goes back to its saved spot.
        // Every window is matched so known ones keep their own entries.
        if cfg.remember_layout && new.contains(&true) {
            let keys: Vec<WinKey> = next.iter().map(Win::key).collect();
            for (i, saved) in self.layout.positions(&keys).into_iter().enumerate() {
                if let (true, Some((x, y))) = (new[i], saved) {
                    (next[i].x, next[i].y, fresh[i]) = (x, y, false);
                }
            }
        }

        // New and formerly maximized windows tend to land right on top of
        // others. Slide each one right until it has its own space; topmost
        // windows keep their spot.
        for i in 0..next.len() {
            if !fresh[i] {
                continue;
            }
            let (mut x, y, w, h) = (next[i].x, next[i].y, next[i].w, next[i].h);
            for _ in 0..64 {
                let blocker = next
                    .iter()
                    .enumerate()
                    .filter(|&(j, o)| j != i && (!fresh[j] || j < i) && overlaps((x, y, w, h), o))
                    .map(|(_, o)| o.x + o.w)
                    .fold(None, |m: Option<f64>, r| Some(m.map_or(r, |m| m.max(r))));
                match blocker {
                    Some(right) => x = right + cfg.gap,
                    None => break,
                }
            }
            next[i].x = x;
        }
        self.wins = next;
        self.dirty = true;
    }

    /// Move the real windows to match the canvas at zoom 1.
    fn place_windows(&mut self) {
        for w in &mut self.wins {
            // Windows stops honouring coordinates past roughly +-32k.
            let x = (w.x - self.cam_x).round().clamp(-30000.0, 30000.0) as i32;
            let y = (w.y - self.cam_y).round().clamp(-30000.0, 30000.0) as i32;
            unsafe {
                let _ = SetWindowPos(
                    w.hwnd,
                    None,
                    x,
                    y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
                );
            }
            w.placed = Some(RECT { left: x, top: y, right: x + w.w as i32, bottom: y + w.h as i32 });
        }
    }

    /// Place the real windows, then hide the overlay once they've had a
    /// moment to get there.
    fn commit(&mut self, focus: Option<HWND>) {
        self.zoom = 1.0;
        self.overview = false;
        self.search = None;
        self.stop_timer(TIMER_WHEEL);
        self.place_windows();
        self.pending_focus = focus;
        if let Some(h) = focus {
            self.focused = Some(h.0 as isize);
        }
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_HIDE, HIDE_DELAY_MS, None);
            SetTimer(Some(self.hwnd), TIMER_SAVE, SAVE_DELAY_MS, None);
        }
        self.dirty = true;
    }

    /// Leave everything at zoom 1 with windows placed, then tell the UI to
    /// shut down.
    fn quit(&mut self) {
        self.quitting = true;
        self.cancel_anim();
        self.drag = None;
        if self.shown || self.zoom != 1.0 {
            (self.cam_x, self.cam_y) = self.origin;
            self.zoom = 1.0;
            self.place_windows();
        }
        self.save_layout();
        self.unregister_esc();
        self.release_thumbnails();
        unsafe {
            let _ = ShowWindowAsync(self.overlay, SW_HIDE);
        }
        // Placement is async; give it a moment before checking for strays.
        std::thread::sleep(Duration::from_millis(150));
        let n = wins::rescue_all();
        log!("quit: {n} stranded windows brought back");
        self.shared.emit(UiEvent::Quit);
        // If the UI thread is wedged, don't hang around.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(3));
            std::process::exit(0);
        });
    }

    /// Records the camera and window positions so a restart picks up where
    /// we left off. Always runs at zoom 1 (after a commit, or on quit).
    fn save_layout(&mut self) {
        if !self.cfg().remember_layout {
            return;
        }
        let entries: Vec<(WinKey, f64, f64)> = self.wins.iter().map(|w| (w.key(), w.x, w.y)).collect();
        self.layout.capture((self.cam_x, self.cam_y), &entries);
        self.layout.save();
    }

    /// Applies the saved layout at startup: the camera goes back to the saved
    /// one and windows take their saved canvas positions (via `sync`).
    /// Windows with no saved spot stay where they are on screen.
    fn restore_layout(&mut self) {
        if !self.cfg().remember_layout {
            return;
        }
        let Some((cx, cy)) = self.layout.camera() else { return };
        // Camera first, so unmatched windows map to their current screen spot.
        (self.cam_x, self.cam_y) = (cx, cy);
        self.sync();
        self.place_windows();
        log!("layout restored ({} windows)", self.wins.len());
    }

    fn focus(&mut self, i: usize) {
        let w = &self.wins[i];
        let (tx, ty) = self.centre_on((w.x, w.y, w.w, w.h));
        let h = w.hwnd;
        self.animate_to(tx, ty, 1.0, true, Some(h));
    }

    /// Camera at zoom 1 that centres the canvas rect `(x, y, w, h)`, or pins
    /// its top-left if it's bigger than the screen.
    fn centre_on(&self, (x, y, w, h): (f64, f64, f64, f64)) -> (f64, f64) {
        let tx = if w < self.sw { x + w / 2.0 - self.sw / 2.0 } else { x };
        let ty = if h < self.sh { y + h / 2.0 - self.sh / 2.0 } else { y };
        (tx, ty)
    }

    fn animate_to(&mut self, x: f64, y: f64, z: f64, commit: bool, focus: Option<HWND>) {
        self.cancel_anim();
        self.anim = Some(Anim {
            from: (self.cam_x, self.cam_y, self.zoom),
            to: (x, y, z),
            start: Instant::now(),
            commit,
            focus,
            moves: Vec::new(),
        });
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_ANIM, 8, None);
        }
    }

    /// Stops the animation where it is, except that windows on their way
    /// somewhere (tidy) land there, so an interrupted tidy is still tidy.
    fn cancel_anim(&mut self) {
        let Some(a) = self.anim.take() else { return };
        for (id, _, to) in a.moves {
            if let Some(w) = self.wins.iter_mut().find(|w| w.id() == id) {
                (w.x, w.y) = to;
            }
        }
    }

    fn pan(&mut self, dx: f64, dy: f64) {
        self.cam_x -= dx / self.zoom;
        self.cam_y -= dy / self.zoom;
    }

    fn zoom_about(&mut self, factor: f64, p: (f64, f64)) {
        let z = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let (wx, wy) = (self.cam_x + p.0 / self.zoom, self.cam_y + p.1 / self.zoom);
        self.zoom = z;
        self.cam_x = wx - p.0 / z;
        self.cam_y = wy - p.1 / z;
    }

    fn screen_rect(&self, w: &Win) -> RECT {
        let l = (w.x - self.cam_x) * self.zoom;
        let t = (w.y - self.cam_y) * self.zoom;
        RECT {
            left: l.round() as i32,
            top: t.round() as i32,
            right: (l + w.w * self.zoom).round() as i32,
            bottom: (t + w.h * self.zoom).round() as i32,
        }
    }

    fn cursor_on_canvas(&self) -> bool {
        let (x, y) = cursor();
        let h = unsafe { WindowFromPoint(POINT { x: x as i32, y: y as i32 }) };
        h == self.bg || h == self.overlay
    }

    fn hit(&self, p: (f64, f64)) -> Option<usize> {
        // wins is topmost-first, so the first hit is the visible one.
        self.wins.iter().position(|w| {
            let r = self.screen_rect(w);
            p.0 >= r.left as f64 && p.0 < r.right as f64 && p.1 >= r.top as f64 && p.1 < r.bottom as f64
        })
    }

    fn bounds(&self) -> Option<(f64, f64, f64, f64)> {
        let first = self.wins.first()?;
        let mut b = (first.x, first.y, first.x + first.w, first.y + first.h);
        for w in &self.wins {
            b = (b.0.min(w.x), b.1.min(w.y), b.2.max(w.x + w.w), b.3.max(w.y + w.h));
        }
        Some(b)
    }

    /// Point the thumbnails at the current camera, then queue a UI redraw.
    fn update_view(&mut self) {
        self.dirty = true;
        if !self.shown {
            return;
        }
        let matching = self.search.as_ref().map(|s| &s.results);
        // Thumbnails draw over GPUI, so while searching they're cut off at
        // the bottom of the search panel instead of hiding it.
        let panel = self.search.as_ref().map(|s| search::panel_rect(s, self.sw, dpi_scale()));
        for w in &self.wins {
            if w.thumb == 0 {
                continue;
            }
            let mut r = self.screen_rect(w);
            let mut source = RECT { left: 0, top: 0, right: w.w as i32, bottom: w.h as i32 };
            if let Some((pl, _, pr, pb)) = panel {
                let pb = pb.ceil() as i32;
                if r.top < pb && (r.left as f64) < pr && (r.right as f64) > pl {
                    source.top = ((pb - r.top) as f64 / self.zoom).round() as i32;
                    r.top = pb;
                }
            }
            let on_screen =
                r.right > 0 && r.bottom > r.top.max(0) && r.left < self.sw as i32 && r.top < self.sh as i32;
            let dim = matching.is_some_and(|m| !m.contains(&w.id()));
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION
                    | DWM_TNP_RECTSOURCE
                    | DWM_TNP_VISIBLE
                    | DWM_TNP_OPACITY
                    | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: r,
                rcSource: source,
                opacity: if dim { DIMMED } else { 255 },
                fVisible: on_screen.into(),
                fSourceClientAreaOnly: false.into(),
            };
            unsafe {
                let _ = DwmUpdateThumbnailProperties(w.thumb, &props);
            }
        }
    }

    fn stop_timer(&self, id: usize) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), id);
        }
    }

    // ---- publishing ------------------------------------------------------------

    fn infos(&self) -> Vec<WinInfo> {
        self.wins
            .iter()
            .map(|w| WinInfo {
                id: w.id(),
                title: w.title.clone(),
                exe: w.exe.clone(),
                x: w.x,
                y: w.y,
                w: w.w,
                h: w.h,
                focused: self.focused == Some(w.id()),
            })
            .collect()
    }

    fn mode(&self) -> Mode {
        if self.search.is_some() {
            Mode::Search
        } else if self.overview {
            Mode::Overview
        } else if self.shown {
            Mode::Moving
        } else {
            Mode::Idle
        }
    }

    fn publish(&mut self) {
        self.dirty = false;
        let next = Snapshot {
            cam_x: self.cam_x,
            cam_y: self.cam_y,
            zoom: self.zoom,
            screen_w: self.sw,
            screen_h: self.sh,
            mode: self.mode(),
            overlay_visible: self.shown,
            windows: self.infos(),
            hovered: self.hovered,
            search: self.search.clone(),
            paused: self.paused,
            generation: 0,
        };
        {
            let mut snap = self.shared.snapshot.write().unwrap_or_else(|e| e.into_inner());
            let generation = snap.generation + 1;
            *snap = Snapshot { generation, ..next };
        }
        self.shared.redraw();
    }

    /// Publishes the snapshot if anything changed since the last one.
    pub fn flush(&mut self) {
        if self.dirty {
            self.publish();
        }
    }
}

/// Overlap beyond a few pixels of shadow/border.
fn overlaps(a: (f64, f64, f64, f64), b: &Win) -> bool {
    const SLACK: f64 = 8.0;
    a.0 < b.x + b.w - SLACK && b.x < a.0 + a.2 - SLACK && a.1 < b.y + b.h - SLACK && b.y < a.1 + a.3 - SLACK
}

/// Physical pixels per logical pixel on the primary monitor.
fn dpi_scale() -> f64 {
    unsafe { GetDpiForSystem() as f64 / 96.0 }
}

fn cursor() -> (f64, f64) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    (p.x as f64, p.y as f64)
}
