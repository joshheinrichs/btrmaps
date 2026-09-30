use serde::{Deserialize, Serialize};

/// One JSON line of the stream `btrmaps scan` writes and `btrmaps view` reads.
///
/// A header comes first. A set is always sent before the first cell that uses it.
/// Cells arrive coarse to fine: each level probes the whole curve at 4^level cells,
/// so a viewer can draw every level as it lands and let finer ones overwrite it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    Header(Header),
    Set(SetInfo),
    Cells {
        level: u32,
        cells: Vec<Cell>,
    },
    /// Exact answers to probe requests: whole extents (or free stretches) around each
    /// probed position, so one answer can fill many pixels.
    Runs {
        runs: Vec<Run>,
    },
    Done,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Header {
    pub source: String,
    /// The finest level; the curve is 2^order cells on a side.
    pub order: u32,
    /// Bytes of chunks laid end to end.
    pub total: u64,
    /// Bytes per cell at the finest level. A level-k cell is `cell * 4^(order-k)`.
    pub cell: u64,
    /// Known (generation, unix time) pairs, oldest first: subvolume creation and last
    /// change, and the filesystem's generation at scan time. Lets a viewer put an
    /// approximate date on the generation that wrote an extent.
    #[serde(default)]
    pub calibration: Vec<(u64, i64)>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Data,
    /// No file uses these bytes any more, but an extent the files still partly use pins them.
    Unreachable,
    Free,
    Metadata,
    System,
    /// Past the end of the filesystem; the grid has more cells than bytes.
    Past,
    Error,
}

/// An attribution set: the exact files an extent belongs to, or a non-file kind.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SetInfo {
    /// Dense, in order of first appearance.
    pub id: u32,
    pub kind: Kind,
    /// Up to 32 paths, sorted, relative to the top-level subvolume.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// All distinct paths, including the ones not sent.
    #[serde(default)]
    pub path_count: usize,
    /// Distinct inodes.
    #[serde(default)]
    pub files: usize,
    #[serde(default)]
    pub subvolumes: usize,
    /// The kernel had more references than btrmaps could read.
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// The deepest directory holding every path, computed from all of them ("" = top level).
    /// Deleting anything below it leaves these bytes in use.
    #[serde(default)]
    pub dominator: String,
}

/// Compression of the extent a cell sits in.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Algo {
    /// Not a data cell, or its extent item could not be found.
    #[default]
    Unknown,
    None,
    Zlib,
    Lzo,
    Zstd,
    Other,
}

/// `[curve index at its level, set id, algorithm, decompressed/on-disk ratio × 100,
/// generation that wrote the extent (0 when unknown)]`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct Cell(pub u32, pub u32, pub Algo, pub u32, pub u64);

/// `[start, length, set id, algorithm, ratio × 100, generation]`, in bytes along the
/// curve (chunks laid end to end, as `Header::total` counts them).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct Run(pub u64, pub u64, pub u32, pub Algo, pub u32, pub u64);

/// What the window asks the helper for, one JSON line each on its stdin.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Request {
    /// Find what is at each of these byte positions along the curve.
    Probe { positions: Vec<u64> },
}
