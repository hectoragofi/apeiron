//! Peek: hold Space over a window in the overview to see it at 1:1, let go
//! to glide back to the overview as it was.
//!
//! The keyboard hook (`keys::peek`) holds the press back until it's clearly
//! a hold rather than a typed space; this side times that and moves the
//! camera.

use windows::Win32::UI::WindowsAndMessaging::SetTimer;

use crate::engine::keys;

use super::{Canvas, TIMER_PEEK};

/// Space held this long is a peek rather than a typed space.
const PEEK_HOLD_MS: u32 = 200;

impl Canvas {
    pub fn on_peek(&mut self, code: usize) {
        match code {
            keys::PEEK_PENDING => unsafe {
                SetTimer(Some(self.hwnd), TIMER_PEEK, PEEK_HOLD_MS, None);
            },
            keys::PEEK_TAP => self.stop_timer(TIMER_PEEK),
            keys::PEEK_END => self.end_peek(),
            _ => {}
        }
    }

    /// The hold timer ran out: if Space is still down, zoom the hovered
    /// window to 1:1, centred, remembering where the overview was.
    pub(super) fn start_peek(&mut self) {
        self.stop_timer(TIMER_PEEK);
        if !keys::peek_held() {
            return;
        }
        let target = self.hovered.and_then(|id| self.index(id));
        let Some(i) = target.filter(|_| self.overview && self.shown) else { return };
        let w = &self.wins[i];
        let (x, y) = self.centre_on((w.x, w.y, w.w, w.h));
        if self.peek.is_none() {
            self.peek = Some((self.cam_x, self.cam_y, self.zoom));
        }
        self.animate_to(x, y, 1.0, false, None);
    }

    /// Space let go: back to the overview where it was.
    pub(super) fn end_peek(&mut self) {
        self.stop_timer(TIMER_PEEK);
        let Some((x, y, z)) = self.peek.take() else { return };
        if self.overview && self.shown {
            self.animate_to(x, y, z, false, None);
        }
    }

    /// Lets the keyboard hook take Space only while it could peek.
    pub(super) fn arm_peek(&self) {
        let on = self.cfg().peek
            && self.overview
            && self.shown
            && !self.paused
            && self.hovered.is_some()
            && self.search.is_none()
            && self.menu.is_none()
            && self.drag.is_none();
        keys::set_peek_armed(on);
    }
}
