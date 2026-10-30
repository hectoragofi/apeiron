//! The view behind both fullscreen windows: the dot grid, plus (on the
//! overlay) labels, the hover ring and search, and the minimap on either.
//!
//! The overlay forwards mouse and wheel input to the engine as `Command`s in
//! physical pixels.

use gpui::*;

use super::{labels, minimap, search, theme};
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
}

impl CanvasView {
    pub fn new(shared: Shared, engine: EngineHandle, role: Role, cx: &mut Context<Self>) -> Self {
        CanvasView { shared, engine, role, focus: cx.focus_handle() }
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
        let snap = self.shared.snap();
        let (cam_x, cam_y, zoom) = (snap.cam_x, snap.cam_y, snap.zoom);
        let grid = settings.show_grid;

        let mut root = div()
            .size_full()
            .relative()
            .bg(theme::color(theme::BG))
            .child(canvas(|_, _, _| (), move |bounds, _, window, _| {
                if grid {
                    paint_grid(bounds, window, cam_x, cam_y, zoom, scale);
                }
            })
            .absolute()
            .size_full());

        if self.role == Role::Overlay {
            let engine = self.engine.clone();
            let down = engine.clone();
            let moved = engine.clone();
            let up = engine.clone();
            let keys = engine.clone();
            root = root
                .track_focus(&self.focus)
                .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, _, _| {
                    let (x, y) = phys(ev.position, scale);
                    down.send(Command::MouseDown { x, y });
                })
                .on_mouse_move(move |ev: &MouseMoveEvent, _, _| {
                    let (x, y) = phys(ev.position, scale);
                    moved.send(Command::MouseMove { x, y });
                })
                .on_mouse_up(MouseButton::Left, move |_: &MouseUpEvent, _, _| up.send(Command::MouseUp))
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
                .on_key_down(move |ev: &KeyDownEvent, _, _| search::on_key(ev, &keys));
            crate::icons::prune(&snap.windows, window);
            if settings.show_labels {
                root = root.children(labels::render(&snap, &settings, scale));
            }
            root = root.children(search::render(&snap, &self.engine, scale));
        }
        if settings.show_minimap {
            root = root.children(minimap::render(&snap, &settings, &self.engine, scale));
        }
        root
    }
}

/// Dot grid that moves with the camera, so empty canvas still reads as motion.
fn paint_grid(bounds: Bounds<Pixels>, window: &mut Window, cam_x: f64, cam_y: f64, zoom: f64, scale: f32) {
    let mut step = theme::GRID_STEP * zoom;
    while step < theme::GRID_MIN_STEP {
        step *= 2.0;
    }
    let s = scale as f64;
    let (w, h) = (f32::from(bounds.size.width) as f64 * s, f32::from(bounds.size.height) as f64 * s);
    let ox = (-cam_x * zoom).rem_euclid(step);
    let oy = (-cam_y * zoom).rem_euclid(step);
    let dot = theme::color(theme::DOT);
    let dot_size = px((theme::DOT_SIZE / s) as f32);
    let mut y = oy;
    while y < h {
        let mut x = ox;
        while x < w {
            let origin = bounds.origin + point(px((x.floor() / s) as f32), px((y.floor() / s) as f32));
            window.paint_quad(fill(Bounds::new(origin, size(dot_size, dot_size)), dot));
            x += step;
        }
        y += step;
    }
}
