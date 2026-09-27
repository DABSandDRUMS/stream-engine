//! Alerts and goals (§14.1–14.2), two views in the list-detail grammar:
//! - [`ui`] (Automation → Alerts): the alerts, grouped by event, and the selected one's
//!   inspector (when, content and variations, sound & voice, duration & priority, also run).
//!   The list's first row, "Look & timing", opens the source that draws them, how they take
//!   turns, and the live queue (on screen, waiting with the mod-skip countdown, shown recently).
//! - [`goals_ui`] (Community → Goals): goals as a list with progress, plus this stream's numbers.
//!
//! Data: queries `alerts`, `alerts.config`, `project.assets`, `tts`, `patches`, `scenes`,
//! `project.read` (project.toml); state `alerts.*`, `stats.*`, `goals.*`, `patch.*.state`;
//! commands: `alerts.veto|approve|skip|pause|resume|clear|replay`, `alerts.edit|add|remove|policy`,
//! `goals.set|add|reset|save|delete`, `stats.purge_simulated`, `patch.enable|disable`, `sim.*`.

use crate::app::App;
use crate::views::live::nice;
use crate::views::rail::money;
use crate::views::rules::steps_editor;
use egui::{Align, Color32, CornerRadius, Layout, RichText, Sense, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_bold, font_medium, font_mono, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, Tone, icon};
use std::collections::BTreeMap;

/// Every look field an alert or variation can set (the `do` steps are kept apart, as a list).
const LOOK: [&str; 10] = ["title", "message", "sound", "image", "duration", "priority", "tts", "tts_text", "voice", "interrupt"];

/// Blank variation fields fall back to the alert's own.
const SAME: &str = "Same as main alert";

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

/// True when `every` seconds passed since `last` (or it never ran); then restarts the clock.
fn poll(ui: &egui::Ui, last: &mut f64, every: f64) -> bool {
    let now = ui.input(|i| i.time);
    if *last == 0.0 || now - *last > every {
        *last = now;
        true
    } else {
        false
    }
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

/// `text` shortened to `max` characters with an ellipsis.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max { text.to_string() } else { format!("{}…", text.chars().take(max.saturating_sub(1)).collect::<String>().trim_end()) }
}

/// `images/cheer.gif` → `cheer.gif`.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Dim helper text that wraps inside a row.
fn note(ui: &mut egui::Ui, t: &Theme, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(type_scale::SMALL + 0.5).color(t.text_dim)).wrap());
}

/// Text form of a look field from `alerts.config`.
fn look_text(look: &Value, k: &str) -> String {
    match k {
        "duration" => dur(i(look, "duration_ms")),
        "title" | "message" | "tts_text" => look.get_path(k).and_then(Value::as_str).map(to_words).unwrap_or_default(),
        _ => match look.get_path(k) {
            Some(Value::Str(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => v.to_string(),
        },
    }
}

/// The `do` steps of a look.
fn look_cmds(look: &Value) -> Vec<String> {
    look.get_path("do").and_then(Value::as_list).map(|l| l.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
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
        "tts" | "veto" | "interrupt" => Value::Bool(matches!(t, "true" | "yes" | "on" | "1")),
        "title" | "message" | "tts_text" => Value::Str(from_words(t)),
        "name" => Value::Str(t.replace(' ', "_")),
        _ => Value::Str(t.into()),
    })
}

/// The `do` list as saved: the steps in order; none = remove the field.
fn cmds_value(cmds: &[String]) -> Value {
    let l: Vec<Value> = cmds.iter().map(|c| c.trim()).filter(|c| !c.is_empty()).map(|c| Value::Str(c.into())).collect();
    if l.is_empty() { Value::Null } else { Value::List(l) }
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
    /// (button label, simulator command); the first is the plain test.
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

/// List groups, in order; [`family`] picks one per event.
const FAMILIES: [&str; 5] = ["Followers & subs", "Bits & tips", "Raids", "Channel points", "Other"];

fn family(when: &str) -> usize {
    match when {
        "twitch.follow" | "twitch.sub" | "twitch.resub" | "twitch.gift" | "twitch.gift_bomb" => 0,
        "twitch.cheer" | "tip" | "kofi.tip" => 1,
        "twitch.raid" => 2,
        "twitch.redeem" => 3,
        _ => 4,
    }
}

/// An event in words: `twitch.cheer` → "Cheer", `twitch.hype_train` → "Twitch hype train".
fn event_label(when: &str) -> String {
    let k = kind_of(when);
    if !k.label.is_empty() {
        k.label.to_string()
    } else if when.trim().is_empty() {
        "No event".into()
    } else {
        nice(&when.replace('.', " "))
    }
}

/// One line on when an alert shows.
fn what_of(when: &str) -> String {
    let k = kind_of(when);
    if k.what.is_empty() { format!("When {} happens", event_label(when).to_lowercase()) } else { k.what.to_string() }
}

/// An alert's display name: its kind ("Cheer"), or its own name when that says more ("Big cheer").
fn alert_label(a: &Value) -> String {
    let k = kind_of(s(a, "when"));
    let own = nice(s(a, "name"));
    if k.label.is_empty() || (!own.is_empty() && !own.eq_ignore_ascii_case(k.label)) { own } else { k.label.to_string() }
}

fn alert_key(a: &Value) -> String {
    format!("{}|{}|", s(a, "file"), i(a, "index"))
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

/// When a variation is used, in words.
fn used_when(k: &AlertKind, cond: &str) -> String {
    match (k.unit, min_amount(cond)) {
        (Some(unit), Some(n)) if n > 0.0 => format!("For {} {unit} or more", show_num(n)),
        _ if cond.trim().is_empty() => "Always".to_string(),
        _ => format!("When {cond}"),
    }
}

fn sound_name(s: &str) -> String {
    nice(s.strip_prefix("alert_").unwrap_or(s))
}

// ---- Automation → Alerts ------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Form {
    /// Selected alert (`file|index|`).
    selected: Option<String>,
    /// "Look & timing" is open in the detail pane instead of an alert.
    look: bool,
    /// New alert being set up.
    draft: Option<Draft>,
    /// Just created (file, name): selected once it shows up in `alerts.config`.
    pending: Option<(String, String)>,
    /// Text edits keyed by `file|index|field` (variations: `file|index|vN|field`).
    edits: BTreeMap<String, String>,
    /// `do` steps being edited, keyed by the same prefixes.
    cmds: BTreeMap<String, Vec<String>>,
    /// New variation per alert prefix: (name, condition as typed).
    new_var: BTreeMap<String, (String, String)>,
    last_query: f64,
    last_slow: f64,
    last_assets: f64,
}

#[derive(Clone, Default)]
struct Draft {
    /// Index into [`NEW_KINDS`]; `NEW_KINDS.len()` = another event; `None` = still choosing.
    kind: Option<usize>,
    /// The event name, for another event.
    event: String,
    name: String,
    title: String,
    file: String,
}

/// Choices for the pickers, gathered once per frame.
struct Choices {
    sounds: Vec<String>,
    /// Pictures as stored in an alert (relative to `assets/`).
    images: Vec<String>,
    voices: Vec<String>,
}

fn choices(app: &App, alerts: &[Value]) -> Choices {
    let assets = app.m.q_list("project.assets");
    let mut sounds: Vec<String> = assets
        .iter()
        .filter(|a| s(a, "kind") == "sounds")
        .map(|a| if s(a, "sound").is_empty() { s(a, "name").to_string() } else { s(a, "sound").to_string() })
        .filter(|x| !x.is_empty())
        .collect();
    for a in alerts {
        let looks =
            std::iter::once(a.get_path("look")).chain(a.get_path("variations").and_then(Value::as_list).unwrap_or(&[]).iter().map(|v| v.get_path("look")));
        sounds.extend(looks.filter_map(|l| l.and_then(|l| l.get_path("sound")).and_then(Value::as_str)).filter(|x| !x.is_empty()).map(String::from));
    }
    sounds.sort();
    sounds.dedup();
    let mut images: Vec<String> = assets
        .iter()
        .filter(|a| match s(a, "kind") {
            "images" => true,
            "video" => {
                let p = s(a, "path").to_lowercase();
                p.ends_with(".gif") || p.ends_with(".webm")
            }
            _ => false,
        })
        .map(|a| s(a, "path").strip_prefix("assets/").unwrap_or(s(a, "path")).to_string())
        .collect();
    images.sort();
    images.dedup();
    let voices = app
        .m
        .q("tts")
        .and_then(|q| q.get_path("voices"))
        .and_then(Value::as_list)
        .map(|l| l.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default();
    Choices { sounds, images, voices }
}

/// One row of the alerts list.
struct Row {
    key: String,
    family: usize,
    icon: &'static str,
    title: String,
    sub: String,
    trailing: String,
}

fn list_rows(list: &[Value]) -> Vec<Row> {
    list.iter()
        .map(|a| {
            let when = s(a, "when");
            let k = kind_of(when);
            let nv = a.get_path("variations").and_then(Value::as_list).map(<[Value]>::len).unwrap_or(0);
            let trailing = if !a.get_path("enabled").is_some_and(Value::truthy) {
                "Off".to_string()
            } else if nv > 0 {
                format!("{nv} variation{}", if nv == 1 { "" } else { "s" })
            } else {
                String::new()
            };
            let tpl = a.get_path("look.title").and_then(Value::as_str).unwrap_or("{user}");
            let sub = clip(&sample(tpl, &k), if trailing.is_empty() { 34 } else { 24 });
            Row { key: alert_key(a), family: family(when), icon: k.icon, title: alert_label(a), sub, trailing }
        })
        .collect()
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("alerts-form");
    let mut form: Form = ui.data_mut(|d| d.get_temp::<Form>(id)).unwrap_or_default();
    if poll(ui, &mut form.last_query, 0.25) {
        app.m.query("alerts", Value::Null);
    }
    if poll(ui, &mut form.last_slow, 2.0) {
        app.m.query("alerts.config", Value::Null);
    }
    if poll(ui, &mut form.last_assets, 5.0) {
        app.m.query("project.assets", Value::Null);
        app.m.query("tts", Value::Null);
    }
    let live = app.m.q("alerts").cloned().unwrap_or_default();
    let cfg = app.m.q("alerts.config").cloned();
    let loaded = cfg.is_some();
    let cfg = cfg.unwrap_or_default();
    let list: Vec<Value> = cfg.get_path("alerts").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    if let Some((file, name)) = form.pending.clone()
        && let Some(a) = list.iter().find(|a| s(a, "file") == file && s(a, "name") == name)
    {
        form.selected = Some(alert_key(a));
        form.pending = None;
    }
    let selected = form.selected.as_ref().and_then(|k| list.iter().find(|a| alert_key(a) == *k)).cloned();
    if loaded && selected.is_none() {
        form.selected = None;
    }
    let ch = choices(app, &list);
    let rows = list_rows(&list);
    let sel_key = form.selected.clone();

    status_bar(app, ui, &live, loaded.then_some(list.len()));
    ui.add_space(spacing::M);
    let look = form.look;
    let ((pick, create, open_look), _) = widgets::split(
        ui,
        300.0,
        |ui| alert_list(ui, &t, &rows, sel_key.as_deref(), look),
        |ui| {
            if look {
                delivery_ui(app, ui);
                return;
            }
            egui::ScrollArea::vertical().id_salt("alerts-detail").auto_shrink([false, false]).show(ui, |ui| {
                if form.draft.is_some() {
                    new_alert(app, ui, &mut form, &cfg);
                } else if let Some(a) = &selected {
                    alert_detail(app, ui, &mut form, a, &ch);
                } else if widgets::empty_state(
                    ui,
                    &t,
                    icon::ALERT,
                    "Alerts",
                    "A pop-up on stream when someone follows, subs, cheers, raids or tips.",
                    Some("New alert"),
                ) {
                    form.draft = Some(Draft::default());
                }
            });
        },
    );
    if create {
        form.draft = Some(Draft::default());
        form.selected = None;
        form.look = false;
    }
    if let Some(k) = pick {
        form.selected = Some(k);
        form.draft = None;
        form.look = false;
    }
    if open_look {
        form.look = true;
        form.selected = None;
        form.draft = None;
    }
    ui.data_mut(|d| d.insert_temp(id, form));
}

/// The list pane: "Look & timing", then the alerts grouped by event. Returns (clicked alert,
/// "+" clicked, "Look & timing" clicked).
fn alert_list(ui: &mut egui::Ui, t: &Theme, rows: &[Row], selected: Option<&str>, look: bool) -> (Option<String>, bool, bool) {
    let create = widgets::pane_header(ui, t, "Alerts", Some(rows.len()), Some("New alert"));
    let mut pick = None;
    let mut open_look = false;
    egui::ScrollArea::vertical().id_salt("alerts-list").auto_shrink([false, false]).show(ui, |ui| {
        open_look = widgets::list_row(ui, t, icon::SLIDERS, "Look & timing", "Where alerts show, turns, the queue", "", look).clicked();
        if rows.is_empty() {
            widgets::hint(ui, t, "No alerts yet.");
        }
        for (fi, fam) in FAMILIES.iter().enumerate() {
            let mut group = rows.iter().filter(|r| r.family == fi).peekable();
            if group.peek().is_none() {
                continue;
            }
            widgets::group_label(ui, t, fam);
            for r in group {
                if widgets::list_row(ui, t, r.icon, &r.title, &r.sub, &r.trailing, selected == Some(r.key.as_str())).clicked() {
                    pick = Some(r.key.clone());
                }
            }
        }
    });
    (pick, create, open_look)
}

/// Show whether configured alerts can be displayed; never imply a blank show has live alerts.
fn status_bar(app: &mut App, ui: &mut egui::Ui, live: &Value, configured: Option<usize>) {
    let t = app.t.clone();
    let paused = live.get_path("paused").is_some_and(Value::truthy);
    let enabled = app.m.get("alerts.enabled").is_none_or(Value::truthy);
    let health = app.m.get("health.alerts").cloned().unwrap_or_default();
    // One control: the button says what it does next.
    if configured == Some(0) {
        widgets::callout(
            ui,
            &t,
            Tone::Warn,
            icon::ALERT,
            "No alerts set up",
            "Create an alert below, then add a source that shows alerts to your scenes.",
            None,
        );
        return;
    }
    if configured.is_none() {
        widgets::hint(ui, &t, "Waiting for alert settings…");
        return;
    }
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

// ---- new alert -----------------------------------------------------------------------------------

fn new_alert(app: &mut App, ui: &mut egui::Ui, form: &mut Form, cfg: &Value) {
    let t = app.t.clone();
    let files: Vec<String> = cfg
        .get_path("files")
        .and_then(Value::as_list)
        .map(|l| l.iter().filter_map(Value::as_str).filter(|f| !f.ends_with("queue.toml")).map(String::from).collect())
        .unwrap_or_default();
    let Some(d) = form.draft.as_mut() else { return };
    if d.file.is_empty() {
        d.file = "alerts/custom.toml".into();
    }
    let other = NEW_KINDS.len();
    let when = match d.kind {
        Some(n) if n < other => NEW_KINDS[n].0.to_string(),
        Some(_) => d.event.trim().to_string(),
        None => String::new(),
    };
    let chosen = d.kind.is_some();
    let ok = !when.is_empty() && !d.name.trim().is_empty();
    let sub = if !chosen {
        "Pick what it's for. Nothing is saved until you create it.".to_string()
    } else if d.kind == Some(other) {
        "When another event happens".to_string()
    } else {
        what_of(&when)
    };
    let (mut create, mut cancel) = (false, false);
    widgets::detail_header(ui, &t, if chosen { kind_of(&when).icon } else { icon::ALERT }, "New alert", &sub, |ui| {
        if chosen {
            create = widgets::button_ex(ui, &t, Some(icon::CHECK), "Create", Kind::Primary, Size::Medium, 0.0, ok).clicked();
        }
        cancel = widgets::button_ex(ui, &t, None, "Cancel", Kind::Ghost, Size::Medium, 0.0, true).clicked();
    });
    widgets::group_label(ui, &t, "What it's for");
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(spacing::S);
        for (n, (w, l)) in NEW_KINDS.iter().enumerate() {
            let k = kind_of(w);
            if widgets::kind_tile(ui, &t, k.icon, l, k.what, d.kind == Some(n)).clicked() {
                d.kind = Some(n);
            }
        }
        if widgets::kind_tile(ui, &t, icon::ALERT, "Other event", "Any other event, by its name.", d.kind == Some(other)).clicked() {
            d.kind = Some(other);
        }
    });
    if chosen {
        ui.add_space(spacing::L);
        widgets::inspector_section(
            ui,
            &t,
            "alert-new-form",
            "Alert",
            true,
            |_| {},
            |ui| {
                if d.kind == Some(other) {
                    widgets::prop_row(ui, &t, "Event", |ui| {
                        let w = ui.available_width().min(360.0);
                        ui.add(widgets::field(&mut d.event).font(egui::TextStyle::Monospace).hint_text("Event name, like twitch.follow").desired_width(w));
                    });
                }
                widgets::prop_row(ui, &t, "Name", |ui| {
                    let w = ui.available_width().min(360.0);
                    ui.add(widgets::field(&mut d.name).hint_text("A short name, like Big cheer").desired_width(w));
                });
                widgets::prop_row(ui, &t, "Headline", |ui| {
                    let w = ui.available_width().min(460.0);
                    ui.add(widgets::field(&mut d.title).hint_text("(their name)").desired_width(w));
                });
                widgets::prop_row(ui, &t, "", |ui| {
                    note(ui, &t, "Type (their name), (amount) or (message) and the viewer's details are filled in. You can set the rest after creating it.");
                });
                widgets::details(ui, &t, "alert-new-details", "Details", |ui| {
                    widgets::prop_row(ui, &t, "Save in", |ui| {
                        egui::ComboBox::from_id_salt("alerts-new-file").selected_text(d.file.clone()).width(240.0).show_ui(ui, |ui| {
                            for f in &files {
                                ui.selectable_value(&mut d.file, f.clone(), f);
                            }
                            if !files.iter().any(|f| f == "alerts/custom.toml") {
                                ui.selectable_value(&mut d.file, "alerts/custom.toml".to_string(), "alerts/custom.toml");
                            }
                        });
                    });
                });
            },
        );
    }
    let add = (create && ok).then(|| {
        let name = d.name.trim().replace(' ', "_");
        let mut fields = Value::map().with("name", name.clone()).with("when", when.clone());
        if !d.title.trim().is_empty() {
            fields = fields.with("title", from_words(d.title.trim()));
        }
        (d.file.clone(), name, fields)
    });
    if let Some((file, name, fields)) = add {
        act(app, "alerts.add", Value::map().with("file", file.clone()).with("path", Value::List(vec![])).with("key", "alert").with("fields", fields));
        form.pending = Some((file, name));
        form.draft = None;
        form.last_slow = 0.0;
    } else if cancel {
        form.draft = None;
    }
}

// ---- alert inspector -----------------------------------------------------------------------------

/// (field, saved text) for everything the alert's Save button writes (besides `do`).
fn alert_originals(a: &Value, look: &Value) -> Vec<(String, String)> {
    let mut o: Vec<(String, String)> =
        vec![("when".into(), s(a, "when").into()), ("if".into(), s(a, "if").into()), ("veto".into(), a.get_path("veto").is_none_or(Value::truthy).to_string())];
    o.extend(LOOK.iter().map(|k| (k.to_string(), look_text(look, k))));
    o
}

/// `v0|title` (a variation's field) vs `title` (the alert's own).
fn is_version_key(rest: &str) -> bool {
    rest.strip_prefix('v').and_then(|r| r.chars().next()).is_some_and(|c| c.is_ascii_digit())
}

fn is_dirty(form: &Form, prefix: &str, originals: &[(String, String)], orig_cmds: &[String]) -> bool {
    originals.iter().any(|(k, o)| form.edits.get(&format!("{prefix}{k}")).is_some_and(|x| x != o))
        || form.cmds.get(prefix).is_some_and(|c| c.as_slice() != orig_cmds)
}

/// Forget the edits under `prefix` (an alert's keeps its variations' edits).
fn drop_edits(form: &mut Form, prefix: &str) {
    form.edits.retain(|k, _| k.strip_prefix(prefix).is_none_or(is_version_key));
    form.cmds.remove(prefix);
}

fn save_fields(app: &mut App, form: &mut Form, file: &str, path: Value, prefix: &str, originals: &[(String, String)], orig_cmds: &[String]) {
    let mut fields = Value::map();
    let mut n = 0;
    for (k, orig) in originals {
        let Some(text) = form.edits.get(&format!("{prefix}{k}")) else { continue };
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
    if let Some(cmds) = form.cmds.get(prefix)
        && cmds.as_slice() != orig_cmds
    {
        fields = fields.with("do", cmds_value(cmds));
        n += 1;
    }
    if n == 0 {
        return;
    }
    act(app, "alerts.edit", Value::map().with("file", file).with("path", path).with("fields", fields));
    drop_edits(form, prefix);
    form.last_slow = 0.0;
}

fn look_buf<'a>(edits: &'a mut BTreeMap<String, String>, prefix: &str, look: &Value, k: &str) -> &'a mut String {
    edits.entry(format!("{prefix}{k}")).or_insert_with(|| look_text(look, k))
}

fn alert_detail(app: &mut App, ui: &mut egui::Ui, form: &mut Form, a: &Value, ch: &Choices) {
    let t = app.t.clone();
    let saved_when = s(a, "when").to_string();
    let k = kind_of(&saved_when);
    let file = s(a, "file").to_string();
    let idx = i(a, "index");
    let prefix = alert_key(a);
    let path = Value::List(vec!["alert".into(), Value::Int(idx)]);
    let look = a.get_path("look").cloned().unwrap_or_default();
    let originals = alert_originals(a, &look);
    let orig_cmds = look_cmds(&look);
    let dirty = is_dirty(form, &prefix, &originals, &orig_cmds);
    let mut enabled = a.get_path("enabled").is_some_and(Value::truthy);
    let (mut save, mut undo) = (false, false);
    widgets::detail_header(ui, &t, k.icon, &format!("{} alert", alert_label(a)), &what_of(&saved_when), |ui| {
        save = widgets::button_ex(ui, &t, Some(icon::CHECK), "Save", Kind::Primary, Size::Medium, 0.0, dirty).clicked();
        if dirty {
            undo = widgets::button_ex(ui, &t, Some(icon::UNDO), "Undo", Kind::Ghost, Size::Medium, 0.0, true).clicked();
        }
        ui.add_space(spacing::S);
        if widgets::toggle(ui, &t, &mut enabled).on_hover_text(if enabled { "On: shows on stream" } else { "Off: counted, but nothing pops up" }).changed() {
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
    if undo {
        drop_edits(form, &prefix);
    }
    if save {
        save_fields(app, form, &file, path.clone(), &prefix, &originals, &orig_cmds);
    }

    widgets::inspector_section(
        ui,
        &t,
        "alert-sec-when",
        "When",
        true,
        |_| {},
        |ui| {
            when_rows(ui, &t, &mut form.edits, &prefix, &originals);
        },
    );
    let when_now = form.edits.get(&format!("{prefix}when")).cloned().unwrap_or_else(|| saved_when.clone());
    widgets::inspector_section(
        ui,
        &t,
        "alert-sec-content",
        "Content",
        true,
        |_| {},
        |ui| {
            content_rows(ui, &t, &mut form.edits, &prefix, &look, false, &ch.images);
            let get = |f: &str| form.edits.get(&format!("{prefix}{f}")).cloned().unwrap_or_default();
            let (title, msg, image) = (get("title"), get("message"), get("image"));
            widgets::prop_row(ui, &t, "Preview", |ui| preview(ui, &t, &kind_of(&when_now), &title, &msg, &image));
            ui.add_space(spacing::S);
            variations(app, ui, form, a, &k, ch, &when_now);
        },
    );
    widgets::inspector_section(
        ui,
        &t,
        "alert-sec-sound",
        "Sound & voice",
        true,
        |_| {},
        |ui| {
            sound_rows(ui, &t, &mut form.edits, &prefix, &look, false, ch);
        },
    );
    widgets::inspector_section(
        ui,
        &t,
        "alert-sec-timing",
        "Duration & priority",
        true,
        |_| {},
        |ui| {
            timing_rows(ui, &t, &mut form.edits, &prefix, &look, false);
        },
    );
    widgets::inspector_section(
        ui,
        &t,
        "alert-sec-do",
        "Also run",
        true,
        |_| {},
        |ui| {
            widgets::hint(ui, &t, "Steps that run when this alert shows on stream.");
            ui.add_space(spacing::XS);
            let mut cmds = form.cmds.get(&prefix).cloned().unwrap_or_else(|| orig_cmds.clone());
            if steps_editor(app, ui, &format!("alert-do-{prefix}"), &mut cmds, &when_now) {
                form.cmds.insert(prefix.clone(), cmds);
            }
        },
    );
    widgets::inspector_section(
        ui,
        &t,
        "alert-sec-try",
        "Try it",
        true,
        |_| {},
        |ui| {
            if k.tests.is_empty() {
                widgets::hint(ui, &t, "There's no test for this event.");
                return;
            }
            if dirty {
                widgets::hint(ui, &t, "Tests use the saved alert. Save first to try your changes.");
            }
            ui.horizontal_wrapped(|ui| {
                for (l, cmd) in k.tests {
                    if widgets::button_ex(ui, &t, Some(icon::PLAY), l, Kind::Secondary, Size::Medium, 0.0, true)
                        .on_hover_text("Shows a pretend one on stream")
                        .clicked()
                    {
                        app.m.text(cmd);
                    }
                }
            });
        },
    );
    let mut deleted = false;
    widgets::inspector_section(
        ui,
        &t,
        "alert-sec-details",
        "Details",
        false,
        |_| {},
        |ui| {
            widgets::prop_row(ui, &t, "Name", |ui| {
                ui.label(RichText::new(s(a, "name")).font(font_mono(type_scale::SMALL)).color(t.text_dim));
            });
            widgets::prop_row(ui, &t, "Saved in", |ui| {
                ui.label(RichText::new(&file).font(font_mono(type_scale::SMALL)).color(t.text_dim));
            });
            ui.add_space(spacing::S);
            if widgets::hold_button(ui, &t, "Hold to delete alert", t.bright_red, 0.8) {
                act(app, "alerts.remove", Value::map().with("file", file.clone()).with("path", Value::List(vec![])).with("key", "alert").with("index", idx));
                deleted = true;
            }
        },
    );
    if deleted {
        // Later alerts in the file move up one: forget everything typed for this file.
        let fp = format!("{file}|");
        form.edits.retain(|k, _| !k.starts_with(&fp));
        form.cmds.retain(|k, _| !k.starts_with(&fp));
        form.new_var.retain(|k, _| !k.starts_with(&fp));
        form.selected = None;
        form.last_slow = 0.0;
    }
}

/// Event, condition and mod skip.
fn when_rows(ui: &mut egui::Ui, t: &Theme, edits: &mut BTreeMap<String, String>, prefix: &str, originals: &[(String, String)]) {
    let orig = |k: &str| originals.iter().find(|(o, _)| o == k).map(|(_, v)| v.clone()).unwrap_or_default();
    widgets::prop_row(ui, t, "Event", |ui| {
        let b = edits.entry(format!("{prefix}when")).or_insert_with(|| orig("when"));
        let cur = b.clone();
        egui::ComboBox::from_id_salt(("alert-event", prefix)).selected_text(event_label(&cur)).width(240.0).show_ui(ui, |ui| {
            if !cur.is_empty() && !NEW_KINDS.iter().any(|(w, _)| *w == cur) {
                ui.selectable_value(&mut *b, cur.clone(), event_label(&cur));
            }
            for (w, l) in NEW_KINDS {
                ui.selectable_value(&mut *b, w.to_string(), l);
            }
        });
    });
    let k = kind_of(edits.get(&format!("{prefix}when")).map(String::as_str).unwrap_or(""));
    let b = edits.entry(format!("{prefix}if")).or_insert_with(|| orig("if"));
    match (k.unit, min_amount(b)) {
        (Some(unit), Some(min)) => widgets::prop_row(ui, t, "At least", |ui| {
            let mut v = min;
            if ui.add(egui::DragValue::new(&mut v).range(0.0..=1_000_000.0).speed(1.0).suffix(format!(" {unit}"))).changed() {
                *b = if v > 0.0 { format!("amount >= {}", fmt_num(v)) } else { String::new() };
            }
            note(ui, t, "Smaller ones don't get this alert. 0 = every one.");
        }),
        (unit, _) => widgets::prop_row(ui, t, "Only if", |ui| {
            let w = ui.available_width().min(360.0);
            ui.add(widgets::field(&mut *b).font(egui::TextStyle::Monospace).hint_text("Always, or e.g. tier == 3").desired_width(w));
            if unit.is_some() && widgets::button_ex(ui, t, None, "Use a minimum", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                b.clear();
            }
        }),
    }
    widgets::prop_row(ui, t, "Let mods skip", |ui| {
        let b = edits.entry(format!("{prefix}veto")).or_insert_with(|| orig("veto"));
        let mut on = b.trim() == "true";
        if widgets::toggle(ui, t, &mut on).changed() {
            *b = on.to_string();
        }
        note(ui, t, "When they wrote a message, it waits a moment so you or a mod can skip it.");
    });
}

/// Headline, second line and picture. `inherit`: a variation (blank = the alert's own).
fn content_rows(ui: &mut egui::Ui, t: &Theme, edits: &mut BTreeMap<String, String>, prefix: &str, look: &Value, inherit: bool, images: &[String]) {
    widgets::prop_row(ui, t, "Headline", |ui| {
        let w = ui.available_width().min(460.0);
        let b = look_buf(edits, prefix, look, "title");
        ui.add(widgets::field(b).hint_text(if inherit { SAME } else { "(their name)" }).desired_width(w));
    });
    widgets::prop_row(ui, t, "Second line", |ui| {
        let w = ui.available_width().min(460.0);
        let b = look_buf(edits, prefix, look, "message");
        ui.add(widgets::field(b).hint_text(if inherit { SAME } else { "(message)" }).desired_width(w));
    });
    if !inherit {
        widgets::prop_row(ui, t, "", |ui| {
            note(ui, t, "Type (their name), (amount), (message), (months) or (tier) and the viewer's details are filled in.");
        });
    }
    widgets::prop_row(ui, t, "Picture", |ui| {
        let b = look_buf(edits, prefix, look, "image");
        let empty = if inherit { SAME } else { "No picture" };
        let cur = b.trim().to_string();
        let shown = if cur.is_empty() { empty.to_string() } else { file_name(&cur).to_string() };
        egui::ComboBox::from_id_salt(("alert-image", prefix)).selected_text(shown).width(240.0).show_ui(ui, |ui| {
            if ui.selectable_label(cur.is_empty(), empty).clicked() {
                b.clear();
            }
            if !cur.is_empty() && !images.contains(&cur) {
                let _ = ui.selectable_label(true, file_name(&cur));
            }
            for img in images {
                if ui.selectable_label(cur == *img, file_name(img)).on_hover_text(img).clicked() {
                    *b = img.clone();
                }
            }
        });
        if images.is_empty() {
            widgets::hint(ui, t, "Add pictures under Sources → Files.");
        }
    });
}

/// What viewers see, with example values (blank fields show what the alert shows by default).
fn preview(ui: &mut egui::Ui, t: &Theme, k: &AlertKind, title: &str, msg: &str, image: &str) {
    egui::Frame::new().fill(t.surface_hi).corner_radius(CornerRadius::same(radius::CARD)).inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
        ui.set_width(ui.available_width().min(460.0));
        let title = if title.trim().is_empty() { "(their name)" } else { title };
        let msg = if msg.trim().is_empty() { "(message)" } else { msg };
        ui.add(egui::Label::new(RichText::new(sample(title, k)).font(font_bold(type_scale::HEADING)).color(t.fg)).wrap());
        ui.add(egui::Label::new(RichText::new(sample(msg, k)).size(type_scale::LARGE).color(t.text_dim)).wrap());
        if !image.trim().is_empty() {
            ui.label(RichText::new(format!("{}  {}", icon::IMAGE, file_name(image.trim()))).size(type_scale::SMALL).color(t.text_faint));
        }
    });
}

/// "Same as main alert" / Yes / No, for a variation's switches.
fn tri(ui: &mut egui::Ui, salt: (&str, &str), b: &mut String) {
    let label = |v: &str| match v {
        "true" => "Yes",
        "false" => "No",
        _ => SAME,
    };
    let cur = b.trim().to_string();
    egui::ComboBox::from_id_salt(salt).selected_text(label(&cur)).width(200.0).show_ui(ui, |ui| {
        for v in ["", "true", "false"] {
            if ui.selectable_label(cur == v, label(v)).clicked() {
                *b = v.to_string();
            }
        }
    });
}

/// Sound, read aloud, spoken text, voice.
fn sound_rows(ui: &mut egui::Ui, t: &Theme, edits: &mut BTreeMap<String, String>, prefix: &str, look: &Value, inherit: bool, ch: &Choices) {
    widgets::prop_row(ui, t, "Sound", |ui| {
        let b = look_buf(edits, prefix, look, "sound");
        let empty = if inherit { SAME } else { "No sound" };
        let cur = b.trim().to_string();
        let shown = if cur.is_empty() { empty.to_string() } else { sound_name(&cur) };
        egui::ComboBox::from_id_salt(("alert-sound", prefix)).selected_text(shown).width(240.0).show_ui(ui, |ui| {
            if ui.selectable_label(cur.is_empty(), empty).clicked() {
                b.clear();
            }
            for snd in &ch.sounds {
                if ui.selectable_label(cur == *snd, sound_name(snd)).clicked() {
                    *b = snd.clone();
                }
            }
        });
        if ch.sounds.is_empty() {
            widgets::hint(ui, t, "Add sounds under Sources → Files.");
        }
    });
    widgets::prop_row(ui, t, "Read aloud", |ui| {
        let b = look_buf(edits, prefix, look, "tts");
        if inherit {
            tri(ui, ("alert-tts", prefix), b);
        } else {
            let mut on = b.trim() == "true";
            if widgets::toggle(ui, t, &mut on).changed() {
                *b = on.to_string();
            }
            note(ui, t, "Text to speech reads their message.");
        }
    });
    let reads = inherit || edits.get(&format!("{prefix}tts")).is_some_and(|v| v.trim() == "true");
    if !reads {
        return;
    }
    widgets::prop_row(ui, t, "Spoken text", |ui| {
        let w = ui.available_width().min(460.0);
        let b = look_buf(edits, prefix, look, "tts_text");
        ui.add(widgets::field(b).hint_text(if inherit { SAME } else { "(message)" }).desired_width(w));
    });
    widgets::prop_row(ui, t, "Voice", |ui| {
        let b = look_buf(edits, prefix, look, "voice");
        let empty = if inherit { SAME } else { "Default voice" };
        if ch.voices.is_empty() {
            let w = ui.available_width().min(300.0);
            ui.add(widgets::field(b).hint_text(empty).desired_width(w));
            return;
        }
        let cur = b.trim().to_string();
        egui::ComboBox::from_id_salt(("alert-voice", prefix)).selected_text(if cur.is_empty() { empty.to_string() } else { cur.clone() }).width(240.0).show_ui(
            ui,
            |ui| {
                if ui.selectable_label(cur.is_empty(), empty).clicked() {
                    b.clear();
                }
                if !cur.is_empty() && !ch.voices.contains(&cur) {
                    let _ = ui.selectable_label(true, cur.as_str());
                }
                for v in &ch.voices {
                    if ui.selectable_label(cur == *v, v.as_str()).clicked() {
                        *b = v.clone();
                    }
                }
            },
        );
    });
}

/// How long, priority, cut in.
fn timing_rows(ui: &mut egui::Ui, t: &Theme, edits: &mut BTreeMap<String, String>, prefix: &str, look: &Value, inherit: bool) {
    widgets::prop_row(ui, t, "How long", |ui| {
        let b = look_buf(edits, prefix, look, "duration");
        let has = parse_secs(b).is_some();
        let mut secs = parse_secs(b).unwrap_or(6.0) as f32;
        ui.spacing_mut().slider_width = (ui.available_width() - 220.0).clamp(140.0, 320.0);
        if ui.add(egui::Slider::new(&mut secs, 1.0..=30.0).step_by(0.5).custom_formatter(|v, _| secs_label(v))).changed() {
            *b = secs_text(secs as f64);
        }
        if inherit {
            if has {
                if widgets::button_ex(ui, t, None, "Same as main", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                    b.clear();
                }
            } else {
                widgets::hint(ui, t, SAME);
            }
        }
    });
    widgets::prop_row(ui, t, "Priority", |ui| {
        let b = look_buf(edits, prefix, look, "priority");
        let mut v: i64 = b.trim().parse().unwrap_or(50);
        if ui.add(egui::DragValue::new(&mut v).range(0..=1000).speed(1.0)).changed() {
            *b = v.to_string();
        }
        if inherit && !b.trim().is_empty() {
            if widgets::button_ex(ui, t, None, "Same as main", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                b.clear();
            }
        } else if inherit {
            widgets::hint(ui, t, SAME);
        } else {
            note(ui, t, "Higher goes first.");
        }
    });
    widgets::prop_row(ui, t, "Can cut in", |ui| {
        let b = look_buf(edits, prefix, look, "interrupt");
        if inherit {
            tri(ui, ("alert-interrupt", prefix), b);
        } else {
            let mut on = b.trim() != "false";
            if widgets::toggle(ui, t, &mut on).changed() {
                *b = on.to_string();
            }
            note(ui, t, "It can interrupt a smaller alert that's showing.");
        }
    });
}

/// Variations: e.g. a bigger alert for 1,000 bits or more. One expandable row each.
fn variations(app: &mut App, ui: &mut egui::Ui, form: &mut Form, a: &Value, k: &AlertKind, ch: &Choices, when: &str) {
    let t = app.t.clone();
    let file = s(a, "file").to_string();
    let idx = i(a, "index");
    let prefix = alert_key(a);
    let vars: Vec<Value> = a.get_path("variations").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    widgets::group_label(ui, &t, "Variations");
    note(ui, &t, "A different look for some of them, like a bigger alert for big cheers. The first one that fits is used.");
    ui.add_space(spacing::XS);
    for (vi, v) in vars.iter().enumerate() {
        let vprefix = format!("{prefix}v{vi}|");
        let vlook = v.get_path("look").cloned().unwrap_or_default();
        let cond = s(v, "if");
        let mut originals: Vec<(String, String)> = vec![("name".into(), s(v, "name").into()), ("if".into(), cond.into())];
        originals.extend(LOOK.iter().map(|k| (k.to_string(), look_text(&vlook, k))));
        let orig_cmds = look_cmds(&vlook);
        let dirty = is_dirty(form, &vprefix, &originals, &orig_cmds);
        let open_id = egui::Id::new(("alert-version-open", &vprefix));
        let mut open = ui.data(|d| d.get_temp::<bool>(open_id)).unwrap_or(false);
        let title = look_text(&vlook, "title");
        let used = used_when(k, cond);
        let sub = if title.is_empty() { used } else { clip(&format!("{used} · {}", sample(&title, k)), 44) };
        if widgets::list_row(ui, &t, if open { icon::DOWN } else { icon::RIGHT }, &nice(s(v, "name")), &sub, if dirty { "Not saved" } else { "" }, open)
            .clicked()
        {
            open = !open;
            ui.data_mut(|d| d.insert_temp(open_id, open));
        }
        if !open {
            continue;
        }
        let (mut save, mut undo, mut delete) = (false, false, false);
        ui.push_id(("alert-var", &vprefix), |ui| {
            egui::Frame::new().inner_margin(egui::Margin { left: 28, right: 0, top: 4, bottom: 8 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::prop_row(ui, &t, "Name", |ui| {
                    let w = ui.available_width().min(300.0);
                    let b = form.edits.entry(format!("{vprefix}name")).or_insert_with(|| s(v, "name").to_string());
                    ui.add(widgets::field(b).desired_width(w));
                });
                widgets::prop_row(ui, &t, "Used when", |ui| {
                    let b = form.edits.entry(format!("{vprefix}if")).or_insert_with(|| cond.to_string());
                    match (k.unit, min_amount(b)) {
                        (Some(unit), Some(min)) => {
                            let mut n = min.max(1.0);
                            if ui.add(egui::DragValue::new(&mut n).range(1.0..=1_000_000.0).speed(1.0).suffix(format!(" {unit} or more"))).changed() {
                                *b = format!("amount >= {}", fmt_num(n));
                            }
                        }
                        _ => {
                            let w = ui.available_width().min(360.0);
                            ui.add(widgets::field(b).font(egui::TextStyle::Monospace).hint_text("e.g. tier == 3").desired_width(w));
                        }
                    }
                });
                content_rows(ui, &t, &mut form.edits, &vprefix, &vlook, true, &ch.images);
                sound_rows(ui, &t, &mut form.edits, &vprefix, &vlook, true, ch);
                timing_rows(ui, &t, &mut form.edits, &vprefix, &vlook, true);
                widgets::group_label(ui, &t, "Also run");
                let mut cmds = form.cmds.get(&vprefix).cloned().unwrap_or_else(|| orig_cmds.clone());
                if cmds.is_empty() {
                    widgets::hint(ui, &t, "None: the main alert's steps run.");
                }
                if steps_editor(app, ui, &format!("alert-do-{vprefix}"), &mut cmds, when) {
                    form.cmds.insert(vprefix.clone(), cmds);
                }
                ui.add_space(spacing::S);
                ui.horizontal(|ui| {
                    save = widgets::button_ex(ui, &t, Some(icon::CHECK), "Save variation", Kind::Primary, Size::Medium, 0.0, dirty).clicked();
                    if dirty {
                        undo = widgets::button_ex(ui, &t, Some(icon::UNDO), "Undo", Kind::Ghost, Size::Medium, 0.0, true).clicked();
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        delete = widgets::hold_button(ui, &t, "Hold to delete variation", t.bright_red, 0.6);
                    });
                });
            });
        });
        if save {
            let path = Value::List(vec!["alert".into(), Value::Int(idx), "variation".into(), Value::Int(vi as i64)]);
            save_fields(app, form, &file, path, &vprefix, &originals, &orig_cmds);
        }
        if undo {
            drop_edits(form, &vprefix);
        }
        if delete {
            act(
                app,
                "alerts.remove",
                Value::map()
                    .with("file", file.clone())
                    .with("path", Value::List(vec!["alert".into(), Value::Int(idx)]))
                    .with("key", "variation")
                    .with("index", vi as i64),
            );
            // Later variations move up one: forget what was typed for them.
            let vp = format!("{prefix}v");
            form.edits.retain(|key, _| !key.starts_with(&vp));
            form.cmds.retain(|key, _| !key.starts_with(&vp));
            form.last_slow = 0.0;
        }
    }

    if !form.new_var.contains_key(&prefix) {
        if widgets::list_row(ui, &t, icon::PLUS, "Add a variation", "", "", false).clicked() {
            form.new_var.insert(prefix.clone(), Default::default());
        }
        return;
    }
    let (mut add, mut cancel) = (None, false);
    if let Some(nv) = form.new_var.get_mut(&prefix) {
        egui::Frame::new().inner_margin(egui::Margin { left: 28, right: 0, top: 4, bottom: 8 }).show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::group_label(ui, &t, "New variation");
            widgets::prop_row(ui, &t, "Name", |ui| {
                let w = ui.available_width().min(300.0);
                ui.add(widgets::field(&mut nv.0).hint_text("e.g. Big").desired_width(w));
            });
            let cond = widgets::prop_row(ui, &t, "Used when", |ui| match k.unit {
                Some(unit) => {
                    let mut n = nv.1.trim().parse::<f64>().unwrap_or(1.0).max(1.0);
                    if ui.add(egui::DragValue::new(&mut n).range(1.0..=1_000_000.0).speed(1.0).suffix(format!(" {unit} or more"))).changed() {
                        nv.1 = fmt_num(n);
                    }
                    Some(format!("amount >= {}", fmt_num(n)))
                }
                None => {
                    let w = ui.available_width().min(360.0);
                    ui.add(widgets::field(&mut nv.1).font(egui::TextStyle::Monospace).hint_text("e.g. tier == 3").desired_width(w));
                    (!nv.1.trim().is_empty()).then(|| nv.1.trim().to_string())
                }
            });
            let ok = cond.is_some() && !nv.0.trim().is_empty();
            ui.add_space(spacing::XS);
            ui.horizontal(|ui| {
                if widgets::button_ex(ui, &t, Some(icon::PLUS), "Add variation", Kind::Primary, Size::Medium, 0.0, ok).clicked() {
                    add = cond.clone().map(|c| (nv.0.trim().replace(' ', "_"), c));
                }
                cancel = widgets::button_ex(ui, &t, None, "Cancel", Kind::Ghost, Size::Medium, 0.0, true).clicked();
            });
        });
    }
    if let Some((name, cond)) = add {
        act(
            app,
            "alerts.add",
            Value::map()
                .with("file", file.clone())
                .with("path", Value::List(vec!["alert".into(), Value::Int(idx)]))
                .with("key", "variation")
                .with("fields", Value::map().with("name", name).with("if", cond)),
        );
        let new_prefix = format!("{prefix}v{}|", vars.len());
        ui.data_mut(|d| d.insert_temp(egui::Id::new(("alert-version-open", &new_prefix)), true));
        form.last_slow = 0.0;
        cancel = true;
    }
    if cancel {
        form.new_var.remove(&prefix);
    }
}

// ---- Automation → Alerts → Look & timing ----------------------------------------------------------

/// Reply key for project.toml (placement of the notification source).
const PROJECT_KEY: &str = "alerts.delivery.project";

#[derive(Clone, Default)]
struct Delivery {
    /// Queue policy as typed, keyed like [`POLICY`].
    policy: BTreeMap<String, String>,
    /// project.toml `[overlays.<id>]`: (canvases, hidden by `when = "false"`), by source id.
    placement: BTreeMap<String, (Option<Vec<String>>, bool)>,
    project_seq: u64,
    last_live: f64,
    last_slow: f64,
    last_project: f64,
}

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

/// A web source pinned above every scene that looks like it draws notifications.
fn is_notification_source(p: &Value) -> bool {
    let named = |x: &str| {
        let x = x.to_lowercase();
        x.contains("alert") || x.contains("notification")
    };
    s(p, "kind") == "web" && s(p, "layer") == "overlay" && (named(s(p, "id")) || named(s(p, "label")))
}

fn source_title(p: &Value) -> String {
    let id = s(p, "id");
    let label = s(p, "label");
    if label.is_empty() || label == id { nice(id) } else { label.to_string() }
}

fn read_placement(reply: Option<&Value>) -> BTreeMap<String, (Option<Vec<String>>, bool)> {
    let doc = reply.and_then(|v| v.get_path("text")).and_then(Value::as_str).and_then(|text| toml::from_str::<toml::Value>(text).ok());
    let Some(ov) = doc.as_ref().and_then(|d| d.get("overlays")).and_then(toml::Value::as_table) else { return BTreeMap::new() };
    ov.iter()
        .map(|(id, v)| {
            let canvases: Option<Vec<String>> =
                v.get("canvases").and_then(toml::Value::as_array).map(|a| a.iter().filter_map(toml::Value::as_str).map(String::from).collect());
            let hidden = v.get("when").is_some_and(|w| w.as_str() == Some("false") || w.as_bool() == Some(false));
            (id.clone(), (canvases, hidden))
        })
        .collect()
}

/// Engine canvas id in words (`wide` → "Main").
fn canvas_name(c: &str) -> String {
    match c {
        "wide" => "Main".into(),
        "tall" => "Vertical".into(),
        "preview" => "Preview".into(),
        other => nice(other),
    }
}

/// The canvases a source draws on, in words. The preview canvas follows the others, so it is
/// only named when it is the only one.
fn canvases_words(canvases: Option<&Vec<String>>) -> String {
    let Some(c) = canvases else { return "Main and Vertical".into() };
    let shown: Vec<&String> = c.iter().filter(|x| x.as_str() != "preview").collect();
    let shown = if shown.is_empty() { c.iter().collect() } else { shown };
    shown.iter().map(|x| canvas_name(x)).collect::<Vec<_>>().join(" and ")
}

fn delivery_ui(app: &mut App, ui: &mut egui::Ui) {
    let id = egui::Id::new("alerts-delivery");
    let mut form: Delivery = ui.data_mut(|d| d.get_temp::<Delivery>(id)).unwrap_or_default();
    if poll(ui, &mut form.last_live, 0.25) {
        app.m.query("alerts", Value::Null);
    }
    if poll(ui, &mut form.last_slow, 2.0) {
        app.m.query("alerts.config", Value::Null);
        app.m.query("patches", Value::Null);
        if app.m.q("scenes").is_none() {
            app.m.query("scenes", Value::Null);
        }
    }
    if poll(ui, &mut form.last_project, 5.0) {
        app.m.query_as(PROJECT_KEY, "project.read", Value::map().with("path", "project.toml"));
    }
    let seq = app.m.q_seq(PROJECT_KEY);
    if seq != form.project_seq {
        form.project_seq = seq;
        form.placement = read_placement(app.m.q(PROJECT_KEY));
    }
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    let live = app.m.q("alerts").cloned().unwrap_or_default();
    let cfg = app.m.q("alerts.config").cloned().unwrap_or_default();
    let sources: Vec<Value> = app.m.q_list("patches").iter().filter(|p| is_notification_source(p)).cloned().collect();
    egui::ScrollArea::vertical().id_salt("alert-delivery").auto_shrink([false, false]).show(ui, |ui| {
        ui.set_max_width(900.0);
        shown_by(app, ui, &form, &sources);
        queue_timing(app, ui, &mut form, &cfg);
        live_queue(app, ui, &live);
    });
    ui.data_mut(|d| d.insert_temp(id, form));
}

/// The source(s) drawing the notifications: status, where they appear, their look.
fn shown_by(app: &mut App, ui: &mut egui::Ui, form: &Delivery, sources: &[Value]) {
    let t = app.t.clone();
    widgets::inspector_section(
        ui,
        &t,
        "delivery-shown-by",
        "Shown by",
        true,
        |_| {},
        |ui| {
            if sources.is_empty() {
                if app.m.q("patches").is_none() {
                    widgets::hint(ui, &t, "Looking for the source that shows alerts…");
                } else if widgets::empty_state(
                    ui,
                    &t,
                    icon::ALERT,
                    "No source shows alerts yet",
                    "Alerts need a web source on every scene to draw them. Add one under Sources.",
                    Some("Add a source for alerts"),
                ) {
                    app.open_view(crate::app::ViewId::Sources);
                }
                return;
            }
            for p in sources {
                source_block(app, ui, &t, form, p);
            }
        },
    );
    let with_look: Vec<&Value> = sources.iter().filter(|p| p.get_path("params").and_then(Value::as_list).is_some_and(|l| !l.is_empty())).collect();
    if with_look.is_empty() {
        return;
    }
    widgets::inspector_section(
        ui,
        &t,
        "delivery-look",
        "Look",
        true,
        |_| {},
        |ui| {
            widgets::hint(ui, &t, "Changes show on stream right away.");
            ui.add_space(spacing::XS);
            for p in &with_look {
                if with_look.len() > 1 {
                    widgets::group_label(ui, &t, &source_title(p));
                }
                crate::views::patches::params_ui(app, ui, p);
            }
        },
    );
}

fn source_block(app: &mut App, ui: &mut egui::Ui, t: &Theme, form: &Delivery, p: &Value) {
    let id = s(p, "id").to_string();
    let st = app.m.get(&format!("patch.{id}.state")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| s(p, "state").to_string());
    let enabled = p.get_path("enabled").is_none_or(Value::truthy) && st != "disabled";
    let (status, color) = match st.as_str() {
        _ if !enabled => ("Off", t.text_dim),
        "error" | "suspended" => ("Has a problem", t.bright_red),
        _ => ("Working", t.green),
    };
    let mut on = enabled || st == "suspended";
    widgets::detail_header(ui, t, icon::IMAGE, &source_title(p), "Web source that draws the alerts", |ui| {
        if widgets::toggle(ui, t, &mut on).on_hover_text(if on { "Turn off" } else { "Turn on" }).changed() {
            act(app, if on { "patch.enable" } else { "patch.disable" }, Value::map().with("id", id.clone()));
        }
        ui.add_space(spacing::S);
        widgets::badge(ui, t, status, color);
    });
    let (canvases, hidden) = form.placement.get(&id).cloned().unwrap_or((None, false));
    let everywhere = !hidden && canvases.as_ref().is_none_or(|c| !c.is_empty());
    let layer = format!("patch.{id}");
    let placed: Vec<String> = app
        .m
        .q_list("scenes")
        .iter()
        .filter(|sc| sc.get_path("sources").and_then(Value::as_list).is_some_and(|l| l.iter().any(|v| v.as_str() == Some(layer.as_str()))))
        .map(|sc| nice(if s(sc, "label").is_empty() { s(sc, "name") } else { s(sc, "label") }))
        .collect();
    widgets::prop_row(ui, t, "Where it appears", |ui| {
        let text = if everywhere {
            let on = canvases_words(canvases.as_ref());
            format!("On every scene · {on}")
        } else {
            "Only in scenes where it's placed".into()
        };
        ui.label(RichText::new(text).color(t.fg));
    });
    if !placed.is_empty() {
        widgets::prop_row(ui, t, if everywhere { "Also placed in" } else { "Placed in" }, |ui| {
            ui.add(egui::Label::new(RichText::new(placed.join(", ")).color(t.fg)).wrap());
        });
    } else if !everywhere {
        widgets::prop_row(ui, t, "", |ui| {
            ui.label(RichText::new("It isn't placed in any scene, so alerts won't show.").color(t.yellow));
        });
    }
    if st == "error" {
        widgets::prop_row(ui, t, "", |ui| {
            note(ui, t, "It has a mistake. Open it under Sources to see what's wrong.");
        });
    }
    ui.add_space(spacing::S);
}

/// A seconds slider row of the queue policy.
fn secs_row(ui: &mut egui::Ui, t: &Theme, policy: &mut BTreeMap<String, String>, key: &str, label: &str, help: &str, range: std::ops::RangeInclusive<f32>) {
    widgets::prop_row(ui, t, label, |ui| {
        let Some(buf) = policy.get_mut(key) else { return };
        let mut v = parse_secs(buf).unwrap_or(0.0) as f32;
        ui.spacing_mut().slider_width = (ui.available_width() - 260.0).clamp(140.0, 320.0);
        if ui.add(egui::Slider::new(&mut v, range).step_by(0.5).custom_formatter(|v, _| secs_label(v))).changed() {
            *buf = secs_text(v as f64);
        }
        note(ui, t, help);
    });
}

/// A whole-number row of the queue policy.
fn int_row(ui: &mut egui::Ui, t: &Theme, policy: &mut BTreeMap<String, String>, key: &str, label: &str, help: &str) {
    widgets::prop_row(ui, t, label, |ui| {
        let Some(buf) = policy.get_mut(key) else { return };
        let mut v: i64 = buf.trim().parse().unwrap_or(0);
        if ui.add(egui::DragValue::new(&mut v).range(0..=10_000).speed(1.0)).changed() {
            *buf = v.to_string();
        }
        note(ui, t, help);
    });
}

fn queue_timing(app: &mut App, ui: &mut egui::Ui, form: &mut Delivery, cfg: &Value) {
    let t = app.t.clone();
    let q = cfg.get_path("queue").cloned();
    let current = |src: &str| -> String {
        match q.as_ref().and_then(|q| q.get_path(src)) {
            Some(Value::List(l)) => l.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
            Some(Value::Int(ms)) if src.ends_with("_ms") => dur(*ms),
            Some(Value::Str(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => String::new(),
        }
    };
    if q.is_some() {
        for (k, src) in POLICY {
            form.policy.entry(k.to_string()).or_insert_with(|| current(src));
        }
    }
    let dirty = POLICY.iter().any(|(k, src)| form.policy.get(*k).is_some_and(|v| *v != current(src)));
    let (mut save, mut undo) = (false, false);
    widgets::inspector_section(
        ui,
        &t,
        "delivery-timing",
        "Queue timing",
        true,
        |_| {},
        |ui| {
            let mut enabled = app.m.get("alerts.enabled").is_none_or(Value::truthy);
            widgets::prop_row(ui, &t, "Alerts on stream", |ui| {
                if widgets::toggle(ui, &t, &mut enabled).changed() {
                    app.m.command(Op::Set { address: "alerts.enabled".into(), value: Value::Bool(enabled) });
                }
                note(ui, &t, "Off: nothing pops up, but follows, subs and cheers are still counted.");
            });
            if q.is_none() {
                widgets::hint(ui, &t, "Loading the timing…");
                return;
            }
            let p = &mut form.policy;
            secs_row(ui, &t, p, "min_spacing", "Pause between", "A breather between alerts.", 0.0..=10.0);
            secs_row(ui, &t, p, "max_on_screen", "Longest on screen", "Even big ones end after this.", 3.0..=60.0);
            widgets::prop_row(ui, &t, "Big ones cut in", |ui| {
                let Some(buf) = p.get_mut("interrupt") else { return };
                let mut on = buf.trim() == "true";
                if widgets::toggle(ui, &t, &mut on).changed() {
                    *buf = on.to_string();
                }
                note(ui, &t, "A much bigger alert can interrupt a small one.");
            });
            widgets::prop_row(ui, &t, "Replay cut ones", |ui| {
                let Some(buf) = p.get_mut("on_interrupt") else { return };
                let mut on = buf.trim() != "drop";
                if widgets::toggle(ui, &t, &mut on).changed() {
                    *buf = if on { "requeue".into() } else { "drop".into() };
                }
                note(ui, &t, "Off: an interrupted alert is dropped.");
            });
            secs_row(ui, &t, p, "veto_window", "Time to skip", "Alerts with a message wait this long.", 0.0..=15.0);
            secs_row(ui, &t, p, "gift_window", "Group gifts within", "Gifts this close together are one alert.", 1.0..=15.0);
            widgets::prop_row(ui, &t, "Wait during", |ui| {
                let Some(buf) = p.get_mut("pause_modes") else { return };
                let mut modes: Vec<String> = buf.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
                let mut known: Vec<(String, String)> = vec![("ad_break".into(), "Ad breaks".into()), ("brb".into(), "Be right back".into())];
                for m in &modes {
                    if !known.iter().any(|(k, _)| k == m) {
                        known.push((m.clone(), nice(m)));
                    }
                }
                let mut changed = false;
                for (m, label) in &known {
                    let on = modes.contains(m);
                    if widgets::chip(ui, &t, icon::PAUSE, label, on).clicked() {
                        changed = true;
                        if on {
                            modes.retain(|x| x != m);
                        } else {
                            modes.push(m.clone());
                        }
                    }
                }
                if changed {
                    *buf = modes.join(", ");
                }
            });
            widgets::details(ui, &t, "timing-details", "Details", |ui| {
                int_row(ui, &t, p, "max_queue", "Most waiting", "Beyond this, the smallest new ones are dropped.");
                int_row(ui, &t, p, "interrupt_margin", "Cut-in margin", "How much higher the priority must be to cut in.");
                int_row(ui, &t, p, "max_message", "Longest message", "Characters shown from a viewer's message.");
            });
            ui.add_space(spacing::S);
            ui.horizontal(|ui| {
                save = widgets::button_ex(ui, &t, Some(icon::CHECK), "Save", Kind::Primary, Size::Medium, 0.0, dirty).clicked();
                if dirty {
                    undo = widgets::button_ex(ui, &t, Some(icon::UNDO), "Undo", Kind::Ghost, Size::Medium, 0.0, true).clicked();
                } else {
                    widgets::hint(ui, &t, "Everything is saved.");
                }
            });
        },
    );
    if undo {
        form.policy.clear();
    }
    if !save {
        return;
    }
    let mut fields = Value::map();
    for (k, src) in POLICY {
        let Some(text) = form.policy.get(k) else { continue };
        if *text == current(src) {
            continue;
        }
        let tx = text.trim();
        let v = match k {
            "max_queue" | "interrupt_margin" | "max_message" => match tx.parse::<i64>() {
                Ok(n) => Value::Int(n),
                Err(_) => {
                    app.m.toast(format!("\"{tx}\" isn't a whole number"), true);
                    return;
                }
            },
            "interrupt" => Value::Bool(matches!(tx, "true" | "yes" | "on" | "1")),
            "pause_modes" => Value::List(tx.split(',').map(str::trim).filter(|x| !x.is_empty()).map(|x| Value::Str(x.into())).collect()),
            _ => Value::Str(tx.into()),
        };
        fields = fields.with(k, v);
    }
    act(app, "alerts.policy", Value::map().with("fields", fields));
    form.policy.clear();
    form.last_slow = 0.0;
}

/// A title + one dim line on the left, controls on the right.
fn item_row(ui: &mut egui::Ui, t: &Theme, title: &str, sub: &str, controls: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - 220.0).max(120.0));
            ui.add(egui::Label::new(RichText::new(title).font(font_medium(type_scale::BODY)).color(t.fg)).truncate());
            if !sub.is_empty() {
                ui.add(egui::Label::new(RichText::new(sub).size(type_scale::SMALL + 0.5).color(t.text_dim)).truncate());
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), controls);
    });
}

/// On screen now, waiting (with the mod-skip countdown) and shown recently.
fn live_queue(app: &mut App, ui: &mut egui::Ui, live: &Value) {
    let t = app.t.clone();
    let cur = live.get_path("current").cloned().unwrap_or_default();
    let queue: Vec<Value> = live.get_path("queue").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let history: Vec<Value> = live.get_path("history").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let mut clear = false;
    widgets::inspector_section(
        ui,
        &t,
        "delivery-live",
        "Live queue",
        true,
        |_| {},
        |ui| {
            widgets::group_label(ui, &t, "On screen now");
            if cur.is_null() {
                widgets::hint(ui, &t, "Nothing on screen right now.");
            } else {
                let n = cur.get_path("recipients").and_then(Value::as_list).map(|l| l.len()).unwrap_or(0);
                let mut line = format!("{} seconds left", i(&cur, "remaining_ms") / 1000);
                if n > 0 {
                    line.push_str(&format!(" · {n} people got a gift"));
                }
                if !s(&cur, "message").is_empty() {
                    ui.add(egui::Label::new(RichText::new(s(&cur, "message")).color(t.text_dim)).wrap());
                }
                item_row(ui, &t, s(&cur, "title"), &line, |ui| {
                    if widgets::button_ex(ui, &t, Some(icon::CROSS), "Remove", Kind::Danger, Size::Small, 0.0, true)
                        .on_hover_text("Take it off and never replay it")
                        .clicked()
                    {
                        act(app, "alerts.veto", Value::map().with("id", i(&cur, "id")));
                    }
                    if widgets::button_ex(ui, &t, Some(icon::RIGHT), "Skip", Kind::Secondary, Size::Small, 0.0, true)
                        .on_hover_text("End it now and go to the next one")
                        .clicked()
                    {
                        act(app, "alerts.skip", Value::Null);
                    }
                });
            }

            widgets::group_label(ui, &t, "Waiting");
            if queue.is_empty() {
                widgets::hint(ui, &t, "No alerts waiting.");
            }
            for a in &queue {
                let aid = i(a, "id");
                let veto_ms = i(a, "veto_remaining_ms");
                let fill = if veto_ms > 0 { mix(t.surface, t.yellow, 0.08) } else { t.surface_hi };
                egui::Frame::new().fill(fill).corner_radius(CornerRadius::same(radius::CONTROL)).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let msg = s(a, "message");
                    let sub = if msg.is_empty() { String::new() } else { format!("“{msg}”") };
                    item_row(ui, &t, s(a, "title"), &sub, |ui| {
                        if widgets::icon_button(ui, &t, icon::CROSS, "Remove it (it won't show)").clicked() {
                            act(app, "alerts.veto", Value::map().with("id", aid));
                        }
                        if veto_ms > 0 && widgets::button_ex(ui, &t, Some(icon::CHECK), "Show now", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                            act(app, "alerts.approve", Value::map().with("id", aid));
                        }
                        if a.get_path("sim").is_some_and(Value::truthy) {
                            widgets::badge(ui, &t, "Test", t.text_dim);
                        }
                    });
                    if veto_ms > 0 {
                        let window = app.m.get(&format!("alerts.veto.{aid}")).and_then(Value::as_f64).unwrap_or(veto_ms as f64 / 1000.0);
                        ui.label(RichText::new(format!("Mods can skip it for {window:.1} s")).size(type_scale::SMALL + 0.5).color(mix(t.yellow, t.fg, 0.3)));
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
            if !queue.is_empty() {
                clear = widgets::hold_button(ui, &t, "Hold to clear waiting", t.yellow, 0.6);
            }

            widgets::group_label(ui, &t, "Shown recently");
            if history.is_empty() {
                widgets::hint(ui, &t, "Alerts you've had this session show up here, so you can replay them.");
            }
            for h in history.iter().take(30) {
                item_row(ui, &t, s(h, "title"), &ago(i(h, "ago_ms")), |ui| {
                    if widgets::button_ex(ui, &t, Some(icon::UNDO), "Replay", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                        act(app, "alerts.replay", Value::map().with("id", i(h, "id")));
                    }
                });
                ui.add_space(spacing::XS);
            }
        },
    );
    if clear {
        act(app, "alerts.clear", Value::Null);
    }
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

// ---- Community → Goals ---------------------------------------------------------------------------

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
    if k.starts_with("custom:") {
        return "A custom event".into();
    }
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

#[derive(Clone, Default)]
struct Goals {
    /// 0 = goals, 1 = this stream.
    tab: usize,
    selected: Option<String>,
    draft: Option<GoalDraft>,
    /// Progress / target being set for the selected goal.
    set_progress: Option<f64>,
    set_target: Option<f64>,
    last_slow: f64,
}

#[derive(Clone, Default)]
struct GoalDraft {
    label: String,
    counts: String,
    target: f64,
}

/// A goal's current count (live state first, then the config reply).
fn goal_current(app: &App, g: &Value) -> f64 {
    app.m.get(&format!("goals.{}.current", s(g, "name"))).and_then(Value::as_f64).unwrap_or_else(|| f(g, "current"))
}

fn goal_label(g: &Value) -> String {
    if s(g, "label").is_empty() { nice(s(g, "name")) } else { s(g, "label").to_string() }
}

pub fn goals_ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("alerts-goals");
    let mut form: Goals = ui.data_mut(|d| d.get_temp::<Goals>(id)).unwrap_or_default();
    if poll(ui, &mut form.last_slow, 2.0) {
        app.m.query("alerts.config", Value::Null);
    }
    let cfg = app.m.q("alerts.config").cloned().unwrap_or_default();
    let list: Vec<Value> = cfg.get_path("goals").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    widgets::segmented(ui, &t, &mut form.tab, &["Goals", "This stream"]);
    ui.add_space(spacing::M);
    if form.tab == 1 {
        egui::ScrollArea::vertical().id_salt("goals-stats").auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(900.0);
            stats(app, ui);
        });
        ui.data_mut(|d| d.insert_temp(id, form));
        return;
    }
    let rows: Vec<(String, String, String, String)> = list
        .iter()
        .map(|g| {
            let cur = goal_current(app, g);
            (
                s(g, "name").to_string(),
                goal_label(g),
                format!("Counts {}", counts_label(s(g, "counts")).to_lowercase()),
                format!("{} / {}", show_num(cur), show_num(f(g, "target"))),
            )
        })
        .collect();
    let sel = form.selected.clone();
    let selected = sel.as_ref().and_then(|n| list.iter().find(|g| s(g, "name") == n.as_str())).cloned();
    let ((pick, create), _) = widgets::split(
        ui,
        300.0,
        |ui| {
            let create = widgets::pane_header(ui, &t, "Goals", Some(rows.len()), Some("New goal"));
            let mut pick = None;
            egui::ScrollArea::vertical().id_salt("goals-list").auto_shrink([false, false]).show(ui, |ui| {
                if rows.is_empty() {
                    widgets::hint(ui, &t, "No goals yet.");
                }
                for (name, label, sub, progress) in &rows {
                    if widgets::list_row(ui, &t, icon::TROPHY, label, sub, progress, sel.as_deref() == Some(name.as_str())).clicked() {
                        pick = Some(name.clone());
                    }
                }
            });
            (pick, create)
        },
        |ui| {
            egui::ScrollArea::vertical().id_salt("goals-detail").auto_shrink([false, false]).show(ui, |ui| {
                if form.draft.is_some() {
                    new_goal(app, ui, &mut form);
                } else if let Some(g) = &selected {
                    goal_detail(app, ui, &mut form, g);
                } else if widgets::empty_state(
                    ui,
                    &t,
                    icon::TROPHY,
                    "Goals",
                    "A progress bar on stream, like \"Sub goal 32 / 50\". It keeps counting across streams.",
                    Some("New goal"),
                ) {
                    form.draft = Some(GoalDraft::default());
                }
            });
        },
    );
    if create {
        form.draft = Some(GoalDraft::default());
        form.selected = None;
    }
    if let Some(n) = pick {
        if form.selected.as_ref() != Some(&n) {
            form.set_progress = None;
            form.set_target = None;
        }
        form.selected = Some(n);
        form.draft = None;
    }
    ui.data_mut(|d| d.insert_temp(id, form));
}

fn goal_detail(app: &mut App, ui: &mut egui::Ui, form: &mut Goals, g: &Value) {
    let t = app.t.clone();
    let name = s(g, "name").to_string();
    let cur = goal_current(app, g);
    let target = f(g, "target");
    let done = target > 0.0 && cur >= target;
    let mut add1 = false;
    widgets::detail_header(ui, &t, icon::TROPHY, &goal_label(g), &format!("Counts {}", counts_label(s(g, "counts")).to_lowercase()), |ui| {
        add1 = widgets::button_ex(ui, &t, Some(icon::PLUS), "Add 1", Kind::Secondary, Size::Medium, 0.0, true).on_hover_text("Count one by hand").clicked();
        if done {
            widgets::badge(ui, &t, "Reached!", t.green);
        }
    });
    if add1 {
        act(app, "goals.add", Value::map().with("name", name.clone()).with("amount", 1.0));
    }
    widgets::inspector_section(
        ui,
        &t,
        "goal-sec-progress",
        "Progress",
        true,
        |_| {},
        |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(show_num(cur)).font(font_bold(type_scale::DISPLAY)).color(t.fg));
                ui.label(RichText::new(format!("/ {}", show_num(target))).font(font_medium(type_scale::HEADING)).color(t.text_dim));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let pct = if target > 0.0 { (cur / target * 100.0).round() } else { 0.0 };
                    ui.label(RichText::new(format!("{pct}%")).font(font_mono(type_scale::BODY)).color(t.text_dim));
                });
            });
            progress_bar(ui, &t, if target > 0.0 { (cur / target) as f32 } else { 0.0 }, if done { t.green } else { t.accent });
        },
    );
    widgets::inspector_section(
        ui,
        &t,
        "goal-sec-change",
        "Change",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, &t, "Progress", |ui| {
                let mut v = form.set_progress.unwrap_or(cur);
                if ui.add(egui::DragValue::new(&mut v).range(0.0..=1_000_000_000.0).speed(1.0)).changed() {
                    form.set_progress = Some(v);
                }
                let ready = form.set_progress.is_some_and(|x| x != cur);
                if widgets::button_ex(ui, &t, None, "Set", Kind::Secondary, Size::Small, 0.0, ready).clicked() {
                    act(app, "goals.set", Value::map().with("name", name.clone()).with("value", v));
                    form.set_progress = None;
                }
            });
            widgets::prop_row(ui, &t, "Target", |ui| {
                let mut v = form.set_target.unwrap_or(target);
                if ui.add(egui::DragValue::new(&mut v).range(1.0..=1_000_000_000.0).speed(1.0)).changed() {
                    form.set_target = Some(v);
                }
                let ready = form.set_target.is_some_and(|x| x != target && x > 0.0);
                if widgets::button_ex(ui, &t, None, "Set", Kind::Secondary, Size::Small, 0.0, ready).clicked() {
                    act(app, "goals.save", Value::map().with("name", name.clone()).with("fields", Value::map().with("target", v)));
                    form.set_target = None;
                }
            });
        },
    );
    ui.add_space(spacing::M);
    ui.horizontal(|ui| {
        if widgets::hold_button(ui, &t, "Hold to start from 0", t.yellow, 0.6) {
            act(app, "goals.reset", Value::map().with("name", name.clone()));
        }
        if widgets::hold_button(ui, &t, "Hold to delete goal", t.bright_red, 0.8) {
            act(app, "goals.delete", Value::map().with("name", name.clone()));
            form.selected = None;
            form.last_slow = 0.0;
        }
    });
}

fn new_goal(app: &mut App, ui: &mut egui::Ui, form: &mut Goals) {
    let t = app.t.clone();
    let Some(d) = form.draft.as_mut() else { return };
    let slug: String =
        d.label.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect::<String>().trim_matches('_').to_string();
    let ok = !slug.is_empty() && !d.counts.is_empty() && d.target > 0.0;
    let (mut create, mut cancel) = (false, false);
    widgets::detail_header(ui, &t, icon::TROPHY, "New goal", "Shows as a progress bar on stream and keeps counting across streams.", |ui| {
        create = widgets::button_ex(ui, &t, Some(icon::CHECK), "Create", Kind::Primary, Size::Medium, 0.0, ok).clicked();
        cancel = widgets::button_ex(ui, &t, None, "Cancel", Kind::Ghost, Size::Medium, 0.0, true).clicked();
    });
    widgets::inspector_section(
        ui,
        &t,
        "goal-new-form",
        "Goal",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, &t, "Name", |ui| {
                let w = ui.available_width().min(300.0);
                ui.add(widgets::field(&mut d.label).hint_text("What viewers see").desired_width(w));
            });
            widgets::prop_row(ui, &t, "What it counts", |ui| {
                let shown = if d.counts.is_empty() { "Choose…".to_string() } else { counts_label(&d.counts) };
                egui::ComboBox::from_id_salt("goal-counts").selected_text(shown).width(240.0).show_ui(ui, |ui| {
                    for (k, l) in COUNTS {
                        ui.selectable_value(&mut d.counts, k.to_string(), l);
                    }
                });
            });
            widgets::prop_row(ui, &t, "Target", |ui| {
                ui.add(egui::DragValue::new(&mut d.target).range(0.0..=1_000_000_000.0).speed(1.0));
            });
        },
    );
    let fields = (create && ok).then(|| Value::map().with("counts", d.counts.clone()).with("target", d.target).with("label", d.label.trim()));
    if let Some(fields) = fields {
        act(app, "goals.save", Value::map().with("name", slug.clone()).with("fields", fields));
        form.selected = Some(slug);
        form.draft = None;
        form.last_slow = 0.0;
    } else if cancel {
        form.draft = None;
    }
}

// ---- this stream ---------------------------------------------------------------------------------

/// A state value as text (`""` when unset).
fn state_text(app: &App, a: &str) -> String {
    app.m
        .get(a)
        .map(|v| match v {
            Value::Str(s) => s.clone(),
            Value::Float(x) => fmt_num(*x),
            Value::Null => String::new(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

fn stats(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let n = |k: &str| app.m.f(&format!("stats.session.{k}"));
    let tiles = [
        (icon::HEART, show_num(n("follows")), "new followers"),
        (icon::STAR, show_num(n("subs")), "new subs"),
        (icon::GIFT, show_num(n("gifts")), "gifted subs"),
        (icon::BOLT, show_num(n("bits")), "bits cheered"),
        (icon::HEART, money(n("tips"), ""), "in tips"),
        (icon::USERS, show_num(n("raids")), "raids"),
    ];
    widgets::inspector_section(
        ui,
        &t,
        "stats-now",
        "This stream so far",
        true,
        |_| {},
        |ui| {
            let w = ui.available_width();
            let cols = ((w + spacing::M) / 200.0).floor().clamp(1.0, 6.0) as usize;
            let cw = (w - spacing::M * (cols - 1) as f32) / cols as f32;
            for row in tiles.chunks(cols) {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = spacing::M;
                    for (ic, v, l) in row {
                        ui.allocate_ui_with_layout(Vec2::new(cw, 0.0), Layout::top_down(Align::Min), |ui| {
                            egui::Frame::new().fill(t.surface_hi).corner_radius(CornerRadius::same(radius::CARD)).inner_margin(egui::Margin::same(14)).show(
                                ui,
                                |ui| {
                                    ui.set_width(cw - 28.0);
                                    ui.label(RichText::new(*ic).size(type_scale::LARGE).color(t.text_dim));
                                    ui.label(RichText::new(v).font(font_bold(type_scale::TITLE)).color(t.fg));
                                    ui.label(RichText::new(*l).color(t.text_dim));
                                },
                            );
                        });
                    }
                });
                ui.add_space(spacing::M);
            }
        },
    );
    // (label, user state, amount state, unit) per group.
    type Supporter = (&'static str, &'static str, &'static str, &'static str);
    let groups: [(&str, &[Supporter]); 3] = [
        (
            "Latest",
            &[
                ("Follower", "stats.latest.follow.user", "", ""),
                ("Sub", "stats.latest.sub.user", "", ""),
                ("Gifter", "stats.latest.gift.user", "stats.latest.gift.count", "subs"),
                ("Cheer", "stats.latest.cheer.user", "stats.latest.cheer.amount", "bits"),
                ("Tip", "stats.latest.tip.user", "stats.latest.tip.amount", "$"),
                ("Raid", "stats.latest.raid.user", "stats.latest.raid.viewers", "viewers"),
            ],
        ),
        (
            "Biggest this stream",
            &[
                ("Cheer", "stats.top.cheer.session.user", "stats.top.cheer.session.amount", "bits"),
                ("Tip", "stats.top.tip.session.user", "stats.top.tip.session.amount", "$"),
                ("Gifter", "stats.top.gift.session.user", "stats.top.gift.session.amount", "subs"),
            ],
        ),
        (
            "Biggest ever",
            &[
                ("Cheer", "stats.top.cheer.alltime.user", "stats.top.cheer.alltime.amount", "bits"),
                ("Tip", "stats.top.tip.alltime.user", "stats.top.tip.alltime.amount", "$"),
                ("Gifter", "stats.top.gift.alltime.user", "stats.top.gift.alltime.amount", "subs"),
            ],
        ),
    ];
    widgets::inspector_section(
        ui,
        &t,
        "stats-people",
        "Your supporters",
        true,
        |_| {},
        |ui| {
            widgets::hint(ui, &t, "The latest and biggest, for shout-outs.");
            for (group, rows) in groups {
                widgets::group_label(ui, &t, group);
                for (label, user, amount, unit) in rows {
                    let who = state_text(app, user);
                    let amt = if amount.is_empty() { String::new() } else { state_text(app, amount) };
                    let value = match (who.is_empty(), amt.is_empty() || amt == "0") {
                        (true, _) => "Nobody yet".to_string(),
                        (false, true) => who.clone(),
                        (false, false) if *unit == "$" => format!("{who} · {}", money(amt.parse().unwrap_or(0.0), "")),
                        (false, false) => format!("{who} · {amt} {unit}"),
                    };
                    widgets::prop_row(ui, &t, label, |ui| {
                        ui.label(RichText::new(value).font(font_medium(type_scale::BODY)).color(if who.is_empty() { t.text_faint } else { t.fg }));
                    });
                }
            }
        },
    );
    widgets::inspector_section(
        ui,
        &t,
        "stats-test-data",
        "Test data",
        false,
        |_| {},
        |ui| {
            note(ui, &t, "Removes everything the Test buttons added to these numbers, goals and top chatters.");
            ui.add_space(spacing::S);
            if widgets::hold_button(ui, &t, "Hold to remove test data", t.yellow, 1.0) {
                act(app, "stats.purge_simulated", Value::Null);
            }
        },
    );
}
