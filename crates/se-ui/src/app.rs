//! Main window (§15): one window with a sidebar of pages, each page split into a few tabs.
//! Pages are organised by what the streamer wants to do (run the show, build scenes, run the
//! lights, …), not by engine subsystem. Also: the command palette, shortcuts, layouts, and
//! optional pop-outs / confidence window.

use crate::frames::Frames;
use crate::model::Model;
use crate::palette::Palette;
use crate::panels::Panel;
use crate::views;
use crate::windows::Windows;
use se_proto::{Op, Value};
use se_ui_kit::widgets::icon;
use se_ui_kit::{Theme, ThemeChange, ThemeWatcher};
use std::collections::HashSet;
use std::time::Instant;

/// Sidebar pages, in sidebar order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Page {
    Live,
    Scenes,
    Lights,
    Sound,
    Automation,
    Community,
    Recordings,
    Settings,
}

impl Page {
    pub const ALL: [Page; 8] = [Page::Live, Page::Scenes, Page::Lights, Page::Sound, Page::Automation, Page::Community, Page::Recordings, Page::Settings];

    pub fn id(self) -> &'static str {
        match self {
            Page::Live => "live",
            Page::Scenes => "scenes",
            Page::Lights => "lights",
            Page::Sound => "sound",
            Page::Automation => "automation",
            Page::Community => "community",
            Page::Recordings => "recordings",
            Page::Settings => "settings",
        }
    }

    pub fn parse(s: &str) -> Option<Page> {
        Page::ALL.into_iter().find(|p| p.id() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Page::Live => "Live",
            Page::Scenes => "Scenes",
            Page::Lights => "Lights",
            Page::Sound => "Sound",
            Page::Automation => "Automation",
            Page::Community => "Community",
            Page::Recordings => "Recordings",
            Page::Settings => "Settings",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Page::Live => icon::LIVE,
            Page::Scenes => icon::LAYERS,
            Page::Lights => icon::LIGHT,
            Page::Sound => icon::VOLUME,
            Page::Automation => icon::BOLT,
            Page::Community => icon::USERS,
            Page::Recordings => icon::FILM,
            Page::Settings => icon::SETTINGS,
        }
    }

    /// One sentence under the page title: what you do here.
    pub fn blurb(self) -> &'static str {
        match self {
            Page::Live => "",
            Page::Scenes => "Arrange cameras and overlays, pick transitions and effects.",
            Page::Lights => "Pick a look, run cue lists, and edit your lighting.",
            Page::Sound => "Levels for everything your viewers hear.",
            Page::Automation => "Make things happen on their own: when something happens, do something.",
            Page::Community => "Alerts, goals, song requests, the chat bot, and giveaways.",
            Page::Recordings => "Past streams, markers, and clips ready to review.",
            Page::Settings => "Accounts, devices, backups, and help when something's off.",
        }
    }

    /// The tabs of a page: (view, tab label). Pages with one entry show no tab bar.
    pub fn tabs(self) -> &'static [(ViewId, &'static str)] {
        match self {
            Page::Live => &[],
            Page::Scenes => &[(ViewId::Composition, "Layout"), (ViewId::Patches, "Overlays & effects")],
            Page::Lights => &[(ViewId::Lights, "Lights")],
            Page::Sound => &[(ViewId::Audio, "Mix"), (ViewId::Mixer, "Mixing desk"), (ViewId::Tts, "Text to speech")],
            Page::Automation => &[
                (ViewId::Reactions, "Reactions"),
                (ViewId::Controllers, "Buttons & pedals"),
                (ViewId::Chatbot, "Chat commands"),
                (ViewId::Timeline, "Timelines"),
            ],
            Page::Community => {
                &[(ViewId::Alerts, "Alerts & goals"), (ViewId::Songs, "Song requests"), (ViewId::Twitch, "Twitch"), (ViewId::Giveaway, "Giveaways")]
            }
            Page::Recordings => &[(ViewId::Sessions, "Recordings & clips")],
            Page::Settings => &[
                (ViewId::Setup, "Get started"),
                (ViewId::Settings, "Accounts & app"),
                (ViewId::Devices, "Devices"),
                (ViewId::Maintenance, "Backups"),
                (ViewId::Performance, "Performance"),
                (ViewId::Troubleshoot, "Troubleshooting"),
            ],
        }
    }

    /// The page (and tab index) that shows `v`.
    pub fn of(v: ViewId) -> (Page, usize) {
        for p in Page::ALL {
            if let Some(i) = p.tabs().iter().position(|(t, _)| *t == v) {
                return (p, i);
            }
        }
        (Page::Settings, 0)
    }
}

/// Every tab of every page (§15.6). Subsystems add a variant, a `views/<name>.rs` with
/// `pub fn ui(app: &mut App, ui: &mut egui::Ui)`, a line in [`App::view_ui`] / [`ViewId::ALL`],
/// and a place in [`Page::tabs`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ViewId {
    Composition,
    Reactions,
    Troubleshoot,
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
        (ViewId::Composition, icon::LAYERS, "Scene layout"),
        (ViewId::Reactions, icon::BOLT, "Reactions"),
        (ViewId::Troubleshoot, icon::CONSOLE, "Troubleshooting"),
        (ViewId::Sessions, icon::FILM, "Recordings & clips"),
        (ViewId::Audio, icon::MIX, "Sound mix"),
        (ViewId::Setup, icon::ROCKET, "Get started"),
        (ViewId::Songs, icon::MUSIC, "Song requests"),
        (ViewId::Giveaway, icon::GIFT, "Giveaways"),
        (ViewId::Maintenance, icon::SAVE, "Backups"),
        (ViewId::Tts, icon::MIC, "Text to speech"),
        (ViewId::Lights, icon::LIGHT, "Lights"),
        (ViewId::Patches, icon::SPARKLE, "Overlays & effects"),
        (ViewId::Devices, icon::DEVICE, "Devices"),
        (ViewId::Performance, icon::PERF, "Performance"),
        (ViewId::Settings, icon::SETTINGS, "Accounts & app"),
        (ViewId::Timeline, icon::TIMELINE, "Timelines"),
        (ViewId::Chatbot, icon::BOT, "Chat commands"),
        (ViewId::Alerts, icon::ALERT, "Alerts & goals"),
        (ViewId::Mixer, icon::SLIDERS, "Mixing desk"),
        (ViewId::Twitch, icon::TWITCH, "Twitch"),
        (ViewId::Controllers, icon::CONTROLLER, "Buttons & pedals"),
    ];
}

pub struct App {
    pub m: Model,
    pub t: Theme,
    watcher: ThemeWatcher,
    font: Option<String>,
    pub zoom: f32,
    pub page: Page,
    /// Selected tab per page (index into [`Page::tabs`]).
    pub tab: [usize; 8],
    /// Selected address prefix (inspector).
    pub selected: Option<String>,
    pub show: views::show::ShowState,
    pub build: views::build::BuildState,
    pub perf: views::perf::PerfState,
    pub settings: views::settings::SettingsState,
    pub troubleshoot: views::troubleshoot::TroubleshootState,
    pub palette: Palette,
    pub keys: crate::shortcuts_ui::Keys,
    pub frames: Frames,
    pub win: Windows,
    /// `stream-engine ui --program`: only the confidence window.
    pub program_only: bool,
    meta_asked: HashSet<String>,
    events_handled: u64,
    info_conn: u64,
    project_root: Option<std::path::PathBuf>,
    last_frame_ms: f32,
    /// Engine unreachable: since when, and the "Start Stream Engine" click.
    pub down: views::status::EngineDown,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, opts: crate::UiOpts) -> App {
        let t = Theme::load();
        let font = se_ui_kit::install_font(&cc.egui_ctx, None);
        t.apply(&cc.egui_ctx, 1.0);
        let watcher = ThemeWatcher::start(cc.egui_ctx.clone());
        let layout = if opts.program_only { opts.layout.clone().or(Some("show-3disp".into())) } else { opts.layout.clone() };
        crate::windows::make_opaque();
        App {
            m: Model::new(opts.socket.clone()),
            t,
            watcher,
            font,
            zoom: 1.0,
            page: Page::Live,
            tab: [0; 8],
            selected: None,
            show: Default::default(),
            build: Default::default(),
            perf: Default::default(),
            settings: Default::default(),
            troubleshoot: Default::default(),
            palette: Default::default(),
            keys: Default::default(),
            frames: Frames::start(None, cc.egui_ctx.clone()),
            win: Windows::new(&cc.egui_ctx, layout),
            program_only: opts.program_only,
            meta_asked: HashSet::new(),
            events_handled: 0,
            info_conn: 0,
            project_root: None,
            last_frame_ms: 0.0,
            down: Default::default(),
        }
    }

    pub fn view_ui(&mut self, v: ViewId, ui: &mut egui::Ui) {
        match v {
            ViewId::Composition => views::composition::ui(self, ui),
            ViewId::Reactions => views::rules::ui(self, ui),
            ViewId::Troubleshoot => views::troubleshoot::ui(self, ui),
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

    /// Go to the page and tab that shows `v`.
    pub fn open_view(&mut self, v: ViewId) {
        let (p, i) = Page::of(v);
        self.page = p;
        self.tab[p as usize] = i;
    }

    /// The view currently on screen (`None` on the Live page).
    pub fn current_view(&self) -> Option<ViewId> {
        self.page.tabs().get(self.tab[self.page as usize]).map(|(v, _)| *v)
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

    /// `Tab`: jump between the Live page and scene editing.
    pub fn toggle_page(&mut self) {
        self.page = if self.page == Page::Live { Page::Scenes } else { Page::Live };
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
        }
    }

    pub fn dock_back(&mut self, panel: &str) {
        self.win.popouts.remove(panel);
    }

    /// Bring a panel to the front: its page and tab (or its pop-out window if it has one).
    pub fn open_panel(&mut self, p: Panel) {
        if self.is_popped_out(&p.id()) {
            return;
        }
        if let Some(tab) = p.rail_tab() {
            self.page = Page::Live;
            self.show.rail = tab;
            return;
        }
        match p {
            Panel::View(v) => self.open_view(v),
            Panel::Library | Panel::Canvas | Panel::Inspector => self.open_view(ViewId::Composition),
            Panel::Rules => self.open_view(ViewId::Reactions),
            Panel::Modulate | Panel::Scopes | Panel::Trace | Panel::Simulator | Panel::Console => {
                self.troubleshoot.tool = views::troubleshoot::Tool::for_panel(p);
                self.open_view(ViewId::Troubleshoot);
            }
            _ => self.page = Page::Live,
        }
    }

    /// Same as [`App::open_panel`] (kept for call sites that "focus" a tool).
    pub fn focus_panel(&mut self, p: Panel) {
        self.open_panel(p);
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
        l.page = self.page.id().into();
        l.zoom = self.zoom;
        l.show.rail_tab = self.show.rail.id().into();
        l.show.multiview = self.show.multiview_in_main;
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
                "ui.mode" | "ui.page" => {
                    if let Some(p) = s("page").or_else(|| s("mode")).or_else(first).as_deref().and_then(|m| match m {
                        "show" => Some(Page::Live),
                        "build" => Some(Page::Scenes),
                        other => Page::parse(other),
                    }) {
                        self.page = p;
                    } else {
                        self.toggle_page();
                    }
                }
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
        egui::Area::new(egui::Id::new("toasts")).anchor(egui::Align2::RIGHT_BOTTOM, [-20.0, -20.0]).order(egui::Order::Foreground).show(ctx, |ui| {
            for toast in self.m.toasts.iter().rev().take(4) {
                let c = if toast.error { t.bright_red } else { t.green };
                egui::Frame::new()
                    .fill(t.surface_hi)
                    .stroke(egui::Stroke::new(1.0, t.border))
                    .corner_radius(se_ui_kit::theme::radius::CARD)
                    .inner_margin(egui::Margin::symmetric(16, 12))
                    .shadow(egui::Shadow { offset: [0, 6], blur: 20, spread: 0, color: egui::Color32::from_black_alpha(90) })
                    .show(ui, |ui| {
                        ui.set_max_width(420.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(if toast.error { icon::WARN } else { icon::CHECK }).color(c));
                            ui.label(egui::RichText::new(&toast.text).color(t.fg));
                        });
                    });
                ui.add_space(8.0);
            }
        });
    }

    /// Left sidebar: brand, pages, connection state.
    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let t = self.t.clone();
        egui::Panel::left("nav")
            .exact_size(232.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(t.chrome).inner_margin(egui::Margin { left: 12, right: 12, top: 18, bottom: 14 }))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(icon::LIVE).size(18.0).color(t.accent));
                    ui.add_space(2.0);
                    ui.label(egui::RichText::new("Stream Engine").font(se_ui_kit::theme::font_bold(17.0)).color(t.fg));
                });
                ui.add_space(22.0);
                let setup_left = views::setup::steps_left(self);
                for p in Page::ALL {
                    if p == Page::Settings {
                        continue;
                    }
                    let badge = views::live::nav_badge(self, p);
                    if se_ui_kit::widgets::nav_item(ui, &t, p.icon(), p.label(), self.page == p, badge).clicked() {
                        self.page = p;
                    }
                    ui.add_space(2.0);
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    let (dot, text) = if self.m.connected { (t.green, "Engine running".to_string()) } else { (t.bright_red, "Engine not running".to_string()) };
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        let (r, _) = ui.allocate_exact_size(egui::vec2(8.0, 16.0), egui::Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.0, dot);
                        ui.label(egui::RichText::new(text).size(se_ui_kit::theme::type_scale::SMALL).color(t.text_dim));
                    });
                    ui.add_space(10.0);
                    let badge = (setup_left > 0).then(|| (setup_left.to_string(), t.accent));
                    if se_ui_kit::widgets::nav_item(ui, &t, Page::Settings.icon(), Page::Settings.label(), self.page == Page::Settings, badge).clicked() {
                        self.page = Page::Settings;
                    }
                });
            });
    }

    /// A non-Live page: title, one-line blurb, tabs, then the selected tab's view.
    fn page_ui(&mut self, ui: &mut egui::Ui) {
        let t = self.t.clone();
        let page = self.page;
        let tabs = page.tabs();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(t.bg).inner_margin(egui::Margin { left: 32, right: 32, top: 26, bottom: 20 })).show(
            ui,
            |ui| {
                se_ui_kit::widgets::page_header(ui, &t, page.label(), page.blurb(), |_| {});
                let idx = &mut self.tab[page as usize];
                *idx = (*idx).min(tabs.len().saturating_sub(1));
                if tabs.len() > 1 {
                    let labels: Vec<&str> = tabs.iter().map(|(_, l)| *l).collect();
                    se_ui_kit::widgets::tabs(ui, &t, idx, &labels);
                }
                let v = tabs[*idx].0;
                if self.is_popped_out(&Panel::View(v).id()) {
                    let back = se_ui_kit::widgets::empty_state(
                        ui,
                        &t,
                        icon::EXPAND,
                        "Open in its own window",
                        "This part is showing in a separate window.",
                        Some("Bring it back here"),
                    );
                    if back {
                        self.dock_back(&Panel::View(v).id());
                    }
                    return;
                }
                self.view_ui(v, ui);
            },
        );
    }

    fn main_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        crate::shortcuts_ui::handle(self, ctx);
        self.sidebar(ui);
        egui::Panel::top("topbar")
            .exact_size(64.0)
            .frame(egui::Frame::new().fill(self.t.bg).inner_margin(egui::Margin { left: 28, right: 20, top: 0, bottom: 0 }).stroke(egui::Stroke::NONE))
            .show(ui, |ui| views::status::ui(self, ui));
        if views::status::engine_down(self) {
            views::status::engine_down_ui(self, ui);
        } else if self.page == Page::Live {
            views::live::ui(self, ui);
        } else {
            self.page_ui(ui);
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
            "sources",
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
