//! Command palette (§15.1, Ctrl+K): every command, scene, preset, mode, simulator preset,
//! layout, view, panel, and state address — fuzzy-matched, with shortcuts shown. Free text
//! runs as a one-line command (`set fx.vhs.amount 0.5`).

use crate::app::{App, ViewId};
use crate::panels::Panel;
use egui::{Key, RichText};
use se_proto::{Op, Value};
use se_ui_kit::widgets::icon;

#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    Text(String),
    Op(Op),
    Select(String),
    View(ViewId),
    Layout(String),
    PopOut(String),
    Focus(Panel),
    ToggleMode,
    Take,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub label: String,
    pub detail: String,
    pub shortcut: String,
    pub act: Act,
}

#[derive(Default)]
pub struct Palette {
    pub open: bool,
    pub text: String,
    pub cursor: usize,
}

/// Subsequence match score (higher is better); `None` when `q` isn't a subsequence of `s`.
pub fn score(q: &str, s: &str) -> Option<i32> {
    if q.is_empty() {
        return Some(0);
    }
    let s = s.to_lowercase();
    if let Some(pos) = s.find(q) {
        return Some(1000 - pos as i32 - s.len() as i32 / 8 + if pos == 0 { 200 } else { 0 });
    }
    let mut it = s.chars().enumerate();
    let mut last = 0usize;
    let mut gaps = 0i32;
    for qc in q.chars() {
        let (i, _) = it.by_ref().find(|(_, c)| *c == qc)?;
        gaps += (i - last) as i32;
        last = i;
    }
    Some(500 - gaps - s.len() as i32 / 8)
}

fn items(app: &App) -> Vec<Item> {
    let mut v = Vec::new();
    let mut add = |label: String, detail: &str, shortcut: String, act: Act| v.push(Item { label, detail: detail.to_string(), shortcut, act });
    add(format!("{} take", icon::PLAY), "preview → program", app.keys_label("take"), Act::Take);
    add(format!("{} switch Live ↔ Scenes", icon::SETTINGS), "page", app.keys_label("mode.toggle"), Act::ToggleMode);
    add(format!("{} clean", icon::CROSS), "clear chat effects", app.keys_label("clean"), Act::Op(Op::Clean));
    add(format!("{} undo", icon::PLAY), "", app.keys_label("undo"), Act::Op(Op::Undo));
    add(format!("{} redo", icon::PLAY), "", app.keys_label("redo"), Act::Op(Op::Redo));
    add(format!("{} panic", icon::WARN), "all automation off, safe lights + mix", app.keys_label("panic"), Act::Op(Op::Panic));
    for (l, c) in [
        ("reload project", "project.reload"),
        ("undo last change to the project", "project.undo"),
        ("redo the project change you undid", "project.redo"),
        ("add session marker", "session.marker"),
        ("start stream (OBS)", "obs.stream.start"),
        ("stop stream (OBS)", "obs.stream.stop"),
        ("start recording", "recording.start"),
        ("stop recording", "recording.stop"),
        ("Twitch stream marker", "twitch.marker"),
        ("lights panic (safe look)", "lights.panic"),
        ("audio panic", "audio.panic"),
        ("mixer panic (safe mix)", "mixer.panic"),
        ("skip song", "queue.skip"),
    ] {
        add(format!("{} {l}", icon::PLAY), c, String::new(), Act::Text(c.into()));
    }
    let keyed = |i: usize, prefix: &str| if i < 9 { app.keys_label(&format!("{prefix}.{}", i + 1)) } else { String::new() };
    for (i, s) in app.m.q_list("scenes").iter().enumerate() {
        if let Some(n) = s.get_path("name").and_then(Value::as_str) {
            add(format!("{} scene {n} → preview", icon::SCENE), "scene.go", keyed(i, "scene.preview"), Act::Op(Op::SceneGo { scene: n.into() }));
            add(
                format!("{} scene {n} → program (cut)", icon::SCENE),
                "scene.cut",
                keyed(i, "scene.program"),
                Act::Op(Op::SceneCut { scene: n.into(), transition: None }),
            );
        }
    }
    for p in app.m.q_list("presets") {
        if let Some(n) = p.get_path("name").and_then(Value::as_str) {
            add(format!("{} preset {n}", icon::PRESET), "preset.fire", String::new(), Act::Op(Op::PresetFire { name: n.into(), payload: Value::Null }));
            add(format!("{} release preset {n}", icon::STOP), "preset.release", String::new(), Act::Op(Op::PresetRelease { name: n.into() }));
        }
    }
    for m in app.m.q_list("modes") {
        if let Some(n) = m.as_str() {
            add(format!("{} mode {n}", icon::LIVE), "mode.set", String::new(), Act::Op(Op::ModeSet { mode: n.into() }));
        }
    }
    for s in app.m.q_list("sim.presets") {
        if let Some(n) = s.get_path("name").and_then(Value::as_str) {
            let args = s.get_path("args").and_then(Value::as_str).unwrap_or("");
            add(format!("{} simulate {n}", icon::SIM), args, String::new(), Act::Text(format!("sim.{n} {args}").trim().into()));
        }
    }
    for (label, cmd) in crate::views::tools::SIM_SHORTCUTS {
        add(format!("{} simulate {label}", icon::SIM), cmd, String::new(), Act::Text(cmd.to_string()));
    }
    for r in app.m.q_list("rules") {
        if let Some(n) = r.get_path("name").and_then(Value::as_str) {
            let en = r.get_path("enabled").is_some_and(Value::truthy);
            add(
                format!("{} {} rule {n}", icon::RULE, if en { "disable" } else { "enable" }),
                "rule",
                String::new(),
                Act::Text(format!("{} '{n}'", if en { "rule.disable" } else { "rule.enable" })),
            );
        }
    }
    for l in app.layout_names() {
        add(format!("{} layout {l}", icon::SETTINGS), "switch layout", app.keys_label("layout.next"), Act::Layout(l));
    }
    for (vid, ic, label) in ViewId::ALL {
        add(format!("{ic} open {label}"), "view", String::new(), Act::View(*vid));
    }
    for p in Panel::all() {
        add(format!("{} pop out {}", icon::LINK, p.title()), &format!("stream-engine.{}", p.id()), String::new(), Act::PopOut(p.id()));
        add(format!("{} show {}", icon::SEARCH, p.title()), "go to", String::new(), Act::Focus(p));
    }
    for a in app.m.state.keys().chain(app.m.fetched.keys()) {
        add(format!("{} {a}", icon::SEARCH), "inspect address", String::new(), Act::Select(a.clone()));
    }
    v
}

/// Best matches for `q`, highest score first.
pub fn matches(all: Vec<Item>, q: &str, n: usize) -> Vec<Item> {
    let q = q.trim().to_lowercase();
    let mut scored: Vec<(i32, Item)> = all.into_iter().filter_map(|it| score(&q, &format!("{} {}", it.label, it.detail)).map(|s| (s, it))).collect();
    scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
    scored.into_iter().take(n).map(|(_, i)| i).collect()
}

pub fn run(app: &mut App, act: Act) {
    match act {
        Act::Text(t) => app.m.text(&t),
        Act::Op(op) => app.m.command(op),
        Act::Select(a) => {
            app.select(a);
            app.open_view(crate::app::ViewId::Composition);
        }
        Act::View(v) => app.open_view(v),
        Act::Layout(l) => app.switch_layout(&l),
        Act::PopOut(p) => app.pop_out(&p),
        Act::Focus(p) => app.open_panel(p),
        Act::ToggleMode => app.toggle_page(),
        Act::Take => app.take(),
    }
}

pub fn ui(app: &mut App, ctx: &egui::Context) {
    if !app.palette.open {
        return;
    }
    let t = app.t.clone();
    let (up, down, enter, esc) = ctx.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
            i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
            i.consume_key(egui::Modifiers::NONE, Key::Enter),
            i.consume_key(egui::Modifiers::NONE, Key::Escape),
        )
    });
    let found = matches(items(app), &app.palette.text, 14);
    if up {
        app.palette.cursor = app.palette.cursor.saturating_sub(1);
    }
    if down {
        app.palette.cursor = (app.palette.cursor + 1).min(found.len().saturating_sub(1));
    }
    let mut chosen: Option<Act> = None;
    egui::Window::new("palette").title_bar(false).anchor(egui::Align2::CENTER_TOP, [0.0, 80.0]).fixed_size([720.0, 420.0]).show(ctx, |ui| {
        ui.label(RichText::new(format!("{} commands, scenes, presets, layouts, views, addresses — or type a command", icon::SEARCH)).color(t.fg_dim));
        let r = ui.add(
            se_ui_kit::widgets::field(&mut app.palette.text)
                .desired_width(f32::INFINITY)
                .hint_text("take · preset.fire hype · set fx.vhs.amount 0.5 · layout build"),
        );
        r.request_focus();
        if r.changed() {
            app.palette.cursor = 0;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, it) in found.iter().enumerate() {
                let sel = i == app.palette.cursor;
                let resp = ui
                    .horizontal(|ui| {
                        let l = ui.selectable_label(sel, RichText::new(&it.label).color(if sel { t.accent } else { t.fg }));
                        ui.label(RichText::new(&it.detail).small().color(t.fg_dim));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if !it.shortcut.is_empty() {
                                ui.label(RichText::new(&it.shortcut).small().monospace().color(t.fg_dim));
                            }
                        });
                        l
                    })
                    .inner;
                if resp.clicked() {
                    chosen = Some(it.act.clone());
                }
            }
            if found.is_empty() && !app.palette.text.trim().is_empty() {
                ui.label(RichText::new(format!("⏎ run `{}`", app.palette.text.trim())).color(t.fg_dim));
            }
        });
    });
    if enter {
        chosen = found.get(app.palette.cursor).map(|i| i.act.clone()).or_else(|| {
            let txt = app.palette.text.trim();
            (!txt.is_empty()).then(|| Act::Text(txt.to_string()))
        });
    }
    if let Some(a) = chosen {
        app.palette.open = false;
        app.palette.text.clear();
        run(app, a);
    }
    if esc {
        app.palette.open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn it(label: &str) -> Item {
        Item { label: label.into(), detail: String::new(), shortcut: String::new(), act: Act::Text(label.into()) }
    }

    #[test]
    fn substring_beats_subsequence_and_prefix_beats_infix() {
        assert!(score("take", "take") > score("take", "retake"));
        assert!(score("tk", "take").is_some());
        assert!(score("xyz", "take").is_none());
        let got = matches(vec![it("fx.vhs.amount"), it("preset hype"), it("scene hype-cam")], "hype", 5);
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|i| i.label != "fx.vhs.amount"));
        let got = matches(vec![it("hype preset"), it("big hype")], "hype", 5);
        assert_eq!(got[0].label, "hype preset", "prefix matches rank first");
    }
}
