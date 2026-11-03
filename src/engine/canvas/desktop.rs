//! The real desktop around the canvas: windows coming, going and moving on
//! their own, sticky groups, minimized windows that come back, the screen
//! and session changing, and committing the canvas back to real windows.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::engine::{keys, mouse, tidy, wins};
use crate::layout::WinKey;
use crate::shared::{UiEvent, WinInfo};
use crate::ui::labels;

use super::{
    screen_size, Canvas, Win, ADOPT_DELAY_MS, ADOPT_MAX_MS, APPEARED_FOR, FOCUS_QUIET, HIDE_DELAY_MS, LAUNCH_GRACE,
    MAX_AWAY, MOVED_DELAY_MS,
    REFRESH_DELAY_MS, REFRESH_MAX_MS, SAVE_DELAY_MS, SCREEN_DELAY_MS, TIMER_HIDE, TIMER_MOVED, TIMER_REFRESH,
    TIMER_SAVE, TIMER_SCREEN, TIMER_WHEEL,
};
use super::camera::WinMove;
use super::gesture::Gesture;

/// How `sync` treats a window when finding it a spot.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Known,
    /// Found at startup with no spot yet (saved or free).
    New,
}

impl Canvas {
    // ---- the desktop around us ---------------------------------------------

    /// A window appeared, vanished, moved or changed state somewhere: refresh
    /// once things settle. Every event restarts the wait, so a burst (a
    /// virtual desktop switch) is read once, when it's over, but never later
    /// than a cap after the first event. While moving, a window that opens
    /// joins the canvas almost at once.
    pub fn windows_changed(&mut self) {
        if !self.attached || self.quitting {
            return;
        }
        let now = Instant::now();
        let since = *self.refresh_since.get_or_insert(now);
        let delay = if self.shown {
            refresh_delay(since, now, ADOPT_DELAY_MS, ADOPT_MAX_MS)
        } else {
            refresh_delay(since, now, REFRESH_DELAY_MS, REFRESH_MAX_MS)
        };
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_REFRESH, delay, None);
        }
    }

    /// A window was cloaked or uncloaked: usually a virtual desktop switch.
    pub fn desktop_changed(&mut self, h: HWND) {
        if !self.attached || self.quitting {
            return;
        }
        if self.wins.iter().any(|w| w.hwnd == h) {
            self.desktop_moved = Some(Instant::now());
            // The windows being moved are leaving the screen.
            self.abort_motion();
        }
        self.windows_changed();
    }

    /// A window moved or resized (the user dragging it, or an app moving
    /// itself). While idle the canvas mirrors the screen, so follow it, at
    /// most about 30 times a second.
    pub fn window_moved(&mut self, h: HWND) {
        if !self.attached || self.shown || self.quitting {
            return;
        }
        // New windows arrive through `windows_changed`.
        if !self.wins.iter().any(|w| w.hwnd == h) || self.moved.contains(&h) {
            return;
        }
        self.moved.push(h);
        if self.moved.len() == 1 {
            unsafe {
                SetTimer(Some(self.hwnd), TIMER_MOVED, MOVED_DELAY_MS, None);
            }
        }
    }

    pub(super) fn follow_moves(&mut self) {
        self.stop_timer(TIMER_MOVED);
        let moved = std::mem::take(&mut self.moved);
        if self.shown || self.quitting {
            return;
        }
        if self.title_drag.is_some() {
            self.follow_title_drag();
        }
        let cam = (self.cam_x, self.cam_y);
        for h in moved {
            let Some(w) = self.wins.iter_mut().find(|w| w.hwnd == h) else { continue };
            // Minimizing goes through the full refresh instead.
            if unsafe { IsIconic(h) }.as_bool() {
                continue;
            }
            let Some(r) = wins::window_rect(h) else { continue };
            // Where we put it: our canvas position stands.
            if w.placed == Some(r) {
                continue;
            }
            let (nx, ny) = (cam.0 + r.left as f64, cam.1 + r.top as f64);
            let (nw, nh) = ((r.right - r.left) as f64, (r.bottom - r.top) as f64);
            // Moved, not resized: whatever is stuck to it comes along.
            let shift = (w.w == nw && w.h == nh).then_some((nx - w.x, ny - w.y));
            (w.x, w.y, w.w, w.h) = (nx, ny, nw, nh);
            w.placed = Some(r);
            self.dirty = true;
            if let Some((dx, dy)) = shift.filter(|&(dx, dy)| dx != 0.0 || dy != 0.0) {
                self.drag_partners(h.0 as isize, dx, dy);
            }
        }
    }

    /// Moves the windows stuck to `id` by `dx`,`dy` along with it, real
    /// windows and all (only while idle).
    fn drag_partners(&mut self, id: isize, dx: f64, dy: f64) {
        let mut changed = false;
        for other in self.stuck_with(id) {
            if other == id {
                continue;
            }
            let Some(i) = self.index(other) else { continue };
            self.wins[i].x += dx;
            self.wins[i].y += dy;
            changed |= self.place_one(i, false);
        }
        if changed {
            wins::save_parked(&self.parked);
        }
    }

    // ---- sticky windows ------------------------------------------------------

    /// Every window stuck to `id`, itself included.
    pub(super) fn stuck_with(&self, id: isize) -> Vec<isize> {
        self.sticks.iter().find(|g| g.contains(&id)).cloned().unwrap_or_else(|| vec![id])
    }

    /// `ids` and everything stuck to any of them.
    pub(super) fn with_partners(&self, ids: &[isize]) -> Vec<isize> {
        let mut out: Vec<isize> = Vec::new();
        for &id in ids {
            for o in self.stuck_with(id) {
                if !out.contains(&o) {
                    out.push(o);
                }
            }
        }
        out
    }

    /// Joins `ids` (and the groups they're already in) into one group.
    pub(super) fn stick(&mut self, ids: &[isize]) {
        let group = self.with_partners(ids);
        if group.len() < 2 {
            return;
        }
        self.sticks.retain(|g| !g.iter().any(|id| group.contains(id)));
        log!("stuck {} windows together", group.len());
        self.sticks.push(group);
        self.dirty = true;
    }

    /// Takes `ids` out of their groups.
    pub(super) fn unstick(&mut self, ids: &[isize]) {
        for g in &mut self.sticks {
            g.retain(|id| !ids.contains(id));
        }
        self.sticks.retain(|g| g.len() > 1);
        self.dirty = true;
    }

    /// Forgets windows that closed; groups left with one window dissolve.
    fn prune_sticks(&mut self) {
        let mut sticks = std::mem::take(&mut self.sticks);
        for g in &mut sticks {
            g.retain(|id| self.wins.iter().chain(&self.away).any(|w| w.id() == *id));
        }
        sticks.retain(|g| g.len() > 1);
        self.sticks = sticks;
    }

    // ---- minimized and hidden windows ----------------------------------------

    /// Windows that left the canvas: the minimized and hidden ones keep their
    /// spot in `away` for when they come back. Closed ones (and ones gone to
    /// another virtual desktop) are just let go.
    fn stash(&mut self, gone: Vec<Win>) {
        for mut w in gone {
            Self::unregister(&mut w);
            let h = w.hwnd;
            let away = unsafe { IsWindow(Some(h)).as_bool() && (IsIconic(h).as_bool() || !IsWindowVisible(h).as_bool()) }
                && !wins::is_cloaked(h);
            if !away {
                continue;
            }
            w.placed = None;
            w.shown = [None; 2];
            self.away.retain(|o| o.hwnd != h);
            self.away.insert(0, w);
        }
        self.away.truncate(MAX_AWAY);
    }

    /// Drops closed windows from `away` and refreshes what the minimap shows.
    fn prune_away(&mut self) {
        let on: Vec<HWND> = self.wins.iter().map(|w| w.hwnd).collect();
        self.away.retain(|w| !on.contains(&w.hwnd) && unsafe { IsWindow(Some(w.hwnd)).as_bool() });
        self.away_infos = self
            .away
            .iter()
            .filter(|w| !wins::is_cloaked(w.hwnd))
            .map(|w| {
                let (x, y, vw, vh) = w.vis();
                WinInfo { id: w.id(), title: w.title.clone(), exe: w.exe.clone(), x, y, w: vw, h: vh, focused: false }
            })
            .collect();
        self.prune_sticks();
        self.dirty = true;
    }

    /// A window came back from minimized or hidden, now at its old canvas
    /// spot. If that's out of sight and it's the one in use, go there.
    pub(super) fn welcome_back(&mut self) {
        let fg = unsafe { GetForegroundWindow() }.0 as isize;
        let returned = std::mem::take(&mut self.returned);
        if !returned.contains(&fg) {
            return;
        }
        let Some(i) = self.index(fg) else { return };
        let Some(r) = self.wins[i].placed else { return };
        if !wins::reachable(self.wins[i].hwnd, &r) {
            log!("a window came back off screen; flying to it");
            self.focus(i);
        }
    }

    /// A top-level window was shown or uncloaked: if it's an app window
    /// new to the canvas, it has just opened.
    pub fn window_appeared(&mut self, h: HWND) {
        let now = Instant::now();
        self.appeared.retain(|&(id, t)| id != h.0 as isize && now - t < APPEARED_FOR);
        self.appeared.push((h.0 as isize, now));
    }

    fn appeared_lately(&self, id: isize) -> bool {
        self.appeared.iter().any(|&(a, t)| a == id && t.elapsed() < APPEARED_FOR)
    }

    fn launched_lately(&self) -> bool {
        self.launched.is_some_and(|t| t.elapsed() < LAUNCH_GRACE)
    }

    /// Something not on the canvas came to the front over the overview: a
    /// launcher bar, the Start menu, a flyout. Under the topmost overlay it
    /// may not even be seen, and whatever it opens belongs at 1:1, so drop
    /// straight back to where the overview came up and give it the screen.
    fn make_way(&mut self, h: HWND) {
        log!("{} came to the front over the overview; making way", wins::class_name(h));
        self.cancel_anim();
        self.close_menu();
        self.drag = None;
        self.peek = None;
        self.search = None;
        self.minimized.clear();
        // Nothing of ours may take the focus back from it.
        self.pending_focus = None;
        (self.cam_x, self.cam_y) = self.origin;
        self.zoom = 1.0;
        self.update_view();
        self.commit(None);
    }

    /// Something brought a window to the front (Alt+Tab, the taskbar, a
    /// notification). If it's parked out of sight, fly there.
    pub fn foreground_changed(&mut self, h: HWND) {
        if !self.attached || self.quitting {
            return;
        }
        // A hidden tab brought forward (Alt+Tab, the taskbar): show it.
        if self.is_hidden_tab(h.0 as isize) {
            if !self.shown {
                self.switch_tab(h.0 as isize);
            }
            return;
        }
        // On the overlay (a live window clicked into, say): the focus ring
        // follows, the camera stays.
        if self.shown {
            let id = h.0 as isize;
            if self.live.is_some_and(|l| l.hwnd == h) {
                self.focused = Some(id);
                self.dirty = true;
            } else if let Some(i) = self.index(id) {
                // An app started from the search that was already running
                // just brings its window forward: go there. (Hovering hands
                // windows the keyboard too, but marks them focused first.)
                let busy = self.search.is_some() || self.drag.is_some() || self.hiding;
                if self.overview && self.launched_lately() && self.focused != Some(id) && !busy {
                    self.launched = None;
                    self.focus(i);
                }
            } else if self.overview && !self.hiding && !self.launched_lately() && wins::is_foreign_popup(h) {
                // A dialog of a window on the canvas is the app's business.
                let owner = wins::root_owner(h).0 as isize;
                let ours = self.wins.iter().chain(&self.tabbed).any(|w| w.id() == owner || w.id() == id);
                if !ours {
                    self.make_way(h);
                }
            }
            return;
        }
        self.close_menu();
        let id = h.0 as isize;
        let Some(i) = self.index(id) else { return };
        let prev = self.focused.replace(id);
        self.dirty = true;
        // Only a switch to another window counts: the one in use getting
        // focus back (after a pan left it behind, or around a desktop
        // switch) must never drag the camera back to it.
        let recent = |t: Option<Instant>| t.is_some_and(|t| t.elapsed() < FOCUS_QUIET);
        if prev == Some(id) || recent(self.last_commit) || recent(self.desktop_moved) {
            return;
        }
        let Some(r) = self.wins[i].placed else { return };
        if wins::window_rect(h) == Some(r) && !wins::reachable(h, &r) {
            log!("activated an off-screen window; flying to it");
            self.focus_window(h.0 as isize);
        }
    }

    /// The display's size or scale changed (or is changing).
    pub fn screen_changed(&mut self) {
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_SCREEN, SCREEN_DELAY_MS, None);
        }
    }

    pub(super) fn apply_screen(&mut self) {
        self.stop_timer(TIMER_SCREEN);
        let (sw, sh) = screen_size();
        if (sw, sh) != (self.sw, self.sh) {
            log!("screen is now {sw}x{sh}");
            // It was scaled to fill the old screen.
            crate::wallpaper::reload();
        }
        (self.sw, self.sh) = (sw, sh);
        self.shared.emit(UiEvent::Screen);
        self.update_view();
    }

    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
        self.show_bg();
    }

    /// Sign-out, shutdown or restart: the process will be killed, so leave
    /// every window reachable and the layout saved before returning. Moves
    /// are synchronous (except to hung apps) so they land before apps save
    /// their own window positions.
    pub fn end_session(&mut self) {
        log!("session ending: putting windows back");
        self.cancel_anim();
        self.drag = None;
        self.gesture = Gesture::default();
        self.pending_focus = None;
        self.go_home(true);
    }

    /// Lets go of a minimized or hidden window's spot: its placeholder goes,
    /// and it comes back in the middle of the view instead.
    pub(super) fn forget_away(&mut self, id: isize) {
        self.away.retain(|w| w.id() != id);
        self.prune_away();
        self.update_view();
    }

    pub(super) fn set_paused(&mut self, paused: bool) {
        if paused == self.paused {
            return;
        }
        self.paused = paused;
        self.gesture = Gesture::default();
        self.keys_applied = None;
        self.apply_keys();
        if paused {
            if self.overview {
                self.leave_overview();
            } else if self.shown && !self.anim.as_ref().is_some_and(|a| a.commit) {
                // Mid-gesture: land where the camera is rather than undo it.
                self.land(self.cam_x, self.cam_y, None);
            }
        }
        self.show_bg();
        self.dirty = true;
    }

    /// The infinite canvas was turned on or off in settings. Off: back to
    /// plain windows at 1:1, none left out of reach, the canvas desktop gone.
    pub(super) fn canvas_mode_changed(&mut self) {
        if !self.settings.canvas {
            if self.shown {
                self.cancel_anim();
                (self.cam_x, self.cam_y) = self.origin;
                self.commit(None);
            }
            let mut n = 0;
            for w in &self.wins {
                let Some(r) = w.placed.or_else(|| wins::window_rect(w.hwnd)) else { continue };
                if !wins::reachable(w.hwnd, &r) {
                    wins::send_home(w.hwnd, n, false);
                    n += 1;
                }
            }
            log!("infinite canvas off: tabs only ({n} windows brought on screen)");
        }
        self.show_bg();
        self.dirty = true;
    }

    fn show_bg(&self) {
        let cmd = if self.paused || self.locked || !self.cfg().canvas { SW_HIDE } else { SW_SHOWNOACTIVATE };
        unsafe {
            let _ = ShowWindowAsync(self.bg, cmd);
        }
    }

    /// Lay every window out in neat rows, keeping their reading order. Each
    /// one glides to its new spot while the camera frames the result.
    pub(super) fn tidy(&mut self) {
        if !self.cfg().canvas {
            return;
        }
        if !self.shown {
            if self.nothing_to_show() {
                return;
            }
            self.begin();
        }
        let Some((x0, y0, x1, y1)) = self.bounds().filter(|_| self.shown) else { return };
        self.drag = None;
        // Windows stuck together are arranged as one block.
        let mut units: Vec<Vec<usize>> = Vec::new();
        let mut seen = vec![false; self.wins.len()];
        for i in 0..self.wins.len() {
            if seen[i] {
                continue;
            }
            let members: Vec<usize> = self.stuck_with(self.wins[i].id()).iter().filter_map(|&id| self.index(id)).collect();
            for &m in &members {
                seen[m] = true;
            }
            units.push(members);
        }
        let rects: Vec<(f64, f64, f64, f64)> = units
            .iter()
            .map(|u| {
                let (l, t, r, b) = u.iter().map(|&m| &self.wins[m]).fold(
                    (f64::MAX, f64::MAX, f64::MIN, f64::MIN),
                    |(l, t, r, b), w| (l.min(w.x), t.min(w.y), r.max(w.x + w.w), b.max(w.y + w.h)),
                );
                (l, t, r - l, b - t)
            })
            .collect();
        let order = tidy::reading_order(&rects);
        let cfg = self.cfg();
        let gap = cfg.gap.round();
        // Labels sit above windows at a fixed screen size, so each row needs
        // room for one in canvas pixels at the zoom that frames the result.
        // That zoom depends on the room; a few passes settle it.
        let label = if cfg.show_labels { (labels::LABEL_ROOM as f64 + 6.0) * self.scale() } else { 0.0 };
        let arrange = |room: f64| {
            let padded: Vec<(f64, f64)> = order.iter().map(|&u| (rects[u].2, rects[u].3 + room)).collect();
            let spots = tidy::arrange(&padded, gap);
            let extent = spots
                .iter()
                .zip(&padded)
                .fold((0.0_f64, 0.0_f64), |(w, h), (p, s)| (w.max(p.0 + s.0), h.max(p.1 + s.1)));
            (spots, extent)
        };
        let mut room = 0.0;
        for _ in 0..3 {
            let (w, h) = arrange(room).1;
            room = (label / self.frame((0.0, 0.0, w, h)).2).ceil();
        }
        let (spots, (w, h)) = arrange(room);
        // Centred where the windows were, on whole pixels.
        let ox = ((x0 + x1 - w) / 2.0).round();
        let oy = ((y0 + y1 - h) / 2.0).round();
        let moves: Vec<WinMove> = order
            .iter()
            .zip(&spots)
            .flat_map(|(&u, &(x, y))| {
                let (bx, by) = (rects[u].0, rects[u].1);
                units[u].iter().map(move |&m| (m, bx, by, x, y))
            })
            .map(|(m, bx, by, x, y)| {
                let w = &self.wins[m];
                (w.id(), (w.x, w.y), (ox + x + (w.x - bx), oy + y + room + (w.y - by)))
            })
            .collect();
        // Esc returns to 1:1 where the overlay came up; carry that spot along
        // with the focused (or topmost) window that was on screen there, so it
        // doesn't land on empty canvas. With none on screen, centre one.
        let (gx, gy) = self.origin;
        let seen = |w: &&Win| w.x < gx + self.sw && w.y < gy + self.sh && w.x + w.w > gx && w.y + w.h > gy;
        let focused_first = |w: &&Win| Some(w.id()) != self.focused;
        let on_screen = self.wins.iter().filter(seen).min_by_key(focused_first);
        if let Some(w) = on_screen.or_else(|| self.wins.iter().min_by_key(focused_first)) {
            if let Some(&(_, from, to)) = moves.iter().find(|m| m.0 == w.id()) {
                self.origin = match on_screen {
                    Some(_) => (gx + to.0 - from.0, gy + to.1 - from.1),
                    None => self.centre_on((to.0, to.1, w.w, w.h)),
                };
            }
        }
        let (cx, cy, z) = self.frame((ox, oy, ox + w, oy + h));
        self.animate_to(cx, cy, z, false, None);
        if let Some(a) = self.anim.as_mut() {
            a.moves = moves;
        }
        self.enter_overview();
        log!("tidy: arranged {} windows", spots.len());
    }

    /// The next free spot in the layout for window `i`, tidy-style: the end
    /// of the row nearest the view, or a new row, clear of every window and
    /// minimized window's spot, with room for labels as tidying leaves.
    fn slot_for(&self, i: usize) -> (f64, f64) {
        let me = self.wins[i].id();
        let rects: Vec<(f64, f64, f64, f64)> =
            self.wins.iter().filter(|w| w.id() != me).chain(&self.away).map(|w| (w.x, w.y, w.w, w.h)).collect();
        let cfg = self.cfg();
        // Labels are drawn at a fixed screen size: room for one at the zoom
        // that frames the whole layout.
        let room = match self.bounds() {
            Some(b) if cfg.show_labels => ((labels::LABEL_ROOM as f64 + 6.0) * self.scale() / self.frame(b).2).ceil(),
            _ => 0.0,
        };
        let view = (self.cam_x, self.cam_y, self.sw / self.zoom, self.sh / self.zoom);
        let w = &self.wins[i];
        tidy::next_slot(&rects, (w.w, w.h), view, cfg.gap.round(), room)
    }

    /// While idle: a window that just opened in front of the user takes the
    /// next free spot in the layout, and the camera follows it there.
    /// Windows that open behind others are left where they are rather than
    /// moved out of sight, and so are maximized ones (they fill the screen
    /// where they are).
    pub(super) fn place_arrivals(&mut self) {
        let arrived = std::mem::take(&mut self.arrived);
        let cfg = self.cfg();
        let switching = self.desktop_moved.is_some_and(|t| t.elapsed() < FOCUS_QUIET);
        if !cfg.place_new || !cfg.canvas || self.paused || self.locked || self.shown || switching {
            return;
        }
        let fg = unsafe { GetForegroundWindow() };
        let id = fg.0 as isize;
        if !arrived.contains(&id) || !self.appeared_lately(id) || unsafe { IsZoomed(fg) }.as_bool() {
            return;
        }
        let Some(i) = self.index(id) else { return };
        let from = (self.wins[i].x, self.wins[i].y);
        let to = self.slot_for(i);
        if to == from {
            return;
        }
        log!("a new window opened; placing it next to the others");
        self.begin();
        // `begin` re-reads the windows; this one stays where it is.
        let Some(i) = self.index(id).filter(|_| self.shown) else { return };
        (self.wins[i].x, self.wins[i].y) = to;
        self.launched = None;
        self.show_new(i, Some(from));
    }

    /// While moving: windows that closed leave the canvas, and windows that
    /// opened join it: back from minimized at their old spot, new ones in
    /// the next free spot in the layout (or, with that off, the middle of
    /// the view). In the overview the camera then takes you to the one in
    /// front (or the one just launched from the search).
    pub(super) fn adopt(&mut self) {
        let alive = |w: &Win| unsafe { IsWindow(Some(w.hwnd)).as_bool() && IsWindowVisible(w.hwnd).as_bool() };
        let (keep, gone): (Vec<Win>, Vec<Win>) = std::mem::take(&mut self.wins).into_iter().partition(alive);
        self.wins = keep;
        let mut changed = !gone.is_empty();
        self.stash(gone);
        let place_new = self.cfg().place_new;
        let mut opened: Vec<isize> = Vec::new();
        let mut fresh = false;
        for h in wins::app_windows(false) {
            if self.wins.iter().chain(&self.tabbed).any(|w| w.hwnd == h) {
                continue;
            }
            let Some(r) = wins::window_rect(h) else { continue };
            let (w, hgt) = ((r.right - r.left) as f64, (r.bottom - r.top) as f64);
            // Back from minimized or hidden: its old spot. Otherwise the
            // middle of the view.
            let back = self.away.iter().position(|o| o.hwnd == h).map(|k| self.away.remove(k));
            let (x, y) = match &back {
                Some(o) => (o.x, o.y),
                None => (self.cam_x + (self.sw / self.zoom - w) / 2.0, self.cam_y + (self.sh / self.zoom - hgt) / 2.0),
            };
            let win = Win {
                hwnd: h,
                title: wins::title(h),
                exe: wins::exe_name(h),
                class: wins::class_name(h),
                x,
                y,
                w,
                h: hgt,
                placed: None,
                thumbs: [0; 2],
                shown: [None; 2],
                inset: wins::frame_inset(h, &r),
                asked: None,
            };
            self.wins.insert(0, win);
            if back.is_none() && place_new {
                (self.wins[0].x, self.wins[0].y) = self.slot_for(0);
                fresh = true;
            }
            self.register(0);
            changed = true;
            opened.push(h.0 as isize);
        }
        if !changed {
            return;
        }
        self.prune_away();
        self.prune_tabs();
        self.selected.retain(|id| self.wins.iter().any(|w| w.id() == *id));
        self.update_view();
        if opened.is_empty() || !self.overview || self.drag.is_some() || self.gesture.engaged || self.search.is_some() {
            return;
        }
        // The one in front; else the newest, unless it opened on its own
        // in the background (then it's just placed, not flown to).
        let fg = unsafe { GetForegroundWindow() }.0 as isize;
        let launched = self.launched_lately();
        let pick = match opened.iter().find(|&&id| id == fg) {
            Some(&id) => Some(id),
            None if launched || !place_new => opened.last().copied(),
            None => None,
        };
        let Some(i) = pick.and_then(|id| self.index(id)) else { return };
        self.launched = None;
        if fresh {
            self.show_new(i, None);
        } else {
            self.focus(i);
        }
    }

    /// Puts the camera back where the motion started and hands back to the
    /// real windows at once (the desktop is being switched under it).
    pub(super) fn abort_motion(&mut self) {
        self.gesture = Gesture::default();
        if !self.shown {
            return;
        }
        self.cancel_anim();
        self.drag = None;
        (self.cam_x, self.cam_y) = self.origin;
        self.zoom = 1.0;
        self.update_view();
        self.commit(None);
    }

    /// Refresh the window list from reality. Only valid at zoom 1 with the
    /// overlay hidden, when real windows mirror the canvas.
    ///
    /// `live` is the gesture start: maximized windows are turned into normal
    /// ones and new windows get a spot (saved or free). Otherwise (idle, for
    /// the background's minimap) the list just mirrors the screen and new
    /// windows are left for the next live sync to place.
    pub(super) fn sync(&mut self, live: bool) {
        // The list is rebuilt; the last overlay's thumbnails can't wait.
        self.release_thumbnails();
        let cfg = self.cfg();
        let startup = live && !self.restored;
        let mut next: Vec<Win> = Vec::new();
        let mut kinds = Vec::new();
        let mut returned: Vec<isize> = Vec::new();
        let mut arrived: Vec<isize> = Vec::new();
        for h in wins::app_windows(true) {
            if self.is_hidden_tab(h.0 as isize) {
                continue;
            }
            let maximized = unsafe { IsZoomed(h).as_bool() };
            let rect = if live && maximized {
                // Un-maximizing has to wait on the app; a hung one is left
                // off the canvas this time rather than stall the engine.
                if wins::is_hung(h) {
                    continue;
                }
                wins::unmaximize(h)
            } else {
                wins::window_rect(h)
            };
            let Some(r) = rect else { continue };
            let (w, hgt) = ((r.right - r.left) as f64, (r.bottom - r.top) as f64);
            let known = self.wins.iter().find(|o| o.hwnd == h);
            // Back from minimized or hidden.
            let back = match known {
                None => self.away.iter().find(|o| o.hwnd == h),
                Some(_) => None,
            };
            let (exe, class) = match known.or(back) {
                Some(o) => (o.exe.clone(), o.class.clone()),
                None => (wins::exe_name(h), wins::class_name(h)),
            };
            let title = wins::title(h);
            if back.is_some() {
                returned.push(h.0 as isize);
            }
            if known.is_none() && back.is_none() && self.restored {
                arrived.push(h.0 as isize);
            }
            let (x, y, placed) = match (known, back) {
                (Some(o), _) if o.placed == Some(r) => (o.x, o.y, o.placed),
                // Its old spot; the real window is moved there below (or
                // on the next commit).
                (None, Some(o)) => (o.x, o.y, None),
                // New, or moved/resized by the user since we placed it. While
                // idle the canvas mirrors the screen, so that's where it is.
                _ => (self.cam_x + r.left as f64, self.cam_y + r.top as f64, (!live).then_some(r)),
            };
            kinds.push(if startup && known.is_none() && back.is_none() { Kind::New } else { Kind::Known });
            let inset = match known {
                Some(o) if o.w == w && o.h == hgt => o.inset,
                _ => wins::frame_inset(h, &r),
            };
            next.push(Win {
                hwnd: h,
                title,
                exe,
                class,
                x,
                y,
                w,
                h: hgt,
                placed,
                thumbs: [0; 2],
                shown: [None; 2],
                inset,
                asked: None,
            });
        }

        // A new window that was here last time goes back to its saved spot.
        // Every window is matched so known ones keep their own entries.
        if cfg.remember_layout && kinds.contains(&Kind::New) {
            let keys: Vec<WinKey> = next.iter().map(Win::key).collect();
            for (i, saved) in self.layout.positions(&keys).into_iter().enumerate() {
                if let (Kind::New, Some((x, y))) = (kinds[i], saved) {
                    (next[i].x, next[i].y, kinds[i]) = (x, y, Kind::Known);
                }
            }
        }

        // At startup, windows with no saved spot tend to land right on top
        // of others. Slide each one right until it has its own space, making
        // way for every settled window and for those above it.
        for i in 0..next.len() {
            if kinds[i] == Kind::Known {
                continue;
            }
            let (mut x, y, w, h) = (next[i].x, next[i].y, next[i].w, next[i].h);
            for _ in 0..64 {
                let blocks = |j: usize| kinds[j] == Kind::Known || j < i;
                let blocker = next
                    .iter()
                    .enumerate()
                    .filter(|&(j, o)| j != i && blocks(j) && overlaps((x, y, w, h), o))
                    .map(|(_, o)| o.x + o.w)
                    .fold(None, |m: Option<f64>, r| Some(m.map_or(r, |m| m.max(r))));
                match blocker {
                    Some(right) => x = right + cfg.gap,
                    None => break,
                }
            }
            next[i].x = x;
        }
        let old = std::mem::take(&mut self.wins);
        let gone: Vec<Win> = old.into_iter().filter(|o| !next.iter().any(|n| n.hwnd == o.hwnd)).collect();
        self.wins = next;
        self.stash(gone);
        self.prune_away();
        self.prune_tabs();
        if !live {
            self.arrived = arrived;
        }
        if !live && !returned.is_empty() {
            let mut changed = false;
            for i in 0..self.wins.len() {
                if returned.contains(&self.wins[i].id()) {
                    changed |= self.place_one(i, false);
                }
            }
            if changed {
                wins::save_parked(&self.parked);
            }
            self.returned = returned;
        }
        let fg = unsafe { GetForegroundWindow() }.0 as isize;
        self.focused = self.wins.iter().find(|w| w.id() == fg).map(Win::id);
        self.dirty = true;
    }

    /// Move the real windows to match the canvas at zoom 1. With `wait`, the
    /// moves happen before this returns (except to hung apps).
    fn place_windows(&mut self, wait: bool) {
        let mut changed = false;
        for i in 0..self.wins.len() {
            changed |= self.place_one(i, wait);
        }
        if changed {
            wins::save_parked(&self.parked);
        }
    }

    /// Moves real window `i` to its canvas spot; returns whether its
    /// out-of-reach status changed.
    fn place_one(&mut self, i: usize, wait: bool) -> bool {
        let w = &mut self.wins[i];
        // Windows stops honouring coordinates past roughly +-32k.
        let x = (w.x - self.cam_x).round().clamp(-30000.0, 30000.0) as i32;
        let y = (w.y - self.cam_y).round().clamp(-30000.0, 30000.0) as i32;
        let mut flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
        if !wait || wins::is_hung(w.hwnd) {
            flags |= SWP_ASYNCWINDOWPOS;
        }
        unsafe {
            let _ = SetWindowPos(w.hwnd, None, x, y, 0, 0, flags);
        }
        let r = RECT { left: x, top: y, right: x + w.w as i32, bottom: y + w.h as i32 };
        w.placed = Some(r);
        let id = w.id();
        if wins::reachable(w.hwnd, &r) { self.parked.remove(&id) } else { self.parked.insert(id) }
    }

    /// Place the real windows, then hide the overlay once they've had a
    /// moment to get there.
    pub(super) fn commit(&mut self, focus: Option<HWND>) {
        self.peek = None;
        self.end_live();
        self.zoom = 1.0;
        self.overview = false;
        self.search = None;
        self.minimized.clear();
        self.drag = None;
        self.stop_timer(TIMER_WHEEL);
        self.place_windows(false);
        self.last_commit = Some(Instant::now());
        // A focus picked earlier in this overlay session still stands unless
        // this commit names its own.
        if let Some(h) = focus {
            self.pending_focus = Some(h);
            self.focused = Some(h.0 as isize);
        }
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_HIDE, HIDE_DELAY_MS, None);
            SetTimer(Some(self.hwnd), TIMER_SAVE, SAVE_DELAY_MS, None);
        }
        self.hiding = true;
        self.dirty = true;
    }

    /// Leave everything at zoom 1 with windows placed, then tell the UI to
    /// shut down.
    pub(super) fn quit(&mut self) {
        self.quitting = true;
        keys::set_switcher_enabled(false);
        mouse::configure(&self.cfg(), false);
        self.cancel_anim();
        self.drag = None;
        self.go_home(false);
        self.shared.emit(UiEvent::Quit);
        // If the UI thread is wedged, don't hang around.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(3));
            std::process::exit(0);
        });
    }

    /// The exit path: back to zoom 1 at the origin with real windows placed,
    /// the layout saved, the overlay down, and nothing left out of reach.
    fn go_home(&mut self, wait: bool) {
        if self.shown || self.zoom != 1.0 {
            (self.cam_x, self.cam_y) = self.origin;
            self.zoom = 1.0;
            self.place_windows(wait);
        }
        self.save_layout();
        self.unhide_tabs(wait);
        self.cancel_hide();
        self.unregister_esc();
        self.release_thumbnails();
        unsafe {
            let _ = ShowWindowAsync(self.overlay, SW_HIDE);
        }
        (self.shown, self.overview, self.search, self.hovered) = (false, false, None, None);
        self.dirty = true;
        // Judge each window by where we sent it, not where it is: a busy app
        // may not have moved yet. A move queued now lands after that one.
        let mut n = 0;
        for w in &self.wins {
            let Some(r) = w.placed.or_else(|| wins::window_rect(w.hwnd)) else { continue };
            if !wins::reachable(w.hwnd, &r) {
                wins::send_home(w.hwnd, n, wait && !wins::is_hung(w.hwnd));
                n += 1;
            }
        }
        // Then anything else out of reach (minimized, hidden, moved by hand).
        if !wait {
            std::thread::sleep(Duration::from_millis(150));
        }
        n += wins::rescue_all();
        log!("{n} windows brought back on screen");
    }

    /// Records the camera and window positions so a restart picks up where
    /// we left off. Always runs at zoom 1 (after a commit, or on quit).
    pub(super) fn save_layout(&mut self) {
        if !self.cfg().remember_layout || self.layout_frozen {
            return;
        }
        let entries: Vec<(WinKey, f64, f64)> = self.wins.iter().chain(&self.away).chain(&self.tabbed).map(|w| (w.key(), w.x, w.y)).collect();
        self.layout.capture((self.cam_x, self.cam_y), &entries);
        self.layout.save();
    }

    /// Applies the saved layout at startup: the camera goes back to the saved
    /// one and windows take their saved canvas positions (via `sync`).
    /// Windows with no saved spot stay where they are on screen.
    pub(super) fn restore_layout(&mut self) {
        if !self.cfg().remember_layout || !self.cfg().canvas {
            self.sync(false);
            return;
        }
        let Some((cx, cy)) = self.layout.camera() else {
            self.sync(false);
            return;
        };
        // Camera first, so unmatched windows map to their current screen spot.
        (self.cam_x, self.cam_y) = (cx, cy);
        self.sync(true);
        self.place_windows(false);
        log!("layout restored ({} windows)", self.wins.len());
    }
}

/// Overlap beyond a few pixels of shadow/border.
fn overlaps(a: (f64, f64, f64, f64), b: &Win) -> bool {
    const SLACK: f64 = 8.0;
    a.0 < b.x + b.w - SLACK && b.x < a.0 + a.2 - SLACK && a.1 < b.y + b.h - SLACK && b.y < a.1 + a.3 - SLACK
}

/// How long to wait before refreshing after a window event: the usual
/// settle time, cut short so the refresh lands no later than `max_ms` after
/// the first event still waiting on it.
fn refresh_delay(since: Instant, now: Instant, settle_ms: u32, max_ms: u32) -> u32 {
    let waited = now.saturating_duration_since(since).as_millis().min(u32::MAX as u128) as u32;
    settle_ms.min(max_ms.saturating_sub(waited)).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_waits_to_settle_but_not_forever() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        assert_eq!(refresh_delay(t0, t0, 300, 1000), 300);
        assert_eq!(refresh_delay(t0, at(500), 300, 1000), 300);
        assert_eq!(refresh_delay(t0, at(800), 300, 1000), 200);
        // Past the cap, it fires as soon as it can.
        assert_eq!(refresh_delay(t0, at(1000), 300, 1000), 1);
        assert_eq!(refresh_delay(t0, at(5000), 300, 1000), 1);
        assert_eq!(refresh_delay(t0, at(200), 60, 250), 50);
    }
}
