//! Resizing a window by dragging its edges in the overview. The real window
//! is asked for each new size and the thumbnail follows what it takes.

use std::time::Instant;

use windows::Win32::UI::WindowsAndMessaging::*;

use crate::engine::wins;
use crate::shared::{EDGE_L, EDGE_T};

use super::{Canvas, Win, RESIZE_WAIT};

/// A window being resized by dragging its edges.
pub(super) struct Resizing {
    pub(super) id: isize,
    pub(super) edges: u8,
    /// The canvas rect the user is asking for `(x, y, w, h)` (whole window,
    /// invisible borders included).
    pub(super) rect: (f64, f64, f64, f64),
    /// The mouse is up; stop following then.
    pub(super) until: Option<Instant>,
}

/// The window has taken the size asked of it.
fn r_settled(w: &Win, want: (i32, i32)) -> bool {
    (w.w.round() as i32, w.h.round() as i32) == want
}

/// Asks the real window to take its canvas size (queued, never waits).
pub(super) fn resize_window(w: &Win) {
    let flags = SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
    unsafe {
        let _ = SetWindowPos(w.hwnd, None, 0, 0, w.w.round() as i32, w.h.round() as i32, flags);
    }
}

impl Canvas {
    /// Keeps a window being resized in step with the size asked for. The
    /// real window is asked for the latest size only once it has taken the
    /// last one (apps lay out at their own pace; asking faster just queues
    /// stale sizes), and the thumbnail always shows the window as it really
    /// is, never stretched, pinned at the edges not being dragged. The
    /// requested rect is drawn as an outline that follows the mouse exactly.
    pub(super) fn follow_resize(&mut self) {
        let Some(r) = self.resizing.as_ref() else { return };
        let (id, e, (x, y, ww, wh)) = (r.id, r.edges, r.rect);
        let done = r.until.is_some_and(|t| Instant::now() >= t);
        let Some(w) = self.wins.iter_mut().find(|w| w.id() == id) else {
            self.resizing = None;
            return;
        };
        let want = (ww.round() as i32, wh.round() as i32);
        let actual = wins::window_rect(w.hwnd).map(|r| (r.right - r.left, r.bottom - r.top));
        let ready = w.asked.is_none_or(|(size, at)| Some(size) == actual || at.elapsed() >= RESIZE_WAIT);
        if ready && w.asked.map(|a| a.0) != Some(want) && actual != Some(want) {
            let flags = SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
            unsafe {
                let _ = SetWindowPos(w.hwnd, None, 0, 0, want.0, want.1, flags);
            }
            w.asked = Some((want, Instant::now()));
        }
        if let Some((aw, ah)) = actual {
            let (aw, ah) = (aw as f64, ah as f64);
            w.x = if e & EDGE_L != 0 { x + ww - aw } else { x };
            w.y = if e & EDGE_T != 0 { y + wh - ah } else { y };
            (w.w, w.h) = (aw, ah);
        }
        if done || (r_settled(w, want) && self.drag.is_none()) {
            w.asked = None;
            self.resizing = None;
        }
        self.update_view();
    }

    /// The outline of the size being asked for, on screen (left, top,
    /// right, bottom), while it differs from the window's.
    pub(super) fn resize_preview(&self) -> Option<(f64, f64, f64, f64)> {
        let r = self.resizing.as_ref().filter(|_| self.drag.is_some())?;
        let w = self.wins.iter().find(|w| w.id() == r.id)?;
        let (l, t, rr, b) = w.inset;
        let (x, y, ww, wh) = r.rect;
        let z = self.zoom;
        let sx = |v: f64| (v - self.cam_x) * z;
        let sy = |v: f64| (v - self.cam_y) * z;
        Some((sx(x + l), sy(y + t), sx(x + ww - rr), sy(y + wh - b)))
    }
}
