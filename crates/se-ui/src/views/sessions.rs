//! Session review (§15.6, §18): past sessions → markers timeline → clip review cards (play,
//! approve, reject, retrim, upload), plus the cross-session review queue of clips waiting for
//! a decision. Data: `sessions`, `clips.session {session}`, `clips {status}` queries and
//! `clips.*` actions (se-clips).

use crate::app::App;
use egui::{Align2, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::widgets::{self, LedState, icon};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// View state kept in egui memory (no fields on `App`).
#[derive(Default)]
struct State {
    /// Selected session; `None` = review queue across sessions.
    selected: Option<String>,
    /// Decoded thumbnails by `path#version` (`None` = unreadable).
    thumbs: HashMap<String, Option<egui::TextureHandle>>,
    /// Retrim edits per clip id: (in, out) in recording seconds.
    trims: HashMap<i64, (f64, f64)>,
    /// Newest event id seen (clip events trigger a refresh).
    last_event: u64,
    last_query: Option<Instant>,
    last_sessions: Option<Instant>,
}

type Shared = Arc<Mutex<State>>;

fn state(ui: &egui::Ui) -> Shared {
    ui.ctx().data_mut(|d| d.get_temp_mut_or_insert_with::<Shared>(egui::Id::new("se.sessions.view"), Shared::default).clone())
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}
fn f(v: &Value, k: &str) -> f64 {
    v.get_path(k).and_then(Value::as_f64).unwrap_or(0.0)
}
fn i(v: &Value, k: &str) -> i64 {
    v.get_path(k).and_then(Value::as_i64).unwrap_or(0)
}

fn clock(secs: f64) -> String {
    let t = secs.max(0.0);
    let m = (t / 60.0).floor();
    format!("{}:{:05.2}", m as i64, t - m * 60.0)
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let st = state(ui);
    let mut st = st.lock().unwrap_or_else(|p| p.into_inner());
    refresh(app, &mut st);

    egui::Panel::left("sessions.list").resizable(true).default_size(260.0).show(ui, |ui| {
        widgets::section(ui, &t, icon::SESSION, "Sessions");
        let pending = app.m.f("clips.pending") as i64;
        let queue_label = format!("{} review queue ({pending})", icon::CHECK);
        if ui.selectable_label(st.selected.is_none(), RichText::new(queue_label).strong()).clicked() {
            st.selected = None;
            st.last_query = None;
        }
        job_status(app, ui, &t);
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for sess in app.m.q_list("sessions").to_vec() {
                let id = s(&sess, "id").to_string();
                let start = i(&sess, "started_at");
                let end = sess.get_path("ended_at").and_then(Value::as_i64);
                let dur = match end {
                    Some(e) => format!("{}m", (e - start).max(0) / 60),
                    None => "open".into(),
                };
                let current = id == app.m.session;
                let label = format!("{id}  {dur}{}", if current { "  ●" } else { "" });
                let sel = st.selected.as_deref() == Some(id.as_str());
                let r = ui.selectable_label(sel, RichText::new(label).monospace().color(if current { t.green } else { t.fg }));
                if r.clicked() {
                    st.selected = Some(id);
                    st.last_query = None;
                }
            }
        });
    });

    egui::CentralPanel::default().show(ui, |ui| match st.selected.clone() {
        None => queue(app, ui, &t, &mut st),
        Some(id) => session(app, ui, &t, &mut st, &id),
    });
}

/// Periodic + event-driven queries.
fn refresh(app: &mut App, st: &mut State) {
    if !app.m.connected {
        return;
    }
    let newest = app.m.events.back().map(|e| e.id).unwrap_or(0);
    let changed = newest != st.last_event
        && app.m.events.iter().rev().take_while(|e| e.id != st.last_event).any(|e| {
            matches!(e.ty.as_str(), "clips.done" | "clips.failed" | "clips.updated" | "session.marker" | "session.closed")
                || (e.ty == "clips.progress" && e.payload.get_path("stage").and_then(Value::as_str) == Some("render"))
        });
    st.last_event = newest;
    if st.last_sessions.is_none_or(|t| t.elapsed().as_secs() >= 5) || changed {
        st.last_sessions = Some(Instant::now());
        app.m.query("sessions", Value::Null);
    }
    let due = st.last_query.is_none_or(|t| t.elapsed().as_secs_f32() >= 3.0);
    if due || changed {
        st.last_query = Some(Instant::now());
        match &st.selected {
            Some(id) => app.m.query("clips.session", Value::map().with("session", id.clone())),
            None => app.m.query("clips", Value::map().with("status", "ready")),
        }
    }
}

fn job_status(app: &App, ui: &mut egui::Ui, t: &Theme) {
    let state = app.m.str("clips.job.state");
    let queued = app.m.f("clips.job.queue") as i64;
    match state {
        "running" => {
            let p = app.m.f("clips.job.progress") as f32;
            widgets::pill(ui, t, icon::PERF, &format!("clip job {} · {}", app.m.str("clips.job.session"), app.m.str("clips.job.stage")), LedState::Active);
            ui.add(egui::ProgressBar::new(p).desired_height(6.0).fill(t.accent));
        }
        "failed" => {
            widgets::pill(ui, t, icon::WARN, &format!("clip job failed ({})", app.m.str("clips.job.session")), LedState::Error);
        }
        _ => {}
    }
    if queued > 0 {
        ui.label(RichText::new(format!("{queued} job(s) queued")).small().color(t.fg_dim));
    }
}

fn queue(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State) {
    widgets::section(ui, t, icon::CHECK, "Clips waiting for review");
    let clips: Vec<Value> = match app.m.q("clips") {
        Some(Value::List(l)) => l.iter().filter(|c| s(c, "status") == "ready").cloned().collect(),
        _ => Vec::new(),
    };
    if clips.is_empty() {
        ui.label(RichText::new("Nothing to review. Clips appear here when a session's clip job finishes.").color(t.fg_dim));
        return;
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for c in &clips {
            clip_card(app, ui, t, st, c, true);
            ui.add_space(6.0);
        }
    });
}

fn session(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, id: &str) {
    let data = app.m.q("clips.session").filter(|d| s(d, "session") == id).cloned();
    let meta = app.m.q_list("sessions").iter().find(|x| s(x, "id") == id).cloned().unwrap_or_default();
    ui.horizontal(|ui| {
        ui.label(RichText::new(id).heading().monospace());
        if id == app.m.session {
            widgets::pill(ui, t, icon::LIVE, "current session", LedState::Healthy);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let running = app.m.str("clips.job.state") == "running" && app.m.str("clips.job.session") == id;
            let label = if data.as_ref().is_some_and(|d| !d.get_path("clips").and_then(Value::as_list).unwrap_or(&[]).is_empty()) {
                "re-run clip job"
            } else {
                "make clips"
            };
            if ui.add_enabled(!running, egui::Button::new(RichText::new(format!("{} {label}", icon::PLAY)).strong())).clicked() {
                app.m.command(Op::Action { name: "clips.process".into(), args: Value::map().with("session", id.to_string()) });
            }
        });
    });
    let Some(data) = data else {
        ui.label(RichText::new("loading…").color(t.fg_dim));
        return;
    };
    // job line
    let job = data.get_path("job").cloned().unwrap_or_default();
    if !job.is_null() {
        ui.horizontal(|ui| {
            let (txt, led) = match s(&job, "state") {
                "done" => (format!("clip job done: {} clip(s)", i(&job, "clips")), LedState::Healthy),
                "failed" => (format!("clip job failed: {}", s(&job, "error")), LedState::Error),
                "running" => (format!("clip job running ({})", s(&job, "stage")), LedState::Active),
                other => (format!("clip job {other}"), LedState::Armed),
            };
            widgets::pill(ui, t, icon::PERF, &txt, led);
            if let Some(Value::Map(tm)) = job.get_path("timings")
                && let Some(total) = tm.get("total_s").and_then(Value::as_f64)
            {
                ui.label(RichText::new(format!("took {total:.1} s")).small().color(t.fg_dim));
            }
        });
    }
    let recs = data.get_path("recordings").and_then(Value::as_list).unwrap_or(&[]).to_vec();
    ui.horizontal_wrapped(|ui| {
        if recs.is_empty() {
            widgets::pill(ui, t, icon::WARN, "no OBS recordings in this session", LedState::Armed);
        }
        for r in &recs {
            let ok = r.get_path("exists").is_some_and(Value::truthy);
            let name = std::path::Path::new(s(r, "path")).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            widgets::pill(
                ui,
                t,
                icon::REC,
                &format!("{} {name} ({} tracks)", s(r, "canvas"), i(r, "tracks")),
                if ok { LedState::Healthy } else { LedState::Error },
            )
            .on_hover_text(s(r, "path"));
        }
    });
    ui.add_space(4.0);
    widgets::section(ui, t, icon::TIMELINE, "Markers");
    let markers = data.get_path("markers").and_then(Value::as_list).unwrap_or(&[]).to_vec();
    let clips = data.get_path("clips").and_then(Value::as_list).unwrap_or(&[]).to_vec();
    let start_ms = i(&meta, "started_at") * 1000;
    let end_ms = meta
        .get_path("ended_at")
        .and_then(Value::as_i64)
        .map(|e| e * 1000)
        .unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(start_ms));
    timeline(ui, t, &markers, start_ms, end_ms);
    ui.add_space(6.0);
    widgets::section(ui, t, icon::SCENE, &format!("Clips ({})", clips.len()));
    if clips.is_empty() {
        ui.label(RichText::new(if markers.is_empty() { "No markers in this session." } else { "No clips yet — run the clip job." }).color(t.fg_dim));
        return;
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for c in &clips {
            clip_card(app, ui, t, st, c, false);
            ui.add_space(6.0);
        }
    });
}

/// Session timeline: hype windows (start→end, peak line) and manual markers.
fn timeline(ui: &mut egui::Ui, t: &Theme, markers: &[Value], start_ms: i64, end_ms: i64) {
    let (lo, hi) = markers.iter().fold((start_ms, end_ms), |(lo, hi), m| {
        let a = i(m, "start_wall_ms");
        let b = i(m, "end_wall_ms");
        (if a > 0 { lo.min(a) } else { lo }, hi.max(b))
    });
    let span = (hi - lo).max(1) as f32;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 58.0), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(4), t.bg_darker);
    let x = |ms: i64| rect.left() + rect.width() * ((ms - lo) as f32 / span).clamp(0.0, 1.0);
    // minute ticks
    let minutes = (span / 60_000.0).ceil() as i64;
    let step = [1, 5, 10, 15, 30, 60].into_iter().find(|s| minutes / s <= 12).unwrap_or(120);
    for k in (0..=minutes).step_by(step as usize) {
        let xx = x(lo + k * 60_000);
        p.line_segment([Pos2::new(xx, rect.bottom() - 6.0), Pos2::new(xx, rect.bottom())], Stroke::new(1.0, t.muted));
        p.text(Pos2::new(xx + 2.0, rect.bottom() - 2.0), Align2::LEFT_BOTTOM, format!("{k}m"), FontId::proportional(9.0), t.fg_dim);
    }
    let mut hover: Option<String> = None;
    let pointer = resp.hover_pos();
    for m in markers {
        let hype = s(m, "kind") == "hype";
        let color = if hype { t.accent } else { t.yellow };
        let (a, b, pk) = (x(i(m, "start_wall_ms")), x(i(m, "end_wall_ms")), x(i(m, "peak_wall_ms")));
        let band = Rect::from_min_max(Pos2::new(a, rect.top() + 8.0), Pos2::new(b.max(a + 2.0), rect.bottom() - 14.0));
        p.rect_filled(band, CornerRadius::same(2), color.gamma_multiply(0.25));
        p.line_segment([Pos2::new(pk, rect.top() + 4.0), Pos2::new(pk, rect.bottom() - 12.0)], Stroke::new(2.0, color));
        if !hype {
            let xm = x(i(m, "wall_ms"));
            p.add(egui::Shape::convex_polygon(
                vec![Pos2::new(xm - 5.0, rect.top() + 2.0), Pos2::new(xm + 5.0, rect.top() + 2.0), Pos2::new(xm, rect.top() + 10.0)],
                color,
                Stroke::NONE,
            ));
        }
        if let Some(pp) = pointer
            && band.expand2(Vec2::new(3.0, 8.0)).contains(pp)
        {
            let reasons =
                m.get_path("reasons").and_then(Value::as_list).map(|l| l.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default();
            hover = Some(format!(
                "{} · {} · score {:.2}\n{reasons}\n{} → {}",
                s(m, "label"),
                s(m, "origin"),
                f(m, "score"),
                clock((i(m, "start_wall_ms") - lo) as f64 / 1000.0),
                clock((i(m, "end_wall_ms") - lo) as f64 / 1000.0)
            ));
        }
    }
    if markers.is_empty() {
        p.text(rect.center(), Align2::CENTER_CENTER, "no markers", FontId::proportional(11.0), t.fg_dim);
    }
    if let Some(h) = hover {
        resp.on_hover_text(h);
    }
}

fn status_led(status: &str) -> LedState {
    match status {
        "ready" => LedState::Armed,
        "approved" => LedState::Healthy,
        "uploaded" => LedState::Modulated,
        "failed" => LedState::Error,
        _ => LedState::Idle,
    }
}

fn open(app: &mut App, path: &str) {
    if path.is_empty() {
        return;
    }
    if let Err(e) = std::process::Command::new("xdg-open").arg(path).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
        app.m.toast(format!("xdg-open: {e}"), true);
    }
}

fn thumb(ui: &egui::Ui, st: &mut State, path: &str, version: i64) -> Option<(egui::TextureId, Rect)> {
    if path.is_empty() {
        return None;
    }
    let key = format!("{path}#{version}");
    let tex = st.thumbs.entry(key.clone()).or_insert_with(|| {
        let bytes = std::fs::read(path).ok()?;
        let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg).ok()?.to_rgba8();
        let size = [img.width() as usize, img.height() as usize];
        let ci = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
        Some(ui.ctx().load_texture(key, ci, egui::TextureOptions::LINEAR))
    });
    tex.as_ref().map(|t| (t.id(), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))))
}

fn clip_card(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, c: &Value, show_session: bool) {
    let id = i(c, "id");
    let status = s(c, "status").to_string();
    let dur = f(c, "duration");
    let title = {
        let base = format!("#{} · {} · score {:.2}", i(c, "rank"), clock(dur), f(c, "score"));
        let named = s(c, "title");
        let base = if named.is_empty() { base } else { format!("{base} · {named}") };
        if show_session { format!("{} · {base}", s(c, "session")) } else { base }
    };
    let action = |name: &str, args: Value| Op::Action { name: name.into(), args };
    widgets::card(ui, t, icon::SCENE, &title, status_led(&status), |ui| {
        ui.horizontal(|ui| {
            let version = i(c, "version");
            let wide_tex = thumb(ui, st, s(c, "wide_thumb"), version);
            if widgets::thumbnail(ui, t, Vec2::new(256.0, 144.0), wide_tex, "wide — click to play", None).on_hover_text(s(c, "wide")).clicked() {
                open(app, s(c, "wide"));
            }
            let tall_tex = thumb(ui, st, s(c, "tall_thumb"), version);
            if widgets::thumbnail(ui, t, Vec2::new(81.0, 144.0), tall_tex, "tall", None).on_hover_text(s(c, "tall")).clicked() {
                open(app, s(c, "tall"));
            }
            ui.vertical(|ui| {
                ui.horizontal_wrapped(|ui| {
                    widgets::pill(ui, t, icon::CHECK, &status, status_led(&status));
                    for r in c.get_path("reasons").and_then(Value::as_list).unwrap_or(&[]) {
                        if let Some(r) = r.as_str() {
                            widgets::pill(ui, t, icon::ALERT, r, LedState::Modulated);
                        }
                    }
                    for l in c.get_path("labels").and_then(Value::as_list).unwrap_or(&[]) {
                        if let Some(l) = l.as_str() {
                            widgets::pill(ui, t, icon::TIMELINE, l, LedState::Armed);
                        }
                    }
                    let music = c.get_path("music_dropped").is_some_and(Value::truthy);
                    let (txt, led) = if music { ("music dropped", LedState::Healthy) } else { ("music may be included", LedState::Armed) };
                    widgets::pill(ui, t, icon::MIX, txt, led).on_hover_text(s(c, "audio"));
                    ui.label(RichText::new(s(c, "encoder")).small().color(t.fg_dim));
                });
                let captions = s(c, "captions");
                let shown: String = captions.chars().take(280).collect();
                ui.add(
                    egui::Label::new(RichText::new(if captions.is_empty() { "(no speech)".into() } else { format!("“{shown}”") }).italics().color(t.fg)).wrap(),
                );
                if status == "failed" {
                    ui.label(RichText::new(s(c, "error")).small().color(t.bright_red));
                }
                let url = s(c, "url");
                if !url.is_empty() && ui.link(RichText::new(url).small()).clicked() {
                    open(app, url);
                }
                // retrim (recording time)
                let (mut a, mut b) = *st.trims.entry(id).or_insert((f(c, "in"), f(c, "out")));
                ui.horizontal(|ui| {
                    ui.label(RichText::new("in").small().color(t.fg_dim));
                    let (a_max, b_min) = (b - 1.0, a + 1.0);
                    ui.add(egui::DragValue::new(&mut a).speed(0.05).range(0.0..=a_max).custom_formatter(|v, _| clock(v)));
                    ui.label(RichText::new("out").small().color(t.fg_dim));
                    ui.add(egui::DragValue::new(&mut b).speed(0.05).range(b_min..=f64::MAX).custom_formatter(|v, _| clock(v)));
                    let peak = f(c, "peak");
                    ui.label(RichText::new(format!("{} long · peak at {}", clock(b - a), clock(peak - a))).small().color(t.fg_dim));
                    let edited = (a - f(c, "in")).abs() > 0.01 || (b - f(c, "out")).abs() > 0.01;
                    if ui.add_enabled(edited, egui::Button::new("retrim")).on_hover_text("re-cut wide + tall with these in/out points").clicked() {
                        app.m.command(action("clips.retrim", Value::map().with("id", id).with("in", a).with("out", b)));
                        st.trims.remove(&id);
                        return;
                    }
                    if edited && ui.small_button("reset").clicked() {
                        (a, b) = (f(c, "in"), f(c, "out"));
                    }
                    st.trims.insert(id, (a, b));
                });
                ui.horizontal(|ui| {
                    if ui.button(format!("{} wide", icon::PLAY)).clicked() {
                        open(app, s(c, "wide"));
                    }
                    if ui.button(format!("{} tall", icon::PLAY)).clicked() {
                        open(app, s(c, "tall"));
                    }
                    ui.separator();
                    let approve = egui::Button::new(RichText::new(format!("{} approve", icon::CHECK)).color(t.green));
                    if ui.add_enabled(status != "approved" && status != "failed", approve).clicked() {
                        app.m.command(action("clips.approve", Value::map().with("id", id)));
                    }
                    let reject = egui::Button::new(RichText::new(format!("{} reject", icon::CROSS)).color(t.bright_red));
                    if ui.add_enabled(status != "rejected", reject).clicked() {
                        app.m.command(action("clips.reject", Value::map().with("id", id)));
                    }
                    if ui.add_enabled(status == "approved", egui::Button::new("upload")).on_hover_text("runs [clips] upload_command").clicked() {
                        app.m.command(action("clips.upload", Value::map().with("id", id)));
                    }
                });
            });
        });
    });
}
