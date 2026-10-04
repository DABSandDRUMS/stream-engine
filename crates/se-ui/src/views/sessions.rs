//! Recordings library → a stream's clips → focused review.
//! Queries are throttled; the source timeline is loaded only when requested.

use crate::app::App;
use super::clip_media::ClipMedia;
use egui::{Align, Align2, CornerRadius, Layout, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_mono, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const CLIP_MIN: f32 = 224.0;

/// View state kept in egui memory (no fields on `App`).
#[derive(Default)]
struct State {
    /// 0 = library, 1 = stream, 2 = clip detail. Parent selections survive Back.
    view: usize,
    selected: Option<String>,
    selected_clip: Option<i64>,
    library_search: String,
    clip_search: String,
    clip_filter: usize,
    clip_sort: usize,
    stream_panel: usize,
    grid_orientation: usize,
    detail_orientation: usize,
    playing: bool,
    still: Option<egui::TextureHandle>,
    detail_media_key: Option<String>,
    hover: Option<(String, Instant)>,
    hover_seen: bool,
    media: ClipMedia,
    status_at: Option<Instant>,
    timeline_key: Option<String>,
    timeline_seq: u64,
    timeline: Option<Value>,
    window_requested: Option<(f64, f64)>,
    window_updated: Option<Instant>,
    /// Visible show window and a two-click clip selection, in show seconds.
    window_start: f64,
    window_len: f64,
    range: Option<(f64, f64)>,
    range_start: Option<f64>,
    cursor: Option<f64>,
    /// Trim edits per clip id: (render version, in, out) in recording seconds.
    trims: HashMap<i64, (i64, f64, f64)>,
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
    st.media.begin_frame();
    st.hover_seen = false;
    recorder_header(app, ui, &t, &mut st);
    ui.add_space(spacing::L);
    if !app.m.connected {
        widgets::callout(ui, &t, widgets::Tone::Warn, icon::WARN, "Not connected", "Reconnect to Stream Engine to refresh recordings and review clips.", None);
        ui.add_space(spacing::M);
    }
    job_banner(app, ui, &t);
    match st.view {
        0 => library(app, ui, &t, &mut st),
        1 => stream(app, ui, &t, &mut st),
        _ => clip_detail(app, ui, &t, &mut st),
    }
    if !st.hover_seen {
        st.hover = None;
    }
    st.media.end_frame();
}

/// Called by the app when navigating away so no file decoder runs in the background.
pub fn leave(ctx: &egui::Context) {
    let shared = ctx.data(|d| d.get_temp::<Shared>(egui::Id::new("se.sessions.view")));
    if let Some(shared) = shared {
        let mut st = shared.lock().unwrap_or_else(|p| p.into_inner());
        st.playing = false;
        st.hover = None;
        st.media.begin_frame();
        st.media.end_frame();
    }
}

/// Periodic + event-driven queries. A large timeline is fetched only for the selected stream.
fn refresh(app: &mut App, st: &mut State) {
    if !app.m.connected {
        return;
    }
    if st.asked_conn != app.m.conn_gen {
        st.asked_conn = app.m.conn_gen;
        st.asked.clear();
        st.timeline_key = None;
        st.timeline = None;
        st.status_at = None;
    }
    if st.status_at.is_none_or(|at| at.elapsed().as_secs() >= 3) {
        st.status_at = Some(Instant::now());
        app.m.query("recording.status", Value::Null);
    }
    let newest = app.m.events.back().map(|e| e.id).unwrap_or(0);
    let changed = newest != st.last_event
        && app.m.events.iter().rev().take_while(|e| e.id != st.last_event).any(|e| {
            matches!(e.ty.as_str(), "clips.done" | "clips.failed" | "clips.updated" | "session.marker" | "session.closed" | "recording.index.done")
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
    let ids: Vec<String> = app.m.q_list("sessions").iter().map(|x| s(x, "id").to_string()).collect();
    for id in ids {
        if st.asked.insert(id.clone()) {
            app.m.query_as(&key(&id), "clips.session", Value::map().with("session", id));
        }
    }
    let due = st.last_query.is_none_or(|t| t.elapsed().as_secs_f32() >= 3.0);
    if due || changed {
        st.last_query = Some(Instant::now());
        for id in [st.selected.clone(), Some(app.m.session.clone())].into_iter().flatten().filter(|id| !id.is_empty()) {
            app.m.query_as(&key(&id), "clips.session", Value::map().with("session", id.clone()));
        }
    }
    if st.view == 1 && st.stream_panel == 1
        && let Some(id) = &st.selected
    {
        let window = (st.window_start.round(), st.window_len.max(1.0).round());
        let new_show = st.timeline_key.as_deref() != Some(id);
        let moved = st.window_requested != Some(window);
        let settled = st.window_updated.is_none_or(|at| at.elapsed().as_millis() >= 300);
        if new_show || changed || (moved && settled) {
            st.timeline_key = Some(id.clone());
            if new_show {
                st.timeline = None;
            }
            st.window_requested = Some(window);
            st.timeline_seq = app.m.q_seq("clipping.timeline");
            app.m.query_as(
                "clipping.timeline",
                "recording.timeline",
                Value::map().with("session", id.clone()).with("from", window.0).with("to", window.0 + window.1).with("limit", 500i64),
            );
        }
        let seq = app.m.q_seq("clipping.timeline");
        if seq != st.timeline_seq {
            st.timeline_seq = seq;
            let reply = app.m.q("clipping.timeline").cloned();
            let same_session = reply.as_ref().and_then(|v| v.get_path("manifest.session")).and_then(Value::as_str).is_none_or(|session| session == id);
            if same_session {
                st.timeline = reply;
            } else {
                st.timeline_key = None;
            }
        }
    }
}



fn recorder_header(app: &mut App, ui: &mut Ui, t: &Theme, st: &mut State) {
    let status = app.m.q("recording.status").cloned();
    let active = status.as_ref().is_some_and(|r| r.get_path("active").is_some_and(Value::truthy));
    let starting = status.as_ref().is_some_and(|r| r.get_path("starting").is_some_and(Value::truthy));
    let stopping = status.as_ref().is_some_and(|r| r.get_path("stopping").is_some_and(Value::truthy));
    ui.horizontal_wrapped(|ui| {
        if st.view == 0 {
            ui.label(RichText::new("Recordings").font(font_semibold(type_scale::TITLE)).color(t.fg));
        } else {
            if widgets::button(ui, t, "Recordings", Kind::Ghost).clicked() {
                st.view = 0;
                st.playing = false;
            }
            ui.label(RichText::new("/").color(t.text_faint));
            let title = selected_title(app, st);
            if st.view == 2 {
                if widgets::button(ui, t, &title, Kind::Ghost).clicked() {
                    st.view = 1;
                    st.playing = false;
                }
                ui.label(RichText::new("/ Review clip").color(t.text_dim));
            } else {
                ui.label(RichText::new(title).font(font_semibold(type_scale::BODY)).color(t.fg));
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::button_ex(ui, t, None, "Settings", Kind::Ghost, Size::Small, 0.0, true)
                .on_hover_text("Recording sources and storage")
                .clicked()
            {
                app.open_view(crate::app::ViewId::Settings);
            }
            if widgets::button_ex(
                ui, t, None,
                if stopping { "Stopping…" } else if starting { "Starting…" } else if active { "Stop recording" } else { "Start recording" },
                Kind::Secondary, Size::Small, 0.0,
                app.m.connected && status.is_some() && !starting && !stopping,
            ).clicked() {
                app.m.action(if active { "recording.stop" } else { "recording.start" }, Value::Null);
                st.status_at = None;
            }
            let failed = app.m.query_errors.contains_key("recording.status");
            let label = if !app.m.connected { "Offline" } else if failed { "Status unavailable" } else if stopping { "Finishing recording" }
                else if starting { "Starting" } else if active { "Recording" } else if status.is_none() { "Checking recorder…" } else { "Recorder ready" };
            let color = if active { t.green } else if failed { t.yellow } else { t.text_dim };
            let badge = widgets::badge(ui, t, label, color);
            if let Some(r) = &status {
                badge.on_hover_text(s(r, "health.detail"));
            }
            super::recording::header_status(app, ui, t, status.as_ref());
        });
    });
}

fn selected_title(app: &App, st: &State) -> String {
    app.m.q_list("sessions").iter().find(|x| Some(s(x, "id")) == st.selected.as_deref())
        .map(stream_title).unwrap_or_else(|| "Stream".into())
}

fn stream_title(session: &Value) -> String {
    let title = s(session, "title");
    if title.is_empty() { when(i(session, "started_at")) } else { title.to_string() }
}

fn capturing(app: &App, id: &str) -> bool {
    id == app.m.session && (
        ["recording.active", "recording.starting", "recording.stopping"].iter().any(|key| app.m.b(key))
        || app.m.q("recording.status").is_some_and(|r| ["active", "starting", "stopping"].iter().any(|key| r.get_path(key).is_some_and(Value::truthy)))
    )
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
            if widgets::callout(ui, t, widgets::Tone::Danger, icon::WARN, &head, &body, (!session.is_empty() && app.m.connected && !capturing(app, &session)).then_some("Try again")) {
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

fn clip_grid(ui: &mut Ui, t: &Theme, st: &mut State, clips: &[&Value]) {
    let gap = spacing::L;
    let (per, w) = grid(ui, if st.grid_orientation == 0 { CLIP_MIN } else { 340.0 }, gap);
    for row in clips.chunks(per) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for c in row {
                ui.allocate_ui_with_layout(Vec2::new(w, 10.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(w);
                    clip_card(ui, t, st, c);
                });
            }
        });
        ui.add_space(gap);
    }
}

fn library(app: &mut App, ui: &mut Ui, t: &Theme, st: &mut State) {
    let sessions = app.m.q_list("sessions");
    let query = st.library_search.trim().to_lowercase();
    let mut recorded: Vec<&Value> = sessions.iter().filter(|sess| {
        let id = s(sess, "id");
        let has_media = app.m.q(&key(id)).is_some_and(|data| !list(data, "recordings").is_empty() || !list(data, "clips").is_empty());
        (has_media || capturing(app, id)) && (query.is_empty() || stream_title(sess).to_lowercase().contains(&query) || id.to_lowercase().contains(&query))
    }).collect();
    recorded.sort_by_key(|sess| std::cmp::Reverse(i(sess, "started_at")));
    ui.horizontal(|ui| {
        ui.add(widgets::field(&mut st.library_search).hint_text("Search recordings").desired_width(300.0));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            widgets::hint(ui, t, &format!("{} recordings · newest first", recorded.len()));
        });
    });
    ui.add_space(spacing::M);
    let failed = app.m.query_errors.contains_key("sessions") || sessions.iter().any(|sess| app.m.query_errors.contains_key(&key(s(sess, "id"))));
    let loading = app.m.connected && (app.m.q("sessions").is_none() || sessions.iter().any(|sess| app.m.q(&key(s(sess, "id"))).is_none()));
    if failed {
        if widgets::callout(ui, t, widgets::Tone::Warn, icon::WARN, "Some recordings couldn't be loaded", "Your available recordings are still shown below.", Some("Try again")) {
            st.last_sessions = None;
            st.asked.clear();
        }
        ui.add_space(spacing::M);
    }
    if recorded.is_empty() {
        let (title, body) = if loading && !failed {
            ("Loading recordings…", "Finding your recorded streams and clips.")
        } else if !query.is_empty() {
            ("No matching recordings", "Try a different title or date.")
        } else {
            ("Your recordings live here", "Start a recording above. Open a recorded stream to find, review and export its best moments.")
        };
        widgets::empty_state(ui, t, icon::FILM, title, body, None);
        return;
    }
    egui::ScrollArea::vertical().id_salt("recordings-library").auto_shrink([false, false]).show(ui, |ui| {
        for sess in recorded {
            if stream_row(app, ui, t, st, sess) {
                select_stream(st, s(sess, "id"));
            }
            ui.add_space(spacing::S);
        }
    });
}

fn select_stream(st: &mut State, id: &str) {
    st.view = 1;
    st.playing = false;
    st.hover = None;
    if st.selected.as_deref() == Some(id) {
        return;
    }
    st.selected = Some(id.to_string());
    st.selected_clip = None;
    st.stream_panel = 0;
    st.clip_filter = 0;
    st.clip_search.clear();
    st.last_query = None;
    st.timeline_key = None;
    st.timeline = None;
    st.window_start = 0.0;
    st.window_len = 1800.0;
    st.window_requested = None;
    st.window_updated = None;
    st.range = None;
    st.range_start = None;
    st.cursor = None;
}

fn span(sess: &Value) -> (i64, i64, bool) {
    let start = i(sess, "started_at");
    match sess.get_path("ended_at").and_then(Value::as_i64) {
        Some(e) => (start, e, false),
        None => (start, now_s(), true),
    }
}

/// Merge simultaneous canvases rather than counting the same recording twice.
fn recording_duration(data: &Value) -> f64 {
    let mut ranges: Vec<(i64, i64)> = list(data, "recordings").iter()
        .filter_map(|r| {
            let (a, b) = (i(r, "start_ns"), i(r, "end_ns"));
            (b > a && a > 0).then_some((a, b))
        }).collect();
    ranges.sort_unstable();
    let mut total = 0i64;
    let mut end = 0i64;
    for (a, b) in ranges {
        total += (b - a.max(end)).max(0);
        end = end.max(b);
    }
    total as f64 / 1_000_000_000.0
}

fn recording_length(data: &Value, active: bool) -> String {
    let duration = recording_duration(data);
    if duration > 0.0 {
        format!("{} recorded{}", short_clock(duration), if active { " + recording now" } else { "" })
    } else if active {
        "Recording in progress".into()
    } else {
        "Recorded stream".into()
    }
}

fn stream_row(app: &App, ui: &mut Ui, t: &Theme, st: &mut State, sess: &Value) -> bool {
    let id = s(sess, "id");
    let data = app.m.q(&key(id)).cloned().unwrap_or_default();
    let clips = list(&data, "clips");
    let active = capturing(app, id);
    let selected = st.selected.as_deref() == Some(id);
    let frame = egui::Frame::new().fill(if selected { mix(t.surface, t.accent, 0.05) } else { t.surface })
        .stroke(Stroke::new(1.0, t.border)).corner_radius(radius::CARD).inner_margin(12);
    let title = stream_title(sess);
    let mut picture = Rect::NOTHING;
    let response = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            picture = ui.allocate_exact_size(Vec2::new(168.0, 94.5), Sense::hover()).0;
            ui.add_space(spacing::M);
            ui.vertical(|ui| {
                ui.add_space(spacing::XS);
                ui.add(egui::Label::new(RichText::new(&title).font(font_semibold(type_scale::LARGE)).color(t.fg)).truncate());
                if !s(sess, "title").is_empty() {
                    widgets::hint(ui, t, &when(i(sess, "started_at")));
                }
                widgets::hint(ui, t, &format!("{} · {} clip{}", recording_length(&data, active), clips.len(), if clips.len() == 1 { "" } else { "s" }));
                ui.add_space(spacing::XS);
                let review = clips.iter().filter(|c| s(c, "status") == "ready").count();
                let (label, color) = if active {
                    ("Recording now".to_string(), t.green)
                } else if app.m.str("clips.job.session") == id && app.m.str("clips.job.state") == "running" {
                    ("Making clips".to_string(), t.accent)
                } else if s(&data, "job.state") == "failed" {
                    ("Processing needs attention".to_string(), t.yellow)
                } else if review > 0 {
                    (format!("{review} to review"), t.accent)
                } else if clips.iter().any(|c| s(c, "status") == "failed") {
                    ("Clips need attention".to_string(), t.yellow)
                } else if clips.iter().any(|c| !matches!(s(c, "status"), "approved" | "rejected" | "uploaded")) {
                    ("Clips processing".to_string(), t.accent)
                } else if !clips.is_empty() {
                    ("Reviewed".to_string(), t.green)
                } else {
                    ("Ready to make clips".to_string(), t.text_dim)
                };
                widgets::badge(ui, t, &label, color);
            });
        });
    }).response;
    let response = ui.interact(response.rect, ui.id().with(("recording", id)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Open recording {id}")));
    if ui.is_rect_visible(picture) {
        let recording = list(&data, "recordings").iter().find(|r| s(r, "canvas") != "tall" && r.get_path("exists").is_some_and(Value::truthy));
        let clip = clips.iter().find(|c| !s(c, "wide").is_empty());
        let (path, poster, version, duration) = if let Some(c) = clip {
            (s(c, "wide"), poster_path(c, 1), i(c, "version"), f(c, "duration"))
        } else if let Some(r) = recording.filter(|_| !active) {
            (s(r, "path"), s(r, "path"), i(r, "end_ns"), recording_duration(&data))
        } else {
            ("", "", 0, 0.0)
        };
        let mut texture = st.media.thumbnail(ui.ctx(), poster, version);
        if hover_ready(ui, st, &response, &format!("stream:{id}")) && !active && duration > 0.0 {
            texture = st.media.preview(ui.ctx(), path, version, 0.0, duration.min(8.0)).or(texture);
        }
        media_frame(ui, t, picture, texture.as_ref(), if active && path.is_empty() { "Recording…" } else { "" });
    }
    response.clicked()
}

fn stream(app: &mut App, ui: &mut Ui, t: &Theme, st: &mut State) {
    let Some(id) = st.selected.clone() else { st.view = 0; return };
    let data = app.m.q(&key(&id)).cloned();
    let title = selected_title(app, st);
    let active = capturing(app, &id);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(&title).font(font_semibold(type_scale::TITLE)).color(t.fg));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let running = app.m.str("clips.job.state") == "running" && app.m.str("clips.job.session") == id;
            let available = data.as_ref().is_some_and(|d| list(d, "recordings").iter().any(|r| s(r, "canvas") != "tall" && r.get_path("exists").is_some_and(Value::truthy)));
            let label = if data.as_ref().is_some_and(|d| !list(d, "clips").is_empty()) { "Make clips again" } else { "Make clips" };
            if widgets::button_ex(ui, t, Some(icon::FILM), label, Kind::Secondary, Size::Medium, 0.0, app.m.connected && available && !running && !active)
                .on_hover_text(if active { "Finish recording before making clips" } else { "Find and cut the best moments from this recording" }).clicked()
            {
                app.m.action("clips.process", Value::map().with("session", id.clone()));
            }
        });
    });
    let Some(data) = data else {
        let failed = app.m.query_errors.contains_key(&key(&id));
        if widgets::empty_state(ui, t, icon::FILM, if failed { "Couldn't load this recording" } else { "Loading clips…" }, "Your clips and source recording appear here.", failed.then_some("Try again")) {
            st.last_query = None;
        }
        return;
    };
    let clips = list(&data, "clips");
    widgets::hint(ui, t, &format!("{} · {} clip{}{}", recording_length(&data, active), clips.len(), if clips.len() == 1 { "" } else { "s" }, if active { " · finish recording to make clips" } else { "" }));
    ui.add_space(spacing::M);
    widgets::segmented(ui, t, &mut st.stream_panel, &["Clips", "Source timeline"]);
    ui.add_space(spacing::M);
    if st.stream_panel == 1 {
        egui::ScrollArea::vertical().id_salt(("source-timeline", &id)).auto_shrink([false, false]).show(ui, |ui| {
            if active {
                widgets::hint(ui, t, "The source timeline and manual cuts become available after recording finishes.");
                return;
            }
            let show_data = st.timeline.take();
            if let Some(show) = show_data.as_ref() {
                show_timeline(app, ui, t, st, &id, show, &data, clips);
            } else if app.m.query_errors.contains_key("clipping.timeline") {
                if widgets::callout(ui, t, widgets::Tone::Warn, icon::WARN, "Couldn't load the timeline", "Your clips remain available.", Some("Try again")) {
                    st.timeline_key = None;
                }
            } else {
                widgets::hint(ui, t, "Loading source timeline…");
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
            let indexed = show_data.as_ref().is_some_and(|show| show.get_path("manifest").is_some_and(|v| !v.is_null()));
            st.timeline = show_data;
            if !indexed && let Some(meta) = app.m.q_list("sessions").iter().find(|x| s(x, "id") == id) {
                let (start, end, _) = span(meta);
                ui.add_space(spacing::M);
                timeline(ui, t, list(&data, "markers"), start * 1000, end * 1000);
                ui.horizontal(|ui| {
                    legend(ui, t, t.accent, "Hype moment");
                    legend(ui, t, t.cyan, "Marker");
                });
            }
            widgets::details(ui, t, ("recording-files", &id), "Source files", |ui| {
                for r in list(&data, "recordings") {
                    ui.label(RichText::new(s(r, "path")).font(font_mono(type_scale::SMALL)).color(t.text_dim));
                    if !r.get_path("exists").is_some_and(Value::truthy) {
                        widgets::hint(ui, t, "File moved or deleted. Restore it to its original location to make clips.");
                    }
                }
            });
        });
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.add(widgets::field(&mut st.clip_search).hint_text("Search clips or transcript").desired_width(260.0));
        egui::ComboBox::from_id_salt("clip-status").selected_text(["All clips", "To review", "Approved", "Rejected", "Uploaded", "Failed"][st.clip_filter]).show_ui(ui, |ui| {
            for (n, label) in ["All clips", "To review", "Approved", "Rejected", "Uploaded", "Failed"].iter().enumerate() {
                ui.selectable_value(&mut st.clip_filter, n, *label);
            }
        });
        egui::ComboBox::from_id_salt("clip-sort").selected_text(["Best moments", "Source order", "Newest first"][st.clip_sort]).show_ui(ui, |ui| {
            for (n, label) in ["Best moments", "Source order", "Newest first"].iter().enumerate() {
                ui.selectable_value(&mut st.clip_sort, n, *label);
            }
        });
        widgets::segmented(ui, t, &mut st.grid_orientation, &["Portrait", "Landscape"]);
    });
    ui.add_space(spacing::M);
    let query = st.clip_search.trim().to_lowercase();
    let status = ["", "ready", "approved", "rejected", "uploaded", "failed"][st.clip_filter];
    let mut shown: Vec<&Value> = clips.iter().filter(|c| {
        (status.is_empty() || s(c, "status") == status)
            && (query.is_empty() || ["title", "captions", "song", "requester"].iter().any(|field| s(c, field).to_lowercase().contains(&query)))
    }).collect();
    shown.sort_by(|a, b| match st.clip_sort {
        1 => f(a, "in").total_cmp(&f(b, "in")),
        2 => i(b, "id").cmp(&i(a, "id")),
        _ => i(a, "rank").cmp(&i(b, "rank")).then_with(|| f(b, "score").total_cmp(&f(a, "score"))),
    });
    if shown.is_empty() {
        widgets::empty_state(ui, t, icon::FILM, if clips.is_empty() { "No clips yet" } else { "No matching clips" },
            if active { "Finish recording, then make clips from this stream." } else if clips.is_empty() { "Choose Make clips, or select a moment in Source timeline." } else { "Try another search or review status." }, None);
        return;
    }
    egui::ScrollArea::vertical().id_salt(("stream-clips", &id)).auto_shrink([false, false]).show(ui, |ui| {
        clip_grid(ui, t, st, &shown);
    });
}

fn clip_show_range(clip: &Value, manifest: &Value) -> (f64, f64) {
    let origin = i(manifest, "t0_ns");
    (i(clip, "start_ns").saturating_sub(origin) as f64 / 1e9, i(clip, "end_ns").saturating_sub(origin) as f64 / 1e9)
}

/// Draw only the chosen window, so a multi-hour show stays legible at either screen width.
fn show_timeline(app: &mut App, ui: &mut Ui, t: &Theme, st: &mut State, id: &str, show: &Value, session: &Value, clips: &[Value]) {
    let Some(manifest) = show.get_path("manifest").filter(|v| !v.is_null()) else {
        widgets::callout(
            ui,
            t,
            widgets::Tone::Info,
            icon::TIMELINE,
            "Timeline not ready",
            "This stream hasn't been indexed yet. You can still use its markers and Make clips.",
            None,
        );
        return;
    };
    let duration = f(manifest, "duration");
    if duration <= 0.0 {
        widgets::hint(ui, t, "The timeline will appear after the recording has finished.");
        return;
    }
    widgets::section(ui, t, icon::TIMELINE, "Show timeline");
    widgets::hint(ui, t, "Move through the show, then click twice to choose the start and end of a clip.");
    ui.add_space(spacing::S);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Show").color(t.text_dim));
        for (label, seconds) in [("15 min", 900.0), ("30 min", 1800.0), ("1 hour", 3600.0), ("All", duration)] {
            if widgets::button_ex(ui, t, None, label, if (st.window_len - seconds).abs() < 1.0 { Kind::Secondary } else { Kind::Ghost }, Size::Small, 0.0, true)
                .clicked()
            {
                st.window_len = seconds;
                st.window_updated = Some(Instant::now());
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(320));
            }
        }
    });
    let len = st.window_len.clamp(1.0, duration);
    st.window_start = st.window_start.clamp(0.0, (duration - len).max(0.0));
    if duration > len && ui.add(egui::Slider::new(&mut st.window_start, 0.0..=(duration - len)).text("From").custom_formatter(|v, _| short_clock(v))).changed()
    {
        st.window_updated = Some(Instant::now());
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(320));
    }
    let left = st.window_start;
    let right = left + len;
    let width = ui.available_width().max(240.0);
    let gutter = 104.0;
    let chart_width = (width - gutter).max(140.0);
    let x = |at: f64| gutter + (((at - left) / len).clamp(0.0, 1.0) as f32) * chart_width;
    let tracks = [
        ("songs", "Songs", t.accent),
        ("talk", "Talk", t.cyan),
        ("scenes", "Scenes", t.modulated()),
        ("modes", "Show", t.text_dim),
        ("lights", "Lights", t.yellow),
        ("effects", "Effects", t.cyan),
        ("chat", "Chat activity", t.green),
        ("moments", "Hype", t.accent),
        ("markers", "Markers", t.cyan),
        ("clips", "Clips", t.green),
    ];
    // Per lane we draw a bounded number of visible items. The query is cached until another
    // show is selected; changing the window does not fetch the whole timeline again.
    for (name, title, color) in tracks {
        let items = match name {
            "talk" => list(show, "transcript"),
            "clips" => clips,
            _ => list(show, &format!("lanes.{name}")),
        };
        if items.is_empty() && !matches!(name, "songs" | "markers") {
            continue;
        }
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 38.0), Sense::click());
        let canvas = Rect::from_min_max(Pos2::new(rect.left() + gutter, rect.top()), rect.max);
        let painter = ui.painter_at(rect);
        painter.rect_filled(canvas, CornerRadius::same(3), t.inset);
        painter.text(Pos2::new(rect.left(), rect.center().y), Align2::LEFT_CENTER, title, font(type_scale::SMALL), t.text_dim);
        let tx = |time: f64| rect.left() + x(time);
        let step = [60.0, 300.0, 900.0, 1800.0, 3600.0].into_iter().find(|step| len / step <= 10.0).unwrap_or(7200.0);
        let mut tick = (left / step).ceil() * step;
        while tick <= right {
            let xx = tx(tick);
            painter.line_segment([Pos2::new(xx, rect.top()), Pos2::new(xx, rect.bottom())], Stroke::new(1.0, t.border));
            if name == "songs" {
                painter.text(Pos2::new(xx + 3.0, rect.top() + 2.0), Align2::LEFT_TOP, short_clock(tick), font_mono(type_scale::SMALL), t.text_dim);
            }
            tick += step;
        }
        if let Some((a, b)) = st.range
            && b >= left
            && a <= right
        {
            let band = Rect::from_min_max(Pos2::new(tx(a.max(left)), canvas.top()), Pos2::new(tx(b.min(right)), canvas.bottom()));
            painter.rect_filled(band, CornerRadius::ZERO, t.accent.gamma_multiply(0.20));
        }
        if let Some(a) = st.range_start.or(st.cursor)
            && (left..=right).contains(&a)
        {
            let xx = tx(a);
            painter.line_segment([Pos2::new(xx, canvas.top()), Pos2::new(xx, canvas.bottom())], Stroke::new(1.5, t.accent));
        }
        let mut hovered = None;
        let mut drawn = 0;
        for item in items {
            let (begin, end) = if name == "clips" {
                clip_show_range(item, manifest)
            } else {
                let begin = item.get_path("t0").and_then(Value::as_f64).or_else(|| item.get_path("t").and_then(Value::as_f64)).unwrap_or(0.0);
                (begin, item.get_path("t1").and_then(Value::as_f64).unwrap_or(begin))
            };
            if end < left || begin > right {
                continue;
            }
            if drawn == 250 {
                break;
            }
            drawn += 1;
            let a = tx(begin.max(left));
            let b = tx(end.min(right)).max(a + 3.0);
            let bar = Rect::from_min_max(Pos2::new(a, canvas.top() + 10.0), Pos2::new(b.min(canvas.right()), canvas.bottom() - 4.0));
            painter.rect_filled(bar, CornerRadius::same(2), color.gamma_multiply(0.60));
            let raw = if name == "talk" {
                s(item, "text")
            } else if name == "clips" {
                s(item, "title")
            } else {
                s(item, "label")
            };
            let label = if matches!(name, "scenes" | "modes" | "effects" | "lights") {
                std::borrow::Cow::Owned(crate::views::live::nice(raw))
            } else {
                std::borrow::Cow::Borrowed(raw)
            };
            if bar.width() > 78.0 {
                let cropped: String = label.chars().take((bar.width() / 7.0).floor().max(2.0) as usize).collect();
                painter.text(Pos2::new(bar.left() + 4.0, bar.center().y), Align2::LEFT_CENTER, cropped, font(type_scale::SMALL), t.fg);
            }
            if response.hover_pos().is_some_and(|p| bar.expand2(Vec2::new(2.0, 3.0)).contains(p)) {
                hovered = Some(format!(
                    "{} · {}{}\n{}{}",
                    short_clock(begin),
                    label,
                    if end > begin { format!(" – {}", short_clock(end)) } else { String::new() },
                    [s(item, "title"), s(item, "user"), s(item, "channel")].into_iter().filter(|v| !v.is_empty()).collect::<Vec<_>>().join(" · "),
                    if item.get_path("dmca").is_some_and(Value::truthy) { "\nRisk of DMCA" } else { "" }
                ));
            }
        }
        if drawn == 250 {
            painter.text(Pos2::new(canvas.right() - 4.0, rect.center().y), Align2::RIGHT_CENTER, "Zoom in for more", font(type_scale::SMALL), t.fg);
        }
        if let Some(text) = hovered {
            response.clone().on_hover_text(text);
        }
        if response.clicked()
            && let Some(pos) = response.interact_pointer_pos()
            && canvas.contains(pos)
        {
            let at = (left + ((pos.x - canvas.left()) / chart_width) as f64 * len).clamp(0.0, duration);
            st.cursor = Some(at);
            if let Some(first) = st.range_start.take() {
                if (at - first).abs() >= 1.0 {
                    st.range = Some((first.min(at), first.max(at)));
                } else {
                    st.range_start = Some(first);
                }
            } else {
                st.range = None;
                st.range_start = Some(at);
            }
        }
    }
    ui.add_space(spacing::M);
    ui.horizontal_wrapped(|ui| {
        if let Some((mut a, mut b)) = st.range {
            ui.label(RichText::new("Selected").color(t.text_dim));
            ui.add(egui::DragValue::new(&mut a).range(0.0..=(b - 1.0).max(0.0)).speed(0.5).custom_formatter(|v, _| short_clock(v)));
            ui.label(RichText::new("to").color(t.text_dim));
            ui.add(egui::DragValue::new(&mut b).range((a + 1.0)..=duration).speed(0.5).custom_formatter(|v, _| short_clock(v)));
            st.range = Some((a, b));
            let min = f(session, "min_len");
            let max = f(session, "max_len");
            let valid = b - a >= min && b - a <= max && recording_covers(manifest, session, a, b);
            if widgets::button_ex(ui, t, None, "Make clip", Kind::Primary, Size::Medium, 0.0, app.m.connected && valid).clicked() {
                app.m.action("clips.make", Value::map().with("session", id).with("in", a).with("out", b));
            }
            if widgets::button_ex(ui, t, None, "Clear", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                st.range = None;
                st.range_start = None;
            }
        } else if let Some(a) = st.range_start {
            widgets::hint(ui, t, &format!("Start at {}. Click again to choose the end.", short_clock(a)));
        } else {
            widgets::hint(ui, t, "Click once for the start, then again for the end.");
        }
        let at = st.cursor.or_else(|| st.range.map(|(a, _)| a)).unwrap_or(0.0);
        if widgets::button_ex(ui, t, Some(icon::PLAY), "Play from here", Kind::Secondary, Size::Medium, 0.0, !list(manifest, "recordings").is_empty()).clicked()
        {
            play_recording(app, manifest, at);
        }
    });
    if let Some((a, b)) = st.range {
        let min = f(session, "min_len");
        let max = f(session, "max_len");
        if b - a < min || b - a > max {
            widgets::hint(ui, t, &format!("Choose a clip between {min:.0} and {max:.0} seconds long."));
        } else if !recording_covers(manifest, session, a, b) {
            widgets::hint(ui, t, "Choose a range inside one available wide recording.");
        }
    }
}

/// A selection cannot cross a recording boundary or use a missing/vertical recording.
fn recording_covers(manifest: &Value, session: &Value, start: f64, end: f64) -> bool {
    list(manifest, "recordings").iter().any(|rec| {
        s(rec, "canvas") != "tall"
            && f(rec, "offset") <= start
            && f(rec, "offset") + f(rec, "duration") >= end
            && list(session, "recordings").iter().any(|file| s(file, "path") == s(rec, "path") && file.get_path("exists").is_some_and(Value::truthy))
    })
}

fn play_recording(app: &mut App, manifest: &Value, at: f64) {
    let recordings = list(manifest, "recordings");
    let chosen = recordings
        .iter()
        .filter(|r| s(r, "canvas") != "tall" && f(r, "offset") <= at)
        .max_by(|a, b| f(a, "offset").total_cmp(&f(b, "offset")))
        .or_else(|| recordings.iter().find(|r| s(r, "canvas") != "tall"));
    let Some(rec) = chosen else {
        app.m.toast("No video recording was found for this point.", true);
        return;
    };
    let path = s(rec, "path");
    if !std::path::Path::new(path).is_file() {
        app.m.toast("That recording has moved. Restore it to its original folder to play it.", true);
        return;
    }
    let offset = (at - f(rec, "offset")).max(0.0);
    if let Err(e) = std::process::Command::new("mpv")
        .arg(format!("--start={offset:.2}"))
        .arg("--")
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        app.m.toast(format!("Couldn't play the recording. Install mpv or check the file. ({e})"), true);
    }
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
        "rejected" => ("Rejected", t.text_dim),
        "uploaded" => ("Uploaded", t.modulated()),
        "failed" => ("Failed", t.bright_red),
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

fn clip_title(c: &Value) -> String {
    let title = s(c, "title");
    if title.is_empty() { format!("Moment {}", i(c, "rank")) } else { title.to_string() }
}

fn clip_orientation(c: &Value, preferred: usize) -> usize {
    if s(c, if preferred == 0 { "tall" } else { "wide" }).is_empty() { 1 - preferred } else { preferred }
}

fn clip_path(c: &Value, orientation: usize) -> &str {
    s(c, if orientation == 0 { "tall" } else { "wide" })
}

fn poster_path(c: &Value, orientation: usize) -> &str {
    let poster = s(c, if orientation == 0 { "tall_thumb" } else { "wide_thumb" });
    if poster.is_empty() { clip_path(c, orientation) } else { poster }
}

fn hover_ready(ui: &Ui, st: &mut State, response: &egui::Response, key: &str) -> bool {
    if !response.hovered() {
        return false;
    }
    st.hover_seen = true;
    if st.hover.as_ref().is_none_or(|(old, _)| old != key) {
        st.hover = Some((key.to_string(), Instant::now()));
    }
    let elapsed = st.hover.as_ref().unwrap().1.elapsed();
    let dwell = std::time::Duration::from_millis(250);
    if elapsed < dwell {
        ui.ctx().request_repaint_after(dwell - elapsed);
        false
    } else {
        true
    }
}

/// Fit real media without stretching portrait sources into landscape cells.
fn media_frame(ui: &Ui, t: &Theme, rect: Rect, texture: Option<&egui::TextureHandle>, label: &str) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    if let Some(texture) = texture {
        ui.painter().rect_filled(rect, CornerRadius::same(radius::TILE), egui::Color32::from_rgb(8, 9, 11));
        let size = texture.size_vec2();
        let scale = (rect.width() / size.x).min(rect.height() / size.y);
        let fit = Rect::from_center_size(rect.center(), size * scale);
        widgets::video_frame(ui, t, fit, Some((texture.id(), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)))), label, None, None);
    } else {
        widgets::video_frame(ui, t, rect, None, label, None, None);
    }
}

fn clip_card(ui: &mut Ui, t: &Theme, st: &mut State, c: &Value) {
    let id = i(c, "id");
    let version = i(c, "version");
    let orientation = clip_orientation(c, st.grid_orientation);
    let title = clip_title(c);
    let mut picture = Rect::NOTHING;
    let frame = egui::Frame::new().fill(t.surface).stroke(Stroke::new(1.0, t.border))
        .corner_radius(radius::CARD).inner_margin(10);
    let response = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        let width = ui.available_width();
        let height = if st.grid_orientation == 0 { (width * 16.0 / 9.0).min(400.0) } else { width * 9.0 / 16.0 };
        picture = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover()).0;
        ui.add_space(spacing::S);
        ui.add(egui::Label::new(RichText::new(&title).font(font_semibold(type_scale::BODY)).color(t.fg)).truncate()).on_hover_text(&title);
        ui.horizontal(|ui| {
            widgets::hint(ui, t, &short_clock(f(c, "duration")));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (label, color) = status_words(t, s(c, "status"));
                widgets::badge(ui, t, label, color);
            });
        });
    }).response;
    let response = ui.interact(response.rect, ui.id().with(("clip", id)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Open clip {id}")));
    if ui.is_rect_visible(picture) {
        let mut texture = st.media.thumbnail(ui.ctx(), poster_path(c, orientation), version);
        let hovering = hover_ready(ui, st, &response, &format!("clip:{id}:{orientation}"));
        if hovering {
            texture = st.media.preview(ui.ctx(), clip_path(c, orientation), version, 0.0, f(c, "duration")).or(texture);
        }
        media_frame(ui, t, picture, texture.as_ref(), if hovering { "Muted preview" } else { "" });
        if response.hovered() && !hovering {
            ui.painter().circle_filled(picture.center(), 22.0, egui::Color32::from_black_alpha(140));
            ui.painter().text(picture.center(), Align2::CENTER_CENTER, icon::PLAY, font(18.0), egui::Color32::WHITE);
        }
    }
    if response.clicked() {
        st.selected_clip = Some(id);
        st.detail_orientation = orientation;
        st.view = 2;
        st.playing = true;
        st.still = None;
        st.detail_media_key = None;
        st.hover = None;
    }
}

fn clip_detail(app: &mut App, ui: &mut Ui, t: &Theme, st: &mut State) {
    let Some(session) = st.selected.clone() else { st.view = 0; return };
    let data = app.m.q(&key(&session)).cloned().unwrap_or_default();
    let Some(c) = list(&data, "clips").iter().find(|c| Some(i(c, "id")) == st.selected_clip) else {
        if widgets::empty_state(ui, t, icon::FILM, "This clip is no longer available", "It may have been replaced while making clips again.", Some("Back to clips")) {
            st.view = 1;
            st.playing = false;
        }
        return;
    };
    ui.horizontal_wrapped(|ui| {
        if widgets::button(ui, t, "Back to clips", Kind::Ghost).clicked() {
            st.view = 1;
            st.playing = false;
        }
        ui.label(RichText::new(clip_title(c)).font(font_semibold(type_scale::HEADING)).color(t.fg));
        let (label, color) = status_words(t, s(c, "status"));
        widgets::badge(ui, t, label, color);
    });
    ui.add_space(spacing::M);
    egui::ScrollArea::vertical().id_salt(("clip-review", i(c, "id"))).auto_shrink([false, false]).show(ui, |ui| {
        let width = ui.available_width();
        if width >= 880.0 {
            let sidebar = 340.0;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = spacing::L;
                ui.allocate_ui_with_layout(Vec2::new(width - sidebar - spacing::L, 10.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(width - sidebar - spacing::L);
                    clip_player(app, ui, t, st, c);
                });
                ui.allocate_ui_with_layout(Vec2::new(sidebar, 10.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(sidebar);
                    clip_review(app, ui, t, st, c, &data);
                });
            });
        } else {
            clip_player(app, ui, t, st, c);
            ui.add_space(spacing::M);
            clip_review(app, ui, t, st, c, &data);
        }
        ui.add_space(spacing::L);
        clip_context(ui, t, c);
    });
}

fn clip_player(app: &mut App, ui: &mut Ui, t: &Theme, st: &mut State, c: &Value) {
    if !s(c, "wide").is_empty() && !s(c, "tall").is_empty() {
        widgets::segmented(ui, t, &mut st.detail_orientation, &["Portrait", "Landscape"]);
        ui.add_space(spacing::S);
    }
    let orientation = clip_orientation(c, st.detail_orientation);
    let path = clip_path(c, orientation);
    let version = i(c, "version");
    let media_key = format!("{}:{version}:{orientation}", i(c, "id"));
    if st.detail_media_key.as_deref() != Some(media_key.as_str()) {
        st.detail_media_key = Some(media_key);
        st.still = None;
    }
    let width = ui.available_width();
    let height = if orientation == 0 { (width * 16.0 / 9.0).min(510.0) } else { (width * 9.0 / 16.0).min(510.0) };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Toggle muted preview"));
    if response.clicked() {
        st.playing = !st.playing;
    }
    let poster = st.media.thumbnail(ui.ctx(), poster_path(c, orientation), version);
    if st.playing && ui.is_rect_visible(rect)
        && let Some(texture) = st.media.preview(ui.ctx(), path, version, 0.0, f(c, "duration"))
    {
        st.still = Some(texture);
    }
    let texture = st.still.as_ref().or(poster.as_ref());
    media_frame(ui, t, rect, texture, if st.playing { "Muted · looping" } else { "Muted · paused" });
    ui.add_space(spacing::S);
    ui.horizontal_wrapped(|ui| {
        if widgets::button_ex(ui, t, None, if st.playing { "Pause preview" } else { "Play preview" }, Kind::Secondary, Size::Medium, 0.0, !path.is_empty()).clicked() {
            st.playing = !st.playing;
        }
        widgets::hint(ui, t, &format!("{} · inline preview has no sound", short_clock(f(c, "duration"))));
        if widgets::button_ex(ui, t, Some(icon::PLAY), "Open player with sound", Kind::Ghost, Size::Medium, 0.0, !path.is_empty())
            .on_hover_text("Play this exported clip in your system video player with audio").clicked()
        {
            open(app, path);
        }
    });
}

fn clip_review(app: &mut App, ui: &mut Ui, t: &Theme, st: &mut State, c: &Value, data: &Value) {
    let id = i(c, "id");
    let version = i(c, "version");
    let status = s(c, "status");
    let active = capturing(app, s(c, "session"));
    let ready = app.m.connected && !active;
    widgets::panel(ui, t, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new("Review").font(font_semibold(type_scale::LARGE)).color(t.fg));
        ui.add_space(spacing::S);
        ui.horizontal_wrapped(|ui| {
            if widgets::button_ex(ui, t, Some(icon::CHECK), "Approve", Kind::Primary, Size::Medium, 0.0, ready && matches!(status, "ready" | "rejected")).clicked() {
                app.m.action("clips.approve", Value::map().with("id", id));
            }
            if widgets::button_ex(ui, t, Some(icon::CROSS), "Reject", Kind::Secondary, Size::Medium, 0.0, ready && status != "rejected").clicked() {
                app.m.action("clips.reject", Value::map().with("id", id));
            }
        });
        if status == "failed" {
            ui.label(RichText::new(s(c, "error")).color(t.bright_red));
        }
        ui.add_space(spacing::L);
        ui.label(RichText::new("Trim").font(font_semibold(type_scale::LARGE)).color(t.fg));
        widgets::hint(ui, t, "Start and end in the source recording.");
        let edit = st.trims.entry(id).or_insert((version, f(c, "in"), f(c, "out")));
        if edit.0 != version {
            *edit = (version, f(c, "in"), f(c, "out"));
        }
        let (_, mut a, mut b) = *edit;
        ui.horizontal(|ui| {
            let start_label = ui.label(RichText::new("In").color(t.text_dim));
            ui.add(egui::DragValue::new(&mut a).speed(0.05).range(0.0..=(b - 1.0).max(0.0)).custom_formatter(|v, _| clock(v))).labelled_by(start_label.id);
            let end_label = ui.label(RichText::new("Out").color(t.text_dim));
            ui.add(egui::DragValue::new(&mut b).speed(0.05).range((a + 1.0)..=f64::MAX).custom_formatter(|v, _| clock(v))).labelled_by(end_label.id);
        });
        let edited = (a - f(c, "in")).abs() > 0.01 || (b - f(c, "out")).abs() > 0.01;
        let min = f(data, "min_len").max(1.0);
        let max = f(data, "max_len");
        let valid = b - a >= min && (max <= 0.0 || b - a <= max);
        widgets::hint(ui, t, &format!("{} selected · peak at {}", short_clock(b - a), short_clock(f(c, "peak"))));
        if !valid {
            widgets::hint(ui, t, &format!("Choose a duration between {min:.0} and {max:.0} seconds."));
        } else if edited {
            widgets::hint(ui, t, "Apply trim to update both exports and the preview.");
        }
        let mut applied = false;
        ui.horizontal_wrapped(|ui| {
            if widgets::button_ex(ui, t, None, "Apply trim", Kind::Secondary, Size::Medium, 0.0, ready && edited && valid).clicked() {
                app.m.action("clips.retrim", Value::map().with("id", id).with("in", a).with("out", b));
                applied = true;
            }
            if widgets::button_ex(ui, t, None, "Reset", Kind::Ghost, Size::Small, 0.0, edited).clicked() {
                (a, b) = (f(c, "in"), f(c, "out"));
            }
        });
        if applied {
            st.trims.remove(&id);
        } else {
            st.trims.insert(id, (version, a, b));
        }
        ui.add_space(spacing::L);
        ui.label(RichText::new("Export & share").font(font_semibold(type_scale::LARGE)).color(t.fg));
        widgets::hint(ui, t, "Portrait and landscape files are saved when the clip is made.");
        let path = clip_path(c, clip_orientation(c, st.detail_orientation));
        if widgets::button_ex(ui, t, None, "Show export folder", Kind::Secondary, Size::Medium, 0.0, !path.is_empty()).clicked()
            && let Some(parent) = std::path::Path::new(path).parent()
        {
            open(app, &parent.to_string_lossy());
        }
        if status == "approved" && widgets::button_ex(ui, t, Some(icon::UP), "Upload approved clip", Kind::Primary, Size::Medium, 0.0, ready)
            .on_hover_text("Send with your configured upload command").clicked()
        {
            app.m.action("clips.upload", Value::map().with("id", id));
        } else if status != "uploaded" && status != "approved" {
            widgets::hint(ui, t, "Approve this clip to enable uploading.");
        }
        if !s(c, "url").is_empty() && ui.link("Open uploaded clip").clicked() {
            open(app, s(c, "url"));
        }
    });
}

fn clip_context(ui: &mut Ui, t: &Theme, c: &Value) {
    widgets::panel(ui, t, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new("Transcript & context").font(font_semibold(type_scale::LARGE)).color(t.fg));
        ui.add_space(spacing::S);
        let captions = s(c, "captions");
        ui.add(egui::Label::new(RichText::new(if captions.is_empty() { "No transcript for this clip." } else { captions })
            .color(if captions.is_empty() { t.text_dim } else { t.fg })).wrap());
        ui.add_space(spacing::M);
        ui.horizontal_wrapped(|ui| {
            for reason in list(c, "reasons").iter().filter_map(Value::as_str) {
                widgets::badge(ui, t, &crate::views::live::nice(reason), t.modulated());
            }
            for label in list(c, "labels").iter().filter_map(Value::as_str) {
                widgets::badge(ui, t, &crate::views::live::nice(label), t.yellow);
            }
            if c.get_path("dmca_risk").is_some_and(Value::truthy) {
                widgets::badge(ui, t, "Review music rights", t.yellow).on_hover_text("This clip includes a requested song. Review its rights before uploading.");
            }
            if s(c, "kind") == "song" {
                widgets::badge(ui, t, "Music preserved", t.modulated());
            } else if c.get_path("music_dropped").is_some_and(Value::truthy) {
                widgets::badge(ui, t, "Separate music track omitted", t.green);
            } else {
                widgets::badge(ui, t, "May include music", t.yellow).on_hover_text("Music mixed into a selected track remains in the clip.");
            }
        });
        for (field, label) in [("song", "Song"), ("requester", "Requested by"), ("context.channel", "Channel"), ("context.video", "Source video")] {
            if !s(c, field).is_empty() {
                widgets::fact(ui, t, label, s(c, field));
            }
        }
        widgets::details(ui, t, ("clip-files", i(c, "id")), "Clip details", |ui| {
            widgets::fact(ui, t, "Rank", &format!("#{} · score {:.2}", i(c, "rank"), f(c, "score")));
            widgets::fact(ui, t, "Audio", s(c, "audio"));
            widgets::fact(ui, t, "Encoder", s(c, "encoder"));
            for field in ["wide", "tall"] {
                if !s(c, field).is_empty() {
                    ui.label(RichText::new(s(c, field)).font(font_mono(type_scale::SMALL)).color(t.text_dim));
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restarted_recording_clips_use_show_time_not_file_time() {
        let show = Value::map().with("t0_ns", 1_000_000_000_000i64);
        let clip = Value::map().with("in", 5.0).with("out", 17.0)
            .with("start_ns", 1_605_000_000_000i64).with("end_ns", 1_617_000_000_000i64);
        assert_eq!(clip_show_range(&clip, &show), (605.0, 617.0));
    }
}
