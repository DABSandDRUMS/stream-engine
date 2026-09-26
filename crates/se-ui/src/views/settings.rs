//! Settings → Accounts & app (§15.6): Twitch accounts (device-code connect), keys kept in the
//! system keyring (YouTube, tips relay; never written to project files), phones and tablets that
//! may control the engine (§19), screen setups (layouts and the program window on the TV),
//! appearance, and keyboard shortcuts (click to change; saved to `layouts/shortcuts.toml`).

use crate::app::App;
use crate::shortcuts::{self, ActionInfo};
use crate::views::live::nice;
use egui::{Align, Layout, RichText, Ui};
use se_proto::Value;
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_medium, font_mono, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};
use std::collections::HashMap;
use std::time::Instant;

/// Widest the card columns get on very large screens.
const MAX_WIDTH: f32 = f32::INFINITY;
/// Below this width the cards stack in one column.
const TWO_COLUMNS: f32 = 1100.0;
/// From this width on, three columns.
const THREE_COLUMNS: f32 = 2100.0;
/// Access levels for paired devices: (engine scope, button label, what it means).
const ACCESS: [(&str, &str, &str); 3] = [
    ("read", "Watch only", "Sees what's happening, can't change anything."),
    ("mod", "Moderate", "Can help with chat, song requests and moderation."),
    ("full", "Full control", "Can do everything you can."),
];
const YOUTUBE_KEY: &str = "youtube.api_key";
const RELAY_SECRET: &str = "relay.secret";

#[derive(Default)]
pub struct SettingsState {
    pub secret_input: HashMap<String, String>,
    pub new_device: String,
    /// Index into [`ACCESS`] for the next paired device.
    pub new_access: usize,
    /// The pairing form is open.
    pub pairing: bool,
    /// Code shown once after pairing a device: (device, code).
    pub paired: Option<(String, String)>,
    /// Relay password made here, shown once so it can be copied to the relay.
    pub relay_made: Option<String>,
    pub asked: Option<Instant>,
    pub layout_name: String,
    /// Shortcut group on screen (Everyday / Scenes / Effect buttons).
    pub keys_group: usize,
}

fn refresh(app: &mut App) {
    if app.m.connected && app.settings.asked.is_none_or(|a| a.elapsed().as_secs() >= 2) {
        app.settings.asked = Some(Instant::now());
        app.m.query("api.info", Value::Null);
        app.m.query("secrets.status", Value::Null);
    }
}

/// Random 32-byte token, hex encoded (device pairing, relay password).
pub fn random_token() -> std::io::Result<String> {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

pub fn ui(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    refresh(app);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let w = ui.available_width().min(MAX_WIDTH);
        ui.set_max_width(w);
        let n = if w >= THREE_COLUMNS {
            3
        } else if w >= TWO_COLUMNS {
            2
        } else {
            1
        };
        if n == 1 {
            accounts_column(app, ui, &t);
            devices(app, ui, &t);
            screens_column(app, ui, &t);
            shortcuts_card(app, ui, &t);
        } else {
            let gaps = ui.spacing().item_spacing;
            ui.spacing_mut().item_spacing.x = spacing::L;
            ui.columns(n, |cols| {
                for c in cols.iter_mut() {
                    c.spacing_mut().item_spacing = gaps;
                }
                accounts_column(app, &mut cols[0], &t);
                if n == 3 {
                    devices(app, &mut cols[1], &t);
                    screens_column(app, &mut cols[1], &t);
                    shortcuts_card(app, &mut cols[2], &t);
                } else {
                    devices(app, &mut cols[0], &t);
                    screens_column(app, &mut cols[1], &t);
                    shortcuts_card(app, &mut cols[1], &t);
                }
            });
        }
        ui.add_space(spacing::XL);
    });
}

/// Twitch and the keys kept in the keyring.
fn accounts_column(app: &mut App, ui: &mut Ui, t: &Theme) {
    twitch(app, ui, t);
    ui.add_space(spacing::L);
    keys(app, ui, t);
}

fn screens_column(app: &mut App, ui: &mut Ui, t: &Theme) {
    screens(app, ui, t);
    ui.add_space(spacing::L);
    appearance(app, ui, t);
    ui.add_space(spacing::L);
}

/// A raised row inside a card: icon, title, one line under it, and actions on the right.
fn item(ui: &mut Ui, t: &Theme, ic: &str, title: &str, line: &str, actions: impl FnOnce(&mut Ui)) {
    egui::Frame::new().fill(t.surface_hi).corner_radius(radius::CONTROL).inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.set_min_height(if line.is_empty() { 30.0 } else { 40.0 });
            ui.label(RichText::new(ic).size(type_scale::LARGE).color(t.text_dim));
            ui.add_space(spacing::XS);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new(title).font(font_medium(type_scale::BODY)).color(t.fg));
                if !line.is_empty() {
                    ui.label(RichText::new(line).size(type_scale::SMALL).color(t.text_dim));
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), actions);
        });
    });
}

fn offline(ui: &mut Ui, t: &Theme) {
    widgets::hint(ui, t, "Shows up when Stream Engine is running.");
}

// ---- Twitch -------------------------------------------------------------------------------------

fn twitch(app: &mut App, ui: &mut Ui, t: &Theme) {
    use crate::views::twitch::{TwitchNext, twitch_do, twitch_state};
    let st = twitch_state(app);
    widgets::titled(
        ui,
        t,
        "Twitch",
        "Alerts, chat and channel points use your Twitch account.",
        |ui| {
            let c = match st.health() {
                "Working" => t.green,
                "Needs a look" => t.yellow,
                _ => t.text_dim,
            };
            widgets::badge(ui, t, st.health(), c);
        },
        |ui| {
            ui.set_width(ui.available_width());
            if !app.m.connected {
                offline(ui, t);
                return;
            }
            if st.connected() {
                account(app, ui, t, "Your channel", "The account you stream from.", "twitch.auth", "broadcaster");
            } else {
                if widgets::callout(ui, t, st.tone, icon::TWITCH, &st.title, &st.body, st.action()) {
                    twitch_do(app, ui.ctx(), &st);
                }
                if matches!(st.next, TwitchNext::Waiting { .. }) {
                    device_code(app, ui, t, "twitch.auth", "your streaming account");
                }
            }
            if st.next != TwitchNext::SetUp {
                ui.add_space(spacing::S);
                account(app, ui, t, "Chat bot", "Optional: a second account that answers in chat for you.", "twitch.auth.bot", "bot");
            }
        },
    );
}

/// `twitch.auth[.bot].status`: none | pending | authorized | expired | error.
fn account(app: &mut App, ui: &mut Ui, t: &Theme, title: &str, blurb: &str, p: &str, who: &str) {
    let status = app.m.str(&format!("{p}.status")).to_string();
    let login = app.m.str(&format!("{p}.login")).to_string();
    let args = Value::map().with("account", who);
    let line = match status.as_str() {
        "authorized" if !login.is_empty() => format!("Connected as {login}"),
        "authorized" => "Connected".to_string(),
        "pending" => "Waiting for you to approve on Twitch".to_string(),
        "expired" => "The connection ran out. Connect again.".to_string(),
        "error" => "That didn't work. Try connecting again.".to_string(),
        _ => blurb.to_string(),
    };
    let main = who == "broadcaster";
    item(ui, t, icon::TWITCH, title, &line, |ui| match status.as_str() {
        "authorized" => {
            if widgets::hold_button(ui, t, "Disconnect", t.bright_red, 0.8) {
                app.m.action("twitch.auth.logout", args.clone());
            }
            widgets::badge(ui, t, "Working", t.green);
        }
        "pending" => {
            if widgets::button(ui, t, "Cancel", Kind::Ghost).clicked() {
                app.m.action("twitch.auth.cancel", args.clone());
            }
            widgets::badge(ui, t, "Needs a look", t.yellow);
        }
        _ => {
            let kind = if main { Kind::Primary } else { Kind::Secondary };
            if widgets::button_ex(ui, t, Some(icon::TWITCH), "Connect", kind, Size::Medium, 0.0, true).clicked() {
                app.m.action("twitch.auth.start", args.clone());
            }
        }
    });
    if status == "pending" {
        device_code(app, ui, t, p, if main { "your streaming account" } else { "your bot account" });
    }
    let err = app.m.str(&format!("{p}.error")).to_string();
    if !err.is_empty() && status != "authorized" {
        ui.add_space(spacing::XS);
        ui.label(RichText::new(format!("{} Twitch said: {err}", icon::WARN)).size(type_scale::SMALL).color(t.bright_red));
    }
    let missing: Vec<String> = app
        .m
        .get(&format!("{p}.missing_scopes"))
        .and_then(Value::as_list)
        .map(|l| l.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if !missing.is_empty() {
        ui.add_space(spacing::XS);
        ui.label(
            RichText::new(format!("{} Some permissions are missing, so a few features won't work. Disconnect and connect again to fix it.", icon::WARN))
                .size(type_scale::SMALL)
                .color(t.yellow),
        );
        widgets::details(ui, t, ("scopes", p), "Details", |ui| {
            widgets::hint(ui, t, &format!("Missing Twitch permissions: {}", missing.join(", ")));
        });
    }
}

fn device_code(app: &App, ui: &mut Ui, t: &Theme, p: &str, who: &str) {
    let code = app.m.str(&format!("{p}.user_code")).to_string();
    let uri = app.m.str(&format!("{p}.verification_uri")).to_string();
    if code.is_empty() {
        return;
    }
    ui.add_space(spacing::S);
    ui.horizontal(|ui| {
        egui::Frame::new().fill(t.surface_hi).corner_radius(radius::CONTROL).inner_margin(egui::Margin::symmetric(18, 8)).show(ui, |ui| {
            ui.label(RichText::new(&code).font(font_mono(26.0)).color(t.fg));
        });
        ui.add_space(spacing::S);
        if widgets::button_ex(ui, t, Some(icon::TWITCH), "Open Twitch", Kind::Primary, Size::Large, 0.0, !uri.is_empty()).clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(&uri));
        }
        if widgets::button_ex(ui, t, Some(icon::COPY), "Copy code", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
            ui.ctx().copy_text(code.clone());
        }
    });
    let exp = app.m.f(&format!("{p}.expires_in_s"));
    let tail = if exp > 0.0 { format!(" The code works for {:.0} more seconds.", exp) } else { String::new() };
    widgets::hint(ui, t, &format!("Sign in as {who} on the Twitch page and type this code.{tail}"));
}

// ---- keys (keyring) -----------------------------------------------------------------------------

fn keys(app: &mut App, ui: &mut Ui, t: &Theme) {
    let rows = app.m.q_list("secrets.status").to_vec();
    let find = |name: &str| rows.iter().find(|r| r.get_path("name").and_then(Value::as_str) == Some(name));
    key_card(
        app,
        ui,
        t,
        YOUTUBE_KEY,
        find(YOUTUBE_KEY),
        "Song requests from YouTube",
        "Viewers can request YouTube songs with !sr. That needs a free YouTube key.",
        Some(Help {
            steps: &[
                "Open Google's page and sign in with any Google account.",
                "Make a project (any name), then search for \"YouTube Data API v3\" and turn it on.",
                "Go to Credentials, choose Create credentials → API key, and copy it.",
                "Paste it above and press Save.",
            ],
            button: "Open Google's page",
            url: "https://console.cloud.google.com/apis/library/youtube.googleapis.com",
        }),
    );
    key_card(
        app,
        ui,
        t,
        RELAY_SECRET,
        find(RELAY_SECRET),
        "Tips & web helper password",
        "Your web helper is a small free website that brings Ko-fi tips, a public song list and mods' browsers to Stream Engine. This password keeps it yours.",
        Some(Help {
            steps: &[
                "Make a free Cloudflare account.",
                "Put the web helper online with the guide that came with Stream Engine (it takes about ten minutes).",
                "Press Make one for me here, and use the same password when the guide asks for it.",
            ],
            button: "Open Cloudflare",
            url: "https://dash.cloudflare.com/sign-up",
        }),
    );
    mods(app, ui, t);
    // keys the engine knows about that this page has no words for yet
    for r in rows.iter().filter(|r| !matches!(r.get_path("name").and_then(Value::as_str), Some(YOUTUBE_KEY | RELAY_SECRET))) {
        let name = r.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
        let label = r.get_path("label").and_then(Value::as_str).map(String::from).unwrap_or_else(|| nice(&name));
        key_card(app, ui, t, &name, Some(r), &label, "Kept safely in your system keyring.", None);
    }
}

/// "Show me how": numbered steps and a button that opens the right web page.
struct Help {
    steps: &'static [&'static str],
    button: &'static str,
    url: &'static str,
}

/// One keyring secret: saved or not, paste to set or replace, hold to remove.
#[allow(clippy::too_many_arguments)]
fn key_card(app: &mut App, ui: &mut Ui, t: &Theme, name: &str, row: Option<&Value>, title: &str, blurb: &str, help: Option<Help>) {
    let set = row.and_then(|r| r.get_path("set")).is_some_and(Value::truthy);
    let err = row.and_then(|r| r.get_path("error")).and_then(Value::as_str).unwrap_or("").to_string();
    widgets::titled(
        ui,
        t,
        title,
        "",
        |ui| match (row.is_some(), set) {
            (false, _) => {}
            (true, true) => {
                widgets::badge(ui, t, "Saved", t.green);
            }
            (true, false) => {
                widgets::badge(ui, t, "Not set", t.text_dim);
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ui.add(egui::Label::new(RichText::new(blurb).color(t.text_dim)).wrap());
            });
            ui.add_space(spacing::S);
            if row.is_none() {
                offline(ui, t);
                return;
            }
            let mut save = None;
            ui.horizontal(|ui| {
                let buf = app.settings.secret_input.entry(name.to_string()).or_default();
                let hint = if set { "Paste a new one to replace it" } else { "Paste it here" };
                let w = (ui.available_width() - 200.0).clamp(160.0, 420.0);
                let r = ui.add(se_ui_kit::widgets::field(buf).password(true).hint_text(hint).desired_width(w));
                let val = buf.trim().to_string();
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let kind = if set { Kind::Secondary } else { Kind::Primary };
                if (widgets::button_ex(ui, t, None, "Save", kind, Size::Medium, 0.0, !val.is_empty()).clicked() || enter) && !val.is_empty() {
                    save = Some(val);
                }
                if name == RELAY_SECRET && !set && widgets::button_ex(ui, t, None, "Make one for me", Kind::Secondary, Size::Medium, 0.0, true).clicked() {
                    match random_token() {
                        Ok(tok) => {
                            app.settings.relay_made = Some(tok.clone());
                            save = Some(tok);
                        }
                        Err(e) => app.m.toast(format!("Couldn't make a password: {e}"), true),
                    }
                }
                if set {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::hold_button(ui, t, "Remove", t.bright_red, 0.8) {
                            app.m.action("secrets.delete", Value::map().with("name", name));
                            app.settings.asked = None;
                        }
                    });
                }
            });
            if let Some(value) = save {
                app.m.action("secrets.set", Value::map().with("name", name).with("value", value));
                app.settings.secret_input.remove(name);
                app.settings.asked = None;
            }
            if name == RELAY_SECRET
                && let Some(made) = app.settings.relay_made.clone()
            {
                ui.add_space(spacing::S);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&made).font(font_mono(type_scale::SMALL)).color(t.accent));
                    if widgets::button_ex(ui, t, Some(icon::COPY), "Copy", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                        ui.ctx().copy_text(made.clone());
                    }
                    if widgets::button_ex(ui, t, None, "Done", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                        app.settings.relay_made = None;
                    }
                });
                widgets::hint(ui, t, "Saved. Copy it now and use it when you put the web helper online; it isn't shown again.");
            }
            if !err.is_empty() {
                ui.label(
                    RichText::new(format!("{} Couldn't reach your system keyring. Unlock it and try again.", icon::WARN))
                        .size(type_scale::SMALL)
                        .color(t.bright_red),
                );
                widgets::details(ui, t, ("keyring", name), "Details", |ui| {
                    widgets::hint(ui, t, &err);
                });
            }
            if let Some(h) = &help {
                ui.add_space(spacing::XS);
                widgets::details(ui, t, ("how", name), "Show me how", |ui| {
                    for (i, step) in h.steps.iter().enumerate() {
                        ui.add(egui::Label::new(RichText::new(format!("{}. {step}", i + 1)).color(t.fg)).wrap());
                    }
                    ui.add_space(spacing::XS);
                    if widgets::button_ex(ui, t, Some(icon::LINK), h.button, Kind::Secondary, Size::Small, 0.0, true).clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(h.url));
                    }
                });
            }
        },
    );
    ui.add_space(spacing::L);
}

// ---- phones & tablets (API devices) -------------------------------------------------------------

fn access_words(scope: &str) -> (&'static str, &'static str) {
    ACCESS.iter().find(|(s, _, _)| *s == scope).map(|(_, l, d)| (*l, *d)).unwrap_or(("Limited access", ""))
}

fn unpair_row(app: &mut App, ui: &mut Ui, t: &Theme, ic: &str, name: &str, title: &str, line: &str) {
    item(ui, t, ic, title, line, |ui| {
        if widgets::hold_button(ui, t, "Unpair", t.bright_red, 0.8) {
            app.m.action("api.device.remove", Value::map().with("name", name));
            app.settings.asked = None;
        }
    });
    ui.add_space(spacing::XS);
}

fn devices(app: &mut App, ui: &mut Ui, t: &Theme) {
    widgets::titled(
        ui,
        t,
        "Phones & tablets",
        "Let a phone or tablet control Stream Engine.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let Some(info) = app.m.q("api.info").cloned() else {
                offline(ui, t);
                return;
            };
            // Paired phones/tablets have a person-level access; the rest are the browser
            // overlay pages (`web.*`, limited to one overlay), which only the details list.
            let (people, overlays): (Vec<Value>, Vec<Value>) = info
                .get_path("devices")
                .and_then(Value::as_list)
                .unwrap_or(&[])
                .iter()
                .cloned()
                .partition(|d| ACCESS.iter().any(|(s, _, _)| Some(*s) == d.get_path("scope").and_then(Value::as_str)));
            if let Some((name, code)) = app.settings.paired.clone() {
                pairing_code(app, ui, t, &info, &name, &code);
            } else if people.is_empty()
                && !app.settings.pairing
                && widgets::empty_state(
                    ui,
                    t,
                    icon::TALL,
                    "No phones or tablets yet",
                    "Pair one to run scenes, lights and chat from the couch.",
                    Some("Pair a phone or tablet"),
                )
            {
                app.settings.pairing = true;
            }
            for d in &people {
                let name = d.get_path("name").and_then(Value::as_str).unwrap_or("");
                let (label, what) = access_words(d.get_path("scope").and_then(Value::as_str).unwrap_or(""));
                unpair_row(app, ui, t, icon::TALL, name, name, &format!("{label}: {what}"));
            }
            if app.settings.pairing {
                pair_form(app, ui, t);
            } else if app.settings.paired.is_none() && !people.is_empty() {
                ui.add_space(spacing::XS);
                if widgets::button_ex(ui, t, Some(icon::PLUS), "Pair another", Kind::Secondary, Size::Medium, 0.0, true).clicked() {
                    app.settings.pairing = true;
                }
            }
            ui.add_space(spacing::S);
            widgets::details(ui, t, "api-details", "Details", |ui| {
                api_details(app, ui, t, &info);
                if !overlays.is_empty() {
                    ui.add_space(spacing::M);
                    ui.label(RichText::new("Overlay pages").font(font_medium(type_scale::BODY)).color(t.fg));
                    widgets::hint(ui, t, "Browser overlays in OBS use these. Each one can only reach its own overlay.");
                    ui.add_space(spacing::XS);
                    for d in &overlays {
                        let name = d.get_path("name").and_then(Value::as_str).unwrap_or("");
                        unpair_row(app, ui, t, icon::IMAGE, name, &nice(name.trim_start_matches("web.")), "");
                    }
                }
            });
        },
    );
    ui.add_space(spacing::L);
}

fn pair_form(app: &mut App, ui: &mut Ui, t: &Theme) {
    ui.add_space(spacing::S);
    egui::Frame::new().fill(t.surface_hi).corner_radius(radius::CONTROL).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new("Name").font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
        ui.add(se_ui_kit::widgets::field(&mut app.settings.new_device).hint_text("e.g. Ben's phone").desired_width(280.0));
        ui.add_space(spacing::S);
        ui.label(RichText::new("What can it do?").font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
        let labels: Vec<&str> = ACCESS.iter().map(|(_, l, _)| *l).collect();
        widgets::segmented(ui, t, &mut app.settings.new_access, &labels);
        widgets::hint(ui, t, ACCESS[app.settings.new_access.min(ACCESS.len() - 1)].2);
        ui.add_space(spacing::M);
        let name = app.settings.new_device.trim().to_string();
        ui.horizontal(|ui| {
            if widgets::button_ex(ui, t, Some(icon::LINK), "Pair", Kind::Primary, Size::Medium, 0.0, !name.is_empty()).clicked() {
                match random_token() {
                    Ok(tok) => {
                        let scope = ACCESS[app.settings.new_access.min(ACCESS.len() - 1)].0;
                        app.m.action("api.device.add", Value::map().with("name", name.clone()).with("token", tok.clone()).with("scope", scope));
                        app.settings.paired = Some((name.clone(), tok));
                        app.settings.new_device.clear();
                        app.settings.pairing = false;
                        app.settings.asked = None;
                    }
                    Err(e) => app.m.toast(format!("Couldn't make a pairing code: {e}"), true),
                }
            }
            if widgets::button(ui, t, "Cancel", Kind::Ghost).clicked() {
                app.settings.pairing = false;
            }
        });
    });
}

fn pairing_code(app: &mut App, ui: &mut Ui, t: &Theme, info: &Value, name: &str, code: &str) {
    egui::Frame::new().fill(t.surface_hi).corner_radius(radius::CONTROL).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(format!("{name} is paired")).font(font_medium(type_scale::BODY)).color(t.fg));
        widgets::hint(ui, t, "Type this code into the app on the device. It's shown only once.");
        ui.add_space(spacing::S);
        ui.add(egui::Label::new(RichText::new(code).font(font_mono(type_scale::SMALL + 0.5)).color(t.accent)).wrap());
        let addr = info.get_path("http").and_then(Value::as_str).unwrap_or("");
        if !addr.is_empty() {
            widgets::hint(ui, t, &format!("Address to connect to: {addr}"));
        }
        ui.add_space(spacing::S);
        ui.horizontal(|ui| {
            if widgets::button_ex(ui, t, Some(icon::COPY), "Copy code", Kind::Secondary, Size::Medium, 0.0, true).clicked() {
                ui.ctx().copy_text(code.to_string());
            }
            if widgets::button(ui, t, "Done", Kind::Primary).clicked() {
                app.settings.paired = None;
            }
        });
    });
    ui.add_space(spacing::S);
}

fn api_details(app: &mut App, ui: &mut Ui, t: &Theme, info: &Value) {
    let g = |k: &str| info.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
    for (label, v) in [("This computer only", g("socket")), ("Web address", g("http")), ("Live updates", g("ws")), ("OSC", g("osc"))] {
        widgets::fact(ui, t, label, &v);
    }
    let set = info.get_path("token_set").is_some_and(Value::truthy);
    widgets::fact(ui, t, "Master key", if set { "Saved in your system keyring (never sent to devices)" } else { "Not made yet" });
    ui.add_space(spacing::XS);
    ui.horizontal(|ui| {
        if widgets::button_ex(ui, t, Some(icon::COPY), "Copy master key", Kind::Secondary, Size::Small, 0.0, true)
            .on_hover_text("Runs `streamctl token` on this computer")
            .clicked()
        {
            match std::process::Command::new("streamctl").arg("token").output() {
                Ok(o) if o.status.success() => {
                    ui.ctx().copy_text(String::from_utf8_lossy(&o.stdout).trim().to_string());
                    app.m.toast("Master key copied", false);
                }
                Ok(o) => app.m.toast(format!("Couldn't read the master key: {}", String::from_utf8_lossy(&o.stderr).trim()), true),
                Err(e) => app.m.toast(format!("Couldn't read the master key: {e}"), true),
            }
        }
        if widgets::hold_button(ui, t, "Make a new master key", t.yellow, 0.8) {
            app.m.action("api.token.rotate", Value::Null);
            app.settings.asked = None;
        }
    });
    widgets::hint(ui, t, "A new master key signs out every tool that used the old one.");
}

// ---- screens (layouts) ---------------------------------------------------------------------------

/// Plain words for a layout: (icon, title, one line).
fn layout_words(name: &str) -> (&'static str, String, &'static str) {
    match name {
        "single" => (icon::WIDE, "One window".into(), "Everything in this window. Best with one screen."),
        "show-3disp" => (icon::GRID, "Three screens".into(), "Controls here, camera previews on the side screen, what viewers see on the TV."),
        "show-2disp" => (icon::LAYERS, "Two screens".into(), "Controls here; camera previews and what viewers see share the side screen."),
        other => (icon::SAVE, nice(other), "A setup you saved."),
    }
}

fn screens(app: &mut App, ui: &mut Ui, t: &Theme) {
    widgets::titled(
        ui,
        t,
        "Screens",
        "Where Stream Engine's windows go.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let chosen = app.win.chosen.clone();
            let mut names = app.layout_names();
            // one, two, three screens, then the ones you saved
            let rank = |n: &str| ["single", "show-2disp", "show-3disp"].iter().position(|b| *b == n).unwrap_or(3);
            names.sort_by_key(|n| rank(n));
            for n in names {
                let (ic, title, line) = layout_words(&n);
                let on = n == chosen;
                if widgets::list_row(ui, t, ic, &title, line, if on { "In use" } else { "" }, on).clicked() && !on {
                    app.switch_layout(&n);
                }
            }
            if app.win.effective != chosen {
                let (_, now, _) = layout_words(&app.win.effective);
                ui.add_space(spacing::XS);
                widgets::hint(ui, t, &format!("The TV is off, so {now} is used until it's back."));
            }
            ui.add_space(spacing::M);
            let mut tv = app.win.set.get(&chosen).is_some_and(|l| l.confidence.enabled);
            if widgets::toggle_row(ui, t, "Show what viewers see on the TV", "A full-screen window with exactly what goes out.", &mut tv).changed() {
                set_program_window(app, &chosen, tv);
            }
            ui.add_space(spacing::S);
            widgets::details(ui, t, "save-layout", "Save this arrangement", |ui| {
                widgets::hint(ui, t, "Keeps the current page, size and separate windows as a setup you can switch to.");
                if app.settings.layout_name.is_empty() {
                    app.settings.layout_name = chosen.clone();
                }
                ui.horizontal(|ui| {
                    ui.add(se_ui_kit::widgets::field(&mut app.settings.layout_name).hint_text("Name").desired_width(200.0));
                    let n = app.settings.layout_name.trim().to_string();
                    if widgets::button_ex(ui, t, Some(icon::SAVE), "Save", Kind::Secondary, Size::Medium, 0.0, !n.is_empty()).clicked() {
                        app.save_layout(&n);
                    }
                });
            });
        },
    );
}

/// Turn the program (confidence) window of `layout` on or off, now and in its layout file.
fn set_program_window(app: &mut App, layout: &str, on: bool) {
    let Some(mut l) = app.win.set.get(layout).cloned() else { return };
    l.confidence.enabled = on;
    let path = format!("layouts/{}.toml", crate::views::modulate::sanitize(layout));
    let existing = app.m.q(&format!("project.read:{path}")).and_then(|v| v.get_path("text")).and_then(Value::as_str).map(String::from);
    let text = l.to_toml(existing.as_deref());
    app.m.action("project.write", Value::map().with("path", path).with("text", text));
    app.win.set.insert(l);
}

// ---- appearance ----------------------------------------------------------------------------------

fn appearance(app: &mut App, ui: &mut Ui, t: &Theme) {
    widgets::titled(
        ui,
        t,
        "Appearance",
        "Colors and the font follow your Omarchy theme.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Size of text and controls").font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
            ui.horizontal(|ui| {
                let mut pct = app.zoom * 100.0;
                ui.spacing_mut().slider_width = (ui.available_width() - 190.0).clamp(160.0, 420.0);
                let r = ui.add(egui::Slider::new(&mut pct, 60.0..=200.0).step_by(5.0).custom_formatter(|v, _| format!("{v:.0}%")));
                if r.changed() {
                    app.set_zoom(ui.ctx(), pct / 100.0);
                }
                if (app.zoom - 1.0).abs() > 0.001 && widgets::button(ui, t, "Reset", Kind::Ghost).clicked() {
                    app.set_zoom(ui.ctx(), 1.0);
                }
            });
            ui.add_space(spacing::S);
            let mut reduced = app.preferences.reduce_motion;
            if widgets::toggle_row(ui, t, "Reduce motion", "Turn off animated movement in controls and menus.", &mut reduced).changed() {
                app.set_reduced_motion(ui.ctx(), reduced);
            }
            widgets::details(ui, t, "appearance-details", "Details", |ui| {
                widgets::fact(ui, t, "Theme", if t.light { "Light" } else { "Dark" });
                widgets::fact(ui, t, "Numbers font", app.font_name().unwrap_or("Built-in"));
                let (bigger, smaller) = (app.keys_label("zoom.in"), app.keys_label("zoom.out"));
                if !bigger.is_empty() && !smaller.is_empty() {
                    widgets::fact(ui, t, "Shortcut", &format!("{bigger} bigger, {smaller} smaller"));
                }
            });
        },
    );
}

// ---- keyboard shortcuts --------------------------------------------------------------------------

/// Plain words for a shortcut (falls back to the action's own description).
fn key_words(a: &ActionInfo) -> &'static str {
    match a.id {
        "palette" => "Find anything",
        "mode.toggle" => "Jump between Overview and Edit",
        "take" => "Put the preview on air",
        "undo" => "Undo",
        "redo" => "Redo",
        "clean" => "Clear chat effects",
        "panic" => "Panic: stop everything (hold)",
        "layout.next" => "Next screen setup",
        "search" => "Search",
        "zoom.in" => "Make everything bigger",
        "zoom.out" => "Make everything smaller",
        "escape" => "Cancel or close",
        _ => a.desc,
    }
}

/// "Ctrl+K is already bound to Command palette (palette)" → "Ctrl+K is already used for Find anything."
fn friendly_error(e: &str) -> String {
    let Some((chord, rest)) = e.split_once(" is already bound to ") else { return e.to_string() };
    let other = rest.rsplit_once(" (").map(|(_, id)| id.trim_end_matches(')')).and_then(shortcuts::action).map(key_words).unwrap_or(rest);
    format!("{chord} is already used for “{other}”. Change that one first.")
}

fn shortcuts_card(app: &mut App, ui: &mut Ui, t: &Theme) {
    // the header's segmented control runs before the body; the cell hands its choice over
    let group = std::cell::Cell::new(app.settings.keys_group);
    widgets::titled(
        ui,
        t,
        "Keyboard shortcuts",
        "Click a key to change it.",
        |ui| {
            let mut g = group.get();
            widgets::segmented(ui, t, &mut g, &["Everyday", "Scenes", "Effect buttons"]);
            group.set(g);
        },
        |ui| {
            ui.set_width(ui.available_width());
            if let Some(e) = app.keys.error.clone() {
                ui.label(RichText::new(format!("{} {}", icon::WARN, friendly_error(&e))).color(t.bright_red));
                ui.add_space(spacing::S);
            }
            match group.get() {
                0 => {
                    for a in shortcuts::ACTIONS.iter().filter(|a| !a.id.starts_with("scene.") && !a.id.starts_with("pad.")) {
                        key_row(app, ui, t, key_words(a), &[a.id]);
                    }
                }
                1 => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Press a number to pick a scene.").size(type_scale::SMALL).color(t.text_dim));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            reset_slot(ui, |_| {});
                            for head in ["Straight on air", "To preview"] {
                                ui.add_space(spacing::S);
                                ui.allocate_ui_with_layout(egui::vec2(KEY_W, 18.0), Layout::left_to_right(Align::Center), |ui| {
                                    ui.set_min_width(KEY_W);
                                    ui.label(RichText::new(head).size(type_scale::SMALL).color(t.text_dim));
                                });
                            }
                        });
                    });
                    for n in 1..=9 {
                        let (pv, pg) = (format!("scene.preview.{n}"), format!("scene.program.{n}"));
                        let ids: Vec<&'static str> = [pv, pg].iter().filter_map(|id| shortcuts::action(id).map(|a| a.id)).collect();
                        key_row(app, ui, t, &format!("Scene {n}"), &ids);
                    }
                }
                _ => {
                    widgets::hint(ui, t, "The quick effect buttons on Overview, in order.");
                    ui.add_space(spacing::XS);
                    for a in shortcuts::ACTIONS.iter().filter(|a| a.id.starts_with("pad.")) {
                        key_row(app, ui, t, &format!("Effect button {}", a.id.trim_start_matches("pad.")), &[a.id]);
                    }
                }
            }
        },
    );
    app.settings.keys_group = group.get();
}

const KEY_W: f32 = 132.0;
const RESET_W: f32 = 36.0;

/// Fixed-width slot at the right end of a shortcut row (holds the "back to usual" button).
fn reset_slot(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(egui::vec2(RESET_W, 28.0), Layout::right_to_left(Align::Center), |ui| {
        ui.set_min_width(RESET_W);
        add(ui);
    });
}

/// One shortcut row: words on the left, one key chip per action on the right.
fn key_row(app: &mut App, ui: &mut Ui, t: &Theme, words: &str, ids: &[&'static str]) {
    ui.horizontal(|ui| {
        ui.set_min_height(38.0);
        ui.label(RichText::new(words).color(t.fg));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let changed = ids.iter().any(|id| !app.keys.map.is_default(id));
            reset_slot(ui, |ui| {
                if changed && widgets::icon_button(ui, t, icon::UNDO, "Back to the usual key").clicked() {
                    for id in ids {
                        if let Err(e) = app.keys.map.reset(id) {
                            app.keys.error = Some(e.to_string());
                        }
                    }
                    crate::shortcuts_ui::save(app);
                }
            });
            for id in ids.iter().rev() {
                ui.add_space(spacing::S);
                key_chip(app, ui, t, id);
            }
        });
    });
}

fn key_chip(app: &mut App, ui: &mut Ui, t: &Theme, id: &'static str) {
    let waiting = app.keys.rebinding == Some(id);
    let label = app.keys.map.label(id);
    let (text, kind) = if waiting {
        ("Press a key…".to_string(), Kind::Primary)
    } else if label.is_empty() {
        ("Not set".to_string(), Kind::Ghost)
    } else {
        (label, Kind::Secondary)
    };
    let r = widgets::button_ex(ui, t, None, &text, kind, Size::Small, KEY_W, true);
    let r = r.on_hover_text(if waiting { "Press the new key now. Esc cancels." } else { "Click to change. Right-click to remove." });
    if r.clicked() {
        app.keys.rebinding = if waiting { None } else { Some(id) };
        app.keys.error = None;
    }
    r.context_menu(|ui| {
        if ui.button("Remove this shortcut").clicked() {
            app.keys.map.unbind(id);
            crate::shortcuts_ui::save(app);
            ui.close();
        }
        if !app.keys.map.is_default(id) && ui.button("Back to the usual key").clicked() {
            match app.keys.map.reset(id) {
                Ok(()) => crate::shortcuts_ui::save(app),
                Err(e) => app.keys.error = Some(e.to_string()),
            }
            ui.close();
        }
    });
}

// ---- mods helping from their browser -------------------------------------------------------------

fn mods(app: &mut App, ui: &mut Ui, t: &Theme) {
    let on = app.m.b("remote_mod.enabled");
    let active: Vec<String> =
        app.m.get("remote_mod.active").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|v| v.as_str().map(String::from)).collect();
    widgets::titled(
        ui,
        t,
        "Mods helping from their browser",
        "",
        |ui| {
            if on {
                widgets::badge(ui, t, "Working", t.green);
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ui.add(
                    egui::Label::new(RichText::new("Your moderators sign in with Twitch on your web helper and help with chat and songs.").color(t.text_dim))
                        .wrap(),
                );
            });
            ui.add_space(spacing::S);
            let mut want = on;
            if widgets::toggle_row(ui, t, "Let mods help from their browser", "Needs the web helper password above.", &mut want).changed() {
                app.m.action("project.write", Value::map().with("path", "project.toml").with("set", Value::map().with("remote_mod.enabled", want)));
            }
            if on {
                ui.add_space(spacing::S);
                if active.is_empty() {
                    widgets::hint(ui, t, "No mods have been active in the last 30 minutes.");
                } else {
                    ui.label(RichText::new(format!("Helping right now: {}", active.join(", "))).color(t.fg));
                }
            }
        },
    );
    ui.add_space(spacing::L);
}
