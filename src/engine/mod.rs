//! The engine thread: owns the canvas model, the raw touchpad input, the
//! hotkeys and the animation timer, and moves real windows. It talks to the
//! GPUI thread through `Snapshot`/`UiEvent` out and `Command` in.

pub mod canvas;
pub mod tidy;
pub mod touchpad;
pub mod wins;

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver};

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::shared::{Command, EngineHandle, Shared};
use canvas::Canvas;
use touchpad::Touchpads;

/// Posted by `EngineHandle::send` to wake the message loop.
pub const WM_APP_CMD: u32 = WM_APP + 1;

pub const HOTKEY_OVERVIEW: i32 = 1;
pub const HOTKEY_QUIT: i32 = 2;
pub const HOTKEY_ESC: i32 = 3;
pub const HOTKEY_FIND: i32 = 4;
pub const HOTKEY_TIDY: i32 = 5;

thread_local! {
    static CANVAS: RefCell<Option<Canvas>> = const { RefCell::new(None) };
    static PADS: RefCell<Touchpads> = RefCell::new(Touchpads::default());
    static COMMANDS: RefCell<Option<Receiver<Command>>> = const { RefCell::new(None) };
}

/// Starts the engine thread and waits for its message window to exist.
pub fn spawn(shared: Shared) -> Result<EngineHandle, String> {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("engine".into())
        .spawn(move || run(shared, cmd_rx, ready_tx))
        .map_err(|e| e.to_string())?;
    let hwnd = ready_rx.recv().map_err(|_| "engine thread died".to_string())??;
    Ok(EngineHandle::new(cmd_tx, hwnd))
}

fn run(shared: Shared, commands: Receiver<Command>, ready: mpsc::Sender<Result<isize, String>>) {
    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(format!("couldn't create the engine window: {e}")));
            return;
        }
    };
    if let Err(e) = touchpad::register(hwnd) {
        log!("warning: couldn't register for touchpad input: {e}");
    }
    register_hotkeys(hwnd);
    COMMANDS.with(|c| *c.borrow_mut() = Some(commands));
    CANVAS.with(|c| *c.borrow_mut() = Some(Canvas::new(hwnd, shared)));
    let _ = ready.send(Ok(hwnd.0 as isize));

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    log!("engine stopped");
}

fn create_window() -> windows::core::Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("InfiniteCanvasEngine"),
            ..Default::default()
        };
        RegisterClassW(&class);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class.lpszClassName,
            w!("infinite-canvas engine"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn register_hotkeys(hwnd: HWND) {
    let mods = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
    for (id, key, name) in [
        (HOTKEY_OVERVIEW, 'Z', "Ctrl+Alt+Z"),
        (HOTKEY_FIND, 'F', "Ctrl+Alt+F"),
        (HOTKEY_TIDY, 'T', "Ctrl+Alt+T"),
        (HOTKEY_QUIT, 'Q', "Ctrl+Alt+Q"),
    ] {
        if let Err(e) = unsafe { RegisterHotKey(Some(hwnd), id, mods, key as u32) } {
            log!("warning: couldn't register {name}: {}", e.message());
        }
    }
}

/// Runs `f` against the canvas unless it's already borrowed, then publishes
/// whatever changed. Win32 calls made from inside the canvas can re-enter
/// this window procedure synchronously; those nested messages are skipped.
fn with_canvas<R>(f: impl FnOnce(&mut Canvas) -> R) -> Option<R> {
    CANVAS.with(|c| {
        let mut guard = c.try_borrow_mut().ok()?;
        let canvas = guard.as_mut()?;
        let r = f(canvas);
        canvas.flush();
        Some(r)
    })
}

fn drain_commands() {
    let pending: Vec<Command> = COMMANDS.with(|c| {
        let guard = c.borrow();
        guard.as_ref().map(|rx| rx.try_iter().collect()).unwrap_or_default()
    });
    for cmd in pending {
        with_canvas(|c| c.command(cmd));
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_INPUT => {
            let frame = PADS.with(|p| {
                let sw = GetSystemMetrics(SM_CXSCREEN) as f64;
                p.try_borrow_mut().ok()?.on_input(lp.0, sw)
            });
            if let Some(fingers) = frame {
                with_canvas(|c| c.on_fingers(&fingers));
            }
            // WM_INPUT must still reach DefWindowProc for cleanup.
        }
        WM_APP_CMD => {
            drain_commands();
            return LRESULT(0);
        }
        WM_TIMER => {
            with_canvas(|c| c.on_timer(wp.0));
            return LRESULT(0);
        }
        WM_HOTKEY => {
            with_canvas(|c| match wp.0 as i32 {
                HOTKEY_OVERVIEW => c.command(Command::ToggleOverview),
                HOTKEY_FIND => c.command(Command::OpenSearch),
                HOTKEY_TIDY => c.command(Command::Tidy),
                HOTKEY_QUIT => c.command(Command::Quit),
                HOTKEY_ESC => c.escape(),
                _ => {}
            });
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}
