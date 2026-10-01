//! Totals, the legend and the ranked list: what the side pane shows, estimated from the
//! sample grid.

use crate::atlas::{Answer, Atlas};
use crate::palette::{
    AGE_BUCKETS, Colorer, Mode, Palette, RATIO_BUCKETS, SHARE_BUCKETS, age_bucket, bucket, date_of,
};
use crate::proto::{Algo, Kind, SetInfo};
use eframe::egui::Color32;
use rayon::prelude::*;
use std::sync::Arc;

/// Largest number of rows in the side list.
const LIST_ROWS: usize = 200;

/// Per-set totals estimated from the sample grid, as long as the set list they came with.
#[derive(Default)]
pub struct Stats {
    /// Bytes.
    pub area: Vec<u64>,
    /// On-disk bytes in compressed extents, and the bytes they hold.
    pub packed: Vec<f64>,
    pub holds: Vec<f64>,
    /// The newest generation that wrote any sampled extent.
    pub newest: Vec<u64>,
}

/// Bytes each sample point stands for.
fn per_point(atlas: &Atlas) -> f64 {
    atlas.header.total as f64 / atlas.valid.max(1) as f64
}

/// One dense pass over the sample points: per set, the points guessed to be in it, those
/// in compressed extents and what they hold, and the newest generation. Faster than
/// splitting the work, which needs a table per thread as long as the set list.
pub fn stats(atlas: &Atlas) -> Stats {
    let (n, each) = (atlas.sets.len(), per_point(atlas));
    let (mut points, mut packed, mut holds) = (vec![0u64; n], vec![0.0; n], vec![0.0; n]);
    let mut newest = vec![0u64; n];
    for a in atlas.estimates_in_order().filter(|a| (a.set as usize) < n) {
        let set = a.set as usize;
        points[set] += 1;
        newest[set] = newest[set].max(a.generation as u64);
        if !matches!(a.algo, Algo::Unknown | Algo::None) {
            packed[set] += each;
            holds[set] += each * a.ratio as f64 / 100.0;
        }
    }
    Stats {
        area: points.iter().map(|&p| (p as f64 * each) as u64).collect(),
        packed,
        holds,
        newest,
    }
}

#[derive(Clone)]
pub struct Category {
    pub label: String,
    pub color: Color32,
    pub bytes: u64,
}

/// One row of the side list.
pub struct Row {
    pub set: u32,
    pub text: String,
    pub size: String,
    pub color: Color32,
}

/// Everything the side pane shows, for one mode, from one moment.
#[derive(Default)]
pub struct Side {
    pub sets: Arc<Vec<Arc<SetInfo>>>,
    pub stats: Stats,
    /// One color per set in the current mode.
    pub swatches: Vec<Color32>,
    pub list_title: &'static str,
    pub list: Vec<Row>,
    pub cats: Vec<Category>,
    pub summary: String,
    pub note: String,
    /// While the sample grid fills in: how far, and a label.
    pub progress: Option<(f32, String)>,
}

pub fn side(atlas: &Atlas, palette: &Palette, colorer: &Colorer, mode: Mode) -> Side {
    let stats = stats(atlas);
    let swatches: Vec<Color32> = (0..atlas.sets.len())
        .map(|i| colorer.swatch(i as u32, stats.packed[i], stats.holds[i], stats.newest[i]))
        .collect();
    let (list_title, list) = ranked(atlas, &stats, &swatches, mode);
    let progress = (atlas.known < atlas.valid).then(|| {
        let frac = atlas.known as f32 / atlas.valid.max(1) as f32;
        (frac, format!("sampling · {:.0}%", frac * 100.0))
    });
    Side {
        cats: categories(atlas, &stats, palette, colorer, mode),
        summary: summary(atlas, &stats),
        note: note(&stats, mode),
        sets: atlas.sets.clone(),
        stats,
        swatches,
        list_title,
        list,
        progress,
    }
}

pub fn kind_label(kind: Kind) -> &'static str {
    match kind {
        Kind::Data => "files",
        Kind::Unreachable => "unreachable",
        Kind::Free => "free",
        Kind::Metadata => "metadata",
        Kind::System => "system",
        Kind::Error => "error",
    }
}

pub fn describe(set: &SetInfo) -> String {
    let first = set.paths.first().map_or("(no path)", String::as_str);
    match set.kind {
        Kind::Data => first.to_string(),
        Kind::Unreachable => format!("unreachable: {first}"),
        Kind::Error => format!("error: {}", set.error),
        k => kind_label(k).to_string(),
    }
}

pub fn bytes(b: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let (mut v, mut i) = (b as f64, 0);
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    match (i, v) {
        (0, _) => format!("{b} B"),
        (_, v) if v < 10.0 => format!("{v:.2} {}", units[i]),
        (_, v) if v < 100.0 => format!("{v:.1} {}", units[i]),
        _ => format!("{v:.0} {}", units[i]),
    }
}

/// `YYYY-MM-DD` for a unix time (UTC), by Howard Hinnant's civil-from-days.
pub fn ymd(unix: i64) -> String {
    let z = unix.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Byte totals per legend entry for the current mode, in display order.
fn categories(
    atlas: &Atlas,
    stats: &Stats,
    palette: &Palette,
    colorer: &Colorer,
    mode: Mode,
) -> Vec<Category> {
    let mut cats: Vec<Category> = Vec::new();
    let mut add = |label: &str, color: Color32, bytes: u64| {
        match cats.iter_mut().find(|c| c.label == label) {
            Some(c) => c.bytes += bytes,
            None => cats.push(Category {
                label: label.to_string(),
                color,
                bytes,
            }),
        };
    };
    match mode {
        Mode::Owners | Mode::Sharing => {
            for (set, &area) in atlas.sets.iter().zip(&stats.area) {
                let color = match mode {
                    Mode::Owners => palette.kind(set.kind),
                    _ => palette.sharing(set),
                };
                let label = match (mode, set.kind) {
                    (Mode::Sharing, Kind::Data) if set.files < 2 => "unshared",
                    (Mode::Sharing, Kind::Data) => {
                        SHARE_BUCKETS[bucket(&SHARE_BUCKETS, set.files)].1
                    }
                    (_, k) => kind_label(k),
                };
                add(label, color, area);
            }
        }
        Mode::Compression | Mode::Age => {
            // Count points per slot, then label the slots: kinds by their number, then
            // "unknown", "uncompressed" and the five buckets.
            const KINDS: usize = 6;
            const UNKNOWN: usize = KINDS;
            const UNCOMPRESSED: usize = KINDS + 1;
            const BUCKETS: usize = KINDS + 2;
            let kinds: Vec<Kind> = atlas.sets.iter().map(|s| s.kind).collect();
            let cutoffs = colorer.cutoffs();
            let slot = |a: Answer| {
                let kind = *kinds.get(a.set as usize)?;
                Some(match (kind, mode) {
                    (Kind::Data | Kind::Unreachable, Mode::Compression) => match a.algo {
                        Algo::Unknown => UNKNOWN,
                        Algo::None => UNCOMPRESSED,
                        _ => BUCKETS + bucket(&RATIO_BUCKETS, a.ratio),
                    },
                    (Kind::Data | Kind::Unreachable, _) => {
                        match age_bucket(cutoffs, a.generation as u64) {
                            None => UNKNOWN,
                            Some(b) => BUCKETS + b,
                        }
                    }
                    (k, _) => k as usize,
                })
            };
            let tally = atlas
                .estimates()
                .fold(
                    || [0u64; BUCKETS + 5],
                    |mut t, a: Option<Answer>| {
                        if let Some(s) = a.and_then(slot) {
                            t[s] += 1;
                        }
                        t
                    },
                )
                .reduce(
                    || [0u64; BUCKETS + 5],
                    |a, b| std::array::from_fn(|i| a[i] + b[i]),
                );
            let each = per_point(atlas);
            let bytes = |n: u64| (n as f64 * each) as u64;
            for (i, &n) in tally[BUCKETS..].iter().enumerate().filter(|(_, n)| **n > 0) {
                match mode {
                    Mode::Compression => add(RATIO_BUCKETS[i].1, palette.ratio[i], bytes(n)),
                    _ => add(AGE_BUCKETS[i].1, palette.age[4 - i], bytes(n)),
                }
            }
            if tally[UNCOMPRESSED] > 0 {
                add("uncompressed", palette.neutral, bytes(tally[UNCOMPRESSED]));
            }
            if tally[UNKNOWN] > 0 {
                add("unknown", palette.unknown, bytes(tally[UNKNOWN]));
            }
            for k in [Kind::Metadata, Kind::System, Kind::Free, Kind::Error] {
                if tally[k as usize] > 0 {
                    add(kind_label(k), palette.kind(k), bytes(tally[k as usize]));
                }
            }
        }
    }
    let rank = |label: &str| {
        ["metadata", "system", "free", "error"]
            .iter()
            .position(|l| *l == label)
            .map_or(0, |i| i + 1)
    };
    cats.sort_by_key(|c| rank(&c.label));
    cats
}

/// The side list for the current mode.
fn ranked(
    atlas: &Atlas,
    stats: &Stats,
    swatches: &[Color32],
    mode: Mode,
) -> (&'static str, Vec<Row>) {
    let sets = &atlas.sets;
    let saved = |i: usize| stats.holds[i] - stats.packed[i];
    let mut ids: Vec<usize> = (0..sets.len()).filter(|&i| stats.area[i] > 0).collect();
    let title = match mode {
        Mode::Owners => "Largest sets",
        Mode::Sharing => {
            ids.retain(|&i| sets[i].kind == Kind::Data && sets[i].files > 1);
            "Largest shared sets"
        }
        Mode::Compression => {
            ids.retain(|&i| saved(i) > 0.0);
            "Most space saved"
        }
        Mode::Age => {
            ids.retain(|&i| stats.newest[i] > 0);
            "Most recently written"
        }
    };
    match mode {
        Mode::Compression => ids.sort_by(|&a, &b| saved(b).total_cmp(&saved(a))),
        Mode::Age => ids.sort_by_key(|&i| std::cmp::Reverse(stats.newest[i])),
        _ => ids.sort_by_key(|&i| std::cmp::Reverse(stats.area[i])),
    }
    let calibration = &atlas.header.calibration;
    let rows = ids
        .into_iter()
        .take(LIST_ROWS)
        .map(|i| {
            let set = &sets[i];
            let tag = match set.files {
                n if set.kind == Kind::Data && n > 1 => format!("×{n} "),
                _ => String::new(),
            };
            let size = match mode {
                Mode::Compression => format!("saves {}", bytes(saved(i) as u64)),
                Mode::Age => date_of(calibration, stats.newest[i])
                    .map_or("?".into(), |t| format!("≈ {}", ymd(t))),
                _ => bytes(stats.area[i]),
            };
            Row {
                set: i as u32,
                text: format!("{tag}{}", describe(set)),
                size,
                color: swatches[i],
            }
        })
        .collect();
    (title, rows)
}

fn summary(atlas: &Atlas, stats: &Stats) -> String {
    let shared: u64 = atlas
        .sets
        .iter()
        .zip(&stats.area)
        .filter(|(s, _)| s.kind == Kind::Data && s.files > 1)
        .map(|(_, a)| a)
        .sum();
    let file_sets = atlas.sets.iter().filter(|s| s.kind == Kind::Data).count();
    format!(
        "{} allocated · {file_sets} file sets · {} shared · sizes from {} samples",
        bytes(atlas.header.total),
        bytes(shared),
        atlas.known
    )
}

/// What the current mode's colors mean.
fn note(stats: &Stats, mode: Mode) -> String {
    let packed: f64 = stats.packed.iter().sum();
    let holds: f64 = stats.holds.iter().sum();
    match mode {
        Mode::Owners => "Each color is one exact set of files sharing those bytes.".into(),
        Mode::Sharing => "Colored by how many files reference each extent (reflinks, snapshots). Hardlinks are one file.".into(),
        Mode::Compression if packed > 0.0 => format!(
            "{} of compressed extents hold {}, saving {}. Colored by each extent's ratio.",
            bytes(packed as u64),
            bytes(holds as u64),
            bytes((holds - packed) as u64)
        ),
        Mode::Compression => "No compressed extents on this filesystem.".into(),
        Mode::Age => "Colored by when each extent was written, dated from the times btrfs records for its subvolumes. Approximate.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_print_as_calendar_days() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(1_790_000_000), "2026-09-21");
    }
}
