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
    /// Split screen: a second tab shown beside the active one, and whether
    /// it's on the left.
    pub(super) split: Option<(isize, bool)>,
}

/// The two halves of a group's spot, canvas pixels, with a gap between.
/// A canvas rect `(x, y, w, h)`.
pub(super) type Rect = (f64, f64, f64, f64);

pub(super) fn halves((x, y, w, h): Rect) -> (Rect, Rect) {
    let gap = SPLIT_GAP.min(w / 4.0);
    let lw = ((w - gap) / 2.0).round();
    ((x, y, lw, h), (x + lw + gap, y, w - lw - gap, h))
}

/// Space between split tabs, canvas pixels.
const SPLIT_GAP: f64 = 8.0;

/// Sizes `w` so its visible frame is `spot` (apps' invisible borders differ).
pub(super) fn fit(w: &mut Win, (x, y, vw, vh): (f64, f64, f64, f64)) {
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
        for &id in ids {
            if let Some(g) = self.tab_group(id) {
                self.unsplit(g);
            }
        }
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
        // Tabs get sized to the spot, which a maximized window can't be.
        for &id in &members {
            if let Some(i) = self.index(id) {
                self.let_go(i);
            }
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
        self.tabs.push(TabGroup { ids: members, active: show, spot, split: None });
        wins::save_parked(&self.parked);
        self.update_view();
    }

    /// Takes `w` off the canvas into its group's hidden tabs.
    pub(super) fn hide_tab(&mut self, mut w: Win, spot: (f64, f64, f64, f64)) {
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
    pub(super) fn show_tab(&mut self, i: usize, activate: bool) {
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
        // The other half of a split: it becomes the active one.
        if let Some((other, left)) = self.tabs[g].split.filter(|s| s.0 == id) {
            self.tabs[g].active = other;
            self.tabs[g].split = Some((old, !left));
            if !self.shown {
                self.focused = Some(id);
                wins::activate(HWND(id as *mut _));
            }
            self.dirty = true;
            return;
        }
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
        self.detach(id, None);
    }

    /// Takes every tab of `id`'s group out, cascaded from the group's spot.
    pub(super) fn ungroup_tabs(&mut self, id: isize) {
        let Some(g) = self.tab_group(id) else { return };
        self.unsplit(g);
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
    pub(super) fn pop_out(&mut self, mut w: Win, n: f64) {
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
            let other = self.tabs[gi].split.map(|s| s.0);
            for id in ids.into_iter().filter(|&id| id != active && Some(id) != other) {
                if let Some(i) = self.index(id) {
                    let w = self.wins.remove(i);
                    self.hide_tab(w, spot);
                    changed = true;
                }
            }
        }
        // A split half that left (closed, minimized): the other takes the
        // whole spot.
        for gi in 0..self.tabs.len() {
            let Some((other, _)) = self.tabs[gi].split else { continue };
            let active = self.tabs[gi].active;
            match (self.index(active).is_some(), self.index(other).is_some()) {
                (true, true) => continue,
                (false, true) => {
                    self.tabs[gi].active = other;
                    self.tabs[gi].split = None;
                }
                _ => self.tabs[gi].split = None,
            }
            self.sticks.retain(|s| !(s.contains(&active) && s.contains(&other)));
            let (a, spot) = (self.tabs[gi].active, self.tabs[gi].spot);
            if let Some(i) = self.index(a) {
                fit(&mut self.wins[i], spot);
                self.show_tab(i, false);
            }
            changed = true;
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
            let Some(i) = self.index(self.tabs[gi].active) else { continue };
            let mut spot = self.wins[i].vis();
            if let Some(j) = self.tabs[gi].split.and_then(|(o, _)| self.index(o)) {
                let (a, b) = (spot, self.wins[j].vis());
                let (l, t) = (a.0.min(b.0), a.1.min(b.1));
                let (r, bot) = ((a.0 + a.2).max(b.0 + b.2), (a.1 + a.3).max(b.1 + b.3));
                spot = (l, t, r - l, bot - t);
            }
            self.tabs[gi].spot = spot;
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
                split: g.split.map(|s| s.0),
                spot: g.spot,
                tabs: g
                    .ids
                    .iter()
                    .filter_map(|&id| self.wins.iter().chain(&self.tabbed).chain(&self.away).find(|w| w.id() == id))
                    .map(info)
                    .collect(),
            })
            .collect()
    }

    /// The window on the canvas (not `skip` or its group) whose top part,
    /// or the label or tab strip above it, is under `p`: dropping there
    /// joins its tabs.
    pub(super) fn tab_drop_at(&self, p: (f64, f64), skip: isize) -> Option<isize> {
        self.drop_target(p, skip, false)
    }

    /// `title_only`: just the title bar and the room above it (a real
    /// window dragged across the desktop shouldn't merge when it passes
    /// over the top of another).
    fn drop_target(&self, p: (f64, f64), skip: isize, title_only: bool) -> Option<isize> {
        let room = (crate::ui::labels::LABEL_ROOM as f64) * self.scale();
        let title = 34.0 * self.scale();
        let cam = (self.cam_x, self.cam_y, self.zoom);
        let skip_group = self.tab_group(skip);
        self.wins
            .iter()
            .filter(|w| w.id() != skip && (skip_group.is_none() || self.tab_group(w.id()) != skip_group))
            .find(|w| {
                let r = super::thumbs::vis_screen_rect(w, cam);
                let (l, t, rr, b) = (r.left as f64, r.top as f64, r.right as f64, r.bottom as f64);
                // The top third: far enough from the middle that moving a
                // window across another doesn't merge them by accident.
                let band = if title_only { title } else { ((b - t) / 3.0).max(24.0) };
                p.0 >= l && p.0 < rr && p.1 >= t - room && p.1 < t + band
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

    // ---- dragging real windows together --------------------------------

    /// The user started moving or sizing a real window.
    pub fn movesize_start(&mut self, h: HWND) {
        if self.shown {
            return;
        }
        let id = h.0 as isize;
        let Some(i) = self.index(id) else { return };
        self.title_drag = Some((id, (self.wins[i].w, self.wins[i].h), None));
    }

    /// While it moves: is the cursor on another window's title bar?
    pub(super) fn follow_title_drag(&mut self) {
        let Some((id, size, target)) = self.title_drag else { return };
        let Some(i) = self.index(id) else { return };
        // Resizing, not moving.
        if (self.wins[i].w, self.wins[i].h) != size {
            if target.is_some() {
                self.dirty = true;
            }
            self.title_drag = Some((id, size, None));
            return;
        }
        let t = self.drop_target(super::cursor(), id, true);
        if t != target {
            self.title_drag = Some((id, size, t));
            self.dirty = true;
        }
    }

    /// Let go: dropped on a title bar, it joins that window's tabs.
    pub fn movesize_end(&mut self, h: HWND) {
        let Some((id, _, _)) = self.title_drag.filter(|t| t.0 == h.0 as isize) else { return };
        self.follow_title_drag();
        let target = self.title_drag.take().and_then(|t| t.2);
        self.dirty = true;
        if let Some(at) = target {
            if !self.cfg().tabs_by_title_bar {
                return;
            }
            // Its real position first, so the group starts from the truth.
            self.follow_moves();
            self.make_tabs(&[at, id], id, at);
            if let Some(i) = self.index(id) {
                self.show_tab(i, true);
            }
        }
    }
}
