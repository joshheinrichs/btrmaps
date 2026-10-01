# btrmaps — design

Two processes from one binary. `btrmaps` is the egui window (never root).
`btrmaps scan` is the root helper: a probe server. It sends a header (curve length,
generation dates), then answers each line of positions on stdin with the sets it
hasn't announced yet and one run per position, as JSON lines (`proto.rs`).

```
helper (root) ──stdout──▶ reader threads ──▶ model thread ──Snapshot──▶ window ──▶ GPU
             ◀──stdin── probe requests ◀──┘ (atlas, tiles)  ◀─Look/View/Inspect─┘
```

- **One kind of answer: runs.** A probe answers with everything known to share the
  position: the used part of its extent (offset/num_bytes of the file extent item
  holding it; the whole extent if compressed — widening an uncompressed one painted
  other files' bytes in its color), free space up to the next extent item, or the
  whole chunk for metadata. One answer often fills many pixels.
- **Atlas** (`atlas.rs`): exact runs (BTreeMap, newest wins) and a sample grid, the
  pixels of the map 2048 across (or block level if coarser), filled by any run
  covering a point's middle. "What is at this byte": exact, else the grid's guess,
  which borrows from the nearest answered point standing for a coarser pixel.
- **Tile pyramid** (`tiles.rs`), like a web map. Level z is the map 256·2^z pixels
  across in 256² tiles, down to block level. An aligned square on the Hilbert curve
  is one contiguous byte range (tested), so a tile changes only when a run overlaps
  its range; others stay cached (128 tiles). A tile renders from its own answers
  (grid points up to the grid's resolution, exact runs below it) and borrows the
  rest from its parent tile: one picture that only sharpens. ~0.5 ms a tile.
- **The window shows the level matching the zoom**, falling back per tile to the
  nearest ancestor it has. Panning and zooming render only tiles new to the view.
- **What to ask** (`tiles::wanted`): the view's pixels without their own answer,
  from 4 levels coarser than the shown one down to it (levels above the top tile are
  coarser maps answered by the grid alone), spread within a level and round-robin
  across tiles, so the view sharpens evenly. Batches of 1024, 8 in flight; a quarter
  of each batch goes to a coarse-to-fine sweep of the whole grid, which the side
  pane's sizes are estimated from.
- **The GPU colors; tiles hold data** (`gpu.rs`). A tile pixel is 32 bits: set id
  and compression and age codes (`palette::pack`). The shader looks the set's color
  up in a table (transparent = color by the pixel's code, from a 16-entry ramp) and
  fades every set but the focused one. A new mode or theme sends a new table; the
  table only grows as sets arrive, so otherwise only new rows go.
- **A virtual texture, one draw.** Tiles live in a fixed 4096² atlas of 256 slots,
  allocated once and evicted least recently used. A 64×64 page table maps each
  on-screen tile of the shown level to its slot, or to its nearest resident
  ancestor and how many levels up that is; the shader scales the ancestor up. So a
  tile not yet rendered or uploaded shows its parent, and the frame never waits.
  The map is one quad drawn once, whatever the zoom.
- **Uploads are whole tiles, at most 1 MB a frame** (four 256 KB tiles, more than
  the model thread renders); the rest wait for the next frame. Diffing tiles and
  pixel buffer objects were tried and bought nothing measurable.
- **The window does no heavy work.** It holds the latest snapshot only (tiles are
  shared `Arc`s, so dropping one is cheap). The model thread and its rayon pool
  (all cores but one) run at nice 10, so the UI thread always gets a core.
- **Higher levels only lend pixels.** The worker renders every level down to the
  shown one (each borrows from its parent), but only stale tiles of the shown level
  re-render on answers; the level above at most every 500 ms (they span huge byte
  ranges, so every answer touches them). So coarser tiles' open lists go stale:
  `wanted` checks each pixel against the atlas, and the list only gives the order —
  trusting it starved the shown level and re-asked answered pixels forever. egui clamps a paint callback's viewport to
  the screen and rounds it to pixels, so only the on-screen rect is passed, snapped,
  with matching corners — otherwise the map squashes or swims.
- **The model thread does work in proportion to change.** Tiles: at once for a new
  view or look, at most every 50 ms for answers. Side pane (one dense pass over the
  grid, ~30 ms): at most every 500 ms. Nothing when no answers arrive.
- **Colors per mode** (`Colorer`): a color per set where the set decides it, else by
  the answer's compression ratio or age; extended as sets arrive. Age buckets are
  converted to generation cutoffs once (dates only grow with generations).
- **Curve = btrfs logical address space**, chunks end to end (gaps skipped).
  Metadata/system chunks are labeled, not probed.
- **A set's identity is its sorted (root, inode) refs**, not paths: hardlinks
  are one inode. Sets are numbered in first-seen order and sent before first use;
  paths (up to 32) are resolved once per new set.
- **Probe outcomes follow btdu:** `ENOENT` from `LOGICAL_INO` = free; no refs at
  the exact offset but refs with `IGNORE_OFFSET` = unreachable.
- **Subvolume paths only resolve from the top level (subvolid=5).** Given another
  subvolume, the helper unshares a mount namespace, mounts subvolid=5 read-only on
  a temp dir, opens it, detaches and removes the dir.
- **Compression and age are per extent,** from the file extent item: ratio =
  `ram_bytes / disk_num_bytes`, age = its `generation`, dated by interpolating
  the (generation, time) pairs btrfs records in root items (otransid/otime at
  303/339, ctransid/ctime at 295/327) and `FS_INFO`'s current generation.
- **Elevation** (`elevate.rs`): already root → run the helper directly. Otherwise
  sudo: with a password, `sudo -S -v` checks it alone (stdin closed after one
  line, so a wrong password fails at once instead of sudo waiting for a retry on
  the pipe), then `sudo -n` starts the helper on the cached credentials. Without a
  tty, sudo's timestamp is keyed on the parent process. All of it runs on the
  model thread; the window never blocks.
- **Stopping = dropping the channels.** Readers quit, pipes close, the helper
  exits on EPIPE. It is root, so it can't be signalled.
- **Helper priority:** nice 19, idle I/O, all cores but one, so it gives way to
  everything else, even when that leaves it starved under heavy load.
- **Scan paths vs real paths.** Paths are relative to the top-level subvolume;
  menus act on files where they are mounted here (longest subvolume prefix wins).
  Unmounted subvolumes and `/nix/store` are not offered for deletion. Trash and
  delete run as the user, on a background thread.
- **Clicks under menus:** egui delivers a menu item's click to the widget beneath
  too, so map and list ignore clicks while a popup is open.
- **Portability:** `SUDO` and `XDG_OPEN` are baked in when set at build time,
  else found on PATH. egui dlopens Wayland, X11, xkbcommon and GL; the Nix build
  adds them to the rpath. The shader picks GLSL 1.20/1.40/ES 1.00/3.00 at runtime.
- **Costs** (1 TiB, a million runs, 500k sets, release, 16 cores;
  `cargo test --release costs -- --ignored --nocapture`): a tile ~0.5 ms at any
  level, a batch of 1024 answers ~1 ms, side pane ~33 ms, colors for 500k sets
  ~3 ms.
- **Frame timing:** `BTRMAPS_FRAMES=1` redraws every refresh and prints dropped
  frames, moving and still, every 2 s. egui only draws on input, so without the
  constant redraw a pause in the hand reads as a stall. Drops while the app is idle
  are the desktop's: compare with `vkgears` before blaming the app.
- Test: `nix-build -A tests.e2e` — NixOS VM with a real btrfs disk (snapshot,
  reflink, hardlink, overwrite, zstd file). Probes an even grid through the helper
  and checks sizes, sharing, compression, generations and runs, then drives the app
  under sway: a wrong password then the right one, zoom, resize, hover, delete from
  the menu, with screenshots.
