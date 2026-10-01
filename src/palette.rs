//! What each mode colors by, and the colors.

use crate::atlas::Answer;
use crate::proto::{Algo, Kind, SetInfo};
use eframe::egui::Color32;
use rayon::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Owners,
    Sharing,
    Compression,
    Age,
}

pub const MODES: [(Mode, &str); 4] = [
    (Mode::Owners, "Owners"),
    (Mode::Sharing, "Sharing"),
    (Mode::Compression, "Compression"),
    (Mode::Age, "Age"),
];

pub const SHARE_BUCKETS: [(usize, &str); 5] = [
    (2, "2 files"),
    (3, "3–4"),
    (5, "5–9"),
    (10, "10–49"),
    (50, "50+"),
];
pub const RATIO_BUCKETS: [(u32, &str); 5] = [
    (100, "<1.5×"),
    (150, "1.5–2×"),
    (200, "2–4×"),
    (400, "4–8×"),
    (800, "≥8×"),
];
pub const DAY: i64 = 86_400;
/// Age since the scan, youngest first.
pub const AGE_BUCKETS: [(i64, &str); 5] = [
    (0, "< 1 day"),
    (DAY, "< 1 week"),
    (7 * DAY, "< 1 month"),
    (30 * DAY, "< 6 months"),
    (182 * DAY, "older"),
];

pub fn bucket<T: PartialOrd + Copy>(buckets: &[(T, &str)], v: T) -> usize {
    buckets.iter().rposition(|(min, _)| v >= *min).unwrap_or(0)
}

/// Colors for one theme. Sequential ramps are one hue each, light to dark against the
/// background (reversed in dark mode, so "more" always stands out).
#[derive(Clone)]
pub struct Palette {
    pub dark: bool,
    pub free: Color32,
    pub metadata: Color32,
    pub system: Color32,
    pub error: Color32,
    pub neutral: Color32,
    pub unknown: Color32,
    pub share: [Color32; 5],
    pub ratio: [Color32; 5],
    /// Oldest first.
    pub age: [Color32; 5],
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

impl Palette {
    pub fn new(dark: bool) -> Self {
        match dark {
            false => Palette {
                dark,
                free: hex(0xe7e7e2),
                metadata: hex(0x8d8d93),
                system: hex(0xb8b8bd),
                error: hex(0xd6246e),
                neutral: hex(0xc9c7c0),
                unknown: hex(0xb9a89c),
                share: [0x6da7ec, 0x3987e5, 0x256abf, 0x184f95, 0x0d366b].map(hex),
                ratio: [0xf19565, 0xeb6834, 0xc9501f, 0x9c3d16, 0x6e2a0e].map(hex),
                age: [0x48b889, 0x2a9f73, 0x1d845d, 0x156848, 0x0c4c33].map(hex),
            },
            true => Palette {
                dark,
                free: hex(0x202024),
                metadata: hex(0x56565c),
                system: hex(0x7a7a80),
                error: hex(0xff4f98),
                neutral: hex(0x3b3b40),
                unknown: hex(0x4a3f38),
                share: [0x184f95, 0x256abf, 0x3987e5, 0x6da7ec, 0x9ec5f4].map(hex),
                ratio: [0x8a3512, 0xb5461b, 0xd95926, 0xf08a5d, 0xf7b596].map(hex),
                age: [0x12603f, 0x1a7d55, 0x27a06f, 0x52c392, 0x93dcb9].map(hex),
            },
        }
    }

    /// The color of everything that isn't file data.
    pub fn kind(&self, kind: Kind) -> Color32 {
        match kind {
            Kind::Free => self.free,
            Kind::Metadata => self.metadata,
            Kind::System => self.system,
            Kind::Error => self.error,
            Kind::Data | Kind::Unreachable => self.neutral,
        }
    }

    /// A distinct hue per set: ids step around the wheel by the golden angle.
    pub fn owner(&self, set: &SetInfo) -> Color32 {
        let hue = (set.id as f32 * 137.508) % 360.0;
        let (sat, light) = match set.kind {
            Kind::Unreachable => (30.0, if self.dark { 30.0 } else { 72.0 }),
            _ => (
                60.0 + (set.id * 7 % 25) as f32,
                (if self.dark { 50.0 } else { 44.0 }) + (set.id * 11 % 14) as f32,
            ),
        };
        hsl(hue, sat, light)
    }

    pub fn sharing(&self, set: &SetInfo) -> Color32 {
        match set.kind {
            Kind::Unreachable => self.unknown,
            _ if set.files < 2 => self.neutral,
            _ => self.share[bucket(&SHARE_BUCKETS, set.files)],
        }
    }
}

fn hsl(h: f32, s: f32, l: f32) -> Color32 {
    let (s, l) = (s / 100.0, l / 100.0);
    let a = s * l.min(1.0 - l);
    let f = |n: f32| {
        let k = (n + h / 30.0) % 12.0;
        let v = l - a * (k - 3.0).min(9.0 - k).clamp(-1.0, 1.0);
        (v * 255.0).round() as u8
    };
    Color32::from_rgb(f(0.0), f(8.0), f(4.0))
}

/// Approximate unix time `generation` was written, interpolated between the times btrfs
/// recorded for known generations. Clamped to the first and last of them.
pub fn date_of(calibration: &[(u64, i64)], generation: u64) -> Option<i64> {
    if generation == 0 || calibration.is_empty() {
        return None;
    }
    let i = calibration.partition_point(|p| p.0 <= generation);
    Some(match i {
        0 => calibration[0].1,
        i if i == calibration.len() => calibration[i - 1].1,
        i => {
            let ((g0, t0), (g1, t1)) = (calibration[i - 1], calibration[i]);
            let along = (generation - g0) as f64 / (g1 - g0) as f64;
            t0 + (along * (t1 - t0) as f64) as i64
        }
    })
}

/// The newest generation in each age bucket: since dates only grow with generations,
/// bucketing an extent by age is comparing its generation against these.
pub fn age_cutoffs(calibration: &[(u64, i64)]) -> [u64; 5] {
    let now = calibration.last().map_or(0, |p| p.1);
    let old_enough =
        |g: u64, secs: i64| date_of(calibration, g).is_some_and(|t| (now - t).max(0) >= secs);
    AGE_BUCKETS.map(|(secs, _)| {
        let (mut lo, mut hi) = (0u64, u64::MAX / 2);
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            match old_enough(mid, secs) {
                true => lo = mid,
                false => hi = mid - 1,
            }
        }
        lo
    })
}

/// Which age bucket a generation falls in; None when it is unknown.
pub fn age_bucket(cutoffs: &[u64; 5], generation: u64) -> Option<usize> {
    (generation != 0).then(|| {
        (0..5)
            .rev()
            .find(|&k| generation <= cutoffs[k])
            .unwrap_or(0)
    })
}

/// A pixel as the GPU sees it: the set id in the low 24 bits; in the top byte, the
/// compression code (low 3 bits: ratio bucket 0–4, 5 unknown, 6 uncompressed) and the
/// age code (next 3: bucket 0–4, 5 unknown). `NOTHING` where nothing is known.
pub fn pack(a: Answer, cutoffs: &[u64; 5]) -> u32 {
    let ratio = match a.algo {
        Algo::Unknown => 5,
        Algo::None => 6,
        _ => bucket(&RATIO_BUCKETS, a.ratio) as u32,
    };
    let age = age_bucket(cutoffs, a.generation as u64).map_or(5, |b| b as u32);
    a.set.min(0xFF_FFFE) | (ratio | age << 3) << 24
}

/// No pixel: nothing known, or past the end.
pub const NOTHING: u32 = u32::MAX;

/// What the GPU colors pixels by.
#[derive(Clone, Default)]
pub struct GpuColors {
    /// Changes whenever the colors do, other than by sets being added.
    pub look: u64,
    /// A color per set; transparent where the pixel's own compression or age decides.
    pub table: Vec<[u8; 4]>,
    /// Colors for compression codes (0..8), then age codes (8..16).
    pub ramp: [[u8; 4]; 16],
    /// Whether this mode colors by age rather than compression.
    pub age: bool,
}

/// How to color answers in one mode, worked out once per mode and set list: a color per
/// set where the set alone decides it, else by the answer's extent.
pub struct Colorer {
    /// None: color by the answer's compression or age.
    by_set: Vec<Option<Color32>>,
    mode: Mode,
    palette: Palette,
    cutoffs: [u64; 5],
}

impl Colorer {
    pub fn new(palette: &Palette, mode: Mode, calibration: &[(u64, i64)]) -> Self {
        Colorer {
            by_set: Vec::new(),
            mode,
            palette: palette.clone(),
            cutoffs: age_cutoffs(calibration),
        }
    }

    /// Work out the colors of sets found since the last call.
    pub fn extend(&mut self, sets: &[std::sync::Arc<SetInfo>]) {
        let (mode, palette) = (self.mode, &self.palette);
        let more: Vec<Option<Color32>> = sets[self.by_set.len().min(sets.len())..]
            .par_iter()
            .map(|set| match (mode, set.kind) {
                (Mode::Owners, Kind::Data | Kind::Unreachable) => Some(palette.owner(set)),
                (Mode::Sharing, Kind::Data | Kind::Unreachable) => Some(palette.sharing(set)),
                (_, Kind::Data | Kind::Unreachable) => None,
                (_, k) => Some(palette.kind(k)),
            })
            .collect();
        self.by_set.extend(more);
    }

    /// Each set's color for the GPU's lookup table, transparent where the pixel's own
    /// compression or age decides, from `from` on (the table only grows).
    pub fn table(&self, from: usize) -> Vec<[u8; 4]> {
        self.by_set[from.min(self.by_set.len())..]
            .iter()
            .map(|c| c.map_or([0; 4], |c| c.to_array()))
            .collect()
    }

    /// The colors of compression codes (0..8) then age codes (8..16), as `pack` codes
    /// them, and whether this mode colors by age.
    pub fn ramp(&self) -> ([[u8; 4]; 16], bool) {
        let p = &self.palette;
        let mut ramp = [p.neutral.to_array(); 16];
        for b in 0..5 {
            ramp[b] = p.ratio[b].to_array();
            ramp[8 + b] = p.age[4 - b].to_array();
        }
        ramp[5] = p.unknown.to_array();
        ramp[8 + 5] = p.unknown.to_array();
        (ramp, self.mode == Mode::Age)
    }

    /// One color for a whole set, for lists and legends: its own where the set decides
    /// it, else from its compressed bytes (`packed` on disk holding `holds`) or its
    /// newest write.
    pub fn swatch(&self, set: u32, packed: f64, holds: f64, newest: u64) -> Color32 {
        match (self.by_set.get(set as usize), self.mode) {
            (Some(Some(c)), _) => *c,
            (None, _) => self.palette.neutral,
            (Some(None), Mode::Compression) if packed > 0.0 => {
                self.palette.ratio[bucket(&RATIO_BUCKETS, (100.0 * holds / packed) as u32)]
            }
            (Some(None), Mode::Compression) => self.palette.neutral,
            (Some(None), _) => match age_bucket(&self.cutoffs, newest) {
                Some(b) => self.palette.age[4 - b],
                None => self.palette.unknown,
            },
        }
    }

    pub fn cutoffs(&self) -> &[u64; 5] {
        &self.cutoffs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generations_date_by_interpolating_recorded_times() {
        let cal = [(100, 1_000_000), (200, 2_000_000), (300, 2_000_000)];
        assert_eq!(date_of(&cal, 150), Some(1_500_000));
        assert_eq!(
            date_of(&cal, 50),
            Some(1_000_000),
            "clamped before the first"
        );
        assert_eq!(
            date_of(&cal, 900),
            Some(2_000_000),
            "clamped after the last"
        );
        assert_eq!(date_of(&cal, 0), None, "generation 0 is unknown");
        assert_eq!(date_of(&[], 150), None);
    }

    #[test]
    fn age_cutoffs_bucket_generations_like_their_dates() {
        let now = 1_790_000_000;
        let cal = [
            (10, now - 400 * DAY),
            (1000, now - 20 * DAY),
            (5000, now - 3600),
            (5100, now),
        ];
        let cut = age_cutoffs(&cal);
        for g in [
            1, 10, 11, 500, 999, 1000, 1001, 3000, 4999, 5000, 5050, 5100, 9000,
        ] {
            let by_date = date_of(&cal, g).map(|t| bucket(&AGE_BUCKETS, (now - t).max(0)));
            assert_eq!(age_bucket(&cut, g), by_date, "generation {g}");
        }
        assert_eq!(age_bucket(&cut, 0), None);
    }
}
