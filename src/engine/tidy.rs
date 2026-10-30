//! Tidy-up layout: a pure function from window sizes to positions.

// FEATURE D: keep the current reading order (sort by y then x), wrap rows at
// about sqrt(total area * 1.6), align row tops. `sizes` is in the engine's
// window order; return one top-left per entry, in the same order.
pub fn arrange(sizes: &[(f64, f64)], gap: f64) -> Vec<(f64, f64)> {
    let mut x = 0.0;
    sizes
        .iter()
        .map(|&(w, _)| {
            let pos = (x, 0.0);
            x += w + gap;
            pos
        })
        .collect()
}
