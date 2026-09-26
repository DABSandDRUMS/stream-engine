//! TTS panel (§14.4): the live on/off toggle, what is being spoken (SKIP), the waiting queue
//! with per-item drop and CLEAR, a test box, and the Kokoro model status with Fetch.
//! Data: query `tts` (polled every 0.5 s) and `tts.*` state; commands: `tts.*` actions.

use crate::app::App;
use egui::{RichText, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};

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

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
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
    let status = s(&q, "model_status").to_string();
    let health = app.m.get("health.tts").cloned().unwrap_or_default();
    if form.voice.is_empty() || !voices.contains(&form.voice) {
        form.voice = Some(s(&q, "voice")).filter(|v| !v.is_empty()).or(voices.first().map(String::as_str)).unwrap_or("").to_string();
    }

    ui.horizontal(|ui| {
        widgets::section(ui, &t, icon::CHAT, "Text to speech");
        let (led, label) = if ready { (LedState::Healthy, "READY") } else { (LedState::Error, "NOT READY") };
        widgets::pill(ui, &t, icon::LIVE, label, led);
        if !s(&health, "detail").is_empty() {
            ui.label(RichText::new(s(&health, "detail")).color(t.fg_dim));
        }
    });
    ui.add_space(6.0);

    ui.columns(2, |cols| {
        // --- left: toggle, now speaking, queue
        let ui = &mut cols[0];
        ui.horizontal(|ui| {
            let (label, color, led) = if enabled { ("TTS ON", Some(t.green), LedState::Active) } else { ("TTS OFF", None, LedState::Idle) };
            let hint = if enabled { "click to mute + clear" } else { "click to enable" };
            if widgets::pad(ui, &t, Vec2::new(150.0, 64.0), icon::CHAT, label, color, led, None, Some(hint)).clicked() {
                app.m.text(if enabled { "set tts.enabled false" } else { "set tts.enabled true" });
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::hold_button(ui, &t, "CLEAR ALL", t.bright_red, 0.5) {
                    act(app, "tts.clear", Value::Null);
                }
            });
        });
        ui.add_space(8.0);
        let led = if current.is_some() { LedState::Active } else { LedState::Idle };
        widgets::card(ui, &t, icon::PLAY, "Speaking", led, |ui| match &current {
            Some(c) => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(s(c, "user")).strong().color(t.accent));
                    ui.label(RichText::new(s(c, "voice")).color(t.fg_dim));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(RichText::new(format!("{} SKIP", icon::STOP)).strong().color(t.bright_red)).clicked() {
                            act(app, "tts.skip", Value::Null);
                        }
                    });
                });
                ui.label(RichText::new(s(c, "text")).size(16.0));
            }
            None => {
                ui.label(RichText::new("nothing playing").color(t.fg_dim));
            }
        });
        ui.add_space(8.0);
        widgets::section(ui, &t, icon::QUEUE, &format!("Queue ({})", queue.len()));
        egui::ScrollArea::vertical().id_salt("tts-queue").max_height(ui.available_height()).auto_shrink([false, false]).show(ui, |ui| {
            if queue.is_empty() {
                ui.label(RichText::new("empty").color(t.fg_dim));
            }
            egui::Grid::new("tts-queue-grid").striped(true).num_columns(4).spacing([10.0, 4.0]).show(ui, |ui| {
                for item in &queue {
                    ui.label(RichText::new(s(item, "user")).strong());
                    ui.label(RichText::new(s(item, "voice")).color(t.fg_dim));
                    let text = s(item, "text");
                    let short: String = text.chars().take(80).collect();
                    ui.label(if short.len() < text.len() { format!("{short}…") } else { short }).on_hover_text(text);
                    if ui.small_button(icon::CROSS).on_hover_text("drop this request").clicked() {
                        act(app, "tts.skip", Value::map().with("id", s(item, "id")));
                    }
                    ui.end_row();
                }
            });
        });

        // --- right: test box, model
        let ui = &mut cols[1];
        widgets::card(ui, &t, icon::SIM, "Test", LedState::Idle, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut form.text).hint_text("Thanks for the five hundred bits, let's go!").desired_rows(3).desired_width(f32::INFINITY),
            );
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("tts-voice").selected_text(form.voice.clone()).show_ui(ui, |ui| {
                    for v in &voices {
                        ui.selectable_value(&mut form.voice, v.clone(), v);
                    }
                });
                let can = ready && enabled && !form.text.trim().is_empty();
                if ui.add_enabled(can, egui::Button::new(RichText::new(format!("{} Speak", icon::PLAY)).strong())).clicked() {
                    form.tests += 1;
                    let mut args =
                        Value::map().with("text", form.text.trim()).with("id", format!("ui-test-{}", form.tests)).with("kind", "test").with("user", "ui");
                    if !form.voice.is_empty() {
                        args = args.with("voice", form.voice.clone());
                    }
                    act(app, "tts.say", args);
                }
            });
        });
        ui.add_space(8.0);
        let fetching = status.starts_with("fetching");
        let led = if ready {
            LedState::Healthy
        } else if fetching || status == "loading" {
            LedState::Armed
        } else {
            LedState::Error
        };
        widgets::card(ui, &t, icon::DEVICE, "Kokoro model", led, |ui| {
            ui.label(RichText::new(if status.is_empty() { "unknown" } else { &status }).monospace());
            ui.label(RichText::new(format!("{} voices installed", voices.len())).color(t.fg_dim));
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!fetching, egui::Button::new(format!("{} Fetch / verify model", icon::LINK)))
                    .on_hover_text("download Kokoro-82M + voices (sha256-pinned)")
                    .clicked()
                {
                    act(app, "tts.model.fetch", Value::Null);
                }
            });
        });
    });
    ui.data_mut(|d| d.insert_temp(id, form));
}
