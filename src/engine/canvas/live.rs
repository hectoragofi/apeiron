//! Live windows in the overview: the mouse works the window under it as if
//! it were full size.
//!
//! The thumbnail is only a picture, so the overlay turns see-through to the
//! mouse and the real window is slid under it (still beneath the overlay,
//! at full size) so that the point under the cursor is the very point its
//! thumbnail shows there. Clicks, drags, the wheel and the cursor shape are
//! then the app's own. The mouse hook keeps the window following the cursor
//! (`mouse::set_live`); the engine decides where that applies: a window's
//! client area, scroll bars and caption buttons. Its title bar, its edges,
//! the canvas's own panels and anything with Alt held stay the overlay's,
//! so windows can still be moved, resized and picked.

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_MENU};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::engine::mouse::{self, LiveMap};
use crate::engine::wins;
use crate::ui::minimap;

use super::thumbs::vis_screen_rect;
use super::Canvas;

/// How long to wait on an app to say what's under the cursor.
const HIT_TIMEOUT_MS: u32 = 20;

impl Canvas {
    /// Start, retarget, refresh or end the live window for the cursor at
    /// `p`. Called whenever the cursor, the camera or the windows move.
    pub(super) fn sync_live(&mut self, p: (f64, f64)) {
        // Mid-drag inside the app the window stays live wherever the cursor
        // goes; only follow the camera if it moves.
        let target = if mouse::live_held() {
            self.live.and_then(|l| self.index(l.hwnd.0 as isize))
        } else {
            self.live_target(p)
        };
        let Some(i) = target else { return self.end_live() };
        let map = self.live_map(i);
        if self.live == Some(map) {
            return;
        }
        let (x, y) = map.window_pos(p);
        let flags = SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
        let same = self.live.map(|l| l.hwnd) == Some(map.hwnd);
        let on_top = unsafe { GetWindowLongPtrW(map.hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0 != 0;
        unsafe {
            if same || on_top {
                let _ = SetWindowPos(map.hwnd, None, x, y, 0, 0, flags | SWP_NOZORDER);
            } else {
                // A new window goes above the other real ones (all under the
                // overlay), so nothing unseen catches its clicks. Windows
                // ignores HWND_TOP from a background app while another app
                // is in front, but not a trip through the topmost band:
                // first just under the overlay (so it never shows over it),
                // then back down to the top of the ordinary windows.
                let _ = SetWindowPos(map.hwnd, Some(self.overlay), x, y, 0, 0, flags);
                let _ = SetWindowPos(map.hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags | SWP_NOMOVE);
            }
        }
        mouse::set_live(Some(map));
        if self.live.is_none() {
            pass_through(self.overlay, true);
        }
        self.live = Some(map);
    }

    /// Hand the mouse back to the overlay.
    pub(super) fn end_live(&mut self) {
        if self.live.take().is_some() {
            mouse::set_live(None);
            pass_through(self.overlay, false);
        }
    }

    fn live_map(&self, i: usize) -> LiveMap {
        let w = &self.wins[i];
        let cam = (self.cam_x, self.cam_y, self.zoom);
        LiveMap {
            hwnd: w.hwnd,
            origin: ((w.x - self.cam_x) * self.zoom, (w.y - self.cam_y) * self.zoom),
            zoom: self.zoom,
            rect: vis_screen_rect(w, cam),
        }
    }

    /// The window the mouse should work directly at `p`, if any.
    fn live_target(&self, p: (f64, f64)) -> Option<usize> {
        let cfg = self.cfg();
        let idle = self.drag.is_none() && self.menu.is_none() && self.search.is_none() && self.resizing.is_none();
        if !cfg.live_windows || !self.overview || !self.shown || self.hiding || self.paused || !idle {
            return None;
        }
        // Zoomed in past 1:1 the real window wouldn't cover its thumbnail.
        if self.zoom > 1.0 || unsafe { GetKeyState(VK_MENU.0 as i32) } < 0 || self.edge_at(p).is_some() {
            return None;
        }
        if cfg.show_minimap && !self.wins.is_empty() {
            let (l, t, r, b) = minimap::panel_rect(self.sw, self.scale());
            if p.0 >= l && p.0 < r && p.1 >= t && p.1 < b {
                return None;
            }
        }
        let i = self.hit(p)?;
        let w = &self.wins[i];
        // Ask the app what's at that spot of the real window, wherever it is.
        let real = wins::window_rect(w.hwnd)?;
        let q = POINT {
            x: real.left + ((p.0 - (w.x - self.cam_x) * self.zoom) / self.zoom).round() as i32,
            y: real.top + ((p.1 - (w.y - self.cam_y) * self.zoom) / self.zoom).round() as i32,
        };
        let ht = nc_hit(w.hwnd, q)?;
        let usable = [HTCLIENT, HTMENU, HTHSCROLL, HTVSCROLL, HTMINBUTTON, HTMAXBUTTON, HTCLOSE, HTHELP, HTSYSMENU];
        usable.contains(&ht).then_some(i)
    }
}

/// `WM_NCHITTEST` at screen point `q`; None if the app doesn't answer in time.
fn nc_hit(h: HWND, q: POINT) -> Option<u32> {
    let lp = (q.x as i16 as u16 as u32) | ((q.y as i16 as u16 as u32) << 16);
    let mut out = 0usize;
    let ok = unsafe {
        SendMessageTimeoutW(
            h,
            WM_NCHITTEST,
            WPARAM(0),
            LPARAM(lp as i32 as isize),
            SMTO_ABORTIFHUNG,
            HIT_TIMEOUT_MS,
            Some(&mut out),
        )
    };
    (ok.0 != 0).then_some(out as i16 as i32 as u32)
}

/// Lets the mouse through the overlay to what's under it, or not. The
/// overlay is layered from the start (see `ui::restyle`), as see-through
/// hit testing needs.
fn pass_through(overlay: HWND, on: bool) {
    unsafe {
        let ex = GetWindowLongPtrW(overlay, GWL_EXSTYLE) as u32;
        let new = if on { ex | WS_EX_TRANSPARENT.0 } else { ex & !WS_EX_TRANSPARENT.0 };
        if new != ex {
            SetWindowLongPtrW(overlay, GWL_EXSTYLE, new as isize);
        }
    }
}
