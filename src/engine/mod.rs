//! The engine thread: owns the canvas model, the raw touchpad input, the
//! hotkeys and the animation ticks, and moves real windows. It talks to the
//! GPUI thread through `Snapshot`/`UiEvent` out and `Command` in.

pub mod canvas;
pub mod keys;
pub mod mouse;
pub mod tidy;
pub mod ticker;
pub mod trace;
pub mod snap;
pub mod touchpad;
pub mod wins;

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver};

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::shared::{Command, EngineHandle, Shared};
use canvas::Canvas;
use ticker::Ticker;
use touchpad::Touchpads;

/// Posted by `EngineHandle::send` to wake the message loop.
pub const WM_APP_CMD: u32 = WM_APP + 1;
/// Posted by the ticker once per composed frame while animating.
pub const WM_APP_TICK: u32 = WM_APP + 2;
/// Posted by the UI when one of its windows sees the display or its scale change.
pub const WM_APP_SCREEN: u32 = WM_APP + 3;
/// Posted by the keyboard hook: Tab (or an arrow) with Alt held.
pub const WM_APP_SWITCH: u32 = WM_APP + 4;
/// Posted by the keyboard hook: Alt let go (accept) or Esc (cancel).
pub const WM_APP_SWITCH_END: u32 = WM_APP + 5;
/// Posted by the mouse hook (see `mouse`).
pub const WM_APP_MOUSE: u32 = WM_APP + 6;

pub const HOTKEY_OVERVIEW: i32 = 1;
pub const HOTKEY_QUIT: i32 = 2;
pub const HOTKEY_ESC: i32 = 3;
pub const HOTKEY_FIND: i32 = 4;
pub const HOTKEY_TIDY: i32 = 5;
/// Delete, only while the overview is up.
pub const HOTKEY_DELETE: i32 = 6;

/// Retries draining commands that arrived while the canvas was busy.
const TIMER_DRAIN: usize = 50;

const WTS_SESSION_LOCK: usize = 7;
const WTS_SESSION_UNLOCK: usize = 8;

thread_local! {
    static CANVAS: RefCell<Option<Canvas>> = const { RefCell::new(None) };
    static PADS: RefCell<Touchpads> = RefCell::new(Touchpads::default());
    static COMMANDS: RefCell<Option<Receiver<Command>>> = const { RefCell::new(None) };
    static TICKER: RefCell<Option<Ticker>> = const { RefCell::new(None) };
}

/// Starts the engine thread and waits for its window to exist.
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
    keys::spawn_hook(hwnd);
    mouse::spawn_hook(hwnd);
    hook_window_events();
    if let Err(e) = unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } {
        log!("warning: no session lock notifications: {e}");
    }
    COMMANDS.with(|c| *c.borrow_mut() = Some(commands));
    let ticker = Ticker::spawn(hwnd);
    TICKER.with(|t| *t.borrow_mut() = Some(ticker.clone()));
    CANVAS.with(|c| *c.borrow_mut() = Some(Canvas::new(hwnd, shared, ticker)));
    with_canvas(|c| c.apply_keys());
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

/// A hidden top-level window rather than a message-only one: only top-level
/// windows hear broadcasts such as end-of-session and display changes.
fn create_window() -> windows::core::Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("ApeironEngine"),
            ..Default::default()
        };
        RegisterClassW(&class);
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.lpszClassName,
            w!("Apeiron engine"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

/// Window switches, opens, closes, moves and minimizes elsewhere on the
/// desktop. Deliberately narrow ranges: focus events fire for every caret
/// blink, and location events for every cursor move (those are dropped at
/// the top of the callback).
fn hook_window_events() {
    let ranges = [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZEEND),
        (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
        (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
        (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
        // Virtual desktop switches cloak and uncloak windows.
        (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
    ];
    for (min, max) in ranges {
        let hook = unsafe {
            SetWinEventHook(min, max, None, Some(on_win_event), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS)
        };
        if hook.is_invalid() {
            log!("warning: couldn't watch window events {min:#x}..{max:#x}");
        }
    }
}

unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    child: i32,
    _thread: u32,
    _time: u32,
) {
    if hwnd.is_invalid() || object != OBJID_WINDOW.0 || child != CHILDID_SELF as i32 {
        return;
    }
    if GetAncestor(hwnd, GA_ROOT) != hwnd {
        return;
    }
    with_canvas(|c| match event {
        EVENT_OBJECT_LOCATIONCHANGE => c.window_moved(hwnd),
        EVENT_OBJECT_CLOAKED | EVENT_OBJECT_UNCLOAKED => c.desktop_changed(hwnd),
        EVENT_SYSTEM_FOREGROUND => {
            c.foreground_changed(hwnd);
            c.windows_changed();
        }
        _ => c.windows_changed(),
    });
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

/// Applies every queued command under one borrow and publishes once. If the
/// canvas is busy (a re-entrant call), they stay queued and a timer retries.
fn drain_commands(hwnd: HWND) {
    let done = with_canvas(|c| {
        let pending: Vec<Command> = COMMANDS.with(|q| {
            let guard = q.borrow();
            guard.as_ref().map(|rx| rx.try_iter().collect()).unwrap_or_default()
        });
        for cmd in coalesce(pending) {
            c.command(cmd);
        }
    });
    if done.is_none() {
        unsafe {
            SetTimer(Some(hwnd), TIMER_DRAIN, 10, None);
        }
    }
}

/// Only the latest of a run of mouse moves matters, and a run of wheel
/// steps adds up to one.
fn coalesce(cmds: Vec<Command>) -> Vec<Command> {
    let mut out: Vec<Command> = Vec::with_capacity(cmds.len());
    for cmd in cmds {
        let merged = match (out.last_mut(), &cmd) {
            (Some(Command::MouseMove { x, y }), Command::MouseMove { x: nx, y: ny }) => {
                (*x, *y) = (*nx, *ny);
                true
            }
            (
                Some(Command::Wheel { dx, dy, pinch, mouse, shift, x, y }),
                Command::Wheel { dx: ndx, dy: ndy, pinch: npinch, mouse: nmouse, shift: nshift, x: nx, y: ny },
            ) if pinch == npinch && mouse == nmouse && shift == nshift => {
                (*dx, *dy, *x, *y) = (*dx + ndx, *dy + ndy, *nx, *ny);
                true
            }
            _ => false,
        };
        if !merged {
            out.push(cmd);
        }
    }
    out
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
            drain_commands(hwnd);
            return LRESULT(0);
        }
        WM_APP_SWITCH => {
            with_canvas(|c| c.switch_step(wp.0 == keys::STEP_BACK));
            return LRESULT(0);
        }
        WM_APP_SWITCH_END => {
            with_canvas(|c| c.switch_end(wp.0 == keys::END_ACCEPT));
            return LRESULT(0);
        }
        WM_APP_MOUSE => {
            with_canvas(|c| c.on_mouse_hook(wp.0, mouse::unpack(lp)));
            return LRESULT(0);
        }
        WM_APP_TICK => {
            // Acknowledged even if the canvas is busy, or no tick would come again.
            TICKER.with(|t| t.borrow().as_ref().map(Ticker::taken));
            with_canvas(|c| c.on_tick());
            return LRESULT(0);
        }
        WM_TIMER => {
            match wp.0 {
                TIMER_DRAIN => {
                    let _ = KillTimer(Some(hwnd), TIMER_DRAIN);
                    drain_commands(hwnd);
                }
                ticker::TIMER_TICK => {
                    with_canvas(|c| c.on_tick());
                }
                id => {
                    with_canvas(|c| c.on_timer(id));
                }
            }
            return LRESULT(0);
        }
        WM_HOTKEY => {
            with_canvas(|c| match wp.0 as i32 {
                HOTKEY_OVERVIEW => c.command(Command::ToggleOverview),
                HOTKEY_FIND => c.command(Command::OpenSearch),
                HOTKEY_TIDY => c.command(Command::Tidy),
                HOTKEY_QUIT => c.command(Command::Quit),
                HOTKEY_ESC => c.escape(),
                HOTKEY_DELETE => c.delete_key(),
                _ => {}
            });
            return LRESULT(0);
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED | WM_APP_SCREEN => {
            with_canvas(|c| c.screen_changed());
            return LRESULT(0);
        }
        WM_SETTINGCHANGE if wp.0 == SPI_SETDESKWALLPAPER.0 as usize => {
            crate::wallpaper::reload();
        }
        WM_SETTINGCHANGE if wp.0 == SPI_SETWORKAREA.0 as usize => {
            with_canvas(|c| c.screen_changed());
        }
        // Sign-out, shutdown or restart: the process is about to be killed
        // without unwinding, so put every window back now.
        WM_QUERYENDSESSION => {
            with_canvas(|c| c.end_session());
            return LRESULT(1);
        }
        WM_ENDSESSION => {
            if wp.0 != 0 {
                with_canvas(|c| c.end_session());
            }
            return LRESULT(0);
        }
        WM_WTSSESSION_CHANGE => match wp.0 {
            WTS_SESSION_LOCK => {
                with_canvas(|c| c.set_locked(true));
            }
            WTS_SESSION_UNLOCK => {
                with_canvas(|c| c.set_locked(false));
            }
            _ => {}
        },
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_moves_and_wheel_runs() {
        let cmds = vec![
            Command::MouseMove { x: 1.0, y: 1.0 },
            Command::MouseMove { x: 5.0, y: 6.0 },
            Command::MouseUp,
            Command::Wheel { dx: 1.0, dy: 2.0, pinch: false, mouse: false, shift: false, x: 0.0, y: 0.0 },
            Command::Wheel { dx: 3.0, dy: 4.0, pinch: false, mouse: false, shift: false, x: 9.0, y: 9.0 },
            Command::Wheel { dx: 0.0, dy: 60.0, pinch: true, mouse: false, shift: false, x: 9.0, y: 9.0 },
        ];
        let out = coalesce(cmds);
        assert_eq!(out.len(), 4);
        assert!(matches!(out[0], Command::MouseMove { x, y } if (x, y) == (5.0, 6.0)));
        assert!(matches!(out[1], Command::MouseUp));
        assert!(matches!(out[2], Command::Wheel { dx, dy, pinch: false, x, .. } if (dx, dy, x) == (4.0, 6.0, 9.0)));
        assert!(matches!(out[3], Command::Wheel { pinch: true, .. }));
    }
}
