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
use crate::shared::{Mode, Snapshot, WinInfo};

/// Labels appear below this zoom; closer in, title bars are readable.
const LABEL_ZOOM: f64 = 0.75;
const LABEL_H: f32 = 22.0;
/// Space between a label and the top of its window.
const LABEL_GAP: f32 = 4.0;
/// Narrower windows get no label, and below `TEXT_MIN_W` only the icon.
const LABEL_MIN_W: f32 = 22.0;
const TEXT_MIN_W: f32 = 64.0;
/// Opacity of labels of windows that don't match the search.
const DIMMED: f32 = 0.3;

pub fn render(snap: &Snapshot, _settings: &Settings, scale: f32) -> Vec<AnyElement> {
    let mut out = Vec::new();
    if !snap.overlay_visible {
        return out;
    }
    if snap.zoom < LABEL_ZOOM {
        let screen = size(px((snap.screen_w / scale as f64) as f32), px((snap.screen_h / scale as f64) as f32));
        let matching = snap.search.as_ref().map(|s| &s.results);
        // Bottom-most first, so where labels overlap the upper window's wins.
        for w in snap.windows.iter().rev() {
            let dim = matching.is_some_and(|m| !m.contains(&w.id));
            out.extend(label(snap, w, scale, screen, dim));
        }
    }
    let browsing = matches!(snap.mode, Mode::Overview | Mode::Search);
    let selected = snap.search.as_ref().and_then(|s| s.results.get(s.selected).copied());
    if let Some(w) = snap.windows.iter().find(|w| Some(w.id) == snap.hovered && browsing && snap.hovered != selected) {
        out.push(hover_ring(snap, w, scale));
    }
    out
}

fn label(snap: &Snapshot, w: &WinInfo, scale: f32, screen: Size<Pixels>, dim: bool) -> Option<AnyElement> {
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
    let mut row = div()
        .absolute()
        .left(b.origin.x)
        .top(top)
        .w(b.size.width)
        .h(px(LABEL_H))
        .flex()
        .items_center()
        .gap(px(6.))
        .overflow_hidden()
        .child(app_icon(w, icon_size));
    if width >= TEXT_MIN_W {
        let color = if w.focused { theme::TEXT } else { theme::TEXT_DIM };
        row = row.child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(if compact { 11. } else { 12. }))
                .text_color(theme::color(color))
                .child(SharedString::from(w.title.clone())),
        );
    }
    if dim {
        row = row.opacity(DIMMED);
    }
    Some(row.into_any_element())
}

/// A soft ring just outside the window under the mouse.
fn hover_ring(snap: &Snapshot, w: &WinInfo, scale: f32) -> AnyElement {
    let b = win_bounds(snap, w, scale);
    let out = px(4.);
    div()
        .absolute()
        .left(b.origin.x - out)
        .top(b.origin.y - out)
        .w(b.size.width + out * 2.)
        .h(b.size.height + out * 2.)
        .rounded(px(8.))
        .border_2()
        .border_color(theme::color_a(HOVER))
        .into_any_element()
}

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
