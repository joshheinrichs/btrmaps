# btrmaps — intent

- See where btrfs space goes by laying the disk out on a Hilbert curve, like
  fzakaria's Nix closure visualizer, using btdu's approach to attribution.
- A native GUI app, not a CLI tool: run `btrmaps`, it finds the btrfs
  filesystems, you pick one, it asks for your password in its own window and
  scans.
- The scan runs as root so shared extents (snapshots, reflinks, hardlinks) are
  attributed to every file that holds them; the window never runs as root.
- Each color is one unique set of files an extent is attributed to.
- The curve follows the filesystem's own address order, not grouped by owner.
- Deterministic: one probe per cell on a fixed grid, not random sampling.
- Heatmaps beside the owner view: how shared each region is (by file count), and
  how compressed it is.
- Compression comes from what btrfs already records per extent — never
  estimated by reading and compressing data.
- An Age heatmap: when each extent was written.
- Feels like a normal app: a top bar with btrmaps and a filesystem dropdown on the
  left, modes in the middle, Refresh on the right.
- Few knobs: no detail selector (the scan picks it from the window size), no
  shading, full resolution always on, no treemap.
- The filesystem menu only selects; a big Scan button starts the scan. The only
  filesystem (or the one at /) is preselected.
- Long paths are shortened in the middle ("…"), keeping the file name.
- Right-click menus on files: copy path, show in folder, move to
  Trash, delete (confirmed). Nix-managed paths are left to garbage collection.
- The window stays responsive, even at the highest detail and mid-scan.
- Full resolution: zooming past the scan's detail refines what is on screen down
  to single blocks, filling in coarse to fine like progressive ray tracing,
  without leftovers from coarser levels showing through.
- Hovering the map never dims it, so panning and zooming stay readable. Click a
  set to spotlight it, or hold Shift to peek at whatever is under the pointer.
  Hovering a row in the list spotlights it too.
- Hovering or selecting free space doesn't spotlight it; empty space isn't a focus.
- Refresh re-runs the scan; the side pane is resizable.
- Resizing the window shows more or less of the map at the same scale; it never
  rescales it. Panning never rescans what was already looked at.
- An extremely simple architecture: a few very good decisions that make the whole
  thing efficient. Simple is fast.
- Performance matters: use every core, and the GPU where it fits. CPU use in
  proportion to what changed; nothing when idle.
- Like a web map: progressive 2× resolutions with sensible caching. Detail resolves
  continuously for what is visible; no separate scan or scan levels.
- A generic tool, its own open-source repo (MIT or Apache-2.0).
