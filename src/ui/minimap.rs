//! The minimap panel in the bottom-right corner.

use gpui::AnyElement;

use crate::settings::Settings;
use crate::shared::{EngineHandle, Snapshot};

// FEATURE D: a ~220x140 logical rounded translucent panel with every window's
// rect and the camera viewport rect; click or drag moves the camera with
// Command::FlyTo. Drawn on both the overlay and the background.
pub fn render(_snap: &Snapshot, _settings: &Settings, _engine: &EngineHandle, _scale: f32) -> Option<AnyElement> {
    None
}
