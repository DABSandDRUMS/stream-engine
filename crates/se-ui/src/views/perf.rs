//! Settings → Performance (§15.6, §21): a plain-words summary first ("Video: smooth · Sound: no
//! glitches · OBS: 0 dropped frames"), then the detail — per-pass GPU timings, frame times,
//! dropped/late frames, VRAM, audio xruns/DSP load, OBS encoder health, and this window's own cost
//! (frames transport, zero-readback counters, preview rates under GPU pressure).

use crate::app::App;
use crate::frames::Transport;
use egui::{Color32, CornerRadius, Rect, RichText, Ui, Vec2};
use se_proto::Value;
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_mono, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets;
use std::collections::VecDeque;

const HIST: usize = 300;
/// Engine GPU budget per frame (ms).
const GPU_BUDGET: f64 = 8.0;
/// Widest the page gets on very large screens.
const MAX_WIDTH: f32 = f32::INFINITY;

#[derive(Default)]
pub struct PerfState {
    pub gpu: VecDeque<f32>,
    pub frame: VecDeque<f32>,
    pub ui_ms: VecDeque<f32>,
    last_sample: Option<std::time::Instant>,
}

fn push(h: &mut VecDeque<f32>, v: f32) {
    if h.len() >= HIST {
        h.pop_front();
    }
    h.push_back(v);
}

/// Sample the histories (10 Hz, independent of which view is open).
pub fn sample(app: &mut App, ui_frame_ms: f32) {
    let due = app.perf.last_sample.is_none_or(|t| t.elapsed().as_millis() >= 100);
    if !due {
        return;
    }
    app.perf.last_sample = Some(std::time::Instant::now());
    let (g, f) = (app.m.f("perf.gpu_ms") as f32, app.m.f("perf.frame_ms") as f32);
    push(&mut app.perf.gpu, g);
    push(&mut app.perf.frame, f);
    push(&mut app.perf.ui_ms, ui_frame_ms);
}

/// A value for the detail tables (`—` when the engine doesn't report it).
fn opt(app: &App, a: &str, unit: &str) -> String {
    match app.m.get(a) {
        Some(Value::Float(f)) => format!("{f:.2}{unit}"),
        Some(v) if !v.is_null() => format!("{v}{unit}"),
        _ => "—".into(),
    }
}

/// Fixed-width, left-aligned label column for the detail rows.
fn key_label(ui: &mut Ui, t: &Theme, k: &str) {
    ui.allocate_ui_with_layout(Vec2::new(170.0, 18.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_width(170.0);
        ui.add(egui::Label::new(RichText::new(k).size(type_scale::SMALL + 0.5).color(t.text_dim)).truncate());
    });
}

/// Label/value row with a mono value.
fn kv(ui: &mut Ui, t: &Theme, k: &str, v: &str) {
    ui.horizontal(|ui| {
        key_label(ui, t, k);
        ui.label(RichText::new(v).font(font_mono(type_scale::SMALL + 0.5)).color(t.fg));
    });
}

/// Label, horizontal bar (green → yellow → red as it fills), value.
fn bar(ui: &mut Ui, t: &Theme, label: &str, v: f64, max: f64, text: &str) {
    ui.horizontal(|ui| {
        key_label(ui, t, label);
        let frac = if max > 0.0 { (v / max).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let fill = if frac > 0.9 {
            t.bright_red
        } else if frac > 0.7 {
            t.yellow
        } else {
            t.green
        };
        let w = (ui.available_width() - 100.0).max(80.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 8.0), egui::Sense::hover());
        let r = CornerRadius::same(radius::PILL);
        ui.painter().rect_filled(rect, r, t.inset);
        if frac > 0.0 {
            ui.painter().rect_filled(Rect::from_min_size(rect.min, Vec2::new((rect.width() * frac).max(8.0), rect.height())), r, fill);
        }
        ui.label(RichText::new(text).font(font_mono(type_scale::SMALL)).color(t.fg));
    });
}

/// Small caption above a chart.
fn caption(ui: &mut Ui, t: &Theme, text: &str) {
    ui.label(RichText::new(text).size(type_scale::SMALL).color(t.text_dim));
}

// ---- summary -------------------------------------------------------------------------------------

struct Stat {
    title: &'static str,
    value: String,
    line: String,
    color: Color32,
}

fn stats(app: &App, t: &Theme) -> [Stat; 4] {
    let m = &app.m;
    let gpu = m.f("perf.gpu_ms");
    let video = if !m.has("perf.gpu_ms") {
        Stat { title: "Video", value: "Not running".into(), line: "The picture engine isn't reporting.".into(), color: t.text_dim }
    } else {
        let fps = m.f("perf.fps");
        let (value, color) = if gpu > 11.0 {
            ("Struggling", t.bright_red)
        } else if gpu > GPU_BUDGET {
            ("Working hard", t.yellow)
        } else {
            ("Smooth", t.green)
        };
        Stat { title: "Video", value: value.into(), line: format!("{fps:.0} frames a second"), color }
    };
    let sound = if !m.has("perf.audio.load") && !m.has("perf.audio.xruns") {
        Stat { title: "Sound", value: "Not running".into(), line: "The sound engine isn't reporting.".into(), color: t.text_dim }
    } else {
        let xr = m.f("perf.audio.xruns") as i64;
        let load = m.f("perf.audio.load") * 100.0;
        let (value, color) = if xr == 0 { ("No glitches".to_string(), t.green) } else { (format!("{xr} glitches"), t.yellow) };
        Stat { title: "Sound", value, line: format!("{load:.0}% busy"), color }
    };
    let obs = if !m.has("obs.link") {
        Stat { title: "OBS", value: "Not linked".into(), line: "Stream Engine isn't talking to OBS.".into(), color: t.text_dim }
    } else if !m.b("obs.link") {
        Stat { title: "OBS", value: "Not open".into(), line: "Open OBS and it connects by itself.".into(), color: t.yellow }
    } else {
        let dropped = m.f("obs.stream.dropped") as i64;
        let kbps = m.f("obs.stream.kbps");
        let line = if kbps > 0.0 { format!("Sending {:.1} Mbps", kbps / 1000.0) } else { "Not streaming right now".into() };
        let color = if dropped > 0 { t.yellow } else { t.green };
        Stat { title: "OBS", value: format!("{dropped} dropped frames"), line, color }
    };
    let ui_ms = app.perf.ui_ms.iter().rev().take(20).copied().fold(0.0f32, f32::max);
    let pressure = app.gpu_pressure();
    let (value, color) = match (ui_ms > 25.0, pressure) {
        (_, 2) => ("Saving power", t.yellow),
        (true, _) => ("A bit slow", t.yellow),
        _ => ("Smooth", t.green),
    };
    let win = Stat { title: "This window", value: value.into(), line: format!("Previews at {:.0} a second", app.preview_hz()), color };
    [video, sound, obs, win]
}

fn summary(app: &App, ui: &mut Ui, t: &Theme) {
    let s = stats(app, t);
    let headline = s.iter().map(|x| format!("{}: {}", x.title, x.value.to_lowercase())).collect::<Vec<_>>().join(" · ");
    // the same list (and counts) as the top bar's health pill, plus frames OBS dropped
    let mut warns = crate::views::status::warnings(app);
    let dropped = app.m.f("obs.stream.dropped") as i64;
    if dropped > 0 {
        warns.push(crate::views::status::Warning { text: format!("OBS dropped {dropped} frames while sending."), fail: false, open: None, setup: false });
    }
    let fails = warns.iter().filter(|w| w.fail && !w.setup).count();
    let checks = warns.iter().filter(|w| !w.fail && !w.setup).count();
    let setup = warns.iter().filter(|w| w.setup).count();
    // same words as the top bar: "1 problem · 2 things to check"
    let mut parts = Vec::new();
    if fails > 0 {
        parts.push(if fails == 1 { "1 problem".to_string() } else { format!("{fails} problems") });
    }
    if checks > 0 {
        parts.push(if checks == 1 { "1 thing to check".to_string() } else { format!("{checks} things to check") });
    }
    let head = if parts.is_empty() { "Stream Engine is running smoothly".to_string() } else { parts.join(" · ") };
    widgets::panel(ui, t, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(head).font(font_semibold(type_scale::HEADING)).color(t.fg));
        if !headline.is_empty() {
            ui.label(RichText::new(headline).color(t.text_dim));
        }
        if setup > 0 {
            ui.label(
                RichText::new(if setup == 1 { "1 thing left to set up.".to_string() } else { format!("{setup} things left to set up.") }).color(t.text_dim),
            );
        }
        ui.add_space(spacing::XS);
        for w in warns.iter().take(10) {
            let (ic, c) = match (w.setup, w.fail) {
                (true, _) => (se_ui_kit::widgets::icon::INFO, t.accent),
                (false, true) => (se_ui_kit::widgets::icon::WARN, t.bright_red),
                (false, false) => (se_ui_kit::widgets::icon::WARN, t.yellow),
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(ic).color(c));
                ui.add(egui::Label::new(RichText::new(&w.text).color(t.fg)).truncate()).on_hover_text(&w.text);
            });
        }
        ui.add_space(spacing::M);
        let gap = spacing::M;
        let n = s.len() as f32;
        let w = ((ui.available_width() - gap * (n - 1.0)) / n).floor();
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for x in &s {
                egui::Frame::new().fill(t.surface_hi).corner_radius(radius::CONTROL).inner_margin(egui::Margin::symmetric(14, 12)).show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.set_width(w - 28.0);
                        ui.spacing_mut().item_spacing.y = 4.0;
                        ui.label(RichText::new(x.title).size(type_scale::SMALL + 0.5).color(t.text_dim));
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
                            ui.painter().circle_filled(r.center(), 5.0, x.color);
                            ui.label(RichText::new(&x.value).font(font_semibold(type_scale::LARGE)).color(t.fg));
                        });
                        ui.add(egui::Label::new(RichText::new(&x.line).size(type_scale::SMALL).color(t.text_dim)).truncate());
                    });
                });
            }
        });
    });
}

// ---- detail --------------------------------------------------------------------------------------

pub fn ui(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        // one width for the summary and the cards below it
        let w = ui.available_width().min(MAX_WIDTH);
        ui.allocate_ui_with_layout(Vec2::new(w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_width(w);
            summary(app, ui, &t);
            ui.add_space(spacing::L);
            let gaps = ui.spacing().item_spacing;
            ui.spacing_mut().item_spacing.x = spacing::L;
            let n = if ui.available_width() >= 2200.0 { 3 } else { 2 };
            ui.columns(n, |cols| {
                for c in cols.iter_mut() {
                    c.spacing_mut().item_spacing = gaps;
                }
                render(app, &mut cols[0], &t);
                audio(app, &mut cols[1], &t);
                obs(app, &mut cols[1], &t);
                ui_card(app, &mut cols[if n == 3 { 2 } else { 0 }], &t);
            });
            ui.spacing_mut().item_spacing = gaps;
            other(app, ui, &t);
            ui.add_space(spacing::XL);
        });
    });
}

fn render(app: &mut App, ui: &mut Ui, t: &Theme) {
    widgets::titled(
        ui,
        t,
        "Video engine",
        &format!("GPU time per frame; the budget is {GPU_BUDGET:.0} ms."),
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let gpu: Vec<f32> = app.perf.gpu.iter().copied().collect();
            let frame: Vec<f32> = app.perf.frame.iter().copied().collect();
            caption(ui, t, "GPU ms");
            widgets::scope(ui, t, Vec2::new(ui.available_width(), 64.0), &gpu, Some((0.0, 16.0)), Some(t.accent));
            ui.add_space(spacing::XS);
            caption(ui, t, "CPU frame build ms");
            widgets::scope(ui, t, Vec2::new(ui.available_width(), 40.0), &frame, Some((0.0, 16.0)), Some(t.cyan));
            ui.add_space(spacing::S);
            kv(ui, t, "GPU", &opt(app, "perf.gpu_ms", " ms"));
            kv(ui, t, "Frame build", &opt(app, "perf.frame_ms", " ms"));
            kv(ui, t, "Frames a second", &opt(app, "perf.fps", ""));
            kv(ui, t, "Dropped", &opt(app, "perf.dropped", ""));
            kv(ui, t, "Late", &opt(app, "perf.late", ""));
            if app.m.has("perf.vram_mb") {
                let budget = app.m.get("perf.vram_budget_mb").and_then(Value::as_f64).unwrap_or(3072.0);
                let mb = app.m.f("perf.vram_mb");
                bar(ui, t, "Video memory", mb, budget, &format!("{:.1} GB of {:.1} GB", mb / 1000.0, budget / 1000.0));
            }
            ui.add_space(spacing::S);
            caption(ui, t, "Passes");
            let mut passes: Vec<(String, f64)> = app
                .m
                .under("perf.pass")
                .filter_map(|(a, v)| Some((a.strip_prefix("perf.pass.")?.trim_end_matches("_ms").trim_end_matches(".gpu_ms").to_string(), v.as_f64()?)))
                .collect();
            passes.sort_by(|a, b| b.1.total_cmp(&a.1));
            if passes.is_empty() {
                widgets::hint(ui, t, "No per-pass timings (the video engine isn't running).");
            }
            for (n, ms) in passes {
                bar(ui, t, &n, ms, GPU_BUDGET, &format!("{ms:.2} ms"));
            }
        },
    );
    ui.add_space(spacing::L);
}

fn audio(app: &mut App, ui: &mut Ui, t: &Theme) {
    widgets::titled(
        ui,
        t,
        "Sound engine",
        "Glitches (xruns) are clicks or drop-outs in the sound.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if !app.m.has("perf.audio.load") && !app.m.has("perf.audio.xruns") {
                widgets::hint(ui, t, "The sound engine isn't running.");
                return;
            }
            let pct = app.m.f("perf.audio.load") * 100.0;
            bar(ui, t, "Busy", pct, 100.0, &format!("{pct:.0}%"));
            kv(ui, t, "Glitches (xruns)", &opt(app, "perf.audio.xruns", ""));
            kv(ui, t, "Callback", &opt(app, "perf.audio.dsp_ms", " ms"));
            kv(ui, t, "Buffer (quantum)", &opt(app, "perf.audio.quantum", ""));
            kv(ui, t, "Sample rate", &opt(app, "perf.audio.rate", " Hz"));
            kv(ui, t, "Latency", &opt(app, "perf.audio.latency_ms", " ms"));
        },
    );
    ui.add_space(spacing::L);
}

fn obs(app: &mut App, ui: &mut Ui, t: &Theme) {
    widgets::titled(
        ui,
        t,
        "OBS",
        "What OBS reports about drawing, encoding and sending your stream.",
        |ui| {
            if app.m.has("obs.link") {
                let on = app.m.b("obs.link");
                widgets::badge(ui, t, if on { "Working" } else { "Not connected" }, if on { t.green } else { t.yellow });
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            if !app.m.has("obs.link") {
                widgets::hint(ui, t, "The OBS link isn't running.");
                return;
            }
            kv(ui, t, "Render fps", &opt(app, "obs.fps", ""));
            kv(ui, t, "Render time", &opt(app, "obs.render.ms", " ms"));
            kv(ui, t, "Render lagged", &opt(app, "obs.render.lagged", ""));
            kv(ui, t, "Encoder skipped", &opt(app, "obs.encode.skipped", ""));
            kv(ui, t, "Stream kbps", &opt(app, "obs.stream.kbps", ""));
            kv(ui, t, "Stream dropped", &opt(app, "obs.stream.dropped", ""));
            kv(ui, t, "Congestion", &opt(app, "obs.stream.congestion", ""));
            kv(ui, t, "Lag", &opt(app, "obs.stream.lag_ms", " ms"));
            let mut ids: Vec<String> =
                app.m.under("obs.output").filter_map(|(a, _)| a.strip_prefix("obs.output.")?.strip_suffix(".label").map(String::from)).collect();
            ids.dedup();
            if !ids.is_empty() {
                ui.add_space(spacing::S);
                caption(ui, t, "Outputs");
            }
            for id in ids {
                let p = format!("obs.output.{id}");
                let active = app.m.b(&format!("{p}.active"));
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
                    ui.painter().circle_filled(r.center(), 4.0, if active { t.green } else { t.text_faint });
                    ui.label(RichText::new(app.m.str(&format!("{p}.label"))).color(t.fg));
                    ui.label(
                        RichText::new(format!(
                            "{} · {} · {:.0} kbps · {} dropped of {}",
                            app.m.str(&format!("{p}.kind")),
                            app.m.str(&format!("{p}.canvas")),
                            app.m.f(&format!("{p}.kbps")),
                            app.m.f(&format!("{p}.dropped")),
                            app.m.f(&format!("{p}.total")),
                        ))
                        .font(font_mono(type_scale::SMALL))
                        .color(t.text_dim),
                    );
                });
            }
        },
    );
    ui.add_space(spacing::L);
}

fn ui_card(app: &mut App, ui: &mut Ui, t: &Theme) {
    let st = app.frames.stats();
    widgets::titled(
        ui,
        t,
        "This window",
        "How much work the Stream Engine window itself does.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let ms: Vec<f32> = app.perf.ui_ms.iter().copied().collect();
            caption(ui, t, "Window frame ms (CPU)");
            widgets::scope(ui, t, Vec2::new(ui.available_width(), 40.0), &ms, Some((0.0, 20.0)), Some(t.magenta));
            ui.add_space(spacing::S);
            let (pv, pg, at) = (app.preview_hz(), app.program_hz(), app.atlas_hz());
            kv(ui, t, "GPU pressure level", &app.gpu_pressure().to_string());
            kv(ui, t, "Refresh rates", &format!("preview {pv:.0} Hz · program {pg:.0} Hz · multiview {at:.0} Hz"));
            kv(ui, t, "Video link", &format!("{} ({})", if st.connected { "connected" } else { "not connected" }, st.socket.display()));
            ui.add_space(spacing::S);
            egui::Grid::new("frames-kv").num_columns(6).spacing([16.0, 4.0]).show(ui, |ui| {
                for h in ["Canvas", "Transport", "Size", "Gen", "Shown", "Age"] {
                    ui.label(RichText::new(h).size(type_scale::SMALL).color(t.text_dim));
                }
                ui.end_row();
                let cell = |ui: &mut Ui, s: String, c: Color32| {
                    ui.label(RichText::new(s).font(font_mono(type_scale::SMALL)).color(c));
                };
                for (i, c) in st.canvases.iter().enumerate() {
                    cell(ui, crate::frames::Canvas::from_index(i).map(|c| c.name()).unwrap_or("?").to_string(), t.fg);
                    let (tr, col) = match c.transport {
                        Transport::Dmabuf => ("dmabuf", t.green),
                        Transport::Shm => ("shm", t.yellow),
                        Transport::None => ("—", t.text_dim),
                    };
                    cell(ui, tr.to_string(), col);
                    cell(ui, format!("{}×{}", c.width, c.height), t.fg);
                    cell(ui, c.generation.to_string(), t.fg);
                    cell(ui, c.frames_presented.to_string(), t.fg);
                    cell(ui, c.age_ms.map(|a| format!("{a:.0} ms")).unwrap_or_else(|| "—".into()), t.fg);
                    ui.end_row();
                }
            });
            ui.add_space(spacing::S);
            kv(ui, t, "dmabuf buffers imported", &st.dmabuf_imports.to_string());
            kv(ui, t, "dmabuf frames shown", &st.dmabuf_frames_presented.to_string());
            kv(ui, t, "shm frames shown", &st.shm_frames_presented.to_string());
            kv(ui, t, "shm bytes uploaded", &st.shm_bytes_uploaded.to_string());
            kv(ui, t, "GPU→CPU readback", "0 bytes (no readback path exists)");
            kv(ui, t, "Releases sent", &st.releases_sent.to_string());
            kv(ui, t, "Reconnects", &st.reconnects.to_string());
            if let Some(e) = &st.last_error {
                ui.label(RichText::new(e).size(type_scale::SMALL).color(t.yellow));
            }
        },
    );
    ui.add_space(spacing::L);
}

fn other(app: &mut App, ui: &mut Ui, t: &Theme) {
    let known = ["perf.gpu_ms", "perf.frame_ms", "perf.fps", "perf.dropped", "perf.late", "perf.vram_mb", "perf.vram_budget_mb"];
    let rows: Vec<(String, String)> = app
        .m
        .under("perf")
        .filter(|(a, _)| !known.contains(&a.as_str()) && !a.starts_with("perf.pass.") && !a.starts_with("perf.audio."))
        .map(|(a, v)| (a.clone(), v.to_string()))
        .collect();
    if rows.is_empty() {
        return;
    }
    widgets::titled(
        ui,
        t,
        "Threads and other counters",
        "Everything else the engine measures.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let half = rows.len().div_ceil(2);
            ui.columns(2, |cols| {
                for (i, (a, v)) in rows.iter().enumerate() {
                    let col = &mut cols[usize::from(i >= half)];
                    col.horizontal(|ui| {
                        ui.label(RichText::new(a.trim_start_matches("perf.")).font(font_mono(type_scale::SMALL)).color(t.text_dim));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(spacing::XL);
                            ui.label(RichText::new(v).font(font_mono(type_scale::SMALL)).color(mix(t.fg, t.text_dim, 0.2)));
                        });
                    });
                }
            });
        },
    );
}
