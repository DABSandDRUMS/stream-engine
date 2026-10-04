//! Keyboard handling (§15.7): dispatches the rebindable shortcuts and the hold-to-panic chord,
//! and loads/saves `layouts/shortcuts.toml` (the Settings → Accounts & app card rebinds keys).

use crate::app::{App, Page};
use crate::shortcuts::{self, Chord, HoldState, HoldTracker, Shortcuts};
use egui::RichText;
use se_proto::{Op, Value};
use se_ui_kit::widgets::icon;

pub const SHORTCUTS_FILE: &str = "layouts/shortcuts.toml";

#[derive(Default)]
pub struct Keys {
    pub map: Shortcuts,
    pub hold: HoldTracker,
    /// Progress of a running hold chord (0–1) for the on-screen indicator.
    pub holding: Option<f32>,
    /// Action waiting for a key press in Settings.
    pub rebinding: Option<&'static str>,
    pub error: Option<String>,
    /// Text of `layouts/shortcuts.toml` as last read (for comment-preserving saves).
    pub file_text: Option<String>,
}

/// Run every shortcut for this frame.
pub fn handle(app: &mut App, ctx: &egui::Context) {
    // rebinding captures the next chord instead of running it
    if let Some(action) = app.keys.rebinding {
        if let Some(chord) = Chord::capture(ctx) {
            if chord.key == egui::Key::Escape && !chord.ctrl && !chord.shift && !chord.alt {
                app.keys.rebinding = None;
                return;
            }
            let chord = Chord { hold: shortcuts::action(action).is_some_and(|a| a.hold), ..chord };
            match app.keys.map.rebind(action, chord) {
                Ok(()) => {
                    app.keys.error = None;
                    save(app);
                }
                Err(e) => app.keys.error = Some(e.to_string()),
            }
            app.keys.rebinding = None;
        }
        return;
    }
    let panic_chords = app.keys.map.chords("panic").to_vec();
    match app.keys.hold.update(ctx, &panic_chords, shortcuts::HOLD_SECS) {
        HoldState::Fired => {
            app.m.command(Op::Panic);
            app.keys.holding = None;
        }
        HoldState::Holding(p) => {
            app.keys.holding = Some(p);
            ctx.request_repaint();
        }
        HoldState::Idle => app.keys.holding = None,
    }
    let fired = |app: &App, a: &str| app.keys.map.fired(ctx, a);
    if fired(app, "palette") {
        app.palette.open = !app.palette.open;
        app.palette.text.clear();
        app.palette.cursor = 0;
    }
    if app.palette.open {
        return;
    }
    if fired(app, "clean") {
        app.m.command(Op::Clean);
    }
    if fired(app, "redo") {
        app.m.command(Op::Redo);
    }
    if fired(app, "undo") {
        app.m.command(Op::Undo);
    }
    if fired(app, "zoom.in") {
        app.set_zoom(ctx, (app.zoom + 0.1).min(2.0));
    }
    if fired(app, "zoom.out") {
        app.set_zoom(ctx, (app.zoom - 0.1).max(0.6));
    }
    if fired(app, "layout.next") {
        app.next_layout();
    }
    if fired(app, "search") {
        if app.page == Page::Live {
            app.show.rail = crate::views::show::RailTab::Queue;
        } else {
            app.build.library_focus = true;
        }
    }
    if fired(app, "escape") {
        if app.m.b("controllers.learn.active") {
            app.m.action("midi.learn.cancel", Value::Null);
        } else if app.page != Page::Live && app.selected.is_none() && app.build.canvas.selected.is_none() {
            app.page = Page::Live;
        } else {
            app.selected = None;
            app.build.canvas.selected = None;
        }
    }
    if fired(app, "mode.toggle") {
        app.toggle_page();
    }
    if fired(app, "take") {
        app.take();
    }
    let scenes: Vec<(Option<i64>, String)> = app
        .m
        .q_list("scenes")
        .iter()
        .map(|s| (s.get_path("key").and_then(Value::as_i64), s.get_path("name").and_then(Value::as_str).unwrap_or("").to_string()))
        .collect();
    for n in 1..=9usize {
        let target = scenes.iter().find(|(k, _)| *k == Some(n as i64)).or_else(|| scenes.get(n - 1).filter(|(k, _)| k.is_none())).map(|(_, s)| s.clone());
        if fired(app, &format!("scene.program.{n}")) {
            if let Some(s) = &target {
                app.m.command(Op::SceneCut { scene: s.clone(), transition: app.show.transition.clone() });
            }
        } else if fired(app, &format!("scene.preview.{n}"))
            && let Some(s) = target
        {
            app.m.command(Op::SceneGo { scene: s });
        }
    }
    for n in 1..=12usize {
        if fired(app, &format!("pad.{n}")) {
            crate::views::show::fire_pad(app, n - 1);
        }
    }
}

/// Hold-chord progress overlay (e.g. panic).
pub fn overlay(app: &App, ctx: &egui::Context) {
    let Some(p) = app.keys.holding else { return };
    let t = app.t.clone();
    egui::Area::new(egui::Id::new("hold-overlay")).anchor(egui::Align2::CENTER_TOP, [0.0, 48.0]).show(ctx, |ui| {
        egui::Frame::new().fill(t.bg_dark).stroke(egui::Stroke::new(2.0, t.bright_red)).corner_radius(6).inner_margin(10).show(ui, |ui| {
            ui.label(RichText::new(format!("{} hold for PANIC", icon::WARN)).strong().color(t.bright_red));
            ui.add(egui::ProgressBar::new(p).desired_width(220.0).fill(t.bright_red));
        });
    });
}

/// Load `layouts/shortcuts.toml` text (from `project.read`).
pub fn load(app: &mut App, text: &str) {
    match Shortcuts::load(text) {
        Ok((map, warnings)) => {
            app.keys.map = map;
            for w in warnings {
                app.m.toast(format!("shortcuts.toml: {w}"), true);
            }
        }
        Err(e) => app.m.toast(format!("shortcuts.toml: {e:#} (defaults in use)"), true),
    }
    app.keys.file_text = Some(text.to_string());
}

/// Persist the current bindings (only overrides are written; comments kept).
pub fn save(app: &mut App) {
    let text = app.keys.map.to_toml(app.keys.file_text.as_deref());
    app.keys.file_text = Some(text.clone());
    app.m.action("project.write", Value::map().with("path", SHORTCUTS_FILE).with("text", text));
}
