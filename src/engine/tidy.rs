//! Tidy-up layout: pure functions from window rects to a neat arrangement.

/// Width:height the arrangement aims for, roughly screen-shaped.
const ASPECT: f64 = 1.6;

/// Reading order of canvas rects `(x, y, w, h)`: top to bottom, left to
/// right. Windows whose tops fall within the upper half of a row's first
/// window count as that row, so a slightly lower neighbour on the right
/// still comes after its left neighbour.
pub fn reading_order(rects: &[(f64, f64, f64, f64)]) -> Vec<usize> {
    let mut by_top: Vec<usize> = (0..rects.len()).collect();
    by_top.sort_by(|&a, &b| rects[a].1.total_cmp(&rects[b].1).then(rects[a].0.total_cmp(&rects[b].0)));
    let mut order = Vec::with_capacity(rects.len());
    let mut row_start = 0;
    while row_start < by_top.len() {
        let first = rects[by_top[row_start]];
        let band = first.1 + first.3 / 2.0;
        let row_end = by_top[row_start..].iter().position(|&i| rects[i].1 > band).map_or(by_top.len(), |n| row_start + n);
        let row = &mut by_top[row_start..row_end];
        row.sort_by(|&a, &b| rects[a].0.total_cmp(&rects[b].0));
        order.extend_from_slice(row);
        row_start = row_end;
    }
    order
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

    #[test]
    fn reading_order_groups_rows() {
        // Right neighbour sits a little lower, the third window is a row below.
        let r = [(900.0, 20.0, 800.0, 600.0), (0.0, 1000.0, 800.0, 600.0), (0.0, 0.0, 800.0, 600.0)];
        assert_eq!(reading_order(&r), vec![2, 0, 1]);
    }
}
