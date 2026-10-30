//! The canvas: every app window has a position on an unbounded plane and the
//! screen is a camera onto it.
//!
//! Real windows can't be scaled and repaint late when moved, so all motion
//! happens on a fullscreen overlay that shows live DWM thumbnails. At zoom 1
//! the thumbnails line up pixel-for-pixel with the real windows, so a
//! gesture looks like the windows themselves gliding. When motion stops at
//! zoom 1 the real windows are moved to match and the overlay disappears.

use std::time::Instant;

use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::touchpad::Contact;
use crate::wins;

pub const MIN_ZOOM: f64 = 0.04;
pub const MAX_ZOOM: f64 = 1.0;
/// A gesture that ends this close to zoom 1 snaps back to real windows.
const SNAP: f64 = 0.15;
/// Finger travel (px) before three fingers count as a gesture, not a tap.
const START_SLOP: f64 = 6.0;
/// How far a flick carries, in seconds of release velocity.
const FLICK: f64 = 0.25;
const ANIM_SECS: f64 = 0.32;

pub const TIMER_ANIM: usize = 1;
pub const TIMER_HIDE: usize = 2;
pub const HOTKEY_ESC: i32 = 3;

const BG: COLORREF = COLORREF(0x0016_1312);
const DOT: COLORREF = COLORREF(0x0040_3a38);

struct Win {
    hwnd: HWND,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    /// Where we last put the real window; if it's still there, our canvas
    /// position is authoritative (the window may be clamped off-screen).
    placed: Option<RECT>,
    thumb: isize,
}

struct Anim {
    from: (f64, f64, f64),
    to: (f64, f64, f64),
    start: Instant,
    /// Hand control back to real windows when done.
    commit: bool,
    focus: Option<HWND>,
}

struct Drag {
    target: Option<usize>,
    last: POINT,
    moved: f64,
}

#[derive(Default)]
struct Gesture {
    prev: Option<(usize, f64, f64, f64)>,
    active: bool,
    travel: f64,
    last: Option<Instant>,
    vel: (f64, f64),
}

pub struct Canvas {
    pub overlay: HWND,
    pub sw: f64,
    pub sh: f64,
    cam_x: f64,
    cam_y: f64,
    zoom: f64,
    wins: Vec<Win>,
    shown: bool,
    anim: Option<Anim>,
    drag: Option<Drag>,
    gesture: Gesture,
    pending_focus: Option<HWND>,
    /// Camera when the overlay came up; Esc returns here.
    origin: (f64, f64),
}

impl Canvas {
    pub fn new(overlay: HWND, sw: f64, sh: f64) -> Self {
        Canvas {
            overlay,
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
            origin: (0.0, 0.0),
        }
    }

    // ---- touchpad ----------------------------------------------------------

    pub fn on_fingers(&mut self, fingers: &[Contact]) {
        let n = fingers.len();
        if n < 3 {
            if self.gesture.active {
                self.gesture_end();
            }
            self.gesture = Gesture::default();
            return;
        }
        let cx = fingers.iter().map(|c| c.x).sum::<f64>() / n as f64;
        let cy = fingers.iter().map(|c| c.y).sum::<f64>() / n as f64;
        let spread = fingers.iter().map(|c| (c.x - cx).hypot(c.y - cy)).sum::<f64>() / n as f64;
        let now = Instant::now();

        // A finger joining or leaving shifts the centroid; re-anchor instead of jumping.
        if let Some((pn, px, py, ps)) = self.gesture.prev.filter(|p| p.0 == n) {
            let (dx, dy) = (cx - px, cy - py);
            if !self.gesture.active {
                self.gesture.travel += dx.hypot(dy) + (spread - ps).abs();
                if self.gesture.travel > START_SLOP {
                    self.gesture.active = true;
                    self.begin();
                }
            }
            if self.gesture.active {
                let _ = pn;
                self.pan(dx, dy);
                if ps > 1.0 {
                    let pivot = cursor();
                    self.zoom_about(spread / ps, pivot);
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
        let tx = self.cam_x - vx * FLICK / self.zoom;
        let ty = self.cam_y - vy * FLICK / self.zoom;
        if (self.zoom - 1.0).abs() < SNAP {
            // Keep the point under the cursor fixed while snapping to 1:1.
            let p = cursor();
            let (wx, wy) = (tx + p.0 / self.zoom, ty + p.1 / self.zoom);
            self.animate_to(wx - p.0, wy - p.1, 1.0, true, None);
        } else {
            self.animate_to(tx, ty, self.zoom, false, None);
            self.enter_overview();
        }
    }

    // ---- mouse on the overlay (overview mode) ------------------------------

    pub fn mouse_down(&mut self, p: POINT) {
        self.anim = None;
        let target = self.hit(p);
        self.drag = Some(Drag { target, last: p, moved: 0.0 });
    }

    pub fn mouse_move(&mut self, p: POINT) {
        let Some(d) = self.drag.as_mut() else { return };
        let (dx, dy) = ((p.x - d.last.x) as f64, (p.y - d.last.y) as f64);
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

    pub fn mouse_up(&mut self) {
        let Some(d) = self.drag.take() else { return };
        if d.moved < 4.0 {
            if let Some(i) = d.target {
                self.focus(i);
            }
        }
    }

    /// Touchpad two-finger scroll and pinch arrive as wheel events; pinch
    /// comes with Ctrl held.
    pub fn wheel(&mut self, delta: f64, horizontal: bool, pinch: bool, p: POINT) {
        self.anim = None;
        if pinch {
            self.zoom_about((delta / 120.0 * 0.12).exp(), (p.x as f64, p.y as f64));
        } else if horizontal {
            self.pan(-delta * 0.5, 0.0);
        } else {
            self.pan(0.0, delta * 0.5);
        }
        self.update_view();
    }

    // ---- hotkeys -----------------------------------------------------------

    /// Zoom out to fit every window, or back in if already zoomed out.
    pub fn toggle_overview(&mut self) {
        if self.shown {
            self.leave_overview();
            return;
        }
        self.begin();
        let Some((x0, y0, x1, y1)) = self.bounds() else { return };
        let margin = 80.0;
        let z = ((self.sw - margin * 2.0) / (x1 - x0))
            .min((self.sh - margin * 2.0) / (y1 - y0))
            .clamp(MIN_ZOOM, 0.85);
        let cx = (x0 + x1) / 2.0 - self.sw / 2.0 / z;
        let cy = (y0 + y1) / 2.0 - self.sh / 2.0 / z;
        self.animate_to(cx, cy, z, false, None);
        self.enter_overview();
    }

    /// Esc: zoom back to 1:1 where the overlay first came up.
    pub fn leave_overview(&mut self) {
        if !self.shown {
            return;
        }
        let (x, y) = self.origin;
        self.animate_to(x, y, 1.0, true, None);
    }

    // ---- timers ------------------------------------------------------------

    pub fn tick(&mut self) {
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
        self.update_view();
        if t >= 1.0 {
            let a = self.anim.take().unwrap();
            self.stop_timer(TIMER_ANIM);
            if a.commit {
                self.commit(a.focus);
            }
        }
    }

    /// Real windows have been moved; drop the overlay.
    pub fn hide_overlay(&mut self) {
        self.stop_timer(TIMER_HIDE);
        unsafe {
            let _ = ShowWindow(self.overlay, SW_HIDE);
            let _ = UnregisterHotKey(Some(self.overlay), HOTKEY_ESC);
        }
        for w in &mut self.wins {
            if w.thumb != 0 {
                unsafe {
                    let _ = DwmUnregisterThumbnail(w.thumb);
                }
                w.thumb = 0;
            }
        }
        self.shown = false;
        if let Some(h) = self.pending_focus.take() {
            unsafe {
                let _ = SetForegroundWindow(h);
            }
        }
    }

    // ---- core --------------------------------------------------------------

    /// Show the overlay with thumbnails standing in for the real windows.
    fn begin(&mut self) {
        self.stop_timer(TIMER_HIDE);
        self.pending_focus = None;
        if self.shown {
            return;
        }
        self.sync();
        self.origin = (self.cam_x, self.cam_y);
        unsafe {
            let _ = SetWindowPos(
                self.overlay,
                Some(HWND_TOPMOST),
                0,
                0,
                self.sw as i32,
                self.sh as i32,
                SWP_SHOWWINDOW | SWP_NOACTIVATE,
            );
        }
        // Bottom-most first so later (higher) windows draw on top.
        for w in self.wins.iter_mut().rev() {
            w.thumb = unsafe { DwmRegisterThumbnail(self.overlay, w.hwnd) }.unwrap_or(0);
        }
        self.shown = true;
        self.update_view();
    }

    fn enter_overview(&mut self) {
        unsafe {
            let _ = RegisterHotKey(Some(self.overlay), HOTKEY_ESC, MOD_NOREPEAT, VK_ESCAPE.0 as u32);
        }
    }

    /// Refresh the window list from reality. Only valid at zoom 1 with the
    /// overlay hidden, when real windows mirror the canvas.
    fn sync(&mut self) {
        let live = wins::app_windows(false);
        let mut next = Vec::with_capacity(live.len());
        for h in live {
            let Some(r) = wins::window_rect(h) else { continue };
            let (w, hgt) = ((r.right - r.left) as f64, (r.bottom - r.top) as f64);
            let known = self.wins.iter().position(|w| w.hwnd == h);
            let win = match known {
                Some(i) if self.wins[i].placed.is_some_and(|p| p == r) => {
                    let o = &self.wins[i];
                    Win { hwnd: h, x: o.x, y: o.y, w, h: hgt, placed: o.placed, thumb: 0 }
                }
                // New, or moved/resized by the user since we placed it.
                _ => Win {
                    hwnd: h,
                    x: self.cam_x + r.left as f64,
                    y: self.cam_y + r.top as f64,
                    w,
                    h: hgt,
                    placed: None,
                    thumb: 0,
                },
            };
            next.push(win);
        }
        self.wins = next;
    }

    /// Move the real windows to match the canvas at zoom 1, then hide the
    /// overlay once they've had a moment to get there.
    fn commit(&mut self, focus: Option<HWND>) {
        self.zoom = 1.0;
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
        self.pending_focus = focus;
        unsafe {
            SetTimer(Some(self.overlay), TIMER_HIDE, 70, None);
        }
    }

    fn focus(&mut self, i: usize) {
        let w = &self.wins[i];
        // Centre it, or pin its top-left if it's bigger than the screen.
        let tx = if w.w < self.sw { w.x + w.w / 2.0 - self.sw / 2.0 } else { w.x };
        let ty = if w.h < self.sh { w.y + w.h / 2.0 - self.sh / 2.0 } else { w.y };
        let h = w.hwnd;
        self.animate_to(tx, ty, 1.0, true, Some(h));
    }

    fn animate_to(&mut self, x: f64, y: f64, z: f64, commit: bool, focus: Option<HWND>) {
        self.anim = Some(Anim {
            from: (self.cam_x, self.cam_y, self.zoom),
            to: (x, y, z),
            start: Instant::now(),
            commit,
            focus,
        });
        unsafe {
            SetTimer(Some(self.overlay), TIMER_ANIM, 8, None);
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

    fn hit(&self, p: POINT) -> Option<usize> {
        // wins is topmost-first, so the first hit is the visible one.
        self.wins.iter().position(|w| {
            let r = self.screen_rect(w);
            p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
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

    fn update_view(&self) {
        if !self.shown {
            return;
        }
        for w in &self.wins {
            if w.thumb == 0 {
                continue;
            }
            let r = self.screen_rect(w);
            let on_screen = r.right > 0 && r.bottom > 0 && r.left < self.sw as i32 && r.top < self.sh as i32;
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: r,
                opacity: 255,
                fVisible: on_screen.into(),
                fSourceClientAreaOnly: false.into(),
                ..Default::default()
            };
            unsafe {
                let _ = DwmUpdateThumbnailProperties(w.thumb, &props);
            }
        }
        unsafe {
            let _ = InvalidateRect(Some(self.overlay), None, false);
        }
    }

    fn stop_timer(&self, id: usize) {
        unsafe {
            let _ = KillTimer(Some(self.overlay), id);
        }
    }

    /// Dot grid that moves with the camera, so empty canvas still reads as motion.
    pub fn paint(&self, hdc: HDC) {
        let (w, h) = (self.sw as i32, self.sh as i32);
        unsafe {
            let mem = CreateCompatibleDC(Some(hdc));
            let bmp = CreateCompatibleBitmap(hdc, w, h);
            let old = SelectObject(mem, bmp.into());
            let bg = CreateSolidBrush(BG);
            FillRect(mem, &RECT { left: 0, top: 0, right: w, bottom: h }, bg);
            let dot = CreateSolidBrush(DOT);
            let mut step = 48.0 * self.zoom;
            while step < 18.0 {
                step *= 2.0;
            }
            let ox = (-self.cam_x * self.zoom).rem_euclid(step);
            let oy = (-self.cam_y * self.zoom).rem_euclid(step);
            let mut y = oy;
            while y < h as f64 {
                let mut x = ox;
                while x < w as f64 {
                    let (xi, yi) = (x as i32, y as i32);
                    FillRect(mem, &RECT { left: xi, top: yi, right: xi + 2, bottom: yi + 2 }, dot);
                    x += step;
                }
                y += step;
            }
            let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
            SelectObject(mem, old);
            let _ = DeleteObject(bg.into());
            let _ = DeleteObject(dot.into());
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
        }
    }
}

fn cursor() -> (f64, f64) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    (p.x as f64, p.y as f64)
}
