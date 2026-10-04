//! "Get started" (§15.9): a plain-language checklist instead of a wizard that takes over the
//! window. Each step says what it is for, whether it's done, and has one obvious button. Steps
//! drive the owning subsystem's own actions; "I'm done" writes `[setup] done = true`.

use crate::app::App;
use egui::{Align, Layout, RichText, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::theme::{font_bold, font_mono, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};
use std::collections::HashMap;

#[derive(Clone, Default)]
struct State {
    labels: HashMap<String, String>,
    youtube_key: String,
    twitch_id: String,
    relay_secret: String,
    relay_shown: Option<String>,
    cameras_open: bool,
    last_query: f64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Done,
    Todo,
    Optional,
    Working,
}

fn act(app: &mut App, name: &str, args: Value) {
    app.m.command(Op::Action { name: name.into(), args });
}

fn health_status(app: &App, check: &str) -> &'static str {
    match app.m.get(&format!("health.{check}")).and_then(|v| v.get_path("status")).and_then(Value::as_str) {
        Some("pass") => "pass",
        Some("warn") => "warn",
        Some("fail") => "fail",
        _ => "",
    }
}

fn cameras(app: &App) -> Vec<Value> {
    app.m
        .q("devices")
        .and_then(|v| v.get_path("devices"))
        .and_then(Value::as_list)
        .map(|l| l.iter().filter(|d| d.get_path("kind").and_then(Value::as_str) == Some("camera")).cloned().collect())
        .unwrap_or_default()
}

fn camera_status(app: &App) -> (Status, String) {
    let cams = cameras(app);
    let live = cams.iter().filter(|d| d.get_path("signal").is_some_and(Value::truthy)).count();
    match (cams.len(), health_status(app, "devices")) {
        // the camera list loads on this page; elsewhere the health check is enough
        (0, "pass") => (Status::Done, "All your cameras are connected.".into()),
        (0, _) => (Status::Todo, "No cameras found yet. Plug them in and they'll show up here.".into()),
        (n, "pass") => (Status::Done, format!("{n} cameras found, {live} showing a picture.")),
        (n, _) => (Status::Todo, format!("{n} cameras found, {live} showing a picture. Something expected is missing.")),
    }
}

fn twitch_status(app: &App) -> Status {
    match app.m.str("twitch.auth.status") {
        "authorized" => Status::Done,
        "pending" => Status::Working,
        _ => Status::Todo,
    }
}

fn obs_status(app: &App) -> Status {
    // OBS streaming readiness is independent of the app's recorder and selected inputs.
    if app.m.b("obs.link") && health_status(app, "obs") == "pass" { Status::Done } else { Status::Todo }
}

/// Required steps not done yet (the sidebar badge).
pub fn steps_left(app: &App) -> usize {
    if !app.m.connected || app.m.b("setup.done") {
        return 0;
    }
    [camera_status(app).0, twitch_status(app), obs_status(app)].iter().filter(|s| **s != Status::Done).count()
}

/// One checklist step: status marker, title, one sentence, and a body with the actions.
fn step(ui: &mut egui::Ui, t: &se_ui_kit::Theme, n: usize, status: Status, title: &str, blurb: &str, body: impl FnOnce(&mut egui::Ui)) {
    widgets::panel(ui, t, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(36.0), egui::Sense::hover());
            let p = ui.painter();
            match status {
                Status::Done => {
                    p.circle_filled(r.center(), 16.0, mix(t.surface, t.green, 0.25));
                    p.text(r.center(), egui::Align2::CENTER_CENTER, icon::CHECK, se_ui_kit::theme::font(15.0), t.green);
                }
                Status::Working => {
                    p.circle_filled(r.center(), 16.0, mix(t.surface, t.yellow, 0.25));
                    p.text(r.center(), egui::Align2::CENTER_CENTER, icon::CLOCK, se_ui_kit::theme::font(15.0), t.yellow);
                }
                Status::Todo => {
                    p.circle_filled(r.center(), 16.0, t.accent);
                    p.text(r.center(), egui::Align2::CENTER_CENTER, n.to_string(), font_bold(15.0), t.on_accent);
                }
                Status::Optional => {
                    p.circle_stroke(r.center(), 15.5, egui::Stroke::new(1.5, t.border));
                    p.text(r.center(), egui::Align2::CENTER_CENTER, n.to_string(), font_bold(15.0), t.text_dim);
                }
            }
            ui.add_space(spacing::M);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(title).font(font_semibold(type_scale::LARGE)).color(t.fg));
                    match status {
                        Status::Done => {
                            widgets::badge(ui, t, "Done", t.green);
                        }
                        // optional steps sit under the OPTIONAL heading already
                        Status::Optional => {}
                        Status::Working => {
                            widgets::badge(ui, t, "Waiting for you", t.yellow);
                        }
                        Status::Todo => {}
                    }
                });
                ui.label(RichText::new(blurb).color(t.text_dim));
                ui.add_space(spacing::S);
                body(ui);
            });
        });
    });
    ui.add_space(spacing::M);
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let id = egui::Id::new("get-started");
    let mut st: State = ui.data_mut(|d| d.get_temp::<State>(id)).unwrap_or_default();
    let now = ui.input(|i| i.time);
    if now - st.last_query > 2.0 {
        st.last_query = now;
        for q in ["devices", "sources"] {
            app.m.query(q, Value::Null);
        }
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        // one readable column, centered on wide screens
        let pad = ((ui.available_width() - 900.0) / 2.0).clamp(0.0, 600.0);
        ui.horizontal_top(|ui| {
            ui.add_space(pad);
            ui.vertical(|ui| checklist(app, ui, &mut st));
        });
    });
    ui.data_mut(|d| d.insert_temp(id, st));
}

fn checklist(app: &mut App, ui: &mut egui::Ui, st: &mut State) {
    let t = app.t.clone();
    {
        ui.set_max_width(900.0);
        if app.m.b("setup.done") {
            widgets::panel(ui, &t, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::CHECK).size(20.0).color(t.green));
                    ui.label(RichText::new("You're all set up. You can change anything here at any time.").color(t.fg));
                });
            });
            ui.add_space(spacing::L);
        } else {
            widgets::hint(ui, &t, "Three things are needed before your first stream. The rest is optional and can wait.");
            ui.add_space(spacing::M);
        }

        // 1. cameras
        let (cam_st, cam_text) = camera_status(app);
        let any_cams = !cameras(app).is_empty();
        step(ui, &t, 1, cam_st, "Cameras", &cam_text, |ui| {
            if !any_cams {
                // nothing to name yet: the blurb says to plug them in; they appear by themselves
                return;
            }
            let label = if st.cameras_open { "Hide cameras" } else { "Name your cameras" };
            if widgets::button_ex(
                ui,
                &t,
                Some(icon::CAMERA),
                label,
                if cam_st == Status::Done { Kind::Secondary } else { Kind::Primary },
                Size::Medium,
                0.0,
                true,
            )
            .clicked()
            {
                st.cameras_open = !st.cameras_open;
            }
            if st.cameras_open {
                ui.add_space(spacing::S);
                camera_list(app, ui, st);
            }
        });

        // 2. twitch
        twitch_step(app, ui, st);

        // 3. OBS
        let obs = obs_status(app);
        let installed = app.m.b("obs.plugin.installed");
        let obs_text = if obs == Status::Done {
            "OBS is connected and receives your video for streaming."
        } else if !installed {
            "OBS sends your stream to Twitch. The Stream Engine add-on for OBS isn't installed yet."
        } else if !app.m.b("obs.link") {
            "OBS sends your stream to Twitch. Open OBS and it connects by itself."
        } else {
            "OBS is connected. Add the Stream Engine video for streaming."
        };
        let mut open_obs = false;
        step(ui, &t, 3, obs, "OBS", obs_text, |ui| {
            if obs != Status::Done && app.m.b("obs.link") {
                if widgets::button_ex(ui, &t, Some(icon::PLUS), "Add our video to OBS", Kind::Primary, Size::Medium, 0.0, true).clicked() {
                    act(app, "obs.setup", Value::Null);
                }
                widgets::hint(ui, &t, "Adds wide and vertical video for streaming. Recording is independent; choose its inputs in Settings.");
            } else if obs != Status::Done && installed {
                if widgets::button_ex(ui, &t, Some(icon::PLAY), "Open OBS", Kind::Primary, Size::Medium, 0.0, true).clicked() {
                    open_obs = true;
                }
            } else if !installed {
                widgets::hint(ui, &t, "Install it with the Stream Engine package, then restart OBS.");
            }
        });

        if open_obs {
            crate::views::status::open_obs(app);
        }
        ui.add_space(spacing::S);
        widgets::section(ui, &t, "", "OPTIONAL");

        // 4. YouTube
        let yt = health_status(app, "youtube") == "pass";
        step(
            ui,
            &t,
            4,
            if yt { Status::Done } else { Status::Optional },
            "Song requests",
            "Lets viewers request YouTube songs with !sr. Needs a free YouTube key.",
            |ui| {
                if !yt {
                    ui.horizontal(|ui| {
                        ui.add(se_ui_kit::widgets::field(&mut st.youtube_key).password(true).desired_width(340.0).hint_text("Paste your YouTube key"));
                        if widgets::button_ex(ui, &t, None, "Save", Kind::Primary, Size::Medium, 0.0, !st.youtube_key.trim().is_empty()).clicked() {
                            let key = std::mem::take(&mut st.youtube_key);
                            act(app, "youtube.key.set", Value::map().with("key", key.trim()));
                        }
                    });
                    show_me_how(
                        ui,
                        &t,
                        "youtube",
                        &[
                            "Open Google Cloud and sign in with your Google account.",
                            "Make a project (any name), then turn on \"YouTube Data API v3\".",
                            "Go to Credentials → Create credentials → API key, and copy it.",
                            "Paste it above and press Save. It's kept safely in your system keyring.",
                        ],
                        Some(("Open Google Cloud", "https://console.cloud.google.com/apis/library/youtube.googleapis.com")),
                    );
                }
            },
        );

        // 5. voice
        let tts = health_status(app, "tts") == "pass";
        step(ui, &t, 5, if tts { Status::Done } else { Status::Optional }, "Text-to-speech voice", "Reads out donation and bits messages on stream.", |ui| {
            if !tts && widgets::button_ex(ui, &t, Some(icon::DOWN), "Download the voice (about 90 MB)", Kind::Secondary, Size::Medium, 0.0, true).clicked() {
                act(app, "tts.model.fetch", Value::Null);
            }
            let s = app.m.str("tts.model.status");
            if !tts && !s.is_empty() {
                widgets::hint(ui, &t, s);
            }
        });

        // 6. relay
        let relay = health_status(app, "relay") == "pass";
        step(
            ui,
            &t,
            6,
            if relay { Status::Done } else { Status::Optional },
            "Tips and public song queue",
            "Ko-fi tips on stream, a public song-queue web page, and moderators helping from their browser.",
            |ui| {
                if relay {
                    return;
                }
                show_me_how(
                    ui,
                    &t,
                    "relay",
                    &[
                        "Press \"Make one for me\" below and copy the password it shows.",
                        "Follow the guide to put the helper on your Cloudflare account, using that password.",
                        "Put your helper's address into Stream Engine's settings, then come back here.",
                    ],
                    Some(("Open the guide", "https://github.com/DABSandDRUMS/stream-engine/blob/main/docs/relay.md")),
                );
                ui.horizontal(|ui| {
                    ui.add(se_ui_kit::widgets::field(&mut st.relay_secret).password(true).desired_width(300.0).hint_text("Shared password"));
                    if widgets::button_ex(ui, &t, None, "Save", Kind::Secondary, Size::Medium, 0.0, st.relay_secret.trim().len() >= 16).clicked() {
                        let secret = std::mem::take(&mut st.relay_secret);
                        act(app, "relay.secret.set", Value::map().with("secret", secret.trim()));
                        st.relay_shown = None;
                    }
                    if widgets::button_ex(ui, &t, None, "Make one for me", Kind::Secondary, Size::Medium, 0.0, true).clicked()
                        && let Some(secret) = random_secret()
                    {
                        act(app, "relay.secret.set", Value::map().with("secret", secret.clone()));
                        st.relay_shown = Some(secret);
                    }
                });
                if let Some(secret) = st.relay_shown.clone() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&secret).font(font_mono(type_scale::SMALL)).color(t.accent));
                        if widgets::button_ex(ui, &t, Some(icon::COPY), "Copy", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                            ui.ctx().copy_text(secret.clone());
                        }
                    });
                    widgets::hint(ui, &t, "Saved. Use this same password when you set up the Cloudflare part (RELAY_SECRET).");
                }
            },
        );

        ui.add_space(spacing::L);
        if !app.m.b("setup.done") {
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                if widgets::button_ex(ui, &t, Some(icon::CHECK), "I'm done setting up", Kind::Primary, Size::Large, 0.0, true).clicked() {
                    act(app, "setup.complete", Value::Null);
                }
                widgets::hint(ui, &t, "Hides the setup reminder. You can come back here any time.");
            });
        }
        ui.add_space(spacing::XL);
    }
}

fn twitch_step(app: &mut App, ui: &mut egui::Ui, st: &mut State) {
    let t = app.t.clone();
    let status = twitch_status(app);
    let has_id = !app.m.str("twitch.client_id").is_empty();
    let blurb = match status {
        Status::Done => format!("Connected as {}. Alerts, chat and channel points work.", app.m.str("twitch.auth.login")),
        Status::Working => "Almost there: approve Stream Engine on the Twitch page with the code below.".to_string(),
        _ if !has_id => "So alerts, chat and channel points work. First, paste your Twitch app ID.".to_string(),
        _ => "So alerts, chat and channel points work.".to_string(),
    };
    step(ui, &t, 2, status, "Twitch", &blurb, |ui| match status {
        Status::Done => {
            if widgets::button_ex(ui, &t, None, "Connect again", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                act(app, "twitch.auth.start", Value::map().with("account", "broadcaster"));
            }
        }
        Status::Working => {
            let uri = app.m.str("twitch.auth.verification_uri").to_string();
            let code = app.m.str("twitch.auth.user_code").to_string();
            ui.horizontal(|ui| {
                egui::Frame::new().fill(t.surface_hi).corner_radius(radius::CONTROL).inner_margin(egui::Margin::symmetric(18, 8)).show(ui, |ui| {
                    ui.label(RichText::new(&code).font(font_mono(26.0)).color(t.fg));
                });
                ui.add_space(spacing::S);
                if widgets::button_ex(ui, &t, Some(icon::TWITCH), "Open Twitch", Kind::Primary, Size::Large, 0.0, !uri.is_empty()).clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(&uri));
                }
                if widgets::button_ex(ui, &t, None, "Cancel", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                    act(app, "twitch.auth.cancel", Value::Null);
                }
            });
            widgets::hint(
                ui,
                &t,
                &format!("Sign in as your streaming account and type the code. It expires in {} seconds.", app.m.f("twitch.auth.expires_in_s") as i64),
            );
        }
        _ if !has_id => {
            ui.horizontal(|ui| {
                ui.add(se_ui_kit::widgets::field(&mut st.twitch_id).desired_width(340.0).hint_text("Paste your Twitch app ID"));
                if widgets::button_ex(ui, &t, None, "Save", Kind::Primary, Size::Medium, 0.0, st.twitch_id.trim().len() >= 20).clicked() {
                    let id = std::mem::take(&mut st.twitch_id);
                    act(app, "project.write", Value::map().with("path", "project.toml").with("set", Value::map().with("twitch.client_id", id.trim())));
                }
            });
            show_me_how(
                ui,
                &t,
                "twitch",
                &[
                    "Open Twitch's developer page and sign in (Twitch asks you to turn on two-step login first).",
                    "Press \"Register Your Application\". Name it anything, e.g. My Stream Engine.",
                    "For the address box type http://localhost, pick Category \"Other\" and Client Type \"Public\".",
                    "Press Create, then Manage, and copy the \"Client ID\". Paste it above and press Save.",
                ],
                Some(("Open Twitch's page", "https://dev.twitch.tv/console/apps/create")),
            );
        }
        _ => {
            let err = app.m.str("twitch.auth.error").to_string();
            if !err.is_empty() {
                ui.label(RichText::new(err).color(t.bright_red));
            }
            if widgets::button_ex(ui, &t, Some(icon::TWITCH), "Connect Twitch", Kind::Primary, Size::Large, 0.0, true).clicked() {
                act(app, "twitch.auth.start", Value::map().with("account", "broadcaster"));
            }
        }
    });
}

fn camera_list(app: &mut App, ui: &mut egui::Ui, st: &mut State) {
    let t = app.t.clone();
    let s = |v: &Value, k: &str| v.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
    let sources: Vec<String> = app
        .m
        .q_list("sources")
        .iter()
        .filter(|s| s.get_path("kind").and_then(Value::as_str) != Some("file"))
        .filter_map(|s| s.get_path("name").and_then(Value::as_str).map(String::from))
        .collect();
    for d in cameras(app) {
        let did = s(&d, "id");
        let signal = d.get_path("signal").is_some_and(Value::truthy);
        let present = d.get_path("present").is_none_or(Value::truthy);
        egui::Frame::new().fill(t.surface_hi).corner_radius(radius::CONTROL).inner_margin(egui::Margin::symmetric(12, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (c, txt) = if !present {
                    (t.bright_red, "Unplugged")
                } else if signal {
                    (t.green, "Picture")
                } else {
                    (t.yellow, "No picture")
                };
                widgets::badge(ui, &t, txt, c);
                let current = if s(&d, "label").is_empty() { s(&d, "name") } else { s(&d, "label") };
                let buf = st.labels.entry(did.clone()).or_insert_with(|| current.clone());
                let r = ui.add(se_ui_kit::widgets::field(buf).desired_width(220.0).hint_text("Name, e.g. Kit front"));
                let label = buf.trim().to_string();
                if r.lost_focus() && !label.is_empty() && label != current {
                    act(app, "devices.rename", Value::map().with("id", did.clone()).with("label", label));
                }
                ui.label(RichText::new("used as").color(t.text_dim));
                let assigned = s(&d, "source");
                egui::ComboBox::from_id_salt(("setup-src", &did))
                    .width(160.0)
                    .selected_text(if assigned.is_empty() { "Not used".to_string() } else { assigned.clone() })
                    .show_ui(ui, |ui| {
                        for src in &sources {
                            if ui.selectable_label(*src == assigned, src).clicked() {
                                act(app, "source.assign", Value::map().with("source", src.clone()).with("identity", s(&d, "identity")));
                            }
                        }
                    });
            });
        });
        ui.add_space(6.0);
    }
}

/// 32 random bytes as hex (from the kernel CSPRNG).
fn random_secret() -> Option<String> {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom").ok()?.read_exact(&mut b).ok()?;
    Some(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// "Show me how": numbered plain steps behind a disclosure, with a button that opens the page.
fn show_me_how(ui: &mut egui::Ui, t: &se_ui_kit::Theme, id: &str, steps: &[&str], link: Option<(&str, &str)>) {
    widgets::details(ui, t, ("show-me-how", id), "Show me how", |ui| {
        for (i, s) in steps.iter().enumerate() {
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(format!("{}.", i + 1)).font(font_semibold(type_scale::BODY)).color(t.accent));
                ui.add(egui::Label::new(RichText::new(*s).color(t.fg)).wrap());
            });
        }
        if let Some((label, url)) = link {
            ui.add_space(spacing::S);
            if widgets::button_ex(ui, t, Some(icon::RIGHT), label, Kind::Secondary, Size::Small, 0.0, true).clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
        }
    });
}
