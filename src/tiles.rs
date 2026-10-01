//! The map as a pyramid of tiles, like a web map: level z is the map 256·2^z pixels
//! across, cut into 256×256 tiles. Each tile is an aligned square, so on the Hilbert
//! curve it is one contiguous range of bytes: answers about those bytes are the only
//! thing that can change it.

use crate::atlas::{Answer, Atlas, deep_order};
use crate::hilbert::{d2xy, spread, xy2d};
use crate::palette::{NOTHING, pack};
use rayon::prelude::*;

pub const TILE: u64 = 256;
const TILE_ORDER: u32 = 8;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct Key {
    pub level: u32,
    pub x: u64,
    pub y: u64,
}

impl Key {
    /// Pixels across the whole map at this tile's level.
    pub fn res(self) -> u64 {
        TILE << self.level
    }

    pub fn parent(self) -> Option<Key> {
        (self.level > 0).then(|| Key {
            level: self.level - 1,
            x: self.x / 2,
            y: self.y / 2,
        })
    }

    /// Where the tile sits on the map, in map units (0..1 across): left, top, side.
    pub fn rect(self) -> (f64, f64, f64) {
        let side = 1.0 / (1u64 << self.level) as f64;
        (self.x as f64 * side, self.y as f64 * side, side)
    }
}

/// The part of the map on screen, in map units, and how many physical pixels the
/// whole map spans at the current zoom.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub u0: f64,
    pub v0: f64,
    pub u1: f64,
    pub v1: f64,
    pub across: f64,
}

/// One rendered tile: what each pixel shows (its own answer, else one borrowed from a
/// coarser pixel), which pixels have their own, and the pixels for the GPU, row by row.
pub struct Tile {
    pub key: Key,
    /// Tells renderings apart, so each is uploaded to the GPU once.
    pub id: u64,
    pub shown: Vec<Answer>,
    /// The pixels still waiting for their own answer, in the order to ask about them
    /// (spread over the tile, so it sharpens evenly).
    pub open: Vec<u16>,
    /// Each pixel as the GPU colors it (see `palette::pack`): data, not colors, so a
    /// change of mode or theme leaves tiles as they are.
    pub packed: Vec<u32>,
}

impl Tile {
    /// The set shown at map point (u, v), if this tile covers it.
    pub fn set_at(&self, u: f64, v: f64) -> Option<u32> {
        let (left, top, side) = self.key.rect();
        let (i, j) = ((u - left) / side, (v - top) / side);
        if !(0.0..1.0).contains(&i) || !(0.0..1.0).contains(&j) {
            return None;
        }
        let p = (j * TILE as f64) as usize * TILE as usize + (i * TILE as f64) as usize;
        Some(self.shown.get(p)?.known()?.set)
    }
}

/// The byte along the curve at map point (u, v): the middle of the block there.
pub fn position(total: u64, u: f64, v: f64) -> Option<u64> {
    let (order, block) = deep_order(total);
    let side = 1u64 << order;
    let inside = (0.0..1.0).contains(&u) && (0.0..1.0).contains(&v);
    let (x, y) = ((u * side as f64) as u64, (v * side as f64) as u64);
    let pos = xy2d(side, x.min(side - 1), y.min(side - 1)) * block + block / 2;
    (inside && pos < total).then_some(pos)
}

/// The level to show: the coarsest whose pixels are no bigger than the screen's at
/// this zoom, and no finer than blocks.
pub fn level_for(deep: u32, across: f64) -> u32 {
    let finest = finest(deep);
    (0..=finest)
        .find(|&z| (TILE << z) as f64 >= across)
        .unwrap_or(finest)
}

pub fn finest(deep: u32) -> u32 {
    deep.saturating_sub(TILE_ORDER)
}

/// The tiles of `level` overlapping the view.
pub fn visible(view: View, level: u32) -> Vec<Key> {
    let n = 1u64 << level;
    let at = |u: f64| ((u * n as f64).floor().max(0.0) as u64).min(n - 1);
    let (x0, x1, y0, y1) = (
        at(view.u0),
        at(view.u1 - 1e-9),
        at(view.v0),
        at(view.v1 - 1e-9),
    );
    (y0..=y1)
        .flat_map(|y| (x0..=x1).map(move |x| Key { level, x, y }))
        .collect()
}

/// Render a tile from what the atlas knows, borrowing what its pixels don't know yet
/// from the parent tile (at the top, from the sample grid's best guess).
pub fn render(atlas: &Atlas, cutoffs: &[u64; 5], key: Key, parent: Option<&Tile>, id: u64) -> Tile {
    let (res, total) = (key.res(), atlas.header.total);
    let n = (TILE * TILE) as usize;
    let (mut shown, mut settled) = (vec![Answer::NONE; n], vec![true; n]);
    shown
        .par_iter_mut()
        .zip(settled.par_iter_mut())
        .enumerate()
        .for_each(|(p, (shown, settled))| {
            let (i, j) = (p as u64 % TILE, p as u64 / TILE);
            let (x, y) = (key.x * TILE + i, key.y * TILE + j);
            if atlas.center(res, x, y) >= total {
                return;
            }
            if let Some(a) = atlas.own(res, x, y) {
                *shown = a;
                return;
            }
            *shown = match parent {
                Some(t) => {
                    let (pi, pj) = ((key.x % 2) * 128 + i / 2, (key.y % 2) * 128 + j / 2);
                    t.shown[(pj * TILE + pi) as usize]
                }
                None => {
                    let (gx, gy) = atlas.rep(res, x, y);
                    atlas.estimate(gx, gy).unwrap_or(Answer::NONE)
                }
            };
            // A pixel whose sample point lies past the end can't be answered; it keeps
            // what it borrows rather than being asked about forever.
            *settled = atlas.target(res, x, y) >= total;
        });
    let packed: Vec<u32> = shown
        .par_iter()
        .map(|a| a.known().map_or(NOTHING, |a| pack(a, cutoffs)))
        .collect();
    let open = (0..TILE * TILE)
        .map(|t| {
            let (i, j) = d2xy(TILE, spread(TILE_ORDER, t));
            (j * TILE + i) as u16
        })
        .filter(|&p| !settled[p as usize])
        .collect();
    Tile {
        key,
        id,
        shown,
        open,
        packed,
    }
}

/// Where to ask next, coarse to fine: for each level in turn, the pixels inside the view
/// without an answer of their own, spread over the level so it sharpens evenly and
/// taken from every tile in turn. Levels below 0 are the coarser maps above the top
/// tile (128 pixels across, 64, … 1), answered by the sample grid alone; they come
/// first when zoomed out, since that is what unanswered pixels borrow from.
pub fn wanted(atlas: &Atlas, levels: &[(i32, Vec<&Tile>)], view: View, up_to: usize) -> Vec<u64> {
    let total = atlas.header.total;
    let inside = |res: u64, x: u64, y: u64| {
        let (u, v) = ((x as f64 + 0.5) / res as f64, (y as f64 + 0.5) / res as f64);
        u >= view.u0 && u < view.u1 && v >= view.v0 && v < view.v1
    };
    let mut out = Vec::new();
    for (level, tiles) in levels {
        if *level < 0 {
            let bits = TILE_ORDER - level.unsigned_abs();
            let res = 1u64 << bits;
            for t in 0..res * res {
                let (x, y) = d2xy(res, spread(bits, t));
                let target = atlas.target(res, x, y);
                let open = atlas.own(res, x, y).is_none() && target < total;
                if open && inside(res, x, y) {
                    out.push(target);
                    if out.len() >= up_to {
                        return out;
                    }
                }
            }
            continue;
        }
        // Each tile's open pixels are listed in asking order; take from every tile in turn.
        // The list is only an order: coarser tiles aren't re-rendered as answers arrive,
        // so whether a pixel is still open is the atlas's to say.
        let longest = tiles.iter().map(|t| t.open.len()).max().unwrap_or(0);
        for k in 0..longest {
            for tile in tiles {
                let Some(&p) = tile.open.get(k) else { continue };
                let (res, key) = (tile.key.res(), tile.key);
                let (x, y) = (
                    key.x * TILE + p as u64 % TILE,
                    key.y * TILE + p as u64 / TILE,
                );
                if inside(res, x, y) && atlas.own(res, x, y).is_none() {
                    out.push(atlas.target(res, x, y));
                    if out.len() >= up_to {
                        return out;
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::tests::{atlas, header, run, runs, set};
    use crate::proto::{Kind, Run};

    fn whole() -> View {
        View {
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            across: 256.0,
        }
    }

    #[test]
    fn tiles_borrow_from_their_parent_until_their_own_answers_arrive() {
        // 16 GiB: the sample grid is level 3's pixels; level 4 is finer.
        let total = 1u64 << 34;
        let a = atlas([header(total), set(0, Kind::Free), set(1, Kind::Data)]);
        let top = Key {
            level: 0,
            x: 0,
            y: 0,
        };
        // The first thing a blank map asks about is the one pixel of the map one pixel
        // across; its answer fills the whole top tile.
        let first = wanted(&a, &[(-8, vec![])], whole(), 1);
        let a = a.apply(runs([run(first[0], 1, 0)]));
        let parent = render(&a, &[0; 5], top, None, 2);
        assert_eq!(
            parent.open.len(),
            (TILE * TILE) as usize,
            "no top pixel has its own"
        );
        assert!(
            parent.shown.iter().all(|s| s.set == 0),
            "one answer fills the tile"
        );
        // A child shows the parent's answers where it has none, its own where it has.
        let key = Key {
            level: 1,
            x: 1,
            y: 0,
        };
        let mine = a.target(key.res(), 300, 10);
        let a = a.apply(runs([run(mine, 1, 1)]));
        let child = render(&a, &[0; 5], key, Some(&parent), 3);
        let (u, v) = (300.5 / 512.0, 10.5 / 512.0);
        assert_eq!(child.set_at(u, v), Some(1));
        assert_eq!(child.set_at(u + 0.1, v + 0.1), Some(0), "borrowed");
        assert!(!child.open.contains(&((10 * TILE + 300 - TILE) as u16)));
    }

    #[test]
    fn requests_go_coarse_to_fine_reach_every_tile_and_stay_in_view() {
        let a = atlas([header(1 << 34), set(0, Kind::Free)]);
        let keys = visible(whole(), 1);
        assert_eq!(keys.len(), 4);
        let tiles: Vec<Tile> = keys
            .iter()
            .map(|&k| render(&a, &[0; 5], k, None, 1))
            .collect();
        let levels = [(-1, vec![]), (1, tiles.iter().collect())];
        let out = wanted(&a, &levels, whole(), 128 * 128 + 8);
        assert_eq!(out.len(), 128 * 128 + 8);
        let coarse: std::collections::HashSet<u64> = (0..128 * 128)
            .map(|p| a.target(128, p % 128, p / 128))
            .collect();
        assert!(
            out[..128 * 128].iter().all(|p| coarse.contains(p)),
            "coarser first"
        );
        let quarter = |p: &u64| (*p * 4 / (a.block << (2 * a.deep))) as usize;
        let quarters: std::collections::BTreeSet<usize> =
            out[128 * 128..].iter().map(quarter).collect();
        assert_eq!(quarters.len(), 4, "then every tile in turn");
        let left = View { u1: 0.5, ..whole() };
        let out = wanted(&a, &levels, left, 1024);
        assert!(
            out.iter().all(|p| {
                let (x, _) = d2xy(1 << a.deep, p / a.block);
                x < 1 << (a.deep - 1)
            }),
            "nothing off screen"
        );
    }

    #[test]
    fn answered_pixels_are_not_asked_again_through_a_coarser_tiles_stale_list() {
        // Coarser tiles are not re-rendered as answers arrive, so their open lists go
        // stale; what they still list must not crowd out the level being looked at.
        let a = atlas([header(1 << 34), set(0, Kind::Free)]);
        let coarse: Vec<Tile> = visible(whole(), 1)
            .iter()
            .map(|&k| render(&a, &[0; 5], k, None, 1))
            .collect();
        let res = Key {
            level: 1,
            x: 0,
            y: 0,
        }
        .res();
        let answers: Vec<Run> = (0..res * res)
            .map(|p| run(a.target(res, p % res, p / res), 1, 0))
            .collect();
        let a = a.apply(runs(answers));
        let fine: Vec<Tile> = visible(whole(), 2)
            .iter()
            .map(|&k| render(&a, &[0; 5], k, None, 2))
            .collect();
        let levels = [(1, coarse.iter().collect()), (2, fine.iter().collect())];
        let out = wanted(&a, &levels, whole(), 1024);
        assert_eq!(out.len(), 1024, "the finer level gets asked");
        assert!(
            out.iter().all(|&p| a.exact(p).is_none()),
            "nothing already answered"
        );
    }

    #[test]
    fn the_level_shown_matches_the_screen_down_to_blocks() {
        let a = atlas([header(1 << 34)]);
        assert_eq!(finest(a.deep), 3);
        assert_eq!(level_for(a.deep, 900.0), 2, "1024 pixels across covers 900");
        assert_eq!(level_for(a.deep, 100.0), 0);
        assert_eq!(level_for(a.deep, 1e9), 3, "no finer than blocks");
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::atlas::tests::{atlas, header};
    use crate::palette::{Colorer, Mode, Palette};
    use crate::proto::{Algo, Kind, Msg, Run, SetInfo};
    use std::time::Instant;

    fn time<T>(what: &str, f: impl FnOnce() -> T) -> T {
        let t = Instant::now();
        let out = f();
        eprintln!("{what:>32}: {:>8.2} ms", t.elapsed().as_secs_f64() * 1e3);
        out
    }

    /// 1 TiB answered by a million 1 MiB runs over half a million file sets.
    #[test]
    #[ignore]
    fn costs() {
        let (sets, total) = (500_000u32, 1u64 << 40);
        let mut msgs = vec![header(total)];
        msgs.extend((0..sets).map(|id| {
            Msg::Set(SetInfo {
                id,
                kind: Kind::Data,
                paths: vec![format!("@/home/d{}/e{}/file{id}", id % 97, id % 1013)],
                path_count: 1,
                files: 1 + (id % 3 == 0) as usize,
                subvolumes: 1,
                truncated: false,
                error: String::new(),
            })
        }));
        let a = time("build atlas", || atlas(msgs));
        let run = |i: u64| {
            let set = ((i as u32).wrapping_mul(2654435761) >> 8) % sets;
            Run(
                i << 20,
                1 << 20,
                set,
                Algo::Zstd,
                150 + (i % 400) as u32,
                1 + i % 900_000,
            )
        };
        let runs: Vec<Run> = (0..total >> 20).map(run).collect();
        let a = time("answer 1M runs", || a.apply(Msg::Runs { runs }));
        let palette = Palette::new(true);
        let cal = [(1, 1_700_000_000), (1_000_000, 1_790_000_000)];
        for mode in [Mode::Owners, Mode::Age] {
            let mut colorer = Colorer::new(&palette, mode, &cal);
            time(&format!("colors for 500k sets, {mode:?}"), || {
                colorer.extend(&a.sets)
            });
            time(&format!("side pane, {mode:?}"), || {
                crate::stats::side(&a, &palette, &colorer, mode)
            });
        }
        let mut colorer = Colorer::new(&palette, Mode::Owners, &cal);
        colorer.extend(&a.sets);
        let top = time("render the top tile", || {
            render(
                &a,
                colorer.cutoffs(),
                Key {
                    level: 0,
                    x: 0,
                    y: 0,
                },
                None,
                1,
            )
        });
        let finest = finest(a.deep);
        let mut parent = top;
        for level in 1..=finest {
            let key = Key { level, x: 0, y: 0 };
            let tile = time(&format!("render a level {level} tile"), || {
                render(&a, colorer.cutoffs(), key, Some(&parent), 1)
            });
            parent = tile;
        }
        let batch = Msg::Runs {
            runs: (0..1024).map(|i| run(i * 977)).collect(),
        };
        time("answer one batch of 1024", || a.apply(batch));
    }
}
