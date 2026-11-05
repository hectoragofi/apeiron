//! DWM thumbnail plumbing: showing the overlay, registering a thumbnail per
//! window and telling DWM where each one goes as the camera moves.

use std::time::Instant;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::engine::wins;
use crate::ui::search;

/// Opacity of a window being dropped into another's tabs.
const GHOST: u8 = 110;

use super::{cursor, Canvas, Win, DIMMED, RELEASE_DELAY_MS, TIMER_HIDE, TIMER_REFRESH, TIMER_RELEASE};

/// What a thumbnail was last told, so unchanged ones aren't sent to DWM again.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct Thumb {
    dest: RECT,
    source: RECT,
    opacity: u8,
    visible: bool,
}

impl Canvas {
    /// Real windows have been moved; drop the overlay.
    pub(super) fn hide_overlay(&mut self) {
        self.end_live();
        self.cancel_hide();
        unsafe {
            let _ = ShowWindowAsync(self.overlay, SW_HIDE);
            SetTimer(Some(self.hwnd), TIMER_RELEASE, RELEASE_DELAY_MS, None);
        }
        self.unregister_esc();
        self.menu = None;
        self.shown = false;
        self.hovered = None;
        self.drag = None;
        self.selected.clear();
        self.hover_edge = 0;
        self.peek = None;
        self.arm_peek();
        self.dirty = true;
        if let Some(h) = self.pending_focus.take() {
            unsafe {
                let _ = SetForegroundWindow(h);
            }
            self.focused = Some(h.0 as isize);
        }
    }

    /// Show the overlay with thumbnails standing in for the real windows.
    pub(super) fn begin(&mut self) {
        self.cancel_hide();
        if self.shown || !self.attached || !self.cfg().canvas {
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
        self.refresh_since = None;
        self.sync(true);
        self.origin = (self.cam_x, self.cam_y);
        self.shown = true;
        self.fresh = true;
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
        // Bottom-most first so later (higher) windows draw on top; a
        // window's second thumbnail goes right after its first so it keeps
        // the same place in the stack.
        for i in (0..self.wins.len()).rev() {
            self.register(i);
        }
        self.update_view();
    }

    /// Gives window `i` its thumbnail, drawn above every existing one.
    pub(super) fn register(&mut self, i: usize) {
        let w = &mut self.wins[i];
        w.thumb = unsafe { DwmRegisterThumbnail(self.overlay, w.hwnd) }.unwrap_or(0);
        w.shown = None;
    }

    pub(super) fn unregister(w: &mut Win) {
        if w.thumb != 0 {
            unsafe {
                let _ = DwmUnregisterThumbnail(w.thumb);
            }
            w.thumb = 0;
        }
    }

    /// Brings window `i` to the top of the overlay (a window being dragged
    /// shouldn't slide under the others).
    pub(super) fn raise(&mut self, i: usize) {
        let mut w = self.wins.remove(i);
        Self::unregister(&mut w);
        self.wins.insert(0, w);
        self.register(0);
    }

    pub(super) fn release_thumbnails(&mut self) {
        self.stop_timer(TIMER_RELEASE);
        for w in &mut self.wins {
            Self::unregister(w);
        }
    }

    /// Input moved the camera or windows: the next frame updates the view,
    /// so however fast input comes, thumbnails move once per refresh.
    pub(super) fn queue_view(&mut self) {
        if !self.shown {
            return self.update_view();
        }
        self.pacer.queue(Instant::now());
        self.ticker.start();
    }

    /// Point the thumbnails at the current camera, then queue a UI redraw.
    pub(super) fn update_view(&mut self) {
        self.pacer.applied();
        self.dirty = true;
        if !self.shown {
            return;
        }
        // The camera or the windows may have moved under a still mouse.
        if self.drag.is_none() {
            let p = cursor();
            let hovered = self.hit(p).map(|i| self.wins[i].id());
            self.hovered = hovered;
            self.sync_live(p);
        }
        self.arm_peek();
        let cam = (self.cam_x, self.cam_y, self.zoom);
        let (sw, sh) = (self.sw as i32, self.sh as i32);
        let scale = self.scale();
        // Thumbnails are placed from the overlay's window corner, but
        // everything else (rings, hit tests) from its client area, which
        // GPUI insets by an invisible frame.
        let (ox, oy) = wins::client_offset(self.overlay);
        let matching = self.search.as_ref().map(|s| &s.results);
        // Thumbnails draw over GPUI, so they're cut back from the search
        // panel at the top. (The minimap has a window of its own above them.)
        let panel = self.search.as_ref().map(|s| search::panel_rect(s, self.sw, scale));
        // A window about to be dropped into tabs goes see-through, so the
        // one it joins (and the preview above it) shows.
        let mut ghosts = self.tab_ghosts();
        ghosts.extend(self.drag.as_ref().filter(|d| d.tab_drop.is_some()).and_then(|d| d.target));
        for w in &mut self.wins {
            if w.thumb == 0 {
                continue;
            }
            let full = screen_rect(w, cam);
            let mut r = full;
            if let Some((pl, _, pr, pb)) = panel {
                let pb = pb.ceil() as i32;
                if r.top < pb && (r.left as f64) < pr && (r.right as f64) > pl {
                    r.top = pb;
                }
            }
            let dim = matching.is_some_and(|m| !m.contains(&w.id()));
            let opacity = if ghosts.contains(&w.id()) { GHOST } else if dim { DIMMED } else { 255 };
            let visible = r.right > r.left.max(0) && r.bottom > r.top.max(0) && r.left < sw && r.top < sh;
            let source = source_rect(r, full, w.w, w.h);
            let dest = RECT { left: r.left + ox, top: r.top + oy, right: r.right + ox, bottom: r.bottom + oy };
            let next = Thumb { dest, source, opacity, visible };
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
                rcDestination: dest,
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

    /// The overlay stays up after all (new motion), or is hiding now.
    pub(super) fn cancel_hide(&mut self) {
        self.stop_timer(TIMER_HIDE);
        self.dirty |= self.hiding;
        self.hiding = false;
    }
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

/// The part of a `w` x `h` window that `piece` shows, where the whole
/// window would fill `full` on screen.
fn source_rect(piece: RECT, full: RECT, w: f64, h: f64) -> RECT {
    let kx = w / (full.right - full.left).max(1) as f64;
    let ky = h / (full.bottom - full.top).max(1) as f64;
    let sx = |x: i32| ((x - full.left) as f64 * kx).round() as i32;
    let sy = |y: i32| ((y - full.top) as f64 * ky).round() as i32;
    RECT { left: sx(piece.left), top: sy(piece.top), right: sx(piece.right), bottom: sy(piece.bottom) }
}

/// The visible frame's screen rect (hit testing, unlike the thumbnail,
/// leaves out the invisible borders).
pub(super) fn vis_screen_rect(w: &Win, (cam_x, cam_y, zoom): (f64, f64, f64)) -> RECT {
    let (x, y, vw, vh) = w.vis();
    let (l, t) = ((x - cam_x) * zoom, (y - cam_y) * zoom);
    RECT { left: l as i32, top: t as i32, right: (l + vw * zoom) as i32, bottom: (t + vh * zoom) as i32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
        RECT { left: l, top: t, right: r, bottom: b }
    }

    #[test]
    fn source_follows_the_piece() {
        // A 1000 x 800 window shown at half size from (100, 100).
        let full = rect(100, 100, 600, 500);
        assert_eq!(source_rect(full, full, 1000.0, 800.0), rect(0, 0, 1000, 800));
        assert_eq!(source_rect(rect(400, 200, 600, 500), full, 1000.0, 800.0), rect(600, 200, 1000, 800));
    }
}
