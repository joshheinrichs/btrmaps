use serde::{Deserialize, Serialize};

/// One JSON line of what `btrmaps scan` writes: a header, then for each probe request
/// any sets not seen before, followed by the request's runs.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    Header(Header),
    Set(SetInfo),
    /// The answer to one probe request: for each position, in order, everything known
    /// to share it (the used part of its extent, a free stretch, a metadata chunk), so
    /// one answer can fill many pixels.
    Runs {
        runs: Vec<Run>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Header {
    pub source: String,
    /// Bytes of chunks laid end to end: the length of the curve.
    pub total: u64,
    /// Known (generation, unix time) pairs, oldest first: subvolume creation and last
    /// change, and the filesystem's generation now. Lets the window put an
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
}

/// Compression of the extent a position sits in.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Algo {
    /// Not file data, or its extent item could not be found.
    #[default]
    Unknown,
    None,
    Zlib,
    Lzo,
    Zstd,
    Other,
}

/// `[start, length, set id, algorithm, ratio × 100, generation]`, in bytes along the
/// curve (chunks laid end to end, as `Header::total` counts them). Generation 0 means
/// unknown.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct Run(pub u64, pub u64, pub u32, pub Algo, pub u32, pub u64);

/// What the window asks the helper for, one JSON line each on its stdin.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Request {
    /// Find what is at each of these byte positions along the curve.
    Probe { positions: Vec<u64> },
}
