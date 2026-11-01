//! Tab groups: several app windows sharing one spot on the canvas, one shown
//! at a time, with a strip of tabs above it to switch between them.
//!
//! The shown tab is an ordinary canvas window. The others leave `wins` for
//! `tabbed` and wait far off screen (parked, so the watchdog and rescue still
//! bring them back if Apeiron dies). Activating a hidden tab any other way
//! (Alt+Tab, the taskbar) switches the group to it.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::engine::wins;
use crate::shared::{TabsSnap, WinInfo};

use super::{Canvas, Win};

/// Where hidden tabs wait, screen pixels: off every monitor.
const HIDE_AT: (i32, i32) = (-30000, -30000);
/// How far a window taken out of its tabs lands from the group, canvas px.
const POP_OUT: f64 = 48.0;

#[derive(Clone)]
pub(super) struct TabGroup {
    /// Tab order.
    pub(super) ids: Vec<isize>,
    pub(super) active: isize,
    /// The shown tab's visible frame, last seen (a closed one's spot goes to
    /// the next tab).
    pub(super) spot: (f64, f64, f64, f64),
}

/// Sizes `w` so its visible frame is `spot` (apps' invisible borders differ).
fn fit(w: &mut Win, (x, y, vw, vh): (f64, f64, f64, f64)) {
    let (l, t, r, b) = w.inset;
    (w.x, w.y, w.w, w.h) = (x - l, y - t, vw + l + r, vh + t + b);
}

impl Canvas {
    pub(super) fn tab_group(&self, id: isize) -> Option<usize> {
        self.tabs.iter().position(|g| g.ids.contains(&id))
    }

    pub(super) fn is_hidden_tab(&self, id: isize) -> bool {
        self.tabbed.iter().any(|w| w.id() == id)
    }

    /// Puts `ids` (and the groups they're in) in one tab group at the spot of
    /// window `at`, showing `show`.
    pub(super) fn make_tabs(&mut self, ids: &[isize], show: isize, at: isize) {
        let mut members: Vec<isize> = Vec::new();
        for &id in ids {
            let more = self.tab_group(id).map_or(vec![id], |g| self.tabs[g].ids.clone());
            for m in more {
                if !members.contains(&m) && (self.index(m).is_some() || self.is_hidden_tab(m)) {
                    members.push(m);
                }
            }
        }
        let (Some(_), Some(a)) = (self.index(show), self.index(at)) else { return };
        if members.len() < 2 {
            return;
        }
        let spot = self.wins[a].vis();
        self.tabs.retain(|g| !g.ids.iter().any(|id| members.contains(id)));
        let others: Vec<isize> = members.iter().copied().filter(|&id| id != show).collect();
        self.unstick(&others);
        for &id in &others {
            if let Some(i) = self.index(id) {
                let w = self.wins.remove(i);
                self.hide_tab(w, spot);
            } else if let Some(w) = self.tabbed.iter_mut().find(|w| w.id() == id) {
                fit(w, spot);
            }
        }
        if let Some(i) = self.index(show) {
            fit(&mut self.wins[i], spot);
            self.show_tab(i, false);
        }
        self.selected.retain(|id| !others.contains(id));
        log!("{} windows put in tabs", members.len());
        self.tabs.push(TabGroup { ids: members, active: show, spot });
        wins::save_parked(&self.parked);
        self.update_view();
    }

    /// Takes `w` off the canvas into its group's hidden tabs.
    fn hide_tab(&mut self, mut w: Win, spot: (f64, f64, f64, f64)) {
        Self::unregister(&mut w);
        if self.live.is_some_and(|l| l.hwnd == w.hwnd) {
            self.end_live();
        }
        fit(&mut w, spot);
        (w.placed, w.shown) = (None, [None; 2]);
        let flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
        unsafe {
            let _ = SetWindowPos(w.hwnd, None, HIDE_AT.0, HIDE_AT.1, w.w.round() as i32, w.h.round() as i32, flags);
        }
        self.parked.insert(w.id());
        self.tabbed.push(w);
    }

    /// Puts canvas window `i`'s real window at its spot and size (while idle;
    /// in motion the next commit places it), optionally giving it the focus.
    fn show_tab(&mut self, i: usize, activate: bool) {
        let w = &mut self.wins[i];
        let (ww, wh) = (w.w.round() as i32, w.h.round() as i32);
        if self.shown {
            super::resize::resize_window(w);
            return;
        }
        let x = (w.x - self.cam_x).round().clamp(-30000.0, 30000.0) as i32;
        let y = (w.y - self.cam_y).round().clamp(-30000.0, 30000.0) as i32;
        // Waits (unless hung): a sync reading it mid-move would think the
        // user dragged it off screen.
        let mut flags = SWP_NOACTIVATE;
        if wins::is_hung(w.hwnd) {
            flags |= SWP_ASYNCWINDOWPOS;
        }
        unsafe {
            let _ = SetWindowPos(w.hwnd, Some(HWND_TOP), x, y, ww, wh, flags);
        }
        let r = windows::Win32::Foundation::RECT { left: x, top: y, right: x + ww, bottom: y + wh };
        w.placed = Some(r);
        let (id, h) = (w.id(), w.hwnd);
        if wins::reachable(h, &r) { self.parked.remove(&id) } else { self.parked.insert(id) };
        if activate {
            self.focused = Some(id);
            wins::activate(h);
        }
    }

    /// Shows tab `id` of its group in place of the one shown.
    pub(super) fn switch_tab(&mut self, id: isize) {
        let Some(g) = self.tab_group(id) else { return };
        let old = self.tabs[g].active;
        let Some(k) = self.tabbed.iter().position(|w| w.id() == id) else {
            if old == id && !self.shown {
                wins::activate(HWND(id as *mut _));
            }
            return;
        };
        let spot = match (self.index(old), self.away.iter().find(|w| w.id() == old)) {
            (Some(i), _) => self.wins[i].vis(),
            (None, Some(a)) => a.vis(),
            (None, None) => self.tabs[g].spot,
        };
        let mut new = self.tabbed.remove(k);
        fit(&mut new, spot);
        self.wins.insert(0, new);
        if self.shown {
            self.register(0);
        }
        self.show_tab(0, true);
        if let Some(i) = self.index(old) {
            let prev = self.wins.remove(i);
            self.hide_tab(prev, spot);
        }
        let swap = |v: &mut isize| {
            if *v == old {
                *v = id;
            }
        };
        self.sticks.iter_mut().flatten().for_each(swap);
        self.selected.iter_mut().for_each(swap);
        self.tabs[g].active = id;
        wins::save_parked(&self.parked);
        self.update_view();
    }

    /// Takes `id` out of its group, next to it.
    pub(super) fn untab(&mut self, id: isize) {
        let Some(g) = self.tab_group(id) else { return };
        if self.tabs[g].active == id {
            let next = self.tabs[g].ids.iter().copied().find(|&o| o != id && self.is_hidden_tab(o));
            match next {
                Some(n) => self.switch_tab(n),
                None => return,
            }
        }
        if let Some(g) = self.tab_group(id) {
            self.tabs[g].ids.retain(|&o| o != id);
        }
        if let Some(k) = self.tabbed.iter().position(|w| w.id() == id) {
            let w = self.tabbed.remove(k);
            self.pop_out(w, 1.0);
        }
        self.prune_tabs();
    }

    /// Takes every tab of `id`'s group out, cascaded from the group's spot.
    pub(super) fn ungroup_tabs(&mut self, id: isize) {
        let Some(g) = self.tab_group(id) else { return };
        let group = self.tabs.remove(g);
        let mut n = 1.0;
        for m in group.ids {
            if let Some(k) = self.tabbed.iter().position(|w| w.id() == m) {
                let w = self.tabbed.remove(k);
                self.pop_out(w, n);
                n += 1.0;
            }
        }
        wins::save_parked(&self.parked);
        self.update_view();
    }

    /// Back onto the canvas, `n` steps down-right of where it was hidden.
    fn pop_out(&mut self, mut w: Win, n: f64) {
        w.x += POP_OUT * n;
        w.y += POP_OUT * n;
        self.wins.insert(0, w);
        if self.shown {
            self.register(0);
        }
        self.show_tab(0, false);
    }

    /// After a sync: closed tabs leave, a closed shown tab hands its spot to
    /// the next, and groups down to one window dissolve.
    pub(super) fn prune_tabs(&mut self) {
        self.tabbed.retain(|w| unsafe { IsWindow(Some(w.hwnd)).as_bool() });
        for w in &mut self.tabbed {
            w.title = wins::title(w.hwnd);
        }
        let alive: Vec<isize> = self.wins.iter().chain(&self.away).chain(&self.tabbed).map(Win::id).collect();
        for g in &mut self.tabs {
            g.ids.retain(|id| alive.contains(id));
        }
        let mut changed = false;
        // A tab back on the canvas that isn't the shown one (it was
        // minimized while shown, and another tab took over): hide it again.
        for gi in 0..self.tabs.len() {
            let (ids, active, spot) = (self.tabs[gi].ids.clone(), self.tabs[gi].active, self.tabs[gi].spot);
            if self.index(active).is_none() {
                continue;
            }
            for id in ids.into_iter().filter(|&id| id != active) {
                if let Some(i) = self.index(id) {
                    let w = self.wins.remove(i);
                    self.hide_tab(w, spot);
                    changed = true;
                }
            }
        }
        for gi in 0..self.tabs.len() {
            let g = &self.tabs[gi];
            if g.ids.contains(&g.active) {
                continue;
            }
            let spot = g.spot;
            let Some(next) = g.ids.iter().copied().find(|&id| self.is_hidden_tab(id)) else { continue };
            let k = self.tabbed.iter().position(|w| w.id() == next).unwrap_or(0);
            let mut w = self.tabbed.remove(k);
            fit(&mut w, spot);
            self.wins.insert(0, w);
            if self.shown {
                self.register(0);
            }
            self.show_tab(0, true);
            self.tabs[gi].active = next;
            changed = true;
        }
        for g in std::mem::take(&mut self.tabs) {
            if g.ids.len() >= 2 {
                self.tabs.push(g);
                continue;
            }
            for id in g.ids {
                if let Some(k) = self.tabbed.iter().position(|w| w.id() == id) {
                    let w = self.tabbed.remove(k);
                    self.pop_out(w, 0.0);
                }
            }
            changed = true;
        }
        if changed {
            wins::save_parked(&self.parked);
            self.dirty = true;
        }
    }

    /// Remembers where each group's shown tab is (called on publish).
    pub(super) fn refresh_tab_spots(&mut self) {
        for gi in 0..self.tabs.len() {
            if let Some(i) = self.index(self.tabs[gi].active) {
                self.tabs[gi].spot = self.wins[i].vis();
            }
        }
    }

    /// What the UI draws: each group's tabs in order.
    pub(super) fn tab_snaps(&self) -> Vec<TabsSnap> {
        let info = |w: &Win| WinInfo {
            id: w.id(),
            title: w.title.clone(),
            exe: w.exe.clone(),
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            focused: self.focused == Some(w.id()),
        };
        self.tabs
            .iter()
            .map(|g| TabsSnap {
                active: g.active,
                tabs: g
                    .ids
                    .iter()
                    .filter_map(|&id| self.wins.iter().chain(&self.tabbed).chain(&self.away).find(|w| w.id() == id))
                    .map(info)
                    .collect(),
            })
            .collect()
    }

    /// The window on the canvas (not `skip` or its group) whose title bar or
    /// label is under `p`: dropping there joins its tabs.
    pub(super) fn tab_drop_at(&self, p: (f64, f64), skip: isize) -> Option<isize> {
        let room = (crate::ui::labels::LABEL_ROOM as f64) * self.scale();
        let bar = (40.0 * self.zoom).max(8.0);
        let cam = (self.cam_x, self.cam_y, self.zoom);
        let skip_group = self.tab_group(skip);
        self.wins
            .iter()
            .filter(|w| w.id() != skip && (skip_group.is_none() || self.tab_group(w.id()) != skip_group))
            .find(|w| {
                let r = super::thumbs::vis_screen_rect(w, cam);
                let (l, t, rr) = (r.left as f64, r.top as f64, r.right as f64);
                p.0 >= l && p.0 < rr && p.1 >= t - room && p.1 < t + bar
            })
            .map(Win::id)
    }

    /// Closing a hidden tab shows it first (an app asking to save would
    /// otherwise ask off screen).
    pub(super) fn show_before_close(&mut self, ids: &[isize]) {
        for &id in ids {
            if self.is_hidden_tab(id) {
                self.switch_tab(id);
            }
        }
    }

    /// Hidden tabs back at their group's spot, under the shown one (on quit).
    pub(super) fn unhide_tabs(&mut self, wait: bool) {
        for w in &self.tabbed {
            let Some(g) = self.tab_group(w.id()) else { continue };
            let above = self.index(self.tabs[g].active).map(|i| self.wins[i].hwnd);
            let x = (w.x - self.cam_x).round().clamp(-30000.0, 30000.0) as i32;
            let y = (w.y - self.cam_y).round().clamp(-30000.0, 30000.0) as i32;
            let mut flags = SWP_NOSIZE | SWP_NOACTIVATE;
            if above.is_none() {
                flags |= SWP_NOZORDER;
            }
            if !wait || wins::is_hung(w.hwnd) {
                flags |= SWP_ASYNCWINDOWPOS;
            }
            unsafe {
                let _ = SetWindowPos(w.hwnd, above, x, y, 0, 0, flags);
            }
        }
    }
}
