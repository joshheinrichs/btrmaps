# btrmaps

A map of where the space on a btrfs filesystem goes, drawn along a Hilbert curve
in disk order.

Every color is one exact set of files owning those bytes, so snapshots, reflinks
and hardlinks show up as shared regions instead of being counted twice. Other
modes color the same map by how widely each region is shared, how well it
compresses (from what btrfs already records per extent), and how old it is.

Zooming refines what is on screen down to single blocks, filling in coarse to
fine.

## Install

Linux only (btrfs), on Wayland or X11, with `sudo`.

- **Release binaries** for x86_64 and aarch64 are on the releases page; they need
  glibc 2.35 or newer.
- **Nix:** `nix-build` builds `result/bin/btrmaps`. The expression takes
  `pkgs` if you want to build against your own nixpkgs.
- **Cargo:** `cargo install --path .`

## Usage

```
btrmaps
```

Pick a filesystem, press Scan and enter your password. The window never runs as
root: it starts `btrmaps scan` through `sudo` to read the filesystem's trees and
streams the results back.

## How it works

The scan samples the filesystem at evenly spaced points along the curve and asks
btrfs which files own the bytes at each point. Detail grows coarse to fine, so a
rough picture appears at once and sharpens as the scan runs. Past the scan's
detail, the window asks the root helper about exactly what is on screen and draws
the answers in place.

## Credits

- **[btdu](https://github.com/CyberShadow/btdu)** by Vladimir Panteleev, the
  sampling disk usage profiler for btrfs. btrmaps borrows its core idea: pick a
  point on the disk, ask btrfs what is there (`LOGICAL_INO`), and attribute
  shared extents to every file that owns them, including unreachable and
  compressed space. btdu samples at random; btrmaps samples a fixed grid along
  the curve so the picture is deterministic and spatial.
- **[Visualizing Nix closures](https://fzakaria.com/2026/09/15/visualizing-nix-closures)**
  by Farid Zakaria ([seenix](https://github.com/fzakaria/seenix)), which lays the
  bytes of Nix closures out on a Hilbert curve. That post is where the idea of
  mapping btrfs space the same way came from.
- **[Visualising binaries](https://corte.si/posts/visualisation/binvis/)** by
  Aldo Cortesi ([binvis.io](https://binvis.io)), the earlier work behind using
  Hilbert curves to keep neighbouring bytes neighbours on screen, which the seenix
  post credits in turn.

## Development

`cargo test` runs the unit tests. `nix-build -A tests.e2e` runs the end-to-end
test: a NixOS VM with a real btrfs disk (snapshots, reflinks, hardlinks,
compression) that checks the scan's output and then drives the app under sway,
leaving screenshots in `result`. CI runs both; pushing a `v*` tag matching
`Cargo.toml`'s version builds release binaries.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in btrmaps by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
