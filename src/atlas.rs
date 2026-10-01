//! Everything known about one filesystem behind one question, "what is at this byte?":
//! exact runs from probes, and an even grid of sample points across the map that the
//! runs fill in as they arrive.

use crate::hilbert::{d2xy, xy2d};
use crate::proto::{Algo, Header, Msg, Run, SetInfo};
use anyhow::{Result, anyhow, bail};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::sync::Arc;

/// No answer yet.
pub const UNSET: u32 = u32::MAX;
/// A sample point past the end of the curve, which nothing will answer.
const PAST: u32 = u32::MAX - 1;
/// The sample grid is at most 2^11 = 2048 points a side.
const GRID_ORDER: u32 = 11;

/// What is known about one byte: whose it is, and about the extent it sits in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Answer {
    pub set: u32,
    /// Decompressed/on-disk × 100.
    pub ratio: u32,
    /// Generation that wrote the extent (0 = unknown).
    pub generation: u32,
    pub algo: Algo,
}

impl Answer {
    pub const NONE: Answer = Answer {
        set: UNSET,
        ratio: 0,
        generation: 0,
        algo: Algo::Unknown,
    };

    pub fn known(self) -> Option<Answer> {
        (self.set < PAST).then_some(self)
    }
}

/// The block-level curve for a filesystem: the smallest order whose cells are no bigger
/// than a sector, and each cell's size in bytes.
pub fn deep_order(total: u64) -> (u32, u64) {
    let order = (0..=20)
        .find(|&m| total.div_ceil(1u64 << (2 * m)) <= 4096)
        .unwrap_or(20);
    (order, total.div_ceil(1u64 << (2 * order)).max(1))
}

/// Positions are bytes along the curve (chunks laid end to end, as `Header::total`
/// counts them). Points on the map are pixels of a square map some power of two
/// across, `res`: pixel (x, y) stands for the byte at its middle.
pub struct Atlas {
    pub header: Header,
    /// Shared with the window, which only reads it; appended to as sets are found.
    pub sets: Arc<Vec<Arc<SetInfo>>>,
    /// The block-level curve: 2^deep blocks a side, each `block` bytes.
    pub deep: u32,
    pub block: u64,
    /// The sample grid: the pixels of a map `grid` pixels across, row by row, answered
    /// once a run covers their middle. Spread evenly over the whole map, they are what
    /// sizes are estimated from, and what pixels of maps up to that size show.
    pub grid: u64,
    points: Vec<Answer>,
    /// Points before the end of the curve, and how many of them are answered.
    pub valid: usize,
    pub known: usize,
    /// Exact runs by start: (end, answer), never overlapping.
    runs: BTreeMap<u64, (u64, Answer)>,
}

impl Atlas {
    pub fn new(header: Header) -> Self {
        let (deep, block) = deep_order(header.total);
        let grid = 1u64 << GRID_ORDER.min(deep);
        let mut atlas = Atlas {
            header,
            sets: Arc::new(Vec::new()),
            deep,
            block,
            grid,
            points: Vec::new(),
            valid: 0,
            known: 0,
            runs: BTreeMap::new(),
        };
        atlas.points = (0..(grid * grid) as usize)
            .into_par_iter()
            .map(|p| {
                match atlas.center(grid, p as u64 % grid, p as u64 / grid) < atlas.header.total {
                    true => Answer::NONE,
                    false => Answer {
                        set: PAST,
                        ..Answer::NONE
                    },
                }
            })
            .collect();
        atlas.valid = atlas.points.iter().filter(|a| a.set == UNSET).count();
        atlas
    }

    /// The byte at the middle of pixel (x, y) of a map `res` pixels across.
    pub fn center(&self, res: u64, x: u64, y: u64) -> u64 {
        let side = 1u64 << self.deep;
        let at = |v: u64| match res <= side {
            true => v * (side / res) + side / res / 2,
            false => v * side / res,
        };
        xy2d(side, at(x), at(y)) * self.block + self.block / 2
    }

    /// The bytes along the curve of the aligned square that is tile (x, y) of a map
    /// `2^level` tiles across: one contiguous range, since the curve fills aligned
    /// squares one at a time.
    pub fn span(&self, level: u32, x: u64, y: u64) -> (u64, u64) {
        let len = (self.block << (2 * self.deep)) >> (2 * level);
        let b = xy2d(1 << level, x, y);
        (b * len, (b + 1) * len)
    }

    /// The sample point standing for pixel (x, y) of a map `res` pixels across: the
    /// pixel itself on the grid, the point at its middle on a coarser map, the point
    /// under it on a finer one.
    pub fn rep(&self, res: u64, x: u64, y: u64) -> (u64, u64) {
        match res <= self.grid {
            true => {
                let k = self.grid / res;
                (x * k + k / 2, y * k + k / 2)
            }
            false => (x * self.grid / res, y * self.grid / res),
        }
    }

    /// Sample point (x, y)'s own answer.
    pub fn point(&self, x: u64, y: u64) -> Option<Answer> {
        self.points.get((y * self.grid + x) as usize)?.known()
    }

    /// The best guess for sample point (x, y): its own answer, else that of the nearest
    /// answered point standing for a coarser pixel around it, which is where coarse to
    /// fine asks first.
    pub fn estimate(&self, x: u64, y: u64) -> Option<Answer> {
        (0..=GRID_ORDER).find_map(|k| {
            let s = 1u64 << k;
            match s {
                1 => self.point(x, y),
                s if s <= self.grid => self.point(x - x % s + s / 2, y - y % s + s / 2),
                _ => None,
            }
        })
    }

    /// Pixel (x, y)'s own answer on a map `res` pixels across: its sample point's on a
    /// map no finer than the grid, else an exact run over its middle.
    pub fn own(&self, res: u64, x: u64, y: u64) -> Option<Answer> {
        match res <= self.grid {
            true => {
                let (x, y) = self.rep(res, x, y);
                self.point(x, y)
            }
            false => self.exact(self.center(res, x, y)),
        }
    }

    /// Where to ask, to answer pixel (x, y) of a map `res` pixels across.
    pub fn target(&self, res: u64, x: u64, y: u64) -> u64 {
        match res <= self.grid {
            true => {
                let (x, y) = self.rep(res, x, y);
                self.center(self.grid, x, y)
            }
            false => self.center(res, x, y),
        }
    }

    /// Every sample point's best guess (None when nothing is known yet or it is past the
    /// end).
    pub fn estimates(&self) -> impl IndexedParallelIterator<Item = Option<Answer>> + '_ {
        let g = self.grid;
        (0..(g * g) as usize)
            .into_par_iter()
            .map(move |p| match self.points[p].set {
                PAST => None,
                _ => self.estimate(p as u64 % g, p as u64 / g),
            })
    }

    /// Every sample point's best guess, skipping those without one.
    pub fn estimates_in_order(&self) -> impl Iterator<Item = Answer> + '_ {
        let g = self.grid;
        (0..g * g).filter_map(move |p| match self.points[p as usize].set {
            PAST => None,
            _ => self.estimate(p % g, p / g),
        })
    }

    /// Whether sample point (x, y) still needs asking about.
    pub fn open(&self, x: u64, y: u64) -> bool {
        self.points
            .get((y * self.grid + x) as usize)
            .is_some_and(|a| a.set == UNSET)
    }

    /// Why `msg` doesn't fit what has arrived so far, if it doesn't.
    pub fn check(&self, msg: &Msg) -> Result<()> {
        match msg {
            Msg::Header(_) => bail!("a second header"),
            Msg::Set(s) if s.id as usize != self.sets.len() => {
                bail!("set {} arrived out of order", s.id)
            }
            Msg::Runs { runs } => match runs.iter().find(|r| r.2 as usize >= self.sets.len()) {
                Some(r) => bail!("run at {} uses unknown set {}", r.0, r.2),
                None => Ok(()),
            },
            Msg::Set(_) => Ok(()),
        }
    }

    /// Fold in a message that passed `check`.
    pub fn apply(mut self, msg: Msg) -> Self {
        match msg {
            Msg::Header(_) => {}
            Msg::Set(s) => Arc::make_mut(&mut self.sets).push(Arc::new(s)),
            Msg::Runs { runs } => runs.iter().for_each(|r| self.insert(r)),
        }
        self
    }

    /// Record a run, newest wins: anything it overlaps is trimmed or replaced, and every
    /// sample point whose middle it covers takes its answer.
    fn insert(&mut self, run: &Run) {
        let (start, end) = (run.0, run.0 + run.1);
        if end <= start {
            return;
        }
        let answer = Answer {
            set: run.2,
            ratio: run.4,
            generation: run.5.min(u32::MAX as u64) as u32,
            algo: run.3,
        };
        // A run starting before `start` that reaches into it keeps only its head, and
        // its tail past `end` if it had one.
        let before = self.runs.range(..start).next_back().map(|(&s, &r)| (s, r));
        if let Some((s, (e, a))) = before.filter(|(_, (e, _))| *e > start) {
            self.runs.insert(s, (start, a));
            if e > end {
                self.runs.insert(end, (e, a));
            }
        }
        let inside: Vec<(u64, (u64, Answer))> =
            self.runs.range(start..end).map(|(&s, &r)| (s, r)).collect();
        for (s, (e, a)) in inside {
            self.runs.remove(&s);
            if e > end {
                self.runs.insert(end, (e, a));
            }
        }
        self.runs.insert(start, (end, answer));

        // Each grid cell is a contiguous stretch of the curve holding its point's middle.
        let cell = (self.block << (2 * self.deep)) / (self.grid * self.grid);
        for d in start / cell..=(end - 1) / cell {
            let (x, y) = d2xy(self.grid, d.min(self.grid * self.grid - 1));
            let middle = self.center(self.grid, x, y);
            let p = (y * self.grid + x) as usize;
            if (start..end).contains(&middle) && self.points[p].set != PAST {
                self.known += usize::from(self.points[p].set == UNSET);
                self.points[p] = answer;
            }
        }
    }

    /// The exact answer for `pos`, if a probe has covered it.
    pub fn exact(&self, pos: u64) -> Option<Answer> {
        let (_, &(end, answer)) = self.runs.range(..=pos).next_back()?;
        (pos < end).then_some(answer)
    }

    /// The best answer known for `pos`: exact, else the sample grid's guess for it.
    pub fn at(&self, pos: u64) -> Option<Answer> {
        let cell = (self.block << (2 * self.deep)) / (self.grid * self.grid);
        let (x, y) = d2xy(self.grid, (pos / cell).min(self.grid * self.grid - 1));
        self.exact(pos)
            .or_else(|| self.estimate(x, y))
            .filter(|_| pos < self.header.total)
    }
}

/// Fold one message into what has arrived so far, starting from its header.
pub fn fold(atlas: Option<Atlas>, msg: Msg) -> (Option<Atlas>, Option<anyhow::Error>) {
    match (atlas, msg) {
        (None, Msg::Header(h)) => (Some(Atlas::new(h)), None),
        (None, _) => (None, Some(anyhow!("data before the header"))),
        (Some(atlas), msg) => match atlas.check(&msg) {
            Ok(()) => (Some(atlas.apply(msg)), None),
            Err(e) => (Some(atlas), Some(e)),
        },
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::proto::Kind;

    pub fn set(id: u32, kind: Kind) -> Msg {
        Msg::Set(SetInfo {
            id,
            kind,
            paths: vec![],
            path_count: 0,
            files: 0,
            subvolumes: 0,
            truncated: false,
            error: String::new(),
        })
    }

    pub fn header(total: u64) -> Msg {
        Msg::Header(Header {
            source: "/".into(),
            total,
            calibration: vec![],
        })
    }

    pub fn runs(runs: impl IntoIterator<Item = Run>) -> Msg {
        Msg::Runs {
            runs: runs.into_iter().collect(),
        }
    }

    pub fn run(start: u64, len: u64, set: u32) -> Run {
        Run(start, len, set, Algo::None, 100, 1)
    }

    pub fn atlas(msgs: impl IntoIterator<Item = Msg>) -> Atlas {
        let (atlas, error) = msgs.into_iter().fold((None, None), |(a, e), msg| {
            let (a, new) = fold(a, msg);
            (a, e.or(new))
        });
        assert!(error.is_none(), "{error:?}");
        atlas.unwrap()
    }

    #[test]
    fn runs_answer_the_sample_points_whose_middles_they_cover() {
        // 16 GiB: a 2048² grid of block-sized cells.
        let total = 1u64 << 34;
        let a = atlas([header(total), set(0, Kind::Free)]);
        assert_eq!((a.grid, a.block, a.valid), (2048, 4096, 2048 * 2048));
        let (x, y) = (5, 9);
        let middle = a.center(2048, x, y);
        let a = a.apply(runs([run(middle, 1, 0)]));
        assert_eq!(a.known, 1);
        assert!(a.point(x, y).is_some() && a.point(x + 1, y).is_none());
        assert_eq!(a.target(2048, x, y), middle);
        let finer = a.own(4096, 2 * x + 1, 2 * y).map(|a| a.set);
        assert_eq!(finer, Some(0), "pixels finer than a block share its answer");
    }

    #[test]
    fn tiles_are_contiguous_byte_ranges_holding_their_pixels() {
        let a = atlas([header(1 << 34)]);
        for (level, x, y) in [(0, 0, 0), (1, 1, 0), (3, 5, 2)] {
            let (start, end) = a.span(level, x, y);
            let res = 256u64 << level;
            for (i, j) in [(0, 0), (255, 0), (17, 200), (255, 255)] {
                let pos = a.center(res, x * 256 + i, y * 256 + j);
                assert!((start..end).contains(&pos), "level {level} ({x}, {y})");
            }
        }
    }

    #[test]
    fn unanswered_points_borrow_from_the_nearest_coarser_one() {
        let total = 1u64 << 34;
        let a = atlas([header(total), set(0, Kind::Free), set(1, Kind::Data)]);
        // Answer the point standing for pixel (1, 0) of a 4-pixel map.
        let (px, py) = a.rep(4, 1, 0);
        let middle = a.center(2048, px, py);
        let a = a.apply(runs([run(middle, 1, 1)]));
        assert_eq!(a.own(4, 1, 0).map(|x| x.set), Some(1));
        assert_eq!(
            a.estimate(600, 100).map(|x| x.set),
            Some(1),
            "in that quarter"
        );
        assert_eq!(a.estimate(100, 100), None, "nothing above it answered");
        let (cx, cy) = a.rep(1, 0, 0);
        let middle = a.center(2048, cx, cy);
        let a = a.apply(runs([run(middle, 1, 0)]));
        assert_eq!(a.estimate(100, 100).map(|x| x.set), Some(0));
        assert_eq!(
            a.estimate(600, 100).map(|x| x.set),
            Some(1),
            "the nearer answer"
        );
    }

    #[test]
    fn what_does_not_fit_is_rejected_and_the_rest_kept() {
        let (a, error) = fold(Some(atlas([header(4096)])), runs([run(0, 1, 7)]));
        assert!(error.is_some());
        assert!(a.is_some_and(|a| a.known == 0));
        assert!(
            fold(None, set(0, Kind::Free)).1.is_some(),
            "before the header"
        );
    }

    #[test]
    fn newer_runs_trim_what_they_overlap() {
        let a = atlas([
            header(1000),
            set(0, Kind::Free),
            set(1, Kind::Data),
            set(2, Kind::Data),
            set(3, Kind::Data),
            runs([run(0, 100, 1)]),
            runs([run(40, 20, 2)]),
        ]);
        let at = |a: &Atlas, p| a.exact(p).map(|r| r.set);
        assert_eq!(
            (at(&a, 39), at(&a, 40), at(&a, 59), at(&a, 60), at(&a, 99)),
            (Some(1), Some(2), Some(2), Some(1), Some(1))
        );
        assert_eq!(at(&a, 100), None);
        let a = a.apply(runs([run(30, 50, 3)]));
        assert_eq!(
            (at(&a, 29), at(&a, 30), at(&a, 79), at(&a, 80)),
            (Some(1), Some(3), Some(3), Some(1))
        );
        assert_eq!(a.runs.len(), 3, "no leftovers of the swallowed run");
    }
}
