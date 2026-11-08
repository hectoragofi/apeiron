//! The engine's half of the IPC server (`crate::ipc`): requests that move
//! the camera or windows, answered at once or once things have settled.

use serde_json::json;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{SetTimer, ShowWindowAsync, SW_RESTORE};

use crate::engine::wins;
use crate::ipc::{EngineOp, IpcCall, Rect, Reply, CANVAS_OFF};

use super::{tabs, Canvas, SAVE_DELAY_MS, SNAP, TIMER_SAVE};

/// Space between a new region and everything already on the canvas.
const REGION_GAP: f64 = 400.0;

impl Canvas {
    pub(super) fn ipc(&mut self, IpcCall { op, reply }: IpcCall) {
        if let EngineOp::FocusWindow(id) = op {
            self.ipc_focus(id, reply);
            return;
        }
        if !self.cfg().canvas {
            let _ = reply.send(Err(CANVAS_OFF.into()));
            return;
        }
        let now: Reply = match op {
            EngineOp::FocusWindow(_) => unreachable!(),
            EngineOp::LeaveStage => {
                self.leave_overview();
                return self.ipc_wait(reply);
            }
            EngineOp::FlyTo(r) => {
                self.ipc_fly(r);
                return self.ipc_wait(reply);
            }
            EngineOp::Back => match self.ipc_back.take() {
                Some(to) => {
                    self.ipc_go(to);
                    return self.ipc_wait(reply);
                }
                None => Err("nothing to go back to".into()),
            },
            EngineOp::ReserveRegion { name, w, h } => {
                Ok(json!({ "canvas_rect": self.ipc_reserve(&name, w, h) }))
            }
            EngineOp::PlaceWindow { hwnd, region } => self.ipc_place(hwnd, &region),
        };
        let _ = reply.send(now);
    }

    /// Answers `reply` once the camera has come to rest.
    fn ipc_wait(&mut self, reply: std::sync::mpsc::Sender<Reply>) {
        self.ipc_waiting.push(reply);
    }

    /// Nothing is moving: no animation, no fingers or mouse on the canvas,
    /// and real windows have been placed if it landed.
    fn ipc_at_rest(&self) -> bool {
        self.anim.is_none() && !self.hiding && !self.gesture.engaged && self.drag.is_none()
    }

    /// `(moving, gesture)` as published for IPC clients.
    pub(super) fn ipc_flags(&self) -> (bool, bool) {
        let gesture = self.gesture.engaged
            || self.drag.is_some()
            || self.tab_drag.is_some()
            || self.title_drag.is_some();
        let staged = (self.overview || self.search.is_some()) && self.ipc_at_rest();
        (self.shown && !staged, gesture)
    }

    /// Replies to requests waiting for the camera, if it has settled.
    pub(super) fn ipc_settle(&mut self) {
        if self.ipc_waiting.is_empty() || !self.ipc_at_rest() {
            return;
        }
        for r in self.ipc_waiting.drain(..) {
            let _ = r.send(Ok(json!({})));
        }
    }

    /// Where the camera is, or is heading: `(x, y, zoom)`.
    fn ipc_camera(&self) -> (f64, f64, f64) {
        match &self.anim {
            Some(a) if self.shown => a.to,
            _ if self.shown => (self.cam_x, self.cam_y, self.zoom),
            _ => (self.cam_x, self.cam_y, 1.0),
        }
    }

    fn ipc_focus(&mut self, id: isize, reply: std::sync::mpsc::Sender<Reply>) {
        let hidden_tab = self.is_hidden_tab(id);
        let away =
            self.away.iter().any(|w| w.id() == id) || self.minimized.iter().any(|w| w.id == id);
        if self.index(id).is_none() && !hidden_tab && !away {
            let _ = reply.send(Err(format!("unknown window: {id}")));
            return;
        }
        if !self.cfg().canvas {
            // Tabs only: no camera, just show it and hand it the keyboard.
            if hidden_tab {
                self.switch_tab(id);
            } else {
                let h = HWND(id as *mut _);
                if away {
                    unsafe {
                        let _ = ShowWindowAsync(h, SW_RESTORE);
                    }
                }
                wins::activate(h);
                self.focused = Some(id);
                self.dirty = true;
            }
            let _ = reply.send(Ok(json!({})));
            return;
        }
        self.ipc_back = Some(self.ipc_camera());
        if hidden_tab {
            self.switch_tab(id);
        }
        self.focus_window(id);
        self.ipc_wait(reply);
    }

    /// Frames canvas rect `r`: at 1:1 centred on it if it fits on screen,
    /// else zoomed out to fit it (the overview).
    fn ipc_fly(&mut self, [x, y, w, h]: Rect) {
        self.ipc_back = Some(self.ipc_camera());
        if !self.shown {
            self.begin();
        }
        if !self.shown {
            return;
        }
        if w <= self.sw && h <= self.sh {
            let (tx, ty) = self.centre_on((x, y, w, h));
            self.animate_to(tx, ty, 1.0, true, None);
        } else {
            let (cx, cy, z) = self.fit((x, y, x + w, y + h));
            self.animate_to(cx, cy, z, false, None);
            if !self.overview {
                self.enter_overview();
            }
        }
    }

    /// Glides back to camera `(x, y, zoom)`.
    fn ipc_go(&mut self, (x, y, z): (f64, f64, f64)) {
        if !self.shown {
            self.begin();
        }
        if !self.shown {
            return;
        }
        if (z - 1.0).abs() < SNAP {
            self.animate_to(x, y, 1.0, true, None);
        } else {
            self.animate_to(x, y, z, false, None);
            if !self.overview {
                self.enter_overview();
            }
        }
    }

    /// The region called `name`: where it already is (resized to `w` x `h`
    /// if that changed), else a new one clear of everything on the canvas.
    fn ipc_reserve(&mut self, name: &str, w: f64, h: f64) -> Rect {
        let rect = match self.layout.region(name) {
            Some([x, y, _, _]) => [x, y, w, h],
            None => {
                let mut taken: Vec<Rect> = self
                    .wins
                    .iter()
                    .chain(&self.away)
                    .chain(&self.tabbed)
                    .map(|w| [w.x, w.y, w.w, w.h])
                    .collect();
                taken.extend(self.layout.regions());
                let view = [self.cam_x, self.cam_y, self.sw, self.sh];
                let rect = free_spot(&taken, view, w, h);
                log!("ipc: reserved region {name:?} at {rect:?}");
                rect
            }
        };
        self.layout.set_region(name, rect);
        self.layout.save();
        rect
    }

    /// Puts window `hwnd` in the region's top-left corner, shrunk to fit
    /// it if it's bigger, without moving the camera.
    fn ipc_place(&mut self, hwnd: isize, region: &str) -> Reply {
        let Some([rx, ry, rw, rh]) = self.layout.region(region) else {
            return Err(format!("unknown region: {region}"));
        };
        let Some(i) = self.index(hwnd) else {
            let known = self.is_hidden_tab(hwnd) || self.away.iter().any(|w| w.id() == hwnd);
            return Err(if known {
                "that window isn't on the canvas (a hidden tab, or minimized)".into()
            } else {
                format!("unknown window: {hwnd}")
            });
        };
        self.let_go(i);
        let (_, _, vw, vh) = self.wins[i].vis();
        let rect = [rx, ry, vw.min(rw), vh.min(rh)];
        tabs::fit(&mut self.wins[i], (rect[0], rect[1], rect[2], rect[3]));
        self.wins[i].placed = None;
        self.show_tab(i, false);
        wins::save_parked(&self.parked);
        if !self.shown {
            unsafe {
                SetTimer(Some(self.hwnd), TIMER_SAVE, SAVE_DELAY_MS, None);
            }
        }
        self.update_view();
        self.dirty = true;
        Ok(json!({ "canvas_rect": rect }))
    }
}

/// A `w` x `h` spot right of everything `taken` and of the current view
/// `[x, y, w, h]`, its top level with the highest of them.
fn free_spot(taken: &[Rect], view: Rect, w: f64, h: f64) -> Rect {
    let all = taken.iter().chain(std::iter::once(&view));
    let right = all.clone().map(|r| r[0] + r[2]).fold(f64::MIN, f64::max);
    let top = all.map(|r| r[1]).fold(f64::MAX, f64::min);
    [(right + REGION_GAP).round(), top.round(), w, h]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlaps(a: Rect, b: Rect) -> bool {
        a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
    }

    #[test]
    fn free_spot_is_clear_of_windows_and_the_view() {
        let view = [0.0, 0.0, 1920.0, 1080.0];
        let taken = [
            [-3000.0, -200.0, 800.0, 600.0],
            [2500.0, 300.0, 1000.0, 800.0],
        ];
        let r = free_spot(&taken, view, 900.0, 700.0);
        assert_eq!(r, [3900.0, -200.0, 900.0, 700.0]);
        for t in taken.iter().chain([&view]) {
            assert!(!overlaps(r, *t));
        }
        // A second region goes right of the first.
        let r2 = free_spot(&[taken[0], taken[1], r], view, 100.0, 100.0);
        assert!(!overlaps(r, r2) && r2[0] >= r[0] + r[2]);
        // Nothing on the canvas: right of the view.
        assert_eq!(free_spot(&[], view, 10.0, 10.0), [2320.0, 0.0, 10.0, 10.0]);
    }
}
