//! Alerts & goals (§14.1–14.2). One card per alert (Follow, Sub, Gift subs, Cheer, Raid, Tip…)
//! with an on/off switch, a big Test button and friendly settings; goals as progress cards; the
//! stream's numbers in words; alert timing. The right column shows what's on screen, what's
//! waiting (with the mod-skip countdown) and what was shown recently.
//! Data: queries `alerts`, `alerts.config`; state `alerts.*`, `stats.*`, `goals.*`;
//! commands: `alerts.veto|approve|skip|pause|resume|clear|replay`, `alerts.edit|add|remove|policy`,
//! `goals.set|add|reset|save|delete`, `stats.purge_simulated`, `sim.*`.

use crate::app::App;
use crate::views::live::nice;
use crate::views::rail::money;
use egui::{Align, Color32, CornerRadius, Layout, RichText, Sense, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_bold, font_medium, font_mono, font_semibold, mix, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Alerts,
    Goals,
    Stats,
    Timing,
}

const TABS: [(Tab, &str); 4] = [(Tab::Alerts, "Alerts"), (Tab::Goals, "Goals"), (Tab::Stats, "This stream"), (Tab::Timing, "Settings")];

/// Every look field an alert or version can set (Details shows the technical ones).
const LOOK: [&str; 9] = ["title", "message", "sound", "duration", "priority", "tts", "tts_text", "voice", "do"];

#[derive(Clone, Default)]
struct Form {
    tab: Tab,
    /// Alert being edited (`file|index|`), `None` = the card grid.
    editing: Option<String>,
    /// `alerts.config` edits keyed by `file|path|field` (as typed).
    edits: BTreeMap<String, String>,
    policy: BTreeMap<String, String>,
    adding: bool,
    new_alert_file: String,
    new_alert_name: String,
    new_alert_when: String,
    new_var: BTreeMap<String, (String, String)>,
    /// New goal: (label, counts, target).
    goal_new: (String, String, String),
    goal_adding: bool,
    goal_open: Option<String>,
    goal_set: BTreeMap<String, String>,
    /// Every sound any alert uses (the Sound picker's choices).
    sounds: Vec<String>,
    last_query: f64,
    last_slow: f64,
}

fn act(app: &mut App, name: &str, args: Value) {
    app.m.command(Op::Action { name: name.into(), args });
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn i(v: &Value, k: &str) -> i64 {
    v.get_path(k).and_then(Value::as_i64).unwrap_or(0)
}

fn f(v: &Value, k: &str) -> f64 {
    v.get_path(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 { format!("{}", x as i64) } else { format!("{x:.2}") }
}

/// Numbers people read: `12,500`, `2.50`.
fn show_num(x: f64) -> String {
    if x.fract() == 0.0 { crate::views::rail::grouped(x as i64) } else { format!("{x:.2}") }
}

/// Slider label for seconds: `1 second`, `1.5 seconds`.
fn secs_label(v: f64) -> String {
    match v {
        1.0 => "1 second".into(),
        v if v.fract() == 0.0 => format!("{v:.0} seconds"),
        v => format!("{v:.1} seconds"),
    }
}

fn dur(ms: i64) -> String {
    if ms <= 0 {
        String::new()
    } else if ms % 1000 == 0 {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

/// Seconds from a duration text (`6s`, `1500ms`, `2m`, bare seconds).
fn parse_secs(t: &str) -> Option<f64> {
    let t = t.trim();
    if let Some(ms) = t.strip_suffix("ms") {
        return ms.trim().parse::<f64>().ok().map(|x| x / 1000.0);
    }
    if let Some(m) = t.strip_suffix('m') {
        return m.trim().parse::<f64>().ok().map(|x| x * 60.0);
    }
    t.trim_end_matches('s').trim().parse().ok()
}

/// Duration text for seconds (`6s`, `1500ms`).
fn secs_text(secs: f64) -> String {
    if secs.fract() == 0.0 { format!("{}s", secs as i64) } else { format!("{}ms", (secs * 1000.0).round() as i64) }
}

/// Text form of a look field from `alerts.config`.
fn look_text(look: &Value, k: &str) -> String {
    match k {
        "duration" => dur(i(look, "duration_ms")),
        "do" => look.get_path("do").and_then(Value::as_list).map(|l| l.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")).unwrap_or_default(),
        "title" | "message" | "tts_text" => look.get_path(k).and_then(Value::as_str).map(to_words).unwrap_or_default(),
        _ => match look.get_path(k) {
            Some(Value::Str(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => v.to_string(),
        },
    }
}

/// Fill-ins viewers' details go into: engine placeholder ↔ the words shown while editing.
const FILL_INS: [(&str, &str); 8] = [
    ("{user}", "(their name)"),
    ("{amount}", "(amount)"),
    ("{money}", "(money)"),
    ("{message}", "(message)"),
    ("{count}", "(count)"),
    ("{viewers}", "(viewers)"),
    ("{months}", "(months)"),
    ("{tier}", "(tier)"),
];

/// `{user} cheered {amount} bits!` → `(their name) cheered (amount) bits!`.
fn to_words(tpl: &str) -> String {
    FILL_INS.iter().fold(tpl.to_string(), |acc, (p, w)| acc.replace(p, w))
}

/// The reverse of [`to_words`], for saving.
fn from_words(text: &str) -> String {
    FILL_INS.iter().fold(text.to_string(), |acc, (p, w)| acc.replace(w, p))
}

/// Typed field value from what was typed (empty = remove the field).
fn field_value(k: &str, text: &str) -> Result<Value, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(Value::Null);
    }
    Ok(match k {
        "priority" => Value::Int(t.parse().map_err(|_| "Priority must be a whole number".to_string())?),
        "tts" | "enabled" | "veto" | "interrupt" => Value::Bool(matches!(t, "true" | "yes" | "on" | "1")),
        "do" => Value::List(t.split(';').map(str::trim).filter(|x| !x.is_empty()).map(|x| Value::Str(x.into())).collect()),
        "title" | "message" | "tts_text" => Value::Str(from_words(t)),
        _ => Value::Str(t.into()),
    })
}

/// `amount >= N` → N; empty → 0; anything else is a custom condition (`None`).
fn min_amount(cond: &str) -> Option<f64> {
    let c = cond.trim();
    if c.is_empty() {
        return Some(0.0);
    }
    c.strip_prefix("amount").map(str::trim_start).and_then(|r| r.strip_prefix(">=")).and_then(|n| n.trim().parse().ok())
}

// ---- what each kind of alert is ------------------------------------------------------------------

struct AlertKind {
    label: &'static str,
    what: &'static str,
    icon: &'static str,
    /// (button label, simulator command); the first is the main Test button.
    tests: &'static [(&'static str, &'static str)],
    /// Unit of the alert's amount ("bits"), when a minimum makes sense.
    unit: Option<&'static str>,
    sample: &'static str,
}

fn kind_of(when: &str) -> AlertKind {
    let k = |label, what, icon, tests, unit, sample| AlertKind { label, what, icon, tests, unit, sample };
    match when {
        "twitch.follow" => k("Follow", "When someone follows you", icon::HEART, &[("Test", "sim.follow")][..], None, ""),
        "twitch.sub" => k("New sub", "When someone subscribes", icon::STAR, &[("Test", "sim.sub"), ("Tier 3 sub", "sim.sub tier=3")][..], None, "1"),
        "twitch.resub" => {
            k("Resub", "When someone subscribes again", icon::STAR, &[("Test", "sim.resub months=12 message='a year of drums!'")][..], Some("months"), "12")
        }
        "twitch.gift" | "twitch.gift_bomb" => k(
            "Gift subs",
            "When someone gifts subs",
            icon::GIFT,
            &[("Test", "sim.gift_bomb count=5"), ("50 gifts", "sim.gift_bomb count=50")][..],
            Some("subs"),
            "5",
        ),
        "twitch.cheer" => k(
            "Cheer",
            "When someone cheers bits",
            icon::BOLT,
            &[
                ("Test", "sim.cheer bits=1000 message='Cheer1000 HYPE'"),
                ("50 bits", "sim.cheer bits=50 message='nice groove Kappa'"),
                ("10,000 bits", "sim.cheer bits=10000 message='take my bits'"),
            ][..],
            Some("bits"),
            "1000",
        ),
        "twitch.raid" => k("Raid", "When another streamer raids you", icon::USERS, &[("Test", "sim.raid viewers=40")][..], Some("viewers"), "40"),
        "tip" | "kofi.tip" => k(
            "Tip",
            "When someone sends you a tip",
            icon::HEART,
            &[("Test", "sim.tip amount=5 message='for new sticks'"), ("$50 tip", "sim.tip amount=50 message='cymbal fund!'")][..],
            Some("dollars"),
            "5",
        ),
        "twitch.redeem" => k("Channel points", "When someone redeems a reward", icon::STAR, &[("Test", "sim.redeem")][..], None, "2000"),
        _ => k("", "", icon::ALERT, &[][..], None, ""),
    }
}

/// The headline with example values filled in.
fn sample(tpl: &str, k: &AlertKind) -> String {
    to_words(tpl)
        .replace("(their name)", "drumfan42")
        .replace("(amount)", if k.sample.is_empty() { "1" } else { k.sample })
        .replace("(money)", "$5.00")
        .replace("(message)", "nice groove!")
        .replace("(count)", "5")
        .replace("(viewers)", "40")
        .replace("(months)", "12")
        .replace("(tier)", "1")
}

fn sound_name(s: &str) -> String {
    nice(s.strip_prefix("alert_").unwrap_or(s))
}

// ---- page ----------------------------------------------------------------------------------------

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("alerts-form");
    let mut form: Form = ui.data_mut(|d| d.get_temp::<Form>(id)).unwrap_or_default();
    let now = ui.input(|i| i.time);
    if now - form.last_query > 0.25 {
        form.last_query = now;
        app.m.query("alerts", Value::Null);
    }
    if now - form.last_slow > 2.0 {
        form.last_slow = now;
        app.m.query("alerts.config", Value::Null);
    }
    let live = app.m.q("alerts").cloned().unwrap_or_default();
    let cfg = app.m.q("alerts.config").cloned().unwrap_or_default();

    egui::ScrollArea::vertical().id_salt("alerts-page").auto_shrink([false, false]).show(ui, |ui| {
        status_bar(app, ui, &live);
        ui.add_space(spacing::L);
        let w = ui.available_width();
        let right = (w * 0.3).clamp(340.0, 440.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.allocate_ui_with_layout(Vec2::new(w - right - spacing::L, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(w - right - spacing::L);
                let mut idx = TABS.iter().position(|(tb, _)| *tb == form.tab).unwrap_or(0);
                let labels: Vec<&str> = TABS.iter().map(|(_, l)| *l).collect();
                if widgets::segmented(ui, &t, &mut idx, &labels) {
                    form.tab = TABS[idx].0;
                }
                ui.add_space(spacing::M);
                match form.tab {
                    Tab::Alerts => alerts(app, ui, &mut form, &cfg),
                    Tab::Goals => goals(app, ui, &mut form, &cfg),
                    Tab::Stats => stats(app, ui),
                    Tab::Timing => timing(app, ui, &mut form, &cfg),
                }
            });
            ui.add_space(spacing::L);
            ui.allocate_ui_with_layout(Vec2::new(right, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(right);
                live_column(app, ui, &live);
            });
        });
        ui.add_space(spacing::XL);
    });
    ui.data_mut(|d| d.insert_temp(id, form));
}

/// "Alerts are on" + master switch + hold/resume.
fn status_bar(app: &mut App, ui: &mut egui::Ui, live: &Value) {
    use widgets::Tone;
    let t = app.t.clone();
    let paused = live.get_path("paused").is_some_and(Value::truthy);
    let enabled = app.m.get("alerts.enabled").is_none_or(Value::truthy);
    let health = app.m.get("health.alerts").cloned().unwrap_or_default();
    // One control: the button says what it does next. Turning alerts off completely lives in
    // the Settings view.
    let (tone, title, body, action): (Tone, &str, String, Option<(&str, &str)>) = if !enabled {
        (Tone::Warn, "Alerts are off", "Nothing pops up on stream. Follows, subs and cheers are still counted.".into(), Some(("Turn alerts on", "")))
    } else if paused {
        match s(live, "pause_reason") {
            "manual" => {
                (Tone::Warn, "Alerts are on hold", "New alerts wait here and all play when you resume.".into(), Some(("Resume alerts", "alerts.resume")))
            }
            "panic" => (
                Tone::Danger,
                "Alerts stopped",
                "Alerts stopped after an emergency stop. Resume when you're ready.".into(),
                Some(("Resume alerts", "alerts.resume")),
            ),
            _ => (Tone::Warn, "Alerts are waiting", "Alerts wait during ad breaks and Be right back, then play afterwards.".into(), None),
        }
    } else if s(&health, "status") == "warn" {
        (Tone::Warn, "Alerts need a look", s(&health, "detail").to_string(), Some(("Hold alerts", "alerts.pause")))
    } else {
        (
            Tone::Ok,
            "Alerts are on",
            "New followers, subs, cheers, raids and tips pop up on stream. Hold them for a moment with the button.".into(),
            Some(("Hold alerts", "alerts.pause")),
        )
    };
    if widgets::callout(ui, &t, tone, icon::ALERT, title, &body, action.map(|(l, _)| l)) {
        match action {
            Some((_, "")) => app.m.command(Op::Set { address: "alerts.enabled".into(), value: Value::Bool(true) }),
            Some((_, a)) => act(app, a, Value::Null),
            None => {}
        }
    }
}

// ---- alerts --------------------------------------------------------------------------------------

fn alerts(app: &mut App, ui: &mut egui::Ui, form: &mut Form, cfg: &Value) {
    let t = app.t.clone();
    let list: Vec<Value> = cfg.get_path("alerts").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let mut sounds: Vec<String> = list
        .iter()
        .flat_map(|a| {
            std::iter::once(a.get_path("look.sound"))
                .chain(a.get_path("variations").and_then(Value::as_list).unwrap_or(&[]).iter().map(|v| v.get_path("look.sound")))
        })
        .filter_map(|v| v.and_then(Value::as_str).map(String::from))
        .collect();
    sounds.sort();
    sounds.dedup();
    form.sounds = sounds;
    if let Some(key) = form.editing.clone() {
        match list.iter().find(|a| format!("{}|{}|", s(a, "file"), i(a, "index")) == key) {
            Some(a) => return editor(app, ui, form, a),
            None => form.editing = None,
        }
    }
    if list.is_empty() {
        let add = widgets::panel(ui, &t, |ui| {
            ui.set_width(ui.available_width());
            widgets::empty_state(
                ui,
                &t,
                icon::ALERT,
                "No alerts yet",
                "Alerts pop up on stream when someone follows, subscribes, cheers, raids or tips.",
                Some("Add an alert"),
            )
        });
        if add {
            form.adding = true;
        }
    } else {
        let w = ui.available_width();
        let cols = ((w + spacing::L) / 420.0).floor().max(1.0) as usize;
        let cw = (w - spacing::L * (cols - 1) as f32) / cols as f32;
        for (r, row) in list.chunks(cols).enumerate() {
            // Cards in a row line up: each pads its body to the tallest one (last frame).
            let hid = egui::Id::new(("alert-row-body", r, cols));
            let pad_to: f32 = ui.data(|d| d.get_temp(hid)).unwrap_or(0.0);
            let mut tallest = 0.0_f32;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = spacing::L;
                for a in row {
                    ui.allocate_ui_with_layout(Vec2::new(cw, 0.0), Layout::top_down(Align::Min), |ui| {
                        ui.set_width(cw);
                        tallest = tallest.max(alert_card(app, ui, form, a, pad_to));
                    });
                }
            });
            if (tallest - pad_to).abs() > 0.5 {
                ui.data_mut(|d| d.insert_temp(hid, tallest));
                ui.ctx().request_repaint();
            }
            ui.add_space(spacing::L);
        }
    }
    add_alert(app, ui, form, cfg);
}

/// One alert card. Pads the body to `pad_to` so the buttons line up across a row; returns the
/// body's natural height.
fn alert_card(app: &mut App, ui: &mut egui::Ui, form: &mut Form, a: &Value, pad_to: f32) -> f32 {
    let t = app.t.clone();
    let k = kind_of(s(a, "when"));
    let file = s(a, "file").to_string();
    let idx = i(a, "index");
    let mut enabled = a.get_path("enabled").is_some_and(Value::truthy);
    let look = a.get_path("look").cloned().unwrap_or_default();
    let label = if k.label.is_empty() { nice(s(a, "name")) } else { k.label.to_string() };
    widgets::panel(ui, &t, |ui| {
        ui.set_width(ui.available_width());
        let top = ui.cursor().top();
        ui.horizontal(|ui| {
            let c = if enabled { t.accent } else { t.text_faint };
            let (r, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 20.0, mix(t.surface, c, 0.18));
            ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, k.icon, font(17.0), c);
            ui.add_space(spacing::S);
            ui.vertical(|ui| {
                ui.label(RichText::new(&label).font(font_semibold(type_scale::LARGE)).color(t.fg));
                let what = if k.what.is_empty() { format!("When {}", nice(&s(a, "when").replace('.', " ")).to_lowercase()) } else { k.what.to_string() };
                ui.label(RichText::new(what).size(type_scale::SMALL + 0.5).color(t.text_dim));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::toggle(ui, &t, &mut enabled).on_hover_text(if enabled { "On: shows on stream" } else { "Off: doesn't show" }).changed() {
                    act(
                        app,
                        "alerts.edit",
                        Value::map()
                            .with("file", file.clone())
                            .with("path", Value::List(vec!["alert".into(), Value::Int(idx)]))
                            .with("fields", Value::map().with("enabled", if enabled { Value::Null } else { Value::Bool(false) })),
                    );
                }
            });
        });
        ui.add_space(spacing::M);
        // What viewers see, with example values.
        egui::Frame::new().fill(t.surface_hi).corner_radius(CornerRadius::same(8)).inner_margin(egui::Margin::symmetric(12, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let title = look_text(&look, "title");
            ui.add(
                egui::Label::new(
                    RichText::new(if title.is_empty() { label.clone() } else { sample(&title, &k) }).font(font_semibold(type_scale::BODY + 0.5)).color(t.fg),
                )
                .truncate(),
            );
            let msg = look_text(&look, "message");
            if !msg.is_empty() {
                ui.add(egui::Label::new(RichText::new(sample(&msg, &k)).color(t.text_dim)).truncate());
            }
        });
        ui.add_space(spacing::S);
        let mut facts: Vec<(&str, String)> = Vec::new();
        let sound = look_text(&look, "sound");
        facts.push(if sound.is_empty() { (icon::MUTE, "No sound".into()) } else { (icon::VOLUME, format!("Sound: {}", sound_name(&sound))) });
        if let Some(secs) = parse_secs(&look_text(&look, "duration")) {
            facts.push((icon::CLOCK, secs_label(secs)));
        }
        if look.get_path("tts").is_some_and(Value::truthy) {
            facts.push((icon::MIC, "Reads the message aloud".into()));
        }
        if let (Some(unit), Some(min)) = (k.unit, min_amount(s(a, "if"))) {
            if min > 0.0 {
                facts.push((icon::SLIDERS, format!("From {} {unit}", show_num(min))));
            }
        } else if !s(a, "if").is_empty() {
            facts.push((icon::SLIDERS, "Only sometimes".into()));
        }
        let nv = a.get_path("variations").and_then(Value::as_list).map(<[Value]>::len).unwrap_or(0);
        if nv > 0 {
            facts.push((icon::STAR, format!("{nv} variation{}", if nv == 1 { "" } else { "s" })));
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(spacing::L, spacing::XS);
            for (ic, fct) in &facts {
                // Keep each icon with its words: wrap before the pair, not between them.
                let need = ui.painter().layout_no_wrap(fct.clone(), font(type_scale::SMALL + 0.5), t.fg).size().x + 24.0;
                if ui.available_size_before_wrap().x < need {
                    ui.end_row();
                }
                ui.label(RichText::new(*ic).size(type_scale::SMALL).color(t.text_faint));
                ui.add_space(-spacing::L + 10.0);
                ui.add(egui::Label::new(RichText::new(fct).size(type_scale::SMALL + 0.5).color(t.text_dim)).wrap_mode(egui::TextWrapMode::Extend));
            }
        });
        let body_h = ui.cursor().top() - top;
        ui.add_space((pad_to - body_h).max(0.0) + spacing::M);
        ui.horizontal(|ui| {
            if let Some((_, cmd)) = k.tests.first() {
                if widgets::button_ex(ui, &t, Some(icon::PLAY), "Test", Kind::Secondary, Size::Medium, 96.0, true)
                    .on_hover_text("Shows a pretend one on stream")
                    .clicked()
                {
                    app.m.text(cmd);
                }
                if k.tests.len() > 1 {
                    let more = widgets::button_ex(ui, &t, Some(icon::DOWN), "More tests", Kind::Secondary, Size::Medium, 0.0, true);
                    egui::Popup::menu(&more).show(|ui| {
                        for (l, cmd) in &k.tests[1..] {
                            if ui.button(*l).clicked() {
                                app.m.text(cmd);
                                ui.close();
                            }
                        }
                    });
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::button_ex(ui, &t, Some(icon::EDIT), "Edit", Kind::Secondary, Size::Medium, 0.0, true).clicked() {
                    form.editing = Some(format!("{file}|{idx}|"));
                }
            });
        });
        body_h
    })
}

/// A label + optional help on the left, the control on the right.
fn form_row(ui: &mut egui::Ui, t: &Theme, label: &str, help: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(Vec2::new(230.0, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(230.0);
            ui.label(RichText::new(label).font(font_medium(type_scale::BODY)).color(t.fg));
            if !help.is_empty() {
                ui.add(egui::Label::new(RichText::new(help).size(type_scale::SMALL).color(t.text_dim)).wrap());
            }
        });
        ui.add_space(spacing::M);
        body(ui);
    });
    ui.add_space(spacing::M);
}

/// The friendly fields shared by an alert and its variations. `prefix` keys `form.edits`.
fn look_fields(ui: &mut egui::Ui, t: &Theme, edits: &mut BTreeMap<String, String>, prefix: &str, look: &Value, inherit: bool, sounds: &[String]) {
    let blank = if inherit { "Same as the main alert" } else { "" };
    let w = ui.available_width() - 250.0;
    form_row(ui, t, "Headline", "Type (their name), (amount) or (message) and the viewer's details are filled in.", |ui| {
        let buf = edits.entry(format!("{prefix}title")).or_insert_with(|| look_text(look, "title"));
        ui.add(se_ui_kit::widgets::field(buf).hint_text(blank).desired_width(w));
    });
    form_row(ui, t, "Second line", "", |ui| {
        let buf = edits.entry(format!("{prefix}message")).or_insert_with(|| look_text(look, "message"));
        ui.add(se_ui_kit::widgets::field(buf).hint_text(blank).desired_width(w));
    });
    form_row(ui, t, "Sound", "", |ui| {
        let buf = edits.entry(format!("{prefix}sound")).or_insert_with(|| look_text(look, "sound"));
        let empty = if inherit { blank } else { "No sound" };
        let shown = if buf.trim().is_empty() { empty.to_string() } else { sound_name(buf.trim()) };
        egui::ComboBox::from_id_salt(("alert-sound", prefix)).selected_text(shown).width(w.min(320.0)).show_ui(ui, |ui| {
            if ui.selectable_label(buf.trim().is_empty(), empty).clicked() {
                buf.clear();
            }
            for snd in sounds {
                if ui.selectable_label(buf.trim() == snd, sound_name(snd)).clicked() {
                    *buf = snd.clone();
                }
            }
        });
    });
    form_row(ui, t, "How long it stays up", "", |ui| {
        let key = format!("{prefix}duration");
        let buf = edits.entry(key).or_insert_with(|| look_text(look, "duration"));
        let has = parse_secs(buf).is_some();
        let mut secs = parse_secs(buf).unwrap_or(5.0) as f32;
        ui.spacing_mut().slider_width = (w - 120.0).clamp(160.0, 360.0);
        if ui.add(egui::Slider::new(&mut secs, 1.0..=30.0).step_by(0.5).custom_formatter(|v, _| secs_label(v))).changed() {
            *buf = secs_text(secs as f64);
        }
        if inherit && has && widgets::button_ex(ui, t, None, "Same as main", Kind::Ghost, Size::Small, 0.0, true).clicked() {
            buf.clear();
        } else if inherit && !has {
            widgets::hint(ui, t, blank);
        }
    });
    form_row(ui, t, "Read the message aloud", "Text to speech reads the viewer's message.", |ui| {
        let buf = edits.entry(format!("{prefix}tts")).or_insert_with(|| look_text(look, "tts"));
        if inherit {
            let label = |v: &str| match v {
                "true" => "Yes",
                "false" => "No",
                _ => "Same as the main alert",
            };
            let cur = buf.trim().to_string();
            egui::ComboBox::from_id_salt(("tts-choice", prefix)).selected_text(label(&cur)).width(220.0).show_ui(ui, |ui| {
                for v in ["", "true", "false"] {
                    if ui.selectable_label(cur == v, label(v)).clicked() {
                        *buf = v.to_string();
                    }
                }
            });
        } else {
            let mut on = buf.trim() == "true";
            if widgets::toggle(ui, t, &mut on).changed() {
                *buf = if on { "true".into() } else { "false".into() };
            }
        }
    });
}

fn save_fields(app: &mut App, form: &mut Form, file: &str, path: Value, prefix: &str, originals: &[(String, String)]) {
    let mut fields = Value::map();
    let mut n = 0;
    for (k, orig) in originals {
        let key = format!("{prefix}{k}");
        let Some(text) = form.edits.get(&key) else { continue };
        if text == orig {
            continue;
        }
        match field_value(k, text) {
            Ok(v) => {
                fields = fields.with(k.clone(), v);
                n += 1;
            }
            Err(e) => {
                app.m.toast(e, true);
                return;
            }
        }
    }
    if n == 0 {
        return;
    }
    act(app, "alerts.edit", Value::map().with("file", file).with("path", path).with("fields", fields));
    form.edits.retain(|k, _| !k.starts_with(prefix));
}

/// `v0|title` (a variation's field) vs `title` (the alert's own).
fn is_version_key(rest: &str) -> bool {
    rest.strip_prefix('v').and_then(|r| r.chars().next()).is_some_and(|c| c.is_ascii_digit())
}

fn is_dirty(form: &Form, prefix: &str, originals: &[(String, String)]) -> bool {
    originals.iter().any(|(k, o)| form.edits.get(&format!("{prefix}{k}")).is_some_and(|x| x != o))
}

fn editor(app: &mut App, ui: &mut egui::Ui, form: &mut Form, a: &Value) {
    let t = app.t.clone();
    let k = kind_of(s(a, "when"));
    let file = s(a, "file").to_string();
    let idx = i(a, "index");
    let prefix = format!("{file}|{idx}|");
    let path = Value::List(vec!["alert".into(), Value::Int(idx)]);
    let look = a.get_path("look").cloned().unwrap_or_default();
    let label = if k.label.is_empty() { nice(s(a, "name")) } else { k.label.to_string() };
    let mut originals: Vec<(String, String)> = vec![("when".into(), s(a, "when").into()), ("if".into(), s(a, "if").into())];
    originals.extend(LOOK.iter().map(|k| (k.to_string(), look_text(&look, k))));

    ui.horizontal(|ui| {
        if widgets::button_ex(ui, &t, Some(icon::LEFT), "All alerts", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
            form.editing = None;
        }
    });
    ui.add_space(spacing::S);
    let mut close = false;
    let mut test = false;
    widgets::titled(
        ui,
        &t,
        &format!("{label} alert"),
        if k.what.is_empty() { "" } else { k.what },
        |ui| {
            if !k.tests.is_empty() && widgets::button_ex(ui, &t, Some(icon::PLAY), "Test", Kind::Secondary, Size::Medium, 0.0, true).clicked() {
                test = true;
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            // Live preview with the typed text.
            let title = form.edits.get(&format!("{prefix}title")).cloned().unwrap_or_else(|| look_text(&look, "title"));
            let msg = form.edits.get(&format!("{prefix}message")).cloned().unwrap_or_else(|| look_text(&look, "message"));
            egui::Frame::new().fill(t.surface_hi).corner_radius(CornerRadius::same(10)).inner_margin(egui::Margin::symmetric(18, 14)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Preview").font(font_semibold(type_scale::SMALL + 0.5)).color(t.text_faint));
                ui.label(RichText::new(sample(&title, &k)).font(font_bold(type_scale::HEADING)).color(t.fg));
                if !msg.is_empty() {
                    ui.label(RichText::new(sample(&msg, &k)).size(type_scale::LARGE).color(t.text_dim));
                }
            });
            ui.add_space(spacing::L);
            let mut enabled = a.get_path("enabled").is_some_and(Value::truthy);
            form_row(ui, &t, "Show this alert", "Off: it's still counted, but nothing pops up.", |ui| {
                if widgets::toggle(ui, &t, &mut enabled).changed() {
                    act(
                        app,
                        "alerts.edit",
                        Value::map()
                            .with("file", file.clone())
                            .with("path", path.clone())
                            .with("fields", Value::map().with("enabled", if enabled { Value::Null } else { Value::Bool(false) })),
                    );
                }
            });
            look_fields(ui, &t, &mut form.edits, &prefix, &look, false, &form.sounds);
            if let Some(unit) = k.unit {
                let buf = form.edits.entry(format!("{prefix}if")).or_insert_with(|| s(a, "if").to_string());
                match min_amount(buf) {
                    Some(min) => form_row(ui, &t, "Minimum amount", &format!("Smaller ones don't get this alert. 0 = every one ({unit})."), |ui| {
                        let mut v = min;
                        if ui.add(egui::DragValue::new(&mut v).range(0.0..=1_000_000.0).speed(1.0).suffix(format!(" {unit}"))).changed() {
                            *buf = if v > 0.0 { format!("amount >= {}", fmt_num(v)) } else { String::new() };
                        }
                    }),
                    None => form_row(ui, &t, "Minimum amount", "", |ui| {
                        widgets::hint(ui, &t, "This alert has a custom condition — see Details below.");
                    }),
                }
            }
            let veto = a.get_path("veto").is_some_and(Value::truthy);
            let mut v = veto;
            form_row(ui, &t, "Let mods skip it first", "When the viewer wrote a message, it waits a moment so you or a mod can skip it.", |ui| {
                if widgets::toggle(ui, &t, &mut v).changed() {
                    act(
                        app,
                        "alerts.edit",
                        Value::map()
                            .with("file", file.clone())
                            .with("path", path.clone())
                            .with("fields", Value::map().with("veto", if v { Value::Null } else { Value::Bool(false) })),
                    );
                }
            });
            let dirty = is_dirty(form, &prefix, &originals);
            ui.horizontal(|ui| {
                if widgets::button_ex(ui, &t, Some(icon::CHECK), "Save changes", Kind::Primary, Size::Medium, 0.0, dirty).clicked() {
                    save_fields(app, form, &file, path.clone(), &prefix, &originals);
                }
                if dirty && widgets::button_ex(ui, &t, Some(icon::UNDO), "Undo changes", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                    form.edits.retain(|k, _| k.strip_prefix(prefix.as_str()).is_none_or(is_version_key));
                }
                if !dirty {
                    widgets::hint(ui, &t, "Everything is saved.");
                }
            });
            ui.add_space(spacing::M);
            widgets::details(ui, &t, ("alert-details", &prefix), "Details", |ui| {
                let w = ui.available_width() - 250.0;
                for (key, lbl, help) in [
                    ("when", "Happens on", "The event that triggers this alert."),
                    ("sound", "Sound file name", "Any sound in your project's sounds folder."),
                    ("if", "Only if", "A condition, e.g. amount >= 500. Empty = always."),
                    ("priority", "Priority", "Higher numbers go first and can cut in on smaller alerts."),
                    ("tts_text", "Spoken text", "What text to speech says. Empty = the message."),
                    ("voice", "Voice", "Text to speech voice name."),
                    ("do", "Also run", "Commands to run with the alert, separated by ;"),
                ] {
                    form_row(ui, &t, lbl, help, |ui| {
                        let orig = originals.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()).unwrap_or_default();
                        let buf = form.edits.entry(format!("{prefix}{key}")).or_insert(orig);
                        ui.add(se_ui_kit::widgets::field(buf).font(egui::TextStyle::Monospace).desired_width(w));
                    });
                }
                form_row(ui, &t, "Saved in", "", |ui| {
                    ui.label(RichText::new(&file).font(font_mono(type_scale::SMALL)).color(t.text_dim));
                });
                ui.horizontal(|ui| {
                    if widgets::hold_button(ui, &t, "Hold to delete this alert", t.bright_red, 0.8) {
                        act(
                            app,
                            "alerts.remove",
                            Value::map().with("file", file.clone()).with("path", Value::List(vec![])).with("key", "alert").with("index", idx),
                        );
                        close = true;
                    }
                });
            });
        },
    );
    if test && let Some((_, cmd)) = k.tests.first() {
        app.m.text(cmd);
    }
    if close {
        form.editing = None;
        return;
    }
    ui.add_space(spacing::L);
    versions(app, ui, form, a, &k);
}

/// Variations (variations): e.g. a bigger alert for 1,000 bits or more.
fn versions(app: &mut App, ui: &mut egui::Ui, form: &mut Form, a: &Value, k: &AlertKind) {
    let t = app.t.clone();
    let file = s(a, "file").to_string();
    let idx = i(a, "index");
    let prefix = format!("{file}|{idx}|");
    let vars: Vec<Value> = a.get_path("variations").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    widgets::titled(
        ui,
        &t,
        "Variations",
        "A different look for some of them, like a bigger alert for big cheers. The first one that fits is used.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if vars.is_empty() {
                widgets::hint(ui, &t, "None yet. Every one gets the normal alert.");
            }
            for (vi, v) in vars.iter().enumerate() {
                let vprefix = format!("{prefix}v{vi}|");
                let vlook = v.get_path("look").cloned().unwrap_or_default();
                let cond = s(v, "if");
                let when = match (k.unit, min_amount(cond)) {
                    (Some(unit), Some(n)) if n > 0.0 => format!("For {} {unit} or more", show_num(n)),
                    _ if cond.is_empty() => "Always".to_string(),
                    _ => format!("When {cond}"),
                };
                let mut originals: Vec<(String, String)> = vec![("name".into(), s(v, "name").into()), ("if".into(), cond.into())];
                originals.extend(LOOK.iter().map(|k| (k.to_string(), look_text(&vlook, k))));
                let open_id = egui::Id::new(("alert-version-open", &vprefix));
                let mut open = ui.data(|d| d.get_temp::<bool>(open_id)).unwrap_or(false);
                let title = look_text(&vlook, "title");
                let sub = if title.is_empty() { when.clone() } else { format!("{when} · “{}”", sample(&title, k)) };
                if widgets::list_row(ui, &t, if open { icon::DOWN } else { icon::RIGHT }, &nice(s(v, "name")), &sub, "", open).clicked() {
                    open = !open;
                    ui.data_mut(|d| d.insert_temp(open_id, open));
                }
                if !open {
                    continue;
                }
                egui::Frame::new().inner_margin(egui::Margin { left: 40, right: 0, top: 8, bottom: 8 }).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    look_fields(ui, &t, &mut form.edits, &vprefix, &vlook, true, &form.sounds);
                    let w = ui.available_width() - 250.0;
                    form_row(ui, &t, "Used when", "e.g. amount >= 1000", |ui| {
                        let buf = form.edits.entry(format!("{vprefix}if")).or_insert_with(|| cond.to_string());
                        ui.add(se_ui_kit::widgets::field(buf).font(egui::TextStyle::Monospace).desired_width(w));
                    });
                    widgets::details(ui, &t, ("version-details", &vprefix), "Details", |ui| {
                        for (key, lbl) in [("name", "Name"), ("priority", "Priority"), ("tts_text", "Spoken text"), ("voice", "Voice"), ("do", "Also run")] {
                            form_row(ui, &t, lbl, "", |ui| {
                                let orig = originals.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()).unwrap_or_default();
                                let buf = form.edits.entry(format!("{vprefix}{key}")).or_insert(orig);
                                ui.add(se_ui_kit::widgets::field(buf).font(egui::TextStyle::Monospace).desired_width(w));
                            });
                        }
                    });
                    let dirty = is_dirty(form, &vprefix, &originals);
                    ui.horizontal(|ui| {
                        let path = Value::List(vec!["alert".into(), Value::Int(idx), "variation".into(), Value::Int(vi as i64)]);
                        if widgets::button_ex(ui, &t, Some(icon::CHECK), "Save this variation", Kind::Primary, Size::Medium, 0.0, dirty).clicked() {
                            save_fields(app, form, &file, path, &vprefix, &originals);
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::hold_button(ui, &t, "Hold to delete", t.bright_red, 0.6) {
                                act(
                                    app,
                                    "alerts.remove",
                                    Value::map()
                                        .with("file", file.clone())
                                        .with("path", Value::List(vec!["alert".into(), Value::Int(idx)]))
                                        .with("key", "variation")
                                        .with("index", vi as i64),
                                );
                            }
                        });
                    });
                });
            }
            ui.add_space(spacing::M);
            let nv = form.new_var.entry(prefix.clone()).or_default();
            ui.horizontal(|ui| {
                ui.label(RichText::new("Add a variation").font(font_medium(type_scale::BODY)).color(t.fg));
                ui.add(se_ui_kit::widgets::field(&mut nv.0).hint_text("Name, e.g. big").desired_width(150.0));
                let cond = match k.unit {
                    Some(unit) => {
                        let mut n = nv.1.trim().parse::<f64>().unwrap_or(0.0);
                        ui.label(RichText::new("for").color(t.text_dim));
                        if ui.add(egui::DragValue::new(&mut n).range(0.0..=1_000_000.0).suffix(format!(" {unit} or more"))).changed() {
                            nv.1 = fmt_num(n);
                        }
                        (n > 0.0).then(|| format!("amount >= {}", fmt_num(n)))
                    }
                    None => {
                        ui.add(se_ui_kit::widgets::field(&mut nv.1).hint_text("Used when… (e.g. tier == 3)").desired_width(200.0));
                        (!nv.1.trim().is_empty()).then(|| nv.1.trim().to_string())
                    }
                };
                let ok = cond.is_some() && !nv.0.trim().is_empty();
                if widgets::button_ex(ui, &t, Some(icon::PLUS), "Add", Kind::Secondary, Size::Medium, 0.0, ok).clicked()
                    && let Some(cond) = cond
                {
                    let fields = Value::map().with("name", nv.0.trim().replace(' ', "_")).with("if", cond);
                    act(
                        app,
                        "alerts.add",
                        Value::map()
                            .with("file", file.clone())
                            .with("path", Value::List(vec!["alert".into(), Value::Int(idx)]))
                            .with("key", "variation")
                            .with("fields", fields),
                    );
                    *nv = Default::default();
                }
            });
        },
    );
}

/// Kinds offered when adding an alert (event → label).
const NEW_KINDS: [(&str, &str); 8] = [
    ("twitch.follow", "Follow"),
    ("twitch.sub", "New sub"),
    ("twitch.resub", "Resub"),
    ("twitch.gift", "Gift subs"),
    ("twitch.cheer", "Cheer"),
    ("twitch.raid", "Raid"),
    ("tip", "Tip"),
    ("twitch.redeem", "Channel points"),
];

fn add_alert(app: &mut App, ui: &mut egui::Ui, form: &mut Form, cfg: &Value) {
    let t = app.t.clone();
    if !form.adding {
        if widgets::button_ex(ui, &t, Some(icon::PLUS), "Add an alert", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
            form.adding = true;
        }
        return;
    }
    let files: Vec<String> = cfg
        .get_path("files")
        .and_then(Value::as_list)
        .map(|l| l.iter().filter_map(Value::as_str).filter(|f| !f.ends_with("queue.toml")).map(String::from).collect())
        .unwrap_or_default();
    if form.new_alert_file.is_empty() {
        form.new_alert_file = "alerts/custom.toml".into();
    }
    if form.new_alert_when.is_empty() {
        form.new_alert_when = NEW_KINDS[0].0.into();
    }
    widgets::titled(
        ui,
        &t,
        "Add an alert",
        "Pick what it's for. You can change how it looks right after.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            form_row(ui, &t, "What it's for", "", |ui| {
                let sel = NEW_KINDS.iter().find(|(w, _)| *w == form.new_alert_when).map(|(_, l)| l.to_string()).unwrap_or_else(|| form.new_alert_when.clone());
                egui::ComboBox::from_id_salt("alerts-new-kind").selected_text(sel).width(240.0).show_ui(ui, |ui| {
                    for (w, l) in NEW_KINDS {
                        ui.selectable_value(&mut form.new_alert_when, w.to_string(), l);
                    }
                });
            });
            form_row(ui, &t, "Name", "A short name, e.g. big_cheer.", |ui| {
                ui.add(se_ui_kit::widgets::field(&mut form.new_alert_name).desired_width(240.0));
            });
            widgets::details(ui, &t, "new-alert-details", "Details", |ui| {
                form_row(ui, &t, "Happens on", "Any event name.", |ui| {
                    ui.add(se_ui_kit::widgets::field(&mut form.new_alert_when).font(egui::TextStyle::Monospace).desired_width(240.0));
                });
                form_row(ui, &t, "Save in", "", |ui| {
                    egui::ComboBox::from_id_salt("alerts-new-file").selected_text(form.new_alert_file.clone()).show_ui(ui, |ui| {
                        for f in &files {
                            ui.selectable_value(&mut form.new_alert_file, f.clone(), f);
                        }
                        ui.selectable_value(&mut form.new_alert_file, "alerts/custom.toml".to_string(), "alerts/custom.toml");
                    });
                });
            });
            ui.add_space(spacing::S);
            ui.horizontal(|ui| {
                let ok = !form.new_alert_when.trim().is_empty() && !form.new_alert_name.trim().is_empty();
                if widgets::button_ex(ui, &t, Some(icon::PLUS), "Add alert", Kind::Primary, Size::Medium, 0.0, ok).clicked() {
                    let fields = Value::map()
                        .with("name", form.new_alert_name.trim().replace(' ', "_"))
                        .with("when", form.new_alert_when.trim())
                        .with("title", "{user}");
                    act(
                        app,
                        "alerts.add",
                        Value::map().with("file", form.new_alert_file.clone()).with("path", Value::List(vec![])).with("key", "alert").with("fields", fields),
                    );
                    form.new_alert_name.clear();
                    form.adding = false;
                }
                if widgets::button_ex(ui, &t, None, "Cancel", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                    form.adding = false;
                }
            });
        },
    );
}

// ---- right column: on screen / waiting / recent --------------------------------------------------

fn live_column(app: &mut App, ui: &mut egui::Ui, live: &Value) {
    let t = app.t.clone();
    let cur = live.get_path("current").cloned().unwrap_or_default();
    widgets::titled(
        ui,
        &t,
        "On screen now",
        "",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if cur.is_null() {
                widgets::hint(ui, &t, "Nothing on screen right now.");
                return;
            }
            ui.add(egui::Label::new(RichText::new(s(&cur, "title")).font(font_semibold(type_scale::LARGE)).color(t.fg)).wrap());
            if !s(&cur, "message").is_empty() {
                ui.add(egui::Label::new(RichText::new(s(&cur, "message")).color(t.text_dim)).wrap());
            }
            let n = cur.get_path("recipients").and_then(Value::as_list).map(|l| l.len()).unwrap_or(0);
            let mut line = format!("{} seconds left", i(&cur, "remaining_ms") / 1000);
            if n > 0 {
                line.push_str(&format!(" · {n} people got a gift"));
            }
            ui.label(RichText::new(line).size(type_scale::SMALL + 0.5).color(t.text_faint));
            ui.add_space(spacing::S);
            ui.horizontal(|ui| {
                if widgets::button_ex(ui, &t, Some(icon::RIGHT), "Skip", Kind::Secondary, Size::Medium, 0.0, true)
                    .on_hover_text("End it now and go to the next one")
                    .clicked()
                {
                    act(app, "alerts.skip", Value::Null);
                }
                if widgets::button_ex(ui, &t, Some(icon::CROSS), "Remove", Kind::Danger, Size::Medium, 0.0, true)
                    .on_hover_text("Take it off and never replay it")
                    .clicked()
                {
                    act(app, "alerts.veto", Value::map().with("id", i(&cur, "id")));
                }
            });
        },
    );
    ui.add_space(spacing::L);

    let queue: Vec<Value> = live.get_path("queue").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let mut clear = false;
    widgets::titled(
        ui,
        &t,
        "Waiting",
        if queue.is_empty() { "" } else { "Plays in this order" },
        |ui| {
            clear = !queue.is_empty() && widgets::hold_button(ui, &t, "Hold to clear", t.yellow, 0.6);
        },
        |ui| {
            ui.set_width(ui.available_width());
            if queue.is_empty() {
                widgets::hint(ui, &t, "No alerts waiting.");
            }
            for a in &queue {
                let aid = i(a, "id");
                let veto_ms = i(a, "veto_remaining_ms");
                let fill = if veto_ms > 0 { mix(t.surface, t.yellow, 0.08) } else { t.surface_hi };
                egui::Frame::new().fill(fill).corner_radius(CornerRadius::same(8)).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(RichText::new(s(a, "title")).font(font_medium(type_scale::BODY)).color(t.fg)).truncate());
                        if a.get_path("sim").is_some_and(Value::truthy) {
                            widgets::badge(ui, &t, "Test", t.text_dim);
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::icon_button(ui, &t, icon::CROSS, "Remove it (it won't show)").clicked() {
                                act(app, "alerts.veto", Value::map().with("id", aid));
                            }
                        });
                    });
                    if !s(a, "message").is_empty() {
                        ui.add(egui::Label::new(RichText::new(format!("“{}”", s(a, "message"))).color(t.text_dim)).wrap());
                    }
                    if veto_ms > 0 {
                        let window = app.m.get(&format!("alerts.veto.{aid}")).and_then(Value::as_f64).unwrap_or(veto_ms as f64 / 1000.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("Mods can skip it for {window:.1} s")).size(type_scale::SMALL + 0.5).color(mix(t.yellow, t.fg, 0.3)),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if widgets::button_ex(ui, &t, Some(icon::CHECK), "Show now", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                                    act(app, "alerts.approve", Value::map().with("id", aid));
                                }
                            });
                        });
                        let frac = (veto_ms as f32 / 3000.0).clamp(0.0, 1.0);
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 4.0), Sense::hover());
                        ui.painter().rect_filled(rect, CornerRadius::same(2), t.inset);
                        let mut fr = rect;
                        fr.set_width(rect.width() * frac);
                        ui.painter().rect_filled(fr, CornerRadius::same(2), t.yellow);
                    }
                });
                ui.add_space(spacing::XS);
            }
        },
    );
    if clear {
        act(app, "alerts.clear", Value::Null);
    }
    ui.add_space(spacing::L);

    let history: Vec<Value> = live.get_path("history").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    widgets::titled(
        ui,
        &t,
        "Shown recently",
        "",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if history.is_empty() {
                widgets::hint(ui, &t, "Alerts you've had this session show up here, so you can replay them.");
            }
            for h in history.iter().take(30) {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width() - 90.0);
                        ui.add(egui::Label::new(RichText::new(s(h, "title")).font(font_medium(type_scale::BODY)).color(t.fg)).truncate());
                        ui.label(RichText::new(ago(i(h, "ago_ms"))).size(type_scale::SMALL).color(t.text_faint));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::button_ex(ui, &t, Some(icon::UNDO), "Replay", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                            act(app, "alerts.replay", Value::map().with("id", i(h, "id")));
                        }
                    });
                });
                ui.add_space(spacing::XS);
            }
        },
    );
}

fn ago(ms: i64) -> String {
    let s = ms / 1000;
    match s {
        0..=9 => "just now".into(),
        10..=59 => format!("{s} seconds ago"),
        60..=3599 => format!("{} min ago", s / 60),
        _ => format!("{} h ago", s / 3600),
    }
}

// ---- goals ---------------------------------------------------------------------------------------

/// What a goal can count (key → words).
const COUNTS: [(&str, &str); 7] = [
    ("follows", "New followers"),
    ("subs", "New subs"),
    ("sub_points", "Sub points"),
    ("bits", "Bits"),
    ("tips", "Tips"),
    ("gifts", "Gifted subs"),
    ("raids", "Raids"),
];

fn counts_label(k: &str) -> String {
    COUNTS.iter().find(|(c, _)| *c == k).map(|(_, l)| l.to_string()).unwrap_or_else(|| nice(k))
}

fn progress_bar(ui: &mut egui::Ui, t: &Theme, frac: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 10.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(5), t.inset);
    let mut fill = rect;
    fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
    if frac > 0.0 {
        ui.painter().rect_filled(fill, CornerRadius::same(5), color);
    }
}

fn goals(app: &mut App, ui: &mut egui::Ui, form: &mut Form, cfg: &Value) {
    let t = app.t.clone();
    let list: Vec<Value> = cfg.get_path("goals").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    if list.is_empty() && !form.goal_adding {
        let add = widgets::panel(ui, &t, |ui| {
            ui.set_width(ui.available_width());
            widgets::empty_state(ui, &t, icon::STAR, "No goals yet", "A goal shows a progress bar on stream, like \"Sub goal 32 / 50\".", Some("Add a goal"))
        });
        if add {
            form.goal_adding = true;
        }
        return;
    }
    let w = ui.available_width();
    let cols = ((w + spacing::L) / 380.0).floor().max(1.0) as usize;
    let cw = (w - spacing::L * (cols - 1) as f32) / cols as f32;
    for row in list.chunks(cols) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = spacing::L;
            for g in row {
                ui.allocate_ui_with_layout(Vec2::new(cw, 0.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(cw);
                    goal_card(app, ui, form, g);
                });
            }
        });
        ui.add_space(spacing::L);
    }
    add_goal(app, ui, form);
}

fn goal_card(app: &mut App, ui: &mut egui::Ui, form: &mut Form, g: &Value) {
    let t = app.t.clone();
    let name = s(g, "name").to_string();
    let cur = app.m.f(&format!("goals.{name}.current"));
    let target = f(g, "target");
    let done = target > 0.0 && cur >= target;
    let label = if s(g, "label").is_empty() { nice(&name) } else { s(g, "label").to_string() };
    widgets::panel(ui, &t, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(&label).font(font_semibold(type_scale::LARGE)).color(t.fg));
                ui.label(RichText::new(format!("Counts {}", counts_label(s(g, "counts")).to_lowercase())).size(type_scale::SMALL + 0.5).color(t.text_dim));
            });
            if done {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    widgets::badge(ui, &t, "Reached!", t.green);
                });
            }
        });
        ui.add_space(spacing::M);
        ui.horizontal(|ui| {
            ui.label(RichText::new(show_num(cur)).font(font_bold(type_scale::DISPLAY)).color(t.fg));
            ui.label(RichText::new(format!("/ {}", show_num(target))).font(font_medium(type_scale::HEADING)).color(t.text_dim));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let pct = if target > 0.0 { (cur / target * 100.0).round() } else { 0.0 };
                ui.label(RichText::new(format!("{pct}%")).font(font_mono(type_scale::BODY)).color(t.text_dim));
            });
        });
        progress_bar(ui, &t, if target > 0.0 { (cur / target) as f32 } else { 0.0 }, if done { t.green } else { t.accent });
        ui.add_space(spacing::M);
        let open = form.goal_open.as_deref() == Some(name.as_str());
        ui.horizontal(|ui| {
            if widgets::button_ex(ui, &t, Some(icon::PLUS), "Add 1", Kind::Secondary, Size::Medium, 0.0, true).on_hover_text("Count one by hand").clicked() {
                act(app, "goals.add", Value::map().with("name", name.clone()).with("amount", 1.0));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::button_ex(
                    ui,
                    &t,
                    Some(if open { icon::UP } else { icon::EDIT }),
                    if open { "Close" } else { "Edit" },
                    Kind::Ghost,
                    Size::Medium,
                    0.0,
                    true,
                )
                .clicked()
                {
                    form.goal_open = if open { None } else { Some(name.clone()) };
                }
            });
        });
        if !open {
            return;
        }
        ui.add_space(spacing::M);
        ui.separator();
        ui.add_space(spacing::S);
        let mut buf = form.goal_set.get(&name).cloned().unwrap_or_default();
        ui.horizontal(|ui| {
            if ui.add(se_ui_kit::widgets::field(&mut buf).hint_text("Number").desired_width(90.0)).changed() {
                form.goal_set.insert(name.clone(), buf.clone());
            }
            let v = buf.trim().parse::<f64>();
            if widgets::button_ex(ui, &t, None, "Set progress", Kind::Secondary, Size::Small, 0.0, v.is_ok()).clicked() {
                act(app, "goals.set", Value::map().with("name", name.clone()).with("value", v.clone().unwrap_or(0.0)));
                form.goal_set.remove(&name);
            } else if widgets::button_ex(ui, &t, None, "Set target", Kind::Secondary, Size::Small, 0.0, v.as_ref().is_ok_and(|x| *x > 0.0)).clicked() {
                act(app, "goals.save", Value::map().with("name", name.clone()).with("fields", Value::map().with("target", v.unwrap_or(1.0))));
                form.goal_set.remove(&name);
            }
        });
        ui.add_space(spacing::S);
        ui.horizontal(|ui| {
            if widgets::hold_button(ui, &t, "Hold to start from 0", t.yellow, 0.6) {
                act(app, "goals.reset", Value::map().with("name", name.clone()));
            }
            if widgets::hold_button(ui, &t, "Hold to delete", t.bright_red, 0.8) {
                act(app, "goals.delete", Value::map().with("name", name.clone()));
                form.goal_open = None;
            }
        });
    });
}

fn add_goal(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    if !form.goal_adding {
        if widgets::button_ex(ui, &t, Some(icon::PLUS), "Add a goal", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
            form.goal_adding = true;
        }
        return;
    }
    widgets::titled(
        ui,
        &t,
        "Add a goal",
        "It shows as a progress bar on stream and keeps counting across streams.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let (label, counts, target) = &mut form.goal_new;
            if counts.is_empty() {
                *counts = "subs".into();
            }
            form_row(ui, &t, "Name", "What viewers see, e.g. \"New cymbal\".", |ui| {
                ui.add(se_ui_kit::widgets::field(label).desired_width(260.0));
            });
            form_row(ui, &t, "What it counts", "", |ui| {
                egui::ComboBox::from_id_salt("goal-counts").selected_text(counts_label(counts)).width(260.0).show_ui(ui, |ui| {
                    for (k, l) in COUNTS {
                        ui.selectable_value(counts, k.to_string(), l);
                    }
                });
            });
            form_row(ui, &t, "Target", "", |ui| {
                ui.add(se_ui_kit::widgets::field(target).hint_text("e.g. 50").desired_width(120.0));
            });
            let tv = target.trim().parse::<f64>();
            let slug: String =
                label.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect::<String>().trim_matches('_').to_string();
            let ok = !slug.is_empty() && tv.as_ref().is_ok_and(|x| *x > 0.0);
            let mut done = false;
            ui.horizontal(|ui| {
                if widgets::button_ex(ui, &t, Some(icon::PLUS), "Add goal", Kind::Primary, Size::Medium, 0.0, ok).clicked() {
                    let fields = Value::map().with("counts", counts.clone()).with("target", tv.clone().unwrap_or(1.0)).with("label", label.trim());
                    act(app, "goals.save", Value::map().with("name", slug.clone()).with("fields", fields));
                    done = true;
                }
                if widgets::button_ex(ui, &t, None, "Cancel", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                    done = true;
                }
            });
            if done {
                form.goal_new = Default::default();
                form.goal_adding = false;
            }
        },
    );
}

// ---- this stream ---------------------------------------------------------------------------------

fn stats(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let st = |a: &str| -> String {
        app.m
            .get(a)
            .map(|v| match v {
                Value::Str(s) => s.clone(),
                Value::Float(x) => fmt_num(*x),
                Value::Null => String::new(),
                other => other.to_string(),
            })
            .unwrap_or_default()
    };
    let n = |k: &str| app.m.f(&format!("stats.session.{k}"));
    let tiles = [
        (icon::HEART, show_num(n("follows")), "new followers"),
        (icon::STAR, show_num(n("subs")), "new subs"),
        (icon::GIFT, show_num(n("gifts")), "gifted subs"),
        (icon::BOLT, show_num(n("bits")), "bits cheered"),
        (icon::HEART, money(n("tips"), ""), "in tips"),
        (icon::USERS, show_num(n("raids")), "raids"),
    ];
    widgets::titled(
        ui,
        &t,
        "This stream so far",
        "",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let w = ui.available_width();
            let cols = ((w + spacing::M) / 200.0).floor().clamp(1.0, 6.0) as usize;
            let cw = (w - spacing::M * (cols - 1) as f32) / cols as f32;
            for row in tiles.chunks(cols) {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = spacing::M;
                    for (ic, v, l) in row {
                        ui.allocate_ui_with_layout(Vec2::new(cw, 0.0), Layout::top_down(Align::Min), |ui| {
                            egui::Frame::new().fill(t.surface_hi).corner_radius(CornerRadius::same(10)).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
                                ui.set_width(cw - 28.0);
                                ui.label(RichText::new(*ic).size(type_scale::LARGE).color(t.accent));
                                ui.label(RichText::new(v).font(font_bold(type_scale::TITLE)).color(t.fg));
                                ui.label(RichText::new(*l).color(t.text_dim));
                            });
                        });
                    }
                });
                ui.add_space(spacing::M);
            }
        },
    );
    ui.add_space(spacing::L);
    widgets::titled(
        ui,
        &t,
        "Your supporters",
        "The latest and biggest, for shout-outs and your overlays.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let rows: [(&str, &str, &str, &str); 12] = [
                ("Latest follower", "stats.latest.follow.user", "", ""),
                ("Latest sub", "stats.latest.sub.user", "", ""),
                ("Latest gifter", "stats.latest.gift.user", "stats.latest.gift.count", "subs"),
                ("Latest cheer", "stats.latest.cheer.user", "stats.latest.cheer.amount", "bits"),
                ("Latest tip", "stats.latest.tip.user", "stats.latest.tip.amount", "$"),
                ("Latest raid", "stats.latest.raid.user", "stats.latest.raid.viewers", "viewers"),
                ("Biggest cheer this stream", "stats.top.cheer.session.user", "stats.top.cheer.session.amount", "bits"),
                ("Biggest cheer ever", "stats.top.cheer.alltime.user", "stats.top.cheer.alltime.amount", "bits"),
                ("Biggest tip this stream", "stats.top.tip.session.user", "stats.top.tip.session.amount", "$"),
                ("Biggest tip ever", "stats.top.tip.alltime.user", "stats.top.tip.alltime.amount", "$"),
                ("Top gifter this stream", "stats.top.gift.session.user", "stats.top.gift.session.amount", "subs"),
                ("Top gifter ever", "stats.top.gift.alltime.user", "stats.top.gift.alltime.amount", "subs"),
            ];
            let w = ui.available_width();
            let cols = if w > 900.0 { 2 } else { 1 };
            let cw = (w - spacing::L * (cols - 1) as f32) / cols as f32;
            for row in rows.chunks(cols) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = spacing::L;
                    for (label, user, amount, unit) in row {
                        let who = st(user);
                        let amt = if amount.is_empty() { String::new() } else { st(amount) };
                        let value = match (who.is_empty(), amt.is_empty() || amt == "0") {
                            (true, _) => "Nobody yet".to_string(),
                            (false, true) => who.clone(),
                            (false, false) if *unit == "$" => format!("{who} · {}", money(amt.parse().unwrap_or(0.0), "")),
                            (false, false) => format!("{who} · {amt} {unit}"),
                        };
                        ui.allocate_ui_with_layout(Vec2::new(cw, 30.0), Layout::left_to_right(Align::Center), |ui| {
                            ui.set_width(cw);
                            ui.allocate_ui_with_layout(Vec2::new(220.0, 22.0), Layout::left_to_right(Align::Center), |ui| {
                                ui.set_width(220.0);
                                ui.add(egui::Label::new(RichText::new(*label).color(t.text_dim)).truncate());
                            });
                            ui.label(RichText::new(value).font(font_medium(type_scale::BODY)).color(if who.is_empty() { t.text_faint } else { t.fg }));
                        });
                    }
                });
            }
        },
    );
    ui.add_space(spacing::M);
    widgets::details(ui, &t, "stats-details", "Details", |ui| {
        ui.horizontal(|ui| {
            if widgets::hold_button(ui, &t, "Hold to remove test data", t.yellow, 1.0) {
                act(app, "stats.purge_simulated", Value::Null);
            }
            widgets::hint(ui, &t, "Removes everything the Test buttons added to these numbers, goals and top chatters.");
        });
    });
}

// ---- timing --------------------------------------------------------------------------------------

/// Queue policy fields: (key, source key in `alerts.config.queue`).
const POLICY: [(&str, &str); 10] = [
    ("min_spacing", "min_spacing_ms"),
    ("max_on_screen", "max_on_screen_ms"),
    ("max_queue", "max_queue"),
    ("interrupt", "interrupt"),
    ("interrupt_margin", "interrupt_margin"),
    ("on_interrupt", "on_interrupt"),
    ("pause_modes", "pause_modes"),
    ("veto_window", "veto_window_ms"),
    ("gift_window", "gift_window_ms"),
    ("max_message", "max_message"),
];

fn timing(app: &mut App, ui: &mut egui::Ui, form: &mut Form, cfg: &Value) {
    let t = app.t.clone();
    let q = cfg.get_path("queue").cloned().unwrap_or_default();
    let current = |src: &str| -> String {
        match q.get_path(src) {
            Some(Value::List(l)) => l.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
            Some(Value::Int(ms)) if src.ends_with("_ms") => dur(*ms),
            Some(Value::Str(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => String::new(),
        }
    };
    for (k, src) in POLICY {
        form.policy.entry(k.to_string()).or_insert_with(|| current(src));
    }
    let mut enabled = app.m.get("alerts.enabled").is_none_or(Value::truthy);
    widgets::panel(ui, &t, |ui| {
        ui.set_width(ui.available_width());
        if widgets::toggle_row(ui, &t, "Alerts on stream", "Off: nothing pops up, but follows, subs and cheers are still counted.", &mut enabled).changed() {
            app.m.command(Op::Set { address: "alerts.enabled".into(), value: Value::Bool(enabled) });
        }
    });
    ui.add_space(spacing::L);
    widgets::titled(
        ui,
        &t,
        "Timing",
        "How alerts take turns on screen.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let sw = (ui.available_width() - 380.0).clamp(160.0, 420.0);
            let secs_row = |ui: &mut egui::Ui, form: &mut Form, key: &str, label: &str, help: &str, range: std::ops::RangeInclusive<f32>| {
                form_row(ui, &t, label, help, |ui| {
                    let buf = form.policy.get_mut(key).expect("filled above");
                    let mut v = parse_secs(buf).unwrap_or(0.0) as f32;
                    ui.spacing_mut().slider_width = sw;
                    if ui.add(egui::Slider::new(&mut v, range).step_by(0.5).custom_formatter(|v, _| secs_label(v))).changed() {
                        *buf = secs_text(v as f64);
                    }
                });
            };
            secs_row(ui, form, "min_spacing", "Pause between alerts", "A short breather so alerts don't run into each other.", 0.0..=10.0);
            secs_row(ui, form, "max_on_screen", "Longest an alert can stay up", "Even big ones end after this.", 3.0..=60.0);
            secs_row(
                ui,
                form,
                "veto_window",
                "Time to skip alerts with messages",
                "Alerts where the viewer wrote something wait this long so you or a mod can skip them.",
                0.0..=15.0,
            );
            secs_row(ui, form, "gift_window", "Group gifted subs within", "Gifts arriving close together become one alert.", 1.0..=15.0);
            form_row(ui, &t, "Big alerts cut in", "A much bigger alert can interrupt a small one.", |ui| {
                let buf = form.policy.get_mut("interrupt").expect("filled above");
                let mut on = buf.trim() == "true";
                if widgets::toggle(ui, &t, &mut on).changed() {
                    *buf = on.to_string();
                }
            });
            form_row(ui, &t, "Interrupted alerts play again", "Off: an interrupted alert is dropped.", |ui| {
                let buf = form.policy.get_mut("on_interrupt").expect("filled above");
                let mut on = buf.trim() != "drop";
                if widgets::toggle(ui, &t, &mut on).changed() {
                    *buf = if on { "requeue".into() } else { "drop".into() };
                }
            });
            form_row(ui, &t, "Wait during", "Alerts are held and play afterwards.", |ui| {
                let buf = form.policy.get_mut("pause_modes").expect("filled above");
                let mut modes: Vec<String> = buf.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
                let mut changed = false;
                let mut known: Vec<(String, String)> = vec![("ad_break".into(), "Ad breaks".into()), ("brb".into(), "Be right back".into())];
                for m in &modes {
                    if !known.iter().any(|(k, _)| k == m) {
                        known.push((m.clone(), nice(m)));
                    }
                }
                ui.vertical(|ui| {
                    for (m, label) in &known {
                        let mut on = modes.contains(m);
                        if ui.checkbox(&mut on, label.as_str()).changed() {
                            changed = true;
                            if on {
                                modes.push(m.clone());
                            } else {
                                modes.retain(|x| x != m);
                            }
                        }
                    }
                });
                if changed {
                    *buf = modes.join(", ");
                }
            });
            widgets::details(ui, &t, "timing-details", "Details", |ui| {
                for (key, label, help) in [
                    ("max_queue", "Most alerts that can wait", "Beyond this, the smallest new ones are dropped."),
                    ("interrupt_margin", "How much bigger to cut in", "Priority difference needed to interrupt."),
                    ("max_message", "Longest viewer message", "Characters shown on an alert."),
                ] {
                    form_row(ui, &t, label, help, |ui| {
                        let buf = form.policy.get_mut(key).expect("filled above");
                        ui.add(se_ui_kit::widgets::field(buf).desired_width(120.0));
                    });
                }
            });
            ui.add_space(spacing::S);
            let dirty = POLICY.iter().any(|(k, src)| form.policy.get(*k).is_some_and(|v| *v != current(src)));
            ui.horizontal(|ui| {
                if widgets::button_ex(ui, &t, Some(icon::CHECK), "Save settings", Kind::Primary, Size::Medium, 0.0, dirty).clicked() {
                    let mut fields = Value::map();
                    let mut bad = None;
                    for (k, src) in POLICY {
                        let Some(text) = form.policy.get(k) else { continue };
                        if *text == current(src) {
                            continue;
                        }
                        let tx = text.trim();
                        let v = match k {
                            "max_queue" | "interrupt_margin" | "max_message" => {
                                tx.parse::<i64>().map(Value::Int).map_err(|_| format!("\"{tx}\" isn't a whole number"))
                            }
                            "interrupt" => Ok(Value::Bool(matches!(tx, "true" | "yes" | "on" | "1"))),
                            "pause_modes" => Ok(Value::List(tx.split(',').map(str::trim).filter(|x| !x.is_empty()).map(|x| Value::Str(x.into())).collect())),
                            _ => Ok(Value::Str(tx.into())),
                        };
                        match v {
                            Ok(v) => fields = fields.with(k, v),
                            Err(e) => bad = Some(e),
                        }
                    }
                    match bad {
                        Some(e) => app.m.toast(e, true),
                        None => {
                            act(app, "alerts.policy", Value::map().with("fields", fields));
                            form.policy.clear();
                        }
                    }
                }
                if dirty && widgets::button_ex(ui, &t, Some(icon::UNDO), "Undo changes", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                    form.policy.clear();
                }
                if !dirty {
                    widgets::hint(ui, &t, "Everything is saved.");
                }
            });
        },
    );
}
