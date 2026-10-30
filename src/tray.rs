//! Tray icon and its menu. Created on the GPUI thread; menu events arrive
//! through GPUI's message loop.

use crate::shared::{EngineHandle, Shared};

/// Keeps the tray icon alive; dropping it removes the icon.
pub struct Tray;

impl gpui::Global for Tray {}

// FEATURE B: build the icon in code (no asset files). Left click =
// Command::ToggleOverview. Menu: Overview, Find window..., Tidy up, Settings...
// (UiEvent::OpenSettings via shared.emit), Pause/Resume, Quit.
pub fn create(_shared: &Shared, _engine: &EngineHandle) -> Option<Tray> {
    None
}
