//! Settings → Troubleshooting tools (§15.5): test events as big "pretend" buttons (simulator),
//! "why did that happen?" as a readable cause chain (trace), messages and file problems with
//! "Open file" (console; `file:line` opens in `$EDITOR`), and live signal graphs with the shaped
//! output of every link on that signal (scopes).

use crate::app::App;
use crate::views::live::nice;
use crate::views::modulate::Draft;
use egui::{Align, Layout, RichText, Ui, Vec2};
use se_core::bindings::BindingRt;
use se_core::config::BindingDef;
use se_proto::Value;
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_medium, font_mono, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, LedState, Size, icon};

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

/// The three big test events (§15.5), on top of the engine's `sim.presets`. Labels read after
/// "Pretend …" here and after "simulate …" in the command palette.
pub const SIM_SHORTCUTS: &[(&str, &str)] = &[
    ("a raid of 300", "sim.raid viewers=300"),
    ("1000 bits with a message", "sim.cheer bits=1000 message='This stream rocks!'"),
    ("a gift bomb of 50", "sim.gift_bomb count=50 tier=1"),
];

fn sim_icon(cmd: &str) -> &'static str {
    let n = cmd.trim_start_matches("sim.").split_whitespace().next().unwrap_or("");
    match n {
        "raid" => icon::USERS,
        "follow" => icon::EYE,
        "cheer" => icon::BOLT,
        "tip" => icon::HAND,
        "gift_bomb" => icon::GIFT,
        "sub" => icon::HEART,
        "resub" => icon::UNDO,
        "redeem" => icon::STAR,
        "chat" => icon::CHAT,
        "ad_break" => icon::CLOCK,
        "hype_train" => icon::ROCKET,
        _ => icon::SIM,
    }
}

/// Plain words for an engine simulator preset.
fn sim_words(name: &str) -> String {
    match name {
        "cheer" => "Cheer (1000 bits)".into(),
        "sub" => "New subscriber".into(),
        "resub" => "Resub (12 months)".into(),
        "gift_bomb" => "Gift bomb (50 subs)".into(),
        "follow" => "New follower".into(),
        "raid" => "Raid (300 viewers)".into(),
        "redeem" => "Channel points".into(),
        "chat" => "Chat message".into(),
        "tip" => "Tip ($5)".into(),
        "ad_break" => "Ad break (90 s)".into(),
        "hype_train" => "Hype train (level 2)".into(),
        other => nice(other),
    }
}

/// Lay `n` equal tiles between `min_w` and `max_w` wide across the width; returns (per row, tile width).
fn tile_grid(ui: &Ui, n: usize, min_w: f32, max_w: f32, gap: f32) -> (usize, f32) {
    let w = ui.available_width();
    let per = (((w + gap) / (min_w + gap)).floor() as usize).clamp(1, n.max(1));
    (per, ((w - gap * (per as f32 - 1.0)) / per as f32).floor().min(max_w))
}

pub fn simulator(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    let extra = app.build.tools.sim_args.trim().to_string();
    let mut fire: Option<(String, String)> = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        widgets::section(ui, &t, "", "Try the big ones");
        let gap = spacing::M;
        let (per, w) = tile_grid(ui, SIM_SHORTCUTS.len(), 220.0, f32::INFINITY, gap);
        for row in SIM_SHORTCUTS.chunks(per) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for (label, cmd) in row {
                    let words = format!("Pretend {label}");
                    if widgets::pad(ui, &t, Vec2::new(w, 112.0), sim_icon(cmd), &words, None, LedState::Idle, None, None).on_hover_text(*cmd).clicked() {
                        fire = Some((words, format!("{cmd} {extra}")));
                    }
                }
            });
            ui.add_space(gap);
        }
        ui.add_space(spacing::S);
        widgets::section(ui, &t, "", "Everything else");
        let presets = app.m.q_list("sim.presets").to_vec();
        if presets.is_empty() {
            widgets::hint(ui, &t, if app.m.connected { "Loading…" } else { "Shows up when Stream Engine is running." });
        }
        let (per, w) = tile_grid(ui, presets.len(), 170.0, f32::INFINITY, gap);
        for row in presets.chunks(per) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for p in row {
                    let n = p.get_path("name").and_then(Value::as_str).unwrap_or("");
                    let defaults = p.get_path("args").and_then(Value::as_str).unwrap_or("");
                    // user args come last so they override the preset defaults
                    let cmd = format!("sim.{n} {defaults} {extra}");
                    let words = sim_words(n);
                    if widgets::pad(ui, &t, Vec2::new(w, 84.0), sim_icon(n), &words, None, LedState::Idle, None, None).on_hover_text(cmd.trim()).clicked() {
                        fire = Some((format!("Pretend: {}", words.to_lowercase()), cmd));
                    }
                }
            });
            ui.add_space(gap);
        }
        ui.add_space(spacing::S);
        widgets::details(ui, &t, "sim-extra", "Change who it's from and other details", |ui| {
            ui.add(se_ui_kit::widgets::field(&mut app.build.tools.sim_args).hint_text("user=drumfan message='hi' tier=2").desired_width(420.0));
            widgets::hint(ui, &t, "Added to every test above, e.g. user=drumfan picks who it's from.");
        });
        ui.add_space(spacing::S);
        widgets::hint(
            ui,
            &t,
            "Each test goes through everything a real one does: reactions, alerts and effects. See what it set off in “Why did that happen?”.",
        );
    });
    if let Some((words, cmd)) = fire {
        app.m.text(cmd.trim());
        app.m.toast(format!("{words}. Watch your alerts."), false);
    }
}

// ---- trace ---------------------------------------------------------------------------------------

/// (icon, plain word, color) for a trace record kind.
fn trace_kind(t: &Theme, kind: &str) -> (&'static str, &'static str, egui::Color32) {
    match kind {
        "event" => (icon::BOLT, "Something happened", t.cyan),
        "rule" => (icon::RULE, "A reaction ran", t.yellow),
        "command" => (icon::RIGHT, "It did", t.blue),
        "preset" => (icon::SPARKLE, "A quick effect", t.magenta),
        "change" => (icon::EDIT, "Changed", t.green),
        "policy" => (icon::MOD, "A safety check", t.orange),
        "error" => (icon::WARN, "Problem", t.bright_red),
        _ => (icon::DOT, "Step", t.text_dim),
    }
}

pub fn trace(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    let mut follow = None;
    let list_w = (ui.available_width() * 0.36).clamp(280.0, 520.0);
    let h = ui.available_height();
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(Vec2::new(list_w, h), Layout::top_down(Align::Min), |ui| {
            ui.set_width(list_w);
            widgets::section(ui, &t, "", "Recent events");
            let roots: Vec<(u64, String, String)> =
                app.m.trace.iter().filter(|r| r.parent.is_none()).rev().take(300).map(|r| (r.id, r.kind.clone(), r.label.clone())).collect();
            if roots.is_empty() {
                widgets::empty_state(ui, &t, icon::TRACE, "Nothing yet", "Events show up here as they happen. Try a test event.", None);
            }
            let selected = app.m.trace_view.first().map(|r| r.id);
            egui::ScrollArea::vertical().id_salt("trace-recent").auto_shrink([false, false]).show(ui, |ui| {
                for (id, kind, label) in &roots {
                    let (ic, words, _) = trace_kind(&t, kind);
                    if widgets::list_row(ui, &t, ic, label, words, "", selected == Some(*id)).clicked() {
                        follow = Some(*id);
                    }
                }
            });
        });
        ui.add_space(spacing::L);
        ui.vertical(|ui| {
            widgets::section(ui, &t, "", "What it set off");
            egui::ScrollArea::vertical().id_salt("trace-view").auto_shrink([false, false]).show(ui, |ui| {
                if app.m.trace_view.is_empty() {
                    widgets::empty_state(ui, &t, icon::TRACE, "Pick an event", "Choose one on the left to see, step by step, what it caused.", None);
                    return;
                }
                chain(ui, &t, &app.m.trace_view);
            });
        });
    });
    if let Some(id) = follow {
        app.m.trace_of(id);
    }
}

/// The cause chain: one card per step, indented under what caused it, joined by a line.
fn chain(ui: &mut Ui, t: &Theme, recs: &[se_proto::wire::TraceRec]) {
    let t0 = recs.first().map(|r| r.ts).unwrap_or(0);
    let mut depth: std::collections::HashMap<u64, usize> = Default::default();
    for (i, r) in recs.iter().enumerate() {
        let d = r.parent.and_then(|p| depth.get(&p).copied()).map(|d| d + 1).unwrap_or(0);
        depth.insert(r.id, d);
        let (ic, words, c) = trace_kind(t, &r.kind);
        ui.horizontal(|ui| {
            ui.add_space(d as f32 * 28.0);
            if d > 0 {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(14.0, 40.0), egui::Sense::hover());
                let p = ui.painter();
                let stroke = egui::Stroke::new(1.5, t.border);
                p.line_segment([rect.left_top() + Vec2::new(4.0, -6.0), egui::pos2(rect.left() + 4.0, rect.center().y)], stroke);
                p.line_segment([egui::pos2(rect.left() + 4.0, rect.center().y), rect.right_center()], stroke);
            }
            egui::Frame::new()
                .fill(mix(t.surface, c, 0.08))
                .stroke(egui::Stroke::new(1.0, mix(t.border, c, 0.35)))
                .corner_radius(radius::CONTROL)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(ic).color(c));
                        ui.label(RichText::new(words).font(font_medium(type_scale::SMALL + 0.5)).color(c));
                        ui.label(RichText::new(&r.label).color(t.fg));
                        if i > 0 {
                            let ms = r.ts.saturating_sub(t0) as f64 / 1e6;
                            ui.label(RichText::new(format!("+{ms:.0} ms")).font(font_mono(type_scale::SMALL)).color(t.text_faint));
                        }
                    });
                });
        });
        ui.add_space(2.0);
    }
}

// ---- console -------------------------------------------------------------------------------------

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

const LOG_ROW: f32 = 30.0;

pub fn console(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
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
    if !errs.is_empty() {
        widgets::section(ui, &t, icon::WARN, &format!("Problems in your files ({})", errs.len()));
        widgets::hint(ui, &t, "The last working version keeps running until you fix these.");
        ui.add_space(spacing::XS);
        for (file, msg) in &errs {
            let target = file_line(msg, Some(file))
                .map(|(p, l)| if p.contains('/') || file.is_empty() || !file.ends_with('/') { (p, l) } else { (format!("{file}{p}"), l) });
            egui::Frame::new()
                .fill(mix(t.surface, t.bright_red, 0.08))
                .stroke(egui::Stroke::new(1.0, mix(t.border, t.bright_red, 0.4)))
                .corner_radius(radius::CONTROL)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(icon::WARN).color(t.bright_red));
                        ui.vertical(|ui| {
                            ui.label(RichText::new(if file.is_empty() { "Project" } else { file.as_str() }).font(font_medium(type_scale::BODY)).color(t.fg));
                            ui.add(egui::Label::new(RichText::new(msg).size(type_scale::SMALL + 0.5).color(t.text_dim)).wrap());
                        });
                        if let Some(tg) = &target {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let tip = format!("{}{}", tg.0, tg.1.map(|l| format!(" at line {l}")).unwrap_or_default());
                                if widgets::button_ex(ui, &t, Some(icon::EDIT), "Open file", Kind::Secondary, Size::Small, 0.0, true)
                                    .on_hover_text(tip)
                                    .clicked()
                                {
                                    open = Some(tg.clone());
                                }
                            });
                        }
                    });
                });
            ui.add_space(spacing::XS);
        }
        ui.add_space(spacing::M);
    }
    // toolbar
    ui.horizontal(|ui| {
        ui.add(se_ui_kit::widgets::field(&mut app.build.tools.console_filter).hint_text("Search messages").desired_width(260.0));
        let mut which = usize::from(app.build.tools.console_errors_only);
        if widgets::segmented(ui, &t, &mut which, &["Everything", "Only problems"]) {
            app.build.tools.console_errors_only = which == 1;
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::button_ex(ui, &t, Some(icon::UNDO), "Reload settings files", Kind::Ghost, Size::Small, 0.0, true)
                .on_hover_text("Read every settings file again")
                .clicked()
            {
                app.m.text("project.reload");
            }
        });
    });
    ui.add_space(spacing::S);
    let f = app.build.tools.console_filter.to_lowercase();
    let errors_only = app.build.tools.console_errors_only;
    let rows: Vec<usize> = app
        .m
        .logs
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            (!errors_only || l.level == "error" || l.level == "warn") && (f.is_empty() || l.msg.to_lowercase().contains(&f) || l.target.contains(&f))
        })
        .map(|(i, _)| i)
        .collect();
    if rows.is_empty() {
        widgets::empty_state(
            ui,
            &t,
            icon::CONSOLE,
            if f.is_empty() { "All quiet" } else { "Nothing matches" },
            if errors_only { "No problems reported." } else { "Messages from Stream Engine show up here." },
            None,
        );
    }
    egui::ScrollArea::vertical().id_salt("console-log").stick_to_bottom(true).auto_shrink([false, false]).show_rows(ui, LOG_ROW, rows.len(), |ui, range| {
        for &i in &rows[range] {
            let l = &app.m.logs[i];
            let (word, c) = match l.level.as_str() {
                "error" => ("Error", t.bright_red),
                "warn" => ("Warning", t.yellow),
                "debug" | "trace" => ("Debug", t.text_faint),
                _ => ("Info", t.text_dim),
            };
            let target = file_line(&l.msg, None);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), LOG_ROW), egui::Sense::hover());
            let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)));
            if matches!(l.level.as_str(), "error" | "warn") {
                row.painter().rect_filled(rect, radius::CONTROL, mix(t.surface, c, 0.07));
            }
            row.add_space(spacing::S);
            row.allocate_ui_with_layout(Vec2::new(64.0, LOG_ROW), Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_width(64.0);
                ui.label(RichText::new(word).font(font_medium(type_scale::SMALL)).color(c));
            });
            row.allocate_ui_with_layout(Vec2::new(120.0, LOG_ROW), Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_width(120.0);
                ui.add(egui::Label::new(RichText::new(&l.target).font(font_mono(type_scale::SMALL)).color(t.text_faint)).truncate());
            });
            row.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(tg) = &target
                    && widgets::button_ex(ui, &t, Some(icon::EDIT), "Open file", Kind::Ghost, Size::Small, 0.0, true).clicked()
                {
                    open = Some(tg.clone());
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(&l.msg).color(if c == t.text_dim || c == t.text_faint { t.fg } else { c })).truncate());
                });
            });
        }
    });
    if let Some((p, l)) = open {
        app.open_in_editor(&p, l);
    }
}

// ---- scopes --------------------------------------------------------------------------------------

pub fn scopes(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Show").color(t.text_dim));
        ui.add(se_ui_kit::widgets::field(&mut app.build.tools.scope_pattern).hint_text("band.* midi.xtouch.*").desired_width(360.0))
            .on_hover_text("Signal names, separated by spaces; * matches anything");
        ui.add_space(spacing::M);
        let (r, _) = ui.allocate_exact_size(Vec2::new(18.0, 4.0), egui::Sense::hover());
        ui.painter().rect_filled(r, 2.0, t.modulated());
        ui.label(RichText::new("what a link makes of it").size(type_scale::SMALL).color(t.text_dim));
    });
    ui.add_space(spacing::S);
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
        widgets::empty_state(ui, &t, icon::SCOPE, "No signals match", "Try band.* for the drums or music.* for the music.", None);
        return;
    }
    let mut edit: Option<BindingDef> = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let gap = spacing::M;
        let (per, w) = tile_grid(ui, names.len().min(32), 260.0, 420.0, gap);
        let shown: Vec<&String> = names.iter().take(32).collect();
        for row in shown.chunks(per) {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for n in row {
                    let v: Vec<f32> = app.m.signals[*n].iter().copied().collect();
                    let bound: Vec<&BindingDef> = defs.iter().filter(|d| d.signal == **n && d.enabled).collect();
                    egui::Frame::new().fill(t.surface_hi).corner_radius(radius::CONTROL).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                        ui.vertical(|ui| {
                            ui.set_width(w - 20.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(n.as_str()).font(font_mono(type_scale::SMALL)).color(t.fg));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(
                                        RichText::new(format!("{:.3}", v.last().copied().unwrap_or(0.0))).font(font_mono(type_scale::SMALL)).color(t.text_dim),
                                    );
                                });
                            });
                            let size = Vec2::new(w - 20.0, 64.0);
                            match bound.first() {
                                Some(def) => {
                                    let shaped: Vec<f32> = match BindingRt::new((*def).clone()) {
                                        Ok(mut rt) => v.iter().map(|x| rt.shape(*x as f64, 1.0 / 30.0) as f32).collect(),
                                        Err(_) => Vec::new(),
                                    };
                                    let r = def.range.map(|[a, b]| [a.min(b) as f32, a.max(b) as f32]).unwrap_or([0.0, 1.0]);
                                    se_ui_kit::curve::dual_scope(ui, &t, size, &v, &shaped, r);
                                    let more = if bound.len() > 1 { format!(" +{}", bound.len() - 1) } else { String::new() };
                                    let lbl = ui.add(
                                        egui::Label::new(
                                            RichText::new(format!("{}  {}{more}", icon::LINK, def.target)).size(type_scale::SMALL).color(t.modulated()),
                                        )
                                        .truncate()
                                        .sense(egui::Sense::click()),
                                    );
                                    if lbl.on_hover_text("Edit this link").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                        edit = Some((*def).clone());
                                    }
                                }
                                None => {
                                    widgets::scope(ui, &t, size, &v, None, None);
                                }
                            }
                        });
                    });
                }
            });
            ui.add_space(gap);
        }
        if names.len() > 32 {
            widgets::hint(ui, &t, &format!("Showing 32 of {} signals. Narrow the list to see others.", names.len()));
        }
    });
    if let Some(def) = edit {
        let mut d = Draft::new(&def.target, &se_proto::Meta::default());
        d.name = def.name.clone();
        app.build.modulate.draft = Some(d);
        let row =
            Value::map().with("name", def.name.clone()).with("target", def.target.clone()).with("signal", def.signal.clone()).with("file", def.file.clone());
        app.build.modulate.edit_existing(&row);
        app.focus_panel(crate::panels::Panel::Modulate);
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
