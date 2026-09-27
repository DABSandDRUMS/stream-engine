//! Sound → Read-out voice (§14.4). One page in three sections: **Voice** (the local
//! voice's state, the usual voice, speed, volume and a test box), **Read-out rules** (which
//! alerts read messages aloud, voice rules, length and queue limits) and **Queue** (what's being
//! read now, what's waiting, skip and clear). The on/off switch and Save sit in the header.
//! Data: query `tts` (polled every 0.5 s) and `tts.*` state, `alerts.config` (alerts that read
//! aloud), `project.toml [tts]` via `project.read`; commands: `tts.*` actions, `tts.enabled`,
//! and `project.write` (only the keys that changed).

use crate::app::{App, ViewId};
use crate::views::live::nice;
use egui::RichText;
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_mono, font_semibold, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};

const TEST_LINE: &str = "Thanks for the five hundred bits, let's go!";
/// Reply key of the `project.toml` read.
const PROJECT_KEY: &str = "tts.project";
/// Widest the page gets (it's a settings column, not a canvas).
const MAX_W: f32 = 960.0;

/// `project.toml [tts]` settings this page edits (engine defaults when absent).
#[derive(Clone, Debug, PartialEq)]
struct Settings {
    voice: String,
    speed: f64,
    volume: f64,
    max_chars: i64,
    max_queue: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { voice: "af_heart".into(), speed: 1.0, volume: 0.9, max_chars: 300, max_queue: 20 }
    }
}

/// One `[[tts.voices]]` rule: a condition, and the voice/speed it picks.
#[derive(Clone, Debug, PartialEq)]
struct VoiceRule {
    when: String,
    voice: String,
    speed: Option<f64>,
}

/// The `[tts]` table of `project.toml` text: the settings and the voice rules.
fn parse_project(text: &str) -> (Settings, Vec<VoiceRule>) {
    let d = Settings::default();
    let Ok(doc) = text.parse::<toml::Table>() else { return (d, Vec::new()) };
    let Some(tts) = doc.get("tts").and_then(toml::Value::as_table) else { return (d, Vec::new()) };
    let num = |k: &str| tts.get(k).and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)));
    let settings = Settings {
        voice: tts.get("voice").and_then(toml::Value::as_str).map_or(d.voice, String::from),
        speed: num("speed").unwrap_or(d.speed),
        volume: num("volume").unwrap_or(d.volume),
        max_chars: num("max_chars").map_or(d.max_chars, |v| v as i64),
        max_queue: num("max_queue").map_or(d.max_queue, |v| v as i64),
    };
    let rules = tts
        .get("voices")
        .and_then(toml::Value::as_array)
        .map(|l| {
            l.iter()
                .filter_map(toml::Value::as_table)
                .map(|r| VoiceRule {
                    when: r.get("when").and_then(toml::Value::as_str).unwrap_or("").to_string(),
                    voice: r.get("voice").and_then(toml::Value::as_str).unwrap_or("").to_string(),
                    speed: r.get("speed").and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64))),
                })
                .collect()
        })
        .unwrap_or_default();
    (settings, rules)
}

/// `project.write` keys for what changed between `saved` and `draft`.
fn changes(saved: &Settings, draft: &Settings) -> Value {
    let mut set = Value::map();
    if draft.voice != saved.voice {
        set = set.with("tts.voice", draft.voice.clone());
    }
    if (draft.speed - saved.speed).abs() > 1e-6 {
        set = set.with("tts.speed", (draft.speed * 100.0).round() / 100.0);
    }
    if (draft.volume - saved.volume).abs() > 1e-6 {
        set = set.with("tts.volume", (draft.volume * 100.0).round() / 100.0);
    }
    if draft.max_chars != saved.max_chars {
        set = set.with("tts.max_chars", draft.max_chars);
    }
    if draft.max_queue != saved.max_queue {
        set = set.with("tts.max_queue", draft.max_queue);
    }
    set
}

#[derive(Clone, Default)]
struct Form {
    text: String,
    voice: String,
    last_query: f64,
    last_slow: f64,
    tests: u64,
    /// The settings as saved in `project.toml` (`None` until read).
    saved: Option<Settings>,
    draft: Settings,
    rules: Vec<VoiceRule>,
}

fn act(app: &mut App, name: &str, args: Value) {
    app.m.command(Op::Action { name: name.into(), args });
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

/// Kokoro voice ids read as words: `af_heart` → "Heart · American English, female".
fn voice_label(id: &str) -> String {
    let Some((tag, name)) = id.split_once('_') else { return nice(id) };
    let mut c = tag.chars();
    let lang = match c.next() {
        Some('a') => "American English",
        Some('b') => "British English",
        Some('e') => "Spanish",
        Some('f') => "French",
        Some('h') => "Hindi",
        Some('i') => "Italian",
        Some('j') => "Japanese",
        Some('p') => "Brazilian Portuguese",
        Some('z') => "Chinese",
        _ => return nice(id),
    };
    let who = match c.next() {
        Some('f') => ", female",
        Some('m') => ", male",
        _ => "",
    };
    format!("{} · {lang}{who}", nice(name))
}

/// The voice's state in the shared status words: (badge, sentence, color, fixable by downloading).
fn model_words(t: &Theme, status: &str, ready: bool, health: &Value) -> (&'static str, &'static str, egui::Color32, bool) {
    let hs = s(health, "status");
    if ready {
        return ("Working", "", t.green, false);
    }
    if status.starts_with("fetching") {
        return ("Downloading…", "This takes a minute or two.", t.yellow, false);
    }
    if status == "loading" {
        return ("Getting ready…", "This only takes a moment.", t.yellow, false);
    }
    if status.starts_with("fetch failed") {
        return ("Needs a look", "The download didn't finish. Check your internet connection and try again.", t.bright_red, true);
    }
    if status == "disabled" {
        return ("Off", "Reading out loud is turned off in this project's settings.", t.text_dim, false);
    }
    if status.starts_with("error") {
        return ("Needs a look", "The voice couldn't load. Download it again to fix it.", t.bright_red, true);
    }
    if hs == "warn" && s(health, "detail").contains("not being consumed") {
        return ("Needs a look", "The voice can't be heard until the sound system is running.", t.yellow, false);
    }
    if s(health, "detail").contains("espeak") {
        return ("Needs a look", "A helper program is missing. Install the espeak-ng package, then restart Stream Engine.", t.bright_red, false);
    }
    ("Needs a look", "The voice isn't downloaded yet. It's about 90 MB and only needs downloading once.", t.bright_red, true)
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("tts-form");
    let mut form: Form = ui.data_mut(|d| d.get_temp::<Form>(id)).unwrap_or_default();
    let now = ui.input(|i| i.time);
    if now - form.last_query > 0.5 {
        form.last_query = now;
        app.m.query("tts", Value::Null);
    }
    if now - form.last_slow > 2.0 {
        form.last_slow = now;
        app.m.query_as(PROJECT_KEY, "project.read", Value::map().with("path", "project.toml"));
        app.m.query("alerts.config", Value::Null);
    }
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    if let Some(text) = app.m.q(PROJECT_KEY).map(|v| s(v, "text").to_string()) {
        let (saved, rules) = parse_project(&text);
        // an untouched draft follows the file; edits stay until saved or undone
        if form.saved.as_ref().is_none_or(|old| *old == form.draft) {
            form.draft = saved.clone();
        }
        form.saved = Some(saved);
        form.rules = rules;
    }

    let q = app.m.q("tts").cloned().unwrap_or_default();
    let ready = q.get_path("ready").is_some_and(Value::truthy);
    // the mirrored state is authoritative once subscribed; the query covers the first frames
    let enabled = app.m.get("tts.enabled").map(Value::truthy).unwrap_or_else(|| q.get_path("enabled").is_some_and(Value::truthy));
    let voices: Vec<String> =
        q.get_path("voices").and_then(Value::as_list).map(|l| l.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    let default_voice = s(&q, "voice").to_string();
    if form.voice.is_empty() || !voices.contains(&form.voice) {
        form.voice = Some(default_voice.as_str()).filter(|v| !v.is_empty()).or(voices.first().map(String::as_str)).unwrap_or("").to_string();
    }

    egui::ScrollArea::vertical().id_salt("tts").auto_shrink([false, false]).show(ui, |ui| {
        ui.set_max_width(MAX_W);
        let dirty = form.saved.as_ref().is_some_and(|sv| *sv != form.draft);
        let (mut save, mut undo) = (false, false);
        let mut on = enabled;
        let sub = if enabled { "Reads viewers' messages out on stream." } else { "Off: messages from viewers aren't read out." };
        widgets::detail_header(ui, &t, icon::MIC, "Read-out voice", sub, |ui| {
            if widgets::toggle(ui, &t, &mut on).on_hover_text(if on { "Turning it off also clears the waiting messages" } else { "Turn on" }).changed() {
                app.m.command(Op::Set { address: "tts.enabled".into(), value: Value::Bool(on) });
            }
            ui.add_space(spacing::S);
            save = widgets::button_ex(ui, &t, Some(icon::CHECK), "Save", Kind::Primary, Size::Medium, 0.0, dirty).clicked();
            if dirty {
                undo = widgets::button_ex(ui, &t, Some(icon::UNDO), "Undo", Kind::Ghost, Size::Medium, 0.0, true).clicked();
            }
        });
        if save && let Some(saved) = form.saved.clone() {
            app.m.action("project.write", Value::map().with("path", "project.toml").with("set", changes(&saved, &form.draft)));
            app.m.toast("Read-out voice saved.", false);
            // the file is re-read shortly; until then the draft is what's saved
            form.saved = Some(form.draft.clone());
            form.last_slow = 0.0;
        }
        if undo && let Some(saved) = form.saved.clone() {
            form.draft = saved;
        }
        let say_voice = voice_section(app, ui, &t, &mut form, &q, &voices, &default_voice, ready, enabled);
        rules_section(app, ui, &t, &mut form);
        let test = queue_section(app, ui, &t, &q, ready && enabled);
        let text = say_voice.or_else(|| test.then(|| TEST_LINE.to_string()));
        if let Some(text) = text {
            form.tests += 1;
            let mut args = Value::map().with("text", text.trim()).with("id", format!("ui-test-{}", form.tests)).with("kind", "test").with("user", "ui");
            if !form.voice.is_empty() {
                args = args.with("voice", form.voice.clone());
            }
            act(app, "tts.say", args);
        }
        ui.add_space(spacing::XL);
    });
    ui.data_mut(|d| d.insert_temp(id, form));
}

/// Voice: state, the usual voice, speed, volume, a test box. Returns text to say (Try it).
#[allow(clippy::too_many_arguments)]
fn voice_section(
    app: &mut App,
    ui: &mut egui::Ui,
    t: &Theme,
    form: &mut Form,
    q: &Value,
    voices: &[String],
    default_voice: &str,
    ready: bool,
    enabled: bool,
) -> Option<String> {
    let status = s(q, "model_status").to_string();
    let health = app.m.get("health.tts").cloned().unwrap_or_default();
    let (badge, line, color, fixable) = model_words(t, &status, ready, &health);
    let fetching = status.starts_with("fetching");
    let mut fetch = false;
    let mut say = None;
    widgets::inspector_section(
        ui,
        t,
        "tts-voice",
        "Voice",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Status", |ui| {
                widgets::badge(ui, t, badge, color);
                let count = format!("{} voices, all on this computer: nothing is sent online.", voices.len());
                widgets::hint(ui, t, if ready { count.as_str() } else { line });
            });
            if fixable {
                widgets::prop_row(ui, t, "", |ui| {
                    let again = status.starts_with("error") || status.starts_with("fetch failed");
                    let label = if again { "Download it again" } else { "Download the voice (about 90 MB)" };
                    fetch = widgets::button_ex(ui, t, Some(icon::DOWN), label, Kind::Primary, Size::Medium, 0.0, !fetching).clicked();
                });
            }
            widgets::prop_row(ui, t, "Usual voice", |ui| {
                let cur = form.draft.voice.clone();
                egui::ComboBox::from_id_salt("tts-usual-voice").width(ui.available_width().min(340.0)).selected_text(voice_label(&cur)).show_ui(ui, |ui| {
                    for v in voices {
                        ui.selectable_value(&mut form.draft.voice, v.clone(), voice_label(v));
                    }
                });
            });
            widgets::prop_row(ui, t, "Speed", |ui| {
                ui.spacing_mut().slider_width = ui.available_width().min(300.0);
                ui.add(egui::Slider::new(&mut form.draft.speed, 0.5..=2.0).step_by(0.05).custom_formatter(|v, _| format!("{v:.2}×")));
            });
            widgets::prop_row(ui, t, "Volume", |ui| {
                ui.spacing_mut().slider_width = ui.available_width().min(300.0);
                ui.add(egui::Slider::new(&mut form.draft.volume, 0.0..=2.0).step_by(0.05).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
            });
            ui.add_space(spacing::S);
            widgets::prop_row(ui, t, "Try it", |ui| {
                let shown = |v: &str| if v == default_voice { format!("{} (usual)", voice_label(v)) } else { voice_label(v) };
                egui::ComboBox::from_id_salt("tts-test-voice").width(ui.available_width().min(340.0)).selected_text(shown(&form.voice)).show_ui(ui, |ui| {
                    for v in voices {
                        ui.selectable_value(&mut form.voice, v.clone(), shown(v));
                    }
                });
            });
            widgets::prop_row(ui, t, "", |ui| {
                let can = ready && enabled && !form.text.trim().is_empty();
                ui.add(widgets::field(&mut form.text).hint_text("Type something to hear it").desired_width((ui.available_width() - 110.0).max(160.0)));
                if widgets::button_ex(ui, t, Some(icon::PLAY), "Say", Kind::Secondary, Size::Medium, 90.0, can).clicked() {
                    say = Some(form.text.clone());
                }
            });
            let why = if !enabled {
                "Turn the read-out voice on first."
            } else if !ready {
                "The voice isn't ready yet."
            } else {
                "Goes to the end of the queue."
            };
            widgets::prop_row(ui, t, "", |ui| widgets::hint(ui, t, why));
            ui.add_space(spacing::S);
            widgets::details(ui, t, "tts-model", "Details", |ui| {
                widgets::fact(ui, t, "Model", if status.is_empty() { "unknown" } else { &status });
                let detail = s(&health, "detail");
                if !detail.is_empty() {
                    widgets::fact(ui, t, "Status", detail);
                }
                widgets::hint(ui, t, "Kokoro-82M with espeak-ng. Downloads are checked against pinned sha256 sums.");
                if !fixable
                    && !fetching
                    && widgets::button_ex(ui, t, Some(icon::CHECK), "Check the voices", Kind::Secondary, Size::Small, 0.0, true)
                        .on_hover_text("Makes sure every voice is complete and downloads anything missing or damaged")
                        .clicked()
                {
                    fetch = true;
                }
            });
        },
    );
    if fetch {
        act(app, "tts.model.fetch", Value::Null);
    }
    say
}

/// Read-out rules: which alerts read messages aloud, which voice reads what, and the limits.
fn rules_section(app: &mut App, ui: &mut egui::Ui, t: &Theme, form: &mut Form) {
    // (alert name, what it reads) for every alert or variation with read-aloud on
    let mut reading: Vec<(String, String)> = Vec::new();
    for a in app.m.q("alerts.config").and_then(|c| c.get_path("alerts")).and_then(Value::as_list).unwrap_or(&[]) {
        let name = nice(s(a, "name"));
        let enabled = a.get_path("enabled").is_none_or(Value::truthy);
        let reads = |look: Option<&Value>| look.and_then(|l| l.get_path("tts")).map(Value::truthy);
        let text = |look: Option<&Value>| match look.map(|l| s(l, "tts_text")).unwrap_or("") {
            "" | "{message}" => "Reads the viewer's message".to_string(),
            other => format!("Reads “{other}”"),
        };
        let look = a.get_path("look");
        if enabled && reads(look) == Some(true) {
            reading.push((name.clone(), text(look)));
        }
        for v in a.get_path("variations").and_then(Value::as_list).unwrap_or(&[]) {
            let vl = v.get_path("look");
            // a variation that turns read-aloud on itself (one that inherits is covered above)
            if enabled && reads(vl) == Some(true) {
                reading.push((format!("{name} · {}", nice(s(v, "name"))), text(vl.filter(|l| !s(l, "tts_text").is_empty()).or(look))));
            }
        }
    }
    let mut open_alerts = false;
    widgets::inspector_section(
        ui,
        t,
        "tts-rules",
        "Read-out rules",
        true,
        |_| {},
        |ui| {
            widgets::group_label(ui, t, "Read aloud by alerts");
            if reading.is_empty() {
                widgets::hint(ui, t, "No alert reads messages aloud yet. Turn on Read aloud in an alert to have its message read.");
            }
            for (name, what) in &reading {
                if widgets::list_row(ui, t, icon::ALERT, name, what, "Change", false).on_hover_text("Open it in Alerts").clicked() {
                    open_alerts = true;
                }
            }
            if !form.rules.is_empty() {
                widgets::group_label(ui, t, "Voice rules");
                widgets::hint(ui, t, "The first rule that fits picks the voice.");
                for r in &form.rules {
                    let voice = if r.voice.is_empty() { "the usual voice".to_string() } else { voice_label(&r.voice) };
                    let speed = r.speed.map(|x| format!(" at {x:.2}×")).unwrap_or_default();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{voice}{speed}")).font(font_semibold(type_scale::BODY)).color(t.fg));
                        ui.label(RichText::new(format!("when {}", r.when)).font(font_mono(type_scale::SMALL)).color(t.text_dim));
                    });
                }
            }
            widgets::group_label(ui, t, "Limits");
            widgets::prop_row(ui, t, "Longest message", |ui| {
                ui.add(egui::DragValue::new(&mut form.draft.max_chars).range(1..=5000).suffix(" characters"));
                widgets::hint(ui, t, "Longer ones are cut at a word.");
            });
            widgets::prop_row(ui, t, "Most waiting", |ui| {
                ui.add(egui::DragValue::new(&mut form.draft.max_queue).range(1..=500).suffix(" messages"));
                widgets::hint(ui, t, "Beyond this, new ones are dropped.");
            });
            widgets::details(ui, t, "tts-rules-details", "Details", |ui| {
                widgets::fact(ui, t, "Saved in", "project.toml [tts]");
                widgets::hint(ui, t, "Voice rules are [[tts.voices]] entries with a condition (when) and a voice or speed.");
            });
        },
    );
    if open_alerts {
        app.open_view(ViewId::Alerts);
    }
}

fn who(item: &Value) -> String {
    match s(item, "user") {
        "" => "Someone".into(),
        "ui" => "Test".into(),
        u => u.to_string(),
    }
}

/// Queue: reading now (Skip), waiting (per-message skip, Clear all). Returns true when the empty
/// state's "Try a test message" was pressed.
fn queue_section(app: &mut App, ui: &mut egui::Ui, t: &Theme, q: &Value, can_say: bool) -> bool {
    let current = q.get_path("current").filter(|c| !c.is_null()).cloned();
    let queue: Vec<Value> = q.get_path("queue").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let (mut skip, mut clear, mut test) = (false, false, false);
    let mut drop: Option<String> = None;
    let title = if queue.is_empty() { "Queue".to_string() } else { format!("Queue ({} waiting)", queue.len()) };
    widgets::inspector_section(
        ui,
        t,
        "tts-queue",
        &title,
        true,
        |ui| {
            if !queue.is_empty() {
                clear = widgets::hold_button(ui, t, "Clear all", t.bright_red, 0.5);
            }
        },
        |ui| {
            widgets::group_label(ui, t, "Reading now");
            match &current {
                Some(c) => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(who(c)).font(font_semibold(type_scale::BODY)).color(t.fg));
                        ui.label(RichText::new(voice_label(s(c, "voice"))).size(type_scale::SMALL + 0.5).color(t.text_dim));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            skip = widgets::button_ex(ui, t, Some(icon::STOP), "Skip", Kind::Danger, Size::Small, 0.0, true)
                                .on_hover_text("Stop reading this message")
                                .clicked();
                        });
                    });
                    ui.add(egui::Label::new(RichText::new(s(c, "text")).size(type_scale::LARGE).color(t.fg)).wrap());
                }
                None => {
                    widgets::hint(ui, t, "Nothing is being read right now.");
                }
            }
            widgets::group_label(ui, t, "Waiting");
            if queue.is_empty() {
                widgets::hint(ui, t, "Nobody's waiting. When viewers send a message with bits or a tip, it lines up here.");
                if can_say {
                    test = widgets::button_ex(ui, t, Some(icon::PLAY), "Try a test message", Kind::Secondary, Size::Small, 0.0, true).clicked();
                }
            }
            for (n, item) in queue.iter().enumerate() {
                let text = s(item, "text");
                let sub = format!("{} · {}", voice_label(s(item, "voice")), text);
                ui.push_id(("tts-waiting", n), |ui| {
                    ui.horizontal(|ui| {
                        let row_w = (ui.available_width() - 80.0).max(120.0);
                        ui.allocate_ui(egui::vec2(row_w, 0.0), |ui| {
                            widgets::list_row(ui, t, icon::CHAT, &who(item), &sub, "", false).on_hover_text(text);
                        });
                        if widgets::button_ex(ui, t, Some(icon::CROSS), "Skip", Kind::Ghost, Size::Small, 0.0, true)
                            .on_hover_text("Don't read this message")
                            .clicked()
                        {
                            drop = Some(s(item, "id").to_string());
                        }
                    });
                });
            }
        },
    );
    if skip {
        act(app, "tts.skip", Value::Null);
    }
    if clear {
        act(app, "tts.clear", Value::Null);
    }
    if let Some(id) = drop {
        act(app, "tts.skip", Value::map().with("id", id));
    }
    test
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_settings_and_rules_parse_with_defaults() {
        assert_eq!(parse_project(""), (Settings::default(), Vec::new()));
        let (st, rules) = parse_project(
            "[tts]\nvoice = \"bf_emma\"\nspeed = 1\nmax_chars = 120\n\n[[tts.voices]]\nwhen = \"amount >= 1000\"\nvoice = \"am_michael\"\nspeed = 1.1\n",
        );
        assert_eq!(st, Settings { voice: "bf_emma".into(), speed: 1.0, max_chars: 120, ..Settings::default() });
        assert_eq!(rules, vec![VoiceRule { when: "amount >= 1000".into(), voice: "am_michael".into(), speed: Some(1.1) }]);
    }

    #[test]
    fn saving_writes_only_what_changed() {
        let saved = Settings::default();
        assert_eq!(changes(&saved, &saved), Value::map());
        let draft = Settings { voice: "am_michael".into(), max_queue: 5, ..saved.clone() };
        let set = changes(&saved, &draft);
        let Value::Map(m) = set else { panic!("a map") };
        assert_eq!(m.keys().map(String::as_str).collect::<Vec<_>>(), ["tts.max_queue", "tts.voice"]);
        assert_eq!(m["tts.max_queue"], Value::Int(5));
    }
}
