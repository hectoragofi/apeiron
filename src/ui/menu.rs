//! The right-click menu. It lives in a small window of its own, kept above
//! the overlay: DWM thumbnails cover anything drawn in the overlay itself.
//! The engine decides what's in it (`Snapshot::menu`); this draws it and
//! reports clicks.

use gpui::*;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::theme;
use crate::shared::{Command, EngineHandle, MenuSnap, Shared};

/// Logical pixels.
pub const WIDTH: f32 = 250.;
const ROW_H: f32 = 30.;
const SEP_H: f32 = 9.;
const PAD: f32 = 5.;

/// Menu background: the panel colour, opaque (the window behind it isn't).
const BG: u32 = 0x202127;
const HOVER: u32 = 0x6c8cffd9;
const DANGER: u32 = 0xc42b1c;

/// The menu's height in logical pixels.
pub fn height(menu: &MenuSnap) -> f32 {
    PAD * 2. + menu.items.iter().map(|i| if i.separator { SEP_H } else { ROW_H }).sum::<f32>()
}

pub struct MenuView {
    shared: Shared,
    engine: EngineHandle,
}

impl MenuView {
    pub fn new(shared: Shared, engine: EngineHandle) -> Self {
        MenuView { shared, engine }
    }
}

impl Render for MenuView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let snap = self.shared.snap();
        let root = div().size_full().bg(theme::color(BG)).p(px(PAD)).flex().flex_col().text_size(px(13.));
        let Some(menu) = snap.menu.as_ref() else { return root };
        let rows = menu.items.iter().enumerate().map(|(k, item)| {
            if item.separator {
                return div()
                    .h(px(SEP_H))
                    .flex()
                    .items_center()
                    .px(px(8.))
                    .child(div().h(px(1.)).w_full().bg(theme::color_a(theme::BORDER)))
                    .into_any_element();
            }
            let engine = self.engine.clone();
            let hover = if item.danger { theme::color(DANGER) } else { theme::color_a(HOVER) };
            div()
                .id(("menu", k))
                .h(px(ROW_H))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(8.))
                .rounded(px(5.))
                .text_color(theme::color(theme::TEXT))
                .cursor_pointer()
                .hover(move |s| s.bg(hover).text_color(theme::color(0xffffff)))
                .child(div().w(px(14.)).flex_none().text_color(theme::color(theme::ACCENT)).child(if item.check { "✓" } else { "" }))
                .child(div().flex_1().min_w_0().truncate().child(SharedString::from(item.label.clone())))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.5))
                        .text_color(theme::color(theme::TEXT_DIM))
                        .child(SharedString::from(item.hint.clone())),
                )
                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                    cx.stop_propagation();
                    engine.send(Command::MenuPick(k));
                })
                .into_any_element()
        });
        root.children(rows)
    }
}

/// Borderless, topmost, never activating, hidden.
pub fn restyle(hwnd: HWND) {
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_STYLE, WS_POPUP.0 as isize);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (ex | WS_EX_NOACTIVATE.0 | WS_EX_TOPMOST.0 | WS_EX_TOOLWINDOW.0) as isize);
        // The window is cut to the menu's shape (a rounded region); the
        // system's own corners and 1 px border would show around it.
        let square = DWMWCP_DONOTROUND;
        let none = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &square as *const _ as *const _, 4);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &none as *const _ as *const _, 4);
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}
