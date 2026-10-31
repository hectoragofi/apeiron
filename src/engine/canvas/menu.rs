//! The right-click menu and the actions behind it (also run from gestures
//! and shortcuts): close, minimize, fill, gather.

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_D, VK_LWIN, VK_TAB};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::engine::{tidy, wins};
use crate::settings::GestureAction;
use crate::shared::{MenuItem, MenuSnap, UiEvent};

use super::{Canvas, Win};
use super::camera::WinMove;
use super::resize::resize_window;

/// What a menu item does (the windows it acts on are fixed when it opens).
#[derive(Clone, PartialEq)]
pub(super) enum MenuAction {
    Open(isize),
    /// The screen's work area, or its left half.
    Fill(isize, bool),
    OnTop(isize, bool),
    Select(Vec<isize>),
    Gather(Vec<isize>),
    Minimize(Vec<isize>),
    Close(Vec<isize>),
    Stick(Vec<isize>),
    Unstick(Vec<isize>),
    Tidy,
    FitAll,
    Search,
    Overview,
    Back,
    Pause,
    Settings,
}

/// The items of a menu and, alongside, what each does (None for dividers).
#[derive(Default)]
struct MenuBuilder {
    items: Vec<MenuItem>,
    actions: Vec<Option<MenuAction>>,
}

impl MenuBuilder {
    fn push(&mut self, item: MenuItem, action: Option<MenuAction>) {
        self.items.push(item);
        self.actions.push(action);
    }

    fn item(&mut self, label: &str, hint: &str, action: MenuAction) {
        self.push(MenuItem { label: label.into(), hint: hint.into(), ..Default::default() }, Some(action));
    }

    fn check(&mut self, label: &str, on: bool, action: MenuAction) {
        self.push(MenuItem { label: label.into(), check: on, ..Default::default() }, Some(action));
    }

    fn danger(&mut self, label: &str, hint: &str, action: MenuAction) {
        self.push(MenuItem { label: label.into(), hint: hint.into(), danger: true, ..Default::default() }, Some(action));
    }

    fn sep(&mut self) {
        self.push(MenuItem { separator: true, ..Default::default() }, None);
    }
}

/// "chrome.exe" -> "Chrome"; the title when the app is unknown.
fn app_name(exe: &str, title: &str) -> String {
    let base = exe.strip_suffix(".exe").unwrap_or(exe);
    let name = if base.is_empty() { title } else { base };
    let mut c = name.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

impl Canvas {
    /// Right click: what can be done with the window under the cursor (and
    /// the rest of the selection, if it's part of it), or with the canvas.
    pub(super) fn context_menu(&mut self, p: (f64, f64)) {
        self.drag = None;
        let target = if self.shown { self.hit(p).map(|i| self.wins[i].id()) } else { None };
        if let Some(id) = target {
            if !self.selected.contains(&id) {
                self.selected = vec![id];
            }
        }
        let mut m = MenuBuilder::default();
        let sel = self.selected.clone();
        let cfg = self.cfg();
        let stuck = sel.iter().any(|id| self.sticks.iter().any(|g| g.contains(id)));
        match target {
            Some(id) if sel.len() == 1 => {
                let w = &self.wins[self.index(id).unwrap_or(0)];
                let (h, exe) = (w.hwnd, w.exe.clone());
                let app = app_name(&exe, &w.title);
                let same: Vec<isize> = self.wins.iter().filter(|o| !exe.is_empty() && o.exe == exe).map(Win::id).collect();
                let on_top = unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0 != 0;
                m.item("Open", "Click", MenuAction::Open(id));
                m.sep();
                m.item("Fill the screen", "", MenuAction::Fill(id, false));
                m.item("Half the screen", "", MenuAction::Fill(id, true));
                m.check("Always on top", on_top, MenuAction::OnTop(id, !on_top));
                if stuck {
                    m.item("Unstick", "", MenuAction::Unstick(vec![id]));
                }
                if same.len() > 1 {
                    m.sep();
                    m.item(&format!("Select all {app} windows"), "", MenuAction::Select(same.clone()));
                    m.item(&format!("Gather {app} windows"), "", MenuAction::Gather(same.clone()));
                }
                m.sep();
                m.item("Minimize", "", MenuAction::Minimize(vec![id]));
                m.danger("Close", "Del", MenuAction::Close(vec![id]));
                if same.len() > 1 {
                    m.danger(&format!("Close all {app} windows"), "", MenuAction::Close(same));
                }
            }
            Some(_) => {
                let n = sel.len();
                m.item(&format!("Gather {n} windows"), "", MenuAction::Gather(sel.clone()));
                m.item(&format!("Stick {n} windows together"), "", MenuAction::Stick(sel.clone()));
                if stuck {
                    m.item("Unstick", "", MenuAction::Unstick(sel.clone()));
                }
                m.item("Clear selection", "Esc", MenuAction::Select(Vec::new()));
                m.sep();
                m.item(&format!("Minimize {n} windows"), "", MenuAction::Minimize(sel.clone()));
                m.danger(&format!("Close {n} windows"), "Del", MenuAction::Close(sel));
            }
            None if self.shown => {
                m.item("Tidy up", &cfg.key_tidy, MenuAction::Tidy);
                m.item("Fit all windows", "", MenuAction::FitAll);
                m.item("Select all", "", MenuAction::Select(self.wins.iter().map(Win::id).collect()));
                m.item("Find a window", &cfg.key_find, MenuAction::Search);
                m.sep();
                m.item("Back to windows", "Esc", MenuAction::Back);
                m.item("Settings", "", MenuAction::Settings);
            }
            None => {
                m.item("Overview", &cfg.key_overview, MenuAction::Overview);
                m.item("Tidy up", &cfg.key_tidy, MenuAction::Tidy);
                m.item("Find a window", &cfg.key_find, MenuAction::Search);
                m.sep();
                m.item(if self.paused { "Resume Apeiron" } else { "Pause Apeiron" }, "", MenuAction::Pause);
                m.item("Settings", "", MenuAction::Settings);
            }
        }
        self.menu_id += 1;
        let snap = MenuSnap { x: p.0, y: p.1, items: m.items, id: self.menu_id };
        self.menu = Some((m.actions, snap));
        // Esc closes it, whatever is up.
        self.register_keys();
        self.dirty = true;
    }

    pub(super) fn close_menu(&mut self) {
        if self.menu.take().is_some() {
            self.dirty = true;
            if !self.overview && self.search.is_none() {
                self.unregister_esc();
            }
        }
    }

    pub(super) fn menu_pick(&mut self, k: usize) {
        let Some((actions, _)) = self.menu.as_ref() else { return };
        let Some(Some(action)) = actions.get(k).cloned() else { return };
        self.close_menu();
        match action {
            MenuAction::Open(id) => {
                self.selected.clear();
                if let Some(i) = self.index(id) {
                    self.focus(i);
                }
            }
            MenuAction::Fill(id, half) => self.fill(id, half),
            MenuAction::OnTop(id, on) => {
                if let Some(i) = self.index(id) {
                    let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
                    let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
                    unsafe {
                        let _ = SetWindowPos(self.wins[i].hwnd, Some(after), 0, 0, 0, 0, flags);
                    }
                }
            }
            MenuAction::Select(ids) => self.selected = ids,
            MenuAction::Gather(ids) => self.gather(&ids),
            MenuAction::Minimize(ids) => self.syscommand(&ids, SC_MINIMIZE),
            MenuAction::Close(ids) => self.close_windows(&ids),
            MenuAction::Stick(ids) => self.stick(&ids),
            MenuAction::Unstick(ids) => self.unstick(&ids),
            MenuAction::Tidy => self.tidy(),
            MenuAction::FitAll => {
                if let Some(b) = self.bounds() {
                    let (x, y, z) = self.frame(b);
                    self.animate_to(x, y, z, false, None);
                }
            }
            MenuAction::Search => self.open_search(),
            MenuAction::Overview => self.toggle_overview(),
            MenuAction::Back => self.leave_overview(),
            MenuAction::Pause => self.set_paused(!self.paused),
            MenuAction::Settings => self.shared.emit(UiEvent::OpenSettings),
        }
        self.dirty = true;
    }

    /// Resizes a window to the screen's work area (or its left half) and
    /// lands on it, so it fills exactly that.
    fn fill(&mut self, id: isize, half: bool) {
        let Some(i) = self.index(id) else { return };
        let work = wins::primary_work();
        let (ww, wh) = ((work.right - work.left) as f64, (work.bottom - work.top) as f64);
        let w = &mut self.wins[i];
        let (l, t, r, b) = w.inset;
        (w.w, w.h) = ((if half { ww / 2.0 } else { ww }) + l + r, wh + t + b);
        let (x, y) = (w.x + l - work.left as f64, w.y + t - work.top as f64);
        resize_window(w);
        self.selected.clear();
        self.animate_to(x, y, 1.0, true, Some(self.wins[i].hwnd));
    }

    /// What a custom gesture was set to do.
    pub(super) fn run_gesture(&mut self, a: GestureAction) {
        match a {
            GestureAction::Nothing => return,
            GestureAction::Overview if !self.overview => self.toggle_overview(),
            GestureAction::Overview => {}
            GestureAction::ToggleOverview => self.toggle_overview(),
            GestureAction::Back if self.overview => self.leave_overview(),
            GestureAction::Back => {}
            GestureAction::Tidy => self.tidy(),
            GestureAction::Search => self.open_search(),
            GestureAction::NextDesktop => wins::switch_desktop(true),
            GestureAction::PrevDesktop => wins::switch_desktop(false),
            GestureAction::TaskView => wins::send_keys(&[VK_LWIN], VK_TAB),
            GestureAction::ShowDesktop => wins::send_keys(&[VK_LWIN], VK_D),
        }
        self.update_view();
    }

    /// Asks these windows to close, as their own close buttons would (an
    /// app with unsaved work asks first). They leave the canvas once gone.
    pub(super) fn close_windows(&mut self, ids: &[isize]) {
        self.syscommand(ids, SC_CLOSE);
    }

    /// Delete in the overview: closes the selection, or the window under
    /// the cursor.
    pub fn delete_key(&mut self) {
        let ids: Vec<isize> = if self.selected.is_empty() { self.hovered.into_iter().collect() } else { self.selected.clone() };
        self.close_windows(&ids);
    }

    fn syscommand(&mut self, ids: &[isize], cmd: u32) {
        for w in self.wins.iter().filter(|w| ids.contains(&w.id())) {
            unsafe {
                let _ = PostMessageW(Some(w.hwnd), WM_SYSCOMMAND, WPARAM(cmd as usize), LPARAM(0));
            }
        }
        self.selected.retain(|id| !ids.contains(id));
        self.dirty = true;
    }

    /// Lines the windows up side by side, tidily, where they are now.
    fn gather(&mut self, ids: &[isize]) {
        let idx: Vec<usize> = ids.iter().filter_map(|&id| self.index(id)).collect();
        if idx.len() < 2 {
            return;
        }
        let rects: Vec<_> = idx.iter().map(|&i| (self.wins[i].x, self.wins[i].y, self.wins[i].w, self.wins[i].h)).collect();
        let order: Vec<usize> = tidy::reading_order(&rects).into_iter().map(|k| idx[k]).collect();
        let sizes: Vec<(f64, f64)> = order.iter().map(|&i| (self.wins[i].w, self.wins[i].h)).collect();
        let spots = tidy::arrange(&sizes, self.cfg().gap.round());
        let (w, h) = spots.iter().zip(&sizes).fold((0.0_f64, 0.0_f64), |(w, h), (p, s)| (w.max(p.0 + s.0), h.max(p.1 + s.1)));
        let n = rects.len() as f64;
        let (cx, cy) = rects.iter().fold((0.0, 0.0), |(x, y), r| (x + (r.0 + r.2 / 2.0) / n, y + (r.1 + r.3 / 2.0) / n));
        let (ox, oy) = ((cx - w / 2.0).round(), (cy - h / 2.0).round());
        let moves: Vec<WinMove> = order
            .iter()
            .zip(&spots)
            .map(|(&i, &(x, y))| (self.wins[i].id(), (self.wins[i].x, self.wins[i].y), (ox + x, oy + y)))
            .collect();
        self.animate_to(self.cam_x, self.cam_y, self.zoom, false, None);
        if let Some(a) = self.anim.as_mut() {
            a.moves = moves;
        }
    }
}
