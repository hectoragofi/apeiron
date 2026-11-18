//! The saved canvas layout: camera plus per-window canvas positions, so the
//! arrangement survives a restart.
//!
//! HWNDs don't survive a restart, so windows are recognised by exe + class +
//! title, falling back to exe + class when the title has changed (browsers,
//! editors). Entries for windows that are gone are kept, so an app opened
//! later still lands where it was; the oldest are dropped past a cap.
//!
//! Tab groups are kept too, as their windows' keys, so they come back
//! together when those windows do (see `match_tabs`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::settings::data_dir;

/// Saved entries kept at most, current windows first.
const MAX_ENTRIES: usize = 200;
/// Saved tab groups kept at most, current ones first.
const MAX_TAB_GROUPS: usize = 50;
/// A saved tab group none of whose windows came back for this long is
/// forgotten, seconds.
const TABS_STALE_SECS: u64 = 30 * 24 * 3600;

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
    /// Maximized when it went on the canvas: maximized again when landed on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    maximized: bool,
}

/// A tab group as saved: its windows in tab order, which one was shown,
/// and the tab shown beside it in split screen.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedTabs {
    pub members: Vec<WinKey>,
    /// Index into `members`.
    pub active: usize,
    /// Index into `members` of the other split half, and whether it's on
    /// the left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<(usize, bool)>,
    /// When it was last there (Unix seconds).
    #[serde(default)]
    pub seen: u64,
}

/// A named canvas area an IPC client reserved (see `crate::ipc`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Region {
    name: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    camera: Option<(f64, f64)>,
    windows: Vec<Entry>,
    /// Kept by "Reset saved layout": they belong to other apps.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    regions: Vec<Region>,
    /// Tab groups, kept whether or not positions are remembered.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tabs: Vec<SavedTabs>,
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

    /// `saved` without the maximized flags.
    #[cfg(test)]
    pub fn positions(&self, keys: &[WinKey]) -> Vec<Option<(f64, f64)>> {
        self.saved(keys).into_iter().map(|s| s.map(|(x, y, _)| (x, y))).collect()
    }

    /// Saved canvas positions for the given windows, and whether each was
    /// maximized, each saved entry handed out at most once. Pass every live
    /// window, not just new ones, so a new window can't take the spot of
    /// one that's already on the canvas.
    pub fn saved(&self, keys: &[WinKey]) -> Vec<Option<(f64, f64, bool)>> {
        assign(&self.windows, keys)
            .into_iter()
            .map(|i| i.map(|i| (self.windows[i].x, self.windows[i].y, self.windows[i].maximized)))
            .collect()
    }

    /// Records the current camera and window positions. Saved entries no
    /// current window accounts for are kept, behind the current ones.
    pub fn capture(&mut self, cam: (f64, f64), wins: &[(WinKey, f64, f64, bool)]) {
        let keys: Vec<WinKey> = wins.iter().map(|(k, ..)| k.clone()).collect();
        let mut used = vec![false; self.windows.len()];
        for i in assign(&self.windows, &keys).into_iter().flatten() {
            used[i] = true;
        }
        let old = std::mem::take(&mut self.windows);
        self.windows =
            wins.iter().map(|(key, x, y, maximized)| Entry { key: key.clone(), x: *x, y: *y, maximized: *maximized }).collect();
        self.windows.extend(old.into_iter().zip(used).filter(|(_, u)| !u).map(|(e, _)| e));
        self.windows.truncate(MAX_ENTRIES);
        self.camera = Some(cam);
    }

    /// The reserved region called `name`, as `[x, y, w, h]` on the canvas.
    pub fn region(&self, name: &str) -> Option<[f64; 4]> {
        self.regions.iter().find(|r| r.name == name).map(|r| [r.x, r.y, r.w, r.h])
    }

    /// Every reserved region, as `[x, y, w, h]`.
    pub fn regions(&self) -> Vec<[f64; 4]> {
        self.regions.iter().map(|r| [r.x, r.y, r.w, r.h]).collect()
    }

    pub fn set_region(&mut self, name: &str, [x, y, w, h]: [f64; 4]) {
        match self.regions.iter_mut().find(|r| r.name == name) {
            Some(r) => (r.x, r.y, r.w, r.h) = (x, y, w, h),
            None => self.regions.push(Region { name: name.to_string(), x, y, w, h }),
        }
    }

    /// The saved tab groups, minus those gone for too long.
    pub fn tab_groups(&self, now: u64) -> Vec<SavedTabs> {
        self.tabs.iter().filter(|t| !stale(t, now)).cloned().collect()
    }

    /// Records the tab groups: `live` as they are now, then those still
    /// waiting for their windows (`pending`, as loaded), unless stale.
    pub fn capture_tabs(&mut self, live: Vec<SavedTabs>, pending: &[SavedTabs], now: u64) {
        self.tabs = live;
        self.tabs.extend(pending.iter().filter(|t| !stale(t, now)).cloned());
        self.tabs.truncate(MAX_TAB_GROUPS);
    }

    /// Forgets the camera and every saved position.
    pub fn reset(&mut self) {
        self.camera = None;
        self.windows.clear();
    }
}

fn stale(t: &SavedTabs, now: u64) -> bool {
    now.saturating_sub(t.seen) > TABS_STALE_SECS
}

/// Now, in Unix seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Finds windows for saved tab groups. `want[g][m]` is the key member `m`
/// of group `g` still needs a window for (None: it has one). `keys` are the
/// windows that could join (on the canvas, in no tabs, not claimed). Each
/// window goes to at most one member.
///
/// Exact matches come first, across every group. Then, titles having
/// changed (browsers, editors), exe + class alone does for groups that
/// allow it (`loose[g]`, the few minutes after start) or that matched a
/// window exactly just now: their app is the one bringing windows back.
/// Returns, per group and member, the index into `keys` it got.
pub fn match_tabs(want: &[Vec<Option<WinKey>>], keys: &[WinKey], loose: &[bool]) -> Vec<Vec<Option<usize>>> {
    let mut claimed = vec![false; keys.len()];
    let mut out: Vec<Vec<Option<usize>>> = want.iter().map(|g| vec![None; g.len()]).collect();
    let mut take = |out: &mut Vec<Vec<Option<usize>>>, g: usize, same: fn(&WinKey, &WinKey) -> bool| {
        for (m, k) in want[g].iter().enumerate() {
            let Some(k) = k else { continue };
            if out[g][m].is_some() {
                continue;
            }
            if let Some(i) = (0..keys.len()).find(|&i| !claimed[i] && same(k, &keys[i])) {
                claimed[i] = true;
                out[g][m] = Some(i);
            }
        }
    };
    for g in 0..want.len() {
        take(&mut out, g, |a, b| a == b);
    }
    for g in 0..want.len() {
        if loose.get(g).copied().unwrap_or(false) || out[g].iter().any(Option::is_some) {
            take(&mut out, g, |a, b| !a.exe.is_empty() && a.exe == b.exe && a.class == b.class);
        }
    }
    out
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
        l.capture((10.0, 20.0), &with_flag(entries));
        l
    }

    fn with_flag(entries: &[(WinKey, f64, f64)]) -> Vec<(WinKey, f64, f64, bool)> {
        entries.iter().map(|(k, x, y)| (k.clone(), *x, *y, false)).collect()
    }

    #[test]
    fn maximized_is_remembered() {
        let mut l = Layout::default();
        l.capture((0.0, 0.0), &[(key("a.exe", "A", "1"), 1.0, 2.0, true), (key("b.exe", "B", "1"), 3.0, 4.0, false)]);
        let got = l.saved(&[key("b.exe", "B", "1"), key("a.exe", "A", "x")]);
        assert_eq!(got, vec![Some((3.0, 4.0, false)), Some((1.0, 2.0, true))]);
        // Old files without the field load as not maximized.
        let json = r#"{"camera":[0,0],"windows":[{"exe":"a.exe","class":"A","title":"1","x":1.0,"y":2.0}]}"#;
        let old: Layout = serde_json::from_str(json).unwrap();
        assert_eq!(old.saved(&[key("a.exe", "A", "1")]), vec![Some((1.0, 2.0, false))]);
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
        l.capture((5.0, 6.0), &with_flag(&[(key("a.exe", "A", "2"), 7.0, 0.0)]));
        assert_eq!(l.camera(), Some((5.0, 6.0)));
        assert_eq!(l.positions(&[key("b.exe", "B", "1")]), vec![Some((2.0, 0.0))]);
        assert_eq!(l.positions(&[key("a.exe", "A", "1")]), vec![Some((7.0, 0.0))]);
        assert_eq!(l.windows.len(), 2);
    }

    #[test]
    fn capture_caps_entries() {
        let mut l = Layout::default();
        let many: Vec<_> = (0..MAX_ENTRIES + 5).map(|i| (key("a.exe", "A", &i.to_string()), i as f64, 0.0)).collect();
        l.capture((0.0, 0.0), &with_flag(&many));
        l.capture((0.0, 0.0), &with_flag(&[(key("b.exe", "B", ""), -1.0, 0.0)]));
        assert_eq!(l.windows.len(), MAX_ENTRIES);
        assert_eq!(l.windows[0].key.exe, "b.exe");
    }

    #[test]
    fn regions_persist_and_survive_reset() {
        let dir = std::env::temp_dir().join(format!("apeiron-test-regions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("layout.json");
        let mut l = layout(&[(key("a.exe", "A", "1"), 1.0, 0.0)]);
        l.set_region("desk", [10.0, 20.0, 300.0, 200.0]);
        l.set_region("desk", [10.0, 20.0, 400.0, 200.0]);
        l.set_region("other", [0.0, 0.0, 1.0, 1.0]);
        l.save_to(&file);
        let mut back = Layout::load_from(&file);
        assert_eq!(back.region("desk"), Some([10.0, 20.0, 400.0, 200.0]));
        assert_eq!(back.regions().len(), 2);
        back.reset();
        assert_eq!(back.region("other"), Some([0.0, 0.0, 1.0, 1.0]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reset_forgets_everything() {
        let mut l = layout(&[(key("a.exe", "A", "1"), 1.0, 0.0)]);
        l.reset();
        assert_eq!(l.camera(), None);
        assert_eq!(l.positions(&[key("a.exe", "A", "1")]), vec![None]);
    }

    fn tabs(members: &[WinKey], active: usize, split: Option<(usize, bool)>, seen: u64) -> SavedTabs {
        SavedTabs { members: members.to_vec(), active, split, seen }
    }

    fn want(groups: &[&[WinKey]]) -> Vec<Vec<Option<WinKey>>> {
        groups.iter().map(|g| g.iter().cloned().map(Some).collect()).collect()
    }

    #[test]
    fn tabs_match_exactly_across_groups_first() {
        let (a, b) = (key("code.exe", "C", "a.rs"), key("code.exe", "C", "b.rs"));
        let n = key("notepad.exe", "Notepad", "x.txt");
        // Group 0 could take b.rs by app, but group 1 wants it exactly.
        let w = want(&[&[a, n.clone()], &[b.clone()]]);
        let got = match_tabs(&w, &[b, n], &[true, true]);
        assert_eq!(got, vec![vec![None, Some(1)], vec![Some(0)]]);
    }

    #[test]
    fn tabs_match_by_app_only_when_allowed() {
        let saved = [key("chrome.exe", "Chrome_WidgetWin_1", "Mail"), key("chrome.exe", "Chrome_WidgetWin_1", "Docs")];
        let now = [key("chrome.exe", "Chrome_WidgetWin_1", "Inbox (3)"), key("chrome.exe", "Chrome_WidgetWin_1", "Docs 2")];
        let w = want(&[&saved]);
        // Long after start, with no exact match: titles alone don't do.
        assert_eq!(match_tabs(&w, &now, &[false]), vec![vec![None, None]]);
        // Just after start they do.
        assert_eq!(match_tabs(&w, &now, &[true]), vec![vec![Some(0), Some(1)]]);
        // Later, the app brought one back exactly: the other comes along.
        let back = [now[0].clone(), saved[1].clone()];
        assert_eq!(match_tabs(&w, &back, &[false]), vec![vec![Some(0), Some(1)]]);
    }

    #[test]
    fn tabs_skip_members_found_already_and_unknown_apps() {
        let k = key("a.exe", "A", "1");
        let w = vec![vec![None, Some(key("a.exe", "A", "2")), Some(key("", "A", "3"))]];
        let got = match_tabs(&w, &[k.clone(), key("", "A", "other")], &[true]);
        // Member 0 has its window; member 2's app is unknown, so only its
        // exact title would do.
        assert_eq!(got, vec![vec![None, Some(0), None]]);
    }

    #[test]
    fn tabs_never_share_a_window() {
        let k = key("a.exe", "A", "1");
        let w = want(&[&[k.clone(), k.clone()], &[k.clone()]]);
        let got = match_tabs(&w, &[k.clone(), k.clone(), k], &[false, false]);
        assert_eq!(got, vec![vec![Some(0), Some(1)], vec![Some(2)]]);
    }

    #[test]
    fn tab_groups_are_captured_and_go_stale() {
        let day = 24 * 3600;
        let (a, b, c) = (key("a.exe", "A", "1"), key("a.exe", "A", "2"), key("c.exe", "C", "1"));
        let mut l = Layout::default();
        let live = tabs(&[a.clone(), b.clone()], 1, Some((0, true)), 100 * day);
        let waiting = tabs(&[c.clone(), a.clone()], 0, None, 90 * day);
        let old = tabs(&[c.clone(), b.clone()], 0, None, 10 * day);
        l.capture_tabs(vec![live.clone()], &[waiting.clone(), old], 100 * day);
        assert_eq!(l.tab_groups(100 * day), vec![live.clone(), waiting.clone()]);
        // Positions being reset leaves them be.
        l.reset();
        assert_eq!(l.tab_groups(100 * day).len(), 2);
        // A month on, the one still waiting is forgotten.
        assert_eq!(l.tab_groups(125 * day), vec![live]);
        let many: Vec<SavedTabs> = (0..MAX_TAB_GROUPS + 3).map(|_| waiting.clone()).collect();
        l.capture_tabs(many, &[], 100 * day);
        assert_eq!(l.tabs.len(), MAX_TAB_GROUPS);
    }

    #[test]
    fn tab_groups_round_trip_and_old_files_load() {
        let dir = std::env::temp_dir().join(format!("apeiron-test-tabs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("layout.json");
        let mut l = Layout::default();
        let t = tabs(&[key("a.exe", "A", "1"), key("b.exe", "B", "")], 1, Some((0, false)), 42);
        l.capture_tabs(vec![t.clone(), tabs(&[key("a.exe", "A", "2")], 0, None, 43)], &[], 50);
        l.save_to(&file);
        assert_eq!(Layout::load_from(&file).tab_groups(50), vec![t, tabs(&[key("a.exe", "A", "2")], 0, None, 43)]);
        std::fs::write(&file, r#"{"camera":[1.0,2.0],"windows":[]}"#).unwrap();
        assert!(Layout::load_from(&file).tab_groups(50).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn json_round_trip() {
        let dir = std::env::temp_dir().join(format!("apeiron-test-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("apeiron-test-bad-{}", std::process::id()));
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
