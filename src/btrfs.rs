use anyhow::{Context, Result, bail};
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const TOP_LEVEL: u64 = 5;
const FIRST_FREE_OBJECTID: u64 = 256;
const ROOT_TREE: u64 = 1;
const CHUNK_TREE: u64 = 3;
const ROOT_BACKREF_KEY: u32 = 144;
const CHUNK_ITEM_KEY: u32 = 228;

const BLOCK_GROUP_DATA: u64 = 1 << 0;
const BLOCK_GROUP_SYSTEM: u64 = 1 << 1;
const LOGICAL_INO_IGNORE_OFFSET: u64 = 1;

const RESULT_BUF: usize = 64 * 1024;

const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | (0x94 << 8) | nr
}
const IOC_TREE_SEARCH: u64 = ioc(3, 17, size_of::<SearchArgs>());
const IOC_INO_LOOKUP: u64 = ioc(3, 18, size_of::<InoLookupArgs>());
const IOC_DEV_INFO: u64 = ioc(3, 30, size_of::<DevInfoArgs>());
const IOC_FS_INFO: u64 = ioc(2, 31, size_of::<FsInfoArgs>());
const IOC_INO_PATHS: u64 = ioc(3, 35, size_of::<PathArgs>());
const IOC_LOGICAL_INO_V2: u64 = ioc(3, 59, size_of::<PathArgs>());

#[repr(C)]
#[derive(Default)]
struct SearchKey {
    tree_id: u64,
    min_objectid: u64,
    max_objectid: u64,
    min_offset: u64,
    max_offset: u64,
    min_transid: u64,
    max_transid: u64,
    min_type: u32,
    max_type: u32,
    nr_items: u32,
    unused: u32,
    unused1: [u64; 4],
}

#[repr(C)]
struct SearchArgs {
    key: SearchKey,
    buf: [u8; 4096 - size_of::<SearchKey>()],
}

#[repr(C)]
struct InoLookupArgs {
    treeid: u64,
    objectid: u64,
    name: [u8; 4080],
}

#[repr(C)]
struct FsInfoArgs {
    data: [u8; 1024],
}

#[repr(C)]
struct DevInfoArgs {
    devid: u64,
    uuid: [u8; 16],
    bytes_used: u64,
    total_bytes: u64,
    fsid: [u8; 16],
    unused: [u64; 377],
    path: [u8; 1024],
}

/// Shared by INO_PATHS and LOGICAL_INO_V2: a u64 in, a size, padding, flags and a result pointer.
#[repr(C)]
struct PathArgs {
    input: u64,
    size: u64,
    reserved: [u64; 3],
    flags: u64,
    result: u64,
}

fn zeroed<T>() -> Box<T> {
    // SAFETY: only used for the plain-integer ioctl argument structs above.
    unsafe { Box::new(std::mem::zeroed()) }
}

fn ioctl<T>(fd: RawFd, request: u64, arg: &mut T) -> io::Result<()> {
    // SAFETY: `arg` is a #[repr(C)] struct sized to match `request`.
    let ret = unsafe { libc::ioctl(fd, request as _, arg as *mut T) };
    match ret {
        0.. => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

fn u64_at(buf: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(buf[at..at + 8].try_into().unwrap())
}

fn u32_at(buf: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap())
}

fn c_str(buf: &[u8]) -> String {
    CStr::from_bytes_until_nul(buf)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|_| String::from_utf8_lossy(buf).into_owned())
}

struct Item {
    objectid: u64,
    key_type: u32,
    offset: u64,
    data: Vec<u8>,
}

/// A btrfs key: (objectid, type, offset), compared in that order.
type Key = (u64, u32, u64);

/// The key right after `k`, or None past the last possible key.
fn next_key((objectid, key_type, offset): Key) -> Option<Key> {
    match (offset.checked_add(1), key_type < 255) {
        (Some(o), _) => Some((objectid, key_type, o)),
        (None, true) => Some((objectid, key_type + 1, 0)),
        (None, false) => objectid.checked_add(1).map(|o| (o, 0, 0)),
    }
}

/// Up to `nr` items in `tree` with keys from `from` to `max` (inclusive, compared as keys).
fn search_page(fd: RawFd, tree: u64, from: Key, max: Key, nr: u32) -> io::Result<Vec<Item>> {
    let mut args = zeroed::<SearchArgs>();
    args.key = SearchKey {
        tree_id: tree,
        min_objectid: from.0,
        min_type: from.1,
        min_offset: from.2,
        max_objectid: max.0,
        max_type: max.1,
        max_offset: max.2,
        max_transid: u64::MAX,
        nr_items: nr,
        ..Default::default()
    };
    ioctl(fd, IOC_TREE_SEARCH, &mut *args)?;
    let found = args.key.nr_items as usize;
    let mut at = 0;
    Ok((0..found)
        .map(|_| {
            let len = u32_at(&args.buf, at + 28) as usize;
            let item = Item {
                objectid: u64_at(&args.buf, at + 8),
                offset: u64_at(&args.buf, at + 16),
                key_type: u32_at(&args.buf, at + 24),
                data: args.buf[at + 32..at + 32 + len].to_vec(),
            };
            at += 32 + len;
            item
        })
        .collect())
}

/// Every item in `tree` with a key between `min` and `max` (inclusive, compared as keys).
fn search(fd: RawFd, tree: u64, min: Key, max: Key) -> io::Result<Vec<Item>> {
    let mut items = Vec::new();
    let mut from = min;
    loop {
        let page = search_page(fd, tree, from, max, 4096)?;
        let Some(last) = page.last() else {
            return Ok(items);
        };
        let next = next_key((last.objectid, last.key_type, last.offset));
        items.extend(page);
        match next {
            Some(k) if k <= max => from = k,
            _ => return Ok(items),
        }
    }
}

/// All items of one key type for `objectid` in `tree` whose key offset lies in `offsets`.
fn tree_search(
    fd: RawFd,
    tree: u64,
    objectid: u64,
    key_type: u32,
    offsets: std::ops::RangeInclusive<u64>,
) -> io::Result<Vec<Item>> {
    search(
        fd,
        tree,
        (objectid, key_type, *offsets.start()),
        (objectid, key_type, *offsets.end()),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkKind {
    Data,
    Metadata,
    System,
}

#[derive(Clone, Copy, Debug)]
pub struct Chunk {
    pub logical: u64,
    pub length: u64,
    pub kind: ChunkKind,
}

/// Every chunk of the filesystem in logical address order.
pub fn chunks(fd: RawFd) -> Result<Vec<Chunk>> {
    let items = tree_search(
        fd,
        CHUNK_TREE,
        FIRST_FREE_OBJECTID,
        CHUNK_ITEM_KEY,
        0..=u64::MAX,
    )
    .context("search chunk tree")?;
    Ok(items
        .iter()
        .map(|item| {
            let flags = u64_at(&item.data, 24);
            Chunk {
                logical: item.offset,
                length: u64_at(&item.data, 0),
                kind: match flags {
                    f if f & BLOCK_GROUP_DATA != 0 => ChunkKind::Data,
                    f if f & BLOCK_GROUP_SYSTEM != 0 => ChunkKind::System,
                    _ => ChunkKind::Metadata,
                },
            }
        })
        .collect())
}

fn ino_lookup(fd: RawFd, tree: u64, objectid: u64) -> io::Result<(u64, String)> {
    let mut args = zeroed::<InoLookupArgs>();
    args.treeid = tree;
    args.objectid = objectid;
    ioctl(fd, IOC_INO_LOOKUP, &mut *args)?;
    Ok((args.treeid, c_str(&args.name)))
}

/// Path of subvolume `root` relative to the top-level subvolume ("" for the top level itself).
pub fn root_path(fd: RawFd, root: u64) -> Result<String> {
    if root == TOP_LEVEL {
        return Ok(String::new());
    }
    let backrefs = tree_search(fd, ROOT_TREE, root, ROOT_BACKREF_KEY, 0..=u64::MAX)
        .with_context(|| format!("search backrefs of subvolume {root}"))?;
    let Some(backref) = backrefs.first() else {
        bail!("subvolume {root} has no backref (deleted?)");
    };
    debug_assert_eq!(backref.objectid, root);
    let parent = backref.offset;
    let dirid = u64_at(&backref.data, 0);
    let name_len = u16::from_ne_bytes(backref.data[16..18].try_into().unwrap()) as usize;
    let name = String::from_utf8_lossy(&backref.data[18..18 + name_len]);
    let (_, dir) = ino_lookup(fd, parent, dirid)
        .with_context(|| format!("look up directory {dirid} in subvolume {parent}"))?;
    let parent_path = root_path(fd, parent)?;
    Ok(match parent_path.as_str() {
        "" => format!("{dir}{name}"),
        p => format!("{p}/{dir}{name}"),
    })
}

fn open_dir(dirfd: RawFd, path: &Path) -> io::Result<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: plain openat on a NUL-terminated path.
    let fd = unsafe {
        libc::openat(
            dirfd,
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    match fd {
        0.. => Ok(unsafe { OwnedFd::from_raw_fd(fd) }),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Open the root directory of a subvolume, given its path from `root_path`.
pub fn open_subvolume(top: RawFd, path: &str) -> io::Result<OwnedFd> {
    open_dir(top, Path::new(if path.is_empty() { "." } else { path }))
}

fn cvt(ret: libc::c_int) -> io::Result<()> {
    match ret {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

fn device_path(fd: RawFd) -> Result<String> {
    let mut fs = zeroed::<FsInfoArgs>();
    ioctl(fd, IOC_FS_INFO, &mut *fs).context("BTRFS_IOC_FS_INFO (is this btrfs?)")?;
    (1..=u64_at(&fs.data, 0))
        .find_map(|devid| {
            let mut dev = zeroed::<DevInfoArgs>();
            dev.devid = devid;
            ioctl(fd, IOC_DEV_INFO, &mut *dev)
                .ok()
                .map(|()| c_str(&dev.path))
        })
        .context("no btrfs device found")
}

/// An fd on the top-level subvolume (id 5) of the filesystem holding `path`.
///
/// Subvolume paths only resolve from the top level, so when `path` is inside some other
/// subvolume, mount the top level read-only in a private mount namespace and detach it,
/// keeping only the fd.
pub fn open_top_level(path: &Path) -> Result<OwnedFd> {
    let fd = open_dir(libc::AT_FDCWD, path).with_context(|| format!("open {}", path.display()))?;
    let (tree, _) = ino_lookup(fd.as_raw_fd(), 0, FIRST_FREE_OBJECTID)
        .context("BTRFS_IOC_INO_LOOKUP (are you root, and is this btrfs?)")?;
    let meta = std::fs::metadata(path)?;
    if tree == TOP_LEVEL && std::os::unix::fs::MetadataExt::ino(&meta) == FIRST_FREE_OBJECTID {
        return Ok(fd);
    }

    let device = CString::new(device_path(fd.as_raw_fd())?)?;
    let dir = std::env::temp_dir().join(format!("btrmaps-{}", std::process::id()));
    let dir_c = CString::new(dir.as_os_str().as_bytes())?;
    // SAFETY: plain syscalls on NUL-terminated strings.
    unsafe {
        cvt(libc::unshare(libc::CLONE_NEWNS)).context("unshare mount namespace")?;
        cvt(libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        ))
        .context("make mounts private")?;
    }
    std::fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mounted = unsafe {
        cvt(libc::mount(
            device.as_ptr(),
            dir_c.as_ptr(),
            c"btrfs".as_ptr(),
            libc::MS_RDONLY,
            c"subvolid=5".as_ptr().cast(),
        ))
    }
    .with_context(|| format!("mount top-level subvolume of {device:?}"))
    .and_then(|()| {
        let top = open_dir(libc::AT_FDCWD, &dir).context("open top-level mount");
        unsafe { cvt(libc::umount2(dir_c.as_ptr(), libc::MNT_DETACH)) }
            .context("detach top-level mount")?;
        top
    });
    std::fs::remove_dir(&dir).with_context(|| format!("remove {}", dir.display()))?;
    mounted
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ref {
    pub root: u64,
    pub inode: u64,
}

pub struct Refs {
    pub refs: Vec<Ref>,
    /// The kernel had more references than fit the result buffer.
    pub truncated: bool,
    /// One reference with the file offset the kernel matched, to find the extent item by.
    pub sample: Option<(Ref, u64)>,
}

/// The container the kernel fills for LOGICAL_INO and INO_PATHS:
/// four u32 counters, then `elem_cnt` u64 values.
fn container(buf: &[u64]) -> (bool, &[u64]) {
    let head = buf[0].to_ne_bytes();
    let counts = buf[1].to_ne_bytes();
    let bytes_missing = u32::from_ne_bytes(head[4..8].try_into().unwrap());
    let elem_cnt = u32::from_ne_bytes(counts[0..4].try_into().unwrap()) as usize;
    (bytes_missing > 0, &buf[2..2 + elem_cnt])
}

/// Files whose extents cover logical address `logical`, sorted and deduplicated.
/// With `ignore_offset`, every file referencing the extent at all.
pub fn logical_ino(fd: RawFd, logical: u64, ignore_offset: bool) -> io::Result<Refs> {
    let mut buf = vec![0u64; RESULT_BUF / 8];
    let mut args = PathArgs {
        input: logical,
        size: RESULT_BUF as u64,
        reserved: [0; 3],
        flags: if ignore_offset {
            LOGICAL_INO_IGNORE_OFFSET
        } else {
            0
        },
        result: buf.as_mut_ptr() as u64,
    };
    ioctl(fd, IOC_LOGICAL_INO_V2, &mut args)?;
    let (truncated, vals) = container(&buf);
    let sample = vals.as_chunks::<3>().0.first().map(|v| {
        (
            Ref {
                inode: v[0],
                root: v[2],
            },
            v[1],
        )
    });
    let mut refs: Vec<Ref> = vals
        .as_chunks::<3>()
        .0
        .iter()
        .map(|v| Ref {
            inode: v[0],
            root: v[2],
        })
        .collect();
    refs.sort();
    refs.dedup();
    Ok(Refs {
        refs,
        truncated,
        sample,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    None,
    Zlib,
    Lzo,
    Zstd,
    Other(u8),
}

/// The on-disk extent holding a logical address, as its file extent item records it.
#[derive(Clone, Copy, Debug)]
pub struct Extent {
    pub compression: Compression,
    /// The part of it (logical start, bytes) this file's extent item uses. The whole
    /// extent when compressed, since compressed bytes can't be split by file offset.
    pub used: (u64, u64),
    /// Bytes the extent occupies on disk.
    pub disk_bytes: u64,
    /// Bytes it holds once decompressed.
    pub ram_bytes: u64,
    /// The transaction that wrote it.
    pub generation: u64,
}

const EXTENT_DATA_KEY: u32 = 108;
const FILE_EXTENT_INLINE: u8 = 0;
/// Largest data extent btrfs writes; bounds how far before `file_offset` its key can sit.
const MAX_EXTENT: u64 = 128 << 20;

/// Find the file extent item of `r` near `file_offset` whose disk extent covers `logical`.
pub fn file_extent(
    fd: RawFd,
    r: Ref,
    file_offset: u64,
    logical: u64,
) -> io::Result<Option<Extent>> {
    let window = file_offset.saturating_sub(MAX_EXTENT)..=file_offset.saturating_add(128 << 10);
    let items = tree_search(fd, r.root, r.inode, EXTENT_DATA_KEY, window)?;
    Ok(items.iter().find_map(|item| {
        let d = &item.data;
        if d.len() < 53 || d[20] == FILE_EXTENT_INLINE {
            return None;
        }
        let (start, disk_bytes) = (u64_at(d, 21), u64_at(d, 29));
        // Several items can point into one extent (a partly overwritten file): take the
        // one whose used part holds `logical`.
        let used = match d[16] {
            0 => (start + u64_at(d, 37), u64_at(d, 45)),
            _ => (start, disk_bytes),
        };
        let holds = start != 0 && (used.0..used.0 + used.1).contains(&logical);
        holds.then(|| Extent {
            used,
            compression: match d[16] {
                0 => Compression::None,
                1 => Compression::Zlib,
                2 => Compression::Lzo,
                3 => Compression::Zstd,
                n => Compression::Other(n),
            },
            disk_bytes,
            ram_bytes: u64_at(d, 8),
            generation: u64_at(d, 0),
        })
    }))
}
/// Every path of `inode`, relative to the root of the subvolume `subvol` is open on.
pub fn ino_paths(subvol: RawFd, inode: u64) -> io::Result<Vec<String>> {
    let mut buf = vec![0u64; RESULT_BUF / 8];
    let mut args = PathArgs {
        input: inode,
        size: RESULT_BUF as u64,
        reserved: [0; 3],
        flags: 0,
        result: buf.as_mut_ptr() as u64,
    };
    ioctl(subvol, IOC_INO_PATHS, &mut args)?;
    let (_, offsets) = container(&buf);
    // SAFETY: viewing the u64 buffer as bytes.
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), RESULT_BUF) };
    let base = 16;
    Ok(offsets
        .iter()
        .map(|&off| c_str(&bytes[base + off as usize..]))
        .collect())
}

const ROOT_ITEM_KEY: u32 = 132;
const LAST_FREE_OBJECTID: u64 = -256i64 as u64;
const FS_INFO_FLAG_GENERATION: u64 = 1 << 1;

/// The filesystem's current transaction generation.
fn generation(fd: RawFd) -> io::Result<u64> {
    let mut fs = zeroed::<FsInfoArgs>();
    fs.data[48..56].copy_from_slice(&FS_INFO_FLAG_GENERATION.to_ne_bytes());
    ioctl(fd, IOC_FS_INFO, &mut *fs)?;
    match u64_at(&fs.data, 48) & FS_INFO_FLAG_GENERATION {
        0 => Err(io::Error::other("kernel reports no generation")),
        _ => Ok(u64_at(&fs.data, 56)),
    }
}

/// (generation, unix time) pairs btrfs records: every subvolume's creation and last
/// change, plus the current generation at `now`. Sorted, with any point whose time runs
/// backwards dropped, so interpolating between them is monotonic.
pub fn calibration(fd: RawFd, now: i64) -> Vec<(u64, i64)> {
    let roots = search(
        fd,
        ROOT_TREE,
        (TOP_LEVEL, ROOT_ITEM_KEY, 0),
        (LAST_FREE_OBJECTID, ROOT_ITEM_KEY, u64::MAX),
    )
    .unwrap_or_default();
    // btrfs_root_item: ctransid @295, otransid @303, ctime @327, otime @339 (packed).
    let recorded = roots
        .iter()
        .filter(|r| r.key_type == ROOT_ITEM_KEY && r.data.len() >= 351)
        .filter(|r| r.objectid == TOP_LEVEL || r.objectid >= FIRST_FREE_OBJECTID)
        .flat_map(|r| {
            let d = &r.data;
            [
                (u64_at(d, 303), u64_at(d, 339) as i64),
                (u64_at(d, 295), u64_at(d, 327) as i64),
            ]
        });
    let mut points: Vec<(u64, i64)> = recorded
        .chain(generation(fd).ok().map(|g| (g, now)))
        .filter(|&(g, t)| g > 0 && t > 0)
        .collect();
    points.sort();
    points.dedup_by_key(|p| p.0);
    let mut latest = i64::MIN;
    points.retain(|&(_, t)| {
        let keep = t >= latest;
        latest = latest.max(t);
        keep
    });
    points
}

const EXTENT_TREE: u64 = 2;
const EXTENT_ITEM_KEY: u32 = 168;
const METADATA_ITEM_KEY: u32 = 169;

/// Where the next allocated extent starts at or after `logical`, up to `end` (exclusive):
/// the end of the free space `logical` is in. None when nothing is allocated before `end`.
pub fn next_allocated(fd: RawFd, logical: u64, end: u64) -> io::Result<Option<u64>> {
    let mut from = (logical, 0, 0);
    let max = (end.saturating_sub(1), u32::MAX, u64::MAX);
    // The extent tree also holds backrefs (same key start) and block groups; skip those.
    loop {
        let page = search_page(fd, EXTENT_TREE, from, max, 64)?;
        if let Some(item) = page
            .iter()
            .find(|i| matches!(i.key_type, EXTENT_ITEM_KEY | METADATA_ITEM_KEY))
        {
            return Ok(Some(item.objectid));
        }
        let Some(last) = page.last() else {
            return Ok(None);
        };
        match next_key((last.objectid, last.key_type, last.offset)) {
            Some(k) if k <= max => from = k,
            _ => return Ok(None),
        }
    }
}
