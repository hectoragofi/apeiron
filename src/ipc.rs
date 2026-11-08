//! IPC: a named pipe through which other apps (screenbuddy) read the canvas
//! and drive the camera. The wire protocol is documented in `docs/IPC.md`.
//!
//! Each client gets a reader thread (requests in) and a writer thread
//! (replies and events out) over one overlapped pipe handle, so neither
//! direction blocks the other, and nothing here ever runs on the UI or engine
//! threads. Reads come straight from the published [`Snapshot`]; anything
//! that moves the camera or windows goes to the engine as a
//! [`Command::Ipc`](crate::shared::Command::Ipc). Events are diffs between
//! successive snapshots, worked out on one thread at most 60 times a second.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::shared::{EngineHandle, Mode, Shared, Snapshot};

pub const PIPE_NAME: &str = r"\\.\pipe\apeiron-v1";
pub const VERSION: u32 = 1;
/// The error for canvas-only requests in tabs-only mode.
pub const CANVAS_OFF: &str = "the canvas is off (tabs-only mode)";
/// A request line longer than this closes the connection.
const MAX_LINE: usize = 64 * 1024;
/// Requests that reply once the camera has settled give up after this.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Events are worked out at most this often (60/s).
const EVENT_INTERVAL: Duration = Duration::from_micros(16_667);
/// Lines queued for a client that isn't reading; events past this are dropped.
const OUT_QUEUE: usize = 1024;

/// A canvas or screen rect `[x, y, w, h]`.
pub type Rect = [f64; 4];
/// What the engine sends back: the reply's `data`, or the error.
pub type Reply = Result<Value, String>;

/// The requests the engine itself has to carry out.
#[derive(Clone, Debug, PartialEq)]
pub enum EngineOp {
    FocusWindow(isize),
    LeaveStage,
    FlyTo(Rect),
    Back,
    ReserveRegion { name: String, w: f64, h: f64 },
    PlaceWindow { hwnd: isize, region: String },
}

impl EngineOp {
    /// Means nothing without the canvas.
    fn canvas_only(&self) -> bool {
        !matches!(self, EngineOp::FocusWindow(_))
    }
}

/// An engine request and where its reply goes. The reply may come much
/// later (once the camera has settled); dropping `reply` unanswered is an
/// error for the client.
#[derive(Clone, Debug)]
pub struct IpcCall {
    pub op: EngineOp,
    pub reply: Sender<Reply>,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Op {
    Hello,
    GetLayout,
    FocusWindow { hwnd: isize },
    LeaveStage,
    FlyTo { canvas_rect: Rect },
    Back,
    ReserveRegion { name: String, w: f64, h: f64 },
    PlaceWindow { hwnd: isize, region: String },
    Subscribe,
}

const OPS: [&str; 9] = [
    "hello",
    "get_layout",
    "focus_window",
    "leave_stage",
    "fly_to",
    "back",
    "reserve_region",
    "place_window",
    "subscribe",
];

/// Splits a request line into its `id` (echoed back as given, `null` if
/// missing) and the request.
fn parse(line: &str) -> (Value, Result<Op, String>) {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return (Value::Null, Err(format!("bad json: {e}"))),
    };
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    if !v.is_object() {
        return (id, Err("a request must be a JSON object".into()));
    }
    match v.get("op").and_then(Value::as_str) {
        None => return (id, Err("missing op".into())),
        Some(op) if !OPS.contains(&op) => return (id, Err(format!("unknown op: {op}"))),
        Some(_) => {}
    }
    let op = serde_json::from_value::<Op>(v)
        .map_err(|e| format!("bad request: {e}"))
        .and_then(validate);
    (id, op)
}

fn validate(op: Op) -> Result<Op, String> {
    let size_ok = |w: f64, h: f64| {
        w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 && w <= 1e6 && h <= 1e6
    };
    match &op {
        Op::FlyTo {
            canvas_rect: [x, y, w, h],
        } if !(x.is_finite() && y.is_finite() && size_ok(*w, *h)) => {
            Err("canvas_rect must be [x, y, w, h] with a positive size".into())
        }
        Op::ReserveRegion { w, h, .. } if !size_ok(*w, *h) => {
            Err("w and h must be positive".into())
        }
        Op::ReserveRegion { name, .. } if name.is_empty() => Err("name must not be empty".into()),
        _ => Ok(op),
    }
}

fn ok_line(id: &Value, data: Value) -> String {
    json!({ "id": id, "ok": true, "data": data }).to_string()
}

fn err_line(id: &Value, error: &str) -> String {
    json!({ "id": id, "ok": false, "error": error }).to_string()
}

// ---- what the server needs from the rest of Apeiron ------------------------

pub trait Backend: Send + Sync + 'static {
    fn snapshot(&self) -> Arc<Snapshot>;
    /// The infinite canvas is on (not tabs-only mode).
    fn canvas(&self) -> bool;
    fn call(&self, call: IpcCall);
    fn is_window(&self, hwnd: isize) -> bool;
    /// Some of this screen rect is on a monitor.
    fn on_screen(&self, r: Rect) -> bool;
}

struct Live {
    shared: Shared,
    engine: EngineHandle,
}

impl Backend for Live {
    fn snapshot(&self) -> Arc<Snapshot> {
        self.shared.snap()
    }

    fn canvas(&self) -> bool {
        self.shared
            .settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .canvas
    }

    fn call(&self, call: IpcCall) {
        self.engine.send(crate::shared::Command::Ipc(call));
    }

    fn is_window(&self, hwnd: isize) -> bool {
        use windows::Win32::Foundation::HWND;
        unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(HWND(hwnd as *mut _))) }
            .as_bool()
    }

    fn on_screen(&self, [x, y, w, h]: Rect) -> bool {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Gdi::{MonitorFromRect, MONITOR_DEFAULTTONULL};
        let r = RECT {
            left: x as i32,
            top: y as i32,
            right: (x + w) as i32,
            bottom: (y + h) as i32,
        };
        if r.right <= r.left || r.bottom <= r.top {
            return false;
        }
        !unsafe { MonitorFromRect(&r, MONITOR_DEFAULTTONULL) }.is_invalid()
    }
}

/// Starts serving `\\.\pipe\apeiron-v1` in the background.
pub fn start(shared: Shared, engine: EngineHandle) {
    let backend = Arc::new(Live {
        shared: shared.clone(),
        engine,
    });
    let hub = Hub::start(backend);
    let wake = hub.clone();
    shared.on_publish(move || wake.wake());
    let spawned = std::thread::Builder::new()
        .name("ipc".into())
        .spawn(move || pipe::listen(PIPE_NAME, hub));
    if let Err(e) = spawned {
        log!("ipc: couldn't start: {e}");
    }
}

// ---- the layout and events, from snapshots -----------------------------------

/// One window as clients see it.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    hwnd: isize,
    title: String,
    exe: String,
    /// The visible frame on the canvas.
    canvas_rect: Rect,
    /// Where that frame is on screen now; `None` for hidden tabs and
    /// minimized windows.
    screen_rect: Option<Rect>,
    tab_hidden: bool,
    away: bool,
}

/// Every window: on the canvas (topmost first), then hidden tabs (at their
/// group's spot), then minimized or hidden ones keeping their spot.
fn entries(s: &Snapshot) -> Vec<Entry> {
    let project = |[x, y, w, h]: Rect| {
        [
            ((x - s.cam_x) * s.zoom).round(),
            ((y - s.cam_y) * s.zoom).round(),
            (w * s.zoom).round(),
            (h * s.zoom).round(),
        ]
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for w in s.windows.iter() {
        if seen.insert(w.id) {
            let r = [w.x, w.y, w.w, w.h];
            out.push(Entry {
                hwnd: w.id,
                title: w.title.clone(),
                exe: w.exe.clone(),
                canvas_rect: r,
                screen_rect: Some(project(r)),
                tab_hidden: false,
                away: false,
            });
        }
    }
    for g in &s.tabs {
        let (x, y, w, h) = g.spot;
        for t in &g.tabs {
            if seen.insert(t.id) {
                out.push(Entry {
                    hwnd: t.id,
                    title: t.title.clone(),
                    exe: t.exe.clone(),
                    canvas_rect: [x, y, w, h],
                    screen_rect: None,
                    tab_hidden: true,
                    away: false,
                });
            }
        }
    }
    for w in &s.away {
        if seen.insert(w.id) {
            out.push(Entry {
                hwnd: w.id,
                title: w.title.clone(),
                exe: w.exe.clone(),
                canvas_rect: [w.x, w.y, w.w, w.h],
                screen_rect: None,
                tab_hidden: false,
                away: true,
            });
        }
    }
    out
}

fn stage(s: &Snapshot) -> bool {
    matches!(s.mode, Mode::Overview | Mode::Search)
}

fn layout_json(s: &Snapshot, on_screen: impl Fn(Rect) -> bool) -> Value {
    let windows: Vec<Value> = entries(s)
        .into_iter()
        .map(|e| {
            json!({
                "hwnd": e.hwnd,
                "title": e.title,
                "exe": e.exe,
                "canvas_rect": e.canvas_rect,
                "screen_rect": e.screen_rect,
                "on_screen": e.screen_rect.is_some_and(&on_screen),
                "tab_hidden": e.tab_hidden,
                "away": e.away,
            })
        })
        .collect();
    json!({
        "camera": { "x": s.cam_x, "y": s.cam_y, "zoom": s.zoom },
        "stage": stage(s),
        "moving": s.moving,
        "windows": windows,
    })
}

fn camera_event(s: &Snapshot) -> Value {
    json!({ "event": "camera", "x": s.cam_x, "y": s.cam_y, "zoom": s.zoom, "moving": s.moving })
}

/// What clients were last told.
#[derive(Default)]
struct Told {
    camera: (f64, f64, f64, bool),
    stage: bool,
    gesture: bool,
    focus: Option<isize>,
    /// Tab groups: every tab, and the one shown.
    groups: Vec<(Vec<isize>, isize)>,
    /// Canvas and screen rect of every window, as last sent.
    rects: HashMap<isize, (Rect, Option<Rect>)>,
}

impl Told {
    fn from(s: &Snapshot) -> Told {
        let mut t = Told::default();
        t.update(s, |_| false);
        t
    }

    /// Takes in a new snapshot and returns the events it makes.
    fn update(&mut self, s: &Snapshot, is_window: impl Fn(isize) -> bool) -> Vec<Value> {
        let mut out = Vec::new();
        let camera = (s.cam_x, s.cam_y, s.zoom, s.moving);
        if camera != self.camera {
            self.camera = camera;
            out.push(camera_event(s));
        }
        if stage(s) != self.stage {
            self.stage = stage(s);
            out.push(json!({ "event": "stage", "open": self.stage }));
        }
        if s.gesture != self.gesture {
            self.gesture = s.gesture;
            out.push(json!({ "event": "gesture", "active": s.gesture }));
        }

        let now = entries(s);
        let ids: HashSet<isize> = now.iter().map(|e| e.hwnd).collect();
        let gone: Vec<isize> = self
            .rects
            .keys()
            .copied()
            .filter(|id| !ids.contains(id))
            .collect();
        for id in gone {
            self.rects.remove(&id);
            if !is_window(id) {
                out.push(json!({ "event": "window_closed", "hwnd": id }));
            }
        }
        for e in &now {
            let last = self.rects.get(&e.hwnd);
            // While moving every screen rect changes each frame; those are
            // only sent once things come to rest.
            let changed = match last {
                None => true,
                Some((c, sr)) => *c != e.canvas_rect || (!s.moving && *sr != e.screen_rect),
            };
            if changed {
                self.rects.insert(e.hwnd, (e.canvas_rect, e.screen_rect));
                out.push(json!({
                    "event": "window_moved",
                    "hwnd": e.hwnd,
                    "canvas_rect": e.canvas_rect,
                    "screen_rect": e.screen_rect,
                }));
            }
        }

        let focus = s.windows.iter().find(|w| w.focused).map(|w| w.id);
        if focus.is_some() && focus != self.focus {
            out.push(json!({ "event": "focus", "hwnd": focus }));
        }
        self.focus = focus;

        let groups: Vec<(Vec<isize>, isize)> = s
            .tabs
            .iter()
            .map(|g| (g.tabs.iter().map(|t| t.id).collect(), g.active))
            .collect();
        for (tabs, active) in &groups {
            let before = self
                .groups
                .iter()
                .find(|(old, _)| old.iter().any(|id| tabs.contains(id)));
            if before.is_some_and(|(_, was)| was != active) {
                out.push(json!({ "event": "tab_switch", "group": tabs.first().copied().unwrap_or(*active), "hwnd": active }));
            }
        }
        self.groups = groups;
        out
    }
}

// ---- connections and subscribers ---------------------------------------------

struct Hub {
    backend: Arc<dyn Backend>,
    subs: Mutex<Vec<SyncSender<String>>>,
    told: Mutex<Option<Told>>,
    wake: SyncSender<()>,
}

impl Hub {
    fn start(backend: Arc<dyn Backend>) -> Arc<Hub> {
        let (wake, woken) = mpsc::sync_channel(1);
        let hub = Arc::new(Hub {
            backend,
            subs: Mutex::new(Vec::new()),
            told: Mutex::new(None),
            wake,
        });
        let weak = Arc::downgrade(&hub);
        let _ = std::thread::Builder::new()
            .name("ipc events".into())
            .spawn(move || events(weak, woken));
        hub
    }

    /// A snapshot was published.
    fn wake(&self) {
        let _ = self.wake.try_send(());
    }

    fn subscribe(&self, out: &SyncSender<String>) {
        let s = self.backend.snapshot();
        for e in [
            camera_event(&s),
            json!({ "event": "stage", "open": stage(&s) }),
            json!({ "event": "gesture", "active": s.gesture }),
        ] {
            let _ = out.try_send(e.to_string());
        }
        self.subs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(out.clone());
    }

    /// Works out what changed since the last snapshot and tells subscribers.
    fn diff(&self) {
        let s = self.backend.snapshot();
        let mut told = self.told.lock().unwrap_or_else(|e| e.into_inner());
        let events = match told.as_mut() {
            Some(t) => t.update(&s, |id| self.backend.is_window(id)),
            None => {
                *told = Some(Told::from(&s));
                Vec::new()
            }
        };
        drop(told);
        if events.is_empty() {
            return;
        }
        let mut subs = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        subs.retain(|tx| {
            for e in &events {
                match tx.try_send(e.to_string()) {
                    Ok(()) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => return false,
                }
            }
            true
        });
    }

    /// Handles one request line; replies go to `out` (now, or once the
    /// engine answers).
    fn handle(&self, line: &str, out: &SyncSender<String>) {
        let (id, op) = parse(line);
        let op = match op {
            Ok(op) => op,
            Err(e) => {
                let _ = out.send(err_line(&id, &e));
                return;
            }
        };
        let engine_op = match op {
            Op::Hello => {
                let _ = out.send(ok_line(
                    &id,
                    json!({ "version": VERSION, "canvas": self.backend.canvas() }),
                ));
                return;
            }
            Op::GetLayout => {
                let s = self.backend.snapshot();
                let _ = out.send(ok_line(&id, layout_json(&s, |r| self.backend.on_screen(r))));
                return;
            }
            Op::Subscribe => {
                let _ = out.send(ok_line(&id, json!({})));
                self.subscribe(out);
                return;
            }
            Op::FocusWindow { hwnd } => EngineOp::FocusWindow(hwnd),
            Op::LeaveStage => EngineOp::LeaveStage,
            Op::FlyTo { canvas_rect } => EngineOp::FlyTo(canvas_rect),
            Op::Back => EngineOp::Back,
            Op::ReserveRegion { name, w, h } => EngineOp::ReserveRegion { name, w, h },
            Op::PlaceWindow { hwnd, region } => EngineOp::PlaceWindow { hwnd, region },
        };
        if engine_op.canvas_only() && !self.backend.canvas() {
            let _ = out.send(err_line(&id, CANVAS_OFF));
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.backend.call(IpcCall {
            op: engine_op,
            reply: tx,
        });
        let out = out.clone();
        // Waiting here would hold up this client's next requests.
        let _ = std::thread::Builder::new()
            .name("ipc reply".into())
            .spawn(move || {
                let line = match rx.recv_timeout(SETTLE_TIMEOUT) {
                    Ok(Ok(data)) => ok_line(&id, data),
                    Ok(Err(e)) => err_line(&id, &e),
                    Err(RecvTimeoutError::Timeout) => {
                        err_line(&id, "timed out waiting for the canvas to settle")
                    }
                    Err(RecvTimeoutError::Disconnected) => err_line(
                        &id,
                        "the canvas didn't take the request (not ready, or quitting)",
                    ),
                };
                let _ = out.send(line);
            });
    }
}

/// The event thread: after a publish, diff the snapshot at most every
/// `EVENT_INTERVAL`, always catching up with the last one.
fn events(hub: std::sync::Weak<Hub>, woken: Receiver<()>) {
    let mut last = Instant::now() - EVENT_INTERVAL;
    let mut pending = false;
    loop {
        let got = if pending {
            woken.recv_timeout((last + EVENT_INTERVAL).saturating_duration_since(Instant::now()))
        } else {
            woken.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        if got == Err(RecvTimeoutError::Disconnected) {
            return;
        }
        pending = true;
        if last.elapsed() < EVENT_INTERVAL {
            continue;
        }
        let Some(hub) = hub.upgrade() else { return };
        pending = false;
        last = Instant::now();
        hub.diff();
    }
}

/// Splits incoming bytes into lines.
#[derive(Default)]
struct Lines {
    buf: Vec<u8>,
}

impl Lines {
    /// Adds `data`; returns the complete lines (blank ones dropped), or
    /// `None` once a line is too long.
    fn push(&mut self, data: &[u8]) -> Option<Vec<String>> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            let text = String::from_utf8_lossy(&line);
            let text = text.trim();
            if !text.is_empty() {
                out.push(text.to_string());
            }
        }
        (self.buf.len() <= MAX_LINE).then_some(out)
    }
}

/// Serves one connected client until it hangs up.
fn serve(
    hub: Arc<Hub>,
    read: impl FnMut(&mut [u8]) -> Option<usize>,
    write: impl FnMut(&[u8]) -> bool + Send + 'static,
) {
    let (out, lines_out) = mpsc::sync_channel::<String>(OUT_QUEUE);
    let writer = std::thread::Builder::new()
        .name("ipc writer".into())
        .spawn(move || {
            let mut write = write;
            for mut line in lines_out {
                line.push('\n');
                if !write(line.as_bytes()) {
                    return;
                }
            }
        });
    if writer.is_err() {
        return;
    }
    let mut read = read;
    let mut lines = Lines::default();
    let mut buf = vec![0u8; 8192];
    while let Some(n) = read(&mut buf).filter(|&n| n > 0) {
        let Some(got) = lines.push(&buf[..n]) else {
            let _ = out.send(err_line(&Value::Null, "request line too long"));
            break;
        };
        for line in got {
            hub.handle(&line, &out);
        }
    }
    // The writer ends once every sender (subscriptions, pending replies) is
    // gone; subscriptions go at the next event.
}

// ---- the pipe itself -----------------------------------------------------------

mod pipe {
    use std::sync::Arc;
    use std::time::Duration;

    use windows::core::{HSTRING, PCWSTR, PWSTR};
    use windows::Win32::Foundation::*;
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows::Win32::Storage::FileSystem::{
        ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_FIRST_PIPE_INSTANCE,
        FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::*;
    use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcess, OpenProcessToken};
    use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

    use super::Hub;

    /// A server end of the pipe.
    pub(super) struct Pipe(HANDLE);

    // The handle is only used through overlapped calls, each with its own
    // OVERLAPPED, which Windows allows from any thread at once.
    unsafe impl Send for Pipe {}
    unsafe impl Sync for Pipe {}

    impl Drop for Pipe {
        fn drop(&mut self) {
            unsafe {
                let _ = DisconnectNamedPipe(self.0);
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// Runs `start` (an overlapped call) and waits for it to finish;
    /// returns the bytes moved, or the error code.
    fn overlapped(
        h: HANDLE,
        start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> Result<u32, WIN32_ERROR> {
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|_| ERROR_OUTOFMEMORY)?;
        let mut ov = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        let started = start(&mut ov);
        let result = match started {
            Err(e) if e.code() != ERROR_IO_PENDING.to_hresult() => {
                Err(WIN32_ERROR::from_error(&e).unwrap_or(ERROR_GEN_FAILURE))
            }
            _ => {
                let mut n = 0u32;
                match unsafe { GetOverlappedResult(h, &ov, &mut n, true) } {
                    Ok(()) => Ok(n),
                    Err(e) => Err(WIN32_ERROR::from_error(&e).unwrap_or(ERROR_GEN_FAILURE)),
                }
            }
        };
        unsafe {
            let _ = CloseHandle(event);
        }
        result
    }

    impl Pipe {
        fn create(
            name: &HSTRING,
            first: bool,
            sa: &SECURITY_ATTRIBUTES,
        ) -> Result<Pipe, WIN32_ERROR> {
            let mut open = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
            if first {
                // Fails if anyone else already serves this name.
                open |= FILE_FLAG_FIRST_PIPE_INSTANCE;
            }
            let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
            let h = unsafe {
                CreateNamedPipeW(
                    name,
                    FILE_FLAGS_AND_ATTRIBUTES(open.0),
                    mode,
                    PIPE_UNLIMITED_INSTANCES,
                    64 * 1024,
                    64 * 1024,
                    0,
                    Some(sa),
                )
            };
            if h.is_invalid() {
                return Err(unsafe { GetLastError() });
            }
            Ok(Pipe(h))
        }

        fn connect(&self) -> Result<(), WIN32_ERROR> {
            match overlapped(self.0, |ov| unsafe { ConnectNamedPipe(self.0, Some(ov)) }) {
                // The client got in between creating and connecting.
                Ok(_) | Err(ERROR_PIPE_CONNECTED) => Ok(()),
                Err(e) => Err(e),
            }
        }

        /// `None` once the client is gone.
        pub(super) fn read(&self, buf: &mut [u8]) -> Option<usize> {
            overlapped(self.0, |ov| unsafe {
                ReadFile(self.0, Some(buf), None, Some(ov))
            })
            .ok()
            .map(|n| n as usize)
        }

        pub(super) fn write(&self, mut data: &[u8]) -> bool {
            while !data.is_empty() {
                match overlapped(self.0, |ov| unsafe {
                    WriteFile(self.0, Some(data), None, Some(ov))
                }) {
                    Ok(n) if n > 0 => data = &data[n as usize..],
                    _ => return false,
                }
            }
            true
        }
    }

    /// Access for this user (and SYSTEM) only. The default security on a
    /// named pipe lets everyone open it for reading.
    fn security() -> Option<SECURITY_ATTRIBUTES> {
        let sid = user_sid();
        if sid.is_none() {
            log!("ipc: couldn't read the user's SID; the pipe is limited to its owner");
        }
        let who = sid.unwrap_or_else(|| "OW".into());
        let sddl = HSTRING::from(format!("D:P(A;;GA;;;{who})(A;;GA;;;SY)"));
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // Lives as long as the process (one for the whole server).
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &sddl,
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .ok()?;
        Some(SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        })
    }

    /// The current user's SID, as a string (`S-1-5-21-...`).
    pub(super) fn user_sid() -> Option<String> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
            let mut len = 0u32;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
            // u64s keep TOKEN_USER aligned.
            let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
            let got = GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr().cast()),
                len,
                &mut len,
            );
            let _ = CloseHandle(token);
            got.ok()?;
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut s = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut s).ok()?;
            let out = s.to_string().ok();
            let _ = LocalFree(Some(HLOCAL(s.0.cast())));
            out
        }
    }

    /// Accepts clients on `name` forever, one thread pair each.
    pub(super) fn listen(name: &str, hub: Arc<Hub>) {
        let Some(sa) = security() else {
            log!("ipc: couldn't build the pipe's security; not serving");
            return;
        };
        let name = HSTRING::from(name);
        let mut first = true;
        let mut complained = false;
        loop {
            let pipe = match Pipe::create(&name, first, &sa) {
                Ok(p) => p,
                Err(e) => {
                    if !complained {
                        log!("ipc: can't serve {name} ({e:?}); retrying");
                        complained = true;
                    }
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };
            if first {
                log!("ipc: serving {name}");
            }
            (first, complained) = (false, false);
            if pipe.connect().is_err() {
                continue;
            }
            let pipe = Arc::new(pipe);
            let hub = hub.clone();
            let _ = std::thread::Builder::new()
                .name("ipc client".into())
                .spawn(move || {
                    let w = pipe.clone();
                    super::serve(hub, |buf| pipe.read(buf), move |data| w.write(data));
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::{TabsSnap, WinInfo};
    use std::io::{BufRead, BufReader, Write};

    fn win(id: isize, x: f64, focused: bool) -> WinInfo {
        WinInfo {
            id,
            title: format!("w{id}"),
            exe: "a.exe".into(),
            x,
            y: 0.0,
            w: 100.0,
            h: 50.0,
            focused,
        }
    }

    fn snap(cam_x: f64, wins: Vec<WinInfo>) -> Snapshot {
        Snapshot {
            cam_x,
            zoom: 1.0,
            windows: Arc::new(wins),
            ..Default::default()
        }
    }

    fn events(v: &[Value]) -> Vec<&str> {
        v.iter().map(|e| e["event"].as_str().unwrap()).collect()
    }

    #[test]
    fn parses_every_op() {
        let cases = [
            (r#"{"id":1,"op":"hello"}"#, Op::Hello),
            (r#"{"id":2,"op":"get_layout"}"#, Op::GetLayout),
            (
                r#"{"id":3,"op":"focus_window","hwnd":1234}"#,
                Op::FocusWindow { hwnd: 1234 },
            ),
            (r#"{"id":4,"op":"leave_stage"}"#, Op::LeaveStage),
            (
                r#"{"id":5,"op":"fly_to","canvas_rect":[1,2,3,4]}"#,
                Op::FlyTo {
                    canvas_rect: [1.0, 2.0, 3.0, 4.0],
                },
            ),
            (r#"{"id":6,"op":"back"}"#, Op::Back),
            (
                r#"{"id":7,"op":"reserve_region","name":"desk","w":800,"h":600}"#,
                Op::ReserveRegion {
                    name: "desk".into(),
                    w: 800.0,
                    h: 600.0,
                },
            ),
            (
                r#"{"id":8,"op":"place_window","hwnd":5,"region":"desk"}"#,
                Op::PlaceWindow {
                    hwnd: 5,
                    region: "desk".into(),
                },
            ),
            (r#"{"id":9,"op":"subscribe"}"#, Op::Subscribe),
        ];
        for (line, want) in cases {
            let (id, op) = parse(line);
            assert!(id.is_number());
            assert_eq!(op, Ok(want), "{line}");
        }
    }

    #[test]
    fn bad_requests_are_errors_with_the_id() {
        let (id, op) = parse(r#"{"id":"x","op":"warp"}"#);
        assert_eq!(id, json!("x"));
        assert_eq!(op, Err("unknown op: warp".into()));
        assert_eq!(parse(r#"{"id":1}"#).1, Err("missing op".into()));
        assert!(parse("{nope").1.unwrap_err().starts_with("bad json"));
        assert_eq!(parse("{nope").0, Value::Null);
        assert!(parse(r#"{"id":1,"op":"focus_window"}"#)
            .1
            .unwrap_err()
            .starts_with("bad request"));
        assert!(parse(r#"{"id":1,"op":"fly_to","canvas_rect":[0,0,0,10]}"#)
            .1
            .is_err());
        assert!(
            parse(r#"{"id":1,"op":"reserve_region","name":"","w":1,"h":1}"#)
                .1
                .is_err()
        );
        assert!(parse(r#"[1]"#).1.is_err());
    }

    #[test]
    fn reply_shapes() {
        let ok: Value = serde_json::from_str(&ok_line(&json!(3), json!({"a":1}))).unwrap();
        assert_eq!(ok, json!({"id":3,"ok":true,"data":{"a":1}}));
        let err: Value = serde_json::from_str(&err_line(&json!(3), "no")).unwrap();
        assert_eq!(err, json!({"id":3,"ok":false,"error":"no"}));
    }

    #[test]
    fn layout_lists_canvas_hidden_tab_and_away_windows() {
        let mut s = snap(100.0, vec![win(1, 150.0, true)]);
        s.zoom = 0.5;
        s.tabs = vec![TabsSnap {
            active: 1,
            split: None,
            spot: (150.0, 0.0, 100.0, 50.0),
            tabs: vec![win(1, 0.0, false), win(2, 0.0, false)],
        }];
        s.away = vec![win(3, -500.0, false)];
        s.mode = Mode::Overview;
        let v = layout_json(&s, |_| true);
        assert_eq!(v["camera"], json!({"x":100.0,"y":0.0,"zoom":0.5}));
        assert_eq!(v["stage"], json!(true));
        let w = v["windows"].as_array().unwrap();
        assert_eq!(w.len(), 3);
        assert_eq!(w[0]["screen_rect"], json!([25.0, 0.0, 50.0, 25.0]));
        assert_eq!(w[0]["on_screen"], json!(true));
        assert_eq!(w[1]["hwnd"], json!(2));
        assert_eq!(w[1]["tab_hidden"], json!(true));
        assert_eq!(w[1]["screen_rect"], Value::Null);
        assert_eq!(w[1]["canvas_rect"], json!([150.0, 0.0, 100.0, 50.0]));
        assert_eq!(w[2]["away"], json!(true));
        assert_eq!(w[2]["on_screen"], json!(false));
    }

    #[test]
    fn diffs_make_events() {
        let mut told = Told::from(&snap(0.0, vec![win(1, 0.0, true), win(2, 200.0, false)]));
        // Nothing changed, nothing to say.
        assert!(told
            .update(
                &snap(0.0, vec![win(1, 0.0, true), win(2, 200.0, false)]),
                |_| false
            )
            .is_empty());

        // Camera moving: screen rects change but aren't sent until rest.
        let mut moving = snap(50.0, vec![win(1, 0.0, true), win(2, 200.0, false)]);
        moving.moving = true;
        moving.gesture = true;
        assert_eq!(
            events(&told.update(&moving, |_| false)),
            ["camera", "gesture"]
        );

        // At rest: camera, gesture over, both windows' new screen rects.
        let rest = snap(50.0, vec![win(1, 0.0, false), win(2, 200.0, true)]);
        let ev = told.update(&rest, |_| false);
        assert_eq!(
            events(&ev),
            ["camera", "gesture", "window_moved", "window_moved", "focus"]
        );
        assert_eq!(ev[2]["screen_rect"], json!([-50.0, 0.0, 100.0, 50.0]));
        assert_eq!(ev[4]["hwnd"], json!(2));

        // Window 1 closes, window 2 moves on the canvas.
        let ev = told.update(&snap(50.0, vec![win(2, 300.0, true)]), |_| false);
        assert_eq!(events(&ev), ["window_closed", "window_moved"]);
        assert_eq!(ev[0]["hwnd"], json!(1));
        assert_eq!(ev[1]["canvas_rect"], json!([300.0, 0.0, 100.0, 50.0]));

        // A window that leaves the list but still exists isn't closed.
        assert!(told.update(&snap(50.0, vec![]), |_| true).is_empty());
    }

    #[test]
    fn stage_and_tab_switch_events() {
        let tabs = |active| {
            vec![TabsSnap {
                active,
                split: None,
                spot: (0.0, 0.0, 1.0, 1.0),
                tabs: vec![win(1, 0.0, false), win(2, 0.0, false)],
            }]
        };
        let mut a = snap(0.0, vec![win(1, 0.0, false)]);
        a.tabs = tabs(1);
        let mut told = Told::from(&a);
        let mut b = snap(0.0, vec![win(2, 0.0, false)]);
        b.tabs = tabs(2);
        b.mode = Mode::Overview;
        let ev = told.update(&b, |_| true);
        assert_eq!(
            events(&ev),
            ["stage", "window_moved", "window_moved", "tab_switch"]
        );
        assert_eq!(ev[0]["open"], json!(true));
        assert_eq!(ev[3], json!({"event":"tab_switch","group":1,"hwnd":2}));
    }

    #[test]
    fn pipe_is_locked_to_this_user() {
        assert!(pipe::user_sid().is_some_and(|s| s.starts_with("S-1-5-")));
    }

    #[test]
    fn lines_split_and_cap() {
        let mut l = Lines::default();
        assert_eq!(
            l.push(b"{\"a\":1}\r\n\n{\"b\""),
            Some(vec!["{\"a\":1}".to_string()])
        );
        assert_eq!(l.push(b":2}\n"), Some(vec!["{\"b\":2}".to_string()]));
        assert_eq!(l.push(&vec![b'x'; MAX_LINE + 1]), None);
    }

    /// A canvas with one window that answers engine requests at once.
    struct Mock {
        snap: Mutex<Arc<Snapshot>>,
        canvas: bool,
        calls: Mutex<Vec<EngineOp>>,
    }

    impl Backend for Mock {
        fn snapshot(&self) -> Arc<Snapshot> {
            self.snap.lock().unwrap().clone()
        }
        fn canvas(&self) -> bool {
            self.canvas
        }
        fn call(&self, call: IpcCall) {
            self.calls.lock().unwrap().push(call.op.clone());
            let data = match call.op {
                EngineOp::ReserveRegion { w, h, .. } => {
                    json!({ "canvas_rect": [5000.0, 0.0, w, h] })
                }
                _ => json!({}),
            };
            let _ = call.reply.send(Ok(data));
        }
        fn is_window(&self, _: isize) -> bool {
            false
        }
        fn on_screen(&self, _: Rect) -> bool {
            true
        }
    }

    fn mock(canvas: bool) -> Arc<Mock> {
        Arc::new(Mock {
            snap: Mutex::new(Arc::new(snap(0.0, vec![win(7, 0.0, true)]))),
            canvas,
            calls: Mutex::new(Vec::new()),
        })
    }

    /// Serves `backend` on a pipe of its own and connects a client to it.
    fn connect(
        backend: Arc<Mock>,
        tag: &str,
    ) -> (Arc<Hub>, std::fs::File, BufReader<std::fs::File>) {
        let name = format!(r"\\.\pipe\apeiron-test-{tag}-{}", std::process::id());
        let hub = Hub::start(backend);
        let h = hub.clone();
        let n = name.clone();
        std::thread::spawn(move || pipe::listen(&n, h));
        let file = (0..100)
            .find_map(|_| {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&name)
                    .ok()
                    .or_else(|| {
                        std::thread::sleep(Duration::from_millis(20));
                        None
                    })
            })
            .expect("pipe never came up");
        let reader = BufReader::new(file.try_clone().unwrap());
        (hub, file, reader)
    }

    fn ask(file: &mut std::fs::File, reader: &mut BufReader<std::fs::File>, line: &str) -> Value {
        writeln!(file, "{line}").unwrap();
        read(reader)
    }

    fn read(reader: &mut BufReader<std::fs::File>) -> Value {
        let mut s = String::new();
        reader.read_line(&mut s).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn serves_over_a_real_pipe() {
        let backend = mock(true);
        let (hub, mut file, mut reader) = connect(backend.clone(), "live");
        let v = ask(&mut file, &mut reader, r#"{"id":1,"op":"hello"}"#);
        assert_eq!(
            v,
            json!({"id":1,"ok":true,"data":{"version":1,"canvas":true}})
        );
        let v = ask(&mut file, &mut reader, r#"{"id":2,"op":"get_layout"}"#);
        assert_eq!(v["data"]["windows"][0]["hwnd"], json!(7));
        let v = ask(
            &mut file,
            &mut reader,
            r#"{"id":3,"op":"reserve_region","name":"desk","w":800,"h":600}"#,
        );
        assert_eq!(v["data"]["canvas_rect"], json!([5000.0, 0.0, 800.0, 600.0]));
        let v = ask(&mut file, &mut reader, r#"{"id":4,"op":"nope"}"#);
        assert_eq!(v["ok"], json!(false));

        // Subscribing: the reply, then the current camera, stage and gesture.
        let v = ask(&mut file, &mut reader, r#"{"id":5,"op":"subscribe"}"#);
        assert_eq!(v["id"], json!(5));
        assert_eq!(read(&mut reader)["event"], json!("camera"));
        assert_eq!(read(&mut reader)["event"], json!("stage"));
        assert_eq!(read(&mut reader)["event"], json!("gesture"));
        hub.diff(); // first diff only records the state
        let mut next = snap(10.0, vec![win(7, 0.0, true)]);
        next.moving = true;
        *backend.snap.lock().unwrap() = Arc::new(next);
        hub.wake();
        let v = read(&mut reader);
        assert_eq!(
            v,
            json!({"event":"camera","x":10.0,"y":0.0,"zoom":1.0,"moving":true})
        );
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn tabs_only_refuses_canvas_ops() {
        let backend = mock(false);
        let (_hub, mut file, mut reader) = connect(backend.clone(), "tabs");
        let v = ask(&mut file, &mut reader, r#"{"id":1,"op":"hello"}"#);
        assert_eq!(v["data"]["canvas"], json!(false));
        for op in [
            r#""op":"fly_to","canvas_rect":[0,0,10,10]"#,
            r#""op":"leave_stage""#,
            r#""op":"back""#,
        ] {
            let v = ask(&mut file, &mut reader, &format!(r#"{{"id":2,{op}}}"#));
            assert_eq!(v, json!({"id":2,"ok":false,"error":CANVAS_OFF}));
        }
        let v = ask(
            &mut file,
            &mut reader,
            r#"{"id":3,"op":"focus_window","hwnd":7}"#,
        );
        assert_eq!(v["ok"], json!(true));
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec![EngineOp::FocusWindow(7)]
        );
    }
}
