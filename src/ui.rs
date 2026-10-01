//! The window: a top bar, the map, a side pane and a status bar. It draws snapshots
//! from the model thread and tells it what changed; it never computes anything big.

use crate::elevate;
use crate::gpu::{self, MapPainter};
use crate::mounts::{self, Filesystem};
use crate::palette::{MODES, Mode, Palette, date_of};
use crate::proto::{Algo, Kind, SetInfo};
use crate::stats::{Side, bytes, describe, kind_label, ymd};
use crate::tiles::{self, View};
use crate::worker::{self, Input, Look, Scan, Snapshot};
use anyhow::Result;
use eframe::egui::{self, Color32, CornerRadius, Key, Rect, RichText, Sense, Vec2, pos2, vec2};
use eframe::egui_glow;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

/// Opens folders in the desktop's file manager.
const XDG_OPEN: &str = match option_env!("XDG_OPEN") {
    Some(path) => path,
    None => "xdg-open",
};
const BAR_HEIGHT: f32 = 24.0;
const BAR_GAP: f32 = 16.0;
/// Height of the side pane's info section, fixed so hovering never moves the list.
const INFO_HEIGHT: f32 = 220.0;

/// A question the window is waiting on.
enum Prompt {
    Password {
        scan: Scan,
        text: String,
        error: Option<String>,
    },
    Delete(Vec<PathBuf>),
}

/// What was last told to the model thread, so each change is sent once.
#[derive(Default)]
struct Sent {
    look: Option<Look>,
    view: Option<Option<View>>,
    inspect: Option<Option<u64>>,
}

struct App {
    gpu: Option<Arc<Mutex<MapPainter>>>,
    filesystems: Vec<Filesystem>,
    /// The filesystem picked in the menu, by a mount point; scanned when Scan is pressed.
    selected: Option<PathBuf>,
    mode: Mode,
    scan: Option<Scan>,
    /// The model thread: where changes go, and where snapshots come from.
    link: Option<(Sender<Input>, Receiver<Snapshot>)>,
    sent: Sent,
    snap: Snapshot,
    /// The map's side in points, fitted to the window when first shown and kept when
    /// the window is resized; and how far it is moved from the middle.
    scale: Option<f32>,
    pan: Vec2,
    hover: Option<u32>,
    /// The list row under the pointer, if any.
    list_hover: Option<u32>,
    pinned: Option<u32>,
    /// The set an open right-click menu is about.
    menu: Option<u32>,
    prompt: Option<Prompt>,
    /// The outcome of the last trash or delete, and the one still running.
    notice: Option<String>,
    removing: Option<Receiver<String>>,
    /// The mode buttons' width as last drawn, to center them.
    modes_width: f32,
    /// Frame timings, when BTRMAPS_FRAMES is set.
    frames: Option<FrameStats>,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        let gpu = cc
            .gl
            .as_ref()
            .and_then(|gl| match MapPainter::new(gl, cc.egui_ctx.clone()) {
                Ok(p) => Some(Arc::new(Mutex::new(p))),
                Err(e) => {
                    eprintln!("btrmaps: can't draw the map: {e}");
                    None
                }
            });
        let filesystems = mounts::discover();
        App {
            gpu,
            selected: mounts::default_selection(&filesystems),
            filesystems,
            mode: Mode::Owners,
            scan: None,
            link: None,
            sent: Sent::default(),
            snap: Snapshot::default(),
            scale: None,
            pan: Vec2::ZERO,
            hover: None,
            list_hover: None,
            pinned: None,
            menu: None,
            prompt: None,
            notice: None,
            removing: None,
            modes_width: 330.0,
            frames: std::env::var_os("BTRMAPS_FRAMES").map(|_| FrameStats::default()),
        }
    }

    fn look(&self, ctx: &egui::Context) -> Look {
        Look {
            mode: self.mode,
            dark: ctx.theme() == egui::Theme::Dark,
        }
    }

    /// Drop the scan (stopping it if it is still running), keeping view settings.
    fn stop(&mut self) {
        self.link = None;
        self.scan = None;
        self.sent = Sent::default();
        self.snap = Snapshot::default();
        self.hover = None;
        self.list_hover = None;
        self.pinned = None;
        self.notice = None;
    }

    fn begin(&mut self, scan: Scan, password: Option<String>, ctx: &egui::Context) {
        self.stop();
        let look = self.look(ctx);
        self.link = Some(worker::start(scan.clone(), password, look, ctx.clone()));
        self.sent.look = Some(look);
        self.scan = Some(scan);
    }

    /// Scan now if no password is needed, otherwise ask for it first.
    fn request(&mut self, scan: Scan, ctx: &egui::Context) {
        match elevate::needs_password() {
            false => self.begin(scan, None, ctx),
            true => {
                self.stop();
                self.prompt = Some(Prompt::Password {
                    scan,
                    text: String::new(),
                    error: None,
                });
            }
        }
    }

    /// Take the newest snapshot, if one arrived.
    fn receive(&mut self) {
        let Some((_, inbox)) = &self.link else {
            return;
        };
        let Some(snap) = inbox.try_iter().last() else {
            return;
        };
        if let (Some(why), Some(scan)) = (&snap.refused, &self.scan) {
            // sudo turned the password down: ask again, with its reason.
            self.prompt = Some(Prompt::Password {
                scan: scan.clone(),
                text: String::new(),
                error: Some(why.clone()),
            });
            self.stop();
            return;
        }
        self.snap = snap;
    }

    fn tell(&self, input: Input) {
        if let Some((outbox, _)) = &self.link {
            let _ = outbox.send(input);
        }
    }

    fn tell_look(&mut self, ctx: &egui::Context) {
        let look = self.look(ctx);
        if self.sent.look != Some(look) {
            self.tell(Input::Look(look));
            self.sent.look = Some(look);
        }
    }

    fn tell_view(&mut self, view: Option<View>) {
        if self.sent.view != Some(view) {
            self.tell(Input::View(view));
            self.sent.view = Some(view);
        }
    }

    fn tell_inspect(&mut self, pos: Option<u64>) {
        if self.sent.inspect != Some(pos) {
            self.tell(Input::Inspect(pos));
            self.sent.inspect = Some(pos);
        }
    }

    fn set(&self, id: u32) -> Option<&Arc<SetInfo>> {
        self.snap.side.sets.get(id as usize)
    }

    fn focus(&self) -> Option<u32> {
        self.pinned.or(self.hover)
    }

    /// The set the map keeps bright, dimming the rest: a clicked one, a hovered list
    /// row, or what is under the pointer while Shift is held. Hovering the map alone
    /// dims nothing, so panning and zooming never darken it. Free space is nothing to
    /// look at.
    fn spotlight(&self, shift: bool) -> Option<u32> {
        self.pinned
            .or(self.list_hover)
            .or(self.hover.filter(|_| shift))
            .filter(|&f| self.set(f).is_some_and(|s| s.kind != Kind::Free))
    }

    fn current_filesystem(&self) -> Option<&Filesystem> {
        let target = self.selected.as_ref()?;
        self.filesystems.iter().find(|fs| fs.mounted_at(target))
    }

    /// Name and filesystem on the left, modes centered, Refresh on the right. The modes
    /// become a menu when the window is too narrow for their buttons.
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        use egui::{Align, Layout, UiBuilder};
        let full = ui.available_rect_before_wrap();
        let bar = Rect::from_min_size(
            full.min + vec2(4.0, 6.0),
            vec2(full.width() - 8.0, BAR_HEIGHT),
        );
        ui.allocate_rect(
            Rect::from_min_size(full.min, vec2(full.width(), BAR_HEIGHT + 12.0)),
            Sense::hover(),
        );
        let group = |ui: &mut egui::Ui, layout: Layout, rect: Rect| {
            let mut child = ui.new_child(UiBuilder::new().max_rect(rect).layout(layout));
            child.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            child
        };

        let mut left = group(ui, Layout::left_to_right(Align::Center), bar);
        left.label(RichText::new("btrmaps").strong().size(16.0));
        let (pick, rediscover) = self.filesystem_menu(&mut left);
        let left_edge = left.min_rect().right() + BAR_GAP;

        let mut right = group(ui, Layout::right_to_left(Align::Center), bar);
        let refresh = right
            .add_enabled(self.scan.is_some(), egui::Button::new("⟳ Refresh"))
            .clicked();
        let right_edge = right.min_rect().left() - BAR_GAP;

        let mut mode = self.mode;
        let width = self.modes_width;
        if right_edge - left_edge >= width {
            let x = (bar.center().x - width / 2.0).clamp(left_edge, right_edge - width);
            let rect = Rect::from_min_max(pos2(x, bar.top()), bar.max);
            let mut mid = group(ui, Layout::left_to_right(Align::Center), rect);
            mid.spacing_mut().button_padding = vec2(10.0, 3.0);
            mid.spacing_mut().item_spacing.x = 2.0;
            for (m, label) in MODES {
                if mid
                    .add(egui::Button::selectable(mode == m, label))
                    .clicked()
                {
                    mode = m;
                }
            }
            self.modes_width = mid.min_rect().width();
        } else {
            let rect = Rect::from_min_max(pos2(left_edge, bar.top()), bar.max);
            let mut mid = group(ui, Layout::left_to_right(Align::Center), rect);
            let current = MODES.iter().find(|(m, _)| *m == mode).map_or("", |m| m.1);
            egui::ComboBox::from_id_salt("mode")
                .selected_text(current)
                .show_ui(&mut mid, |ui| {
                    for (m, label) in MODES {
                        ui.selectable_value(&mut mode, m, label);
                    }
                });
        }
        self.mode = mode;

        let ctx = ui.ctx().clone();
        if rediscover {
            self.filesystems = mounts::discover();
            let gone = self.current_filesystem().is_none();
            if gone {
                self.selected = mounts::default_selection(&self.filesystems);
            }
        }
        // Choosing a filesystem only selects it; the Scan button starts the scan.
        if let Some(target) = pick.filter(|t| self.selected.as_ref() != Some(t)) {
            self.stop();
            self.selected = Some(target);
        } else if let (true, Some(scan)) = (refresh, &self.scan) {
            self.request(scan.clone(), &ctx);
        }
    }

    /// The filesystem dropdown: what was picked, and whether to look for filesystems again.
    fn filesystem_menu(&self, ui: &mut egui::Ui) -> (Option<PathBuf>, bool) {
        let current = self.current_filesystem().map(Filesystem::name);
        let font = egui::TextStyle::Button.resolve(ui.style());
        let (mut pick, mut rediscover) = (None, false);
        let shown = match &current {
            Some(name) => ellipsize_middle(ui.ctx(), name, &font, 200.0),
            None => "Choose a filesystem…".to_string(),
        };
        egui::ComboBox::from_id_salt("filesystem")
            .width(240.0)
            .selected_text(shown)
            .show_ui(ui, |ui| {
                for fs in &self.filesystems {
                    let usage = fs
                        .usage
                        .map(|(size, used)| format!(" · {} of {}", bytes(used), bytes(size)))
                        .unwrap_or_default();
                    let name = ellipsize_middle(ui.ctx(), &fs.name(), &font, 320.0);
                    let chosen = current.as_deref() == Some(&fs.name());
                    if ui
                        .selectable_label(chosen, format!("{name}{usage}"))
                        .clicked()
                    {
                        pick = fs.scan_target();
                    }
                }
                if self.filesystems.is_empty() {
                    ui.weak("No btrfs filesystems are mounted.");
                }
                ui.separator();
                rediscover = ui.button("Look again").clicked();
            });
        (pick, rediscover)
    }

    /// What is under the pointer on the left, the last removal or refinement on the right.
    fn status_bar(&self, ui: &mut egui::Ui) {
        let side = &self.snap.side;
        egui::containers::Sides::new()
            .shrink_left()
            .truncate()
            .show(
                ui,
                |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let Some((id, set)) = self.hover.and_then(|h| Some((h, self.set(h)?))) else {
                        ui.weak(match self.scan {
                            Some(_) => "Hover the map to see where space goes.",
                            None => "No filesystem open.",
                        });
                        return;
                    };
                    ui.label(RichText::new(describe(set)).monospace());
                    let area = side.stats.area.get(id as usize).copied().unwrap_or(0);
                    ui.weak(bytes(area));
                    if let Some((pos, _)) = self.snap.inspected {
                        ui.weak(format!("· offset {}", bytes(pos)));
                    }
                    if self.pinned.is_none() && self.list_hover.is_none() {
                        ui.weak("· click to highlight, hold Shift to peek");
                    }
                },
                |ui| {
                    let (refining, finest) = (self.snap.refining, self.snap.finest);
                    match (&self.notice, refining) {
                        (Some(n), _) => {
                            ui.label(RichText::new(n).strong());
                        }
                        (None, 0) if finest && !self.snap.tiles.is_empty() => {
                            ui.weak("full resolution");
                        }
                        (None, 0) => {}
                        (None, n) => {
                            ui.weak(format!("refining · {n} pixels to go"));
                        }
                    }
                },
            );
    }

    /// Before a scan: what is selected, and a big button to scan it.
    fn welcome(&mut self, ui: &mut egui::Ui) {
        let chosen = self.current_filesystem().cloned();
        let mut scan = false;
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() / 3.0);
            match &chosen {
                Some(fs) => {
                    ui.heading(fs.name());
                    if let Some((size, used)) = fs.usage {
                        ui.label(format!("{} of {} used", bytes(used), bytes(size)));
                    }
                    let mounts: Vec<String> = fs
                        .mounts
                        .iter()
                        .map(|m| m.path.display().to_string())
                        .collect();
                    ui.weak(mounts.join(" · "));
                    ui.add_space(16.0);
                    let button = egui::Button::new(RichText::new("Scan").size(22.0).strong())
                        .min_size(vec2(180.0, 52.0));
                    scan = ui.add(button).clicked();
                    ui.add_space(8.0);
                    ui.weak(match elevate::is_root() {
                        true => "Scanning reads the filesystem's trees as root.",
                        false => {
                            "Scanning reads the filesystem as root, so it asks for your password."
                        }
                    });
                }
                None if self.filesystems.is_empty() => {
                    ui.heading("No btrfs filesystems are mounted");
                    ui.label("Mount one, then choose Look again in the menu at the top left.");
                }
                None => {
                    ui.heading("Where is the space going?");
                    ui.label("Choose a btrfs filesystem from the menu at the top left.");
                }
            }
        });
        if let (true, Some(target)) = (scan, self.selected.clone()) {
            let ctx = ui.ctx().clone();

            self.request(Scan { target }, &ctx);
        }
    }

    /// The password and delete dialogs.
    fn prompts(&mut self, ctx: &egui::Context, palette: &Palette) {
        match self.prompt.take() {
            None => {}
            Some(Prompt::Password {
                scan,
                mut text,
                error,
            }) => {
                let (submit, cancel) = password_dialog(ctx, palette, &scan, &mut text, &error);
                match (submit, cancel) {
                    (true, _) => self.begin(scan, Some(text), ctx),
                    (_, true) => {}
                    _ => self.prompt = Some(Prompt::Password { scan, text, error }),
                }
            }
            Some(Prompt::Delete(targets)) => match delete_dialog(ctx, palette, &targets) {
                Some(true) => self.remove(targets, false, ctx),
                Some(false) => {}
                None => self.prompt = Some(Prompt::Delete(targets)),
            },
        }
    }

    /// Trash or delete in the background, reporting in the status bar when done.
    fn remove(&mut self, targets: Vec<PathBuf>, trash: bool, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        self.notice = Some(match trash {
            true => format!("Moving {} item(s) to Trash…", targets.len()),
            false => format!("Deleting {} item(s)…", targets.len()),
        });
        std::thread::spawn(move || {
            let _ = tx.send(removal(&targets, trash));
            ctx.request_repaint();
        });
        self.removing = Some(rx);
    }

    fn side_panel(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        // Fill the pane, or it shrinks to its content.
        ui.set_min_width(ui.available_width());
        // Fill the pane, or it shrinks to its content.
        ui.set_min_width(ui.available_width());
        let side = self.snap.side.clone();
        ui.add_space(4.0);
        if let Some(Err(e)) = &self.snap.ended {
            ui.colored_label(palette.error, e);
            ui.label(RichText::new(&self.snap.status).monospace().weak());
        }
        let Some(header) = self.snap.header.clone() else {
            if self.snap.ended.is_none() {
                ui.label("Waiting for the scan to start…");
                ui.label(RichText::new(&self.snap.status).monospace().weak());
            }
            return;
        };
        ui.label(RichText::new(&side.summary).weak());
        if let Some((frac, text)) = &side.progress {
            ui.add(egui::ProgressBar::new(*frac).text(text));
        }
        category_bar(ui, &side);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(12.0, 4.0);
            for c in &side.cats {
                chip(ui, c.color, &c.label, &bytes(c.bytes));
            }
        });
        ui.label(RichText::new(&side.note).weak());
        if let Some(e) = &self.snap.error {
            ui.colored_label(palette.error, format!("stream error: {e}"));
        }

        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("info")
            .min_scrolled_height(INFO_HEIGHT)
            .max_height(INFO_HEIGHT)
            .show(ui, |ui| {
                ui.set_min_height(INFO_HEIGHT);
                self.info(ui, &side, &header);
            });
        ui.separator();

        ui.label(RichText::new(side.list_title).small().strong());
        let focus = self.focus();
        let menu_open = egui::Popup::is_any_open(ui.ctx());
        let mut hovered = None;
        if side.list.is_empty() {
            ui.weak("Nothing here.");
        }
        // Only the rows in sight are laid out: the list can be hundreds of rows long.
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
        egui::ScrollArea::vertical().show_rows(ui, row_height, side.list.len(), |ui, range| {
            for row in &side.list[range] {
                let bg = ui.painter().add(egui::Shape::Noop);
                let rect = ui
                    .scope(|ui| {
                        egui::containers::Sides::new()
                            .shrink_left()
                            .truncate()
                            .show(
                                ui,
                                |ui| {
                                    swatch(ui, row.color);
                                    // Cut long paths in the middle, keeping the file name.
                                    let font = egui::TextStyle::Monospace.resolve(ui.style());
                                    let width = ui.available_width() - 2.0;
                                    let fitted =
                                        ellipsize_middle(ui.ctx(), &row.text, &font, width);
                                    ui.label(RichText::new(fitted).monospace())
                                        .on_hover_text(&row.text);
                                },
                                |ui| ui.weak(&row.size),
                            )
                    })
                    .response
                    .rect;
                let resp = ui.interact(rect, egui::Id::new(("row", row.set)), Sense::click());
                if resp.secondary_clicked() {
                    self.menu = Some(row.set);
                }
                resp.context_menu(|ui| self.context_menu(ui));
                if resp.hovered() {
                    hovered = Some(row.set);
                }
                if resp.clicked() && !menu_open {
                    self.pinned = (self.pinned != Some(row.set)).then_some(row.set);
                }
                if focus == Some(row.set) || resp.hovered() {
                    let fill = ui.visuals().widgets.hovered.weak_bg_fill;
                    ui.painter()
                        .set(bg, egui::Shape::rect_filled(rect.expand(2.0), 3, fill));
                }
            }
        });
        self.list_hover = hovered;
        if hovered.is_some() {
            self.hover = hovered;
        }
    }

    /// About the focused set: its size, where it is, and its files.
    fn info(&self, ui: &mut egui::Ui, side: &Side, header: &crate::proto::Header) {
        let Some((id, set)) = self.focus().and_then(|f| Some((f, self.set(f)?))) else {
            ui.weak("Hover the map to see which files own it.");
            return;
        };
        let i = id as usize;
        let area = side.stats.area.get(i).copied().unwrap_or(0);
        ui.horizontal(|ui| {
            swatch(ui, side.swatches.get(i).copied().unwrap_or_default());
            ui.strong(kind_label(set.kind));
            if self.pinned == Some(id) {
                ui.weak("· pinned");
            }
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new(bytes(area)).size(20.0).strong());
            let share = 100.0 * area as f64 / header.total.max(1) as f64;
            ui.weak(format!("≈ {share:.2}%"));
        });
        if let Some((pos, a)) = self.snap.inspected.filter(|(_, a)| a.set == id) {
            let extent = match (set.kind, a.algo) {
                (Kind::Data | Kind::Unreachable, Algo::Unknown) => " · extent unknown".into(),
                (Kind::Data | Kind::Unreachable, Algo::None) => " · uncompressed".into(),
                (Kind::Data | Kind::Unreachable, algo) => format!(
                    " · {} {:.2}×",
                    format!("{algo:?}").to_lowercase(),
                    a.ratio as f64 / 100.0
                ),
                _ => String::new(),
            };
            let line = format!("offset {}{extent}", bytes(pos));
            ui.label(RichText::new(line).monospace().weak());
            if let Some(t) = date_of(&header.calibration, a.generation as u64) {
                let line = format!("written ≈ {} · generation {}", ymd(t), a.generation);
                ui.label(RichText::new(line).monospace().weak());
            }
        }
        match set.kind {
            Kind::Unreachable => {
                ui.weak("Bytes no file uses any more, kept alive by an extent these files still partly reference.");
            }
            Kind::Error => {
                ui.label(RichText::new(&set.error).monospace());
            }
            _ => {}
        }
        let (packed, holds) = (
            side.stats.packed.get(i).copied().unwrap_or(0.0),
            side.stats.holds.get(i).copied().unwrap_or(0.0),
        );
        if packed > 0.0 {
            ui.weak(format!(
                "{} of compressed extents hold {}",
                bytes(packed as u64),
                bytes(holds as u64)
            ));
        }
        if !matches!(set.kind, Kind::Data | Kind::Unreachable) {
            return;
        }
        let plural = |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
        let subvols = match set.subvolumes {
            n if n > 1 => format!(" across {n} subvolumes"),
            _ => String::new(),
        };
        let more = match set.truncated {
            true => " · more references than shown",
            false => "",
        };
        ui.weak(format!(
            "{}{subvols} · {}{more}",
            plural(set.files, "file"),
            plural(set.path_count, "path")
        ));
        for p in &set.paths {
            ui.label(RichText::new(p).monospace());
        }
        if set.path_count > set.paths.len() {
            ui.weak(format!("… {} more", set.path_count - set.paths.len()));
        }
    }

    /// Right-click menu for the files of a set.
    fn context_menu(&mut self, ui: &mut egui::Ui) {
        let Some(set) = self.menu.and_then(|m| self.set(m)).cloned() else {
            ui.close();
            return;
        };
        if !matches!(set.kind, Kind::Data | Kind::Unreachable) {
            ui.weak(format!("{}: not files", kind_label(set.kind)));
            return;
        }
        let mounts = self
            .current_filesystem()
            .map(|f| f.mounts.clone())
            .unwrap_or_default();
        let real: Vec<Option<PathBuf>> = set
            .paths
            .iter()
            .map(|p| mounts::real_path(&mounts, p))
            .collect();
        let title = describe(&set);
        let font = egui::TextStyle::Button.resolve(ui.style());
        ui.label(RichText::new(ellipsize_middle(ui.ctx(), &title, &font, 320.0)).strong());

        let first = real.first().cloned().flatten();
        if ui.button("Copy path").clicked() {
            let text = first.as_ref().map_or(title, |p| p.display().to_string());
            ui.ctx().copy_text(text);
            ui.close();
        }
        let folder = first
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        let show = ui.add_enabled(folder.is_some(), egui::Button::new("Show in folder"));
        if show.clicked() {
            if let Some(dir) = folder {
                let _ = std::process::Command::new(XDG_OPEN).arg(dir).spawn();
            }
            ui.close();
        }
        ui.separator();

        let blocked = if set.path_count != set.paths.len() {
            Some("More paths share this than btrmaps listed, so it can't free the space.")
        } else if real.iter().any(Option::is_none) {
            Some("Not mounted here, so btrmaps can't reach it.")
        } else if real.iter().flatten().any(|p| p.starts_with("/nix/store")) {
            Some("Managed by Nix: collect garbage instead.")
        } else {
            None
        };
        let what = match set.paths.len() {
            1 => "file".to_string(),
            n => format!("{n} paths"),
        };
        let targets: Vec<PathBuf> = real.into_iter().flatten().collect();
        let button = |ui: &mut egui::Ui, label: String| {
            ui.add_enabled(blocked.is_none(), egui::Button::new(label))
                .on_disabled_hover_text(blocked.unwrap_or_default())
                .clicked()
        };
        if button(ui, format!("Move {what} to Trash")) {
            let ctx = ui.ctx().clone();
            self.remove(targets.clone(), true, &ctx);
            ui.close();
        }
        if button(ui, format!("Delete {what}…")) {
            self.prompt = Some(Prompt::Delete(targets));
            ui.close();
        }
        if set.paths.len() > 1 {
            ui.weak("The space is only freed once every path is gone.");
        }
    }

    fn map(&mut self, ui: &mut egui::Ui) {
        // A click on an open menu's item must not also land on the map beneath it.
        let menu_open = egui::Popup::is_any_open(ui.ctx());
        let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        let rect = resp.rect;
        let ppp = ui.ctx().pixels_per_point();
        let Some(total) = self.snap.header.as_ref().map(|h| h.total) else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "waiting for scan…",
                egui::FontId::proportional(16.0),
                ui.visuals().weak_text_color(),
            );
            self.tell_view(None);
            return;
        };

        // Scroll or pinch zooms around the pointer, + and - around the middle; zooming
        // goes down to a few pixels per block. Resizing the window shows more or less
        // of the map around the middle, at the same scale.
        let fit = rect.width().min(rect.height()) * 0.94;
        let largest = (1u64 << crate::atlas::deep_order(total).0) as f32 * 8.0;
        if resp.dragged() {
            self.pan += resp.drag_delta();
        }
        if resp.double_clicked() {
            (self.scale, self.pan) = (None, Vec2::ZERO);
        }
        let current = *self.scale.get_or_insert(fit);
        let typing = ui.memory(|m| m.focused().is_some());
        let keys = ui.input(|i| {
            let zoom_in = i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals);
            match (typing, zoom_in, i.key_pressed(Key::Minus)) {
                (false, true, _) => 1.5,
                (false, _, true) => 1.0 / 1.5,
                _ => 1.0,
            }
        });
        let anchor = match keys {
            1.0 => resp.hover_pos(),
            _ => Some(rect.center()),
        };
        let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta().y, i.zoom_delta()));
        let factor = keys * pinch * (scroll * 0.0015).exp();
        if let (Some(m), true) = (anchor, factor != 1.0) {
            let corner = |size: f32, pan: Vec2| rect.center() - Vec2::splat(size / 2.0) + pan;
            let before = corner(current, self.pan);
            // Zooming out stops where the map fits, or where it already was if smaller.
            let size = (current * factor).clamp(fit.min(current), largest.max(fit));
            // Keep the point under the pointer where it is.
            let want = m - (m - before) * (size / current);
            self.pan = want - corner(size, Vec2::ZERO);
            self.scale = Some(size);
        }
        let size = self.scale.unwrap_or(fit);
        let map = Rect::from_min_size(
            rect.center() - Vec2::splat(size / 2.0) + self.pan,
            Vec2::splat(size),
        );
        let to_map = |p: egui::Pos2| {
            let rel = (p - map.min) / size;
            (rel.x as f64, rel.y as f64)
        };

        // The visible part, and how many physical pixels the whole map spans.
        let visible = map.intersect(rect);
        let view = visible.is_positive().then(|| {
            let ((u0, v0), (u1, v1)) = (to_map(visible.min), to_map(visible.max));
            View {
                u0,
                v0,
                u1,
                v1,
                across: (size * ppp) as f64,
            }
        });
        self.tell_view(view);

        // What is under the pointer: the set drawn there by the sharpest tile, and its
        // full answer from the model thread.
        let at = resp.hover_pos().filter(|p| map.contains(*p)).map(to_map);
        if resp.hovered() {
            let tiles = self.snap.tiles.iter().rev();
            self.hover = at.and_then(|(u, v)| tiles.clone().find_map(|t| t.set_at(u, v)));
        }
        self.tell_inspect(at.and_then(|(u, v)| tiles::position(total, u, v)));
        if resp.clicked() && !menu_open {
            self.pinned = (self.pinned != self.hover).then_some(self.hover).flatten();
        }
        if resp.secondary_clicked() {
            self.menu = self.hover;
        }

        // The whole map in one draw: the shown level's on-screen pages, each falling back
        // to its nearest ancestor on the GPU. On whole pixels, as egui rounds a callback
        // rect to them (and squeezes one hanging off the screen): the corners passed to
        // the shader are for the rounded rect, so the map moves by fractions of a pixel.
        let shift = ui.input(|i| i.modifiers.shift);
        let (focus, background) = (self.spotlight(shift), ui.visuals().panel_fill);
        let round = |p: egui::Pos2| ((p.to_vec2() * ppp).round() / ppp).to_pos2();
        let clipped = map.intersect(rect);
        let shown = Rect::from_min_max(round(clipped.min), round(clipped.max));
        if let (Some(gpu), true) = (self.gpu.clone(), shown.is_positive()) {
            let level = tiles::level_for(crate::atlas::deep_order(total).0, (size * ppp) as f64);
            let res = (tiles::TILE << level) as f64;
            let pages_across = 1u64 << level;
            let ((u0, v0), (u1, v1)) = (to_map(shown.min), to_map(shown.max));
            let page = |u: f64| {
                (u * pages_across as f64)
                    .floor()
                    .clamp(0.0, (pages_across - 1) as f64) as u64
            };
            let origin = (page(u0), page(v0));
            let last = (page(u1), page(v1));
            let scene = gpu::Scene {
                level,
                origin,
                pages: (last.0 - origin.0 + 1, last.1 - origin.1 + 1),
                tiles: self.snap.tiles.clone(),
                colors: self.snap.colors.clone(),
            };
            let corner = |u: f64, o: u64| (u * res - (o * tiles::TILE) as f64) as f32;
            let corners = [
                corner(u0, origin.0),
                corner(v0, origin.1),
                corner(u1, origin.0),
                corner(v1, origin.1),
            ];
            gpu.lock().expect("not painting now").prepare(scene);
            let callback = egui_glow::CallbackFn::new(move |_, painter| {
                let mut gpu = gpu.lock().expect("the painter is only used here");
                gpu.paint(painter.gl(), corners, focus, background);
            });
            painter.add(egui::PaintCallback {
                rect: shown,
                callback: Arc::new(callback),
            });
        }
        resp.context_menu(|ui| self.context_menu(ui));
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let (gap, moving) = ui.input(|i| {
            let moving = i.pointer.delta() != egui::Vec2::ZERO
                || i.smooth_scroll_delta != egui::Vec2::ZERO
                || i.zoom_delta() != 1.0
                || !i.keys_down.is_empty();
            (i.unstable_dt, moving)
        });
        self.frame(ui);
        if let Some(stats) = &mut self.frames {
            stats.record(gap, moving, frame.info().cpu_usage, self.gpu.as_ref());
            ui.ctx().request_repaint();
        }
    }

    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        if let (Some(gpu), Some(gl)) = (&self.gpu, gl) {
            gpu.lock().expect("not painting on exit").destroy(gl);
        }
    }
}

impl App {
    fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.receive();
        if let Some(done) = self.removing.as_ref().and_then(|r| r.try_recv().ok()) {
            (self.notice, self.removing) = (Some(done), None);
        }
        let palette = Palette::new(ctx.theme() == egui::Theme::Dark);
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        // A fixed height: a long hovered path must not resize, and so re-lay out, the map.
        egui::Panel::bottom("status")
            .resizable(false)
            .exact_size(26.0)
            .show(ui, |ui| self.status_bar(ui));
        self.tell_look(&ctx);
        self.prompts(&ctx, &palette);
        if self.scan.is_none() {
            egui::CentralPanel::default().show(ui, |ui| self.welcome(ui));
            return;
        }
        if ui.input(|i| i.key_pressed(Key::Escape)) {
            self.pinned = None;
        }
        egui::Panel::right("side")
            .resizable(true)
            .default_size(380.0)
            .min_size(260.0)
            .max_size(1200.0)
            .show(ui, |ui| self.side_panel(ui, &palette));
        egui::CentralPanel::default().show(ui, |ui| self.map(ui));
    }
}

/// Frame timings, printed to stderr every two seconds. While measuring, the window
/// redraws every refresh, as a game does, so a gap longer than one refresh is a frame
/// the display wanted and didn't get — not a pause in input.
struct FrameStats {
    since: std::time::Instant,
    /// Per frame, milliseconds: since the frame before, whether the map was being
    /// moved, and the CPU time eframe spent on it (building, tessellating, drawing).
    gaps: Vec<(f32, bool)>,
    cpu: Vec<f32>,
}

impl Default for FrameStats {
    fn default() -> Self {
        FrameStats {
            since: std::time::Instant::now(),
            gaps: Vec::new(),
            cpu: Vec::new(),
        }
    }
}

impl FrameStats {
    fn record(
        &mut self,
        gap: f32,
        moving: bool,
        cpu: Option<f32>,
        gpu: Option<&Arc<Mutex<MapPainter>>>,
    ) {
        self.gaps.push((gap * 1e3, moving));
        self.cpu.extend(cpu.map(|c| c * 1e3));
        let elapsed = self.since.elapsed().as_secs_f32();
        if elapsed < 2.0 || self.cpu.is_empty() {
            return;
        }
        let at = |ms: &[f32], q: f32| ms[((ms.len() - 1) as f32 * q) as usize];
        let frames = self.gaps.len();
        // Gaps over half a second are the window hidden, not drawing.
        let mut gaps: Vec<(f32, bool)> = std::mem::take(&mut self.gaps)
            .into_iter()
            .filter(|&(g, _)| g < 500.0)
            .collect();
        gaps.sort_by(|a, b| a.0.total_cmp(&b.0));
        let all: Vec<f32> = gaps.iter().map(|g| g.0).collect();
        let refresh = all.get(all.len() / 2).copied().unwrap_or(16.7);
        let missed = |g: f32| ((g / refresh).round() as usize).saturating_sub(1);
        let dropped = |moving: bool| -> (usize, usize) {
            gaps.iter()
                .filter(|g| g.1 == moving)
                .fold((0, 0), |(n, d), g| (n + 1, d + missed(g.0)))
        };
        let ((moved, dropped_moving), (still, dropped_still)) = (dropped(true), dropped(false));
        let mut cpu = std::mem::take(&mut self.cpu);
        cpu.sort_by(f32::total_cmp);
        let (painting, uploads) = gpu.map_or((0.0, 0), |g| {
            g.lock().expect("not painting now").take_stats()
        });
        eprintln!(
            "refresh {refresh:.1} ms · moving: {dropped_moving} dropped of {moved} · still: {dropped_still} dropped of {still} · longest {:.1} ms · CPU per frame p50 {:.1}, max {:.1} ms · tiles {:.1} ms/frame, {uploads} updated",
            all.last().copied().unwrap_or(0.0),
            at(&cpu, 0.5),
            at(&cpu, 1.0),
            painting / frames as f32,
        );
        self.since = std::time::Instant::now();
    }
}

/// Asks for the password sudo needs. Whether it was submitted, or cancelled.
fn password_dialog(
    ctx: &egui::Context,
    palette: &Palette,
    scan: &Scan,
    text: &mut String,
    error: &Option<String>,
) -> (bool, bool) {
    let user = std::env::var("USER").unwrap_or_else(|_| "you".into());
    let (mut submit, mut cancel) = (false, false);
    egui::Modal::new(egui::Id::new("password")).show(ctx, |ui| {
        ui.set_width(360.0);
        ui.heading("Scanning needs root");
        ui.label(format!(
            "btrmaps reads {} as root with sudo. Enter the password for {user}.",
            scan.target.display()
        ));
        if let Some(e) = error {
            ui.colored_label(palette.error, e);
        }
        let field = ui.add(
            egui::TextEdit::singleline(text)
                .password(true)
                .hint_text("Password")
                .desired_width(f32::INFINITY),
        );
        submit = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        // Keep typing going to the field, except on the frame Enter releases it.
        if !field.has_focus() && !submit {
            field.request_focus();
        }
        ui.horizontal(|ui| {
            submit |= ui.button("Scan").clicked();
            cancel = ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape));
        });
    });
    (submit, cancel)
}

/// Asks before deleting anything for good: Some(true) to delete, Some(false) to cancel.
fn delete_dialog(ctx: &egui::Context, palette: &Palette, targets: &[PathBuf]) -> Option<bool> {
    let mut answer = None;
    egui::Modal::new(egui::Id::new("delete")).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.heading("Delete permanently?");
        ui.label("This can't be undone.");
        let font = egui::TextStyle::Monospace.resolve(ui.style());
        for p in targets.iter().take(8) {
            let text = ellipsize_middle(ui.ctx(), &p.display().to_string(), &font, 400.0);
            ui.label(RichText::new(text).monospace());
        }
        if targets.len() > 8 {
            ui.weak(format!("… and {} more", targets.len() - 8));
        }
        ui.horizontal(|ui| {
            let delete = egui::Button::new(RichText::new("Delete").color(palette.error));
            if ui.add(delete).clicked() {
                answer = Some(true);
            }
            if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                answer = Some(false);
            }
        });
    });
    answer
}

/// Trash or delete every target; what happened, for the status bar.
fn removal(targets: &[PathBuf], trash: bool) -> String {
    if trash {
        return match trash::delete_all(targets) {
            Ok(()) => {
                "Moved to Trash. Space frees when the Trash is emptied; Refresh to update the map."
                    .into()
            }
            Err(e) => format!("Couldn't move to Trash: {e}"),
        };
    }
    let failed: Vec<String> = targets
        .iter()
        .filter_map(|p| {
            let is_dir = p.symlink_metadata().is_ok_and(|m| m.is_dir());
            let removed = match is_dir {
                true => std::fs::remove_dir_all(p),
                false => std::fs::remove_file(p),
            };
            removed.err().map(|e| format!("{}: {e}", p.display()))
        })
        .collect();
    match failed.as_slice() {
        [] => format!(
            "Deleted {} item(s). Refresh to update the map.",
            targets.len()
        ),
        [one] => format!("Couldn't delete {one}"),
        many => format!("Couldn't delete {} items, e.g. {}", many.len(), many[0]),
    }
}

/// The share of each legend entry, as one bar.
fn category_bar(ui: &mut egui::Ui, side: &Side) {
    let total: u64 = side.cats.iter().map(|c| c.bytes).sum::<u64>().max(1);
    let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0), Sense::hover());
    let mut x = bar.left();
    for c in &side.cats {
        let w = bar.width() * c.bytes as f32 / total as f32;
        let r = Rect::from_min_size(pos2(x, bar.top()), vec2((w - 2.0).max(2.0), bar.height()));
        ui.painter().rect_filled(r, CornerRadius::same(2), c.color);
        x += w;
    }
}

/// A legend entry laid out as one piece, so wrapping never splits it.
fn chip(ui: &mut egui::Ui, color: Color32, label: &str, size: &str) {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let (text, weak) = (ui.visuals().text_color(), ui.visuals().weak_text_color());
    let name = ui
        .painter()
        .layout_no_wrap(label.to_string(), font.clone(), text);
    let amount = ui.painter().layout_no_wrap(size.to_string(), font, weak);
    let height = name.size().y;
    let width = 10.0 + 6.0 + name.size().x + 6.0 + amount.size().x;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let mid = rect.left_center();
    let dot = Rect::from_center_size(mid + vec2(5.0, 0.0), Vec2::splat(10.0));
    ui.painter().rect_filled(dot, CornerRadius::same(2), color);
    let name_at = pos2(rect.left() + 16.0, rect.top());
    let amount_at = name_at + vec2(name.size().x + 6.0, 0.0);
    ui.painter().galley(name_at, name, text);
    ui.painter().galley(amount_at, amount, weak);
}

fn swatch(ui: &mut egui::Ui, color: Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::same(2), color);
}

/// How many characters to keep from the front and back of a string whose characters are
/// `widths` wide so that, with an ellipsis of `ellipsis` between them, it fits `width`.
/// None when it fits whole. Takes from both ends in turn, the end first: in a path that
/// is where the file name is.
fn cut_middle(widths: &[f32], ellipsis: f32, width: f32) -> Option<(usize, usize)> {
    if widths.iter().sum::<f32>() <= width {
        return None;
    }
    let (mut front, mut back, mut used) = (0, 0, ellipsis);
    while front + back < widths.len() {
        let from_back = back <= front;
        let i = match from_back {
            true => widths.len() - 1 - back,
            false => front,
        };
        if used + widths[i] > width {
            break;
        }
        used += widths[i];
        match from_back {
            true => back += 1,
            false => front += 1,
        }
    }
    Some((front, back))
}

/// `text` fitted into `width` points by replacing its middle with "…".
fn ellipsize_middle(ctx: &egui::Context, text: &str, font: &egui::FontId, width: f32) -> String {
    let chars: Vec<char> = text.chars().collect();
    let (widths, ellipsis) = ctx.fonts_mut(|f| {
        let widths: Vec<f32> = chars.iter().map(|&c| f.glyph_width(font, c)).collect();
        (widths, f.glyph_width(font, '…'))
    });
    match cut_middle(&widths, ellipsis, width) {
        None => text.to_string(),
        Some((front, back)) => {
            let head: String = chars[..front].iter().collect();
            let tail: String = chars[chars.len() - back..].iter().collect();
            format!("{head}…{tail}")
        }
    }
}

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("btrmaps")
            .with_app_id("btrmaps")
            .with_inner_size([1400.0, 900.0]),
        ..Default::default()
    };
    eframe::run_native(
        "btrmaps",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_names_are_cut_in_the_middle_keeping_both_ends() {
        let w = [1.0; 10];
        assert_eq!(cut_middle(&w, 1.0, 10.0), None, "fits whole");
        assert_eq!(
            cut_middle(&w, 1.0, 7.0),
            Some((3, 3)),
            "front and back in turn"
        );
        assert_eq!(cut_middle(&w, 1.0, 4.0), Some((1, 2)), "the end first");
        assert_eq!(cut_middle(&w, 1.0, 0.5), Some((0, 0)));
    }
}
