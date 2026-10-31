//! The camera: where it rests after a gesture, flying it to a window or to
//! fit them all, and the animation that gets it there.

use std::time::Instant;

use windows::Win32::Foundation::HWND;

use crate::engine::snap;
use crate::settings::Settings;

use super::{cursor, Canvas, Win, ANIM_SECS, FIT_MARGIN, FLICK_MIN, MAX_ZOOM, MIN_ZOOM, SNAP, SNAP_REACH, WARMUP_FRAMES};

/// A window gliding between canvas positions: (id, from, to).
pub(super) type WinMove = (isize, (f64, f64), (f64, f64));

pub(super) struct Anim {
    from: (f64, f64, f64),
    pub(super) to: (f64, f64, f64),
    start: Instant,
    /// Hand control back to real windows when done.
    pub(super) commit: bool,
    focus: Option<HWND>,
    /// Windows that move on the canvas during the animation (tidy).
    pub(super) moves: Vec<WinMove>,
    /// Finished, but the commit waits for resting fingers to lift.
    held: bool,
    /// Frames to wait before the clock starts: the overlay has only just
    /// been shown and its first frame isn't on screen yet.
    warmup: u8,
}

impl Canvas {
    /// Motion has stopped: glide out the flick, then hand back to real
    /// windows if we're near 1:1, otherwise stay zoomed out. The smart
    /// camera (each part a setting) bends where it comes to rest.
    pub(super) fn settle(&mut self, vx: f64, vy: f64) {
        let cfg = self.cfg();
        let tx = self.cam_x - vx * cfg.flick / self.zoom;
        let ty = self.cam_y - vy * cfg.flick / self.zoom;
        // Near 1:1 always means "back to the windows", from the overview too;
        // a search stays up until it's answered or closed.
        let near_one = (self.zoom - 1.0).abs() < SNAP;
        let smart = self.search.is_none();
        if smart && cfg.magnetic_flick && vx.hypot(vy) * cfg.flick > FLICK_MIN {
            if let Some(i) = self.flick_target(tx, ty) {
                let z = if near_one { 1.0 } else { self.zoom };
                let (x, y) = snap::centred(self.wins[i].vis(), (self.sw / z, self.sh / z));
                if near_one {
                    let h = self.wins[i].hwnd;
                    self.animate_to(x, y, 1.0, true, Some(h));
                } else {
                    self.animate_to(x, y, z, false, None);
                    if !self.overview {
                        self.enter_overview();
                    }
                }
                return;
            }
        }
        if near_one && smart {
            self.land(tx, ty, None);
            return;
        }
        let (x, y, z) = match smart.then(|| self.rest_zoomed_out(tx, ty, &cfg)).flatten() {
            Some(rest) => rest,
            None => (tx, ty, self.zoom),
        };
        self.animate_to(x, y, z, false, None);
        if !self.overview {
            self.enter_overview();
        }
    }

    /// Glide to zoom 1 from camera `(tx, ty)` at the current zoom, keeping
    /// the point under the cursor fixed, then hand back to real windows.
    /// With no window to focus given, it frames (and focuses) the one it
    /// lands on, if that's on.
    pub(super) fn land(&mut self, tx: f64, ty: f64, focus: Option<HWND>) {
        let p = cursor();
        let (wx, wy) = (tx + p.0 / self.zoom, ty + p.1 / self.zoom);
        let (mut x, mut y, mut focus) = (wx - p.0, wy - p.1, focus);
        if focus.is_none() && self.search.is_none() && self.cfg().land_framed {
            if let Some(((fx, fy), i)) = snap::framing(&self.vis_rects(), (x, y, self.sw, self.sh)) {
                (x, y, focus) = (fx, fy, Some(self.wins[i].hwnd));
            }
        }
        self.animate_to(x, y, 1.0, true, focus);
    }

    /// The window a flick toward camera `(tx, ty)` should stop on.
    fn flick_target(&self, tx: f64, ty: f64) -> Option<usize> {
        let (vw, vh) = (self.sw / self.zoom, self.sh / self.zoom);
        let from = (self.cam_x + vw / 2.0, self.cam_y + vh / 2.0);
        let travel = (tx - self.cam_x, ty - self.cam_y);
        let reach = (travel.0.hypot(travel.1) * 3.0).max(vw.max(vh) * 1.5);
        snap::flick_target(&self.vis_rects(), from, travel, reach)
    }

    /// Where a zoomed-out camera headed for `(tx, ty)` rests instead: fitting
    /// the window or group a pinch nearly fits, or centred on the groups
    /// mostly in view. As `(x, y, zoom)`.
    fn rest_zoomed_out(&self, tx: f64, ty: f64, cfg: &Settings) -> Option<(f64, f64, f64)> {
        let rects = self.vis_rects();
        let all = snap::hull(&rects)?;
        let groups: Vec<snap::Rect> =
            snap::clusters(&rects, snap::CLUSTER_GAP).iter().map(|g| snap::bbox(&rects, g)).collect();
        if cfg.zoom_fit {
            // A pinch zooms about the cursor, so that's what it's after.
            let p = cursor();
            let at = (tx + p.0 / self.zoom, ty + p.1 / self.zoom);
            let mut candidates: Vec<snap::Rect> =
                rects.iter().chain(&groups).copied().filter(|&r| snap::contains(r, at)).collect();
            candidates.push(all);
            let fit = snap::zoom_fit(&candidates, self.zoom, (self.sw, self.sh), FIT_MARGIN, MIN_ZOOM, 1.0 - SNAP);
            if let Some((r, z)) = fit {
                let (x, y) = snap::centred(r, (self.sw / z, self.sh / z));
                return Some((x, y, z));
            }
        }
        if cfg.overview_snap {
            let view = (tx, ty, self.sw / self.zoom, self.sh / self.zoom);
            if let Some(((x, y), _)) = snap::framing(&groups, view) {
                return Some((x, y, self.zoom));
            }
        }
        None
    }

    fn vis_rects(&self) -> Vec<snap::Rect> {
        self.wins.iter().map(Win::vis).collect()
    }

    /// Where snapping pulls the windows in `group`: their bounds lined up
    /// with the other windows around them.
    pub(super) fn snap_group(&self, group: &[isize]) -> ((f64, f64), Vec<snap::Guide>) {
        let (mine, others): (Vec<&Win>, Vec<&Win>) = self.wins.iter().partition(|w| group.contains(&w.id()));
        let mine: Vec<snap::Rect> = mine.into_iter().map(Win::vis).collect();
        let others: Vec<snap::Rect> = others.into_iter().map(Win::vis).collect();
        match snap::hull(&mine) {
            Some(r) => snap::align(r, &others, self.cfg().gap, SNAP_REACH / self.zoom),
            None => ((0.0, 0.0), Vec::new()),
        }
    }

    /// Zoom out to fit every window, or back in if already zoomed out.
    pub(super) fn toggle_overview(&mut self) {
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
    pub(super) fn nothing_to_show(&self) -> bool {
        // While idle, `sync` keeps `wins` mirroring the screen.
        let empty = self.wins.is_empty();
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
    pub(super) fn frame(&self, b: (f64, f64, f64, f64)) -> (f64, f64, f64) {
        match self.search {
            Some(_) => fit_into(b, self.search_area()),
            None => self.fit(b),
        }
    }

    /// Zoom back to 1:1 where the overlay first came up.
    pub(super) fn leave_overview(&mut self) {
        if !self.shown {
            return;
        }
        let (x, y) = self.origin;
        self.animate_to(x, y, 1.0, true, None);
    }

    /// Outside the overview (e.g. the background's minimap) this brings the
    /// overlay up and lands back on real windows at 1:1.
    pub(super) fn fly_to(&mut self, x: f64, y: f64) {
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
    pub(super) fn focus_window(&mut self, id: isize) {
        if !self.shown {
            self.begin();
        }
        if let Some(i) = self.index(id) {
            self.focus(i);
        } else if self.minimized.iter().any(|w| w.id == id) || self.away.iter().any(|w| w.id() == id) {
            self.search = None;
            self.minimized.clear();
            self.restore_minimized(HWND(id as *mut _));
        }
    }

    // ---- animation and hiding ---------------------------------------------------

    /// Once per composed frame while the ticker runs: steps the animation,
    /// follows a resize, applies input queued since the last frame, then
    /// stops the ticker once nothing needs frames.
    pub(super) fn tick(&mut self) {
        let started = self.trace.begin();
        self.step();
        if self.pacer.pending() {
            self.update_view();
        }
        let busy = self.anim.is_some() || self.resizing.is_some();
        let holding = (self.gesture.engaged && self.fingers_down()) || self.drag.is_some();
        if !self.pacer.keep_going(busy, holding, Instant::now()) {
            self.ticker.stop();
            self.trace.stopped();
            return;
        }
        self.trace.end(started, &self.shared);
    }

    fn step(&mut self) {
        // Fingers on the pad own the camera; nothing may fight them.
        if self.gesture.engaged {
            self.cancel_anim();
            return;
        }
        let down = self.fingers_down();
        if self.resizing.is_some() {
            self.follow_resize();
        }
        let Some(a) = self.anim.as_ref() else { return };
        if a.held && down {
            return;
        }
        if a.warmup > 0 {
            if let Some(a) = self.anim.as_mut() {
                a.warmup -= 1;
                a.start = Instant::now();
            }
            return;
        }
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
            if down && self.anim.as_ref().is_some_and(|a| a.commit) {
                // Resting fingers may still become a gesture: don't move the
                // real windows and drop the overlay from under them. Wait at
                // the end until they lift or take over.
                if let Some(a) = self.anim.as_mut() {
                    a.held = true;
                }
                return;
            }
            if let Some(a) = self.anim.take() {
                if a.commit {
                    self.commit(a.focus);
                } else if std::mem::take(&mut self.launch_tidy) {
                    self.leave_overview();
                }
            }
        }
    }

    pub(super) fn focus(&mut self, i: usize) {
        let w = &self.wins[i];
        let (tx, ty) = self.centre_on((w.x, w.y, w.w, w.h));
        let h = w.hwnd;
        self.animate_to(tx, ty, 1.0, true, Some(h));
    }

    /// Camera at zoom 1 that centres the canvas rect `(x, y, w, h)`, or pins
    /// its top-left if it's bigger than the screen.
    pub(super) fn centre_on(&self, (x, y, w, h): (f64, f64, f64, f64)) -> (f64, f64) {
        let tx = if w < self.sw { x + w / 2.0 - self.sw / 2.0 } else { x };
        let ty = if h < self.sh { y + h / 2.0 - self.sh / 2.0 } else { y };
        (tx, ty)
    }

    pub(super) fn animate_to(&mut self, x: f64, y: f64, z: f64, commit: bool, focus: Option<HWND>) {
        self.cancel_anim();
        // New motion: the overlay must stay up for it.
        self.cancel_hide();
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
            held: false,
            warmup: if self.fresh { WARMUP_FRAMES } else { 0 },
        });
        self.fresh = false;
        self.ticker.start();
    }

    /// Stops the animation where it is, except that windows on their way
    /// somewhere (tidy) land there, so an interrupted tidy is still tidy.
    pub(super) fn cancel_anim(&mut self) {
        let Some(a) = self.anim.take() else { return };
        self.launch_tidy = false;
        for (id, _, to) in a.moves {
            if let Some(w) = self.wins.iter_mut().find(|w| w.id() == id) {
                (w.x, w.y) = to;
            }
        }
    }

    pub(super) fn pan(&mut self, dx: f64, dy: f64) {
        self.cam_x -= dx / self.zoom;
        self.cam_y -= dy / self.zoom;
    }

    pub(super) fn zoom_about(&mut self, factor: f64, p: (f64, f64)) {
        let z = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let (wx, wy) = (self.cam_x + p.0 / self.zoom, self.cam_y + p.1 / self.zoom);
        self.zoom = z;
        self.cam_x = wx - p.0 / z;
        self.cam_y = wy - p.1 / z;
    }
}

/// Camera `(x, y, zoom)` that shows the canvas rect `(x0, y0, x1, y1)`
/// centred in the screen area `(l, t, r, b)`, never closer than 0.85.
fn fit_into((x0, y0, x1, y1): (f64, f64, f64, f64), (l, t, r, b): (f64, f64, f64, f64)) -> (f64, f64, f64) {
    let z = ((r - l) / (x1 - x0)).min((b - t) / (y1 - y0)).clamp(MIN_ZOOM, 0.85);
    ((x0 + x1) / 2.0 - (l + r) / 2.0 / z, (y0 + y1) / 2.0 - (t + b) / 2.0 / z, z)
}
