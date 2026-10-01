//! Which btrfs filesystems are mounted, and where a scanned path lives on this machine.

use std::path::{Path, PathBuf};

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
    pub fn scan_target(&self) -> Option<PathBuf> {
        self.mounts
            .iter()
            .map(|m| m.path.clone())
            .min_by_key(|p| p.as_os_str().len())
    }

    /// How the filesystem is named: its label, else its device.
    pub fn name(&self) -> String {
        self.label.clone().unwrap_or_else(|| self.source.clone())
    }

    pub fn mounted_at(&self, path: &Path) -> bool {
        self.mounts.iter().any(|m| m.path == path)
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

fn usage(path: &Path) -> Option<(u64, u64)> {
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
pub fn discover() -> Vec<Filesystem> {
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

/// The filesystem to preselect: the only one, else the one mounted at /.
pub fn default_selection(filesystems: &[Filesystem]) -> Option<PathBuf> {
    match filesystems {
        [only] => only.scan_target(),
        _ => filesystems
            .iter()
            .find(|fs| fs.mounted_at(Path::new("/")))
            .and_then(Filesystem::scan_target),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_paths_map_to_where_they_are_mounted() {
        let m = |path: &str, subvolume: &str| Mount {
            path: path.into(),
            subvolume: subvolume.into(),
        };
        // Like a desktop: @ at /, the store bind-mounted read-only at /nix/store.
        let desk = [m("/", "/@"), m("/nix/store", "/@/nix/store")];
        assert_eq!(real_path(&desk, "@/home/me/x"), Some("/home/me/x".into()));
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
}
