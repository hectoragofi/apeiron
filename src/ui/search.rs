//! The search box and its results, shown over the overview.
//!
//! The engine owns the search (query, ranked results, selection) and dims
//! the thumbnails of windows that don't match; this module draws the panel,
//! rings the selected window and turns keystrokes into commands. The panel
//! sits under the DWM thumbnails like everything GPUI draws, so the engine
//! keeps windows out of [`panel_rect`] while searching.

use std::cell::{Cell, RefCell};

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, AnyElement, Div, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement, SharedString,
    Styled,
};

use super::canvas_view::win_bounds;
use super::{labels, theme};
use crate::shared::{Command, EngineHandle, SearchState, Snapshot, WinInfo};

// Panel geometry, logical pixels.
const PANEL_W: f64 = 560.0;
const PANEL_TOP: f64 = 24.0;
const FIELD_H: f64 = 52.0;
const ROW_H: f64 = 36.0;
const MAX_ROWS: usize = 5;
const LIST_PAD: f64 = 6.0;
const FOOTER_H: f64 = 28.0;

thread_local! {
    /// What the user has typed. Kept here rather than read back from the
    /// snapshot so fast typing never builds on a stale query.
    static QUERY: RefCell<String> = const { RefCell::new(String::new()) };
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
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
    } else if title.match_indices(q).any(|(i, _)| title[..i].ends_with(|c: char| !c.is_alphanumeric())) {
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

fn list_height(state: &SearchState) -> f64 {
    let rows = match state.results.len() {
        0 if state.query.trim().is_empty() => return 0.0,
        // Room for "No matching windows".
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
    ACTIVE.set(true);
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
        .child(if state.switcher { switcher_title() } else { field(&state.query) })
        .when(list_height(state) > 0.0, |p| p.child(results(snap, state, engine)))
        .child(footer(state));

    Some(
        div()
            .absolute()
            .size_full()
            .children(selected.map(|w| ring(snap, w, scale)))
            .child(panel)
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

fn field(query: &str) -> Div {
    let text = if query.is_empty() {
        div().text_color(theme::color(theme::TEXT_DIM)).child("Find a window")
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
    if state.results.is_empty() {
        return list.child(
            div()
                .h(px(ROW_H as f32))
                .flex()
                .items_center()
                .px(px(12.))
                .text_size(px(14.))
                .text_color(theme::color(theme::TEXT_DIM))
                .child("No matching windows"),
        );
    }
    // Scroll just enough to keep the selection in view.
    let first = (state.selected + 1).saturating_sub(MAX_ROWS);
    let rows = state.results.iter().enumerate().skip(first).take(MAX_ROWS).filter_map(|(i, id)| {
        let (w, minimized) = match snap.windows.iter().find(|w| w.id == *id) {
            Some(w) => (w, false),
            None => (snap.minimized.iter().find(|w| w.id == *id)?, true),
        };
        Some(result_row(w, minimized, i == state.selected, engine))
    });
    list.children(rows)
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
    let count = match state.results.len() {
        1 => "1 window".to_string(),
        n => format!("{n} windows"),
    };
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

    #[test]
    fn panel_grows_with_results() {
        let state = |n: usize, q: &str| SearchState { query: q.into(), results: (0..n as isize).collect(), selected: 0, switcher: false };
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
