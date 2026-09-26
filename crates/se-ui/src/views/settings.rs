//! Settings (§15.6): accounts (Twitch device-code connect; API keys go to the keyring, never
//! to project files), API endpoints / token / paired devices (§19), shortcuts (rebindable,
//! saved to `layouts/shortcuts.toml`), appearance, and layouts.

use crate::app::App;
use egui::RichText;
use se_proto::Value;
use se_ui_kit::widgets::{self, LedState, icon};
use std::time::Instant;

#[derive(Default)]
pub struct SettingsState {
    pub secret_input: std::collections::HashMap<String, String>,
    pub new_device: String,
    pub new_scope: String,
    /// Token shown once after pairing a device.
    pub paired: Option<(String, String)>,
    /// Action waiting for a key press to rebind.
    pub rebinding: Option<String>,
    pub rebind_error: Option<String>,
    pub asked: Option<Instant>,
    pub layout_name: String,
}

fn refresh(app: &mut App) {
    if app.m.connected && app.settings.asked.is_none_or(|a| a.elapsed().as_secs() >= 2) {
        app.settings.asked = Some(Instant::now());
        app.m.query("api.info", Value::Null);
        app.m.query("secrets.status", Value::Null);
    }
}

/// Random 32-byte token, hex encoded (device pairing).
pub fn random_token() -> std::io::Result<String> {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn copy(ui: &egui::Ui, text: &str) {
    ui.ctx().copy_text(text.to_string());
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    refresh(app);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        widgets::section(ui, &t, icon::SETTINGS, "Settings");
        accounts(app, ui);
        ui.add_space(8.0);
        api(app, ui);
        ui.add_space(8.0);
        crate::shortcuts_ui::settings_section(app, ui);
        ui.add_space(8.0);
        appearance(app, ui);
    });
}

/// `twitch.auth.status`: none | pending | authorized | expired | error.
fn account_state(app: &App, p: &str) -> (String, LedState) {
    let st = app.m.str(&format!("{p}.status")).to_string();
    let led = match st.as_str() {
        "authorized" => LedState::Healthy,
        "pending" => LedState::Armed,
        "error" | "expired" => LedState::Error,
        _ => LedState::Idle,
    };
    (if st.is_empty() || st == "none" { "not connected".into() } else { st }, led)
}

fn device_code(app: &App, ui: &mut egui::Ui, p: &str) {
    let t = app.t.clone();
    let code = app.m.str(&format!("{p}.user_code")).to_string();
    let uri = app.m.str(&format!("{p}.verification_uri")).to_string();
    if code.is_empty() {
        return;
    }
    egui::Frame::new().fill(t.bg_darker).corner_radius(6).inner_margin(10).show(ui, |ui| {
        ui.label(RichText::new("Open this page and enter the code:").color(t.fg_dim));
        ui.horizontal(|ui| {
            ui.label(RichText::new(&code).size(28.0).strong().monospace().color(t.accent));
            if ui.button("copy code").clicked() {
                copy(ui, &code);
            }
        });
        ui.horizontal(|ui| {
            ui.hyperlink_to(&uri, &uri);
            if ui.button("open in browser").clicked() {
                let _ = std::process::Command::new("xdg-open").arg(&uri).spawn();
            }
        });
        let exp = app.m.f(&format!("{p}.expires_in_s"));
        if exp > 0.0 && app.m.str(&format!("{p}.status")) == "pending" {
            ui.label(RichText::new(format!("expires in {:.0}s", exp)).small().color(t.fg_dim));
        }
    });
}

fn accounts(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::card(ui, &t, icon::LINK, "Accounts", LedState::Idle, |ui| {
        for (label, p, start_args) in [
            ("Twitch (broadcaster)", "twitch.auth", Value::map().with("account", "broadcaster")),
            ("Twitch (bot account)", "twitch.auth.bot", Value::map().with("account", "bot")),
        ] {
            let (st, led) = account_state(app, p);
            ui.horizontal(|ui| {
                widgets::led(ui, &t, led);
                ui.label(RichText::new(label).strong());
                let user = app.m.str(&format!("{p}.login")).to_string();
                ui.label(RichText::new(if user.is_empty() { st.clone() } else { format!("{st} as {user}") }).color(t.fg_dim));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match st.as_str() {
                    "authorized" => {
                        if widgets::hold_button(ui, &t, "hold: disconnect", t.bright_red, 0.6) {
                            app.m.action("twitch.auth.logout", start_args.clone());
                        }
                    }
                    "pending" => {
                        if ui.button("cancel").clicked() {
                            app.m.action("twitch.auth.cancel", start_args.clone());
                        }
                    }
                    _ => {
                        if ui.button(RichText::new("connect").strong()).clicked() {
                            app.m.action("twitch.auth.start", start_args.clone());
                        }
                    }
                });
            });
            device_code(app, ui, p);
            let err = app.m.str(&format!("{p}.error")).to_string();
            if !err.is_empty() {
                ui.label(RichText::new(err).color(t.bright_red));
            }
            let missing = app
                .m
                .get(&format!("{p}.missing_scopes"))
                .and_then(Value::as_list)
                .map(|l| l.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            if !missing.is_empty() {
                ui.label(RichText::new(format!("missing scopes: {missing} — reconnect")).small().color(t.yellow));
            }
            if !app.m.has(&format!("{p}.status")) && p == "twitch.auth" {
                ui.label(
                    RichText::new("Twitch integration not running (needs a Twitch developer app Client ID in project.toml [twitch])").small().color(t.fg_dim),
                );
            }
        }
        ui.separator();
        ui.label(RichText::new("Keys (stored in the system keyring)").small().strong());
        let secrets = app.m.q_list("secrets.status").to_vec();
        if secrets.is_empty() {
            ui.label(RichText::new("engine offline").small().color(t.fg_dim));
        }
        for s in secrets {
            let name = s.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
            let label = s.get_path("label").and_then(Value::as_str).unwrap_or(&name).to_string();
            let set = s.get_path("set").is_some_and(Value::truthy);
            ui.horizontal(|ui| {
                widgets::led(ui, &t, if set { LedState::Healthy } else { LedState::Idle });
                ui.label(&label);
                let buf = app.settings.secret_input.entry(name.clone()).or_default();
                ui.add(
                    egui::TextEdit::singleline(buf)
                        .password(true)
                        .hint_text(if set { "•••• (set) — paste to replace" } else { "paste key" })
                        .desired_width(260.0),
                );
                let val = buf.trim().to_string();
                if ui.add_enabled(!val.is_empty(), egui::Button::new("save")).clicked() {
                    app.m.action("secrets.set", Value::map().with("name", name.clone()).with("value", val));
                    app.settings.secret_input.remove(&name);
                    app.settings.asked = None;
                }
                if set && ui.small_button("remove").clicked() {
                    app.m.action("secrets.delete", Value::map().with("name", name.clone()));
                    app.settings.asked = None;
                }
            });
        }
        if let Some(Value::Map(accts)) = app.m.get("accounts").cloned() {
            for (id, a) in accts {
                ui.label(format!("{id}: {}", a.get_path("state").map(|v| v.to_string()).unwrap_or_default()));
            }
        }
    });
}

fn api(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::card(ui, &t, icon::LINK, "API & security", LedState::Idle, |ui| {
        let Some(info) = app.m.q("api.info").cloned() else {
            ui.label(RichText::new("engine offline").small().color(t.fg_dim));
            return;
        };
        let g = |k: &str| info.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
        egui::Grid::new("api").num_columns(2).show(ui, |ui| {
            for (k, v) in [("unix socket (UI, CLI; user-only)", g("socket")), ("HTTP", g("http")), ("WebSocket", g("ws")), ("OSC", g("osc"))] {
                ui.label(RichText::new(k).small());
                ui.label(RichText::new(v).monospace());
                ui.end_row();
            }
            ui.label(RichText::new("full-access token").small());
            ui.horizontal(|ui| {
                let set = info.get_path("token_set").is_some_and(Value::truthy);
                ui.label(RichText::new(if set { "in the keyring (never sent over the API)" } else { "not set" }).small().color(t.fg_dim));
                if ui.small_button("copy").on_hover_text("runs `stream token` locally").clicked() {
                    match std::process::Command::new("stream").arg("token").output() {
                        Ok(o) if o.status.success() => {
                            copy(ui, String::from_utf8_lossy(&o.stdout).trim());
                            app.m.toast("API token copied", false);
                        }
                        Ok(o) => app.m.toast(format!("stream token: {}", String::from_utf8_lossy(&o.stderr).trim()), true),
                        Err(e) => app.m.toast(format!("stream token: {e}"), true),
                    }
                }
                if widgets::hold_button(ui, &t, "hold: rotate", t.yellow, 0.8) {
                    app.m.action("api.token.rotate", Value::Null);
                    app.settings.asked = None;
                }
            });
            ui.end_row();
        });
        ui.label(RichText::new("paired devices").small().strong());
        let devices = info.get_path("devices").and_then(Value::as_list).unwrap_or(&[]).to_vec();
        if devices.is_empty() {
            ui.label(RichText::new("none").small().color(t.fg_dim));
        }
        for d in devices {
            let name = d.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
            ui.horizontal(|ui| {
                ui.label(RichText::new(&name).strong());
                ui.label(RichText::new(d.get_path("scope").and_then(Value::as_str).unwrap_or("")).small().color(t.fg_dim));
                if ui.small_button("unpair").clicked() {
                    app.m.action("api.device.remove", Value::map().with("name", name.clone()));
                    app.settings.asked = None;
                }
            });
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut app.settings.new_device).hint_text("device name (phone, tablet)").desired_width(180.0));
            if app.settings.new_scope.is_empty() {
                app.settings.new_scope = "read".into();
            }
            egui::ComboBox::from_id_salt("scope").selected_text(app.settings.new_scope.clone()).show_ui(ui, |ui| {
                for s in ["read", "mod", "full"] {
                    ui.selectable_value(&mut app.settings.new_scope, s.to_string(), s);
                }
            });
            let name = app.settings.new_device.trim().to_string();
            if ui.add_enabled(!name.is_empty(), egui::Button::new("pair")).clicked() {
                match random_token() {
                    Ok(tok) => {
                        app.m.action(
                            "api.device.add",
                            Value::map().with("name", name.clone()).with("token", tok.clone()).with("scope", app.settings.new_scope.clone()),
                        );
                        app.settings.paired = Some((name, tok));
                        app.settings.new_device.clear();
                        app.settings.asked = None;
                    }
                    Err(e) => app.m.toast(format!("token: {e}"), true),
                }
            }
        });
        if let Some((name, tok)) = app.settings.paired.clone() {
            egui::Frame::new().fill(t.bg_darker).corner_radius(6).inner_margin(8).show(ui, |ui| {
                ui.label(RichText::new(format!("token for {name} (shown once):")).small());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&tok).monospace().color(t.accent));
                    if ui.small_button("copy").clicked() {
                        copy(ui, &tok);
                    }
                    if ui.small_button("done").clicked() {
                        app.settings.paired = None;
                    }
                });
            });
        }
    });
}

fn appearance(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::card(ui, &t, icon::SETTINGS, "Appearance & layout", LedState::Idle, |ui| {
        ui.horizontal(|ui| {
            ui.label("zoom");
            let mut z = app.zoom;
            if ui.add(egui::Slider::new(&mut z, 0.6..=2.0).step_by(0.05)).changed() {
                app.set_zoom(ui.ctx(), z);
            }
        });
        ui.label(
            RichText::new(format!("theme: Omarchy ({}), font: {}", if t.light { "light" } else { "dark" }, app.font_name().unwrap_or("egui default")))
                .small()
                .color(t.fg_dim),
        );
        ui.label(RichText::new("theme and font follow `omarchy theme set` / `omarchy font set` live").small().color(t.fg_dim));
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(format!("layout: {}", app.layout_name()));
            if app.settings.layout_name.is_empty() {
                app.settings.layout_name = app.layout_name();
            }
            ui.add(egui::TextEdit::singleline(&mut app.settings.layout_name).desired_width(140.0));
            let n = app.settings.layout_name.trim().to_string();
            if ui.add_enabled(!n.is_empty(), egui::Button::new(format!("save layouts/{n}.toml"))).clicked() {
                app.save_layout(&n);
            }
        });
    });
}
