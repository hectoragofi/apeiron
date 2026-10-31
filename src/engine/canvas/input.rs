//! Touchpad and mouse input on the canvas: fingers become camera motion,
//! taps and four-finger swipes; clicks drag windows, draw a marquee or pan,
//! and the wheel pans or zooms.

use std::time::Instant;

use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_MENU};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::engine::touchpad::Contact;
use crate::engine::{mouse, snap, wins};
use crate::shared::{EDGE_B, EDGE_L, EDGE_R, EDGE_T};

use super::{
    cursor, Canvas, CORNER, FINGERS_GONE_SECS, GRIP, HOVER_FOCUS_MS, MAX_ZOOM, MIN_H, MIN_W, MOUSE_ZOOM_STEP,
    RESIZE_TAIL, SNAP, SWIPE4_PX, TAP_SLOP, TAP_TIME, TIMER_HOVER, TIMER_WHEEL, WHEEL_IDLE_MS, WHEEL_QUIET_SECS,
};
use super::gesture::{centroid, wheel_accepts, Gesture, GestureKind, Motion};
use super::resize::Resizing;
use super::thumbs::vis_screen_rect;

pub(super) struct Drag {
    /// The window being dragged (by id: the list is rebuilt on every sync),
    /// or None for panning or a selection box.
    target: Option<isize>,
    /// Every window moving with it: the selection, if it's part of it.
    group: Vec<isize>,
    pub(super) marquee: Option<Marquee>,
    additive: bool,
    /// Edges being dragged to resize the target (`EDGE_*` bits), from its
    /// canvas rect `start` and the screen point `from`.
    pub(super) resize: u8,
    start: (f64, f64, f64, f64),
    from: (f64, f64),
    last: (f64, f64),
    moved: f64,
    /// How far snapping has pulled the group off the pointer, canvas pixels,
    /// and the lines showing what it lined up with.
    nudge: (f64, f64),
    pub(super) guides: Vec<snap::Guide>,
}

/// A selection box being drawn on the overview, in screen pixels.
pub(super) struct Marquee {
    from: (f64, f64),
    to: (f64, f64),
    /// Selected before the box (kept when it was drawn with Ctrl or Shift).
    base: Vec<isize>,
}

impl Marquee {
    pub(super) fn rect(&self) -> (f64, f64, f64, f64) {
        let (a, b) = (self.from, self.to);
        (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
    }
}

/// A four-finger touch that's ours (Windows' own four-finger gestures are off).
pub(super) struct Swipe4 {
    /// Centroid it's measured from, and with how many fingers.
    start: (f64, f64),
    n: usize,
    since: Instant,
    /// Farthest the centroid got from `start` (px).
    travel: f64,
    /// It has done its thing (a swipe fires once).
    done: bool,
}

impl Canvas {
    // ---- touchpad ----------------------------------------------------------

    pub fn on_fingers(&mut self, fingers: &[Contact]) {
        if !self.attached || self.quitting {
            return;
        }
        let n = fingers.len();
        let now = Instant::now();
        let lifted = self.last_touch.is_none_or(|t| (now - t).as_secs_f64() > FINGERS_GONE_SECS);
        self.taps(fingers, now, lifted);
        self.last_touch = Some(now);
        if n == 0 || lifted {
            self.swipe_block = false;
            self.swipe4 = None;
        }
        if n >= 4 {
            // Whatever three fingers started is undone, and they're ignored
            // until every finger lifts. The swipe itself is Windows' unless
            // its four-finger gestures are turned off; then it's ours.
            if !self.swipe_block {
                self.swipe_block = true;
                if wins::os_four_finger() {
                    self.abort_motion();
                } else {
                    // Ours: three fingers landing first may have started a
                    // move; glide back from it instead of dropping the
                    // overlay, so the swipe's animation starts smoothly.
                    self.gesture = Gesture::default();
                    if self.shown && !self.overview && self.anim.is_none() {
                        let (x, y) = self.origin;
                        self.animate_to(x, y, 1.0, true, None);
                    }
                    self.swipe4 = Some(Swipe4 { start: centroid(fingers), n, since: now, travel: 0.0, done: false });
                }
            }
            self.gesture = Gesture::default();
            self.four_finger_swipe(fingers);
            return;
        }
        if self.swipe_block {
            return;
        }
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
            if self.gesture.engaged {
                self.gesture_end();
            }
            self.gesture = Gesture::default();
            return;
        }
        let was = self.gesture.kind;
        let motion = self.gesture.feed(now, fingers);
        if was == GestureKind::Undecided && self.gesture.kind != GestureKind::Undecided {
            self.gesture.anchor = cursor();
            // The fingers own the camera now: a glide still running (the
            // last flick, a fly-to) or a pending hide gives way.
            self.cancel_anim();
            self.cancel_hide();
            self.drag = None;
            self.begin();
        }
        match motion {
            Some(Motion::Pan(dx, dy)) => self.pan(dx * cfg.pan_speed, dy * cfg.pan_speed),
            Some(Motion::Zoom(f)) => self.zoom_about(f.powf(cfg.pinch_speed), self.gesture.anchor),
            None => return,
        }
        self.queue_view();
    }

    /// A four-finger swipe does what it's set to in Settings (by default:
    /// up opens the overview, down leaves it, sideways switches desktops).
    fn four_finger_swipe(&mut self, fingers: &[Contact]) {
        let c = centroid(fingers);
        let Some(sw) = self.swipe4.as_mut() else { return };
        if sw.n != fingers.len() {
            // A finger joined or left: the centroid jumps, so measure afresh.
            (sw.start, sw.n) = (c, fingers.len());
            return;
        }
        let (dx, dy) = (c.0 - sw.start.0, c.1 - sw.start.1);
        sw.travel = sw.travel.max(dx.hypot(dy));
        if sw.done || dx.hypot(dy) < SWIPE4_PX {
            return;
        }
        sw.done = true;
        let cfg = self.cfg();
        let action = match (dy.abs() > dx.abs(), dx < 0.0, dy < 0.0) {
            (true, _, true) => cfg.four_up,
            (true, _, false) => cfg.four_down,
            (false, true, _) => cfg.four_left,
            (false, false, _) => cfg.four_right,
        };
        self.run_gesture(action);
    }

    /// Taps: four fingers (when they're ours) or three, down and up quickly
    /// without moving, run what they're set to.
    fn taps(&mut self, fingers: &[Contact], now: Instant, lifted: bool) {
        let n = fingers.len();
        if lifted {
            self.tap3 = None;
        }
        // Four: judged as soon as the first finger lifts.
        if n < 4 {
            if let Some(sw) = self.swipe4.as_mut().filter(|sw| !sw.done) {
                sw.done = true;
                if now - sw.since < TAP_TIME && sw.travel < TAP_SLOP && !lifted {
                    let a = self.cfg().four_tap;
                    self.run_gesture(a);
                }
            }
        }
        match (n, self.tap3.as_mut()) {
            (3, None) => {
                self.tap3 = Some((now, centroid(fingers), 0.0));
            }
            (3, Some((_, start, travel))) => {
                let c = centroid(fingers);
                *travel = travel.max((c.0 - start.0).hypot(c.1 - start.1));
                if self.gesture.engaged {
                    *travel = f64::INFINITY;
                }
            }
            (n, Some(&mut (since, _, travel))) if n < 3 => {
                self.tap3 = None;
                if now - since < TAP_TIME && travel < TAP_SLOP {
                    let a = self.cfg().three_tap;
                    self.run_gesture(a);
                }
            }
            (n, Some(_)) if n > 3 => self.tap3 = None,
            _ => {}
        }
    }

    /// Whether fingers that drive the canvas are resting on the pad; they
    /// may still turn into a gesture.
    pub(super) fn fingers_down(&self) -> bool {
        !self.gesture.prev.is_empty() && self.last_touch.is_some_and(|t| t.elapsed().as_secs_f64() < FINGERS_GONE_SECS)
    }

    fn gesture_end(&mut self) {
        let (vx, vy) = self.gesture.release_velocity(Instant::now());
        let speed = self.cfg().pan_speed;
        self.settle(vx * speed, vy * speed);
    }

    // ---- mouse on the overlay ----------------------------------------------

    /// `additive` (Ctrl or Shift held): a click adds or removes a window
    /// from the selection and a box adds to it.
    pub(super) fn mouse_down(&mut self, p: (f64, f64), additive: bool) {
        // A click anywhere else just closes an open menu.
        if self.menu.is_some() {
            self.close_menu();
            return;
        }
        if !self.shown {
            return;
        }
        self.cancel_hide();
        if let Some((i, resize)) = self.edge_at(p) {
            self.cancel_anim();
            self.raise(i);
            let w = &self.wins[0];
            // Browsers stop painting while off screen or covered, so they'd
            // never redraw at the new size: park the real window on screen,
            // above the others (still under the overlay) until the next
            // commit puts it back.
            let work = wins::primary_work();
            let flags = SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
            unsafe {
                let _ = SetWindowPos(w.hwnd, Some(HWND_TOP), work.left, work.top, 0, 0, flags);
            }
            let start = (w.x, w.y, w.w, w.h);
            self.drag = Some(Drag {
                target: Some(w.id()),
                group: vec![w.id()],
                marquee: None,
                additive: false,
                resize,
                start,
                from: p,
                last: p,
                moved: 0.0,
                nudge: (0.0, 0.0),
                guides: Vec::new(),
            });
            // Every frame from here on the thumbnail follows the real window.
            self.ticker.start();
            return;
        }
        let target = self.hit(p).map(|i| self.wins[i].id());
        // A press on empty canvas lets a glide back to real windows finish
        // (often a tap right after a swipe); anything else takes over.
        if target.is_none() && self.anim.as_ref().is_some_and(|a| a.commit) {
            return;
        }
        self.cancel_anim();
        // Dragging a selected window takes the whole selection along.
        let group = match target {
            Some(id) if self.selected.contains(&id) => self.with_partners(&self.selected),
            Some(id) => self.stuck_with(id),
            None => Vec::new(),
        };
        // In the overview a drag over empty canvas draws a selection box;
        // elsewhere it pans.
        let marquee = (target.is_none() && self.overview).then(|| {
            if !additive {
                self.selected.clear();
            }
            Marquee { from: p, to: p, base: self.selected.clone() }
        });
        self.dirty = true;
        self.drag = Some(Drag { target, group, marquee, additive, resize: 0,
            start: (0.0, 0.0, 0.0, 0.0),
            from: p,
            last: p,
            moved: 0.0,
            nudge: (0.0, 0.0),
            guides: Vec::new(),
        });
    }

    pub(super) fn mouse_move(&mut self, p: (f64, f64)) {
        let Some(d) = self.drag.as_mut() else {
            self.set_hover(p);
            return;
        };
        let (dx, dy) = (p.0 - d.last.0, p.1 - d.last.1);
        d.last = p;
        d.moved += dx.abs() + dy.abs();
        if let Some(m) = d.marquee.as_mut() {
            m.to = p;
            let (l, t, r, b) = m.rect();
            let mut sel = m.base.clone();
            let cam = (self.cam_x, self.cam_y, self.zoom);
            for w in &self.wins {
                let s = vis_screen_rect(w, cam);
                let inside = (s.left as f64) < r && (s.right as f64) > l && (s.top as f64) < b && (s.bottom as f64) > t;
                if inside && !sel.contains(&w.id()) {
                    sel.push(w.id());
                }
            }
            self.selected = sel;
            self.queue_view();
            return;
        }
        if d.resize != 0 {
            let (e, start, from, zoom) = (d.resize, d.start, d.from, self.zoom);
            let (dx, dy) = ((p.0 - from.0) / zoom, (p.1 - from.1) / zoom);
            let (mut x, mut y, mut ww, mut wh) = start;
            if e & EDGE_L != 0 {
                ww = (start.2 - dx).max(MIN_W);
                x = start.0 + start.2 - ww;
            } else if e & EDGE_R != 0 {
                ww = (start.2 + dx).max(MIN_W);
            }
            if e & EDGE_T != 0 {
                wh = (start.3 - dy).max(MIN_H);
                y = start.1 + start.3 - wh;
            } else if e & EDGE_B != 0 {
                wh = (start.3 + dy).max(MIN_H);
            }
            let Some(id) = d.target else { return };
            // The next tick asks the window for this size and follows it.
            self.resizing = Some(Resizing { id, edges: e, rect: (x, y, ww, wh), until: None });
            self.queue_view();
            return;
        }
        match d.target {
            Some(id) => {
                let (zoom, group) = (self.zoom, d.group.clone());
                let Some(i) = self.index(id) else {
                    // It closed mid-drag.
                    self.drag = None;
                    return;
                };
                if i != 0 {
                    self.raise(i);
                }
                // Follow the pointer, then snap afresh from there: last
                // move's pull is taken back first so it never accumulates.
                let old = self.drag.as_ref().map_or((0.0, 0.0), |d| d.nudge);
                for w in self.wins.iter_mut().filter(|w| group.contains(&w.id())) {
                    w.x += dx / zoom - old.0;
                    w.y += dy / zoom - old.1;
                }
                let free = unsafe { GetKeyState(VK_MENU.0 as i32) } < 0;
                let (nudge, guides) =
                    if self.cfg().window_snap && !free { self.snap_group(&group) } else { ((0.0, 0.0), Vec::new()) };
                for w in self.wins.iter_mut().filter(|w| group.contains(&w.id())) {
                    w.x += nudge.0;
                    w.y += nudge.1;
                }
                if let Some(d) = self.drag.as_mut() {
                    (d.nudge, d.guides) = (nudge, guides);
                }
                self.dirty = true;
            }
            None => self.pan(dx, dy),
        }
        self.queue_view();
    }

    pub(super) fn mouse_up(&mut self) {
        let Some(d) = self.drag.take() else { return };
        self.dirty = true;
        if let Some(r) = self.resizing.as_mut() {
            // Keep following the window a moment longer while the app
            // catches up with the last size.
            r.until = Some(Instant::now() + RESIZE_TAIL);
            self.follow_resize();
        }
        if d.marquee.is_some() || d.resize != 0 {
            return;
        }
        if d.moved < 4.0 {
            if let Some(id) = d.target {
                if d.additive {
                    match self.selected.iter().position(|&s| s == id) {
                        Some(k) => {
                            self.selected.remove(k);
                        }
                        None => self.selected.push(id),
                    }
                    return;
                }
                self.selected.clear();
                if let Some(i) = self.index(id) {
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
        }
        // Outside the overview nothing else would ever settle the canvas. A
        // window just dropped near 1:1 stays where it was put (no framing)
        // and gets the focus.
        if !self.overview && self.anim.is_none() {
            let dropped = d.target.and_then(|id| self.index(id)).filter(|_| (self.zoom - 1.0).abs() < SNAP);
            match dropped {
                Some(i) => {
                    let h = self.wins[i].hwnd;
                    self.land(self.cam_x, self.cam_y, Some(h));
                }
                None => self.settle(0.0, 0.0),
            }
        }
    }

    /// The window edge (or corner) under `p` on the overlay, as `EDGE_*`
    /// bits: a band either side of the visible frame. Only the window on
    /// top at `p` counts.
    fn edge_at(&self, p: (f64, f64)) -> Option<(usize, u8)> {
        if !self.shown {
            return None;
        }
        let cam = (self.cam_x, self.cam_y, self.zoom);
        for (i, w) in self.wins.iter().enumerate() {
            let r = vis_screen_rect(w, cam);
            let (l, t, rr, b) = (r.left as f64, r.top as f64, r.right as f64, r.bottom as f64);
            if p.0 < l - GRIP || p.0 > rr + GRIP || p.1 < t - GRIP || p.1 > b + GRIP {
                continue;
            }
            if rr - l < 4.0 * GRIP || b - t < 4.0 * GRIP {
                return None;
            }
            let side = |a: f64, b: f64| {
                if (p.0 - a).abs() <= GRIP { 1 } else if (p.0 - b).abs() <= GRIP { 2 } else { 0 }
            };
            let (h_edge, v_edge) = (side(l, rr), {
                if (p.1 - t).abs() <= GRIP { 1 } else if (p.1 - b).abs() <= GRIP { 2 } else { 0 }
            });
            let mut e = 0;
            // On a side, near enough a corner grabs the corner.
            if h_edge != 0 || v_edge != 0 {
                let horiz = if h_edge != 0 { h_edge } else if p.0 - l <= CORNER { 1 } else if rr - p.0 <= CORNER { 2 } else { 0 };
                let vert = if v_edge != 0 { v_edge } else if p.1 - t <= CORNER { 1 } else if b - p.1 <= CORNER { 2 } else { 0 };
                e |= [0, EDGE_L, EDGE_R][horiz] | [0, EDGE_T, EDGE_B][vert];
            }
            return (e != 0).then_some((i, e));
        }
        None
    }

    fn set_hover(&mut self, p: (f64, f64)) {
        let edge = self.edge_at(p).map_or(0, |(_, e)| e);
        if edge != self.hover_edge {
            self.hover_edge = edge;
            self.dirty = true;
        }
        let hovered = if self.shown { self.hit(p).map(|i| self.wins[i].id()) } else { None };
        if hovered != self.hovered {
            self.hovered = hovered;
            self.dirty = true;
            if self.overview && hovered.is_some() && self.cfg().hover_focus {
                unsafe {
                    SetTimer(Some(self.hwnd), TIMER_HOVER, HOVER_FOCUS_MS, None);
                }
            }
        }
    }

    /// The pointer has rested on a window in the overview: hand it the
    /// keyboard, leaving the camera where it is. Its thumbnail is live, so
    /// what's typed shows up right there.
    pub(super) fn hover_focus(&mut self) {
        self.stop_timer(TIMER_HOVER);
        let busy = self.drag.is_some() || self.search.is_some() || self.menu.is_some();
        if !self.overview || busy || !self.cfg().hover_focus {
            return;
        }
        let Some(i) = self.hovered.and_then(|id| self.index(id)) else { return };
        let h = self.wins[i].hwnd;
        if unsafe { GetForegroundWindow() } != h {
            wins::activate(h);
        }
        self.focused = Some(h.0 as isize);
    }

    /// Mouse-wheel input from the overlay; pinch comes with Ctrl held. Works
    /// while paused too: the overlay is only up then because the user asked.
    pub(super) fn wheel(&mut self, dx: f64, dy: f64, pinch: bool, p: (f64, f64)) {
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
        self.cancel_hide();
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
        self.queue_view();
    }

    /// A mouse wheel: each notch zooms about the cursor (Shift: pans
    /// sideways). It opens the overview, so the canvas stays up between
    /// notches; zooming all the way back in lands on the windows.
    pub(super) fn mouse_wheel(&mut self, dx: f64, dy: f64, pinch: bool, shift: bool, p: (f64, f64)) {
        if self.search.is_some() {
            return self.wheel(dx, dy, pinch, p);
        }
        let zoom_in = !shift && dy > 0.0;
        // Already at 1:1 on the real windows: nothing to zoom into.
        if !self.shown && (zoom_in || self.nothing_to_show()) {
            return;
        }
        if self.menu.is_some() {
            self.close_menu();
        }
        self.cancel_hide();
        self.cancel_anim();
        self.stop_timer(TIMER_WHEEL);
        if !self.shown {
            self.begin();
        }
        if !self.overview {
            self.enter_overview();
        }
        if shift {
            self.pan(dx + dy, 0.0);
        } else if dy == 0.0 {
            self.pan(dx, 0.0);
        } else {
            self.zoom_about((dy / 60.0 * MOUSE_ZOOM_STEP).exp(), p);
        }
        self.queue_view();
        if zoom_in && self.zoom >= MAX_ZOOM - 1e-9 {
            self.settle(0.0, 0.0);
        }
    }

    /// From the mouse hook: a middle-button pan (with wheel zoom) over any
    /// app, or a bound button or hot corner.
    pub fn on_mouse_hook(&mut self, wp: usize, p: (f64, f64)) {
        if self.paused {
            return;
        }
        let code = wp & 0xff;
        match code {
            mouse::PAN_START => self.pan_start(p),
            mouse::PAN_MOVE => self.mouse_move(p),
            mouse::PAN_ZOOM => {
                if self.drag.is_none() {
                    self.pan_start(p);
                }
                if !self.shown {
                    return;
                }
                let delta = (wp >> 16) as u16 as i16 as f64;
                self.cancel_anim();
                self.zoom_about((delta / 120.0 * MOUSE_ZOOM_STEP).exp(), p);
                self.queue_view();
            }
            mouse::PAN_END => {
                // Zoomed out: stay there to browse. Near 1:1: land.
                if self.shown && (self.zoom - 1.0).abs() >= SNAP && !self.overview {
                    self.enter_overview();
                }
                self.mouse_up();
            }
            _ => {
                let a = mouse::action(&self.cfg(), code, (wp >> 8) & 0xff);
                self.run_gesture(a);
            }
        }
    }

    /// Middle button down: a drag that only pans, on windows or not. At 1:1
    /// it lands when let go, as a touchpad swipe would.
    pub(super) fn pan_start(&mut self, p: (f64, f64)) {
        if self.menu.is_some() {
            self.close_menu();
        }
        if !self.shown {
            if self.nothing_to_show() {
                return;
            }
            self.begin();
        }
        self.cancel_hide();
        self.cancel_anim();
        self.drag = Some(Drag {
            target: None,
            group: Vec::new(),
            marquee: None,
            additive: false,
            resize: 0,
            start: (0.0, 0.0, 0.0, 0.0),
            from: p,
            last: p,
            moved: 0.0,
            nudge: (0.0, 0.0),
            guides: Vec::new(),
        });
        self.dirty = true;
    }

    pub(super) fn wheel_idle(&mut self) {
        self.stop_timer(TIMER_WHEEL);
        if self.shown && self.anim.is_none() && self.drag.is_none() {
            self.settle(0.0, 0.0);
        }
    }
}
