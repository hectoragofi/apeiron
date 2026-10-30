//! The saved canvas layout: camera plus per-window canvas positions, so the
//! arrangement survives a restart.

/// Identity of a window across restarts (lowercase exe name, class, title).
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct WinKey {
    pub exe: String,
    pub class: String,
    pub title: String,
}

// FEATURE A: persist to `%APPDATA%\infinite-canvas\layout.json` (serde). Match
// windows by exe + class + title, falling back to exe + class (first unclaimed).
#[derive(Default)]
pub struct Layout {}

impl Layout {
    pub fn load() -> Layout {
        Layout {}
    }

    pub fn save(&self) {}

    /// Saved camera, if there is a layout to restore.
    pub fn camera(&self) -> Option<(f64, f64)> {
        None
    }

    /// Takes the saved canvas position for a window that has just appeared.
    /// Each saved entry is handed out at most once per sync.
    pub fn claim(&mut self, _key: &WinKey) -> Option<(f64, f64)> {
        None
    }

    /// Makes every saved entry claimable again; called before each sync.
    pub fn release_claims(&mut self) {}

    /// Replaces the saved state with the current camera and window positions.
    pub fn capture(&mut self, _cam: (f64, f64), _wins: &[(WinKey, f64, f64)]) {}

    pub fn reset(&mut self) {}
}
