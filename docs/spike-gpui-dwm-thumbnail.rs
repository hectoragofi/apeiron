use gpui::*;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::UI::WindowsAndMessaging::*;

struct Grid;
impl Render for Grid {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(rgb(0x121316)).child(
            canvas(|_, _, _| (), |b, _, window, _| {
                let mut y = 0.0;
                while y < f32::from(b.size.height) {
                    let mut x = 0.0;
                    while x < f32::from(b.size.width) {
                        window.paint_quad(fill(Bounds::new(point(px(x), px(y)), size(px(2.), px(2.))), rgb(0x383a40)));
                        x += 40.0;
                    }
                    y += 40.0;
                }
            }).size_full(),
        )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let b = Bounds::new(point(px(0.), px(0.)), size(px(1200.), px(700.)));
        let handle = cx.open_window(WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(b)),
            titlebar: None,
            kind: WindowKind::PopUp,
            focus: false,
            ..Default::default()
        }, |_, cx| cx.new(|_| Grid)).unwrap();
        handle.update(cx, |_, window, _| {
            let RawWindowHandle::Win32(h) = window.window_handle().unwrap().as_raw() else { return };
            let dest = HWND(h.hwnd.get() as *mut _);
            unsafe {
                let src = GetForegroundWindow();
                let mut r = RECT::default();
                let _ = GetWindowRect(src, &mut r);
                println!("dest {:?} src {:?}", dest, src);
                let _ = SetWindowPos(dest, Some(HWND_TOPMOST), 0, 0, 1500, 875, SWP_SHOWWINDOW | SWP_NOACTIVATE);
                match DwmRegisterThumbnail(dest, src) {
                    Ok(t) => {
                        let p = DWM_THUMBNAIL_PROPERTIES {
                            dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY,
                            rcDestination: RECT { left: 100, top: 100, right: 700, bottom: 450 },
                            opacity: 255, fVisible: true.into(), ..Default::default()
                        };
                        println!("update {:?}", DwmUpdateThumbnailProperties(t, &p));
                    }
                    Err(e) => println!("register failed {e:?}"),
                }
            }
        }).unwrap();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(std::time::Duration::from_secs(4)).await;
            let _ = cx.update(|cx| cx.quit());
        }).detach();
    });
}
