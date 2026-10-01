//! The model thread: owns the atlas and the tile cache, decides what to ask the helper
//! next, and hands the window finished snapshots. The window never waits on it.

use crate::atlas::{self, Answer, Atlas};
use crate::elevate;
use crate::hilbert::{d2xy, spread};
use crate::palette::{Colorer, GpuColors, Mode, Palette};
use crate::proto::{Header, Msg, Request};
use crate::stats::{self, Side};
use crate::tiles::{self, Key, Tile, View};
use eframe::egui;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ChildStdin;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

/// How often totals and the list catch up while answers stream in.
const SIDE_EVERY: Duration = Duration::from_millis(500);
/// How often changed tiles are rendered again while answers stream in.
const TILES_EVERY: Duration = Duration::from_millis(50);
/// How often tiles follow a moving view: about a frame, so a burst of pan and zoom
/// events makes one pass, not one each.
const MOVE_EVERY: Duration = Duration::from_millis(8);
/// Positions per probe request, and requests in flight.
const PROBE_BATCH: usize = 1024;
const IN_FLIGHT: usize = 8;
/// Positions remembered as asked for, across pans and zooms.
const ASKED_MAX: usize = 1 << 20;
/// Most positions queued for the visible part at a time.
const WANTED: usize = 8192;
/// Levels coarser than the shown one that are asked about first, so the view sharpens
/// from a 16× coarser picture.
const LEAD: i32 = 4;
/// Tiles kept rendered, about 1.3 MB each.
const CACHE: usize = 128;
/// Inputs taken in one go before refreshing, so a fast stream can't starve the window.
const DRAIN: usize = 1024;

/// The filesystem to look at.
#[derive(Clone, Debug)]
pub struct Scan {
    pub target: PathBuf,
}

/// What the window shows, as far as derived state depends on it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Look {
    pub mode: Mode,
    pub dark: bool,
}

/// What the scan helper reports.
pub enum Event {
    Msg(Result<Msg, String>),
    /// A progress or error line from its stderr.
    Status(String),
    /// sudo turned the password down, with its reason.
    Refused(String),
    Ended(Result<(), String>),
}

pub enum Input {
    Event(Event),
    Look(Look),
    /// The part of the map on screen, when the map is.
    View(Option<View>),
    /// A position to report the full answer for: the one under the pointer.
    Inspect(Option<u64>),
}

/// Everything the window draws from, handed over whole.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub header: Option<Arc<Header>>,
    pub side: Arc<Side>,
    /// The tiles covering the view, coarsest first: each level is drawn over the one
    /// before, so a tile not rendered yet shows its ancestor scaled up.
    pub tiles: Vec<Arc<Tile>>,
    /// What the GPU colors their pixels by.
    pub colors: Arc<GpuColors>,
    /// Pixels of the shown level still waiting for their own answer, and whether that
    /// level is the finest there is.
    pub refining: usize,
    pub finest: bool,
    pub inspected: Option<(u64, Answer)>,
    pub status: String,
    pub refused: Option<String>,
    pub ended: Option<Result<(), String>>,
    pub error: Option<String>,
}

/// The model thread's state.
struct Worker {
    atlas: Option<Atlas>,
    look: Look,
    view: Option<View>,
    inspect: Option<u64>,
    /// Bumped whenever the atlas changes.
    data: u64,
    colorer: Option<(Look, Colorer)>,
    snap: Snapshot,
    side_for: Option<(Look, u64)>,
    inspected_for: Option<(Option<u64>, u64)>,
    /// Rendered tiles, when each was last needed, and the byte ranges answered since
    /// the tiles were last brought up to date.
    cache: HashMap<Key, Arc<Tile>>,
    used: HashMap<Key, u64>,
    answered: Vec<(u64, u64)>,
    /// Cached tiles answers have changed since they were rendered.
    stale: HashSet<Key>,
    last_fallback: Instant,
    /// Pass timings, when BTRMAPS_FRAMES is set.
    stats: Option<PassStats>,
    tiles_for: Option<(Look, Option<View>)>,
    passes: u64,
    renders: u64,
    last_side: Instant,
    last_tiles: Instant,
    last_sent: Instant,
    /// Something to tell the window at once (an end, an error), or with the next update.
    news: bool,
    status_changed: bool,
    /// The helper's stdin for probe requests; what the visible part wants answered, most
    /// wanted last; how far the sweep over the whole sample grid has got (level, index);
    /// what was asked already; and the requests unanswered.
    prober: Option<ChildStdin>,
    wanted: Vec<u64>,
    sweep: (u32, u64),
    requested: HashSet<u64>,
    in_flight: usize,
}

impl Worker {
    fn new(look: Look, prober: Option<ChildStdin>) -> Self {
        let long_ago = Instant::now() - SIDE_EVERY;
        Worker {
            atlas: None,
            look,
            view: None,
            inspect: None,
            data: 0,
            colorer: None,
            snap: Snapshot::default(),
            side_for: None,
            inspected_for: None,
            cache: HashMap::new(),
            used: HashMap::new(),
            answered: Vec::new(),
            stale: HashSet::new(),
            last_fallback: long_ago,
            stats: std::env::var_os("BTRMAPS_FRAMES").map(|_| PassStats {
                since: Instant::now(),
                took: Vec::new(),
                rendered: 0,
            }),
            tiles_for: None,
            passes: 0,
            renders: 0,
            last_side: long_ago,
            last_tiles: long_ago,
            last_sent: long_ago,
            news: false,
            status_changed: false,
            prober,
            wanted: Vec::new(),
            sweep: (0, 0),
            requested: HashSet::new(),
            in_flight: 0,
        }
    }

    fn take(&mut self, input: Input) {
        match input {
            Input::Look(look) => self.look = look,
            Input::View(view) => self.view = view,
            Input::Inspect(pos) => self.inspect = pos,
            Input::Event(Event::Status(s)) => {
                self.snap.status = s;
                self.status_changed = true;
            }
            Input::Event(Event::Refused(why)) => {
                self.snap.refused = Some(why);
                self.news = true;
            }
            Input::Event(Event::Ended(r)) => {
                self.snap.ended = Some(r);
                self.news = true;
            }
            Input::Event(Event::Msg(Err(e))) => {
                self.snap.error = Some(e);
                self.news = true;
            }
            Input::Event(Event::Msg(Ok(msg))) => {
                if let Msg::Runs { runs } = &msg {
                    self.in_flight = self.in_flight.saturating_sub(1);
                    self.answered.extend(runs.iter().map(|r| (r.0, r.0 + r.1)));
                }
                let (atlas, error) = atlas::fold(self.atlas.take(), msg);
                if let (None, Some(a)) = (&self.snap.header, &atlas) {
                    self.snap.header = Some(Arc::new(a.header.clone()));
                }
                self.atlas = atlas;
                if let Some(e) = error {
                    self.snap.error = Some(format!("{e:#}"));
                    self.news = true;
                }
                self.data += 1;
            }
        }
    }

    /// How soon tiles may be brought up to date again: a frame for a moved view or a
    /// new look, longer for answers alone.
    fn tiles_every(&self) -> Duration {
        match self.tiles_for != Some((self.look, self.view)) {
            true => MOVE_EVERY,
            false => TILES_EVERY,
        }
    }

    /// Whether the tiles need bringing up to date, and since when they could be.
    fn tiles_due(&self) -> bool {
        self.view.is_some()
            && (self.tiles_for != Some((self.look, self.view)) || !self.answered.is_empty())
    }

    /// Remake whatever is out of date and due, and keep the helper busy; whether the
    /// window has news.
    fn refresh(&mut self) -> bool {
        let status_due = self.status_changed && self.last_sent.elapsed() >= SIDE_EVERY;
        let mut changed = std::mem::take(&mut self.news) || status_due;
        // Keep the helper busy before spending time rendering.
        self.ask();
        let Some(atlas) = &self.atlas else {
            return changed;
        };
        // Colors for the current look, covering every set found so far. Tiles hold data,
        // not colors, so a new look only changes the GPU's color tables.
        if self.colorer.as_ref().map(|c| c.0) != Some(self.look) {
            let palette = Palette::new(self.look.dark);
            let calibration = &atlas.header.calibration;
            let colorer = Colorer::new(&palette, self.look.mode, calibration);
            let (ramp, age) = colorer.ramp();
            let look = self.snap.colors.look + 1;
            self.snap.colors = Arc::new(GpuColors {
                look,
                table: Vec::new(),
                ramp,
                age,
            });
            self.colorer = Some((self.look, colorer));
            changed = true;
        }
        let colorer = &mut self.colorer.as_mut().expect("just made").1;
        colorer.extend(&atlas.sets);
        let known = self.snap.colors.table.len();
        if known < atlas.sets.len() {
            Arc::make_mut(&mut self.snap.colors)
                .table
                .extend(colorer.table(known));
        }
        let colorer = &self.colorer.as_ref().expect("just made").1;

        // Totals and list: at once for a new look, else at most every SIDE_EVERY while
        // answers stream in.
        let look_changed = self.side_for.map(|k| k.0) != Some(self.look);
        let side_due = look_changed || self.last_side.elapsed() >= SIDE_EVERY;
        if self.side_for != Some((self.look, self.data)) && side_due {
            let palette = Palette::new(self.look.dark);
            let side = stats::side(atlas, &palette, colorer, self.look.mode);
            self.snap.side = Arc::new(side);
            self.side_for = Some((self.look, self.data));
            self.last_side = Instant::now();
            changed = true;
        }

        // Tiles: for a new view or look at most once a frame (however many pan and zoom
        // events arrive), for answers at most every TILES_EVERY.
        if self.tiles_due() && self.last_tiles.elapsed() >= self.tiles_every() {
            let view = self.view.expect("checked by tiles_due");
            let start = Instant::now();
            let rendered = self.bring_tiles_up_to_date(view);
            if let Some(stats) = &mut self.stats {
                stats.record(start.elapsed(), rendered);
            }
            self.tiles_for = Some((self.look, self.view));
            self.last_tiles = Instant::now();
            changed = true;
        }

        let Some(atlas) = &self.atlas else {
            return changed;
        };
        let key = (self.inspect, self.data);
        if self.inspected_for != Some(key) {
            self.snap.inspected = self.inspect.and_then(|p| Some((p, atlas.at(p)?)));
            self.inspected_for = Some(key);
            changed = true;
        }
        self.ask();
        changed
    }

    /// Drop the tiles answers have changed, render the ones the view needs, and list
    /// what to ask about next.
    /// How many tiles it rendered.
    fn bring_tiles_up_to_date(&mut self, view: View) -> usize {
        let (Some(atlas), Some((_, colorer))) = (&self.atlas, &self.colorer) else {
            return 0;
        };
        let cutoffs = &colorer.cutoffs().clone();
        // A tile is a contiguous byte range, so it changed only if an answer overlaps it.
        let mut answered = std::mem::take(&mut self.answered);
        answered.sort_unstable();
        for key in self.cache.keys() {
            let (start, end) = atlas.span(key.level, key.x, key.y);
            let i = answered.partition_point(|r| r.0 < end);
            if answered[..i].iter().any(|r| r.1 > start) {
                self.stale.insert(*key);
            }
        }

        // Every level from the top down to the shown one: each tile borrows from its
        // parent. Missing tiles are rendered at every level; changed ones only where they
        // show: the shown level at once, the one above it (drawn under it while zooming)
        // at most every SIDE_EVERY. Higher ones only lend to their children until shown.
        let shown = tiles::level_for(atlas.deep, view.across);
        let mut rendered = 0;
        let needed: Vec<Vec<Key>> = (0..=shown).map(|z| tiles::visible(view, z)).collect();
        let fallback_due = self.last_fallback.elapsed() >= SIDE_EVERY;
        for (level, keys) in needed.iter().enumerate() {
            let level = level as u32;
            let refresh = level == shown || (level + 1 == shown && fallback_due);
            let todo: Vec<Key> = keys
                .iter()
                .copied()
                .filter(|k| !self.cache.contains_key(k) || (refresh && self.stale.contains(k)))
                .collect();
            // Every rendering gets its own id, so the GPU knows when to upload it.
            let first = self.renders;
            self.renders += todo.len() as u64;
            let made: Vec<Tile> = {
                use rayon::prelude::*;
                todo.par_iter()
                    .enumerate()
                    .map(|(n, &key)| {
                        let parent = key.parent().and_then(|p| self.cache.get(&p));
                        let id = first + n as u64 + 1;
                        tiles::render(atlas, cutoffs, key, parent.map(|t| &**t), id)
                    })
                    .collect()
            };
            rendered += made.len();
            for tile in made {
                self.stale.remove(&tile.key);
                self.cache.insert(tile.key, Arc::new(tile));
            }
        }
        if fallback_due {
            self.last_fallback = Instant::now();
        }
        self.passes += 1;
        for key in needed.iter().flatten() {
            self.used.insert(*key, self.passes);
        }
        // Forget the tiles needed longest ago.
        if self.cache.len() > CACHE {
            let mut old: Vec<(u64, Key)> = self
                .cache
                .keys()
                .map(|k| (self.used.get(k).copied().unwrap_or(0), *k))
                .collect();
            old.sort_unstable();
            for (_, key) in old.iter().take(self.cache.len() - CACHE) {
                self.cache.remove(key);
                self.used.remove(key);
                self.stale.remove(key);
            }
        }

        // Every level from the top down: the window falls back to any of them per pixel.
        self.snap.tiles = needed
            .iter()
            .flatten()
            .filter_map(|k| self.cache.get(k).cloned())
            .collect();
        let shown_tiles: Vec<&Arc<Tile>> = self
            .snap
            .tiles
            .iter()
            .filter(|t| t.key.level == shown)
            .collect();
        self.snap.refining = shown_tiles.iter().map(|t| t.open.len()).sum();
        self.snap.finest = shown == tiles::finest(atlas.deep);

        // What to ask about: from LEAD levels coarser than the shown one down to it.
        let levels: Vec<(i32, Vec<&Tile>)> = (shown as i32 - LEAD..=shown as i32)
            .map(|z| {
                let tiles = match z >= 0 {
                    true => needed[z as usize]
                        .iter()
                        .filter_map(|k| self.cache.get(k))
                        .map(|t| &**t)
                        .collect(),
                    false => Vec::new(),
                };
                (z.max(-8), tiles)
            })
            .collect();
        let wanted = tiles::wanted(atlas, &levels, view, WANTED);
        self.wanted = wanted.into_iter().rev().collect();
        rendered
    }

    /// The next point of the sweep over the whole sample grid, coarse to fine, that is
    /// still unanswered; None once the grid is complete.
    fn next_sweep(&mut self) -> Option<u64> {
        let atlas = self.atlas.as_ref()?;
        let order = atlas.grid.trailing_zeros();
        loop {
            let (level, i) = self.sweep;
            if level > order {
                return None;
            }
            let side = 1u64 << level;
            if i >= side * side {
                self.sweep = (level + 1, 0);
                continue;
            }
            self.sweep.1 += 1;
            let (x, y) = d2xy(side, spread(level, i));
            let (gx, gy) = atlas.rep(side, x, y);
            if atlas.open(gx, gy) {
                return Some(atlas.center(atlas.grid, gx, gy));
            }
        }
    }

    /// Keep a few requests in flight: mostly what the visible part wants, with a share
    /// for the sweep the sizes are estimated from.
    fn ask(&mut self) {
        while self.prober.is_some() && self.in_flight < IN_FLIGHT {
            // Every answer covers the position it was asked about, so what was asked long
            // ago is known by now; the list only has to outlast the requests in flight.
            if self.requested.len() > ASKED_MAX {
                self.requested.clear();
            }
            // Mostly what is on screen; a quarter keeps the sweep the sizes come from
            // moving. Either takes the other's share when it has nothing left.
            let mut batch = Vec::with_capacity(PROBE_BATCH);
            let shares: [(usize, bool); 3] = [
                (PROBE_BATCH * 3 / 4, true),
                (PROBE_BATCH, false),
                (PROBE_BATCH, true),
            ];
            for (up_to, on_screen) in shares {
                while batch.len() < up_to {
                    let next = match on_screen {
                        true => self.wanted.pop(),
                        false => self.next_sweep(),
                    };
                    let Some(pos) = next else { break };
                    if self.requested.insert(pos) {
                        batch.push(pos);
                    }
                }
            }
            if batch.is_empty() {
                return;
            }
            let line = serde_json::to_string(&Request::Probe { positions: batch })
                .expect("requests serialize");
            let prober = self.prober.as_mut().expect("checked above");
            if writeln!(prober, "{line}")
                .and_then(|()| prober.flush())
                .is_err()
            {
                // The helper is gone; nothing more to ask.
                self.prober = None;
                return;
            }
            self.in_flight += 1;
        }
    }

    /// How long until something out of date comes due; None when nothing is.
    fn wait(&self) -> Option<Duration> {
        let loaded = self.atlas.is_some();
        let status = self
            .status_changed
            .then(|| SIDE_EVERY.saturating_sub(self.last_sent.elapsed()));
        let side = (loaded && self.side_for != Some((self.look, self.data)))
            .then(|| SIDE_EVERY.saturating_sub(self.last_side.elapsed()));
        let tiles = (loaded && self.tiles_due())
            .then(|| self.tiles_every().saturating_sub(self.last_tiles.elapsed()));
        [status, side, tiles].into_iter().flatten().min()
    }

    fn run(mut self, inputs: Receiver<Input>, out: Sender<Snapshot>, ctx: egui::Context) {
        loop {
            let first = match self.wait() {
                Some(t) => match inputs.recv_timeout(t) {
                    Ok(input) => Some(input),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match inputs.recv() {
                    Ok(input) => Some(input),
                    Err(_) => return,
                },
            };
            for input in first.into_iter().chain(inputs.try_iter().take(DRAIN)) {
                self.take(input);
            }
            if self.refresh() {
                self.last_sent = Instant::now();
                self.status_changed = false;
                if out.send(self.snap.clone()).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        }
    }
}

/// The model thread's tile passes, printed to stderr every two seconds when
/// BTRMAPS_FRAMES is set.
struct PassStats {
    since: Instant,
    took: Vec<f32>,
    rendered: usize,
}

impl PassStats {
    fn record(&mut self, took: Duration, rendered: usize) {
        self.took.push(took.as_secs_f32() * 1e3);
        self.rendered += rendered;
        let elapsed = self.since.elapsed().as_secs_f32();
        if elapsed < 2.0 {
            return;
        }
        let mut took = std::mem::take(&mut self.took);
        took.sort_by(f32::total_cmp);
        let at = |q: f32| took[((took.len() - 1) as f32 * q) as usize];
        eprintln!(
            "model thread: {} tile passes · p50 {:.1}, max {:.1} ms · {} tiles rendered",
            took.len(),
            at(0.5),
            at(1.0),
            std::mem::take(&mut self.rendered),
        );
        self.since = Instant::now();
    }
}

/// Run below the window: the model thread and its pool of threads for rendering get the
/// CPU after it, and leave it a core. Linux sets niceness per thread.
fn background() {
    // SAFETY: setpriority on the calling thread; no memory involved.
    unsafe {
        let me = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, me, 10);
    }
}

/// Start scanning: the helper, the threads reading it, and the model thread. Dropping
/// both ends stops everything: the model thread quits, the readers stop, the pipes
/// close, and the helper exits on its next write. (It runs as root, so it can't be
/// signalled.)
pub fn start(
    scan: Scan,
    password: Option<String>,
    look: Look,
    ctx: egui::Context,
) -> (Sender<Input>, Receiver<Snapshot>) {
    let (inbox, inputs) = channel();
    let (out, snapshots) = channel();
    let events = inbox.clone();
    std::thread::spawn(move || {
        let prober = launch(&scan, password.as_deref(), &events);
        background();
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(cores.saturating_sub(1).max(1))
            .start_handler(|_| background())
            .build()
            .expect("threads start");
        pool.install(|| Worker::new(look, prober).run(inputs, out, ctx));
    });
    (inbox, snapshots)
}

/// Start the helper and the threads reading its output; its stdin, for probe requests.
fn launch(scan: &Scan, password: Option<&str>, events: &Sender<Input>) -> Option<ChildStdin> {
    let send = |tx: &Sender<Input>, e| tx.send(Input::Event(e)).is_ok();
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            send(events, Event::Ended(Err(format!("find btrmaps: {e}"))));
            return None;
        }
    };
    let args = [exe.into(), "scan".into(), scan.target.clone().into()];
    let mut child = match elevate::spawn(&args, password) {
        Ok(child) => child,
        Err(why) => {
            send(events, Event::Refused(why));
            return None;
        }
    };
    let (stdin, stdout, mut stderr) = (
        child.stdin.take(),
        child.stdout.take()?,
        child.stderr.take()?,
    );

    let status = events.clone();
    std::thread::spawn(move || {
        // Progress lines end in \r; pass on each one.
        let mut buf = [0u8; 4096];
        let mut line = Vec::new();
        while let Ok(n @ 1..) = std::io::Read::read(&mut stderr, &mut buf) {
            for &b in &buf[..n] {
                if b != b'\r' && b != b'\n' {
                    line.push(b);
                    continue;
                }
                let text = String::from_utf8_lossy(&line).trim().to_string();
                line.clear();
                if !text.is_empty() && !send(&status, Event::Status(text)) {
                    return;
                }
            }
        }
    });

    let stream = events.clone();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let msg = line
                .map_err(|e| format!("read: {e}"))
                .and_then(|l| serde_json::from_str::<Msg>(&l).map_err(|e| format!("parse: {e}")));
            if !send(&stream, Event::Msg(msg)) {
                return;
            }
        }
        let ended = match child.wait().map(|s| s.code()) {
            Ok(Some(0)) => Ok(()),
            // The reason, if the helper or sudo gave one, is the latest status line.
            Ok(code) => Err(format!("The scan stopped (exit {}).", code.unwrap_or(-1))),
            Err(e) => Err(format!("wait for scan: {e}")),
        };
        send(&stream, Event::Ended(ended));
    });
    stdin
}
