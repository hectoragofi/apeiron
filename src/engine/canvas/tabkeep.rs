//! Tab groups across restarts. They're saved with the layout as their
//! windows' keys (see `layout::match_tabs`), and put back together when
//! those windows are found again: at startup, or whenever their app opens
//! them later.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsWindow, SetTimer};

use crate::engine::wins;
use crate::layout::{match_tabs, unix_now, SavedTabs, WinKey};

use super::tabs::fit;
use super::{Canvas, Win, SAVE_DELAY_MS, TIMER_SAVE};

/// For this long after start, a saved group's windows may be recognised by
/// their app alone (titles change across a restart), and a group that came
/// back short of some windows still takes them in.
const LOOSE_FOR: Duration = Duration::from_secs(180);

/// A saved group waiting for its windows.
struct Pending {
    saved: SavedTabs,
    /// The window found for each member so far.
    got: Vec<Option<isize>>,
    /// Its windows were made tabs again; ones found later join them.
    formed: bool,
}

/// Each group's tabs in order, the shown one and the split, as last saved.
type Shape = Vec<(Vec<isize>, isize, Option<(isize, bool)>)>;

pub(super) struct TabKeep {
    pending: Vec<Pending>,
    since: Instant,
    saved: Shape,
    /// The groups as of the last sync, with their windows' keys: what to
    /// remember of one whose windows close.
    last: Vec<(Vec<isize>, SavedTabs)>,
    /// Windows the user closed from Apeiron: their groups aren't kept.
    closed_here: Vec<isize>,
}

impl TabKeep {
    pub(super) fn new() -> Self {
        TabKeep { pending: Vec::new(), since: Instant::now(), saved: Vec::new(), last: Vec::new(), closed_here: Vec::new() }
    }
}

impl Canvas {
    /// At startup: the groups saved last time start waiting for their
    /// windows.
    pub(super) fn load_tabs(&mut self) {
        self.keep.since = Instant::now();
        self.keep.pending = self
            .layout
            .tab_groups(unix_now())
            .into_iter()
            .filter(|s| s.members.len() >= 2)
            .map(|saved| Pending { got: vec![None; saved.members.len()], saved, formed: false })
            .collect();
        if !self.keep.pending.is_empty() {
            log!("{} saved tab groups waiting for their windows", self.keep.pending.len());
        }
    }

    /// The live groups as they'd be saved, with their windows.
    fn saved_groups(&self) -> Vec<(Vec<isize>, SavedTabs)> {
        let now = unix_now();
        // To the hour, so an unchanged group doesn't rewrite the file.
        let seen = now - now % 3600;
        let find = |id: isize| self.wins.iter().chain(&self.tabbed).chain(&self.away).find(|w| w.id() == id);
        self.tabs
            .iter()
            .filter_map(|g| {
                let members: Vec<&Win> = g.ids.iter().filter_map(|&id| find(id)).collect();
                if members.len() < 2 {
                    return None;
                }
                let at = |id: isize| members.iter().position(|w| w.id() == id);
                let saved = SavedTabs {
                    members: members.iter().map(|w| w.key()).collect(),
                    active: at(g.active).unwrap_or(0),
                    split: g.split.and_then(|(o, left)| at(o).map(|k| (k, left))),
                    seen,
                };
                Some((members.iter().map(|w| w.id()).collect(), saved))
            })
            .collect()
    }

    /// Records the groups in the layout: the live ones, then the saved ones
    /// still waiting for their windows.
    pub(super) fn capture_tabs(&mut self) {
        let live: Vec<SavedTabs> = self.saved_groups().into_iter().map(|(_, s)| s).collect();
        let waiting: Vec<SavedTabs> = self.keep.pending.iter().filter(|p| !p.formed).map(|p| p.saved.clone()).collect();
        self.layout.capture_tabs(live, &waiting, unix_now());
        self.keep.saved = self.tab_shape();
    }

    /// Notes the groups and their windows' titles (after each sync).
    pub(super) fn note_tab_members(&mut self) {
        self.keep.last = self.saved_groups();
    }

    /// These windows are being closed on purpose (from a menu): groups
    /// they leave aren't kept for later.
    pub(super) fn closing_on_purpose(&mut self, ids: &[isize]) {
        self.keep.closed_here.extend_from_slice(ids);
    }

    /// A sync found windows gone (`alive` are those left). A group whose
    /// windows all closed, or several at once (its app quit), is
    /// remembered, to come back when they do; one tab closed is just that.
    pub(super) fn remember_closed_tabs(&mut self, alive: &[isize]) {
        // Gone from the canvas but still there (another virtual desktop)
        // isn't closed.
        let gone = |id: isize| !alive.contains(&id) && !unsafe { IsWindow(Some(HWND(id as *mut _))) }.as_bool();
        for (ids, mut saved) in std::mem::take(&mut self.keep.last) {
            let left = ids.iter().filter(|&&id| !gone(id)).count();
            let quit = ids.iter().filter(|&&id| gone(id) && !self.keep.closed_here.contains(&id)).count();
            if kept_on_close(left, quit) {
                saved.seen = unix_now();
                log!("tab group of {} windows closed; kept for when they're back", ids.len());
                self.keep.pending.push(Pending { got: vec![None; ids.len()], saved, formed: false });
            }
        }
        self.keep.closed_here.retain(|&id| !gone(id));
    }

    fn tab_shape(&self) -> Shape {
        self.tabs.iter().map(|g| (g.ids.clone(), g.active, g.split)).collect()
    }

    /// On publish: groups made, changed or undone get saved soon.
    pub(super) fn note_tab_changes(&mut self) {
        if self.keep.saved.len() == self.tabs.len()
            && self.keep.saved.iter().zip(&self.tabs).all(|(s, g)| s.0 == g.ids && s.1 == g.active && s.2 == g.split)
        {
            return;
        }
        self.keep.saved = self.tab_shape();
        self.note_tab_members();
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_SAVE, SAVE_DELAY_MS, None);
        }
    }

    /// Puts saved groups back together from the windows on the canvas in no
    /// tabs: two or more found make tabs again, in their old order, showing
    /// the tab that was shown (split, if it was); more found later join.
    pub(super) fn restore_tabs(&mut self) {
        if self.keep.pending.is_empty() {
            return;
        }
        let early = self.keep.since.elapsed() < LOOSE_FOR;
        // What no longer holds: windows found that closed or went into other
        // tabs, groups made again that were undone since, or (after the
        // start) are as complete as they'll get.
        let mut pending = std::mem::take(&mut self.keep.pending);
        pending.retain_mut(|p| {
            if p.formed {
                return early && p.got.iter().flatten().any(|&id| self.tab_group(id).is_some());
            }
            for g in &mut p.got {
                if g.is_some_and(|id| self.index(id).is_none() || self.tab_group(id).is_some()) {
                    *g = None;
                }
            }
            true
        });
        let claimed: Vec<isize> = pending.iter().flat_map(|p| p.got.iter().flatten().copied()).collect();
        let free: Vec<&Win> =
            self.wins.iter().filter(|w| self.tab_group(w.id()).is_none() && !claimed.contains(&w.id())).collect();
        let ids: Vec<isize> = free.iter().map(|w| w.id()).collect();
        let keys: Vec<WinKey> = free.iter().map(|w| w.key()).collect();
        let want: Vec<Vec<Option<WinKey>>> = pending
            .iter()
            .map(|p| p.saved.members.iter().zip(&p.got).map(|(k, g)| g.is_none().then(|| k.clone())).collect())
            .collect();
        let found = match_tabs(&want, &keys, &vec![early; pending.len()]);
        for (p, found) in pending.iter_mut().zip(found) {
            let new: Vec<isize> = found.iter().flatten().map(|&i| ids[i]).collect();
            for (g, f) in p.got.iter_mut().zip(&found) {
                if let Some(i) = f {
                    *g = Some(ids[*i]);
                }
            }
            if p.formed {
                if !new.is_empty() {
                    self.join_saved_tabs(p, &new);
                }
            } else if p.got.iter().flatten().count() >= 2 {
                self.make_saved_tabs(p);
                p.formed = true;
            }
        }
        // Complete groups need nothing more.
        pending.retain(|p| !(p.formed && p.got.iter().all(Option::is_some)));
        self.keep.pending = pending;
    }

    /// Makes the windows found for `p` tabs again.
    fn make_saved_tabs(&mut self, p: &Pending) {
        let pick = |m: usize| p.got.get(m).copied().flatten();
        let ids: Vec<isize> = p.got.iter().flatten().copied().collect();
        let active = pick(p.saved.active).unwrap_or(ids[0]);
        let split = p.saved.split.and_then(|(m, left)| pick(m).filter(|&o| o != active).map(|o| (o, left)));
        // Split: made with the other half shown at the spot both halves
        // cover, then split from there as it was.
        let show = split.map_or(active, |s| s.0);
        if let Some((other, _)) = split {
            let halves: Vec<_> = [active, other].iter().filter_map(|&id| self.index(id)).map(|i| self.wins[i].vis()).collect();
            if let (Some(i), [a, b]) = (self.index(show), halves.as_slice()) {
                let (l, t) = (a.0.min(b.0), a.1.min(b.1));
                let (r, bot) = ((a.0 + a.2).max(b.0 + b.2), (a.1 + a.3).max(b.1 + b.3));
                self.let_go(i);
                fit(&mut self.wins[i], (l, t, r - l, bot - t));
            }
        }
        self.make_tabs(&ids, show, show);
        if let Some((_, left)) = split {
            self.split_tab(active, !left);
        }
        log!("tab group of {} windows back from last time", ids.len());
        self.show_fronted_tab();
    }

    /// Hiding windows as tabs again happens without activation, so no
    /// foreground event says if the window in front was one of them: show
    /// it, rather than leave the keyboard in a window parked off screen.
    fn show_fronted_tab(&mut self) {
        let fg = unsafe { GetForegroundWindow() };
        for h in [fg, wins::root_owner(fg)] {
            if self.is_hidden_tab(h.0 as isize) {
                self.switch_tab(h.0 as isize);
                return;
            }
        }
    }

    /// Windows found later for `p`'s group: they join it as hidden tabs, in
    /// their old places.
    fn join_saved_tabs(&mut self, p: &Pending, new: &[isize]) {
        let Some(g) = p.got.iter().flatten().find_map(|&id| self.tab_group(id)) else { return };
        let spot = self.tabs[g].spot;
        self.unstick(new);
        for &id in new {
            let Some(i) = self.index(id) else { continue };
            self.let_go(i);
            let w = self.wins.remove(i);
            self.hide_tab(w, spot);
            self.tabs[g].ids.push(id);
        }
        let order: Vec<isize> = p.got.iter().flatten().copied().collect();
        self.tabs[g].ids.sort_by_key(|id| order.iter().position(|o| o == id).unwrap_or(usize::MAX));
        self.selected.retain(|id| !new.contains(id));
        wins::save_parked(&self.parked);
        self.show_fronted_tab();
        self.update_view();
    }
}

/// Whether a group is kept for later when windows of it close: `left` are
/// still there, `quit` closed by themselves (not from Apeiron's menus). All
/// gone, or several at once leaving it no group: the app quit. One closed
/// is a tab closed.
fn kept_on_close(left: usize, quit: usize) -> bool {
    quit > 0 && (left == 0 || (quit >= 2 && left < 2))
}

#[cfg(test)]
mod tests {
    use super::kept_on_close;

    #[test]
    fn groups_are_kept_when_their_app_quits_not_when_a_tab_closes() {
        // The app quit: everything closed, or all but one window.
        assert!(kept_on_close(0, 2));
        assert!(kept_on_close(0, 1));
        assert!(kept_on_close(1, 3));
        // One tab closed, or several with a group still left.
        assert!(!kept_on_close(1, 1));
        assert!(!kept_on_close(2, 2));
        // Closed from Apeiron's menus.
        assert!(!kept_on_close(1, 0));
        assert!(!kept_on_close(0, 0));
    }
}
