//! The view behind both fullscreen windows: the backdrop (wallpaper or dot
//! grid), plus (on the overlay) labels, the hover ring and search, and the
//! minimap on either.
//!
//! The overlay forwards mouse and wheel input to the engine as `Command`s in
//! physical pixels.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::*;

use super::{backdrop, labels, minimap, search, theme};
use crate::shared::{Command, EngineHandle, Shared, Snapshot, WinInfo};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Topmost, shown while the canvas is in motion or being browsed.
    Overlay,
    /// The desktop: always there, under every app.
    Background,
}

pub struct CanvasView {
    shared: Shared,
    engine: EngineHandle,
    role: Role,
    pub focus: FocusHandle,
    /// The engine has been told the left button went down on the canvas and
    /// hasn't heard it come up yet.
    pressed: Rc<Cell<bool>>,
    /// The wallpaper last painted, freed from the atlas once replaced.
    wallpaper: Option<Arc<crate::wallpaper::Wallpaper>>,
}

impl CanvasView {
    pub fn new(shared: Shared, engine: EngineHandle, role: Role, cx: &mut Context<Self>) -> Self {
        CanvasView { shared, engine, role, focus: cx.focus_handle(), pressed: Rc::new(Cell::new(false)), wallpaper: None }
    }
}

/// Where a window's rectangle falls on screen, in logical pixels.
#[allow(dead_code)]
pub fn win_bounds(snap: &Snapshot, w: &WinInfo, scale: f32) -> Bounds<Pixels> {
    let s = snap.zoom / scale as f64;
    Bounds::new(
        point(px(((w.x - snap.cam_x) * s) as f32), px(((w.y - snap.cam_y) * s) as f32)),
        size(px((w.w * s) as f32), px((w.h * s) as f32)),
    )
}

fn phys(p: Point<Pixels>, scale: f32) -> (f64, f64) {
    (f32::from(p.x) as f64 * scale as f64, f32::from(p.y) as f64 * scale as f64)
}

impl Render for CanvasView {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let scale = window.scale_factor();
        let settings = self.shared.settings();
        // Taken before reading, so a publish from here on asks for a new frame.
        self.shared.redraw_taken();
        // A cheap clone; the engine can publish again while this frame builds.
        let snap = self.shared.snap();
        self.forget_old_wallpaper(window);

        let (backdrop_snap, backdrop_settings) = (snap.clone(), settings.clone());
        let mut root = div()
            .size_full()
            .relative()
            .bg(theme::color(theme::BG))
            .child(
                canvas(|_, _, _| (), move |bounds, _, window, _| {
                    backdrop::paint(bounds, window, &backdrop_snap, &backdrop_settings, scale)
                })
                .absolute()
                .size_full(),
            );

        if self.role == Role::Overlay {
            let engine = self.engine.clone();
            let (down, moved, keys) = (engine.clone(), engine.clone(), engine.clone());
            let (pressed_down, pressed_move) = (self.pressed.clone(), self.pressed.clone());
            root = root
                .track_focus(&self.focus)
                .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, _, _| {
                    let (x, y) = phys(ev.position, scale);
                    pressed_down.set(true);
                    let additive = ev.modifiers.control || ev.modifiers.shift;
                    down.send(Command::MouseDown { x, y, additive });
                })
                .on_mouse_move(move |ev: &MouseMoveEvent, _, _| {
                    // The button came up somewhere we didn't hear about.
                    if pressed_move.get() && !ev.dragging() {
                        pressed_move.set(false);
                        moved.send(Command::MouseUp);
                    }
                    let (x, y) = phys(ev.position, scale);
                    moved.send(Command::MouseMove { x, y });
                })
                .on_scroll_wheel(move |ev: &ScrollWheelEvent, _, _| {
                    // A mouse-wheel notch is 3 lines of 20 px = 60 px of pan.
                    let d = ev.delta.pixel_delta(px(20.0));
                    let k = if ev.delta.precise() { scale as f64 } else { 1.0 };
                    let (x, y) = phys(ev.position, scale);
                    engine.send(Command::Wheel {
                        dx: f32::from(d.x) as f64 * k,
                        dy: f32::from(d.y) as f64 * k,
                        pinch: ev.modifiers.control,
                        x,
                        y,
                    });
                })
                .on_key_down(move |ev: &KeyDownEvent, _, _| search::on_key(ev, &keys))
                .child(release_listener(self.pressed.clone(), self.engine.clone()));
            let live: Vec<&WinInfo> = snap.windows.iter().chain(&snap.minimized).collect();
            crate::icons::prune(&live, window);
            if settings.show_labels {
                root = root.children(labels::render(&snap, &settings, &self.engine, scale));
            }
            root = root.children(labels::selection(&snap, scale));
            root = root.children(labels::hover(&snap, scale));
            root = root.children(labels::marquee(&snap, scale));
            root = root.children(search::render(&snap, &self.engine, scale));
        }
        // Right click, on the overlay or the desktop itself: what can be done here.
        let menu = self.engine.clone();
        root = root.on_mouse_down(MouseButton::Right, move |ev: &MouseDownEvent, _, _| {
            let (x, y) = phys(ev.position, scale);
            menu.send(Command::ContextMenu { x, y });
        });
        if settings.show_minimap {
            root = root.children(minimap::render(&snap, &settings, &self.engine, scale));
        }
        root
    }
}

impl CanvasView {
    /// Frees the previous wallpaper's atlas space after the user picks a new one.
    fn forget_old_wallpaper(&mut self, window: &mut Window) {
        let now = crate::wallpaper::current();
        let same = match (&self.wallpaper, &now) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (a, b) => a.is_none() && b.is_none(),
        };
        if !same {
            if let Some(old) = self.wallpaper.take() {
                let _ = window.drop_image(old.image.clone());
            }
            self.wallpaper = now;
        }
    }
}

/// Sends `MouseUp` wherever the button is released: the overlay's own
/// handler misses releases over the search panel (it occludes the canvas)
/// and outside the window.
fn release_listener(pressed: Rc<Cell<bool>>, engine: EngineHandle) -> impl IntoElement {
    canvas(|_, _, _| (), move |_, _, window, _| {
        window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, _| {
            if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left && pressed.get() {
                pressed.set(false);
                engine.send(Command::MouseUp);
            }
        });
    })
    .absolute()
    .size_full()
}
