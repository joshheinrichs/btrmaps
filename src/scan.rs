use crate::btrfs::{self, Chunk, ChunkKind, Compression, Extent, Ref};
use crate::proto::{Algo, Cell, Header, Kind, Msg, Request, Run, SetInfo};
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

const SECTOR: u64 = 4096;
const MAX_SET_PATHS: usize = 32;
/// The coarsest level streamed; 64×64 is enough for a first picture.
const FIRST_LEVEL: u32 = 6;
/// Cells probed (in parallel) per message.
const BATCH: usize = 1 << 14;

pub struct Args {
    pub path: PathBuf,
    pub out: Option<PathBuf>,
    pub order: u32,
}

#[derive(Clone, Copy)]
enum Target {
    Past,
    At { logical: u64, kind: ChunkKind },
}

/// One probe address per cell: the sector-aligned middle of the cell's slice of the
/// chunks laid end to end in logical order.
fn targets(chunks: &[Chunk], cells: u64, cell_bytes: u64) -> Vec<Target> {
    let starts: Vec<u64> = chunks
        .iter()
        .scan(0, |pos, c| {
            let start = *pos;
            *pos += c.length;
            Some(start)
        })
        .collect();
    (0..cells)
        .map(|i| {
            let pos = i * cell_bytes + cell_bytes / 2;
            let idx = starts.partition_point(|&s| s <= pos).saturating_sub(1);
            let chunk = &chunks[idx];
            let within = pos - starts[idx];
            match within < chunk.length {
                true => Target::At {
                    logical: chunk.logical + within / SECTOR * SECTOR,
                    kind: chunk.kind,
                },
                false => Target::Past,
            }
        })
        .collect()
}

/// The chunks laid end to end: byte positions along the curve, and back.
struct Layout {
    chunks: Vec<Chunk>,
    /// Where each chunk starts along the curve.
    starts: Vec<u64>,
    total: u64,
}

impl Layout {
    fn new(chunks: Vec<Chunk>) -> Self {
        let starts: Vec<u64> = chunks
            .iter()
            .scan(0, |pos, c| {
                let start = *pos;
                *pos += c.length;
                Some(start)
            })
            .collect();
        let total = chunks.iter().map(|c| c.length).sum();
        Layout {
            chunks,
            starts,
            total,
        }
    }

    /// The chunk holding curve position `pos`, and where that chunk starts on the curve.
    fn chunk_at(&self, pos: u64) -> Option<(&Chunk, u64)> {
        if pos >= self.total {
            return None;
        }
        let i = self.starts.partition_point(|&s| s <= pos).saturating_sub(1);
        Some((&self.chunks[i], self.starts[i]))
    }
}

/// Every index below 4^level, bit-reversed so consecutive batches land spread across the
/// whole curve and the picture sharpens evenly instead of filling in one corner first.
fn spread(level: u32) -> Vec<u32> {
    let bits = 2 * level;
    (0..1u32 << bits)
        .map(|i| match bits {
            0 => 0,
            b => i.reverse_bits() >> (32 - b),
        })
        .collect()
}

/// What one cell holds. Doubles as the identity of an attribution set: cells are
/// grouped by equal probes.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Probe {
    Past,
    Free,
    Metadata,
    System,
    Data {
        refs: Vec<Ref>,
        unreachable: bool,
        truncated: bool,
    },
    Error(String),
}

/// Probe one cell: who owns it, and the on-disk extent it sits in (data cells only).
fn probe(fd: RawFd, target: Target) -> (Probe, Option<Extent>) {
    let logical = match target {
        Target::Past => return (Probe::Past, None),
        Target::At {
            kind: ChunkKind::Metadata,
            ..
        } => return (Probe::Metadata, None),
        Target::At {
            kind: ChunkKind::System,
            ..
        } => return (Probe::System, None),
        Target::At { logical, .. } => logical,
    };
    let classify = |r: std::io::Result<btrfs::Refs>, unreachable| match r {
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Some((Probe::Free, None)),
        Err(e) => Some((Probe::Error(format!("logical ino: {e}")), None)),
        Ok(r) if r.refs.is_empty() => None,
        Ok(r) => {
            let extent = r.sample.and_then(|(sample, file_offset)| {
                btrfs::file_extent(fd, sample, file_offset, logical)
                    .ok()
                    .flatten()
            });
            let probe = Probe::Data {
                refs: r.refs,
                unreachable,
                truncated: r.truncated,
            };
            Some((probe, extent))
        }
    };
    classify(btrfs::logical_ino(fd, logical, false), false)
        .or_else(|| classify(btrfs::logical_ino(fd, logical, true), true))
        .unwrap_or_else(|| (Probe::Error("extent has no references".into()), None))
}

fn compression(extent: Option<Extent>) -> (Algo, u32) {
    let Some(e) = extent else {
        return (Algo::Unknown, 0);
    };
    let algo = match e.compression {
        Compression::None => Algo::None,
        Compression::Zlib => Algo::Zlib,
        Compression::Lzo => Algo::Lzo,
        Compression::Zstd => Algo::Zstd,
        Compression::Other(_) => Algo::Other,
    };
    (algo, (e.ram_bytes * 100 / e.disk_bytes.max(1)) as u32)
}

/// Probe curve position `pos` and widen the answer to everything known to share it:
/// the part of the extent the file uses (the whole extent when compressed), up to the
/// next allocated extent for free space, the whole chunk for metadata and system. The
/// probe, its extent, and the run's start and length along the curve; None past the end.
fn run_at(fd: RawFd, layout: &Layout, pos: u64) -> Option<(Probe, Option<Extent>, u64, u64)> {
    let (chunk, chunk_pos) = layout.chunk_at(pos)?;
    let within = (pos - chunk_pos) / SECTOR * SECTOR;
    let logical = chunk.logical + within;
    let (probe, extent) = probe(
        fd,
        Target::At {
            logical,
            kind: chunk.kind,
        },
    );
    let chunk_end = chunk_pos + chunk.length;
    let to_pos = |l: u64| chunk_pos + l.saturating_sub(chunk.logical);
    let here = chunk_pos + within;
    let (start, end) = match (&probe, extent) {
        (Probe::Metadata | Probe::System, _) => (chunk_pos, chunk_end),
        (Probe::Data { .. }, Some(e)) => (
            to_pos(e.used.0).max(chunk_pos),
            to_pos(e.used.0 + e.used.1).min(chunk_end),
        ),
        (Probe::Free, _) => {
            let next = btrfs::next_allocated(fd, logical, chunk.logical + chunk.length);
            (here, next.ok().flatten().map_or(chunk_end, to_pos))
        }
        _ => (here, here + SECTOR),
    };
    // Whatever the tree said, the run covers at least the probed sector.
    let (start, end) = (start.min(here), end.max(here + SECTOR).min(chunk_end));
    Some((probe, extent, start, end - start))
}

/// Sets seen so far, by identity.
#[derive(Default)]
struct Interner {
    index: HashMap<Probe, u32>,
}

/// Number the probes' sets, making one output per probe with `make(set id, extra)` and
/// returning any sets not seen before.
fn intern<X, T>(
    interner: Interner,
    probes: Vec<(Probe, X)>,
    make: impl Fn(u32, X) -> T,
) -> (Interner, Vec<T>, Vec<(u32, Probe)>) {
    let Interner { mut index } = interner;
    let mut new = Vec::new();
    let out = probes
        .into_iter()
        .map(|(probe, extra)| {
            let next = index.len() as u32;
            let id = *index.entry(probe.clone()).or_insert_with(|| {
                new.push((next, probe));
                next
            });
            make(id, extra)
        })
        .collect();
    (Interner { index }, out, new)
}

/// Opened subvolumes and resolved file paths, grown as new files turn up.
#[derive(Default)]
struct Resolver {
    /// Subvolume path from the top level, and its root opened (or why not).
    subvols: BTreeMap<u64, (String, Result<OwnedFd, String>)>,
    names: HashMap<Ref, Vec<String>>,
}

/// Resolve full paths (relative to the top-level subvolume) for every ref not yet known.
fn resolve(top: RawFd, resolver: Resolver, refs: BTreeSet<Ref>) -> Resolver {
    let Resolver {
        mut subvols,
        mut names,
    } = resolver;
    let missing: Vec<Ref> = refs
        .into_iter()
        .filter(|r| !names.contains_key(r))
        .collect();
    for root in missing.iter().map(|r| r.root).collect::<BTreeSet<_>>() {
        subvols
            .entry(root)
            .or_insert_with(|| match btrfs::root_path(top, root) {
                Ok(path) => {
                    let fd = btrfs::open_subvolume(top, &path)
                        .map_err(|e| format!("open subvolume {path:?}: {e}"));
                    (path, fd)
                }
                Err(e) => (format!("<subvolume {root}>"), Err(format!("{e:#}"))),
            });
    }
    let found: Vec<(Ref, Vec<String>)> = missing
        .into_par_iter()
        .map(|r| {
            let (prefix, fd) = &subvols[&r.root];
            let join = |p: &str| match prefix.as_str() {
                "" => p.to_string(),
                pre => format!("{pre}/{p}"),
            };
            let paths = match fd {
                Err(e) => vec![join(&format!("<inode {}: {e}>", r.inode))],
                Ok(fd) => match btrfs::ino_paths(fd.as_raw_fd(), r.inode) {
                    Ok(paths) if !paths.is_empty() => paths.iter().map(|p| join(p)).collect(),
                    Ok(_) => vec![join(&format!("<inode {}: unlinked>", r.inode))],
                    Err(e) => vec![join(&format!("<inode {}: {e}>", r.inode))],
                },
            };
            (r, paths)
        })
        .collect();
    names.extend(found);
    Resolver { subvols, names }
}

/// The deepest directory holding every path: their common leading components.
fn dominator<'a>(paths: impl IntoIterator<Item = &'a str>) -> String {
    let mut paths = paths.into_iter();
    let Some(first) = paths.next() else {
        return String::new();
    };
    let mut common: Vec<&str> = first.split('/').collect();
    for p in paths {
        let shared = common
            .iter()
            .zip(p.split('/'))
            .take_while(|(a, b)| *a == b)
            .count();
        common.truncate(shared);
    }
    common.join("/")
}

fn set_info(id: u32, probe: &Probe, names: &HashMap<Ref, Vec<String>>) -> SetInfo {
    let base = |kind| SetInfo {
        id,
        kind,
        paths: Vec::new(),
        path_count: 0,
        files: 0,
        subvolumes: 0,
        truncated: false,
        error: String::new(),
        dominator: String::new(),
    };
    match probe {
        Probe::Past => base(Kind::Past),
        Probe::Free => base(Kind::Free),
        Probe::Metadata => base(Kind::Metadata),
        Probe::System => base(Kind::System),
        Probe::Error(e) => SetInfo {
            error: e.clone(),
            ..base(Kind::Error)
        },
        Probe::Data {
            refs,
            unreachable,
            truncated,
        } => {
            let paths: BTreeSet<&str> = refs
                .iter()
                .flat_map(|r| names[r].iter().map(String::as_str))
                .collect();
            SetInfo {
                paths: paths
                    .iter()
                    .take(MAX_SET_PATHS)
                    .map(|p| p.to_string())
                    .collect(),
                path_count: paths.len(),
                dominator: dominator(paths.iter().copied()),
                files: refs.len(),
                subvolumes: refs.iter().map(|r| r.root).collect::<BTreeSet<_>>().len(),
                truncated: *truncated,
                ..base(if *unreachable {
                    Kind::Unreachable
                } else {
                    Kind::Data
                })
            }
        }
    }
}

fn send(out: &mut impl Write, msg: &Msg) -> std::io::Result<()> {
    serde_json::to_writer(&mut *out, msg)?;
    out.write_all(b"\n")
}

/// Resolve paths for sets not seen before and send them, ahead of the cells or runs
/// that use them.
fn announce(
    fd: RawFd,
    resolver: Resolver,
    new: &[(u32, Probe)],
    out: &mut impl Write,
) -> Result<Resolver> {
    let refs = new
        .iter()
        .flat_map(|(_, p)| match p {
            Probe::Data { refs, .. } => refs.clone(),
            _ => Vec::new(),
        })
        .collect();
    let resolver = resolve(fd, resolver, refs);
    for (id, probe) in new {
        send(out, &Msg::Set(set_info(*id, probe, &resolver.names)))?;
    }
    Ok(resolver)
}

/// Answer one probe request with the runs around each position.
fn serve(
    fd: RawFd,
    layout: &Layout,
    (interner, resolver): (Interner, Resolver),
    positions: Vec<u64>,
    out: &mut impl Write,
) -> Result<(Interner, Resolver)> {
    let found: Vec<_> = positions
        .par_iter()
        .filter_map(|&pos| run_at(fd, layout, pos))
        .map(|(probe, extent, start, len)| (probe, (extent, start, len)))
        .collect();
    let (interner, runs, new) = intern(interner, found, |id, (extent, start, len)| {
        let (algo, ratio) = compression(extent);
        Run(
            start,
            len,
            id,
            algo,
            ratio,
            extent.map_or(0, |e| e.generation),
        )
    });
    let resolver = announce(fd, resolver, &new, out)?;
    send(out, &Msg::Runs { runs })?;
    out.flush()?;
    Ok((interner, resolver))
}

/// Probe requests from the window, one JSON line each on stdin, read on a thread so the
/// scan can check for them between batches. Ends when the window closes the pipe.
fn requests() -> std::sync::mpsc::Receiver<Request> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            let Ok(request) = serde_json::from_str::<Request>(&line) else {
                continue;
            };
            if tx.send(request).is_err() {
                return;
            }
        }
    });
    rx
}

/// Scan in the background: lowest CPU priority, idle-class disk I/O, and one core left
/// free, so the window (and everything else) stays responsive while it runs. Best
/// effort; a scan at normal priority is still correct.
fn yield_to_the_desktop() {
    const IOPRIO_WHO_PROCESS: libc::c_long = 1;
    const IOPRIO_CLASS_IDLE: libc::c_long = 3;
    // SAFETY: plain syscalls on this process. Set before rayon starts its threads,
    // which inherit the niceness of the thread that creates them.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            0,
            IOPRIO_CLASS_IDLE << 13,
        );
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(cores.saturating_sub(1).max(1))
        .build_global();
}

pub fn run(args: Args) -> Result<()> {
    // Open the output before entering a private mount namespace.
    let out: Box<dyn Write> = match &args.out {
        Some(p) => {
            Box::new(std::fs::File::create(p).with_context(|| format!("create {}", p.display()))?)
        }
        None => Box::new(std::io::stdout().lock()),
    };
    let mut out = std::io::BufWriter::new(out);
    yield_to_the_desktop();
    let top = btrfs::open_top_level(&args.path)?;
    let fd = top.as_raw_fd();

    let chunks = btrfs::chunks(fd)?;
    let total: u64 = chunks.iter().map(|c| c.length).sum();
    let cell = total.div_ceil(1 << (2 * args.order));
    eprintln!(
        "{} chunks, {} MiB allocated, {}×{} cells of {} KiB",
        chunks.len(),
        total >> 20,
        1 << args.order,
        1 << args.order,
        cell >> 10
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let header = Header {
        source: args.path.display().to_string(),
        order: args.order,
        total,
        cell,
        calibration: btrfs::calibration(fd, now),
    };
    send(&mut out, &Msg::Header(header))?;

    let layout = Layout::new(chunks.clone());
    // Only a window streaming from us sends probe requests; a saved scan just ends.
    let requests = args.out.is_none().then(requests);
    let mut state = (Interner::default(), Resolver::default());
    for level in FIRST_LEVEL.min(args.order)..=args.order {
        let level_cell = cell << (2 * (args.order - level));
        let targets = targets(&chunks, 1 << (2 * level), level_cell);
        let order = spread(level);
        for (batch_no, batch) in order.chunks(BATCH).enumerate() {
            // What the window is looking at comes first.
            for request in requests.iter().flat_map(|r| r.try_iter()) {
                let Request::Probe { positions } = request;
                state = serve(fd, &layout, state, positions, &mut out)?;
            }
            let probes: Vec<_> = batch
                .par_iter()
                .map(|&d| {
                    let (probe, extent) = probe(fd, targets[d as usize]);
                    (probe, (d, extent))
                })
                .collect();
            let (interner, cells, new) = intern(state.0, probes, |id, (d, extent)| {
                let (algo, ratio) = compression(extent);
                Cell(d, id, algo, ratio, extent.map_or(0, |e| e.generation))
            });
            let resolver = announce(fd, state.1, &new, &mut out)?;
            state = (interner, resolver);
            send(&mut out, &Msg::Cells { level, cells })?;
            out.flush()?;
            eprint!(
                "\rlevel {level}/{}: {}%   ",
                args.order,
                ((batch_no + 1) * BATCH).min(order.len()) * 100 / order.len()
            );
        }
    }
    send(&mut out, &Msg::Done)?;
    out.flush()?;
    eprintln!("\ndone: {} sets", state.0.index.len());
    // Keep answering the window's probes until it closes our stdin.
    for request in requests.iter().flat_map(|r| r.iter()) {
        let Request::Probe { positions } = request;
        state = serve(fd, &layout, state, positions, &mut out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_walk_chunks_in_order_and_stop_at_the_end() {
        let chunks = [
            Chunk {
                logical: 1 << 30,
                length: 2 * SECTOR,
                kind: ChunkKind::Metadata,
            },
            Chunk {
                logical: 5 << 30,
                length: 4 * SECTOR,
                kind: ChunkKind::Data,
            },
        ];
        let got: Vec<_> = targets(&chunks, 8, SECTOR)
            .into_iter()
            .map(|t| match t {
                Target::Past => None,
                Target::At { logical, kind } => Some((logical, kind)),
            })
            .collect();
        let m = |i| Some(((1 << 30) + i * SECTOR, ChunkKind::Metadata));
        let d = |i| Some(((5 << 30) + i * SECTOR, ChunkKind::Data));
        assert_eq!(got, [m(0), m(1), d(0), d(1), d(2), d(3), None, None]);
    }

    #[test]
    fn dominator_is_the_deepest_directory_holding_every_path() {
        assert_eq!(
            dominator(["@/home/a/x", "@/home/a/y", "@/home/b"]),
            "@/home"
        );
        assert_eq!(dominator(["@/x", "@snap/x"]), "");
        assert_eq!(dominator(["@/home/ab", "@/home/abc"]), "@/home");
        assert_eq!(dominator(["@/only"]), "@/only");
    }
    #[test]
    fn spread_visits_every_cell_once_and_starts_in_each_quadrant() {
        for level in 0..=6 {
            let order = spread(level);
            let mut sorted = order.clone();
            sorted.sort();
            assert_eq!(sorted, (0..1 << (2 * level)).collect::<Vec<_>>());
        }
        let quadrant = |d: u32| d >> (2 * 3 - 2);
        let firsts: BTreeSet<u32> = spread(3)[..4].iter().map(|&d| quadrant(d)).collect();
        assert_eq!(firsts.len(), 4);
    }
}
