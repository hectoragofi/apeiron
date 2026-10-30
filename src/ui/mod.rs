//! The GPUI side: opens the overlay and background windows, restyles their
//! HWNDs into what the canvas needs, and pumps engine events into redraws.

pub mod canvas_view;
pub mod labels;
pub mod minimap;
pub mod search;
pub mod settings_window;
pub mod theme;

use gpui::*;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::shared::{Command, EngineHandle, Mode, Shared, UiEvent};
use canvas_view::{CanvasView, Role};

struct Canvases {
    overlay: WindowHandle<CanvasView>,
    overlay_hwnd: isize,
    bg: WindowHandle<CanvasView>,
}

/// Runs the GPUI app until the engine says quit.
pub fn run(shared: Shared, engine: EngineHandle, ui_rx: async_channel::Receiver<UiEvent>) {
    Application::new().run(move |cx: &mut App| {
        let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        let Some(canvases) = open_canvases(cx, &shared, &engine, sw, sh) else {
            log!("couldn't open the canvas windows");
            cx.quit();
            return;
        };
        if let Some(tray) = crate::tray::create(&shared, &engine) {
            cx.set_global(tray);
        }

        let (shared, engine) = (shared.clone(), engine.clone());
        let ui_rx = ui_rx.clone();
        cx.spawn(async move |cx| {
            let mut activatable = false;
            while let Ok(event) = ui_rx.recv().await {
                let _ = cx.update(|cx| match event {
                    UiEvent::Redraw => {
                        shared.redraw_taken();
                        if activatable && shared.snap().mode != Mode::Search {
                            activatable = false;
                            set_noactivate(canvases.overlay_hwnd, true);
                        }
                        let _ = canvases.overlay.update(cx, |_, _, cx| cx.notify());
                        let _ = canvases.bg.update(cx, |_, _, cx| cx.notify());
                    }
                    UiEvent::FocusSearch => {
                        // The overlay never takes focus, except to type a query.
                        activatable = true;
                        set_noactivate(canvases.overlay_hwnd, false);
                        let _ = canvases.overlay.update(cx, |view, window, _| {
                            window.activate_window();
                            window.focus(&view.focus);
                        });
                        unsafe {
                            let _ = SetForegroundWindow(HWND(canvases.overlay_hwnd as *mut _));
                        }
                    }
                    UiEvent::OpenSettings => settings_window::open(cx, &shared, &engine),
                    UiEvent::Quit => cx.quit(),
                });
            }
        })
        .detach();
    });
}

fn open_canvases(cx: &mut App, shared: &Shared, engine: &EngineHandle, sw: i32, sh: i32) -> Option<Canvases> {
    let (overlay, overlay_hwnd) = open_canvas(cx, Role::Overlay, shared, engine, sw, sh)?;
    let (bg, bg_hwnd) = open_canvas(cx, Role::Background, shared, engine, sw, sh)?;
    engine.send(Command::Attach { overlay: overlay_hwnd, bg: bg_hwnd });
    Some(Canvases { overlay, overlay_hwnd, bg })
}

fn open_canvas(
    cx: &mut App,
    role: Role,
    shared: &Shared,
    engine: &EngineHandle,
    sw: i32,
    sh: i32,
) -> Option<(WindowHandle<CanvasView>, isize)> {
    // Created at its final size so GPUI never has to resize the swap chain.
    let scale = unsafe { GetDpiForSystem() } as f32 / 96.0;
    let bounds = Bounds::new(point(px(0.), px(0.)), size(px(sw as f32 / scale), px(sh as f32 / scale)));
    let (shared, engine) = (shared.clone(), engine.clone());
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                kind: WindowKind::PopUp,
                focus: false,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                ..Default::default()
            },
            move |_, cx| cx.new(|cx| CanvasView::new(shared, engine, role, cx)),
        )
        .map_err(|e| log!("open_window failed: {e}"))
        .ok()?;
    let hwnd = handle.update(cx, |_, window, _| hwnd_of(window)).ok()??;
    restyle(HWND(hwnd as *mut _), role, sw, sh);
    Some((handle, hwnd))
}

fn hwnd_of(window: &Window) -> Option<isize> {
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// GPUI's PopUp is a bordered tool window; make it a borderless fullscreen
/// one. The overlay is topmost, never activates and starts hidden (the
/// engine shows it); the background is pinned to the bottom of the z-order.
fn restyle(hwnd: HWND, role: Role, sw: i32, sh: i32) {
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let add = WS_EX_NOACTIVATE.0 | if role == Role::Overlay { WS_EX_TOPMOST.0 } else { 0 };
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (ex | add) as isize);
        let order = if role == Role::Overlay { HWND_TOPMOST } else { HWND_BOTTOM };
        if let Err(e) = SetWindowPos(hwnd, Some(order), 0, 0, sw, sh, SWP_FRAMECHANGED | SWP_NOACTIVATE) {
            log!("SetWindowPos failed: {e}");
        }
        // GPUI trims the client area by the thick-frame insets even though a
        // popup has no frame; grow the window by exactly that much so the
        // client area covers the screen.
        let (mut win, mut client, mut origin) = (RECT::default(), RECT::default(), POINT::default());
        if GetWindowRect(hwnd, &mut win).is_ok()
            && GetClientRect(hwnd, &mut client).is_ok()
            && ClientToScreen(hwnd, &mut origin).as_bool()
        {
            let (l, t) = (origin.x - win.left, origin.y - win.top);
            let r = (win.right - win.left) - client.right - l;
            let b = (win.bottom - win.top) - client.bottom - t;
            let _ = SetWindowPos(hwnd, Some(order), -l, -t, sw + l + r, sh + t + b, SWP_NOACTIVATE);
        }
        match role {
            Role::Overlay => {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            Role::Background => {
                if !SetWindowSubclass(hwnd, Some(bg_proc), 1, 0).as_bool() {
                    log!("couldn't subclass the background window");
                }
            }
        }
    }
}

fn set_noactivate(hwnd: isize, on: bool) {
    unsafe {
        let hwnd = HWND(hwnd as *mut _);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let ex = if on { ex | WS_EX_NOACTIVATE.0 } else { ex & !WS_EX_NOACTIVATE.0 };
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex as isize);
    }
}

/// The background is the desktop: it stays at the bottom and never takes focus.
unsafe extern "system" fn bg_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    match msg {
        WM_WINDOWPOSCHANGING => {
            let pos = &mut *(lp.0 as *mut WINDOWPOS);
            if pos.flags.contains(SWP_NOZORDER) {
                return DefSubclassProc(hwnd, msg, wp, lp);
            }
            pos.hwndInsertAfter = HWND_BOTTOM;
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}
