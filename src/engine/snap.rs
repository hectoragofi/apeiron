//! The smart camera: where a pan, flick or pinch comes to rest so it frames
//! windows rather than stopping anywhere, and the edge snapping for windows
//! being dragged. Pure geometry on canvas rects `(x, y, w, h)`.

pub type Rect = (f64, f64, f64, f64);

/// Windows closer than this (canvas pixels) are one cluster.
pub const CLUSTER_GAP: f64 = 120.0;
/// A flick only looks for a window inside this half-angle of its heading
/// (as the tangent: about 35°).
const CONE: f64 = 0.7;

fn area(r: Rect) -> f64 {
    r.2.max(0.0) * r.3.max(0.0)
}

fn centre(r: Rect) -> (f64, f64) {
    (r.0 + r.2 / 2.0, r.1 + r.3 / 2.0)
}

fn union(a: Rect, b: Rect) -> Rect {
    let (l, t) = (a.0.min(b.0), a.1.min(b.1));
    let (r, b) = ((a.0 + a.2).max(b.0 + b.2), (a.1 + a.3).max(b.1 + b.3));
    (l, t, r - l, b - t)
}

fn overlap(a: Rect, b: Rect) -> f64 {
    let w = (a.0 + a.2).min(b.0 + b.2) - a.0.max(b.0);
    let h = (a.1 + a.3).min(b.1 + b.3) - a.1.max(b.1);
    w.max(0.0) * h.max(0.0)
}

pub fn contains(r: Rect, p: (f64, f64)) -> bool {
    p.0 >= r.0 && p.0 < r.0 + r.2 && p.1 >= r.1 && p.1 < r.1 + r.3
}

fn near(a: Rect, b: Rect, gap: f64) -> bool {
    a.0 - gap < b.0 + b.2 && b.0 - gap < a.0 + a.2 && a.1 - gap < b.1 + b.3 && b.1 - gap < a.1 + a.3
}

/// The bounding box of all of `rects`.
pub fn hull(rects: &[Rect]) -> Option<Rect> {
    rects.iter().copied().reduce(union)
}

/// The bounding box of every rect in `idx`.
pub fn bbox(rects: &[Rect], idx: &[usize]) -> Rect {
    idx.iter().map(|&i| rects[i]).reduce(union).unwrap_or_default()
}

/// Groups of windows within `gap` of each other (chained), as indices.
pub fn clusters(rects: &[Rect], gap: f64) -> Vec<Vec<usize>> {
    let mut group: Vec<usize> = (0..rects.len()).collect();
    fn root(g: &mut [usize], mut i: usize) -> usize {
        while g[i] != i {
            g[i] = g[g[i]];
            i = g[i];
        }
        i
    }
    for i in 0..rects.len() {
        for j in i + 1..rects.len() {
            if near(rects[i], rects[j], gap) {
                let (a, b) = (root(&mut group, i), root(&mut group, j));
                group[a] = b;
            }
        }
    }
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut roots: Vec<usize> = Vec::new();
    for i in 0..rects.len() {
        let r = root(&mut group, i);
        match roots.iter().position(|&x| x == r) {
            Some(k) => out[k].push(i),
            None => {
                roots.push(r);
                out.push(vec![i]);
            }
        }
    }
    out
}

/// Camera (the view's top-left) that centres `r` in a view `vw` x `vh`
/// canvas pixels big, or pins its top-left when it doesn't fit.
pub fn centred(r: Rect, (vw, vh): (f64, f64)) -> (f64, f64) {
    let x = if r.2 < vw { r.0 + r.2 / 2.0 - vw / 2.0 } else { r.0 };
    let y = if r.3 < vh { r.1 + r.3 / 2.0 - vh / 2.0 } else { r.1 };
    (x, y)
}

/// What a view coming to rest at `view` should frame: the item most in
/// view, plus every other item mostly in view while the whole still fits.
/// None when the view is mostly empty canvas or the main item doesn't fit,
/// or when framing would move
/// it more than half a screen. Gives the camera and the main item.
pub fn framing(items: &[Rect], view: Rect) -> Option<((f64, f64), usize)> {
    let (main, shown) = items
        .iter()
        .enumerate()
        .map(|(i, &r)| (i, overlap(r, view)))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    let r = items[main];
    // Bigger than the view, it can't be framed; part of it is what's wanted.
    if r.2 > view.2 || r.3 > view.3 || (shown < 0.1 * area(view) && shown < 0.5 * area(r)) {
        return None;
    }
    let mut frame = r;
    for (j, &o) in items.iter().enumerate() {
        if j != main && overlap(o, view) >= 0.5 * area(o) {
            let u = union(frame, o);
            if u.2 <= view.2 && u.3 <= view.3 {
                frame = u;
            }
        }
    }
    let cam = centred(frame, (view.2, view.3));
    let far = (cam.0 - view.0).abs() > view.2 / 2.0 || (cam.1 - view.1).abs() > view.3 / 2.0;
    (!far).then_some((cam, main))
}

/// A flick from the view centre `from` that would carry it `travel`
/// canvas pixels: the window ahead of it to stop on, nearest to where the
/// glide would end. Windows beyond `reach` or around `from` don't count.
pub fn flick_target(rects: &[Rect], from: (f64, f64), travel: (f64, f64), reach: f64) -> Option<usize> {
    let len = travel.0.hypot(travel.1);
    if len < 1.0 {
        return None;
    }
    let dir = (travel.0 / len, travel.1 / len);
    let stop = (from.0 + travel.0, from.1 + travel.1);
    rects
        .iter()
        .enumerate()
        .filter(|(_, &r)| !contains(r, from))
        .filter_map(|(i, &r)| {
            let c = centre(r);
            let d = (c.0 - from.0, c.1 - from.1);
            let along = d.0 * dir.0 + d.1 * dir.1;
            let across = (d.0 * dir.1 - d.1 * dir.0).abs();
            (along > 0.0 && along <= reach && across <= along * CONE)
                .then(|| (i, (c.0 - stop.0).hypot(c.1 - stop.1)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// Of the `candidates`, the one whose fit zoom (on a `sw` x `sh` screen,
/// `margin` pixels clear) is within a third of `zoom` either way and
/// nearest it; never one that fits above `max`. Gives it and that zoom.
pub fn zoom_fit(candidates: &[Rect], zoom: f64, (sw, sh): (f64, f64), margin: f64, min: f64, max: f64) -> Option<(Rect, f64)> {
    let tol = 1.33f64.ln();
    candidates
        .iter()
        .filter(|r| r.2 > 0.0 && r.3 > 0.0)
        .map(|&r| (r, ((sw - 2.0 * margin) / r.2).min((sh - 2.0 * margin) / r.3).max(min)))
        .filter(|&(_, z)| z <= max)
        .map(|(r, z)| (r, z, (zoom / z).ln().abs()))
        .filter(|&(_, _, d)| d < tol)
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(r, z, _)| (r, z))
}

/// A line to draw while a window snaps: `(x0, y0, x1, y1)` in canvas pixels.
pub type Guide = (f64, f64, f64, f64);

/// How far to nudge a dragged rect `r` so an edge or its centre lines up
/// with one of `others`, or sits `gap` from it, within `reach`. Each axis
/// snaps on its own. Gives the nudge and the guides to show.
pub fn align(r: Rect, others: &[Rect], gap: f64, reach: f64) -> ((f64, f64), Vec<Guide>) {
    // Only neighbours count: lining up with something across the canvas
    // would feel like random stickiness.
    let around = 8.0 * reach + gap;
    let close: Vec<Rect> = others.iter().copied().filter(|&o| near(r, o, around)).collect();
    // (offset, other) for the best line on one axis; `lo`/`len` pick that axis.
    let best = |lo: fn(Rect) -> f64, len: fn(Rect) -> f64| {
        let mut best: Option<(f64, f64, Rect)> = None;
        for &o in &close {
            let (a0, a1, b0, b1) = (lo(r), lo(r) + len(r), lo(o), lo(o) + len(o));
            let pairs = [
                (a0, b0),
                (a1, b1),
                (a0, b1),
                (a1, b0),
                ((a0 + a1) / 2.0, (b0 + b1) / 2.0),
                (a0, b1 + gap),
                (a1, b0 - gap),
            ];
            for (k, (from, to)) in pairs.into_iter().enumerate() {
                let off = to - from;
                // Gap spacing is the only snap that needs the two side by side.
                if k >= 5 && gap <= 0.0 {
                    continue;
                }
                if off.abs() <= reach && best.is_none_or(|(b, _, _)| off.abs() < b.abs()) {
                    best = Some((off, to, o));
                }
            }
        }
        best
    };
    let x = best(|r| r.0, |r| r.2);
    let y = best(|r| r.1, |r| r.3);
    let (dx, dy) = (x.map_or(0.0, |b| b.0), y.map_or(0.0, |b| b.0));
    let moved = (r.0 + dx, r.1 + dy, r.2, r.3);
    let mut guides = Vec::new();
    if let Some((_, at, o)) = x {
        let (t, b) = (moved.1.min(o.1), (moved.1 + moved.3).max(o.1 + o.3));
        guides.push((at, t, at, b));
    }
    if let Some((_, at, o)) = y {
        let (l, r) = (moved.0.min(o.0), (moved.0 + moved.2).max(o.0 + o.2));
        guides.push((l, at, r, at));
    }
    ((dx, dy), guides)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landing_half_on_a_window_frames_it() {
        let w = (0.0, 0.0, 800.0, 600.0);
        let view = (300.0, 100.0, 1920.0, 1080.0);
        let (cam, i) = framing(&[w], view).unwrap();
        assert_eq!(i, 0);
        assert_eq!(cam, (400.0 - 960.0, 300.0 - 540.0));
    }

    #[test]
    fn landing_on_empty_canvas_stays_put() {
        let w = (0.0, 0.0, 800.0, 600.0);
        assert!(framing(&[w], (5000.0, 5000.0, 1920.0, 1080.0)).is_none());
        // A sliver of a window in a corner isn't worth flying over to.
        assert!(framing(&[w], (750.0, 550.0, 1920.0, 1080.0)).is_none());
    }

    #[test]
    fn neighbours_in_view_are_framed_together() {
        let a = (0.0, 0.0, 800.0, 600.0);
        let b = (840.0, 0.0, 800.0, 600.0);
        let (cam, _) = framing(&[a, b], (-100.0, -100.0, 1920.0, 1080.0)).unwrap();
        assert_eq!(cam, (820.0 - 960.0, 300.0 - 540.0));
    }

    #[test]
    fn a_flick_stops_on_the_window_ahead() {
        let here = (0.0, 0.0, 800.0, 600.0);
        let right = (1000.0, 0.0, 800.0, 600.0);
        let below = (0.0, 900.0, 800.0, 600.0);
        let rects = [here, right, below];
        assert_eq!(flick_target(&rects, (400.0, 300.0), (300.0, 20.0), 3000.0), Some(1));
        assert_eq!(flick_target(&rects, (400.0, 300.0), (0.0, 300.0), 3000.0), Some(2));
        assert_eq!(flick_target(&rects, (400.0, 300.0), (-300.0, 0.0), 3000.0), None);
    }

    #[test]
    fn a_pinch_near_a_fit_snaps_to_it() {
        let w = (0.0, 0.0, 3840.0, 2160.0);
        let (_, z) = zoom_fit(&[w], 0.4, (1920.0, 1080.0), 0.0, 0.04, 0.85).unwrap();
        assert_eq!(z, 0.5);
        assert!(zoom_fit(&[w], 0.2, (1920.0, 1080.0), 0.0, 0.04, 0.85).is_none());
    }

    #[test]
    fn clusters_chain_neighbours() {
        let rects = [(0.0, 0.0, 100.0, 100.0), (150.0, 0.0, 100.0, 100.0), (300.0, 0.0, 100.0, 100.0), (2000.0, 0.0, 100.0, 100.0)];
        let c = clusters(&rects, 60.0);
        assert_eq!(c, vec![vec![0, 1, 2], vec![3]]);
    }

    #[test]
    fn dragging_close_to_an_edge_lines_up() {
        let other = (0.0, 0.0, 400.0, 300.0);
        // Left edges 6px apart, top 20px under it: only x snaps.
        let ((dx, dy), guides) = align((6.0, 320.0, 200.0, 200.0), &[other], 40.0, 10.0);
        assert_eq!((dx, dy), (-6.0, 0.0));
        assert_eq!(guides.len(), 1);
        // Sitting a gap below it snaps the top.
        let ((_, dy), _) = align((100.0, 345.0, 200.0, 200.0), &[other], 40.0, 10.0);
        assert_eq!(dy, -5.0);
    }

    #[test]
    fn far_windows_dont_pull() {
        let other = (5000.0, 0.0, 400.0, 300.0);
        let ((dx, dy), guides) = align((6.0, 3.0, 200.0, 200.0), &[other], 40.0, 10.0);
        assert_eq!((dx, dy), (0.0, 0.0));
        assert!(guides.is_empty());
    }
}
