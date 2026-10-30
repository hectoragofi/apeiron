//! The search box and its results, shown over the overview.

use gpui::{AnyElement, KeyDownEvent};

use crate::shared::{EngineHandle, Snapshot, WinInfo};

// FEATURE C: prefix matches on title/exe first, then substring matches, all
// case-insensitive. An empty query matches everything in window order.
pub fn rank(query: &str, wins: &[WinInfo]) -> Vec<isize> {
    let q = query.to_lowercase();
    wins.iter()
        .filter(|w| w.title.to_lowercase().contains(&q) || w.exe.contains(&q))
        .map(|w| w.id)
        .collect()
}

// FEATURE C: a centred field at the top of the screen with the results below it
// when `snap.search` is set; None otherwise.
pub fn render(_snap: &Snapshot, _engine: &EngineHandle, _scale: f32) -> Option<AnyElement> {
    None
}

// FEATURE C: typing -> Command::SearchQuery, Up/Down -> SearchMove, Enter ->
// SearchAccept, Esc -> CloseSearch. The overlay view has a focus handle for this.
pub fn on_key(_ev: &KeyDownEvent, _engine: &EngineHandle) {}
