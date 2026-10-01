//! The Hilbert curve, which keeps bytes that are near each other on disk near each
//! other on screen.

/// Curve index of cell (x, y) on a `side`×`side` grid.
pub fn xy2d(side: u64, x: u64, y: u64) -> u64 {
    let (mut x, mut y, mut d) = (x, y, 0u64);
    let mut s = side / 2;
    while s > 0 {
        let rx = u64::from(x & s > 0);
        let ry = u64::from(y & s > 0);
        d += s * s * ((3 * rx) ^ ry);
        if ry == 0 {
            if rx == 1 {
                x = side - 1 - x;
                y = side - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        s /= 2;
    }
    d
}

/// Index `i` of a pass over `4^level` cells, bit-reversed so that consecutive indices
/// land spread across the whole curve and a pass sharpens everything evenly.
pub fn spread(level: u32, i: u64) -> u64 {
    match 2 * level {
        0 => 0,
        bits => i.reverse_bits() >> (64 - bits),
    }
}

/// Cell of curve index `d` on a `side`×`side` grid: the inverse of `xy2d`.
pub fn d2xy(side: u64, d: u64) -> (u64, u64) {
    let (mut x, mut y, mut t) = (0, 0, d);
    let mut s = 1;
    while s < side {
        let rx = 1 & (t >> 1);
        let ry = 1 & (t ^ rx);
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        x += s * rx;
        y += s * ry;
        t /= 4;
        s *= 2;
    }
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visits_every_cell_once_with_unit_steps() {
        let side = 16;
        let points: Vec<_> = (0..side * side).map(|d| d2xy(side, d)).collect();
        let unique: std::collections::BTreeSet<_> = points.iter().collect();
        assert_eq!(unique.len(), points.len());
        for w in points.windows(2) {
            let dist = w[0].0.abs_diff(w[1].0) + w[0].1.abs_diff(w[1].1);
            assert_eq!(dist, 1, "{:?} -> {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn xy2d_inverts_d2xy() {
        let side = 64;
        for d in 0..side * side {
            let (x, y) = d2xy(side, d);
            assert_eq!(xy2d(side, x, y), d);
        }
    }

    #[test]
    fn finer_curves_trace_coarser_ones() {
        // A block-level view must land on the scan's cells: order-n cell d, seen from
        // order n+k, is where fine cells d·4^k.. are.
        for (n, k) in [(3, 1), (3, 2), (4, 3)] {
            let (coarse, fine) = (1u64 << n, 1u64 << (n + k));
            for d in 0..coarse * coarse {
                let (x, y) = d2xy(fine, d << (2 * k));
                assert_eq!((x >> k, y >> k), d2xy(coarse, d), "n={n} k={k} d={d}");
            }
        }
    }

    #[test]
    fn runs_fill_aligned_squares_at_every_scale() {
        // What coarse-to-fine drawing relies on: any aligned run of 4^m indices is
        // exactly one aligned 2^m square.
        let side = 64;
        for m in 0..=6 {
            let run = 1u64 << (2 * m);
            for start in (0..side * side).step_by(run as usize) {
                let cells: Vec<_> = (start..start + run).map(|d| d2xy(side, d)).collect();
                let (x0, y0) = (cells[0].0 >> m, cells[0].1 >> m);
                assert!(
                    cells.iter().all(|&(x, y)| x >> m == x0 && y >> m == y0),
                    "run {start}..{} at scale {m} leaves its square",
                    start + run
                );
            }
        }
    }

    #[test]
    fn a_spread_pass_visits_every_cell_once_starting_in_each_quadrant() {
        for level in 0..=6 {
            let mut seen: Vec<u64> = (0..1 << (2 * level)).map(|i| spread(level, i)).collect();
            seen.sort();
            assert_eq!(seen, (0..1 << (2 * level)).collect::<Vec<_>>());
        }
        let quadrants: std::collections::BTreeSet<u64> =
            (0..4).map(|i| spread(3, i) >> 4).collect();
        assert_eq!(quadrants.len(), 4);
    }
}
