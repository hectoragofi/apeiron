//! The settings window, opened from the tray: a sidebar of pages (General,
//! Touchpad, Keyboard, Canvas, Smart camera, About), each a few labelled
//! groups of controls that apply the moment they change. Only one window is
//! ever open.

use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::*;
use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
};
use windows::Win32::UI::WindowsAndMessaging::{IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE};

use super::theme;
use crate::hotkey;
use crate::settings::{self, Background, GestureAction, Limits, Settings};
use crate::shared::{Command, EngineHandle, Shared};

const SIDEBAR: u32 = 0x16171b;
const CARD: u32 = 0x1b1c21;
const CONTROL: u32 = 0x2a2c33;
const CONTROL_HOVER: u32 = 0x353840;
const DANGER: u32 = 0xff7a7a;
const ICONS: &str = "Segoe Fluent Icons";

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
        window_bounds: Some(WindowBounds::centered(size(px(860.), px(620.)), cx)),
        window_min_size: Some(size(px(640.), px(420.))),
        titlebar: Some(TitlebarOptions { title: Some(crate::shared::SETTINGS_TITLE.into()), ..Default::default() }),
        ..Default::default()
    };
    let (shared, engine) = (shared.clone(), engine.clone());
    let opened = cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| SettingsView {
            shared,
            engine,
            focus: cx.focus_handle(),
            page: preview_page(),
            picking: None,
            recording: None,
            record_hint: false,
            layout_reset: false,
        });
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
        let caption = bgr(SIDEBAR);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &caption as *const _ as _, 4);
        let text = bgr(theme::TEXT);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_TEXT_COLOR, &text as *const _ as _, 4);
    }
}

fn preview_page() -> Page {
    #[cfg(debug_assertions)]
    if let Ok(name) = std::env::var("APEIRON_PAGE") {
        if let Some(p) = Page::ALL.into_iter().find(|p| p.name().eq_ignore_ascii_case(&name)) {
            return p;
        }
    }
    Page::General
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    General,
    Touchpad,
    Mouse,
    Keyboard,
    Canvas,
    Camera,
    About,
}

impl Page {
    const ALL: [Page; 7] =
        [Page::General, Page::Touchpad, Page::Mouse, Page::Keyboard, Page::Canvas, Page::Camera, Page::About];

    fn name(self) -> &'static str {
        match self {
            Page::General => "General",
            Page::Touchpad => "Touchpad",
            Page::Mouse => "Mouse",
            Page::Keyboard => "Keyboard",
            Page::Canvas => "Canvas",
            Page::Camera => "Smart camera",
            Page::About => "About",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Page::General => "\u{E713}",
            Page::Touchpad => "\u{E7C9}",
            Page::Mouse => "\u{E962}",
            Page::Keyboard => "\u{E765}",
            Page::Canvas => "\u{E8B9}",
            Page::Camera => "\u{E71E}",
            Page::About => "\u{E946}",
        }
    }

    fn blurb(self) -> &'static str {
        match self {
            Page::General => "Startup, updates and the saved layout.",
            Page::Touchpad => "How the canvas moves under your fingers, and what extra gestures do.",
            Page::Mouse => "Middle-drag to pan, and what the buttons and screen corners do.",
            Page::Keyboard => "Global shortcuts and the Alt+Tab window switcher.",
            Page::Canvas => "What the canvas looks like and how windows are arranged on it.",
            Page::Camera => "Small helpers that tidy up where the camera and windows come to rest.",
            Page::About => "",
        }
    }
}

/// A rebindable shortcut.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Overview,
    Find,
    Tidy,
    Quit,
}

impl Key {
    const ALL: [Key; 4] = [Key::Overview, Key::Find, Key::Tidy, Key::Quit];

    fn field(self) -> fn(&mut Settings) -> &mut String {
        match self {
            Key::Overview => |s| &mut s.key_overview,
            Key::Find => |s| &mut s.key_find,
            Key::Tidy => |s| &mut s.key_tidy,
            Key::Quit => |s| &mut s.key_quit,
        }
    }

    fn get(self, s: &Settings) -> String {
        let mut s = s.clone();
        (self.field())(&mut s).clone()
    }

    fn id(self) -> &'static str {
        match self {
            Key::Overview => "key_overview",
            Key::Find => "key_find",
            Key::Tidy => "key_tidy",
            Key::Quit => "key_quit",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Key::Overview => "Overview",
            Key::Find => "Find a window",
            Key::Tidy => "Tidy up",
            Key::Quit => "Quit",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Key::Overview => "Zoom out to every window, or back in.",
            Key::Find => "Search windows by title or app.",
            Key::Tidy => "Arrange every window in neat rows.",
            Key::Quit => "Puts every window back on screen first.",
        }
    }
}

struct SettingsView {
    shared: Shared,
    engine: EngineHandle,
    focus: FocusHandle,
    page: Page,
    /// The gesture whose choices are spread out below it.
    picking: Option<&'static str>,
    /// The shortcut waiting for a key press.
    recording: Option<Key>,
    /// The last press while recording can't be a shortcut.
    record_hint: bool,
    /// "Reset saved layout" was pressed while this window was open.
    layout_reset: bool,
}

impl Drop for SettingsView {
    fn drop(&mut self) {
        // Closed mid-recording: give the shortcuts back.
        if self.recording.is_some() {
            self.engine.send(Command::SuspendHotkeys(false));
        }
    }
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
        crate::tray::show_keys(&after);
        self.engine.send(Command::SettingsChanged);
        cx.notify();
    }

    fn go(&mut self, page: Page, cx: &mut Context<Self>) {
        self.stop_recording(cx);
        self.page = page;
        self.picking = None;
        cx.notify();
    }

    fn start_recording(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) {
        if self.recording.is_none() {
            self.engine.send(Command::SuspendHotkeys(true));
        }
        self.recording = Some(key);
        self.record_hint = false;
        window.focus(&self.focus);
        cx.notify();
    }

    fn stop_recording(&mut self, cx: &mut Context<Self>) {
        if self.recording.take().is_none() {
            return;
        }
        self.record_hint = false;
        self.engine.send(Command::SuspendHotkeys(false));
        // Whether Windows took the new keys is known a moment later.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(200)).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
        cx.notify();
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let Some(key) = self.recording else {
            if k.key == "escape" {
                if self.picking.take().is_some() {
                    cx.notify();
                } else {
                    window.remove_window();
                }
            }
            return;
        };
        cx.stop_propagation();
        let m = &k.modifiers;
        let bare = !(m.control || m.alt || m.shift || m.platform);
        match k.key.as_str() {
            "escape" if bare => self.stop_recording(cx),
            "backspace" | "delete" if bare => {
                self.apply(cx, |s| (key.field())(s).clear());
                self.stop_recording(cx);
            }
            name => match hotkey::from_press(name, m.control, m.alt, m.shift, m.platform) {
                Some(combo) => {
                    self.apply(cx, |s| {
                        // One shortcut per action: whatever had it lets go.
                        for other in Key::ALL {
                            let f = (other.field())(s);
                            if *f == combo {
                                f.clear();
                            }
                        }
                        *(key.field())(s) = combo;
                    });
                    self.stop_recording(cx);
                }
                None => {
                    let modifier = matches!(name, "control" | "alt" | "shift" | "platform" | "function" | "win");
                    self.record_hint = !modifier;
                    cx.notify();
                }
            },
        }
    }

    // ---- controls --------------------------------------------------------------

    fn toggle(
        &self,
        id: &'static str,
        label: &'static str,
        hint: &'static str,
        on: bool,
        field: fn(&mut Settings) -> &mut bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let knob = div().size(px(16.)).rounded_full().bg(theme::color(if on { 0xffffff } else { 0xb8bcc6 })).shadow_sm();
        let switch = div()
            .id(id)
            .flex_none()
            .w(px(40.))
            .h(px(22.))
            .p(px(3.))
            .flex()
            .items_center()
            .when(on, |d| d.justify_end())
            .rounded_full()
            .cursor_pointer()
            .bg(theme::color(if on { theme::ACCENT } else { CONTROL }))
            .border_1()
            .border_color(if on { theme::color(theme::ACCENT) } else { theme::color_a(theme::BORDER) })
            .hover(|s| s.opacity(0.88))
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
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .font_family(ICONS)
                .text_size(px(10.))
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
                .when(!enabled, |d| d.text_color(theme::color(theme::TEXT_DIM).opacity(0.35)))
                .child(glyph)
        };
        // How far along its range the value is, as a thin bar under it.
        let frac = ((value - limits.min) / (limits.max - limits.min)).clamp(0.0, 1.0) as f32;
        let readout = div()
            .w(px(72.))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(3.))
            .child(div().text_size(px(13.)).child(format(value)))
            .child(
                div()
                    .w(px(44.))
                    .h(px(2.))
                    .rounded_full()
                    .bg(theme::color_a(theme::BORDER))
                    .child(div().h_full().w(px(44. * frac)).rounded_full().bg(theme::color(theme::ACCENT))),
            );
        let control = div()
            .flex_none()
            .flex()
            .items_center()
            .p(px(2.))
            .rounded(px(8.))
            .bg(theme::color(CONTROL))
            .child(button(-1.0, "\u{E738}", !limits.at_min(value)))
            .child(readout)
            .child(button(1.0, "\u{E710}", !limits.at_max(value)));
        row(label, hint).child(control)
    }

    /// What a gesture does: a button naming the action that spreads the
    /// choices out underneath when clicked.
    fn picker(
        &self,
        id: &'static str,
        glyph: &'static str,
        label: &'static str,
        value: GestureAction,
        field: fn(&mut Settings) -> &mut GestureAction,
        cx: &mut Context<Self>,
    ) -> Div {
        let open = self.picking == Some(id);
        let shown = if value == GestureAction::Nothing { theme::TEXT_DIM } else { theme::TEXT };
        let button = div()
            .id(id)
            .flex_none()
            .min_w(px(168.))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(10.))
            .px(px(12.))
            .py(px(6.))
            .rounded(px(7.))
            .bg(theme::color(if open { CONTROL_HOVER } else { CONTROL }))
            .border_1()
            .border_color(if open { theme::color(theme::ACCENT) } else { theme::color_a(theme::BORDER) })
            .cursor_pointer()
            .hover(|s| s.bg(theme::color(CONTROL_HOVER)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.picking = if this.picking == Some(id) { None } else { Some(id) };
                cx.notify();
            }))
            .child(div().text_color(theme::color(shown)).child(value.label()))
            .child(
                div()
                    .font_family(ICONS)
                    .text_size(px(10.))
                    .text_color(theme::color(theme::TEXT_DIM))
                    .child(if open { "\u{E70E}" } else { "\u{E70D}" }),
            );
        let head = div()
            .flex()
            .items_center()
            .gap(px(16.))
            .px(px(16.))
            .py(px(10.))
            .min_h(px(52.))
            .child(
                div()
                    .flex_none()
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.))
                    .bg(theme::color(CONTROL))
                    .text_size(px(14.))
                    .child(glyph),
            )
            .child(div().flex_1().min_w_0().child(label))
            .child(button);
        let choices = GestureAction::ALL.into_iter().enumerate().map(move |(i, a)| {
            let on = a == value;
            div()
                .id((id, 10 + i))
                .px(px(11.))
                .py(px(5.))
                .rounded(px(6.))
                .text_size(px(12.))
                .border_1()
                .border_color(theme::color_a(theme::BORDER))
                .when(on, |d| d.bg(theme::color(theme::ACCENT)).border_color(theme::color(theme::ACCENT)).text_color(theme::color(0xffffff)))
                .when(!on, |d| {
                    d.bg(theme::color(CONTROL)).cursor_pointer().hover(|s| s.bg(theme::color(CONTROL_HOVER))).on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.picking = None;
                            this.apply(cx, |s| *field(s) = a);
                            cx.notify();
                        }),
                    )
                })
                .child(a.label())
        });
        div().flex().flex_col().child(head).when(open, |d| {
            d.child(div().flex().flex_wrap().gap(px(6.)).px(px(16.)).pb(px(14.)).pl(px(60.)).children(choices))
        })
    }

    /// A segmented control picking the canvas background.
    fn background(&self, value: Background, cx: &mut Context<Self>) -> Div {
        let options = [("Wallpaper", Background::Wallpaper), ("Dot grid", Background::DotGrid)];
        let segments = options.into_iter().enumerate().map(|(i, (name, option))| {
            let on = option == value;
            div()
                .id(("background", i))
                .px(px(12.))
                .py(px(5.))
                .rounded(px(6.))
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
                .p(px(3.))
                .gap(px(2.))
                .rounded(px(8.))
                .bg(theme::color(CONTROL))
                .children(segments),
        )
    }

    /// A shortcut: its key caps in a button that records new ones.
    fn shortcut(&self, key: Key, s: &Settings, taken: &[String], cx: &mut Context<Self>) -> Div {
        let combo = key.get(s);
        let default = key.get(&Settings::default());
        let recording = self.recording == Some(key);
        let is_taken = !combo.is_empty() && taken.contains(&combo);
        let inner = if recording {
            div()
                .text_color(theme::color(theme::ACCENT))
                .child(if self.record_hint { "Add Ctrl, Alt or Win" } else { "Press the new keys…" })
        } else if combo.is_empty() {
            div().text_color(theme::color(theme::TEXT_DIM)).child("Not set")
        } else {
            div().flex().gap(px(4.)).children(hotkey::caps(&combo).into_iter().map(cap))
        };
        let button = div()
            .id(key.id())
            .flex_none()
            .min_w(px(150.))
            .h(px(32.))
            .flex()
            .items_center()
            .justify_center()
            .px(px(8.))
            .rounded(px(7.))
            .bg(theme::color(if recording { CONTROL_HOVER } else { CONTROL }))
            .border_1()
            .border_color(if recording {
                theme::color(theme::ACCENT)
            } else if is_taken {
                theme::color(DANGER)
            } else {
                theme::color_a(theme::BORDER)
            })
            .cursor_pointer()
            .hover(|s| s.bg(theme::color(CONTROL_HOVER)))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                if this.recording == Some(key) {
                    this.stop_recording(cx);
                } else {
                    this.start_recording(key, window, cx);
                }
            }))
            .child(inner);
        let reset = div()
            .id((key.id(), 1usize))
            .flex_none()
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .font_family(ICONS)
            .text_size(px(12.))
            .text_color(theme::color(theme::TEXT_DIM))
            .when(combo != default && !recording, |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(theme::color(CONTROL_HOVER)).text_color(theme::color(theme::TEXT)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let default = key.get(&Settings::default());
                        this.apply(cx, |s| {
                            for other in Key::ALL {
                                let f = (other.field())(s);
                                if *f == default {
                                    f.clear();
                                }
                            }
                            *(key.field())(s) = default;
                        });
                    }))
                    .child("\u{E7A7}")
            });
        let hint = if is_taken { "Another app already uses these keys. Pick others." } else { key.hint() };
        row(key.label(), hint).child(div().flex_none().flex().items_center().gap(px(4.)).child(reset).child(button))
    }

    fn button(
        &self,
        id: &'static str,
        text: &'static str,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .flex_none()
            .px(px(14.))
            .py(px(6.))
            .rounded(px(7.))
            .bg(theme::color(CONTROL))
            .border_1()
            .border_color(theme::color_a(theme::BORDER))
            .cursor_pointer()
            .hover(|s| s.bg(theme::color(CONTROL_HOVER)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| on_click(this, cx)))
            .child(text)
    }

    // ---- pages -----------------------------------------------------------------

    fn general(&mut self, s: &Settings, cx: &mut Context<Self>) -> Vec<Div> {
        let reset = if self.layout_reset {
            row("Forget the saved layout", "Cleared. Nothing is saved again until Apeiron restarts.")
                .child(div().flex_none().px(px(12.)).text_color(theme::color(theme::TEXT_DIM)).child("Done"))
        } else {
            row("Forget the saved layout", "Windows stay where they are; their saved canvas spots are cleared.").child(
                self.button(
                    "reset",
                    "Reset",
                    |this, cx| {
                        this.engine.send(Command::ResetLayout);
                        this.layout_reset = true;
                        cx.notify();
                    },
                    cx,
                ),
            )
        };
        vec![
            section("Startup"),
            card(vec![
                self.toggle("autostart", "Start with Windows", "Apeiron opens when you sign in.", s.autostart, |s| &mut s.autostart, cx),
                self.toggle(
                    "auto_update",
                    "Update automatically",
                    "New versions download in the background and install on the next start.",
                    s.auto_update,
                    |s| &mut s.auto_update,
                    cx,
                ),
            ]),
            section("Layout"),
            card(vec![
                self.toggle(
                    "remember",
                    "Remember layout",
                    "Put windows back in their canvas spots after a restart.",
                    s.remember_layout,
                    |s| &mut s.remember_layout,
                    cx,
                ),
                reset,
            ]),
        ]
    }

    fn mouse(&mut self, s: &Settings, cx: &mut Context<Self>) -> Vec<Div> {
        vec![
            section("Moving around"),
            card(vec![self.toggle(
                "middle_pan",
                "Hold the middle button to pan",
                "From inside any app. Scroll while holding it to zoom; let go zoomed out to browse. While this is on, a middle click reaches apps only when you let go.",
                s.middle_pan,
                |s| &mut s.middle_pan,
                cx,
            )]),
            section("Buttons"),
            note("A button set to Nothing keeps working in apps as usual. The middle click is set to Nothing out of the box, so links still open in new tabs."),
            card(vec![
                self.picker("middle_click", "●", "Middle click", s.middle_click, |s| &mut s.middle_click, cx),
                self.picker("mouse_back", "←", "Back side button", s.mouse_back, |s| &mut s.mouse_back, cx),
                self.picker("mouse_forward", "→", "Forward side button", s.mouse_forward, |s| &mut s.mouse_forward, cx),
            ]),
            section("Hot corners"),
            note("Push the cursor into a corner of the main screen."),
            card(vec![
                self.picker("corner_tl", "↖", "Top left", s.corner_tl, |s| &mut s.corner_tl, cx),
                self.picker("corner_tr", "↗", "Top right", s.corner_tr, |s| &mut s.corner_tr, cx),
                self.picker("corner_bl", "↙", "Bottom left", s.corner_bl, |s| &mut s.corner_bl, cx),
                self.picker("corner_br", "↘", "Bottom right", s.corner_br, |s| &mut s.corner_br, cx),
            ]),
        ]
    }

    fn touchpad(&mut self, s: &Settings, cx: &mut Context<Self>) -> Vec<Div> {
        let times = |v: f64| format!("{v:.2}×");
        vec![
            section("Moving around"),
            card(vec![
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
            ]),
            section("Feel"),
            card(vec![
                self.stepper("pan", "Pan speed", "", s.pan_speed, settings::PAN_SPEED, times, |s| &mut s.pan_speed, cx),
                self.stepper("pinch", "Pinch speed", "", s.pinch_speed, settings::PINCH_SPEED, times, |s| &mut s.pinch_speed, cx),
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
            ]),
            section("Gestures"),
            note(
                "Four-finger gestures only reach Apeiron once Windows' own are off: Settings › Bluetooth & devices › Touchpad › Four-finger gestures › Nothing.",
            ),
            card(vec![
                self.picker("four_up", "↑", "Four fingers up", s.four_up, |s| &mut s.four_up, cx),
                self.picker("four_down", "↓", "Four fingers down", s.four_down, |s| &mut s.four_down, cx),
                self.picker("four_left", "←", "Four fingers left", s.four_left, |s| &mut s.four_left, cx),
                self.picker("four_right", "→", "Four fingers right", s.four_right, |s| &mut s.four_right, cx),
                self.picker("four_tap", "◉", "Four-finger tap", s.four_tap, |s| &mut s.four_tap, cx),
                self.picker("three_tap", "●", "Three-finger tap", s.three_tap, |s| &mut s.three_tap, cx),
            ]),
        ]
    }

    fn keyboard(&mut self, s: &Settings, cx: &mut Context<Self>) -> Vec<Div> {
        let taken = self.shared.hotkeys_taken.read().map(|t| t.clone()).unwrap_or_default();
        let shortcuts = Key::ALL.into_iter().map(|k| self.shortcut(k, s, &taken, cx)).collect();
        vec![
            section("Shortcuts"),
            note("Click a shortcut, then press the new keys. Backspace clears it, Esc cancels."),
            card(shortcuts),
            section("Window switcher"),
            card(vec![self.toggle(
                "alt_tab",
                "Use Apeiron for Alt+Tab",
                "Zooms out to every window; Tab steps through them and letting go of Alt flies there. A quick Alt+Tab switches straight away.",
                s.alt_tab,
                |s| &mut s.alt_tab,
                cx,
            )]),
            section("In the overview"),
            card(vec![
                fixed("Back to your windows", &["Esc"]),
                fixed("Close the selected windows", &["Delete"]),
                fixed("Add to the selection", &["Ctrl", "Click"]),
                fixed("Drag without snapping", &["Alt", "Drag"]),
                fixed("Zoom", &["Ctrl", "Scroll"]),
            ]),
        ]
    }

    fn canvas(&mut self, s: &Settings, cx: &mut Context<Self>) -> Vec<Div> {
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
        vec![
            section("Look"),
            card(vec![
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
                    "A live map of the whole canvas in the top-right corner. Minimized windows show dashed.",
                    s.show_minimap,
                    |s| &mut s.show_minimap,
                    cx,
                ),
            ]),
            section("Arranging"),
            card(vec![
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
                self.toggle(
                    "tidy_on_launch",
                    "Tidy up on launch",
                    "Arrange the windows in rows every time Apeiron starts, instead of keeping where you left them.",
                    s.tidy_on_launch,
                    |s| &mut s.tidy_on_launch,
                    cx,
                ),
                row("Tidy up", "Arrange every window in rows, keeping their order.")
                    .child(self.button("tidy", "Tidy up now", |this, _| this.engine.send(Command::Tidy), cx)),
            ]),
            section("Sticky windows"),
            note(
                "Select two or more windows in the overview (Ctrl-click or drag a box), right-click and pick Stick together. They then drag, move and tidy as one; right-click › Unstick to part them.",
            ),
        ]
    }

    fn camera(&mut self, s: &Settings, cx: &mut Context<Self>) -> Vec<Div> {
        vec![
            section("Camera"),
            card(vec![
                self.toggle(
                    "land_framed",
                    "Land on whole windows",
                    "Coming back to 1:1 centres the window you land on.",
                    s.land_framed,
                    |s| &mut s.land_framed,
                    cx,
                ),
                self.toggle(
                    "magnetic_flick",
                    "Flick to the next window",
                    "A flick stops on the window in that direction.",
                    s.magnetic_flick,
                    |s| &mut s.magnetic_flick,
                    cx,
                ),
                self.toggle(
                    "zoom_fit",
                    "Snap zoom to fit",
                    "A pinch that ends close to fitting a window or group fits it exactly.",
                    s.zoom_fit,
                    |s| &mut s.zoom_fit,
                    cx,
                ),
                self.toggle(
                    "overview_snap",
                    "Settle on groups when zoomed out",
                    "Pans in the overview come to rest centred on nearby windows.",
                    s.overview_snap,
                    |s| &mut s.overview_snap,
                    cx,
                ),
            ]),
            section("Windows"),
            card(vec![
                self.toggle(
                    "window_snap",
                    "Snap windows while dragging",
                    "Line edges up with neighbours. Hold Alt to drag freely.",
                    s.window_snap,
                    |s| &mut s.window_snap,
                    cx,
                ),
                self.toggle(
                    "hover_focus",
                    "Type into the window under the pointer",
                    "In the overview, hovering a window gives it the keyboard.",
                    s.hover_focus,
                    |s| &mut s.hover_focus,
                    cx,
                ),
            ]),
        ]
    }

    fn about(&mut self, cx: &mut Context<Self>) -> Vec<Div> {
        let data = settings::data_dir();
        let shown = SharedString::from(data.display().to_string());
        vec![
            div()
                .mt(px(4.))
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(div().text_size(px(30.)).font_weight(FontWeight::SEMIBOLD).child("Apeiron"))
                .child(dim(concat!("Version ", env!("CARGO_PKG_VERSION"))))
                .child(dim("Your desktop as an endless canvas. Pan and zoom between windows instead of stacking them.").mt(px(6.))),
            section("Files"),
            card(vec![row("Settings and layout", "").child(
                div().flex().items_center().gap(px(10.)).child(dim(shown).text_size(px(12.))).child(self.button(
                    "open_data",
                    "Open folder",
                    move |_, cx| cx.reveal_path(&data.join("settings.json")),
                    cx,
                )),
            )]),
            section("Project"),
            card(vec![row("Source code and releases", "github.com/hectoragofi/apeiron").child(self.button(
                "repo",
                "Open",
                |_, cx| cx.open_url("https://github.com/hectoragofi/apeiron"),
                cx,
            ))]),
        ]
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> Div {
        let items = Page::ALL.into_iter().enumerate().map(|(i, page)| {
            let on = page == self.page;
            div()
                .id(("page", i))
                .relative()
                .flex()
                .items_center()
                .gap(px(12.))
                .h(px(36.))
                .px(px(12.))
                .rounded(px(8.))
                .cursor_pointer()
                .text_color(theme::color(if on { theme::TEXT } else { theme::TEXT_DIM }))
                .when(on, |d| d.bg(theme::color(CONTROL)))
                .when(!on, |d| d.hover(|s| s.bg(theme::color(CARD)).text_color(theme::color(theme::TEXT))))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.go(page, cx)))
                .when(on, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left(px(0.))
                            .top(px(10.))
                            .w(px(3.))
                            .h(px(16.))
                            .rounded_full()
                            .bg(theme::color(theme::ACCENT)),
                    )
                })
                .child(
                    div()
                        .w(px(18.))
                        .flex()
                        .justify_center()
                        .font_family(ICONS)
                        .text_size(px(15.))
                        .text_color(theme::color(if on { theme::ACCENT } else { theme::TEXT_DIM }))
                        .child(page.icon()),
                )
                .child(page.name())
        });
        div()
            .flex_none()
            .w(px(212.))
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::color(SIDEBAR))
            .border_r_1()
            .border_color(theme::color_a(theme::BORDER))
            .px(px(12.))
            .pt(px(18.))
            .pb(px(14.))
            .child(
                div()
                    .px(px(12.))
                    .pb(px(16.))
                    .flex()
                    .flex_col()
                    .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Apeiron"))
                    .child(dim("Settings").text_size(px(12.))),
            )
            .child(div().flex().flex_col().gap(px(2.)).children(items))
            .child(div().flex_1())
            .child(dim(concat!("v", env!("CARGO_PKG_VERSION"))).px(px(12.)).text_size(px(11.)))
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = self.shared.settings();
        let page = self.page;
        let body = match page {
            Page::General => self.general(&s, cx),
            Page::Touchpad => self.touchpad(&s, cx),
            Page::Mouse => self.mouse(&s, cx),
            Page::Keyboard => self.keyboard(&s, cx),
            Page::Canvas => self.canvas(&s, cx),
            Page::Camera => self.camera(&s, cx),
            Page::About => self.about(cx),
        };
        let header = (page != Page::About).then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).child(page.name()))
                .child(dim(page.blurb()))
        });
        let content = div()
            .id(("content", page as usize))
            .flex_1()
            .h_full()
            .min_w_0()
            .overflow_y_scroll()
            .child(
                div()
                    .w_full()
                    .max_w(px(640.))
                    .flex()
                    .flex_col()
                    .px(px(32.))
                    .pt(px(26.))
                    .pb(px(32.))
                    .children(header)
                    .children(body)
                    .child(dim("Changes apply right away.").mt(px(22.)).text_size(px(11.))),
            );
        div()
            .id("settings")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| this.key_down(ev, window, cx)))
            .size_full()
            .flex()
            .bg(theme::color(theme::BG))
            .text_color(theme::color(theme::TEXT))
            .text_size(px(13.))
            .child(self.sidebar(cx))
            .child(content)
    }
}

fn dim(text: impl Into<SharedString>) -> Div {
    div().text_color(theme::color(theme::TEXT_DIM)).child(text.into())
}

fn section(title: &'static str) -> Div {
    div()
        .mt(px(26.))
        .mb(px(8.))
        .px(px(2.))
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::color(theme::TEXT_DIM))
        .child(title)
}

/// A quiet info line with an icon, for things worth knowing before using a
/// group of settings.
fn note(text: &'static str) -> Div {
    div()
        .mb(px(8.))
        .flex()
        .gap(px(10.))
        .px(px(14.))
        .py(px(10.))
        .rounded(px(8.))
        .bg(theme::color_a((theme::ACCENT << 8) | 0x14))
        .border_1()
        .border_color(theme::color_a((theme::ACCENT << 8) | 0x40))
        .text_size(px(12.))
        .child(div().flex_none().pt(px(1.)).font_family(ICONS).text_color(theme::color(theme::ACCENT)).child("\u{E946}"))
        .child(div().flex_1().min_w_0().text_color(theme::color(0xc4c8d2)).child(text))
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
        .overflow_hidden()
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
        .gap(px(3.))
        .child(label)
        .when(!hint.is_empty(), |d| d.child(dim(hint).text_size(px(12.))));
    div().flex().items_center().gap(px(20.)).px(px(16.)).py(px(11.)).min_h(px(52.)).child(text)
}

/// A key cap.
fn cap(k: impl Into<SharedString>) -> Div {
    div()
        .px(px(7.))
        .py(px(1.))
        .rounded(px(5.))
        .bg(theme::color(0x33363e))
        .border_1()
        .border_color(theme::color_a(theme::BORDER))
        .border_b_2()
        .text_size(px(12.))
        .child(k.into())
}

/// A shortcut that can't be changed, for reference.
fn fixed(label: &'static str, keys: &[&'static str]) -> Div {
    row(label, "").child(div().flex_none().flex().gap(px(4.)).children(keys.iter().map(|k| cap(*k))))
}
