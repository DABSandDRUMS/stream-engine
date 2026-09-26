//! Top bar (§15.4, every page): on-air state + time, what the show is doing (in plain words), a
//! single "all good / needs attention" pill with the list behind it, Clear chat effects,
//! Emergency stop (hold), and the one big Go live / End stream button.

use crate::app::{App, ViewId};
use crate::panels::Panel;
use egui::{Align, Layout, RichText};
use se_proto::{Op, Value};
use se_ui_kit::theme::{font_bold, font_semibold, mix, radius, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, Tone, icon};

/// Something the streamer should know about, in plain words, with where to fix it.
#[derive(Clone, Debug, PartialEq)]
pub struct Warning {
    pub text: String,
    pub fail: bool,
    pub open: Option<Panel>,
    /// Part of finishing setup (Twitch not connected, OBS not open while off air), not a fault.
    pub setup: bool,
}

/// `hh:mm:ss` for a duration in seconds.
pub fn hms(secs: i64) -> String {
    let s = secs.max(0);
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Friendly name of a show mode.
pub fn mode_label(mode: &str) -> String {
    match mode {
        "offline" => "Off air".into(),
        "preshow" => "Starting soon".into(),
        "live" => "Live".into(),
        "brb" => "Be right back".into(),
        "ad_break" => "Ad break".into(),
        "outro" => "Ending".into(),
        "rehearsal" => "Rehearsal".into(),
        "" => "…".into(),
        other => {
            let mut c = other.replace('_', " ");
            if let Some(f) = c.get_mut(0..1) {
                f.make_ascii_uppercase();
            }
            c
        }
    }
}

/// Health checks that are optional features: "not set up" there is not a problem.
const OPTIONAL: [&str; 4] = ["relay", "youtube", "tiktok", "twitch"];

/// Plain-language title for a health check, and where to fix it.
fn check_info(check: &str) -> (&'static str, Option<Panel>) {
    let base = check.split('.').next().unwrap_or(check);
    match base {
        "obs" => ("OBS", Some(Panel::View(ViewId::Setup))),
        "twitch" => ("Twitch", Some(Panel::View(ViewId::Setup))),
        "youtube" => ("Song requests", Some(Panel::View(ViewId::Setup))),
        "relay" => ("Tips & public queue", Some(Panel::View(ViewId::Setup))),
        "sources" | "devices" => ("Cameras & devices", Some(Panel::View(ViewId::Devices))),
        "audio" => ("Sound", Some(Panel::View(ViewId::Audio))),
        "mixer" => ("Mixing desk", Some(Panel::View(ViewId::Mixer))),
        "dmx" | "lights" => ("Lights", Some(Panel::View(ViewId::Lights))),
        "deck" | "midi" | "input" | "voice" => ("Buttons & pedals", Some(Panel::View(ViewId::Controllers))),
        "render" | "gpu" => ("Video", Some(Panel::View(ViewId::Performance))),
        "patches" | "cef" => ("Overlays", Some(Panel::View(ViewId::Patches))),
        "tts" => ("Text to speech", Some(Panel::View(ViewId::Tts))),
        "timecode" => ("Timelines", Some(Panel::View(ViewId::Timeline))),
        "clips" | "recordings" => ("Recordings", Some(Panel::View(ViewId::Sessions))),
        "backup" => ("Backups", Some(Panel::View(ViewId::Maintenance))),
        "alerts" | "bot" | "player" => ("Community", Some(Panel::View(ViewId::Alerts))),
        "disk" => ("Disk space", Some(Panel::View(ViewId::Maintenance))),
        "idle_inhibitor" => ("Screen saver", None),
        "night_light" => ("Night light", None),
        _ => ("Engine", Some(Panel::View(ViewId::Troubleshoot))),
    }
}

/// A plain sentence for the health checks people actually meet (the engine's detail text is
/// for troubleshooting; it stays in Settings → Troubleshooting and Performance).
fn friendly(app: &App, check: &str, status: &str, detail: &str) -> Option<String> {
    let fail = status == "fail";
    Some(match check.split('.').next().unwrap_or(check) {
        "obs" if check == "obs" => {
            if fail && detail.contains("no frames") {
                "OBS isn't getting your video.".into()
            } else if fail {
                "OBS isn't open.".into()
            } else {
                "OBS needs a look: our video isn't in its scenes yet.".into()
            }
        }
        "audio" if check == "audio.mic" => "Your microphone is silent. Is it muted or unplugged?".into(),
        "audio" if check == "audio.obs" => "OBS can't hear Stream Engine's sound yet. Sound → \"Add our sound to OBS\" fixes it.".into(),
        "audio" => "Something's off with the sound. See Sound → Mix → Advanced.".into(),
        "twitch" if detail.contains("not authorized") || detail.contains("no Client ID") => "Twitch isn't set up yet.".into(),
        "twitch" if ["token", "scopes", "revoked"].iter().any(|w| detail.contains(w)) => "Twitch needs you to sign in again: Community → Twitch.".into(),
        "twitch" if fail => "Twitch isn't connected right now. It reconnects by itself; if it stays, see Community → Twitch.".into(),
        "twitch" => "Twitch needs a look: see Community → Twitch.".into(),
        "night_light" => "Night light is on, so your screens look warmer than your stream. Turn it off while you adjust colours.".into(),
        "idle_inhibitor" => "Your screen may lock during the show. It normally stays awake by itself once you start.".into(),
        "gpu" => "The graphics card is running hot or nearly full.".into(),
        "mixer" => {
            if fail {
                "Can't find your mixing desk on the network. Is it switched on?".into()
            } else {
                "The mixing desk needs a look.".into()
            }
        }
        "sources" => {
            // "3 live; cam_3: no signal" → "Camera 3 (HDMI 3) has no picture."
            let cams: Vec<String> = detail
                .split(';')
                .filter_map(|p| {
                    p.trim().split_once(':').filter(|(_, why)| why.contains("signal")).map(|(n, _)| crate::views::composition::source_label(app, n.trim()))
                })
                .collect();
            match cams.as_slice() {
                [] => "A camera needs a look.".into(),
                [one] => format!("{one} has no picture. Is the camera on?"),
                many => format!("{} have no picture.", many.join(", ")),
            }
        }
        "devices" => {
            if fail {
                "A device you use is unplugged.".into()
            } else {
                "A device needs a look.".into()
            }
        }
        "dmx" | "lights" => "The lights box isn't connected.".into(),
        "deck" | "midi" | "input" => "A controller isn't connected.".into(),
        "voice" => "Voice control isn't ready.".into(),
        "render" => "Video is struggling to keep up.".into(),
        "cef" | "patches" => "An overlay has a problem.".into(),
        "tts" => "The read-out voice isn't ready.".into(),
        "disk" => "You're running low on disk space.".into(),
        "backup" => "Backups aren't being made.".into(),
        "recordings" => "Recordings are using more space than you allowed.".into(),
        "timecode" => "A timeline can't hear its clock.".into(),
        "clips" => "Clip making needs a look.".into(),
        _ => return None,
    })
}

/// The engine connection went away: when, and the result of a "Start" click.
#[derive(Default)]
pub struct EngineDown {
    since: Option<std::time::Instant>,
    starting: Option<std::time::Instant>,
    result: std::sync::Arc<std::sync::Mutex<Option<Result<(), String>>>>,
}

/// True when the engine has been unreachable long enough to say so (not a reconnect blip).
pub fn engine_down(app: &mut App) -> bool {
    if app.m.connected {
        app.down = EngineDown::default();
        return false;
    }
    let since = *app.down.since.get_or_insert_with(std::time::Instant::now);
    since.elapsed().as_secs_f32() > 3.0
}

/// The page shown while the engine isn't running: one button starts it (and keeps it starting
/// at login) through the systemd user service.
pub fn engine_down_ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    egui::CentralPanel::default().frame(egui::Frame::new().fill(t.bg).inner_margin(32)).show(ui, |ui| {
        ui.add_space((ui.available_height() * 0.2).max(0.0));
        let result = app.down.result.lock().map(|r| r.clone()).unwrap_or(None);
        let starting = app.down.starting.is_some_and(|s| s.elapsed().as_secs() < 20) && !matches!(result, Some(Err(_)));
        let (title, body) = match (&result, starting) {
            (Some(Err(e)), _) => ("Stream Engine didn't start", format!("{e}\nTry again, or restart the computer.")),
            (_, true) => ("Starting Stream Engine…", "This takes a few seconds.".to_string()),
            _ => {
                ("Stream Engine isn't running", "It normally starts by itself when you log in. Start it now; it will start by itself from now on.".to_string())
            }
        };
        let clicked = widgets::empty_state(ui, &t, icon::POWER, title, &body, (!starting).then_some("Start Stream Engine"));
        if starting {
            ui.vertical_centered(|ui| ui.spinner());
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
        if clicked {
            app.down.starting = Some(std::time::Instant::now());
            let slot = app.down.result.clone();
            if let Ok(mut r) = slot.lock() {
                *r = None;
            }
            std::thread::spawn(move || {
                let out = std::process::Command::new("systemctl").args(["--user", "enable", "--now", "stream-engine"]).output();
                let r = match out {
                    Ok(o) if o.status.success() => Ok(()),
                    Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
                    Err(e) => Err(format!("Couldn't run systemctl: {e}")),
                };
                if let Ok(mut s) = slot.lock() {
                    *s = Some(r);
                }
            });
        }
    });
}

/// Every current warning, most severe first.
pub fn warnings(app: &App) -> Vec<Warning> {
    let mut w = Vec::new();
    let on_air = streaming(app) || app.m.get("show.live_since").and_then(Value::as_i64).unwrap_or(0) > 0;
    if !app.m.connected {
        w.push(Warning { text: "Stream Engine isn't running.".into(), fail: true, open: None, setup: false });
        return w;
    }
    for (a, v) in app.m.under("health") {
        let check = a.trim_start_matches("health.");
        let st = v.get_path("status").and_then(Value::as_str).unwrap_or("");
        let optional = OPTIONAL.iter().any(|o| check.starts_with(o));
        if st == "fail" || (st == "warn" && !optional) {
            let detail = v.get_path("detail").and_then(Value::as_str).unwrap_or("");
            let (title, open) = check_info(check);
            let setup = check.starts_with("twitch") || (check.starts_with("obs") && !on_air);
            let text = match friendly(app, check, st, detail) {
                Some(f) => f,
                None => format!("{title}: {detail}"),
            };
            w.push(Warning { text, fail: st == "fail" && !optional, open, setup });
        }
    }
    let errs = app.m.f("project.errors") as i64;
    if errs > 0 {
        w.push(Warning {
            text: format!("{errs} settings file(s) have a mistake. The last working version is still in use."),
            fail: false,
            open: Some(Panel::Console),
            setup: false,
        });
    }
    for (a, v) in app.m.under("patch") {
        if a.ends_with(".error") && v.as_str().is_some_and(|s| !s.is_empty()) {
            let name = a.trim_start_matches("patch.").trim_end_matches(".error");
            w.push(Warning {
                text: format!("Overlay \"{name}\" has an error. The previous version is still showing."),
                fail: false,
                open: Some(Panel::View(ViewId::Patches)),
                setup: false,
            });
        }
    }
    for c in ["wide", "tall"] {
        // only meaningful while OBS is connected (otherwise "OBS isn't open" says it all)
        if app.m.b("obs.link") && app.m.b(&format!("obs.stale.{c}")) {
            w.push(Warning {
                text: format!("OBS isn't getting the {} video.", if c == "wide" { "main" } else { "vertical" }),
                fail: true,
                open: None,
                setup: false,
            });
        }
    }
    if app.m.b("obs.fallback.active") {
        w.push(Warning { text: "OBS is showing the \"Technical difficulties\" screen.".into(), fail: true, open: None, setup: false });
    }
    for (a, v) in app.m.under("obs.output") {
        if let Some(id) = a.strip_suffix(".dropped").and_then(|p| p.strip_prefix("obs.output."))
            && v.as_f64().is_some_and(|d| d > 0.0)
        {
            let label = app.m.str(&format!("obs.output.{id}.label")).to_string();
            w.push(Warning {
                text: format!("{}: {} frames dropped (internet too slow?)", if label.is_empty() { id } else { &label }, v),
                fail: false,
                open: None,
                setup: false,
            });
        }
    }
    let gpu = app.m.f("perf.gpu_ms");
    if gpu > 8.0 {
        w.push(Warning {
            text: "The graphics card is busy. Previews here are slowed down to keep the stream smooth.".into(),
            fail: gpu > 14.0,
            open: Some(Panel::View(ViewId::Performance)),
            setup: false,
        });
    }
    let xruns = app.m.f("perf.audio.xruns") as i64;
    if xruns > 0 {
        w.push(Warning { text: format!("Sound glitched {xruns} time(s)."), fail: false, open: Some(Panel::View(ViewId::Performance)), setup: false });
    }
    if app.m.b("show.panic") {
        w.push(Warning { text: "Emergency stop is on.".into(), fail: true, open: None, setup: false });
    }
    w.sort_by_key(|x| (x.setup, !x.fail));
    w
}

// ---- Go live checklist (§15.8: preflight all green, or acknowledged) ------------------------------

/// Optional extras: not set up doesn't hold up going live.
const OPTIONAL_EXTRAS: [&str; 3] = ["relay", "youtube", "tiktok"];

/// How a preflight check reads in the checklist, most urgent first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Fail,
    Warn,
    /// An optional extra that isn't set up.
    Optional,
    Pass,
}

pub fn level(check: &str, status: &str) -> Level {
    let base = check.split('.').next().unwrap_or(check);
    match status {
        "pass" => Level::Pass,
        "fail" => Level::Fail,
        "warn" if OPTIONAL_EXTRAS.contains(&base) => Level::Optional,
        _ => Level::Warn,
    }
}

/// Going on air needs every check green (optional extras may be off) or "Go live anyway".
/// `None`: the checks haven't answered.
pub fn may_go_live(levels: Option<&[Level]>, ack: bool) -> bool {
    ack || levels.is_some_and(|l| l.iter().all(|l| matches!(l, Level::Pass | Level::Optional)))
}

/// A plain sentence for a check that passed.
fn ready_text(check: &str, detail: &str) -> String {
    match check {
        "obs" => "OBS is open and getting your video.",
        "twitch" => "Connected to Twitch.",
        "devices" => "Everything you use is plugged in.",
        "sources" => "Your cameras have a picture.",
        "audio.mic" => "Your microphone is picking up sound.",
        "audio.obs" => "OBS can hear Stream Engine.",
        "dmx" => "The lights box is connected.",
        "mixer" => "The mixing desk is connected.",
        "disk" => return format!("Enough disk space ({detail})."),
        "idle_inhibitor" => "Your screen will stay awake during the show.",
        "gpu" => "The graphics card has room to spare.",
        "night_light" => "Night light is off.",
        "backup" => "Backups are up to date.",
        "recordings" => "Recordings fit in the space you allowed.",
        _ => return format!("{}: ready.", check_info(check).0),
    }
    .into()
}

pub struct Check {
    pub level: Level,
    pub text: String,
    pub open: Option<Panel>,
}

/// The checklist from a `preflight` reply: most urgent first, each sentence once.
fn checklist(app: &App, list: &[Value]) -> Vec<Check> {
    let mut out: Vec<Check> = Vec::new();
    for v in list {
        let s = |k: &str| v.get_path(k).and_then(Value::as_str).unwrap_or("");
        let (name, status, detail) = (s("name"), s("status"), s("detail"));
        let lv = level(name, status);
        let (title, open) = check_info(name);
        let text = match lv {
            Level::Pass => ready_text(name, detail),
            Level::Optional => format!("{title}: not set up (optional)."),
            Level::Fail | Level::Warn => friendly(app, name, status, detail).unwrap_or_else(|| format!("{title}: {detail}")),
        };
        if !out.iter().any(|c| c.text == text) {
            out.push(Check { level: lv, text, open: open.filter(|_| matches!(lv, Level::Fail | Level::Warn)) });
        }
    }
    out.sort_by_key(|c| c.level);
    out
}

/// The "Before you go live" window.
#[derive(Default)]
pub struct GoLive {
    pub open: bool,
    /// "Go live anyway" is on.
    pub ack: bool,
    /// `preflight` replies already seen when it opened: only newer ones count.
    seq: u64,
    opened: Option<std::time::Instant>,
    asked: Option<std::time::Instant>,
}

/// The Go live button: run every check and show the list.
pub fn open_golive(app: &mut App) {
    let now = std::time::Instant::now();
    app.golive = GoLive { open: true, ack: false, seq: app.m.q_seq("preflight"), opened: Some(now), asked: Some(now) };
    app.m.query("preflight", Value::Null);
}

fn golive_window(app: &mut App, ctx: &egui::Context) {
    let t = app.t.clone();
    // keep it current while open: disk, GPU, something fixed meanwhile
    if app.golive.asked.is_none_or(|a| a.elapsed().as_secs_f32() > 3.0) {
        app.golive.asked = Some(std::time::Instant::now());
        app.m.query("preflight", Value::Null);
    }
    let fresh = app.m.q_seq("preflight") > app.golive.seq;
    let failed = app.m.query_errors.contains_key("preflight") || app.golive.opened.is_some_and(|o| o.elapsed().as_secs() >= 6);
    let checks: Option<Vec<Check>> = fresh.then(|| checklist(app, app.m.q_list("preflight")));
    let levels: Option<Vec<Level>> = checks.as_ref().map(|c| c.iter().map(|c| c.level).collect());
    let problems = checks.as_ref().map_or(0, |c| c.iter().filter(|c| matches!(c.level, Level::Fail | Level::Warn)).count());
    let mut ack = app.golive.ack;
    let (mut start, mut fix, mut close) = (None, None, false);
    let resp = egui::Modal::new(egui::Id::new("golive")).show(ctx, |ui| {
        ui.set_width(600.0);
        ui.label(RichText::new("Before you go live").font(font_semibold(type_scale::LARGE)).color(t.fg));
        ui.add_space(10.0);
        match &checks {
            None if failed => {
                widgets::callout(
                    ui,
                    &t,
                    Tone::Warn,
                    icon::WARN,
                    "The checks didn't answer",
                    "Stream Engine couldn't check everything. You can still go live.",
                    None,
                );
            }
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Checking everything…").color(t.text_dim));
                });
            }
            Some(_) if problems == 0 => {
                widgets::callout(ui, &t, Tone::Ok, icon::CHECK, "Everything is ready", "Pick how you want to start.", None);
            }
            Some(_) => {
                let title = if problems == 1 { "1 thing isn't ready".to_string() } else { format!("{problems} things aren't ready") };
                widgets::callout(ui, &t, Tone::Warn, icon::WARN, &title, "Fix them first, or switch on \"Go live anyway\".", None);
            }
        }
        if let Some(checks) = &checks {
            ui.add_space(8.0);
            egui::ScrollArea::vertical().max_height(380.0).auto_shrink([false, true]).show(ui, |ui| {
                for c in checks {
                    ui.horizontal(|ui| {
                        let (ic, col) = match c.level {
                            Level::Fail => (icon::CROSS, t.bright_red),
                            Level::Warn => (icon::WARN, t.yellow),
                            Level::Optional => (icon::DOT, t.text_dim),
                            Level::Pass => (icon::CHECK, t.green),
                        };
                        ui.label(RichText::new(ic).color(col));
                        let fg = if matches!(c.level, Level::Fail | Level::Warn) { t.fg } else { t.text_dim };
                        ui.add(egui::Label::new(RichText::new(&c.text).color(fg)).wrap());
                        if let Some(p) = c.open {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if widgets::button_ex(ui, &t, None, "Fix", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                                    fix = Some(p);
                                }
                            });
                        }
                    });
                    ui.add_space(2.0);
                }
            });
        }
        if problems > 0 || (checks.is_none() && failed) {
            ui.add_space(10.0);
            widgets::toggle_row(ui, &t, "Go live anyway", "I've looked at these and want to start.", &mut ack);
        }
        ui.add_space(14.0);
        let ok = may_go_live(levels.as_deref(), ack);
        ui.horizontal(|ui| {
            if widgets::button_ex(ui, &t, Some(icon::CLOCK), "Starting-soon screen first", Kind::Secondary, Size::Medium, 0.0, ok).clicked() {
                start = Some("preshow");
            }
            if widgets::button_ex(ui, &t, Some(icon::LIVE), "Go live right now", Kind::Live, Size::Medium, 0.0, ok).clicked() {
                start = Some("live");
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::button_ex(ui, &t, None, "Not now", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                    close = true;
                }
            });
        });
    });
    app.golive.ack = ack;
    if let Some(mode) = start {
        // mode first: OBS judges stream.start after it (rehearsal never streams)
        set_mode(app, mode);
        obs(app, "obs.stream.start");
        app.golive = GoLive::default();
    } else if let Some(p) = fix {
        app.golive = GoLive::default();
        app.open_panel(p);
    } else if close || resp.should_close() {
        app.golive = GoLive::default();
    }
}

/// Start OBS detached from the UI (closing the UI must not close OBS).
pub fn open_obs(app: &mut App) {
    match std::process::Command::new("setsid").args(["-f", "obs"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
        Ok(_) => app.m.toast("Opening OBS…", false),
        Err(e) => app.m.toast(format!("Couldn't open OBS: {e}"), true),
    }
}

fn streaming(app: &App) -> bool {
    app.m.b("obs.stream.active") || app.m.under("obs.output").any(|(a, v)| a.ends_with(".active") && v.truthy())
}

fn set_mode(app: &mut App, mode: &str) {
    app.m.command(Op::ModeSet { mode: mode.into() });
}

fn obs(app: &mut App, name: &str) {
    app.m.command(Op::Action { name: name.into(), args: Value::Null });
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let mode = app.m.str("show.mode").to_string();
    let since = app.m.get("show.live_since").and_then(Value::as_i64).unwrap_or(0);
    let on_air = streaming(app) || since > 0;
    ui.horizontal_centered(|ui| {
        // ---- on-air state -------------------------------------------------------------------
        let (txt, color) = if !app.m.connected {
            ("Engine offline".to_string(), t.bright_red)
        } else if on_air {
            let time = if since > 0 { format!("  {}", hms((now_ms() - since) / 1000)) } else { String::new() };
            (format!("ON AIR{time}"), t.tally_program())
        } else {
            ("Off air".to_string(), t.text_dim)
        };
        let fid = font_bold(type_scale::BODY);
        let g = ui.painter().layout_no_wrap(txt, fid, color);
        let (r, _) = ui.allocate_exact_size(egui::vec2(g.size().x + 34.0, 36.0), egui::Sense::hover());
        let fill = if on_air { t.tally_program() } else { t.surface };
        ui.painter().rect_filled(r, radius::PILL, fill);
        let fg = if on_air { egui::Color32::WHITE } else { color };
        ui.painter().circle_filled(egui::pos2(r.left() + 15.0, r.center().y), 4.5, fg);
        ui.painter().galley_with_override_text_color(egui::pos2(r.left() + 26.0, r.center().y - g.size().y / 2.0), g, fg);
        ui.add_space(4.0);

        // ---- show mode (plain words; only once something is happening) --------------------------
        if app.m.connected && (on_air || (mode != "offline" && !mode.is_empty())) {
            let label = format!("{}  {}", mode_label(&mode), icon::DOWN);
            let resp = widgets::button_ex(ui, &t, None, &label, Kind::Ghost, Size::Medium, 0.0, true).on_hover_text("What the show is doing right now");
            egui::Popup::menu(&resp).show(|ui| {
                ui.set_min_width(220.0);
                let modes: Vec<String> = app.m.q_list("modes").iter().filter_map(|v| v.as_str().map(String::from)).collect();
                for md in modes {
                    if ui.selectable_label(md == mode, mode_label(&md)).clicked() {
                        set_mode(app, &md);
                        ui.close();
                    }
                }
            });
        }

        // ---- health --------------------------------------------------------------------------
        let warns = warnings(app);
        let fails = warns.iter().filter(|w| w.fail && !w.setup).count();
        let checks = warns.iter().filter(|w| !w.fail && !w.setup).count();
        let setup = warns.iter().filter(|w| w.setup).count();
        let plural = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
        let (text, c) = if fails > 0 && checks > 0 {
            (format!("{} · {}", plural(fails, "problem", "problems"), plural(checks, "thing to check", "things to check")), t.bright_red)
        } else if fails > 0 {
            (plural(fails, "problem", "problems"), t.bright_red)
        } else if checks > 0 {
            (plural(checks, "thing to check", "things to check"), t.yellow)
        } else if setup > 0 {
            ("Finish setting up".to_string(), t.accent)
        } else {
            ("All good".to_string(), t.green)
        };
        let resp = widgets::pill(ui, &t, if warns.is_empty() { icon::CHECK } else { icon::WARN }, &text, se_ui_kit::widgets::LedState::Idle);
        ui.painter().rect_stroke(resp.rect, radius::PILL, egui::Stroke::new(1.0, mix(t.border, c, 0.7)), egui::StrokeKind::Inside);
        let resp = resp.on_hover_text("Click to see details");
        egui::Popup::menu(&resp).show(|ui| {
            ui.set_min_width(420.0);
            if warns.is_empty() {
                ui.label(RichText::new(format!("{}  Everything is working.", icon::CHECK)).color(t.green));
            }
            for w in &warns {
                ui.horizontal(|ui| {
                    let (ic, c) = match (w.setup, w.fail) {
                        (true, _) => (icon::ROCKET, t.accent),
                        (false, true) => (icon::CROSS, t.bright_red),
                        (false, false) => (icon::WARN, t.yellow),
                    };
                    ui.label(RichText::new(ic).color(c));
                    ui.add(egui::Label::new(RichText::new(&w.text).color(t.fg)).wrap());
                    if let Some(p) = w.open {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::button_ex(ui, &t, None, "Fix", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                                app.open_panel(p);
                                ui.close();
                            }
                        });
                    }
                });
            }
        });
        if app.m.b("obs.record.active") {
            widgets::badge(ui, &t, "Recording", t.tally_program());
        }
        if app.m.connected && mode == "rehearsal" {
            let viewers = if app.m.b("show.rehearsal.simulating") { "Pretend viewers cheer, follow and chat now and then. " } else { "" };
            widgets::badge(ui, &t, "Practice run: nothing goes out", t.accent).on_hover_text(format!(
                "{viewers}The stream doesn't start, the lights show the practice in the Lights view (your room lights keep their look), and nothing is changed on Twitch. Pick another mode to finish."
            ));
        }

        // ---- right side: big actions ------------------------------------------------------------
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let obs_ok = app.m.b("obs.link");
            if on_air {
                let r = widgets::button_ex(ui, &t, Some(icon::STOP), "End stream", Kind::Danger, Size::Large, 150.0, app.m.connected);
                egui::Popup::menu(&r).show(|ui| {
                    ui.set_min_width(260.0);
                    ui.label(RichText::new("End the stream?").font(font_semibold(type_scale::LARGE)));
                    ui.add_space(6.0);
                    if widgets::button_ex(ui, &t, Some(icon::FILM), "Play the ending first", Kind::Secondary, Size::Medium, 240.0, true).clicked() {
                        set_mode(app, "outro");
                        ui.close();
                    }
                    ui.add_space(4.0);
                    if widgets::button_ex(ui, &t, Some(icon::STOP), "Stop now", Kind::Danger, Size::Medium, 240.0, true).clicked() {
                        obs(app, "obs.stream.stop");
                        set_mode(app, "offline");
                        ui.close();
                    }
                });
            } else if app.m.connected && !obs_ok {
                let r = widgets::button_ex(ui, &t, Some(icon::PLAY), "Open OBS", Kind::Secondary, Size::Medium, 0.0, true)
                    .on_hover_text("OBS sends your stream to Twitch. Open it and Go live appears here.");
                if r.clicked() {
                    open_obs(app);
                }
            } else {
                let r = widgets::button_ex(ui, &t, Some(icon::LIVE), "Go live", Kind::Live, Size::Large, 150.0, app.m.connected);
                if r.on_hover_text("Checks that everything is ready, then starts").clicked() {
                    open_golive(app);
                }
            }
            if on_air && mode != "live" && app.m.connected {
                if widgets::button_ex(ui, &t, Some(icon::LIVE), "Go live", Kind::Primary, Size::Large, 0.0, true).clicked() {
                    set_mode(app, "live");
                }
            } else if on_air
                && mode == "live"
                && widgets::button_ex(ui, &t, Some(icon::PAUSE), "Be right back", Kind::Secondary, Size::Large, 0.0, true).clicked()
            {
                set_mode(app, "brb");
            }
            ui.add_space(8.0);
            if widgets::hold_button(ui, &t, "Emergency stop", t.bright_red, 0.8) {
                app.m.command(Op::Panic);
            }
            let clean = widgets::button_ex(ui, &t, Some(icon::BROOM), "Clear chat effects", Kind::Ghost, Size::Medium, 0.0, app.m.connected);
            if clean.on_hover_text(format!("Remove every effect viewers triggered from chat ({})", app.keys_label("clean"))).clicked() {
                app.m.command(Op::Clean);
            }
        });
    });
    let r = ui.max_rect();
    ui.painter().hline(r.x_range(), r.bottom() - 0.5, egui::Stroke::new(1.0, t.border));
    if app.golive.open {
        golive_window(app, &ui.ctx().clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hms_formats_hours() {
        assert_eq!(hms(0), "00:00:00");
        assert_eq!(hms(2 * 3600 + 14 * 60 + 33), "02:14:33");
        assert_eq!(hms(-5), "00:00:00");
    }

    #[test]
    fn modes_read_as_words() {
        assert_eq!(mode_label("preshow"), "Starting soon");
        assert_eq!(mode_label("ad_break"), "Ad break");
        assert_eq!(mode_label("my_custom"), "My custom");
    }

    #[test]
    fn going_live_needs_all_green_or_an_acknowledgement() {
        use Level::*;
        assert_eq!(level("tiktok", "warn"), Optional, "an extra that isn't set up");
        assert_eq!(level("youtube.quota", "warn"), Optional);
        assert_eq!(level("youtube", "fail"), Fail, "a broken extra still counts");
        assert_eq!(level("twitch", "warn"), Warn, "Twitch is not an optional extra here");
        assert_eq!(level("disk", "unknown"), Warn);
        assert!(may_go_live(Some(&[Pass, Optional, Pass]), false));
        assert!(!may_go_live(Some(&[Pass, Warn]), false));
        assert!(may_go_live(Some(&[Fail, Warn]), true));
        assert!(!may_go_live(None, false), "no answer yet");
        assert!(may_go_live(None, true), "no answer, but acknowledged");
    }
}
