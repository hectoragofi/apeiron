mod canvas;
mod touchpad;
mod wins;

use std::cell::RefCell;
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use canvas::Canvas;
use touchpad::Touchpads;

const HOTKEY_OVERVIEW: i32 = 1;
const HOTKEY_QUIT: i32 = 2;
const MK_CONTROL: usize = 0x0008;

thread_local! {
    static CANVAS: RefCell<Option<Canvas>> = const { RefCell::new(None) };
    static PADS: RefCell<Touchpads> = RefCell::new(Touchpads::default());
}
static OVERLAY: AtomicIsize = AtomicIsize::new(0);
static BG: AtomicIsize = AtomicIsize::new(0);

fn main() -> windows::core::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("rescue") => {
            println!("moved {} windows back on screen", wins::rescue_all());
            return Ok(());
        }
        Some("--watchdog") => {
            watchdog(args.get(2).and_then(|p| p.parse().ok()).unwrap_or(0));
            return Ok(());
        }
        _ => {}
    }

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = SetConsoleCtrlHandler(Some(on_console_close), true);
    }
    std::panic::set_hook(Box::new(|info| {
        eprintln!("{info}");
        wins::rescue_all();
    }));
    spawn_watchdog();

    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let overlay = create_window(w!("InfiniteCanvasOverlay"), WS_EX_TOOLWINDOW | WS_EX_TOPMOST, (0, 0))?;
    let bg = create_window(w!("InfiniteCanvasBackground"), WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, (sw, sh))?;
    OVERLAY.store(overlay.0 as isize, Ordering::SeqCst);
    BG.store(bg.0 as isize, Ordering::SeqCst);
    CANVAS.with(|c| *c.borrow_mut() = Some(Canvas::new(overlay, bg, sw as f64, sh as f64)));
    unsafe {
        let _ = SetWindowPos(bg, Some(HWND_BOTTOM), 0, 0, sw, sh, SWP_SHOWWINDOW | SWP_NOACTIVATE);
    }

    touchpad::register(overlay)?;
    for (id, key, name) in [(HOTKEY_OVERVIEW, 'Z', "Ctrl+Alt+Z"), (HOTKEY_QUIT, 'Q', "Ctrl+Alt+Q")] {
        if let Err(e) = unsafe { RegisterHotKey(Some(overlay), id, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, key as u32) } {
            eprintln!("warning: couldn't register {name}: {}", e.message());
        }
    }

    println!("infinite-canvas running");
    println!("  three-finger drag   pan the canvas");
    println!("  three-finger pinch  zoom");
    println!("  two-finger scroll / pinch on empty canvas  pan / zoom");
    println!("  Ctrl+Alt+Z          overview / back");
    println!("  Esc                 back to 1:1 (in overview)");
    println!("  Ctrl+Alt+Q          quit");
    println!("Turn off Windows' own three-finger gestures in Settings > Bluetooth & devices > Touchpad.");

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    let n = wins::rescue_all();
    println!("bye ({n} windows brought back on screen)");
    Ok(())
}

fn create_window(class_name: PCWSTR, ex: WINDOW_EX_STYLE, size: (i32, i32)) -> windows::core::Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&class);
        CreateWindowExW(
            ex,
            class_name,
            w!("infinite-canvas"),
            WS_POPUP,
            0,
            0,
            size.0,
            size.1,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

/// Runs `f` against the canvas unless it's already borrowed. Win32 calls made
/// from inside the canvas can re-enter this window procedure synchronously;
/// those nested messages fall through to DefWindowProc.
fn with_canvas<R>(f: impl FnOnce(&mut Canvas) -> R) -> Option<R> {
    CANVAS.with(|c| c.try_borrow_mut().ok()?.as_mut().map(f))
}

fn point(lp: LPARAM) -> POINT {
    POINT { x: (lp.0 & 0xffff) as i16 as i32, y: ((lp.0 >> 16) & 0xffff) as i16 as i32 }
}

fn is_bg(hwnd: HWND) -> bool {
    hwnd.0 as isize == BG.load(Ordering::SeqCst)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let handled = match msg {
        WM_INPUT => {
            let frame = PADS.with(|p| {
                let sw = GetSystemMetrics(SM_CXSCREEN) as f64;
                p.try_borrow_mut().ok()?.on_input(lp.0, sw)
            });
            if let Some(fingers) = frame {
                with_canvas(|c| c.on_fingers(&fingers));
            }
            // WM_INPUT must still reach DefWindowProc for cleanup.
            None
        }
        WM_TIMER => with_canvas(|c| match wp.0 {
            canvas::TIMER_ANIM => c.tick(),
            canvas::TIMER_HIDE => c.hide_overlay(),
            canvas::TIMER_WHEEL => c.wheel_idle(),
            _ => {}
        }),
        WM_HOTKEY => match wp.0 as i32 {
            HOTKEY_OVERVIEW => with_canvas(|c| c.toggle_overview()),
            canvas::HOTKEY_ESC => with_canvas(|c| c.leave_overview()),
            HOTKEY_QUIT => {
                PostQuitMessage(0);
                Some(())
            }
            _ => None,
        },
        // The background is the desktop: it stays at the bottom and never takes focus.
        WM_WINDOWPOSCHANGING if is_bg(hwnd) => {
            let pos = &mut *(lp.0 as *mut WINDOWPOS);
            pos.hwndInsertAfter = HWND_BOTTOM;
            pos.flags &= !SWP_NOZORDER;
            Some(())
        }
        WM_MOUSEACTIVATE if is_bg(hwnd) => return LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONDOWN if is_bg(hwnd) => None,
        WM_LBUTTONDOWN => {
            SetCapture(hwnd);
            with_canvas(|c| c.mouse_down(point(lp)))
        }
        WM_MOUSEMOVE => with_canvas(|c| c.mouse_move(point(lp))),
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            with_canvas(|c| c.mouse_up())
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let delta = ((wp.0 >> 16) & 0xffff) as i16 as f64;
            let pinch = wp.0 & MK_CONTROL != 0;
            with_canvas(|c| c.wheel(delta, msg == WM_MOUSEHWHEEL, pinch, point(lp)))
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            with_canvas(|c| c.paint(hdc));
            let _ = EndPaint(hwnd, &ps);
            Some(())
        }
        WM_ERASEBKGND => return LRESULT(1),
        _ => None,
    };
    match handled {
        Some(()) if msg != WM_INPUT => LRESULT(0),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Closing the console kills the process without unwinding; put any parked
/// windows back first.
unsafe extern "system" fn on_console_close(_ctrl: u32) -> BOOL {
    wins::rescue_all();
    false.into()
}

/// A detached copy of ourselves that waits for this process to exit (however
/// it exits) and then brings stranded windows back on screen.
fn spawn_watchdog() {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let Ok(exe) = std::env::current_exe() else { return };
    let _ = std::process::Command::new(exe)
        .args(["--watchdog", &std::process::id().to_string()])
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn();
}

fn watchdog(pid: u32) {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else { return };
        WaitForSingleObject(h, INFINITE);
        let _ = CloseHandle(h);
    }
    wins::rescue_all();
}
