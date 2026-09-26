//! Rules editor (§15.5): when / if / do blocks with autocomplete (event types, payload fields,
//! addresses, presets, scenes, commands), live validation with the engine's own parsers, a
//! "test" button that fires the event through the simulator, and saving through
//! `project.write` into `rules/*.toml` (comments in the file are kept).

use crate::app::App;
use egui::RichText;
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};

/// Simulator preset for an event type (se-core `sim.rs`), with default args.
pub const SIM_FOR: &[(&str, &str)] = &[
    ("twitch.cheer", "cheer"),
    ("twitch.sub", "sub"),
    ("twitch.resub", "resub"),
    ("twitch.gift", "gift_bomb"),
    ("twitch.follow", "follow"),
    ("twitch.raid", "raid"),
    ("twitch.redeem", "redeem"),
    ("twitch.chat", "chat"),
    ("tip", "tip"),
    ("twitch.ad_break", "ad_break"),
    ("twitch.hype_train.begin", "hype_train"),
];

/// Payload fields per event type (the normalized Twitch/tip contract, se-core `sim.rs`).
pub const FIELDS: &[(&str, &[&str])] = &[
    ("twitch.cheer", &["bits", "message", "user"]),
    ("twitch.sub", &["tier", "months", "is_gift", "user"]),
    ("twitch.resub", &["tier", "months", "message", "user"]),
    ("twitch.gift", &["count", "tier", "user"]),
    ("twitch.follow", &["user"]),
    ("twitch.raid", &["viewers", "from", "user"]),
    ("twitch.redeem", &["reward", "cost", "input", "user"]),
    ("twitch.chat", &["message", "message_id", "user"]),
    ("tip", &["amount", "currency", "message", "user"]),
    ("twitch.ad_break", &["duration", "automatic"]),
    ("twitch.hype_train.begin", &["level"]),
];

const VERBS: &[&str] = &[
    "preset.fire",
    "preset.release",
    "scene.go",
    "scene.cut",
    "scene.take",
    "set",
    "animate",
    "trigger",
    "release",
    "mode.set",
    "emit",
    "wait",
    "bot.say",
    "lights.cue",
    "lights.release",
    "audio.play",
    "audio.duck",
    "mixer.snapshot.recall",
    "obs.stream.start",
    "obs.stream.stop",
    "twitch.marker",
    "queue.skip",
    "timeline.play",
    "timeline.stop",
    "clean",
    "panic",
];

#[derive(Clone, Debug, PartialEq)]
pub struct RuleDraft {
    /// Name of the rule in its file (None = new rule).
    pub orig: Option<String>,
    pub file: String,
    pub name: String,
    pub when: String,
    pub cond: String,
    pub commands: Vec<String>,
    pub cooldown_global: String,
    pub cooldown_actor: String,
    pub modes: String,
    pub scenes: String,
    pub enabled: bool,
    pub test_args: String,
    filled: bool,
}

impl RuleDraft {
    fn new() -> RuleDraft {
        RuleDraft {
            orig: None,
            file: "rules/ui.toml".into(),
            name: "new rule".into(),
            when: "twitch.cheer".into(),
            cond: String::new(),
            commands: vec!["preset.fire hype".into()],
            cooldown_global: String::new(),
            cooldown_actor: String::new(),
            modes: String::new(),
            scenes: String::new(),
            enabled: true,
            test_args: String::new(),
            filled: true,
        }
    }

    /// Validation errors (empty = saveable), using the engine's parsers.
    pub fn errors(&self) -> Vec<String> {
        let mut e = Vec::new();
        if self.name.trim().is_empty() {
            e.push("name is empty".into());
        }
        if self.when.trim().is_empty() {
            e.push("`when` needs an event pattern".into());
        } else if !se_proto::address::is_valid(self.when.trim(), true) {
            e.push(format!("`when`: `{}` is not a valid event pattern", self.when.trim()));
        }
        if !self.cond.trim().is_empty()
            && let Err(err) = se_expr::Expr::parse(self.cond.trim())
        {
            e.push(format!("if: {err}"));
        }
        for (i, c) in self.commands.iter().enumerate() {
            if c.trim().is_empty() {
                continue;
            }
            if let Err(err) = Op::parse(c) {
                e.push(format!("do #{}: {err}", i + 1));
            }
        }
        for (label, d) in [("cooldown", &self.cooldown_global), ("per-user cooldown", &self.cooldown_actor)] {
            if !d.trim().is_empty() && se_proto::parse_duration_ms(d).is_none() {
                e.push(format!("{label}: `{d}` is not a duration (8s, 5m, 250ms)"));
            }
        }
        if !self.file.starts_with("rules/") || !self.file.ends_with(".toml") {
            e.push("file must be rules/<name>.toml".into());
        }
        e
    }

    fn list(s: &str) -> Value {
        Value::List(s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(|x| Value::Str(x.into())).collect())
    }

    /// Keys written for this rule (null removes options that were cleared).
    pub fn set_value(&self) -> Value {
        let cd = {
            let mut m = Value::map();
            if !self.cooldown_global.trim().is_empty() {
                m = m.with("global", self.cooldown_global.trim());
            }
            if !self.cooldown_actor.trim().is_empty() {
                m = m.with("per_actor", self.cooldown_actor.trim());
            }
            if m.as_map().is_some_and(|m| m.is_empty()) { Value::Null } else { m }
        };
        let modes = Self::list(&self.modes);
        let scenes = Self::list(&self.scenes);
        let empty = |v: &Value| v.as_list().is_some_and(|l| l.is_empty());
        Value::map()
            .with("name", self.name.trim())
            .with("when", self.when.trim())
            .with("if", if self.cond.trim().is_empty() { Value::Null } else { Value::Str(self.cond.trim().into()) })
            .with("do", Value::List(self.commands.iter().map(|c| c.trim()).filter(|c| !c.is_empty()).map(|c| Value::Str(c.into())).collect()))
            .with("cooldown", cd)
            .with("modes", if empty(&modes) { Value::Null } else { modes })
            .with("scenes", if empty(&scenes) { Value::Null } else { scenes })
            .with("enabled", if self.enabled { Value::Null } else { Value::Bool(false) })
    }

    /// `project.write` args. `file_text` decides between `[[rule]]` entries and a single-rule file.
    pub fn write_args(&self, file_text: Option<&str>) -> Value {
        let set = self.set_value();
        let parsed = file_text.and_then(|t| t.parse::<toml::Table>().ok());
        let multi = parsed.as_ref().is_some_and(|t| t.get("rule").is_some_and(|r| r.is_array()));
        match &self.orig {
            None => Value::map().with("path", self.file.clone()).with("table", "rule").with("append", true).with("set", set),
            Some(orig) if multi => {
                let explicit = parsed
                    .as_ref()
                    .and_then(|t| t.get("rule"))
                    .and_then(|r| r.as_array())
                    .is_some_and(|a| a.iter().any(|e| e.get("name").and_then(|n| n.as_str()) == Some(orig.as_str())));
                let args = Value::map().with("path", self.file.clone()).with("table", "rule").with("set", set);
                match (explicit, orig.rsplit_once('#').and_then(|(_, n)| n.parse::<i64>().ok())) {
                    (false, Some(n)) => args.with("index", n - 1),
                    _ => args.with("match", Value::map().with("name", orig.clone())),
                }
            }
            Some(_) => Value::map().with("path", self.file.clone()).with("set", set),
        }
    }

    pub fn delete_args(&self, file_text: Option<&str>) -> Option<Value> {
        let orig = self.orig.as_ref()?;
        let multi = file_text.and_then(|t| t.parse::<toml::Table>().ok()).is_some_and(|t| t.get("rule").is_some_and(|r| r.is_array()));
        Some(if multi {
            Value::map().with("path", self.file.clone()).with("table", "rule").with("match", Value::map().with("name", orig.clone())).with("delete", true)
        } else {
            Value::map().with("path", self.file.clone()).with("delete", true)
        })
    }

    /// The simulator command that fires this rule's event.
    pub fn test_command(&self) -> String {
        let when = self.when.trim();
        let args = self.test_args.trim();
        match SIM_FOR.iter().find(|(ty, _)| se_proto::address::matches(when, ty)) {
            Some((_, sim)) => format!("sim.{sim} {args}").trim().to_string(),
            None => {
                let ty = when.replace("**", "test").replace('*', "test");
                format!("emit {ty} {args}").trim().to_string()
            }
        }
    }
}

#[derive(Default)]
pub struct RulesEditor {
    pub edit: Option<RuleDraft>,
    /// Which text field autocompletes (`when`, `if`, `do:<i>`).
    focus: Option<String>,
}

impl RulesEditor {
    pub fn new_rule(&mut self) {
        self.edit = Some(RuleDraft::new());
    }
    pub fn editing_name(&self) -> Option<&str> {
        self.edit.as_ref().and_then(|d| d.orig.as_deref())
    }
    /// Open a row of the `rules` query.
    pub fn open(&mut self, r: &Value) {
        let s = |k: &str| r.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
        let name = s("name");
        self.edit = Some(RuleDraft {
            orig: Some(name.clone()),
            file: s("file"),
            name,
            when: s("when"),
            cond: s("if"),
            commands: r.get_path("do").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            cooldown_global: String::new(),
            cooldown_actor: String::new(),
            modes: String::new(),
            scenes: String::new(),
            enabled: r.get_path("enabled").is_none_or(Value::truthy),
            test_args: String::new(),
            filled: false,
        });
    }
}

/// Suggestions for the token under the cursor (the last whitespace-separated word).
pub fn suggest(field: &str, text: &str, app: &App) -> Vec<String> {
    let last = text.rsplit(|c: char| c.is_whitespace() || c == '(' || c == '!').next().unwrap_or("");
    let mut pool: Vec<String> = Vec::new();
    match field {
        "when" => {
            pool.extend(SIM_FOR.iter().map(|(t, _)| t.to_string()));
            pool.extend(app.m.events.iter().map(|e| e.ty.clone()));
            pool.extend(
                ["mode.enter.*", "mode.exit.*", "band.kick", "band.snare", "band.drop", "beat", "deck.key", "queue.song_started", "timeline.cue", "twitch.*"]
                    .map(String::from),
            );
        }
        "if" => {
            let when = app.build.rules.edit.as_ref().map(|d| d.when.clone()).unwrap_or_default();
            for (ty, fields) in FIELDS {
                if se_proto::address::matches(&when, ty) {
                    pool.extend(fields.iter().map(|f| format!("event.{f}")));
                }
            }
            for e in app.m.events.iter().rev().take(200).filter(|e| se_proto::address::matches(&when, &e.ty)) {
                if let Value::Map(m) = &e.payload {
                    pool.extend(m.keys().map(|k| format!("event.{k}")));
                }
            }
            pool.extend(["mode", "scene", "event.user", "&&", "||", "in"].map(String::from));
            pool.extend(app.m.state.keys().take(4000).cloned());
        }
        _ => {
            let words = text.split_whitespace().count() + usize::from(text.ends_with(' '));
            if words <= 1 {
                pool.extend(VERBS.iter().map(|v| v.to_string()));
                pool.extend(app.m.q_list("presets").iter().filter_map(|p| p.get_path("name").and_then(Value::as_str)).map(|n| format!("preset.fire {n}")));
            } else {
                let verb = text.split_whitespace().next().unwrap_or("");
                match verb {
                    "preset.fire" | "preset.release" => {
                        pool.extend(app.m.q_list("presets").iter().filter_map(|p| p.get_path("name").and_then(Value::as_str).map(String::from)))
                    }
                    "scene.go" | "scene.cut" => {
                        pool.extend(app.m.q_list("scenes").iter().filter_map(|p| p.get_path("name").and_then(Value::as_str).map(String::from)))
                    }
                    "mode.set" => pool.extend(app.m.q_list("modes").iter().filter_map(|v| v.as_str().map(String::from))),
                    "scene.take" => pool.extend(app.m.q_list("transitions").iter().filter_map(|v| v.as_str().map(String::from))),
                    _ => pool.extend(app.m.state.keys().take(4000).cloned()),
                }
            }
        }
    }
    pool.sort();
    pool.dedup();
    let l = last.to_lowercase();
    pool.into_iter().filter(|p| !l.is_empty() && p.to_lowercase().starts_with(&l) && *p != last).take(8).collect()
}

fn complete(text: &mut String, choice: &str) {
    let cut = text.rfind(|c: char| c.is_whitespace() || c == '(' || c == '!').map(|i| i + 1).unwrap_or(0);
    text.truncate(cut);
    if choice.contains(' ') && cut > 0 {
        // a full command suggestion replaces the whole line
        text.clear();
    }
    text.push_str(choice);
}

fn field(app: &mut App, ui: &mut egui::Ui, key: &str, hint: &str, get: impl Fn(&mut RuleDraft) -> &mut String) {
    let focused = app.build.rules.focus.as_deref() == Some(key);
    let sugg = if focused {
        app.build.rules.edit.as_mut().map(|d| get(d).clone()).map(|txt| suggest(key.split(':').next().unwrap_or(key), &txt, app)).unwrap_or_default()
    } else {
        Vec::new()
    };
    let Some(d) = app.build.rules.edit.as_mut() else { return };
    let r = ui.add(egui::TextEdit::singleline(get(d)).hint_text(hint).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace));
    if r.has_focus() {
        app.build.rules.focus = Some(key.to_string());
    }
    if !sugg.is_empty() && (r.has_focus() || focused) {
        ui.horizontal_wrapped(|ui| {
            for s in sugg {
                if ui.small_button(RichText::new(&s).monospace()).clicked()
                    && let Some(d) = app.build.rules.edit.as_mut()
                {
                    complete(get(d), &s);
                    r.request_focus();
                }
            }
        });
    }
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.horizontal(|ui| {
        widgets::section(ui, &t, icon::RULE, "Rules");
        if ui.small_button(format!("{} new rule", icon::RULE)).clicked() {
            app.build.rules.new_rule();
        }
    });
    let rules = app.m.q_list("rules").to_vec();
    ui.columns(2, |cols| {
        egui::ScrollArea::vertical().id_salt("rules-list").auto_shrink([false, false]).show(&mut cols[0], |ui| {
            if rules.is_empty() {
                ui.label(RichText::new("no rules yet").color(t.fg_dim));
            }
            for r in &rules {
                let name = r.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
                let enabled = r.get_path("enabled").is_some_and(Value::truthy);
                let fired = r.get_path("last_fired_ms_ago").and_then(Value::as_i64);
                ui.horizontal(|ui| {
                    let mut en = enabled;
                    if ui.checkbox(&mut en, "").on_hover_text("enable / disable (runtime)").changed() {
                        app.m.text(&format!("{} '{name}'", if en { "rule.enable" } else { "rule.disable" }));
                    }
                    let recently = fired.is_some_and(|ms| ms < 2000);
                    widgets::led(
                        ui,
                        &t,
                        if recently {
                            LedState::Active
                        } else if enabled {
                            LedState::Healthy
                        } else {
                            LedState::Idle
                        },
                    );
                    let sel = app.build.rules.editing_name() == Some(name.as_str());
                    if ui.selectable_label(sel, RichText::new(&name).strong()).clicked() {
                        app.build.rules.open(r);
                    }
                    ui.label(RichText::new(format!("when {}", r.get_path("when").and_then(Value::as_str).unwrap_or(""))).small().color(t.cyan));
                });
            }
        });
        editor(app, &mut cols[1]);
    });
}

fn editor(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let Some(d) = app.build.rules.edit.as_ref() else {
        ui.label(RichText::new("select a rule or create one").color(t.fg_dim));
        return;
    };
    // fill cooldown/modes/scenes from the full definitions and fetch the file for write mode
    if !d.filled {
        let name = d.orig.clone().unwrap_or_default();
        let file = d.file.clone();
        match app.m.q("config.rules").cloned() {
            None => app.m.query("config.rules", Value::Null),
            Some(cfg) => {
                if let Some(def) = cfg.as_list().and_then(|l| l.iter().find(|r| r.get_path("name").and_then(Value::as_str) == Some(name.as_str())))
                    && let Some(d) = app.build.rules.edit.as_mut()
                {
                    let dur = |v: Option<&Value>| {
                        v.map(|v| v.as_str().map(String::from).unwrap_or_else(|| format!("{v}ms"))).filter(|s| s != "nullms").unwrap_or_default()
                    };
                    d.cooldown_global = dur(def.get_path("cooldown.global").filter(|v| !v.is_null()));
                    d.cooldown_actor = dur(def.get_path("cooldown.per_actor").filter(|v| !v.is_null()));
                    let join =
                        |k: &str| def.get_path(k).and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", ");
                    d.modes = join("modes");
                    d.scenes = join("scenes");
                    d.filled = true;
                }
                app.m.query_as(&format!("project.read:{file}"), "project.read", Value::map().with("path", file));
            }
        }
    }
    egui::ScrollArea::vertical().id_salt("rule-editor").auto_shrink([false, false]).show(ui, |ui| {
        if let Some(d) = app.build.rules.edit.as_mut() {
            ui.horizontal(|ui| {
                ui.label("name");
                ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(200.0));
                ui.checkbox(&mut d.enabled, "enabled");
            });
            ui.horizontal(|ui| {
                ui.label("file");
                ui.add_enabled(d.orig.is_none(), egui::TextEdit::singleline(&mut d.file).desired_width(220.0));
            });
        }
        ui.label(RichText::new("WHEN (event pattern)").small().strong().color(t.cyan));
        field(app, ui, "when", "twitch.cheer · mode.enter.brb · band.drop", |d| &mut d.when);
        ui.label(RichText::new("IF (expression over event, state, mode)").small().strong().color(t.yellow));
        field(app, ui, "if", "event.bits >= 1000 && mode == 'live'", |d| &mut d.cond);
        ui.label(RichText::new("DO (commands, in order; `wait 2s` delays)").small().strong().color(t.green));
        let n = app.build.rules.edit.as_ref().map(|d| d.commands.len()).unwrap_or(0);
        let mut remove = None;
        for i in 0..n {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{}.", i + 1)).monospace().color(t.fg_dim));
                ui.vertical(|ui| field(app, ui, &format!("do:{i}"), "preset.fire hype · bot.say 'thanks {user}!'", move |d| &mut d.commands[i]));
                if ui.small_button(icon::CROSS).clicked() {
                    remove = Some(i);
                }
            });
        }
        if let (Some(i), Some(d)) = (remove, app.build.rules.edit.as_mut()) {
            d.commands.remove(i);
        }
        if let Some(d) = app.build.rules.edit.as_mut() {
            if ui.small_button("+ command").clicked() {
                d.commands.push(String::new());
            }
            ui.horizontal(|ui| {
                ui.label("cooldown");
                ui.add(egui::TextEdit::singleline(&mut d.cooldown_global).hint_text("10s").desired_width(60.0));
                ui.label("per user");
                ui.add(egui::TextEdit::singleline(&mut d.cooldown_actor).hint_text("5m").desired_width(60.0));
            });
            ui.horizontal(|ui| {
                ui.label("modes");
                ui.add(egui::TextEdit::singleline(&mut d.modes).hint_text("live, chill (empty = all)").desired_width(160.0));
                ui.label("scenes");
                ui.add(egui::TextEdit::singleline(&mut d.scenes).hint_text("duo, wide").desired_width(120.0));
            });
        }
        let Some(d) = app.build.rules.edit.clone() else { return };
        let errs = d.errors();
        for e in &errs {
            ui.label(RichText::new(format!("{} {e}", icon::WARN)).color(t.bright_red));
        }
        let file_text = app.m.q(&format!("project.read:{}", d.file)).and_then(|v| v.get_path("text")).and_then(Value::as_str).map(String::from);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(errs.is_empty(), egui::Button::new(RichText::new(format!("{} save", icon::CHECK)).strong()))
                .on_hover_text(format!("write {}", d.file))
                .clicked()
            {
                app.m.action("project.write", d.write_args(file_text.as_deref()));
                if let Some(e) = app.build.rules.edit.as_mut() {
                    e.orig = Some(e.name.trim().to_string());
                }
                app.m.toast(format!("saved rule `{}` → {}", d.name.trim(), d.file), false);
                app.m.refresh_soon();
            }
            if let Some(args) = d.delete_args(file_text.as_deref())
                && widgets::hold_button(ui, &t, "hold: delete", t.bright_red, 0.6)
            {
                app.m.action("project.write", args);
                app.build.rules.edit = None;
                app.m.refresh_soon();
            }
            if ui.button(format!("{} open file", icon::CONSOLE)).clicked() {
                app.open_in_editor(&d.file, None);
            }
        });
        ui.horizontal(|ui| {
            if let Some(e) = app.build.rules.edit.as_mut() {
                ui.add(egui::TextEdit::singleline(&mut e.test_args).hint_text("bits=1500 message='hi'").desired_width(200.0));
            }
            let cmd = d.test_command();
            if ui.button(format!("{} test", icon::SIM)).on_hover_text(format!("fires `{cmd}` (save first to test edits)")).clicked() {
                app.m.text(&cmd);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> RuleDraft {
        RuleDraft {
            orig: Some("big cheer".into()),
            file: "rules/cheers.toml".into(),
            name: "big cheer".into(),
            cond: "event.bits >= 1000".into(),
            ..RuleDraft::new()
        }
    }

    #[test]
    fn validation_uses_engine_parsers() {
        assert!(draft().errors().is_empty(), "{:?}", draft().errors());
        let mut d = draft();
        d.cond = "event.bits >=".into();
        d.commands = vec!["$(boom)".into()];
        d.cooldown_global = "soon".into();
        let e = d.errors();
        assert_eq!(e.len(), 3, "{e:?}");
        d = draft();
        d.file = "../x.toml".into();
        assert_eq!(d.errors().len(), 1);
    }

    #[test]
    fn write_args_pick_entry_mode_from_the_file() {
        let multi = "[[rule]]\nname = \"big cheer\"\nwhen = \"twitch.cheer\"\n";
        let a = draft().write_args(Some(multi));
        assert_eq!(a.get_path("table").and_then(Value::as_str), Some("rule"));
        assert_eq!(a.get_path("match.name").and_then(Value::as_str), Some("big cheer"));
        assert_eq!(a.get_path("set.if").and_then(Value::as_str), Some("event.bits >= 1000"));
        assert!(a.get_path("set.cooldown").is_some_and(Value::is_null), "cleared cooldown is removed");
        let mut auto = draft();
        auto.orig = Some("cheers#2".into());
        let a = auto.write_args(Some("[[rule]]\nwhen='a'\n[[rule]]\nwhen='b'\n"));
        assert_eq!(a.get_path("index").and_then(Value::as_i64), Some(1));
        let single = draft().write_args(Some("name = 'big cheer'\nwhen = 'twitch.cheer'\n"));
        assert!(single.get_path("table").is_none());
        let mut new = draft();
        new.orig = None;
        let a = new.write_args(None);
        assert!(a.get_path("append").is_some_and(Value::truthy));
    }

    #[test]
    fn test_command_uses_the_simulator_when_it_can() {
        let mut d = draft();
        d.test_args = "bits=1500".into();
        assert_eq!(d.test_command(), "sim.cheer bits=1500");
        d.when = "twitch.*".into();
        assert!(d.test_command().starts_with("sim."));
        d.when = "band.drop".into();
        d.test_args.clear();
        assert_eq!(d.test_command(), "emit band.drop");
        d.when = "mode.enter.*".into();
        assert_eq!(d.test_command(), "emit mode.enter.test");
    }

    #[test]
    fn completion_replaces_the_last_token() {
        let mut s = String::from("event.bits >= 10 && ev");
        complete(&mut s, "event.user");
        assert_eq!(s, "event.bits >= 10 && event.user");
        let mut c = String::from("pre");
        complete(&mut c, "preset.fire hype");
        assert_eq!(c, "preset.fire hype");
    }
}
