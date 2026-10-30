//! The saved canvas layout: camera plus per-window canvas positions, so the
//! arrangement survives a restart.
//!
//! HWNDs don't survive a restart, so windows are recognised by exe + class +
//! title, falling back to exe + class when the title has changed (browsers,
//! editors). Entries for windows that are gone are kept, so an app opened
//! later still lands where it was; the oldest are dropped past a cap.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::settings::data_dir;

/// Saved entries kept at most, current windows first.
const MAX_ENTRIES: usize = 200;

/// Identity of a window across restarts (lowercase exe name, class, title).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinKey {
    pub exe: String,
    pub class: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Entry {
    #[serde(flatten)]
    key: WinKey,
    x: f64,
    y: f64,
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    camera: Option<(f64, f64)>,
    windows: Vec<Entry>,
    /// What's on disk, so unchanged layouts aren't rewritten.
    #[serde(skip)]
    written: Option<String>,
}

fn path() -> PathBuf {
    data_dir().join("layout.json")
}

impl Layout {
    pub fn load() -> Layout {
        Layout::load_from(&path())
    }

    fn load_from(path: &Path) -> Layout {
        let Ok(json) = std::fs::read_to_string(path) else { return Layout::default() };
        match serde_json::from_str::<Layout>(&json) {
            Ok(mut layout) => {
                layout.written = Some(json);
                layout
            }
            Err(e) => {
                log!("ignoring unreadable layout.json: {e}");
                Layout::default()
            }
        }
    }

    pub fn save(&mut self) {
        self.save_to(&path());
    }

    /// Writes via a temp file so a crash mid-write never leaves a torn file.
    fn save_to(&mut self, path: &Path) {
        let json = match serde_json::to_string_pretty(self) {
            Ok(json) => json,
            Err(e) => {
                log!("couldn't serialize layout: {e}");
                return;
            }
        };
        if self.written.as_ref() == Some(&json) {
            return;
        }
        let tmp = path.with_extension("json.tmp");
        match std::fs::write(&tmp, &json).and_then(|_| std::fs::rename(&tmp, path)) {
            Ok(()) => self.written = Some(json),
            Err(e) => log!("couldn't save layout: {e}"),
        }
    }

    /// Saved camera, if there is a layout to restore.
    pub fn camera(&self) -> Option<(f64, f64)> {
        self.camera
    }

    /// Saved canvas positions for the given windows, each saved entry handed
    /// out at most once. Pass every live window, not just new ones, so a new
    /// window can't take the spot of one that's already on the canvas.
    pub fn positions(&self, keys: &[WinKey]) -> Vec<Option<(f64, f64)>> {
        assign(&self.windows, keys)
            .into_iter()
            .map(|i| i.map(|i| (self.windows[i].x, self.windows[i].y)))
            .collect()
    }

    /// Records the current camera and window positions. Saved entries no
    /// current window accounts for are kept, behind the current ones.
    pub fn capture(&mut self, cam: (f64, f64), wins: &[(WinKey, f64, f64)]) {
        let keys: Vec<WinKey> = wins.iter().map(|(k, _, _)| k.clone()).collect();
        let mut used = vec![false; self.windows.len()];
        for i in assign(&self.windows, &keys).into_iter().flatten() {
            used[i] = true;
        }
        let old = std::mem::take(&mut self.windows);
        self.windows = wins.iter().map(|(key, x, y)| Entry { key: key.clone(), x: *x, y: *y }).collect();
        self.windows.extend(old.into_iter().zip(used).filter(|(_, u)| !u).map(|(e, _)| e));
        self.windows.truncate(MAX_ENTRIES);
        self.camera = Some(cam);
    }

    /// Forgets the camera and every saved position.
    pub fn reset(&mut self) {
        self.camera = None;
        self.windows.clear();
    }
}

/// Matches each key to a distinct saved entry: exact matches first, then
/// exe + class for the rest, each taking the first unclaimed entry.
fn assign(saved: &[Entry], keys: &[WinKey]) -> Vec<Option<usize>> {
    let mut claimed = vec![false; saved.len()];
    let mut out = vec![None; keys.len()];
    let passes: [fn(&WinKey, &WinKey) -> bool; 2] =
        [|a, b| a == b, |a, b| a.exe == b.exe && a.class == b.class];
    for same in passes {
        for (key, slot) in keys.iter().zip(out.iter_mut()) {
            if slot.is_some() {
                continue;
            }
            if let Some(i) = (0..saved.len()).find(|&i| !claimed[i] && same(&saved[i].key, key)) {
                claimed[i] = true;
                *slot = Some(i);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(exe: &str, class: &str, title: &str) -> WinKey {
        WinKey { exe: exe.into(), class: class.into(), title: title.into() }
    }

    fn layout(entries: &[(WinKey, f64, f64)]) -> Layout {
        let mut l = Layout::default();
        l.capture((10.0, 20.0), entries);
        l
    }

    #[test]
    fn exact_match_wins_over_earlier_fallback() {
        let l = layout(&[
            (key("code.exe", "Chrome_WidgetWin_1", "a.rs"), 1.0, 1.0),
            (key("code.exe", "Chrome_WidgetWin_1", "b.rs"), 2.0, 2.0),
        ]);
        let got = l.positions(&[
            key("code.exe", "Chrome_WidgetWin_1", "renamed"),
            key("code.exe", "Chrome_WidgetWin_1", "a.rs"),
        ]);
        // The exact match claims a.rs even though the renamed window asks
        // first; the renamed one falls back to the remaining entry.
        assert_eq!(got, vec![Some((2.0, 2.0)), Some((1.0, 1.0))]);
    }

    #[test]
    fn fallback_takes_first_unclaimed() {
        let l = layout(&[
            (key("notepad.exe", "Notepad", "x.txt"), 1.0, 0.0),
            (key("notepad.exe", "Notepad", "y.txt"), 2.0, 0.0),
            (key("notepad.exe", "Notepad", "z.txt"), 3.0, 0.0),
        ]);
        let got = l.positions(&[
            key("notepad.exe", "Notepad", "new 1"),
            key("notepad.exe", "Notepad", "y.txt"),
            key("notepad.exe", "Notepad", "new 2"),
            key("notepad.exe", "Notepad", "new 3"),
        ]);
        assert_eq!(got, vec![Some((1.0, 0.0)), Some((2.0, 0.0)), Some((3.0, 0.0)), None]);
    }

    #[test]
    fn duplicate_exact_keys_each_get_one_entry() {
        let k = key("explorer.exe", "CabinetWClass", "Downloads");
        let l = layout(&[(k.clone(), 1.0, 0.0), (k.clone(), 2.0, 0.0)]);
        assert_eq!(l.positions(&[k.clone(), k.clone(), k]), vec![Some((1.0, 0.0)), Some((2.0, 0.0)), None]);
    }

    #[test]
    fn class_or_exe_mismatch_never_matches() {
        let l = layout(&[(key("a.exe", "A", "t"), 1.0, 0.0)]);
        assert_eq!(l.positions(&[key("a.exe", "B", "t"), key("b.exe", "A", "t")]), vec![None, None]);
    }

    #[test]
    fn capture_keeps_entries_of_closed_windows_behind_current_ones() {
        let mut l = layout(&[(key("a.exe", "A", "1"), 1.0, 0.0), (key("b.exe", "B", "1"), 2.0, 0.0)]);
        // b.exe closed; a.exe moved and retitled.
        l.capture((5.0, 6.0), &[(key("a.exe", "A", "2"), 7.0, 0.0)]);
        assert_eq!(l.camera(), Some((5.0, 6.0)));
        assert_eq!(l.positions(&[key("b.exe", "B", "1")]), vec![Some((2.0, 0.0))]);
        assert_eq!(l.positions(&[key("a.exe", "A", "1")]), vec![Some((7.0, 0.0))]);
        assert_eq!(l.windows.len(), 2);
    }

    #[test]
    fn capture_caps_entries() {
        let mut l = Layout::default();
        let many: Vec<_> = (0..MAX_ENTRIES + 5).map(|i| (key("a.exe", "A", &i.to_string()), i as f64, 0.0)).collect();
        l.capture((0.0, 0.0), &many);
        l.capture((0.0, 0.0), &[(key("b.exe", "B", ""), -1.0, 0.0)]);
        assert_eq!(l.windows.len(), MAX_ENTRIES);
        assert_eq!(l.windows[0].key.exe, "b.exe");
    }

    #[test]
    fn reset_forgets_everything() {
        let mut l = layout(&[(key("a.exe", "A", "1"), 1.0, 0.0)]);
        l.reset();
        assert_eq!(l.camera(), None);
        assert_eq!(l.positions(&[key("a.exe", "A", "1")]), vec![None]);
    }

    #[test]
    fn json_round_trip() {
        let dir = std::env::temp_dir().join(format!("infinite-canvas-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("layout.json");
        let mut l = layout(&[
            (key("code.exe", "Chrome_WidgetWin_1", "main.rs — \"quoted\" ✓"), -1234.5, 99.25),
            (key("explorer.exe", "CabinetWClass", ""), 3e6, -7.0),
        ]);
        l.save_to(&file);
        let back = Layout::load_from(&file);
        assert_eq!(back.camera, l.camera);
        assert_eq!(back.windows, l.windows);
        assert!(!dir.join("layout.json.tmp").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_or_corrupt_file_loads_empty() {
        let dir = std::env::temp_dir().join(format!("infinite-canvas-test-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("layout.json");
        assert_eq!(Layout::load_from(&file), Layout::default());
        std::fs::write(&file, "{ not json").unwrap();
        assert_eq!(Layout::load_from(&file), Layout::default());
        // Unknown fields and missing ones are fine (older/newer versions).
        std::fs::write(&file, r#"{"camera":[1.0,2.0],"zoom":1.0}"#).unwrap();
        assert_eq!(Layout::load_from(&file).camera(), Some((1.0, 2.0)));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
