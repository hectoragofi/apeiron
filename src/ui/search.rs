//! The search box and its results, shown over the overview.
//!
//! The engine owns the search (query, ranked results, selection) and dims
//! the thumbnails of windows that don't match; this module draws the panel,
//! rings the selected window and turns keystrokes into commands. The panel
//! sits under the DWM thumbnails like everything GPUI draws, so the engine
//! keeps windows out of [`panel_rect`] while searching.
//!
//! Installed apps are listed after the windows, under their own heading;
//! picking one launches it.

use std::cell::{Cell, RefCell};

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, AnyElement, Div, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement, SharedString,
    Styled,
};

use super::canvas_view::win_bounds;
use super::{labels, theme};
use crate::shared::{AppInfo, Command, EngineHandle, SearchState, Snapshot, WinInfo};

// Panel geometry, logical pixels.
const PANEL_W: f64 = 560.0;
const PANEL_TOP: f64 = 24.0;
const FIELD_H: f64 = 52.0;
const ROW_H: f64 = 36.0;
const MAX_ROWS: usize = 7;
/// Apps listed at most for one query.
pub const MAX_APPS: usize = 20;
const LIST_PAD: f64 = 6.0;
const FOOTER_H: f64 = 28.0;

thread_local! {
    /// What the user has typed. Kept here rather than read back from the
    /// snapshot so fast typing never builds on a stale query.
    static QUERY: RefCell<String> = const { RefCell::new(String::new()) };
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    /// The engine's `SearchState::seed` that `QUERY` was last taken from.
    static SEED: Cell<u32> = const { Cell::new(0) };
}

/// Windows matching `query` by title or exe name (case-insensitive), best
/// first: title prefix, exe prefix, a word in the title starting with it,
/// then plain substring matches. Ties keep window order (topmost first), and
/// an empty query matches everything in window order.
pub fn rank(query: &str, wins: &[WinInfo]) -> Vec<isize> {
    let q = query.trim().to_lowercase();
    let mut hits: Vec<(u8, isize)> = wins.iter().filter_map(|w| Some((tier(&q, w)?, w.id))).collect();
    hits.sort_by_key(|&(tier, _)| tier);
    hits.into_iter().map(|(_, id)| id).collect()
}

/// Installed apps matching `query` by name, best first: name prefix, a word
/// starting with it, the words' initials ("vsc" for Visual Studio Code),
/// then a plain substring. Shorter names first within a tier (the closest
/// match), then the list's own (alphabetical) order. Nothing for an empty
/// query: every app at once is no help.
pub fn rank_apps(query: &str, apps: &[AppInfo]) -> Vec<AppInfo> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(u8, usize, &AppInfo)> = apps
        .iter()
        .filter_map(|a| {
            let name = a.name.to_lowercase();
            let initials: String = name.split(|c: char| !c.is_alphanumeric()).filter_map(|w| w.chars().next()).collect();
            let tier = if name.starts_with(&q) {
                0
            } else if word_start(&name, &q) {
                1
            } else if q.chars().count() >= 2 && initials.starts_with(&q) {
                2
            } else if name.contains(&q) {
                3
            } else {
                return None;
            };
            Some((tier, name.chars().count(), a))
        })
        .collect();
    hits.sort_by_key(|&(tier, len, _)| (tier, len));
    hits.into_iter().take(MAX_APPS).map(|(_, _, a)| a.clone()).collect()
}

/// A word in `text` (after a non-alphanumeric character) starts with `q`.
fn word_start(text: &str, q: &str) -> bool {
    text.match_indices(q).any(|(i, _)| text[..i].ends_with(|c: char| !c.is_alphanumeric()))
}

fn tier(q: &str, w: &WinInfo) -> Option<u8> {
    if q.is_empty() {
        return Some(0);
    }
    let title = w.title.to_lowercase();
    let exe = w.exe.to_lowercase();
    if title.starts_with(q) {
        Some(0)
    } else if exe.starts_with(q) {
        Some(1)
    } else if word_start(&title, q) {
        Some(2)
    } else if title.contains(q) {
        Some(3)
    } else if exe.contains(q) {
        Some(4)
    } else {
        None
    }
}

/// The panel's screen rectangle `(left, top, right, bottom)` in physical
/// pixels, for a screen `screen_w` physical pixels wide.
pub fn panel_rect(state: &SearchState, screen_w: f64, scale: f64) -> (f64, f64, f64, f64) {
    let screen = screen_w / scale;
    let w = PANEL_W.min(screen - 32.0);
    let l = (screen - w) / 2.0;
    let h = FIELD_H + list_height(state) + FOOTER_H;
    (l * scale, PANEL_TOP * scale, (l + w) * scale, (PANEL_TOP + h) * scale)
}

/// One line of the results list.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Line {
    /// The window at this index of `results`.
    Win(usize),
    /// The heading over the apps.
    Apps,
    /// The app at this index of `apps`.
    App(usize),
}

fn lines(state: &SearchState) -> Vec<Line> {
    let mut v: Vec<Line> = (0..state.results.len()).map(Line::Win).collect();
    if !state.apps.is_empty() {
        v.push(Line::Apps);
        v.extend((0..state.apps.len()).map(Line::App));
    }
    v
}

/// The lines shown: a window of at most MAX_ROWS that keeps the selection
/// (and, for the first app, its heading) in view.
fn visible_lines(state: &SearchState) -> Vec<Line> {
    let all = lines(state);
    let k = state.selected;
    let at = all
        .iter()
        .position(|&l| l == Line::Win(k) || state.selected_app().is_some() && l == Line::App(k - state.results.len()))
        .unwrap_or(0);
    let first = (at + 1).saturating_sub(MAX_ROWS);
    all.into_iter().skip(first).take(MAX_ROWS).collect()
}

fn list_height(state: &SearchState) -> f64 {
    let rows = match lines(state).len() {
        0 if state.query.trim().is_empty() => return 0.0,
        // Room for "No matches".
        0 => 1,
        n => n.min(MAX_ROWS),
    };
    rows as f64 * ROW_H + LIST_PAD * 2.0
}

/// The search panel and the ring around the selected window, or None when
/// not searching.
pub fn render(snap: &Snapshot, engine: &EngineHandle, scale: f32) -> Option<AnyElement> {
    let Some(state) = snap.search.as_ref() else {
        ACTIVE.set(false);
        QUERY.with(|q| q.borrow_mut().clear());
        return None;
    };
    // Opening, or the engine typed into it (keys pressed before the panel
    // had the keyboard): take its query over.
    if !ACTIVE.replace(true) || SEED.get() != state.seed {
        SEED.set(state.seed);
        QUERY.with(|q| *q.borrow_mut() = state.query.clone());
    }
    let selected = state.results.get(state.selected).and_then(|id| snap.windows.iter().find(|w| w.id == *id));

    let (l, t, r, _) = panel_rect(state, snap.screen_w, scale as f64);
    let s = scale as f64;
    let panel = div()
        .absolute()
        .left(px((l / s) as f32))
        .top(px((t / s) as f32))
        .w(px(((r - l) / s) as f32))
        .flex()
        .flex_col()
        .rounded(px(14.))
        .bg(theme::color_a(theme::PANEL))
        .border_1()
        .border_color(theme::color_a(theme::BORDER))
        .shadow_lg()
        .overflow_hidden()
        // Clicks and scrolls on the panel stay off the canvas below it.
        .occlude()
        .child(if state.switcher { switcher_title() } else { field(&state.query, state.launcher) })
        .when(list_height(state) > 0.0, |p| p.child(results(snap, state, engine)))
        .child(footer(state));

    Some(
        div()
            .absolute()
            .size_full()
            .children(selected.map(|w| ring(snap, w, scale)))
            .child(super::motion::rise(panel, "searchpanel", super::motion::NORMAL, px((t / s) as f32), -8., 1.))
            .into_any_element(),
    )
}

fn switcher_title() -> Div {
    div()
        .h(px(FIELD_H as f32))
        .flex_shrink_0()
        .flex()
        .items_center()
        .px(px(18.))
        .text_size(px(16.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme::color(theme::TEXT))
        .child("Switch windows")
}

fn field(query: &str, launcher: bool) -> Div {
    let text = if query.is_empty() {
        let hint = if launcher { "Find a window or app" } else { "Find a window" };
        div().text_color(theme::color(theme::TEXT_DIM)).child(hint)
    } else {
        div().text_color(theme::color(theme::TEXT)).child(SharedString::from(query.to_string()))
    };
    let caret = div().w(px(2.)).h(px(22.)).rounded(px(1.)).bg(theme::color(theme::ACCENT));
    let row = div().flex().items_center().gap(px(1.)).min_w_0().overflow_hidden();
    // The caret sits after the text; the placeholder sits after the caret.
    let row = if query.is_empty() { row.child(caret).child(text) } else { row.child(text).child(caret) };
    div()
        .h(px(FIELD_H as f32))
        .flex_shrink_0()
        .flex()
        .items_center()
        .px(px(18.))
        .text_size(px(18.))
        .whitespace_nowrap()
        .child(row)
}

fn results(snap: &Snapshot, state: &SearchState, engine: &EngineHandle) -> Div {
    let list = div()
        .flex()
        .flex_col()
        .p(px(LIST_PAD as f32))
        .border_t_1()
        .border_color(theme::color_a(theme::BORDER));
    if state.len() == 0 {
        return list.child(
            div()
                .h(px(ROW_H as f32))
                .flex()
                .items_center()
                .px(px(12.))
                .text_size(px(14.))
                .text_color(theme::color(theme::TEXT_DIM))
                .child(if state.launcher { "No matching windows or apps" } else { "No matching windows" }),
        );
    }
    let n = state.results.len();
    let rows = visible_lines(state).into_iter().filter_map(|line| match line {
        Line::Win(i) => {
            let id = state.results[i];
            let (w, minimized) = match snap.windows.iter().find(|w| w.id == id) {
                Some(w) => (w, false),
                None => (snap.minimized.iter().find(|w| w.id == id)?, true),
            };
            Some(result_row(w, minimized, i == state.selected, engine))
        }
        Line::Apps => Some(heading("Apps")),
        Line::App(k) => Some(app_row(&state.apps[k], n + k == state.selected, engine)),
    });
    list.children(rows)
}

fn heading(text: &'static str) -> Div {
    div()
        .h(px(ROW_H as f32))
        .flex_shrink_0()
        .flex()
        .items_end()
        .pb(px(6.))
        .px(px(10.))
        .text_size(px(12.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme::color(theme::TEXT_DIM))
        .child(text)
}

/// An installed app; opening it launches it.
fn app_row(app: &AppInfo, selected: bool, engine: &EngineHandle) -> Div {
    let engine = engine.clone();
    let target = app.target.clone();
    div()
        .h(px(ROW_H as f32))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(10.))
        .rounded(px(8.))
        .when(selected, |d| d.bg(theme::color_a(SELECTED_BG)))
        .when(!selected, |d| d.hover(|d| d.bg(theme::color_a(theme::BORDER))))
        .on_mouse_down(MouseButton::Left, move |_, _, _| engine.send(Command::LaunchApp(target.clone())))
        .child(labels::launcher_icon(app, 20.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(14.))
                .text_color(theme::color(theme::TEXT))
                .child(SharedString::from(app.name.clone())),
        )
        .child(div().flex_shrink_0().text_size(px(12.)).text_color(theme::color(theme::TEXT_DIM)).child("Open"))
}

/// One result. Minimized windows aren't on the canvas, so they say so;
/// opening one restores it.
fn result_row(w: &WinInfo, minimized: bool, selected: bool, engine: &EngineHandle) -> Div {
    let engine = engine.clone();
    let id = w.id;
    let exe = w.exe.strip_suffix(".exe").unwrap_or(&w.exe);
    let exe = match (minimized, exe.is_empty()) {
        (false, _) => exe.to_string(),
        (true, true) => "minimized".to_string(),
        (true, false) => format!("{exe} \u{00b7} minimized"),
    };
    div()
        .h(px(ROW_H as f32))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(10.))
        .rounded(px(8.))
        .when(selected, |d| d.bg(theme::color_a(SELECTED_BG)))
        .when(!selected, |d| d.hover(|d| d.bg(theme::color_a(theme::BORDER))))
        .on_mouse_down(MouseButton::Left, move |_, _, _| engine.send(Command::FocusWindow(id)))
        .child(labels::app_icon(w, 20.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(14.))
                .text_color(theme::color(theme::TEXT))
                .child(SharedString::from(w.title.clone())),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(12.))
                .text_color(theme::color(theme::TEXT_DIM))
                .child(SharedString::from(exe)),
        )
}

fn footer(state: &SearchState) -> Div {
    let mut count = match state.results.len() {
        1 => "1 window".to_string(),
        n => format!("{n} windows"),
    };
    match state.apps.len() {
        0 => {}
        1 => count.push_str(" \u{00b7} 1 app"),
        n => count.push_str(&format!(" \u{00b7} {n} apps")),
    }
    div()
        .h(px(FOOTER_H as f32))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_between()
        .px(px(16.))
        .border_t_1()
        .border_color(theme::color_a(theme::BORDER))
        .text_size(px(12.))
        .text_color(theme::color(theme::TEXT_DIM))
        .child(if state.switcher {
            "Tab next \u{00b7} Shift+Tab back \u{00b7} let go of Alt to switch"
        } else {
            "\u{2191}\u{2193} choose \u{00b7} Enter open \u{00b7} Esc close"
        })
        .child(SharedString::from(count))
}

/// Selected row background: the accent at low alpha.
const SELECTED_BG: u32 = (theme::ACCENT << 8) | 0x38;

/// An accent ring just outside the window: the thumbnail covers its inside.
fn ring(snap: &Snapshot, w: &WinInfo, scale: f32) -> AnyElement {
    let b = win_bounds(snap, w, scale);
    let out = px(5.);
    div()
        .absolute()
        .left(b.origin.x - out)
        .top(b.origin.y - out)
        .w(b.size.width + out * 2.)
        .h(b.size.height + out * 2.)
        .rounded(px(9.))
        .border_2()
        .border_color(theme::color(theme::ACCENT))
        .into_any_element()
}

/// Typing edits the query, ↑/↓ (and Tab) move the selection, Enter opens
/// the selected window and Esc closes the search.
pub fn on_key(ev: &KeyDownEvent, engine: &EngineHandle) {
    if !ACTIVE.get() {
        return;
    }
    let k = &ev.keystroke;
    let m = k.modifiers;
    match k.key.as_str() {
        "escape" => engine.send(Command::CloseSearch),
        // The overlay is a real, focusable window while searching; Alt+F4
        // just closes the search.
        "f4" if m.alt => engine.send(Command::CloseSearch),
        "enter" => engine.send(Command::SearchAccept),
        "up" => engine.send(Command::SearchMove(-1)),
        "down" => engine.send(Command::SearchMove(1)),
        "tab" => engine.send(Command::SearchMove(if m.shift { -1 } else { 1 })),
        "backspace" if m.control => edit(engine, |q| {
            let keep = q.trim_end().trim_end_matches(|c: char| !c.is_whitespace()).len();
            q.truncate(keep);
        }),
        "backspace" => edit(engine, |q| {
            q.pop();
        }),
        _ => {
            // Ctrl+Alt is AltGr on many layouts and types real characters.
            let plain = m.control == m.alt && !m.platform;
            if let Some(c) = k.key_char.as_deref().filter(|c| plain && !c.chars().any(char::is_control)) {
                edit(engine, |q| q.push_str(c));
            }
        }
    }
}

fn edit(engine: &EngineHandle, f: impl FnOnce(&mut String)) {
    let changed = QUERY.with(|q| {
        let mut q = q.borrow_mut();
        let before = q.clone();
        f(&mut q);
        (*q != before).then(|| q.clone())
    });
    if let Some(q) = changed {
        engine.send(Command::SearchQuery(q));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: isize, title: &str, exe: &str) -> WinInfo {
        WinInfo { id, title: title.into(), exe: exe.into(), x: 0.0, y: 0.0, w: 100.0, h: 100.0, focused: false }
    }

    fn wins() -> Vec<WinInfo> {
        vec![
            win(1, "main.rs - apeiron - Visual Studio Code", "code.exe"),
            win(2, "Inbox - Mail", "olk.exe"),
            win(3, "Codeberg - Firefox", "firefox.exe"),
            win(4, "Downloads", "explorer.exe"),
            win(5, "Terminal", "windowsterminal.exe"),
        ]
    }

    #[test]
    fn empty_query_keeps_window_order() {
        assert_eq!(rank("", &wins()), vec![1, 2, 3, 4, 5]);
        assert_eq!(rank("   ", &wins()), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn prefix_matches_come_first() {
        // Title prefix (3), exe prefix (1), then nothing else contains "code".
        assert_eq!(rank("code", &wins()), vec![3, 1]);
        // A word in a title starting with the query beats a plain substring.
        let ws = vec![win(10, "Notepad - bestudio", "a.exe"), win(11, "Visual Studio", "b.exe")];
        assert_eq!(rank("studio", &ws), vec![11, 10]);
        assert_eq!(rank("in", &wins()), vec![2, 1, 5]);
    }

    #[test]
    fn case_insensitive_and_trimmed() {
        assert_eq!(rank("  TERM ", &wins()), vec![5]);
        assert_eq!(rank("VISUAL studio", &wins()), vec![1]);
    }

    #[test]
    fn matches_exe_names() {
        assert_eq!(rank("explorer", &wins()), vec![4]);
        assert_eq!(rank("olk", &wins()), vec![2]);
        // Substring inside an exe name ranks last.
        assert_eq!(rank("plor", &wins()), vec![4]);
    }

    #[test]
    fn ties_keep_window_order() {
        let ws = vec![win(7, "Notes b", "a.exe"), win(8, "Notes a", "b.exe"), win(9, "My notes", "c.exe")];
        assert_eq!(rank("notes", &ws), vec![7, 8, 9]);
    }

    #[test]
    fn no_match_is_empty() {
        assert!(rank("zzz", &wins()).is_empty());
    }

    fn app(name: &str) -> AppInfo {
        AppInfo { name: name.into(), target: format!("shell:AppsFolder\\{name}") }
    }

    fn names(v: Vec<AppInfo>) -> Vec<String> {
        v.into_iter().map(|a| a.name).collect()
    }

    #[test]
    fn apps_rank_by_how_they_match() {
        let apps = vec![app("Calculator"), app("Visual Studio Code"), app("Code"), app("Notepad"), app("Snipping Tool"), app("Microsoft Edge")];
        // Prefix (shortest first), then a word start.
        assert_eq!(names(rank_apps("code", &apps)), vec!["Code", "Visual Studio Code"]);
        // Initials, then a plain substring.
        assert_eq!(names(rank_apps("vsc", &apps)), vec!["Visual Studio Code"]);
        assert_eq!(names(rank_apps("ip", &apps)), vec!["Snipping Tool"]);
        assert_eq!(names(rank_apps("  EDGE ", &apps)), vec!["Microsoft Edge"]);
        assert!(rank_apps("zzz", &apps).is_empty());
    }

    #[test]
    fn apps_need_a_query_and_are_capped() {
        let apps: Vec<AppInfo> = (0..50).map(|i| app(&format!("App {i}"))).collect();
        assert!(rank_apps("", &apps).is_empty());
        assert!(rank_apps("   ", &apps).is_empty());
        assert_eq!(rank_apps("app", &apps).len(), MAX_APPS);
    }

    #[test]
    fn apps_follow_windows_under_a_heading() {
        let s = SearchState { query: "x".into(), results: vec![7, 8], apps: vec![app("A"), app("B")], ..Default::default() };
        assert_eq!(lines(&s), vec![Line::Win(0), Line::Win(1), Line::Apps, Line::App(0), Line::App(1)]);
        assert_eq!(s.len(), 4);
        let s = SearchState { selected: 3, ..s };
        assert_eq!(s.selected_app().map(|a| a.name.as_str()), Some("B"));
        // Long lists scroll to keep the selection in view.
        let many = SearchState { results: (0..10).collect(), apps: vec![app("A")], selected: 10, ..Default::default() };
        let shown = visible_lines(&many);
        assert_eq!(shown.len(), MAX_ROWS);
        assert_eq!(shown.last(), Some(&Line::App(0)));
        assert_eq!(visible_lines(&SearchState { selected: 0, ..many })[0], Line::Win(0));
    }

    #[test]
    fn panel_grows_with_results() {
        let state = |n: usize, q: &str| SearchState { query: q.into(), results: (0..n as isize).collect(), ..Default::default() };
        let height = |s: &SearchState| {
            let (_, t, _, b) = panel_rect(s, 1920.0, 1.0);
            b - t
        };
        assert_eq!(height(&state(0, "")), FIELD_H + FOOTER_H);
        assert!(height(&state(0, "x")) > height(&state(0, "")));
        assert!(height(&state(3, "")) > height(&state(1, "")));
        assert_eq!(height(&state(50, "")), height(&state(MAX_ROWS, "")));
        let (l, _, r, _) = panel_rect(&state(1, ""), 1920.0, 1.5);
        assert!(((l + r) / 2.0 - 960.0).abs() < 1e-9);
    }
}
