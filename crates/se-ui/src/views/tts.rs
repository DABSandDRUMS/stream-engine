//! Sound → Text to speech (§14.4): the "read messages out loud" switch, what is being read now
//! (Skip), the waiting line with per-message skip and Clear all, a test box with a voice picker,
//! and the voice download state in words (technical status under Details).
//! Data: query `tts` (polled every 0.5 s) and `tts.*` state; commands: `tts.*` actions.

use crate::app::App;
use crate::views::live::nice;
use egui::{Align, Layout, RichText, Ui};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_medium, font_semibold, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};

const TEST_LINE: &str = "Thanks for the five hundred bits, let's go!";

#[derive(Clone, Default)]
struct Form {
    text: String,
    voice: String,
    last_query: f64,
    tests: u64,
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

pub fn ui(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("tts-form");
    let mut form: Form = ui.data_mut(|d| d.get_temp::<Form>(id)).unwrap_or_default();
    let now = ui.input(|i| i.time);
    if now - form.last_query > 0.5 {
        form.last_query = now;
        app.m.query("tts", Value::Null);
    }
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));

    let q = app.m.q("tts").cloned().unwrap_or_default();
    let ready = q.get_path("ready").is_some_and(Value::truthy);
    // the mirrored state is authoritative once subscribed; the query covers the first frames
    let enabled = app.m.get("tts.enabled").map(Value::truthy).unwrap_or_else(|| q.get_path("enabled").is_some_and(Value::truthy));
    let current = q.get_path("current").filter(|c| !c.is_null()).cloned();
    let queue: Vec<Value> = q.get_path("queue").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let voices: Vec<String> =
        q.get_path("voices").and_then(Value::as_list).map(|l| l.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    let default_voice = s(&q, "voice").to_string();
    let status = s(&q, "model_status").to_string();
    let health = app.m.get("health.tts").cloned().unwrap_or_default();
    if form.voice.is_empty() || !voices.contains(&form.voice) {
        form.voice = Some(default_voice.as_str()).filter(|v| !v.is_empty()).or(voices.first().map(String::as_str)).unwrap_or("").to_string();
    }

    egui::ScrollArea::vertical().id_salt("tts").auto_shrink([false, false]).show(ui, |ui| {
        // on/off, titled like the other cards
        widgets::panel(ui, &t, |ui| {
            ui.set_width(ui.available_width());
            let mut on = enabled;
            let help = if enabled {
                "Bits, tips and other messages from viewers are read out on stream. Turning this off also clears the waiting messages."
            } else {
                "Messages from viewers aren't read out right now."
            };
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("Read messages out loud").font(font_semibold(type_scale::LARGE)).color(t.fg));
                    ui.label(RichText::new(help).size(type_scale::SMALL + 0.5).color(t.text_dim));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::toggle(ui, &t, &mut on).changed() {
                        app.m.command(Op::Set { address: "tts.enabled".into(), value: Value::Bool(on) });
                    }
                });
            });
        });
        ui.add_space(spacing::L);

        // wide screens: messages | try it | voices side by side; otherwise two columns
        let w = ui.available_width();
        let gap = spacing::L;
        let three = w >= 2200.0;
        let left = if three { (w - 2.0 * gap) * 0.5 } else { w - gap - (w * 0.40).clamp(360.0, 620.0) };
        let col = if three { (w - 2.0 * gap - left) / 2.0 } else { w - gap - left };
        let space = |ui: &mut Ui| ui.add_space(gap - ui.spacing().item_spacing.x);
        let mut say_test: Option<String> = None;
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(egui::vec2(left, 0.0), Layout::top_down(Align::Min), |ui| {
                speaking_card(app, ui, &t, current.as_ref());
                ui.add_space(gap);
                if queue_card(app, ui, &t, &queue, ready && enabled) {
                    say_test = Some(TEST_LINE.to_string());
                }
            });
            space(ui);
            ui.allocate_ui_with_layout(egui::vec2(col, 0.0), Layout::top_down(Align::Min), |ui| {
                if let Some(text) = try_card(ui, &t, &mut form, &voices, &default_voice, ready, enabled) {
                    say_test = Some(text);
                }
                if !three {
                    ui.add_space(gap);
                    voice_card(app, ui, &t, &status, ready, &health, voices.len());
                }
            });
            if three {
                space(ui);
                ui.allocate_ui_with_layout(egui::vec2(col, 0.0), Layout::top_down(Align::Min), |ui| {
                    voice_card(app, ui, &t, &status, ready, &health, voices.len())
                });
            }
        });
        if let Some(text) = say_test {
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

fn who(item: &Value) -> String {
    match s(item, "user") {
        "" => "Someone".into(),
        "ui" => "Test".into(),
        u => u.to_string(),
    }
}

fn speaking_card(app: &mut App, ui: &mut Ui, t: &Theme, current: Option<&Value>) {
    let mut skip = false;
    widgets::titled(
        ui,
        t,
        "Reading now",
        "",
        |ui| {
            if current.is_some() {
                skip = widgets::button_ex(ui, t, Some(icon::STOP), "Skip", Kind::Danger, Size::Medium, 0.0, true)
                    .on_hover_text("Stop reading this message")
                    .clicked();
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            match current {
                Some(c) => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(who(c)).font(font_semibold(type_scale::BODY)).color(t.accent));
                        ui.label(RichText::new(voice_label(s(c, "voice"))).size(type_scale::SMALL + 0.5).color(t.text_dim));
                    });
                    ui.add_space(spacing::XS);
                    ui.label(RichText::new(s(c, "text")).size(type_scale::LARGE).color(t.fg));
                }
                None => {
                    widgets::hint(ui, t, "Nothing is being read right now.");
                }
            }
        },
    );
    if skip {
        act(app, "tts.skip", Value::Null);
    }
}

/// The waiting line. Returns true when the empty state's "Try a test message" was pressed.
fn queue_card(app: &mut App, ui: &mut Ui, t: &Theme, queue: &[Value], can_say: bool) -> bool {
    let mut clear = false;
    let mut test = false;
    let mut drop: Option<String> = None;
    let title = if queue.is_empty() { "Waiting".to_string() } else { format!("Waiting ({})", queue.len()) };
    widgets::titled(
        ui,
        t,
        &title,
        "Messages are read one after another, in this order.",
        |ui| {
            if !queue.is_empty() {
                clear = widgets::hold_button(ui, t, "Clear all", t.bright_red, 0.5);
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            if queue.is_empty() {
                test = widgets::empty_state(
                    ui,
                    t,
                    icon::CHAT,
                    "Nobody's waiting",
                    "When viewers send a message with bits or a tip, it lines up here.",
                    can_say.then_some("Try a test message"),
                );
                return;
            }
            for item in queue {
                egui::Frame::new()
                    .fill(t.surface_hi)
                    .corner_radius(egui::CornerRadius::same(radius::CONTROL))
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if widgets::button_ex(ui, t, Some(icon::CROSS), "Skip", Kind::Ghost, Size::Small, 0.0, true)
                                    .on_hover_text("Don't read this message")
                                    .clicked()
                                {
                                    drop = Some(s(item, "id").to_string());
                                }
                                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                    ui.label(RichText::new(who(item)).font(font_semibold(type_scale::BODY)).color(t.fg));
                                    ui.label(RichText::new(voice_label(s(item, "voice"))).size(type_scale::SMALL).color(t.text_dim));
                                });
                            });
                        });
                        let text = s(item, "text");
                        ui.add(egui::Label::new(RichText::new(text).color(t.text_dim)).truncate()).on_hover_text(text);
                    });
                ui.add_space(spacing::XS);
            }
        },
    );
    if clear {
        act(app, "tts.clear", Value::Null);
    }
    if let Some(id) = drop {
        act(app, "tts.skip", Value::map().with("id", id));
    }
    test
}

/// Test box. Returns the text to say when "Say" was pressed.
fn try_card(ui: &mut Ui, t: &Theme, form: &mut Form, voices: &[String], default_voice: &str, ready: bool, enabled: bool) -> Option<String> {
    let mut say = None;
    widgets::titled(
        ui,
        t,
        "Try it",
        "Hear how a message will sound.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Voice").font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
            let shown = |v: &str| if v == default_voice { format!("{} (your usual voice)", voice_label(v)) } else { voice_label(v) };
            egui::ComboBox::from_id_salt("tts-voice").width(ui.available_width()).selected_text(shown(&form.voice)).show_ui(ui, |ui| {
                for v in voices {
                    ui.selectable_value(&mut form.voice, v.clone(), shown(v));
                }
            });
            ui.add_space(spacing::M);
            ui.add(egui::TextEdit::multiline(&mut form.text).hint_text("Type something and press Say").desired_rows(3).desired_width(f32::INFINITY));
            ui.add_space(spacing::M);
            ui.horizontal(|ui| {
                let can = ready && enabled && !form.text.trim().is_empty();
                if widgets::button_ex(ui, t, Some(icon::PLAY), "Say", Kind::Primary, Size::Medium, 110.0, can).clicked() {
                    say = Some(form.text.clone());
                }
                if !enabled {
                    widgets::hint(ui, t, "Turn on “Read messages out loud” first.");
                } else if !ready {
                    widgets::hint(ui, t, "The voice isn't ready yet.");
                } else {
                    widgets::hint(ui, t, "Goes to the end of the waiting line.");
                }
            });
        },
    );
    say
}

fn voice_card(app: &mut App, ui: &mut Ui, t: &Theme, status: &str, ready: bool, health: &Value, voices: usize) {
    let (title, line, color, fixable) = model_words(t, status, ready, health);
    let fetching = status.starts_with("fetching");
    let mut fetch = false;
    widgets::titled(
        ui,
        t,
        "Voices",
        "They run on this computer, so nothing is sent online.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                widgets::badge(ui, t, title, color);
            });
            let count = format!("{voices} voices to choose from.");
            let line = if ready { count.as_str() } else { line };
            if !line.is_empty() {
                ui.add_space(spacing::XS);
                widgets::hint(ui, t, line);
            }
            ui.add_space(spacing::M);
            if fixable {
                let again = status.starts_with("error") || status.starts_with("fetch failed");
                let label = if again { "Download it again" } else { "Download the voice (about 90 MB)" };
                fetch = widgets::button_ex(ui, t, Some(icon::DOWN), label, Kind::Primary, Size::Medium, 0.0, !fetching).clicked();
            } else if !fetching {
                fetch = widgets::button_ex(ui, t, Some(icon::CHECK), "Check the voices", Kind::Secondary, Size::Small, 0.0, true)
                    .on_hover_text("Makes sure every voice is complete and downloads anything missing or damaged")
                    .clicked();
            }
            ui.add_space(spacing::S);
            widgets::details(ui, t, "tts-model", "Details", |ui| {
                widgets::fact(ui, t, "Model", if status.is_empty() { "unknown" } else { status });
                let detail = s(health, "detail");
                if !detail.is_empty() {
                    widgets::fact(ui, t, "Status", detail);
                }
                widgets::hint(ui, t, "Kokoro-82M with espeak-ng. Downloads are checked against pinned sha256 sums.");
            });
        },
    );
    if fetch {
        act(app, "tts.model.fetch", Value::Null);
    }
}
