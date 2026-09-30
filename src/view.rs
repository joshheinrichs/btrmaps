use crate::proto::{Algo, Header, Kind, Msg, Request, Run, SetInfo};
use crate::treemap;
use anyhow::{Context, Result, bail};
use eframe::egui::{
    self, Color32, ColorImage, CornerRadius, Key, Pos2, Rect, RichText, Sense, Shape, Stroke,
    StrokeKind, TextureHandle, TextureOptions, Vec2, pos2, vec2,
};
use egui_taffy::taffy::prelude::{length, percent};
use egui_taffy::{TuiBuilderLogic, taffy, tui};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

/// sudo, baked in by packagers who know where it lives (NixOS's setuid wrapper is
/// outside the store), else found on PATH.
const SUDO: &str = match option_env!("SUDO") {
    Some(path) => path,
    None => "sudo",
};
/// Opens folders in the desktop's file manager.
const XDG_OPEN: &str = match option_env!("XDG_OPEN") {
    Some(path) => path,
    None => "xdg-open",
};

/// One place a filesystem is mounted, and which subvolume it shows there.
#[derive(Clone, Debug, PartialEq)]
pub struct Mount {
    pub path: PathBuf,
    pub subvolume: String,
}

/// A mounted btrfs filesystem and every place it is mounted.
#[derive(Clone, Debug, PartialEq)]
pub struct Filesystem {
    /// `major:minor`, shared by every mount of one filesystem.
    pub device: String,
    pub source: String,
    pub mounts: Vec<Mount>,
    pub label: Option<String>,
    /// Bytes total and used.
    pub usage: Option<(u64, u64)>,
}

impl Filesystem {
    /// Any mount reaches the whole filesystem; the shortest path reads best.
    fn scan_target(&self) -> Option<PathBuf> {
        self.mounts
            .iter()
            .map(|m| m.path.clone())
            .min_by_key(|p| p.as_os_str().len())
    }
}

/// mountinfo escapes space, tab, newline and backslash as octal.
fn unescape(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// The btrfs filesystems in a `/proc/self/mountinfo`, grouped by device, in source order.
pub fn parse_mountinfo(text: &str) -> Vec<Filesystem> {
    let mut found: Vec<Filesystem> = Vec::new();
    for line in text.lines() {
        let Some((mount, fs)) = line.split_once(" - ") else {
            continue;
        };
        let (mount, fs): (Vec<&str>, Vec<&str>) =
            (mount.split(' ').collect(), fs.split(' ').collect());
        let (Some(device), Some(root), Some(path)) = (mount.get(2), mount.get(3), mount.get(4))
        else {
            continue;
        };
        let (Some(&"btrfs"), Some(source)) = (fs.first(), fs.get(1)) else {
            continue;
        };
        let m = Mount {
            path: PathBuf::from(unescape(path)),
            subvolume: unescape(root),
        };
        match found.iter_mut().find(|f| f.device == *device) {
            Some(f) => f.mounts.push(m),
            None => found.push(Filesystem {
                device: device.to_string(),
                source: unescape(source),
                mounts: vec![m],
                label: None,
                usage: None,
            }),
        }
    }
    found.sort_by(|a, b| a.source.cmp(&b.source));
    found
}

fn usage(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: statvfs fills `st` for a NUL-terminated path.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let size = st.f_blocks as u64 * st.f_frsize as u64;
    Some((size, size - st.f_bfree as u64 * st.f_frsize as u64))
}

/// Filesystem labels by the device they resolve to.
fn labels() -> Vec<(PathBuf, String)> {
    let Ok(dir) = std::fs::read_dir("/dev/disk/by-label") else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| {
            let dev = std::fs::canonicalize(e.path()).ok()?;
            let name = e.file_name().to_string_lossy().replace("\\x20", " ");
            Some((dev, name))
        })
        .collect()
}

/// The mounted btrfs filesystems, with their size and label where available.
fn discover() -> Vec<Filesystem> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let labels = labels();
    parse_mountinfo(&text)
        .into_iter()
        .map(|fs| {
            let dev = std::fs::canonicalize(&fs.source).ok();
            Filesystem {
                label: labels
                    .iter()
                    .find(|(d, _)| Some(d) == dev.as_ref())
                    .map(|(_, l)| l.clone()),
                usage: fs.scan_target().and_then(|p| usage(&p)),
                ..fs
            }
        })
        .collect()
}

/// Not probed yet.
const UNSET: u32 = u32::MAX;
/// Largest number of rows in the side list.
const LIST_ROWS: usize = 200;
/// How often totals and the map catch up while a scan streams in. Each catch-up is a few
/// full passes over the cells (~100 ms at 2048²), so not every frame.
const REPAINT_EVERY: Duration = Duration::from_millis(500);
/// Unknown deep-view positions queued for probing at a time, per batch sent, and how
/// many batches may be outstanding: enough to keep the helper busy, few enough that
/// panning away doesn't leave a backlog of stale work.
const DEEP_QUEUE: usize = 8192;
const DEEP_BATCH: usize = 1024;
const DEEP_IN_FLIGHT: usize = 2;
/// How often the deep view is re-rendered while answers stream in.
const DEEP_EVERY: Duration = Duration::from_millis(80);
/// Longest side of the deep view's image, in pixels; bigger windows refine a little coarser.
const DEEP_MAX_SIDE: f32 = 2048.0;
/// Most tiles the treemap lays out; smaller things are drawn whole in their parent.
const TILE_BUDGET: usize = 30_000;
/// Height of the side pane's info section.
const INFO_HEIGHT: f32 = 220.0;

/// Everything received so far, at the finest resolution. A coarse cell is stored by
/// filling every fine cell it covers; finer levels arrive later and overwrite.
#[derive(Default, Clone)]
pub struct Model {
    pub header: Option<Header>,
    /// Shared, so a snapshot of the model copies pointers rather than every path.
    pub sets: Vec<Arc<SetInfo>>,
    /// Per fine cell, in curve order.
    pub set: Vec<u32>,
    pub algo: Vec<Algo>,
    /// Decompressed/on-disk ratio × 100.
    pub ratio: Vec<u32>,
    /// Generation that wrote the extent (0 = unknown).
    pub generation: Vec<u64>,
    /// Level of the latest cells, and how many of that level have arrived.
    pub level: u32,
    pub level_cells: usize,
    pub done: bool,
}

impl Model {
    /// Why `msg` doesn't fit what has arrived so far, if it doesn't.
    fn check(&self, msg: &Msg) -> Result<()> {
        match msg {
            Msg::Runs { runs } => match runs.iter().find(|r| r.2 as usize >= self.sets.len()) {
                Some(r) => bail!("run at {} uses unknown set {}", r.0, r.2),
                None => Ok(()),
            },
            Msg::Set(s) if s.id as usize != self.sets.len() => {
                bail!("set {} arrived out of order", s.id)
            }
            Msg::Cells { level, cells } => {
                let order = self.header.as_ref().context("cells before header")?.order;
                if *level > order {
                    bail!("level {level} is finer than order {order}");
                }
                if let Some(c) = cells
                    .iter()
                    .find(|c| c.1 as usize >= self.sets.len() || c.0 >= 1 << (2 * level))
                {
                    bail!("cell {} of level {level} uses unknown set {}", c.0, c.1);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Fold one message in. A message that doesn't fit is rejected and the model handed back.
    pub fn apply(mut self, msg: Msg) -> Result<Self, Box<(Self, anyhow::Error)>> {
        if let Err(e) = self.check(&msg) {
            return Err(Box::new((self, e)));
        }
        match msg {
            // Runs are kept by the model thread's deep view, not in the grid.
            Msg::Runs { .. } => {}
            Msg::Header(h) => {
                let cells = 1usize << (2 * h.order);
                self.set = vec![UNSET; cells];
                self.algo = vec![Algo::Unknown; cells];
                self.ratio = vec![0; cells];
                self.generation = vec![0; cells];
                self.header = Some(h);
            }
            Msg::Set(s) => self.sets.push(Arc::new(s)),
            Msg::Cells { level, cells } => {
                let order = self.header.as_ref().map_or(0, |h| h.order);
                let span = 1usize << (2 * (order - level));
                for c in &cells {
                    let start = c.0 as usize * span;
                    self.set[start..start + span].fill(c.1);
                    self.algo[start..start + span].fill(c.2);
                    self.ratio[start..start + span].fill(c.3);
                    self.generation[start..start + span].fill(c.4);
                }
                if level != self.level {
                    self.level_cells = 0;
                }
                self.level = level;
                self.level_cells += cells.len();
            }
            Msg::Done => self.done = true,
        }
        Ok(self)
    }
}

/// Curve index of pixel (x, y) on a `side`×`side` grid: the inverse of `d2xy`, in 64
/// bits for the block-level grids of the deep view.
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

/// Hilbert curve index -> (x, y) on a `side`×`side` grid.
pub fn d2xy(side: u32, d: u32) -> (u32, u32) {
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

/// Top bar controls that can move into the "…" menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BarItem {
    View,
    Modes,
    Frames,
    Refresh,
}

/// Which controls go into the "…" menu first when the bar runs out of room.
const OVERFLOW_ORDER: [BarItem; 4] = [
    BarItem::Frames,
    BarItem::View,
    BarItem::Refresh,
    BarItem::Modes,
];
const MODES: [(Mode, &str); 4] = [
    (Mode::Owners, "Owners"),
    (Mode::Sharing, "Sharing"),
    (Mode::Compression, "Compression"),
    (Mode::Age, "Age"),
];
/// Space between the bar's three regions, and what the "…" button and padding take.
const BAR_GAP: f32 = 16.0;
const BAR_MENU: f32 = 32.0;
const BAR_HEIGHT: f32 = 24.0;

/// How wide each part of the top bar was when last drawn (estimates until then).
#[derive(Clone, Copy, Debug)]
struct BarWidths {
    /// "btrmaps" and the filesystem menu, which never overflow.
    left: f32,
    /// Indexed by `BarItem as usize`.
    widths: [f32; 4],
}

impl Default for BarWidths {
    fn default() -> Self {
        BarWidths {
            left: 300.0,
            widths: [150.0, 300.0, 120.0, 90.0],
        }
    }
}

/// How many controls, from the front of OVERFLOW_ORDER, to move into the "…" menu for
/// the bar to fit `width`: the three groups side by side (the middle is centered when
/// there is room, and slides aside rather than overflowing when there is not). Bringing
/// one back needs some room to spare, so the bar doesn't flicker between two layouts
/// at the edge.
fn overflow_count(width: f32, bar: &BarWidths, now_hidden: usize) -> usize {
    let need = |hidden: usize| {
        let shown = |item: BarItem| !OVERFLOW_ORDER[..hidden].contains(&item);
        let sum = |items: &[BarItem]| -> f32 {
            let visible: Vec<f32> = items
                .iter()
                .filter(|&&i| shown(i))
                .map(|&i| bar.widths[i as usize])
                .collect();
            visible.iter().sum::<f32>() + 8.0 * visible.len().saturating_sub(1) as f32
        };
        let center = sum(&[BarItem::View, BarItem::Modes]);
        let right = sum(&[BarItem::Frames, BarItem::Refresh]);
        // The middle is centered when there is room and slides aside when there is not,
        // so only the sum of the three groups has to fit.
        let menu = if hidden > 0 { BAR_MENU } else { 0.0 };
        bar.left + center + right + menu + 2.0 * BAR_GAP + 8.0
    };
    (0..=OVERFLOW_ORDER.len())
        .find(|&hidden| need(hidden) + if hidden < now_hidden { 24.0 } else { 0.0 } <= width)
        .unwrap_or(OVERFLOW_ORDER.len())
}

/// What a right-click menu acts on: one set's files, or a treemap folder (by its path
/// from the top-level subvolume, since tree node ids change on rebuild).
#[derive(Clone, Debug, PartialEq)]
enum MenuTarget {
    Set(u32),
    Folder(String),
}

/// How the samples are drawn: in disk order on the curve, or by directory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Curve,
    Treemap,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Owners,
    Sharing,
    Compression,
    Age,
}

const SHARE_BUCKETS: [(usize, &str); 5] = [
    (2, "2 files"),
    (3, "3–4"),
    (5, "5–9"),
    (10, "10–49"),
    (50, "50+"),
];
const RATIO_BUCKETS: [(u32, &str); 5] = [
    (100, "<1.5×"),
    (150, "1.5–2×"),
    (200, "2–4×"),
    (400, "4–8×"),
    (800, "≥8×"),
];

fn bucket<T: PartialOrd + Copy>(buckets: &[(T, &str)], v: T) -> usize {
    buckets.iter().rposition(|(min, _)| v >= *min).unwrap_or(0)
}

/// Colors for one theme. Both sequential ramps are one hue, validated light→dark against
/// the background (reversed in dark mode so "more" always stands out).
pub struct Palette {
    dark: bool,
    free: Color32,
    metadata: Color32,
    system: Color32,
    error: Color32,
    neutral: Color32,
    unreachable: Color32,
    share: [Color32; 5],
    ratio: [Color32; 5],
    age: [Color32; 5],
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
                unreachable: hex(0xb9a89c),
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
                unreachable: hex(0x4a3f38),
                share: [0x184f95, 0x256abf, 0x3987e5, 0x6da7ec, 0x9ec5f4].map(hex),
                ratio: [0x8a3512, 0xb5461b, 0xd95926, 0xf08a5d, 0xf7b596].map(hex),
                age: [0x12603f, 0x1a7d55, 0x27a06f, 0x52c392, 0x93dcb9].map(hex),
            },
        }
    }

    fn kind(&self, kind: Kind) -> Option<Color32> {
        match kind {
            Kind::Free => Some(self.free),
            Kind::Metadata => Some(self.metadata),
            Kind::System => Some(self.system),
            Kind::Error => Some(self.error),
            Kind::Past => None,
            Kind::Data | Kind::Unreachable => Some(self.neutral),
        }
    }

    /// A distinct hue per set: ids step around the wheel by the golden angle.
    fn owner(&self, set: &SetInfo) -> Option<Color32> {
        if !matches!(set.kind, Kind::Data | Kind::Unreachable) {
            return self.kind(set.kind);
        }
        let i = set.id as f32;
        let hue = (i * 137.508) % 360.0;
        let (sat, light) = match set.kind {
            Kind::Unreachable => (30.0, if self.dark { 30.0 } else { 72.0 }),
            _ => (
                60.0 + (set.id * 7 % 25) as f32,
                (if self.dark { 50.0 } else { 44.0 }) + (set.id * 11 % 14) as f32,
            ),
        };
        Some(hsl(hue, sat, light))
    }

    fn sharing(&self, set: &SetInfo) -> Option<Color32> {
        match set.kind {
            Kind::Unreachable => Some(self.unreachable),
            Kind::Data if set.files < 2 => Some(self.neutral),
            Kind::Data => Some(self.share[bucket(&SHARE_BUCKETS, set.files)]),
            k => self.kind(k),
        }
    }

    fn compression(&self, set: &SetInfo, algo: Algo, ratio: u32) -> Option<Color32> {
        match (set.kind, algo) {
            (Kind::Data | Kind::Unreachable, Algo::Unknown) => Some(self.unreachable),
            (Kind::Data | Kind::Unreachable, Algo::None) => Some(self.neutral),
            (Kind::Data | Kind::Unreachable, _) => Some(self.ratio[bucket(&RATIO_BUCKETS, ratio)]),
            (k, _) => self.kind(k),
        }
    }

    /// Younger writes in stronger color, so recent churn stands out.
    fn age(&self, set: &SetInfo, age: Option<i64>) -> Option<Color32> {
        match (set.kind, age) {
            (Kind::Data | Kind::Unreachable, None) => Some(self.unreachable),
            (Kind::Data | Kind::Unreachable, Some(a)) => {
                Some(self.age[AGE_BUCKETS.len() - 1 - bucket(&AGE_BUCKETS, a)])
            }
            (k, _) => self.kind(k),
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

const DAY: i64 = 86_400;
/// Age since the scan, youngest first.
const AGE_BUCKETS: [(i64, &str); 5] = [
    (0, "< 1 day"),
    (DAY, "< 1 week"),
    (7 * DAY, "< 1 month"),
    (30 * DAY, "< 6 months"),
    (182 * DAY, "older"),
];

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

/// Seconds between the scan and when `generation` was written.
fn age_of(model: &Model, generation: u64) -> Option<i64> {
    let cal = &model.header.as_ref()?.calibration;
    let now = cal.last()?.1;
    date_of(cal, generation).map(|t| (now - t).max(0))
}

/// `YYYY-MM-DD` for a unix time (UTC), by Howard Hinnant's civil-from-days.
fn ymd(unix: i64) -> String {
    let z = unix.div_euclid(DAY) + 719_468;
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

/// Colors for the cells shown at each raster pixel (`cell_at`), transparent where there
/// is nothing. Owners and Sharing color by set alone, so each set's color is worked out
/// once and every cell is a lookup; the other modes depend on the cell's own extent.
fn cell_colors(model: &Model, palette: &Palette, mode: Mode, cell_at: &[u32]) -> Vec<Color32> {
    let clear = Color32::TRANSPARENT;
    let by_set: Vec<Color32> = match mode {
        Mode::Owners => model
            .sets
            .iter()
            .map(|s| palette.owner(s).unwrap_or(clear))
            .collect(),
        Mode::Sharing => model
            .sets
            .iter()
            .map(|s| palette.sharing(s).unwrap_or(clear))
            .collect(),
        Mode::Compression | Mode::Age => Vec::new(),
    };
    let cal = model
        .header
        .as_ref()
        .map_or(&[][..], |h| &h.calibration[..]);
    let now = cal.last().map_or(0, |p| p.1);
    cell_at
        .iter()
        .map(|&d| {
            let d = d as usize;
            let Some(set) = model.sets.get(model.set[d] as usize) else {
                return clear;
            };
            let color = match mode {
                Mode::Owners | Mode::Sharing => Some(by_set[set.id as usize]),
                Mode::Compression => palette.compression(set, model.algo[d], model.ratio[d]),
                Mode::Age => {
                    let age = date_of(cal, model.generation[d]).map(|t| (now - t).max(0));
                    palette.age(set, age)
                }
            };
            color.unwrap_or(clear)
        })
        .collect()
}

/// Totals derived from the cells, recomputed as data streams in.
#[derive(Default)]
struct Stats {
    /// Fine cells per set.
    area: Vec<u64>,
    /// Per set, on-disk bytes in compressed extents and the bytes they hold.
    packed: Vec<f64>,
    holds: Vec<f64>,
    /// Per set, the newest generation that wrote any of its sampled extents.
    newest: Vec<u64>,
}

/// Totals lag the stream by up to REPAINT_EVERY, so a set can arrive (and be hovered)
/// before it has any; it counts as zero until the next recompute.
impl Stats {
    fn area(&self, set: u32) -> u64 {
        self.area.get(set as usize).copied().unwrap_or(0)
    }
    fn packed(&self, set: u32) -> f64 {
        self.packed.get(set as usize).copied().unwrap_or(0.0)
    }
    fn holds(&self, set: u32) -> f64 {
        self.holds.get(set as usize).copied().unwrap_or(0.0)
    }
    fn newest(&self, set: u32) -> u64 {
        self.newest.get(set as usize).copied().unwrap_or(0)
    }
}

fn stats(model: &Model) -> Stats {
    let (cell, n) = (
        model.header.as_ref().map_or(0, |h| h.cell) as f64,
        model.sets.len(),
    );
    let (mut area, mut packed, mut holds) = (vec![0u64; n], vec![0.0; n], vec![0.0; n]);
    let mut newest = vec![0u64; n];
    for d in 0..model.set.len() {
        let set = model.set[d] as usize;
        if set >= n {
            continue;
        }
        area[set] += 1;
        newest[set] = newest[set].max(model.generation[d]);
        if !matches!(model.algo[d], Algo::Unknown | Algo::None) {
            packed[set] += cell;
            holds[set] += cell * model.ratio[d] as f64 / 100.0;
        }
    }
    Stats {
        area,
        packed,
        holds,
        newest,
    }
}

#[derive(Clone)]
struct Category {
    label: String,
    color: Color32,
    bytes: u64,
}

fn kind_label(kind: Kind) -> &'static str {
    match kind {
        Kind::Data => "files",
        Kind::Unreachable => "unreachable",
        Kind::Free => "free",
        Kind::Metadata => "metadata",
        Kind::System => "system",
        Kind::Past => "",
        Kind::Error => "error",
    }
}

/// Byte totals per legend entry for the current mode, in display order.
fn categories(model: &Model, stats: &Stats, palette: &Palette, mode: Mode) -> Vec<Category> {
    let cell = model.header.as_ref().map_or(0, |h| h.cell);
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
            for (set, &area) in model.sets.iter().zip(&stats.area) {
                let Some(color) = (match mode {
                    Mode::Owners => palette.kind(set.kind),
                    _ => palette.sharing(set),
                }) else {
                    continue;
                };
                let label = match (mode, set.kind) {
                    (Mode::Sharing, Kind::Data) if set.files < 2 => "unshared",
                    (Mode::Sharing, Kind::Data) => {
                        SHARE_BUCKETS[bucket(&SHARE_BUCKETS, set.files)].1
                    }
                    (_, k) => kind_label(k),
                };
                add(label, color, area * cell);
            }
        }
        Mode::Compression | Mode::Age => {
            // Count cells per slot first, then label the slots: finding a legend entry by
            // name for each of millions of cells was most of the cost.
            const KINDS: usize = 7;
            const UNKNOWN: usize = KINDS;
            const UNCOMPRESSED: usize = KINDS + 1;
            const BUCKETS: usize = KINDS + 2;
            let kinds: Vec<Kind> = model.sets.iter().map(|s| s.kind).collect();
            let cal = model
                .header
                .as_ref()
                .map_or(&[][..], |h| &h.calibration[..]);
            let now = cal.last().map_or(0, |p| p.1);
            let mut tally = [0u64; BUCKETS + 5];
            for d in 0..model.set.len() {
                let Some(&kind) = kinds.get(model.set[d] as usize) else {
                    continue;
                };
                let slot = match (kind, mode) {
                    (Kind::Data | Kind::Unreachable, Mode::Compression) => match model.algo[d] {
                        Algo::Unknown => UNKNOWN,
                        Algo::None => UNCOMPRESSED,
                        _ => BUCKETS + bucket(&RATIO_BUCKETS, model.ratio[d]),
                    },
                    (Kind::Data | Kind::Unreachable, _) => {
                        match date_of(cal, model.generation[d]) {
                            None => UNKNOWN,
                            Some(t) => BUCKETS + bucket(&AGE_BUCKETS, (now - t).max(0)),
                        }
                    }
                    (k, _) => k as usize,
                };
                tally[slot] += 1;
            }
            for (i, &n) in tally[BUCKETS..].iter().enumerate().filter(|(_, n)| **n > 0) {
                match mode {
                    Mode::Compression => add(RATIO_BUCKETS[i].1, palette.ratio[i], n * cell),
                    _ => add(AGE_BUCKETS[i].1, palette.age[4 - i], n * cell),
                }
            }
            if tally[UNCOMPRESSED] > 0 {
                add("uncompressed", palette.neutral, tally[UNCOMPRESSED] * cell);
            }
            if tally[UNKNOWN] > 0 {
                add("unknown", palette.unreachable, tally[UNKNOWN] * cell);
            }
            for k in [Kind::Metadata, Kind::System, Kind::Free, Kind::Error] {
                if let (n @ 1.., Some(color)) = (tally[k as usize], palette.kind(k)) {
                    add(kind_label(k), color, n * cell);
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

/// Set ids for the side list in the current mode, with the size column text.
fn ranked(model: &Model, stats: &Stats, mode: Mode) -> (&'static str, Vec<(u32, String)>) {
    let cell = model.header.as_ref().map_or(0, |h| h.cell);
    let mut ids: Vec<u32> = (0..model.sets.len() as u32)
        .filter(|&i| stats.area(i) > 0 && model.sets[i as usize].kind != Kind::Past)
        .collect();
    let saved = |i: u32| stats.holds(i) - stats.packed(i);
    let title = match mode {
        Mode::Owners => "Largest sets",
        Mode::Sharing => {
            ids.retain(|&i| {
                let s = &model.sets[i as usize];
                s.kind == Kind::Data && s.files > 1
            });
            "Largest shared sets"
        }
        Mode::Compression => {
            ids.retain(|&i| saved(i) > 0.0);
            "Most space saved"
        }
        Mode::Age => {
            ids.retain(|&i| stats.newest(i) > 0);
            "Most recently written"
        }
    };
    match mode {
        Mode::Compression => ids.sort_by(|&a, &b| saved(b).total_cmp(&saved(a))),
        Mode::Age => ids.sort_by_key(|&i| std::cmp::Reverse(stats.newest(i))),
        _ => ids.sort_by_key(|&i| std::cmp::Reverse(stats.area(i))),
    }
    let rows = ids
        .into_iter()
        .take(LIST_ROWS)
        .map(|i| {
            let size = match mode {
                Mode::Compression => format!("saves {}", bytes(saved(i) as u64)),
                Mode::Age => {
                    let cal = model
                        .header
                        .as_ref()
                        .map_or(&[][..], |h| &h.calibration[..]);
                    date_of(cal, stats.newest(i)).map_or("?".into(), |t| format!("≈ {}", ymd(t)))
                }
                _ => bytes(stats.area(i) * cell),
            };
            (i, size)
        })
        .collect();
    (title, rows)
}

fn bytes(b: u64) -> String {
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

fn describe(set: &SetInfo) -> String {
    let first = set.paths.first().map_or("(no path)", String::as_str);
    match set.kind {
        Kind::Data => first.to_string(),
        Kind::Unreachable => format!("unreachable: {first}"),
        Kind::Error => format!("error: {}", set.error),
        k => kind_label(k).to_string(),
    }
}

/// Which curve cell each raster pixel shows, for one grid size.
struct Grid {
    side: u32,
    cell_at: Vec<u32>,
}

impl Grid {
    fn new(order: u32) -> Self {
        let side = 1u32 << order;
        let mut cell_at = vec![0; (side * side) as usize];
        for d in 0..side * side {
            let (x, y) = d2xy(side, d);
            cell_at[(y * side + x) as usize] = d;
        }
        Grid { side, cell_at }
    }
}

/// What the scan helper reports.
enum Event {
    Msg(Result<Msg, String>),
    /// A progress or error line from its stderr.
    Status(String),
    Ended(Result<(), String>),
}

/// The filesystem being scanned, and at what resolution.
#[derive(Clone)]
struct Scan {
    target: PathBuf,
    order: u32,
}

struct App {
    filesystems: Vec<Filesystem>,
    /// The filesystem picked in the menu, by a mount point; scanned when Scan is pressed.
    selected: Option<PathBuf>,
    /// The map's shorter side on screen, in physical pixels, as last drawn; a scan's
    /// detail follows it.
    map_px: f32,
    scan: Option<Scan>,
    /// The model thread: where view changes go, and where snapshots come from.
    outbox: Option<Sender<Input>>,
    inbox: Option<Receiver<Snapshot>>,
    sent_look: Option<Look>,
    status: String,
    ended: Option<Result<(), String>>,
    model: Arc<Model>,
    error: Option<String>,
    grid: Option<Arc<Grid>>,
    mode: Mode,
    texture: Option<TextureHandle>,
    stats: Arc<Stats>,
    list: (&'static str, Vec<(u32, String)>),
    /// The focus the texture was last dimmed for.
    painted: Option<Option<u32>>,
    /// Map colors before dimming, and each pixel's set, in raster order.
    base: Arc<Vec<Color32>>,
    raster_set: Arc<Vec<u32>>,
    cats: Vec<Category>,
    /// The side pane's summary line and mode note.
    summary_text: String,
    note_text: String,
    zoom: f32,
    pan: Vec2,
    hover_cell: Option<u32>,
    hover_set: Option<u32>,
    pinned: Option<u32>,
    view: View,
    /// The directory treemap, built on the model thread.
    tree: Option<Arc<treemap::Tree>>,
    tree_gen: u64,
    /// The directory being looked at, as names from the root, so it survives rebuilds.
    tree_path: Vec<String>,
    tiles: Vec<treemap::Tile>,
    /// What `tiles` were laid out for: tree, node, pixel rect and folder frames.
    tiles_key: Option<(u64, u32, [i32; 4], bool)>,
    hover_node: Option<u32>,
    /// Folder outlines, padding and name strips in the treemap; off by default, since
    /// they take room from the contents and bend its areas away from scale.
    frames: bool,
    /// The refined view from the latest snapshot, its texture, what that texture was
    /// painted for (snapshot image, focus), and the viewport last sent for refinement.
    deep: Option<Arc<Deep>>,
    deep_texture: Option<TextureHandle>,
    deep_painted: Option<(usize, Option<u32>)>,
    sent_view: Option<Option<Viewport>>,
    /// Top bar widths as last drawn, and how many controls are in its "…" menu.
    bar: BarWidths,
    bar_hidden: usize,
    /// What the open right-click menu is about.
    menu_target: Option<MenuTarget>,
    /// Paths waiting for the user to confirm a permanent delete.
    confirm_delete: Option<Vec<PathBuf>>,
    /// The outcome of the last trash or delete, shown in the status bar.
    notice: Option<String>,
    /// A scan waiting for the password, what is typed so far, and why the last try failed.
    unlock: Option<Scan>,
    password: String,
    auth_error: Option<String>,
}

impl App {
    fn new() -> Self {
        let filesystems = discover();
        App {
            selected: default_selection(&filesystems),
            filesystems,
            map_px: 1024.0,
            scan: None,
            outbox: None,
            inbox: None,
            sent_look: None,
            status: String::new(),
            ended: None,
            model: Arc::new(Model::default()),
            error: None,
            grid: None,
            mode: Mode::Owners,
            texture: None,
            stats: Arc::new(Stats::default()),
            list: ("", Vec::new()),
            painted: None,
            base: Arc::new(Vec::new()),
            raster_set: Arc::new(Vec::new()),
            cats: Vec::new(),
            summary_text: String::new(),
            note_text: String::new(),
            zoom: 1.0,
            pan: Vec2::ZERO,
            hover_cell: None,
            hover_set: None,
            pinned: None,
            view: View::Curve,
            tree: None,
            tree_gen: 0,
            tree_path: Vec::new(),
            tiles: Vec::new(),
            tiles_key: None,
            hover_node: None,
            frames: false,
            deep: None,
            deep_texture: None,
            deep_painted: None,
            sent_view: None,
            bar: BarWidths::default(),
            bar_hidden: 0,
            menu_target: None,
            confirm_delete: None,
            notice: None,
            unlock: None,
            password: String::new(),
            auth_error: None,
        }
    }

    /// What derived state has to be computed for, right now.
    fn look(&self, ctx: &egui::Context) -> Look {
        Look {
            mode: self.mode,
            dark: ctx.theme() == egui::Theme::Dark,
        }
    }

    /// Drop the scan's data (stopping it if it is still running), keeping view settings.
    fn clear(&mut self) {
        self.outbox = None;
        self.inbox = None;
        self.sent_look = None;
        self.tree = None;
        self.deep = None;
        self.deep_painted = None;
        self.sent_view = None;
        self.tiles.clear();
        self.tiles_key = None;
        self.tree_path.clear();
        self.status.clear();
        self.notice = None;
        self.ended = None;
        self.model = Arc::new(Model::default());
        self.error = None;
        self.grid = None;
        self.stats = Arc::new(Stats::default());
        self.list = ("", Vec::new());
        self.pinned = None;
        self.hover_set = None;
        self.painted = None;
        self.base = Arc::new(Vec::new());
        self.raster_set = Arc::new(Vec::new());
        self.cats.clear();
        self.summary_text.clear();
        self.note_text.clear();
    }

    fn begin(&mut self, scan: Scan, password: Option<String>, ctx: &egui::Context) {
        self.clear();
        let look = self.look(ctx);
        let (outbox, inbox) = start(&scan, password, look, ctx);
        (self.outbox, self.inbox, self.sent_look) = (Some(outbox), Some(inbox), Some(look));
        self.scan = Some(scan);
        self.unlock = None;
        self.auth_error = None;
    }

    /// Scan now if sudo still has the password, otherwise ask for it first.
    fn request(&mut self, scan: Scan, ctx: &egui::Context) {
        if sudo_cached() {
            return self.begin(scan, None, ctx);
        }
        self.stop();
        self.unlock = Some(scan);
    }

    fn unlock(&mut self, ctx: &egui::Context) {
        let Some(scan) = self.unlock.take() else {
            return;
        };
        let password = std::mem::take(&mut self.password);
        self.begin(scan, Some(password), ctx);
    }

    fn stop(&mut self) {
        self.clear();
        self.scan = None;
    }

    fn focus(&self) -> Option<u32> {
        self.pinned.or(self.hover_set)
    }

    /// The set the map brightens, dimming the rest. Free space is nothing to look at.
    fn spotlight(&self) -> Option<u32> {
        let f = self.focus()?;
        let set = self.model.sets.get(f as usize)?;
        (set.kind != Kind::Free).then_some(f)
    }

    /// Tell the model thread when what is shown changes, so it recomputes for it.
    fn send_look(&mut self, ctx: &egui::Context) {
        let look = self.look(ctx);
        if self.sent_look == Some(look) {
            return;
        }
        if let Some(outbox) = &self.outbox {
            let _ = outbox.send(Input::Look(look));
        }
        self.sent_look = Some(look);
    }

    /// Take the newest snapshot, if one arrived: cheap moves, nothing recomputed here.
    fn receive(&mut self) {
        let Some(inbox) = &self.inbox else { return };
        let Some(snap) = inbox.try_iter().last() else {
            return;
        };
        if matches!(snap.ended, Some(Err(_))) && snap.model.header.is_none() {
            // Refused before it began, most likely a wrong password: ask again, with sudo's reason.
            self.auth_error = Some(snap.status.clone());
            self.unlock = self.scan.take();
            (self.outbox, self.inbox) = (None, None);
            (self.status, self.ended) = (snap.status, snap.ended);
            return;
        }
        self.model = snap.model;
        self.stats = snap.stats;
        self.list = snap.list;
        self.cats = snap.cats;
        self.summary_text = snap.summary;
        self.note_text = snap.note;
        self.grid = snap.grid;
        self.base = snap.base;
        self.raster_set = snap.raster_set;
        self.tree = snap.tree;
        self.tree_gen = snap.tree_gen;
        self.deep = snap.deep;
        self.status = snap.status;
        self.ended = snap.ended;
        self.error = snap.error;
        self.painted = None;
    }

    /// The selected filesystem, as the dropdown names it.
    fn current_filesystem(&self) -> Option<&Filesystem> {
        let target = self.selected.as_ref()?;
        self.filesystems
            .iter()
            .find(|fs| fs.mounts.iter().any(|m| &m.path == target))
    }

    /// App bar: name and filesystem on the left, view and mode centered, the rest on the
    /// right. Controls that don't fit move into a "…" menu at the end, least important
    /// first. Placed directly rather than by flexbox: three groups with known widths, and
    /// the widths measured here are what decides the overflow next frame.
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        use egui::{Align, Layout, UiBuilder};
        let current = self.current_filesystem().map(fs_name);
        let busy = self.scan.is_some();
        let full = ui.available_rect_before_wrap();
        let bar = Rect::from_min_size(
            full.min + vec2(4.0, 6.0),
            vec2(full.width() - 8.0, BAR_HEIGHT),
        );
        ui.allocate_rect(
            Rect::from_min_size(full.min, vec2(full.width(), BAR_HEIGHT + 12.0)),
            Sense::hover(),
        );
        // Folder frames only mean something in the treemap, full resolution on the curve.
        let applies = |item: BarItem| match item {
            BarItem::Frames => self.view == View::Treemap,
            _ => true,
        };
        let mut fit = self.bar;
        for item in OVERFLOW_ORDER.into_iter().filter(|&i| !applies(i)) {
            fit.widths[item as usize] = 0.0;
        }
        let hidden = overflow_count(bar.width(), &fit, self.bar_hidden);
        self.bar_hidden = hidden;
        let overflowed = |item: BarItem| OVERFLOW_ORDER[..hidden].contains(&item);
        let shown = |item: BarItem| applies(item) && !overflowed(item);
        let in_menu = |item: BarItem| applies(item) && overflowed(item);
        let (mut view, mut mode) = (self.view, self.mode);
        let mut frames = self.frames;
        let (mut pick, mut rediscover, mut refresh) = (None, false, false);
        let mut drawn = self.bar;
        let group = |ui: &mut egui::Ui, layout: Layout, rect: Rect| {
            let mut child = ui.new_child(UiBuilder::new().max_rect(rect).layout(layout));
            child.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            child
        };

        // Left: the name and the filesystem menu, which never overflow.
        let mut left = group(ui, Layout::left_to_right(Align::Center), bar);
        left.add(egui::Label::new(
            RichText::new("btrmaps").strong().size(16.0),
        ));
        egui::ComboBox::from_id_salt("filesystem")
            .width(240.0)
            .selected_text(match &current {
                Some(name) => {
                    let font = egui::TextStyle::Button.resolve(left.style());
                    ellipsize_middle(left.ctx(), name, &font, 200.0)
                }
                None => "Choose a filesystem…".to_string(),
            })
            .show_ui(&mut left, |ui| {
                for fs in &self.filesystems {
                    let usage = fs
                        .usage
                        .map(|(size, used)| format!(" · {} of {}", bytes(used), bytes(size)))
                        .unwrap_or_default();
                    let font = egui::TextStyle::Button.resolve(ui.style());
                    let name = ellipsize_middle(ui.ctx(), &fs_name(fs), &font, 320.0);
                    let chosen = current.as_deref() == Some(&fs_name(fs));
                    if ui
                        .selectable_label(chosen, format!("{name}{usage}"))
                        .clicked()
                    {
                        pick = fs.scan_target();
                    }
                }
                if self.filesystems.is_empty() {
                    ui.weak("No btrfs filesystems are mounted.");
                }
                ui.separator();
                rediscover = ui.button("Look again").clicked();
            });
        let left_edge = left.min_rect().right();
        drawn.left = left.min_rect().width();

        // Right, laid out from the right edge: "…", Refresh, then the view's checkboxes.
        let mut right = group(ui, Layout::right_to_left(Align::Center), bar);
        if OVERFLOW_ORDER.into_iter().any(in_menu) {
            right.menu_button("…", |ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                if in_menu(BarItem::View) {
                    for (v, label) in [(View::Curve, "Curve"), (View::Treemap, "Treemap")] {
                        ui.radio_value(&mut view, v, label);
                    }
                    ui.separator();
                }
                if in_menu(BarItem::Modes) {
                    for (m, label) in MODES {
                        ui.radio_value(&mut mode, m, label);
                    }
                    ui.separator();
                }
                if in_menu(BarItem::Frames) {
                    ui.checkbox(&mut frames, "Folder frames");
                }
                if in_menu(BarItem::Refresh) {
                    ui.separator();
                    refresh |= ui
                        .add_enabled(busy, egui::Button::new("⟳ Refresh"))
                        .clicked();
                }
            });
        }
        if shown(BarItem::Refresh) {
            let r = right.add_enabled(busy, egui::Button::new("⟳ Refresh"));
            drawn.widths[BarItem::Refresh as usize] = r.rect.width();
            refresh |= r.clicked();
        }
        if shown(BarItem::Frames) {
            let r = right.checkbox(&mut frames, "Folder frames");
            drawn.widths[BarItem::Frames as usize] = r.rect.width();
        }
        let right_edge = right.min_rect().left();

        // Middle: centered in the window, kept clear of both sides.
        let width = [BarItem::View, BarItem::Modes]
            .iter()
            .filter(|&&i| shown(i))
            .map(|&i| drawn.widths[i as usize] + 8.0)
            .sum::<f32>();
        let x = (bar.center().x - width / 2.0)
            .min(right_edge - BAR_GAP - width)
            .max(left_edge + BAR_GAP);
        let middle = Rect::from_min_max(pos2(x, bar.top()), pos2(bar.right(), bar.bottom()));
        let mut center = group(ui, Layout::left_to_right(Align::Center), middle);
        center.spacing_mut().button_padding = vec2(10.0, 3.0);
        let segments = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui)| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                add(ui);
            })
            .response
            .rect
            .width()
        };
        if shown(BarItem::View) {
            drawn.widths[BarItem::View as usize] = segments(&mut center, &mut |ui| {
                for (v, label) in [(View::Curve, "Curve"), (View::Treemap, "Treemap")] {
                    if ui.add(egui::Button::selectable(view == v, label)).clicked() {
                        view = v;
                    }
                }
            });
        }
        if shown(BarItem::Modes) {
            drawn.widths[BarItem::Modes as usize] = segments(&mut center, &mut |ui| {
                for (m, label) in MODES {
                    if ui.add(egui::Button::selectable(mode == m, label)).clicked() {
                        mode = m;
                    }
                }
            });
        }

        self.bar = drawn;
        let ctx = ui.ctx().clone();
        (self.view, self.mode) = (view, mode);
        self.frames = frames;
        if rediscover {
            self.filesystems = discover();
            let still_there = self.selected.as_ref().is_some_and(|t| {
                self.filesystems
                    .iter()
                    .any(|fs| fs.mounts.iter().any(|m| &m.path == t))
            });
            if !still_there {
                self.selected = default_selection(&self.filesystems);
            }
        }
        // Choosing a filesystem only selects it; the Scan button starts the scan.
        if let Some(target) = pick.filter(|t| self.selected.as_ref() != Some(t)) {
            self.stop();
            self.selected = Some(target);
        } else if refresh && let Some(scan) = &self.scan {
            // Detail follows the map's size on screen, so a refresh after resizing adapts.
            let scan = Scan {
                order: auto_order(self.map_px),
                ..scan.clone()
            };
            self.request(scan, &ctx);
        }
    }

    /// What is under the pointer, as the chain of folders down to it with their sizes.
    fn status_bar(&self, ui: &mut egui::Ui) {
        let cell = self.model.header.as_ref().map_or(0, |h| h.cell);
        let set_size = |s: u32| bytes(self.stats.area.get(s as usize).copied().unwrap_or(0) * cell);
        // What is hovered on the left, the outcome of the last trash or delete on the right.
        egui::containers::Sides::new()
            .shrink_left()
            .truncate()
            .show(
                ui,
                |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let hovered_set = self.hover_set.and_then(|s| self.model.sets.get(s as usize));
                    match (self.view, &self.tree, self.hover_node) {
                        (View::Treemap, Some(tree), Some(n)) => {
                            let chain = std::iter::once(treemap::ROOT).chain(tree.path(n));
                            for (i, c) in chain.enumerate() {
                                let node = &tree.nodes[c as usize];
                                if i > 0 {
                                    ui.weak("›");
                                }
                                ui.label(RichText::new(&node.name).monospace());
                                ui.weak(bytes(node.size));
                            }
                            // The piece under the pointer, when it is only part of its folder or file.
                            let piece = hovered_set.filter(|s| {
                                let whole = tree.nodes[n as usize].size;
                                self.stats
                                    .area
                                    .get(s.id as usize)
                                    .is_some_and(|a| a * cell != whole)
                            });
                            if let Some(set) = piece {
                                ui.weak("›");
                                ui.label(RichText::new(tile_name(set)).monospace());
                                let files = match set.files {
                                    1 => String::new(),
                                    f => format!(" · {f} files"),
                                };
                                ui.weak(format!("{}{files}", set_size(set.id)));
                            }
                        }
                        (View::Curve, _, _) if hovered_set.is_some() => {
                            let set = hovered_set.unwrap();
                            ui.label(RichText::new(describe(set)).monospace());
                            ui.weak(set_size(set.id));
                            if let Some(d) = self.hover_cell {
                                ui.weak(format!("· offset {}", bytes(d as u64 * cell)));
                            }
                        }
                        _ => {
                            ui.weak(match self.scan {
                                Some(_) => "Hover the map to see where space goes.",
                                None => "No filesystem open.",
                            });
                        }
                    }
                },
                |ui| {
                    if let Some(n) = &self.notice {
                        ui.label(RichText::new(n).strong());
                    } else if let (View::Curve, Some(d)) = (self.view, &self.deep) {
                        let sent = self.sent_view.flatten().is_some();
                        match (sent, d.unknown) {
                            (false, _) => {}
                            (true, 0) => {
                                ui.weak("full resolution");
                            }
                            (true, n) => {
                                ui.weak(format!("refining · {n} pixels to go"));
                            }
                        }
                    }
                },
            );
    }

    /// Before a scan: what is selected, and a big button to scan it.
    fn welcome(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        // The map will share this space with the side pane (380 wide by default).
        let room = ui.available_size() - vec2(380.0, 0.0);
        self.map_px = room.x.min(room.y).max(256.0) * ui.ctx().pixels_per_point();
        let chosen = self.current_filesystem().cloned();
        let mut scan = false;
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() / 3.0);
            match &chosen {
                Some(fs) => {
                    ui.heading(fs_name(fs));
                    if let Some((size, used)) = fs.usage {
                        ui.label(format!("{} of {} used", bytes(used), bytes(size)));
                    }
                    let mounts: Vec<String> = fs
                        .mounts
                        .iter()
                        .map(|m| m.path.display().to_string())
                        .collect();
                    ui.weak(mounts.join(" · "));
                    ui.add_space(16.0);
                    let button = egui::Button::new(RichText::new("Scan").size(22.0).strong())
                        .min_size(vec2(180.0, 52.0));
                    scan = ui.add(button).clicked();
                    ui.add_space(8.0);
                    ui.weak("Scanning reads the filesystem as root, so it asks for your password.");
                }
                None if self.filesystems.is_empty() => {
                    ui.heading("No btrfs filesystems are mounted");
                    ui.label("Mount one, then choose Look again in the menu at the top left.");
                }
                None => {
                    ui.heading("Where is the space going?");
                    ui.label("Choose a btrfs filesystem from the menu at the top left.");
                }
            }
            if let Some(Err(e)) = &self.ended {
                ui.add_space(8.0);
                ui.colored_label(palette.error, e);
                ui.label(RichText::new(&self.status).monospace().weak());
            }
        });
        if let (true, Some(target)) = (scan, self.selected.clone()) {
            let ctx = ui.ctx().clone();
            let order = auto_order(self.map_px);
            self.request(Scan { target, order }, &ctx);
        }
    }

    /// Right-click menu for a set's files or a treemap folder.
    fn context_menu(&mut self, ui: &mut egui::Ui) {
        let Some(target) = self.menu_target.clone() else {
            ui.close();
            return;
        };
        let (title, paths, complete, folder) = match &target {
            MenuTarget::Set(s) => {
                let Some(set) = self.model.sets.get(*s as usize) else {
                    ui.close();
                    return;
                };
                if !matches!(set.kind, Kind::Data | Kind::Unreachable) {
                    ui.weak(format!("{}: not files", kind_label(set.kind)));
                    return;
                }
                (
                    describe(set),
                    set.paths.clone(),
                    set.path_count == set.paths.len(),
                    false,
                )
            }
            MenuTarget::Folder(p) => (p.clone(), vec![p.clone()], true, true),
        };
        let mounts = self
            .current_filesystem()
            .map(|f| f.mounts.clone())
            .unwrap_or_default();
        let real: Vec<Option<PathBuf>> = paths.iter().map(|p| real_path(&mounts, p)).collect();
        let font = egui::TextStyle::Button.resolve(ui.style());
        ui.label(RichText::new(ellipsize_middle(ui.ctx(), &title, &font, 320.0)).strong());

        let first = real.first().cloned().flatten();
        if ui.button("Copy path").clicked() {
            let text = first
                .as_ref()
                .map_or(title.clone(), |p| p.display().to_string());
            ui.ctx().copy_text(text);
            ui.close();
        }
        let shown = first.clone().map(|p| {
            if folder {
                p
            } else {
                p.parent().map(Path::to_path_buf).unwrap_or(p)
            }
        });
        if ui
            .add_enabled(shown.is_some(), egui::Button::new("Show in folder"))
            .clicked()
        {
            if let Some(dir) = shown {
                let _ = std::process::Command::new(XDG_OPEN).arg(dir).spawn();
            }
            ui.close();
        }
        ui.separator();

        let blocked = if !complete {
            Some("More paths share this than btrmaps listed, so it can't free the space.")
        } else if real.iter().any(Option::is_none) {
            Some("Not mounted here, so btrmaps can't reach it.")
        } else if real.iter().flatten().any(|p| p.starts_with("/nix/store")) {
            Some("Managed by Nix: collect garbage instead.")
        } else {
            None
        };
        let what = match (paths.len(), folder) {
            (1, true) => "folder".to_string(),
            (1, false) => "file".to_string(),
            (n, _) => format!("{n} paths"),
        };
        let targets: Vec<PathBuf> = real.into_iter().flatten().collect();
        let trash = ui
            .add_enabled(
                blocked.is_none(),
                egui::Button::new(format!("Move {what} to Trash")),
            )
            .on_disabled_hover_text(blocked.unwrap_or_default());
        if trash.clicked() {
            self.notice = Some(match trash::delete_all(&targets) {
                Ok(()) => format!(
                    "Moved {what} to Trash. Space frees when the Trash is emptied; Refresh to update the map."
                ),
                Err(e) => format!("Couldn't move to Trash: {e}"),
            });
            ui.close();
        }
        let delete = ui
            .add_enabled(
                blocked.is_none(),
                egui::Button::new(format!("Delete {what}…")),
            )
            .on_disabled_hover_text(blocked.unwrap_or_default());
        if delete.clicked() {
            self.confirm_delete = Some(targets);
            ui.close();
        }
        if paths.len() > 1 {
            ui.weak("The space is only freed once every path is gone.");
        }
    }

    /// Asks before deleting anything for good.
    fn delete_prompt(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        let Some(targets) = &self.confirm_delete else {
            return;
        };
        let (mut delete, mut cancel) = (false, false);
        egui::Modal::new(egui::Id::new("delete")).show(ui.ctx(), |ui| {
            ui.set_width(420.0);
            ui.heading("Delete permanently?");
            ui.label("This can't be undone.");
            let font = egui::TextStyle::Monospace.resolve(ui.style());
            for p in targets.iter().take(8) {
                let text = ellipsize_middle(ui.ctx(), &p.display().to_string(), &font, 400.0);
                ui.label(RichText::new(text).monospace());
            }
            if targets.len() > 8 {
                ui.weak(format!("… and {} more", targets.len() - 8));
            }
            ui.horizontal(|ui| {
                let button = egui::Button::new(RichText::new("Delete").color(palette.error));
                delete = ui.add(button).clicked();
                cancel = ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape));
            });
        });
        if delete {
            let targets = self.confirm_delete.take().unwrap_or_default();
            let failed: Vec<String> = targets
                .iter()
                .filter_map(|p| {
                    let is_dir = p.symlink_metadata().is_ok_and(|m| m.is_dir());
                    let removed = if is_dir {
                        std::fs::remove_dir_all(p)
                    } else {
                        std::fs::remove_file(p)
                    };
                    removed.err().map(|e| format!("{}: {e}", p.display()))
                })
                .collect();
            self.notice = Some(match failed.as_slice() {
                [] => format!(
                    "Deleted {} item(s). Refresh to update the map.",
                    targets.len()
                ),
                [one] => format!("Couldn't delete {one}"),
                many => format!("Couldn't delete {} items, e.g. {}", many.len(), many[0]),
            });
        } else if cancel {
            self.confirm_delete = None;
        }
    }

    /// The scan runs as root, so sudo needs the user's password; ask for it right here.
    fn password_prompt(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        let Some(scan) = &self.unlock else { return };
        let target = scan.target.display().to_string();
        let user = std::env::var("USER").unwrap_or_else(|_| "you".into());
        let ctx = ui.ctx().clone();
        let (mut submit, mut cancel) = (false, false);
        egui::Modal::new(egui::Id::new("password")).show(&ctx, |ui| {
            ui.set_width(360.0);
            ui.heading("Scanning needs root");
            ui.label(format!(
                "btrmaps reads {target} as root with sudo. Enter the password for {user}."
            ));
            if let Some(e) = &self.auth_error {
                ui.colored_label(palette.error, e);
            }
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.password)
                    .password(true)
                    .hint_text("Password")
                    .desired_width(f32::INFINITY),
            );
            submit = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            // Keep typing going to the field, except on the frame Enter releases it.
            if !field.has_focus() && !submit {
                field.request_focus();
            }
            ui.horizontal(|ui| {
                submit |= ui.button("Scan").clicked();
                cancel = ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape));
            });
        });
        if cancel {
            self.password.clear();
            self.unlock = None;
            self.auth_error = None;
        } else if submit {
            self.unlock(&ctx);
        }
    }

    /// Upload the refined view, dimmed like the map when a set is focused; only when the
    /// snapshot brought a new rendering or the focus moved.
    fn repaint_deep(&mut self, ctx: &egui::Context) {
        let Some(deep) = &self.deep else { return };
        let focus = self.spotlight();
        let key = (Arc::as_ptr(deep) as usize, focus);
        if self.deep_painted == Some(key) {
            return;
        }
        let pixels: Vec<Color32> = match focus {
            None => deep.pixels.clone(),
            Some(f) => deep
                .pixels
                .iter()
                .zip(&deep.sets)
                .map(|(&c, &s)| {
                    if s == f || s == UNSET {
                        c
                    } else {
                        c.gamma_multiply(0.18)
                    }
                })
                .collect(),
        };
        let size = [deep.view.w as usize, deep.view.h as usize];
        let image = ColorImage::new(size, pixels);
        match &mut self.deep_texture {
            Some(t) => t.set(image, TextureOptions::NEAREST),
            None => {
                self.deep_texture = Some(ctx.load_texture("deep", image, TextureOptions::NEAREST))
            }
        }
        self.deep_painted = Some(key);
    }

    /// Put the snapshot's map colors on screen, dimming all but the focused set. This is
    /// the only per-cell work on the window's thread, and only when the focus or the
    /// snapshot changes; everything it reads was computed on the model thread.
    fn repaint(&mut self, ctx: &egui::Context) {
        let Some(grid) = &self.grid else { return };
        if self.view != View::Curve || self.base.len() != grid.cell_at.len() {
            return;
        }
        let focus = self.spotlight();
        if self.painted == Some(focus) {
            return;
        }
        let pixels: Vec<Color32> = match focus {
            None => self.base.to_vec(),
            Some(f) => self
                .base
                .iter()
                .zip(self.raster_set.iter())
                .map(|(&c, &s)| if s == f { c } else { c.gamma_multiply(0.18) })
                .collect(),
        };
        let side = grid.side as usize;
        let image = ColorImage::new([side, side], pixels);
        match &mut self.texture {
            Some(t) => t.set(image, TextureOptions::NEAREST),
            None => self.texture = Some(ctx.load_texture("map", image, TextureOptions::NEAREST)),
        }
        self.painted = Some(focus);
    }

    fn side_panel(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        let h = self.model.header.clone();
        let cats = self.cats.clone();
        let summary = h.as_ref().map(|_| self.summary_text.clone());
        let progress = h.as_ref().filter(|_| !self.model.done).map(|h| {
            let frac = self.model.level_cells as f32 / (1usize << (2 * self.model.level)) as f32;
            let text = format!(
                "scanning level {}/{} · {:.0}%",
                self.model.level,
                h.order,
                frac * 100.0
            );
            (frac, text)
        });
        let note = self.note_text.clone();

        tui(ui, ui.id().with("side-head"))
            .reserve_available_width()
            .style(column(8.0))
            .show(|tui| {
                if let Some(Err(e)) = &self.ended {
                    tui.ui_add(egui::Label::new(RichText::new(e).color(palette.error)).wrap());
                    tui.ui_add(
                        egui::Label::new(RichText::new(&self.status).monospace().weak()).wrap(),
                    );
                }
                let (Some(_), Some(summary)) = (&h, summary) else {
                    if self.ended.is_none() {
                        tui.label("Waiting for the scan to start…");
                    }
                    if !self.status.is_empty() {
                        tui.ui_add(
                            egui::Label::new(RichText::new(&self.status).monospace().weak()).wrap(),
                        );
                    }
                    return;
                };
                tui.ui_add(egui::Label::new(RichText::new(summary).weak()).wrap());
                if let Some((frac, text)) = progress {
                    tui.ui_add(egui::ProgressBar::new(frac).text(text));
                }

                let total: u64 = cats.iter().map(|c| c.bytes).sum::<u64>().max(1);
                tui.style(taffy::Style {
                    size: taffy::Size {
                        width: percent(1.0_f32),
                        height: length(8.0_f32),
                    },
                    ..Default::default()
                })
                .ui(|ui| {
                    let (bar, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 8.0), Sense::hover());
                    let mut x = bar.left();
                    for c in &cats {
                        let w = bar.width() * c.bytes as f32 / total as f32;
                        let r = Rect::from_min_size(
                            pos2(x, bar.top()),
                            vec2((w - 2.0).max(2.0), bar.height()),
                        );
                        ui.painter().rect_filled(r, CornerRadius::same(2), c.color);
                        x += w;
                    }
                });

                // Legend chips wrap as whole units, never between a label and its size.
                tui.style(taffy::Style {
                    flex_wrap: taffy::FlexWrap::Wrap,
                    gap: taffy::Size {
                        width: length(12.0_f32),
                        height: length(4.0_f32),
                    },
                    ..row(0.0)
                })
                .add(|tui| {
                    for c in &cats {
                        tui.style(row(6.0)).add(|tui| {
                            tui.ui(|ui| swatch(ui, c.color));
                            tui.ui_add(egui::Label::new(&c.label).extend());
                            tui.ui_add(
                                egui::Label::new(RichText::new(bytes(c.bytes)).weak()).extend(),
                            );
                        });
                    }
                });
                tui.ui_add(egui::Label::new(RichText::new(note).weak()).wrap());
                if let Some(e) = &self.error {
                    tui.ui_add(
                        egui::Label::new(
                            RichText::new(format!("stream error: {e}")).color(palette.error),
                        )
                        .wrap(),
                    );
                }
            });

        let Some(h) = h else { return };

        ui.separator();
        // A fixed height, so what is hovered never moves the list below it.
        egui::ScrollArea::vertical()
            .id_salt("info")
            .min_scrolled_height(INFO_HEIGHT)
            .max_height(INFO_HEIGHT)
            .show(ui, |ui| {
                ui.set_min_height(INFO_HEIGHT);
                self.info(ui, palette, &h)
            });
        ui.separator();

        ui.label(RichText::new(self.list.0).small().strong());
        let focus = self.focus();
        let mut hovered_row = None;
        let menu_open = egui::Popup::is_any_open(ui.ctx());
        egui::ScrollArea::vertical().show(ui, |ui| {
            if self.list.1.is_empty() {
                ui.weak("Nothing here.");
            }
            // A copy, so a row's menu can act on the app while the list is drawn.
            let rows = self.list.1.clone();
            for (id, size) in &rows {
                let set = &self.model.sets[*id as usize];
                let bg = ui.painter().add(Shape::Noop);
                let tag = match set.files {
                    n if set.kind == Kind::Data && n > 1 => format!("×{n} "),
                    _ => String::new(),
                };
                let row = ui
                    .scope(|ui| {
                        egui::containers::Sides::new()
                            .shrink_left()
                            .truncate()
                            .show(
                                ui,
                                |ui| {
                                    swatch(
                                        ui,
                                        set_swatch(
                                            &self.model,
                                            set,
                                            &self.stats,
                                            palette,
                                            self.mode,
                                        ),
                                    );
                                    // Cut long paths in the middle, keeping the file name.
                                    let font = egui::TextStyle::Monospace.resolve(ui.style());
                                    let text = format!("{tag}{}", describe(set));
                                    let fitted = ellipsize_middle(
                                        ui.ctx(),
                                        &text,
                                        &font,
                                        ui.available_width() - 2.0,
                                    );
                                    ui.label(RichText::new(fitted).monospace())
                                        .on_hover_text(text);
                                },
                                |ui| ui.weak(size),
                            )
                    })
                    .response;
                let row = ui.interact(row.rect, egui::Id::new(("row", id)), Sense::click());
                if row.secondary_clicked() {
                    self.menu_target = Some(MenuTarget::Set(*id));
                }
                row.context_menu(|ui| self.context_menu(ui));
                if row.hovered() {
                    hovered_row = Some(*id);
                }
                if row.clicked() && !menu_open {
                    self.pinned = if self.pinned == Some(*id) {
                        None
                    } else {
                        Some(*id)
                    };
                }
                if focus == Some(*id) || row.hovered() {
                    let fill = ui.visuals().widgets.hovered.weak_bg_fill;
                    ui.painter()
                        .set(bg, Shape::rect_filled(row.rect.expand(2.0), 3, fill));
                }
            }
        });
        if hovered_row.is_some() {
            self.hover_set = hovered_row;
            self.hover_cell = None;
        }
    }

    fn info(&self, ui: &mut egui::Ui, palette: &Palette, h: &Header) {
        let Some(id) = self.focus() else {
            ui.weak("Hover a cell to see which files own it.");
            return;
        };
        let set = &self.model.sets[id as usize];
        let area = self.stats.area(id) * h.cell;
        ui.horizontal(|ui| {
            swatch(
                ui,
                set_swatch(&self.model, set, &self.stats, palette, self.mode),
            );
            ui.strong(kind_label(set.kind));
            if self.pinned == Some(id) {
                ui.weak("· pinned");
            }
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new(bytes(area)).size(20.0).strong());
            ui.weak(format!(
                "{:.2}% · {} cells",
                100.0 * area as f64 / h.total as f64,
                self.stats.area(id)
            ));
        });
        if let Some(d) = self
            .hover_cell
            .filter(|&d| self.model.set[d as usize] == id)
        {
            let d = d as usize;
            let extent = match (set.kind, self.model.algo[d]) {
                (Kind::Data | Kind::Unreachable, Algo::Unknown) => " · extent unknown".to_string(),
                (Kind::Data | Kind::Unreachable, Algo::None) => " · uncompressed".to_string(),
                (Kind::Data | Kind::Unreachable, algo) => format!(
                    " · {} {:.2}×",
                    format!("{algo:?}").to_lowercase(),
                    self.model.ratio[d] as f64 / 100.0
                ),
                _ => String::new(),
            };
            ui.label(
                RichText::new(format!("offset {}{extent}", bytes(d as u64 * h.cell)))
                    .monospace()
                    .weak(),
            );
            let generation = self.model.generation[d];
            if let Some(t) = date_of(&h.calibration, generation) {
                ui.label(
                    RichText::new(format!("written ≈ {} · generation {generation}", ymd(t)))
                        .monospace()
                        .weak(),
                );
            }
        }
        if set.kind == Kind::Unreachable {
            ui.weak("Bytes no file uses any more, kept alive by an extent these files still partly reference.");
        }
        if set.kind == Kind::Error {
            ui.label(RichText::new(&set.error).monospace());
        }
        let packed = self.stats.packed(id);
        if packed > 0.0 {
            ui.weak(format!(
                "{} of compressed extents hold {}",
                bytes(packed as u64),
                bytes(self.stats.holds(id) as u64)
            ));
        }
        if matches!(set.kind, Kind::Data | Kind::Unreachable) {
            let subvols = match set.subvolumes {
                n if n > 1 => format!(" across {n} subvolumes"),
                _ => String::new(),
            };
            ui.weak(format!(
                "{} file{}{subvols} · {} path{}{}",
                set.files,
                if set.files == 1 { "" } else { "s" },
                set.path_count,
                if set.path_count == 1 { "" } else { "s" },
                if set.truncated {
                    " · more references than shown"
                } else {
                    ""
                }
            ));
            for p in &set.paths {
                ui.label(RichText::new(p).monospace());
            }
            if set.path_count > set.paths.len() {
                ui.weak(format!("… {} more", set.path_count - set.paths.len()));
            }
        }
    }

    /// Rebuild the directory tree from the samples when they changed, at most every
    /// half second while a scan streams in.
    fn treemap(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        // A click on an open menu's item must not also land on the tile beneath it.
        let menu_open = egui::Popup::is_any_open(ui.ctx());
        let Some(tree) = &self.tree else {
            ui.weak("waiting for scan…");
            return;
        };
        let node = tree.find(&self.tree_path);
        let chain = tree.path(node);
        self.tree_path.truncate(chain.len());

        // Breadcrumbs back up the tree, then what is under the pointer.
        let mut up_to = None;
        // One line always: a taller row would shrink the map and re-lay it out.
        ui.horizontal(|ui| {
            if ui
                .selectable_label(chain.is_empty(), &tree.nodes[treemap::ROOT as usize].name)
                .clicked()
            {
                up_to = Some(0);
            }
            for (depth, &n) in chain.iter().enumerate() {
                ui.weak("›");
                if ui
                    .selectable_label(depth + 1 == chain.len(), &tree.nodes[n as usize].name)
                    .clicked()
                {
                    up_to = Some(depth + 1);
                }
            }
        });

        let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click());
        let r = resp.rect;
        self.map_px = r.width().min(r.height()) * ui.ctx().pixels_per_point();
        let key = (
            self.tree_gen,
            node,
            [
                r.min.x as i32,
                r.min.y as i32,
                r.width() as i32,
                r.height() as i32,
            ],
            self.frames,
        );
        if self.tiles_key != Some(key) {
            let area = treemap::Rect {
                x: r.min.x,
                y: r.min.y,
                w: r.width(),
                h: r.height(),
            };
            self.tiles = treemap::layout(tree, node, area, 3.0, TILE_BUDGET, self.frames);
            self.tiles_key = Some(key);
        }

        // The deepest tile under the pointer is the last one drawn there.
        let under = resp
            .hover_pos()
            .and_then(|p| self.tiles.iter().rev().find(|t| t.rect.contains(p.x, p.y)))
            .copied();
        self.hover_cell = None;
        self.hover_node = if resp.hovered() {
            under.map(|t| t.node)
        } else {
            None
        };
        if resp.hovered() {
            self.hover_set = under.and_then(|t| t.set);
        }
        if resp.clicked()
            && !menu_open
            && let Some(t) = under
        {
            // Go straight to the deepest folder holding what was clicked, and pin it.
            let folder = match tree.nodes[t.node as usize].children.is_empty() {
                // A file, or a file's pieces: its folder.
                true => tree.nodes[t.node as usize].parent.unwrap_or(treemap::ROOT),
                false => t.node,
            };
            self.tree_path = tree
                .path(folder)
                .iter()
                .map(|&c| tree.nodes[c as usize].name.clone())
                .collect();
            if t.set.is_some() {
                self.pinned = t.set;
            }
        }
        if resp.secondary_clicked() {
            self.menu_target = under.map(|t| match (t.kind, t.set) {
                (treemap::TileKind::Frame, _) | (_, None) => {
                    let names: Vec<&str> = tree
                        .path(t.node)
                        .iter()
                        .map(|&c| tree.nodes[c as usize].name.as_str())
                        .collect();
                    MenuTarget::Folder(names.join("/"))
                }
                (_, Some(s)) => MenuTarget::Set(s),
            });
        }
        let editing = ui.memory(|m| m.focused().is_some());
        if !editing && ui.input(|i| i.key_pressed(Key::Backspace)) {
            self.tree_path.pop();
        }
        if let Some(depth) = up_to {
            self.tree_path.truncate(depth);
        }

        // All fills in one mesh; frames come before their contents, so they sit below.
        let focus = self.spotlight();
        let frame_fill = ui.visuals().faint_bg_color;
        let mut mesh = egui::Mesh::default();
        for t in &self.tiles {
            let rect = Rect::from_min_size(pos2(t.rect.x, t.rect.y), vec2(t.rect.w, t.rect.h));
            let base = match (t.kind, t.set) {
                (treemap::TileKind::Frame, _) | (_, None) => frame_fill,
                (_, Some(s)) => {
                    let set = &self.model.sets[s as usize];
                    let c = set_swatch(&self.model, set, &self.stats, palette, self.mode);
                    match focus {
                        Some(f) if f != s => c.gamma_multiply(0.25),
                        _ => c,
                    }
                }
            };
            mesh.add_colored_rect(rect, base);
        }
        painter.add(Shape::mesh(mesh));

        // Folder outlines and name strips, only with frames on; without them, areas are exact.
        let line = Stroke::new(1.0, ui.visuals().window_stroke.color);
        let text = ui.visuals().text_color();
        for t in &self.tiles {
            if !self.frames || t.kind != treemap::TileKind::Frame {
                continue;
            }
            let rect = Rect::from_min_size(pos2(t.rect.x, t.rect.y), vec2(t.rect.w, t.rect.h));
            painter.rect_stroke(rect, 0, line, StrokeKind::Inside);
            if t.rect.w > 60.0 && t.rect.h > 3.0 * treemap::LABEL {
                let name = format!("{} · {}", tree.nodes[t.node as usize].name, bytes(t.bytes));
                painter.with_clip_rect(rect).text(
                    rect.left_top() + vec2(4.0, 1.0),
                    egui::Align2::LEFT_TOP,
                    name,
                    egui::FontId::proportional(12.0),
                    text,
                );
            }
        }
        // File names on tiles big enough to read, in whichever ink shows on the fill.
        for t in &self.tiles {
            let big = t.rect.w > 70.0 && t.rect.h > 18.0;
            let (Some(s), true) = (t.set, big && t.kind != treemap::TileKind::Frame) else {
                continue;
            };
            let set = &self.model.sets[s as usize];
            let name = match t.kind {
                treemap::TileKind::Collapsed => tree.nodes[t.node as usize].name.clone(),
                _ if set.files == 1 && set.path_count > 1 => {
                    format!("{} ({} hardlinks)", tile_name(set), set.path_count)
                }
                _ if set.path_count > 1 => {
                    format!("{} shared by {}", tile_name(set), set.files)
                }
                _ => tile_name(set),
            };
            let fill = set_swatch(&self.model, set, &self.stats, palette, self.mode);
            let ink = if fill.intensity() > 0.55 {
                Color32::BLACK
            } else {
                Color32::WHITE
            };
            let rect = Rect::from_min_size(pos2(t.rect.x, t.rect.y), vec2(t.rect.w, t.rect.h));
            painter.with_clip_rect(rect.shrink(2.0)).text(
                rect.left_top() + vec2(4.0, 2.0),
                egui::Align2::LEFT_TOP,
                format!("{name} · {}", bytes(t.bytes)),
                egui::FontId::proportional(12.0),
                ink,
            );
        }
        // Hierarchical highlight: every folder above what is hovered, the nearest strongest.
        if let Some(n) = self.hover_node {
            let chain = tree.path(n);
            let strong = ui.visuals().strong_text_color();
            let frames: Vec<&treemap::Tile> = self
                .tiles
                .iter()
                .filter(|t| t.kind == treemap::TileKind::Frame && chain.contains(&t.node))
                .collect();
            for (i, t) in frames.iter().enumerate() {
                let nearest = i + 1 == frames.len();
                let stroke = match nearest {
                    true => Stroke::new(2.5, strong),
                    false => Stroke::new(1.5, strong.gamma_multiply(0.45)),
                };
                let rect = Rect::from_min_size(pos2(t.rect.x, t.rect.y), vec2(t.rect.w, t.rect.h));
                painter.rect_stroke(rect, 0, stroke, StrokeKind::Inside);
            }
        }
        if let Some(t) = under {
            let rect = Rect::from_min_size(pos2(t.rect.x, t.rect.y), vec2(t.rect.w, t.rect.h));
            painter.rect_stroke(
                rect,
                0,
                Stroke::new(1.5, ui.visuals().strong_text_color()),
                StrokeKind::Inside,
            );
        }
        resp.context_menu(|ui| self.context_menu(ui));
    }

    /// Tell the model thread which part of the curve to refine, when that changes.
    fn send_view(&mut self, view: Option<Viewport>) {
        if self.sent_view == Some(view) {
            return;
        }
        if let Some(outbox) = &self.outbox {
            let _ = outbox.send(Input::View(view));
        }
        self.sent_view = Some(view);
    }

    fn map(&mut self, ui: &mut egui::Ui) {
        // A click on an open menu's item must not also land on the map beneath it.
        let menu_open = egui::Popup::is_any_open(ui.ctx());
        let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        let (Some(grid), Some(texture), Some(h)) = (&self.grid, &self.texture, &self.model.header)
        else {
            painter.text(
                resp.rect.center(),
                egui::Align2::CENTER_CENTER,
                "waiting for scan…",
                egui::FontId::proportional(16.0),
                ui.visuals().weak_text_color(),
            );
            return;
        };
        let (grid, texture_id, total) = (grid.clone(), texture.id(), h.total);
        let rect = resp.rect;
        self.map_px = rect.width().min(rect.height()) * ui.ctx().pixels_per_point();
        let fit = rect.width().min(rect.height()) * 0.94;
        let origin = |zoom: f32, pan: Vec2| rect.center() - Vec2::splat(fit * zoom / 2.0) + pan;
        let ppp = ui.ctx().pixels_per_point();
        // Zooming goes down to a few pixels per block.
        let deep_side = (1u64 << deep_order(total).0) as f32;
        let max_zoom = (deep_side * 8.0 / fit).max(1.0);

        if resp.dragged() {
            self.pan += resp.drag_delta();
        }
        if resp.double_clicked() {
            self.zoom = 1.0;
            self.pan = Vec2::ZERO;
        }
        // Scroll or pinch zooms around the pointer; + and - around the middle of the view.
        let typing = ui.memory(|m| m.focused().is_some());
        let keys = ui.input(|i| {
            let zoom_in = i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals);
            match (typing, zoom_in, i.key_pressed(Key::Minus)) {
                (false, true, _) => 1.5,
                (false, _, true) => 1.0 / 1.5,
                _ => 1.0,
            }
        });
        let anchor = match keys {
            1.0 => resp.hover_pos(),
            _ => Some(rect.center()),
        };
        if let Some(m) = anchor {
            let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta().y, i.zoom_delta()));
            let factor = keys * pinch * (scroll * 0.0015).exp();
            if factor != 1.0 {
                let before = origin(self.zoom, self.pan);
                let zoom = (self.zoom * factor).clamp(1.0, max_zoom);
                let k = zoom / self.zoom;
                // Keep the point under the cursor fixed.
                let want = m - (m - before) * k;
                self.pan = want - origin(zoom, Vec2::ZERO);
                self.zoom = zoom;
            }
        }

        let o = origin(self.zoom, self.pan);
        let size = fit * self.zoom;
        let px = size / grid.side as f32;
        let map_rect = Rect::from_min_size(o, Vec2::splat(size));

        // Past the scan's own detail, refine what is on screen from exact runs.
        let visible = map_rect.intersect(rect);
        let deep_view = (px * ppp > 1.5 && visible.is_positive()).then(|| {
            let scale = (visible.width().max(visible.height()) * ppp / DEEP_MAX_SIDE).max(1.0);
            Viewport {
                u0: ((visible.min.x - o.x) / size) as f64,
                v0: ((visible.min.y - o.y) / size) as f64,
                u1: ((visible.max.x - o.x) / size) as f64,
                v1: ((visible.max.y - o.y) / size) as f64,
                w: ((visible.width() * ppp / scale) as u32).max(1),
                h: ((visible.height() * ppp / scale) as u32).max(1),
            }
        });
        self.send_view(deep_view);
        let deep = self.deep.clone().filter(|_| deep_view.is_some());
        let deep_at = |p: Pos2| {
            let deep = deep.as_ref()?;
            let v = deep.view;
            let u = ((p.x - o.x) / size) as f64;
            let w = ((p.y - o.y) / size) as f64;
            let inside = u >= v.u0 && u < v.u1 && w >= v.v0 && w < v.v1;
            let i = ((u - v.u0) / (v.u1 - v.u0) * v.w as f64) as usize;
            let j = ((w - v.v0) / (v.v1 - v.v0) * v.h as f64) as usize;
            let set = *deep
                .sets
                .get(j.min(v.h as usize - 1) * v.w as usize + i.min(v.w as usize - 1))?;
            (inside && set != UNSET).then_some(set)
        };

        let hovered = resp.hover_pos().and_then(|p| {
            let (x, y) = (((p.x - o.x) / px).floor(), ((p.y - o.y) / px).floor());
            let inside = x >= 0.0 && y >= 0.0 && x < grid.side as f32 && y < grid.side as f32;
            inside.then_some((x as u32, y as u32))
        });
        self.hover_cell = hovered.map(|(x, y)| grid.cell_at[(y * grid.side + x) as usize]);
        if resp.hovered() {
            // What the deep view shows there, else the scan's sample.
            let set = resp
                .hover_pos()
                .and_then(deep_at)
                .or_else(|| self.model.set.get(self.hover_cell? as usize).copied());
            self.hover_set = set.filter(|&s| {
                let info = self.model.sets.get(s as usize);
                info.is_some_and(|i| i.kind != Kind::Past)
            });
        }
        if resp.clicked() && !menu_open {
            self.pinned = if self.pinned == self.hover_set {
                None
            } else {
                self.hover_set
            };
        }

        painter.image(
            texture_id,
            map_rect,
            Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        if let (Some(d), Some(t)) = (&deep, &self.deep_texture) {
            let v = d.view;
            let at = |u: f64, w: f64| o + vec2(u as f32 * size, w as f32 * size);
            let deep_rect = Rect::from_min_max(at(v.u0, v.v0), at(v.u1, v.v1));
            // Dimming fades pixels out, so the sampled map must not be behind them.
            painter.rect_filled(deep_rect, 0.0, ui.visuals().panel_fill);
            painter.image(
                t.id(),
                deep_rect,
                Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        if let (Some((x, y)), None) = (hovered, &deep) {
            let cell =
                Rect::from_min_size(o + vec2(x as f32, y as f32) * px, Vec2::splat(px.max(2.0)));
            painter.rect_stroke(
                cell,
                0,
                Stroke::new(1.5, ui.visuals().strong_text_color()),
                StrokeKind::Outside,
            );
        }
        if resp.secondary_clicked() {
            self.menu_target = self.hover_set.map(MenuTarget::Set);
        }
        resp.context_menu(|ui| self.context_menu(ui));
    }
}

fn set_swatch(
    model: &Model,
    set: &SetInfo,
    stats: &Stats,
    palette: &Palette,
    mode: Mode,
) -> Color32 {
    let color = match mode {
        Mode::Owners => palette.owner(set),
        Mode::Sharing => palette.sharing(set),
        Mode::Compression if matches!(set.kind, Kind::Data | Kind::Unreachable) => {
            let (packed, holds) = (stats.packed(set.id), stats.holds(set.id));
            Some(match packed > 0.0 {
                true => palette.ratio[bucket(&RATIO_BUCKETS, (100.0 * holds / packed) as u32)],
                false => palette.neutral,
            })
        }
        Mode::Compression => palette.kind(set.kind),
        Mode::Age => palette.age(set, age_of(model, stats.newest(set.id))),
    };
    color.unwrap_or(Color32::TRANSPARENT)
}

/// A flex row with its items centered across it, Clay-style.
fn row(gap: f32) -> taffy::Style {
    taffy::Style {
        flex_direction: taffy::FlexDirection::Row,
        align_items: Some(taffy::AlignItems::Center),
        gap: length(gap),
        ..Default::default()
    }
}

/// A full-width flex column whose items stretch to its width.
fn column(gap: f32) -> taffy::Style {
    taffy::Style {
        flex_direction: taffy::FlexDirection::Column,
        align_items: Some(taffy::AlignItems::Stretch),
        gap: length(gap),
        size: taffy::Size {
            width: percent(1.0_f32),
            height: taffy::Dimension::auto(),
        },
        ..Default::default()
    }
}

/// Where a path from the top-level subvolume ("@/home/a") is reachable here: under the
/// mount whose subvolume is the longest prefix of it. None when nothing mounts it.
pub fn real_path(mounts: &[Mount], path: &str) -> Option<PathBuf> {
    let full = format!("/{}", path.trim_start_matches('/'));
    mounts
        .iter()
        .filter_map(|m| {
            let sub = m.subvolume.trim_end_matches('/');
            let rest = match sub {
                "" => full.as_str(),
                s if full == s => "",
                s => full.strip_prefix(s).filter(|r| r.starts_with('/'))?,
            };
            Some((sub.len(), m.path.join(rest.trim_start_matches('/'))))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, p)| p)
}

/// A scan's detail: the smallest curve with a cell for every physical pixel of the map
/// (drawn at 94% of its shorter side), from 512² to 2048². Zooming past it is what full
/// resolution is for.
fn auto_order(map_px: f32) -> u32 {
    (9..=11)
        .find(|&o| (1u32 << o) as f32 >= map_px * 0.94)
        .unwrap_or(11)
}

/// The filesystem to preselect: the only one, else the one mounted at /.
fn default_selection(filesystems: &[Filesystem]) -> Option<PathBuf> {
    let root = std::path::Path::new("/");
    match filesystems {
        [only] => only.scan_target(),
        _ => filesystems
            .iter()
            .find(|fs| fs.mounts.iter().any(|m| m.path == root))
            .and_then(Filesystem::scan_target),
    }
}

/// A short name for a set's tile: the last component of its first path, or its kind.
fn tile_name(set: &SetInfo) -> String {
    match set.paths.first() {
        Some(p) if set.kind == Kind::Unreachable => {
            format!("{} (unreachable)", p.rsplit('/').next().unwrap_or(p))
        }
        Some(p) => p.rsplit('/').next().unwrap_or(p).to_string(),
        None => kind_label(set.kind).to_string(),
    }
}

/// How many characters to keep from the front and back of a string whose characters are
/// `widths` wide so that, with an ellipsis of `ellipsis` between them, it fits `width`.
/// None when it fits whole. Takes from both ends in turn, the end first: in a path that
/// is where the file name is.
fn cut_middle(widths: &[f32], ellipsis: f32, width: f32) -> Option<(usize, usize)> {
    if widths.iter().sum::<f32>() <= width {
        return None;
    }
    let (mut front, mut back, mut used) = (0, 0, ellipsis);
    while front + back < widths.len() {
        let from_back = back <= front;
        let i = if from_back {
            widths.len() - 1 - back
        } else {
            front
        };
        if used + widths[i] > width {
            break;
        }
        used += widths[i];
        if from_back {
            back += 1;
        } else {
            front += 1;
        }
    }
    Some((front, back))
}

/// `text` fitted into `width` points by replacing its middle with "…".
fn ellipsize_middle(ctx: &egui::Context, text: &str, font: &egui::FontId, width: f32) -> String {
    let chars: Vec<char> = text.chars().collect();
    let (widths, ellipsis) = ctx.fonts_mut(|f| {
        let widths: Vec<f32> = chars.iter().map(|&c| f.glyph_width(font, c)).collect();
        (widths, f.glyph_width(font, '…'))
    });
    match cut_middle(&widths, ellipsis, width) {
        None => text.to_string(),
        Some((front, back)) => {
            let head: String = chars[..front].iter().collect();
            let tail: String = chars[chars.len() - back..].iter().collect();
            format!("{head}…{tail}")
        }
    }
}

/// The side pane's one-line summary of a scan.
fn summary_of(model: &Model, stats: &Stats) -> String {
    let Some(h) = &model.header else {
        return String::new();
    };
    let side = 1u64 << h.order;
    let shared: u64 = model
        .sets
        .iter()
        .filter(|s| s.kind == Kind::Data && s.files > 1)
        .map(|s| stats.area(s.id) * h.cell)
        .sum();
    let file_sets = model.sets.iter().filter(|s| s.kind == Kind::Data).count();
    format!(
        "{} allocated · {side}×{side} cells of {} · {file_sets} file sets · {} shared",
        bytes(h.total),
        bytes(h.cell),
        bytes(shared)
    )
}

/// What the current mode's colors mean.
fn note_of(stats: &Stats, mode: Mode) -> String {
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

/// How a filesystem is named in the menu: its label, else its device.
fn fs_name(fs: &Filesystem) -> String {
    fs.label.clone().unwrap_or_else(|| fs.source.clone())
}

fn swatch(ui: &mut egui::Ui, color: Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::same(2), color);
}

/// What one exact run says about its bytes.
#[derive(Clone, Copy, Debug, PartialEq)]
struct RunInfo {
    end: u64,
    set: u32,
    algo: Algo,
    ratio: u32,
    generation: u64,
}

/// Exact knowledge from probe answers: non-overlapping runs along the curve, by start.
#[derive(Default)]
struct RunMap {
    runs: std::collections::BTreeMap<u64, RunInfo>,
}

impl RunMap {
    /// Record a run; anything it overlaps is trimmed or replaced, newest wins.
    fn insert(&mut self, run: &Run) {
        let (start, end) = (run.0, run.0 + run.1);
        if end <= start {
            return;
        }
        // A run starting before `start` that reaches into it keeps only its head, and
        // its tail past `end` if it had one.
        let before = self.runs.range(..start).next_back().map(|(&s, &r)| (s, r));
        if let Some((s, r)) = before.filter(|(_, r)| r.end > start) {
            self.runs.insert(s, RunInfo { end: start, ..r });
            if r.end > end {
                self.runs.insert(end, r);
            }
        }
        let inside: Vec<(u64, RunInfo)> =
            self.runs.range(start..end).map(|(&s, &r)| (s, r)).collect();
        for (s, r) in inside {
            self.runs.remove(&s);
            if r.end > end {
                self.runs.insert(end, r);
            }
        }
        let info = RunInfo {
            end,
            set: run.2,
            algo: run.3,
            ratio: run.4,
            generation: run.5,
        };
        self.runs.insert(start, info);
    }

    fn at(&self, pos: u64) -> Option<&RunInfo> {
        self.runs
            .range(..=pos)
            .next_back()
            .map(|(_, r)| r)
            .filter(|r| pos < r.end)
    }
}

/// The part of the map the window shows, in map units (0..1 across the square), and
/// how many physical pixels it spans.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Viewport {
    u0: f64,
    v0: f64,
    u1: f64,
    v1: f64,
    w: u32,
    h: u32,
}

/// The visible part of the map at one curve cell per physical pixel, from exact runs.
/// Unprobed pixels take the nearest coarser probe's answer, else the scan's sample.
struct Deep {
    view: Viewport,
    pixels: Vec<Color32>,
    /// Each pixel's set as shown (probe, nearest probe or sample), UNSET where none.
    sets: Vec<u32>,
    /// Pixels still unknown.
    unknown: usize,
}

/// The block-level curve for a filesystem: the smallest order whose cells are no bigger
/// than a sector, and each cell's size in bytes.
fn deep_order(total: u64) -> (u32, u64) {
    let order = (0..=20)
        .find(|&m| total.div_ceil(1u64 << (2 * m)) <= 4096)
        .unwrap_or(20);
    (order, total.div_ceil(1u64 << (2 * order)).max(1))
}

/// Color the viewport from exact runs, and list where to probe next: unknown pixels'
/// positions, coarse to fine (every 16th pixel first, then 8th, …), like progressive
/// ray tracing, so the whole view sharpens together.
fn render_deep(
    runs: &RunMap,
    model: &Model,
    palette: &Palette,
    mode: Mode,
    total: u64,
    view: Viewport,
) -> (Deep, Vec<u64>) {
    use rayon::prelude::*;
    let (order, cell) = deep_order(total);
    let side = 1u64 << order;
    let (w, h) = (view.w as usize, view.h as usize);
    let cal = model
        .header
        .as_ref()
        .map_or(&[][..], |h| &h.calibration[..]);
    let now = cal.last().map_or(0, |p| p.1);
    let pos_at = |i: usize, j: usize| {
        let u = view.u0 + (i as f64 + 0.5) / w as f64 * (view.u1 - view.u0);
        let v = view.v0 + (j as f64 + 0.5) / h as f64 * (view.v1 - view.v0);
        let x = ((u * side as f64) as u64).min(side - 1);
        let y = ((v * side as f64) as u64).min(side - 1);
        xy2d(side, x, y) * cell + cell / 2
    };
    let color = |r: &RunInfo| {
        let Some(set) = model.sets.get(r.set as usize) else {
            return Color32::TRANSPARENT;
        };
        let c = match mode {
            Mode::Owners => palette.owner(set),
            Mode::Sharing => palette.sharing(set),
            Mode::Compression => palette.compression(set, r.algo, r.ratio),
            Mode::Age => {
                let age = date_of(cal, r.generation).map(|t| (now - t).max(0));
                palette.age(set, age)
            }
        };
        c.unwrap_or(Color32::TRANSPARENT)
    };
    let found: Vec<(Color32, u32)> = (0..w * h)
        .into_par_iter()
        .map(|p| {
            let pos = pos_at(p % w, p / w);
            match (pos < total).then(|| runs.at(pos)).flatten() {
                Some(r) => (color(r), r.set),
                None => (Color32::TRANSPARENT, UNSET),
            }
        })
        .collect();
    let unknown = found.iter().filter(|f| f.1 == UNSET).count();
    let mut next = Vec::new();
    let mut seen = std::collections::HashSet::new();
    'queue: for stride in [16, 8, 4, 2, 1] {
        for j in (0..h).step_by(stride) {
            for i in (0..w).step_by(stride) {
                let coarser = stride < 16 && i % (stride * 2) == 0 && j % (stride * 2) == 0;
                if coarser || found[j * w + i].1 != UNSET {
                    continue;
                }
                let pos = pos_at(i, j);
                if pos < total && seen.insert(pos / cell) {
                    next.push(pos);
                }
                if next.len() >= DEEP_QUEUE {
                    break 'queue;
                }
            }
        }
    }
    // Every pixel shows the nearest answer known: its own probe, a coarser probe's,
    // else the scan's sample of that spot. One image that only ever sharpens.
    let sample_cell = model.header.as_ref().map_or(0, |h| h.cell);
    let sample = |pos: u64| {
        let d = (pos / sample_cell.max(1)) as usize;
        let set = *model.set.get(d).filter(|&&s| s != UNSET && pos < total)?;
        let r = RunInfo {
            end: 0,
            set,
            algo: model.algo[d],
            ratio: model.ratio[d],
            generation: model.generation[d],
        };
        Some((color(&r), set))
    };
    let filled: Vec<(Color32, u32)> = (0..w * h)
        .into_par_iter()
        .map(|p| {
            let (i, j) = (p % w, p / w);
            let probed = [1, 2, 4, 8, 16]
                .into_iter()
                .map(|s| found[(j - j % s) * w + i - i % s])
                .find(|f| f.1 != UNSET);
            let pos = pos_at(i, j);
            probed
                .or_else(|| sample(pos))
                .unwrap_or((Color32::TRANSPARENT, UNSET))
        })
        .collect();
    let (pixels, sets) = filled.into_iter().unzip();
    let deep = Deep {
        view,
        pixels,
        sets,
        unknown,
    };
    (deep, next)
}

/// What the presentation needs derived state computed for.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Look {
    mode: Mode,
    dark: bool,
}

/// Input to the model thread: the scan's stream, or a change in what is shown.
enum Input {
    Event(Event),
    Look(Look),
    /// The part of the curve on screen, when the deep view should refine it.
    View(Option<Viewport>),
}

/// Everything the window draws from, computed on the model thread and handed over
/// whole. The window only ever reads it, so drawing never waits on computation.
struct Snapshot {
    model: Arc<Model>,
    stats: Arc<Stats>,
    list: (&'static str, Vec<(u32, String)>),
    cats: Vec<Category>,
    summary: String,
    note: String,
    grid: Option<Arc<Grid>>,
    /// Map colors before dimming, and each pixel's set, in raster order.
    base: Arc<Vec<Color32>>,
    raster_set: Arc<Vec<u32>>,
    tree: Option<Arc<treemap::Tree>>,
    tree_gen: u64,
    /// The visible part at block resolution, as far as probes have filled it in.
    deep: Option<Arc<Deep>>,
    status: String,
    ended: Option<Result<(), String>>,
    error: Option<String>,
}

/// The model thread's state: the scan data, and what was last derived from it.
struct Worker {
    model: Model,
    /// The model as last handed out; copied only when the data changes.
    shared: Arc<Model>,
    look: Look,
    status: String,
    ended: Option<Result<(), String>>,
    error: Option<String>,
    grid: Option<Arc<Grid>>,
    stats: Arc<Stats>,
    raster_set: Arc<Vec<u32>>,
    base: Arc<Vec<Color32>>,
    list: (&'static str, Vec<(u32, String)>),
    cats: Vec<Category>,
    summary: String,
    note: String,
    tree: Option<Arc<treemap::Tree>>,
    /// The (level, done) the tree was built at; rebuilt when a newer level completes,
    /// since in between sizes drift and squarified layout would reshuffle.
    tree_at: Option<(u32, bool)>,
    tree_gen: u64,
    /// Exact answers so far, the view being refined, and its latest rendering.
    runs: RunMap,
    view: Option<Viewport>,
    deep: Option<Arc<Deep>>,
    /// Where the helper takes probe requests, what was asked for this view, and how
    /// many batches are unanswered.
    prober: Option<std::process::ChildStdin>,
    requested: std::collections::HashSet<u64>,
    in_flight: usize,
    data_changed: bool,
    look_changed: bool,
    deep_changed: bool,
    news: bool,
    last_sent: Instant,
    last_deep: Instant,
}

impl Worker {
    fn new(look: Look, prober: Option<std::process::ChildStdin>) -> Self {
        Worker {
            model: Model::default(),
            shared: Arc::new(Model::default()),
            look,
            status: String::new(),
            ended: None,
            error: None,
            grid: None,
            stats: Arc::new(Stats::default()),
            raster_set: Arc::new(Vec::new()),
            base: Arc::new(Vec::new()),
            list: ("", Vec::new()),
            cats: Vec::new(),
            summary: String::new(),
            note: String::new(),
            tree: None,
            tree_at: None,
            tree_gen: 0,
            runs: RunMap::default(),
            view: None,
            deep: None,
            prober,
            requested: std::collections::HashSet::new(),
            in_flight: 0,
            data_changed: false,
            look_changed: true,
            deep_changed: false,
            news: false,
            last_sent: Instant::now() - REPAINT_EVERY,
            last_deep: Instant::now() - DEEP_EVERY,
        }
    }

    fn take(&mut self, input: Input) {
        match input {
            Input::Look(look) => {
                self.look_changed |= look != self.look;
                self.deep_changed |= look != self.look;
                self.look = look;
            }
            Input::View(view) => {
                if view != self.view {
                    self.view = view;
                    self.requested.clear();
                    self.deep_changed = true;
                }
            }
            Input::Event(Event::Status(s)) => {
                self.status = s;
                self.news = true;
            }
            Input::Event(Event::Ended(r)) => {
                self.ended = Some(r);
                self.news = true;
            }
            Input::Event(Event::Msg(Err(e))) => {
                self.error = Some(e);
                self.news = true;
            }
            Input::Event(Event::Msg(Ok(msg @ Msg::Runs { .. }))) => {
                match (self.model.check(&msg), &msg) {
                    (Ok(()), Msg::Runs { runs }) => runs.iter().for_each(|r| self.runs.insert(r)),
                    (Err(e), _) => self.error = Some(e.to_string()),
                    _ => {}
                }
                self.in_flight = self.in_flight.saturating_sub(1);
                self.deep_changed = true;
            }
            Input::Event(Event::Msg(Ok(msg))) => {
                let model = std::mem::take(&mut self.model);
                self.model = match model.apply(msg) {
                    Ok(m) => m,
                    Err(rejected) => {
                        let (m, e) = *rejected;
                        self.error = Some(e.to_string());
                        m
                    }
                };
                if self.grid.is_none() {
                    self.grid = self
                        .model
                        .header
                        .as_ref()
                        .map(|h| Arc::new(Grid::new(h.order)));
                }
                self.data_changed = true;
            }
        }
    }

    /// Whether a snapshot is worth sending now: at once for a new look or the end of a
    /// scan, the deep view at most every DEEP_EVERY, and streaming data at most every
    /// REPAINT_EVERY.
    fn due(&self) -> bool {
        let pending = self.data_changed || self.news;
        let settled = self.model.done || self.ended.is_some();
        self.look_changed
            || (pending && (settled || self.last_sent.elapsed() >= REPAINT_EVERY))
            || (self.deep_changed && self.last_deep.elapsed() >= DEEP_EVERY)
    }

    fn pending(&self) -> bool {
        self.data_changed || self.news || self.look_changed || self.deep_changed
    }

    /// How long until something pending becomes due.
    fn wait(&self) -> Duration {
        let data = REPAINT_EVERY.saturating_sub(self.last_sent.elapsed());
        match self.deep_changed {
            true => data.min(DEEP_EVERY.saturating_sub(self.last_deep.elapsed())),
            false => data,
        }
    }

    /// Re-render the deep view, and ask the helper about the next unknown positions.
    fn refine(&mut self) {
        self.deep_changed = false;
        self.last_deep = Instant::now();
        let (Some(view), Some(h)) = (self.view, &self.model.header) else {
            self.deep = None;
            return;
        };
        let palette = Palette::new(self.look.dark);
        let (deep, next) = render_deep(
            &self.runs,
            &self.model,
            &palette,
            self.look.mode,
            h.total,
            view,
        );
        self.deep = Some(Arc::new(deep));
        let Some(prober) = &mut self.prober else {
            return;
        };
        let next: Vec<u64> = next
            .into_iter()
            .filter(|p| !self.requested.contains(p))
            .collect();
        let mut next = next.into_iter();
        while self.in_flight < DEEP_IN_FLIGHT {
            let positions: Vec<u64> = next.by_ref().take(DEEP_BATCH).collect();
            if positions.is_empty() {
                break;
            }
            let request = serde_json::to_string(&Request::Probe {
                positions: positions.clone(),
            });
            let sent = request.map_err(std::io::Error::other).and_then(|line| {
                use std::io::Write as _;
                writeln!(prober, "{line}").and_then(|()| prober.flush())
            });
            if sent.is_err() {
                // The helper is gone (the scan was stopped); nothing more to refine.
                self.prober = None;
                return;
            }
            self.requested.extend(positions);
            self.in_flight += 1;
        }
    }

    fn snapshot(&mut self) -> Snapshot {
        if self.data_changed {
            self.stats = Arc::new(stats(&self.model));
            if let Some(grid) = &self.grid {
                let sets = grid
                    .cell_at
                    .iter()
                    .map(|&d| self.model.set[d as usize])
                    .collect();
                self.raster_set = Arc::new(sets);
            }
            let complete =
                self.model.done || self.model.level_cells >= 1usize << (2 * self.model.level);
            let at = (self.model.level, self.model.done);
            if let (true, false, Some(h)) = (complete, self.tree_at == Some(at), &self.model.header)
            {
                let bytes: Vec<u64> = self.stats.area.iter().map(|a| a * h.cell).collect();
                self.tree = Some(Arc::new(treemap::build(
                    &h.source,
                    &self.model.sets,
                    &bytes,
                )));
                self.tree_at = Some(at);
                self.tree_gen += 1;
            }
            self.shared = Arc::new(self.model.clone());
        }
        if self.data_changed || self.look_changed {
            let palette = Palette::new(self.look.dark);
            self.list = ranked(&self.model, &self.stats, self.look.mode);
            self.cats = categories(&self.model, &self.stats, &palette, self.look.mode);
            self.summary = summary_of(&self.model, &self.stats);
            self.note = note_of(&self.stats, self.look.mode);
            if let Some(grid) = &self.grid {
                let colors = cell_colors(&self.model, &palette, self.look.mode, &grid.cell_at);
                self.base = Arc::new(colors);
            }
        }
        if self.deep_changed && self.last_deep.elapsed() >= DEEP_EVERY {
            self.refine();
        }
        let data_moved = self.data_changed || self.look_changed || self.news;
        self.data_changed = false;
        self.look_changed = false;
        self.news = false;
        if data_moved {
            self.last_sent = Instant::now();
        }
        Snapshot {
            model: self.shared.clone(),
            stats: self.stats.clone(),
            list: self.list.clone(),
            cats: self.cats.clone(),
            summary: self.summary.clone(),
            note: self.note.clone(),
            grid: self.grid.clone(),
            base: self.base.clone(),
            raster_set: self.raster_set.clone(),
            tree: self.tree.clone(),
            tree_gen: self.tree_gen,
            deep: self.deep.clone(),
            status: self.status.clone(),
            ended: self.ended.clone(),
            error: self.error.clone(),
        }
    }
}

/// The model thread: apply what arrives, and send the window a fresh snapshot when one
/// is due. It stops when the window drops its end, which also stops the readers and so
/// the scan.
fn model_thread(
    inputs: Receiver<Input>,
    out: Sender<Snapshot>,
    look: Look,
    prober: Option<std::process::ChildStdin>,
    ctx: egui::Context,
) {
    use std::sync::mpsc::RecvTimeoutError;
    let mut worker = Worker::new(look, prober);
    loop {
        let first = match worker.pending() {
            true => match inputs.recv_timeout(worker.wait()) {
                Ok(input) => Some(input),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            false => match inputs.recv() {
                Ok(input) => Some(input),
                Err(_) => return,
            },
        };
        for input in first.into_iter().chain(inputs.try_iter()) {
            worker.take(input);
        }
        if worker.due() {
            if out.send(worker.snapshot()).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.receive();
        let dark = ctx.theme() == egui::Theme::Dark;
        let palette = Palette::new(dark);
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        // A fixed height: a long hovered chain must not resize, and so re-lay out, the map.
        egui::Panel::bottom("status")
            .resizable(false)
            .exact_size(26.0)
            .show(ui, |ui| self.status_bar(ui));
        // A changed mode or theme goes to the model thread; its next snapshot
        // brings the new colors, legend and list.
        self.send_look(&ctx);
        self.password_prompt(ui, &palette);
        self.delete_prompt(ui, &palette);
        if self.scan.is_none() {
            egui::CentralPanel::default().show(ui, |ui| self.welcome(ui, &palette));
            return;
        }
        if ui.input(|i| i.key_pressed(Key::Escape)) {
            self.pinned = None;
        }
        egui::Panel::right("side")
            .resizable(true)
            .default_size(380.0)
            .min_size(260.0)
            .max_size(1200.0)
            .show(ui, |ui| self.side_panel(ui, &palette));
        match self.view {
            View::Curve => egui::CentralPanel::default().show(ui, |ui| self.map(ui)),
            View::Treemap => {
                // Nothing to refine while the curve is off screen.
                self.send_view(None);
                egui::CentralPanel::default().show(ui, |ui| self.treemap(ui, &palette))
            }
        };
        self.repaint(&ctx);
        self.repaint_deep(&ctx);
    }
}

/// Whether sudo will run a command without asking (a password was given recently).
fn sudo_cached() -> bool {
    std::process::Command::new(SUDO)
        .args(["-n", "true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Run the scan as root through sudo, reading its stream and progress on threads. With a
/// password, sudo reads it from stdin; without one, sudo must not ask.
///
/// Returns the model thread's input (for view changes) and its snapshots. Dropping both
/// stops everything: the model thread quits, the readers stop, the pipe closes, and the
/// scan exits on its next write. (It runs as root, so it can't be signalled.)
fn start(
    scan: &Scan,
    password: Option<String>,
    look: Look,
    ctx: &egui::Context,
) -> (Sender<Input>, Receiver<Snapshot>) {
    use std::io::Write as _;
    let (tx, inputs) = channel();
    let (snapshots_tx, snapshots) = channel();
    let model_ctx = ctx.clone();
    let run_model = move |prober| {
        std::thread::spawn(move || model_thread(inputs, snapshots_tx, look, prober, model_ctx));
    };
    let auth: &[&str] = match password {
        Some(_) => &["-S", "-p", ""],
        None => &["-n"],
    };
    let spawned = std::env::current_exe().and_then(|exe| {
        std::process::Command::new(SUDO)
            .args(auth)
            .arg("--")
            .arg(exe)
            .args(["scan", "--order", &scan.order.to_string()])
            .arg(&scan.target)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
    });
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Input::Event(Event::Ended(Err(format!("run {SUDO}: {e}")))));
            run_model(None);
            return (tx, snapshots);
        }
    };
    if let (Some(password), Some(mut stdin)) = (password, child.stdin.take()) {
        let mut line = password.into_bytes();
        line.push(b'\n');
        let _ = stdin.write_all(&line);
        line.fill(0);
        child.stdin = Some(stdin);
    }
    // After the password, the helper's stdin carries the deep view's probe requests.
    run_model(child.stdin.take());
    let (stdout, mut stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());

    // The readers only feed the model thread; the window wakes when a snapshot is ready.
    let status_tx = tx.clone();
    std::thread::spawn(move || {
        // Progress lines end in \r; show the latest one.
        let mut buf = [0u8; 4096];
        let mut line = Vec::new();
        while let Ok(n @ 1..) = std::io::Read::read(&mut stderr, &mut buf) {
            for &b in &buf[..n] {
                if b != b'\r' && b != b'\n' {
                    line.push(b);
                    continue;
                }
                let text = String::from_utf8_lossy(&line).trim().to_string();
                line.clear();
                if text.is_empty() {
                    continue;
                }
                if status_tx.send(Input::Event(Event::Status(text))).is_err() {
                    return;
                }
            }
        }
    });

    let reader_tx = tx.clone();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let msg = line
                .map_err(|e| format!("read: {e}"))
                .and_then(|l| serde_json::from_str::<Msg>(&l).map_err(|e| format!("parse: {e}")));
            if reader_tx.send(Input::Event(Event::Msg(msg))).is_err() {
                return;
            }
        }
        let ended = match child.wait().map(|s| s.code()) {
            Ok(Some(0)) => Ok(()),
            // sudo explains a wrong password on stderr, which is shown as the status line.
            Ok(code) => Err(format!("The scan stopped (exit {}).", code.unwrap_or(-1))),
            Err(e) => Err(format!("wait for scan: {e}")),
        };
        let _ = reader_tx.send(Input::Event(Event::Ended(ended)));
    });
    (tx, snapshots)
}

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("btrmaps")
            .with_app_id("btrmaps")
            .with_inner_size([1400.0, 900.0]),
        ..Default::default()
    };
    eframe::run_native("btrmaps", options, Box::new(|_| Ok(Box::new(App::new()))))
        .map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Cell;

    #[test]
    fn hilbert_visits_every_pixel_once_with_unit_steps() {
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
        let side = 64u32;
        for d in 0..side * side {
            let (x, y) = d2xy(side, d);
            assert_eq!(xy2d(side as u64, x as u64, y as u64), d as u64);
        }
    }

    #[test]
    fn finer_curves_trace_coarser_ones() {
        // Refining the deep view must land on the coarse cells it refines: order-n
        // cell d, seen from order n+k, is where fine cells d·4^k.. are.
        for (n, k) in [(3, 1), (3, 2), (4, 3)] {
            let (coarse, fine) = (1u32 << n, 1u32 << (n + k));
            for d in 0..coarse * coarse {
                let (x, y) = d2xy(fine, d << (2 * k));
                assert_eq!((x >> k, y >> k), d2xy(coarse, d), "n={n} k={k} d={d}");
            }
        }
    }

    #[test]
    fn hilbert_runs_fill_aligned_squares_at_every_scale() {
        // What coarse-to-fine drawing relies on, and why nearby bytes stay nearby: any
        // aligned run of 4^m indices is exactly one aligned 2^m square.
        let side = 64;
        for m in 0..=6 {
            let run = 1u32 << (2 * m);
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

    fn set(id: u32, kind: Kind) -> Msg {
        Msg::Set(SetInfo {
            id,
            kind,
            paths: vec![],
            path_count: 0,
            files: 0,
            subvolumes: 0,
            truncated: false,
            error: String::new(),
            dominator: String::new(),
        })
    }

    #[test]
    fn finer_levels_overwrite_the_cells_they_cover() {
        let header = Msg::Header(Header {
            source: "/".into(),
            order: 2,
            total: 16,
            cell: 1,
            calibration: vec![],
        });
        let coarse = Msg::Cells {
            level: 1,
            cells: vec![
                Cell(0, 0, Algo::None, 100, 0),
                Cell(3, 1, Algo::Zstd, 400, 0),
            ],
        };
        let fine = Msg::Cells {
            level: 2,
            cells: vec![Cell(13, 0, Algo::None, 100, 0)],
        };
        let model = [header, set(0, Kind::Free), set(1, Kind::Data), coarse, fine]
            .into_iter()
            .try_fold(Model::default(), |m, msg| m.apply(msg).map_err(|r| r.1))
            .unwrap();
        let u = UNSET;
        assert_eq!(model.set, [0, 0, 0, 0, u, u, u, u, u, u, u, u, 1, 0, 1, 1]);
        assert_eq!(model.algo[12], Algo::Zstd);
        assert_eq!((model.level, model.level_cells), (2, 1));
    }

    #[test]
    fn generations_date_by_interpolating_recorded_times() {
        let cal = [(100, 1_000_000), (200, 2_000_000), (300, 2_000_000)];
        assert_eq!(date_of(&cal, 150), Some(1_500_000));
        assert_eq!(
            date_of(&cal, 50),
            Some(1_000_000),
            "before the first point clamps"
        );
        assert_eq!(
            date_of(&cal, 900),
            Some(2_000_000),
            "after the last point clamps"
        );
        assert_eq!(date_of(&cal, 0), None, "generation 0 is unknown");
        assert_eq!(date_of(&[], 150), None);
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(1_790_000_000), "2026-09-21");
    }

    #[test]
    fn sets_newer_than_the_totals_count_as_empty() {
        let s = Stats {
            area: vec![3],
            packed: vec![1.0],
            holds: vec![2.0],
            newest: vec![7],
        };
        assert_eq!((s.area(0), s.newest(0)), (3, 7));
        assert_eq!(
            (s.area(144), s.packed(144), s.holds(144), s.newest(144)),
            (0, 0.0, 0.0, 0)
        );
    }

    #[test]
    fn toolbar_overflows_least_important_first_without_flicker() {
        let bar = BarWidths {
            left: 300.0,
            widths: [150.0, 300.0, 120.0, 90.0],
        };
        let all = overflow_count(10_000.0, &bar, 0);
        assert_eq!(all, 0, "everything fits a wide window");
        let narrow = overflow_count(900.0, &bar, 0);
        assert!(narrow > 0 && narrow < OVERFLOW_ORDER.len(), "{narrow}");
        assert_eq!(overflow_count(10.0, &bar, 0), OVERFLOW_ORDER.len());
        // Just wide enough to show one more: stays hidden until there is room to spare.
        let edge = (0..4000)
            .map(|w| w as f32)
            .find(|&w| overflow_count(w, &bar, narrow) < narrow)
            .unwrap();
        assert!(overflow_count(edge - 1.0, &bar, narrow) == narrow);
        assert!(
            overflow_count(edge - 20.0, &bar, narrow - 1) == narrow - 1,
            "no flicker back"
        );
    }

    #[test]
    fn newer_runs_trim_what_they_overlap() {
        let run = |start, len, set| Run(start, len, set, Algo::None, 100, 1);
        let mut m = RunMap::default();
        m.insert(&run(0, 100, 1));
        m.insert(&run(40, 20, 2));
        let at = |m: &RunMap, p| m.at(p).map(|r| r.set);
        assert_eq!(
            (at(&m, 39), at(&m, 40), at(&m, 59), at(&m, 60), at(&m, 99)),
            (Some(1), Some(2), Some(2), Some(1), Some(1))
        );
        assert_eq!(at(&m, 100), None);
        m.insert(&run(30, 50, 3));
        assert_eq!(
            (at(&m, 29), at(&m, 30), at(&m, 79), at(&m, 80)),
            (Some(1), Some(3), Some(3), Some(1))
        );
        assert_eq!(m.runs.len(), 3, "no leftovers of the swallowed run");
    }

    #[test]
    fn deep_view_fills_from_runs_and_asks_for_the_rest_coarse_first() {
        let total = 4096 * 4096;
        let header = Msg::Header(Header {
            source: "/".into(),
            order: 2,
            total,
            cell: total >> 4,
            calibration: vec![],
        });
        // The scan sampled set 1 everywhere.
        let cells = Msg::Cells {
            level: 2,
            cells: (0..16).map(|d| Cell(d, 1, Algo::None, 100, 1)).collect(),
        };
        let model = [header, set(0, Kind::Data), set(1, Kind::Data), cells]
            .into_iter()
            .try_fold(Model::default(), |m, msg| m.apply(msg).map_err(|r| r.1))
            .unwrap();
        // The first half of the curve is known; the second half is not.
        let mut runs = RunMap::default();
        runs.insert(&Run(0, total / 2, 0, Algo::None, 100, 1));
        let view = Viewport {
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            w: 64,
            h: 64,
        };
        let (deep, next) = render_deep(
            &runs,
            &model,
            &Palette::new(true),
            Mode::Owners,
            total,
            view,
        );
        assert_eq!(deep.unknown, 64 * 64 / 2, "half the picture is known");
        // One picture: runs where probed, the scan's sample elsewhere, both with their
        // set, so a highlight dims them alike.
        let count = |s| deep.sets.iter().filter(|&&x| x == s).count();
        assert_eq!(count(UNSET), 0);
        assert!(count(0) >= 64 * 64 / 2 && count(1) > 0);
        assert!(
            next.iter().all(|&p| p >= total / 2),
            "only unknown places are asked about"
        );
        // The first requests are spread over the whole unknown half, not bunched up.
        let first: std::collections::BTreeSet<u64> =
            next[..8].iter().map(|p| p * 8 / total).collect();
        assert!(first.len() >= 3, "{first:?}");
    }

    #[test]
    fn long_names_are_cut_in_the_middle_keeping_both_ends() {
        let w = [1.0; 10];
        assert_eq!(cut_middle(&w, 1.0, 10.0), None, "fits whole");
        assert_eq!(
            cut_middle(&w, 1.0, 7.0),
            Some((3, 3)),
            "front and back in turn"
        );
        assert_eq!(cut_middle(&w, 1.0, 4.0), Some((1, 2)), "the end first");
        assert_eq!(cut_middle(&w, 1.0, 0.5), Some((0, 0)));
    }

    #[test]
    fn scan_paths_map_to_where_they_are_mounted() {
        let m = |path: &str, subvolume: &str| Mount {
            path: path.into(),
            subvolume: subvolume.into(),
        };
        // Like the desktop: @ at /, the store bind-mounted read-only at /nix/store.
        let desk = [m("/", "/@"), m("/nix/store", "/@/nix/store")];
        assert_eq!(
            real_path(&desk, "@/home/josh/x"),
            Some("/home/josh/x".into())
        );
        assert_eq!(real_path(&desk, "@"), Some("/".into()));
        assert_eq!(
            real_path(&desk, "@/nix/store/abc-foo/bin/foo"),
            Some("/nix/store/abc-foo/bin/foo".into())
        );
        assert_eq!(real_path(&desk, "@snap/x"), None, "not mounted anywhere");
        assert_eq!(
            real_path(&desk, "@x/y"),
            None,
            "a prefix of a name is not its parent"
        );
        // The top level mounted directly reaches every subvolume.
        let top = [m("/top", "/")];
        assert_eq!(real_path(&top, "@snap/x"), Some("/top/@snap/x".into()));
    }

    #[test]
    fn preselects_the_only_filesystem_or_the_root_one() {
        let fs = |source: &str, mount: &str| Filesystem {
            device: source.into(),
            source: source.into(),
            mounts: vec![Mount {
                path: mount.into(),
                subvolume: "/".into(),
            }],
            label: None,
            usage: None,
        };
        assert_eq!(default_selection(&[fs("a", "/data")]), Some("/data".into()));
        assert_eq!(
            default_selection(&[fs("a", "/data"), fs("b", "/")]),
            Some("/".into())
        );
        assert_eq!(
            default_selection(&[fs("a", "/data"), fs("b", "/mnt")]),
            None
        );
        assert_eq!(default_selection(&[]), None);
    }

    #[test]
    fn mountinfo_groups_btrfs_mounts_by_device_and_skips_the_rest() {
        let text = "\
22 1 0:32 /@ / rw,relatime shared:1 - btrfs /dev/mapper/root rw,ssd,subvol=/@
23 22 0:32 /@/nix/store /nix/store ro shared:2 - btrfs /dev/mapper/root rw,ssd
24 22 0:5 / /dev rw shared:3 - devtmpfs devtmpfs rw
25 22 0:40 / /mnt/my\\040disk rw shared:4 - btrfs /dev/sdb1 rw";
        let fs = parse_mountinfo(text);
        let summary: Vec<_> = fs
            .iter()
            .map(|f| {
                let mounts: Vec<_> = f
                    .mounts
                    .iter()
                    .map(|m| (m.path.to_str().unwrap(), m.subvolume.as_str()))
                    .collect();
                (f.source.as_str(), mounts)
            })
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "/dev/mapper/root",
                    vec![("/", "/@"), ("/nix/store", "/@/nix/store")]
                ),
                ("/dev/sdb1", vec![("/mnt/my disk", "/")]),
            ]
        );
        assert_eq!(fs[0].scan_target(), Some(PathBuf::from("/")));
    }

    #[test]
    fn cells_for_unknown_sets_are_rejected() {
        let header = Msg::Header(Header {
            source: "/".into(),
            order: 1,
            total: 4,
            cell: 1,
            calibration: vec![],
        });
        let cells = Msg::Cells {
            level: 1,
            cells: vec![Cell(0, 7, Algo::None, 100, 0)],
        };
        let result = [header, cells]
            .into_iter()
            .try_fold(Model::default(), Model::apply);
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::proto::{Cell, Header};
    use std::time::Instant;

    /// Order 11 (2048²) with half a million file sets in a deep tree.
    fn big_model() -> Model {
        let order = 11;
        let sets = 500_000u32;
        let mut msgs = vec![Msg::Header(Header {
            source: "/".into(),
            order,
            total: 1 << 40,
            cell: (1u64 << 40) >> (2 * order),
            calibration: vec![(1, 1_700_000_000), (1_000_000, 1_790_000_000)],
        })];
        for id in 0..sets {
            let path = format!(
                "@/home/josh/d{}/e{}/f{}/file{id}",
                id % 97,
                id % 1013,
                id % 7919
            );
            msgs.push(Msg::Set(SetInfo {
                id,
                kind: Kind::Data,
                paths: vec![path.clone()],
                path_count: 1,
                files: 1 + (id % 3 == 0) as usize,
                subvolumes: 1,
                truncated: false,
                error: String::new(),
                dominator: path,
            }));
        }
        let cells: Vec<Cell> = (0..1u32 << (2 * order))
            .map(|d| {
                Cell(
                    d,
                    (d.wrapping_mul(2654435761) >> 8) % sets,
                    Algo::Zstd,
                    150 + d % 400,
                    1 + (d as u64 % 900_000),
                )
            })
            .collect();
        msgs.push(Msg::Cells {
            level: order,
            cells,
        });
        msgs.into_iter()
            .try_fold(Model::default(), |m, msg| m.apply(msg).map_err(|r| r.1))
            .unwrap()
    }

    fn time<T>(what: &str, f: impl FnOnce() -> T) -> T {
        let t = Instant::now();
        let out = f();
        eprintln!("{what:>28}: {:>8.1} ms", t.elapsed().as_secs_f64() * 1e3);
        out
    }

    #[test]
    #[ignore]
    fn costs_at_2048() {
        let model = time("build model", big_model);
        let s = time("stats", || stats(&model));
        for mode in [Mode::Owners, Mode::Compression, Mode::Age] {
            time(&format!("ranked {mode:?}"), || ranked(&model, &s, mode));
            time(&format!("categories {mode:?}"), || {
                categories(&model, &s, &Palette::new(true), mode)
            });
        }
        let grid = time("grid", || Grid::new(11));
        let raster: Vec<u32> = time("raster sets", || {
            grid.cell_at
                .iter()
                .map(|&d| model.set[d as usize])
                .collect()
        });
        for mode in [Mode::Owners, Mode::Age] {
            time(&format!("base colors {mode:?}"), || {
                cell_colors(&model, &Palette::new(true), mode, &grid.cell_at).len()
            });
        }
        let base = cell_colors(&model, &Palette::new(true), Mode::Owners, &grid.cell_at);
        time("hover dim pass", || {
            base.iter()
                .zip(&raster)
                .map(|(&c, &s)| if s == 7 { c } else { c.gamma_multiply(0.18) })
                .collect::<Vec<_>>()
        });
        time("hover image build", || {
            ColorImage::new([2048, 2048], base.clone())
        });
        let bytes: Vec<u64> = s.area.iter().map(|a| a * 1000).collect();
        let tree = time("tree build", || treemap::build("/", &model.sets, &bytes));
        let r = treemap::Rect {
            x: 0.0,
            y: 0.0,
            w: 1600.0,
            h: 1000.0,
        };
        time("tree layout", || {
            treemap::layout(&tree, treemap::ROOT, r, 3.0, 30_000, false).len()
        });
    }
}
