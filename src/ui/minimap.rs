//! The minimap in the top-right corner: every window as a small rect plus
//! the camera's viewport, following the canvas live. Clicking a window in it
//! focuses that window (bringing back a minimized one); clicking empty space
//! or dragging flies the camera there.
//!
//! Each window carries its app icon, centred, when there's room. Hovering
//! one names it under the panel and lights it up, as does hovering its
//! window on the canvas; selected windows are outlined.
//!
//! DWM thumbnails draw over everything in the overlay, so over the overlay
//! the map is a small transparent window of its own, which windows show
//! through. On the desktop it's simply part of the background.

use std::cell::Cell;

use gpui::*;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{CreateRectRgn, SetWindowRgn};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::theme;
use crate::icons;
use crate::settings::Settings;
use crate::shared::{Command, EngineHandle, Shared, Snapshot};

/// Panel size and distance from the screen corner, logical pixels.
const WIDTH: f32 = 192.0;
const HEIGHT: f32 = 120.0;
const INSET: f32 = 12.0;
/// Behind the map, RGBA: see-through, but dark enough for the map to read
/// over a bright window.
const GLASS: u32 = 0x1c1d2299;
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
    /// A press on a window in the map: the window, the map then, and where
    /// it went down. It becomes a drag once the mouse moves far enough.
    static PRESS: Cell<Option<(isize, Map, Point<Pixels>)>> = const { Cell::new(None) };
    /// The window under the mouse in the map.
    static HOVER: Cell<Option<isize>> = const { Cell::new(None) };
}

/// App icons: at most this big (logical pixels), this much of the smaller
/// side of a window's box, and left out below this size.
const ICON_MAX: f64 = 22.0;
const ICON_SHARE: f64 = 0.6;
const ICON_MIN: f64 = 7.0;

/// How far the mouse moves (logical pixels) before a press on a window
/// turns into a drag of the camera.
const DRAG_SLOP: f32 = 4.0;

/// The panel's screen rectangle `(left, top, right, bottom)` in physical
/// pixels, on a screen `screen_w` wide. DWM thumbnails would cover it, so
/// the engine keeps them out of it.
pub fn panel_rect(screen_w: f64, scale: f64) -> (f64, f64, f64, f64) {
    let (w, h, inset) = (WIDTH as f64 * scale, HEIGHT as f64 * scale, INSET as f64 * scale);
    (screen_w - inset - w, inset, screen_w - inset, inset + h)
}

/// Whether there's anything to map.
fn has_content(snap: &Snapshot) -> bool {
    (!snap.windows.is_empty() || !snap.away.is_empty()) && snap.zoom > 0.0
}

/// The map as part of a fullscreen view, in the screen's corner.
pub fn render(snap: &Snapshot, _settings: &Settings, engine: &EngineHandle, _scale: f32) -> Option<AnyElement> {
    let panel = panel(snap, engine)?.absolute().right(px(INSET)).top(px(INSET)).w(px(WIDTH)).h(px(HEIGHT));
    Some(panel.into_any_element())
}

/// The panel and the map in it, for the caller to place and size.
fn panel(snap: &Snapshot, engine: &EngineHandle) -> Option<Div> {
    if !has_content(snap) {
        return None;
    }
    let in_map = HOVER.get();
    let lit = |id: isize| in_map == Some(id) || (in_map.is_none() && snap.hovered == Some(id));
    // Bottom-most first, so the topmost window is painted last.
    let wins: Vec<MapWin> = snap
        .windows
        .iter()
        .rev()
        .map(|w| MapWin {
            rect: (w.x, w.y, w.w, w.h),
            id: w.id,
            focused: w.focused,
            lit: lit(w.id),
            selected: snap.selected.contains(&w.id),
        })
        .collect();
    // Minimized and hidden windows, dashed, keeping their spot.
    let away: Vec<MapWin> = snap
        .away
        .iter()
        .map(|w| MapWin { rect: (w.x, w.y, w.w, w.h), id: w.id, focused: false, lit: lit(w.id), selected: false })
        .collect();
    let view: Rect = (snap.cam_x, snap.cam_y, snap.screen_w / snap.zoom, snap.screen_h / snap.zoom);
    let engine = engine.clone();
    // The hovered window's name inside the panel (outside it, window
    // thumbnails would cover it), at the edge away from that window.
    let fitted = Map::fit(wins.iter().chain(&away).map(|v| v.rect).chain([view]), WIDTH as f64, HEIGHT as f64);
    let tip = in_map
        .and_then(|id| snap.windows.iter().chain(&snap.away).find(|w| w.id == id))
        .filter(|w| !w.title.is_empty() || !w.exe.is_empty())
        .map(|w| {
            let t = if w.title.is_empty() { w.exe.clone() } else { w.title.clone() };
            let low = fitted.is_some_and(|m| m.to_map(w.x + w.w / 2.0, w.y + w.h / 2.0).1 > HEIGHT as f64 / 2.0);
            let tip = div().absolute().left(px(4.0));
            let tip = if low { tip.top(px(4.0)) } else { tip.bottom(px(4.0)) };
            tip
                .max_w(px(WIDTH - 10.0))
                .px(px(8.0))
                .py(px(3.0))
                .rounded(px(6.0))
                .bg(theme::color_a(theme::PANEL))
                .border_1()
                .border_color(theme::color_a(theme::BORDER))
                .text_size(px(12.0))
                .text_color(theme::color(theme::TEXT))
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .child(SharedString::from(t))
        });
    let panel = div()
        .rounded(px(10.0))
        .bg(theme::color_a(GLASS))
        .border_1()
        .border_color(theme::color_a(theme::BORDER))
        .child(
            canvas(|_, _, _| (), move |bounds, _, window, _| paint(bounds, window, &wins, &away, view, engine))
                .size_full(),
        )
        .children(tip);
    Some(panel)
}

/// The map's own window, over the overlay.
pub struct MapView {
    shared: Shared,
    engine: EngineHandle,
}

impl Render for MapView {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        icons::free_gone(icons::Atlas::Map, window);
        // The mouse left: no more moves will come here to say so.
        if !window.is_window_hovered() {
            HOVER.set(None);
        }
        let snap = self.shared.snap();
        let scale = window.scale_factor();
        let (l, t, ..) = panel_rect(snap.screen_w, scale as f64);
        div()
            .size_full()
            .relative()
            .child(forward_input(self.engine.clone(), scale, (l, t)))
            .children(panel(&snap, &self.engine).map(|p| p.size_full()))
    }
}

/// Hands the engine what the overlay would have, had the mouse been over it
/// instead of this window: moves, right clicks, middle drags and the wheel.
/// `origin` is the window's place on screen in physical pixels.
fn forward_input(engine: EngineHandle, scale: f32, origin: (f64, f64)) -> impl IntoElement {
    let at = move |p: Point<Pixels>| {
        (f32::from(p.x) as f64 * scale as f64 + origin.0, f32::from(p.y) as f64 * scale as f64 + origin.1)
    };
    canvas(|_, _, _| (), move |_, _, window, _| {
        let (moved, down, up, wheel) = (engine.clone(), engine.clone(), engine.clone(), engine);
        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, _| {
            if phase == DispatchPhase::Bubble {
                let (x, y) = at(ev.position);
                moved.send(Command::MouseMove { x, y });
            }
        });
        window.on_mouse_event(move |ev: &MouseDownEvent, phase, _, _| {
            if phase == DispatchPhase::Bubble {
                let (x, y) = at(ev.position);
                match ev.button {
                    MouseButton::Right => down.send(Command::ContextMenu { x, y }),
                    MouseButton::Middle => down.send(Command::PanStart { x, y }),
                    _ => {}
                }
            }
        });
        window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, _| {
            if phase == DispatchPhase::Bubble && ev.button == MouseButton::Middle {
                up.send(Command::MouseUp);
            }
        });
        window.on_mouse_event(move |ev: &ScrollWheelEvent, phase, _, _| {
            if phase == DispatchPhase::Bubble {
                wheel.send(super::canvas_view::wheel_command(ev, scale, origin));
            }
        });
    })
    .absolute()
    .size_full()
}

pub struct MapWindow {
    pub handle: WindowHandle<MapView>,
    pub hwnd: isize,
    /// Rect it was put at, in this thread's coordinates.
    placed: Option<(i32, i32, i32, i32)>,
    shown: bool,
}

pub fn open(cx: &mut App, shared: &Shared, engine: &EngineHandle, overlay: isize) -> Option<MapWindow> {
    let bounds = Bounds::new(point(px(0.), px(0.)), size(px(WIDTH), px(HEIGHT)));
    let (shared, engine) = (shared.clone(), engine.clone());
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                kind: WindowKind::PopUp,
                focus: false,
                show: false,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            move |_, cx| cx.new(|_| MapView { shared, engine }),
        )
        .map_err(|e| log!("couldn't open the minimap window: {e}"))
        .ok()?;
    let hwnd = handle
        .update(cx, |_, window, cx| {
            window.on_window_should_close(cx, |_, _| false);
            super::hwnd_of(window)
        })
        .ok()??;
    let h = HWND(hwnd as *mut _);
    super::strips::restyle(h);
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
        SetWindowLongPtrW(h, GWL_EXSTYLE, (ex | WS_EX_TOPMOST.0) as isize);
        // An owned window stays above its owner, however often that's raised.
        SetWindowLongPtrW(h, GWLP_HWNDPARENT, overlay);
    }
    Some(MapWindow { handle, hwnd, placed: None, shown: false })
}

impl MapWindow {
    /// Shows it in the corner while the overlay is up, hides it otherwise,
    /// and returns whether it's up. Runs outside any GPUI update (moving a
    /// window re-enters GPUI).
    pub fn place(&mut self, snap: &Snapshot, enabled: bool) -> bool {
        let hwnd = HWND(self.hwnd as *mut _);
        if !enabled || !snap.overlay_visible || !has_content(snap) {
            if self.shown {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                self.shown = false;
                HOVER.set(None);
            }
            return false;
        }
        // This thread may see DPI-scaled coordinates; the engine's are physical.
        let k = super::screen_size().0 as f64 / snap.screen_w.max(1.0);
        let (l, t, r, b) = panel_rect(snap.screen_w, super::primary_dpi() as f64 / 96.0);
        let rect = ((l * k).round() as i32, (t * k).round() as i32, ((r - l) * k).round() as i32, ((b - t) * k).round() as i32);
        unsafe {
            if self.placed != Some(rect) {
                let (il, it) = super::place(hwnd, HWND_TOPMOST, rect.0, rect.1, rect.2, rect.3);
                // Only the panel shows: the frame GPUI keeps around its
                // client area is cut away.
                let rgn = CreateRectRgn(il, it, il + rect.2, it + rect.3);
                let _ = SetWindowRgn(hwnd, Some(rgn), true);
                self.placed = Some(rect);
            }
            if !self.shown {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                self.shown = true;
            }
        }
        true
    }
}

/// A window as the map draws it.
struct MapWin {
    rect: Rect,
    id: isize,
    focused: bool,
    /// Hovered, here or on the canvas.
    lit: bool,
    selected: bool,
}

/// The app icon centred in a window's box, if there's room for it.
fn paint_icon(window: &mut Window, b: Bounds<Pixels>, id: isize, dim: bool) {
    let (w, h) = (f32::from(b.size.width) as f64, f32::from(b.size.height) as f64);
    let s = (w.min(h) * ICON_SHARE).min(ICON_MAX);
    if s < ICON_MIN {
        return;
    }
    let Some(image) = icons::icon_for(id) else { return };
    let s = px(s as f32);
    let at = b.center() - point(s / 2.0, s / 2.0);
    let _ = window.paint_image(Bounds::new(at, size(s, s)), Corners::default(), image, 0, dim);
}

fn paint(bounds: Bounds<Pixels>, window: &mut Window, wins: &[MapWin], away: &[MapWin], view: Rect, engine: EngineHandle) {
    let (w, h) = (f32::from(bounds.size.width) as f64, f32::from(bounds.size.height) as f64);
    let fitted = Map::fit(wins.iter().chain(away).map(|v| v.rect).chain([view]), w, h);
    let Some(map) = DRAG.get().or(fitted) else { return };

    let text = theme::color(theme::TEXT);
    let accent = theme::color(theme::ACCENT);
    for a in away {
        let b = map.bounds(bounds.origin, a.rect, 3.0);
        let (fill, edge) = if a.lit { (0.12, 0.8) } else { (0.04, 0.45) };
        window.paint_quad(quad(b, px(2.0), text.opacity(fill), px(1.0), text.opacity(edge), BorderStyle::Dashed));
        paint_icon(window, b, a.id, true);
    }
    for v in wins {
        let b = map.bounds(bounds.origin, v.rect, 2.0);
        let fill = match (v.focused, v.lit) {
            (true, lit) => accent.opacity(if lit { 0.6 } else { 0.45 }),
            (false, true) => text.opacity(0.3),
            (false, false) => text.opacity(0.16),
        };
        let edge = if v.focused || v.selected {
            accent
        } else if v.lit {
            text.opacity(0.85)
        } else {
            text.opacity(0.32)
        };
        let width = if v.selected || v.lit { 1.5 } else { 1.0 };
        window.paint_quad(quad(b, px(2.0), fill, px(width), edge, BorderStyle::Solid));
        paint_icon(window, b, v.id, false);
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
    // Topmost window first, then the minimized ones under them; tiny ones
    // are as big to the mouse as they're drawn.
    let hits: Vec<(Bounds<Pixels>, isize)> = wins
        .iter()
        .rev()
        .map(|v| (map.bounds(bounds.origin, v.rect, 2.0), v.id))
        .chain(away.iter().map(|a| (map.bounds(bounds.origin, a.rect, 3.0), a.id)))
        .collect();
    let hover_hits = hits.clone();
    let (down, moved) = (engine.clone(), engine.clone());
    window.on_mouse_event(move |ev: &MouseDownEvent, phase, _, cx| {
        if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left && bounds.contains(&ev.position) {
            match hits.iter().find(|(b, _)| b.contains(&ev.position)) {
                Some(&(_, id)) => PRESS.set(Some((id, map, ev.position))),
                None => {
                    DRAG.set(Some(map));
                    fly(&down, map, ev.position);
                }
            }
            cx.stop_propagation();
        }
    });
    window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, _| {
        if phase != DispatchPhase::Bubble {
            return;
        }
        let over = (bounds.contains(&ev.position) && DRAG.get().is_none())
            .then(|| hover_hits.iter().find(|(b, _)| b.contains(&ev.position)).map(|&(_, id)| id))
            .flatten();
        if HOVER.replace(over) != over {
            window.refresh();
        }
        if let Some((_, m, at)) = PRESS.get() {
            if !ev.dragging() {
                PRESS.set(None);
            } else if (ev.position - at).magnitude() > DRAG_SLOP as f64 {
                PRESS.set(None);
                DRAG.set(Some(m));
            }
        }
        match DRAG.get() {
            Some(m) if ev.dragging() => fly(&moved, m, ev.position),
            // The button came up somewhere we didn't hear about.
            Some(_) => DRAG.set(None),
            None => {}
        }
    });
    window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, _| {
        if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left {
            DRAG.set(None);
            if let Some((id, ..)) = PRESS.take() {
                engine.send(Command::FocusWindow(id));
            }
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
