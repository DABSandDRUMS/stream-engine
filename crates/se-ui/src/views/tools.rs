//! Build-mode dock tools (§15.5): signal scopes (with the shaped output of every binding on
//! that signal), trace (event → rule → command chains), simulator presets, and the console
//! (logs, project and patch errors; click a `file:line` to open it in `$EDITOR`).

use crate::app::App;
use crate::views::modulate::Draft;
use egui::{RichText, Vec2};
use se_core::bindings::BindingRt;
use se_core::config::BindingDef;
use se_proto::Value;
use se_ui_kit::widgets::{self, icon};

pub struct ToolsState {
    pub scope_pattern: String,
    pub sim_args: String,
    pub console_filter: String,
    pub console_errors_only: bool,
}

impl Default for ToolsState {
    fn default() -> Self {
        ToolsState { scope_pattern: "band.* music.* lfo.*".into(), sim_args: String::new(), console_filter: String::new(), console_errors_only: false }
    }
}

/// Simulator presets called out in the plan (§15.5), on top of the engine's `sim.presets`.
pub const SIM_SHORTCUTS: &[(&str, &str)] = &[
    ("gift bomb 50", "sim.gift_bomb count=50 tier=1"),
    ("raid 300", "sim.raid viewers=300"),
    ("1000 bits with message", "sim.cheer bits=1000 message='This stream rocks!'"),
];

pub fn scopes(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.horizontal(|ui| {
        ui.label(format!("{} signals", icon::SCOPE));
        ui.add(egui::TextEdit::singleline(&mut app.build.tools.scope_pattern).hint_text("space-separated patterns: band.* midi.xtouch.*").desired_width(360.0));
        ui.label(RichText::new("magenta = shaped output of bindings on the signal").small().color(t.modulated()));
    });
    if app.m.q("config.bindings").is_none() || app.m.q_seq("config.bindings") == 0 {
        app.m.query("config.bindings", Value::Null);
    }
    let pats: Vec<String> = app.build.tools.scope_pattern.split_whitespace().map(String::from).collect();
    let mut names: Vec<String> = app.m.signals.keys().filter(|n| pats.iter().any(|p| se_proto::address::matches(p, n))).cloned().collect();
    names.sort();
    let defs: Vec<BindingDef> = app
        .m
        .q("config.bindings")
        .and_then(Value::as_list)
        .unwrap_or(&[])
        .iter()
        .filter_map(|v| serde_json::from_value::<BindingDef>(serde_json::Value::from(v)).ok())
        .collect();
    if names.is_empty() {
        ui.label(RichText::new("no signals match").color(t.fg_dim));
        return;
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            for n in names.iter().take(32) {
                let v: Vec<f32> = app.m.signals[n].iter().copied().collect();
                let bound: Vec<&BindingDef> = defs.iter().filter(|d| d.signal == *n && d.enabled).collect();
                ui.vertical(|ui| {
                    ui.label(RichText::new(format!("{n} {:.3}", v.last().copied().unwrap_or(0.0))).small().monospace());
                    match bound.first() {
                        Some(def) => {
                            let shaped: Vec<f32> = match BindingRt::new((*def).clone()) {
                                Ok(mut rt) => v.iter().map(|x| rt.shape(*x as f64, 1.0 / 30.0) as f32).collect(),
                                Err(_) => Vec::new(),
                            };
                            let r = def.range.map(|[a, b]| [a.min(b) as f32, a.max(b) as f32]).unwrap_or([0.0, 1.0]);
                            se_ui_kit::curve::dual_scope(ui, &t, Vec2::new(240.0, 64.0), &v, &shaped, r);
                            let more = if bound.len() > 1 { format!(" +{}", bound.len() - 1) } else { String::new() };
                            let lbl = ui.add(
                                egui::Label::new(RichText::new(format!("→ {}{more}", def.target)).small().color(t.modulated())).sense(egui::Sense::click()),
                            );
                            if lbl.on_hover_text("edit this binding").clicked() {
                                let mut d = Draft::new(&def.target, &se_proto::Meta::default());
                                d.name = def.name.clone();
                                app.build.modulate.draft = Some(d);
                                let row = Value::map()
                                    .with("name", def.name.clone())
                                    .with("target", def.target.clone())
                                    .with("signal", def.signal.clone())
                                    .with("file", def.file.clone());
                                app.build.modulate.edit_existing(&row);
                                app.focus_panel(crate::panels::Panel::Modulate);
                            }
                        }
                        None => {
                            widgets::scope(ui, &t, Vec2::new(240.0, 64.0), &v, None, None);
                        }
                    }
                });
            }
        });
    });
}

pub fn trace(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.columns(2, |cols| {
        cols[0].label(RichText::new("recent roots (click to follow)").small().color(t.fg_dim));
        let mut follow = None;
        egui::ScrollArea::vertical().id_salt("trace-recent").stick_to_bottom(true).auto_shrink([false, false]).show(&mut cols[0], |ui| {
            let roots: Vec<(u64, String)> =
                app.m.trace.iter().filter(|r| r.parent.is_none()).rev().take(300).map(|r| (r.id, format!("{} {}", r.kind, r.label))).collect();
            for (id, label) in roots.into_iter().rev() {
                if ui.selectable_label(app.m.trace_view.first().map(|r| r.id) == Some(id), label).clicked() {
                    follow = Some(id);
                }
            }
        });
        if let Some(id) = follow {
            app.m.trace_of(id);
        }
        cols[1].label(RichText::new("cause chain").small().color(t.fg_dim));
        egui::ScrollArea::vertical().id_salt("trace-view").auto_shrink([false, false]).show(&mut cols[1], |ui| {
            if app.m.trace_view.is_empty() {
                ui.label(RichText::new("select an event here, in the Events rail, or a value's provenance").color(t.fg_dim));
            }
            let mut depth: std::collections::HashMap<u64, usize> = Default::default();
            for r in &app.m.trace_view {
                let d = r.parent.and_then(|p| depth.get(&p).copied()).map(|d| d + 1).unwrap_or(0);
                depth.insert(r.id, d);
                let c = match r.kind.as_str() {
                    "event" => t.cyan,
                    "rule" => t.yellow,
                    "command" => t.blue,
                    "preset" => t.magenta,
                    "change" => t.green,
                    "policy" => t.orange,
                    "error" => t.bright_red,
                    _ => t.fg,
                };
                ui.label(RichText::new(format!("{}{} {}", "  ".repeat(d), r.kind, r.label)).color(c).monospace());
            }
        });
    });
}

pub fn simulator(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.horizontal(|ui| {
        ui.label(format!("{} extra args", icon::SIM));
        ui.add(egui::TextEdit::singleline(&mut app.build.tools.sim_args).hint_text("user=drumfan message='hi' tier=2").desired_width(400.0));
    });
    ui.label(RichText::new("realistic scenarios").small().color(t.fg_dim));
    let extra = app.build.tools.sim_args.trim().to_string();
    ui.horizontal_wrapped(|ui| {
        for (label, cmd) in SIM_SHORTCUTS {
            if ui.button(RichText::new(*label).strong().color(t.accent)).on_hover_text(*cmd).clicked() {
                app.m.text(format!("{cmd} {extra}").trim());
            }
        }
    });
    ui.label(RichText::new("engine presets (sim.presets)").small().color(t.fg_dim));
    let presets = app.m.q_list("sim.presets").to_vec();
    if presets.is_empty() {
        ui.label(RichText::new("engine offline").color(t.fg_dim));
    }
    ui.horizontal_wrapped(|ui| {
        for p in presets {
            let n = p.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
            let defaults = p.get_path("args").and_then(Value::as_str).unwrap_or("").to_string();
            if ui.button(format!("{} {n}", icon::SIM)).on_hover_text(format!("sim.{n} {defaults} {extra}")).clicked() {
                // user args come last so they override the preset defaults
                app.m.text(format!("sim.{n} {defaults} {extra}").trim());
            }
        }
    });
    ui.label(RichText::new("every simulated event goes through the full pipeline (policy, rules, presets) and shows in Trace").small().color(t.fg_dim));
}

/// `(path, line)` found in a message: `path/file.ext:12[:col]` or `file … line 12`.
pub fn file_line(msg: &str, file_hint: Option<&str>) -> Option<(String, Option<u32>)> {
    for tok in msg.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '`' || c == '"' || c == '\'' || c == ',') {
        let tok = tok.trim_end_matches(':');
        let mut parts = tok.split(':');
        let (Some(path), line) = (parts.next(), parts.next()) else { continue };
        let has_ext =
            path.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && matches!(ext, "toml" | "lua" | "wgsl" | "js" | "html" | "css" | "json" | "rs"));
        if has_ext {
            return Some((path.to_string(), line.and_then(|l| l.parse().ok())));
        }
    }
    let line = msg.split("line ").nth(1).and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()).and_then(|n| n.parse().ok());
    file_hint.map(|f| (f.to_string(), line))
}

pub fn console(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.horizontal(|ui| {
        ui.label(format!("{} filter", icon::CONSOLE));
        ui.add(egui::TextEdit::singleline(&mut app.build.tools.console_filter).desired_width(260.0));
        ui.checkbox(&mut app.build.tools.console_errors_only, "errors only");
        if ui.small_button("reload project").clicked() {
            app.m.text("project.reload");
        }
    });
    let mut open: Option<(String, Option<u32>)> = None;
    // project file errors (the last good version stays live) and patch errors
    let mut errs: Vec<(String, String)> = app
        .m
        .q_list("errors")
        .iter()
        .map(|e| (e.get_path("file").and_then(Value::as_str).unwrap_or("").to_string(), e.get_path("msg").and_then(Value::as_str).unwrap_or("").to_string()))
        .collect();
    for (a, v) in app.m.under("patch") {
        if a.ends_with(".error")
            && let Some(msg) = v.as_str().filter(|s| !s.is_empty())
        {
            let id = a.trim_start_matches("patch.").trim_end_matches(".error");
            errs.push((format!("patches/{id}/"), msg.to_string()));
        }
    }
    for (file, msg) in &errs {
        let target = file_line(msg, Some(file))
            .map(|(p, l)| if p.contains('/') || file.is_empty() || !file.ends_with('/') { (p, l) } else { (format!("{file}{p}"), l) });
        let text = format!("{} {file}: {msg}", icon::WARN);
        let r = ui.add(egui::Label::new(RichText::new(text).color(t.bright_red)).sense(egui::Sense::click()));
        if let Some(tg) = target
            && r.on_hover_text(format!("open {}{}", tg.0, tg.1.map(|l| format!(":{l}")).unwrap_or_default())).clicked()
        {
            open = Some(tg);
        }
    }
    if !errs.is_empty() {
        ui.separator();
    }
    let f = app.build.tools.console_filter.to_lowercase();
    let errors_only = app.build.tools.console_errors_only;
    egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
        for l in app.m.logs.iter().filter(|l| {
            (!errors_only || l.level == "error" || l.level == "warn") && (f.is_empty() || l.msg.to_lowercase().contains(&f) || l.target.contains(&f))
        }) {
            let c = match l.level.as_str() {
                "error" => t.bright_red,
                "warn" => t.yellow,
                _ => t.fg_dim,
            };
            let target = file_line(&l.msg, None);
            let r = ui.add(egui::Label::new(RichText::new(format!("[{}] {}: {}", l.level, l.target, l.msg)).color(c).monospace()).sense(egui::Sense::click()));
            if let Some(tg) = target
                && r.on_hover_text("click: open in $EDITOR").clicked()
            {
                open = Some(tg);
            }
        }
    });
    if let Some((p, l)) = open {
        app.open_in_editor(&p, l);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_file_and_line_in_messages() {
        assert_eq!(file_line("patches/sub_meteors/main.lua:42: attempt to index nil", None), Some(("patches/sub_meteors/main.lua".into(), Some(42))));
        assert_eq!(file_line("shader error in (patches/aurora/shader.wgsl:7:3)", None), Some(("patches/aurora/shader.wgsl".into(), Some(7))));
        assert_eq!(file_line("TOML parse error at line 3, column 9", Some("rules/subs.toml")), Some(("rules/subs.toml".into(), Some(3))));
        assert_eq!(file_line("rule `x` has no `when`", Some("rules/x.toml")), Some(("rules/x.toml".into(), None)));
        assert_eq!(file_line("engine ready (session abc)", None), None);
        assert_eq!(file_line("value 0.5 out of range", None), None);
    }
}
