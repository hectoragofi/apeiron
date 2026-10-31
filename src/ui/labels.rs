//! Window labels (app icon + title above each window when zoomed out) and
//! the hover ring.
//!
//! Both are drawn outside the window's rectangle because the DWM thumbnail
//! covers everything GPUI draws inside it.

use gpui::*;

use super::canvas_view::win_bounds;
use super::theme;
use crate::icons;
use crate::settings::Settings;
use crate::shared::{Command, EngineHandle, Mode, Snapshot, WinInfo};

/// Labels appear below this zoom; closer in, title bars are readable.
const LABEL_ZOOM: f64 = 0.75;
const LABEL_H: f32 = 22.0;
/// Space between a label and the top of its window.
const LABEL_GAP: f32 = 4.0;
/// Height a label takes above its window, logical pixels.
pub const LABEL_ROOM: f32 = LABEL_H + LABEL_GAP;
/// Narrower windows get no label, and below `TEXT_MIN_W` only the icon.
const LABEL_MIN_W: f32 = 22.0;
const TEXT_MIN_W: f32 = 64.0;
/// Opacity of labels of windows that don't match the search.
const DIMMED: f32 = 0.3;

/// Labels above every window, when zoomed out far enough.
pub fn render(snap: &Snapshot, _settings: &Settings, engine: &EngineHandle, scale: f32) -> Vec<AnyElement> {
    let mut out = Vec::new();
    if !snap.overlay_visible || snap.zoom >= LABEL_ZOOM {
        return out;
    }
    let screen = size(px((snap.screen_w / scale as f64) as f32), px((snap.screen_h / scale as f64) as f32));
    let matching = snap.search.as_ref().map(|s| &s.results);
    // Bottom-most first, so where labels overlap the upper window's wins.
    for w in snap.windows.iter().rev() {
        let dim = matching.is_some_and(|m| !m.contains(&w.id));
        out.extend(label(snap, w, engine, scale, screen, dim));
    }
    out
}

/// The ring around the window a click would open, while browsing. Shown
/// whether or not labels are on; the selected search result has its own.
pub fn hover(snap: &Snapshot, scale: f32) -> Option<AnyElement> {
    let browsing = matches!(snap.mode, Mode::Overview | Mode::Search);
    let selected = snap.search.as_ref().and_then(|s| s.results.get(s.selected).copied());
    if !snap.overlay_visible || !browsing || snap.hovered == selected {
        return None;
    }
    let w = snap.windows.iter().find(|w| Some(w.id) == snap.hovered)?;
    Some(hover_ring(snap, w, scale))
}

fn label(
    snap: &Snapshot,
    w: &WinInfo,
    engine: &EngineHandle,
    scale: f32,
    screen: Size<Pixels>,
    dim: bool,
) -> Option<AnyElement> {
    let b = win_bounds(snap, w, scale);
    let width = f32::from(b.size.width);
    let top = b.origin.y - px(LABEL_H + LABEL_GAP);
    let off_screen = b.origin.x > screen.width
        || b.origin.x + b.size.width < px(0.)
        || top > screen.height
        || top + px(LABEL_H) < px(0.);
    if width < LABEL_MIN_W || off_screen {
        return None;
    }
    // Small windows get a slightly smaller label, so it never dwarfs them.
    let compact = width < 160.0;
    let icon_size = if compact { 14.0 } else { 16.0 };
    // On a pill of its own, so it reads over any wallpaper.
    let mut pill = div()
        .h_full()
        .max_w_full()
        .min_w_0()
        .flex()
        .items_center()
        .gap(px(6.))
        .px(px(if width >= TEXT_MIN_W { 6. } else { 3. }))
        .rounded(px(6.))
        .bg(theme::color_a(theme::LABEL_BG))
        .overflow_hidden()
        .child(app_icon(w, icon_size));
    if width >= TEXT_MIN_W {
        let color = if w.focused { theme::TEXT } else { theme::TEXT_DIM };
        pill = pill.child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(if compact { 11. } else { 12. }))
                .text_color(theme::color(color))
                .child(SharedString::from(w.title.clone())),
        );
    }
    let mut row = div().absolute().left(b.origin.x).top(top).w(b.size.width).h(px(LABEL_H)).flex().gap(px(4.)).child(pill);
    if width >= TEXT_MIN_W {
        row = row.child(div().flex_1()).child(close_button(w.id, engine));
    }
    if dim {
        row = row.opacity(DIMMED);
    }
    Some(row.into_any_element())
}

/// A soft ring just outside the window under the mouse.
fn hover_ring(snap: &Snapshot, w: &WinInfo, scale: f32) -> AnyElement {
    ring(snap, w, scale, theme::color_a(HOVER))
}

/// A 2 px ring hugging the window's visible edge from outside (the
/// thumbnail covers anything drawn inside it), its corners following the
/// window's own rounded ones.
fn ring(snap: &Snapshot, w: &WinInfo, scale: f32, color: Hsla) -> AnyElement {
    let b = win_bounds(snap, w, scale);
    let out = px(2.);
    let radius = px((8.0 * snap.zoom) as f32 / scale) + out;
    div()
        .absolute()
        .left(b.origin.x - out)
        .top(b.origin.y - out)
        .w(b.size.width + out * 2.)
        .h(b.size.height + out * 2.)
        .rounded(radius)
        .border_2()
        .border_color(color)
        .into_any_element()
}

/// Rings around the selected windows.
pub fn selection(snap: &Snapshot, scale: f32) -> Vec<AnyElement> {
    if !snap.overlay_visible {
        return Vec::new();
    }
    let color = theme::color(theme::ACCENT);
    snap.windows.iter().filter(|w| snap.selected.contains(&w.id)).map(|w| ring(snap, w, scale, color)).collect()
}

/// The outline of the size a window is being resized to.
pub fn resize_preview(snap: &Snapshot, scale: f32) -> Option<AnyElement> {
    let (l, t, r, b) = snap.resize_preview?;
    let at = |v: f64| px(v as f32 / scale);
    let out = px(2.);
    let radius = px((8.0 * snap.zoom) as f32 / scale) + out;
    Some(
        div()
            .absolute()
            .left(at(l) - out)
            .top(at(t) - out)
            .w(at(r - l) + out * 2.)
            .h(at(b - t) + out * 2.)
            .rounded(radius)
            .border_2()
            .border_color(theme::color(theme::ACCENT))
            .into_any_element(),
    )
}

/// The selection box being drawn.
pub fn marquee(snap: &Snapshot, scale: f32) -> Option<AnyElement> {
    let (l, t, r, b) = snap.marquee?;
    let at = |v: f64| px(v as f32 / scale);
    let mut fill = theme::color(theme::ACCENT);
    fill.a = 0.12;
    Some(
        div()
            .absolute()
            .left(at(l))
            .top(at(t))
            .w(at(r - l))
            .h(at(b - t))
            .rounded(px(3.))
            .bg(fill)
            .border_1()
            .border_color(theme::color(theme::ACCENT))
            .into_any_element(),
    )
}

/// The lines a dragged window is snapping to.
pub fn guides(snap: &Snapshot, scale: f32) -> Vec<AnyElement> {
    let at = |v: f64| px(v as f32 / scale);
    snap.guides
        .iter()
        .map(|&(x0, y0, x1, y1)| {
            let (l, t) = (x0.min(x1), y0.min(y1));
            div()
                .absolute()
                .left(at(l))
                .top(at(t))
                .w(at((x1 - x0).abs()).max(px(1.)))
                .h(at((y1 - y0).abs()).max(px(1.)))
                .bg(theme::color(theme::ACCENT))
                .into_any_element()
        })
        .collect()
}

/// A small x beside the label that closes the window (the app may still
/// ask to save first).
fn close_button(id: isize, engine: &EngineHandle) -> AnyElement {
    let engine = engine.clone();
    div()
        .id(("close", id as u64))
        .flex_shrink_0()
        .size(px(LABEL_H))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .bg(theme::color_a(theme::LABEL_BG))
        .text_size(px(12.))
        .text_color(theme::color(theme::TEXT_DIM))
        .hover(|s| s.bg(theme::color(CLOSE_HOVER)).text_color(theme::color(theme::TEXT)))
        .child("✕")
        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
            // Not a press on the canvas (that would start a selection box).
            cx.stop_propagation();
            engine.send(Command::CloseWindow(id));
        })
        .into_any_element()
}

/// Close button under the cursor: the system's close red.
const CLOSE_HOVER: u32 = 0xc42b1c;

/// Hover ring colour: white at low alpha.
const HOVER: u32 = 0xffffff60;

/// A window's app icon, or the initial of its app on a tile when it has none.
pub fn app_icon(w: &WinInfo, size: f32) -> AnyElement {
    if let Some(image) = icons::icon_for(w.id) {
        return img(image).size(px(size)).flex_shrink_0().into_any_element();
    }
    let name = if w.exe.is_empty() { &w.title } else { &w.exe };
    let initial = name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_default();
    div()
        .size(px(size))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(size / 4.))
        .bg(theme::color(theme::DOT))
        .text_size(px(size * 0.6))
        .text_color(theme::color(theme::TEXT))
        .child(SharedString::from(initial))
        .into_any_element()
}

/// Where minimized and hidden windows will come back: dashed outlines at
/// their spots, named, while zoomed out. Clicking one brings its window
/// back there; its ✕ forgets the spot.
pub fn away(snap: &Snapshot, engine: &EngineHandle, scale: f32) -> Vec<AnyElement> {
    if !snap.overlay_visible || snap.zoom >= 0.999 {
        return Vec::new();
    }
    let dim = theme::color(theme::TEXT_DIM);
    let text = theme::color(theme::TEXT);
    // Live windows drawn over a spot take the click there (GPUI can't see
    // the thumbnails, so it would hand it to the placeholder).
    let covers: std::rc::Rc<Vec<Bounds<Pixels>>> =
        std::rc::Rc::new(snap.windows.iter().map(|w| super::canvas_view::win_bounds(snap, w, scale)).collect());
    snap.away
        .iter()
        .map(|w| {
            let b = super::canvas_view::win_bounds(snap, w, scale);
            let name = if w.title.is_empty() { w.exe.clone() } else { w.title.clone() };
            let (id, restore, forget) = (w.id, engine.clone(), engine.clone());
            let (covers, covers_x) = (covers.clone(), covers.clone());
            // Too small for a button: a click still restores it.
            let roomy = f32::from(b.size.width) >= 48.0 && f32::from(b.size.height) >= 36.0;
            let mut spot = div()
                .id(("away", id as u64))
                .absolute()
                .left(b.origin.x)
                .top(b.origin.y)
                .w(b.size.width)
                .h(b.size.height)
                .rounded(px((8.0 * snap.zoom) as f32 / scale).max(px(3.)))
                .border_1()
                .border_dashed()
                .border_color(dim.opacity(0.7))
                .bg(dim.opacity(0.06))
                .hover(|s| s.bg(text.opacity(0.12)).border_color(text.opacity(0.9)))
                .cursor_pointer()
                .flex()
                .items_center()
                .justify_center()
                .overflow_hidden()
                .p(px(6.))
                .on_mouse_down(MouseButton::Left, move |ev, _, cx| {
                    if covers.iter().any(|c| c.contains(&ev.position)) {
                        return;
                    }
                    // Not a press on the canvas (that would start a selection box).
                    cx.stop_propagation();
                    restore.send(Command::FocusWindow(id));
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .min_w_0()
                        .text_size(px(12.))
                        .text_color(dim)
                        .child(div().max_w_full().truncate().child(SharedString::from(name)))
                        .child(div().text_size(px(11.)).opacity(0.7).child("Minimized · click to restore")),
                );
            if roomy {
                spot = spot.child(
                    div()
                        .id(("forget", id as u64))
                        .absolute()
                        .top(px(4.))
                        .right(px(4.))
                        .size(px(20.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(5.))
                        .bg(theme::color_a(theme::LABEL_BG))
                        .text_size(px(11.))
                        .text_color(dim)
                        .hover(|s| s.bg(theme::color(theme::ACCENT)).text_color(text))
                        .child("✕")
                        .on_mouse_down(MouseButton::Left, move |ev, _, cx| {
                            if covers_x.iter().any(|c| c.contains(&ev.position)) {
                                return;
                            }
                            cx.stop_propagation();
                            forget.send(Command::ForgetAway(id));
                        }),
                );
            }
            spot.into_any_element()
        })
        .collect()
}

/// A dashed accent outline around each group of windows stuck together.
pub fn sticks(snap: &Snapshot, scale: f32) -> Vec<AnyElement> {
    if !snap.overlay_visible || snap.zoom >= 0.999 {
        return Vec::new();
    }
    let accent = theme::color(theme::ACCENT);
    snap.sticks
        .iter()
        .filter_map(|g| {
            let rects: Vec<_> = snap
                .windows
                .iter()
                .filter(|w| g.contains(&w.id))
                .map(|w| super::canvas_view::win_bounds(snap, w, scale))
                .collect();
            if rects.len() < 2 {
                return None;
            }
            let l = rects.iter().map(|b| b.origin.x).fold(px(f32::MAX), |a, b| a.min(b));
            let t = rects.iter().map(|b| b.origin.y).fold(px(f32::MAX), |a, b| a.min(b));
            let r = rects.iter().map(|b| b.origin.x + b.size.width).fold(px(f32::MIN), |a, b| a.max(b));
            let bot = rects.iter().map(|b| b.origin.y + b.size.height).fold(px(f32::MIN), |a, b| a.max(b));
            let out = px(8.);
            Some(
                div()
                    .absolute()
                    .left(l - out)
                    .top(t - out)
                    .w(r - l + out * 2.)
                    .h(bot - t + out * 2.)
                    .rounded(px(10.))
                    .border_2()
                    .border_dashed()
                    .border_color(accent.opacity(0.8))
                    .into_any_element(),
            )
        })
        .collect()
}
