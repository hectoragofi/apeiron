//! Finding windows: the search panel that frames what matches, and the
//! Alt+Tab switcher. The UI side lives in `ui::search`.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::engine::wins;
use crate::shared::{SearchState, UiEvent, WinInfo};
use crate::ui::search;

use super::{Canvas, FRAME_MARGIN, MIN_ZOOM, SWITCH_DELAY_MS, TIMER_SWITCH};

/// Alt+Tab before its panel shows: the windows in switch order (the
/// active one first, minimized ones last) and which is picked.
pub(super) struct Switch {
    order: Vec<isize>,
    sel: usize,
}

impl Canvas {
    // ---- search --------------------------------------------------------------

    pub(super) fn open_search(&mut self) {
        let fresh = !self.shown;
        self.begin();
        if !self.shown {
            return;
        }
        if !self.overview {
            self.enter_overview();
        }
        if self.search.as_ref().is_some_and(|s| s.switcher) {
            self.search = None;
        }
        if self.search.is_none() {
            self.minimized = minimized_infos();
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

    // ---- the Alt+Tab switcher ------------------------------------------------

    /// Tab (or an arrow) with Alt held: start switching, or move the pick.
    pub fn switch_step(&mut self, back: bool) {
        if !self.attached || self.quitting || self.paused {
            return;
        }
        let dir = if back { -1 } else { 1 };
        if self.search.as_ref().is_some_and(|s| s.switcher) {
            self.search_move(dir);
            return;
        }
        if let Some(sw) = self.switch.as_mut() {
            sw.sel = (sw.sel as i32 + dir).rem_euclid(sw.order.len() as i32) as usize;
            // A second press means they're looking: show the panel.
            self.show_switcher();
            return;
        }
        let fg = unsafe { GetForegroundWindow() }.0 as isize;
        let mut order: Vec<isize> = wins::app_windows(true).into_iter().map(|h| h.0 as isize).collect();
        if let Some(k) = order.iter().position(|&id| id == fg) {
            let id = order.remove(k);
            order.insert(0, id);
        }
        order.extend(wins::minimized_windows().into_iter().map(|h| h.0 as isize));
        if order.is_empty() {
            return;
        }
        // The active window is first, so the first press picks the one after.
        let start = if order.first() == Some(&fg) { dir } else { 0 };
        let sel = start.rem_euclid(order.len() as i32) as usize;
        self.switch = Some(Switch { order, sel });
        if self.shown {
            self.show_switcher();
        } else {
            unsafe {
                SetTimer(Some(self.hwnd), TIMER_SWITCH, SWITCH_DELAY_MS, None);
            }
        }
    }

    /// Alt+Tab is still held: zoom out to everything with the pick ringed,
    /// the camera following it.
    pub(super) fn show_switcher(&mut self) {
        self.stop_timer(TIMER_SWITCH);
        let Some(sw) = self.switch.take() else { return };
        let want = sw.order[sw.sel];
        let fresh = !self.shown;
        self.close_menu();
        self.begin();
        if !self.shown {
            return;
        }
        if !self.overview {
            self.enter_overview();
        }
        self.minimized = minimized_infos();
        let mut s = self.ranked(String::new());
        s.results.sort_by_key(|id| sw.order.iter().position(|o| o == id).unwrap_or(usize::MAX));
        s.selected = s.results.iter().position(|&id| id == want).unwrap_or(0);
        s.switcher = true;
        self.search = Some(s);
        if let Some(b) = self.bounds().filter(|_| fresh) {
            let (x, y, z) = self.frame(b);
            self.animate_to(x, y, z, false, None);
        }
        self.frame_selected();
        self.update_view();
    }

    /// Alt let go (`accept`) or Esc.
    pub fn switch_end(&mut self, accept: bool) {
        if let Some(sw) = self.switch.take() {
            // Quick Alt+Tab: no panel, just switch.
            self.stop_timer(TIMER_SWITCH);
            if accept {
                let h = HWND(sw.order[sw.sel] as *mut _);
                if unsafe { IsIconic(h) }.as_bool() {
                    wins::restore(h);
                }
                wins::activate(h);
            }
            return;
        }
        if !self.search.as_ref().is_some_and(|s| s.switcher) {
            return;
        }
        if !accept {
            self.close_search();
            self.leave_overview();
            return;
        }
        // A window already in view where the overlay came up is switched to
        // without moving the camera.
        let id = self.search.as_ref().and_then(|s| s.results.get(s.selected).copied());
        let (ox, oy) = self.origin;
        let in_view = id.and_then(|id| self.index(id)).filter(|&i| {
            let (x, y, w, h) = self.wins[i].vis();
            x >= ox && y >= oy && x + w <= ox + self.sw && y + h <= oy + self.sh
        });
        match in_view {
            Some(i) => {
                let h = self.wins[i].hwnd;
                self.search = None;
                self.minimized.clear();
                self.animate_to(ox, oy, 1.0, true, Some(h));
                self.update_view();
            }
            None => self.search_accept(),
        }
    }

    /// Canvas windows first, then minimized ones.
    fn ranked(&self, query: String) -> SearchState {
        let mut all = self.infos();
        all.extend(self.minimized.iter().cloned());
        let results = search::rank(&query, &all);
        SearchState { query, results, selected: 0, switcher: false }
    }

    pub(super) fn search_query(&mut self, query: String) {
        if self.search.is_some() {
            self.search = Some(self.ranked(query));
            self.frame_selected();
            self.update_view();
        }
    }

    pub(super) fn search_move(&mut self, delta: i32) {
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
    pub(super) fn search_area(&self) -> (f64, f64, f64, f64) {
        let empty = SearchState::default();
        let panel = search::panel_rect(self.search.as_ref().unwrap_or(&empty), self.sw, self.scale());
        (FRAME_MARGIN, panel.3 + FRAME_MARGIN, self.sw - FRAME_MARGIN, self.sh - FRAME_MARGIN)
    }

    /// Enter: open the selected result. With nothing selected (no matches)
    /// the search stays as it is.
    pub(super) fn search_accept(&mut self) {
        let Some(id) = self.search.as_ref().and_then(|s| s.results.get(s.selected).copied()) else { return };
        self.focus_window(id);
        self.search = None;
        self.minimized.clear();
        self.update_view();
    }

    /// Un-minimize a window picked in the search and land back at 1:1 where
    /// the overlay came up, handing it the focus.
    pub(super) fn restore_minimized(&mut self, h: HWND) {
        wins::restore(h);
        // It comes back to its own spot; land there.
        let (x, y) = match self.away.iter().find(|w| w.hwnd == h) {
            Some(w) => self.centre_on((w.x, w.y, w.w, w.h)),
            None => self.origin,
        };
        self.animate_to(x, y, 1.0, true, Some(h));
    }

    pub(super) fn close_search(&mut self) {
        self.minimized.clear();
        if self.search.take().is_some() {
            self.update_view();
        }
    }
}

/// Minimized windows as the search lists them (no canvas spot).
fn minimized_infos() -> Vec<WinInfo> {
    wins::minimized_windows()
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
        .collect()
}
