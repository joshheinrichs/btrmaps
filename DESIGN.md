# btrmaps — design

Two processes from one binary. `btrmaps` is the egui window (never root); it lists
the mounted btrfs filesystems from `/proc/self/mountinfo` (grouped by the
superblock's `major:minor`, which every subvolume mount shares), and on Scan runs
`sudo <itself> scan` — `sudo -n` if the password is still cached, otherwise
`sudo -S` with the password typed into btrmaps's own prompt. The root helper
streams JSON lines (`proto.rs`) on stdout; progress and sudo's complaints come
back on stderr and are shown as the status line.

- **Stopping a scan = dropping the channel.** The reader thread quits, the pipe
  closes, the helper gets EPIPE and exits. The helper is root, so the window
  can't signal it.
- **sudo, not pkexec/run0.** NixOS no longer installs the setuid pkexec
  wrapper by default, and run0 hangs until a polkit agent answers. sudo's
  path comes from `PATH` unless a packager bakes one in (`SUDO` at build time).
  Without a tty, sudo's timestamp is keyed on the parent process, so a Refresh
  within a few minutes doesn't ask again.
- **Coarse to fine.** Levels 6..=order each probe the whole curve; within a
  level cells go in bit-reversed Hilbert order so the picture sharpens evenly.
  Cell sizes nest exactly (`cell * 4^(order-level)`), so the viewer fills each
  coarse cell's fine range and finer levels overwrite. Costs ~⅓ extra probes.
- **Curve = btrfs logical address space**, chunks end to end (gaps skipped).
  Metadata/system chunks are labeled, not probed. The Hilbert mapping lives only
  in the viewer.
- **A set's identity is its sorted (root, inode) refs**, not paths: hardlinks
  are one inode, and grouping never needs paths. Sets are numbered in first-seen
  order and sent before the first cell that uses them; paths (up to 32) are
  resolved once per new set.
- **Probe outcomes follow btdu:** `ENOENT` from `LOGICAL_INO` = free; no refs at
  the exact offset but refs with `IGNORE_OFFSET` = unreachable.
- **Subvolume paths only resolve from the top level (subvolid=5).** Given any
  other subvolume, the helper unshares a mount namespace, mounts subvolid=5
  read-only on a temp dir, opens it, detaches and removes the dir — only the fd
  survives.
- **Compression is per extent, so per cell.** From LOGICAL_INO's first (inode,
  file offset, root), search that subvolume's EXTENT_DATA items in a 128 MiB
  window before the offset for the one whose disk range covers the probe;
  `ram_bytes / disk_num_bytes` is the ratio. Unreachable probes usually miss.
- **Treemap = the same samples, by directory** (`treemap.rs`). A set's bytes
  (sampled area × cell) sit at one node: its only path, else its `dominator` —
  the deepest directory holding every path, which the scanner computes from all
  paths, not just the 32 it sends. Nothing is counted twice. The tree is rebuilt
  at most every 500 ms and only while shown; layout is squarified, recursing
  until tiles hit 3 px or 30k tiles, and is recomputed only when the tree, the
  open directory or the rect changes. Fills are one mesh. Nodes too small to open
  draw whole in their largest set's color. Folder frames (outlines,
  padding and name strips) are off by default: any of them takes room from the
  contents and bends areas off scale. Without them tiles fill their folder
  exactly (unit-tested); hover outlines every enclosing folder on top instead, and
  the status bar spells out the chain. The open
  directory is kept as names,
  since node ids change on rebuild.
- **Age = the file extent item's `generation`**, read alongside compression.
  Dates come from calibration points btrfs records: every subvolume root item's
  (otransid, otime) and (ctransid, ctime) at packed offsets 303/339 and 295/327,
  plus the current generation (`FS_INFO` with the generation flag) at scan
  time; interpolated linearly, clamped at the ends. Approximate by design.
- **Side pane layout is flexbox** (`egui_taffy`, i.e. taffy under egui), Clay-style
  rows and columns: the mode switch is equal-basis grow items, legend chips wrap
  as whole units. Buttons and chip labels inside taffy must use
  `TextWrapMode::Extend`, or taffy sizes them to their min-content width and
  egui wraps the text one letter per line.
- **Scan paths vs real paths.** Paths are relative to the top-level subvolume
  (`@/home/x`); menus act on files where they are mounted here: the mount whose
  subvolume (mountinfo field 4) is the longest prefix wins, e.g. `/@` at `/`
  gives `/home/x`. Unmounted subvolumes (snapshots) and `/nix/store` are not
  offered for deletion. Trash and delete run as the user, not root.
- **Clicks under menus:** egui delivers the click on a menu item to the widget
  beneath too, so map, treemap and list ignore clicks while any popup is open.
- egui dlopens Wayland/xkbcommon/GL, so the derivation adds them to the rpath.
- Test: `nix-build -A tests.e2e` — NixOS VM with a real btrfs disk
  (snapshot, reflink, hardlink, overwrite, zstd file). Asserts the helper's
  stream (ordering, coverage, per-set sizes, sharing, compression, dominators,
  generations growing in write order, calibration), then runs the app under
  sway: Scan the preselected disk, type the password (ydotool),
  check sudo ran the scan, and screenshot curve, deep zoom, treemap, hover and delete.
- **Costs at 2048² (4M cells, 500k sets), release build.** Measured by the ignored
  `bench::costs_at_2048` test (`cargo test --release costs_at_2048 -- --ignored
  --nocapture`): totals ~37 ms, legend ~35 ms, base colors 30–50 ms, hover dim
  ~9 ms, tree build ~240 ms, layout <1 ms. So the rules: nothing proportional to
  cells runs per frame; hover only re-dims cached base colors; base colors come from
  a per-set color table; streaming catch-ups run every 500 ms; the tree rebuilds only
  when a scan level completes. Tree interning keys on borrowed names with FxHash, and
  squarify keeps running row totals (it was quadratic in a folder's entries).
- **Three threads, one direction.** Readers parse the helper's stdout and stderr and
  feed a model thread, which owns the `Model` and everything derived from it (totals,
  list, legend, base colors, the treemap's tree) and sends the window an
  immutable `Snapshot` when one is due: at once for a new look (mode/theme) or
  the end of a scan, else at most every 500 ms. The window moves the latest snapshot
  in and only dims the focused set and lays out treemap tiles. Sets are
  `Arc<SetInfo>` so a snapshot copies pointers, not paths. The helper runs at nice 19,
  idle I/O, on all cores but one, so the window always gets CPU.
- **Top bar is placed directly, not by flexbox.** Three groups with measured widths:
  left and right laid out from their edges, the middle centered and slid aside when
  space is tight. Controls that still don't fit go into a "…" menu, least important
  first (frames, view, refresh, modes; frames only in the treemap), judged from
  last frame's measured widths with a little hysteresis. Inside egui_taffy the
  middle region came out narrower than its content and clipped its last buttons,
  and squeezed widths fed back into the fit decision.
- **Full resolution = probes on demand, answered with runs.** After the sampled scan
  the helper keeps reading `{"t":"probe","positions":[…]}` lines on stdin (the pipe
  sudo took the password from) until the window closes it; requests jump ahead of
  scan batches. Each position is widened to what is known to share it: the used
  part of its extent (offset/num_bytes of the file extent item whose range holds
  it; the whole extent if compressed — widening an uncompressed one to the whole
  extent painted other files' bytes in its color), free space up to the next
  EXTENT_ITEM/METADATA_ITEM in the extent tree, or the whole chunk for metadata.
  Answers are `Runs` in bytes along the curve. The model thread keeps them as
  non-overlapping runs (newest wins), renders the viewport at a finer curve order
  (smallest with cells ≤ 4 KiB; finer Hilbert orders trace coarser ones, tested),
  and asks next for unknown pixels every 16th, 8th, … pixel, two batches in flight.
  Unprobed pixels borrow the nearest coarser probe (16, 8, … lattice), else the
  scan's sample of that spot (same curve, so the scan is just the coarsest pass):
  one image that only sharpens. Samples showing through transparent holes read as
  layered artifacts; dimming them flashed the map and fought Age/Compression, where
  brightness is data. The scan's order comes from the map's size in physical pixels.
