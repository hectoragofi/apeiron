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

use std::collections::HashSet;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::ticker::Ticker;
use super::touchpad::Contact;
use super::{tidy, wins, HOTKEY_ESC};
use crate::layout::{Layout, WinKey};
use crate::settings::Settings;
use crate::shared::{Command, Mode, SearchState, Shared, Snapshot, UiEvent, WinInfo};
use crate::ui::{labels, minimap, search};

pub const MIN_ZOOM: f64 = 0.04;
pub const MAX_ZOOM: f64 = 1.0;
/// A gesture that ends this close to zoom 1 snaps back to real windows.
const SNAP: f64 = 0.15;
/// Finger travel (px) before three fingers count as a gesture, not a tap.
const START_SLOP: f64 = 6.0;
const ANIM_SECS: f64 = 0.32;
/// A gesture only counts as a pinch once spread has changed by this much
/// (log ratio) as well, so finger wobble alone never zooms.
const PINCH_MIN: f64 = 0.04;
/// Wheel (two-finger scroll / pinch) gestures end after this much quiet.
const WHEEL_IDLE_MS: u32 = 140;
/// Windows also turns touchpad motion into coarse, laggy wheel events; those
/// are ignored for this long after the last precision-touchpad frame.
const WHEEL_QUIET_SECS: f64 = 1.5;
/// How long the overlay stays up after real windows are moved, so they have
/// time to land before it disappears.
const HIDE_DELAY_MS: u32 = 70;
const SAVE_DELAY_MS: u32 = 1000;
/// Window events elsewhere on the desktop are batched this long before the
/// idle window list is refreshed.
const REFRESH_DELAY_MS: u32 = 300;
/// Display changes come in bursts; act once they've settled.
const SCREEN_DELAY_MS: u32 = 250;
/// Thumbnail opacity of windows that don't match the search.
const DIMMED: u8 = 70;
/// Space kept around a search result the camera frames, physical pixels.
const FRAME_MARGIN: f64 = 48.0;
/// Space kept around everything when zoomed out to fit, physical pixels.
const FIT_MARGIN: f64 = 80.0;

pub const TIMER_HIDE: usize = 2;
pub const TIMER_WHEEL: usize = 3;
pub const TIMER_SAVE: usize = 4;
pub const TIMER_REFRESH: usize = 5;
pub const TIMER_SCREEN: usize = 6;

/// What a thumbnail was last told, so unchanged ones aren't sent to DWM again.
#[derive(Clone, Copy, PartialEq)]
struct Thumb {
    dest: RECT,
    source: RECT,
    opacity: u8,
    visible: bool,
}

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
    shown: Option<Thumb>,
    /// Picked up while idle and not yet given a spot (saved or free); the
    /// next gesture treats it as new.
    unsettled: bool,
}

impl Win {
    fn id(&self) -> isize {
        self.hwnd.0 as isize
    }

    fn key(&self) -> WinKey {
        WinKey { exe: self.exe.clone(), class: self.class.clone(), title: self.title.clone() }
    }
}

/// How `sync` treats a window when finding it a spot.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Known,
    New,
    /// Was maximized; now fills the work area as a normal window.
    Unmaximized,
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
    /// The window being dragged (by id: the list is rebuilt on every sync),
    /// or None for panning.
    target: Option<isize>,
    last: (f64, f64),
    moved: f64,
}

/// What a finger gesture does. Decided once it's clear and kept until every
/// finger lifts: a drag never zooms and a pinch never pans.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum GestureKind {
    #[default]
    Undecided,
    Pan,
    Zoom,
}

#[derive(Default)]
struct Gesture {
    prev: Option<(usize, f64, f64, f64)>,
    kind: GestureKind,
    /// Evidence while undecided: summed centroid travel, and summed spread
    /// change (a radial pinch of k px per finger moves the spread by k).
    pan_ev: f64,
    zoom_ev: f64,
    /// Finger spread when the gesture (or its current finger count) started.
    spread0: f64,
    /// Screen point a pinch zooms about, fixed when it was recognised.
    anchor: (f64, f64),
    last: Option<Instant>,
    vel: (f64, f64),
}

impl Gesture {
    fn active(&self) -> bool {
        self.kind != GestureKind::Undecided
    }
}

/// Tells a drag from a pinch by which kind of finger motion dominated:
/// centroid travel `pan_ev` or spread change `zoom_ev`, with `log_ratio` the
/// spread's overall change. Fingers drift together and apart while dragging
/// and the centroid wobbles while pinching, so only a clear pinch zooms.
fn classify(pan_ev: f64, zoom_ev: f64, log_ratio: f64) -> GestureKind {
    if pan_ev.max(zoom_ev) <= START_SLOP {
        GestureKind::Undecided
    } else if zoom_ev <= 2.0 * pan_ev {
        GestureKind::Pan
    } else if log_ratio.abs() > PINCH_MIN {
        GestureKind::Zoom
    } else {
        // Mostly spread motion that hasn't gone anywhere yet: wait.
        GestureKind::Undecided
    }
}

/// Whether a wheel event of this kind (pinch or scroll) belongs to the
/// current burst `(pinch, last event)`: a burst keeps the kind it started
/// with until it has been quiet for WHEEL_IDLE_MS.
fn wheel_accepts(burst: Option<(bool, Instant)>, pinch: bool, now: Instant) -> bool {
    match burst {
        Some((kind, t)) if kind != pinch => {
            now.duration_since(t) >= Duration::from_millis(WHEEL_IDLE_MS as u64)
        }
        _ => true,
    }
}

pub struct Canvas {
    /// The engine's own window (timers and hotkeys hang off it).
    hwnd: HWND,
    overlay: HWND,
    /// Fullscreen window under every app that draws the canvas itself.
    bg: HWND,
    attached: bool,
    shared: Shared,
    ticker: Ticker,
    layout: Layout,
    /// "Reset saved layout" was pressed: nothing is saved until the next start.
    layout_frozen: bool,
    /// Zoomed-out and browsing; stays up until a window is picked or Esc.
    overview: bool,
    search: Option<SearchState>,
    /// Minimized windows offered by the search (they aren't on the canvas).
    minimized: Vec<WinInfo>,
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
    /// The session is locked; nothing is visible, so the background hides.
    locked: bool,
    quitting: bool,
    esc_registered: bool,
    refresh_armed: bool,
    /// Windows we last placed somewhere the user couldn't reach, mirrored to
    /// disk for the watchdog and the rescue CLI.
    parked: HashSet<isize>,
    /// Last precision-touchpad frame.
    last_touch: Option<Instant>,
    /// Current wheel burst: (pinch, last event of that kind).
    wheel_burst: Option<(bool, Instant)>,
    /// Camera at zoom 1 the canvas is resting at or heading for; Esc
    /// returns here.
    origin: (f64, f64),
    /// State changed since the last publish.
    dirty: bool,
}

impl Canvas {
    pub fn new(hwnd: HWND, shared: Shared, ticker: Ticker) -> Self {
        let (sw, sh) = screen_size();
        Canvas {
            hwnd,
            overlay: HWND::default(),
            bg: HWND::default(),
            attached: false,
            shared,
            ticker,
            layout: Layout::load(),
            layout_frozen: false,
            overview: false,
            search: None,
            minimized: Vec::new(),
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
            locked: false,
            quitting: false,
            esc_registered: false,
            refresh_armed: false,
            parked: HashSet::new(),
            last_touch: None,
            wheel_burst: None,
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
            Command::SettingsChanged => self.update_view(),
            Command::SetPaused(p) => self.set_paused(p),
            Command::ResetLayout => {
                // Every commit and quit would capture the current spots again;
                // forgetting means not saving for the rest of this run.
                self.stop_timer(TIMER_SAVE);
                self.layout.reset();
                self.layout.save();
                self.layout_frozen = true;
                log!("saved layout cleared");
            }
            Command::Quit => self.quit(),
        }
    }

    fn attach(&mut self, overlay: isize, bg: isize) {
        self.overlay = HWND(overlay as *mut _);
        self.bg = HWND(bg as *mut _);
        self.attached = !self.overlay.0.is_null() && !self.bg.0.is_null();
        (self.sw, self.sh) = screen_size();
        // A previous run that was killed with its watchdog (sign-out) may
        // have left windows out of reach.
        let n = wins::rescue_all();
        if n > 0 {
            log!("startup: {n} stranded windows brought back");
        }
        wins::clear_parked();
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
            TIMER_HIDE => self.hide_overlay(),
            TIMER_WHEEL => self.wheel_idle(),
            TIMER_SAVE => {
                self.stop_timer(TIMER_SAVE);
                // Mid-gesture the camera is in flux; the next commit re-arms.
                if !self.shown {
                    self.save_layout();
                }
            }
            TIMER_REFRESH => {
                self.stop_timer(TIMER_REFRESH);
                self.refresh_armed = false;
                if self.attached && !self.shown && !self.quitting {
                    self.sync(false);
                }
            }
            TIMER_SCREEN => self.apply_screen(),
            _ => {}
        }
    }

    /// A composed frame went by while animating.
    pub fn on_tick(&mut self) {
        self.tick();
    }

    // ---- the desktop around us ---------------------------------------------

    /// A window appeared, vanished, moved or changed state somewhere. While
    /// idle the background's minimap should follow, so refresh soon.
    pub fn windows_changed(&mut self) {
        if !self.attached || self.shown || self.quitting || self.refresh_armed {
            return;
        }
        self.refresh_armed = true;
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_REFRESH, REFRESH_DELAY_MS, None);
        }
    }

    /// Something brought a window to the front (Alt+Tab, the taskbar, a
    /// notification). If it's parked out of sight, fly there.
    pub fn foreground_changed(&mut self, h: HWND) {
        if !self.attached || self.shown || self.quitting {
            return;
        }
        let Some(r) = self.wins.iter().find(|w| w.hwnd == h).and_then(|w| w.placed) else { return };
        if wins::window_rect(h) == Some(r) && !wins::reachable(h, &r) {
            log!("activated an off-screen window; flying to it");
            self.focus_window(h.0 as isize);
        }
    }

    /// The display's size or scale changed (or is changing).
    pub fn screen_changed(&mut self) {
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_SCREEN, SCREEN_DELAY_MS, None);
        }
    }

    fn apply_screen(&mut self) {
        self.stop_timer(TIMER_SCREEN);
        let (sw, sh) = screen_size();
        if (sw, sh) != (self.sw, self.sh) {
            log!("screen is now {sw}x{sh}");
        }
        (self.sw, self.sh) = (sw, sh);
        self.shared.emit(UiEvent::Screen);
        self.update_view();
    }

    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
        self.show_bg();
    }

    /// Sign-out, shutdown or restart: the process will be killed, so leave
    /// every window reachable and the layout saved before returning. Moves
    /// are synchronous (except to hung apps) so they land before apps save
    /// their own window positions.
    pub fn end_session(&mut self) {
        log!("session ending: putting windows back");
        self.cancel_anim();
        self.drag = None;
        self.gesture = Gesture::default();
        self.pending_focus = None;
        self.go_home(true);
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
            if self.gesture.active() {
                self.gesture_end();
            }
            self.gesture = Gesture::default();
            return;
        }
        let cx = fingers.iter().map(|c| c.x).sum::<f64>() / n as f64;
        let cy = fingers.iter().map(|c| c.y).sum::<f64>() / n as f64;
        let spread = fingers.iter().map(|c| (c.x - cx).hypot(c.y - cy)).sum::<f64>() / n as f64;

        // A finger joining or leaving shifts the centroid and the spread;
        // re-anchor both instead of jumping. The gesture keeps its kind.
        if self.gesture.prev.is_none_or(|p| p.0 != n) {
            self.gesture.spread0 = spread;
        }
        if let Some((_, px, py, ps)) = self.gesture.prev.filter(|p| p.0 == n) {
            if !self.gesture.active() {
                let g = &mut self.gesture;
                g.pan_ev += (cx - px).hypot(cy - py);
                g.zoom_ev += (spread - ps).abs();
                let ratio = if g.spread0 > 1.0 && spread > 1.0 { (spread / g.spread0).ln() } else { 0.0 };
                g.kind = classify(g.pan_ev, g.zoom_ev, ratio);
                if self.gesture.active() {
                    self.gesture.anchor = cursor();
                    // The fingers own the camera now: a glide still running
                    // (the last flick, a fly-to) or a pending hide gives way.
                    self.cancel_anim();
                    self.stop_timer(TIMER_HIDE);
                    self.drag = None;
                    self.begin();
                }
            }
            match self.gesture.kind {
                GestureKind::Undecided => {}
                GestureKind::Pan => {
                    let (dx, dy) = ((cx - px) * cfg.pan_speed, (cy - py) * cfg.pan_speed);
                    self.pan(dx, dy);
                    if let Some(t) = self.gesture.last {
                        let dt = now.duration_since(t).as_secs_f64().max(0.004);
                        let v = &mut self.gesture.vel;
                        v.0 = v.0 * 0.6 + dx / dt * 0.4;
                        v.1 = v.1 * 0.6 + dy / dt * 0.4;
                    }
                    self.gesture.last = Some(now);
                    self.update_view();
                }
                GestureKind::Zoom => {
                    if ps > 1.0 && spread > 1.0 {
                        self.zoom_about((spread / ps).powf(cfg.pinch_speed), self.gesture.anchor);
                    }
                    self.gesture.last = Some(now);
                    self.update_view();
                }
            }
        }
        self.gesture.prev = Some((n, cx, cy, spread));
    }

    fn gesture_end(&mut self) {
        let stale = self.gesture.last.map(|t| t.elapsed().as_secs_f64() > 0.08).unwrap_or(true);
        // Only a drag flicks; letting go of a pinch never throws a pan.
        let flick = self.gesture.kind == GestureKind::Pan && !stale;
        let (vx, vy) = if flick { self.gesture.vel } else { (0.0, 0.0) };
        self.settle(vx, vy);
    }

    /// Motion has stopped: glide out the flick, then hand back to real
    /// windows if we're near 1:1, otherwise stay zoomed out.
    fn settle(&mut self, vx: f64, vy: f64) {
        let flick = self.cfg().flick;
        let tx = self.cam_x - vx * flick / self.zoom;
        let ty = self.cam_y - vy * flick / self.zoom;
        // Near 1:1 always means "back to the windows", from the overview too;
        // a search stays up until it's answered or closed.
        if (self.zoom - 1.0).abs() < SNAP && self.search.is_none() {
            self.land(tx, ty, None);
        } else {
            self.animate_to(tx, ty, self.zoom, false, None);
            if !self.overview {
                self.enter_overview();
            }
        }
    }

    /// Glide to zoom 1 from camera `(tx, ty)` at the current zoom, keeping
    /// the point under the cursor fixed, then hand back to real windows.
    fn land(&mut self, tx: f64, ty: f64, focus: Option<HWND>) {
        let p = cursor();
        let (wx, wy) = (tx + p.0 / self.zoom, ty + p.1 / self.zoom);
        self.animate_to(wx - p.0, wy - p.1, 1.0, true, focus);
    }

    // ---- mouse on the overlay ----------------------------------------------

    fn mouse_down(&mut self, p: (f64, f64)) {
        if !self.shown {
            return;
        }
        self.stop_timer(TIMER_HIDE);
        let target = self.hit(p).map(|i| self.wins[i].id());
        // A press on empty canvas lets a glide back to real windows finish
        // (often a tap right after a swipe); anything else takes over.
        if target.is_none() && self.anim.as_ref().is_some_and(|a| a.commit) {
            return;
        }
        self.cancel_anim();
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
            Some(id) => {
                let zoom = self.zoom;
                let Some(w) = self.wins.iter_mut().find(|w| w.id() == id) else {
                    // It closed mid-drag.
                    self.drag = None;
                    return;
                };
                w.x += dx / zoom;
                w.y += dy / zoom;
            }
            None => self.pan(dx, dy),
        }
        self.update_view();
    }

    fn mouse_up(&mut self) {
        let Some(d) = self.drag.take() else { return };
        if d.moved < 4.0 {
            if let Some(i) = d.target.and_then(|id| self.index(id)) {
                if self.overview {
                    self.focus(i);
                } else {
                    // Near 1:1 the thumbnail is the window: just land on it.
                    let h = self.wins[i].hwnd;
                    self.land(self.cam_x, self.cam_y, Some(h));
                }
                return;
            }
        }
        // Outside the overview nothing else would ever settle the canvas.
        if !self.overview && self.anim.is_none() {
            self.settle(0.0, 0.0);
        }
    }

    fn set_hover(&mut self, p: (f64, f64)) {
        let hovered = if self.shown { self.hit(p).map(|i| self.wins[i].id()) } else { None };
        if hovered != self.hovered {
            self.hovered = hovered;
            self.dirty = true;
        }
    }

    /// Mouse-wheel input from the overlay; pinch comes with Ctrl held. Works
    /// while paused too: the overlay is only up then because the user asked.
    fn wheel(&mut self, dx: f64, dy: f64, pinch: bool, p: (f64, f64)) {
        if self.last_touch.is_some_and(|t| t.elapsed().as_secs_f64() < WHEEL_QUIET_SECS) {
            return;
        }
        // A pinch never pans and a scroll never zooms: whichever started the
        // burst keeps it until the wheel goes quiet.
        let now = Instant::now();
        if !wheel_accepts(self.wheel_burst, pinch, now) {
            return;
        }
        self.wheel_burst = Some((pinch, now));
        self.stop_timer(TIMER_HIDE);
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
        if self.overview {
            self.leave_overview();
            return;
        }
        if !self.shown && self.nothing_to_show() {
            return;
        }
        // Mid-gesture or settling counts as "not in the overview": zoom out
        // from wherever the camera is.
        self.begin();
        let Some(b) = self.bounds() else { return };
        let (cx, cy, z) = self.frame(b);
        self.animate_to(cx, cy, z, false, None);
        self.enter_overview();
    }

    /// With no app windows an overview would be an empty screen-sized grid
    /// with nothing to click; don't bring the overlay up for it.
    fn nothing_to_show(&self) -> bool {
        let empty = wins::app_windows(true).is_empty();
        if empty {
            log!("no windows to show");
        }
        empty
    }

    /// Camera and zoom that show the canvas rect `b` with a margin.
    fn fit(&self, b: (f64, f64, f64, f64)) -> (f64, f64, f64) {
        fit_into(b, (FIT_MARGIN, FIT_MARGIN, self.sw - FIT_MARGIN, self.sh - FIT_MARGIN))
    }

    /// Like `fit`, but below the search panel while searching.
    fn frame(&self, b: (f64, f64, f64, f64)) -> (f64, f64, f64) {
        match self.search {
            Some(_) => fit_into(b, self.search_area()),
            None => self.fit(b),
        }
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

    /// Fly to a window and focus it; a minimized one offered by the search
    /// is restored instead.
    fn focus_window(&mut self, id: isize) {
        if !self.shown {
            self.begin();
        }
        if let Some(i) = self.index(id) {
            self.focus(i);
        } else if self.minimized.iter().any(|w| w.id == id) {
            self.search = None;
            self.minimized.clear();
            self.restore_minimized(HWND(id as *mut _));
        }
    }

    fn set_paused(&mut self, paused: bool) {
        if paused == self.paused {
            return;
        }
        self.paused = paused;
        self.gesture = Gesture::default();
        if paused {
            if self.overview {
                self.leave_overview();
            } else if self.shown && !self.anim.as_ref().is_some_and(|a| a.commit) {
                // Mid-gesture: land where the camera is rather than undo it.
                self.land(self.cam_x, self.cam_y, None);
            }
        }
        self.show_bg();
        self.dirty = true;
    }

    fn show_bg(&self) {
        let cmd = if self.paused || self.locked { SW_HIDE } else { SW_SHOWNOACTIVATE };
        unsafe {
            let _ = ShowWindowAsync(self.bg, cmd);
        }
    }

    /// Lay every window out in neat rows, keeping their reading order. Each
    /// one glides to its new spot while the camera frames the result.
    fn tidy(&mut self) {
        if !self.shown {
            if self.nothing_to_show() {
                return;
            }
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
        let label = if cfg.show_labels { (labels::LABEL_ROOM as f64 + 6.0) * self.scale() } else { 0.0 };
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
            room = (label / self.frame((0.0, 0.0, w, h)).2).ceil();
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
        let (cx, cy, z) = self.frame((ox, oy, ox + w, oy + h));
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
            self.minimized = wins::minimized_windows()
                .into_iter()
                .map(|h| WinInfo {
                    id: h.0 as isize,
                    title: wins::title(h),
                    exe: wins::exe_name(h),
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 0.0,
                    focused: false,
                })
                .collect();
            self.search = Some(self.ranked(String::new()));
        }
        // Zoom out to everything, fitted below the search panel.
        if let Some(b) = self.bounds().filter(|_| fresh) {
            let (x, y, z) = self.frame(b);
            self.animate_to(x, y, z, false, None);
        }
        self.shared.emit(UiEvent::FocusSearch);
        self.update_view();
    }

    /// Canvas windows first, then minimized ones.
    fn ranked(&self, query: String) -> SearchState {
        let mut all = self.infos();
        all.extend(self.minimized.iter().cloned());
        let results = search::rank(&query, &all);
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
        let panel = search::panel_rect(self.search.as_ref().unwrap_or(&empty), self.sw, self.scale());
        (FRAME_MARGIN, panel.3 + FRAME_MARGIN, self.sw - FRAME_MARGIN, self.sh - FRAME_MARGIN)
    }

    /// Enter: open the selected result. With nothing selected (no matches)
    /// the search stays as it is.
    fn search_accept(&mut self) {
        let Some(id) = self.search.as_ref().and_then(|s| s.results.get(s.selected).copied()) else { return };
        self.focus_window(id);
        self.search = None;
        self.minimized.clear();
        self.update_view();
    }

    /// Un-minimize a window picked in the search and land back at 1:1 where
    /// the overlay came up, handing it the focus.
    fn restore_minimized(&mut self, h: HWND) {
        wins::restore(h);
        let (x, y) = self.origin;
        self.animate_to(x, y, 1.0, true, Some(h));
    }

    fn close_search(&mut self) {
        self.minimized.clear();
        if self.search.take().is_some() {
            self.update_view();
        }
    }

    // ---- animation and hiding ---------------------------------------------------

    fn tick(&mut self) {
        // Fingers on the pad own the camera; nothing may fight them.
        if self.gesture.active() {
            self.cancel_anim();
            return;
        }
        let Some(a) = self.anim.as_ref() else {
            self.ticker.stop();
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
                self.ticker.stop();
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
        self.drag = None;
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
        if self.shown || !self.attached {
            return;
        }
        if !unsafe { IsWindow(Some(self.overlay)) }.as_bool() {
            log!("the overlay window is gone; quitting");
            self.quit();
            return;
        }
        self.pending_focus = None;
        self.drag = None;
        self.stop_timer(TIMER_REFRESH);
        self.refresh_armed = false;
        self.sync(true);
        self.origin = (self.cam_x, self.cam_y);
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
            w.shown = None;
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
    ///
    /// `live` is the gesture start: maximized windows are turned into normal
    /// ones and new windows get a spot (saved or free). Otherwise (idle, for
    /// the background's minimap) the list just mirrors the screen and new
    /// windows are left for the next live sync to place.
    fn sync(&mut self, live: bool) {
        let cfg = self.cfg();
        let mut next: Vec<Win> = Vec::new();
        let mut kinds = Vec::new();
        for h in wins::app_windows(true) {
            let maximized = unsafe { IsZoomed(h).as_bool() };
            let rect = if live && maximized {
                // Un-maximizing has to wait on the app; a hung one is left
                // off the canvas this time rather than stall the engine.
                if wins::is_hung(h) {
                    continue;
                }
                wins::unmaximize(h)
            } else {
                wins::window_rect(h)
            };
            let Some(r) = rect else { continue };
            let (w, hgt) = ((r.right - r.left) as f64, (r.bottom - r.top) as f64);
            let known = self.wins.iter().find(|o| o.hwnd == h);
            let (exe, class) = match known {
                Some(o) => (o.exe.clone(), o.class.clone()),
                None => (wins::exe_name(h), wins::class_name(h)),
            };
            let title = wins::title(h);
            let (x, y, placed) = match known {
                Some(o) if o.placed == Some(r) => (o.x, o.y, o.placed),
                // New, or moved/resized by the user since we placed it. While
                // idle the canvas mirrors the screen, so that's where it is.
                _ => (self.cam_x + r.left as f64, self.cam_y + r.top as f64, (!live).then_some(r)),
            };
            let unsettled = known.is_none_or(|o| o.unsettled);
            kinds.push(match (live, unsettled, maximized) {
                (false, ..) => Kind::Known,
                (true, true, _) => Kind::New,
                (true, false, true) => Kind::Unmaximized,
                (true, false, false) => Kind::Known,
            });
            next.push(Win {
                hwnd: h,
                title,
                exe,
                class,
                x,
                y,
                w,
                h: hgt,
                placed,
                thumb: 0,
                shown: None,
                unsettled: unsettled && !live,
            });
        }

        // A new window that was here last time goes back to its saved spot.
        // Every window is matched so known ones keep their own entries.
        if live && cfg.remember_layout && kinds.contains(&Kind::New) {
            let keys: Vec<WinKey> = next.iter().map(Win::key).collect();
            for (i, saved) in self.layout.positions(&keys).into_iter().enumerate() {
                if let (Kind::New, Some((x, y))) = (kinds[i], saved) {
                    (next[i].x, next[i].y, kinds[i]) = (x, y, Kind::Known);
                }
            }
        }

        // New and formerly maximized windows tend to land right on top of
        // others. Slide each one right until it has its own space. A new one
        // makes way for every settled window and for those above it; a
        // formerly maximized one only for windows above it, so the one the
        // user was looking at (on top) keeps its spot.
        for i in 0..next.len() {
            if kinds[i] == Kind::Known {
                continue;
            }
            let (mut x, y, w, h) = (next[i].x, next[i].y, next[i].w, next[i].h);
            for _ in 0..64 {
                let blocks = |j: usize| match kinds[i] {
                    Kind::New => kinds[j] == Kind::Known || j < i,
                    _ => j < i,
                };
                let blocker = next
                    .iter()
                    .enumerate()
                    .filter(|&(j, o)| j != i && blocks(j) && overlaps((x, y, w, h), o))
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
        let fg = unsafe { GetForegroundWindow() }.0 as isize;
        self.focused = self.wins.iter().find(|w| w.id() == fg).map(Win::id);
        self.dirty = true;
    }

    /// Move the real windows to match the canvas at zoom 1. With `wait`, the
    /// moves happen before this returns (except to hung apps).
    fn place_windows(&mut self, wait: bool) {
        let mut changed = false;
        for w in &mut self.wins {
            // Windows stops honouring coordinates past roughly +-32k.
            let x = (w.x - self.cam_x).round().clamp(-30000.0, 30000.0) as i32;
            let y = (w.y - self.cam_y).round().clamp(-30000.0, 30000.0) as i32;
            let mut flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
            if !wait || wins::is_hung(w.hwnd) {
                flags |= SWP_ASYNCWINDOWPOS;
            }
            unsafe {
                let _ = SetWindowPos(w.hwnd, None, x, y, 0, 0, flags);
            }
            let r = RECT { left: x, top: y, right: x + w.w as i32, bottom: y + w.h as i32 };
            w.placed = Some(r);
            let id = w.id();
            changed |= if wins::reachable(w.hwnd, &r) { self.parked.remove(&id) } else { self.parked.insert(id) };
        }
        if changed {
            wins::save_parked(&self.parked);
        }
    }

    /// Place the real windows, then hide the overlay once they've had a
    /// moment to get there.
    fn commit(&mut self, focus: Option<HWND>) {
        self.zoom = 1.0;
        self.overview = false;
        self.search = None;
        self.minimized.clear();
        self.drag = None;
        self.stop_timer(TIMER_WHEEL);
        self.place_windows(false);
        // A focus picked earlier in this overlay session still stands unless
        // this commit names its own.
        if let Some(h) = focus {
            self.pending_focus = Some(h);
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
        self.go_home(false);
        self.shared.emit(UiEvent::Quit);
        // If the UI thread is wedged, don't hang around.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(3));
            std::process::exit(0);
        });
    }

    /// The exit path: back to zoom 1 at the origin with real windows placed,
    /// the layout saved, the overlay down, and nothing left out of reach.
    fn go_home(&mut self, wait: bool) {
        if self.shown || self.zoom != 1.0 {
            (self.cam_x, self.cam_y) = self.origin;
            self.zoom = 1.0;
            self.place_windows(wait);
        }
        self.save_layout();
        self.stop_timer(TIMER_HIDE);
        self.unregister_esc();
        self.release_thumbnails();
        unsafe {
            let _ = ShowWindowAsync(self.overlay, SW_HIDE);
        }
        (self.shown, self.overview, self.search, self.hovered) = (false, false, None, None);
        self.dirty = true;
        // Judge each window by where we sent it, not where it is: a busy app
        // may not have moved yet. A move queued now lands after that one.
        let mut n = 0;
        for w in &self.wins {
            let Some(r) = w.placed.or_else(|| wins::window_rect(w.hwnd)) else { continue };
            if !wins::reachable(w.hwnd, &r) {
                wins::send_home(w.hwnd, n, wait && !wins::is_hung(w.hwnd));
                n += 1;
            }
        }
        // Then anything else out of reach (minimized, hidden, moved by hand).
        if !wait {
            std::thread::sleep(Duration::from_millis(150));
        }
        n += wins::rescue_all();
        log!("{n} windows brought back on screen");
    }

    /// Records the camera and window positions so a restart picks up where
    /// we left off. Always runs at zoom 1 (after a commit, or on quit).
    fn save_layout(&mut self) {
        if !self.cfg().remember_layout || self.layout_frozen {
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
            self.sync(false);
            return;
        }
        let Some((cx, cy)) = self.layout.camera() else {
            self.sync(false);
            return;
        };
        // Camera first, so unmatched windows map to their current screen spot.
        (self.cam_x, self.cam_y) = (cx, cy);
        self.sync(true);
        self.place_windows(false);
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
        // New motion: the overlay must stay up for it.
        self.stop_timer(TIMER_HIDE);
        if commit {
            // Where we'll rest at 1:1, so a later Esc comes back here rather
            // than undoing this move.
            self.origin = (x, y);
        }
        self.anim = Some(Anim {
            from: (self.cam_x, self.cam_y, self.zoom),
            to: (x, y, z),
            start: Instant::now(),
            commit,
            focus,
            moves: Vec::new(),
        });
        self.ticker.start();
    }

    /// Stops the animation where it is, except that windows on their way
    /// somewhere (tidy) land there, so an interrupted tidy is still tidy.
    fn cancel_anim(&mut self) {
        let Some(a) = self.anim.take() else { return };
        self.ticker.stop();
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
        screen_rect(w, (self.cam_x, self.cam_y, self.zoom))
    }

    fn cursor_on_canvas(&self) -> bool {
        let (x, y) = cursor();
        let h = unsafe { WindowFromPoint(POINT { x: x as i32, y: y as i32 }) };
        h == self.bg || h == self.overlay
    }

    fn index(&self, id: isize) -> Option<usize> {
        self.wins.iter().position(|w| w.id() == id)
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

    /// Physical pixels per logical pixel on the overlay, which is what the
    /// UI draws with (the primary monitor's real scale, not the system DPI
    /// fixed at sign-in).
    fn scale(&self) -> f64 {
        let dpi = match unsafe { GetDpiForWindow(self.overlay) } {
            0 => unsafe { GetDpiForSystem() },
            dpi => dpi,
        };
        dpi as f64 / 96.0
    }

    /// Point the thumbnails at the current camera, then queue a UI redraw.
    fn update_view(&mut self) {
        self.dirty = true;
        if !self.shown {
            return;
        }
        // The camera or the windows may have moved under a still mouse.
        if self.drag.is_none() {
            let hovered = self.hit(cursor()).map(|i| self.wins[i].id());
            self.hovered = hovered;
        }
        let cam = (self.cam_x, self.cam_y, self.zoom);
        let (sw, sh) = (self.sw as i32, self.sh as i32);
        let scale = self.scale();
        let matching = self.search.as_ref().map(|s| &s.results);
        // Thumbnails draw over GPUI, so they're cut back from the panels it
        // draws: the search panel at the top and the minimap in the corner.
        let panel = self.search.as_ref().map(|s| search::panel_rect(s, self.sw, scale));
        let map = (self.cfg().show_minimap && !self.wins.is_empty())
            .then(|| minimap::panel_rect(self.sw, self.sh, scale));
        for w in &mut self.wins {
            if w.thumb == 0 {
                continue;
            }
            let mut r = screen_rect(w, cam);
            let mut source = RECT { left: 0, top: 0, right: w.w as i32, bottom: w.h as i32 };
            let mut visible = r.right > 0 && r.bottom > r.top.max(0) && r.left < sw && r.top < sh;
            if let Some((pl, _, pr, pb)) = panel {
                let pb = pb.ceil() as i32;
                if r.top < pb && (r.left as f64) < pr && (r.right as f64) > pl {
                    source.top = ((pb - r.top) as f64 / cam.2).round() as i32;
                    r.top = pb;
                }
            }
            if let Some((ml, mt, mr, mb)) = map {
                let (ml, mt) = (ml.floor() as i32, mt.floor() as i32);
                if r.right > ml && r.bottom > mt && (r.left as f64) < mr && (r.top as f64) < mb {
                    // Keep whichever is bigger: the part above the minimap
                    // or the part left of it.
                    let above = (r.right - r.left) as i64 * (mt - r.top).max(0) as i64;
                    let left = (ml - r.left).max(0) as i64 * (r.bottom - r.top) as i64;
                    if above == 0 && left == 0 {
                        visible = false;
                    } else if above >= left {
                        source.bottom = source.top + ((mt - r.top) as f64 / cam.2).round() as i32;
                        r.bottom = mt;
                    } else {
                        source.right = ((ml - r.left) as f64 / cam.2).round() as i32;
                        r.right = ml;
                    }
                }
            }
            visible &= r.bottom > r.top;
            let dim = matching.is_some_and(|m| !m.contains(&w.id()));
            let next = Thumb { dest: r, source, opacity: if dim { DIMMED } else { 255 }, visible };
            // Off-screen thumbnails that stay off-screen need nothing at all.
            let same = w.shown.is_some_and(|s| s == next || (!s.visible && !visible));
            if same {
                continue;
            }
            w.shown = Some(next);
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION
                    | DWM_TNP_RECTSOURCE
                    | DWM_TNP_VISIBLE
                    | DWM_TNP_OPACITY
                    | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: r,
                rcSource: source,
                opacity: next.opacity,
                fVisible: visible.into(),
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
        self.shared.publish(Snapshot {
            cam_x: self.cam_x,
            cam_y: self.cam_y,
            zoom: self.zoom,
            screen_w: self.sw,
            screen_h: self.sh,
            mode: self.mode(),
            overlay_visible: self.shown,
            windows: self.infos(),
            minimized: if self.search.is_some() { self.minimized.clone() } else { Vec::new() },
            hovered: self.hovered,
            search: self.search.clone(),
            paused: self.paused,
            generation: 0,
        });
    }

    /// Publishes the snapshot if anything changed since the last one.
    pub fn flush(&mut self) {
        if self.dirty {
            self.publish();
        }
    }
}

/// Camera `(x, y, zoom)` that shows the canvas rect `(x0, y0, x1, y1)`
/// centred in the screen area `(l, t, r, b)`, never closer than 0.85.
fn fit_into((x0, y0, x1, y1): (f64, f64, f64, f64), (l, t, r, b): (f64, f64, f64, f64)) -> (f64, f64, f64) {
    let z = ((r - l) / (x1 - x0)).min((b - t) / (y1 - y0)).clamp(MIN_ZOOM, 0.85);
    ((x0 + x1) / 2.0 - (l + r) / 2.0 / z, (y0 + y1) / 2.0 - (t + b) / 2.0 / z, z)
}

fn screen_rect(w: &Win, (cam_x, cam_y, zoom): (f64, f64, f64)) -> RECT {
    let l = (w.x - cam_x) * zoom;
    let t = (w.y - cam_y) * zoom;
    RECT {
        left: l.round() as i32,
        top: t.round() as i32,
        right: (l + w.w * zoom).round() as i32,
        bottom: (t + w.h * zoom).round() as i32,
    }
}

/// Overlap beyond a few pixels of shadow/border.
fn overlaps(a: (f64, f64, f64, f64), b: &Win) -> bool {
    const SLACK: f64 = 8.0;
    a.0 < b.x + b.w - SLACK && b.x < a.0 + a.2 - SLACK && a.1 < b.y + b.h - SLACK && b.y < a.1 + a.3 - SLACK
}

/// The primary monitor's size in physical pixels.
fn screen_size() -> (f64, f64) {
    unsafe { (GetSystemMetrics(SM_CXSCREEN) as f64, GetSystemMetrics(SM_CYSCREEN) as f64) }
}

fn cursor() -> (f64, f64) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    (p.x as f64, p.y as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds frames of `(centroid, spread)` through the classifier the way
    /// `on_fingers` does, returning the first decided kind.
    fn run(frames: &[((f64, f64), f64)]) -> GestureKind {
        let (mut pan_ev, mut zoom_ev) = (0.0, 0.0);
        let spread0 = frames[0].1;
        for w in frames.windows(2) {
            let (((px, py), ps), ((cx, cy), s)) = (w[0], w[1]);
            pan_ev += (cx - px).hypot(cy - py);
            zoom_ev += (s - ps).abs();
            let kind = classify(pan_ev, zoom_ev, (s / spread0).ln());
            if kind != GestureKind::Undecided {
                return kind;
            }
        }
        GestureKind::Undecided
    }

    #[test]
    fn straight_drag_with_spread_drift_pans() {
        // 3 px a frame sideways while the fingers creep 1 px apart.
        let frames: Vec<_> = (0..10).map(|i| ((i as f64 * 3.0, 0.0), 120.0 + i as f64)).collect();
        assert_eq!(run(&frames), GestureKind::Pan);
    }

    #[test]
    fn diagonal_drag_pans() {
        let frames: Vec<_> = (0..10).map(|i| ((i as f64 * 2.0, i as f64 * -2.0), 120.0)).collect();
        assert_eq!(run(&frames), GestureKind::Pan);
    }

    #[test]
    fn pinch_out_and_in_zooms() {
        let out: Vec<_> = (0..10).map(|i| ((500.0, 300.0), 100.0 + i as f64 * 3.0)).collect();
        assert_eq!(run(&out), GestureKind::Zoom);
        let inward: Vec<_> = (0..10).map(|i| ((500.0, 300.0), 100.0 - i as f64 * 3.0)).collect();
        assert_eq!(run(&inward), GestureKind::Zoom);
    }

    #[test]
    fn pinch_with_centroid_wobble_zooms() {
        let frames: Vec<_> = (0..10)
            .map(|i| {
                let wobble = if i % 2 == 0 { 0.3 } else { -0.3 };
                ((500.0 + wobble, 300.0 - wobble), 100.0 + i as f64 * 3.0)
            })
            .collect();
        assert_eq!(run(&frames), GestureKind::Zoom);
    }

    #[test]
    fn tiny_motion_is_undecided() {
        let frames = [((0.0, 0.0), 100.0), ((1.0, 1.0), 101.0), ((2.0, 1.0), 100.0)];
        assert_eq!(run(&frames), GestureKind::Undecided);
        assert_eq!(classify(3.0, 4.0, 0.03), GestureKind::Undecided);
    }

    #[test]
    fn spread_jitter_without_net_change_waits() {
        // Lots of spread motion, but it hasn't gone anywhere: not a pinch yet.
        assert_eq!(classify(1.0, 10.0, 0.01), GestureKind::Undecided);
        // Then the centroid moves off: a drag.
        assert_eq!(classify(6.0, 10.0, 0.01), GestureKind::Pan);
    }

    #[test]
    fn wheel_burst_keeps_its_kind_until_quiet() {
        let t = Instant::now();
        let soon = t + Duration::from_millis(20);
        let later = t + Duration::from_millis(WHEEL_IDLE_MS as u64 + 1);
        assert!(wheel_accepts(None, true, t));
        assert!(wheel_accepts(Some((true, t)), true, soon));
        assert!(!wheel_accepts(Some((true, t)), false, soon));
        assert!(!wheel_accepts(Some((false, t)), true, soon));
        assert!(wheel_accepts(Some((false, t)), true, later));
    }
}
