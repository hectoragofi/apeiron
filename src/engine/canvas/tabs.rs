//! Tab groups: several app windows sharing one spot on the canvas, one shown
//! at a time, with a bar of tabs on top of it to switch between them.
//!
//! The shown tab is an ordinary canvas window. The others leave `wins` for
//! `tabbed` and wait far off screen (parked, so the watchdog and rescue still
//! bring them back if Apeiron dies). Activating a hidden tab any other way
//! (Alt+Tab, the taskbar) switches the group to it.
//!
//! The bar is part of the window, so a maximized tabbed window stays
//! maximized but is made shorter by the bar, which sits above it. Windows
//! then does the rest as for any maximized window: restoring it (the
//! button, Win+Down, a double click, dragging it off) puts it back where it
//! was. A window left alone in its group fills its monitor again.

use std::cell::Cell;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::engine::wins;
use crate::shared::{TabsSnap, WinInfo};
use crate::ui::labels::STRIP_H;

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
    /// Maximized: filling its monitor below the bar. Where it was before,
    /// and the spot it was given (canvas, visible frame).
    pub(super) zoom: Option<(Rect, Rect)>,
}

/// The + of a tab bar was clicked: the window picked in the search (or,
/// once an app is started from it, the first window to open) joins the
/// group of `member` as its shown tab.
pub(super) struct Adding {
    member: isize,
    /// Started an app or restored a minimized window, then: a window
    /// appearing joins.
    since: Option<Instant>,
    /// The window to wait for, or else any not among `known`.
    want: Option<isize>,
    known: Vec<isize>,
}

/// How long after an app is started from the + its window may take to open.
const ADD_WAIT: Duration = Duration::from_secs(8);
/// Rects this close count as the same, physical pixels (apps round sizes).
const SAME: f64 = 3.0;
/// A window moved to make room for its bar isn't moved again this soon.
const ROOM_AGAIN: Duration = Duration::from_secs(1);

thread_local! {
    /// The window last moved to make room for its bar, and when.
    static ROOMED: Cell<Option<(isize, Instant)>> = const { Cell::new(None) };
    /// The maximized window last made shorter by its bar, when, and how
    /// many times in a row.
    static SHORTENED: Cell<Option<(isize, Instant, u32)>> = const { Cell::new(None) };
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
        // A group joined keeps its maximized spot; a maximized window joined
        // fills its monitor below the bar, remembering where it was before.
        let zoom = self.tab_group(at).and_then(|g| self.tabs[g].zoom);
        let maxed = unsafe { IsZoomed(self.wins[a].hwnd) }.as_bool().then(|| normal_rect(self.wins[a].hwnd)).flatten();
        // Tabs get sized to the spot, which a maximized window can't be.
        for &id in &members {
            if let Some(i) = self.index(id) {
                self.let_go(i);
            }
        }
        let prev = maxed.filter(|_| zoom.is_none()).map(|r| self.canvas_vis(&self.wins[a], &r));
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
        self.tabs.push(TabGroup { ids: members, active: show, spot, split: None, zoom });
        let g = self.tabs.len() - 1;
        match prev {
            Some(prev) => self.zoom_group(g, prev),
            None => {
                self.make_room(g);
            }
        }
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
        (w.placed, w.shown) = (None, None);
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
        if self.tabs[g].zoom.is_some() && self.tabs[g].split.is_none() && !self.shown {
            self.maximize_under_bar(g, 0);
        }
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
                let mut w = self.tabbed.remove(k);
                if let Some((prev, _)) = group.zoom {
                    fit(&mut w, prev);
                }
                self.pop_out(w, n);
                n += 1.0;
            }
        }
        if let Some((prev, _)) = group.zoom {
            self.maximize_alone(group.active, prev);
        }
        wins::save_parked(&self.parked);
        self.update_view();
    }

    /// Back onto the canvas, `n` steps down-right of where it was hidden.
    pub(super) fn pop_out(&mut self, mut w: Win, n: f64) {
        w.x += POP_OUT * n;
        w.y += POP_OUT * n;
        self.unmaximize(&w);
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
            for &id in &g.ids {
                if let Some(k) = self.tabbed.iter().position(|w| w.id() == id) {
                    let w = self.tabbed.remove(k);
                    self.pop_out(w, 0.0);
                }
            }
            // Alone again: a maximized group's last window maximizes for real.
            if let (Some((prev, _)), Some(&id)) = (g.zoom, g.ids.first()) {
                self.maximize_alone(id, prev);
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
            // Moved or resized by hand (not while still being dragged):
            // an ordinary window again.
            let dragging = self.title_drag.is_some_and(|t| t.0 == self.tabs[gi].active);
            if self.tabs[gi].zoom.is_some_and(|(_, at)| !same_rect(at, spot)) && !dragging && !self.shown {
                self.tabs[gi].zoom = None;
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
        let room = (crate::ui::labels::LABEL_ROOM.max(STRIP_H) as f64) * self.scale();
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
        if target.is_none() {
            self.follow_moves();
            self.tab_dropped(id);
        }
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

/// Two canvas rects within `SAME` of each other.
fn same_rect(a: Rect, b: Rect) -> bool {
    (a.0 - b.0).abs() <= SAME && (a.1 - b.1).abs() <= SAME && (a.2 - b.2).abs() <= SAME && (a.3 - b.3).abs() <= SAME
}

/// The work area of the monitor showing `h`, screen pixels.
fn work_area(h: HWND) -> Option<RECT> {
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe { GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() }.then_some(mi.rcWork)
}

/// The tab bar's height over window `h`, physical pixels (it's drawn at
/// that window's monitor's scale).
fn bar_height(h: HWND) -> f64 {
    let dpi = match unsafe { GetDpiForWindow(h) } {
        0 => 96,
        d => d,
    };
    (STRIP_H as f64 * dpi as f64 / 96.0).round()
}

/// The window's rect when not maximized, screen pixels.
fn normal_rect(h: HWND) -> Option<RECT> {
    let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    unsafe { GetWindowPlacement(h, &mut wp).ok()? };
    let r = wp.rcNormalPosition;
    // Placement is in workspace coordinates, relative to the primary work area.
    let o = wins::primary_work();
    (r.right > r.left && r.bottom > r.top).then(|| RECT {
        left: r.left + o.left,
        top: r.top + o.top,
        right: r.right + o.left,
        bottom: r.bottom + o.top,
    })
}

/// Shows `h` (maximized or not) at screen rect `r` as its normal rect, in
/// one step, as `cmd` says. False if it couldn't be asked (a hung app).
fn place_normal(h: HWND, r: &RECT, cmd: SHOW_WINDOW_CMD) -> bool {
    if wins::is_hung(h) {
        return false;
    }
    let o = wins::primary_work();
    let wp = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        showCmd: cmd.0 as u32,
        rcNormalPosition: RECT { left: r.left - o.left, top: r.top - o.top, right: r.right - o.left, bottom: r.bottom - o.top },
        ..Default::default()
    };
    unsafe { SetWindowPlacement(h, &wp).is_ok() }
}

impl Canvas {
    // ---- maximized tab groups -------------------------------------------------

    /// What's drawn of window `w` (canvas pixels) if its window rect on
    /// screen were `r`.
    fn canvas_vis(&self, w: &Win, r: &RECT) -> Rect {
        let (l, t, rr, b) = w.inset;
        let (x, y) = (self.cam_x + r.left as f64 + l, self.cam_y + r.top as f64 + t);
        (x, y, (r.right - r.left) as f64 - l - rr, (r.bottom - r.top) as f64 - t - b)
    }

    /// Window `w`'s rect on screen at zoom 1.
    fn screen_rect(&self, w: &Win) -> RECT {
        let x = (w.x - self.cam_x).round().clamp(-30000.0, 30000.0) as i32;
        let y = (w.y - self.cam_y).round().clamp(-30000.0, 30000.0) as i32;
        RECT { left: x, top: y, right: x + w.w.round() as i32, bottom: y + w.h.round() as i32 }
    }

    /// Whether `id` is the shown window of a maximized tab group (made
    /// shorter by its bar: not to be put back to full size on landing).
    pub(super) fn under_bar(&self, id: isize) -> bool {
        self.tabs.iter().any(|g| g.zoom.is_some() && g.active == id && g.split.is_none())
    }

    /// The group whose shown window (either, when split) is `id`.
    fn shown_group(&self, id: isize) -> Option<usize> {
        self.tabs.iter().position(|g| g.active == id || g.split.is_some_and(|s| s.0 == id))
    }

    /// While idle, a tab group's shown window moved or changed state. One
    /// that's maximized stays so but is made shorter by its bar; one that
    /// was and isn't any more (restored by Windows) is an ordinary tab
    /// again. True if it was handled.
    pub(super) fn tab_window_moved(&mut self, h: HWND) -> bool {
        let Some(g) = self.shown_group(h.0 as isize) else { return false };
        if self.shown || unsafe { IsIconic(h) }.as_bool() || wins::is_hung(h) {
            return false;
        }
        if !unsafe { IsZoomed(h) }.as_bool() {
            if self.tabs[g].zoom.is_some() {
                log!("tabs: restored; an ordinary tab group again");
                self.tabs[g].zoom = None;
                self.dirty = true;
            }
            return false;
        }
        if self.tabs[g].split.is_some() {
            return false;
        }
        let Some(i) = self.index(h.0 as isize) else { return false };
        self.maximize_under_bar(g, i)
    }

    /// Tab groups whose shown window is maximized (found by a refresh rather
    /// than as it happened): made shorter by the bar.
    pub(super) fn fix_maximized_tabs(&mut self) {
        for g in 0..self.tabs.len() {
            if let Some(i) = self.index(self.tabs[g].active) {
                let h = self.wins[i].hwnd;
                self.tab_window_moved(h);
            }
        }
    }

    /// A maximized window joined group `g`: it makes room for the bar.
    pub(super) fn zoom_group(&mut self, g: usize, _prev: Rect) {
        if let Some(i) = self.index(self.tabs[g].active) {
            self.maximize_under_bar(g, i);
        }
    }

    /// Group `g`'s shown window `wins[i]`, maximized (maximizing it first if
    /// it isn't), made shorter so its bar fits above it. Windows still sees
    /// it as maximized: its own restore (button, Win+Down, double click,
    /// dragging it off) works as ever. True if it was handled.
    pub(super) fn maximize_under_bar(&mut self, g: usize, i: usize) -> bool {
        let h = self.wins[i].hwnd;
        if wins::is_hung(h) {
            return false;
        }
        unsafe {
            if !IsZoomed(h).as_bool() {
                // Restores to the group's spot from before, when there is one.
                if let Some((prev, _)) = self.tabs[g].zoom {
                    let (l, t, rr, b) = self.wins[i].inset;
                    let (x, y) = ((prev.0 - l - self.cam_x).round() as i32, (prev.1 - t - self.cam_y).round() as i32);
                    let (ww, wh) = ((prev.2 + l + rr).round() as i32, (prev.3 + t + b).round() as i32);
                    place_normal(h, &RECT { left: x, top: y, right: x + ww, bottom: y + wh }, SW_SHOWMAXIMIZED);
                } else {
                    let _ = ShowWindow(h, SW_SHOWMAXIMIZED);
                }
            }
        }
        let Some(work) = work_area(h) else { return false };
        let mut r = RECT::default();
        if unsafe { GetWindowRect(h, &mut r) }.is_err() {
            return false;
        }
        // Maximized, its borders hang off the work area; only the top one
        // matters here: what's drawn starts `inset` below the rect.
        let inset = wins::frame_inset(h, &r).1.round() as i32;
        let top = work.top + bar_height(h) as i32 - inset;
        let target = RECT { left: r.left, top, right: r.right, bottom: r.bottom };
        if r.top != top {
            // An app that keeps putting itself back isn't fought over.
            let id = h.0 as isize;
            let now = Instant::now();
            let tries = match SHORTENED.get() {
                Some((o, at, n)) if o == id && now - at < ROOM_AGAIN => n + 1,
                _ => 1,
            };
            SHORTENED.set(Some((id, now, tries)));
            if tries > 3 {
                return false;
            }
            log!("tabs: maximized; shorter by the tab bar");
            let flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
            unsafe {
                let _ = SetWindowPos(h, None, target.left, target.top, target.right - target.left, target.bottom - target.top, flags);
            }
        }
        let at = self.canvas_vis(&self.wins[i], &target);
        if self.tabs[g].zoom.is_none() {
            let prev = normal_rect(h).map_or(self.tabs[g].spot, |n| self.canvas_vis(&self.wins[i], &n));
            self.tabs[g].zoom = Some((prev, at));
        } else if let Some(z) = self.tabs[g].zoom.as_mut() {
            z.1 = at;
        }
        let w = &mut self.wins[i];
        (w.maximized, w.normal) = (false, None);
        fit(w, at);
        w.placed = Some(target);
        self.tabs[g].spot = at;
        self.dirty = true;
        true
    }

    /// The last window of a maximized group, alone now: fills its monitor
    /// again (maximized for real, back to `prev`, canvas visible frame, when
    /// restored).
    fn maximize_alone(&mut self, id: isize, prev: Rect) {
        let Some(i) = self.index(id) else { return };
        if self.shown || wins::is_hung(self.wins[i].hwnd) {
            return;
        }
        let w = &mut self.wins[i];
        let (l, t, r, b) = w.inset;
        let (x, y) = ((prev.0 - l - self.cam_x).round() as i32, (prev.1 - t - self.cam_y).round() as i32);
        let (ww, wh) = ((prev.2 + l + r).round() as i32, (prev.3 + t + b).round() as i32);
        let rect = RECT { left: x, top: y, right: x + ww, bottom: y + wh };
        // Already maximized (made shorter): restoring for an instant puts the
        // whole rect back in one go.
        if unsafe { IsZoomed(w.hwnd) }.as_bool() {
            place_normal(w.hwnd, &rect, SW_SHOWNOACTIVATE);
        }
        if place_normal(w.hwnd, &rect, SW_SHOWMAXIMIZED) {
            log!("tabs: one window left; maximized it");
            (w.maximized, w.normal, w.placed) = (true, Some((ww, wh)), None);
        }
    }

    /// A maximized window leaving a group (popped or dragged out): restored
    /// to its spot, as it was before it maximized.
    pub(super) fn unmaximize(&self, w: &Win) {
        if self.shown || !unsafe { IsZoomed(w.hwnd) }.as_bool() {
            return;
        }
        let r = self.screen_rect(w);
        place_normal(w.hwnd, &r, SW_SHOWNOACTIVATE);
    }

    /// A group's window let go after being dragged by its title bar (not
    /// onto another): makes room for its bar if it needs to. (A maximized
    /// one dragged off was restored by Windows itself.)
    fn tab_dropped(&mut self, id: isize) {
        let Some(g) = self.tabs.iter().position(|g| g.active == id && g.split.is_none()) else { return };
        if unsafe { IsZoomed(HWND(id as *mut _)) }.as_bool() {
            return;
        }
        self.tabs[g].zoom = None;
        self.make_room(g);
    }

    /// Group `g`'s window was moved where its bar has no room (snapped, or
    /// up against the top of its monitor): down by what's missing, and
    /// shorter by as much if it would leave the work area. True if it moved.
    pub(super) fn make_room(&mut self, g: usize) -> bool {
        if self.shown || self.tabs[g].split.is_some() || self.tabs[g].zoom.is_some() {
            return false;
        }
        let Some(i) = self.index(self.tabs[g].active) else { return false };
        let h = self.wins[i].hwnd;
        if unsafe { IsZoomed(h).as_bool() || IsIconic(h).as_bool() } {
            return false;
        }
        let Some(work) = work_area(h) else { return false };
        let bar = bar_height(h);
        let (x, y, vw, vh) = self.wins[i].vis();
        let (sx, sy) = (x - self.cam_x, y - self.cam_y);
        let (top, bottom) = (work.top as f64, work.bottom as f64);
        let on = sx < work.right as f64 && sx + vw > work.left as f64;
        if !on || sy < top - 1.0 || sy >= top + bar {
            return false;
        }
        // An app that won't stay put isn't chased.
        let id = self.tabs[g].active;
        if ROOMED.get().is_some_and(|(o, at)| o == id && at.elapsed() < ROOM_AGAIN) {
            return false;
        }
        ROOMED.set(Some((id, Instant::now())));
        let dy = top + bar - sy;
        let shrink = (sy + dy + vh - bottom).max(0.0);
        let spot = (x, y + dy, vw, (vh - shrink).max(vh * 0.5));
        log!("tabs: moved a window down to make room for its tab bar");
        fit(&mut self.wins[i], spot);
        self.show_tab(i, false);
        self.tabs[g].spot = spot;
        self.dirty = true;
        true
    }

    /// After windows moved by hand while idle: groups whose window landed
    /// where its bar has no room make room (once no longer being dragged).
    pub(super) fn tabs_moved(&mut self, moved: &[HWND]) {
        for &h in moved {
            let id = h.0 as isize;
            if self.title_drag.is_some_and(|t| t.0 == id) {
                continue;
            }
            if let Some(g) = self.tabs.iter().position(|g| g.active == id) {
                self.make_room(g);
            }
        }
    }

    // ---- the + of a tab bar ---------------------------------------------------

    /// The + of the bar of `id`'s group: the search comes up, and what's
    /// picked there joins the group.
    pub(super) fn add_tab(&mut self, id: isize) {
        if self.tab_group(id).is_none() {
            return;
        }
        self.close_menu();
        self.open_search();
        if self.search.is_some() {
            self.adding = Some(Adding { member: id, since: None, want: None, known: Vec::new() });
        }
    }

    /// A window picked in the search: joins the group the search was opened
    /// for, if it was. A minimized one joins once it's back. True if done
    /// here (nothing else to do for the pick).
    pub(super) fn add_pick(&mut self, id: isize) -> bool {
        let Some(member) = self.adding.as_ref().filter(|a| a.since.is_none()).map(|a| a.member) else { return false };
        if self.search.is_none() {
            self.adding = None;
            return false;
        }
        if self.index(id).is_none() {
            // Minimized: it's restored as usual, and joins when it shows.
            self.adding = Some(Adding { member, since: Some(Instant::now()), want: Some(id), known: Vec::new() });
            return false;
        }
        self.adding = None;
        self.join_tab(member, id)
    }

    /// An app was started from the search: if it was opened by a +, the
    /// first new window joins that group.
    pub(super) fn add_launched(&mut self) {
        if self.search.is_none() {
            return;
        }
        let known: Vec<isize> = self.wins.iter().chain(&self.away).chain(&self.tabbed).map(Win::id).collect();
        if let Some(a) = self.adding.as_mut().filter(|a| a.since.is_none()) {
            (a.since, a.known) = (Some(Instant::now()), known);
        }
    }

    /// The search closed without a pick: nothing joins (unless an app was
    /// started from it, whose window still may).
    pub(super) fn drop_adding(&mut self) {
        if self.adding.as_ref().is_some_and(|a| a.since.is_none()) {
            self.adding = None;
        }
    }

    /// Windows that just appeared: the one a + is waiting for joins its
    /// group. True if one did.
    pub(super) fn join_opened(&mut self, opened: &[isize]) -> bool {
        let Some(a) = self.adding.as_ref() else { return false };
        let Some(since) = a.since else { return false };
        if since.elapsed() > ADD_WAIT {
            self.adding = None;
            return false;
        }
        let new = opened.iter().copied().find(|id| a.want.map_or(!a.known.contains(id), |w| w == *id));
        let Some(id) = new else { return false };
        let member = a.member;
        self.adding = None;
        self.join_tab(member, id)
    }

    /// After an idle refresh: a window a + waited for that opened or came
    /// back joins its group (and isn't placed like other new windows).
    pub(super) fn join_arrivals(&mut self) {
        let fresh: Vec<isize> = self.arrived.iter().chain(&self.returned).copied().collect();
        if fresh.is_empty() || !self.join_opened(&fresh) {
            return;
        }
        let joined: Vec<isize> = self.tabs.iter().map(|g| g.active).collect();
        self.arrived.retain(|id| !joined.contains(id));
    }

    /// Window `id` joins the group of `member` as its shown tab; on the
    /// overlay the camera then goes to it.
    fn join_tab(&mut self, member: isize, id: isize) -> bool {
        let Some(g) = self.tab_group(member) else { return false };
        let at = self.tabs[g].active;
        if self.index(at).is_none() || self.index(id).is_none() {
            return false;
        }
        self.search = None;
        self.minimized.clear();
        self.make_tabs(&[at, id], id, at);
        let Some(i) = self.index(id).filter(|_| self.tab_group(id).is_some()) else { return false };
        log!("tabs: added a tab from the +");
        if self.shown {
            self.focus(i);
        } else {
            self.show_tab(i, true);
        }
        true
    }
}
