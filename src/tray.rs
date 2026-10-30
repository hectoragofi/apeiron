//! Tray icon and its menu. Created on the GPUI thread; clicks and menu events
//! arrive through GPUI's message loop on that same thread, so the icon and
//! the Pause item live in a thread-local the handlers can reach.

use std::cell::RefCell;
use std::time::Duration;

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};

use crate::shared::{Command, EngineHandle, Shared, UiEvent};
use crate::ui::theme;

const ID_OVERVIEW: &str = "overview";
const ID_FIND: &str = "find";
const ID_TIDY: &str = "tidy";
const ID_SETTINGS: &str = "settings";
const ID_PAUSE: &str = "pause";
const ID_QUIT: &str = "quit";

struct State {
    icon: TrayIcon,
    pause: MenuItem,
    paused: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Keeps the tray icon alive; dropping it removes the icon.
pub struct Tray;

impl gpui::Global for Tray {}

impl Drop for Tray {
    fn drop(&mut self) {
        remove();
    }
}

pub fn create(shared: &Shared, engine: &EngineHandle) -> Option<Tray> {
    let pause = MenuItem::with_id(ID_PAUSE, "Pause", true, None);
    let menu = match Menu::with_items(&[
        &MenuItem::with_id(ID_OVERVIEW, "Overview\tCtrl+Alt+Z", true, None),
        &MenuItem::with_id(ID_FIND, "Find window…\tCtrl+Alt+F", true, None),
        &MenuItem::with_id(ID_TIDY, "Tidy up\tCtrl+Alt+T", true, None),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id(ID_SETTINGS, "Settings…", true, None),
        &pause,
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id(ID_QUIT, "Quit\tCtrl+Alt+Q", true, None),
    ]) {
        Ok(m) => m,
        Err(e) => {
            log!("tray: couldn't build the menu: {e}");
            return None;
        }
    };

    let click = engine.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
            click.send(Command::ToggleOverview);
        }
    }));
    let (shared, engine) = (shared.clone(), engine.clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| match event.id.as_ref() {
        ID_OVERVIEW => engine.send(Command::ToggleOverview),
        ID_FIND => engine.send(Command::OpenSearch),
        ID_TIDY => engine.send(Command::Tidy),
        ID_SETTINGS => shared.emit(UiEvent::OpenSettings),
        ID_PAUSE => toggle_pause(&engine),
        ID_QUIT => quit(&engine),
        _ => {}
    }));

    let icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip(tooltip(false))
        .with_icon(icon(false)?)
        .build()
        .map_err(|e| log!("tray: couldn't create the icon: {e}"))
        .ok()?;
    STATE.with(|s| *s.borrow_mut() = Some(State { icon, pause, paused: false }));
    Some(Tray)
}

/// Takes the icon out of the notification area. Safe to call more than once.
pub fn remove() {
    let state = STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.take()));
    if let Some(state) = state {
        let _ = state.icon.set_visible(false);
    }
}

/// Asks the engine to quit. If it's stuck and the process is still here a
/// few seconds later, bring windows back from this side and exit anyway.
fn quit(engine: &EngineHandle) {
    engine.send(Command::Quit);
    let spawned = std::thread::Builder::new().name("quit-fallback".into()).spawn(|| {
        std::thread::sleep(QUIT_GRACE);
        log!("engine didn't quit; rescuing windows and exiting");
        crate::engine::wins::rescue_all();
        std::process::exit(0);
    });
    if let Err(e) = spawned {
        log!("couldn't start the quit fallback: {e}");
    }
}

/// The engine's own quit takes well under a second; it exits after 3 s even
/// if the UI hangs.
const QUIT_GRACE: Duration = Duration::from_secs(5);

fn toggle_pause(engine: &EngineHandle) {
    STATE.with(|s| {
        let Ok(mut guard) = s.try_borrow_mut() else { return };
        let Some(state) = guard.as_mut() else { return };
        state.paused = !state.paused;
        engine.send(Command::SetPaused(state.paused));
        state.pause.set_text(if state.paused { "Resume" } else { "Pause" });
        let _ = state.icon.set_tooltip(Some(tooltip(state.paused)));
        if let Some(icon) = icon(state.paused) {
            let _ = state.icon.set_icon(Some(icon));
        }
    });
}

fn tooltip(paused: bool) -> &'static str {
    if paused {
        "infinite-canvas (paused)"
    } else {
        "infinite-canvas"
    }
}

fn icon(paused: bool) -> Option<Icon> {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64) as u32;
    Icon::from_rgba(draw_icon(size, paused), size, size)
        .map_err(|e| log!("tray: bad icon: {e}"))
        .ok()
}

/// Two overlapping windows: an outlined one behind, a filled accent one in
/// front with a gap cut around it. Grey all over while paused. Drawn on a
/// 32-unit grid, antialiased with signed distances, RGBA out.
fn draw_icon(size: u32, paused: bool) -> Vec<u8> {
    const BACK: u32 = 0x9aa0ad;
    let front = if paused { 0x6b7080 } else { theme::ACCENT };
    let k = size as f64 / 32.0;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            // Pixel centre in design units.
            let (x, y) = ((px as f64 + 0.5) / k, (py as f64 + 0.5) / k);
            let cover = |d: f64| (0.5 - d * k).clamp(0.0, 1.0);
            let back = cover(rounded_rect(x, y, (3.0, 4.0, 21.0, 20.0), 3.5))
                - cover(rounded_rect(x, y, (5.5, 6.5, 18.5, 17.5), 1.5));
            let gap = cover(rounded_rect(x, y, (9.0, 10.0, 31.0, 30.0), 5.5));
            let fill = cover(rounded_rect(x, y, (11.0, 12.0, 29.0, 28.0), 3.5));

            // Back outline, knocked out around the front window, then the front on top.
            let back_a = back.max(0.0) * (1.0 - gap);
            let a = fill + back_a * (1.0 - fill);
            let mix = |ch: u32| {
                let (f, b) = (((front >> ch) & 0xff) as f64, ((BACK >> ch) & 0xff) as f64);
                if a > 0.0 {
                    ((f * fill + b * back_a * (1.0 - fill)) / a).round() as u8
                } else {
                    0
                }
            };
            out.extend_from_slice(&[mix(16), mix(8), mix(0), (a * 255.0).round() as u8]);
        }
    }
    out
}

/// Signed distance from (x, y) to a rounded rectangle given as (x0, y0, x1, y1).
fn rounded_rect(x: f64, y: f64, (x0, y0, x1, y1): (f64, f64, f64, f64), r: f64) -> f64 {
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let (hx, hy) = ((x1 - x0) / 2.0 - r, (y1 - y0) / 2.0 - r);
    let (qx, qy) = ((x - cx).abs() - hx, (y - cy).abs() - hy);
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_both_windows_and_a_clear_corner() {
        for size in [16, 24, 32] {
            let px = draw_icon(size, false);
            assert_eq!(px.len(), (size * size * 4) as usize);
            let at = |x: u32, y: u32| &px[((y * size + x) * 4) as usize..][..4];
            // Top-right corner is empty, the front window's middle is solid accent.
            assert_eq!(at(size - 1, 0)[3], 0);
            let mid = at(size * 20 / 32, size * 20 / 32);
            assert_eq!(mid, &[0x6c, 0x8c, 0xff, 0xff]);
        }
    }
}
