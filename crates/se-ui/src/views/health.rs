//! Settings → Health and the alert banner under the header. Every `health.*` check with its
//! status, how long it has been like that, the scoped recoveries of [`crate::recovery`] and
//! "Fix with AI" ([`crate::diagnose`]). A recovery counts as done only when its check is seen
//! passing again (or the engine reconnects), never on the action's acknowledgement.

use crate::app::{App, ViewId};
use crate::diagnose;
use crate::health::{self, Check, Status};
use crate::recovery::{self, Recovery, Safety, Step};
use crate::views::status;
use egui::{Align, Layout, RichText};
use se_proto::Value;
use se_ui_kit::theme::{font_semibold, mix, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Pseudo-check name for the engine connection.
pub const ENGINE: &str = "engine";
/// How long a recovery's outcome stays next to its check.
const OUTCOME_SHOWN: Duration = Duration::from_secs(30);
/// Banner rows before "and N more".
const BANNER_ROWS: usize = 3;

/// Result of a helper command run in the background (`systemctl --user …`).
pub type Slot = Arc<Mutex<Option<Result<(), String>>>>;

struct Pending {
    started: Instant,
    wait: Duration,
    step: Step,
    /// Background command result (service restart, engine start).
    result: Option<Slot>,
    /// Twitch sign-in: the device-code page was opened.
    opened_url: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Outcome {
    Recovered,
    StillFailing(String),
}

#[derive(Default)]
pub struct HealthState {
    pub tracker: health::Tracker,
    /// Current checks (rebuilt when `health.*` or the connection changes).
    checks: Vec<Check>,
    seen_gen: Option<(u64, u64)>,
    /// Recoveries waiting for their check to pass, by check.
    pending: HashMap<String, Pending>,
    outcome: HashMap<String, (Outcome, Instant)>,
    /// Acknowledged banner rows, until their status changes or the engine reconnects.
    dismissed: HashMap<String, Status>,
    /// A recovery viewers would notice, waiting for "Do it".
    confirm: Option<(String, Recovery)>,
    /// Fix with AI folder, keyed by `engine.info.share`.
    ai: Option<(String, Result<PathBuf, String>)>,
}

impl HealthState {
    fn retain_dismissed(&mut self, connected: bool) {
        self.dismissed.retain(|name, status| {
            if name == ENGINE {
                !connected
            } else {
                self.checks.iter().any(|c| c.name == *name && c.status == *status)
            }
        });
    }
}

/// One banner row.
pub struct Item {
    check: String,
    title: String,
    detail: String,
    fail: bool,
    since: String,
}

// ---- per-frame bookkeeping ----------------------------------------------------------------------

fn status_of(app: &App, check: &str) -> Status {
    Status::parse(app.m.get(&format!("health.{check}")).and_then(|v| v.get_path("status")).and_then(Value::as_str).unwrap_or(""))
}

fn title_of(check: &str) -> String {
    if check == ENGINE { "Stream Engine".into() } else { health::title(check) }
}

/// Track status transitions and settle recoveries. Runs every frame.
pub fn tick(app: &mut App, ctx: &egui::Context) {
    let now = Instant::now();
    let g = (app.m.gen_of("health"), app.m.conn_gen);
    if app.health.seen_gen != Some(g) {
        if app.health.seen_gen.is_some_and(|(_, connection)| connection != g.1) {
            app.health.dismissed.clear();
        }
        app.health.seen_gen = Some(g);
        app.health.checks = health::checks(&app.m);
        app.health.tracker.update(&app.health.checks, app.m.conn_gen, now);
    }
    app.health.retain_dismissed(app.m.connected);
    if app.health.pending.is_empty() {
        app.health.outcome.retain(|_, (_, at)| at.elapsed() < OUTCOME_SHOWN);
        return;
    }
    let uri = app.m.str("twitch.auth.verification_uri").to_string();
    let auth_pending = app.m.str("twitch.auth.status") == "pending";
    let names: Vec<String> = app.health.pending.keys().cloned().collect();
    for check in names {
        let passed = if check == ENGINE { app.m.connected } else { app.m.connected && status_of(app, &check) == Status::Pass };
        let Some(p) = app.health.pending.get_mut(&check) else { continue };
        if matches!(p.step, Step::Action { name: "twitch.auth.start", .. }) && !p.opened_url && auth_pending && !uri.is_empty() {
            p.opened_url = true;
            ctx.open_url(egui::OpenUrl::new_tab(&uri));
        }
        let failed = p.result.as_ref().and_then(|r| r.lock().ok().and_then(|g| g.clone())).and_then(Result::err);
        let outcome = if passed {
            Outcome::Recovered
        } else if let Some(e) = failed {
            Outcome::StillFailing(e)
        } else if p.started.elapsed() > p.wait {
            Outcome::StillFailing("Still failing. Try Fix with AI.".into())
        } else {
            continue;
        };
        app.health.pending.remove(&check);
        let title = title_of(&check);
        match &outcome {
            Outcome::Recovered => app.m.toast(format!("{title} is working again."), false),
            Outcome::StillFailing(why) => app.m.toast(format!("{title}: {why}"), true),
        }
        app.health.outcome.insert(check, (outcome, now));
    }
    ctx.request_repaint_after(Duration::from_millis(250));
}

// ---- recovery -----------------------------------------------------------------------------------

fn systemctl(args: [&'static str; 3]) -> Slot {
    let slot: Slot = Arc::default();
    let out = slot.clone();
    std::thread::spawn(move || {
        let r = match std::process::Command::new("systemctl").args(args).output() {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Err(e) => Err(format!("Couldn't run systemctl: {e}")),
        };
        if let Ok(mut s) = out.lock() {
            *s = Some(r);
        }
    });
    slot
}

fn recovering(app: &App, check: &str) -> bool {
    app.health.pending.contains_key(check)
}

/// Dispatch a recovery (once per check at a time).
fn run(app: &mut App, check: &str, r: Recovery) {
    if recovering(app, check) {
        return;
    }
    let result = match &r.step {
        Step::Action { name, args } => {
            app.m.action(name, args.clone());
            None
        }
        Step::Restart(unit) => Some(systemctl(["--user", "restart", unit.name()])),
        Step::StartEngine => Some(status::start_engine(app)),
    };
    app.health.outcome.remove(check);
    app.health.pending.insert(check.to_string(), Pending { started: Instant::now(), wait: r.wait, step: r.step, result, opened_url: false });
}

/// A Recover click: safe ones run, the others ask first.
fn click(app: &mut App, check: &str, r: Recovery) {
    match r.safety {
        Safety::Safe => run(app, check, r),
        Safety::Interrupts(_) | Safety::OffAirOnly(_) => app.health.confirm = Some((check.to_string(), r)),
    }
}

/// Recoveries for a check right now.
fn offered(app: &App, c: &Check) -> Vec<Recovery> {
    let on_air = status::on_air(app);
    recovery::offered(&c.name, &c.detail, on_air, &|s: &str| crate::views::composition::source_label(app, s))
}

fn first_safe(app: &App, check: &str, detail: &str, st: Status) -> Option<Recovery> {
    if check == ENGINE {
        return Some(recovery::engine_down());
    }
    offered(app, &Check::new(check, st, detail)).into_iter().find(|r| r.safety == Safety::Safe)
}

// ---- Fix with AI --------------------------------------------------------------------------------

/// The Stream Engine folder holding the diagnosis overlay, or why Fix with AI is unavailable.
fn ai_share(app: &mut App) -> Result<PathBuf, String> {
    let engine = app.m.q("engine.info").and_then(|v| v.get_path("share")).and_then(Value::as_str).unwrap_or("").to_string();
    match &app.health.ai {
        Some((k, r)) if *k == engine => r.clone(),
        _ => {
            let r = diagnose::share_dir(&engine, std::env::var("STREAM_ENGINE_SHARE").ok().as_deref());
            app.health.ai = Some((engine, r.clone()));
            r
        }
    }
}

/// Write the redacted bundle and open a diagnosis session about `check` (`None`: everything).
/// Sends nothing to the engine.
pub fn fix_with_ai(app: &mut App, check: Option<&str>) {
    let share = match ai_share(app) {
        Ok(s) => s,
        Err(e) => return app.m.toast(e, true),
    };
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) else {
        return app.m.toast("Fix with AI needs XDG_RUNTIME_DIR (the desktop session's runtime folder).", true);
    };
    let project = app.m.q("engine.info").and_then(|v| v.get_path("project")).and_then(Value::as_str).filter(|s| !s.is_empty()).map(PathBuf::from);
    let unix_s = status::now_ms() / 1000;
    let engine_down_s = (!app.m.connected).then(|| status::down_for(app).as_secs());
    let text = diagnose::Bundle {
        unix_s,
        m: &app.m,
        on_air: status::on_air(app),
        check: check.filter(|c| *c != ENGINE),
        checks: &app.health.checks,
        tracker: &app.health.tracker,
        engine_down_s,
        now: Instant::now(),
    }
    .markdown();
    let bundle = match diagnose::write_bundle(std::path::Path::new(&runtime), unix_s, check, &text) {
        Ok(p) => p,
        Err(e) => return app.m.toast(format!("Couldn't write the diagnostic bundle: {e}"), true),
    };
    let argv = diagnose::argv(&share, project.as_deref(), &bundle, check);
    match diagnose::spawn(&argv) {
        Ok(()) => app.m.toast("Opening Fix with AI. It asks before it changes anything.", false),
        Err(e) => app.m.toast(format!("Couldn't open Fix with AI: {e}"), true),
    }
}

fn ai_button(app: &mut App, ui: &mut egui::Ui, check: Option<&str>, size: Size) {
    let t = app.t.clone();
    let share = ai_share(app);
    let r = widgets::button_ex(ui, &t, Some(icon::SPARKLE), "Fix with AI", Kind::Ghost, size, 0.0, share.is_ok());
    let r = match &share {
        Ok(_) => r.on_hover_text("Opens a diagnosis session in a terminal with a redacted report. It only looks until you approve a change."),
        Err(e) => r.on_disabled_hover_text(e.as_str()),
    };
    if r.clicked() {
        fix_with_ai(app, check);
    }
}

fn recover_button(app: &mut App, ui: &mut egui::Ui, check: &str, r: Recovery, label: &str, kind: Kind, size: Size) {
    let t = app.t.clone();
    let busy = recovering(app, check);
    let text = if busy { "Recovering…" } else { label };
    let tip = match &r.safety {
        Safety::Safe => format!("{}: viewers won't notice.", r.label),
        Safety::Interrupts(what) => format!("{}. Asks first: {what}", r.label),
        Safety::OffAirOnly(what) => format!("{}. Off air only: {what}", r.label),
    };
    let resp = widgets::button_ex(ui, &t, if busy { None } else { Some(icon::UNDO) }, text, kind, size, 0.0, !busy && app.health.confirm.is_none());
    if busy {
        ui.spinner();
    }
    if resp.on_hover_text(tip).clicked() {
        click(app, check, r);
    }
}

fn outcome_label(app: &App, ui: &mut egui::Ui, check: &str) {
    let t = &app.t;
    match app.health.outcome.get(check) {
        Some((Outcome::Recovered, _)) => {
            ui.label(RichText::new(format!("{} Working again", icon::CHECK)).size(type_scale::SMALL).color(t.green));
        }
        Some((Outcome::StillFailing(why), _)) => {
            ui.label(RichText::new(why).size(type_scale::SMALL).color(t.bright_red));
        }
        None => {}
    }
}

// ---- banner -------------------------------------------------------------------------------------

/// What the alert banner shows: an unreachable engine, then unacknowledged alerting checks.
pub fn banner_items(app: &App) -> Vec<Item> {
    let now = Instant::now();
    if !app.m.connected {
        let down = status::down_for(app);
        if down < Duration::from_secs(3) || app.health.dismissed.contains_key(ENGINE) {
            return Vec::new();
        }
        let why = app.m.last_disconnect.as_deref().filter(|s| !s.is_empty()).unwrap_or("not running");
        return vec![Item {
            check: ENGINE.into(),
            title: "Stream Engine".into(),
            detail: format!("Can't reach the engine ({why})."),
            fail: true,
            since: format!("for {}", health::ago(down)),
        }];
    }
    health::alerts(&app.health.checks, status::on_air(app))
        .into_iter()
        .filter(|c| app.health.dismissed.get(&c.name) != Some(&c.status))
        .map(|c| Item {
            check: c.name.clone(),
            title: health::title(&c.name),
            detail: c.detail.clone(),
            fail: c.status == Status::Fail,
            since: app.health.tracker.get(&c.name).map(|s| s.for_how_long(now)).unwrap_or_default(),
        })
        .collect()
}

/// The alert banner (only drawn when [`banner_items`] has something).
pub fn banner(app: &mut App, ui: &mut egui::Ui, items: &[Item]) {
    let t = app.t.clone();
    for it in items.iter().take(BANNER_ROWS) {
        let c = if it.fail { t.bright_red } else { t.yellow };
        ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 34.0), Layout::right_to_left(Align::Center), |ui| {
            let close = ui.push_id(("dismiss", &it.check), |ui| {
                widgets::icon_button(ui, &t, icon::CROSS, &format!("Dismiss {} notification", it.title))
                    .on_hover_text("Hide this banner until its status changes. The problem stays in Health.")
            });
            if close.inner.clicked() {
                app.health.dismissed.insert(it.check.clone(), if it.fail { Status::Fail } else { Status::Warn });
            }
            ai_button(app, ui, Some(&it.check), Size::Small);
            if widgets::button_ex(ui, &t, None, "Details", Kind::Ghost, Size::Small, 0.0, true).on_hover_text("Every check, with what you can do").clicked() {
                app.open_view(ViewId::Health);
            }
            let st = if it.fail { Status::Fail } else { Status::Warn };
            if let Some(r) = first_safe(app, &it.check, &it.detail, st) {
                recover_button(app, ui, &it.check, r, "Recover", Kind::Primary, Size::Small);
            }
            outcome_label(app, ui, &it.check);
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.label(RichText::new(if it.fail { icon::CROSS } else { icon::WARN }).color(c));
                ui.label(RichText::new(&it.title).font(font_semibold(type_scale::BODY)).color(t.fg));
                if !it.since.is_empty() {
                    ui.label(RichText::new(&it.since).size(type_scale::SMALL).color(c));
                }
                ui.add(egui::Label::new(RichText::new(&it.detail).color(t.text_dim)).truncate()).on_hover_text(&it.detail);
            });
        });
    }
    if items.len() > BANNER_ROWS {
        let more = items.len() - BANNER_ROWS;
        if ui.link(RichText::new(format!("and {more} more: see Health")).size(type_scale::SMALL).color(t.text_dim)).clicked() {
            app.open_view(ViewId::Health);
        }
    }
}

/// Frame colours for the banner: red for a failure, yellow for on-air warnings only.
pub fn banner_frame(app: &App, items: &[Item]) -> egui::Frame {
    let t = &app.t;
    let c = if items.iter().any(|i| i.fail) { t.bright_red } else { t.yellow };
    egui::Frame::new().fill(mix(t.chrome, c, 0.14)).stroke(egui::Stroke::new(1.0, mix(t.border, c, 0.5))).inner_margin(egui::Margin {
        left: 24,
        right: 24,
        top: 6,
        bottom: 6,
    })
}

// ---- confirmation ---------------------------------------------------------------------------------

/// "Viewers would notice this": shown over every page while a confirmation is open.
pub fn modals(app: &mut App, ctx: &egui::Context) {
    let Some((check, r)) = app.health.confirm.clone() else { return };
    let t = app.t.clone();
    let on_air = status::on_air(app);
    let (what, allowed) = match &r.safety {
        Safety::Interrupts(w) => (w.clone(), true),
        Safety::OffAirOnly(w) => (w.clone(), !on_air),
        Safety::Safe => (String::new(), true),
    };
    let (mut go, mut close) = (false, false);
    let resp = egui::Modal::new(egui::Id::new("health-confirm")).show(ctx, |ui| {
        ui.set_width(460.0);
        ui.label(RichText::new(format!("{}?", r.label)).font(font_semibold(type_scale::LARGE)).color(t.fg));
        ui.add_space(spacing::S);
        let lead = if on_air { "Viewers will notice:" } else { "If you were live, viewers would notice:" };
        ui.label(RichText::new(lead).color(t.text_dim));
        ui.add(egui::Label::new(RichText::new(&what).color(t.fg)).wrap());
        if !allowed {
            ui.add_space(spacing::S);
            ui.label(RichText::new("Not while you're on air. Do it after the stream.").color(t.bright_red));
        }
        ui.add_space(spacing::M);
        ui.horizontal(|ui| {
            if widgets::button_ex(ui, &t, None, "Do it", Kind::Danger, Size::Medium, 0.0, allowed).clicked() {
                go = true;
            }
            if widgets::button_ex(ui, &t, None, "Cancel", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                close = true;
            }
        });
    });
    if go {
        app.health.confirm = None;
        run(app, &check, r);
    } else if close || resp.should_close() {
        app.health.confirm = None;
    }
}

// ---- the view -----------------------------------------------------------------------------------

fn status_look(app: &App, st: Status) -> (&'static str, egui::Color32) {
    let t = &app.t;
    match st {
        Status::Fail => (icon::CROSS, t.bright_red),
        Status::Warn => (icon::WARN, t.yellow),
        Status::Unknown => (icon::HELP, t.text_dim),
        Status::Pass => (icon::CHECK, t.green),
    }
}

fn engine_row(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::panel(ui, &t, |ui| {
        ui.set_width(ui.available_width());
        let (st, text) = if app.m.connected {
            let info = app.m.q("engine.info");
            let version = info.and_then(|v| v.get_path("version")).and_then(Value::as_str).unwrap_or("?").to_string();
            (Status::Pass, format!("Connected · version {version} · pid {}", app.m.engine_pid))
        } else {
            let why = app.m.last_disconnect.clone().unwrap_or_else(|| "not running".into());
            (Status::Fail, format!("Can't reach the engine ({why}) for {}. Health below is from before.", health::ago(status::down_for(app))))
        };
        let (ic, c) = status_look(app, st);
        ui.horizontal(|ui| {
            ui.label(RichText::new(ic).color(c));
            ui.label(RichText::new("Stream Engine").font(font_semibold(type_scale::BODY)).color(t.fg));
            ui.add(egui::Label::new(RichText::new(text).color(t.text_dim)).wrap());
        });
        if st != Status::Pass {
            ui.horizontal(|ui| {
                recover_button(app, ui, ENGINE, recovery::engine_down(), "Start Stream Engine", Kind::Primary, Size::Small);
                ai_button(app, ui, Some(ENGINE), Size::Small);
                outcome_label(app, ui, ENGINE);
            });
        }
    });
}

fn check_row(app: &mut App, ui: &mut egui::Ui, c: &Check) {
    let t = app.t.clone();
    let (ic, col) = status_look(app, c.status);
    let since = app.health.tracker.get(&c.name).map(|s| s.for_how_long(Instant::now())).unwrap_or_default();
    let open = status::check_info(&c.name).1;
    widgets::panel(ui, &t, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(ic).color(col));
            ui.label(RichText::new(health::title(&c.name)).font(font_semibold(type_scale::BODY)).color(t.fg));
            ui.label(RichText::new(format!("health.{}", c.name)).size(type_scale::SMALL).color(t.text_faint));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let word = match c.status {
                    Status::Fail => "Failing",
                    Status::Warn => "Needs a look",
                    Status::Unknown => "Unknown",
                    Status::Pass => "OK",
                };
                ui.label(RichText::new(format!("{word} {since}")).size(type_scale::SMALL).color(if c.status == Status::Pass { t.text_dim } else { col }));
            });
        });
        if !c.detail.is_empty() {
            ui.add(egui::Label::new(RichText::new(&c.detail).color(t.text_dim)).wrap());
        }
        if c.status == Status::Pass {
            return;
        }
        let recoveries = offered(app, c);
        ui.add_space(spacing::XS);
        ui.horizontal_wrapped(|ui| {
            for r in recoveries {
                let kind = if r.safety == Safety::Safe { Kind::Secondary } else { Kind::Ghost };
                let label = if r.safety == Safety::Safe { r.label.clone() } else { format!("{}…", r.label) };
                recover_button(app, ui, &c.name, r, &label, kind, Size::Small);
            }
            ai_button(app, ui, Some(&c.name), Size::Small);
            if let Some(p) = open
                && widgets::button_ex(ui, &t, Some(icon::RIGHT), "Open", Kind::Ghost, Size::Small, 0.0, true).on_hover_text("Where this is set up").clicked()
            {
                app.open_panel(p);
            }
            outcome_label(app, ui, &c.name);
        });
    });
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::hint(
        ui,
        &t,
        "Every check Stream Engine runs, worst first. Recover only does things viewers won't notice; anything they would notice asks first, and nothing that stops the stream is offered while you're on air. Fix with AI opens a diagnosis session that asks before it changes anything.",
    );
    ui.add_space(spacing::S);
    let checks = app.health.checks.clone();
    let failing = checks.iter().filter(|c| c.status == Status::Fail).count() + usize::from(!app.m.connected);
    let warning = checks.iter().filter(|c| c.status == Status::Warn).count();
    ui.horizontal(|ui| {
        let (text, c) = match (failing, warning) {
            (0, 0) => ("Everything is working.".to_string(), t.green),
            (0, w) => (format!("{w} need a look."), t.yellow),
            (f, 0) => (format!("{f} failing."), t.bright_red),
            (f, w) => (format!("{f} failing, {w} need a look."), t.bright_red),
        };
        ui.label(RichText::new(text).font(font_semibold(type_scale::BODY)).color(c));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| ai_button(app, ui, None, Size::Medium));
    });
    ui.add_space(spacing::S);
    egui::ScrollArea::vertical().id_salt("health").auto_shrink([false, false]).show(ui, |ui| {
        engine_row(app, ui);
        ui.add_space(spacing::XS);
        if checks.is_empty() {
            ui.label(RichText::new("No checks reported yet.").color(t.text_dim));
        }
        for c in &checks {
            check_row(app, ui, c);
            ui.add_space(spacing::XS);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};
    use se_proto::wire::ServerMsg;

    struct BannerApp(App);

    impl eframe::App for BannerApp {
        fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
            tick(&mut self.0, ui.ctx());
            let items = banner_items(&self.0);
            egui::CentralPanel::default().show(ui, |ui| banner(&mut self.0, ui, &items));
        }
    }

    fn harness() -> Harness<'static, BannerApp> {
        Harness::builder().with_size([1200.0, 400.0]).build_eframe(|cc| {
            let mut app = App::new(
                cc,
                crate::UiOpts { socket: Some("/nonexistent/se-ui-banner-test.sock".into()), layout: Some("single".into()), program_only: false },
            );
            app.m.connected = true;
            app.m.conn_gen = 1;
            BannerApp(app)
        })
    }

    fn publish(h: &mut Harness<'_, BannerApp>, name: &str, status: &str, detail: &str) {
        h.state_mut().0.m.apply(ServerMsg::State {
            changes: vec![(format!("health.{name}"), Value::map().with("status", status).with("detail", detail))],
        });
        h.run_steps(3);
    }

    #[test]
    fn dismissing_one_problem_keeps_health_and_other_problems_visible() {
        let mut h = harness();
        publish(&mut h, "relay", "fail", "retrying in 8s");
        publish(&mut h, "obs", "fail", "plugin disconnected");
        h.get_by_label("Dismiss Tips & queue link notification").click();
        h.run_steps(3);
        let app = &h.state().0;
        assert_eq!(banner_items(app).iter().map(|i| i.check.as_str()).collect::<Vec<_>>(), ["obs"]);
        assert_eq!(health::checks(&app.m).iter().filter(|c| c.status == Status::Fail).count(), 2);

        publish(&mut h, "relay", "fail", "retrying in 7s");
        assert_eq!(banner_items(&h.state().0).iter().map(|i| i.check.as_str()).collect::<Vec<_>>(), ["obs"]);
        publish(&mut h, "relay", "pass", "connected");
        publish(&mut h, "relay", "fail", "connection lost again");
        assert!(banner_items(&h.state().0).iter().any(|i| i.check == "relay"));
    }

    #[test]
    fn a_dismissed_warning_returns_when_it_becomes_a_failure() {
        let mut h = harness();
        h.state_mut().0.m.state.insert("show.live_since".into(), Value::Int(1));
        publish(&mut h, "relay", "warn", "reconnecting");
        h.get_by_label("Dismiss Tips & queue link notification").click();
        h.run_steps(3);
        assert!(banner_items(&h.state().0).is_empty());
        publish(&mut h, "relay", "fail", "connection failed");
        assert!(banner_items(&h.state().0).iter().any(|i| i.check == "relay" && i.fail));
    }

    #[test]
    fn dismissed_failures_return_after_engine_reconnection() {
        let mut h = harness();
        publish(&mut h, "relay", "fail", "connection refused");
        h.get_by_label("Dismiss Tips & queue link notification").click();
        h.run_steps(3);
        assert!(banner_items(&h.state().0).is_empty());
        h.state_mut().0.m.connected = false;
        h.run_steps(3);
        h.state_mut().0.m.connected = true;
        h.state_mut().0.m.conn_gen += 1;
        h.run_steps(3);
        assert_eq!(banner_items(&h.state().0).iter().map(|i| i.check.as_str()).collect::<Vec<_>>(), ["relay"]);
    }

    #[test]
    fn engine_disconnect_stays_dismissed_until_the_connection_recovers() {
        let mut state = HealthState::default();
        state.dismissed.insert(ENGINE.into(), Status::Fail);
        state.retain_dismissed(false);
        assert_eq!(state.dismissed.get(ENGINE), Some(&Status::Fail));
        state.retain_dismissed(true);
        state.retain_dismissed(false);
        assert!(!state.dismissed.contains_key(ENGINE));
    }
}
