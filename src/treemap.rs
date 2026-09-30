//! A directory treemap built from the curve's samples.
//!
//! Every set's bytes sit at its dominator: a file's own path when one path references
//! the extent, otherwise the deepest directory holding every path that does. Deleting
//! anything below that directory leaves the bytes in use, so that is where they are
//! freed, and nothing is counted twice.

use crate::proto::{Kind, SetInfo};
use rustc_hash::FxHashMap as HashMap;
use std::borrow::Borrow;

pub const ROOT: u32 = 0;

pub struct Node {
    pub name: String,
    pub parent: Option<u32>,
    /// Largest first.
    pub children: Vec<u32>,
    /// Sets whose bytes sit exactly here, largest first.
    pub pieces: Vec<(u32, u64)>,
    /// Bytes here and below.
    pub size: u64,
    /// The largest single piece here or below; stands in when the node is drawn whole.
    pub top: Option<(u32, u64)>,
}

pub struct Tree {
    pub nodes: Vec<Node>,
}

/// Where a set's bytes belong in the tree, as path components.
pub fn home(set: &SetInfo) -> impl Iterator<Item = &str> {
    let path = match set.kind {
        Kind::Data | Kind::Unreachable if set.path_count == 1 => set.paths[0].as_str(),
        Kind::Data | Kind::Unreachable => set.dominator.as_str(),
        _ => "",
    };
    path.split('/').filter(|c| !c.is_empty())
}

/// Build the tree, given each set's size in bytes.
pub fn build<S: Borrow<SetInfo>>(root: &str, sets: &[S], bytes: &[u64]) -> Tree {
    let mut nodes = vec![Node {
        name: root.to_string(),
        parent: None,
        children: Vec::new(),
        pieces: Vec::new(),
        size: 0,
        top: None,
    }];
    // Keyed by names borrowed from the sets: only a new node allocates its name.
    let mut index: HashMap<(u32, &str), u32> = HashMap::default();
    for (set, &size) in sets.iter().zip(bytes) {
        let set = set.borrow();
        if size == 0 || set.kind == Kind::Past {
            continue;
        }
        let mut at = ROOT;
        for part in home(set) {
            at = *index.entry((at, part)).or_insert_with(|| {
                nodes.push(Node {
                    name: part.to_string(),
                    parent: Some(at),
                    children: Vec::new(),
                    pieces: Vec::new(),
                    size: 0,
                    top: None,
                });
                let id = (nodes.len() - 1) as u32;
                nodes[at as usize].children.push(id);
                id
            });
        }
        nodes[at as usize].pieces.push((set.id, size));
    }
    // Children always come after their parent, so one backwards pass sums bottom-up.
    for i in (0..nodes.len()).rev() {
        let own: u64 = nodes[i].pieces.iter().map(|p| p.1).sum();
        let largest = nodes[i].pieces.iter().copied().max_by_key(|p| p.1);
        let node = &mut nodes[i];
        node.size += own;
        node.top = match (node.top, largest) {
            (Some(a), Some(b)) => Some(if b.1 > a.1 { b } else { a }),
            (a, b) => a.or(b),
        };
        let (size, top) = (node.size, node.top);
        if let Some(p) = node.parent {
            let parent = &mut nodes[p as usize];
            parent.size += size;
            parent.top = match (parent.top, top) {
                (Some(a), Some(b)) => Some(if b.1 > a.1 { b } else { a }),
                (a, b) => a.or(b),
            };
        }
    }
    let sizes: Vec<u64> = nodes.iter().map(|n| n.size).collect();
    for node in &mut nodes {
        node.children
            .sort_by_key(|&c| std::cmp::Reverse(sizes[c as usize]));
        node.pieces.sort_by_key(|p| std::cmp::Reverse(p.1));
    }
    Tree { nodes }
}

impl Tree {
    /// Component names from the root down to `node`, excluding the root.
    pub fn path(&self, node: u32) -> Vec<u32> {
        let mut chain = vec![node];
        while let Some(p) = self.nodes[*chain.last().unwrap() as usize].parent {
            chain.push(p);
        }
        chain.pop();
        chain.reverse();
        chain
    }

    /// Follow component names down from the root, as far as they exist.
    pub fn find(&self, names: &[String]) -> u32 {
        let mut at = ROOT;
        for name in names {
            let next = self.nodes[at as usize]
                .children
                .iter()
                .find(|&&c| self.nodes[c as usize].name == *name);
            match next {
                Some(&c) => at = c,
                None => break,
            }
        }
        at
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TileKind {
    /// A directory drawn with its contents inside.
    Frame,
    /// One set's bytes.
    Piece,
    /// A node too small to open, drawn whole in its largest set's color.
    Collapsed,
}

#[derive(Clone, Copy, Debug)]
pub struct Tile {
    pub rect: Rect,
    pub kind: TileKind,
    pub node: u32,
    pub set: Option<u32>,
    pub bytes: u64,
}

/// Room kept above a frame's contents for its name, when the frame is tall enough.
pub const LABEL: f32 = 15.0;
const PAD: f32 = 2.0;

/// Squarified layout of `node`'s contents into `rect`, recursing into directories
/// until tiles get smaller than `min` pixels or `budget` tiles are used. With `frames`,
/// directories inset their contents and tall ones keep a strip above them for their
/// name; without, contents fill their directory exactly, so every area is to scale.
pub fn layout(
    tree: &Tree,
    node: u32,
    rect: Rect,
    min: f32,
    budget: usize,
    frames: bool,
) -> Vec<Tile> {
    let mut tiles = Vec::new();
    fill(tree, node, rect, (min, budget, frames), &mut tiles);
    tiles
}

#[derive(Clone, Copy)]
enum Item {
    Child(u32),
    Piece(u32, u64),
}

fn fill(tree: &Tree, node: u32, rect: Rect, limits: (f32, usize, bool), tiles: &mut Vec<Tile>) {
    let (min, budget, frames) = limits;
    let n = &tree.nodes[node as usize];
    let mut items: Vec<(u64, Item)> = n
        .children
        .iter()
        .map(|&c| (tree.nodes[c as usize].size, Item::Child(c)))
        .chain(n.pieces.iter().map(|&(s, b)| (b, Item::Piece(s, b))))
        .filter(|(size, _)| *size > 0)
        .collect();
    items.sort_by_key(|(size, _)| std::cmp::Reverse(*size));
    for (r, item) in squarify(&items, rect) {
        if tiles.len() >= budget {
            return;
        }
        match item {
            Item::Piece(set, bytes) => tiles.push(Tile {
                rect: r,
                kind: TileKind::Piece,
                node,
                set: Some(set),
                bytes,
            }),
            Item::Child(c) => {
                let child = &tree.nodes[c as usize];
                let opens = r.w >= 4.0 * min && r.h >= 4.0 * min;
                // A single file with one piece is just that piece.
                let single = child.children.is_empty() && child.pieces.len() == 1;
                if single {
                    let (set, bytes) = child.pieces[0];
                    tiles.push(Tile {
                        rect: r,
                        kind: TileKind::Piece,
                        node: c,
                        set: Some(set),
                        bytes,
                    });
                } else if opens {
                    tiles.push(Tile {
                        rect: r,
                        kind: TileKind::Frame,
                        node: c,
                        set: None,
                        bytes: child.size,
                    });
                    let (pad, label) = match frames {
                        true => (PAD, if r.h > 3.0 * LABEL { LABEL } else { 0.0 }),
                        false => (0.0, 0.0),
                    };
                    let inner = Rect {
                        x: r.x + pad,
                        y: r.y + pad + label,
                        w: r.w - 2.0 * pad,
                        h: r.h - 2.0 * pad - label,
                    };
                    if inner.w > min && inner.h > min {
                        fill(tree, c, inner, limits, tiles);
                    }
                } else {
                    tiles.push(Tile {
                        rect: r,
                        kind: TileKind::Collapsed,
                        node: c,
                        set: child.top.map(|t| t.0),
                        bytes: child.size,
                    });
                }
            }
        }
    }
}

/// Bruls et al.'s squarified treemap: fill rows along the shorter side, adding items
/// while that improves the row's worst aspect ratio. `items` are largest first.
fn squarify<T: Copy>(items: &[(u64, T)], rect: Rect) -> Vec<(Rect, T)> {
    let total: f64 = items.iter().map(|i| i.0 as f64).sum();
    if total <= 0.0 || rect.w <= 0.0 || rect.h <= 0.0 {
        return Vec::new();
    }
    let scale = (rect.w as f64 * rect.h as f64) / total;
    let areas: Vec<f64> = items.iter().map(|i| i.0 as f64 * scale).collect();
    let mut out = Vec::with_capacity(items.len());
    let mut free = rect;
    let mut start = 0;
    while start < items.len() {
        let side = free.w.min(free.h) as f64;
        // Worst aspect ratio of a row from its sum and extremes. Items come largest
        // first, so the row's max is its first item and its min the one just added:
        // running totals, rather than rescanning the row for every candidate.
        let worst = |sum: f64, max: f64, min: f64| {
            (side * side * max / (sum * sum)).max(sum * sum / (side * side * min))
        };
        let max = areas[start];
        let (mut end, mut row_area) = (start + 1, areas[start]);
        while end < items.len() {
            let grown = row_area + areas[end];
            if worst(grown, max, areas[end]) > worst(row_area, max, areas[end - 1]) {
                break;
            }
            row_area = grown;
            end += 1;
        }
        let thick = (row_area / side) as f32;
        let mut along = 0.0f32;
        for i in start..end {
            let len = (areas[i] / row_area * side) as f32;
            let r = match free.w >= free.h {
                // Column on the left.
                true => Rect {
                    x: free.x,
                    y: free.y + along,
                    w: thick,
                    h: len,
                },
                // Row on top.
                false => Rect {
                    x: free.x + along,
                    y: free.y,
                    w: len,
                    h: thick,
                },
            };
            out.push((r, items[i].1));
            along += len;
        }
        free = match free.w >= free.h {
            true => Rect {
                x: free.x + thick,
                w: free.w - thick,
                ..free
            },
            false => Rect {
                y: free.y + thick,
                h: free.h - thick,
                ..free
            },
        };
        start = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(id: u32, kind: Kind, paths: &[&str], dominator: &str) -> SetInfo {
        SetInfo {
            id,
            kind,
            paths: paths.iter().map(|p| p.to_string()).collect(),
            path_count: paths.len(),
            files: paths.len(),
            subvolumes: 1,
            truncated: false,
            error: String::new(),
            dominator: dominator.to_string(),
        }
    }

    #[test]
    fn shared_bytes_sit_at_the_dominator_and_totals_add_up() {
        let sets = [
            set(0, Kind::Free, &[], ""),
            set(1, Kind::Data, &["@/home/a"], "@/home/a"),
            set(2, Kind::Data, &["@/home/a", "@/home/b"], "@/home"),
            set(3, Kind::Data, &["@/x", "@snap/x"], ""),
        ];
        let tree = build("/", &sets, &[100, 10, 20, 40]);
        let at = |names: &[&str]| {
            let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            &tree.nodes[tree.find(&names) as usize]
        };
        assert_eq!(tree.nodes[ROOT as usize].size, 170);
        assert_eq!(tree.nodes[ROOT as usize].pieces, [(0, 100), (3, 40)]);
        assert_eq!(at(&["@", "home"]).pieces, [(2, 20)]);
        assert_eq!(at(&["@", "home"]).size, 30);
        assert_eq!(at(&["@", "home", "a"]).pieces, [(1, 10)]);
        assert_eq!(tree.nodes[ROOT as usize].top, Some((0, 100)));
    }

    #[test]
    fn without_frames_every_tile_is_to_scale() {
        let sets = [
            set(0, Kind::Free, &[], ""),
            set(1, Kind::Data, &["@/home/a"], "@/home/a"),
            set(2, Kind::Data, &["@/home/b"], "@/home/b"),
            set(3, Kind::Data, &["@/home/a", "@/home/b"], "@/home"),
            set(4, Kind::Data, &["@/nix/x"], "@/nix/x"),
            set(5, Kind::Data, &["@/nix/y"], "@/nix/y"),
        ];
        let bytes = [500, 120, 80, 60, 150, 90];
        let tree = build("/", &sets, &bytes);
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            w: 400.0,
            h: 300.0,
        };
        let tiles = layout(&tree, ROOT, rect, 1.0, 1000, false);
        let total: u64 = bytes.iter().sum();
        let pieces: Vec<_> = tiles.iter().filter(|t| t.kind == TileKind::Piece).collect();
        assert_eq!(pieces.len(), sets.len());
        for t in pieces {
            let want = t.bytes as f32 / total as f32 * rect.w * rect.h;
            assert!(
                (t.rect.w * t.rect.h - want).abs() < 1.0,
                "{t:?} should cover {want}"
            );
        }
    }
    #[test]
    fn squarified_tiles_cover_the_rect_in_proportion_without_overlap() {
        let items: Vec<(u64, usize)> = [60u64, 30, 25, 10, 5, 3, 1]
            .iter()
            .copied()
            .zip(0..)
            .collect();
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            w: 300.0,
            h: 200.0,
        };
        let tiles = squarify(&items, rect);
        assert_eq!(tiles.len(), items.len());
        let total: u64 = items.iter().map(|i| i.0).sum();
        for (r, i) in &tiles {
            let want = items[*i].0 as f32 / total as f32 * rect.w * rect.h;
            assert!((r.w * r.h - want).abs() < 1.0, "{r:?} for {}", items[*i].0);
        }
        for (a, (ra, _)) in tiles.iter().enumerate() {
            for (rb, _) in &tiles[a + 1..] {
                let ox = (ra.x + ra.w).min(rb.x + rb.w) - ra.x.max(rb.x);
                let oy = (ra.y + ra.h).min(rb.y + rb.h) - ra.y.max(rb.y);
                assert!(ox < 0.01 || oy < 0.01, "{ra:?} overlaps {rb:?}");
            }
        }
    }
}
