//! Window labels (app icon + title above each window when zoomed out) and
//! the hover ring.

use gpui::AnyElement;

use crate::settings::Settings;
use crate::shared::Snapshot;

// FEATURE C: when zoom < 0.75 give every window a label above it (16px icon from
// `icons::icon_for` + truncated title) and ring `snap.hovered`. Use
// `canvas_view::win_bounds` for positions. Return absolutely positioned elements.
pub fn render(_snap: &Snapshot, _settings: &Settings, _scale: f32) -> Vec<AnyElement> {
    Vec::new()
}
