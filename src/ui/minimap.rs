//! The minimap in the top-right corner: every window as a small rect plus
//! the camera's viewport, following the canvas live. Clicking or dragging
//! in it flies the camera there.

use std::cell::Cell;

use gpui::*;

use super::theme;
use crate::settings::Settings;
use crate::shared::{Command, EngineHandle, Snapshot};

/// Panel size and distance from the screen corner, logical pixels.
const WIDTH: f32 = 192.0;
const HEIGHT: f32 = 120.0;
const INSET: f32 = 12.0;
/// Space between the panel edge and the map.
const PAD: f64 = 8.0;
/// Canvas rect: x, y, w, h.
type Rect = (f64, f64, f64, f64);

/// Canvas coordinates to logical pixels from the map's top-left.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Map {
    x0: f64,
    y0: f64,
    k: f64,
    ox: f64,
    oy: f64,
}

impl Map {
    /// Fits every rect into a `w` x `h` area, centred, inside the padding.
    fn fit(rects: impl IntoIterator<Item = Rect>, w: f64, h: f64) -> Option<Map> {
        let (x0, y0, x1, y1) = rects
            .into_iter()
            .map(|(x, y, w, h)| (x, y, x + w, y + h))
            .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))?;
        let (bw, bh) = ((x1 - x0).max(1.0), (y1 - y0).max(1.0));
        let (iw, ih) = ((w - PAD * 2.0).max(1.0), (h - PAD * 2.0).max(1.0));
        let k = (iw / bw).min(ih / bh);
        Some(Map { x0, y0, k, ox: PAD + (iw - bw * k) / 2.0, oy: PAD + (ih - bh * k) / 2.0 })
    }

    fn to_map(self, x: f64, y: f64) -> (f64, f64) {
        (self.ox + (x - self.x0) * self.k, self.oy + (y - self.y0) * self.k)
    }

    fn to_canvas(self, x: f64, y: f64) -> (f64, f64) {
        (self.x0 + (x - self.ox) / self.k, self.y0 + (y - self.oy) / self.k)
    }

    /// Where a canvas rect lands on screen, at least `min` across.
    fn bounds(self, origin: Point<Pixels>, (x, y, w, h): Rect, min: f64) -> Bounds<Pixels> {
        let (l, t) = self.to_map(x, y);
        let (w, h) = ((w * self.k).max(min), (h * self.k).max(min));
        Bounds::new(origin + point(px(l as f32), px(t as f32)), size(px(w as f32), px(h as f32)))
    }
}

thread_local! {
    /// The map as it was when a drag in the minimap started. Holding it
    /// still keeps the point under the cursor from sliding as the viewport
    /// (part of the fitted area) moves.
    static DRAG: Cell<Option<Map>> = const { Cell::new(None) };
}

/// The panel's screen rectangle `(left, top, right, bottom)` in physical
/// pixels, on a screen `screen_w` wide. DWM thumbnails would cover it, so
/// the engine keeps them out of it.
pub fn panel_rect(screen_w: f64, scale: f64) -> (f64, f64, f64, f64) {
    let (w, h, inset) = (WIDTH as f64 * scale, HEIGHT as f64 * scale, INSET as f64 * scale);
    (screen_w - inset - w, inset, screen_w - inset, inset + h)
}

pub fn render(snap: &Snapshot, _settings: &Settings, engine: &EngineHandle, _scale: f32) -> Option<AnyElement> {
    if (snap.windows.is_empty() && snap.away.is_empty()) || snap.zoom <= 0.0 {
        return None;
    }
    // Bottom-most first, so the topmost window is painted last.
    let wins: Vec<(Rect, bool)> = snap.windows.iter().rev().map(|w| ((w.x, w.y, w.w, w.h), w.focused)).collect();
    // Minimized and hidden windows, dashed, keeping their spot.
    let away: Vec<Rect> = snap.away.iter().map(|w| (w.x, w.y, w.w, w.h)).collect();
    let view: Rect = (snap.cam_x, snap.cam_y, snap.screen_w / snap.zoom, snap.screen_h / snap.zoom);
    let engine = engine.clone();
    let panel = div()
        .absolute()
        .right(px(INSET))
        .top(px(INSET))
        .w(px(WIDTH))
        .h(px(HEIGHT))
        .rounded(px(10.0))
        .bg(theme::color_a(theme::PANEL))
        .border_1()
        .border_color(theme::color_a(theme::BORDER))
        .child(
            canvas(|_, _, _| (), move |bounds, _, window, _| paint(bounds, window, &wins, &away, view, engine))
                .size_full(),
        );
    Some(panel.into_any_element())
}

fn paint(
    bounds: Bounds<Pixels>,
    window: &mut Window,
    wins: &[(Rect, bool)],
    away: &[Rect],
    view: Rect,
    engine: EngineHandle,
) {
    let (w, h) = (f32::from(bounds.size.width) as f64, f32::from(bounds.size.height) as f64);
    let fitted = Map::fit(wins.iter().map(|&(r, _)| r).chain(away.iter().copied()).chain([view]), w, h);
    let Some(map) = DRAG.get().or(fitted) else { return };

    let text = theme::color(theme::TEXT);
    let accent = theme::color(theme::ACCENT);
    for &r in away {
        let b = map.bounds(bounds.origin, r, 3.0);
        window.paint_quad(quad(b, px(2.0), text.opacity(0.04), px(1.0), text.opacity(0.45), BorderStyle::Dashed));
    }
    for &(r, focused) in wins {
        let fill = if focused { accent.opacity(0.45) } else { text.opacity(0.16) };
        let edge = if focused { accent } else { text.opacity(0.32) };
        window.paint_quad(quad(map.bounds(bounds.origin, r, 2.0), px(2.0), fill, px(1.0), edge, BorderStyle::Solid));
    }
    window.paint_quad(quad(
        map.bounds(bounds.origin, view, 4.0),
        px(3.0),
        accent.opacity(0.08),
        px(1.5),
        accent.opacity(0.9),
        BorderStyle::Solid,
    ));

    // Window-level listeners so a drag keeps steering after it leaves the
    // panel. They run before the overlay's own handlers, which never see a
    // press that lands here.
    let fly = move |engine: &EngineHandle, map: Map, p: Point<Pixels>| {
        let local = p - bounds.origin;
        let (x, y) = map.to_canvas(f32::from(local.x) as f64, f32::from(local.y) as f64);
        engine.send(Command::FlyTo { x, y });
    };
    let down = engine.clone();
    window.on_mouse_event(move |ev: &MouseDownEvent, phase, _, cx| {
        if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left && bounds.contains(&ev.position) {
            DRAG.set(Some(map));
            fly(&down, map, ev.position);
            cx.stop_propagation();
        }
    });
    window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, _| {
        if phase != DispatchPhase::Bubble {
            return;
        }
        match DRAG.get() {
            Some(m) if ev.dragging() => fly(&engine, m, ev.position),
            // The button came up somewhere we didn't hear about.
            Some(_) => DRAG.set(None),
            None => {}
        }
    });
    window.on_mouse_event(|ev: &MouseUpEvent, phase, _, _| {
        if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left {
            DRAG.set(None);
        }
    });
}

#[cfg(test)]
mod tests {
    // Not a glob: gpui exports its own `test` attribute.
    use super::{Map, PAD};

    #[test]
    fn fit_contains_everything_and_round_trips() {
        let rects = [(0.0, 0.0, 1600.0, 900.0), (-3000.0, 2000.0, 800.0, 600.0), (5000.0, -400.0, 1920.0, 1080.0)];
        let (w, h) = (218.0, 138.0);
        let map = Map::fit(rects, w, h).unwrap();
        for (x, y, rw, rh) in rects {
            let (l, t) = map.to_map(x, y);
            let (r, b) = map.to_map(x + rw, y + rh);
            assert!(l >= PAD - 1e-9 && t >= PAD - 1e-9 && r <= w - PAD + 1e-9 && b <= h - PAD + 1e-9);
        }
        let (x, y) = map.to_canvas(100.0, 70.0);
        let (mx, my) = map.to_map(x, y);
        assert!((mx - 100.0).abs() < 1e-9 && (my - 70.0).abs() < 1e-9);
    }

    #[test]
    fn panel_sits_in_the_top_right_corner() {
        let (l, t, r, b) = super::panel_rect(1920.0, 1.5);
        assert_eq!((r, t), (1920.0 - 18.0, 18.0));
        assert_eq!((r - l, b - t), (288.0, 180.0));
    }

    #[test]
    fn fit_needs_something() {
        assert_eq!(Map::fit([], 200.0, 100.0), None);
    }
}
