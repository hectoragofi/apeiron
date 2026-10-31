//! The canvas: every app window has a position on an unbounded plane and the
//! screen is a camera onto it.
//!
//! Real windows can't be scaled and repaint late when moved, so all motion
//! happens on a fullscreen overlay that shows live DWM thumbnails. At zoom 1
//! the thumbnails line up pixel-for-pixel with the real windows, so a
//! gesture looks like the windows themselves gliding. When motion stops at
//! zoom 1 the real windows are moved to match and the overlay disappears.
//!
//! This module only models and moves things; the GPUI thread draws the grid,
//! labels and so on from the `Snapshot` published by [`Canvas::flush`].

mod gesture;
mod input;
mod menu;
mod resize;
mod search;
mod thumbs;

use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT, VK_DELETE, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::ticker::{Pacer, Ticker};
use super::trace::FrameTrace;
use super::{keys, mouse, snap, tidy, wins, HOTKEY_DELETE, HOTKEY_ESC};
use crate::layout::{Layout, WinKey};
use crate::settings::Settings;
use crate::shared::{Command, MenuSnap, Mode, SearchState, Shared, Snapshot, UiEvent, WinInfo};
use crate::ui::labels;

use gesture::Gesture;
use input::{Drag, Marquee, Swipe4};
use menu::MenuAction;
use resize::Resizing;
use search::Switch;
use thumbs::{vis_screen_rect, Thumb};

pub const MIN_ZOOM: f64 = 0.04;
pub const MAX_ZOOM: f64 = 1.0;
/// A gesture that ends this close to zoom 1 snaps back to real windows.
const SNAP: f64 = 0.15;
/// Finger travel (px) before fingers count as a gesture, not a tap.
const START_SLOP: f64 = 6.0;
const ANIM_SECS: f64 = 0.32;
/// A pinch must also change the finger spread by this much (log ratio), so
/// a drag whose fingers splay as it starts stays a drag.
const PINCH_MIN: f64 = 0.08;
/// Fingers land one at a time; a new set of fingers has to stay the same
/// this long (and this many frames) before its motion counts.
const SETTLE: Duration = Duration::from_millis(40);
const SETTLE_FRAMES: u32 = 3;
/// Fingers that stay within STILL_PX for REST are resting: what they did
/// before (landing drift, sensor noise) doesn't count toward deciding.
const STILL_PX: f64 = 1.5;
const REST: Duration = Duration::from_millis(100);
/// After a finger lifts mid-gesture the others are ignored this long, while
/// the hand relaxes.
const LIFT_HOLD: Duration = Duration::from_millis(60);
/// Fingers count as down for the animation only while frames keep coming.
const FINGERS_GONE_SECS: f64 = 0.25;
/// The flick is the average finger speed over this much of the end of a drag.
/// Frames an animation that brings the overlay up waits before moving.
const WARMUP_FRAMES: u8 = 2;
const FLICK_WINDOW: Duration = Duration::from_millis(90);
/// Fastest flick, finger px/s.
const MAX_FLICK: f64 = 4000.0;
/// Finger travel (px, a full pad width is the screen width) that makes a
/// four-finger swipe.
const SWIPE4_PX: f64 = 140.0;
/// How far (screen px) either side of a window's edge grabs it to resize.
const GRIP: f64 = 8.0;
/// Corners: this far (screen px) along an edge from a corner grabs both.
const CORNER: f64 = 18.0;
/// While resizing, a window that hasn't taken the last size by now is
/// asked again (it may refuse it: a minimum size).
const RESIZE_WAIT: Duration = Duration::from_millis(80);
/// After the mouse is up, how long the thumbnail keeps following the window.
const RESIZE_TAIL: Duration = Duration::from_millis(400);
/// Smallest a window gets by dragging its edges.
const MIN_W: f64 = 240.0;
const MIN_H: f64 = 160.0;
/// A tap: fingers down and up this fast, moving this little (px).
const TAP_TIME: Duration = Duration::from_millis(250);
const TAP_SLOP: f64 = 14.0;
/// While moving, a newly opened window joins the canvas this soon.
const ADOPT_DELAY_MS: u32 = 60;
/// Focus changes this soon after a commit or a desktop switch never move
/// the camera.
const FOCUS_QUIET: Duration = Duration::from_millis(1200);
/// Wheel (two-finger scroll / pinch) gestures end after this much quiet.
const WHEEL_IDLE_MS: u32 = 140;
/// How much one mouse-wheel notch zooms: e^0.2, about 22%.
const MOUSE_ZOOM_STEP: f64 = 0.2;
/// Windows also turns touchpad motion into coarse, laggy wheel events; those
/// are ignored for this long after the last precision-touchpad frame.
const WHEEL_QUIET_SECS: f64 = 1.5;
/// How long the overlay stays up after real windows are moved, so they have
/// time to land before it disappears.
const HIDE_DELAY_MS: u32 = 70;
const SAVE_DELAY_MS: u32 = 1000;
/// Window events elsewhere on the desktop are batched this long before the
/// idle window list is refreshed.
const REFRESH_DELAY_MS: u32 = 300;
/// However steadily events keep coming (an app churning child windows), the
/// refresh waits at most this long after the first one it hasn't serviced.
const REFRESH_MAX_MS: u32 = 1000;
const ADOPT_MAX_MS: u32 = 250;
/// Windows dragged by hand on the desktop are followed at most this often,
/// so the background's minimap keeps up without waking the UI per event.
/// Timers tick in 15.6 ms steps by default, so this lands near 30 Hz.
const MOVED_DELAY_MS: u32 = 25;
/// Thumbnails outlive the overlay by this long: it hides asynchronously, and
/// releasing them first would flash the empty canvas where windows were.
const RELEASE_DELAY_MS: u32 = 150;
/// Display changes come in bursts; act once they've settled.
const SCREEN_DELAY_MS: u32 = 250;
/// Thumbnail opacity of windows that don't match the search.
const DIMMED: u8 = 70;
/// Space kept around a search result the camera frames, physical pixels.
const FRAME_MARGIN: f64 = 48.0;
/// Space kept around everything when zoomed out to fit, physical pixels.
const FIT_MARGIN: f64 = 80.0;

pub const TIMER_HIDE: usize = 2;
pub const TIMER_WHEEL: usize = 3;
pub const TIMER_SAVE: usize = 4;
pub const TIMER_REFRESH: usize = 5;
pub const TIMER_SCREEN: usize = 6;
pub const TIMER_MOVED: usize = 7;
pub const TIMER_RELEASE: usize = 8;
pub const TIMER_HOVER: usize = 9;
/// Alt+Tab held this long brings up the switcher panel; a quicker
/// Alt+Tab just switches.
pub const TIMER_SWITCH: usize = 10;
const SWITCH_DELAY_MS: u32 = 140;
/// Minimized and hidden windows remembered at most.
const MAX_AWAY: usize = 64;

/// How long the pointer rests on a window in the overview before it gets
/// the keyboard (a sweep across windows shouldn't focus each one).
const HOVER_FOCUS_MS: u32 = 150;
/// A release slower than this glide (screen pixels) isn't a flick.
const FLICK_MIN: f64 = 80.0;
/// How close (screen pixels) a dragged window's edge has to come to snap.
const SNAP_REACH: f64 = 10.0;

struct Win {
    hwnd: HWND,
    title: String,
    exe: String,
    class: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    /// Where we last put the real window; if it's still there, our canvas
    /// position is authoritative (the window may be clamped off-screen).
    placed: Option<RECT>,
    /// DWM thumbnails: the main one, and a second that shows the part under
    /// the minimap when the minimap would cut a window into an L.
    thumbs: [isize; 2],
    shown: [Option<Thumb>; 2],
    /// Invisible resize borders (left, top, right, bottom): the window rect
    /// is bigger than what's drawn.
    inset: (f64, f64, f64, f64),
    /// The size last asked of the real window while resizing, and when.
    asked: Option<((i32, i32), Instant)>,
}

impl Win {
    fn id(&self) -> isize {
        self.hwnd.0 as isize
    }

    /// The visible frame `(x, y, w, h)` on the canvas.
    fn vis(&self) -> (f64, f64, f64, f64) {
        let (l, t, r, b) = self.inset;
        (self.x + l, self.y + t, self.w - l - r, self.h - t - b)
    }

    fn key(&self) -> WinKey {
        WinKey { exe: self.exe.clone(), class: self.class.clone(), title: self.title.clone() }
    }
}

/// How `sync` treats a window when finding it a spot.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Known,
    /// Found at startup with no spot yet (saved or free).
    New,
}

/// A window gliding between canvas positions: (id, from, to).
type WinMove = (isize, (f64, f64), (f64, f64));

struct Anim {
    from: (f64, f64, f64),
    to: (f64, f64, f64),
    start: Instant,
    /// Hand control back to real windows when done.
    commit: bool,
    focus: Option<HWND>,
    /// Windows that move on the canvas during the animation (tidy).
    moves: Vec<WinMove>,
    /// Finished, but the commit waits for resting fingers to lift.
    held: bool,
    /// Frames to wait before the clock starts: the overlay has only just
    /// been shown and its first frame isn't on screen yet.
    warmup: u8,
}

pub struct Canvas {
    /// The engine's own window (timers and hotkeys hang off it).
    hwnd: HWND,
    overlay: HWND,
    /// Fullscreen window under every app that draws the canvas itself.
    bg: HWND,
    attached: bool,
    shared: Shared,
    ticker: Ticker,
    layout: Layout,
    /// "Reset saved layout" was pressed: nothing is saved until the next start.
    layout_frozen: bool,
    /// Zoomed-out and browsing; stays up until a window is picked or Esc.
    overview: bool,
    search: Option<SearchState>,
    /// Minimized windows offered by the search (they aren't on the canvas).
    minimized: Vec<WinInfo>,
    sw: f64,
    sh: f64,
    cam_x: f64,
    cam_y: f64,
    zoom: f64,
    wins: Vec<Win>,
    shown: bool,
    anim: Option<Anim>,
    drag: Option<Drag>,
    gesture: Gesture,
    pending_focus: Option<HWND>,
    focused: Option<isize>,
    hovered: Option<isize>,
    paused: bool,
    /// The session is locked; nothing is visible, so the background hides.
    locked: bool,
    quitting: bool,
    esc_registered: bool,
    delete_registered: bool,
    /// The saved layout has been applied; windows found from now on stay
    /// where they open.
    restored: bool,
    last_commit: Option<Instant>,
    /// The overlay was just brought up; the next animation waits for it.
    fresh: bool,
    /// Last time a window on the canvas was cloaked or uncloaked (a virtual
    /// desktop switch).
    desktop_moved: Option<Instant>,
    /// Four fingers came down: three-finger motion is off until every
    /// finger lifts.
    swipe_block: bool,
    /// The current four-finger swipe is ours (Windows' own four-finger
    /// gestures are off): where it started, with how many fingers, and
    /// whether it has already done its thing.
    swipe4: Option<Swipe4>,
    /// Window edges under the cursor (`EDGE_*` bits), for its shape.
    hover_edge: u8,
    /// A window being resized by its edges (and briefly after, while the
    /// app catches up).
    resizing: Option<Resizing>,
    /// Windows picked out in the overview (Ctrl/Shift-click, a box).
    selected: Vec<isize>,
    /// Tidy up once the canvas is attached, then go back to the windows.
    launch_tidy: bool,
    /// The first window event the pending refresh hasn't serviced yet.
    refresh_since: Option<Instant>,
    /// The open right-click menu: what each item does, and what's shown.
    menu: Option<(Vec<Option<MenuAction>>, MenuSnap)>,
    menu_id: u64,
    /// Three fingers down: when, where, and how far they've wandered (a
    /// three-finger tap if they lift quickly having barely moved).
    tap3: Option<(Instant, (f64, f64), f64)>,
    /// Windows we last placed somewhere the user couldn't reach, mirrored to
    /// disk for the watchdog and the rescue CLI.
    parked: HashSet<isize>,
    /// Last precision-touchpad frame.
    last_touch: Option<Instant>,
    /// Current wheel burst: (pinch, last event of that kind).
    wheel_burst: Option<(bool, Instant)>,
    /// Camera at zoom 1 the canvas is resting at or heading for; Esc
    /// returns here.
    origin: (f64, f64),
    /// Real windows are placed and the overlay is about to hide; the
    /// background must catch up with the camera before it does.
    hiding: bool,
    /// Windows the user is dragging around the desktop while idle.
    moved: Vec<HWND>,
    /// Minimized and hidden windows, still holding their canvas spot for
    /// when they come back (most recently gone first).
    away: Vec<Win>,
    /// What the minimap shows of `away`: those on this virtual desktop.
    away_infos: Vec<WinInfo>,
    /// Windows that just came back from `away` while idle.
    returned: Vec<isize>,
    /// Groups of windows stuck together (two or more each).
    sticks: Vec<Vec<isize>>,
    /// Alt+Tab pressed, panel not up yet.
    switch: Option<Switch>,
    /// The shortcuts last registered, and whether they were suspended.
    keys_applied: Option<(String, bool)>,
    keys_suspended: bool,
    /// State changed since the last publish.
    dirty: bool,
    /// The settings as of the last `SettingsChanged`.
    settings: Rc<Settings>,
    /// The published window list, reused until something in it changes.
    win_infos: Arc<Vec<WinInfo>>,
    pacer: Pacer,
    trace: FrameTrace,
}

impl Canvas {
    pub fn new(hwnd: HWND, shared: Shared, ticker: Ticker) -> Self {
        let (sw, sh) = screen_size();
        let settings = Rc::new(shared.settings());
        Canvas {
            hwnd,
            overlay: HWND::default(),
            bg: HWND::default(),
            attached: false,
            shared,
            ticker,
            layout: Layout::load(),
            layout_frozen: false,
            overview: false,
            search: None,
            minimized: Vec::new(),
            away: Vec::new(),
            away_infos: Vec::new(),
            returned: Vec::new(),
            sticks: Vec::new(),
            switch: None,
            keys_applied: None,
            keys_suspended: false,
            sw,
            sh,
            cam_x: 0.0,
            cam_y: 0.0,
            zoom: 1.0,
            wins: Vec::new(),
            shown: false,
            anim: None,
            drag: None,
            gesture: Gesture::default(),
            pending_focus: None,
            focused: None,
            hovered: None,
            paused: false,
            locked: false,
            quitting: false,
            esc_registered: false,
            delete_registered: false,
            restored: false,
            last_commit: None,
            fresh: false,
            desktop_moved: None,
            swipe_block: false,
            swipe4: None,
            selected: Vec::new(),
            hover_edge: 0,
            resizing: None,
            launch_tidy: false,
            refresh_since: None,
            menu: None,
            menu_id: 0,
            tap3: None,
            parked: HashSet::new(),
            last_touch: None,
            wheel_burst: None,
            origin: (0.0, 0.0),
            hiding: false,
            moved: Vec::new(),
            dirty: true,
            settings,
            win_infos: Arc::default(),
            pacer: Pacer::default(),
            trace: FrameTrace::default(),
        }
    }

    fn cfg(&self) -> Rc<Settings> {
        self.settings.clone()
    }

    // ---- UI commands -------------------------------------------------------

    pub fn command(&mut self, cmd: Command) {
        if self.quitting {
            return;
        }
        if !self.attached && !matches!(cmd, Command::Attach { .. } | Command::Quit) {
            return;
        }
        match cmd {
            Command::Attach { overlay, bg } => self.attach(overlay, bg),
            Command::MouseDown { x, y, additive } => self.mouse_down((x, y), additive),
            Command::ContextMenu { x, y } => self.context_menu((x, y)),
            Command::CloseWindow(id) => self.close_windows(&[id]),
            Command::MenuPick(k) => self.menu_pick(k),
            Command::MouseMove { x, y } => self.mouse_move((x, y)),
            Command::MouseUp => self.mouse_up(),
            Command::Wheel { dx, dy, pinch, mouse: true, shift, x, y } => self.mouse_wheel(dx, dy, pinch, shift, (x, y)),
            Command::Wheel { dx, dy, pinch, x, y, .. } => self.wheel(dx, dy, pinch, (x, y)),
            Command::PanStart { x, y } => self.pan_start((x, y)),
            Command::ToggleOverview => self.toggle_overview(),
            Command::LeaveOverview => self.leave_overview(),
            Command::Tidy => self.tidy(),
            Command::FocusWindow(id) => self.focus_window(id),
            Command::ForgetAway(id) => self.forget_away(id),
            Command::FlyTo { x, y } => self.fly_to(x, y),
            Command::OpenSearch => self.open_search(),
            Command::SearchQuery(q) => self.search_query(q),
            Command::SearchMove(d) => self.search_move(d),
            Command::SearchAccept => self.search_accept(),
            Command::CloseSearch => self.close_search(),
            Command::SettingsChanged => {
                self.settings = Rc::new(self.shared.settings());
                self.apply_keys();
                self.update_view();
            }
            Command::SetPaused(p) => self.set_paused(p),
            Command::SuspendHotkeys(on) => {
                self.keys_suspended = on;
                self.apply_keys();
            }
            Command::ResetLayout => {
                // Every commit and quit would capture the current spots again;
                // forgetting means not saving for the rest of this run.
                self.stop_timer(TIMER_SAVE);
                self.layout.reset();
                self.layout.save();
                self.layout_frozen = true;
                log!("saved layout cleared");
            }
            Command::Quit => self.quit(),
        }
    }

    fn attach(&mut self, overlay: isize, bg: isize) {
        self.overlay = HWND(overlay as *mut _);
        self.bg = HWND(bg as *mut _);
        self.attached = !self.overlay.0.is_null() && !self.bg.0.is_null();
        (self.sw, self.sh) = screen_size();
        // A previous run that was killed with its watchdog (sign-out) may
        // have left windows out of reach.
        let n = wins::rescue_all();
        if n > 0 {
            log!("startup: {n} stranded windows brought back");
        }
        wins::clear_parked();
        self.restore_layout();
        self.restored = true;
        self.dirty = true;
        // Optionally, every start lays the windows out tidily (as Ctrl+Alt+T
        // does), then returns to them. Off by default: it overrides the
        // restored layout and pulls windows off other monitors into rows.
        if self.cfg().tidy_on_launch {
            self.tidy();
            self.launch_tidy = self.anim.is_some();
        }
        log!("canvas attached (screen {}x{})", self.sw, self.sh);
    }

    /// Esc: closes search first, then leaves the overview.
    pub fn escape(&mut self) {
        if self.menu.is_some() {
            self.close_menu();
        } else if self.search.is_some() {
            self.close_search();
        } else if !self.selected.is_empty() {
            self.selected.clear();
            self.dirty = true;
        } else {
            self.leave_overview();
        }
    }

    pub fn on_timer(&mut self, id: usize) {
        match id {
            TIMER_HIDE => self.hide_overlay(),
            TIMER_WHEEL => self.wheel_idle(),
            TIMER_SAVE => {
                self.stop_timer(TIMER_SAVE);
                // Mid-gesture the camera is in flux; the next commit re-arms.
                if !self.shown {
                    self.save_layout();
                }
            }
            TIMER_REFRESH => {
                self.stop_timer(TIMER_REFRESH);
                self.refresh_since = None;
                if self.attached && !self.quitting {
                    if self.shown {
                        self.adopt();
                    } else {
                        self.sync(false);
                        self.welcome_back();
                    }
                }
            }
            TIMER_SCREEN => self.apply_screen(),
            TIMER_MOVED => self.follow_moves(),
            TIMER_RELEASE if !self.shown => self.release_thumbnails(),
            TIMER_HOVER => self.hover_focus(),
            TIMER_SWITCH => self.show_switcher(),
            _ => {}
        }
    }

    /// A composed frame went by while animating.
    pub fn on_tick(&mut self) {
        self.tick();
    }

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

    fn follow_moves(&mut self) {
        self.stop_timer(TIMER_MOVED);
        let moved = std::mem::take(&mut self.moved);
        if self.shown || self.quitting {
            return;
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
    fn stuck_with(&self, id: isize) -> Vec<isize> {
        self.sticks.iter().find(|g| g.contains(&id)).cloned().unwrap_or_else(|| vec![id])
    }

    /// `ids` and everything stuck to any of them.
    fn with_partners(&self, ids: &[isize]) -> Vec<isize> {
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
    fn stick(&mut self, ids: &[isize]) {
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
    fn unstick(&mut self, ids: &[isize]) {
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
    fn welcome_back(&mut self) {
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

    /// Something brought a window to the front (Alt+Tab, the taskbar, a
    /// notification). If it's parked out of sight, fly there.
    pub fn foreground_changed(&mut self, h: HWND) {
        if !self.attached || self.shown || self.quitting {
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

    fn apply_screen(&mut self) {
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

    /// Motion has stopped: glide out the flick, then hand back to real
    /// windows if we're near 1:1, otherwise stay zoomed out. The smart
    /// camera (each part a setting) bends where it comes to rest.
    fn settle(&mut self, vx: f64, vy: f64) {
        let cfg = self.cfg();
        let tx = self.cam_x - vx * cfg.flick / self.zoom;
        let ty = self.cam_y - vy * cfg.flick / self.zoom;
        // Near 1:1 always means "back to the windows", from the overview too;
        // a search stays up until it's answered or closed.
        let near_one = (self.zoom - 1.0).abs() < SNAP;
        let smart = self.search.is_none();
        if smart && cfg.magnetic_flick && vx.hypot(vy) * cfg.flick > FLICK_MIN {
            if let Some(i) = self.flick_target(tx, ty) {
                let z = if near_one { 1.0 } else { self.zoom };
                let (x, y) = snap::centred(self.wins[i].vis(), (self.sw / z, self.sh / z));
                if near_one {
                    let h = self.wins[i].hwnd;
                    self.animate_to(x, y, 1.0, true, Some(h));
                } else {
                    self.animate_to(x, y, z, false, None);
                    if !self.overview {
                        self.enter_overview();
                    }
                }
                return;
            }
        }
        if near_one && smart {
            self.land(tx, ty, None);
            return;
        }
        let (x, y, z) = match smart.then(|| self.rest_zoomed_out(tx, ty, &cfg)).flatten() {
            Some(rest) => rest,
            None => (tx, ty, self.zoom),
        };
        self.animate_to(x, y, z, false, None);
        if !self.overview {
            self.enter_overview();
        }
    }

    /// Glide to zoom 1 from camera `(tx, ty)` at the current zoom, keeping
    /// the point under the cursor fixed, then hand back to real windows.
    /// With no window to focus given, it frames (and focuses) the one it
    /// lands on, if that's on.
    fn land(&mut self, tx: f64, ty: f64, focus: Option<HWND>) {
        let p = cursor();
        let (wx, wy) = (tx + p.0 / self.zoom, ty + p.1 / self.zoom);
        let (mut x, mut y, mut focus) = (wx - p.0, wy - p.1, focus);
        if focus.is_none() && self.search.is_none() && self.cfg().land_framed {
            if let Some(((fx, fy), i)) = snap::framing(&self.vis_rects(), (x, y, self.sw, self.sh)) {
                (x, y, focus) = (fx, fy, Some(self.wins[i].hwnd));
            }
        }
        self.animate_to(x, y, 1.0, true, focus);
    }

    /// The window a flick toward camera `(tx, ty)` should stop on.
    fn flick_target(&self, tx: f64, ty: f64) -> Option<usize> {
        let (vw, vh) = (self.sw / self.zoom, self.sh / self.zoom);
        let from = (self.cam_x + vw / 2.0, self.cam_y + vh / 2.0);
        let travel = (tx - self.cam_x, ty - self.cam_y);
        let reach = (travel.0.hypot(travel.1) * 3.0).max(vw.max(vh) * 1.5);
        snap::flick_target(&self.vis_rects(), from, travel, reach)
    }

    /// Where a zoomed-out camera headed for `(tx, ty)` rests instead: fitting
    /// the window or group a pinch nearly fits, or centred on the groups
    /// mostly in view. As `(x, y, zoom)`.
    fn rest_zoomed_out(&self, tx: f64, ty: f64, cfg: &Settings) -> Option<(f64, f64, f64)> {
        let rects = self.vis_rects();
        let all = snap::hull(&rects)?;
        let groups: Vec<snap::Rect> =
            snap::clusters(&rects, snap::CLUSTER_GAP).iter().map(|g| snap::bbox(&rects, g)).collect();
        if cfg.zoom_fit {
            // A pinch zooms about the cursor, so that's what it's after.
            let p = cursor();
            let at = (tx + p.0 / self.zoom, ty + p.1 / self.zoom);
            let mut candidates: Vec<snap::Rect> =
                rects.iter().chain(&groups).copied().filter(|&r| snap::contains(r, at)).collect();
            candidates.push(all);
            let fit = snap::zoom_fit(&candidates, self.zoom, (self.sw, self.sh), FIT_MARGIN, MIN_ZOOM, 1.0 - SNAP);
            if let Some((r, z)) = fit {
                let (x, y) = snap::centred(r, (self.sw / z, self.sh / z));
                return Some((x, y, z));
            }
        }
        if cfg.overview_snap {
            let view = (tx, ty, self.sw / self.zoom, self.sh / self.zoom);
            if let Some(((x, y), _)) = snap::framing(&groups, view) {
                return Some((x, y, self.zoom));
            }
        }
        None
    }

    fn vis_rects(&self) -> Vec<snap::Rect> {
        self.wins.iter().map(Win::vis).collect()
    }

    /// Where snapping pulls the windows in `group`: their bounds lined up
    /// with the other windows around them.
    fn snap_group(&self, group: &[isize]) -> ((f64, f64), Vec<snap::Guide>) {
        let (mine, others): (Vec<&Win>, Vec<&Win>) = self.wins.iter().partition(|w| group.contains(&w.id()));
        let mine: Vec<snap::Rect> = mine.into_iter().map(Win::vis).collect();
        let others: Vec<snap::Rect> = others.into_iter().map(Win::vis).collect();
        match snap::hull(&mine) {
            Some(r) => snap::align(r, &others, self.cfg().gap, SNAP_REACH / self.zoom),
            None => ((0.0, 0.0), Vec::new()),
        }
    }

    // ---- hotkeys and commands ------------------------------------------------

    /// Zoom out to fit every window, or back in if already zoomed out.
    fn toggle_overview(&mut self) {
        if self.overview {
            self.leave_overview();
            return;
        }
        if !self.shown && self.nothing_to_show() {
            return;
        }
        // Mid-gesture or settling counts as "not in the overview": zoom out
        // from wherever the camera is.
        self.begin();
        let Some(b) = self.bounds() else { return };
        let (cx, cy, z) = self.frame(b);
        self.animate_to(cx, cy, z, false, None);
        self.enter_overview();
    }

    /// With no app windows an overview would be an empty screen-sized grid
    /// with nothing to click; don't bring the overlay up for it.
    fn nothing_to_show(&self) -> bool {
        // While idle, `sync` keeps `wins` mirroring the screen.
        let empty = self.wins.is_empty();
        if empty {
            log!("no windows to show");
        }
        empty
    }

    /// Camera and zoom that show the canvas rect `b` with a margin.
    fn fit(&self, b: (f64, f64, f64, f64)) -> (f64, f64, f64) {
        fit_into(b, (FIT_MARGIN, FIT_MARGIN, self.sw - FIT_MARGIN, self.sh - FIT_MARGIN))
    }

    /// Like `fit`, but below the search panel while searching.
    fn frame(&self, b: (f64, f64, f64, f64)) -> (f64, f64, f64) {
        match self.search {
            Some(_) => fit_into(b, self.search_area()),
            None => self.fit(b),
        }
    }

    /// Zoom back to 1:1 where the overlay first came up.
    fn leave_overview(&mut self) {
        if !self.shown {
            return;
        }
        let (x, y) = self.origin;
        self.animate_to(x, y, 1.0, true, None);
    }

    /// Outside the overview (e.g. the background's minimap) this brings the
    /// overlay up and lands back on real windows at 1:1.
    fn fly_to(&mut self, x: f64, y: f64) {
        if !self.shown {
            self.begin();
        }
        if !self.shown {
            return;
        }
        let commit = !self.overview;
        let z = if commit { 1.0 } else { self.zoom };
        self.animate_to(x - self.sw / 2.0 / z, y - self.sh / 2.0 / z, z, commit, None);
    }

    /// Fly to a window and focus it; a minimized one offered by the search
    /// is restored instead.
    fn focus_window(&mut self, id: isize) {
        if !self.shown {
            self.begin();
        }
        if let Some(i) = self.index(id) {
            self.focus(i);
        } else if self.minimized.iter().any(|w| w.id == id) || self.away.iter().any(|w| w.id() == id) {
            self.search = None;
            self.minimized.clear();
            self.restore_minimized(HWND(id as *mut _));
        }
    }

    /// Lets go of a minimized or hidden window's spot: its placeholder goes,
    /// and it comes back in the middle of the view instead.
    fn forget_away(&mut self, id: isize) {
        self.away.retain(|w| w.id() != id);
        self.prune_away();
        self.update_view();
    }

    fn set_paused(&mut self, paused: bool) {
        if paused == self.paused {
            return;
        }
        self.paused = paused;
        self.gesture = Gesture::default();
        keys::set_switcher_enabled(self.cfg().alt_tab && !paused);
        mouse::configure(&self.cfg(), !paused);
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

    fn show_bg(&self) {
        let cmd = if self.paused || self.locked { SW_HIDE } else { SW_SHOWNOACTIVATE };
        unsafe {
            let _ = ShowWindowAsync(self.bg, cmd);
        }
    }

    /// Lay every window out in neat rows, keeping their reading order. Each
    /// one glides to its new spot while the camera frames the result.
    fn tidy(&mut self) {
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

    // ---- shortcuts -------------------------------------------------------------

    /// Registers the shortcuts from settings if they changed, and turns the
    /// Alt+Tab switcher on or off.
    pub fn apply_keys(&mut self) {
        let cfg = self.cfg();
        keys::set_switcher_enabled(cfg.alt_tab && !self.paused && !self.quitting);
        mouse::configure(&cfg, !self.paused && !self.quitting);
        let bound = [&cfg.key_overview, &cfg.key_find, &cfg.key_tidy, &cfg.key_quit].map(|k| k.as_str()).join("\n");
        let want = (bound, self.keys_suspended);
        if self.keys_applied.as_ref() == Some(&want) {
            return;
        }
        let taken = keys::register_hotkeys(self.hwnd, &cfg, self.keys_suspended);
        *self.shared.hotkeys_taken.write().unwrap_or_else(|e| e.into_inner()) = taken;
        self.keys_applied = Some(want);
    }

    // ---- animation and hiding ---------------------------------------------------

    /// Once per composed frame while the ticker runs: steps the animation,
    /// follows a resize, applies input queued since the last frame, then
    /// stops the ticker once nothing needs frames.
    fn tick(&mut self) {
        let started = self.trace.begin();
        self.step();
        if self.pacer.pending() {
            self.update_view();
        }
        let busy = self.anim.is_some() || self.resizing.is_some();
        let holding = (self.gesture.engaged && self.fingers_down()) || self.drag.is_some();
        if !self.pacer.keep_going(busy, holding, Instant::now()) {
            self.ticker.stop();
            self.trace.stopped();
            return;
        }
        self.trace.end(started, &self.shared);
    }

    fn step(&mut self) {
        // Fingers on the pad own the camera; nothing may fight them.
        if self.gesture.engaged {
            self.cancel_anim();
            return;
        }
        let down = self.fingers_down();
        if self.resizing.is_some() {
            self.follow_resize();
        }
        let Some(a) = self.anim.as_ref() else { return };
        if a.held && down {
            return;
        }
        if a.warmup > 0 {
            if let Some(a) = self.anim.as_mut() {
                a.warmup -= 1;
                a.start = Instant::now();
            }
            return;
        }
        let t = (a.start.elapsed().as_secs_f64() / ANIM_SECS).min(1.0);
        let e = 1.0 - (1.0 - t).powi(3);
        let lerp = |a: f64, b: f64| a + (b - a) * e;
        self.cam_x = lerp(a.from.0, a.to.0);
        self.cam_y = lerp(a.from.1, a.to.1);
        // Interpolate zoom in log space so it feels even across scales.
        self.zoom = (a.from.2.ln() + (a.to.2.ln() - a.from.2.ln()) * e).exp();
        for &(id, from, to) in &a.moves {
            if let Some(w) = self.wins.iter_mut().find(|w| w.id() == id) {
                w.x = lerp(from.0, to.0);
                w.y = lerp(from.1, to.1);
            }
        }
        self.update_view();
        if t >= 1.0 {
            if down && self.anim.as_ref().is_some_and(|a| a.commit) {
                // Resting fingers may still become a gesture: don't move the
                // real windows and drop the overlay from under them. Wait at
                // the end until they lift or take over.
                if let Some(a) = self.anim.as_mut() {
                    a.held = true;
                }
                return;
            }
            if let Some(a) = self.anim.take() {
                if a.commit {
                    self.commit(a.focus);
                } else if std::mem::take(&mut self.launch_tidy) {
                    self.leave_overview();
                }
            }
        }
    }

    /// While moving: windows that closed leave the canvas, and windows that
    /// opened join it in the middle of the view, on top. In the overview a
    /// new window is flown to, as if picked.
    fn adopt(&mut self) {
        let alive = |w: &Win| unsafe { IsWindow(Some(w.hwnd)).as_bool() && IsWindowVisible(w.hwnd).as_bool() };
        let (keep, gone): (Vec<Win>, Vec<Win>) = std::mem::take(&mut self.wins).into_iter().partition(alive);
        self.wins = keep;
        let mut changed = !gone.is_empty();
        self.stash(gone);
        let mut opened = false;
        for h in wins::app_windows(false) {
            if self.wins.iter().any(|w| w.hwnd == h) {
                continue;
            }
            let Some(r) = wins::window_rect(h) else { continue };
            let (w, hgt) = ((r.right - r.left) as f64, (r.bottom - r.top) as f64);
            // Back from minimized or hidden: its old spot. Otherwise the
            // middle of the view.
            let (x, y) = match self.away.iter().position(|o| o.hwnd == h) {
                Some(k) => {
                    let o = self.away.remove(k);
                    (o.x, o.y)
                }
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
            self.register(0);
            (changed, opened) = (true, true);
        }
        if !changed {
            return;
        }
        self.prune_away();
        self.selected.retain(|id| self.wins.iter().any(|w| w.id() == *id));
        self.update_view();
        if opened && self.overview && self.drag.is_none() && !self.gesture.engaged && self.search.is_none() {
            self.focus(0);
        }
    }

    /// Puts the camera back where the motion started and hands back to the
    /// real windows at once (the desktop is being switched under it).
    fn abort_motion(&mut self) {
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

    fn enter_overview(&mut self) {
        self.overview = true;
        self.register_keys();
        self.dirty = true;
    }

    /// Esc and Delete belong to Apeiron while the overview or a menu is up.
    fn register_keys(&mut self) {
        if !self.esc_registered {
            self.esc_registered =
                unsafe { RegisterHotKey(Some(self.hwnd), HOTKEY_ESC, MOD_NOREPEAT, VK_ESCAPE.0 as u32) }.is_ok();
        }
        self.sync_delete_key();
    }

    /// Delete goes with Esc, except while hovering hands the keyboard to
    /// windows: then only a selection claims it, so a Delete typed into a
    /// window reaches it instead of closing it.
    fn sync_delete_key(&mut self) {
        let want = self.esc_registered && (!self.selected.is_empty() || !self.cfg().hover_focus);
        if want == self.delete_registered {
            return;
        }
        unsafe {
            if want {
                self.delete_registered =
                    RegisterHotKey(Some(self.hwnd), HOTKEY_DELETE, MOD_NOREPEAT, VK_DELETE.0 as u32).is_ok();
            } else {
                let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_DELETE);
                self.delete_registered = false;
            }
        }
    }

    fn unregister_esc(&mut self) {
        if self.esc_registered {
            unsafe {
                let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_ESC);
            }
            self.esc_registered = false;
        }
        self.sync_delete_key();
    }

    /// Refresh the window list from reality. Only valid at zoom 1 with the
    /// overlay hidden, when real windows mirror the canvas.
    ///
    /// `live` is the gesture start: maximized windows are turned into normal
    /// ones and new windows get a spot (saved or free). Otherwise (idle, for
    /// the background's minimap) the list just mirrors the screen and new
    /// windows are left for the next live sync to place.
    fn sync(&mut self, live: bool) {
        // The list is rebuilt; the last overlay's thumbnails can't wait.
        self.release_thumbnails();
        let cfg = self.cfg();
        let startup = live && !self.restored;
        let mut next: Vec<Win> = Vec::new();
        let mut kinds = Vec::new();
        let mut returned: Vec<isize> = Vec::new();
        for h in wins::app_windows(true) {
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
    fn commit(&mut self, focus: Option<HWND>) {
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
    fn quit(&mut self) {
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
    fn save_layout(&mut self) {
        if !self.cfg().remember_layout || self.layout_frozen {
            return;
        }
        let entries: Vec<(WinKey, f64, f64)> = self.wins.iter().chain(&self.away).map(|w| (w.key(), w.x, w.y)).collect();
        self.layout.capture((self.cam_x, self.cam_y), &entries);
        self.layout.save();
    }

    /// Applies the saved layout at startup: the camera goes back to the saved
    /// one and windows take their saved canvas positions (via `sync`).
    /// Windows with no saved spot stay where they are on screen.
    fn restore_layout(&mut self) {
        if !self.cfg().remember_layout {
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

    fn focus(&mut self, i: usize) {
        let w = &self.wins[i];
        let (tx, ty) = self.centre_on((w.x, w.y, w.w, w.h));
        let h = w.hwnd;
        self.animate_to(tx, ty, 1.0, true, Some(h));
    }

    /// Camera at zoom 1 that centres the canvas rect `(x, y, w, h)`, or pins
    /// its top-left if it's bigger than the screen.
    fn centre_on(&self, (x, y, w, h): (f64, f64, f64, f64)) -> (f64, f64) {
        let tx = if w < self.sw { x + w / 2.0 - self.sw / 2.0 } else { x };
        let ty = if h < self.sh { y + h / 2.0 - self.sh / 2.0 } else { y };
        (tx, ty)
    }

    fn animate_to(&mut self, x: f64, y: f64, z: f64, commit: bool, focus: Option<HWND>) {
        self.cancel_anim();
        // New motion: the overlay must stay up for it.
        self.cancel_hide();
        if commit {
            // Where we'll rest at 1:1, so a later Esc comes back here rather
            // than undoing this move.
            self.origin = (x, y);
        }
        self.anim = Some(Anim {
            from: (self.cam_x, self.cam_y, self.zoom),
            to: (x, y, z),
            start: Instant::now(),
            commit,
            focus,
            moves: Vec::new(),
            held: false,
            warmup: if self.fresh { WARMUP_FRAMES } else { 0 },
        });
        self.fresh = false;
        self.ticker.start();
    }

    /// Stops the animation where it is, except that windows on their way
    /// somewhere (tidy) land there, so an interrupted tidy is still tidy.
    fn cancel_anim(&mut self) {
        let Some(a) = self.anim.take() else { return };
        self.launch_tidy = false;
        for (id, _, to) in a.moves {
            if let Some(w) = self.wins.iter_mut().find(|w| w.id() == id) {
                (w.x, w.y) = to;
            }
        }
    }

    fn pan(&mut self, dx: f64, dy: f64) {
        self.cam_x -= dx / self.zoom;
        self.cam_y -= dy / self.zoom;
    }

    fn zoom_about(&mut self, factor: f64, p: (f64, f64)) {
        let z = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let (wx, wy) = (self.cam_x + p.0 / self.zoom, self.cam_y + p.1 / self.zoom);
        self.zoom = z;
        self.cam_x = wx - p.0 / z;
        self.cam_y = wy - p.1 / z;
    }

    fn cursor_on_canvas(&self) -> bool {
        let (x, y) = cursor();
        let h = unsafe { WindowFromPoint(POINT { x: x as i32, y: y as i32 }) };
        h == self.bg || h == self.overlay
    }

    fn index(&self, id: isize) -> Option<usize> {
        self.wins.iter().position(|w| w.id() == id)
    }

    fn hit(&self, p: (f64, f64)) -> Option<usize> {
        // wins is topmost-first, so the first hit is the visible one.
        self.wins.iter().position(|w| {
            let r = vis_screen_rect(w, (self.cam_x, self.cam_y, self.zoom));
            p.0 >= r.left as f64 && p.0 < r.right as f64 && p.1 >= r.top as f64 && p.1 < r.bottom as f64
        })
    }

    fn bounds(&self) -> Option<(f64, f64, f64, f64)> {
        let first = self.wins.first()?;
        let mut b = (first.x, first.y, first.x + first.w, first.y + first.h);
        for w in &self.wins {
            b = (b.0.min(w.x), b.1.min(w.y), b.2.max(w.x + w.w), b.3.max(w.y + w.h));
        }
        Some(b)
    }

    /// Physical pixels per logical pixel on the overlay, which is what the
    /// UI draws with (the primary monitor's real scale, not the system DPI
    /// fixed at sign-in).
    fn scale(&self) -> f64 {
        let dpi = match unsafe { GetDpiForWindow(self.overlay) } {
            0 => unsafe { GetDpiForSystem() },
            dpi => dpi,
        };
        dpi as f64 / 96.0
    }

    fn stop_timer(&self, id: usize) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), id);
        }
    }

    // ---- publishing ------------------------------------------------------------

    fn infos(&self) -> Vec<WinInfo> {
        self.wins
            .iter()
            .map(|w| {
                let (x, y, vw, vh) = w.vis();
                WinInfo { id: w.id(), title: w.title.clone(), exe: w.exe.clone(), x, y, w: vw, h: vh,
                focused: self.focused == Some(w.id()) }
            })
            .collect()
    }

    /// The window list to publish: the last one again unless a window, its
    /// spot, the focus or a title changed, so frames don't copy every title.
    fn shared_infos(&mut self) -> Arc<Vec<WinInfo>> {
        let focused = self.focused;
        let same = self.win_infos.len() == self.wins.len()
            && self.win_infos.iter().zip(&self.wins).all(|(i, w)| {
                let (x, y, vw, vh) = w.vis();
                i.id == w.id()
                    && (i.x, i.y, i.w, i.h) == (x, y, vw, vh)
                    && i.focused == (focused == Some(i.id))
                    && i.title == w.title
                    && i.exe == w.exe
            });
        if !same {
            self.win_infos = Arc::new(self.infos());
        }
        self.win_infos.clone()
    }

    fn mode(&self) -> Mode {
        if self.search.is_some() {
            Mode::Search
        } else if self.overview {
            Mode::Overview
        } else if self.shown {
            Mode::Moving
        } else {
            Mode::Idle
        }
    }

    fn publish(&mut self) {
        self.dirty = false;
        self.sync_delete_key();
        let (cam_x, cam_y, zoom) = (self.cam_x, self.cam_y, self.zoom);
        let on_screen = |(x0, y0, x1, y1): snap::Guide| {
            ((x0 - cam_x) * zoom, (y0 - cam_y) * zoom, (x1 - cam_x) * zoom, (y1 - cam_y) * zoom)
        };
        let guides = self.drag.as_ref().map_or(Vec::new(), |d| d.guides.iter().copied().map(on_screen).collect());
        let windows = self.shared_infos();
        self.shared.publish(Snapshot {
            cam_x: self.cam_x,
            cam_y: self.cam_y,
            zoom: self.zoom,
            screen_w: self.sw,
            screen_h: self.sh,
            mode: self.mode(),
            overlay_visible: self.shown,
            windows,
            minimized: if self.search.is_some() { self.minimized.clone() } else { Vec::new() },
            away: self.away_infos.clone(),
            sticks: self.sticks.clone(),
            hovered: self.hovered,
            search: self.search.clone(),
            paused: self.paused,
            settled: self.hiding,
            selected: self.selected.clone(),
            marquee: self.drag.as_ref().and_then(|d| d.marquee.as_ref()).map(Marquee::rect),
            menu: self.menu.as_ref().map(|(_, m)| m.clone()),
            resize_edge: self.drag.as_ref().map_or(self.hover_edge, |d| d.resize),
            resize_preview: self.resize_preview(),
            guides,
            generation: 0,
        });
    }

    /// Publishes the snapshot if anything changed since the last one. Input
    /// waiting for the next frame publishes with it instead.
    pub fn flush(&mut self) {
        if self.dirty && !self.pacer.pending() {
            self.publish();
        }
    }
}

/// Camera `(x, y, zoom)` that shows the canvas rect `(x0, y0, x1, y1)`
/// centred in the screen area `(l, t, r, b)`, never closer than 0.85.
fn fit_into((x0, y0, x1, y1): (f64, f64, f64, f64), (l, t, r, b): (f64, f64, f64, f64)) -> (f64, f64, f64) {
    let z = ((r - l) / (x1 - x0)).min((b - t) / (y1 - y0)).clamp(MIN_ZOOM, 0.85);
    ((x0 + x1) / 2.0 - (l + r) / 2.0 / z, (y0 + y1) / 2.0 - (t + b) / 2.0 / z, z)
}

/// Overlap beyond a few pixels of shadow/border.
fn overlaps(a: (f64, f64, f64, f64), b: &Win) -> bool {
    const SLACK: f64 = 8.0;
    a.0 < b.x + b.w - SLACK && b.x < a.0 + a.2 - SLACK && a.1 < b.y + b.h - SLACK && b.y < a.1 + a.3 - SLACK
}

/// The primary monitor's size in physical pixels.
fn screen_size() -> (f64, f64) {
    unsafe { (GetSystemMetrics(SM_CXSCREEN) as f64, GetSystemMetrics(SM_CYSCREEN) as f64) }
}

fn cursor() -> (f64, f64) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    (p.x as f64, p.y as f64)
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
