//! The egui application: matrix grid, EQ editor with draggable response curve, setup page.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, RichText, Sense, Stroke, StrokeKind, Ui, Vec2,
    WidgetInfo, WidgetType,
};
use egui_plot::{HLine, Line, Plot, PlotBounds, PlotPoint, Points};
use linapo_core::apo::{layout_for, parse_file};
use linapo_core::matrix::lin_to_db;
use linapo_core::Setup;

use crate::backend::{Backend, Event, Job, Sink, Status, Worker};
use linapo_core::profiles::{valid_name, Workspace};
use linapo_pw::WatchLock;

mod profile_bar;
pub use profile_bar::ProfileDialog;
use crate::model::{self, KINDS};
use crate::picker::{with_json_extension, FilePicker, NativePicker, Purpose};
use crate::plot_math as pm;

pub const ACCENT: Color32 = Color32::from_rgb(64, 196, 170);
pub const WARN: Color32 = Color32::from_rgb(240, 170, 60);
pub const BAD: Color32 = Color32::from_rgb(230, 100, 100);
pub const OK: Color32 = Color32::from_rgb(90, 200, 120);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Matrix,
    Equalizer,
    Setup,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Wait this long after the last edit before pushing values to PipeWire.
    pub debounce_apply: Duration,
    /// Wait this long after the last edit before writing the setup file.
    pub debounce_save: Duration,
    /// Edits closer together than this form one undo step (so a whole drag is a single step).
    pub history_gap: Duration,
    /// How often the output device is checked (profile bar, automatic switching).
    pub poll_interval: Duration,
    /// Wake the UI every `poll_interval` even when nothing happens, so the device is re-read while
    /// the window is idle. (Tests switch this off: their harness treats it as a never-ending repaint.)
    pub repaint_while_idle: bool,
    /// Where the automatic-switching lock lives (`None`: the standard place; tests use their own).
    pub watch_lock_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            debounce_apply: Duration::from_millis(60),
            debounce_save: Duration::from_millis(600),
            history_gap: Duration::from_millis(400),
            poll_interval: Duration::from_millis(500),
            repaint_while_idle: true,
            watch_lock_path: None,
        }
    }
}

struct Message {
    text: String,
    error: bool,
    at: Instant,
}

pub struct App {
    pub setup: Setup,
    pub path: Option<PathBuf>,
    pub tab: Tab,
    pub selected_eq: usize,
    pub selected_filter: Option<usize>,
    /// Output channel whose combined response is drawn in the EQ plot.
    pub monitor: usize,
    pub status: Option<Status>,
    pub busy: bool,
    pub sinks: Vec<Sink>,
    pub import_path: String,
    pub file_field: String,
    worker: Worker,
    cfg: Config,
    message: Option<Message>,
    dirty_since: Option<Instant>,
    save_since: Option<Instant>,
    pub saved: bool,
    drag_filter: Option<usize>,
    /// Undo history: snapshots from before each gesture.
    undo: Vec<Setup>,
    redo: Vec<Setup>,
    /// The setup as of the last finished gesture.
    committed: Setup,
    /// State from before the gesture currently in progress.
    pending_before: Option<Setup>,
    gesture_at: Option<Instant>,
    picker: Arc<dyn FilePicker>,
    wake: Arc<dyn Fn() + Send + Sync>,
    dialog_tx: Sender<(Purpose, Option<PathBuf>, Duration)>,
    dialog_rx: Receiver<(Purpose, Option<PathBuf>, Duration)>,
    /// A file dialog is open (they are modal, so a second one must not start).
    pub dialog_open: bool,
    last_dir: Option<PathBuf>,
    workspace: Option<Workspace>,
    profiles: Vec<String>,
    loaded_profile: String,
    pub current_device: Option<Sink>,
    watch_lock: Option<WatchLock>,
    /// Automatic switching is on but another process holds the lock.
    pub watch_elsewhere: bool,
    last_device: Option<String>,
    next_poll: Instant,
    pub profile_dialog: Option<ProfileDialog>,
}

const MAX_HISTORY: usize = 200;

/// Above this many filters the plot only shows a handle for the selected one.
const MANY_FILTERS: usize = 24;

impl App {
    pub fn new(
        setup: Setup,
        path: Option<PathBuf>,
        backend: impl Backend,
        wake: impl Fn() + Send + Sync + 'static,
        cfg: Config,
    ) -> App {
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let for_worker = wake.clone();
        let worker = Worker::spawn(backend, move || for_worker());
        let (dialog_tx, dialog_rx) = channel();
        worker.send(Job::Refresh(setup.clone()));
        worker.send(Job::ListSinks);
        App {
            file_field: path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            committed: setup.clone(),
            setup,
            path,
            tab: Tab::Matrix,
            selected_eq: 0,
            selected_filter: None,
            monitor: 0,
            status: None,
            busy: false,
            sinks: Vec::new(),
            import_path: String::new(),
            worker,
            cfg,
            message: None,
            dirty_since: None,
            save_since: None,
            saved: true,
            drag_filter: None,
            undo: Vec::new(),
            redo: Vec::new(),
            pending_before: None,
            gesture_at: None,
            picker: Arc::new(NativePicker),
            wake,
            dialog_tx,
            dialog_rx,
            dialog_open: false,
            last_dir: None,
            workspace: None,
            profiles: Vec::new(),
            loaded_profile: String::new(),
            current_device: None,
            watch_lock: None,
            watch_elsewhere: false,
            last_device: None,
            next_poll: Instant::now(),
            profile_dialog: None,
        }
    }

    /// Use another file-dialog implementation (tests).
    pub fn with_picker(mut self, picker: Arc<dyn FilePicker>) -> App {
        self.picker = picker;
        self
    }

    // ------------------------------------------------------------ state

    /// Call after every user edit: records undo history and schedules a live apply and an autosave.
    pub fn touch(&mut self) {
        if self.pending_before.is_none() {
            self.pending_before = Some(self.committed.clone());
        }
        self.gesture_at = Some(Instant::now());
        self.mark_dirty();
    }

    /// Schedule apply + autosave without touching the history (used by undo/redo).
    fn mark_dirty(&mut self) {
        let now = Instant::now();
        self.dirty_since = Some(now);
        self.save_since = Some(now);
        self.saved = false;
    }

    /// Close the gesture in progress: it becomes one undo step.
    pub fn commit_history(&mut self) {
        if let Some(before) = self.pending_before.take() {
            if before != self.setup {
                self.undo.push(before);
                if self.undo.len() > MAX_HISTORY {
                    self.undo.remove(0);
                }
                self.redo.clear();
            }
            self.committed = self.setup.clone();
        }
        self.gesture_at = None;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty() || self.pending_before.as_ref().is_some_and(|b| *b != self.setup)
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    fn restore(&mut self, setup: Setup) {
        self.setup = setup.clone();
        self.committed = setup;
        self.selected_eq = self.selected_eq.min(self.setup.eqs.len().saturating_sub(1));
        self.selected_filter = None;
        self.monitor = self.monitor.min(self.setup.output_channels.len().saturating_sub(1));
        self.mark_dirty();
    }

    pub fn undo(&mut self) {
        self.commit_history();
        if let Some(prev) = self.undo.pop() {
            self.redo.push(self.setup.clone());
            self.restore(prev);
        }
    }

    pub fn redo(&mut self) {
        self.commit_history();
        if let Some(next) = self.redo.pop() {
            self.undo.push(self.setup.clone());
            self.restore(next);
        }
    }

    pub fn say(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some(Message { text: text.into(), error, at: Instant::now() });
    }

    pub fn message_text(&self) -> Option<&str> {
        self.message.as_ref().map(|m| m.text.as_str())
    }

    pub fn message_is_error(&self) -> bool {
        self.message.as_ref().is_some_and(|m| m.error)
    }

    fn poll_worker(&mut self) {
        for ev in self.worker.poll() {
            match ev {
                Event::Status(s) => self.status = Some(s),
                Event::Message(m) => self.say(m, false),
                Event::Error(m) => self.say(m, true),
                Event::Sinks(s) => self.sinks = s,
                Event::Device(d) => self.on_device(d),
                Event::Busy(b) => self.busy = b,
            }
        }
    }

    fn flush(&mut self, ctx: &egui::Context) {
        if let Some(t) = self.dirty_since {
            let el = t.elapsed();
            if el >= self.cfg.debounce_apply {
                self.worker.send(Job::Apply(self.setup.clone()));
                self.dirty_since = None;
            } else {
                ctx.request_repaint_after(self.cfg.debounce_apply - el);
            }
        }
        if let Some(t) = self.save_since {
            let el = t.elapsed();
            if el >= self.cfg.debounce_save {
                self.save_now();
            } else {
                ctx.request_repaint_after(self.cfg.debounce_save - el);
            }
        }
        if let Some(t) = self.gesture_at {
            let el = t.elapsed();
            if el >= self.cfg.history_gap {
                self.commit_history();
            } else {
                ctx.request_repaint_after(self.cfg.history_gap - el);
            }
        }
        if let Some(m) = &self.message {
            if m.at.elapsed() > Duration::from_secs(8) {
                self.message = None;
            } else {
                ctx.request_repaint_after(Duration::from_secs(1));
            }
        }
    }

    pub fn save_now(&mut self) {
        self.save_since = None;
        let Some(path) = self.path.clone() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match self.setup.save(&path) {
            Ok(()) => self.saved = true,
            Err(e) => self.say(format!("Could not save {}: {e}", path.display()), true),
        }
    }

    /// Open a native file dialog on a background thread; the result arrives through `poll_dialogs`.
    pub fn browse(&mut self, purpose: Purpose) {
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let (picker, tx, wake, dir) = (self.picker.clone(), self.dialog_tx.clone(), self.wake.clone(), self.last_dir.clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            let result = picker.pick(purpose, dir.as_deref());
            let _ = tx.send((purpose, result, started.elapsed()));
            wake();
        });
    }

    fn poll_dialogs(&mut self) {
        while let Ok((purpose, result, took)) = self.dialog_rx.try_recv() {
            self.dialog_open = false;
            let Some(path) = result else {
                // Failing to open at all is usually instant. A slower "nothing" is either the user
                // cancelling or a portal that failed slowly; the app cannot tell which, so it
                // says so gently and mentions the alternatives.
                if took < Duration::from_millis(250) {
                    self.say("No file dialog could be opened on this system. Paste the path into the box, or drop the file on the window.", true);
                } else {
                    self.say("No file chosen. (If no dialog appeared, paste the path into the box or drop the file on the window.)", false);
                }
                continue;
            };
            self.last_dir = path.parent().map(Path::to_path_buf);
            match purpose {
                Purpose::ImportEqs => {
                    self.import_path = path.display().to_string();
                    self.import_apo(&path, false);
                }
                Purpose::ImportReplace => {
                    self.import_path = path.display().to_string();
                    self.import_apo(&path, true);
                }
                Purpose::OpenSetup => {
                    self.file_field = path.display().to_string();
                    self.load_file(&path);
                }
                Purpose::SaveSetup => {
                    let path = with_json_extension(path);
                    if self.workspace.is_some() {
                        // Profiles live in the store; a copy elsewhere does not change what is edited.
                        match self.setup.save(&path) {
                            Ok(()) => self.say(format!("Saved a copy to {}", path.display()), false),
                            Err(e) => self.say(format!("Could not save {}: {e}", path.display()), true),
                        }
                    } else {
                        self.file_field = path.display().to_string();
                        self.path = Some(path.clone());
                        self.save_now();
                        if self.saved {
                            self.say(format!("Saved {}", path.display()), false);
                        }
                    }
                }
            }
        }
    }

    /// Remove the selected filter of the selected EQ (Delete key).
    pub fn delete_selected_filter(&mut self) {
        let (Some(fi), Some(eq)) = (self.selected_filter, self.setup.eqs.get_mut(self.selected_eq)) else { return };
        if fi < eq.filters.len() {
            eq.filters.remove(fi);
            self.selected_filter = None;
            self.touch();
        }
    }

    pub fn install(&mut self) {
        self.worker.send(Job::Install(self.setup.clone()));
    }

    pub fn load_file(&mut self, path: &Path) {
        match Setup::load(path) {
            Ok(s) => {
                self.setup = s;
                if self.workspace.is_none() {
                    self.path = Some(path.to_path_buf());
                }
                self.selected_eq = 0;
                self.selected_filter = None;
                self.monitor = 0;
                self.say(format!("Loaded {}", path.display()), false);
                self.touch();
            }
            Err(e) => self.say(format!("Could not load {}: {e}", path.display()), true),
        }
    }

    /// Import an Equalizer APO profile, either as additional EQs or replacing everything.
    pub fn import_apo(&mut self, path: &Path, replace: bool) {
        let profile = match parse_file(path) {
            Ok(p) => p,
            Err(e) => return self.say(format!("Could not read {}: {e}", path.display()), true),
        };
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "profile".into());
        let mut warnings: Vec<String> = profile.diagnostics.iter().map(|d| d.to_string()).collect();
        let summary;
        if replace {
            let layout = self.setup.output_channels.clone();
            let (mut s, w) = Setup::from_profile(&profile, &layout, &name);
            warnings.extend(w);
            s.master_gain_db = self.setup.master_gain_db;
            s.output_target = self.setup.output_target.clone();
            self.setup = s;
            self.selected_eq = 0;
            summary = format!("Replaced the setup with '{name}'");
        } else {
            let first = self.setup.eqs.len();
            let (n, w) = model::import_eqs(&mut self.setup, &profile, &name);
            warnings.extend(w);
            if n > 0 {
                self.selected_eq = first;
            }
            self.tab = Tab::Equalizer;
            summary = format!("Imported {n} EQ(s) from '{name}'");
        }
        self.selected_filter = None;
        let text = match warnings.len() {
            0 => summary,
            1 => format!("{summary}. Note: {}", warnings[0]),
            n => format!("{summary}. {n} notes, first: {}", warnings[0]),
        };
        self.say(text, false);
        self.touch();
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for f in dropped {
            if let Some(p) = f.path {
                if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")) {
                    self.load_file(&p);
                } else {
                    self.import_apo(&p, false);
                }
            }
        }
    }

    // ------------------------------------------------------------ frame

    pub fn ui(&mut self, ctx: &egui::Context) {
        self.poll_worker();
        self.poll_dialogs();
        self.poll_device_if_due(ctx);
        self.handle_drops(ctx);
        if self.tab == Tab::Equalizer && !ctx.wants_keyboard_input() && ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
            self.delete_selected_filter();
        }
        if !ctx.wants_keyboard_input() {
            let redo_shift = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Z);
            let redo_y = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Y);
            let undo = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Z);
            if ctx.input_mut(|i| i.consume_shortcut(&redo_shift) || i.consume_shortcut(&redo_y)) {
                self.redo();
            } else if ctx.input_mut(|i| i.consume_shortcut(&undo)) {
                self.undo();
            }
        }
        self.selected_eq = self.selected_eq.min(self.setup.eqs.len().saturating_sub(1));
        self.monitor = self.monitor.min(self.setup.output_channels.len().saturating_sub(1));

        egui::TopBottomPanel::top("top").show(ctx, |ui| self.top_bar(ui));
        if self.has_workspace() {
            egui::TopBottomPanel::top("profiles").show(ctx, |ui| self.profile_bar(ui));
        }
        egui::TopBottomPanel::top("banner").show_separator_line(false).show(ctx, |ui| self.banner(ui));
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status_bar(ui));
        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Matrix => self.matrix_tab(ui),
            Tab::Equalizer => self.eq_tab(ui),
            Tab::Setup => self.setup_tab(ui),
        });
        self.profile_dialog_window(ctx);
        self.flush(ctx);
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("LinAPO").heading().color(ACCENT));
            ui.add_space(14.0);
            for (tab, name) in [(Tab::Matrix, "Matrix"), (Tab::Equalizer, "Equalizer"), (Tab::Setup, "Setup")] {
                if ui.selectable_label(self.tab == tab, RichText::new(name).size(15.0)).clicked() {
                    self.tab = tab;
                }
            }
            ui.add_space(10.0);
            if ui.add_enabled(self.can_undo(), egui::Button::new("Undo")).on_hover_text("Ctrl+Z").clicked() {
                self.undo();
            }
            if ui.add_enabled(self.can_redo(), egui::Button::new("Redo")).on_hover_text("Ctrl+Shift+Z").clicked() {
                self.redo();
            }
            let bypass = self.setup.bypass_eq;
            let r = ui
                .selectable_label(bypass, RichText::new("Bypass EQ").color(if bypass { WARN } else { ui.visuals().text_color() }))
                .on_hover_text("Switch every equalizer off to compare with the original sound. Matrix and master gain still apply.");
            if r.clicked() {
                self.setup.bypass_eq = !bypass;
                self.touch();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (color, text) = match (self.busy, self.status) {
                    (true, _) => (WARN, "Restarting…"),
                    (_, Some(Status::Live)) => (OK, "Live"),
                    (_, Some(Status::NeedsRestart)) => (WARN, "Restart needed"),
                    (_, Some(Status::NotInstalled)) => (BAD, "Not installed"),
                    (_, Some(Status::NotRunning)) => (WARN, "Not running"),
                    (_, None) => (Color32::GRAY, "Checking…"),
                };
                ui.label(RichText::new(text).color(color));
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                ui.painter().circle_filled(dot.center(), 5.0, color);
                ui.add_space(18.0);
                let r = ui.add(
                    egui::Slider::new(&mut self.setup.master_gain_db, -60.0..=12.0)
                        .suffix(" dB")
                        .text("Master gain")
                        .fixed_decimals(1),
                );
                if r.changed() {
                    self.touch();
                }
            });
        });
        ui.add_space(6.0);
    }

    fn banner(&mut self, ui: &mut Ui) {
        let (color, text, button): (Color32, &str, Option<&str>) = if self.busy {
            (WARN, "Restarting the audio engine…", None)
        } else {
            match self.status {
                Some(Status::NotInstalled) => (BAD, "LinAPO is not installed in PipeWire yet.", Some("Install")),
                Some(Status::NeedsRestart) => (
                    WARN,
                    "Channels, filters or the output device changed. Restart the audio engine to apply that.",
                    Some("Apply and restart audio"),
                ),
                Some(Status::NotRunning) => (WARN, "Installed, but the LinAPO sink is not running in PipeWire.", Some("Restart audio")),
                _ => return,
            }
        };
        egui::Frame::new()
            .fill(color.gamma_multiply(0.16))
            .stroke(Stroke::new(1.0, color))
            .inner_margin(Margin::symmetric(12, 7))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.busy {
                        ui.spinner();
                    }
                    ui.label(text);
                    if let Some(b) = button {
                        if ui.button(b).clicked() {
                            self.install();
                        }
                    }
                });
            });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if let Some(m) = &self.message {
                ui.label(RichText::new(&m.text).color(if m.error { BAD } else { ui.visuals().text_color() }));
            } else {
                ui.label(RichText::new("Changes apply instantly and are saved automatically.").weak());
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(p) = &self.path {
                    ui.label(RichText::new(format!("{}{}", p.display(), if self.saved { "" } else { "  (unsaved)" })).weak());
                }
            });
        });
    }

    // ------------------------------------------------------------ matrix

    fn matrix_tab(&mut self, ui: &mut Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Presets:");
            if ui.button("Straight through").on_hover_text("Same-named channels connect directly").clicked() {
                model::matrix_identity(&mut self.setup);
                self.touch();
            }
            if ui.button("Downmix to stereo").on_hover_text("Centre and surrounds at -3 dB, LFE dropped").clicked() {
                if model::matrix_downmix_to_stereo(&mut self.setup) {
                    self.touch();
                } else {
                    self.say("Downmix needs L and R output channels", true);
                }
            }
            if ui.button("Clear all").clicked() {
                model::matrix_clear(&mut self.setup);
                self.touch();
            }
        });
        ui.label(
            RichText::new("Click a cell to connect / disconnect · drag up and down to change its volume · scroll for ±0.5 dB · right-click for exact value and polarity")
                .weak()
                .small(),
        );
        ui.add_space(10.0);

        let ins: Vec<String> = self.setup.input_channels.iter().map(|c| c.name().to_string()).collect();
        let outs: Vec<String> = self.setup.output_channels.iter().map(|c| c.name().to_string()).collect();
        let mut events: Vec<(usize, usize, CellEvent)> = Vec::new();
        let mut jump_to_eq: Option<usize> = None;

        egui::ScrollArea::both().show(ui, |ui| {
            ui.label(RichText::new("Rows: inputs     Columns: outputs").weak().small());
            egui::Grid::new("matrix").spacing([6.0, 6.0]).show(ui, |ui| {
                ui.label("");
                for (o, name) in outs.iter().enumerate() {
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(name).strong().size(15.0));
                        let n = self.setup.chain_for_output(&self.setup.output_channels[o]).filters.len();
                        let r = ui.small_button(format!("{n} filter{}", if n == 1 { "" } else { "s" })).on_hover_text("Open the equalizer for this output");
                        r.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, format!("equalizer for output {name}")));
                        if r.clicked() {
                            jump_to_eq = Some(o);
                        }
                    });
                }
                ui.end_row();
                for (i, iname) in ins.iter().enumerate() {
                    ui.label(RichText::new(iname).strong().size(15.0));
                    for (o, oname) in outs.iter().enumerate() {
                        let ev = route_cell(ui, &format!("{iname} to {oname}"), self.setup.matrix.get(i, o));
                        if !matches!(ev, CellEvent::None) {
                            events.push((i, o, ev));
                        }
                    }
                    ui.end_row();
                }
            });
        });

        for (i, o, ev) in events {
            let db = lin_to_db(self.setup.matrix.get(i, o)).unwrap_or(0.0);
            match ev {
                CellEvent::None => {}
                CellEvent::Toggle => model::toggle_route(&mut self.setup, i, o),
                CellEvent::Off => self.setup.matrix.set(i, o, 0.0),
                CellEvent::SetDb(v) => {
                    if self.setup.matrix.get(i, o) == 0.0 {
                        self.setup.matrix.set(i, o, 1.0);
                    }
                    model::set_route_db(&mut self.setup, i, o, v.clamp(-60.0, 12.0));
                }
                CellEvent::Nudge(d) => {
                    if self.setup.matrix.get(i, o) == 0.0 {
                        self.setup.matrix.set(i, o, 1.0);
                    }
                    model::set_route_db(&mut self.setup, i, o, (db + d).clamp(-60.0, 12.0));
                }
                CellEvent::Invert(v) => model::set_route_inverted(&mut self.setup, i, o, v),
            }
            self.touch();
        }
        if let Some(o) = jump_to_eq {
            self.tab = Tab::Equalizer;
            self.monitor = o;
            let ch = self.setup.output_channels[o].clone();
            if let Some(p) = self.setup.eqs.iter().position(|e| e.channels.contains(&ch)) {
                self.selected_eq = p;
                self.selected_filter = None;
            }
        }
    }

    // ------------------------------------------------------------ equalizer

    fn eq_tab(&mut self, ui: &mut Ui) {
        egui::SidePanel::left("eq_list").exact_width(240.0).resizable(false).show_inside(ui, |ui| self.eq_list(ui));
        egui::CentralPanel::default().show_inside(ui, |ui| {
            if self.setup.eqs.is_empty() {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No equalizers yet").heading());
                    ui.label("Create one, or import an Equalizer APO profile (drop a .txt file on the window).");
                    if ui.button("Create an equalizer").clicked() {
                        self.selected_eq = model::add_eq(&mut self.setup);
                        self.touch();
                    }
                });
                return;
            }
            self.eq_plot(ui);
            ui.add_space(6.0);
            self.eq_editor(ui);
        });
    }

    fn eq_list(&mut self, ui: &mut Ui) {
        ui.add_space(6.0);
        ui.label(RichText::new("Equalizers").strong());
        ui.add_space(4.0);
        let mut select = None;
        let mut touched = false;
        egui::ScrollArea::vertical().max_height(ui.available_height() - 150.0).show(ui, |ui| {
            for (i, eq) in self.setup.eqs.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    let r = ui.add(egui::Checkbox::without_text(&mut eq.enabled));
                    let (name, on) = (eq.name.clone(), eq.enabled);
                    r.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, on, format!("enable {name}")));
                    if r.changed() {
                        touched = true;
                    }
                    let chans = eq.channels.iter().map(|c| c.name()).collect::<Vec<_>>().join(" ");
                    let text = if eq.enabled { RichText::new(&eq.name) } else { RichText::new(&eq.name).weak() };
                    if ui.selectable_label(i == self.selected_eq, text).on_hover_text(format!("{} filter(s) on: {chans}", eq.filters.len())).clicked() {
                        select = Some(i);
                    }
                });
            }
        });
        if let Some(i) = select {
            self.selected_eq = i;
            self.selected_filter = None;
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("New").clicked() {
                self.selected_eq = model::add_eq(&mut self.setup);
                self.selected_filter = None;
                touched = true;
            }
            if ui.add_enabled(!self.setup.eqs.is_empty(), egui::Button::new("Duplicate")).clicked() {
                self.selected_eq = model::duplicate_eq(&mut self.setup, self.selected_eq);
                touched = true;
            }
            if ui.add_enabled(!self.setup.eqs.is_empty(), egui::Button::new("Delete")).clicked() {
                self.selected_eq = model::remove_eq(&mut self.setup, self.selected_eq);
                self.selected_filter = None;
                touched = true;
            }
        });
        ui.add_space(10.0);
        ui.separator();
        ui.label(RichText::new("Import Equalizer APO profile").strong());
        if ui.add_enabled(!self.dialog_open, egui::Button::new("Browse\u{2026}")).clicked() {
            self.browse(Purpose::ImportEqs);
        }
        ui.add(egui::TextEdit::singleline(&mut self.import_path).hint_text("or paste a path / drop a file").desired_width(f32::INFINITY));
        if ui.add_enabled(!self.import_path.trim().is_empty(), egui::Button::new("Import as new EQs")).clicked() {
            let p = PathBuf::from(self.import_path.trim());
            self.import_apo(&p, false);
        }
        if touched {
            self.touch();
        }
    }

    fn eq_plot(&mut self, ui: &mut Ui) {
        let outputs = self.setup.output_channels.clone();
        ui.horizontal(|ui| {
            ui.label("Output shown:");
            egui::ComboBox::from_id_salt("monitor").selected_text(outputs[self.monitor].name()).show_ui(ui, |ui| {
                for (i, c) in outputs.iter().enumerate() {
                    ui.selectable_value(&mut self.monitor, i, c.name());
                }
            });
            ui.label(RichText::new("Drag a dot to change frequency and gain · scroll over it for Q · double-click to add a filter").weak().small());
            if self.setup.bypass_eq {
                ui.label(RichText::new("EQ bypassed").color(WARN).strong());
            }
        });

        let overall = self.setup.chain_for_output(&outputs[self.monitor]);
        let overall_curve = pm::curve(&overall, 360);
        let eq = &self.setup.eqs[self.selected_eq];
        let sel_curve = pm::curve(&model::eq_chain(eq), 360);
        let handles_plot: Vec<[f64; 2]> = eq.filters.iter().map(pm::handle_position).collect();
        let handle_on: Vec<bool> = eq.filters.iter().map(|f| f.enabled).collect();
        // An EQ with dozens of filters (an imported GraphicEQ, say) would be a wall of dots:
        // then only the selected filter gets a handle.
        let crowded = eq.filters.len() > MANY_FILTERS;
        let handle_visible: Vec<bool> = (0..eq.filters.len()).map(|i| !crowded || self.selected_filter == Some(i)).collect();
        let bypassed = self.setup.bypass_eq;
        let extra: Vec<f64> = handles_plot.iter().map(|h| h[1]).collect();
        let r = pm::y_range(&[&overall_curve, &sel_curve], &extra);
        let (x0, x1) = (pm::x_of(pm::F_MIN), pm::x_of(pm::F_MAX));
        let selected_filter = self.selected_filter;
        let height = (ui.available_height() * 0.45).clamp(190.0, 440.0);
        let marks = pm::grid_marks(x0, x1);
        let monitor_name = format!("Output {}", outputs[self.monitor]);
        let eq_enabled = eq.enabled;

        let resp = Plot::new("response")
            .height(height)
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_drag(false)
            .allow_boxed_zoom(false)
            .allow_double_click_reset(false)
            .auto_bounds(false)
            .x_grid_spacer(move |_| {
                marks
                    .iter()
                    .map(|(x, major)| {
                        let m = (10f64.powf(*x) / 10f64.powf(x.floor())).round() as i32;
                        let step_size = if *major { 1.0 } else if m == 2 || m == 5 { 0.5 } else { 0.1 };
                        egui_plot::GridMark { value: *x, step_size }
                    })
                    .collect()
            })
            .x_axis_formatter(|mark, _| {
                let f = pm::freq_of(mark.value);
                let m = f / 10f64.powf(f.log10().floor());
                if (m - 1.0).abs() < 0.01 || (m - 2.0).abs() < 0.01 || (m - 5.0).abs() < 0.01 {
                    pm::freq_label(f.round())
                } else {
                    String::new()
                }
            })
            .y_axis_formatter(|mark, _| format!("{:+.0}", mark.value))
            .show(ui, |plot_ui| {
                plot_ui.set_plot_bounds(PlotBounds::from_min_max([x0, -r], [x1, r]));
                plot_ui.hline(HLine::new("0 dB", 0.0).color(Color32::from_gray(110)).width(1.0));
                plot_ui.line(Line::new(monitor_name, overall_curve.clone()).color(Color32::from_gray(210)).width(1.5));
                let sel_color = if eq_enabled && !bypassed { ACCENT } else { Color32::from_gray(120) };
                plot_ui.line(Line::new("Selected EQ", sel_curve.clone()).color(sel_color).width(2.5));
                for (i, h) in handles_plot.iter().enumerate() {
                    if !handle_visible[i] {
                        continue;
                    }
                    let is_sel = selected_filter == Some(i);
                    plot_ui.points(
                        Points::new(format!("filter {}", i + 1), vec![*h])
                            .radius(if is_sel { 8.0 } else { 6.0 })
                            .color(if !handle_on[i] { Color32::from_gray(90) } else if is_sel { Color32::WHITE } else { sel_color })
                            .filled(true),
                    );
                }
            });

        let tr = resp.transform;
        let resp = resp.response;
        let handles: Vec<(usize, [f32; 2])> = handles_plot
            .iter()
            .enumerate()
            .filter(|(i, _)| handle_visible[*i])
            .map(|(i, h)| {
                let p = tr.position_from_point(&PlotPoint::new(h[0], h[1]));
                (i, [p.x, p.y])
            })
            .collect();

        let hover = resp.hover_pos().map(|p| [p.x, p.y]);
        let near_hover = hover.and_then(|h| pm::nearest_handle(&handles, h, 16.0));
        if near_hover.is_some() || self.drag_filter.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }

        let mut changed = false;
        if resp.drag_started() {
            if let Some(o) = ui.input(|i| i.pointer.press_origin()) {
                self.drag_filter = pm::nearest_handle(&handles, [o.x, o.y], 16.0);
                if self.drag_filter.is_some() {
                    self.selected_filter = self.drag_filter;
                }
            }
        }
        if let (true, Some(fi), Some(p)) = (resp.dragged(), self.drag_filter, resp.interact_pointer_pos()) {
            let v = tr.value_from_position(p);
            if let Some(f) = self.setup.eqs[self.selected_eq].filters.get_mut(fi) {
                pm::apply_drag(f, [v.x, v.y]);
                changed = true;
            }
        }
        if resp.drag_stopped() {
            self.drag_filter = None;
        }
        if resp.clicked() {
            self.selected_filter = near_hover;
        }
        if resp.double_clicked() && near_hover.is_none() {
            if let Some(p) = resp.interact_pointer_pos() {
                let v = tr.value_from_position(p);
                let i = model::add_filter_at(&mut self.setup.eqs[self.selected_eq], pm::freq_of(v.x), v.y);
                self.selected_filter = Some(i);
                changed = true;
            }
        }
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            let target = self.drag_filter.or(near_hover);
            if let (true, Some(fi)) = (scroll != 0.0, target) {
                if let Some(f) = self.setup.eqs[self.selected_eq].filters.get_mut(fi) {
                    if model::uses_q(f.kind) {
                        f.q = pm::scroll_q(f.q, scroll);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.touch();
        }
    }

    fn eq_editor(&mut self, ui: &mut Ui) {
        let outputs = self.setup.output_channels.clone();
        let sel = self.selected_eq;
        let mut changed = false;
        let mut remove: Option<usize> = None;
        let mut select_filter = self.selected_filter;
        let eq = &mut self.setup.eqs[sel];

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Name");
                changed |= ui.add(egui::TextEdit::singleline(&mut eq.name).desired_width(180.0)).changed();
                ui.add_space(12.0);
                ui.label("Preamp");
                changed |= ui
                    .add(egui::DragValue::new(&mut eq.preamp_db).range(-30.0..=12.0).speed(0.1).suffix(" dB").fixed_decimals(1))
                    .on_hover_text("Lower this if the EQ boosts and the sound distorts")
                    .changed();
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Applies to");
                for c in &outputs {
                    let on = eq.channels.contains(c);
                    if ui.selectable_label(on, c.name()).on_hover_text("Click to add or remove this output channel").clicked() {
                        model::toggle_eq_channel(eq, c, &outputs);
                        changed = true;
                    }
                }
                if ui.small_button("All").clicked() {
                    eq.channels = outputs.clone();
                    changed = true;
                }
                if ui.small_button("None").clicked() {
                    eq.channels.clear();
                    changed = true;
                }
            });
            if eq.filters.len() > MANY_FILTERS {
                ui.label(RichText::new(format!("{} filters. Click a row number to show its handle on the plot.", eq.filters.len())).weak().small());
            }
            if eq.channels.is_empty() {
                ui.label(RichText::new("Warning: this equalizer is not assigned to any output channel, so it has no effect.").color(WARN));
            }
            ui.add_space(6.0);

            egui::Grid::new("filters").num_columns(7).spacing([10.0, 6.0]).striped(true).show(ui, |ui| {
                for h in ["On", "#", "Type", "Frequency", "Gain", "Q", ""] {
                    ui.label(RichText::new(h).weak().small());
                }
                ui.end_row();
                for (i, f) in eq.filters.iter_mut().enumerate() {
                    let on = ui.add(egui::Checkbox::without_text(&mut f.enabled));
                    let state = f.enabled;
                    on.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, state, format!("enable filter {}", i + 1)));
                    changed |= on.changed();
                    let num_text = if f.enabled { RichText::new(format!("{}", i + 1)) } else { RichText::new(format!("{}", i + 1)).weak() };
                    let r = ui.selectable_label(select_filter == Some(i), num_text);
                    if r.clicked() {
                        select_filter = Some(i);
                    }
                    egui::ComboBox::from_id_salt(("kind", i)).selected_text(model::kind_name(f.kind)).width(130.0).show_ui(ui, |ui| {
                        for (k, name) in KINDS {
                            changed |= ui.selectable_value(&mut f.kind, k, name).changed();
                        }
                    });
                    let speed = (f.fc_hz * 0.01).max(0.5);
                    changed |= ui
                        .add(egui::DragValue::new(&mut f.fc_hz).range(10.0..=20_000.0).speed(speed).suffix(" Hz").max_decimals(1))
                        .changed();
                    changed |= ui
                        .add_enabled(
                            model::uses_gain(f.kind),
                            egui::DragValue::new(&mut f.gain_db).range(-30.0..=30.0).speed(0.05).suffix(" dB").fixed_decimals(1),
                        )
                        .changed();
                    changed |= ui
                        .add_enabled(
                            model::uses_q(f.kind),
                            egui::DragValue::new(&mut f.q).range(0.1..=30.0).speed(0.01).fixed_decimals(2),
                        )
                        .changed();
                    let del = ui.small_button("×");
                    del.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, format!("delete filter {}", i + 1)));
                    if del.clicked() {
                        remove = Some(i);
                    }
                    ui.end_row();
                }
            });
            if ui.button("Add filter").clicked() {
                select_filter = Some(model::add_filter(eq));
                changed = true;
            }
        });

        if let Some(i) = remove {
            self.setup.eqs[sel].filters.remove(i);
            select_filter = None;
            changed = true;
        }
        self.selected_filter = select_filter;
        if changed {
            self.touch();
        }
    }

    // ------------------------------------------------------------ setup

    fn setup_tab(&mut self, ui: &mut Ui) {
        ui.add_space(8.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            self.profiles_section(ui);
            ui.label(RichText::new("Channels").heading());
            ui.add_space(4.0);
            let mut changed = false;
            ui.horizontal(|ui| {
                ui.label("Input channels");
                egui::ComboBox::from_id_salt("in_layout").selected_text(model::layout_name(&self.setup.input_channels)).show_ui(ui, |ui| {
                    for (name, n) in model::LAYOUTS {
                        if ui.selectable_label(layout_for(n) == self.setup.input_channels, name).clicked() {
                            model::set_input_layout(&mut self.setup, layout_for(n));
                            changed = true;
                        }
                    }
                });
                ui.add_space(20.0);
                ui.label("Output channels");
                egui::ComboBox::from_id_salt("out_layout").selected_text(model::layout_name(&self.setup.output_channels)).show_ui(ui, |ui| {
                    for (name, n) in model::LAYOUTS {
                        if ui.selectable_label(layout_for(n) == self.setup.output_channels, name).clicked() {
                            model::set_output_layout(&mut self.setup, layout_for(n));
                            changed = true;
                        }
                    }
                });
            });
            ui.label(RichText::new("Existing routes between channels that stay are kept. Changing channels needs an audio engine restart.").weak().small());
            ui.add_space(14.0);

            ui.label(RichText::new("Output device").heading());
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let current = match &self.setup.output_target {
                    None => "Default output".to_string(),
                    Some(t) => self.sinks.iter().find(|s| &s.node_name == t).map(|s| s.description.clone()).unwrap_or_else(|| t.clone()),
                };
                egui::ComboBox::from_id_salt("device").selected_text(current).width(320.0).show_ui(ui, |ui| {
                    if ui.selectable_label(self.setup.output_target.is_none(), "Default output").clicked() {
                        self.setup.output_target = None;
                        changed = true;
                    }
                    for s in self.sinks.clone() {
                        let on = self.setup.output_target.as_deref() == Some(s.node_name.as_str());
                        if ui.selectable_label(on, &s.description).clicked() {
                            self.setup.output_target = Some(s.node_name.clone());
                            changed = true;
                        }
                    }
                });
                if ui.button("Refresh list").clicked() {
                    self.worker.send(Job::ListSinks);
                }
            });
            ui.label(RichText::new("Where the processed audio goes. Select \"LinAPO EQ\" as your system's output device to use it.").weak().small());
            ui.add_space(14.0);

            ui.label(RichText::new("Equalizer APO profiles").heading());
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.import_path).hint_text("path to an Equalizer APO .txt file (or drop it on the window)").desired_width(420.0));
                let enabled = !self.import_path.trim().is_empty();
                if ui.add_enabled(enabled, egui::Button::new("Import as new EQs")).clicked() {
                    let p = PathBuf::from(self.import_path.trim());
                    self.import_apo(&p, false);
                }
                if ui.add_enabled(enabled, egui::Button::new("Replace everything")).on_hover_text("Also turns Copy: lines into the matrix").clicked() {
                    let p = PathBuf::from(self.import_path.trim());
                    self.import_apo(&p, true);
                }
            });
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.dialog_open, egui::Button::new("Browse\u{2026}")).clicked() {
                    self.browse(Purpose::ImportEqs);
                }
                if ui
                    .add_enabled(!self.dialog_open, egui::Button::new("Browse and replace\u{2026}"))
                    .on_hover_text("Pick a profile and replace the whole setup with it (Copy: lines become the matrix)")
                    .clicked()
                {
                    self.browse(Purpose::ImportReplace);
                }
            });
            ui.add_space(14.0);

            ui.label(RichText::new("Setup file").heading());
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.file_field).desired_width(420.0));
                if ui.button("Load").clicked() {
                    let p = PathBuf::from(self.file_field.trim());
                    self.load_file(&p);
                }
                if ui.add_enabled(!self.dialog_open, egui::Button::new("Open\u{2026}")).clicked() {
                    self.browse(Purpose::OpenSetup);
                }
                if ui.add_enabled(!self.dialog_open, egui::Button::new("Save a copy\u{2026}")).on_hover_text("Save the setup to another file and keep working on that one").clicked() {
                    self.browse(Purpose::SaveSetup);
                }
            });
            ui.add_space(14.0);

            ui.label(RichText::new("PipeWire").heading());
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Install / restart audio engine").clicked() {
                    self.install();
                }
                if ui.button("Remove from PipeWire").clicked() {
                    self.worker.send(Job::Uninstall);
                }
            });
            if changed {
                self.touch();
            }
        });
    }
}

// ---------------------------------------------------------------- matrix cell

#[derive(Debug, Clone, Copy)]
enum CellEvent {
    None,
    Toggle,
    Off,
    SetDb(f64),
    Nudge(f64),
    Invert(bool),
}

fn route_cell(ui: &mut Ui, name: &str, gain: f64) -> CellEvent {
    let db = lin_to_db(gain);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(68.0, 40.0), Sense::click_and_drag());
    let on = gain != 0.0;
    let inverted = gain < 0.0;
    let level = db.map(|d| ((d + 48.0) / 60.0).clamp(0.0, 1.0) as f32).unwrap_or(0.0);
    let base = if inverted { WARN } else { ACCENT };
    let fill = if on { base.gamma_multiply(0.22 + 0.62 * level) } else { Color32::from_gray(34) };
    let stroke = if resp.hovered() { Color32::WHITE } else if on { base } else { Color32::from_gray(60) };
    let state = match db {
        None => "off".to_string(),
        Some(d) => format!("{d:.1} dB{}", if inverted { " inverted" } else { "" }),
    };
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, format!("route {name}: {state}")));

    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(7), fill);
    p.rect_stroke(rect, CornerRadius::same(7), Stroke::new(1.0, stroke), StrokeKind::Inside);
    match db {
        None => {
            p.text(rect.center(), Align2::CENTER_CENTER, "·", FontId::proportional(18.0), Color32::from_gray(90));
        }
        Some(d) => {
            let y = if inverted { rect.center().y - 6.0 } else { rect.center().y };
            p.text(egui::pos2(rect.center().x, y), Align2::CENTER_CENTER, format!("{d:.1}"), FontId::proportional(15.0), Color32::WHITE);
            if inverted {
                p.text(egui::pos2(rect.center().x, rect.center().y + 9.0), Align2::CENTER_CENTER, "inverted", FontId::proportional(9.5), Color32::from_gray(230));
            }
        }
    }

    let mut ev = CellEvent::None;
    if resp.clicked() {
        ev = CellEvent::Toggle;
    }
    if resp.dragged() {
        let dy = resp.drag_delta().y;
        if dy != 0.0 {
            ev = CellEvent::Nudge(-dy as f64 * 0.15);
        }
    }
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 && on {
            ev = CellEvent::Nudge((scroll / 50.0) as f64 * 0.5);
        }
    }
    if resp.double_clicked() {
        ev = CellEvent::SetDb(0.0);
    }
    let mut exact = db.unwrap_or(0.0);
    let mut inv = inverted;
    resp.context_menu(|ui| {
        ui.label(RichText::new(name).strong());
        if ui.add(egui::DragValue::new(&mut exact).range(-60.0..=12.0).speed(0.1).suffix(" dB").fixed_decimals(1)).changed() {
            ev = CellEvent::SetDb(exact);
        }
        if ui.button("Unity (0 dB)").clicked() {
            ev = CellEvent::SetDb(0.0);
            ui.close();
        }
        if ui.checkbox(&mut inv, "Invert polarity").changed() {
            ev = CellEvent::Invert(inv);
        }
        if ui.button("Off").clicked() {
            ev = CellEvent::Off;
            ui.close();
        }
    });
    ev
}

/// The eframe wrapper.
pub struct EframeApp(pub App);

impl eframe::App for EframeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.0.ui(ctx);
    }
}

pub fn apply_style(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    ctx.style_mut(|s| {
        s.spacing.item_spacing = Vec2::new(8.0, 6.0);
        s.spacing.button_padding = Vec2::new(10.0, 5.0);
        s.visuals.selection.bg_fill = ACCENT.gamma_multiply(0.45);
        s.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
    });
}
