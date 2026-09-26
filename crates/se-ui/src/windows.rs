//! Windows and layouts (§15.3, §16.3): named layouts from `layouts/*.toml` (+ built-ins),
//! pop-out panels as separate viewports (`stream-engine.<panel>`), the confidence window
//! `stream-engine.program` on the TV that follows hotplug (TV absent → DP-2, back when it
//! returns), placement through Hyprland, and the `--program` mode.

use crate::app::{App, Page};
use crate::frames::Canvas;
use crate::layout::{self, Canvases, Layout, LayoutSet};
use crate::monitors::MonitorWatcher;
use crate::panels::Panel;
use crate::views::monitor;
use egui::{RichText, Vec2, ViewportBuilder, ViewportId};
use se_proto::Value;
use se_ui_kit::widgets::LedState;
use std::collections::BTreeSet;

pub struct Windows {
    pub set: LayoutSet,
    /// Layout the operator picked.
    pub chosen: String,
    /// Layout in effect (the chosen one, or its auto-fallback while the TV is off).
    pub effective: String,
    /// Layout whose settings (mode, dock, pop-outs) were last applied.
    applied: Option<String>,
    /// Panel ids open as their own windows.
    pub popouts: BTreeSet<String>,
    watcher: Option<MonitorWatcher>,
    seen_gen: u64,
    /// Monitor the confidence window is on (for hotplug follow).
    pub confidence_on: Option<String>,
    files_seq: u64,
    files_conn: u64,
    reads: Vec<String>,
    reads_done: BTreeSet<String>,
    pub errors: Vec<String>,
}

impl Windows {
    pub fn new(ctx: &egui::Context, chosen: Option<String>) -> Windows {
        let set = LayoutSet::builtin();
        let chosen = chosen.filter(|c| set.get(c).is_some() || !c.is_empty()).unwrap_or_else(|| layout::DEFAULT_LAYOUT.into());
        Windows {
            set,
            effective: chosen.clone(),
            chosen,
            applied: None,
            popouts: BTreeSet::new(),
            watcher: crate::monitors::Hypr::detect().map(|_| MonitorWatcher::start(ctx.clone())),
            seen_gen: 0,
            confidence_on: None,
            files_seq: 0,
            files_conn: 0,
            reads: Vec::new(),
            reads_done: BTreeSet::new(),
            errors: Vec::new(),
        }
    }

    pub fn layout(&self) -> Layout {
        self.set.get(&self.effective).or_else(|| self.set.get(&self.chosen)).cloned().unwrap_or_default()
    }

    pub fn monitors(&self) -> Vec<crate::monitors::Monitor> {
        self.watcher.as_ref().map(|w| w.latest().monitors.as_ref().clone()).unwrap_or_default()
    }

    pub fn hyprland(&self) -> bool {
        self.watcher.as_ref().is_some_and(|w| w.latest().hyprland)
    }
}

/// Read `layouts/*.toml` through the engine (on connect and when the project reloads them).
fn load_files(app: &mut App) {
    let conn = app.m.conn_gen;
    let reloaded = app.m.events.iter().rev().take(4).any(|e| {
        e.ty == "project.reloaded"
            && e.payload.get_path("files").and_then(Value::as_list).is_some_and(|l| l.iter().any(|f| f.as_str().is_some_and(|s| s.starts_with("layouts/"))))
    });
    if app.m.connected && (app.win.files_conn != conn || reloaded && app.win.reads_done.len() == app.win.reads.len()) {
        app.win.files_conn = conn;
        app.m.query_as("layouts.files", "project.files", Value::map().with("kind", "layouts"));
    }
    let seq = app.m.q_seq("layouts.files");
    if seq != app.win.files_seq {
        app.win.files_seq = seq;
        let paths: Vec<String> = app.m.q_list("layouts.files").iter().filter_map(|f| f.get_path("path").and_then(Value::as_str).map(String::from)).collect();
        app.win.reads_done.clear();
        for p in &paths {
            app.m.query_as(&format!("project.read:{p}"), "project.read", Value::map().with("path", p.clone()));
        }
        app.win.reads = paths;
    }
    if app.win.reads.is_empty() || app.win.reads_done.len() == app.win.reads.len() {
        return;
    }
    let ready: Vec<(String, String)> = app
        .win
        .reads
        .iter()
        .filter_map(|p| app.m.q(&format!("project.read:{p}")).and_then(|v| v.get_path("text")).and_then(Value::as_str).map(|t| (p.clone(), t.to_string())))
        .collect();
    if ready.len() < app.win.reads.len() {
        return;
    }
    app.win.reads_done = app.win.reads.iter().cloned().collect();
    let (set, errors) = LayoutSet::from_files(ready.iter().map(|(p, t)| (p.as_str(), t.as_str())));
    for e in &errors {
        app.m.toast(format!("layout: {e}"), true);
    }
    app.win.errors = errors;
    app.win.set = set;
    if let Some((_, t)) = ready.iter().find(|(p, _)| p == crate::shortcuts_ui::SHORTCUTS_FILE) {
        let t = t.clone();
        crate::shortcuts_ui::load(app, &t);
    }
    if app.win.set.get(&app.win.chosen).is_none() {
        app.m.toast(format!("layout `{}` not found; using {}", app.win.chosen, layout::DEFAULT_LAYOUT), true);
        app.win.chosen = layout::DEFAULT_LAYOUT.into();
    }
    // re-apply with the file versions
    app.win.applied = None;
    app.win.seen_gen = u64::MAX;
}

/// Apply a layout's UI settings (start page, zoom, rail, pop-outs).
fn apply(app: &mut App, l: &Layout, ctx: &egui::Context) {
    if app.win.applied.as_deref() == Some(l.name.as_str()) {
        return;
    }
    let first = app.win.applied.is_none();
    app.win.applied = Some(l.name.clone());
    if !app.program_only
        && let Some(p) = Page::parse(&l.page)
    {
        app.page = p;
    }
    if first || (l.zoom - app.zoom).abs() > f32::EPSILON {
        app.set_zoom(ctx, l.zoom.clamp(0.6, 2.0));
    }
    if let Some(tab) = crate::views::show::RailTab::parse(&l.show.rail_tab) {
        app.show.rail = tab;
    }
    app.show.rail_width = l.show.rail_width.max(260.0);
    app.show.multiview_in_main = l.show.multiview;
    app.win.popouts = l.windows.iter().filter(|p| Panel::parse(&p.panel).is_some()).map(|p| p.panel.clone()).collect();
    for p in l.warnings() {
        tracing::warn!("layout {}: {p}", l.name);
    }
}

/// Per frame: layouts from files, monitor hotplug → effective layout → window placement.
pub fn tick(app: &mut App, ctx: &egui::Context) {
    load_files(app);
    let snap = app.win.watcher.as_ref().map(|w| w.latest());
    let generation = snap.as_ref().map(|s| s.generation).unwrap_or(0);
    let monitors = snap.as_ref().map(|s| s.monitors.as_ref().clone()).unwrap_or_default();
    let chosen = app.win.set.get(&app.win.chosen).cloned().unwrap_or_default();
    let eff = layout::effective(&chosen, &app.win.set, &monitors).clone();
    if eff.name != app.win.effective {
        if app.win.applied.is_some() {
            app.m.toast(format!("layout → {} ({})", eff.name, if eff.name == chosen.name { "TV back" } else { "TV off" }), false);
        }
        app.win.effective = eff.name.clone();
    }
    apply(app, &eff, ctx);
    if generation == app.win.seen_gen || generation == 0 {
        return;
    }
    app.win.seen_gen = generation;
    let (plans, placement) = eff.plan(&monitors, app.win.confidence_on.as_deref());
    if placement.changed {
        tracing::info!("confidence window → {:?} ({:?})", placement.monitor, placement.reason);
    }
    app.win.confidence_on = placement.monitor.clone();
    let Some(w) = app.win.watcher.as_ref() else { return };
    for p in plans {
        let ours = if app.program_only {
            p.app_id == layout::CONFIDENCE_APP_ID
        } else {
            p.app_id == layout::MAIN_APP_ID
                || p.app_id == layout::CONFIDENCE_APP_ID
                || app.win.popouts.iter().any(|id| format!("{}.{id}", layout::MAIN_APP_ID) == p.app_id)
        };
        match (ours, &p.monitor) {
            (true, Some(m)) => w.place(&p.app_id, m, p.fullscreen),
            _ => w.forget(&p.app_id),
        }
    }
}

/// Keep every stream-engine window fully opaque on Hyprland (Omarchy makes windows slightly
/// see-through by default; a video app must not be). Runtime rules, nothing written to disk.
pub fn make_opaque() {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return;
    }
    let lua = r#"hl.window_rule({ match = { class = "^stream-engine(\\..+)?$" }, tag = "-default-opacity" }); hl.window_rule({ match = { class = "^stream-engine(\\..+)?$" }, opacity = "1 1" })"#;
    let _ = std::process::Command::new("hyprctl").args(["eval", lua]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}

/// Every pop-out panel in its own OS window.
pub fn popouts(app: &mut App, ctx: &egui::Context) {
    let l = app.win.layout();
    let ids: Vec<String> = app.win.popouts.iter().cloned().collect();
    for id in ids {
        let Some(panel) = Panel::parse(&id) else { continue };
        let size =
            l.windows.iter().find(|p| p.panel == id).and_then(|p| p.size).unwrap_or(if panel == Panel::Multiview { [1600.0, 300.0] } else { [900.0, 640.0] });
        let builder = ViewportBuilder::default()
            .with_app_id(format!("{}.{id}", layout::MAIN_APP_ID))
            .with_title(format!("stream-engine · {}", panel.title()))
            .with_inner_size(size);
        let mut close = false;
        ctx.show_viewport_immediate(ViewportId::from_hash_of(("popout", &id)), builder, |ui, _class| {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(panel.title()).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Put back").on_hover_text("Put this back into the main window").clicked() {
                            close = true;
                        }
                    });
                });
                ui.separator();
                panel.ui(app, ui);
            });
            if ui.ctx().input(|i| i.viewport().close_requested()) {
                close = true;
            }
        });
        if close {
            app.dock_back(&id);
        }
    }
}

/// The confidence window (unless running as `--program`, where the root window is it).
pub fn confidence(app: &mut App, ctx: &egui::Context) {
    let l = app.win.layout();
    if !l.confidence.enabled {
        return;
    }
    // With Hyprland, only open it once a target monitor exists (TV, else DP-2).
    if app.win.hyprland() && app.win.confidence_on.is_none() {
        return;
    }
    let size = l.confidence.size.unwrap_or([1920.0, 1080.0]);
    let builder = ViewportBuilder::default()
        .with_app_id(layout::CONFIDENCE_APP_ID)
        .with_title("stream-engine · program")
        .with_decorations(false)
        .with_inner_size(size)
        // without Hyprland placement the window manager can't be told the monitor; go fullscreen here
        .with_fullscreen(l.confidence.fullscreen && !app.win.hyprland());
    ctx.show_viewport_immediate(ViewportId::from_hash_of("confidence"), builder, |ui, _class| {
        egui::CentralPanel::default().frame(egui::Frame::new().fill(egui::Color32::BLACK)).show(ui, |ui| confidence_contents(app, ui));
    });
}

/// `stream-engine ui --program`: the root window is the confidence window.
pub fn program_root(app: &mut App, ui: &mut egui::Ui) {
    egui::CentralPanel::default().frame(egui::Frame::new().fill(egui::Color32::BLACK)).show(ui, |ui| confidence_contents(app, ui));
    if ui.ctx().input(|i| i.key_pressed(egui::Key::F11)) {
        let fs = ui.ctx().input(|i| i.viewport().fullscreen.unwrap_or(false));
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fs));
    }
}

/// Exactly what goes out: program wide / tall (per layout), scaled to fit, no chrome.
pub fn confidence_contents(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let l = app.win.layout();
    let program = app.m.str("show.scene.program").to_string();
    let hz = app.confidence_hz();
    let avail = ui.available_size();
    let status_h = 22.0;
    let area = Vec2::new(avail.x, (avail.y - status_h).max(40.0));
    let gap = 8.0;
    let (wide, tall) = match l.confidence.canvases {
        Canvases::Wide => (true, false),
        Canvases::Tall => (false, true),
        Canvases::Both => (true, true),
    };
    // fit: wide 16:9 and/or tall 9:16 at a common height
    let unit = match (wide, tall) {
        (true, true) => 16.0 / 9.0 + 9.0 / 16.0,
        (true, false) => 16.0 / 9.0,
        _ => 9.0 / 16.0,
    };
    let h = ((area.x - if wide && tall { gap } else { 0.0 }) / unit).min(area.y).max(20.0);
    let used = h * unit + if wide && tall { gap } else { 0.0 };
    ui.horizontal(|ui| {
        ui.add_space(((avail.x - used) / 2.0).max(0.0));
        if wide {
            monitor::monitor(app, ui, Canvas::Wide, "", "", Vec2::new(h * 16.0 / 9.0, h), LedState::Active, hz);
        }
        if wide && tall {
            ui.add_space(gap);
        }
        if tall {
            monitor::monitor(app, ui, Canvas::Tall, "", "", Vec2::new(h * 9.0 / 16.0, h), LedState::Active, hz);
        }
    });
    let since = app.m.get("show.live_since").and_then(Value::as_i64).unwrap_or(0);
    let rec = app.m.b("obs.record.active");
    let status = if since > 0 {
        format!("● LIVE {}{}  ·  {program}", crate::views::status::hms((crate::views::status::now_ms() - since) / 1000), if rec { "  ·  REC" } else { "" })
    } else {
        format!("{}  ·  {program}", app.m.str("show.mode").to_uppercase())
    };
    ui.vertical_centered(|ui| ui.label(RichText::new(status).small().color(if since > 0 { t.tally_program() } else { t.fg_dim })));
}
