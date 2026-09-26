//! Performance view (§15.6, §21): per-pass GPU timings, frame times, dropped/late frames,
//! VRAM, audio xruns/DSP load, OBS encoder health, and the UI's own cost (frames transport,
//! zero-readback counters, preview rates under GPU pressure).

use crate::app::App;
use crate::frames::Transport;
use egui::{RichText, Vec2};
use se_proto::Value;
use se_ui_kit::widgets::{self, LedState, icon};
use std::collections::VecDeque;

const HIST: usize = 300;

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

fn kv(ui: &mut egui::Ui, k: &str, v: String) {
    ui.label(RichText::new(k).small());
    ui.label(RichText::new(v).monospace());
    ui.end_row();
}

fn opt(app: &App, a: &str, unit: &str) -> String {
    match app.m.get(a) {
        Some(Value::Float(f)) => format!("{f:.2}{unit}"),
        Some(v) if !v.is_null() => format!("{v}{unit}"),
        _ => "—".into(),
    }
}

fn bar(ui: &mut egui::Ui, t: &se_ui_kit::Theme, label: &str, v: f64, max: f64, unit: &str) {
    ui.horizontal(|ui| {
        ui.add_sized([150.0, 16.0], egui::Label::new(RichText::new(label).small().monospace()));
        let frac = if max > 0.0 { (v / max).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let fill = if frac > 0.9 {
            t.bright_red
        } else if frac > 0.7 {
            t.yellow
        } else {
            t.green
        };
        ui.add(egui::ProgressBar::new(frac).desired_width(220.0).fill(fill).text(format!("{v:.2}{unit}")));
    });
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::section(ui, &t, icon::PERF, "Performance");
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.columns(2, |cols| {
            render(app, &mut cols[0]);
            right(app, &mut cols[1]);
        });
        ui.separator();
        other(app, ui);
    });
}

fn render(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::card(ui, &t, icon::GPU, "Render (engine)", if app.m.f("perf.gpu_ms") > 8.0 { LedState::Armed } else { LedState::Healthy }, |ui| {
        let gpu: Vec<f32> = app.perf.gpu.iter().copied().collect();
        let frame: Vec<f32> = app.perf.frame.iter().copied().collect();
        ui.label(RichText::new("GPU ms (budget 8 ms)").small());
        widgets::scope(ui, &t, Vec2::new(ui.available_width(), 60.0), &gpu, Some((0.0, 16.0)), Some(t.accent));
        ui.label(RichText::new("CPU frame build ms").small());
        widgets::scope(ui, &t, Vec2::new(ui.available_width(), 40.0), &frame, Some((0.0, 16.0)), Some(t.cyan));
        egui::Grid::new("render-kv").num_columns(2).show(ui, |ui| {
            kv(ui, "gpu", opt(app, "perf.gpu_ms", " ms"));
            kv(ui, "frame", opt(app, "perf.frame_ms", " ms"));
            kv(ui, "fps", opt(app, "perf.fps", ""));
            kv(ui, "dropped", opt(app, "perf.dropped", ""));
            kv(ui, "late", opt(app, "perf.late", ""));
        });
        let vram = app.m.f("perf.vram_mb");
        let budget = app.m.get("perf.vram_budget_mb").and_then(Value::as_f64).unwrap_or(3072.0);
        if app.m.has("perf.vram_mb") {
            bar(ui, &t, "VRAM (engine budget)", vram, budget, " MB");
        }
        ui.label(RichText::new("passes").small().strong());
        let mut passes: Vec<(String, f64)> = app
            .m
            .under("perf.pass")
            .filter_map(|(a, v)| Some((a.strip_prefix("perf.pass.")?.trim_end_matches("_ms").trim_end_matches(".gpu_ms").to_string(), v.as_f64()?)))
            .collect();
        passes.sort_by(|a, b| b.1.total_cmp(&a.1));
        if passes.is_empty() {
            ui.label(RichText::new("no per-pass timings (renderer not running)").small().color(t.fg_dim));
        }
        for (n, ms) in passes {
            bar(ui, &t, &n, ms, 8.0, " ms");
        }
    });
}

fn right(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let xr = app.m.f("perf.audio.xruns");
    widgets::card(ui, &t, icon::MIX, "Audio", if xr > 0.0 { LedState::Armed } else { LedState::Healthy }, |ui| {
        if !app.m.has("perf.audio.load") && !app.m.has("perf.audio.xruns") {
            ui.label(RichText::new("audio engine not running").small().color(t.fg_dim));
            return;
        }
        bar(ui, &t, "DSP load", app.m.f("perf.audio.load") * 100.0, 100.0, " %");
        egui::Grid::new("audio-kv").num_columns(2).show(ui, |ui| {
            kv(ui, "xruns", opt(app, "perf.audio.xruns", ""));
            kv(ui, "callback", opt(app, "perf.audio.dsp_ms", " ms"));
            kv(ui, "quantum", opt(app, "perf.audio.quantum", ""));
            kv(ui, "rate", opt(app, "perf.audio.rate", " Hz"));
            kv(ui, "latency", opt(app, "perf.audio.latency_ms", " ms"));
        });
    });
    let obs_ok = app.m.b("obs.link");
    widgets::card(
        ui,
        &t,
        icon::REC,
        "OBS encoder",
        if !obs_ok {
            LedState::Idle
        } else if app.m.f("obs.stream.dropped") > 0.0 {
            LedState::Armed
        } else {
            LedState::Healthy
        },
        |ui| {
            if !app.m.has("obs.link") {
                ui.label(RichText::new("OBS link not running").small().color(t.fg_dim));
                return;
            }
            egui::Grid::new("obs-kv").num_columns(2).show(ui, |ui| {
                kv(ui, "plugin", if obs_ok { "connected".into() } else { "disconnected".into() });
                kv(ui, "render fps", opt(app, "obs.fps", ""));
                kv(ui, "render ms", opt(app, "obs.render.ms", " ms"));
                kv(ui, "render lagged", opt(app, "obs.render.lagged", ""));
                kv(ui, "encoder skipped", opt(app, "obs.encode.skipped", ""));
                kv(ui, "stream kbps", opt(app, "obs.stream.kbps", ""));
                kv(ui, "stream dropped", opt(app, "obs.stream.dropped", ""));
                kv(ui, "congestion", opt(app, "obs.stream.congestion", ""));
                kv(ui, "lag", opt(app, "obs.stream.lag_ms", " ms"));
            });
            let mut ids: Vec<String> =
                app.m.under("obs.output").filter_map(|(a, _)| a.strip_prefix("obs.output.")?.strip_suffix(".label").map(String::from)).collect();
            ids.dedup();
            for id in ids {
                let p = format!("obs.output.{id}");
                ui.label(
                    RichText::new(format!(
                        "{} {} ({}, {}): {:.0} kbps, {} dropped / {}",
                        if app.m.b(&format!("{p}.active")) { "●" } else { "○" },
                        app.m.str(&format!("{p}.label")),
                        app.m.str(&format!("{p}.kind")),
                        app.m.str(&format!("{p}.canvas")),
                        app.m.f(&format!("{p}.kbps")),
                        app.m.f(&format!("{p}.dropped")),
                        app.m.f(&format!("{p}.total")),
                    ))
                    .small()
                    .monospace(),
                );
            }
        },
    );
    ui_card(app, ui);
}

fn ui_card(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let st = app.frames.stats();
    widgets::card(ui, &t, icon::SCENE, "UI (this window)", if st.connected { LedState::Healthy } else { LedState::Idle }, |ui| {
        let ms: Vec<f32> = app.perf.ui_ms.iter().copied().collect();
        ui.label(RichText::new("UI frame ms (CPU)").small());
        widgets::scope(ui, &t, Vec2::new(ui.available_width(), 36.0), &ms, Some((0.0, 20.0)), Some(t.magenta));
        let (pv, pg, at) = (app.preview_hz(), app.program_hz(), app.atlas_hz());
        ui.label(RichText::new(format!("GPU pressure level {} → preview {pv:.0} Hz, program {pg:.0} Hz, multiview {at:.0} Hz", app.gpu_pressure())).small());
        ui.label(
            RichText::new(format!("frames.sock {} ({})", if st.connected { "connected" } else { "not connected" }, st.socket.display())).small().monospace(),
        );
        egui::Grid::new("frames-kv").num_columns(6).striped(true).show(ui, |ui| {
            for h in ["canvas", "transport", "size", "gen", "presented", "age"] {
                ui.label(RichText::new(h).small().strong());
            }
            ui.end_row();
            for (i, c) in st.canvases.iter().enumerate() {
                ui.label(RichText::new(crate::frames::Canvas::from_index(i).map(|c| c.name()).unwrap_or("?")).small());
                ui.label(
                    RichText::new(match c.transport {
                        Transport::Dmabuf => "dmabuf",
                        Transport::Shm => "shm",
                        Transport::None => "—",
                    })
                    .small()
                    .color(if c.transport == Transport::Dmabuf { t.green } else { t.fg_dim }),
                );
                ui.label(RichText::new(format!("{}×{}", c.width, c.height)).small().monospace());
                ui.label(RichText::new(format!("{}", c.generation)).small().monospace());
                ui.label(RichText::new(format!("{}", c.frames_presented)).small().monospace());
                ui.label(RichText::new(c.age_ms.map(|a| format!("{a:.0} ms")).unwrap_or_else(|| "—".into())).small().monospace());
                ui.end_row();
            }
        });
        egui::Grid::new("frames-totals").num_columns(2).show(ui, |ui| {
            kv(ui, "dmabuf buffers imported", st.dmabuf_imports.to_string());
            kv(ui, "dmabuf frames presented", st.dmabuf_frames_presented.to_string());
            kv(ui, "shm frames presented", st.shm_frames_presented.to_string());
            kv(ui, "shm bytes uploaded", st.shm_bytes_uploaded.to_string());
            kv(ui, "GPU→CPU readback bytes", "0 (no readback path exists)".into());
            kv(ui, "releases sent", st.releases_sent.to_string());
            kv(ui, "reconnects", st.reconnects.to_string());
        });
        if let Some(e) = &st.last_error {
            ui.label(RichText::new(e).small().color(t.yellow));
        }
    });
}

fn other(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
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
    widgets::section(ui, &t, icon::PERF, "Threads and other counters");
    egui::Grid::new("perf-other").striped(true).num_columns(2).show(ui, |ui| {
        for (a, v) in rows {
            kv(ui, &a, v);
        }
    });
}
