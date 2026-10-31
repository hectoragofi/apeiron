//! Types shared by the engine thread and the GPUI thread.
//!
//! The engine owns the canvas and publishes a [`Snapshot`] after every state
//! change; views read it while rendering. Input travels the other way as
//! [`Command`]s. Coordinates in the snapshot are physical pixels of the
//! primary monitor (windows in canvas space, everything else in screen space).

// Feature modules read fields and send commands that nothing in the core
// does yet.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::engine::WM_APP_CMD;
use crate::settings::Settings;

#[derive(Clone, Debug)]
pub struct WinInfo {
    /// The HWND.
    pub id: isize,
    pub title: String,
    /// Lowercase executable file name, e.g. `chrome.exe`.
    pub exe: String,
    /// Canvas coordinates.
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub focused: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Overlay hidden; real windows are on screen.
    #[default]
    Idle,
    /// Overlay up during a gesture or animation, not browsing.
    Moving,
    Overview,
    Search,
}

#[derive(Clone, Debug, Default)]
pub struct SearchState {
    pub query: String,
    /// Matching window ids, best match first.
    pub results: Vec<isize>,
    /// Index into `results`.
    pub selected: usize,
    /// Opened by Alt+Tab: no query, letting go of Alt picks.
    pub switcher: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub cam_x: f64,
    pub cam_y: f64,
    pub zoom: f64,
    pub screen_w: f64,
    pub screen_h: f64,
    pub mode: Mode,
    pub overlay_visible: bool,
    /// Topmost first.
    pub windows: Vec<WinInfo>,
    /// Minimized windows, which aren't on the canvas; only filled while
    /// searching (their position fields are zero).
    pub minimized: Vec<WinInfo>,
    /// Minimized and hidden windows that keep their spot on the canvas
    /// (drawn dashed in the minimap), on this virtual desktop.
    pub away: Vec<WinInfo>,
    /// Windows stuck together: each group moves as one.
    pub sticks: Vec<Vec<isize>>,
    pub hovered: Option<isize>,
    pub search: Option<SearchState>,
    pub paused: bool,
    /// Motion has ended at zoom 1 with real windows placed, and the overlay
    /// is about to hide: the background has to show this camera already.
    pub settled: bool,
    /// Windows picked out in the overview.
    pub selected: Vec<isize>,
    /// Selection box being drawn, screen pixels (left, top, right, bottom).
    pub marquee: Option<(f64, f64, f64, f64)>,
    /// What a dragged window is snapping to: lines in screen pixels
    /// `(x0, y0, x1, y1)`.
    pub guides: Vec<(f64, f64, f64, f64)>,
    /// The open right-click menu.
    pub menu: Option<MenuSnap>,
    /// Window edges under the cursor (or being dragged), as `EDGE_*` bits:
    /// the cursor shows which way it resizes.
    pub resize_edge: u8,
    /// The size being asked for while resizing a window: an outline on
    /// screen (left, top, right, bottom).
    pub resize_preview: Option<(f64, f64, f64, f64)>,
    /// Bumped on every publish.
    pub generation: u64,
}

pub const EDGE_L: u8 = 1;
pub const EDGE_T: u8 = 2;
pub const EDGE_R: u8 = 4;
pub const EDGE_B: u8 = 8;

/// A right-click menu, at screen point `x`,`y` (physical pixels).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MenuSnap {
    pub x: f64,
    pub y: f64,
    pub items: Vec<MenuItem>,
    /// New for every menu opened.
    pub id: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MenuItem {
    pub label: String,
    /// Shortcut shown on the right.
    pub hint: String,
    pub check: bool,
    /// Destroys something (closing windows): red when hovered.
    pub danger: bool,
    /// A divider line, not an item.
    pub separator: bool,
}

/// Input from the GPUI thread (positions are physical pixels).
#[derive(Clone, Debug)]
pub enum Command {
    /// The overlay and background HWNDs; the engine ignores everything else
    /// until it has them.
    Attach { overlay: isize, bg: isize },
    /// `additive`: Ctrl or Shift held (add to the selection).
    MouseDown { x: f64, y: f64, additive: bool },
    /// Ask the window to close (as its own close button would).
    CloseWindow(isize),
    /// An item of the open menu was clicked (its index in `MenuSnap::items`).
    MenuPick(usize),
    /// Right click.
    ContextMenu { x: f64, y: f64 },
    MouseMove { x: f64, y: f64 },
    MouseUp,
    /// `dx`/`dy` are pan pixels (a mouse-wheel notch is 60); with `pinch`
    /// (Ctrl held) `dy` zooms about `x`,`y` instead. `mouse`: a notched
    /// mouse wheel, not a touchpad (it zooms; with `shift` it pans sideways).
    Wheel { dx: f64, dy: f64, pinch: bool, mouse: bool, shift: bool, x: f64, y: f64 },
    /// Middle button down: drag the canvas around from here, over windows too.
    PanStart { x: f64, y: f64 },
    ToggleOverview,
    LeaveOverview,
    Tidy,
    FocusWindow(isize),
    /// Drop a minimized or hidden window's saved spot (its placeholder).
    ForgetAway(isize),
    /// Centre the camera on a canvas point, keeping the zoom.
    FlyTo { x: f64, y: f64 },
    OpenSearch,
    SearchQuery(String),
    SearchMove(i32),
    SearchAccept,
    CloseSearch,
    SettingsChanged,
    SetPaused(bool),
    /// The settings window is recording a shortcut: let every key through.
    SuspendHotkeys(bool),
    ResetLayout,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiEvent {
    Redraw,
    FocusSearch,
    OpenSettings,
    /// The primary monitor's size or scale changed; refit the fullscreen windows.
    Screen,
    /// The update check moved on; the tray item shows where it's at.
    Update,
    Quit,
}

/// A queued `Redraw` that no frame has picked up after this long is sent
/// again, so a missed frame can never freeze the UI.
const REDRAW_RETRY: Duration = Duration::from_millis(250);

/// State both threads hold on to.
#[derive(Clone)]
pub struct Shared {
    /// Swapped whole on every publish, so neither side ever holds the lock
    /// for longer than an `Arc` clone.
    snapshot: Arc<RwLock<Arc<Snapshot>>>,
    pub settings: Arc<RwLock<Settings>>,
    /// Shortcuts Windows wouldn't register (another app holds them).
    pub hotkeys_taken: Arc<RwLock<Vec<String>>>,
    ui_tx: async_channel::Sender<UiEvent>,
    redraw_pending: Arc<AtomicBool>,
    /// When the pending `Redraw` was sent, in ms since [`epoch`].
    redraw_sent: Arc<AtomicU64>,
}

impl Shared {
    pub fn new(settings: Settings) -> (Shared, async_channel::Receiver<UiEvent>) {
        let (ui_tx, ui_rx) = async_channel::unbounded();
        let shared = Shared {
            snapshot: Arc::new(RwLock::new(Arc::new(Snapshot { zoom: 1.0, ..Default::default() }))),
            settings: Arc::new(RwLock::new(settings)),
            hotkeys_taken: Arc::new(RwLock::new(Vec::new())),
            ui_tx,
            redraw_pending: Arc::new(AtomicBool::new(false)),
            redraw_sent: Arc::new(AtomicU64::new(0)),
        };
        (shared, ui_rx)
    }

    pub fn snap(&self) -> Arc<Snapshot> {
        self.snapshot.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replaces the snapshot (bumping its generation) and asks for a redraw.
    pub fn publish(&self, next: Snapshot) {
        {
            let mut snap = self.snapshot.write().unwrap_or_else(|e| e.into_inner());
            let generation = snap.generation + 1;
            *snap = Arc::new(Snapshot { generation, ..next });
        }
        self.redraw();
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn emit(&self, event: UiEvent) {
        let _ = self.ui_tx.try_send(event);
    }

    /// Asks the UI to repaint, keeping at most one `Redraw` in flight until
    /// a frame has been drawn.
    pub fn redraw(&self) {
        let now = epoch().elapsed().as_millis() as u64;
        let stale = now.saturating_sub(self.redraw_sent.load(Ordering::Acquire)) > REDRAW_RETRY.as_millis() as u64;
        if !self.redraw_pending.swap(true, Ordering::AcqRel) || stale {
            self.redraw_sent.store(now, Ordering::Release);
            self.emit(UiEvent::Redraw);
        }
    }

    /// The UI calls this once it has drawn a frame from the snapshot (or
    /// knows none will be drawn), so the next change sends a new `Redraw`.
    pub fn redraw_taken(&self) {
        self.redraw_pending.store(false, Ordering::Release);
    }
}

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// How the UI talks to the engine.
#[derive(Clone)]
pub struct EngineHandle {
    tx: Sender<Command>,
    /// The engine's window, woken after every send.
    hwnd: isize,
}

impl EngineHandle {
    pub fn new(tx: Sender<Command>, hwnd: isize) -> Self {
        EngineHandle { tx, hwnd }
    }

    pub fn hwnd(&self) -> isize {
        self.hwnd
    }

    pub fn send(&self, cmd: Command) {
        if self.tx.send(cmd).is_ok() {
            unsafe {
                let _ = PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_APP_CMD, WPARAM(0), LPARAM(0));
            }
        }
    }
}
