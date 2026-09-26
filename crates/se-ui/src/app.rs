//! Main window: status bar, Show mode, Build mode, command palette, shortcuts, layouts,
//! pop-outs, and the confidence window (§15).

use crate::frames::Frames;
use crate::model::Model;
use crate::palette::Palette;
use crate::panels::Panel;
use crate::views;
use crate::windows::Windows;
use egui::RichText;
use egui_dock::DockState;
use se_proto::{Op, Value};
use se_ui_kit::widgets::icon;
use se_ui_kit::{Theme, ThemeChange, ThemeWatcher};
use std::collections::HashSet;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Show,
    Build,
}

/// Full-area views (§15.6), opened from the Views menu or the palette. Subsystems add a
/// variant, a `views/<name>.rs` with `pub fn ui(app: &mut App, ui: &mut egui::Ui)`, and a
/// line in [`App::view_ui`] / [`ViewId::ALL`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ViewId {
    Sessions,
    Audio,
    Setup,
    Songs,
    Giveaway,
    Maintenance,
    Tts,
    Lights,
    Patches,
    Devices,
    Performance,
    Settings,
    Timeline,
    Chatbot,
    Alerts,
    Mixer,
    Twitch,
    Controllers,
}

impl ViewId {
    pub const ALL: &'static [(ViewId, &'static str, &'static str)] = &[
        (ViewId::Sessions, icon::SESSION, "Session review"),
        (ViewId::Audio, icon::MIX, "Mix / Audio"),
        (ViewId::Setup, icon::SETTINGS, "Setup wizard"),
        (ViewId::Songs, icon::QUEUE, "Song requests"),
        (ViewId::Giveaway, icon::ALERT, "Giveaway"),
        (ViewId::Maintenance, icon::SESSION, "Backups & retention"),
        (ViewId::Tts, icon::BOT, "Text to speech"),
        (ViewId::Lights, icon::LIGHT, "Lights"),
        (ViewId::Patches, icon::PATCH, "Patches"),
        (ViewId::Devices, icon::DEVICE, "Devices"),
        (ViewId::Performance, icon::PERF, "Performance"),
        (ViewId::Settings, icon::SETTINGS, "Settings"),
        (ViewId::Timeline, icon::TIMELINE, "Timeline"),
        (ViewId::Chatbot, icon::BOT, "Chatbot"),
        (ViewId::Alerts, icon::ALERT, "Alerts, goals & stats"),
        (ViewId::Mixer, icon::MIX, "Mixer (16R)"),
        (ViewId::Twitch, icon::CHAT, "Twitch"),
        (ViewId::Controllers, icon::CONTROLLER, "Controllers"),
    ];
}

pub struct App {
    pub m: Model,
    pub t: Theme,
    watcher: ThemeWatcher,
    font: Option<String>,
    pub zoom: f32,
    pub mode: Mode,
    /// Open full-area view (replaces the mode layout's center while set).
    pub view: Option<ViewId>,
    /// Selected address prefix (inspector).
    pub selected: Option<String>,
    pub show: views::show::ShowState,
    pub build: views::build::BuildState,
    pub perf: views::perf::PerfState,
    pub settings: views::settings::SettingsState,
    pub palette: Palette,
    pub keys: crate::shortcuts_ui::Keys,
    pub dock: DockState<Panel>,
    pub frames: Frames,
    pub win: Windows,
    /// `stream-engine ui --program`: only the confidence window.
    pub program_only: bool,
    meta_asked: HashSet<String>,
    events_handled: u64,
    info_conn: u64,
    project_root: Option<std::path::PathBuf>,
    last_frame_ms: f32,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, opts: crate::UiOpts) -> App {
        let t = Theme::load();
        let font = se_ui_kit::install_font(&cc.egui_ctx, None);
        t.apply(&cc.egui_ctx, 1.0);
        let watcher = ThemeWatcher::start(cc.egui_ctx.clone());
        let layout = if opts.program_only { opts.layout.clone().or(Some("show-3disp".into())) } else { opts.layout.clone() };
        App {
            m: Model::new(opts.socket.clone()),
            t,
            watcher,
            font,
            zoom: 1.0,
            mode: Mode::Show,
            view: None,
            selected: None,
            show: Default::default(),
            build: Default::default(),
            perf: Default::default(),
            settings: Default::default(),
            palette: Default::default(),
            keys: Default::default(),
            dock: crate::dock::build(&crate::dock::default_groups(), &[]),
            frames: Frames::start(None, cc.egui_ctx.clone()),
            win: Windows::new(&cc.egui_ctx, layout),
            program_only: opts.program_only,
            meta_asked: HashSet::new(),
            events_handled: 0,
            info_conn: 0,
            project_root: None,
            last_frame_ms: 0.0,
        }
    }

    pub fn view_ui(&mut self, v: ViewId, ui: &mut egui::Ui) {
        match v {
            ViewId::Sessions => views::sessions::ui(self, ui),
            ViewId::Audio => views::audio::ui(self, ui),
            ViewId::Setup => views::setup::ui(self, ui),
            ViewId::Songs => views::songs::ui(self, ui),
            ViewId::Giveaway => views::giveaway::ui(self, ui),
            ViewId::Maintenance => views::maintenance::ui(self, ui),
            ViewId::Tts => views::tts::ui(self, ui),
            ViewId::Lights => views::lights::ui(self, ui),
            ViewId::Patches => views::patches::ui(self, ui),
            ViewId::Devices => views::devices::ui(self, ui),
            ViewId::Performance => views::perf::ui(self, ui),
            ViewId::Settings => views::settings::ui(self, ui),
            ViewId::Timeline => views::timeline::ui(self, ui),
            ViewId::Chatbot => views::chatbot::ui(self, ui),
            ViewId::Alerts => views::alerts::ui(self, ui),
            ViewId::Mixer => views::mixer::ui(self, ui),
            ViewId::Twitch => views::twitch::ui(self, ui),
            ViewId::Controllers => views::controllers::ui(self, ui),
        }
    }

    // ---- degrade under GPU pressure (§15.1.7) ----------------------------------------------

    /// 0 = fine, 1 = engine GPU over budget, 2 = far over budget.
    pub fn gpu_pressure(&self) -> u8 {
        let g = self.m.f("perf.gpu_ms");
        if g > 11.0 {
            2
        } else if g > 8.0 {
            1
        } else {
            0
        }
    }
    pub fn preview_hz(&self) -> f32 {
        [30.0, 15.0, 8.0][self.gpu_pressure() as usize]
    }
    pub fn program_hz(&self) -> f32 {
        [30.0, 20.0, 10.0][self.gpu_pressure() as usize]
    }
    pub fn atlas_hz(&self) -> f32 {
        [30.0, 15.0, 8.0][self.gpu_pressure() as usize]
    }
    /// The confidence window shows full rate until the GPU is really tight.
    pub fn confidence_hz(&self) -> f32 {
        [60.0, 60.0, 30.0][self.gpu_pressure() as usize]
    }

    // ---- helpers used by views ---------------------------------------------------------------

    pub fn select(&mut self, address: String) {
        self.selected = Some(address);
    }

    pub fn toggle_mode(&mut self) {
        self.mode = if self.mode == Mode::Show { Mode::Build } else { Mode::Show };
        self.view = None;
    }

    pub fn take(&mut self) {
        self.m.command(Op::SceneTake { transition: self.show.transition.clone(), ms: self.show.take_ms });
    }

    pub fn keys_label(&self, action: &str) -> String {
        self.keys.map.label(action)
    }

    pub fn set_zoom(&mut self, ctx: &egui::Context, z: f32) {
        self.zoom = z;
        self.t.apply(ctx, z);
    }

    pub fn font_name(&self) -> Option<&str> {
        self.font.as_deref()
    }

    pub fn is_popped_out(&self, panel: &str) -> bool {
        self.win.popouts.contains(panel)
    }

    pub fn pop_out(&mut self, panel: &str) {
        if Panel::parse(panel).is_some() {
            self.win.popouts.insert(panel.to_string());
            if let Some(p) = Panel::parse(panel)
                && let Some(path) = self.dock.find_tab(&p)
            {
                self.dock.remove_tab(path);
            }
        }
    }

    pub fn dock_back(&mut self, panel: &str) {
        self.win.popouts.remove(panel);
        if let Some(p) = Panel::parse(panel).filter(|p| p.is_dock_tab()) {
            crate::dock::dock_back(&mut self.dock, p);
        }
    }

    /// Bring a panel to the front: dock tab (Build), rail tab (Show), view, or pop-out window.
    pub fn open_panel(&mut self, p: Panel) {
        if self.is_popped_out(&p.id()) {
            return;
        }
        if let Some(tab) = p.rail_tab() {
            self.mode = Mode::Show;
            self.view = None;
            self.show.rail = tab;
            return;
        }
        if let Panel::View(v) = p
            && self.dock.find_tab(&p).is_none()
        {
            self.view = Some(v);
            return;
        }
        self.focus_panel(p);
    }

    /// Show a Build-mode dock tab (switching to Build mode).
    pub fn focus_panel(&mut self, p: Panel) {
        if crate::dock::focus(&mut self.dock, p) {
            self.mode = Mode::Build;
            self.view = None;
        }
    }

    /// Is any active binding driving this address?
    pub fn is_modulated(&self, address: &str) -> bool {
        self.m.q_list("bindings").iter().any(|b| {
            b.get_path("active").is_some_and(Value::truthy)
                && b.get_path("target").and_then(Value::as_str).is_some_and(|t| t == address || se_proto::address::matches(t, address))
        })
    }

    /// Fetch metadata for an address once per connection.
    pub fn fetch_meta_once(&mut self, address: &str) {
        if self.meta_asked.insert(format!("{}:{address}", self.m.conn_gen)) {
            self.m.fetch(address);
        }
    }

    pub fn layout_name(&self) -> String {
        if self.win.effective == self.win.chosen { self.win.chosen.clone() } else { format!("{} (auto: {})", self.win.chosen, self.win.effective) }
    }
    pub fn layout_names(&self) -> Vec<String> {
        self.win.set.names().into_iter().map(String::from).collect()
    }
    pub fn next_layout(&mut self) {
        let n = self.win.set.next(&self.win.chosen).to_string();
        self.switch_layout(&n);
    }
    pub fn switch_layout(&mut self, name: &str) {
        if self.win.set.get(name).is_none() {
            self.m.toast(format!("no layout named `{name}`"), true);
            return;
        }
        self.win.chosen = name.to_string();
        self.m.toast(format!("layout {name}"), false);
    }

    /// Save the current arrangement as `layouts/<name>.toml` (comments of an existing file kept).
    pub fn save_layout(&mut self, name: &str) {
        let name = views::modulate::sanitize(name);
        let path = format!("layouts/{name}.toml");
        let mut l = self.win.layout();
        l.name = name.clone();
        l.mode = if self.mode == Mode::Build { crate::layout::Mode::Build } else { crate::layout::Mode::Show };
        l.zoom = self.zoom;
        l.show.rail_tab = self.show.rail.id().into();
        l.show.multiview = self.show.multiview_in_main;
        l.dock.groups = crate::dock::groups(&self.dock).into_iter().map(|g| g.into_iter().map(|p| p.id()).collect()).collect();
        l.dock.fractions = Vec::new();
        l.dock.height = self.build.dock_height;
        let keep: Vec<crate::layout::PopOut> = l.windows.iter().filter(|w| self.win.popouts.contains(&w.panel)).cloned().collect();
        let mut windows = keep;
        for id in &self.win.popouts {
            if !windows.iter().any(|w| &w.panel == id) {
                windows.push(crate::layout::PopOut { panel: id.clone(), monitor: None, fullscreen: false, size: None });
            }
        }
        l.windows = windows;
        let existing = self.m.q(&format!("project.read:{path}")).and_then(|v| v.get_path("text")).and_then(Value::as_str).map(String::from);
        let text = l.to_toml(existing.as_deref());
        self.m.action("project.write", Value::map().with("path", path.clone()).with("text", text));
        self.win.set.insert(l);
        self.win.chosen = name;
        self.m.toast(format!("saved {path}"), false);
    }

    /// Open a project file (relative or absolute) at a line in `$EDITOR`.
    pub fn open_in_editor(&mut self, path: &str, line: Option<u32>) {
        let p = std::path::Path::new(path);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            match &self.project_root {
                Some(root) => root.join(path),
                None => {
                    self.m.toast("project path unknown (engine offline)", true);
                    return;
                }
            }
        };
        match crate::editor::open(&abs, line) {
            Ok(cmd) => self.m.toast(format!("opened {} ({cmd})", abs.display()), false),
            Err(e) => self.m.toast(format!("could not open {}: {e}", abs.display()), true),
        }
    }

    fn theme_changes(&mut self, ctx: &egui::Context) {
        while let Ok(c) = self.watcher.rx.try_recv() {
            match c {
                ThemeChange::Theme(th) => {
                    self.t = th;
                    self.t.apply(ctx, self.zoom);
                }
                ThemeChange::Font(f) => {
                    self.font = se_ui_kit::install_font(ctx, Some(&f));
                }
            }
        }
    }

    /// React to engine events addressed to the UI (`ui.layout`, `ui.mode`, `ui.open`, font hook,
    /// MIDI learn results).
    fn ui_events(&mut self, ctx: &egui::Context) {
        let new = self.m.events_seen.saturating_sub(self.events_handled) as usize;
        self.events_handled = self.m.events_seen;
        let n = self.m.events.len();
        let evs: Vec<se_proto::Event> = self
            .m
            .events
            .iter()
            .skip(n.saturating_sub(new))
            .filter(|e| e.ty.starts_with("ui.") || e.ty.starts_with("midi.learn") || e.ty == "omarchy.font_set")
            .cloned()
            .collect();
        for e in evs {
            let s = |k: &str| e.payload.get_path(k).and_then(Value::as_str).map(String::from);
            let first = || e.payload.get_path("args.0").and_then(Value::as_str).map(String::from);
            match e.ty.as_str() {
                "ui.layout" => {
                    if let Some(n) = s("name").or_else(first) {
                        self.switch_layout(&n);
                    }
                }
                "ui.mode" => match s("mode").or_else(first).as_deref() {
                    Some("build") => self.mode = Mode::Build,
                    Some("show") => self.mode = Mode::Show,
                    _ => self.toggle_mode(),
                },
                "ui.open" => {
                    if let Some(p) = s("view").or_else(|| s("panel")).or_else(first).and_then(|v| Panel::parse(&v)) {
                        self.open_panel(p);
                    }
                }
                "omarchy.font_set" => {
                    if let Some(f) = s("font") {
                        self.font = se_ui_kit::install_font(ctx, Some(&f));
                    }
                }
                "midi.learned" => {
                    let target = s("target").unwrap_or_default();
                    let signal = s("signal").unwrap_or_default();
                    self.m.toast(format!("MIDI learned: {signal} → {target}"), false);
                    self.m.refresh_soon();
                }
                "midi.learn.timeout" => self.m.toast("MIDI learn timed out", true),
                _ => {}
            }
        }
    }

    fn engine_info(&mut self) {
        if self.m.connected && self.info_conn != self.m.conn_gen {
            self.info_conn = self.m.conn_gen;
            self.m.query("engine.info", Value::Null);
        }
        if let Some(p) = self.m.q("engine.info").and_then(|v| v.get_path("project")).and_then(Value::as_str) {
            let p = std::path::PathBuf::from(p);
            if self.project_root.as_ref() != Some(&p) {
                self.project_root = Some(p);
            }
        }
    }

    fn toasts(&mut self, ctx: &egui::Context) {
        let t = self.t.clone();
        egui::Area::new(egui::Id::new("toasts")).anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -12.0]).show(ctx, |ui| {
            for toast in self.m.toasts.iter().rev().take(5) {
                egui::Frame::new()
                    .fill(t.bg_dark)
                    .stroke(egui::Stroke::new(1.5, if toast.error { t.bright_red } else { t.green }))
                    .corner_radius(6)
                    .inner_margin(8)
                    .show(ui, |ui| {
                        ui.label(RichText::new(format!("{} {}", if toast.error { icon::WARN } else { icon::CHECK }, toast.text)).color(t.fg));
                    });
            }
        });
    }

    fn main_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        crate::shortcuts_ui::handle(self, ctx);
        egui::Panel::top("status").exact_size(34.0).show(ui, |ui| views::status::ui(self, ui));
        match (self.view, self.mode) {
            (Some(v), _) => {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui.small_button(format!("{} back", icon::CROSS)).on_hover_text("close view (Esc)").clicked() {
                            self.view = None;
                        }
                        if ui.small_button("pop out").clicked() {
                            self.pop_out(&Panel::View(v).id());
                            self.view = None;
                        }
                    });
                    if self.view.is_some() {
                        self.view_ui(v, ui);
                    }
                });
            }
            (None, Mode::Show) => views::show::ui(self, ui),
            (None, Mode::Build) => views::build::ui(self, ui),
        }
        crate::windows::popouts(self, ctx);
        crate::windows::confidence(self, ctx);
        crate::palette::ui(self, ctx);
        crate::shortcuts_ui::overlay(self, ctx);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let started = Instant::now();
        let ctx = &ui.ctx().clone();
        self.theme_changes(ctx);
        self.m.pump();
        crate::views::setup::auto_open(self);
        self.ui_events(ctx);
        self.engine_info();
        self.m.refresh(&[
            "presets",
            "scenes",
            "active",
            "modes",
            "transitions",
            "sim.presets",
            "rules",
            "bindings",
            "errors",
            "config.scenes",
            "project.files",
            "patches",
            "controllers.page",
        ]);
        views::build::sync(self);
        if let Some(rs) = frame.wgpu_render_state() {
            self.frames.update(rs);
        }
        crate::windows::tick(self, ctx);
        if self.program_only {
            crate::windows::program_root(self, ui);
        } else {
            self.main_ui(ui, ctx);
        }
        self.toasts(ctx);
        let repaint = [33, 50, 80][self.gpu_pressure() as usize];
        ctx.request_repaint_after(std::time::Duration::from_millis(repaint));
        self.last_frame_ms = started.elapsed().as_secs_f32() * 1000.0;
        views::perf::sample(self, self.last_frame_ms);
    }
}
