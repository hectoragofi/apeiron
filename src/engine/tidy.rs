//! Tidy-up layout: pure functions from window rects to a neat arrangement.

/// Width:height the arrangement aims for, roughly screen-shaped.
const ASPECT: f64 = 1.6;

/// Reading order of canvas rects `(x, y, w, h)`: top to bottom, left to
/// right. Windows whose tops fall within the upper half of a row's first
/// window count as that row, so a slightly lower neighbour on the right
/// still comes after its left neighbour.
pub fn reading_order(rects: &[(f64, f64, f64, f64)]) -> Vec<usize> {
    rows(rects).concat()
}

/// The rows `reading_order` reads, top to bottom, each left to right.
fn rows(rects: &[(f64, f64, f64, f64)]) -> Vec<Vec<usize>> {
    let mut by_top: Vec<usize> = (0..rects.len()).collect();
    by_top.sort_by(|&a, &b| rects[a].1.total_cmp(&rects[b].1).then(rects[a].0.total_cmp(&rects[b].0)));
    let mut rows = Vec::new();
    let mut row_start = 0;
    while row_start < by_top.len() {
        let first = rects[by_top[row_start]];
        let band = first.1 + first.3 / 2.0;
        let row_end = by_top[row_start..].iter().position(|&i| rects[i].1 > band).map_or(by_top.len(), |n| row_start + n);
        let mut row = by_top[row_start..row_end].to_vec();
        row.sort_by(|&a, &b| rects[a].0.total_cmp(&rects[b].0));
        rows.push(row);
        row_start = row_end;
    }
    rows
}

/// Where a new window of `size` joins the layout `rects` (canvas `(x, y,
/// w, h)`) without moving anything: at the end of one of its rows, top
/// edges lined up, or in a new row under them all, whichever lands nearest
/// the middle of `view`. `gap` is kept to every window and `room` clear
/// above each for its label, as tidying does. Returns the top-left, on
/// whole pixels.
pub fn next_slot(
    rects: &[(f64, f64, f64, f64)],
    size: (f64, f64),
    view: (f64, f64, f64, f64),
    gap: f64,
    room: f64,
) -> (f64, f64) {
    let (w, h) = size;
    let mid = (view.0 + view.2 / 2.0, view.1 + view.3 / 2.0);
    if rects.is_empty() {
        return ((mid.0 - w / 2.0).round(), (mid.1 - h / 2.0).round());
    }
    let right = |row: &[usize]| row.iter().map(|&i| rects[i].0 + rects[i].2).fold(f64::MIN, f64::max);
    let top = |row: &[usize]| row.iter().map(|&i| rects[i].1).fold(f64::MAX, f64::min);
    let left = rects.iter().map(|r| r.0).fold(f64::MAX, f64::min);
    let bottom = rects.iter().map(|r| r.1 + r.3).fold(f64::MIN, f64::max);
    let mut spots: Vec<(f64, f64)> = rows(rects).iter().map(|row| (right(row) + gap, top(row))).collect();
    spots.push((left, bottom + gap + room));
    // A spot can still clip a taller window from the row below, or one that
    // sticks out of its row: slide right past whatever is in the way.
    let clear = |(mut x, y): (f64, f64)| {
        for _ in 0..rects.len() + 1 {
            let blocker = rects
                .iter()
                .filter(|r| {
                    x < r.0 + r.2 + gap && r.0 < x + w + gap && y - room < r.1 + r.3 + gap && r.1 - room < y + h + gap
                })
                .map(|r| r.0 + r.2)
                .fold(None, |m: Option<f64>, r| Some(m.map_or(r, |m| m.max(r))));
            match blocker {
                Some(r) => x = r + gap,
                None => break,
            }
        }
        (x, y)
    };
    let dist = |&(x, y): &(f64, f64)| (x + w / 2.0 - mid.0).hypot(y + h / 2.0 - mid.1);
    let best = spots.into_iter().map(clear).min_by(|a, b| dist(a).total_cmp(&dist(b)));
    let (x, y) = best.unwrap_or((left, bottom + gap + room));
    (x.round(), y.round())
}

/// Lays windows out in rows, left to right and top to bottom, in the order
/// given. Rows wrap at about sqrt(total area * 1.6) wide and every window in
/// a row shares its top edge. Returns one top-left per entry, in the same
/// order, relative to (0, 0).
pub fn arrange(sizes: &[(f64, f64)], gap: f64) -> Vec<(f64, f64)> {
    let area: f64 = sizes.iter().map(|&(w, h)| (w + gap) * (h + gap)).sum();
    let wrap = (area * ASPECT).sqrt();
    let (mut x, mut y, mut row_h) = (0.0, 0.0, 0.0_f64);
    sizes
        .iter()
        .map(|&(w, h)| {
            // Wrap once a window would sit mostly past the line; a hard edge
            // turns four 16:9 windows into a column instead of a 2x2 grid.
            if x > 0.0 && x + w / 2.0 > wrap {
                x = 0.0;
                y += row_h + gap;
                row_h = 0.0;
            }
            let pos = (x, y);
            x += w + gap;
            row_h = row_h.max(h);
            pos
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAP: f64 = 40.0;

    fn rects(sizes: &[(f64, f64)]) -> Vec<(f64, f64, f64, f64)> {
        arrange(sizes, GAP).iter().zip(sizes).map(|(&(x, y), &(w, h))| (x, y, w, h)).collect()
    }

    fn mixed() -> Vec<(f64, f64)> {
        vec![
            (1600.0, 900.0),
            (800.0, 600.0),
            (1200.0, 1000.0),
            (400.0, 300.0),
            (2560.0, 1400.0),
            (640.0, 480.0),
            (1000.0, 700.0),
            (300.0, 900.0),
        ]
    }

    #[test]
    fn empty_and_single() {
        assert!(arrange(&[], GAP).is_empty());
        assert_eq!(arrange(&[(800.0, 600.0)], GAP), vec![(0.0, 0.0)]);
    }

    #[test]
    fn no_overlaps_and_gaps_kept() {
        let r = rects(&mixed());
        for (i, a) in r.iter().enumerate() {
            for b in &r[i + 1..] {
                let apart_x = a.0 + a.2 + GAP <= b.0 || b.0 + b.2 + GAP <= a.0;
                let apart_y = a.1 + a.3 + GAP <= b.1 || b.1 + b.3 + GAP <= a.1;
                assert!(apart_x || apart_y, "{a:?} and {b:?} are closer than the gap");
            }
        }
    }

    #[test]
    fn order_kept_and_row_tops_aligned() {
        let pos = arrange(&mixed(), GAP);
        for pair in pos.windows(2) {
            let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
            // Same row further right, or the start of a lower row.
            assert!((y1 == y0 && x1 > x0) || (y1 > y0 && x1 == 0.0), "{pair:?} out of order");
        }
        assert_eq!(reading_order(&rects(&mixed())), (0..mixed().len()).collect::<Vec<_>>());
    }

    #[test]
    fn rows_wrap_into_a_grid() {
        let pos = arrange(&[(1600.0, 900.0); 9], 0.0);
        let rows: Vec<f64> = pos.iter().map(|p| p.1).collect();
        assert_eq!(rows, vec![0.0, 0.0, 0.0, 900.0, 900.0, 900.0, 1800.0, 1800.0, 1800.0]);
        let quad = arrange(&[(1600.0, 900.0); 4], 0.0);
        assert_eq!(quad, vec![(0.0, 0.0), (1600.0, 0.0), (0.0, 900.0), (1600.0, 900.0)]);
    }

    #[test]
    fn oversized_window_gets_its_own_row() {
        let pos = arrange(&[(400.0, 300.0), (20000.0, 500.0), (400.0, 300.0)], GAP);
        assert_eq!(pos[1], (0.0, 340.0));
        assert_eq!(pos[2], (0.0, 880.0));
    }

    fn apart(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64), room: f64) -> bool {
        a.0 + a.2 + GAP <= b.0 || b.0 + b.2 + GAP <= a.0 || a.1 + a.3 + GAP <= b.1 - room || b.1 + b.3 + GAP <= a.1 - room
    }

    #[test]
    fn slot_in_an_empty_canvas_is_the_middle_of_the_view() {
        assert_eq!(next_slot(&[], (800.0, 600.0), (0.0, 0.0, 1920.0, 1080.0), GAP, 0.0), (560.0, 240.0));
    }

    #[test]
    fn slot_ends_the_row_nearest_the_view() {
        // Two rows of two; looking at the top one.
        let r = [(0.0, 0.0, 800.0, 600.0), (840.0, 0.0, 800.0, 600.0), (0.0, 700.0, 800.0, 600.0), (840.0, 700.0, 800.0, 600.0)];
        let view = (0.0, 0.0, 1920.0, 600.0);
        assert_eq!(next_slot(&r, (800.0, 600.0), view, GAP, 30.0), (1680.0, 0.0));
        // Looking at the right of the bottom one.
        let view = (1000.0, 700.0, 1920.0, 600.0);
        assert_eq!(next_slot(&r, (800.0, 600.0), view, GAP, 30.0), (1680.0, 700.0));
        // A shorter row close by is filled before the far end of a long one.
        let view = (0.0, 0.0, 1920.0, 600.0);
        assert_eq!(next_slot(&r[..3], (800.0, 600.0), view, GAP, 30.0), (840.0, 700.0));
    }

    #[test]
    fn slot_opens_a_new_row_below_when_thats_nearest() {
        let r = [(0.0, 0.0, 1600.0, 900.0), (1640.0, 0.0, 1600.0, 900.0)];
        let view = (0.0, 1400.0, 1920.0, 1080.0);
        assert_eq!(next_slot(&r, (800.0, 600.0), view, GAP, 30.0), (0.0, 970.0));
    }

    #[test]
    fn slot_never_overlaps_and_keeps_the_gap() {
        // A tall window in the second row reaches up beside the first row's end.
        let r = [(0.0, 0.0, 800.0, 400.0), (0.0, 500.0, 800.0, 400.0), (900.0, 300.0, 600.0, 900.0)];
        for view in [(0.0, 0.0, 1000.0, 400.0), (0.0, 600.0, 1000.0, 600.0), (-5000.0, -5000.0, 100.0, 100.0)] {
            let (x, y) = next_slot(&r, (700.0, 500.0), view, GAP, 30.0);
            for &o in &r {
                assert!(apart((x, y, 700.0, 500.0), o, 30.0), "{view:?}: ({x}, {y}) hits {o:?}");
            }
        }
    }

    #[test]
    fn reading_order_groups_rows() {
        // Right neighbour sits a little lower, the third window is a row below.
        let r = [(900.0, 20.0, 800.0, 600.0), (0.0, 1000.0, 800.0, 600.0), (0.0, 0.0, 800.0, 600.0)];
        assert_eq!(reading_order(&r), vec![2, 0, 1]);
    }
}
