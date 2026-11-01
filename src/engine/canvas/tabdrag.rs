//! Dragging a tab out of its strip: along the strip reorders, onto either
//! half of the group's window splits the screen between two tabs, anywhere
//! else takes the window out of the group, there.
//!
//! The strip is GPUI, but once dragged the cursor is over other apps, so
//! the mouse hook follows it (`mouse::set_tab_drag`). The overlay comes up
//! at 1:1 for the drag so the preview draws above every window; windows
//! under the cursor go see-through so the tab being dragged shows.

use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};

use crate::engine::{mouse, wins};
use crate::shared::TabDragSnap;
use crate::ui::labels::{STRIP_GAP, STRIP_H};

use super::tabs::{fit, halves};
use super::Canvas;

/// Cursor travel (px) before a press on a tab is a drag.
const SLOP: f64 = 6.0;
/// Where a detached window lands relative to the cursor: the cursor ends up
/// this far into its top edge, canvas pixels.
const GRAB: (f64, f64) = (90.0, 14.0);

pub(super) struct TabDrag {
    id: isize,
    from: (f64, f64),
    p: (f64, f64),
    moved: bool,
    /// The overlay was brought up for this drag; it commits when it ends.
    began: bool,
    target: Option<Target>,
}

#[derive(Clone, Copy, PartialEq)]
enum Target {
    /// Along the strip: to this place in the tab order.
    Reorder(usize),
    /// Onto the window: into its left half (true) or right.
    Split(bool),
    /// Anywhere else.
    Detach,
}

impl Canvas {
    /// Left button down on a tab: show it, and follow the mouse in case it
    /// turns into a drag.
    pub(super) fn tab_press(&mut self, id: isize, p: (f64, f64)) {
        self.close_menu();
        let Some(g) = self.tab_group(id) else { return };
        if self.tabs[g].active != id {
            self.switch_tab(id);
        }
        // Already released (a quick click): nothing to follow.
        if unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } as u16 & 0x8000 == 0 {
            return;
        }
        self.tab_drag = Some(TabDrag { id, from: p, p, moved: false, began: false, target: None });
        mouse::set_tab_drag(true);
    }

    pub(super) fn tab_drag_move(&mut self, p: (f64, f64)) {
        // The release went somewhere the hook didn't see (paused, say).
        if unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } as u16 & 0x8000 == 0 {
            return self.tab_drag_end(p);
        }
        let Some(d) = self.tab_drag.as_mut() else { return };
        d.p = p;
        if !d.moved {
            if (p.0 - d.from.0).hypot(p.1 - d.from.1) < SLOP {
                return;
            }
            d.moved = true;
            if !self.shown {
                self.begin();
                if let Some(d) = self.tab_drag.as_mut() {
                    d.began = true;
                }
            }
            self.end_live();
        }
        let target = self.tab_target(p);
        let Some(d) = self.tab_drag.as_mut() else { return };
        d.target = target;
        let id = d.id;
        // Reordering is live: the tab moves along the strip as it's dragged.
        if let (Some(Target::Reorder(k)), Some(g)) = (target, self.tab_group(id)) {
            let ids = &mut self.tabs[g].ids;
            if let Some(at) = ids.iter().position(|&o| o == id) {
                let k = k.min(ids.len() - 1);
                if at != k {
                    let t = ids.remove(at);
                    ids.insert(k, t);
                }
            }
        }
        self.queue_view();
    }

    pub(super) fn tab_drag_end(&mut self, p: (f64, f64)) {
        mouse::set_tab_drag(false);
        // Judged while the drag is still on (the target reads it).
        let target = self.tab_drag.as_ref().filter(|d| d.moved).and_then(|_| self.tab_target(p));
        let Some(d) = self.tab_drag.take() else { return };
        self.dirty = true;
        if !d.moved {
            return;
        }
        match target {
            Some(Target::Split(left)) => self.split_tab(d.id, left),
            Some(Target::Detach) => self.detach(d.id, Some(p)),
            Some(Target::Reorder(_)) | None => {}
        }
        if d.began && self.shown && !self.overview {
            let h = self.index(d.id).map(|i| self.wins[i].hwnd);
            self.commit(h);
        } else {
            self.update_view();
        }
    }

    /// The tab strip of group `g` on screen (left, top, right, bottom).
    fn strip_rect(&self, g: usize) -> (f64, f64, f64, f64) {
        let (x, y, w, _) = self.tabs[g].spot;
        let s = self.scale();
        let (l, t) = ((x - self.cam_x) * self.zoom, (y - self.cam_y) * self.zoom);
        let top = t - (STRIP_H + STRIP_GAP) as f64 * s;
        (l, top, l + w * self.zoom, top + STRIP_H as f64 * s)
    }

    fn spot_rect(&self, g: usize) -> (f64, f64, f64, f64) {
        let (x, y, w, h) = self.tabs[g].spot;
        let (l, t) = ((x - self.cam_x) * self.zoom, (y - self.cam_y) * self.zoom);
        (l, t, l + w * self.zoom, t + h * self.zoom)
    }

    fn tab_target(&self, p: (f64, f64)) -> Option<Target> {
        let d = self.tab_drag.as_ref()?;
        let g = self.tab_group(d.id)?;
        let n = self.tabs[g].ids.len();
        let (l, t, r, b) = self.strip_rect(g);
        let slack = 14.0 * self.scale();
        if p.0 >= l - slack && p.0 < r + slack && p.1 >= t - slack && p.1 < b + slack {
            let k = ((p.0 - l) / ((r - l) / n as f64)).floor().clamp(0.0, (n - 1) as f64) as usize;
            return Some(Target::Reorder(k));
        }
        let (sl, st, sr, sb) = self.spot_rect(g);
        if n >= 2 && p.0 >= sl && p.0 < sr && p.1 >= st && p.1 < sb {
            return Some(Target::Split(p.0 < (sl + sr) / 2.0));
        }
        Some(Target::Detach)
    }

    /// What the UI draws for the drag.
    pub(super) fn tab_drag_snap(&self) -> Option<TabDragSnap> {
        let d = self.tab_drag.as_ref().filter(|d| d.moved)?;
        let g = self.tab_group(d.id)?;
        let (preview, hint) = match d.target {
            Some(Target::Split(left)) => {
                let (a, b) = halves(self.tabs[g].spot);
                let (x, y, w, h) = if left { a } else { b };
                let (l, t) = ((x - self.cam_x) * self.zoom, (y - self.cam_y) * self.zoom);
                (Some((l, t, l + w * self.zoom, t + h * self.zoom)), if left { "Split left" } else { "Split right" })
            }
            Some(Target::Detach) => {
                let w = self.wins.iter().chain(&self.tabbed).find(|w| w.id() == d.id)?;
                let (_, _, vw, vh) = w.vis();
                let (l, t) = (d.p.0 - GRAB.0 * self.zoom, d.p.1 - GRAB.1 * self.zoom);
                (Some((l, t, l + vw * self.zoom, t + vh * self.zoom)), "Open as window")
            }
            _ => (None, ""),
        };
        Some(TabDragSnap { id: d.id, x: d.p.0, y: d.p.1, preview, hint })
    }

    /// Windows under a dragged tab go see-through, as do the split halves'
    /// windows when that's where it would land.
    pub(super) fn tab_ghosts(&self) -> Vec<isize> {
        let Some(d) = self.tab_drag.as_ref().filter(|d| d.moved) else { return Vec::new() };
        let cam = (self.cam_x, self.cam_y, self.zoom);
        let mut out: Vec<isize> = self
            .wins
            .iter()
            .filter(|w| {
                let r = super::thumbs::vis_screen_rect(w, cam);
                d.p.0 >= r.left as f64 && d.p.0 < r.right as f64 && d.p.1 >= r.top as f64 && d.p.1 < r.bottom as f64
            })
            .map(|w| w.id())
            .collect();
        if let (Some(Target::Split(_)), Some(g)) = (d.target, self.tab_group(d.id)) {
            out.push(self.tabs[g].active);
            out.extend(self.tabs[g].split.map(|s| s.0));
        }
        out
    }

    // ---- split screen ------------------------------------------------------

    /// Shows tab `id` in the `left` (or right) half of its group's spot, with
    /// another tab in the other half.
    pub(super) fn split_tab(&mut self, id: isize, left: bool) {
        let Some(g) = self.tab_group(id) else { return };
        self.refresh_tab_spots();
        let grp = &self.tabs[g];
        let spot = grp.spot;
        // Who's on each side now, if split.
        let (cur_l, cur_r) = match grp.split {
            Some((o, true)) => (Some(o), Some(grp.active)),
            Some((o, false)) => (Some(grp.active), Some(o)),
            None => (None, None),
        };
        let partner = match (cur_l, cur_r) {
            // Split already: the other side stays, unless it's this tab
            // (then they swap).
            (Some(a), Some(b)) => {
                let other_side = if left { b } else { a };
                if other_side == id { if left { a } else { b } } else { other_side }
            }
            // Not split: the shown tab, or (dragging that) the next one.
            _ if grp.active != id => grp.active,
            _ => {
                let at = grp.ids.iter().position(|&o| o == id).unwrap_or(0);
                let n = grp.ids.len();
                match (1..n).map(|k| grp.ids[(at + k) % n]).next() {
                    Some(o) => o,
                    None => return,
                }
            }
        };
        let shown_before: Vec<isize> = [Some(grp.active), grp.split.map(|s| s.0)].into_iter().flatten().collect();
        let (lspot, rspot) = halves(spot);
        for (who, half) in [(id, if left { lspot } else { rspot }), (partner, if left { rspot } else { lspot })] {
            if let Some(k) = self.tabbed.iter().position(|w| w.id() == who) {
                let w = self.tabbed.remove(k);
                self.parked.remove(&who);
                self.wins.insert(0, w);
                if self.shown {
                    self.register(0);
                }
            }
            if let Some(i) = self.index(who) {
                fit(&mut self.wins[i], half);
                self.show_tab(i, who == id);
            }
        }
        for old in shown_before.into_iter().filter(|&o| o != id && o != partner) {
            if let Some(i) = self.index(old) {
                let w = self.wins.remove(i);
                self.hide_tab(w, spot);
            }
        }
        let g = self.tab_group(id).unwrap_or(g);
        self.sticks.retain(|s| !s.iter().any(|o| self.tabs[g].ids.contains(o)));
        self.sticks.push(vec![id, partner]);
        self.tabs[g].active = id;
        self.tabs[g].split = Some((partner, !left));
        self.tabs[g].spot = spot;
        wins::save_parked(&self.parked);
        log!("split screen: two tabs side by side");
        self.update_view();
    }

    /// Back to one tab at a time: the active one takes the whole spot.
    pub(super) fn unsplit(&mut self, g: usize) {
        let Some((other, _)) = self.tabs[g].split.take() else { return };
        self.refresh_tab_spots_one(g, other);
        let (active, spot) = (self.tabs[g].active, self.tabs[g].spot);
        self.sticks.retain(|s| !(s.contains(&active) && s.contains(&other)));
        if let Some(i) = self.index(other) {
            let w = self.wins.remove(i);
            self.hide_tab(w, spot);
        }
        if let Some(i) = self.index(active) {
            fit(&mut self.wins[i], spot);
            self.show_tab(i, false);
        }
        wins::save_parked(&self.parked);
        self.update_view();
    }

    /// Union of the split halves into `spot`, before `other` goes.
    fn refresh_tab_spots_one(&mut self, g: usize, other: isize) {
        let rects: Vec<_> =
            [self.tabs[g].active, other].iter().filter_map(|&id| self.index(id)).map(|i| self.wins[i].vis()).collect();
        if rects.is_empty() {
            return;
        }
        let l = rects.iter().map(|r| r.0).fold(f64::MAX, f64::min);
        let t = rects.iter().map(|r| r.1).fold(f64::MAX, f64::min);
        let r = rects.iter().map(|r| r.0 + r.2).fold(f64::MIN, f64::max);
        let b = rects.iter().map(|r| r.1 + r.3).fold(f64::MIN, f64::max);
        self.tabs[g].spot = (l, t, r - l, b - t);
    }

    /// Takes `id` out of its group: dropped at screen point `at` (or just
    /// beside the group).
    pub(super) fn detach(&mut self, id: isize, at: Option<(f64, f64)>) {
        let Some(g) = self.tab_group(id) else { return };
        if self.tabs[g].split.is_some_and(|s| s.0 == id) || (self.tabs[g].split.is_some() && self.tabs[g].active == id) {
            // A split half: the other half takes the group's whole spot, and
            // this one stays a window of its own.
            if self.tabs[g].active == id {
                if let Some((o, l)) = self.tabs[g].split {
                    self.tabs[g].active = o;
                    self.tabs[g].split = Some((id, !l));
                }
            }
            self.refresh_tab_spots_one(g, id);
            let (active, spot) = (self.tabs[g].active, self.tabs[g].spot);
            self.tabs[g].split = None;
            self.sticks.retain(|s| !(s.contains(&active) && s.contains(&id)));
            if let Some(i) = self.index(active) {
                fit(&mut self.wins[i], spot);
                self.show_tab(i, false);
            }
        } else if self.tabs[g].active == id {
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
            match at {
                Some(_) => {
                    self.parked.remove(&id);
                    self.wins.insert(0, w);
                    if self.shown {
                        self.register(0);
                    }
                }
                None => self.pop_out(w, 1.0),
            }
        }
        if let (Some(p), Some(i)) = (at, self.index(id)) {
            let (cx, cy) = (self.cam_x + p.0 / self.zoom, self.cam_y + p.1 / self.zoom);
            let (_, _, vw, vh) = self.wins[i].vis();
            fit(&mut self.wins[i], (cx - GRAB.0, cy - GRAB.1, vw, vh));
            let w = self.wins.remove(i);
            self.wins.insert(0, w);
            if self.shown {
                self.raise(0);
            }
            self.show_tab(0, true);
        }
        self.prune_tabs();
        wins::save_parked(&self.parked);
        self.update_view();
    }
}
