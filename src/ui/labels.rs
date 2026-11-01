//! Window labels (app icon + title above each window when zoomed out) and
//! the hover ring.
//!
//! Both are drawn outside the window's rectangle because the DWM thumbnail
//! covers everything GPUI draws inside it.

use gpui::*;

use super::canvas_view::win_bounds;
use super::motion::{self, fade, rise, FAST, NORMAL};
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
        // A tab group's strip stands in for its label.
        if snap.tabs.iter().any(|t| t.active == w.id) {
            continue;
        }
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
    Some(fade(hover_ring(snap, w, scale), ("hover", w.id as u64), FAST, 1.).into_any_element())
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
    let to = if dim { DIMMED } else { 1. };
    Some(rise(row, ("label", w.id as u64), NORMAL, top, 4., to).into_any_element())
}

/// A soft ring just outside the window under the mouse.
fn hover_ring(snap: &Snapshot, w: &WinInfo, scale: f32) -> Div {
    ring(snap, w, scale, theme::color_a(HOVER))
}

/// A 2 px ring hugging the window's visible edge from outside (the
/// thumbnail covers anything drawn inside it), its corners following the
/// window's own rounded ones.
fn ring(snap: &Snapshot, w: &WinInfo, scale: f32, color: Hsla) -> Div {
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
}

/// Rings around the selected windows.
pub fn selection(snap: &Snapshot, scale: f32) -> Vec<AnyElement> {
    if !snap.overlay_visible {
        return Vec::new();
    }
    let color = theme::color(theme::ACCENT);
    snap.windows
        .iter()
        .filter(|w| snap.selected.contains(&w.id))
        .map(|w| fade(ring(snap, w, scale, color), ("sel", w.id as u64), FAST, 1.).into_any_element())
        .collect()
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
            fade(spot, ("awayfade", id as u64), NORMAL, 1.).into_any_element()
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

/// The tab strip floats this far above its window, logical pixels.
pub const STRIP_GAP: f32 = 6.0;
pub const STRIP_H: f32 = 26.0;
/// Room inside the capsule around the tabs.
const STRIP_PAD: f32 = 3.0;
/// Below this width per tab only icons show.
const TAB_TEXT_MIN_W: f32 = 76.0;
/// The capsule: frosted dark glass with a hairline edge.
const GLASS: u32 = 0x1e1f24d6;
const HAIRLINE: u32 = 0xffffff1f;
/// The shown tab: a raised, lighter segment.
const SEGMENT: u32 = 0xffffff26;
const SEGMENT_HOVER: u32 = 0xffffff10;
const SEPARATOR: u32 = 0xffffff1a;
/// System blue, as macOS draws it on dark.
const BLUE: u32 = 0x0a84ff;

/// Where a group's tab strip goes: floating just above its spot, as wide
/// as it. None when it would be off screen or too narrow.
fn strip_bounds(snap: &Snapshot, spot: (f64, f64, f64, f64), scale: f32) -> Option<Bounds<Pixels>> {
    let (sw, sh) = ((snap.screen_w / scale as f64) as f32, (snap.screen_h / scale as f64) as f32);
    let s = snap.zoom / scale as f64;
    let (x, y, w) = (((spot.0 - snap.cam_x) * s) as f32, ((spot.1 - snap.cam_y) * s) as f32, (spot.2 * s) as f32);
    let top = y - STRIP_GAP - STRIP_H;
    let off = x > sw || x + w < 0. || top > sh || top + STRIP_H < 0.;
    (!off && w >= LABEL_MIN_W).then(|| Bounds::new(point(px(x), px(top)), size(px(w), px(STRIP_H))))
}

/// The tab strip above each tab group, at any zoom, on the desktop and the
/// overlay alike: a frosted capsule of equal segments, the shown tab (both,
/// when split) raised. Click to show, drag to reorder, split or take out,
/// right click for more.
pub fn tabs(snap: &Snapshot, engine: &EngineHandle, scale: f32) -> Vec<AnyElement> {
    let matching = snap.search.as_ref().map(|s| &s.results);
    let mut out = Vec::new();
    for group in &snap.tabs {
        if !snap.windows.iter().any(|w| w.id == group.active) || group.tabs.is_empty() {
            continue;
        }
        let Some(b) = strip_bounds(snap, group.spot, scale) else { continue };
        let bar = strip_bar(snap, group, f32::from(b.size.width), engine, scale, (0., 0.))
            .absolute()
            .left(b.origin.x)
            .top(b.origin.y)
            .w(b.size.width)
            .h(b.size.height);
        let to = if matching.is_some_and(|m| !m.contains(&group.active)) { DIMMED } else { 1. };
        // Keyed by the group (its first window), not the shown tab, so a
        // switch doesn't replay it.
        out.push(rise(bar, ("strip", group_key(group)), NORMAL, b.origin.y, 6., to).into_any_element());
    }
    out
}

/// A tab group's identity for element ids: its first window.
pub fn group_key(group: &crate::shared::TabsSnap) -> u64 {
    group.tabs.iter().map(|t| t.id).min().unwrap_or(0) as u64
}

/// The capsule of tabs itself, `width` logical pixels wide, unplaced.
/// `origin` is where it sits on screen in physical pixels (its own window,
/// or 0 on the fullscreen ones), so clicks report screen points.
pub fn strip_bar(
    snap: &Snapshot,
    group: &crate::shared::TabsSnap,
    width: f32,
    engine: &EngineHandle,
    scale: f32,
    origin: (f64, f64),
) -> Div {
    let dragged = snap.tab_drag.as_ref().map(|d| d.id);
    let n = group.tabs.len().max(1) as f32;
    let text = (width - STRIP_PAD * 2.) / n >= TAB_TEXT_MIN_W;
    let shown = |id: isize| id == group.active || Some(id) == group.split;
    let mut bar = div()
        .p(px(STRIP_PAD))
        .flex()
        .items_center()
        .rounded_full()
        .bg(theme::color_a(GLASS))
        .border_1()
        .border_color(theme::color_a(HAIRLINE))
        .shadow_md()
        .overflow_hidden();
    for (k, t) in group.tabs.iter().enumerate() {
        // Hairlines between segments, except beside a raised one.
        if k > 0 && !shown(t.id) && !shown(group.tabs[k - 1].id) {
            bar = bar.child(div().flex_shrink_0().w(px(1.)).h(px(12.)).bg(theme::color_a(SEPARATOR)));
        } else if k > 0 {
            bar = bar.child(div().flex_shrink_0().w(px(1.)));
        }
        let faded = dragged == Some(t.id);
        bar = bar.child(tab(t, shown(t.id), t.id == group.active, text, faded, engine, scale, origin));
    }
    bar
}

#[allow(clippy::too_many_arguments)]
fn tab(
    w: &WinInfo,
    raised: bool,
    focused: bool,
    text: bool,
    faded: bool,
    engine: &EngineHandle,
    scale: f32,
    origin: (f64, f64),
) -> AnyElement {
    let (press, menu) = (engine.clone(), engine.clone());
    let id = w.id;
    let group = SharedString::from(format!("tab{id}"));
    let mut el = div()
        .id(("tab", id as u64))
        .group(group.clone())
        .relative()
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .px(px(if text { 22. } else { 4. }))
        .rounded_full()
        .on_mouse_down(MouseButton::Left, move |ev, _, cx| {
            cx.stop_propagation();
            let (x, y) = (f32::from(ev.position.x) * scale, f32::from(ev.position.y) * scale);
            press.send(Command::TabPress { id, x: x as f64 + origin.0, y: y as f64 + origin.1 });
        })
        .on_mouse_down(MouseButton::Right, move |ev, _, cx| {
            cx.stop_propagation();
            let (x, y) = (f32::from(ev.position.x) * scale, f32::from(ev.position.y) * scale);
            menu.send(Command::TabMenu { id, x: x as f64 + origin.0, y: y as f64 + origin.1 });
        })
        .child(app_icon(w, 14.0));
    el = if raised { el.shadow_sm() } else { el.hover(|s| s.bg(theme::color_a(SEGMENT_HOVER))) };
    if faded {
        el = el.opacity(0.35);
    }
    if text {
        let name = if w.title.is_empty() { w.exe.clone() } else { w.title.clone() };
        el = el
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .font_weight(if focused { FontWeight::MEDIUM } else { FontWeight::NORMAL })
                    .text_color(theme::color(if raised { theme::TEXT } else { theme::TEXT_DIM }))
                    .child(SharedString::from(name)),
            )
            .child(tab_close(id, group, engine));
    }
    if !raised {
        return el.into_any_element();
    }
    // Becoming the shown tab: its segment lights up.
    let seg = theme::color_a(SEGMENT);
    el.with_animation(("seg", motion::key(id, 1)), Animation::new(NORMAL).with_easing(ease_out_quint()), move |el, t| {
        let mut c = seg;
        c.a *= t;
        el.bg(c)
    })
    .into_any_element()
}

/// The close button: a small circle at the tab's left end, shown while the
/// pointer is on the tab, as Safari does.
fn tab_close(id: isize, group: SharedString, engine: &EngineHandle) -> AnyElement {
    let engine = engine.clone();
    div()
        .id(("tabclose", id as u64))
        .absolute()
        .left(px(4.))
        .size(px(16.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .text_size(px(9.))
        .text_color(theme::color(theme::TEXT_DIM))
        .opacity(0.)
        .group_hover(group, |s| s.opacity(1.))
        .hover(|s| s.bg(theme::color_a(0xffffff24)).text_color(theme::color(theme::TEXT)))
        .child("✕")
        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
            cx.stop_propagation();
            engine.send(Command::CloseWindow(id));
        })
        .into_any_element()
}

/// A soft blue outline where something will land.
fn landing(l: Pixels, t: Pixels, w: Pixels, h: Pixels, radius: Pixels) -> Div {
    let mut fill = theme::color(BLUE);
    fill.a = 0.16;
    let mut edge = theme::color(BLUE);
    edge.a = 0.9;
    div().absolute().left(l).top(t).w(w).h(h).rounded(radius).bg(fill).border_2().border_color(edge)
}

/// Where a dragged window would land as a tab: an outline around the
/// window it joins and an "Add as tab" capsule where its strip goes.
pub fn tab_drop(snap: &Snapshot, scale: f32) -> Vec<AnyElement> {
    let Some(w) = snap.tab_drop.and_then(|id| snap.windows.iter().find(|w| w.id == id)) else { return Vec::new() };
    let wb = win_bounds(snap, w, scale);
    let radius = px((8.0 * snap.zoom) as f32 / scale + 2.);
    let ring = landing(wb.origin.x - px(2.), wb.origin.y - px(2.), wb.size.width + px(4.), wb.size.height + px(4.), radius)
        .bg(transparent_black());
    let mut out = vec![fade(ring, ("droptarget", w.id as u64), FAST, 1.).into_any_element()];
    let spot = snap.tabs.iter().find(|t| t.active == w.id).map_or((w.x, w.y, w.w, w.h), |t| t.spot);
    if let Some(b) = strip_bounds(snap, spot, scale) {
        out.push(
            landing(b.origin.x, b.origin.y, b.size.width, b.size.height, px(STRIP_H / 2.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::color(theme::TEXT))
                .child("Add as tab")
                .with_animation(("addtab", w.id as u64), Animation::new(NORMAL).with_easing(ease_out_quint()), {
                    let top = b.origin.y;
                    move |el, t| el.top(top + px(4. * (1. - t))).opacity(t)
                })
                .into_any_element(),
        );
    }
    out
}

/// A tab being dragged: where it would land, and the tab itself under the
/// cursor with a word on what letting go does.
pub fn tab_dragging(snap: &Snapshot, scale: f32) -> Vec<AnyElement> {
    let Some(d) = snap.tab_drag.as_ref() else { return Vec::new() };
    let at = |v: f64| px(v as f32 / scale);
    let mut out = Vec::new();
    if let Some((l, t, r, b)) = d.preview {
        let radius = px((10.0 * snap.zoom) as f32 / scale).max(px(6.));
        let k = motion::key(d.id, d.hint.len() as u64);
        out.push(fade(landing(at(l), at(t), at(r - l), at(b - t), radius), ("landing", k), FAST, 1.).into_any_element());
    }
    let Some(w) = snap.tabs.iter().flat_map(|g| &g.tabs).find(|t| t.id == d.id) else { return out };
    let name = if w.title.is_empty() { w.exe.clone() } else { w.title.clone() };
    let pill = div()
        .w_full()
        .h(px(STRIP_H))
        .px(px(12.))
        .flex()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .rounded_full()
        .bg(theme::color_a(0x2c2e35f2))
        .border_1()
        .border_color(theme::color_a(HAIRLINE))
        .shadow_lg()
        .child(app_icon(w, 14.0))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::color(theme::TEXT))
                .child(SharedString::from(name)),
        );
    let top = at(d.y) - px(STRIP_H / 2.);
    let mut card = div()
        .absolute()
        .left(at(d.x) - px(90.))
        .top(top)
        .w(px(180.))
        .flex()
        .flex_col()
        .items_center()
        .gap(px(6.))
        .child(pill);
    if !d.hint.is_empty() {
        card = card.child(
            div()
                .px(px(10.))
                .py(px(3.))
                .rounded_full()
                .bg(theme::color_a(GLASS))
                .text_size(px(11.))
                .text_color(theme::color(theme::TEXT_DIM))
                .child(d.hint),
        );
    }
    out.push(rise(card, ("tabcard", d.id as u64), FAST, top, 6., 1.).into_any_element());
    out
}

/// The "Add as tab" capsule: where a window will join a strip.
pub fn add_tab_capsule() -> Div {
    landing(px(0.), px(0.), px(0.), px(0.), px(STRIP_H / 2.))
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme::color(theme::TEXT))
        .child("Add as tab")
}
