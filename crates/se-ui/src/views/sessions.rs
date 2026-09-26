//! Recordings (§15.6, §18): clips waiting for review as video cards (play, approve, reject, trim,
//! upload), and past streams as cards (date, length, markers, clips) with each stream's markers
//! timeline, recordings and clips; clip-making progress in words. Data: `sessions`,
//! `clips.session {session}` (one reply per stream, kept side by side), `clips {status}` queries
//! and `clips.*` actions (se-clips).

use crate::app::App;
use egui::{Align, Align2, CornerRadius, Layout, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_mono, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Clip cards are at least this wide.
const CLIP_MIN: f32 = 400.0;

/// View state kept in egui memory (no fields on `App`).
#[derive(Default)]
struct State {
    /// 0 = clips to review, 1 = past streams.
    view: usize,
    /// The first view was picked (clips to review when there are any, else past streams).
    view_picked: bool,
    /// Stream open in "Past streams".
    selected: Option<String>,
    /// Decoded thumbnails by `path#version` (`None` = unreadable).
    thumbs: HashMap<String, Option<egui::TextureHandle>>,
    /// Trim edits per clip id: (in, out) in recording seconds.
    trims: HashMap<i64, (f64, f64)>,
    /// Newest event id seen (clip events trigger a refresh).
    last_event: u64,
    last_query: Option<Instant>,
    last_sessions: Option<Instant>,
    /// Streams whose summary was asked for on this connection.
    asked: HashSet<String>,
    asked_conn: u64,
}

type Shared = Arc<Mutex<State>>;

fn state(ui: &Ui) -> Shared {
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
fn list<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get_path(k).and_then(Value::as_list).unwrap_or(&[])
}

/// Query key of one stream's markers/clips/recordings.
fn key(session: &str) -> String {
    format!("clips.session:{session}")
}

fn clock(secs: f64) -> String {
    let t = secs.max(0.0);
    let m = (t / 60.0).floor();
    format!("{}:{:05.2}", m as i64, t - m * 60.0)
}

/// "0:32", "12:05".
fn short_clock(secs: f64) -> String {
    let t = secs.max(0.0).round() as i64;
    format!("{}:{:02}", t / 60, t % 60)
}

/// "45 min", "1 h 32 min".
fn length_words(secs: i64) -> String {
    let m = (secs.max(0) + 30) / 60;
    if m < 60 { format!("{m} min") } else { format!("{} h {} min", m / 60, m % 60) }
}

fn now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Local calendar time of a unix timestamp.
fn local(ts: i64) -> Option<libc::tm> {
    let t = ts as libc::time_t;
    // SAFETY: `localtime_r` only writes the `tm` we own; a zeroed `tm` is a valid value.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        (!libc::localtime_r(&t, &mut tm).is_null()).then_some(tm)
    }
}

/// "Today, 8:04 pm", "Yesterday, 9:10 pm", "Thu 25 Sep, 8:04 pm".
fn when(ts: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (Some(tm), Some(today)) = (local(ts), local(now_s())) else { return String::new() };
    let h12 = match tm.tm_hour % 12 {
        0 => 12,
        h => h,
    };
    let time = format!("{h12}:{:02} {}", tm.tm_min, if tm.tm_hour < 12 { "am" } else { "pm" });
    let same_year = tm.tm_year == today.tm_year;
    let day = if same_year && tm.tm_yday == today.tm_yday {
        "Today".to_string()
    } else if same_year && tm.tm_yday + 1 == today.tm_yday {
        "Yesterday".to_string()
    } else {
        let d = format!("{} {} {}", DAYS[tm.tm_wday.clamp(0, 6) as usize], tm.tm_mday, MONTHS[tm.tm_mon.clamp(0, 11) as usize]);
        if same_year { d } else { format!("{d} {}", tm.tm_year + 1900) }
    };
    format!("{day}, {time}")
}

pub fn ui(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    let st = state(ui);
    let mut st = st.lock().unwrap_or_else(|p| p.into_inner());
    refresh(app, &mut st);

    let pending = app.m.f("clips.pending").max(0.0) as i64;
    if !st.view_picked && app.m.has("clips.pending") {
        st.view_picked = true;
        st.view = usize::from(pending == 0);
    }
    ui.horizontal(|ui| {
        let review = if pending > 0 { format!("Clips to review ({pending})") } else { "Clips to review".to_string() };
        if widgets::segmented(ui, &t, &mut st.view, &[review.as_str(), "Past streams"]) {
            st.last_query = None;
        }
    });
    ui.add_space(spacing::M);
    job_banner(app, ui, &t);
    match st.view {
        0 => queue(app, ui, &t, &mut st),
        _ => streams(app, ui, &t, &mut st),
    }
}

/// Periodic + event-driven queries.
fn refresh(app: &mut App, st: &mut State) {
    if !app.m.connected {
        return;
    }
    if st.asked_conn != app.m.conn_gen {
        st.asked_conn = app.m.conn_gen;
        st.asked.clear();
    }
    let newest = app.m.events.back().map(|e| e.id).unwrap_or(0);
    let changed = newest != st.last_event
        && app.m.events.iter().rev().take_while(|e| e.id != st.last_event).any(|e| {
            matches!(e.ty.as_str(), "clips.done" | "clips.failed" | "clips.updated" | "session.marker" | "session.closed")
                || (e.ty == "clips.progress" && e.payload.get_path("stage").and_then(Value::as_str) == Some("render"))
        });
    st.last_event = newest;
    if changed {
        st.asked.clear();
    }
    if st.last_sessions.is_none_or(|t| t.elapsed().as_secs() >= 5) || changed {
        st.last_sessions = Some(Instant::now());
        app.m.query("sessions", Value::Null);
    }
    // one summary per stream (markers, clips, recordings); the open one and the live one refresh
    let ids: Vec<String> = app.m.q_list("sessions").iter().map(|x| s(x, "id").to_string()).collect();
    for id in ids {
        if st.asked.insert(id.clone()) {
            app.m.query_as(&key(&id), "clips.session", Value::map().with("session", id));
        }
    }
    let due = st.last_query.is_none_or(|t| t.elapsed().as_secs_f32() >= 3.0);
    if due || changed {
        st.last_query = Some(Instant::now());
        app.m.query("clips", Value::map().with("status", "ready"));
        for id in [st.selected.clone(), Some(app.m.session.clone())].into_iter().flatten().filter(|id| !id.is_empty()) {
            app.m.query_as(&key(&id), "clips.session", Value::map().with("session", id.clone()));
        }
    }
}

/// Clip-making stage in words.
fn stage_words(stage: &str) -> String {
    match stage {
        "model" => "getting the speech model ready".into(),
        "transcribe" => "listening to what was said".into(),
        "rank" => "picking the best moments".into(),
        "render" => "cutting the clips".into(),
        "" => "starting".into(),
        other => crate::views::live::nice(other).to_lowercase(),
    }
}

/// What the clip maker is doing right now, in words.
fn job_banner(app: &mut App, ui: &mut Ui, t: &Theme) {
    let state = app.m.str("clips.job.state").to_string();
    let queued = app.m.f("clips.job.queue") as i64;
    let session = app.m.str("clips.job.session").to_string();
    let started = app.m.q_list("sessions").iter().find(|x| s(x, "id") == session).map(|x| i(x, "started_at"));
    let stream = started.map(when).filter(|w| !w.is_empty()).unwrap_or_else(|| "a stream".into());
    let waiting = match queued {
        0 => String::new(),
        1 => "1 more stream is waiting.".to_string(),
        n => format!("{n} more streams are waiting."),
    };
    match state.as_str() {
        "running" => {
            let head = format!("Making clips from {stream}: {}…", stage_words(app.m.str("clips.job.stage")));
            widgets::callout(ui, t, widgets::Tone::Info, icon::FILM, &head, &waiting, None);
            ui.add_space(spacing::XS);
            let p = app.m.f("clips.job.progress").clamp(0.0, 1.0) as f32;
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
            ui.painter().rect_filled(rect, CornerRadius::same(radius::PILL), t.inset);
            ui.painter().rect_filled(Rect::from_min_size(rect.min, Vec2::new(rect.width() * p, rect.height())), CornerRadius::same(radius::PILL), t.accent);
        }
        "failed" => {
            let head = format!("Making clips from {stream} didn't work");
            let body = if waiting.is_empty() { "Try again. If it keeps failing, look in Settings → Troubleshooting.".to_string() } else { waiting };
            if widgets::callout(ui, t, widgets::Tone::Danger, icon::WARN, &head, &body, (!session.is_empty()).then_some("Try again")) {
                app.m.command(Op::Action { name: "clips.process".into(), args: Value::map().with("session", session.clone()) });
            }
        }
        _ if queued > 0 => {
            widgets::callout(ui, t, widgets::Tone::Info, icon::FILM, "Clips are on the way", &waiting, None);
        }
        _ => return,
    }
    ui.add_space(spacing::M);
}

/// Equal-width card columns of at least `min` points.
fn grid(ui: &Ui, min: f32, gap: f32) -> (usize, f32) {
    let w = ui.available_width();
    let per = (((w + gap) / (min + gap)).floor() as usize).clamp(1, 6);
    (per, ((w - gap * (per as f32 - 1.0)) / per as f32).floor())
}

fn clip_grid(app: &mut App, ui: &mut Ui, t: &Theme, st: &mut State, clips: &[Value], show_stream: bool) {
    let gap = spacing::L;
    let (per, w) = grid(ui, CLIP_MIN, gap);
    for row in clips.chunks(per) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for c in row {
                ui.allocate_ui_with_layout(Vec2::new(w, 10.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(w);
                    clip_card(app, ui, t, st, c, show_stream);
                });
            }
        });
        ui.add_space(gap);
    }
}

fn queue(app: &mut App, ui: &mut Ui, t: &Theme, st: &mut State) {
    let clips: Vec<Value> = match app.m.q("clips") {
        Some(Value::List(l)) => l.iter().filter(|c| s(c, "status") == "ready").cloned().collect(),
        _ => Vec::new(),
    };
    if clips.is_empty() {
        widgets::panel(ui, t, |ui| {
            ui.set_width(ui.available_width());
            if widgets::empty_state(
                ui,
                t,
                icon::FILM,
                "Nothing to review",
                "After a stream, Stream Engine picks the best moments and cuts them into clips. They wait for you here.",
                Some("See past streams"),
            ) {
                st.view = 1;
            }
        });
        return;
    }
    widgets::hint(ui, t, "Keep the good ones, skip the rest. Approved clips can be uploaded.");
    ui.add_space(spacing::S);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        clip_grid(app, ui, t, st, &clips, true);
    });
}

// ---- past streams --------------------------------------------------------------------------------

fn streams(app: &mut App, ui: &mut Ui, t: &Theme, st: &mut State) {
    let sessions = app.m.q_list("sessions").to_vec();
    if sessions.is_empty() {
        widgets::panel(ui, t, |ui| {
            ui.set_width(ui.available_width());
            let body = if app.m.connected { "Your streams show up here after you go live." } else { "They show up here once Stream Engine is running." };
            widgets::empty_state(ui, t, icon::FILM, "No streams yet", body, None);
        });
        return;
    }
    if st.selected.as_ref().is_none_or(|sel| !sessions.iter().any(|x| s(x, "id") == sel)) {
        st.selected = Some(s(&sessions[0], "id").to_string());
        st.last_query = None;
    }
    let list_w = (ui.available_width() * 0.28).clamp(300.0, 420.0);
    let h = ui.available_height();
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(Vec2::new(list_w, h), Layout::top_down(Align::Min), |ui| {
            ui.set_width(list_w);
            egui::ScrollArea::vertical().id_salt("streams-list").auto_shrink([false, false]).show(ui, |ui| {
                for sess in &sessions {
                    if stream_card(app, ui, t, st, sess) {
                        st.selected = Some(s(sess, "id").to_string());
                        st.last_query = None;
                    }
                    ui.add_space(spacing::S);
                }
            });
        });
        ui.add_space(spacing::L);
        ui.vertical(|ui| {
            egui::ScrollArea::vertical().id_salt("stream-detail").auto_shrink([false, false]).show(ui, |ui| {
                if let Some(id) = st.selected.clone() {
                    stream(app, ui, t, st, &id);
                }
            });
        });
    });
}

/// Start, end (now while it runs) and whether it's still open.
fn span(sess: &Value) -> (i64, i64, bool) {
    let start = i(sess, "started_at");
    match sess.get_path("ended_at").and_then(Value::as_i64) {
        Some(e) => (start, e, false),
        None => (start, now_s(), true),
    }
}

/// A past-stream card; true when clicked.
fn stream_card(app: &App, ui: &mut Ui, t: &Theme, st: &State, sess: &Value) -> bool {
    let id = s(sess, "id");
    let (start, end, open) = span(sess);
    let live = open && id == app.m.session && app.m.f("show.live_since") > 0.0;
    let data = app.m.q(&key(id));
    let mut facts = vec![length_words(end - start)];
    if let Some(d) = data {
        let (m, c) = (list(d, "markers").len(), list(d, "clips").len());
        facts.push(if m == 1 { "1 marker".into() } else { format!("{m} markers") });
        facts.push(if c == 1 { "1 clip".into() } else { format!("{c} clips") });
    }
    let selected = st.selected.as_deref() == Some(id);
    let fill = if selected { mix(t.surface, t.accent, 0.14) } else { t.surface };
    let stroke = if selected { Stroke::new(1.5, t.accent) } else { Stroke::new(1.0, t.border) };
    let r = egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(radius::CARD)
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(when(start)).font(font_semibold(type_scale::BODY + 0.5)).color(t.fg));
                if live {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        widgets::badge(ui, t, "On air", t.bright_red);
                    });
                }
            });
            ui.label(RichText::new(facts.join(" · ")).size(type_scale::SMALL + 0.5).color(t.text_dim));
        })
        .response;
    let r = ui.interact(r.rect, ui.id().with(("stream", id)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
    r.clicked()
}

fn stream(app: &mut App, ui: &mut Ui, t: &Theme, st: &mut State, id: &str) {
    let data = app.m.q(&key(id)).cloned();
    let meta = app.m.q_list("sessions").iter().find(|x| s(x, "id") == id).cloned().unwrap_or_default();
    let (start, end, open) = span(&meta);
    let clips = data.as_ref().map(|d| list(d, "clips").to_vec()).unwrap_or_default();
    let markers = data.as_ref().map(|d| list(d, "markers").to_vec()).unwrap_or_default();
    let recorded = data.as_ref().is_some_and(|d| !list(d, "recordings").is_empty());
    widgets::titled(
        ui,
        t,
        &when(start),
        &format!("{}{}", length_words(end - start), if open && id == app.m.session { " so far" } else { "" }),
        |ui| {
            let running = app.m.str("clips.job.state") == "running" && app.m.str("clips.job.session") == id;
            let label = if clips.is_empty() { "Make clips" } else { "Make clips again" };
            let kind = if clips.is_empty() { Kind::Primary } else { Kind::Secondary };
            if widgets::button_ex(ui, t, Some(icon::FILM), label, kind, Size::Medium, 0.0, !running && recorded)
                .on_hover_text(if recorded { "Find the best moments and cut them into clips" } else { "There's no recording of this stream to cut clips from" })
                .clicked()
            {
                app.m.command(Op::Action { name: "clips.process".into(), args: Value::map().with("session", id.to_string()) });
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            let Some(data) = &data else {
                widgets::hint(ui, t, "Loading…");
                return;
            };
            // what the clip maker last did for this stream
            let job = data.get_path("job").cloned().unwrap_or_default();
            if !job.is_null() {
                let (c, txt) = match s(&job, "state") {
                    "done" => (t.green, if i(&job, "clips") == 1 { "Made 1 clip.".to_string() } else { format!("Made {} clips.", i(&job, "clips")) }),
                    "failed" => (t.bright_red, "Making clips didn't work. Try again.".to_string()),
                    "running" => (t.accent, format!("Making clips: {}…", stage_words(s(&job, "stage")))),
                    _ => (t.yellow, "Waiting to make clips.".to_string()),
                };
                ui.horizontal(|ui| {
                    widgets::badge(ui, t, &txt, c);
                });
            }
            // recordings
            let recs = list(data, "recordings");
            ui.add_space(spacing::S);
            if recs.is_empty() {
                widgets::callout(
                    ui,
                    t,
                    widgets::Tone::Warn,
                    icon::WARN,
                    "OBS didn't record this stream",
                    "Clips are cut from OBS recordings. Before your next stream, press Start Recording in OBS (or turn on automatic recording in OBS's settings under Output → Recording).",
                    None,
                );
            } else {
                let missing = recs.iter().filter(|r| !r.get_path("exists").is_some_and(Value::truthy)).count();
                let names: Vec<String> = recs.iter().map(|r| if s(r, "canvas") == "tall" { "vertical".to_string() } else { "wide".to_string() }).collect();
                let line = if missing == 0 {
                    format!("Recorded in {}.", names.join(" and "))
                } else {
                    format!("{missing} of the recordings can't be found anymore (moved or deleted).")
                };
                ui.label(RichText::new(line).color(if missing == 0 { t.text_dim } else { t.yellow }));
            }
            widgets::details(ui, t, ("stream-details", id), "Details", |ui| {
                widgets::fact(ui, t, "Stream", id);
                widgets::fact(ui, t, "Folder", s(data, "dir"));
                for r in recs {
                    let ok = r.get_path("exists").is_some_and(Value::truthy);
                    ui.label(
                        RichText::new(format!("{} {} ({} tracks){}", s(r, "canvas"), s(r, "path"), i(r, "tracks"), if ok { "" } else { " — missing" }))
                            .font(font_mono(type_scale::SMALL))
                            .color(if ok { t.text_dim } else { t.yellow }),
                    );
                }
                if let Some(Value::Map(tm)) = job.get_path("timings")
                    && let Some(total) = tm.get("total_s").and_then(Value::as_f64)
                {
                    widgets::fact(ui, t, "Clip making took", &format!("{total:.1} s"));
                }
                if !s(&job, "error").is_empty() {
                    ui.label(RichText::new(s(&job, "error")).size(type_scale::SMALL).color(t.bright_red));
                }
            });
            ui.add_space(spacing::M);
            widgets::section(ui, t, icon::TIMELINE, "Markers");
            timeline(ui, t, &markers, start * 1000, end * 1000);
            ui.horizontal(|ui| {
                legend(ui, t, t.accent, "Hype moment (found for you)");
                ui.add_space(spacing::M);
                legend(ui, t, t.cyan, "Marker you placed");
            });
        },
    );
    ui.add_space(spacing::L);
    let count = if clips.len() == 1 { "1 clip".to_string() } else { format!("{} clips", clips.len()) };
    widgets::titled(
        ui,
        t,
        "Clips",
        &count,
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if clips.is_empty() {
                let body = if !recorded {
                    "There's no recording of this stream, so there's nothing to cut."
                } else if markers.is_empty() {
                    "No markers in this stream, so there were no moments to cut."
                } else {
                    "Press Make clips to cut the best moments."
                };
                widgets::empty_state(ui, t, icon::FILM, "No clips yet", body, None);
            } else {
                clip_grid(app, ui, t, st, &clips, false);
            }
        },
    );
}

fn legend(ui: &mut Ui, t: &Theme, c: egui::Color32, text: &str) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 10.0), Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::same(2), c);
    ui.label(RichText::new(text).size(type_scale::SMALL).color(t.text_dim));
}

/// Stream timeline: hype windows (start→end, peak line) and placed markers.
fn timeline(ui: &mut Ui, t: &Theme, markers: &[Value], start_ms: i64, end_ms: i64) {
    let (lo, hi) = markers.iter().fold((start_ms, end_ms), |(lo, hi), m| {
        let a = i(m, "start_wall_ms");
        let b = i(m, "end_wall_ms");
        (if a > 0 { lo.min(a) } else { lo }, hi.max(b))
    });
    let span = (hi - lo).max(1) as f32;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 64.0), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(radius::CONTROL), t.inset);
    let x = |ms: i64| rect.left() + rect.width() * ((ms - lo) as f32 / span).clamp(0.0, 1.0);
    // minute ticks
    let minutes = (span / 60_000.0).ceil() as i64;
    let step = [1, 5, 10, 15, 30, 60].into_iter().find(|s| minutes / s <= 12).unwrap_or(120);
    for k in (0..=minutes).step_by(step as usize) {
        let xx = x(lo + k * 60_000);
        p.line_segment([Pos2::new(xx, rect.bottom() - 6.0), Pos2::new(xx, rect.bottom())], Stroke::new(1.0, t.border));
        let label = format!("{}:{:02}", k / 60, k % 60);
        p.text(Pos2::new(xx + 3.0, rect.bottom() - 2.0), Align2::LEFT_BOTTOM, label, font_mono(type_scale::SMALL), t.text_dim);
    }
    let mut hover: Option<String> = None;
    let pointer = resp.hover_pos();
    for m in markers {
        let hype = s(m, "kind") == "hype";
        let color = if hype { t.accent } else { t.cyan };
        let (a, b, pk) = (x(i(m, "start_wall_ms")), x(i(m, "end_wall_ms")), x(i(m, "peak_wall_ms")));
        let band = Rect::from_min_max(Pos2::new(a, rect.top() + 10.0), Pos2::new(b.max(a + 2.0), rect.bottom() - 16.0));
        p.rect_filled(band, CornerRadius::same(2), color.gamma_multiply(0.25));
        p.line_segment([Pos2::new(pk, rect.top() + 6.0), Pos2::new(pk, rect.bottom() - 14.0)], Stroke::new(2.0, color));
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
            let reasons = list(m, "reasons").iter().filter_map(Value::as_str).map(crate::views::live::nice).collect::<Vec<_>>().join(", ");
            hover = Some(format!(
                "{}\n{reasons}\n{} → {} · score {:.2} ({})",
                s(m, "label"),
                clock((i(m, "start_wall_ms") - lo) as f64 / 1000.0),
                clock((i(m, "end_wall_ms") - lo) as f64 / 1000.0),
                f(m, "score"),
                s(m, "origin"),
            ));
        }
    }
    if markers.is_empty() {
        p.text(rect.center(), Align2::CENTER_CENTER, "No markers", font(type_scale::SMALL), t.text_dim);
    }
    if let Some(h) = hover {
        resp.on_hover_text(h);
    }
}

// ---- clip cards ------------------------------------------------------------------------------------

/// (badge text, color) for a clip status.
fn status_words(t: &Theme, status: &str) -> (&'static str, egui::Color32) {
    match status {
        "ready" => ("To review", t.accent),
        "approved" => ("Approved", t.green),
        "rejected" => ("Skipped", t.text_dim),
        "uploaded" => ("Uploaded", t.modulated()),
        "failed" => ("Didn't work", t.bright_red),
        _ => ("Waiting", t.text_dim),
    }
}

fn open(app: &mut App, path: &str) {
    if path.is_empty() {
        return;
    }
    if let Err(e) = std::process::Command::new("xdg-open").arg(path).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
        app.m.toast(format!("Couldn't open it: {e}"), true);
    }
}

fn thumb(ui: &Ui, st: &mut State, path: &str, version: i64) -> Option<(egui::TextureId, Rect)> {
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

fn clip_card(app: &mut App, ui: &mut Ui, t: &Theme, st: &mut State, c: &Value, show_stream: bool) {
    let id = i(c, "id");
    let status = s(c, "status").to_string();
    let action = |name: &str, args: Value| Op::Action { name: name.into(), args };
    widgets::panel(ui, t, |ui| {
        ui.set_width(ui.available_width());
        // pictures: wide with the vertical cut beside it; click to play
        let version = i(c, "version");
        let w = ui.available_width();
        let tall_w = (w * 0.2).round();
        let wide_w = w - tall_w - spacing::S;
        let hgt = (wide_w * 9.0 / 16.0).round();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = spacing::S;
            let wide_tex = thumb(ui, st, s(c, "wide_thumb"), version);
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(wide_w, hgt), Sense::click());
            widgets::video_frame(ui, t, rect, wide_tex, "", None, Some((&short_clock(f(c, "duration")), egui::Color32::from_black_alpha(190))));
            if resp.hovered() {
                ui.painter().circle_filled(rect.center(), 26.0, egui::Color32::from_black_alpha(150));
                ui.painter().text(rect.center() + Vec2::new(2.0, 0.0), Align2::CENTER_CENTER, icon::PLAY, font(20.0), egui::Color32::WHITE);
            }
            if resp.on_hover_text("Play the wide clip").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                open(app, s(c, "wide"));
            }
            let tall_tex = thumb(ui, st, s(c, "tall_thumb"), version);
            if widgets::thumbnail(ui, t, Vec2::new(tall_w, hgt), tall_tex, "", None).on_hover_text("Play the vertical clip").clicked() {
                open(app, s(c, "tall"));
            }
        });
        ui.add_space(spacing::S);
        // title and status
        let named = s(c, "title");
        let title = if named.is_empty() { format!("Moment {}", i(c, "rank")) } else { named.to_string() };
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).font(font_semibold(type_scale::LARGE)).color(t.fg));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (txt, col) = status_words(t, &status);
                widgets::badge(ui, t, txt, col);
            });
        });
        let mut sub = Vec::new();
        if show_stream && let Some(start) = app.m.q_list("sessions").iter().find(|x| s(x, "id") == s(c, "session")).map(|x| i(x, "started_at")) {
            sub.push(format!("From {}", when(start)));
        }
        sub.push(format!("{} long", short_clock(f(c, "duration"))));
        widgets::hint(ui, t, &sub.join(" · "));
        // why it was picked
        ui.add_space(spacing::XS);
        ui.horizontal_wrapped(|ui| {
            for r in list(c, "reasons").iter().filter_map(Value::as_str) {
                widgets::badge(ui, t, &crate::views::live::nice(r), t.modulated());
            }
            for l in list(c, "labels").iter().filter_map(Value::as_str) {
                widgets::badge(ui, t, &crate::views::live::nice(l), t.yellow);
            }
            let music = c.get_path("music_dropped").is_some_and(Value::truthy);
            let (txt, col) = if music { ("Music removed", t.green) } else { ("May include music", t.yellow) };
            widgets::badge(ui, t, txt, col).on_hover_text(s(c, "audio"));
        });
        // what was said
        let captions = s(c, "captions");
        ui.add_space(spacing::XS);
        let shown: String = captions.chars().take(220).collect();
        let more = if captions.chars().count() > 220 { "…" } else { "" };
        ui.add(
            egui::Label::new(
                RichText::new(if captions.is_empty() { "No talking in this one.".into() } else { format!("“{shown}{more}”") })
                    .italics()
                    .color(if captions.is_empty() { t.text_faint } else { t.fg }),
            )
            .wrap(),
        );
        if status == "failed" {
            ui.label(RichText::new(format!("{} {}", icon::WARN, s(c, "error"))).size(type_scale::SMALL).color(t.bright_red));
        }
        let url = s(c, "url");
        if !url.is_empty() && ui.link(RichText::new("Open the uploaded clip").size(type_scale::SMALL + 0.5)).on_hover_text(url).clicked() {
            open(app, url);
        }
        ui.add_space(spacing::S);
        // decide
        ui.horizontal(|ui| {
            if widgets::button_ex(
                ui,
                t,
                Some(icon::CHECK),
                "Keep",
                Kind::Primary,
                Size::Medium,
                0.0,
                status != "approved" && status != "failed" && status != "uploaded",
            )
            .on_hover_text("Approve this clip")
            .clicked()
            {
                app.m.command(action("clips.approve", Value::map().with("id", id)));
            }
            if widgets::button_ex(ui, t, Some(icon::CROSS), "Skip", Kind::Secondary, Size::Medium, 0.0, status != "rejected")
                .on_hover_text("Reject this clip")
                .clicked()
            {
                app.m.command(action("clips.reject", Value::map().with("id", id)));
            }
            if status == "approved"
                && widgets::button_ex(ui, t, Some(icon::UP), "Upload", Kind::Secondary, Size::Medium, 0.0, true)
                    .on_hover_text("Send it with your upload command")
                    .clicked()
            {
                app.m.command(action("clips.upload", Value::map().with("id", id)));
            }
        });
        ui.add_space(spacing::XS);
        // trim (recording time)
        widgets::details(ui, t, ("trim", id), "Trim", |ui| {
            let (mut a, mut b) = *st.trims.entry(id).or_insert((f(c, "in"), f(c, "out")));
            ui.horizontal(|ui| {
                ui.label(RichText::new("Starts at").color(t.text_dim));
                let (a_max, b_min) = (b - 1.0, a + 1.0);
                ui.add(egui::DragValue::new(&mut a).speed(0.05).range(0.0..=a_max).custom_formatter(|v, _| clock(v)));
                ui.label(RichText::new("ends at").color(t.text_dim));
                ui.add(egui::DragValue::new(&mut b).speed(0.05).range(b_min..=f64::MAX).custom_formatter(|v, _| clock(v)));
            });
            let peak = f(c, "peak");
            widgets::hint(ui, t, &format!("{} long · the big moment is at {}. Drag the times to change them.", short_clock(b - a), short_clock(peak - a)));
            let edited = (a - f(c, "in")).abs() > 0.01 || (b - f(c, "out")).abs() > 0.01;
            let mut recut = false;
            ui.horizontal(|ui| {
                if widgets::button_ex(ui, t, Some(icon::FILM), "Cut it again", Kind::Secondary, Size::Small, 0.0, edited)
                    .on_hover_text("Re-cut the wide and vertical clips with these times")
                    .clicked()
                {
                    recut = true;
                }
                if edited && widgets::button_ex(ui, t, None, "Undo changes", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                    (a, b) = (f(c, "in"), f(c, "out"));
                }
            });
            if recut {
                app.m.command(action("clips.retrim", Value::map().with("id", id).with("in", a).with("out", b)));
                st.trims.remove(&id);
            } else {
                st.trims.insert(id, (a, b));
            }
        });
        widgets::details(ui, t, ("clip-details", id), "Details", |ui| {
            widgets::fact(ui, t, "Rank", &format!("#{} · score {:.2}", i(c, "rank"), f(c, "score")));
            widgets::fact(ui, t, "Encoder", s(c, "encoder"));
            widgets::fact(ui, t, "Sound", s(c, "audio"));
            ui.label(RichText::new(s(c, "wide")).font(font_mono(type_scale::SMALL)).color(t.text_dim));
            ui.label(RichText::new(s(c, "tall")).font(font_mono(type_scale::SMALL)).color(t.text_dim));
        });
    });
}
