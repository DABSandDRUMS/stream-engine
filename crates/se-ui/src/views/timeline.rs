//! Timeline editor (§2.7, §9.2, §15.5): per-timeline transport with timecode, record mode and
//! tap, and a track editor over the media analysis (waveform, beat grid, sections, chorus):
//! cue markers, automation curves, and regions. Pure client of the `timelines` /
//! `timeline.grid` queries, `timeline.<name>.*` state, and `timeline.*` actions; edits go through
//! `timeline.edit.*` (the engine rewrites the TOML and reloads the project).

use crate::app::App;
use crate::model::Model;
use egui::{Align2, CornerRadius, CursorIcon, FontId, Id, Key, Modifiers, Pos2, Rect, RichText, Sense, Shape, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};
use se_core::timeline::{format_time, value_text};
use se_proto::{Ease, Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};
use se_ui_kit::{Theme, spacing, type_scale};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const LIST_W: f32 = 230.0;
/// Track header gutter left of the lanes (ruler and strip share it).
const HEAD_W: f32 = 160.0;
const RULER_H: f32 = 26.0;
const STRIP_H: f32 = 60.0;
const CUE_H: f32 = 36.0;
const LANE_H: f32 = 80.0;
const REGION_H: f32 = 38.0;
const MIN_PPS: f32 = 0.5;
const MAX_PPS: f32 = 4000.0;
/// `timelines` refresh while idle / while any timeline plays (region activity, status).
const LIST_EVERY: Duration = Duration::from_secs(1);
const LIST_FAST: Duration = Duration::from_millis(250);
/// After an action, re-query this soon (the engine applies actions asynchronously).
const AFTER_ACTION: Duration = Duration::from_millis(150);
/// The grid can appear later (analysis finishes in the background).
const GRID_EVERY: Duration = Duration::from_secs(5);
/// A released drag keeps its preview until the reloaded project arrives (or this long).
const GHOST_HOLD: Duration = Duration::from_secs(3);
/// Longest stretch the playhead is extrapolated between state updates.
const PLAYHEAD_COAST: f64 = 1.0;
const CUE_FLASH: Duration = Duration::from_millis(450);
const EDGE_PX: f32 = 6.0;
const DEFAULT_REGION: f64 = 4.0;
/// Waveform bin width of `timeline.grid` `peaks`.
const PEAK_BIN: f64 = 0.01;
const CURVES: [&str; 10] = ["linear", "in_quad", "out_quad", "in_out_quad", "in_cubic", "out_cubic", "in_out_cubic", "smoothstep", "out_back", "step"];
const FPS_CHOICES: [&str; 4] = ["24", "25", "29.97df", "30"];
const SOURCE_KINDS: [&str; 5] = ["internal", "manual", "media", "mtc", "ltc"];

const PAUSE: &str = "\u{f04c}";
const TO_START: &str = "\u{f048}";
const BACK: &str = "\u{f04a}";
const FORWARD: &str = "\u{f04e}";
const PLUS: &str = "\u{f067}";
const MINUS: &str = "\u{f068}";
const FIT: &str = "\u{f065}";
const FOLLOW: &str = "\u{f05b}";
const SNAP: &str = "\u{f076}";
const TAP: &str = "\u{f25a}";
const CUE_ICON: &str = "\u{f024}";
const REGION_ICON: &str = "\u{f0c8}";
const MUTED: &str = "\u{f026}";
const UNMUTED: &str = "\u{f028}";
const TRASH: &str = "\u{f1f8}";
const EDIT: &str = "\u{f040}";
const LOOP: &str = "\u{f01e}";

// ---------------------------------------------------------------- pure helpers

/// Pixel ↔ time mapping of the lanes: `x0` is the screen x of `origin` seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Axis {
    x0: f32,
    origin: f64,
    pps: f32,
}

impl Axis {
    fn x(&self, t: f64) -> f32 {
        self.x0 + ((t - self.origin) * self.pps as f64) as f32
    }

    fn t(&self, x: f32) -> f64 {
        self.origin + ((x - self.x0) / self.pps) as f64
    }

    /// Zoom by `factor` keeping the time under `anchor_x` fixed (origin never goes below 0).
    fn zoomed(self, anchor_x: f32, factor: f32) -> Axis {
        let at = self.t(anchor_x);
        let pps = (self.pps * factor).clamp(MIN_PPS, MAX_PPS);
        Axis { pps, origin: (at - ((anchor_x - self.x0) / pps) as f64).max(0.0), ..self }
    }

    /// Pan by a pointer/wheel delta in pixels (content follows the pointer).
    fn scrolled(self, dx_px: f32) -> Axis {
        Axis { origin: (self.origin - (dx_px / self.pps) as f64).max(0.0), ..self }
    }
}

/// Keep the playhead in view while following: page when it leaves the left 90 %.
fn follow_origin(origin: f64, visible: f64, playhead: f64) -> f64 {
    if playhead < origin || playhead > origin + visible * 0.9 { (playhead - visible * 0.1).max(0.0) } else { origin }
}

/// Seconds per pixel density that shows `span` seconds in `width` pixels (with a margin).
fn fit_pps(span: f64, width: f32) -> f32 {
    (width / (span.max(1.0) * 1.05) as f32).clamp(MIN_PPS, MAX_PPS)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum Snap {
    #[default]
    Off,
    Beat,
    Bar,
}

impl Snap {
    const ALL: [(Snap, &'static str); 3] = [(Snap::Off, "off"), (Snap::Beat, "beat"), (Snap::Bar, "bar")];

    fn parse(s: &str) -> Snap {
        match s {
            "beat" => Snap::Beat,
            "bar" => Snap::Bar,
            _ => Snap::Off,
        }
    }
}

/// Nearest value of a sorted slice.
fn nearest(sorted: &[f64], t: f64) -> Option<f64> {
    let i = sorted.partition_point(|&b| b < t);
    let before = i.checked_sub(1).and_then(|j| sorted.get(j));
    [before, sorted.get(i)].into_iter().flatten().copied().min_by(|a, b| (a - t).abs().total_cmp(&(b - t).abs()))
}

/// Snap `t` to the nearest beat / downbeat of the grid (unchanged without a grid or when off).
fn snap_time(t: f64, snap: Snap, grid: Option<&Grid>) -> f64 {
    let t = t.max(0.0);
    let points = match (snap, grid) {
        (Snap::Beat, Some(g)) => &g.beats[..],
        (Snap::Bar, Some(g)) => &g.downbeats[..],
        _ => return t,
    };
    nearest(points, t).unwrap_or(t)
}

/// The downbeat one bar after / before `t` (a tolerance lets repeated presses keep stepping while playing).
fn bar_target(downbeats: &[f64], t: f64, forward: bool) -> Option<f64> {
    if forward { downbeats.iter().copied().find(|&d| d > t + 1e-3) } else { downbeats.iter().rev().copied().find(|&d| d < t - 0.05) }
}

/// The slice of a sorted list inside `t0..=t1`.
fn visible(sorted: &[f64], t0: f64, t1: f64) -> &[f64] {
    let a = sorted.partition_point(|&v| v < t0);
    let b = sorted.partition_point(|&v| v <= t1);
    &sorted[a..b.max(a)]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rate {
    R24,
    R25,
    R2997Df,
    R30,
}

impl Rate {
    fn parse(s: &str) -> Rate {
        match s {
            "24" => Rate::R24,
            "25" => Rate::R25,
            "29.97df" | "29.97" => Rate::R2997Df,
            _ => Rate::R30,
        }
    }

    fn nominal(self) -> u64 {
        match self {
            Rate::R24 => 24,
            Rate::R25 => 25,
            Rate::R2997Df | Rate::R30 => 30,
        }
    }

    fn fps(self) -> f64 {
        if self == Rate::R2997Df { 30000.0 / 1001.0 } else { self.nominal() as f64 }
    }

    fn label(self) -> &'static str {
        match self {
            Rate::R24 => "24",
            Rate::R25 => "25",
            Rate::R2997Df => "29.97df",
            Rate::R30 => "30",
        }
    }
}

/// `hh:mm:ss:ff` of the frame containing `secs` (drop-frame labels with `;`, like `se_clock`).
fn timecode(secs: f64, rate: Rate) -> String {
    let fps = rate.nominal();
    let df = rate == Rate::R2997Df;
    let mut frames = (secs.max(0.0) * rate.fps() + 1e-6).floor() as u64;
    frames %= if df { 24 * 107_892 } else { fps * 86_400 };
    if df {
        let (d, m) = (frames / 17_982, frames % 17_982);
        frames += 18 * d + if m < 2 { 0 } else { 2 * ((m - 2) / 1798) };
    }
    let sep = if df { ';' } else { ':' };
    format!("{:02}:{:02}:{:02}{sep}{:02}", frames / (fps * 3600) % 24, frames / (fps * 60) % 60, frames / fps % 60, frames % fps)
}

/// Ruler tick spacing (seconds) giving at least `min_px` between ticks. In timecode mode the
/// steps are whole frames so every tick lands on a frame label.
fn tick_step(pps: f32, min_px: f32, tc: Option<Rate>) -> f64 {
    const SECS: [f64; 19] = [0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0];
    const FRAMES: [u64; 4] = [1, 2, 5, 10];
    const TC_SECS: [u64; 13] = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600];
    let fits = |s: f64| s * pps as f64 >= min_px as f64;
    match tc {
        None => SECS.into_iter().find(|&s| fits(s)).unwrap_or(3600.0),
        Some(r) => {
            let frames = FRAMES.into_iter().chain(TC_SECS.into_iter().map(|s| s * r.nominal()));
            let steps: Vec<f64> = frames.map(|f| f as f64 / r.fps()).collect();
            steps.iter().copied().find(|&s| fits(s)).unwrap_or(steps[steps.len() - 1])
        }
    }
}

/// Seconds ruler label with as many decimals as the tick step needs (`1:05`, `0:05.5`, `1:02:00`).
fn ruler_label(t: f64, step: f64) -> String {
    let decimals: u32 = if step >= 1.0 - 1e-9 {
        0
    } else if step >= 0.1 - 1e-9 {
        1
    } else if step >= 0.01 - 1e-9 {
        2
    } else {
        3
    };
    let scale = 10u64.pow(decimals);
    let units = (t.max(0.0) * scale as f64).round() as u64;
    let (whole, frac) = (units / scale, units % scale);
    let (h, m, s) = (whole / 3600, whole / 60 % 60, whole % 60);
    let base = if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") };
    if decimals == 0 { base } else { format!("{base}.{frac:0w$}", w = decimals as usize) }
}

/// Parse an editor time: seconds (`12.5`), `m:ss(.fff)`, or `h:mm:ss(.fff)`.
fn parse_secs(s: &str) -> Option<f64> {
    let s = s.trim().trim_end_matches('s');
    if s.is_empty() {
        return None;
    }
    let mut total = 0.0;
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    for (i, p) in parts.iter().enumerate() {
        let v: f64 = p.trim().parse().ok()?;
        if !v.is_finite() || v < 0.0 || (i > 0 && v >= 60.0) {
            return None;
        }
        total = total * 60.0 + v;
    }
    Some(total)
}

/// Evaluate numeric keys `(at, value, curve)` like the engine's automation lanes: hold outside
/// the keyed span, the left key's curve shapes each segment.
fn eval_curve(keys: &[(f64, f64, Ease)], t: f64) -> Option<f64> {
    let first = keys.first()?;
    if t <= first.0 {
        return Some(first.1);
    }
    let last = keys.last()?;
    if t >= last.0 {
        return Some(last.1);
    }
    let i = keys.partition_point(|k| k.0 <= t) - 1;
    let (a, b) = (keys[i], keys[i + 1]);
    let span = b.0 - a.0;
    if span <= 0.0 {
        return Some(b.1);
    }
    Some(a.1 + (b.1 - a.1) * a.2.apply((t - a.0) / span))
}

fn ease(name: &str) -> Ease {
    Ease::parse(name).unwrap_or_default()
}

/// Plot range of an automation lane: 0–1 when every value fits, else min..max padded 10 %.
fn value_range(values: impl Iterator<Item = f64>) -> (f64, f64) {
    let (lo, hi) = values.filter(|v| v.is_finite()).fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| (lo.min(v), hi.max(v)));
    if !lo.is_finite() || (lo >= 0.0 && hi <= 1.0) {
        return (0.0, 1.0);
    }
    let pad = if hi > lo { (hi - lo) * 0.1 } else { lo.abs().max(1.0) * 0.5 };
    (lo - pad, hi + pad)
}

/// Displayed playhead: the last reported time extrapolated (bounded) while playing.
fn playhead(base: f64, elapsed: f64, playing: bool, speed: f64, length: Option<f64>) -> f64 {
    let t = if playing { base + elapsed.clamp(0.0, PLAYHEAD_COAST) * speed } else { base };
    match length {
        Some(l) if l > 0.0 => t.clamp(0.0, l),
        _ => t.max(0.0),
    }
}

/// Loudest waveform bin covering `t0..t1` (sampled with a stride when zoomed far out).
fn peak_span(peaks: &[f32], t0: f64, t1: f64) -> f32 {
    if peaks.is_empty() || t1 < 0.0 {
        return 0.0;
    }
    let i0 = (t0.max(0.0) / PEAK_BIN).floor() as usize;
    if i0 >= peaks.len() {
        return 0.0;
    }
    let i1 = ((t1 / PEAK_BIN).ceil() as usize).clamp(i0 + 1, peaks.len());
    let stride = ((i1 - i0) / 64).max(1);
    peaks[i0..i1].iter().step_by(stride).copied().fold(0.0, f32::max)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Start,
    End,
    Body,
}

/// New `(start, end)` of a region dragged by `part`; `grab` is the pointer time at drag start.
fn drag_region(start: f64, end: f64, part: Part, pointer: f64, grab: f64, snap: &dyn Fn(f64) -> f64) -> (f64, f64) {
    const MIN_LEN: f64 = 0.05;
    match part {
        Part::Start => (snap(pointer).clamp(0.0, (end - MIN_LEN).max(0.0)), end),
        Part::End => (start, snap(pointer).max(start + MIN_LEN)),
        Part::Body => {
            let new_start = snap((start + pointer - grab).max(0.0));
            (new_start, new_start + (end - start))
        }
    }
}

fn status_led(status: &str) -> LedState {
    match status {
        "locked" | "playing" => LedState::Healthy,
        "locking" | "freewheel" => LedState::Armed,
        "lost" => LedState::Error,
        _ => LedState::Idle,
    }
}

// ---------------------------------------------------------------- data

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn opt_s(v: &Value, k: &str) -> Option<String> {
    v.get_path(k).and_then(Value::as_str).filter(|x| !x.is_empty()).map(String::from)
}

fn num(v: &Value, k: &str) -> f64 {
    v.get_path(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn flag(v: &Value, k: &str) -> bool {
    v.get_path(k).is_some_and(Value::truthy)
}

fn list<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get_path(k).and_then(Value::as_list).unwrap_or(&[])
}

fn strings(v: &Value, k: &str) -> Vec<String> {
    match v.get_path(k) {
        Some(Value::Str(x)) => vec![x.clone()],
        Some(Value::List(l)) => l.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    }
}

fn sorted_floats(v: &Value, k: &str) -> Vec<f64> {
    let mut out: Vec<f64> = list(v, k).iter().filter_map(Value::as_f64).filter(|x| x.is_finite()).collect();
    out.sort_by(f64::total_cmp);
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceKind {
    Internal,
    Manual,
    Media,
    Mtc,
    Ltc,
}

impl SourceKind {
    fn chased(self) -> bool {
        matches!(self, SourceKind::Media | SourceKind::Mtc | SourceKind::Ltc)
    }
}

#[derive(Clone, Debug)]
struct Cue {
    at: f64,
    label: Option<String>,
    commands: Vec<String>,
    mode: String,
}

#[derive(Clone, Debug)]
struct KeyPt {
    at: f64,
    value: Value,
    curve: String,
}

#[derive(Clone, Debug)]
enum RegionAction {
    Preset(String),
    Cuelist(String, Option<String>),
    Fx(String),
    Commands(Vec<String>, Vec<String>),
}

impl RegionAction {
    fn parse(a: &Value) -> RegionAction {
        match s(a, "kind") {
            "preset" => RegionAction::Preset(s(a, "preset").into()),
            "cuelist" => RegionAction::Cuelist(s(a, "cuelist").into(), opt_s(a, "cue")),
            "fx" => RegionAction::Fx(s(a, "fx").into()),
            _ => RegionAction::Commands(strings(a, "on"), strings(a, "off")),
        }
    }

    fn describe(&self) -> (&'static str, String) {
        match self {
            RegionAction::Preset(p) => (icon::PRESET, format!("preset {p}")),
            RegionAction::Cuelist(c, Some(q)) => (icon::LIGHT, format!("cuelist {c} · {q}")),
            RegionAction::Cuelist(c, None) => (icon::LIGHT, format!("cuelist {c}")),
            RegionAction::Fx(f) => (icon::EFFECT, format!("fx {f}")),
            RegionAction::Commands(on, _) => {
                let first = on.first().cloned().unwrap_or_else(|| "commands".into());
                (icon::CONSOLE, if on.len() > 1 { format!("{first} +{}", on.len() - 1) } else { first })
            }
        }
    }
}

#[derive(Clone, Debug)]
struct Region {
    start: f64,
    end: f64,
    label: Option<String>,
    action: RegionAction,
}

#[derive(Clone, Debug)]
enum TrackKind {
    Cues(Vec<Cue>),
    Automation { address: String, keys: Vec<KeyPt> },
    Regions(Vec<Region>),
}

#[derive(Clone, Debug)]
struct Track {
    name: String,
    mute: bool,
    kind: TrackKind,
}

impl Track {
    fn parse(v: &Value) -> Option<Track> {
        let name = opt_s(v, "name")?;
        let kind = match s(v, "type") {
            "cues" => TrackKind::Cues(
                list(v, "cues")
                    .iter()
                    .map(|c| Cue { at: num(c, "at"), label: opt_s(c, "label"), commands: strings(c, "do"), mode: s(c, "track").into() })
                    .collect(),
            ),
            "automation" => TrackKind::Automation {
                address: s(v, "lane.address").into(),
                keys: list(v, "lane.keys")
                    .iter()
                    .map(|k| KeyPt {
                        at: num(k, "at"),
                        value: k.get_path("value").cloned().unwrap_or_default(),
                        curve: opt_s(k, "curve").unwrap_or_else(|| "linear".into()),
                    })
                    .collect(),
            },
            "regions" => TrackKind::Regions(
                list(v, "regions")
                    .iter()
                    .map(|r| Region {
                        start: num(r, "start"),
                        end: num(r, "end"),
                        label: opt_s(r, "label"),
                        action: RegionAction::parse(r.get_path("action").unwrap_or(&Value::Null)),
                    })
                    .collect(),
            ),
            _ => return None,
        };
        Some(Track { name, mute: flag(v, "mute"), kind })
    }

    fn type_icon(&self) -> &'static str {
        match self.kind {
            TrackKind::Cues(_) => CUE_ICON,
            TrackKind::Automation { .. } => icon::SCOPE,
            TrackKind::Regions(_) => REGION_ICON,
        }
    }
}

#[derive(Clone, Debug)]
struct RecInfo {
    from: f64,
    cues: i64,
    keys: i64,
}

#[derive(Clone, Debug)]
struct Tl {
    name: String,
    label: Option<String>,
    source: SourceKind,
    source_id: Option<String>,
    rate: Rate,
    length: Option<f64>,
    looping: bool,
    enabled: bool,
    snap: Snap,
    tracks: Vec<Track>,
    time: f64,
    status: String,
    playing: bool,
    recording: bool,
    extent: f64,
    speed: f64,
    source_rate: Option<String>,
    regions_on: Vec<Vec<bool>>,
    record: Option<RecInfo>,
}

impl Tl {
    fn parse(v: &Value) -> Option<Tl> {
        let name = opt_s(v, "name")?;
        let source = match s(v, "source.kind") {
            "manual" => SourceKind::Manual,
            "media" => SourceKind::Media,
            "mtc" => SourceKind::Mtc,
            "ltc" => SourceKind::Ltc,
            _ => SourceKind::Internal,
        };
        let regions_on = list(v, "regions_on").iter().map(|t| t.as_list().unwrap_or(&[]).iter().map(Value::truthy).collect()).collect();
        let record = v.get_path("record").filter(|r| !r.is_null()).map(|r| RecInfo {
            from: num(r, "from"),
            cues: r.get_path("cues").and_then(Value::as_i64).unwrap_or(0),
            keys: r.get_path("keys").and_then(Value::as_i64).unwrap_or(0),
        });
        Some(Tl {
            name,
            label: opt_s(v, "label"),
            source,
            source_id: opt_s(v, "source.id"),
            rate: Rate::parse(s(v, "rate")),
            length: v.get_path("length").and_then(Value::as_f64).filter(|l| *l > 0.0),
            looping: flag(v, "loop"),
            enabled: v.get_path("enabled").is_none_or(Value::truthy),
            snap: Snap::parse(s(v, "snap")),
            tracks: list(v, "tracks").iter().filter_map(Track::parse).collect(),
            time: num(v, "time"),
            status: s(v, "status").into(),
            playing: flag(v, "playing"),
            recording: flag(v, "recording"),
            extent: num(v, "extent"),
            speed: num(v, "speed"),
            source_rate: opt_s(v, "source_rate"),
            regions_on,
            record,
        })
    }

    fn source_label(&self) -> String {
        let kind = match self.source {
            SourceKind::Internal => "internal",
            SourceKind::Manual => "manual",
            SourceKind::Media => "media",
            SourceKind::Mtc => "mtc",
            SourceKind::Ltc => "ltc",
        };
        match &self.source_id {
            Some(id) => format!("{kind}:{id}"),
            None => kind.into(),
        }
    }

    /// Seconds worth showing when fitting the view.
    fn span(&self, grid: Option<&Grid>) -> f64 {
        self.length.unwrap_or(0.0).max(self.extent).max(grid.map_or(0.0, |g| g.duration)).max(30.0)
    }
}

#[derive(Clone, Debug, Default)]
struct Grid {
    bpm: f64,
    beats: Vec<f64>,
    downbeats: Vec<f64>,
    sections: Vec<(f64, f64, String)>,
    chorus: Vec<f64>,
    duration: f64,
    peaks: Vec<f32>,
}

impl Grid {
    fn parse(v: &Value) -> Option<Grid> {
        v.as_map()?;
        Some(Grid {
            bpm: num(v, "bpm"),
            beats: sorted_floats(v, "beats"),
            downbeats: sorted_floats(v, "downbeats"),
            sections: list(v, "sections").iter().map(|x| (num(x, "start"), num(x, "end"), s(x, "label").to_string())).collect(),
            chorus: sorted_floats(v, "chorus"),
            duration: num(v, "duration"),
            peaks: list(v, "peaks").iter().map(|p| p.as_f32().unwrap_or(0.0).clamp(0.0, 1.0)).collect(),
        })
    }
}

/// Live values from the state mirror (falling back to the last `timelines` reply).
struct Live {
    time: f64,
    playing: bool,
    active: bool,
    locked: bool,
    recording: bool,
    status: String,
    source: String,
    timecode: String,
}

impl Live {
    fn read(m: &Model, tl: &Tl) -> Live {
        let a = |k: &str| format!("timeline.{}.{k}", tl.name);
        let b = |k: &str, d: bool| m.get(&a(k)).map_or(d, Value::truthy);
        let text = |k: &str, d: String| m.get(&a(k)).and_then(Value::as_str).filter(|x| !x.is_empty()).map_or(d, String::from);
        Live {
            time: m.get(&a("time")).and_then(Value::as_f64).unwrap_or(tl.time),
            playing: b("playing", tl.playing),
            active: b("active", false),
            locked: b("locked", false),
            recording: b("recording", tl.recording),
            status: text("status", tl.status.clone()),
            source: text("source", tl.source_label()),
            timecode: text("timecode", timecode(tl.time, tl.rate)),
        }
    }
}

// ---------------------------------------------------------------- view state

#[derive(Clone, Copy, Debug, PartialEq)]
enum DragWhat {
    Cue { at: f64 },
    Key { at: f64, value: f64 },
    Region { start: f64, end: f64 },
}

#[derive(Clone, Debug)]
struct Drag {
    timeline: String,
    track: String,
    index: usize,
    /// Pointer time at drag start.
    grab: f64,
    orig: DragWhat,
    cur: DragWhat,
    part: Part,
    /// Automation value range frozen while dragging a key.
    range: (f64, f64),
    /// Set once released: preview held until the reload arrives.
    released: Option<Instant>,
    reload_seq: Option<u64>,
}

#[derive(Clone, Debug)]
enum ActionKind {
    Preset,
    Cuelist,
    Fx,
    Commands,
}

#[derive(Clone, Debug)]
enum Editor {
    Cue {
        timeline: String,
        track: String,
        index: Option<usize>,
        at: String,
        label: String,
        commands: String,
        pos: Pos2,
    },
    Key {
        timeline: String,
        track: String,
        index: usize,
        at: String,
        value: String,
        curve: String,
        pos: Pos2,
    },
    Region {
        timeline: String,
        track: String,
        index: Option<usize>,
        start: String,
        end: String,
        label: String,
        kind: ActionKind,
        target: String,
        cue: String,
        on: String,
        off: String,
        pos: Pos2,
    },
    Track {
        timeline: String,
        name: String,
        kind: &'static str,
        address: String,
    },
    Timeline {
        name: String,
        source: &'static str,
        source_id: String,
        fps: &'static str,
        length: String,
    },
}

#[derive(Clone, Copy, Debug)]
struct Viewport {
    origin: f64,
    pps: f32,
}

#[derive(Clone)]
struct TlState {
    selected: Option<String>,
    list_at: Option<Instant>,
    grid_at: HashMap<String, Instant>,
    /// `Model::events_seen` at the last sync (`None` before the first: history is not replayed).
    events_seen: Option<u64>,
    parsed: Option<(u64, Arc<Vec<Tl>>)>,
    grid: Option<(String, u64, Option<Arc<Grid>>)>,
    views: HashMap<String, Viewport>,
    snap: HashMap<String, Snap>,
    follow: bool,
    arm: String,
    tap_label: String,
    focused: bool,
    playhead: HashMap<String, (f64, Instant)>,
    drag: Option<Drag>,
    editor: Option<Editor>,
    flashes: Vec<(String, String, f64, Instant)>,
    /// Lane width of the last frame (for "fit").
    lane_w: f32,
}

impl Default for TlState {
    fn default() -> Self {
        TlState {
            selected: None,
            list_at: None,
            grid_at: HashMap::new(),
            events_seen: None,
            parsed: None,
            grid: None,
            views: HashMap::new(),
            snap: HashMap::new(),
            follow: true,
            arm: String::new(),
            tap_label: String::new(),
            focused: false,
            playhead: HashMap::new(),
            drag: None,
            editor: None,
            flashes: Vec::new(),
            lane_w: 0.0,
        }
    }
}

fn grid_key(name: &str) -> String {
    format!("timeline.grid:{name}")
}

impl TlState {
    /// Follow events, keep queries fresh, and expire released drag previews.
    fn sync(&mut self, m: &mut Model, now: Instant) {
        let fresh = self.events_seen.map_or(0, |prev| m.events_seen.saturating_sub(prev) as usize);
        self.events_seen = Some(m.events_seen);
        let (mut reload, mut changed) = (false, false);
        for e in m.events.iter().rev().take(fresh) {
            match e.ty.as_str() {
                "project.reloaded" => reload = true,
                "timeline.cue" => {
                    self.flashes.push((s(&e.payload, "timeline").into(), s(&e.payload, "track").into(), num(&e.payload, "at"), now));
                }
                ty if ty.starts_with("timeline.") => changed = true,
                _ => {}
            }
        }
        self.flashes.retain(|f| now.saturating_duration_since(f.3) < CUE_FLASH);
        if reload {
            self.grid_at.clear();
            if let Some(d) = self.drag.as_mut().filter(|d| d.released.is_some()) {
                d.reload_seq = Some(m.q_seq("timelines"));
            }
        }
        if let Some(d) = &self.drag
            && let Some(at) = d.released
            && (d.reload_seq.is_some_and(|seq| m.q_seq("timelines") > seq) || now.saturating_duration_since(at) > GHOST_HOLD)
        {
            self.drag = None;
        }
        if !m.connected {
            return;
        }
        let playing = m.q_list("timelines").iter().any(|v| flag(v, "playing"));
        let every = if playing { LIST_FAST } else { LIST_EVERY };
        if reload || changed || self.list_at.is_none_or(|t| now.saturating_duration_since(t) >= every) {
            self.list_at = Some(now);
            m.query("timelines", Value::Null);
        }
        if let Some(sel) = self.selected.clone()
            && self.grid_at.get(&sel).is_none_or(|t| now.saturating_duration_since(*t) >= GRID_EVERY)
        {
            self.grid_at.insert(sel.clone(), now);
            m.query_as(&grid_key(&sel), "timeline.grid", Value::map().with("name", sel.as_str()));
        }
    }

    fn refresh_soon(&mut self, now: Instant) {
        self.list_at = now.checked_sub(LIST_EVERY.saturating_sub(AFTER_ACTION));
    }

    fn timelines(&mut self, m: &Model) -> Arc<Vec<Tl>> {
        let seq = m.q_seq("timelines");
        match &self.parsed {
            Some((s, v)) if *s == seq => v.clone(),
            _ => {
                let v = Arc::new(m.q_list("timelines").iter().filter_map(Tl::parse).collect::<Vec<_>>());
                self.parsed = Some((seq, v.clone()));
                v
            }
        }
    }

    fn grid_for(&mut self, m: &Model, name: &str) -> Option<Arc<Grid>> {
        let key = grid_key(name);
        let seq = m.q_seq(&key);
        match &self.grid {
            Some((n, s, g)) if n == name && *s == seq => g.clone(),
            _ => {
                let g = m.q(&key).and_then(Grid::parse).map(Arc::new);
                self.grid = Some((name.to_string(), seq, g.clone()));
                g
            }
        }
    }

    /// Current preview of an item being dragged (or just released).
    fn preview(&self, timeline: &str, track: &str, index: usize) -> Option<DragWhat> {
        self.drag.as_ref().filter(|d| d.timeline == timeline && d.track == track && d.index == index).map(|d| d.cur)
    }

    fn dragging(&self) -> bool {
        self.drag.as_ref().is_some_and(|d| d.released.is_none())
    }
}

fn act(name: &str, args: Value) -> Op {
    Op::Action { name: name.into(), args }
}

fn transport(name: &str, timeline: &str) -> Op {
    act(name, Value::map().with("name", timeline))
}

fn edit_args(timeline: &str, track: &str) -> Value {
    Value::map().with("timeline", timeline).with("track", track)
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

// ---------------------------------------------------------------- view

/// Full-area Timeline view (also embedded as a Build-mode dock tab).
pub fn ui(app: &mut App, ui: &mut Ui) {
    let id = Id::new("se.timeline.view");
    let now = Instant::now();
    let mut st = ui.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<TlState>(id)));
    st.sync(&mut app.m, now);
    let t = app.t.clone();
    let mut out = Vec::new();
    view(&app.m, &t, &mut st, ui, &mut out, now);
    if !out.is_empty() {
        st.refresh_soon(now);
    }
    ui.data_mut(|d| d.insert_temp(id, st));
    for op in out {
        app.m.command(op);
    }
}

fn view(m: &Model, t: &Theme, st: &mut TlState, ui: &mut Ui, out: &mut Vec<Op>, now: Instant) {
    let full = ui.available_rect_before_wrap();
    let (pressed, pos) = ui.input(|i| (i.pointer.any_pressed(), i.pointer.interact_pos()));
    if pressed {
        st.focused = pos.is_some_and(|p| full.contains(p));
    }
    let tls = st.timelines(m);
    if st.selected.as_ref().is_none_or(|n| !tls.iter().any(|tl| &tl.name == n)) && !tls.is_empty() {
        st.selected = tls.first().map(|tl| tl.name.clone());
    }
    let list_rect = Rect::from_min_max(full.min, pos2((full.left() + LIST_W).min(full.right()), full.bottom()));
    let main_rect = Rect::from_min_max(pos2(list_rect.right() + spacing::M, full.top()), full.max);
    ui.scope_builder(UiBuilder::new().max_rect(list_rect), |ui| list_column(m, t, st, ui, &tls));
    ui.painter().vline(list_rect.right() + spacing::M / 2.0, full.y_range(), Stroke::new(1.0, t.muted));
    let selected = st.selected.clone().and_then(|n| tls.iter().find(|tl| tl.name == n));
    ui.scope_builder(UiBuilder::new().max_rect(main_rect), |ui| match selected {
        Some(tl) => {
            let grid = st.grid_for(m, &tl.name);
            main_area(m, t, st, ui, out, now, tl, grid.as_deref());
        }
        None => {
            ui.add_space(spacing::XL);
            let msg = if !m.connected {
                "engine offline".to_string()
            } else if let Some(e) = m.query_errors.get("timelines") {
                format!("timelines unavailable: {e}")
            } else {
                "No timelines yet — create one on the left (or add timelines/*.toml to the project).".to_string()
            };
            ui.label(RichText::new(msg).color(t.fg_dim));
        }
    });
    ui.allocate_rect(full, Sense::hover());
    editor_window(ui.ctx(), t, st, out, m);
}

fn list_column(m: &Model, t: &Theme, st: &mut TlState, ui: &mut Ui, tls: &[Tl]) {
    widgets::section(ui, t, icon::TIMELINE, "Timelines");
    let h = (ui.available_height() - 40.0).max(60.0);
    egui::ScrollArea::vertical().id_salt("tl_list").max_height(h).auto_shrink([false, true]).show(ui, |ui| {
        for tl in tls {
            let live = Live::read(m, tl);
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::click());
            let sel = st.selected.as_deref() == Some(tl.name.as_str());
            let fill = if sel {
                t.selection
            } else if resp.hovered() {
                t.bg_light
            } else {
                t.bg_dark
            };
            let p = ui.painter_at(rect);
            p.rect_filled(rect, CornerRadius::same(5), fill);
            if sel {
                p.rect_stroke(rect, CornerRadius::same(5), Stroke::new(1.0, t.accent), StrokeKind::Inside);
            }
            let led = if tl.enabled { status_led(&live.status) } else { LedState::Idle };
            p.circle_filled(rect.left_center() + vec2(12.0, 0.0), 4.5, widgets::led_color(t, led));
            let name_col = if tl.enabled { t.fg_bright } else { t.fg_dim };
            p.text(
                rect.left_top() + vec2(24.0, 7.0),
                Align2::LEFT_TOP,
                tl.label.as_deref().unwrap_or(&tl.name),
                FontId::proportional(type_scale::BODY),
                name_col,
            );
            let sub = format!("{}  ·  {}", live.source, if tl.enabled { live.status.as_str() } else { "disabled" });
            p.text(rect.left_bottom() + vec2(24.0, -7.0), Align2::LEFT_BOTTOM, sub, FontId::proportional(type_scale::SMALL), t.fg_dim);
            if live.recording {
                let badge = Rect::from_min_size(rect.right_top() + vec2(-44.0, 6.0), vec2(38.0, 16.0));
                p.rect_filled(badge, CornerRadius::same(3), t.tally_program());
                p.text(badge.center(), Align2::CENTER_CENTER, "REC", FontId::proportional(10.0), widgets::contrast(t.tally_program()));
            } else if live.playing {
                p.text(rect.right_top() + vec2(-10.0, 7.0), Align2::RIGHT_TOP, icon::PLAY, FontId::proportional(11.0), t.healthy());
            }
            if resp.clicked() && !sel {
                st.selected = Some(tl.name.clone());
                st.drag = None;
            }
            ui.add_space(spacing::XS);
        }
    });
    ui.add_space(spacing::S);
    if ui.button(RichText::new(format!("{PLUS} New timeline")).strong()).clicked() {
        st.editor = Some(Editor::Timeline { name: String::new(), source: "internal", source_id: String::new(), fps: "30", length: String::new() });
    }
}

#[allow(clippy::too_many_arguments)]
fn main_area(m: &Model, t: &Theme, st: &mut TlState, ui: &mut Ui, out: &mut Vec<Op>, now: Instant, tl: &Tl, grid: Option<&Grid>) {
    let live = Live::read(m, tl);
    let entry = st.playhead.entry(tl.name.clone()).or_insert((live.time, now));
    if entry.0 != live.time {
        *entry = (live.time, now);
    }
    let speed = if tl.speed.abs() > 0.01 { tl.speed } else { 1.0 };
    let ph = playhead(entry.0, now.saturating_duration_since(entry.1).as_secs_f64(), live.playing, speed, tl.length);
    if live.playing {
        ui.ctx().request_repaint();
    }
    let snap_mode = *st.snap.entry(tl.name.clone()).or_insert(tl.snap);
    let snap = |x: f64| snap_time(x, snap_mode, grid);

    header(t, st, ui, out, tl, &live, ph, grid);
    shortcuts(st, ui, out, tl, &live);
    ui.add_space(spacing::S);
    toolbar(t, st, ui, tl, grid);
    ui.add_space(spacing::XS);
    tracks_area(t, st, ui, out, now, tl, &live, grid, ph, &snap);
}

#[allow(clippy::too_many_arguments)]
fn header(t: &Theme, st: &mut TlState, ui: &mut Ui, out: &mut Vec<Op>, tl: &Tl, live: &Live, ph: f64, grid: Option<&Grid>) {
    let chased = tl.source.chased();
    ui.horizontal(|ui| {
        ui.label(RichText::new(tl.label.as_deref().unwrap_or(&tl.name)).size(type_scale::HEADING).strong().color(t.fg_bright));
        if tl.label.is_some() {
            ui.label(RichText::new(&tl.name).small().color(t.fg_dim));
        }
        widgets::pill(ui, t, icon::TIMELINE, &live.status, status_led(&live.status)).on_hover_text("transport / chase status");
        let (src_icon, src_led) = match (chased, live.locked) {
            (true, true) => (icon::LINK, LedState::Healthy),
            (true, false) => (icon::UNLINK, status_led(&live.status)),
            (false, _) => (icon::LINK, LedState::Idle),
        };
        let src_text = if chased { format!("{} · {}", live.source, if live.locked { "locked" } else { "unlocked" }) } else { live.source.clone() };
        widgets::pill(ui, t, src_icon, &src_text, src_led).on_hover_text("timecode source");
        if live.active {
            widgets::pill(ui, t, icon::LIVE, "active", LedState::Active).on_hover_text("applying cues and automation");
        }
        if chased {
            ui.label(RichText::new(format!("×{:.3}", tl.speed)).color(t.fg_dim)).on_hover_text("chase speed");
        }
        let rate = match &tl.source_rate {
            Some(r) if r != tl.rate.label() => format!("{} fps (source {r})", tl.rate.label()),
            _ => format!("{} fps", tl.rate.label()),
        };
        ui.label(RichText::new(rate).small().color(t.fg_dim));
        let len = tl.length.map_or_else(|| "open".to_string(), format_time);
        ui.label(RichText::new(format!("length {len}")).small().color(t.fg_dim));
        if tl.looping {
            ui.label(RichText::new(LOOP).color(t.fg_dim)).on_hover_text("loops");
        }
        if let Some(g) = grid.filter(|g| g.bpm > 0.0) {
            ui.label(RichText::new(format!("{:.1} BPM", g.bpm)).small().color(t.fg_dim));
        }
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new(&live.timecode).font(FontId::monospace(type_scale::DISPLAY * 1.3)).color(if live.playing { t.fg_bright } else { t.fg }));
        ui.vertical(|ui| {
            ui.label(RichText::new(format_time(ph)).font(FontId::monospace(type_scale::LARGE)).color(t.fg));
            ui.label(RichText::new(format!("extent {}", format_time(tl.extent))).small().color(t.fg_dim));
        });
        ui.separator();
        let btn = |ui: &mut Ui, text: &str, tip: &str, enabled: bool| {
            ui.add_enabled(enabled, egui::Button::new(RichText::new(text).size(type_scale::LARGE))).on_hover_text(tip).clicked()
        };
        if chased {
            if btn(ui, icon::PLAY, &format!("arm: follow {}", live.source), true) {
                out.push(transport("timeline.play", &tl.name));
            }
            if btn(ui, icon::STOP, "disarm", true) {
                out.push(transport("timeline.stop", &tl.name));
            }
            ui.label(RichText::new(format!("transport follows {}", live.source)).small().color(t.fg_dim));
        } else {
            if btn(ui, TO_START, "locate to start (Home)", true) {
                out.push(locate(&tl.name, 0.0));
            }
            let bars = grid.map(|g| &g.downbeats[..]).filter(|d| !d.is_empty());
            if let Some(d) = bars
                && btn(ui, &format!("{BACK} bar"), "back one bar", bar_target(d, ph, false).is_some())
                && let Some(to) = bar_target(d, ph, false)
            {
                out.push(locate(&tl.name, to));
            }
            if btn(ui, &format!("{BACK} 1s"), "jog back 1 s", true) {
                out.push(act("timeline.jog", Value::map().with("name", tl.name.as_str()).with("delta", -1.0)));
            }
            let (play_icon, play_tip) = if live.playing { (PAUSE, "pause (Space)") } else { (icon::PLAY, "play (Space)") };
            if btn(ui, play_icon, play_tip, true) {
                out.push(transport(if live.playing { "timeline.pause" } else { "timeline.play" }, &tl.name));
            }
            if btn(ui, icon::STOP, "stop (back to 0)", true) {
                out.push(transport("timeline.stop", &tl.name));
            }
            if btn(ui, &format!("1s {FORWARD}"), "jog forward 1 s", true) {
                out.push(act("timeline.jog", Value::map().with("name", tl.name.as_str()).with("delta", 1.0)));
            }
            if let Some(d) = bars
                && btn(ui, &format!("bar {FORWARD}"), "forward one bar", bar_target(d, ph, true).is_some())
                && let Some(to) = bar_target(d, ph, true)
            {
                out.push(locate(&tl.name, to));
            }
        }
        ui.separator();
        widgets::live_edge(ui, t, live.recording, |ui| {
            ui.horizontal(|ui| {
                let rec = RichText::new(format!("{} REC", icon::REC)).strong().color(if live.recording { t.tally_program() } else { t.fg });
                if ui.button(rec).on_hover_text("record mode toggle (R): manual commands become cues, armed addresses become keys").clicked() {
                    out.push(record(&tl.name, !live.recording, &st.arm));
                }
                ui.add_enabled(
                    !live.recording,
                    egui::TextEdit::singleline(&mut st.arm).hint_text("arm: fx.*.mix, lights.* (empty = automation lanes)").desired_width(190.0),
                );
                if let Some(r) = &tl.record {
                    ui.label(RichText::new(format!("from {} · {} cues · {} keys", format_time(r.from), r.cues, r.keys)).small().color(t.tally_program()));
                }
                ui.add(egui::TextEdit::singleline(&mut st.tap_label).hint_text("tap label").desired_width(90.0));
                if ui
                    .add_enabled(live.recording, egui::Button::new(format!("{TAP} Tap")))
                    .on_hover_text("drop a cue at the playhead while recording (T)")
                    .clicked()
                {
                    out.push(tap(&tl.name, &st.tap_label));
                }
            });
        });
    });
}

fn locate(timeline: &str, time: f64) -> Op {
    act("timeline.locate", Value::map().with("name", timeline).with("time", time.max(0.0)))
}

fn record(timeline: &str, on: bool, arm: &str) -> Op {
    let mut args = Value::map().with("name", timeline).with("on", on);
    let patterns: Vec<String> = arm.split(',').map(str::trim).filter(|p| !p.is_empty()).map(String::from).collect();
    if on && !patterns.is_empty() {
        args = args.with("arm", patterns);
    }
    act("timeline.record", args)
}

fn tap(timeline: &str, label: &str) -> Op {
    let mut args = Value::map().with("name", timeline);
    if !label.trim().is_empty() {
        args = args.with("label", label.trim());
    }
    act("timeline.tap", args)
}

/// Space / Home / R / T while the view has keyboard focus (not while typing or editing).
fn shortcuts(st: &mut TlState, ui: &mut Ui, out: &mut Vec<Op>, tl: &Tl, live: &Live) {
    if !st.focused || st.editor.is_some() || ui.ctx().egui_wants_keyboard_input() {
        return;
    }
    let (space, home, r, tk) = ui.input_mut(|i| {
        (
            i.consume_key(Modifiers::NONE, Key::Space),
            i.consume_key(Modifiers::NONE, Key::Home),
            i.consume_key(Modifiers::NONE, Key::R),
            i.consume_key(Modifiers::NONE, Key::T),
        )
    });
    let chased = tl.source.chased();
    if space {
        let op = match (chased, live.playing) {
            (false, true) => "timeline.pause",
            (false, false) => "timeline.play",
            (true, _) if matches!(live.status.as_str(), "idle" | "stopped" | "disabled") => "timeline.play",
            (true, _) => "timeline.stop",
        };
        out.push(transport(op, &tl.name));
    }
    if home && !chased {
        out.push(locate(&tl.name, 0.0));
    }
    if r {
        out.push(record(&tl.name, !live.recording, &st.arm));
    }
    if tk && live.recording {
        out.push(tap(&tl.name, &st.tap_label));
    }
}

fn toolbar(t: &Theme, st: &mut TlState, ui: &mut Ui, tl: &Tl, grid: Option<&Grid>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(SNAP).color(t.fg_dim)).on_hover_text("snap drags and adds");
        let cur = st.snap.get(&tl.name).copied().unwrap_or(tl.snap);
        for (mode, label) in Snap::ALL {
            let enabled = mode == Snap::Off || grid.is_some();
            let text = RichText::new(label).color(if cur == mode { t.accent } else { t.fg_dim });
            let resp = ui.add_enabled(enabled, egui::Button::selectable(cur == mode, text));
            if resp.clicked() {
                st.snap.insert(tl.name.clone(), mode);
            }
            if !enabled {
                resp.on_disabled_hover_text("needs a beat grid (media analysis)");
            }
        }
        ui.separator();
        let follow = RichText::new(format!("{FOLLOW} follow")).color(if st.follow { t.accent } else { t.fg_dim });
        if ui.add(egui::Button::selectable(st.follow, follow)).on_hover_text("keep the playhead in view").clicked() {
            st.follow = !st.follow;
        }
        let vp = st.views.get(&tl.name).copied();
        if let Some(mut vp) = vp {
            let lanes_w = st.lane_w.max(100.0);
            if ui.button(MINUS).on_hover_text("zoom out (Ctrl+wheel)").clicked() {
                vp.pps = (vp.pps / 1.5).clamp(MIN_PPS, MAX_PPS);
            }
            if ui.button(PLUS).on_hover_text("zoom in (Ctrl+wheel)").clicked() {
                vp.pps = (vp.pps * 1.5).clamp(MIN_PPS, MAX_PPS);
            }
            if ui.button(FIT).on_hover_text("fit the whole timeline").clicked() {
                vp = Viewport { origin: 0.0, pps: fit_pps(tl.span(grid), lanes_w) };
                st.follow = false;
            }
            st.views.insert(tl.name.clone(), vp);
        }
        ui.separator();
        if ui.button(format!("{PLUS} Track")).clicked() {
            st.editor = Some(Editor::Track { timeline: tl.name.clone(), name: String::new(), kind: "cues", address: String::new() });
        }
        ui.label(RichText::new("double-click a lane to add · drag to move · right-click for more").small().color(t.fg_dim));
    });
}

#[allow(clippy::too_many_arguments)]
fn tracks_area(
    t: &Theme,
    st: &mut TlState,
    ui: &mut Ui,
    out: &mut Vec<Op>,
    now: Instant,
    tl: &Tl,
    live: &Live,
    grid: Option<&Grid>,
    ph: f64,
    snap: &dyn Fn(f64) -> f64,
) {
    let region = ui.available_rect_before_wrap();
    if region.width() < HEAD_W + 40.0 || region.height() < RULER_H + 20.0 {
        return;
    }
    let x0 = region.left() + HEAD_W;
    let lane_w = region.right() - x0;
    st.lane_w = lane_w;
    let vp = *st.views.entry(tl.name.clone()).or_insert_with(|| Viewport { origin: 0.0, pps: fit_pps(tl.span(grid), lane_w) });
    let mut axis = Axis { x0, origin: vp.origin, pps: vp.pps };

    // wheel = scroll time, ctrl+wheel = zoom (around the pointer)
    let lanes = Rect::from_min_max(pos2(x0, region.top()), region.max);
    if ui.rect_contains_pointer(region) {
        let (zoom, scroll, hover) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta, i.pointer.hover_pos()));
        if zoom != 1.0 {
            axis = axis.zoomed(hover.map_or(x0, |p| p.x.max(x0)), zoom);
        } else if scroll.x + scroll.y != 0.0 {
            axis = axis.scrolled(scroll.x + scroll.y);
            st.follow = false;
        }
    }

    // ruler: drag pans, click locates
    let ruler = Rect::from_min_max(pos2(x0, region.top()), pos2(region.right(), region.top() + RULER_H));
    let rr = ui.interact(ruler, Id::new(("se.tl.ruler", &tl.name)), Sense::click_and_drag()).on_hover_cursor(CursorIcon::PointingHand);
    if rr.dragged() {
        axis = axis.scrolled(rr.drag_delta().x);
        st.follow = false;
    }
    if rr.clicked()
        && !tl.source.chased()
        && let Some(p) = rr.interact_pointer_pos()
    {
        out.push(locate(&tl.name, snap(axis.t(p.x))));
    }
    if st.follow && live.playing && !st.dragging() {
        axis.origin = follow_origin(axis.origin, (lane_w / axis.pps) as f64, ph);
    }
    st.views.insert(tl.name.clone(), Viewport { origin: axis.origin, pps: axis.pps });

    let tc_rate = matches!(tl.source, SourceKind::Mtc | SourceKind::Ltc).then_some(tl.rate);
    paint_ruler(ui, t, ruler, axis, tc_rate, grid, tl, live, ph);
    let gutter = Rect::from_min_max(region.min, pos2(x0, region.top() + RULER_H));
    ui.painter().rect_filled(gutter, CornerRadius::ZERO, t.bg_dark);
    ui.painter().text(
        gutter.left_center() + vec2(8.0, 0.0),
        Align2::LEFT_CENTER,
        if tc_rate.is_some() { "timecode" } else { "time" },
        FontId::proportional(type_scale::SMALL),
        t.fg_dim,
    );

    let mut top = ruler.bottom();
    if let Some(g) = grid {
        let strip = Rect::from_min_max(pos2(x0, top), pos2(region.right(), top + STRIP_H));
        paint_strip(ui, t, strip, axis, g);
        let head = Rect::from_min_max(pos2(region.left(), top), pos2(x0, strip.bottom()));
        let p = ui.painter();
        p.rect_filled(head, CornerRadius::ZERO, t.bg_dark);
        p.text(head.left_top() + vec2(8.0, 6.0), Align2::LEFT_TOP, "analysis", FontId::proportional(type_scale::SMALL), t.fg_dim);
        if g.bpm > 0.0 {
            p.text(head.left_top() + vec2(8.0, 22.0), Align2::LEFT_TOP, format!("{:.1} BPM", g.bpm), FontId::proportional(type_scale::BODY), t.fg);
        }
        top = strip.bottom();
    }

    let rows = Rect::from_min_max(pos2(region.left(), top + 1.0), region.max);
    ui.scope_builder(UiBuilder::new().max_rect(rows), |ui| {
        egui::ScrollArea::vertical()
            .id_salt(("tl_tracks", &tl.name))
            .scroll_source(egui::containers::scroll_area::ScrollSource::SCROLL_BAR)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                if tl.tracks.is_empty() {
                    ui.add_space(spacing::M);
                    ui.label(RichText::new("No tracks — add one with  + Track.").color(t.fg_dim));
                }
                for (ti, track) in tl.tracks.iter().enumerate() {
                    track_row(t, st, ui, out, now, tl, ti, track, axis, grid, snap);
                }
            });
    });

    // playhead over ruler, strip and lanes
    let p = ui.painter_at(lanes);
    let x = axis.x(ph);
    let col = if live.recording { t.tally_program() } else { t.fg_bright };
    p.vline(x, lanes.y_range(), Stroke::new(1.5, col));
    p.add(Shape::convex_polygon(vec![pos2(x - 6.0, lanes.top()), pos2(x + 6.0, lanes.top()), pos2(x, lanes.top() + 8.0)], col, Stroke::NONE));
    ui.allocate_rect(region, Sense::hover());
}

#[allow(clippy::too_many_arguments)]
fn paint_ruler(ui: &Ui, t: &Theme, rect: Rect, axis: Axis, tc: Option<Rate>, grid: Option<&Grid>, tl: &Tl, live: &Live, ph: f64) {
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::ZERO, t.bg_darker);
    let (t0, t1) = (axis.t(rect.left()).max(0.0), axis.t(rect.right()));
    if let Some(l) = tl.length {
        let x = axis.x(l);
        if x < rect.right() {
            p.rect_filled(Rect::from_min_max(pos2(x.max(rect.left()), rect.top()), rect.max), CornerRadius::ZERO, t.bg_dark);
        }
    }
    if let (Some(r), true) = (&tl.record, live.recording) {
        let band = Rect::from_min_max(pos2(axis.x(r.from).max(rect.left()), rect.top()), pos2(axis.x(ph).min(rect.right()), rect.bottom()));
        p.rect_filled(band, CornerRadius::ZERO, t.tally_program().gamma_multiply(0.25));
    }
    if let Some(g) = grid {
        let bar_px = if g.bpm > 0.0 { (240.0 / g.bpm) as f32 * axis.pps } else { 0.0 };
        if bar_px >= 6.0 {
            for &d in visible(&g.downbeats, t0, t1) {
                p.vline(axis.x(d), (rect.bottom() - 6.0)..=rect.bottom(), Stroke::new(1.0, t.fg_dim));
            }
        }
    }
    let step = tick_step(axis.pps, 80.0, tc);
    let minor = step / 5.0;
    if minor * axis.pps as f64 >= 6.0 {
        let mut i = (t0 / minor).floor() as i64;
        while (i as f64) * minor <= t1 {
            p.vline(axis.x(i as f64 * minor), (rect.bottom() - 4.0)..=rect.bottom(), Stroke::new(1.0, t.muted));
            i += 1;
        }
    }
    let mut i = (t0 / step).floor() as i64;
    while (i as f64) * step <= t1 + step {
        let at = i as f64 * step;
        let x = axis.x(at);
        p.vline(x, (rect.top() + 4.0)..=rect.bottom(), Stroke::new(1.0, t.fg_dim));
        let label = match tc {
            Some(r) => timecode(at, r),
            None => ruler_label(at, step),
        };
        p.text(pos2(x + 3.0, rect.top() + 3.0), Align2::LEFT_TOP, label, FontId::monospace(type_scale::SMALL), t.fg_dim);
        i += 1;
    }
}

fn paint_strip(ui: &Ui, t: &Theme, rect: Rect, axis: Axis, g: &Grid) {
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::ZERO, t.bg_darker);
    let (t0, t1) = (axis.t(rect.left()), axis.t(rect.right()));
    for (i, (a, b, label)) in g.sections.iter().enumerate() {
        if *b < t0 || *a > t1 {
            continue;
        }
        let band = Rect::from_min_max(pos2(axis.x(*a).max(rect.left()), rect.top()), pos2(axis.x(*b).min(rect.right()), rect.bottom()));
        let c = if i % 2 == 0 { t.blue } else { t.cyan };
        p.rect_filled(band, CornerRadius::ZERO, c.gamma_multiply(0.14));
        p.vline(axis.x(*a), rect.y_range(), Stroke::new(1.0, c.gamma_multiply(0.6)));
        p.text(pos2(band.left() + 4.0, rect.top() + 2.0), Align2::LEFT_TOP, label, FontId::proportional(type_scale::SMALL), c);
    }
    if !g.peaks.is_empty() {
        let mid = rect.center().y + 4.0;
        let half = rect.height() / 2.0 - 8.0;
        let mut x = rect.left();
        let wave = t.fg.gamma_multiply(0.55);
        while x < rect.right() {
            let pk = peak_span(&g.peaks, axis.t(x), axis.t(x + 1.0));
            if pk > 0.002 {
                p.vline(x + 0.5, (mid - pk * half)..=(mid + pk * half), Stroke::new(1.0, wave));
            }
            x += 1.0;
        }
    }
    beat_lines(&p, t, rect, axis, g);
    let bar_px = if g.bpm > 0.0 { (240.0 / g.bpm) as f32 * axis.pps } else { 0.0 };
    if bar_px >= 26.0 {
        let first = g.downbeats.partition_point(|&d| d < t0);
        for (n, &d) in visible(&g.downbeats, t0, t1).iter().enumerate() {
            p.text(pos2(axis.x(d) + 2.0, rect.bottom() - 2.0), Align2::LEFT_BOTTOM, format!("{}", first + n + 1), FontId::proportional(9.5), t.fg_dim);
        }
    }
    for &c in visible(&g.chorus, t0, t1) {
        let x = axis.x(c);
        p.vline(x, rect.y_range(), Stroke::new(1.5, t.orange));
        p.add(Shape::convex_polygon(vec![pos2(x, rect.top()), pos2(x + 9.0, rect.top() + 5.0), pos2(x, rect.top() + 10.0)], t.orange, Stroke::NONE));
    }
}

/// Beat lines behind lanes (downbeats stronger), thinned out when too dense.
fn beat_lines(p: &egui::Painter, t: &Theme, rect: Rect, axis: Axis, g: &Grid) {
    if g.bpm <= 0.0 {
        return;
    }
    let (t0, t1) = (axis.t(rect.left()), axis.t(rect.right()));
    let beat_px = (60.0 / g.bpm) as f32 * axis.pps;
    if beat_px >= 8.0 {
        for &b in visible(&g.beats, t0, t1) {
            p.vline(axis.x(b), rect.y_range(), Stroke::new(1.0, t.muted.gamma_multiply(0.35)));
        }
    }
    if beat_px * 4.0 >= 8.0 {
        for &d in visible(&g.downbeats, t0, t1) {
            p.vline(axis.x(d), rect.y_range(), Stroke::new(1.0, t.muted.gamma_multiply(0.8)));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn track_row(
    t: &Theme,
    st: &mut TlState,
    ui: &mut Ui,
    out: &mut Vec<Op>,
    now: Instant,
    tl: &Tl,
    ti: usize,
    track: &Track,
    axis: Axis,
    grid: Option<&Grid>,
    snap: &dyn Fn(f64) -> f64,
) {
    let h = match track.kind {
        TrackKind::Cues(_) => CUE_H,
        TrackKind::Automation { .. } => LANE_H,
        TrackKind::Regions(_) => REGION_H,
    };
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let head = Rect::from_min_max(row.min, pos2(axis.x0, row.bottom()));
    let lane = Rect::from_min_max(pos2(axis.x0, row.top()), row.max);

    // header: type icon, name, mute toggle; right-click removes
    let hr = ui.interact(head, Id::new(("se.tl.head", &tl.name, &track.name)), Sense::click());
    let p = ui.painter_at(head);
    p.rect_filled(head, CornerRadius::ZERO, if hr.hovered() { t.bg_light } else { t.bg_dark });
    let name_col = if track.mute { t.fg_dim } else { t.fg };
    p.text(head.left_center() + vec2(8.0, 0.0), Align2::LEFT_CENTER, track.type_icon(), FontId::proportional(type_scale::BODY), t.accent);
    p.text(head.left_center() + vec2(26.0, 0.0), Align2::LEFT_CENTER, &track.name, FontId::proportional(type_scale::BODY), name_col);
    let mute_rect = Rect::from_center_size(pos2(head.right() - 16.0, head.center().y), vec2(22.0, 20.0));
    let mr = ui.interact(mute_rect, Id::new(("se.tl.mute", &tl.name, &track.name)), Sense::click());
    ui.painter().text(
        mute_rect.center(),
        Align2::CENTER_CENTER,
        if track.mute { MUTED } else { UNMUTED },
        FontId::proportional(type_scale::BODY),
        if track.mute {
            t.yellow
        } else if mr.hovered() {
            t.fg
        } else {
            t.fg_dim
        },
    );
    if mr.on_hover_text(if track.mute { "unmute track" } else { "mute track" }).clicked() {
        out.push(act("timeline.edit.track.mute", Value::map().with("timeline", tl.name.as_str()).with("name", track.name.as_str()).with("mute", !track.mute)));
    }
    let tip = match &track.kind {
        TrackKind::Cues(c) => format!("cue track · {} cues", c.len()),
        TrackKind::Automation { address, keys } => format!("automation · {address} · {} keys", keys.len()),
        TrackKind::Regions(r) => format!("region track · {} regions", r.len()),
    };
    let hr = hr.on_hover_text(tip);
    hr.context_menu(|ui| {
        if ui.button(format!("{} {}", if track.mute { UNMUTED } else { MUTED }, if track.mute { "Unmute" } else { "Mute" })).clicked() {
            out.push(act(
                "timeline.edit.track.mute",
                Value::map().with("timeline", tl.name.as_str()).with("name", track.name.as_str()).with("mute", !track.mute),
            ));
            ui.close();
        }
        if ui.button(RichText::new(format!("{TRASH} Remove track")).color(t.bright_red)).clicked() {
            out.push(act("timeline.edit.track.remove", Value::map().with("timeline", tl.name.as_str()).with("name", track.name.as_str())));
            ui.close();
        }
    });

    // lane background
    let lp = ui.painter_at(lane);
    lp.rect_filled(lane, CornerRadius::ZERO, t.bg_darker);
    if let Some(g) = grid {
        beat_lines(&lp, t, lane, axis, g);
    }
    if let Some(l) = tl.length {
        let x = axis.x(l);
        if x < lane.right() {
            lp.rect_filled(Rect::from_min_max(pos2(x.max(lane.left()), lane.top()), lane.max), CornerRadius::ZERO, t.bg_dark.gamma_multiply(0.8));
        }
    }
    let lr = ui.interact(lane, Id::new(("se.tl.lane", &tl.name, &track.name)), Sense::click());
    match &track.kind {
        TrackKind::Cues(cues) => cue_lane(t, st, ui, out, now, tl, track, cues, lane, &lr, axis, snap),
        TrackKind::Automation { keys, .. } => key_lane(t, st, ui, out, tl, track, keys, lane, &lr, axis, snap),
        TrackKind::Regions(regions) => {
            let on = tl.regions_on.get(ti).map(Vec::as_slice).unwrap_or(&[]);
            region_lane(t, st, ui, out, tl, track, regions, on, lane, &lr, axis, snap);
        }
    }
    if track.mute {
        lp.rect_filled(lane, CornerRadius::ZERO, t.bg.gamma_multiply(0.45));
    }
    lp.hline(lane.x_range(), lane.bottom(), Stroke::new(1.0, t.bg));
}

/// Begin / update / finish a drag on an item. Returns the drag when it was just released.
fn drive_drag(st: &mut TlState, resp: &egui::Response, axis: Axis, start: impl FnOnce(f64) -> Drag, update: impl FnOnce(&mut Drag, Pos2, f64)) -> Option<Drag> {
    let pointer = resp.interact_pointer_pos();
    if resp.drag_started()
        && let Some(p) = pointer
    {
        st.drag = Some(start(axis.t(p.x)));
    }
    if resp.dragged()
        && let (Some(p), Some(d)) = (pointer, st.drag.as_mut())
        && d.released.is_none()
    {
        update(d, p, axis.t(p.x));
    }
    if resp.drag_stopped()
        && let Some(d) = st.drag.as_mut()
        && d.released.is_none()
    {
        d.released = Some(Instant::now());
        if d.cur == d.orig {
            st.drag = None;
            return None;
        }
        return Some(d.clone());
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn cue_lane(
    t: &Theme,
    st: &mut TlState,
    ui: &mut Ui,
    out: &mut Vec<Op>,
    now: Instant,
    tl: &Tl,
    track: &Track,
    cues: &[Cue],
    lane: Rect,
    lr: &egui::Response,
    axis: Axis,
    snap: &dyn Fn(f64) -> f64,
) {
    if lr.double_clicked()
        && let Some(p) = lr.interact_pointer_pos()
    {
        st.editor = Some(Editor::Cue {
            timeline: tl.name.clone(),
            track: track.name.clone(),
            index: None,
            at: format!("{:.3}", snap(axis.t(p.x))),
            label: String::new(),
            commands: String::new(),
            pos: p,
        });
    }
    let p = ui.painter_at(lane);
    let y = lane.center().y;
    for (ci, cue) in cues.iter().enumerate() {
        let at = match st.preview(&tl.name, &track.name, ci) {
            Some(DragWhat::Cue { at }) => at,
            _ => cue.at,
        };
        let x = axis.x(at);
        let active_drag = st.dragging() && st.preview(&tl.name, &track.name, ci).is_some();
        if !active_drag && (x < lane.left() - 10.0 || x > lane.right() + 10.0) {
            continue;
        }
        let hit = Rect::from_center_size(pos2(x, y), vec2(16.0, lane.height() - 4.0));
        let resp = ui.interact(hit, Id::new(("se.tl.cue", &tl.name, &track.name, ci)), Sense::click_and_drag()).on_hover_cursor(CursorIcon::Grab);
        let released = drive_drag(
            st,
            &resp,
            axis,
            |grab| Drag {
                timeline: tl.name.clone(),
                track: track.name.clone(),
                index: ci,
                grab,
                orig: DragWhat::Cue { at: cue.at },
                cur: DragWhat::Cue { at: cue.at },
                part: Part::Body,
                range: (0.0, 1.0),
                released: None,
                reload_seq: None,
            },
            |d, _, pt| d.cur = DragWhat::Cue { at: snap((cue.at + pt - d.grab).max(0.0)) },
        );
        if let Some(Drag { cur: DragWhat::Cue { at }, .. }) = released {
            out.push(act("timeline.edit.cue.set", edit_args(&tl.name, &track.name).with("index", ci).with("at", at)));
        }
        let flash =
            st.flashes.iter().any(|f| f.0 == tl.name && f.1 == track.name && (f.2 - cue.at).abs() < 1e-3 && now.saturating_duration_since(f.3) < CUE_FLASH);
        let fill = if flash {
            t.fg_bright
        } else if resp.hovered() || resp.dragged() {
            t.accent.gamma_multiply(1.2)
        } else {
            t.accent
        };
        let r = if flash { 8.0 } else { 6.0 };
        p.add(Shape::convex_polygon(vec![pos2(x, y - r), pos2(x + r, y), pos2(x, y + r), pos2(x - r, y)], fill, Stroke::new(1.0, t.bg_darker)));
        if let Some(label) = &cue.label {
            p.text(pos2(x + 9.0, y), Align2::LEFT_CENTER, label, FontId::proportional(type_scale::SMALL), t.fg);
        }
        let resp = resp.on_hover_ui(|ui| {
            ui.label(RichText::new(cue.label.as_deref().unwrap_or("cue")).strong());
            ui.label(RichText::new(format!("{}  ·  track {}", format_time(cue.at), cue.mode)).small().color(t.fg_dim));
            for c in &cue.commands {
                ui.label(RichText::new(c).monospace());
            }
        });
        let open_editor = |st: &mut TlState, pos: Pos2| {
            st.editor = Some(Editor::Cue {
                timeline: tl.name.clone(),
                track: track.name.clone(),
                index: Some(ci),
                at: format!("{:.3}", cue.at),
                label: cue.label.clone().unwrap_or_default(),
                commands: cue.commands.join("\n"),
                pos,
            });
        };
        if resp.double_clicked() {
            open_editor(st, pos2(x, y));
        }
        resp.context_menu(|ui| {
            if ui.button(format!("{EDIT} Edit…")).clicked() {
                open_editor(st, pos2(x, y));
                ui.close();
            }
            if ui.button(RichText::new(format!("{TRASH} Delete cue")).color(t.bright_red)).clicked() {
                out.push(act("timeline.edit.cue.remove", edit_args(&tl.name, &track.name).with("index", ci)));
                ui.close();
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn key_lane(
    t: &Theme,
    st: &mut TlState,
    ui: &mut Ui,
    out: &mut Vec<Op>,
    tl: &Tl,
    track: &Track,
    keys: &[KeyPt],
    lane: Rect,
    lr: &egui::Response,
    axis: Axis,
    snap: &dyn Fn(f64) -> f64,
) {
    let numeric = !keys.is_empty() && keys.iter().all(|k| k.value.as_f64().is_some() && !matches!(k.value, Value::Bool(_)));
    let dragging_here = st.drag.as_ref().filter(|d| d.timeline == tl.name && d.track == track.name);
    let range = match dragging_here {
        Some(d) => d.range,
        None => value_range(keys.iter().filter_map(|k| k.value.as_f64())),
    };
    let plot = lane.shrink2(vec2(0.0, 8.0));
    let span = (range.1 - range.0).max(1e-9);
    let y_of = |v: f64| plot.bottom() - (((v - range.0) / span).clamp(0.0, 1.0) as f32) * plot.height();
    let v_of = |y: f32| range.0 + ((plot.bottom() - y) / plot.height()).clamp(0.0, 1.0) as f64 * span;

    // keys with the drag preview applied, time-sorted for evaluation
    let mut pts: Vec<(usize, f64, f64)> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| match st.preview(&tl.name, &track.name, i) {
            Some(DragWhat::Key { at, value }) => (i, at, if numeric { value } else { 0.0 }),
            _ => (i, k.at, k.value.as_f64().unwrap_or(0.0)),
        })
        .collect();
    pts.sort_by(|a, b| a.1.total_cmp(&b.1));
    let curve_keys: Vec<(f64, f64, Ease)> = pts.iter().map(|&(i, at, v)| (at, v, ease(&keys[i].curve))).collect();

    if lr.double_clicked()
        && let Some(p) = lr.interact_pointer_pos()
    {
        let at = snap(axis.t(p.x));
        let value = if numeric {
            Value::Float(eval_curve(&curve_keys, at).unwrap_or_else(|| v_of(p.y)))
        } else if let Some(&(i, _, _)) = pts.iter().rev().find(|k| k.1 <= at).or(pts.first()) {
            keys[i].value.clone()
        } else {
            Value::Float(v_of(p.y))
        };
        out.push(act("timeline.edit.key.add", edit_args(&tl.name, &track.name).with("at", at).with("value", value)));
    }

    let p = ui.painter_at(lane);
    let col = if track.mute { t.muted } else { t.modulated() };
    if numeric || keys.is_empty() {
        p.text(lane.left_top() + vec2(4.0, 2.0), Align2::LEFT_TOP, fmt_num(range.1), FontId::proportional(9.5), t.fg_dim);
        p.text(lane.left_bottom() + vec2(4.0, -2.0), Align2::LEFT_BOTTOM, fmt_num(range.0), FontId::proportional(9.5), t.fg_dim);
    }
    if numeric {
        let mut line = Vec::with_capacity((lane.width() / 2.0) as usize + 2);
        let mut x = lane.left();
        while x <= lane.right() + 2.0 {
            if let Some(v) = eval_curve(&curve_keys, axis.t(x)) {
                line.push(pos2(x, y_of(v)));
            }
            x += 2.0;
        }
        p.add(Shape::line(line, Stroke::new(1.5, col)));
    } else if !keys.is_empty() {
        let y = lane.center().y;
        p.hline(lane.x_range(), y, Stroke::new(1.0, col.gamma_multiply(0.5)));
    }

    for &(i, at, v) in &pts {
        let k = &keys[i];
        let x = axis.x(at);
        let y = if numeric { y_of(v) } else { lane.center().y };
        let active_drag = st.dragging() && st.preview(&tl.name, &track.name, i).is_some();
        if !active_drag && (x < lane.left() - 8.0 || x > lane.right() + 8.0) {
            continue;
        }
        let resp = ui
            .interact(Rect::from_center_size(pos2(x, y), vec2(14.0, 14.0)), Id::new(("se.tl.key", &tl.name, &track.name, i)), Sense::click_and_drag())
            .on_hover_cursor(CursorIcon::Grab);
        let orig_v = k.value.as_f64().unwrap_or(0.0);
        let released = drive_drag(
            st,
            &resp,
            axis,
            |grab| Drag {
                timeline: tl.name.clone(),
                track: track.name.clone(),
                index: i,
                grab,
                orig: DragWhat::Key { at: k.at, value: orig_v },
                cur: DragWhat::Key { at: k.at, value: orig_v },
                part: Part::Body,
                range,
                released: None,
                reload_seq: None,
            },
            |d, pos, pt| {
                let value = if numeric { v_of(pos.y) } else { orig_v };
                d.cur = DragWhat::Key { at: snap((k.at + pt - d.grab).max(0.0)), value };
            },
        );
        if let Some(Drag { cur: DragWhat::Key { at, value }, orig: DragWhat::Key { value: ov, .. }, .. }) = released {
            let mut args = edit_args(&tl.name, &track.name).with("index", i).with("at", at);
            if numeric && (value - ov).abs() > 1e-9 {
                args = args.with("value", value);
            }
            out.push(act("timeline.edit.key.set", args));
        }
        let hot = resp.hovered() || resp.dragged();
        p.circle_filled(pos2(x, y), if hot { 5.5 } else { 4.5 }, if hot { t.fg_bright } else { col });
        p.circle_stroke(pos2(x, y), if hot { 5.5 } else { 4.5 }, Stroke::new(1.0, t.bg_darker));
        if !numeric {
            p.text(pos2(x + 7.0, y - 3.0), Align2::LEFT_BOTTOM, value_text(&k.value), FontId::proportional(9.5), t.fg);
        }
        let shown = if numeric { fmt_num(v) } else { value_text(&k.value) };
        let resp = resp.on_hover_text(format!("{}  =  {shown}\ncurve to next: {}", format_time(at), k.curve));
        let open_editor = |st: &mut TlState| {
            st.editor = Some(Editor::Key {
                timeline: tl.name.clone(),
                track: track.name.clone(),
                index: i,
                at: format!("{:.3}", k.at),
                value: value_text(&k.value),
                curve: k.curve.clone(),
                pos: pos2(x, y),
            });
        };
        if resp.double_clicked() {
            open_editor(st);
        }
        resp.context_menu(|ui| {
            ui.menu_button(format!("{} Curve: {}", icon::SCOPE, k.curve), |ui| {
                for c in CURVES {
                    if ui.selectable_label(k.curve == c, c).clicked() {
                        out.push(act("timeline.edit.key.set", edit_args(&tl.name, &track.name).with("index", i).with("curve", c)));
                        ui.close();
                    }
                }
            });
            if ui.button(format!("{EDIT} Edit…")).clicked() {
                open_editor(st);
                ui.close();
            }
            if ui.button(RichText::new(format!("{TRASH} Delete key")).color(t.bright_red)).clicked() {
                out.push(act("timeline.edit.key.remove", edit_args(&tl.name, &track.name).with("index", i)));
                ui.close();
            }
        });
    }
}

fn fmt_num(v: f64) -> String {
    if v.abs() >= 100.0 || v == v.trunc() { format!("{v:.0}") } else { format!("{v:.2}") }
}

#[allow(clippy::too_many_arguments)]
fn region_lane(
    t: &Theme,
    st: &mut TlState,
    ui: &mut Ui,
    out: &mut Vec<Op>,
    tl: &Tl,
    track: &Track,
    regions: &[Region],
    on: &[bool],
    lane: Rect,
    lr: &egui::Response,
    axis: Axis,
    snap: &dyn Fn(f64) -> f64,
) {
    if lr.double_clicked()
        && let Some(p) = lr.interact_pointer_pos()
    {
        let start = snap(axis.t(p.x));
        st.editor = Some(Editor::Region {
            timeline: tl.name.clone(),
            track: track.name.clone(),
            index: None,
            start: format!("{start:.3}"),
            end: format!("{:.3}", start + DEFAULT_REGION),
            label: String::new(),
            kind: ActionKind::Preset,
            target: String::new(),
            cue: String::new(),
            on: String::new(),
            off: String::new(),
            pos: p,
        });
    }
    let p = ui.painter_at(lane);
    for (ri, r) in regions.iter().enumerate() {
        let (start, end) = match st.preview(&tl.name, &track.name, ri) {
            Some(DragWhat::Region { start, end }) => (start, end),
            _ => (r.start, r.end),
        };
        let (xa, xb) = (axis.x(start), axis.x(end).max(axis.x(start) + 4.0));
        let active_drag = st.dragging() && st.preview(&tl.name, &track.name, ri).is_some();
        if !active_drag && (xb < lane.left() || xa > lane.right()) {
            continue;
        }
        let rect = Rect::from_min_max(pos2(xa, lane.top() + 4.0), pos2(xb, lane.bottom() - 4.0));
        let body = ui.interact(rect, Id::new(("se.tl.region", &tl.name, &track.name, ri)), Sense::click_and_drag()).on_hover_cursor(CursorIcon::Grab);
        let edges = [
            (Part::Start, Rect::from_min_max(pos2(xa - EDGE_PX / 2.0, rect.top()), pos2(xa + EDGE_PX / 2.0, rect.bottom()))),
            (Part::End, Rect::from_min_max(pos2(xb - EDGE_PX / 2.0, rect.top()), pos2(xb + EDGE_PX / 2.0, rect.bottom()))),
        ];
        let mut resps = vec![(Part::Body, body.clone())];
        for (part, er) in edges {
            let er = ui
                .interact(er, Id::new(("se.tl.region.edge", &tl.name, &track.name, ri, part == Part::Start)), Sense::drag())
                .on_hover_cursor(CursorIcon::ResizeHorizontal);
            resps.push((part, er));
        }
        for (part, resp) in &resps {
            let part = *part;
            let released = drive_drag(
                st,
                resp,
                axis,
                |grab| Drag {
                    timeline: tl.name.clone(),
                    track: track.name.clone(),
                    index: ri,
                    grab,
                    orig: DragWhat::Region { start: r.start, end: r.end },
                    cur: DragWhat::Region { start: r.start, end: r.end },
                    part,
                    range: (0.0, 1.0),
                    released: None,
                    reload_seq: None,
                },
                |d, _, pt| {
                    let (s0, e0) = drag_region(r.start, r.end, d.part, pt, d.grab, snap);
                    d.cur = DragWhat::Region { start: s0, end: e0 };
                },
            );
            if let Some(Drag { cur: DragWhat::Region { start, end }, .. }) = released {
                out.push(act("timeline.edit.region.set", edit_args(&tl.name, &track.name).with("index", ri).with("start", start).with("end", end)));
            }
        }
        let active = on.get(ri).copied().unwrap_or(false);
        let hot = resps.iter().any(|(_, r)| r.hovered() || r.dragged());
        let (fill, stroke) = if active {
            (t.healthy().gamma_multiply(0.45), Stroke::new(2.0, t.healthy()))
        } else {
            (t.blue.gamma_multiply(0.28), Stroke::new(1.0, t.blue.gamma_multiply(0.8)))
        };
        p.rect_filled(rect, CornerRadius::same(3), if hot { fill.gamma_multiply(1.3) } else { fill });
        p.rect_stroke(rect, CornerRadius::same(3), stroke, StrokeKind::Inside);
        let (ic, what) = r.action.describe();
        let text = match &r.label {
            Some(l) => format!("{ic} {l} · {what}"),
            None => format!("{ic} {what}"),
        };
        let clip = ui.painter_at(rect.shrink(2.0).intersect(lane));
        clip.text(
            pos2(rect.left() + 6.0, rect.center().y),
            Align2::LEFT_CENTER,
            text,
            FontId::proportional(type_scale::SMALL),
            if active { t.fg_bright } else { t.fg },
        );
        let body = body.on_hover_ui(|ui| {
            ui.label(RichText::new(r.label.as_deref().unwrap_or(&what)).strong());
            ui.label(
                RichText::new(format!("{} → {}{}", format_time(r.start), format_time(r.end), if active { "  · active" } else { "" })).small().color(t.fg_dim),
            );
            if let RegionAction::Commands(on, off) = &r.action {
                for c in on {
                    ui.label(RichText::new(format!("on:  {c}")).monospace());
                }
                for c in off {
                    ui.label(RichText::new(format!("off: {c}")).monospace());
                }
            } else {
                ui.label(RichText::new(what.clone()).monospace());
            }
        });
        let open_editor = |st: &mut TlState, pos: Pos2| {
            let (kind, target, cue) = match &r.action {
                RegionAction::Preset(x) => (ActionKind::Preset, x.clone(), String::new()),
                RegionAction::Cuelist(c, q) => (ActionKind::Cuelist, c.clone(), q.clone().unwrap_or_default()),
                RegionAction::Fx(f) => (ActionKind::Fx, f.clone(), String::new()),
                RegionAction::Commands(..) => (ActionKind::Commands, String::new(), String::new()),
            };
            let (on, off) = match &r.action {
                RegionAction::Commands(on, off) => (on.join("\n"), off.join("\n")),
                _ => (String::new(), String::new()),
            };
            st.editor = Some(Editor::Region {
                timeline: tl.name.clone(),
                track: track.name.clone(),
                index: Some(ri),
                start: format!("{:.3}", r.start),
                end: format!("{:.3}", r.end),
                label: r.label.clone().unwrap_or_default(),
                kind,
                target,
                cue,
                on,
                off,
                pos,
            });
        };
        if body.double_clicked() {
            open_editor(st, rect.center());
        }
        body.context_menu(|ui| {
            if ui.button(format!("{EDIT} Edit…")).clicked() {
                open_editor(st, rect.center());
                ui.close();
            }
            if ui.button(RichText::new(format!("{TRASH} Delete region")).color(t.bright_red)).clicked() {
                out.push(act("timeline.edit.region.remove", edit_args(&tl.name, &track.name).with("index", ri)));
                ui.close();
            }
        });
    }
}

// ---------------------------------------------------------------- editors

fn field(ui: &mut Ui, label: &str, text: &mut String, hint: &str, width: f32) {
    ui.label(label);
    ui.add(egui::TextEdit::singleline(text).hint_text(hint).desired_width(width));
    ui.end_row();
}

fn time_ok(t: &Theme, ui: &mut Ui, text: &str) -> Option<f64> {
    let v = parse_secs(text);
    if v.is_none() && !text.trim().is_empty() {
        ui.label(RichText::new(format!("{} bad time `{}` (seconds or m:ss.fff)", icon::WARN, text.trim())).small().color(t.bright_red));
    }
    v
}

fn editor_window(ctx: &egui::Context, t: &Theme, st: &mut TlState, out: &mut Vec<Op>, m: &Model) {
    let Some(mut ed) = st.editor.take() else { return };
    let title = match &ed {
        Editor::Cue { index: None, .. } => "Add cue",
        Editor::Cue { .. } => "Edit cue",
        Editor::Key { .. } => "Edit key",
        Editor::Region { index: None, .. } => "Add region",
        Editor::Region { .. } => "Edit region",
        Editor::Track { .. } => "Add track",
        Editor::Timeline { .. } => "New timeline",
    };
    let pos = match &ed {
        Editor::Cue { pos, .. } | Editor::Key { pos, .. } | Editor::Region { pos, .. } => Some(*pos + vec2(12.0, 12.0)),
        _ => None,
    };
    let mut open = true;
    let mut done = false;
    let mut win = egui::Window::new(RichText::new(title).strong()).id(Id::new("se.tl.editor")).collapsible(false).resizable(false).open(&mut open);
    win = match pos {
        Some(p) => win.default_pos(p),
        None => win.anchor(Align2::CENTER_TOP, [0.0, 90.0]),
    };
    win.show(ctx, |ui| {
        match &mut ed {
            Editor::Cue { timeline, track, index, at, label, commands, .. } => {
                egui::Grid::new("tl-ed-cue").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    field(ui, "At", at, "seconds or m:ss.fff", 120.0);
                    field(ui, "Label", label, "optional", 220.0);
                    ui.label("Do");
                    ui.add(
                        egui::TextEdit::multiline(commands)
                            .hint_text("one command per line\npreset.fire drop\nset fx.glow.amount 1")
                            .code_editor()
                            .desired_rows(4)
                            .desired_width(320.0),
                    );
                    ui.end_row();
                });
                let at_v = time_ok(t, ui, at);
                ui.horizontal(|ui| {
                    let ok = at_v.is_some() && (index.is_some() || !lines(commands).is_empty() || !label.trim().is_empty());
                    if ui.add_enabled(ok, egui::Button::new(RichText::new(format!("{} Save", icon::CHECK)).strong())).clicked()
                        && let Some(at_v) = at_v
                    {
                        let mut args = edit_args(timeline, track).with("at", at_v).with("do", lines(commands));
                        if !label.trim().is_empty() || index.is_some() {
                            args = args.with("label", label.trim());
                        }
                        match index {
                            Some(i) => out.push(act("timeline.edit.cue.set", args.with("index", *i))),
                            None => out.push(act("timeline.edit.cue.add", args)),
                        }
                        done = true;
                    }
                    if let Some(i) = index
                        && ui.button(RichText::new(format!("{TRASH} Delete")).color(t.bright_red)).clicked()
                    {
                        out.push(act("timeline.edit.cue.remove", edit_args(timeline, track).with("index", *i)));
                        done = true;
                    }
                    if ui.button("Cancel").clicked() {
                        done = true;
                    }
                });
            }
            Editor::Key { timeline, track, index, at, value, curve, .. } => {
                egui::Grid::new("tl-ed-key").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    field(ui, "At", at, "seconds or m:ss.fff", 120.0);
                    field(ui, "Value", value, "0.5 · #ff8800 · true", 160.0);
                    ui.label("Curve to next");
                    egui::ComboBox::from_id_salt("tl-ed-curve").selected_text(curve.as_str()).show_ui(ui, |ui| {
                        for c in CURVES {
                            ui.selectable_value(curve, c.to_string(), c);
                        }
                    });
                    ui.end_row();
                });
                let at_v = time_ok(t, ui, at);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(at_v.is_some() && !value.trim().is_empty(), egui::Button::new(RichText::new(format!("{} Save", icon::CHECK)).strong()))
                        .clicked()
                        && let Some(at_v) = at_v
                    {
                        let args = edit_args(timeline, track)
                            .with("index", *index)
                            .with("at", at_v)
                            .with("value", Value::parse_text(value.trim()))
                            .with("curve", curve.as_str());
                        out.push(act("timeline.edit.key.set", args));
                        done = true;
                    }
                    if ui.button(RichText::new(format!("{TRASH} Delete")).color(t.bright_red)).clicked() {
                        out.push(act("timeline.edit.key.remove", edit_args(timeline, track).with("index", *index)));
                        done = true;
                    }
                    if ui.button("Cancel").clicked() {
                        done = true;
                    }
                });
            }
            Editor::Region { timeline, track, index, start, end, label, kind, target, cue, on, off, .. } => {
                egui::Grid::new("tl-ed-region").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    field(ui, "Start", start, "seconds or m:ss.fff", 120.0);
                    field(ui, "End", end, "seconds or m:ss.fff", 120.0);
                    field(ui, "Label", label, "optional", 220.0);
                    ui.label("Action");
                    if index.is_some() {
                        ui.label(RichText::new("fixed once created (remove and re-add to change)").small().color(t.fg_dim));
                        ui.end_row();
                    } else {
                        ui.horizontal(|ui| {
                            for (k, l) in
                                [(ActionKind::Preset, "preset"), (ActionKind::Cuelist, "cuelist"), (ActionKind::Fx, "fx"), (ActionKind::Commands, "commands")]
                            {
                                let sel = std::mem::discriminant(kind) == std::mem::discriminant(&k);
                                if ui.add(egui::Button::selectable(sel, l)).clicked() {
                                    *kind = k;
                                }
                            }
                        });
                        ui.end_row();
                        match kind {
                            ActionKind::Preset => {
                                ui.label("Preset");
                                let presets: Vec<String> = m.q_list("presets").iter().filter_map(|p| opt_s(p, "name")).collect();
                                egui::ComboBox::from_id_salt("tl-ed-preset")
                                    .selected_text(if target.is_empty() { "choose…" } else { target.as_str() })
                                    .show_ui(ui, |ui| {
                                        for p in presets {
                                            ui.selectable_value(target, p.clone(), p);
                                        }
                                    });
                                ui.end_row();
                            }
                            ActionKind::Cuelist => {
                                field(ui, "Cue list", target, "cue list name", 160.0);
                                field(ui, "Cue", cue, "optional cue", 120.0);
                            }
                            ActionKind::Fx => field(ui, "Effect", target, "fx name", 160.0),
                            ActionKind::Commands => {
                                ui.label("On enter");
                                ui.add(egui::TextEdit::multiline(on).hint_text("one command per line").code_editor().desired_rows(3).desired_width(300.0));
                                ui.end_row();
                                ui.label("On exit");
                                ui.add(egui::TextEdit::multiline(off).hint_text("one command per line").code_editor().desired_rows(3).desired_width(300.0));
                                ui.end_row();
                            }
                        }
                    }
                });
                let (sv, ev) = (time_ok(t, ui, start), time_ok(t, ui, end));
                let span_ok = matches!((sv, ev), (Some(a), Some(b)) if b > a);
                if sv.is_some() && ev.is_some() && !span_ok {
                    ui.label(RichText::new(format!("{} end must be after start", icon::WARN)).small().color(t.bright_red));
                }
                let action_ok = index.is_some()
                    || match kind {
                        ActionKind::Commands => !lines(on).is_empty(),
                        _ => !target.trim().is_empty(),
                    };
                ui.horizontal(|ui| {
                    if ui.add_enabled(span_ok && action_ok, egui::Button::new(RichText::new(format!("{} Save", icon::CHECK)).strong())).clicked()
                        && let (Some(sv), Some(ev)) = (sv, ev)
                    {
                        let mut args = edit_args(timeline, track).with("start", sv).with("end", ev);
                        if !label.trim().is_empty() || index.is_some() {
                            args = args.with("label", label.trim());
                        }
                        match index {
                            Some(i) => out.push(act("timeline.edit.region.set", args.with("index", *i))),
                            None => {
                                args = match kind {
                                    ActionKind::Preset => args.with("preset", target.trim()),
                                    ActionKind::Cuelist if cue.trim().is_empty() => args.with("cuelist", target.trim()),
                                    ActionKind::Cuelist => args.with("cuelist", target.trim()).with("cue", cue.trim()),
                                    ActionKind::Fx => args.with("fx", target.trim()),
                                    ActionKind::Commands => args.with("do", lines(on)).with("undo", lines(off)),
                                };
                                out.push(act("timeline.edit.region.add", args));
                            }
                        }
                        done = true;
                    }
                    if let Some(i) = index
                        && ui.button(RichText::new(format!("{TRASH} Delete")).color(t.bright_red)).clicked()
                    {
                        out.push(act("timeline.edit.region.remove", edit_args(timeline, track).with("index", *i)));
                        done = true;
                    }
                    if ui.button("Cancel").clicked() {
                        done = true;
                    }
                });
            }
            Editor::Track { timeline, name, kind, address } => {
                egui::Grid::new("tl-ed-track").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    field(ui, "Name", name, "e.g. drops", 180.0);
                    ui.label("Type");
                    ui.horizontal(|ui| {
                        for k in ["cues", "automation", "regions"] {
                            if ui.add(egui::Button::selectable(*kind == k, k)).clicked() {
                                *kind = k;
                            }
                        }
                    });
                    ui.end_row();
                    if *kind == "automation" {
                        field(ui, "Address", address, "fx.glow.amount", 220.0);
                    }
                });
                ui.horizontal(|ui| {
                    let ok = !name.trim().is_empty() && (*kind != "automation" || !address.trim().is_empty());
                    if ui.add_enabled(ok, egui::Button::new(RichText::new(format!("{} Add", icon::CHECK)).strong())).clicked() {
                        let mut args = Value::map().with("timeline", timeline.as_str()).with("name", name.trim()).with("type", *kind);
                        if *kind == "automation" {
                            args = args.with("address", address.trim());
                        }
                        out.push(act("timeline.edit.track.add", args));
                        done = true;
                    }
                    if ui.button("Cancel").clicked() {
                        done = true;
                    }
                });
            }
            Editor::Timeline { name, source, source_id, fps, length } => {
                egui::Grid::new("tl-ed-new").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    field(ui, "Name", name, "e.g. intro_song", 180.0);
                    ui.label("Source");
                    ui.horizontal(|ui| {
                        for k in SOURCE_KINDS {
                            if ui.add(egui::Button::selectable(*source == k, k)).clicked() {
                                *source = k;
                            }
                        }
                    });
                    ui.end_row();
                    match *source {
                        "media" => field(ui, "Media", source_id, "yt:VIDEO_ID", 180.0),
                        "mtc" => field(ui, "MIDI port", source_id, "optional", 180.0),
                        "ltc" => field(ui, "LTC input", source_id, "input.ltc.0 (optional)", 180.0),
                        _ => {}
                    }
                    ui.label("Frame rate");
                    ui.horizontal(|ui| {
                        for f in FPS_CHOICES {
                            if ui.add(egui::Button::selectable(*fps == f, f)).clicked() {
                                *fps = f;
                            }
                        }
                    });
                    ui.end_row();
                    field(ui, "Length", length, "empty = open", 120.0);
                });
                let len = time_ok(t, ui, length);
                let needs_id = *source == "media";
                let name_ok = !name.trim().is_empty() && name.trim().chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
                if !name.trim().is_empty() && !name_ok {
                    ui.label(RichText::new(format!("{} letters, digits, _ and - only", icon::WARN)).small().color(t.bright_red));
                }
                ui.horizontal(|ui| {
                    let ok = name_ok && (!needs_id || !source_id.trim().is_empty()) && (length.trim().is_empty() || len.is_some());
                    if ui.add_enabled(ok, egui::Button::new(RichText::new(format!("{} Create", icon::CHECK)).strong())).clicked() {
                        let src = if source_id.trim().is_empty() || !matches!(*source, "media" | "mtc" | "ltc") {
                            source.to_string()
                        } else {
                            format!("{source}:{}", source_id.trim())
                        };
                        let mut args = Value::map().with("name", name.trim()).with("source", src).with("fps", *fps);
                        if let Some(l) = len {
                            args = args.with("length", l);
                        }
                        out.push(act("timeline.create", args));
                        st.selected = Some(name.trim().to_string());
                        done = true;
                    }
                    if ui.button("Cancel").clicked() {
                        done = true;
                    }
                });
            }
        }
        if ui.input(|i| i.key_pressed(Key::Escape)) {
            done = true;
        }
    });
    if open && !done {
        st.editor = Some(ed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> Grid {
        Grid {
            bpm: 120.0,
            beats: (0..32).map(|i| i as f64 * 0.5).collect(),
            downbeats: (0..8).map(|i| i as f64 * 2.0).collect(),
            duration: 16.0,
            ..Grid::default()
        }
    }

    #[test]
    fn axis_maps_time_and_pixels_both_ways() {
        let a = Axis { x0: 100.0, origin: 10.0, pps: 50.0 };
        assert_eq!(a.x(10.0), 100.0);
        assert_eq!(a.x(12.0), 200.0);
        assert!((a.t(a.x(13.37)) - 13.37).abs() < 1e-4);
        assert_eq!(a.t(50.0), 9.0, "left of the lane start is earlier time");
    }

    #[test]
    fn zoom_keeps_the_time_under_the_pointer() {
        let a = Axis { x0: 100.0, origin: 10.0, pps: 50.0 };
        let anchor = 400.0;
        let before = a.t(anchor);
        let z = a.zoomed(anchor, 2.0);
        assert_eq!(z.pps, 100.0);
        assert!((z.t(anchor) - before).abs() < 1e-6);
        let out = a.zoomed(anchor, 0.5);
        assert!((out.t(anchor) - before).abs() < 1e-6);
        // clamped zoom and origin
        assert_eq!(a.zoomed(anchor, 1e6).pps, MAX_PPS);
        let far_out = Axis { x0: 0.0, origin: 1.0, pps: 100.0 }.zoomed(500.0, 0.01);
        assert_eq!(far_out.origin, 0.0, "zooming out never scrolls before 0");
    }

    #[test]
    fn scrolling_pans_with_the_pointer_and_stops_at_zero() {
        let a = Axis { x0: 0.0, origin: 10.0, pps: 20.0 };
        assert_eq!(a.scrolled(-40.0).origin, 12.0, "dragging left moves later in time");
        assert_eq!(a.scrolled(40.0).origin, 8.0);
        assert_eq!(a.scrolled(10_000.0).origin, 0.0);
    }

    #[test]
    fn follow_pages_only_when_the_playhead_leaves_the_view() {
        assert_eq!(follow_origin(10.0, 20.0, 15.0), 10.0);
        assert_eq!(follow_origin(10.0, 20.0, 29.5), 27.5);
        assert_eq!(follow_origin(10.0, 20.0, 5.0), 3.0);
        assert_eq!(follow_origin(10.0, 20.0, 1.0), 0.0);
    }

    #[test]
    fn snapping_to_beats_and_bars() {
        let g = grid();
        assert_eq!(snap_time(1.2, Snap::Beat, Some(&g)), 1.0);
        assert_eq!(snap_time(1.3, Snap::Beat, Some(&g)), 1.5);
        assert_eq!(snap_time(2.9, Snap::Bar, Some(&g)), 2.0);
        assert_eq!(snap_time(3.1, Snap::Bar, Some(&g)), 4.0);
        assert_eq!(snap_time(99.0, Snap::Bar, Some(&g)), 14.0, "past the grid snaps to the last bar");
        assert_eq!(snap_time(1.234, Snap::Off, Some(&g)), 1.234);
        assert_eq!(snap_time(1.234, Snap::Beat, None), 1.234, "no grid = no snapping");
        assert_eq!(snap_time(-3.0, Snap::Off, None), 0.0);
        let empty = Grid::default();
        assert_eq!(snap_time(1.234, Snap::Bar, Some(&empty)), 1.234);
    }

    #[test]
    fn bar_steps_move_between_downbeats() {
        let d = grid().downbeats;
        assert_eq!(bar_target(&d, 3.0, true), Some(4.0));
        assert_eq!(bar_target(&d, 4.0, true), Some(6.0), "on a downbeat steps to the next");
        assert_eq!(bar_target(&d, 4.0, false), Some(2.0), "on a downbeat steps to the previous");
        assert_eq!(bar_target(&d, 4.02, false), Some(2.0), "just after a downbeat (playing) still steps back a bar");
        assert_eq!(bar_target(&d, 5.0, false), Some(4.0));
        assert_eq!(bar_target(&d, 0.0, false), None);
        assert_eq!(bar_target(&d, 14.0, true), None);
    }

    #[test]
    fn visible_slices_sorted_points() {
        let pts = [0.0, 1.0, 2.0, 3.0, 4.0];
        assert_eq!(visible(&pts, 0.5, 3.0), &[1.0, 2.0, 3.0]);
        assert!(visible(&pts, 5.0, 9.0).is_empty());
        assert!(visible(&pts, 3.0, 1.0).is_empty(), "inverted range is empty, not a panic");
    }

    #[test]
    fn timecode_formatting_matches_se_clock() {
        assert_eq!(timecode(0.0, Rate::R25), "00:00:00:00");
        assert_eq!(timecode(61.5, Rate::R25), "00:01:01:12");
        assert_eq!(timecode(3723.0 + 23.0 / 24.0, Rate::R24), "01:02:03:23");
        assert_eq!(timecode(1.0 / 30.0, Rate::R30), "00:00:00:01", "exact frame boundaries round-trip");
        // drop-frame: frame 1800 is labelled 00:01:00;02, frame 17982 is 00:10:00;00
        assert_eq!(timecode(1800.0 / Rate::R2997Df.fps(), Rate::R2997Df), "00:01:00;02");
        assert_eq!(timecode(1799.0 / Rate::R2997Df.fps(), Rate::R2997Df), "00:00:59;29");
        assert_eq!(timecode(17_982.0 / Rate::R2997Df.fps(), Rate::R2997Df), "00:10:00;00");
        assert_eq!(timecode(107_892.0 / Rate::R2997Df.fps(), Rate::R2997Df), "01:00:00;00");
        assert_eq!(timecode(-5.0, Rate::R30), "00:00:00:00");
    }

    #[test]
    fn ruler_labels_and_steps() {
        assert_eq!(ruler_label(65.0, 5.0), "1:05");
        assert_eq!(ruler_label(5.5, 0.5), "0:05.5");
        assert_eq!(ruler_label(5.25, 0.05), "0:05.25");
        assert_eq!(ruler_label(3720.0, 60.0), "1:02:00");
        assert_eq!(tick_step(40.0, 80.0, None), 2.0);
        assert_eq!(tick_step(1000.0, 80.0, None), 0.1);
        assert_eq!(tick_step(0.01, 80.0, None), 3600.0, "far out uses the widest step");
        // timecode steps are whole frames
        let s = tick_step(1500.0, 80.0, Some(Rate::R25));
        assert!((s - 2.0 / 25.0).abs() < 1e-12);
        let s = tick_step(40.0, 80.0, Some(Rate::R2997Df));
        assert!((s * Rate::R2997Df.fps() - 60.0).abs() < 1e-9, "2 s worth of nominal frames");
    }

    #[test]
    fn editor_times_parse() {
        assert_eq!(parse_secs("12.5"), Some(12.5));
        assert_eq!(parse_secs(" 1:02.5 "), Some(62.5));
        assert_eq!(parse_secs("1:00:00"), Some(3600.0));
        assert_eq!(parse_secs("4s"), Some(4.0));
        assert_eq!(parse_secs("1:75"), None, "seconds field must be < 60");
        assert_eq!(parse_secs("-1"), None);
        assert_eq!(parse_secs(""), None);
        assert_eq!(parse_secs("abc"), None);
        assert_eq!(parse_secs("1:2:3:4"), None);
    }

    #[test]
    fn curve_evaluation_between_keys() {
        let keys = [(0.0, 0.0, Ease::Linear), (2.0, 1.0, Ease::InQuad), (4.0, 0.0, Ease::Step), (6.0, 1.0, Ease::Linear)];
        assert_eq!(eval_curve(&keys, -1.0), Some(0.0), "holds the first key before it");
        assert_eq!(eval_curve(&keys, 1.0), Some(0.5), "linear segment");
        // in_quad from 1 → 0 at the midpoint: 1 + (0 - 1) * 0.25
        assert!((eval_curve(&keys, 3.0).unwrap() - 0.75).abs() < 1e-12);
        assert_eq!(eval_curve(&keys, 5.0), Some(0.0), "step holds until the next key");
        assert_eq!(eval_curve(&keys, 6.0), Some(1.0));
        assert_eq!(eval_curve(&keys, 9.0), Some(1.0), "holds the last key after it");
        assert_eq!(eval_curve(&[], 1.0), None);
        let same_time = [(1.0, 0.0, Ease::Linear), (1.0, 5.0, Ease::Linear), (2.0, 5.0, Ease::Linear)];
        assert_eq!(eval_curve(&same_time, 1.5), Some(5.0));
        assert_eq!(ease("out_back"), Ease::OutBack);
        assert_eq!(ease("nonsense"), Ease::Linear);
    }

    #[test]
    fn value_ranges_pad_or_use_unit() {
        assert_eq!(value_range([0.2, 0.8].into_iter()), (0.0, 1.0));
        assert_eq!(value_range(std::iter::empty()), (0.0, 1.0));
        let (lo, hi) = value_range([0.0, 100.0].into_iter());
        assert!((lo + 10.0).abs() < 1e-9 && (hi - 110.0).abs() < 1e-9);
        let (lo, hi) = value_range([5.0].into_iter());
        assert!(lo < 5.0 && hi > 5.0, "a single value gets a non-empty range");
    }

    #[test]
    fn playhead_extrapolates_boundedly() {
        assert_eq!(playhead(10.0, 0.5, false, 1.0, None), 10.0);
        assert_eq!(playhead(10.0, 0.5, true, 1.0, None), 10.5);
        assert_eq!(playhead(10.0, 0.5, true, 0.5, None), 10.25);
        assert_eq!(playhead(10.0, 30.0, true, 1.0, None), 10.0 + PLAYHEAD_COAST, "stale updates don't run away");
        assert_eq!(playhead(99.8, 0.5, true, 1.0, Some(100.0)), 100.0);
    }

    #[test]
    fn waveform_peaks_per_column() {
        let mut peaks = vec![0.1f32; 1000];
        peaks[500] = 0.9;
        assert_eq!(peak_span(&peaks, 5.0, 5.01), 0.9);
        assert_eq!(peak_span(&peaks, 4.0, 4.001), 0.1, "sub-bin spans still read one bin");
        assert_eq!(peak_span(&peaks, 20.0, 21.0), 0.0, "past the end is silent");
        assert_eq!(peak_span(&peaks, -1.0, -0.5), 0.0);
        assert_eq!(peak_span(&[], 0.0, 1.0), 0.0);
    }

    #[test]
    fn region_drags_move_and_resize() {
        let g = grid();
        let snap = |t: f64| snap_time(t, Snap::Beat, Some(&g));
        let none = |t: f64| t;
        assert_eq!(drag_region(2.0, 6.0, Part::Body, 3.2, 2.0, &snap), (3.0, 7.0), "moves keep the length and snap the start");
        assert_eq!(drag_region(2.0, 6.0, Part::Body, 0.0, 5.0, &none), (0.0, 4.0), "cannot move before 0");
        assert_eq!(drag_region(2.0, 6.0, Part::Start, 2.7, 2.0, &snap), (2.5, 6.0));
        assert_eq!(drag_region(2.0, 6.0, Part::End, 7.1, 6.0, &snap), (2.0, 7.0));
        let (s0, e0) = drag_region(2.0, 6.0, Part::Start, 9.0, 2.0, &none);
        assert!(s0 < e0, "start never passes the end");
        let (s1, e1) = drag_region(2.0, 6.0, Part::End, 0.0, 6.0, &none);
        assert!(e1 > s1, "end never passes the start");
    }

    #[test]
    fn parses_timeline_query_rows() {
        let v: Value = serde_json::json!({
            "name": "show", "file": "timelines/show.toml", "label": null,
            "source": {"kind": "media", "id": "yt:abc"}, "rate": "29.97df", "priority": 60, "length": null, "loop": false,
            "enabled": true, "snap": "bar", "time": 12.5, "status": "locked", "active": true, "playing": true, "recording": false,
            "extent": 30.0, "speed": 1.0, "source_rate": null, "regions_on": [[], [], [true, false]],
            "tracks": [
                {"name": "hits", "mute": false, "type": "cues", "cues": [{"at": 1.0, "label": "drop", "do": ["preset.fire drop"], "track": "auto"}]},
                {"name": "glow", "mute": true, "type": "automation", "lane": {"address": "fx.glow.amount", "keys": [{"at": 0.0, "value": 0.0, "curve": "linear"}]}},
                {"name": "looks", "mute": false, "type": "regions", "regions": [
                    {"start": 0.0, "end": 4.0, "label": null, "action": {"kind": "cuelist", "cuelist": "main", "cue": null}},
                    {"start": 4.0, "end": 8.0, "label": "x", "action": {"kind": "commands", "on": ["a", "b"], "off": []}}
                ]}
            ]
        })
        .into();
        let tl = Tl::parse(&v).unwrap();
        assert_eq!(tl.source, SourceKind::Media);
        assert!(tl.source.chased());
        assert_eq!(tl.source_label(), "media:yt:abc");
        assert_eq!(tl.rate, Rate::R2997Df);
        assert_eq!(tl.snap, Snap::Bar);
        assert_eq!(tl.length, None);
        assert_eq!(tl.tracks.len(), 3);
        assert!(matches!(&tl.tracks[0].kind, TrackKind::Cues(c) if c[0].commands == ["preset.fire drop"] && c[0].label.as_deref() == Some("drop")));
        assert!(tl.tracks[1].mute);
        assert!(matches!(&tl.tracks[1].kind, TrackKind::Automation { address, keys } if address == "fx.glow.amount" && keys.len() == 1));
        let TrackKind::Regions(r) = &tl.tracks[2].kind else { panic!("regions") };
        assert_eq!(r[0].action.describe().1, "cuelist main");
        assert_eq!(r[1].action.describe().1, "a +1");
        assert_eq!(tl.regions_on[2], vec![true, false]);
        assert_eq!(tl.span(None), 30.0);
    }

    #[test]
    fn status_colours() {
        assert_eq!(status_led("locked"), LedState::Healthy);
        assert_eq!(status_led("playing"), LedState::Healthy);
        assert_eq!(status_led("freewheel"), LedState::Armed);
        assert_eq!(status_led("locking"), LedState::Armed);
        assert_eq!(status_led("lost"), LedState::Error);
        assert_eq!(status_led("waiting"), LedState::Idle);
        assert_eq!(status_led("stopped"), LedState::Idle);
    }
}
