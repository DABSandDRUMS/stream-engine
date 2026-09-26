//! Main window: status bar, Show mode, Build mode, command palette, shortcuts (§15).

use crate::model::Model;
use crate::views;
use egui::{Key, KeyboardShortcut, Modifiers, RichText};
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};
use se_ui_kit::{Theme, ThemeChange, ThemeWatcher};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Show,
    Build,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RailTab {
    Events,
    Chat,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DockTab {
    Rules,
    Scopes,
    Trace,
    Simulator,
    Console,
}

pub struct App {
    pub m: Model,
    pub t: Theme,
    watcher: ThemeWatcher,
    pub mode: Mode,
    pub rail: RailTab,
    pub dock: DockTab,
    pub palette_open: bool,
    pub palette_text: String,
    pub zoom: f32,
    pub selected: Option<String>,
    pub sim_args: String,
    pub scope_pattern: String,
    pub console_filter: String,
    pub library_filter: String,
    pub transition_choice: Option<String>,
    pub panic_armed_at: Option<f64>,
    font: Option<String>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, socket: Option<std::path::PathBuf>, layout: Option<String>) -> App {
        let t = Theme::load();
        let font = se_ui_kit::install_font(&cc.egui_ctx, None);
        t.apply(&cc.egui_ctx, 1.0);
        let watcher = ThemeWatcher::start(cc.egui_ctx.clone());
        let mode = if layout.as_deref() == Some("build") { Mode::Build } else { Mode::Show };
        App {
            m: Model::new(socket),
            t,
            watcher,
            mode,
            rail: RailTab::Events,
            dock: DockTab::Simulator,
            palette_open: false,
            palette_text: String::new(),
            zoom: 1.0,
            selected: None,
            sim_args: String::new(),
            scope_pattern: "lfo.*".into(),
            console_filter: String::new(),
            library_filter: String::new(),
            transition_choice: None,
            panic_armed_at: None,
            font,
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();
        let sc = |m: Modifiers, k: Key| KeyboardShortcut::new(m, k);
        if ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::CTRL, Key::K))) {
            self.palette_open = !self.palette_open;
            self.palette_text.clear();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::CTRL, Key::Period))) {
            self.m.command(Op::Clean);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::CTRL | Modifiers::SHIFT, Key::Z))) {
            self.m.command(Op::Redo);
        } else if ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::CTRL, Key::Z))) {
            self.m.command(Op::Undo);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::CTRL, Key::Plus)) || i.consume_shortcut(&sc(Modifiers::CTRL, Key::Equals))) {
            self.zoom = (self.zoom + 0.1).min(2.0);
            self.t.apply(ctx, self.zoom);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::CTRL, Key::Minus))) {
            self.zoom = (self.zoom - 0.1).max(0.6);
            self.t.apply(ctx, self.zoom);
        }
        // hold Ctrl+Esc = panic (1 s)
        let (ctrl_esc, now) = ctx.input(|i| (i.modifiers.ctrl && i.key_down(Key::Escape), i.time));
        if ctrl_esc {
            let s = *self.panic_armed_at.get_or_insert(now);
            if now - s > 1.0 {
                self.m.command(Op::Panic);
                self.panic_armed_at = Some(f64::INFINITY);
            }
            ctx.request_repaint();
        } else {
            self.panic_armed_at = None;
        }
        if typing || self.palette_open {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
            self.mode = if self.mode == Mode::Show { Mode::Build } else { Mode::Show };
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
            self.m.command(Op::SceneTake { transition: self.transition_choice.clone(), ms: None });
        }
        let scenes: Vec<(Option<i64>, String)> = self
            .m
            .q_list("scenes")
            .iter()
            .map(|s| (s.get_path("key").and_then(Value::as_i64), s.get_path("name").and_then(Value::as_str).unwrap_or("").to_string()))
            .collect();
        let keys = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
        for (n, k) in keys.iter().enumerate() {
            let num = n as i64 + 1;
            let target = scenes.iter().find(|(key, _)| *key == Some(num)).or_else(|| scenes.get(n)).map(|(_, s)| s.clone());
            if ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, *k)) {
                if let Some(s) = &target {
                    self.m.command(Op::SceneCut { scene: s.clone(), transition: self.transition_choice.clone() });
                }
            } else if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, *k))
                && let Some(s) = target
            {
                self.m.command(Op::SceneGo { scene: s });
            }
        }
        let fkeys = [Key::F1, Key::F2, Key::F3, Key::F4, Key::F5, Key::F6, Key::F7, Key::F8, Key::F9, Key::F10, Key::F11, Key::F12];
        let presets: Vec<String> = self.m.q_list("presets").iter().filter_map(|p| p.get_path("name").and_then(Value::as_str).map(String::from)).collect();
        for (i, k) in fkeys.iter().enumerate() {
            if ctx.input_mut(|inp| inp.consume_key(Modifiers::NONE, *k))
                && let Some(p) = presets.get(i)
            {
                self.m.command(Op::PresetFire { name: p.clone(), payload: Value::Null });
            }
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let t = self.t.clone();
        ui.horizontal(|ui| {
            let mode = self.m.str("show.mode").to_string();
            let live = mode == "live";
            let (txt, st) = if !self.m.connected {
                ("ENGINE OFFLINE", LedState::Error)
            } else if live {
                ("LIVE", LedState::Active)
            } else {
                (mode.as_str(), LedState::Idle)
            };
            let txt = txt.to_uppercase();
            widgets::led(ui, &t, st);
            ui.label(RichText::new(txt).strong().color(if live { t.tally_program() } else { t.fg }));
            ui.separator();
            ui.label(RichText::new(if self.mode == Mode::Show { "SHOW" } else { "BUILD" }).strong().color(t.accent)).on_hover_text("Tab toggles Show/Build");
            ui.separator();
            // outputs health (populated by the OBS link)
            for (name, prefix) in [("Twitch", "obs.stream"), ("REC", "obs.record")] {
                let active = self.m.b(&format!("{prefix}.active"));
                let kbps = self.m.f(&format!("{prefix}.kbps"));
                let dropped = self.m.f(&format!("{prefix}.dropped"));
                let text = if active { format!("{name} {:.1}Mb/s {} drop", kbps / 1000.0, dropped as i64) } else { format!("{name} off") };
                widgets::pill(ui, &t, if name == "REC" { icon::REC } else { icon::LIVE }, &text, if active { LedState::Healthy } else { LedState::Idle });
            }
            ui.separator();
            egui::ComboBox::from_id_salt("mode").selected_text(format!("mode: {}", if mode.is_empty() { "?" } else { &mode })).show_ui(ui, |ui| {
                let modes: Vec<String> = self.m.q_list("modes").iter().filter_map(|v| v.as_str().map(String::from)).collect();
                for md in modes {
                    if ui.selectable_label(md == mode, &md).clicked() {
                        self.m.command(Op::ModeSet { mode: md });
                    }
                }
            });
            let gpu = self.m.f("perf.gpu_ms");
            if gpu > 0.0 {
                ui.separator();
                widgets::pill(ui, &t, icon::GPU, &format!("GPU {gpu:.1}ms"), if gpu > 8.0 { LedState::Armed } else { LedState::Healthy });
            }
            let errors = self.m.f("project.errors") as i64;
            if errors > 0 {
                ui.separator();
                if widgets::pill(ui, &t, icon::WARN, &format!("{errors}"), LedState::Armed).on_hover_text("project errors — click for console").clicked() {
                    self.mode = Mode::Build;
                    self.dock = DockTab::Console;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::hold_button(ui, &t, "PANIC", t.bright_red, 0.8) {
                    self.m.command(Op::Panic);
                }
                if ui.button(RichText::new("CLEAN").strong()).on_hover_text("Clear chat effects (Ctrl+.)").clicked() {
                    self.m.command(Op::Clean);
                }
                ui.separator();
                let conn = if self.m.connected {
                    format!("{} session {}", icon::LINK, self.m.session)
                } else {
                    format!("{} {}", icon::UNLINK, self.m.last_disconnect.as_deref().unwrap_or("connecting…"))
                };
                ui.label(RichText::new(conn).small().color(if self.m.connected { t.fg_dim } else { t.bright_red }));
            });
        });
    }

    fn palette(&mut self, ctx: &egui::Context) {
        if !self.palette_open {
            return;
        }
        let t = self.t.clone();
        egui::Window::new("palette").title_bar(false).anchor(egui::Align2::CENTER_TOP, [0.0, 80.0]).fixed_size([640.0, 360.0]).show(ctx, |ui| {
            ui.label(RichText::new(format!("{} command palette — type a command, a scene, or a preset", icon::SEARCH)).color(t.fg_dim));
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.palette_text)
                    .desired_width(f32::INFINITY)
                    .hint_text("preset.fire hype · scene.go duo · set fx.x 0.5 · panic"),
            );
            r.request_focus();
            let q = self.palette_text.to_lowercase();
            let mut items: Vec<(String, String)> = Vec::new();
            for s in self.m.q_list("scenes") {
                if let Some(n) = s.get_path("name").and_then(Value::as_str) {
                    items.push((format!("{} scene {n}", icon::SCENE), format!("scene.go {n}")));
                }
            }
            for p in self.m.q_list("presets") {
                if let Some(n) = p.get_path("name").and_then(Value::as_str) {
                    items.push((format!("{} preset {n}", icon::PRESET), format!("preset.fire {n}")));
                }
            }
            for m in self.m.q_list("modes") {
                if let Some(n) = m.as_str() {
                    items.push((format!("{} mode {n}", icon::LIVE), format!("mode.set {n}")));
                }
            }
            for s in self.m.q_list("sim.presets") {
                if let Some(n) = s.get_path("name").and_then(Value::as_str) {
                    items.push((format!("{} simulate {n}", icon::SIM), format!("sim.{n}")));
                }
            }
            for (label, cmd) in [
                ("take", "scene.take"),
                ("clean", "clean"),
                ("undo", "undo"),
                ("redo", "redo"),
                ("reload project", "project.reload"),
                ("add marker", "session.marker"),
            ] {
                items.push((format!("{} {label}", icon::PLAY), cmd.to_string()));
            }
            let matches: Vec<&(String, String)> =
                items.iter().filter(|(l, c)| q.is_empty() || l.to_lowercase().contains(&q) || c.contains(&q)).take(12).collect();
            let enter = ui.input(|i| i.key_pressed(Key::Enter));
            let esc = ui.input(|i| i.key_pressed(Key::Escape));
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (i, (label, cmd)) in matches.iter().enumerate() {
                    let sel = i == 0;
                    if ui.selectable_label(sel, format!("{label}    {}", RichText::new(cmd.as_str()).small().text())).clicked() {
                        self.m.text(cmd);
                        self.palette_open = false;
                    }
                }
            });
            if enter {
                let cmd = if let Some((_, c)) = matches.first() { c.clone() } else { self.palette_text.clone() };
                if !cmd.trim().is_empty() {
                    self.m.text(cmd.trim());
                }
                self.palette_open = false;
            }
            if esc {
                self.palette_open = false;
            }
        });
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
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
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
        self.m.pump();
        self.m.refresh(&["presets", "scenes", "active", "modes", "transitions", "sim.presets", "rules", "bindings", "errors"]);
        self.shortcuts(ctx);
        egui::Panel::top("status").exact_size(34.0).show(ui, |ui| self.status_bar(ui));
        match self.mode {
            Mode::Show => views::show::ui(self, ui),
            Mode::Build => views::build::ui(self, ui),
        }
        self.palette(ctx);
        self.toasts(ctx);
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
}
