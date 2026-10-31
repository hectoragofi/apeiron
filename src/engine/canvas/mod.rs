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

mod camera;
mod desktop;
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
use super::{keys, mouse, snap, wins, HOTKEY_DELETE, HOTKEY_ESC};
use crate::layout::{Layout, WinKey};
use crate::settings::Settings;
use crate::shared::{Command, MenuSnap, Mode, SearchState, Shared, Snapshot, WinInfo};

use camera::Anim;
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
