//! Twitch (§11): the connection ("Connected as …"), your stream (live, ads, hype train, raids,
//! shout-outs, markers), channel point rewards as cards, polls & predictions, and moderation
//! (requests waiting for your OK, AutoMod holds, and everything that was done under Details).
//!
//! Data: `twitch.*` state (`auth.*`, `eventsub.*`, `stream.*`, `ad.*`, `poll`, `prediction`,
//! `hype_train`, `rewards`, `delay*`), `health.twitch`, signals `twitch.viewers`,
//! `twitch.chat_rate`; queries `project.files {kind: rewards}` + `project.read` (reward files).
//! Actions: `twitch.auth.{start,cancel}`, `twitch.ad.snooze`, `twitch.poll.{start,end}`,
//! `twitch.prediction.{start,lock,resolve,cancel}`, `twitch.raid`, `twitch.raid.cancel`,
//! `twitch.shoutout`, `twitch.marker`, `twitch.rewards.sync`, `twitch.delay.set`,
//! `project.write` (reward on/off), `mod.*` (via the rail helpers).

use crate::app::{App, ViewId};
use crate::views::live::nice;
use crate::views::rail::{self, grouped};
use egui::{Align, Color32, CornerRadius, Layout, RichText, Sense, Vec2};
use se_proto::Value;
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_bold, font_medium, font_mono, font_semibold, mix, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Stream,
    Rewards,
    Polls,
    Moderation,
}

const TABS: [(Tab, &str); 4] =
    [(Tab::Stream, "Stream"), (Tab::Rewards, "Channel points"), (Tab::Polls, "Polls & predictions"), (Tab::Moderation, "Moderation")];

/// Poll/prediction lengths offered (label, engine duration).
const LENGTHS: [(&str, &str); 6] =
    [("1 minute", "1m"), ("2 minutes", "2m"), ("3 minutes", "3m"), ("5 minutes", "5m"), ("10 minutes", "10m"), ("15 minutes", "15m")];

#[derive(Clone)]
struct Form {
    tab: Tab,
    poll_title: String,
    poll_choices: [String; 4],
    poll_len: usize,
    pred_title: String,
    pred_outcomes: [String; 4],
    pred_len: usize,
    raid: String,
    shoutout: String,
    marker: String,
    delay_ms: String,
    /// When the last raid was started (Cancel raid shows for 90 s).
    raid_at: f64,
    last_files: f64,
}

impl Default for Form {
    fn default() -> Self {
        Form {
            tab: Tab::default(),
            poll_title: String::new(),
            poll_choices: Default::default(),
            poll_len: 1,
            pred_title: String::new(),
            pred_outcomes: Default::default(),
            pred_len: 1,
            raid: String::new(),
            shoutout: String::new(),
            marker: String::new(),
            delay_ms: String::new(),
            raid_at: f64::NEG_INFINITY,
            last_files: -10.0,
        }
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn i(v: &Value, k: &str) -> i64 {
    v.get_path(k).and_then(Value::as_i64).unwrap_or(0)
}

fn clock(secs: i64) -> String {
    if secs < 0 {
        return "—".into();
    }
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// A labelled result bar ("Funk — 18 votes").
fn bar(ui: &mut egui::Ui, t: &Theme, frac: f32, label: &str, win: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(8), t.inset);
    let mut fill = rect;
    fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
    p.rect_filled(fill, CornerRadius::same(8), mix(t.inset, if win { t.green } else { t.accent }, 0.45));
    p.text(rect.left_center() + Vec2::new(12.0, 0.0), egui::Align2::LEFT_CENTER, label, font_medium(type_scale::BODY), t.fg);
    p.text(
        rect.right_center() - Vec2::new(12.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        format!("{:.0}%", frac * 100.0),
        font_mono(type_scale::SMALL + 0.5),
        t.text_dim,
    );
    ui.add_space(spacing::XS);
}

fn round_icon(ui: &mut egui::Ui, t: &Theme, glyph: &str, c: Color32) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::hover());
    ui.painter().circle_filled(r.center(), 22.0, mix(t.surface, c, 0.2));
    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, glyph, font(19.0), c);
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("twitch-view");
    let mut form: Form = ui.data_mut(|d| d.get_temp::<Form>(id)).unwrap_or_default();
    egui::ScrollArea::vertical().id_salt("twitch-scroll").auto_shrink([false, false]).show(ui, |ui| {
        connection(app, ui);
        ui.add_space(spacing::L);
        let labels: Vec<String> = TABS
            .iter()
            .map(|(tb, l)| {
                let n = if *tb == Tab::Moderation { rail::badge_count(app, crate::views::show::RailTab::Mod) } else { 0 };
                if n > 0 { format!("{l} ({n})") } else { l.to_string() }
            })
            .collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut idx = TABS.iter().position(|(tb, _)| *tb == form.tab).unwrap_or(0);
        if widgets::segmented(ui, &t, &mut idx, &refs) {
            form.tab = TABS[idx].0;
        }
        ui.add_space(spacing::M);
        match form.tab {
            Tab::Stream => stream_tab(app, ui, &mut form),
            Tab::Rewards => rewards(app, ui, &mut form),
            Tab::Polls => polls_tab(app, ui, &mut form),
            Tab::Moderation => moderation(app, ui),
        }
        ui.add_space(spacing::XL);
    });
    ui.data_mut(|d| d.insert_temp(id, form));
}

fn authorized(app: &App) -> bool {
    app.m.str("twitch.auth.status") == "authorized"
}

// ---- shared Twitch status (Live rail, this tab, Get started, Accounts) ----------------------------

/// What the streamer should do next about Twitch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TwitchNext {
    /// No Twitch app ID yet: Settings → Get started walks through it.
    SetUp,
    /// Ready to sign in.
    Connect,
    /// Sign-in started: type `code` on the Twitch page at `url`.
    Waiting { code: String, url: String },
    /// Signed in as `login`.
    Connected { login: String },
}

/// One plain-words Twitch status used everywhere Twitch comes up.
#[derive(Clone, Debug)]
pub struct TwitchState {
    pub title: String,
    pub body: String,
    pub tone: widgets::Tone,
    pub next: TwitchNext,
}

impl TwitchState {
    /// The one button for this state (`None` when connected).
    pub fn action(&self) -> Option<&'static str> {
        match self.next {
            TwitchNext::SetUp => Some("Set up Twitch"),
            TwitchNext::Connect => Some("Connect Twitch"),
            TwitchNext::Waiting { .. } => Some("Open Twitch"),
            TwitchNext::Connected { .. } => None,
        }
    }

    /// Health word for status badges: Working / Needs a look / Not connected.
    pub fn health(&self) -> &'static str {
        match (&self.next, self.tone) {
            (TwitchNext::Connected { .. }, widgets::Tone::Ok) => "Working",
            (TwitchNext::Connected { .. } | TwitchNext::Waiting { .. }, _) => "Needs a look",
            _ => "Not connected",
        }
    }

    pub fn connected(&self) -> bool {
        matches!(self.next, TwitchNext::Connected { .. })
    }
}

pub fn twitch_state(app: &App) -> TwitchState {
    use widgets::Tone;
    let status = app.m.str("twitch.auth.status");
    let err = app.m.str("twitch.auth.error");
    let st = |title: String, body: String, tone, next| TwitchState { title, body, tone, next };
    match status {
        "authorized" => {
            let login = app.m.str("twitch.auth.login").to_string();
            let live_updates = app.m.b("twitch.eventsub.connected");
            let failed = app.m.get("twitch.eventsub.failed").and_then(Value::as_list).is_some_and(|l| !l.is_empty());
            let (body, tone) = if live_updates && !failed {
                ("Alerts, chat, channel points and polls all work.", Tone::Ok)
            } else if live_updates {
                ("Connected, but a few things aren't coming through. See Community → Twitch → Details.", Tone::Warn)
            } else {
                ("Connected, but alerts and chat aren't coming in yet. It keeps trying by itself.", Tone::Warn)
            };
            st(format!("Connected as {login}"), body.into(), tone, TwitchNext::Connected { login })
        }
        "pending" => {
            let code = app.m.str("twitch.auth.user_code").to_string();
            let url = app.m.str("twitch.auth.verification_uri").to_string();
            let body = format!("On the Twitch page, sign in as your streaming account and type the code {code}.");
            st("Almost connected".into(), body, Tone::Warn, TwitchNext::Waiting { code, url })
        }
        _ if app.m.str("twitch.client_id").is_empty() => {
            st("Twitch isn't set up yet".into(), "Get started walks you through it in a couple of minutes.".into(), Tone::Info, TwitchNext::SetUp)
        }
        _ => {
            let mut body = "Connect so alerts, chat, channel points and polls work.".to_string();
            if !err.is_empty() {
                body = format!("Twitch said: {err}. Try connecting again.");
            }
            st("Twitch isn't connected".into(), body, if err.is_empty() { Tone::Info } else { Tone::Danger }, TwitchNext::Connect)
        }
    }
}

/// Do the state's action (the button from [`TwitchState::action`]).
pub fn twitch_do(app: &mut App, ctx: &egui::Context, state: &TwitchState) {
    match &state.next {
        TwitchNext::SetUp => app.open_view(ViewId::Setup),
        TwitchNext::Connect => app.m.action("twitch.auth.start", Value::map().with("account", "broadcaster")),
        TwitchNext::Waiting { url, .. } if !url.is_empty() => ctx.open_url(egui::OpenUrl::new_tab(url)),
        _ => {}
    }
}

// ---- connection ----------------------------------------------------------------------------------

fn connection(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let state = twitch_state(app);
    let icon_ = if state.connected() { icon::TWITCH } else { icon::UNLINK };
    if widgets::callout(ui, &t, state.tone, icon_, &state.title, &state.body, state.action()) {
        twitch_do(app, ui.ctx(), &state);
    }
    let mut do_start = false;
    let mut do_cancel = false;
    match &state.next {
        TwitchNext::Waiting { code, .. } => {
            ui.add_space(spacing::S);
            ui.horizontal(|ui| {
                egui::Frame::new().fill(t.inset).corner_radius(CornerRadius::same(8)).inner_margin(egui::Margin::symmetric(18, 8)).show(ui, |ui| {
                    ui.label(RichText::new(code).font(font_mono(26.0)).color(t.fg));
                });
                ui.add_space(spacing::S);
                widgets::hint(ui, &t, &format!("Code runs out in {}", clock(app.m.f("twitch.auth.expires_in_s") as i64)));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    do_cancel = widgets::button_ex(ui, &t, None, "Cancel", Kind::Ghost, Size::Medium, 0.0, true).clicked();
                });
            });
        }
        TwitchNext::Connected { .. } => {
            ui.add_space(spacing::XS);
            widgets::details(ui, &t, "twitch-connection-details", "Details", |ui| {
                let live_updates = app.m.b("twitch.eventsub.connected");
                let n = app.m.f("twitch.eventsub.subscriptions") as i64;
                widgets::fact(ui, &t, "Live updates", &if live_updates { format!("Working ({n} kinds of updates)") } else { "Not connected".to_string() });
                let reconnects = app.m.f("twitch.eventsub.reconnects") as i64;
                if reconnects > 0 {
                    widgets::fact(ui, &t, "Reconnects", &reconnects.to_string());
                }
                for fl in app.m.get("twitch.eventsub.failed").and_then(Value::as_list).unwrap_or(&[]) {
                    widgets::fact(ui, &t, &nice(&s(fl, "type").replace('.', " ")), s(fl, "error"));
                }
                if let Some(missing) = app.m.get("twitch.auth.missing_scopes").and_then(Value::as_list).filter(|l| !l.is_empty()) {
                    widgets::fact(ui, &t, "Missing permissions", &missing.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
                }
                let health = app.m.get("health.twitch").cloned().unwrap_or_default();
                if !s(&health, "detail").is_empty() {
                    widgets::fact(ui, &t, "Status", s(&health, "detail"));
                }
                ui.add_space(spacing::S);
                do_start = widgets::button_ex(ui, &t, None, "Connect again", Kind::Secondary, Size::Small, 0.0, true)
                    .on_hover_text("Sign in again, e.g. after changing accounts")
                    .clicked();
            });
        }
        _ => {}
    }
    if do_start {
        app.m.action("twitch.auth.start", Value::map().with("account", "broadcaster"));
    }
    if do_cancel {
        app.m.action("twitch.auth.cancel", Value::map().with("account", "broadcaster"));
    }
}

/// Lays out `n` equal-width columns.
fn columns(ui: &mut egui::Ui, n: usize, mut body: impl FnMut(&mut egui::Ui, usize)) {
    let w = ui.available_width();
    let cw = (w - spacing::L * (n - 1) as f32) / n as f32;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = spacing::L;
        for c in 0..n {
            ui.allocate_ui_with_layout(Vec2::new(cw, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(cw);
                body(ui, c);
            });
        }
    });
}

/// "Set up Twitch first…" / "Connect Twitch first…", from the shared Twitch status.
fn not_connected_words(app: &App) -> &'static str {
    match twitch_state(app).next {
        TwitchNext::SetUp => "Set up Twitch first (the button at the top of this page).",
        TwitchNext::Waiting { .. } => "Finish connecting Twitch first (the code at the top of this page).",
        _ => "Connect Twitch first (the button at the top of this page).",
    }
}

fn not_connected_hint(ui: &mut egui::Ui, t: &Theme, app: &App) {
    widgets::hint(ui, t, not_connected_words(app));
}

// ---- your stream ---------------------------------------------------------------------------------

fn stream_tab(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let n = if ui.available_width() > 1100.0 { 2 } else { 1 };
    if n == 2 {
        columns(ui, 2, |ui, c| {
            if c == 0 {
                stream_card(app, ui, form);
                ui.add_space(spacing::L);
                hype(app, ui);
            } else {
                ads(app, ui);
                ui.add_space(spacing::L);
                channel_actions(app, ui, form);
            }
        });
    } else {
        stream_card(app, ui, form);
        ui.add_space(spacing::L);
        ads(app, ui);
        ui.add_space(spacing::L);
        hype(app, ui);
        channel_actions(app, ui, form);
    }
}

fn stream_card(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    let live = app.m.b("twitch.stream.live");
    let title = app.m.str("twitch.stream.title").to_string();
    let category = app.m.str("twitch.stream.category").to_string();
    let mut set_delay: Option<i64> = None;
    widgets::titled(
        ui,
        &t,
        "Right now",
        "",
        |ui| {
            widgets::badge(ui, &t, if live { "On air" } else { "Off air" }, if live { t.bright_red } else { t.text_dim });
        },
        |ui| {
            ui.set_width(ui.available_width());
            if !title.is_empty() {
                ui.add(egui::Label::new(RichText::new(&title).font(font_medium(type_scale::LARGE)).color(t.fg)).wrap());
                if !category.is_empty() {
                    ui.label(RichText::new(&category).color(t.text_dim));
                }
                ui.add_space(spacing::M);
            }
            let started = app.m.f("twitch.stream.started_at") as i64;
            let tiles = [
                if live && started > 0 { (clock(unix_now() - started), "on air so far") } else { ("Off air".to_string(), "not streaming right now") },
                (grouped(app.m.sig("twitch.viewers").unwrap_or(0.0) as i64), "watching"),
                (format!("{:.0}", app.m.sig("twitch.chat_rate").unwrap_or(0.0)), "chat messages a minute"),
            ];
            columns(ui, 3, |ui, c| {
                let (v, l) = &tiles[c];
                egui::Frame::new().fill(t.surface_hi).corner_radius(CornerRadius::same(10)).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(v).font(font_bold(type_scale::HEADING)).color(t.fg));
                    ui.add(egui::Label::new(RichText::new(*l).size(type_scale::SMALL + 0.5).color(t.text_dim)).truncate());
                });
            });
            ui.add_space(spacing::M);
            widgets::details(ui, &t, "twitch-delay", "Chat delay", |ui| {
                let ms = app.m.f("twitch.delay_ms") as i64;
                widgets::fact(ui, &t, "Chat reaches you", &format!("{:.1} seconds after viewers see it", ms as f64 / 1000.0));
                widgets::hint(
                    ui,
                    &t,
                    &format!(
                        "Chat effects wait this long so they line up with what viewers see (your Twitch stream delay of {} seconds plus {:.1} seconds for chat to arrive).",
                        app.m.f("twitch.delay.stream_s") as i64,
                        app.m.f("twitch.delay.chat_ms") / 1000.0
                    ),
                );
                ui.horizontal(|ui| {
                    ui.add(se_ui_kit::widgets::field(&mut form.delay_ms).hint_text("Seconds, or empty to measure").desired_width(200.0));
                    if widgets::button_ex(ui, &t, None, "Set", Kind::Secondary, Size::Small, 0.0, true)
                        .on_hover_text("Leave it empty to measure it automatically")
                        .clicked()
                    {
                        set_delay = Some(form.delay_ms.trim().replace(',', ".").parse::<f64>().map(|s| (s * 1000.0).round() as i64).unwrap_or(-1));
                    }
                });
            });
        },
    );
    if let Some(ms) = set_delay {
        app.m.action("twitch.delay.set", Value::map().with("ms", ms));
    }
}

fn ads(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let next = app.m.f("twitch.ad.next_in_s") as i64;
    let soon = (0..=120).contains(&next);
    let snoozes = app.m.f("twitch.ad.snoozes") as i64;
    let running = app.m.str("show.mode") == "ad_break";
    let mut snooze = false;
    widgets::titled(
        ui,
        &t,
        "Ads",
        "Twitch runs ads on a schedule. You get a warning two minutes before.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if running {
                ui.label(RichText::new("Ad break running — chat effects wait until it's over.").font(font_medium(type_scale::BODY)).color(t.yellow));
                return;
            }
            if next < 0 {
                widgets::hint(ui, &t, if authorized(app) { "No ad scheduled right now." } else { not_connected_words(app) });
                return;
            }
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("Next ad in").color(t.text_dim));
                    ui.label(RichText::new(clock(next)).font(font_mono(type_scale::TITLE)).color(if soon { t.yellow } else { t.fg }));
                    ui.label(RichText::new(format!("{} second ad", app.m.f("twitch.ad.duration_s") as i64)).size(type_scale::SMALL + 0.5).color(t.text_dim));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let label = if snoozes > 0 { format!("Snooze 5 min ({snoozes} left)") } else { "No snoozes left".into() };
                    snooze = widgets::button_ex(
                        ui,
                        &t,
                        Some(icon::CLOCK),
                        &label,
                        if soon { Kind::Primary } else { Kind::Secondary },
                        Size::Medium,
                        0.0,
                        snoozes > 0,
                    )
                    .clicked();
                });
            });
        },
    );
    if snooze {
        app.m.action("twitch.ad.snooze", Value::Null);
    }
}

fn hype(app: &mut App, ui: &mut egui::Ui) {
    if !app.m.b("twitch.hype_train.active") {
        return;
    }
    let t = app.t.clone();
    let h = app.m.get("twitch.hype_train").cloned().unwrap_or_default();
    widgets::titled(
        ui,
        &t,
        &format!("Hype train · level {}", i(&h, "level")),
        "Your viewers are on a roll!",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            bar(
                ui,
                &t,
                app.m.f("twitch.hype_train.fraction") as f32,
                &format!("{} of {} to the next level", grouped(i(&h, "progress")), grouped(i(&h, "goal"))),
                false,
            );
        },
    );
    ui.add_space(spacing::L);
}

fn channel_actions(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    let now = ui.input(|i| i.time);
    let ok = authorized(app);
    let mut act: Option<(&'static str, Value)> = None;
    widgets::titled(
        ui,
        &t,
        "Shout-outs, raids & markers",
        "",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if !ok {
                not_connected_hint(ui, &t, app);
                ui.add_space(spacing::S);
            }
            let fw = (ui.available_width() - 190.0).clamp(140.0, 320.0);
            ui.label(RichText::new("Shout out a streamer").font(font_medium(type_scale::BODY)).color(t.fg));
            ui.horizontal(|ui| {
                ui.add(se_ui_kit::widgets::field(&mut form.shoutout).hint_text("Their channel name").desired_width(fw));
                if widgets::button_ex(ui, &t, Some(icon::HEART), "Shout out", Kind::Secondary, Size::Medium, 0.0, ok && !form.shoutout.trim().is_empty())
                    .clicked()
                {
                    act = Some(("twitch.shoutout", Value::map().with("user", form.shoutout.trim())));
                }
            });
            ui.add_space(spacing::M);
            ui.label(RichText::new("Raid a channel").font(font_medium(type_scale::BODY)).color(t.fg));
            widgets::hint(ui, &t, "Sends your viewers to them at the end of your stream.");
            ui.horizontal(|ui| {
                ui.add(se_ui_kit::widgets::field(&mut form.raid).hint_text("Their channel name").desired_width(fw));
                if ok && !form.raid.trim().is_empty() {
                    if widgets::hold_button(ui, &t, "Hold to raid", t.bright_red, 1.0) {
                        act = Some(("twitch.raid", Value::map().with("user", form.raid.trim())));
                        form.raid_at = now;
                    }
                } else {
                    widgets::button_ex(ui, &t, Some(icon::USERS), "Raid", Kind::Secondary, Size::Medium, 0.0, false)
                        .on_disabled_hover_text("Type a channel name first");
                }
                // Twitch holds a raid for 90 seconds before it goes; Cancel only makes sense then.
                if now - form.raid_at < 90.0 && widgets::button_ex(ui, &t, None, "Cancel raid", Kind::Secondary, Size::Medium, 0.0, ok).clicked() {
                    act = Some(("twitch.raid.cancel", Value::Null));
                    form.raid_at = f64::NEG_INFINITY;
                }
            });
            ui.add_space(spacing::M);
            ui.label(RichText::new("Mark this moment").font(font_medium(type_scale::BODY)).color(t.fg));
            widgets::hint(ui, &t, "Adds a marker to your Twitch video so you can find it later.");
            ui.horizontal(|ui| {
                ui.add(se_ui_kit::widgets::field(&mut form.marker).hint_text("Note (optional)").desired_width(fw));
                if widgets::button_ex(ui, &t, Some(icon::REC), "Add marker", Kind::Secondary, Size::Medium, 0.0, ok).clicked() {
                    act = Some(("twitch.marker", Value::map().with("description", form.marker.trim())));
                    form.marker.clear();
                }
            });
        },
    );
    if let Some((n, a)) = act {
        app.m.action(n, a);
    }
}

// ---- channel point rewards -----------------------------------------------------------------------

/// A reward file from the project (`rewards/<key>.toml`), parsed.
struct RewardFile {
    key: String,
    path: String,
    table: toml::Table,
}

fn reward_files(app: &mut App, form: &mut Form, now: f64) -> Vec<RewardFile> {
    if now - form.last_files > 5.0 {
        form.last_files = now;
        app.m.query_as("project.files:rewards", "project.files", Value::map().with("kind", "rewards"));
        let paths: Vec<String> = app.m.q_list("project.files:rewards").iter().map(|f| s(f, "path").to_string()).collect();
        for p in paths {
            app.m.query_as(&format!("project.read:{p}"), "project.read", Value::map().with("path", p.clone()));
        }
    }
    app.m
        .q_list("project.files:rewards")
        .iter()
        .filter_map(|f| {
            let path = s(f, "path").to_string();
            let text = app.m.q(&format!("project.read:{path}")).map(|r| s(r, "text").to_string())?;
            Some(RewardFile { key: s(f, "name").to_string(), path, table: text.parse::<toml::Table>().unwrap_or_default() })
        })
        .collect()
}

/// `5m` → `5 minutes`, `30s` → `30 seconds`, `1h` → `1 hour`.
fn span_words(d: &str) -> String {
    let d = d.trim();
    let (n, unit) = d.split_at(d.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(d.len()));
    let one = n == "1";
    let w = match unit {
        "s" | "" => "second",
        "m" => "minute",
        "h" => "hour",
        "d" => "day",
        _ => return d.to_string(),
    };
    format!("{n} {w}{}", if one { "" } else { "s" })
}

/// "Plays the Hype effect" for a reward's commands.
fn fires_words(cmds: &[String]) -> String {
    cmds.iter()
        .map(|c| {
            let head = c.split_whitespace().next().unwrap_or("");
            if let Some(p) = head.strip_prefix("preset.") {
                format!("Plays the {} effect", nice(p))
            } else if head == "queue.request" {
                "Adds the viewer's song to Up next".to_string()
            } else if let Some(sc) = head.strip_prefix("scene.") {
                format!("Switches to {}", nice(sc))
            } else {
                format!("Runs {}", nice(&head.replace('.', " ")).to_lowercase())
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn rewards(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    let now = ui.input(|i| i.time);
    let files = reward_files(app, form, now);
    let live: Vec<Value> = app.m.get("twitch.rewards").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let ok = authorized(app);
    let mut sync = false;
    ui.horizontal(|ui| {
        widgets::hint(ui, &t, "Rewards made by Stream Engine. Viewers spend channel points on them; you decide what happens.");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            sync = widgets::button_ex(ui, &t, Some(icon::UNDO), "Update on Twitch", Kind::Secondary, Size::Medium, 0.0, ok)
                .on_hover_text("Send your reward settings to Twitch")
                .clicked();
        });
    });
    ui.add_space(spacing::M);
    if files.is_empty() && live.is_empty() {
        widgets::panel(ui, &t, |ui| {
            ui.set_width(ui.available_width());
            widgets::empty_state(
                ui,
                &t,
                icon::STAR,
                "No rewards yet",
                "Rewards live in your project's rewards folder. Each one becomes a channel point reward on Twitch.",
                None,
            );
        });
    }
    // Project files first (they describe what the reward does), then Twitch-only leftovers.
    let mut cards: Vec<(Option<&RewardFile>, Option<Value>)> = files.iter().map(|f| (Some(f), live.iter().find(|r| s(r, "key") == f.key).cloned())).collect();
    for r in &live {
        if !files.iter().any(|f| f.key == s(r, "key")) {
            cards.push((None, Some(r.clone())));
        }
    }
    let w = ui.available_width();
    let cols = ((w + spacing::L) / 400.0).floor().max(1.0) as usize;
    let mut writes: Vec<(String, bool)> = Vec::new();
    for row in cards.chunks(cols) {
        columns(ui, cols, |ui, c| {
            let Some((file, state)) = row.get(c) else { return };
            if let Some(w) = reward_card(ui, &t, *file, state.as_ref(), ok) {
                writes.push(w);
            }
        });
        ui.add_space(spacing::L);
    }
    for (path, on) in writes {
        app.m.action(
            "project.write",
            Value::map().with("path", path).with("set", Value::map().with("enabled", if on { Value::Null } else { Value::Bool(false) })),
        );
        if ok {
            app.m.action("twitch.rewards.sync", Value::Null);
        }
        form.last_files = -10.0;
    }
    if sync {
        app.m.action("twitch.rewards.sync", Value::Null);
    }
}

/// One reward card. Returns `(file path, on)` when the switch was flipped.
fn reward_card(ui: &mut egui::Ui, t: &Theme, file: Option<&RewardFile>, state: Option<&Value>, connected: bool) -> Option<(String, bool)> {
    let tb = file.map(|f| &f.table);
    let tget = |k: &str| tb.and_then(|x| x.get(k));
    let title = tget("title").and_then(toml::Value::as_str).map(String::from).or_else(|| state.map(|r| s(r, "title").to_string())).unwrap_or_default();
    let cost = tget("cost").and_then(toml::Value::as_integer).or_else(|| state.map(|r| i(r, "cost"))).unwrap_or(0);
    let mut on = tget("enabled").and_then(toml::Value::as_bool).or_else(|| state.and_then(|r| r.get_path("enabled")).map(Value::truthy)).unwrap_or(true);
    let prompt = tget("prompt").and_then(toml::Value::as_str).map(String::from).or_else(|| state.map(|r| s(r, "prompt").to_string())).unwrap_or_default();
    let fires: Vec<String> = match tget("fires") {
        Some(toml::Value::String(x)) => vec![x.clone()],
        Some(toml::Value::Array(a)) => a.iter().filter_map(toml::Value::as_str).map(String::from).collect(),
        _ => Vec::new(),
    };
    let status = state.map(|r| s(r, "status").to_string()).unwrap_or_default();
    let mut flipped = None;
    widgets::panel(ui, t, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            round_icon(ui, t, icon::STAR, if on { t.accent } else { t.text_faint });
            ui.add_space(spacing::S);
            ui.vertical(|ui| {
                ui.label(RichText::new(&title).font(font_semibold(type_scale::LARGE)).color(t.fg));
                ui.label(RichText::new(format!("{} points", grouped(cost))).font(font_medium(type_scale::BODY)).color(t.accent));
            });
            if let Some(f) = file {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::toggle(ui, t, &mut on).on_hover_text(if on { "On: viewers can redeem it" } else { "Off: hidden on Twitch" }).changed() {
                        flipped = Some((f.path.clone(), on));
                    }
                });
            }
        });
        ui.add_space(spacing::M);
        if !prompt.is_empty() {
            ui.add(egui::Label::new(RichText::new(&prompt).color(t.fg)).wrap());
        }
        if !fires.is_empty() {
            ui.add(egui::Label::new(RichText::new(fires_words(&fires)).size(type_scale::SMALL + 0.5).color(t.text_dim)).wrap());
        }
        let mut limits = Vec::new();
        if let Some(c) = tget("cooldown").and_then(toml::Value::as_str) {
            limits.push(format!("Once every {} for everyone", span_words(c)));
        }
        if let Some(c) = tget("user_cooldown").and_then(toml::Value::as_str) {
            limits.push(format!("once every {} per viewer", span_words(c)));
        }
        if let Some(n) = tget("max_per_user_per_stream").and_then(toml::Value::as_integer) {
            limits.push(format!("{n} per viewer per stream"));
        }
        if tget("approval").and_then(toml::Value::as_bool) == Some(true) {
            limits.push("a mod approves each one".into());
        }
        if !limits.is_empty() {
            ui.add_space(spacing::XS);
            ui.add(egui::Label::new(RichText::new(limits.join(" · ")).size(type_scale::SMALL).color(t.text_faint)).wrap());
        }
        ui.add_space(spacing::S);
        ui.horizontal(|ui| {
            let (label, c) = if status.starts_with("error") {
                ("Twitch refused it", t.bright_red)
            } else if !status.is_empty() {
                ("On Twitch", t.green)
            } else if connected {
                ("Not on Twitch yet", t.yellow)
            } else {
                ("Connect Twitch to publish", t.text_dim)
            };
            let r = widgets::badge(ui, t, label, c);
            if status.starts_with("error") {
                r.on_hover_text(&status);
            }
        });
        widgets::details(ui, t, ("reward-details", &title), "Details", |ui| {
            if let Some(f) = file {
                widgets::fact(ui, t, "File", &f.path);
            }
            for c in &fires {
                ui.label(RichText::new(c).font(font_mono(type_scale::SMALL)).color(t.text_dim));
            }
            if !status.is_empty() {
                widgets::fact(ui, t, "Twitch status", &status);
            }
        });
    });
    flipped
}

// ---- polls & predictions -------------------------------------------------------------------------

fn polls_tab(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let n = if ui.available_width() > 1000.0 { 2 } else { 1 };
    if n == 2 {
        columns(ui, 2, |ui, c| if c == 0 { poll(app, ui, form) } else { prediction(app, ui, form) });
    } else {
        poll(app, ui, form);
        ui.add_space(spacing::L);
        prediction(app, ui, form);
    }
}

fn length_picker(ui: &mut egui::Ui, id: &str, sel: &mut usize) {
    egui::ComboBox::from_id_salt(id).selected_text(LENGTHS[*sel].0).width(160.0).show_ui(ui, |ui| {
        for (k, (l, _)) in LENGTHS.iter().enumerate() {
            ui.selectable_value(sel, k, *l);
        }
    });
}

fn poll(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    let active = app.m.b("twitch.poll.active");
    let p = app.m.get("twitch.poll").cloned().unwrap_or_default();
    let ok = authorized(app);
    let mut act: Option<(&'static str, Value)> = None;
    widgets::titled(
        ui,
        &t,
        "Poll",
        if active { "Running now" } else { "Ask your viewers a question." },
        |ui| {
            if active {
                widgets::badge(ui, &t, "Running", t.bright_red);
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            if !s(&p, "title").is_empty() {
                ui.label(RichText::new(s(&p, "title")).font(font_semibold(type_scale::LARGE)).color(t.fg));
                ui.add_space(spacing::S);
                let total = i(&p, "total_votes").max(1);
                for c in p.get_path("choices").and_then(Value::as_list).unwrap_or(&[]) {
                    let votes = i(c, "votes");
                    let win = !active && s(&p, "winner") == s(c, "id");
                    bar(
                        ui,
                        &t,
                        votes as f32 / total as f32,
                        &format!("{}{} — {} vote{}", if win { "★ " } else { "" }, s(c, "title"), votes, if votes == 1 { "" } else { "s" }),
                        win,
                    );
                }
                if active {
                    ui.add_space(spacing::S);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("Ends in {}", clock(i(&p, "ends_at") - unix_now()))).color(t.text_dim));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::button_ex(ui, &t, Some(icon::STOP), "End poll now", Kind::Secondary, Size::Medium, 0.0, true).clicked() {
                                act = Some(("twitch.poll.end", Value::Null));
                            }
                        });
                    });
                    return;
                }
                ui.add_space(spacing::M);
                ui.separator();
                ui.add_space(spacing::M);
            }
            ui.label(RichText::new("New poll").font(font_medium(type_scale::BODY)).color(t.fg));
            ui.add(se_ui_kit::widgets::field(&mut form.poll_title).hint_text("Question, e.g. Which groove next?").desired_width(f32::INFINITY));
            for (k, c) in form.poll_choices.iter_mut().enumerate() {
                ui.add(
                    se_ui_kit::widgets::field(c)
                        .hint_text(if k < 2 { format!("Choice {}", k + 1) } else { format!("Choice {} (optional)", k + 1) })
                        .desired_width(f32::INFINITY),
                );
            }
            ui.add_space(spacing::S);
            let choices: Vec<&str> = form.poll_choices.iter().map(|c| c.trim()).filter(|c| !c.is_empty()).collect();
            let ready = ok && !form.poll_title.trim().is_empty() && choices.len() >= 2;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Runs for").color(t.text_dim));
                length_picker(ui, "poll-len", &mut form.poll_len);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::button_ex(ui, &t, Some(icon::PLAY), "Start poll", Kind::Primary, Size::Medium, 0.0, ready).clicked() {
                        act = Some((
                            "twitch.poll.start",
                            Value::map().with("title", form.poll_title.trim()).with("choices", choices.join(" | ")).with("duration", LENGTHS[form.poll_len].1),
                        ));
                    }
                });
            });
            if !ok {
                not_connected_hint(ui, &t, app);
            }
        },
    );
    if let Some((n, a)) = act {
        app.m.action(n, a);
    }
}

fn prediction(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    let active = app.m.b("twitch.prediction.active");
    let p = app.m.get("twitch.prediction").cloned().unwrap_or_default();
    let ok = authorized(app);
    let locked = s(&p, "status") == "locked";
    let mut act: Option<(&'static str, Value)> = None;
    let mut cancel = false;
    widgets::titled(
        ui,
        &t,
        "Prediction",
        if !active {
            "Viewers bet channel points on what happens."
        } else if locked {
            "Betting closed — pick what happened."
        } else {
            "Viewers are betting now."
        },
        |ui| {
            if active {
                widgets::badge(ui, &t, if locked { "Waiting for the result" } else { "Betting open" }, if locked { t.yellow } else { t.bright_red });
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            if !s(&p, "title").is_empty() {
                ui.label(RichText::new(s(&p, "title")).font(font_semibold(type_scale::LARGE)).color(t.fg));
                ui.add_space(spacing::S);
                let outcomes = p.get_path("outcomes").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
                let total: i64 = outcomes.iter().map(|o| i(o, "points")).sum::<i64>().max(1);
                for o in &outcomes {
                    bar(
                        ui,
                        &t,
                        i(o, "points") as f32 / total as f32,
                        &format!("{} — {} points from {} people", s(o, "title"), grouped(i(o, "points")), i(o, "users")),
                        false,
                    );
                    if active
                        && locked
                        && widgets::button_ex(ui, &t, Some(icon::CHECK), &format!("\"{}\" happened", s(o, "title")), Kind::Secondary, Size::Small, 0.0, true)
                            .clicked()
                    {
                        act = Some(("twitch.prediction.resolve", Value::map().with("winner", s(o, "id"))));
                    }
                }
                if active {
                    ui.add_space(spacing::S);
                    ui.horizontal(|ui| {
                        if !locked && widgets::button_ex(ui, &t, Some(icon::STOP), "Close betting", Kind::Primary, Size::Medium, 0.0, true).clicked() {
                            act = Some(("twitch.prediction.lock", Value::Null));
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            cancel = widgets::hold_button(ui, &t, "Hold to cancel & refund", t.yellow, 0.8);
                        });
                    });
                    return;
                }
                ui.add_space(spacing::M);
                ui.separator();
                ui.add_space(spacing::M);
            }
            ui.label(RichText::new("New prediction").font(font_medium(type_scale::BODY)).color(t.fg));
            ui.add(se_ui_kit::widgets::field(&mut form.pred_title).hint_text("What will happen? e.g. Will I nail the solo?").desired_width(f32::INFINITY));
            for (k, c) in form.pred_outcomes.iter_mut().enumerate() {
                ui.add(
                    se_ui_kit::widgets::field(c)
                        .hint_text(if k < 2 { format!("Outcome {}", k + 1) } else { format!("Outcome {} (optional)", k + 1) })
                        .desired_width(f32::INFINITY),
                );
            }
            ui.add_space(spacing::S);
            let outcomes: Vec<&str> = form.pred_outcomes.iter().map(|c| c.trim()).filter(|c| !c.is_empty()).collect();
            let ready = ok && !form.pred_title.trim().is_empty() && outcomes.len() >= 2;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Betting open for").color(t.text_dim));
                length_picker(ui, "pred-len", &mut form.pred_len);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::button_ex(ui, &t, Some(icon::PLAY), "Start prediction", Kind::Primary, Size::Medium, 0.0, ready).clicked() {
                        act = Some((
                            "twitch.prediction.start",
                            Value::map().with("title", form.pred_title.trim()).with("outcomes", outcomes.join(" | ")).with("window", LENGTHS[form.pred_len].1),
                        ));
                    }
                });
            });
            if !ok {
                not_connected_hint(ui, &t, app);
            }
        },
    );
    if cancel {
        act = Some(("twitch.prediction.cancel", Value::Null));
    }
    if let Some((n, a)) = act {
        app.m.action(n, a);
    }
}

// ---- moderation ----------------------------------------------------------------------------------

fn moderation(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let w = ui.available_width().min(900.0);
    ui.allocate_ui_with_layout(Vec2::new(w, 0.0), Layout::top_down(Align::Min), |ui| {
        ui.set_width(w);
        widgets::titled(
            ui,
            &t,
            "Waiting for you",
            "Rewards that need a mod's OK and chat messages Twitch AutoMod is holding back.",
            |_| {},
            |ui| {
                ui.set_width(ui.available_width());
                let n = rail::approvals(app, ui) + rail::automod(app, ui);
                if n == 0 {
                    widgets::empty_state(ui, &t, icon::CHECK, "All clear", "Nothing needs your OK right now.", None);
                }
            },
        );
        ui.add_space(spacing::M);
        rail::audit(app, ui, "twitch-audit");
    });
}
