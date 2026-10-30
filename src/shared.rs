//! Types shared by the engine thread and the GPUI thread.
//!
//! The engine owns the canvas and publishes a [`Snapshot`] after every state
//! change; views read it while rendering. Input travels the other way as
//! [`Command`]s. Coordinates in the snapshot are physical pixels of the
//! primary monitor (windows in canvas space, everything else in screen space).

// Feature modules read fields and send commands that nothing in the core
// does yet.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock, RwLockReadGuard};

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
    pub hovered: Option<isize>,
    pub search: Option<SearchState>,
    pub paused: bool,
    /// Bumped on every publish.
    pub generation: u64,
}

/// Input from the GPUI thread (positions are physical pixels).
#[derive(Clone, Debug)]
pub enum Command {
    /// The overlay and background HWNDs; the engine ignores everything else
    /// until it has them.
    Attach { overlay: isize, bg: isize },
    MouseDown { x: f64, y: f64 },
    MouseMove { x: f64, y: f64 },
    MouseUp,
    /// `dx`/`dy` are pan pixels (a mouse-wheel notch is 60); with `pinch`
    /// (Ctrl held) `dy` zooms about `x`,`y` instead.
    Wheel { dx: f64, dy: f64, pinch: bool, x: f64, y: f64 },
    ToggleOverview,
    LeaveOverview,
    Tidy,
    FocusWindow(isize),
    /// Centre the camera on a canvas point, keeping the zoom.
    FlyTo { x: f64, y: f64 },
    OpenSearch,
    SearchQuery(String),
    SearchMove(i32),
    SearchAccept,
    CloseSearch,
    SettingsChanged,
    SetPaused(bool),
    ResetLayout,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiEvent {
    Redraw,
    FocusSearch,
    OpenSettings,
    Quit,
}

/// State both threads hold on to.
#[derive(Clone)]
pub struct Shared {
    pub snapshot: Arc<RwLock<Snapshot>>,
    pub settings: Arc<RwLock<Settings>>,
    ui_tx: async_channel::Sender<UiEvent>,
    redraw_pending: Arc<AtomicBool>,
}

impl Shared {
    pub fn new(settings: Settings) -> (Shared, async_channel::Receiver<UiEvent>) {
        let (ui_tx, ui_rx) = async_channel::unbounded();
        let shared = Shared {
            snapshot: Arc::new(RwLock::new(Snapshot { zoom: 1.0, ..Default::default() })),
            settings: Arc::new(RwLock::new(settings)),
            ui_tx,
            redraw_pending: Arc::new(AtomicBool::new(false)),
        };
        (shared, ui_rx)
    }

    pub fn snap(&self) -> RwLockReadGuard<'_, Snapshot> {
        self.snapshot.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn emit(&self, event: UiEvent) {
        let _ = self.ui_tx.try_send(event);
    }

    /// Asks the UI to repaint, keeping at most one `Redraw` queued.
    pub fn redraw(&self) {
        if !self.redraw_pending.swap(true, Ordering::AcqRel) {
            self.emit(UiEvent::Redraw);
        }
    }

    /// The UI calls this when it takes a `Redraw` off the queue, before it
    /// reads the snapshot.
    pub fn redraw_taken(&self) {
        self.redraw_pending.store(false, Ordering::Release);
    }
}

/// How the UI talks to the engine.
#[derive(Clone)]
pub struct EngineHandle {
    tx: Sender<Command>,
    /// The engine's message-only window, woken after every send.
    hwnd: isize,
}

impl EngineHandle {
    pub fn new(tx: Sender<Command>, hwnd: isize) -> Self {
        EngineHandle { tx, hwnd }
    }

    pub fn send(&self, cmd: Command) {
        if self.tx.send(cmd).is_ok() {
            unsafe {
                let _ = PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_APP_CMD, WPARAM(0), LPARAM(0));
            }
        }
    }
}
