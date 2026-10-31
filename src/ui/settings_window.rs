//! The settings window, opened from the tray: one calm column of toggles and
//! steppers that apply the moment they change, a shortcut reference and a
//! couple of actions. Only one is ever open.

use gpui::prelude::FluentBuilder;
use gpui::*;
use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
};
use windows::Win32::UI::WindowsAndMessaging::{IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE};

use super::theme;
use crate::settings::{GestureAction, self, Background, Limits, Settings};
use crate::shared::{Command, EngineHandle, Shared};

const CARD: u32 = 0x1a1b20;
const CONTROL: u32 = 0x2a2c33;
const CONTROL_HOVER: u32 = 0x353840;

/// The open settings window, if any.
struct Open(WindowHandle<SettingsView>);

impl Global for Open {}

pub fn open(cx: &mut App, shared: &Shared, engine: &EngineHandle) {
    if let Some(&Open(handle)) = cx.try_global::<Open>() {
        if handle.update(cx, |_, window, _| bring_to_front(window)).is_ok() {
            return;
        }
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(460.), px(720.)), cx)),
        window_min_size: Some(size(px(380.), px(320.))),
        titlebar: Some(TitlebarOptions { title: Some("Apeiron settings".into()), ..Default::default() }),
        ..Default::default()
    };
    let (shared, engine) = (shared.clone(), engine.clone());
    let opened = cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| SettingsView { shared, engine, focus: cx.focus_handle(), layout_reset: false });
        window.focus(&view.read(cx).focus);
        view
    });
    match opened {
        Ok(handle) => {
            let _ = handle.update(cx, |_, window, _| dark_caption(window));
            cx.set_global(Open(handle));
        }
        Err(e) => log!("couldn't open the settings window: {e}"),
    }
}

fn bring_to_front(window: &mut Window) {
    if let Some(hwnd) = super::hwnd_of(window) {
        let hwnd = HWND(hwnd as *mut _);
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(hwnd);
        }
    }
    window.activate_window();
}

/// Paints the title bar in the window's own colours instead of the system's.
fn dark_caption(window: &mut Window) {
    let Some(hwnd) = super::hwnd_of(window) else { return };
    let hwnd = HWND(hwnd as *mut _);
    // COLORREF is 0x00BBGGRR.
    let bgr = |c: u32| COLORREF(((c & 0xff) << 16) | (c & 0xff00) | ((c >> 16) & 0xff));
    unsafe {
        let dark = windows::core::BOOL::from(true);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
        // Windows 11 only; older systems ignore these.
        let caption = bgr(theme::BG);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &caption as *const _ as _, 4);
        let text = bgr(theme::TEXT);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_TEXT_COLOR, &text as *const _ as _, 4);
    }
}

struct SettingsView {
    shared: Shared,
    engine: EngineHandle,
    focus: FocusHandle,
    /// "Reset saved layout" was pressed while this window was open.
    layout_reset: bool,
}

impl SettingsView {
    /// Edits the shared settings, then saves them and tells the engine.
    fn apply(&mut self, cx: &mut Context<Self>, edit: impl FnOnce(&mut Settings)) {
        let (before, after) = {
            let mut s = self.shared.settings.write().unwrap_or_else(|e| e.into_inner());
            let before = s.clone();
            edit(&mut s);
            *s = s.clone().sanitized();
            (before, s.clone())
        };
        if before == after {
            return;
        }
        after.save();
        if before.autostart != after.autostart {
            crate::autostart::sync(&after);
        }
        self.engine.send(Command::SettingsChanged);
        cx.notify();
    }

    fn toggle(
        &self,
        id: &'static str,
        label: &'static str,
        hint: &'static str,
        on: bool,
        field: fn(&mut Settings) -> &mut bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let knob = div().size(px(16.)).rounded_full().bg(theme::color(if on { 0xffffff } else { theme::TEXT_DIM }));
        let switch = div()
            .id(id)
            .flex_none()
            .w(px(36.))
            .h(px(20.))
            .p(px(2.))
            .flex()
            .items_center()
            .when(on, |d| d.justify_end())
            .rounded_full()
            .cursor_pointer()
            .bg(theme::color(if on { theme::ACCENT } else { CONTROL }))
            .hover(|s| s.opacity(0.85))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.apply(cx, |s| {
                    let v = field(s);
                    *v = !*v;
                })
            }))
            .child(knob);
        row(label, hint).child(switch)
    }

    #[allow(clippy::too_many_arguments)]
    fn stepper(
        &self,
        id: &'static str,
        label: &'static str,
        hint: &'static str,
        value: f64,
        limits: Limits,
        format: fn(f64) -> String,
        field: fn(&mut Settings) -> &mut f64,
        cx: &mut Context<Self>,
    ) -> Div {
        let button = |dir: f64, glyph: &'static str, enabled: bool| {
            div()
                .id((id, usize::from(dir > 0.0)))
                .size(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .bg(theme::color(CONTROL))
                .text_size(px(15.))
                .when(enabled, |d| {
                    d.cursor_pointer().hover(|s| s.bg(theme::color(CONTROL_HOVER))).on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            this.apply(cx, |s| {
                                let v = field(s);
                                *v = limits.step(*v, dir);
                            })
                        },
                    ))
                })
                // Dimmed but still legible, so a limit reads as a limit.
                .when(!enabled, |d| d.text_color(theme::color(theme::TEXT_DIM).opacity(0.4)))
                .child(glyph)
        };
        let control = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(button(-1.0, "−", !limits.at_min(value)))
            .child(div().w(px(64.)).flex().justify_center().child(format(value)))
            .child(button(1.0, "+", !limits.at_max(value)));
        row(label, hint).child(control)
    }

    /// Picks what a gesture does: the action's name between arrows that step
    /// through the choices.
    fn picker(
        &self,
        id: &'static str,
        label: &'static str,
        value: GestureAction,
        field: fn(&mut Settings) -> &mut GestureAction,
        cx: &mut Context<Self>,
    ) -> Div {
        let button = |dir: i32, glyph: &'static str| {
            div()
                .id((id, usize::from(dir > 0)))
                .size(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .bg(theme::color(CONTROL))
                .text_size(px(15.))
                .cursor_pointer()
                .hover(|s| s.bg(theme::color(CONTROL_HOVER)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.apply(cx, |s| {
                        let v = field(s);
                        *v = v.cycle(dir);
                    })
                }))
                .child(glyph)
        };
        let shown = if value == GestureAction::Nothing { theme::TEXT_DIM } else { theme::TEXT };
        let control = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(button(-1, "‹"))
            .child(div().w(px(132.)).flex().justify_center().text_color(theme::color(shown)).child(value.label()))
            .child(button(1, "›"));
        row(label, "").child(control)
    }

    /// A segmented control picking the canvas background.
    fn background(&self, value: Background, cx: &mut Context<Self>) -> Div {
        let options = [("Wallpaper", Background::Wallpaper), ("Dot grid", Background::DotGrid)];
        let segments = options.into_iter().enumerate().map(|(i, (name, option))| {
            let on = option == value;
            div()
                .id(("background", i))
                .px(px(10.))
                .py(px(4.))
                .rounded(px(5.))
                .when(on, |d| d.bg(theme::color(theme::ACCENT)).text_color(theme::color(0xffffff)))
                .when(!on, |d| {
                    d.cursor_pointer().hover(|s| s.bg(theme::color(CONTROL_HOVER))).on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| this.apply(cx, |s| s.background = option),
                    ))
                })
                .child(name)
        });
        row("Background", "Your wallpaper as one endless scene, or a calm dot grid.").child(
            div()
                .flex_none()
                .flex()
                .p(px(2.))
                .gap(px(2.))
                .rounded(px(7.))
                .bg(theme::color(CONTROL))
                .children(segments),
        )
    }

    fn action(
        &self,
        id: &'static str,
        label: &'static str,
        hint: &'static str,
        button: &'static str,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Div {
        row(label, hint).child(
            div()
                .id(id)
                .flex_none()
                .px(px(12.))
                .py(px(5.))
                .rounded(px(6.))
                .bg(theme::color(CONTROL))
                .cursor_pointer()
                .hover(|s| s.bg(theme::color(CONTROL_HOVER)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| on_click(this, cx)))
                .child(button),
        )
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = self.shared.settings();
        let times = |v: f64| format!("{v:.2}×");

        let gestures = card(vec![
            self.toggle(
                "three",
                "Three fingers anywhere",
                "Pan and pinch the canvas from inside any app.",
                s.three_finger,
                |s| &mut s.three_finger,
                cx,
            ),
            self.toggle(
                "two",
                "Two fingers on the desktop",
                "Pan and pinch while the pointer is over empty canvas.",
                s.two_finger_canvas,
                |s| &mut s.two_finger_canvas,
                cx,
            ),
            self.stepper("pan", "Pan speed", "", s.pan_speed, settings::PAN_SPEED, times, |s| &mut s.pan_speed, cx),
            self.stepper(
                "pinch",
                "Pinch speed",
                "",
                s.pinch_speed,
                settings::PINCH_SPEED,
                times,
                |s| &mut s.pinch_speed,
                cx,
            ),
            self.stepper(
                "flick",
                "Flick momentum",
                "How far the canvas glides after a quick swipe.",
                s.flick,
                settings::FLICK,
                |v| if v <= 0.0 { "Off".into() } else { format!("{v:.2} s") },
                |s| &mut s.flick,
                cx,
            ),
        ]);

        let custom = card(vec![
            self.picker("four_up", "Four fingers up", s.four_up, |s| &mut s.four_up, cx),
            self.picker("four_down", "Four fingers down", s.four_down, |s| &mut s.four_down, cx),
            self.picker("four_left", "Four fingers left", s.four_left, |s| &mut s.four_left, cx),
            self.picker("four_right", "Four fingers right", s.four_right, |s| &mut s.four_right, cx),
            self.picker("four_tap", "Four-finger tap", s.four_tap, |s| &mut s.four_tap, cx),
            self.picker("three_tap", "Three-finger tap", s.three_tap, |s| &mut s.three_tap, cx),
        ]);

        let backdrop = match s.background {
            Background::Wallpaper => self.stepper(
                "parallax",
                "Wallpaper depth",
                "How far the wallpaper drifts as you pan: lower feels further away.",
                s.parallax,
                settings::PARALLAX,
                |v| if v <= 0.0 { "Still".into() } else { format!("{:.0}%", v * 100.0) },
                |s| &mut s.parallax,
                cx,
            ),
            Background::DotGrid => self.toggle("grid", "Dots", "", s.show_grid, |s| &mut s.show_grid, cx),
        };
        let canvas = card(vec![
            self.background(s.background, cx),
            backdrop,
            self.toggle(
                "labels",
                "Window labels",
                "Icons and titles above windows when zoomed out.",
                s.show_labels,
                |s| &mut s.show_labels,
                cx,
            ),
            self.toggle(
                "minimap",
                "Minimap",
                "A live map of the whole canvas in the top-right corner.",
                s.show_minimap,
                |s| &mut s.show_minimap,
                cx,
            ),
            self.stepper(
                "gap",
                "Space between windows",
                "Left between windows when tidying up.",
                s.gap,
                settings::GAP,
                |v| format!("{v:.0} px"),
                |s| &mut s.gap,
                cx,
            ),
            self.action(
                "tidy",
                "Tidy up",
                "Arrange every window in rows, keeping their order.",
                "Tidy up now",
                |this, _| this.engine.send(Command::Tidy),
                cx,
            ),
        ]);

        let reset = if self.layout_reset {
            row("Saved layout", "Cleared. Nothing is saved again until the app restarts.").child(
                div().flex_none().px(px(12.)).py(px(5.)).text_color(theme::color(theme::TEXT_DIM)).child("Done"),
            )
        } else {
            self.action(
                "reset",
                "Saved layout",
                "Forget where windows were placed on the canvas.",
                "Reset saved layout",
                |this, cx| {
                    this.engine.send(Command::ResetLayout);
                    this.layout_reset = true;
                    cx.notify();
                },
                cx,
            )
        };
        let general = card(vec![
            self.toggle("autostart", "Start with Windows", "", s.autostart, |s| &mut s.autostart, cx),
            self.toggle(
                "auto_update",
                "Update automatically",
                "Download new versions in the background; they install on the next start.",
                s.auto_update,
                |s| &mut s.auto_update,
                cx,
            ),
            self.toggle(
                "remember",
                "Remember layout",
                "Put windows back in their canvas spots after a restart.",
                s.remember_layout,
                |s| &mut s.remember_layout,
                cx,
            ),
            reset,
        ]);

        let shortcuts = card(vec![
            shortcut("Overview", &["Ctrl", "Alt", "Z"]),
            shortcut("Find a window", &["Ctrl", "Alt", "F"]),
            shortcut("Tidy up", &["Ctrl", "Alt", "T"]),
            shortcut("Back to your windows", &["Esc"]),
            shortcut("Quit (puts every window back)", &["Ctrl", "Alt", "Q"]),
        ]);

        div()
            .id("settings")
            .track_focus(&self.focus)
            .on_key_down(|ev: &KeyDownEvent, window, _| {
                if ev.keystroke.key == "escape" {
                    window.remove_window();
                }
            })
            .size_full()
            .overflow_y_scroll()
            .bg(theme::color(theme::BG))
            .text_color(theme::color(theme::TEXT))
            .text_size(px(13.))
            .child(
                div()
                    .w_full()
                    .max_w(px(560.))
                    .mx_auto()
                    .flex()
                    .flex_col()
                    .px(px(20.))
                    .pt(px(18.))
                    .pb(px(24.))
                    .child(div().text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).child("Settings"))
                    .child(dim("Changes apply right away.").mt(px(2.)))
                    .child(section("Gestures"))
                    .child(gestures)
                    .child(section("Custom gestures"))
                    .child(dim("Four-finger gestures need Windows' own turned off: Settings > Bluetooth & devices > Touchpad > Four-finger gestures > Nothing."))
                    .child(custom)
                    .child(section("Canvas"))
                    .child(canvas)
                    .child(section("General"))
                    .child(general)
                    .child(section("Shortcuts"))
                    .child(shortcuts)
                    .child(
                        dim(concat!("Apeiron ", env!("CARGO_PKG_VERSION")))
                            .mt(px(20.))
                            .flex()
                            .justify_center()
                            .text_size(px(11.)),
                    ),
            )
    }
}

fn dim(text: impl Into<SharedString>) -> Div {
    div().text_color(theme::color(theme::TEXT_DIM)).child(text.into())
}

fn section(title: &'static str) -> Div {
    dim(title).mt(px(22.)).mb(px(8.)).px(px(4.)).text_size(px(12.)).font_weight(FontWeight::SEMIBOLD)
}

/// Rows stacked in a rounded panel with hairlines between them.
fn card(rows: Vec<Div>) -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(10.))
        .border_1()
        .border_color(theme::color_a(theme::BORDER))
        .bg(theme::color(CARD))
        .children(
            rows.into_iter()
                .enumerate()
                .map(|(i, r)| r.when(i > 0, |r| r.border_t_1().border_color(theme::color_a(theme::BORDER)))),
        )
}

/// A label (with an optional hint under it) on the left; the caller adds the
/// control on the right.
fn row(label: &'static str, hint: &'static str) -> Div {
    let text = div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(label)
        .when(!hint.is_empty(), |d| d.child(dim(hint).text_size(px(12.))));
    div().flex().items_center().gap(px(16.)).px(px(16.)).py(px(11.)).min_h(px(48.)).child(text)
}

fn shortcut(label: &'static str, keys: &[&'static str]) -> Div {
    let keys = keys.iter().map(|k| {
        div()
            .px(px(7.))
            .py(px(2.))
            .rounded(px(5.))
            .bg(theme::color(CONTROL))
            .border_1()
            .border_color(theme::color_a(theme::BORDER))
            .text_size(px(12.))
            .child(*k)
    });
    row(label, "").child(div().flex_none().flex().gap(px(4.)).children(keys))
}
