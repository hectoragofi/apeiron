//! What the canvas shows behind the windows: the wallpaper as one endless
//! scene that drifts by slower than the windows (parallax), or a dot grid.
//!
//! Both are pure functions of the snapshot's camera, so the overlay and the
//! background window paint the same pixels and the hand-off between them
//! never shows.

use gpui::*;

use super::theme;
use crate::settings::{Background, Settings};
use crate::shared::Snapshot;
use crate::wallpaper::{self, Wallpaper};

/// The wallpaper shrinks as `zoom ^ ZOOM_EXP`: zooming out pulls back from
/// it gently, so it still reads as the same place.
const ZOOM_EXP: f64 = 0.35;

/// Paints the backdrop into `bounds` (the whole window).
pub fn paint(bounds: Bounds<Pixels>, window: &mut Window, snap: &Snapshot, settings: &Settings, scale: f32) {
    if settings.background == Background::Wallpaper {
        if let Some(wp) = wallpaper::current() {
            paint_wallpaper(bounds, window, snap, &wp, settings.parallax, scale);
            return;
        }
    }
    if settings.show_grid {
        paint_grid(bounds, window, snap.cam_x, snap.cam_y, snap.zoom, scale);
    }
}

fn paint_wallpaper(
    bounds: Bounds<Pixels>,
    window: &mut Window,
    snap: &Snapshot,
    wp: &Wallpaper,
    parallax: f64,
    scale: f32,
) {
    let s = scale as f64;
    let at = |v: f64| px((v / s) as f32);
    let (sw, sh) = (snap.screen_w, snap.screen_h);
    let pad = wallpaper::PAD as f64;
    for (x, y, w, h) in tiles(snap, wp.w, wp.h, parallax) {
        // Quarter edges on whole pixels, so neighbours meet exactly.
        let xs = [x, (x + w / 2.0).round(), (x + w).round()];
        let ys = [y, (y + h / 2.0).round(), (y + h).round()];
        for (k, quad) in wp.quads.iter().enumerate() {
            let (l, r, t, b) = (xs[k % 2], xs[k % 2 + 1], ys[k / 2], ys[k / 2 + 1]);
            // Off-screen quarters are never painted, so never uploaded.
            if r <= 0.0 || b <= 0.0 || l >= sw || t >= sh {
                continue;
            }
            // Drawn with its border hanging outside its place and clipped
            // to it, so filtering at the edge only ever sees edge pixels.
            let s = quad.size(0);
            let texels = (s.width.0 as f64 - 2.0 * pad, s.height.0 as f64 - 2.0 * pad);
            let (dx, dy) = (pad * (r - l) / texels.0, pad * (b - t) / texels.1);
            let image = Bounds::new(bounds.origin + point(at(l - dx), at(t - dy)), size(at(r - l + 2.0 * dx), at(b - t + 2.0 * dy)));
            // One pixel over on the right and bottom: neighbouring clips
            // round to device pixels apart at fractional scaling, leaving a
            // hairline otherwise.
            let place = Bounds::new(bounds.origin + point(at(l), at(t)), size(at(r - l + 1.0), at(b - t + 1.0)));
            window.with_content_mask(Some(ContentMask { bounds: place }), |window| {
                let _ = window.paint_image(image, Corners::default(), quad.clone(), 0, false);
            });
        }
    }
}

/// Screen rects `(x, y, w, h)` in physical pixels of the 2x2 mirror tiles
/// that cover the screen, for a `pic_w` x `pic_h` picture.
///
/// The scene scrolls `parallax` times as far as the canvas does (measured
/// at the screen centre, so zooming about the centre doesn't slide it), and
/// at the canvas origin and zoom 1 the picture fills the screen, centred,
/// just like the real desktop.
fn tiles(snap: &Snapshot, pic_w: f64, pic_h: f64, parallax: f64) -> Vec<(f64, f64, f64, f64)> {
    let (sw, sh, z) = (snap.screen_w, snap.screen_h, snap.zoom);
    if pic_w <= 0.0 || pic_h <= 0.0 || z <= 0.0 {
        return Vec::new();
    }
    let k = z.powf(ZOOM_EXP);
    let (pw, ph) = (pic_w * k, pic_h * k);
    // Where the picture's top-left lands: centred at home, then shifted by
    // the scroll (the canvas point under the screen centre, relative to
    // home, in screen pixels).
    let x0 = (sw - pw) / 2.0 - parallax * (snap.cam_x * z + sw / 2.0 * (1.0 - z));
    let y0 = (sh - ph) / 2.0 - parallax * (snap.cam_y * z + sh / 2.0 * (1.0 - z));
    let (tw, th) = (pw * 2.0, ph * 2.0);
    let first = |origin: f64, step: f64| origin + ((-origin) / step).floor() * step;
    let mut out = Vec::new();
    let mut y = first(y0, th);
    while y < sh {
        let mut x = first(x0, tw);
        while x < sw {
            // Whole pixels, so zoom 1 maps texels to pixels one to one.
            out.push((x.round(), y.round(), tw, th));
            x += tw;
        }
        y += th;
    }
    out
}

/// Dot grid that moves with the camera, so empty canvas still reads as motion.
fn paint_grid(bounds: Bounds<Pixels>, window: &mut Window, cam_x: f64, cam_y: f64, zoom: f64, scale: f32) {
    let mut step = theme::GRID_STEP * zoom;
    while step < theme::GRID_MIN_STEP {
        step *= 2.0;
    }
    let s = scale as f64;
    let (w, h) = (f32::from(bounds.size.width) as f64 * s, f32::from(bounds.size.height) as f64 * s);
    let ox = (-cam_x * zoom).rem_euclid(step);
    let oy = (-cam_y * zoom).rem_euclid(step);
    let dot = theme::color(theme::DOT);
    let dot_size = px((theme::DOT_SIZE / s) as f32);
    let mut y = oy;
    while y < h {
        let mut x = ox;
        while x < w {
            let origin = bounds.origin + point(px((x.floor() / s) as f32), px((y.floor() / s) as f32));
            window.paint_quad(fill(Bounds::new(origin, size(dot_size, dot_size)), dot));
            x += step;
        }
        y += step;
    }
}

#[cfg(test)]
mod tests {
    use super::tiles;
    use crate::shared::Snapshot;

    fn snap(cam_x: f64, cam_y: f64, zoom: f64) -> Snapshot {
        Snapshot { cam_x, cam_y, zoom, screen_w: 1920.0, screen_h: 1080.0, ..Default::default() }
    }

    fn covers(t: &[(f64, f64, f64, f64)]) -> bool {
        let inside = |px: f64, py: f64| t.iter().any(|&(x, y, w, h)| px >= x && px < x + w && py >= y && py < y + h);
        [(0.0, 0.0), (1919.0, 0.0), (0.0, 1079.0), (1919.0, 1079.0), (960.0, 540.0)].iter().all(|&(x, y)| inside(x, y))
    }

    #[test]
    fn home_fills_the_screen_like_the_desktop() {
        let t = tiles(&snap(0.0, 0.0, 1.0), 1920.0, 1080.0, 0.35);
        assert_eq!(t, vec![(0.0, 0.0, 3840.0, 2160.0)]);
    }

    #[test]
    fn scrolls_a_fraction_of_the_pan() {
        let a = tiles(&snap(0.0, 0.0, 1.0), 1920.0, 1080.0, 0.5);
        let b = tiles(&snap(100.0, 0.0, 1.0), 1920.0, 1080.0, 0.5);
        let left = |t: &[(f64, f64, f64, f64)]| t.iter().map(|r| r.0).fold(f64::MAX, f64::min);
        assert_eq!(left(&a) - left(&b), 50.0);
    }

    #[test]
    fn always_covers_the_screen() {
        for (x, y, z) in [(0.0, 0.0, 1.0), (-12345.0, 678.0, 1.0), (5000.0, -9000.0, 0.3), (1e6, 1e6, 0.04)] {
            let t = tiles(&snap(x, y, z), 2560.0, 1440.0, 0.35);
            assert!(covers(&t), "{x} {y} {z}");
        }
    }

    #[test]
    fn zooming_about_the_centre_keeps_it_centred() {
        // Zoom 0.5 about the screen centre of home: the camera moves to
        // keep (960, 540) in the middle.
        let z = 0.5;
        let t = tiles(&snap(960.0 - 960.0 / z, 540.0 - 540.0 / z, z), 1920.0, 1080.0, 0.35);
        let k = z.powf(super::ZOOM_EXP);
        let (x, y, ..) = t.iter().copied().find(|r| r.0 <= 960.0 && r.1 <= 540.0 && r.0 + r.2 > 960.0 && r.1 + r.3 > 540.0).unwrap();
        // The picture (top-left quarter) is centred on the screen.
        assert!((x + 1920.0 * k / 2.0 - 960.0).abs() <= 1.0 && (y + 1080.0 * k / 2.0 - 540.0).abs() <= 1.0);
    }
}
