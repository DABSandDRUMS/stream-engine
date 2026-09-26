//! First-run setup wizard (§15.9): devices → accounts → OBS → extras → done. Opens by itself
//! while the engine reports `setup.done = false` (project.toml lacks `[setup] done = true`).
//! Every step drives the owning subsystem's own actions; finishing writes `[setup] done`.

use crate::app::{App, ViewId};
use egui::{RichText, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};
use std::collections::HashMap;
use std::sync::Mutex;

const STEPS: [(&str, &str); 5] = [("Devices", icon::DEVICE), ("Accounts", icon::LINK), ("OBS", icon::REC), ("Extras", icon::SETTINGS), ("Finish", icon::CHECK)];

#[derive(Clone, Default)]
struct WizardState {
    step: usize,
    labels: HashMap<String, String>,
    youtube_key: String,
    relay_secret: String,
    relay_shown: Option<String>,
    last_query: f64,
}

/// Session for which the wizard was opened automatically (once per engine connection).
static AUTO_OPENED: Mutex<String> = Mutex::new(String::new());

/// Open the wizard once per connection while setup isn't done.
pub fn auto_open(app: &mut App) {
    if !app.m.connected || app.m.get("setup.done") != Some(&Value::Bool(false)) {
        return;
    }
    let Ok(mut last) = AUTO_OPENED.lock() else { return };
    if *last != app.m.session {
        *last = app.m.session.clone();
        app.view = Some(ViewId::Setup);
    }
}

fn act(app: &mut App, name: &str, args: Value) {
    app.m.command(Op::Action { name: name.into(), args });
}

fn health(app: &App, check: &str) -> (LedState, String) {
    let v = app.m.get(&format!("health.{check}"));
    let status = v.and_then(|v| v.get_path("status")).and_then(Value::as_str).unwrap_or("");
    let detail = v.and_then(|v| v.get_path("detail")).and_then(Value::as_str).unwrap_or("not reported").to_string();
    let led = match status {
        "pass" => LedState::Healthy,
        "warn" => LedState::Armed,
        "fail" => LedState::Error,
        _ => LedState::Idle,
    };
    (led, detail)
}

fn health_row(ui: &mut egui::Ui, app: &App, label: &str, check: &str) -> LedState {
    let (led, detail) = health(app, check);
    ui.horizontal(|ui| {
        widgets::led(ui, &app.t, led);
        ui.label(RichText::new(label).strong());
        ui.label(RichText::new(detail).color(app.t.fg_dim));
    });
    led
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("setup-wizard");
    let mut st: WizardState = ui.data_mut(|d| d.get_temp::<WizardState>(id)).unwrap_or_default();
    let now = ui.input(|i| i.time);
    if now - st.last_query > 1.0 {
        st.last_query = now;
        for q in ["devices", "sources"] {
            app.m.query(q, Value::Null);
        }
    }

    ui.horizontal(|ui| {
        widgets::section(ui, &t, icon::SETTINGS, "First-run setup");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Close").on_hover_text("Setup reopens next time until finished").clicked() {
                app.view = None;
            }
            if ui.button("Skip setup").on_hover_text("Mark setup as done without finishing the steps").clicked() {
                act(app, "setup.complete", Value::Null);
                app.view = None;
            }
        });
    });
    ui.add_space(6.0);
    // step bar
    ui.horizontal(|ui| {
        for (i, (label, ic)) in STEPS.iter().enumerate() {
            let state = if i == st.step {
                LedState::Active
            } else if i < st.step {
                LedState::Healthy
            } else {
                LedState::Idle
            };
            if widgets::pad(ui, &t, Vec2::new(120.0, 56.0), ic, label, None, state, None, Some(&format!("{}", i + 1))).clicked() {
                st.step = i;
            }
        }
    });
    ui.separator();
    egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - 44.0).show(ui, |ui| match st.step {
        0 => devices(app, ui, &mut st),
        1 => accounts(app, ui, &mut st),
        2 => obs(app, ui),
        3 => extras(app, ui, &mut st),
        _ => finish(app, ui),
    });
    ui.separator();
    ui.horizontal(|ui| {
        if st.step > 0 && ui.button("◀ Back").clicked() {
            st.step -= 1;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if st.step + 1 < STEPS.len() {
                if ui.button(RichText::new("Next ▶").strong()).clicked() {
                    st.step += 1;
                }
            } else if ui.button(RichText::new(format!("{} Finish setup", icon::CHECK)).strong().color(t.green)).clicked() {
                act(app, "setup.complete", Value::Null);
                app.view = None;
            }
        });
    });
    ui.data_mut(|d| d.insert_temp(id, st));
}

fn devices(app: &mut App, ui: &mut egui::Ui, st: &mut WizardState) {
    let t = app.t.clone();
    ui.label("Name each camera and pick the scene source it feeds. Names are saved to project.toml ([devices.expected]).");
    ui.add_space(4.0);
    let devs: Vec<Value> = app.m.q("devices").and_then(|v| v.get_path("devices")).and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let sources: Vec<String> = app
        .m
        .q_list("sources")
        .iter()
        .filter(|s| s.get_path("kind").and_then(Value::as_str) != Some("file"))
        .filter_map(|s| s.get_path("name").and_then(Value::as_str).map(String::from))
        .collect();
    let s = |v: &Value, k: &str| v.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
    widgets::card(ui, &t, icon::SCENE, "Cameras", if devs.iter().any(|d| s(d, "kind") == "camera") { LedState::Healthy } else { LedState::Armed }, |ui| {
        let cams: Vec<&Value> = devs.iter().filter(|d| s(d, "kind") == "camera").collect();
        if cams.is_empty() {
            ui.label(RichText::new("No cameras reported yet (the video input subsystem publishes them).").color(t.fg_dim));
        }
        egui::Grid::new("setup-cams").num_columns(5).spacing([12.0, 6.0]).show(ui, |ui| {
            for h in ["", "device", "name", "source", ""] {
                ui.label(RichText::new(h).small().color(t.fg_dim));
            }
            ui.end_row();
            for d in cams {
                let did = s(d, "id");
                let signal = d.get_path("signal").map(Value::truthy).unwrap_or(false);
                let present = d.get_path("present").is_none_or(Value::truthy);
                widgets::led(
                    ui,
                    &t,
                    if !present {
                        LedState::Error
                    } else if signal {
                        LedState::Healthy
                    } else {
                        LedState::Armed
                    },
                )
                .on_hover_text(if !present {
                    "unplugged"
                } else if signal {
                    "live signal"
                } else {
                    "no signal"
                });
                ui.label(RichText::new(format!("{}  {}", s(d, "path"), s(d, "card"))).monospace()).on_hover_text(s(d, "identity"));
                let current = { if s(d, "label").is_empty() { s(d, "name") } else { s(d, "label") } };
                let buf = st.labels.entry(did.clone()).or_insert_with(|| current.clone());
                let r = ui.add(egui::TextEdit::singleline(buf).desired_width(180.0).hint_text("e.g. kit front"));
                let label = buf.trim().to_string();
                if (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) || ui.small_button("save").clicked())
                    && !label.is_empty()
                    && label != current
                {
                    act(app, "devices.rename", Value::map().with("id", did.clone()).with("label", label));
                }
                let assigned = s(d, "source");
                egui::ComboBox::from_id_salt(("setup-src", &did)).selected_text(if assigned.is_empty() { "—".to_string() } else { assigned.clone() }).show_ui(
                    ui,
                    |ui| {
                        for src in &sources {
                            if ui.selectable_label(*src == assigned, src).clicked() {
                                act(app, "source.assign", Value::map().with("source", src.clone()).with("identity", s(d, "identity")));
                            }
                        }
                    },
                );
                ui.label(RichText::new(if assigned.is_empty() { "" } else { "assigned" }).small().color(t.green));
                ui.end_row();
            }
        });
    });
    ui.add_space(6.0);
    widgets::card(ui, &t, icon::DEVICE, "Other devices", LedState::Idle, |ui| {
        let expected: Vec<Value> = app.m.q("devices").and_then(|v| v.get_path("expected")).and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
        for e in &expected {
            let present = e.get_path("present").is_some_and(Value::truthy);
            ui.horizontal(|ui| {
                widgets::led(ui, &t, if present { LedState::Healthy } else { LedState::Error });
                ui.label(RichText::new(s(e, "label")).strong());
                ui.label(RichText::new(format!("{} {}", s(e, "kind"), if present { "present" } else { "missing" })).color(t.fg_dim));
            });
        }
        for d in devs.iter().filter(|d| s(d, "kind") != "camera" && d.get_path("expected").is_none_or(Value::is_null)) {
            ui.horizontal(|ui| {
                widgets::led(ui, &t, LedState::Idle);
                ui.label(format!("{} — {}", s(d, "kind"), s(d, "name")));
                if ui.small_button("expect").on_hover_text("Add to the expected devices checked by preflight").clicked() {
                    act(app, "devices.expect", Value::map().with("identity", s(d, "identity")).with("label", s(d, "name")));
                }
            });
        }
        ui.add_space(4.0);
        let checks: Vec<String> = app
            .m
            .state
            .keys()
            .filter_map(|k| k.strip_prefix("health."))
            .filter(|k| ["devices", "mixer", "dmx", "lights", "deck", "midi", "input", "audio"].iter().any(|p| k.starts_with(p)))
            .map(String::from)
            .collect();
        for c in checks {
            health_row(ui, app, &c, &c);
        }
    });
}

fn accounts(app: &mut App, ui: &mut egui::Ui, st: &mut WizardState) {
    let t = app.t.clone();
    let status = app.m.str("twitch.auth.status").to_string();
    let led = match status.as_str() {
        "authorized" => LedState::Healthy,
        "pending" => LedState::Armed,
        "expired" | "error" => LedState::Error,
        _ => LedState::Idle,
    };
    widgets::card(ui, &t, icon::CHAT, "Twitch", led, |ui| {
        if app.m.str("twitch.client_id").is_empty() {
            ui.label(RichText::new("Set your Twitch application's client id first: `[twitch] client_id = \"…\"` in project.toml (dev.twitch.tv/console → Applications, device code flow enabled).").color(t.yellow));
        }
        match status.as_str() {
            "authorized" => {
                ui.label(RichText::new(format!("{} Signed in as {}", icon::CHECK, app.m.str("twitch.auth.login"))).color(t.green));
                if ui.small_button("Sign in again").clicked() {
                    act(app, "twitch.auth.start", Value::map().with("account", "broadcaster"));
                }
            }
            "pending" => {
                let uri = app.m.str("twitch.auth.verification_uri").to_string();
                ui.label("Open this page, sign in with the broadcaster account, and enter the code:");
                ui.horizontal(|ui| {
                    ui.hyperlink(&uri);
                    ui.label(RichText::new(app.m.str("twitch.auth.user_code")).monospace().size(22.0).strong().color(t.accent));
                    ui.label(RichText::new(format!("expires in {} s", app.m.f("twitch.auth.expires_in_s") as i64)).small().color(t.fg_dim));
                });
                if ui.small_button("Cancel").clicked() {
                    act(app, "twitch.auth.cancel", Value::Null);
                }
            }
            _ => {
                let err = app.m.str("twitch.auth.error").to_string();
                if !err.is_empty() {
                    ui.label(RichText::new(err).color(t.bright_red));
                }
                if ui.button(RichText::new("Connect Twitch (device code)").strong()).clicked() {
                    act(app, "twitch.auth.start", Value::map().with("account", "broadcaster"));
                }
            }
        }
        ui.add_space(2.0);
        let bot = app.m.str("twitch.auth.bot.status").to_string();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Optional bot account:").color(t.fg_dim));
            if bot == "authorized" {
                ui.label(RichText::new(app.m.str("twitch.auth.bot.login")).color(t.green));
            } else if bot == "pending" {
                ui.hyperlink(app.m.str("twitch.auth.bot.verification_uri"));
                ui.label(RichText::new(app.m.str("twitch.auth.bot.user_code")).monospace().strong());
            } else if ui.small_button("Connect bot").clicked() {
                act(app, "twitch.auth.start", Value::map().with("account", "bot"));
            }
        });
    });
    ui.add_space(6.0);
    let (yt, _) = health(app, "youtube");
    widgets::card(ui, &t, icon::QUEUE, "YouTube (song requests)", yt, |ui| {
        health_row(ui, app, "key", "youtube");
        ui.label(RichText::new("Google Cloud → enable YouTube Data API v3 → Credentials → API key. Stored in the keyring only.").color(t.fg_dim));
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut st.youtube_key).password(true).desired_width(360.0).hint_text("AIza…"));
            if ui.add_enabled(!st.youtube_key.trim().is_empty(), egui::Button::new("Save key")).clicked() {
                let key = std::mem::take(&mut st.youtube_key);
                act(app, "youtube.key.set", Value::map().with("key", key.trim()));
            }
        });
    });
}

fn obs(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let installed = app.m.b("obs.plugin.installed");
    let linked = app.m.b("obs.link");
    let (h, _) = health(app, "obs");
    widgets::card(ui, &t, icon::REC, "OBS plugin", h, |ui| {
        ui.horizontal(|ui| {
            widgets::led(ui, &t, if installed { LedState::Healthy } else { LedState::Error });
            ui.label(if installed { format!("plugin installed ({})", app.m.str("obs.plugin.version")) } else { "plugin not installed".into() });
        });
        ui.horizontal(|ui| {
            widgets::led(ui, &t, if linked { LedState::Healthy } else { LedState::Armed });
            ui.label(if linked { format!("connected to OBS {}", app.m.str("obs.version")) } else { "OBS not connected".into() });
        });
        health_row(ui, app, "frames", "obs");
        if !installed {
            ui.label(
                RichText::new(
                    "The package installs the plugin to /usr/lib/obs-plugins; from a source checkout run obs-plugin/install.sh. Restart OBS afterwards.",
                )
                .color(t.yellow),
            );
        } else if !linked {
            ui.label(
                RichText::new("Start (or restart) OBS: the plugin connects to the engine by itself. Add the “stream-engine” sources to your scenes.")
                    .color(t.fg_dim),
            );
        }
    });
}

fn extras(app: &mut App, ui: &mut egui::Ui, st: &mut WizardState) {
    let t = app.t.clone();
    let (tts, _) = health(app, "tts");
    widgets::card(ui, &t, icon::BOT, "Text to speech (Kokoro)", tts, |ui| {
        health_row(ui, app, "model", "tts");
        ui.horizontal(|ui| {
            if ui.button("Download voice model").on_hover_text("~/.local/share/stream-engine/models/kokoro, checksum-verified").clicked() {
                act(app, "tts.model.fetch", Value::Null);
            }
            ui.label(RichText::new(app.m.str("tts.model.status")).color(t.fg_dim));
        });
    });
    ui.add_space(6.0);
    let (relay, _) = health(app, "relay");
    widgets::card(ui, &t, icon::LINK, "Relay (public queue, Ko-fi tips, remote mods)", relay, |ui| {
        health_row(ui, app, "link", "relay");
        ui.label(
            RichText::new("Deploy relay/ to your Cloudflare domain (docs/relay), set `[relay] url = \"wss://<domain>/link\"` in project.toml, and use the same shared secret on both sides.")
                .color(t.fg_dim),
        );
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut st.relay_secret).password(true).desired_width(300.0).hint_text("shared secret"));
            if ui.add_enabled(st.relay_secret.trim().len() >= 16, egui::Button::new("Save secret")).on_hover_text("At least 16 characters").clicked() {
                let secret = std::mem::take(&mut st.relay_secret);
                act(app, "relay.secret.set", Value::map().with("secret", secret.trim()));
                st.relay_shown = None;
            }
            if ui
                .button("Generate")
                .on_hover_text("Create a random secret, store it in the keyring, and show it once for `npx wrangler secret put RELAY_SECRET`")
                .clicked()
                && let Some(secret) = random_secret()
            {
                act(app, "relay.secret.set", Value::map().with("secret", secret.clone()));
                st.relay_shown = Some(secret);
            }
        });
        if let Some(secret) = st.relay_shown.clone() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&secret).monospace().color(t.accent));
                if ui.small_button("copy").clicked() {
                    ui.ctx().copy_text(secret.clone());
                }
            });
            ui.label(RichText::new("In relay/: npx wrangler secret put RELAY_SECRET  (paste the value above)").small().monospace().color(t.fg_dim));
        }
        let on = app.m.b("remote_mod.enabled");
        ui.horizontal(|ui| {
            widgets::led(ui, &t, if on { LedState::Healthy } else { LedState::Idle });
            ui.label(if on { "remote mod console enabled" } else { "remote mod console off — enable with [remote_mod] enabled = true" });
        });
    });
    ui.add_space(6.0);
    widgets::card(ui, &t, icon::SESSION, "Backups", health(app, "backup").0, |ui| {
        health_row(ui, app, "runtime DB", "backup");
        health_row(ui, app, "recordings", "recordings");
    });
}

fn finish(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.label(RichText::new("Summary").strong());
    ui.add_space(4.0);
    let twitch = app.m.str("twitch.auth.status") == "authorized";
    ui.horizontal(|ui| {
        widgets::led(ui, &t, if twitch { LedState::Healthy } else { LedState::Armed });
        ui.label("Twitch account");
    });
    for (label, check) in [("Devices", "devices"), ("YouTube key", "youtube"), ("OBS", "obs"), ("TTS", "tts"), ("Relay", "relay"), ("Backups", "backup")] {
        health_row(ui, app, label, check);
    }
    ui.add_space(8.0);
    ui.label(
        RichText::new("Anything still yellow can be fixed later; preflight (Show mode) keeps checking. Finishing writes [setup] done = true to project.toml.")
            .color(t.fg_dim),
    );
}

/// 32 random bytes as hex (from the kernel CSPRNG).
fn random_secret() -> Option<String> {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom").ok()?.read_exact(&mut b).ok()?;
    Some(b.iter().map(|x| format!("{x:02x}")).collect())
}
