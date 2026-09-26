//! Timelines (§2.7): cue tracks, automation lanes, and region tracks bound to a timecode
//! source, evaluated inside the deterministic core tick.
//!
//! * **Sources:** `internal` (show clock: `timeline.play|pause|stop|locate|jog`), `manual`
//!   (jog/locate, optional scrub signal), `media:<id>` (follows `song.position` while
//!   `song.media` is `<id>`), `mtc[:port]`, `ltc[:input]` (observations from the I/O glue).
//! * **Chase:** external sources go through [`se_clock::timecode::chase`] (lock with jitter
//!   tolerance, freewheel on dropout). On a jump the timeline applies the *tracked state* at
//!   the new time — the latest stateful command per target (`set`, cue-list cue, latching
//!   preset, scene, mode, mix snapshot) — and reverts targets that no cue before the new
//!   time touched, instead of firing every skipped cue.
//! * **Priority:** automation and cue `set`s are overrides with key `timeline:<name>` at the
//!   timeline's priority, so several timelines (and presets, chat, manual moves) resolve
//!   through the normal resolver.
//! * **Replay:** MTC/LTC positions arrive as replayable `Input::Timecode`; signal-derived
//!   positions (`song.position`, scrub signals) are converted into recorded observations
//!   here, because signals are not part of the session log.
//! * **Record:** manual commands become cues, armed addresses become keyframes; stopping
//!   emits `timeline.record.commit` for the I/O glue, which writes the TOML with `toml_edit`.

use super::{Core, Ctx, Input, Output};
use crate::config::{Config, ConfigError};
use crate::state::{Override, lerp_value};
use se_clock::timecode::chase::{Chase, ChaseConfig, ChaseEvent, ChaseStatus};
use se_clock::timecode::{FrameRate, ObsKind, TcObs, Timecode};
use se_proto::{Command, Ease, Event, Meta, Op, Origin, PRIORITY_MANUAL, PRIORITY_PRESET, Ts, Value, address, next_id, parse_duration_ms};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Observation source key for media players (`song.position`).
pub const MEDIA: &str = "media";
/// Signal carrying the current media position (seconds).
pub const SONG_POSITION: &str = "song.position";
/// State carrying the current media key (`yt:<videoId>`).
pub const SONG_MEDIA: &str = "song.media";
/// Player state (`playing`, `paused`, `buffering`, …).
pub const SONG_STATE: &str = "song.state";

pub const STATUSES: &[&str] = &["idle", "disabled", "stopped", "paused", "playing", "waiting", "locking", "locked", "freewheel", "lost"];

const FX_HOLD_MS: i64 = 86_400_000;
const EPS: f64 = 1e-6;

// ---- definitions ---------------------------------------------------------------------------

/// Where a timeline gets its time from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum SourceDef {
    Internal,
    Manual,
    /// `yt:<videoId>`, `file:<hash>`, `isrc:<code>`: matched against `song.media`.
    Media(String),
    /// Port pattern (empty = the project's default MTC input).
    Mtc(String),
    /// Audio input (empty = the project's default LTC input).
    Ltc(String),
}

impl SourceDef {
    pub fn parse(s: &str) -> Result<SourceDef, String> {
        let s = s.trim();
        Ok(match s {
            "" | "internal" => SourceDef::Internal,
            "manual" => SourceDef::Manual,
            "mtc" => SourceDef::Mtc(String::new()),
            "ltc" => SourceDef::Ltc(String::new()),
            _ => {
                if let Some(p) = s.strip_prefix("mtc:") {
                    SourceDef::Mtc(p.to_string())
                } else if let Some(p) = s.strip_prefix("ltc:") {
                    SourceDef::Ltc(p.to_string())
                } else {
                    let id = s.strip_prefix("media:").unwrap_or(s);
                    let ok = ["yt:", "file:", "isrc:"].iter().any(|p| id.strip_prefix(p).is_some_and(|r| !r.is_empty()));
                    if !ok {
                        return Err(format!("unknown source `{s}` (internal, manual, mtc[:port], ltc[:input], media:yt:<id>|file:<hash>|isrc:<code>)"));
                    }
                    SourceDef::Media(id.to_string())
                }
            }
        })
    }

    /// Key under which observations arrive (`Input::Timecode.source`).
    pub fn key(&self) -> String {
        match self {
            SourceDef::Internal => "internal".into(),
            SourceDef::Manual => "manual".into(),
            SourceDef::Media(_) => MEDIA.into(),
            SourceDef::Mtc(p) if p.is_empty() => "mtc".into(),
            SourceDef::Mtc(p) => format!("mtc:{p}"),
            SourceDef::Ltc(p) if p.is_empty() => "ltc".into(),
            SourceDef::Ltc(p) => format!("ltc:{p}"),
        }
    }

    /// Display/state form (`media:yt:abc`, `mtc`, …).
    pub fn label(&self) -> String {
        match self {
            SourceDef::Media(id) => format!("media:{id}"),
            s => s.key(),
        }
    }

    pub fn chased(&self) -> bool {
        matches!(self, SourceDef::Media(_) | SourceDef::Mtc(_) | SourceDef::Ltc(_))
    }

    pub fn is_timecode(&self) -> bool {
        matches!(self, SourceDef::Mtc(_) | SourceDef::Ltc(_))
    }
}

/// Whether a cue's commands take part in tracked-state recall on jumps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TrackMode {
    /// Stateful commands (set, cue-list cues, latching presets, scene, mode, snapshot) track.
    #[default]
    Auto,
    /// Every command is re-applied on a jump (once, as the latest of its kind).
    Always,
    /// Never re-applied: fires only when the playhead crosses it.
    Never,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CueDef {
    pub at: f64,
    pub label: Option<String>,
    #[serde(rename = "do")]
    pub commands: Vec<String>,
    pub track: TrackMode,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct KeyDef {
    pub at: f64,
    pub value: Value,
    /// Shape of the segment from this key to the next.
    pub curve: Ease,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LaneDef {
    /// Address or pattern (`lights.group.*.master`).
    pub address: String,
    pub keys: Vec<KeyDef>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RegionAction {
    Preset { preset: String },
    Cuelist { cuelist: String, cue: Option<String> },
    Fx { fx: String },
    Commands { on: Vec<String>, off: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RegionDef {
    pub start: f64,
    pub end: f64,
    pub label: Option<String>,
    pub action: RegionAction,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TrackKind {
    Cues { cues: Vec<CueDef> },
    Automation { lane: LaneDef },
    Regions { regions: Vec<RegionDef> },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TrackDef {
    pub name: String,
    pub mute: bool,
    #[serde(flatten)]
    pub kind: TrackKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Snap {
    #[default]
    Off,
    Beat,
    Bar,
}

/// One `timelines/<name>.toml`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TimelineDef {
    pub name: String,
    pub file: String,
    pub label: Option<String>,
    pub source: SourceDef,
    /// Frame rate for timecode labels (MTC/LTC report their own).
    pub rate: FrameRate,
    pub priority: u16,
    pub length: Option<f64>,
    #[serde(rename = "loop")]
    pub looping: bool,
    /// Follow the source from load (chased/manual); `timeline.stop` disarms at runtime.
    pub enabled: bool,
    pub chase: ChaseConfig,
    /// Release the timeline's state when an external source stops or is lost (default: hold).
    pub release_on_stop: bool,
    /// Snap edits/recorded cues to the beat grid (offline analysis of the media, §8.4).
    pub snap: Snap,
    /// Audio file (project-relative or absolute) whose offline analysis provides the beat
    /// grid when the source isn't a `file:` media item (e.g. an MTC show's backing track).
    pub analysis: Option<String>,
    /// MTC output port pattern, generated from this timeline's position.
    pub mtc_out: Option<String>,
    /// LTC output (audio slot `timecode.ltc`), generated from this timeline's position.
    pub ltc_out: bool,
    /// Manual source: signal whose value × `scrub_range` is the position.
    pub scrub: Option<String>,
    pub scrub_range: f64,
    /// Track that recorded cues are written to.
    pub record_track: String,
    pub tracks: Vec<TrackDef>,
}

/// Parse a time: `"1:12.400"`, `"72.4s"`, `"01:00:10:12"` (timecode label at `rate`), or a
/// number of milliseconds.
pub fn parse_time(v: &toml::Value, rate: FrameRate) -> Result<f64, String> {
    match v {
        toml::Value::Integer(i) if *i >= 0 => Ok(*i as f64 / 1000.0),
        toml::Value::Float(f) if *f >= 0.0 => Ok(*f / 1000.0),
        toml::Value::String(s) => parse_time_str(s, rate),
        other => Err(format!("bad time `{other}`")),
    }
}

pub fn parse_time_str(s: &str, rate: FrameRate) -> Result<f64, String> {
    let t = s.trim();
    if t.split([':', ';']).count() == 4 {
        return Timecode::parse(t, rate).map(|tc| tc.to_seconds()).ok_or_else(|| format!("bad timecode `{s}` at {rate} fps"));
    }
    parse_duration_ms(t).map(|ms| ms as f64 / 1000.0).ok_or_else(|| format!("bad time `{s}`"))
}

/// Parse a time given as a command argument (seconds as a number, or a time string).
pub fn parse_time_value(v: &Value, rate: FrameRate) -> Result<f64, String> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) if f.is_finite() => Ok(*f),
        Value::Str(s) => parse_time_str(s, rate),
        other => Err(format!("bad time `{other}`")),
    }
}

/// Format seconds as `m:ss.mmm` (`h:mm:ss.mmm` from one hour).
pub fn format_time(secs: f64) -> String {
    let ms = (secs.max(0.0) * 1000.0).round() as u64;
    let (h, m, s, f) = (ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60, ms % 1000);
    if h > 0 { format!("{h}:{m:02}:{s:02}.{f:03}") } else { format!("{m}:{s:02}.{f:03}") }
}

fn commands(v: Option<&toml::Value>, what: &str) -> Result<Vec<String>, String> {
    let list = match v {
        None => return Ok(Vec::new()),
        Some(toml::Value::String(s)) => vec![s.clone()],
        Some(toml::Value::Array(a)) => {
            a.iter().map(|x| x.as_str().map(String::from).ok_or_else(|| format!("{what}: commands must be strings"))).collect::<Result<_, _>>()?
        }
        Some(_) => return Err(format!("{what}: `do` must be a string or a list of strings")),
    };
    for c in &list {
        Op::parse(c).map_err(|e| format!("{what}: command `{c}`: {e}"))?;
    }
    Ok(list)
}

fn str_of(t: &toml::Table, k: &str) -> Option<String> {
    t.get(k).and_then(|v| v.as_str()).map(String::from)
}

fn bool_of(t: &toml::Table, k: &str, d: bool) -> Result<bool, String> {
    match t.get(k) {
        None => Ok(d),
        Some(toml::Value::Boolean(b)) => Ok(*b),
        Some(v) => Err(format!("`{k}` must be true/false, not {v}")),
    }
}

fn dur_secs(t: &toml::Table, k: &str, d: f64) -> Result<f64, String> {
    match t.get(k) {
        None => Ok(d),
        Some(toml::Value::String(s)) => parse_duration_ms(s).map(|ms| ms as f64 / 1000.0).ok_or_else(|| format!("bad duration `{k} = {s}`")),
        Some(toml::Value::Integer(i)) if *i >= 0 => Ok(*i as f64 / 1000.0),
        Some(toml::Value::Float(f)) if *f >= 0.0 => Ok(*f / 1000.0),
        Some(v) => Err(format!("bad duration `{k} = {v}`")),
    }
}

fn parse_cue(t: &toml::Table, rate: FrameRate, what: &str) -> Result<CueDef, String> {
    let at = parse_time(t.get("at").ok_or_else(|| format!("{what}: cue without `at`"))?, rate).map_err(|e| format!("{what}: {e}"))?;
    let track = match t.get("track") {
        None => TrackMode::Auto,
        Some(toml::Value::Boolean(true)) => TrackMode::Always,
        Some(toml::Value::Boolean(false)) => TrackMode::Never,
        Some(toml::Value::String(s)) if s == "auto" => TrackMode::Auto,
        Some(v) => return Err(format!("{what}: `track` must be true, false, or \"auto\" (got {v})")),
    };
    Ok(CueDef { at, label: str_of(t, "label"), commands: commands(t.get("do"), what)?, track })
}

fn tables<'a>(v: &'a toml::Value, what: &str) -> Result<Vec<&'a toml::Table>, String> {
    v.as_array()
        .ok_or_else(|| format!("{what} must be an array"))?
        .iter()
        .map(|x| x.as_table().ok_or_else(|| format!("{what}: entries must be tables")))
        .collect()
}

fn parse_track(t: &toml::Table, rate: FrameRate, idx: usize) -> Result<TrackDef, String> {
    let name = str_of(t, "name").unwrap_or_else(|| format!("track{}", idx + 1));
    let what = format!("track `{name}`");
    let kind_s = str_of(t, "type").or_else(|| str_of(t, "kind")).unwrap_or_else(|| {
        if t.contains_key("address") || t.contains_key("keys") {
            "automation".into()
        } else if t.contains_key("regions") {
            "regions".into()
        } else {
            "cues".into()
        }
    });
    let kind = match kind_s.as_str() {
        "cues" | "cue" => {
            let mut cues = match t.get("cues") {
                Some(v) => tables(v, &what)?.into_iter().map(|c| parse_cue(c, rate, &what)).collect::<Result<Vec<_>, _>>()?,
                None => Vec::new(),
            };
            cues.sort_by(|a, b| a.at.total_cmp(&b.at));
            TrackKind::Cues { cues }
        }
        "automation" | "lane" => {
            let address = str_of(t, "address").ok_or_else(|| format!("{what}: automation needs `address`"))?;
            if !address::is_valid(&address, true) {
                return Err(format!("{what}: bad address `{address}`"));
            }
            let default_curve = match str_of(t, "curve") {
                Some(c) => Ease::parse(&c).ok_or_else(|| format!("{what}: unknown curve `{c}`"))?,
                None => Ease::Linear,
            };
            let mut keys = Vec::new();
            if let Some(v) = t.get("keys") {
                for k in tables(v, &what)? {
                    let at = parse_time(k.get("at").ok_or_else(|| format!("{what}: key without `at`"))?, rate).map_err(|e| format!("{what}: {e}"))?;
                    let value = Value::from(k.get("value").cloned().ok_or_else(|| format!("{what}: key without `value`"))?);
                    let curve = match k.get("curve").and_then(|c| c.as_str()) {
                        Some(c) => Ease::parse(c).ok_or_else(|| format!("{what}: unknown curve `{c}`"))?,
                        None => default_curve,
                    };
                    keys.push(KeyDef { at, value, curve });
                }
            }
            keys.sort_by(|a, b| a.at.total_cmp(&b.at));
            TrackKind::Automation { lane: LaneDef { address, keys } }
        }
        "regions" | "region" => {
            let mut regions = Vec::new();
            if let Some(v) = t.get("regions") {
                for r in tables(v, &what)? {
                    let start =
                        parse_time(r.get("start").ok_or_else(|| format!("{what}: region without `start`"))?, rate).map_err(|e| format!("{what}: {e}"))?;
                    let end = parse_time(r.get("end").ok_or_else(|| format!("{what}: region without `end`"))?, rate).map_err(|e| format!("{what}: {e}"))?;
                    if end <= start {
                        return Err(format!("{what}: region end must be after start"));
                    }
                    let action = if let Some(p) = str_of(r, "preset") {
                        RegionAction::Preset { preset: p }
                    } else if let Some(cl) = str_of(r, "cuelist") {
                        RegionAction::Cuelist { cuelist: cl, cue: str_of(r, "cue") }
                    } else if let Some(fx) = str_of(r, "fx") {
                        RegionAction::Fx { fx }
                    } else if r.contains_key("do") || r.contains_key("undo") {
                        RegionAction::Commands { on: commands(r.get("do"), &what)?, off: commands(r.get("undo"), &what)? }
                    } else {
                        return Err(format!("{what}: region needs `preset`, `cuelist`, `fx`, or `do`/`undo`"));
                    };
                    regions.push(RegionDef { start, end, label: str_of(r, "label"), action });
                }
            }
            regions.sort_by(|a, b| a.start.total_cmp(&b.start));
            TrackKind::Regions { regions }
        }
        other => return Err(format!("{what}: unknown track type `{other}` (cues, automation, regions)")),
    };
    Ok(TrackDef { name, mute: bool_of(t, "mute", false)?, kind })
}

impl TimelineDef {
    pub fn parse(name: &str, file: &str, t: &toml::Table) -> Result<TimelineDef, String> {
        let source = match (str_of(t, "source"), str_of(t, "media")) {
            (Some(_), Some(_)) => return Err("use either `source` or `media`, not both".into()),
            (Some(s), None) => SourceDef::parse(&s)?,
            (None, Some(m)) => SourceDef::parse(&format!("media:{}", m.strip_prefix("media:").unwrap_or(&m)))?,
            (None, None) => SourceDef::Internal,
        };
        let rate = match t.get("fps") {
            None => FrameRate::Fps30,
            Some(toml::Value::String(s)) => FrameRate::parse(s).ok_or_else(|| format!("bad fps `{s}` (24, 25, 29.97df, 30)"))?,
            Some(toml::Value::Integer(i)) => FrameRate::parse(&i.to_string()).ok_or_else(|| format!("bad fps `{i}`"))?,
            Some(toml::Value::Float(f)) => FrameRate::parse(&f.to_string()).ok_or_else(|| format!("bad fps `{f}`"))?,
            Some(v) => return Err(format!("bad fps `{v}`")),
        };
        let priority = match t.get("priority") {
            None => PRIORITY_PRESET,
            Some(toml::Value::Integer(p)) if (1..PRIORITY_MANUAL as i64).contains(p) => *p as u16,
            Some(v) => return Err(format!("priority must be 1..{} (got {v})", PRIORITY_MANUAL - 1)),
        };
        let length = t.get("length").map(|v| parse_time(v, rate)).transpose()?;
        let mut chase = if source.is_timecode() { ChaseConfig::timecode(rate) } else { ChaseConfig::media() };
        if let Some(c) = t.get("chase") {
            let c = c.as_table().ok_or("`chase` must be a table")?;
            chase.jitter = dur_secs(c, "jitter", chase.jitter)?;
            chase.freewheel = dur_secs(c, "freewheel", chase.freewheel)?;
            chase.jump = dur_secs(c, "jump", chase.jump.max(chase.jitter))?.max(chase.jitter);
            chase.dropout = chase.dropout.min(chase.freewheel);
        }
        let snap = match str_of(t, "snap").as_deref() {
            None | Some("off") | Some("none") => Snap::Off,
            Some("beat") | Some("beats") => Snap::Beat,
            Some("bar") | Some("bars") => Snap::Bar,
            Some(s) => return Err(format!("bad snap `{s}` (off, beat, bar)")),
        };
        let (mtc_out, ltc_out) = match t.get("timecode_out") {
            None => (None, false),
            Some(v) => {
                let o = v.as_table().ok_or("`timecode_out` must be a table")?;
                (str_of(o, "mtc"), bool_of(o, "ltc", false)?)
            }
        };
        let scrub = str_of(t, "scrub");
        let scrub_range = match t.get("scrub_range") {
            Some(v) => parse_time(v, rate)?,
            None => length.unwrap_or(0.0),
        };
        if scrub.is_some() && (source != SourceDef::Manual || scrub_range <= 0.0) {
            return Err("`scrub` needs `source = \"manual\"` and a `length` or `scrub_range`".into());
        }
        let mut tracks = Vec::new();
        if let Some(v) = t.get("cues") {
            let mut cues = tables(v, "cues")?.into_iter().map(|c| parse_cue(c, rate, "cues")).collect::<Result<Vec<_>, _>>()?;
            cues.sort_by(|a, b| a.at.total_cmp(&b.at));
            tracks.push(TrackDef { name: "cues".into(), mute: false, kind: TrackKind::Cues { cues } });
        }
        if let Some(v) = t.get("track") {
            for (i, tt) in tables(v, "track")?.into_iter().enumerate() {
                tracks.push(parse_track(tt, rate, i)?);
            }
        }
        let mut seen = BTreeSet::new();
        for tr in &tracks {
            if !seen.insert(tr.name.as_str()) {
                return Err(format!("duplicate track name `{}`", tr.name));
            }
        }
        if !address::is_valid(&format!("timeline.{name}"), false) {
            return Err(format!("timeline name `{name}` is not a valid address segment"));
        }
        Ok(TimelineDef {
            name: name.to_string(),
            file: file.to_string(),
            label: str_of(t, "label"),
            source,
            rate,
            priority,
            length,
            looping: bool_of(t, "loop", false)?,
            enabled: bool_of(t, "enabled", true)?,
            chase,
            release_on_stop: bool_of(t, "release_on_stop", false)?,
            snap,
            analysis: str_of(t, "analysis"),
            mtc_out,
            ltc_out,
            scrub,
            scrub_range,
            record_track: str_of(t, "record_track").unwrap_or_else(|| "recorded".into()),
            tracks,
        })
    }

    /// Time of the last event in any track (for UI extents).
    pub fn extent(&self) -> f64 {
        let mut e = self.length.unwrap_or(0.0);
        for t in &self.tracks {
            match &t.kind {
                TrackKind::Cues { cues } => e = cues.iter().fold(e, |m, c| m.max(c.at)),
                TrackKind::Automation { lane } => e = lane.keys.iter().fold(e, |m, k| m.max(k.at)),
                TrackKind::Regions { regions } => e = regions.iter().fold(e, |m, r| m.max(r.end)),
            }
        }
        e
    }
}

/// Evaluate an automation lane at `t` (holds the first/last key outside the keyed span).
pub fn lane_value(keys: &[KeyDef], t: f64) -> Option<Value> {
    let first = keys.first()?;
    if t <= first.at {
        return Some(first.value.clone());
    }
    let last = keys.last()?;
    if t >= last.at {
        return Some(last.value.clone());
    }
    let i = keys.partition_point(|k| k.at <= t) - 1;
    let (a, b) = (&keys[i], &keys[i + 1]);
    let span = b.at - a.at;
    if span <= 0.0 {
        return Some(b.value.clone());
    }
    let p = a.curve.apply((t - a.at) / span);
    Some(lerp_value(&a.value, &b.value, p))
}

/// Parse all timelines of a configuration (errors per file).
pub fn parse_all(config: &Config) -> (BTreeMap<String, TimelineDef>, Vec<ConfigError>) {
    let mut defs = BTreeMap::new();
    let mut errs = Vec::new();
    for (name, t) in config.other.get("timelines").into_iter().flatten() {
        let file = config.files.get(&format!("timelines/{name}")).cloned().unwrap_or_else(|| format!("timelines/{name}.toml"));
        match TimelineDef::parse(name, &file, t) {
            Ok(d) => {
                defs.insert(name.clone(), d);
            }
            Err(msg) => errs.push(ConfigError { file, msg }),
        }
    }
    (defs, errs)
}

// ---- runtime ------------------------------------------------------------------------------

/// Persisted transport state (crash recovery).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersistedTimeline {
    pub name: String,
    pub time: f64,
    pub playing: bool,
    pub armed: bool,
}

struct SourceRt {
    chase: Chase,
    /// Bumped on every jump (timelines compare against their last seen value).
    jumps: u64,
}

#[derive(Default)]
struct Recording {
    from: f64,
    arm: Vec<String>,
    ids: Vec<usize>,
    ids_gen: u64,
    last: Vec<Value>,
    cues: Vec<(f64, Vec<String>, Option<String>)>,
    keys: BTreeMap<String, Vec<(f64, Value)>>,
}

struct TimelineRt {
    def: TimelineDef,
    armed: bool,
    active: bool,
    playing: bool,
    anchor_ts: Ts,
    anchor_pos: f64,
    /// A locate/jog/stop moved the playhead: apply tracked state.
    relocated: bool,
    pos: f64,
    fired_upto: f64,
    seen_jumps: u64,
    /// Tracked keys currently applied by this timeline.
    applied: BTreeSet<String>,
    regions_on: Vec<Vec<bool>>,
    /// Per track: cached target ids (with state generation) and last written value.
    lane_ids: Vec<(u64, Vec<usize>)>,
    lane_last: Vec<Option<Value>>,
    recording: Option<Recording>,
    status: &'static str,
    /// Rate for timecode labels: the source's (MTC/LTC) once known, else the file's.
    rate: FrameRate,
    ended: bool,
}

impl TimelineRt {
    fn new(def: TimelineDef) -> Self {
        let n = def.tracks.len();
        TimelineRt {
            armed: def.enabled && def.source != SourceDef::Internal,
            active: false,
            playing: false,
            anchor_ts: 0,
            anchor_pos: 0.0,
            relocated: false,
            pos: 0.0,
            fired_upto: f64::NEG_INFINITY,
            seen_jumps: 0,
            applied: BTreeSet::new(),
            regions_on: def.tracks.iter().map(|t| if let TrackKind::Regions { regions } = &t.kind { vec![false; regions.len()] } else { Vec::new() }).collect(),
            lane_ids: vec![(u64::MAX, Vec::new()); n],
            lane_last: vec![None; n],
            recording: None,
            status: "idle",
            rate: def.rate,
            ended: false,
            def,
        }
    }

    fn key(&self) -> String {
        format!("timeline:{}", self.def.name)
    }

    fn transport_pos(&self, now: Ts) -> f64 {
        if self.playing { self.anchor_pos + (now as i128 - self.anchor_ts as i128) as f64 / 1e9 } else { self.anchor_pos }
    }
}

/// All timelines of the project plus the chased sources they follow.
#[derive(Default)]
pub struct Timelines {
    rts: BTreeMap<String, TimelineRt>,
    sources: BTreeMap<String, SourceRt>,
    /// Transport actions, applied at the start of the next timeline pass.
    pending: Vec<(String, Value)>,
    /// Last `song.position` value converted into an observation.
    sig_media: Option<f32>,
    /// Last scrub signal values converted into observations.
    sig_scrub: BTreeMap<String, f32>,
    /// Last media position observed (paused position).
    media_pos: f64,
}

/// Transport actions handled inside the core (the rest of `timeline.*` goes to the I/O glue).
pub fn is_transport(name: &str) -> bool {
    matches!(
        name,
        "timeline.play" | "timeline.pause" | "timeline.toggle" | "timeline.stop" | "timeline.locate" | "timeline.jog" | "timeline.record" | "timeline.tap"
    )
}

/// Manual origins whose commands become cues in record mode.
fn recordable_origin(o: Origin) -> bool {
    matches!(o, Origin::Ui | Origin::Cli | Origin::Api | Origin::Deck | Origin::Midi | Origin::Voice | Origin::Osc)
}

impl Timelines {
    /// Feed an observation (from `Input::Timecode`).
    pub fn observe(&mut self, source: &str, obs: &TcObs) {
        if let Some(name) = source.strip_prefix("scrub:") {
            if let Some(rt) = self.rts.get_mut(name) {
                let jump = (obs.seconds - rt.anchor_pos).abs() > rt.def.chase.jump;
                rt.anchor_pos = obs.seconds.max(0.0);
                rt.relocated |= jump;
            }
            return;
        }
        if source == MEDIA {
            self.media_pos = obs.seconds;
        }
        if let Some(s) = self.sources.get_mut(source)
            && matches!(s.chase.observe(obs), ChaseEvent::Jump { .. })
        {
            s.jumps += 1;
        }
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.rts.keys().map(String::as_str)
    }

    pub fn def(&self, name: &str) -> Option<&TimelineDef> {
        self.rts.get(name).map(|r| &r.def)
    }

    fn recording(&self) -> bool {
        self.rts.values().any(|r| r.recording.is_some())
    }
}

/// Parseable one-line text for a recorded command (round-trips through `Op::parse`).
pub fn op_text(op: &Op) -> Option<String> {
    let kv = |v: &Value| -> String {
        match v.as_map() {
            Some(m) => m.iter().map(|(k, v)| format!(" {k}={}", value_text(v))).collect(),
            None => String::new(),
        }
    };
    Some(match op {
        Op::Set { address, value } => format!("set {address} {}", value_text(value)),
        Op::Animate { address, to, ms, ease } => {
            let e = serde_json::to_value(ease).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
            format!("animate {address} {} {ms}ms {e}", value_text(to))
        }
        Op::Trigger { address, payload } => format!("trigger {address}{}", kv(payload)),
        Op::Release { address } => format!("release {address}"),
        Op::SceneGo { scene } => format!("scene.go {scene}"),
        Op::SceneCut { scene, transition } => format!("scene.cut {scene}{}", transition.as_ref().map(|t| format!(" {t}")).unwrap_or_default()),
        Op::SceneTake { transition, ms } => {
            format!("scene.take{}{}", transition.as_ref().map(|t| format!(" {t}")).unwrap_or_default(), ms.map(|m| format!(" {m}ms")).unwrap_or_default())
        }
        Op::PresetFire { name, payload } => format!("preset.fire {name}{}", kv(payload)),
        Op::PresetRelease { name } => format!("preset.release {name}"),
        Op::ModeSet { mode } => format!("mode.set {mode}"),
        Op::Emit { ty, payload } => format!("emit {ty}{}", kv(payload)),
        Op::Action { name, args } => {
            let mut s = name.clone();
            if let Some(m) = args.as_map() {
                if let Some(pos) = m.get("args").and_then(Value::as_list) {
                    for p in pos {
                        s.push(' ');
                        s.push_str(&value_text(p));
                    }
                }
                for (k, v) in m.iter().filter(|(k, _)| k.as_str() != "args") {
                    s.push_str(&format!(" {k}={}", value_text(v)));
                }
            }
            s
        }
        Op::SetBase { .. } | Op::Panic | Op::Clean | Op::Undo | Op::Redo | Op::Wait { .. } => return None,
    })
}

/// Command-text form of a value (`Value::parse_text` reads it back).
pub fn value_text(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            let s = format!("{f}");
            if s.contains(['.', 'e', 'E', 'n', 'i']) { s } else { format!("{s}.0") }
        }
        Value::Str(s) => {
            let plain = !s.is_empty()
                && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/' | '#'))
                && !matches!(Value::parse_text(s), Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null);
            if plain { s.clone() } else { format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")) }
        }
        Value::List(l) => format!("[{}]", l.iter().map(value_text).collect::<Vec<_>>().join(", ")),
        Value::Map(m) => format!("{{{}}}", m.iter().map(|(k, v)| format!("{k} = {}", value_text(v))).collect::<Vec<_>>().join(", ")),
    }
}

/// Ramer–Douglas–Peucker thinning of recorded numeric keyframes.
fn thin(points: &[(f64, f64)], eps: f64) -> Vec<usize> {
    fn rec(p: &[(f64, f64)], lo: usize, hi: usize, eps: f64, keep: &mut Vec<usize>) {
        if hi <= lo + 1 {
            return;
        }
        let (x0, y0) = p[lo];
        let (x1, y1) = p[hi];
        let mut best = (0.0, lo);
        for (i, &(x, y)) in p.iter().enumerate().take(hi).skip(lo + 1) {
            let yl = if x1 > x0 { y0 + (y1 - y0) * (x - x0) / (x1 - x0) } else { y0 };
            let d = (y - yl).abs();
            if d > best.0 {
                best = (d, i);
            }
        }
        if best.0 > eps {
            rec(p, lo, best.1, eps, keep);
            keep.push(best.1);
            rec(p, best.1, hi, eps, keep);
        }
    }
    if points.len() <= 2 {
        return (0..points.len()).collect();
    }
    let mut keep = vec![0];
    rec(points, 0, points.len() - 1, eps, &mut keep);
    keep.push(points.len() - 1);
    keep
}

fn cuelist_of(args: &Value) -> String {
    args.get_path("cuelist").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from).unwrap_or_else(|| {
        // `lights.cue <list>`: the positional/`cue` argument names the list
        args.get_path("args.0").or_else(|| args.get_path("cue")).map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).unwrap_or_default()
    })
}

// ---- core integration ----------------------------------------------------------------------

impl Core {
    /// (Re)load timeline definitions; the previous definition stays live for broken files.
    pub(super) fn configure_timelines(&mut self, config: &Config) -> Vec<ConfigError> {
        let (mut defs, errs) = parse_all(config);
        for e in &errs {
            if let Some((_, rt)) = self.timelines.rts.iter().find(|(_, r)| r.def.file == e.file) {
                defs.insert(rt.def.name.clone(), rt.def.clone());
            }
            self.log("error", format!("{}: {}", e.file, e.msg));
        }
        let mut tl = std::mem::take(&mut self.timelines);
        let gone: Vec<String> = tl.rts.keys().filter(|n| !defs.contains_key(*n)).cloned().collect();
        for n in gone {
            if let Some(mut rt) = tl.rts.remove(&n) {
                self.tl_release(&mut rt);
            }
            self.state.remove_prefix(&format!("timeline.{n}"));
        }
        for (name, def) in defs {
            match tl.rts.get_mut(&name) {
                Some(rt) if rt.def == def => {}
                Some(rt) => {
                    // changed definition: drop its state and re-apply at the current time
                    let was_active = rt.active;
                    self.tl_release(rt);
                    let mut fresh = TimelineRt::new(def);
                    fresh.armed = rt.armed || (fresh.def.enabled && fresh.def.source != SourceDef::Internal && rt.def.source != fresh.def.source);
                    fresh.playing = rt.playing;
                    fresh.anchor_ts = rt.anchor_ts;
                    fresh.anchor_pos = rt.anchor_pos;
                    fresh.recording = rt.recording.take();
                    fresh.relocated = was_active;
                    *rt = fresh;
                }
                None => {
                    tl.rts.insert(name.clone(), TimelineRt::new(def));
                }
            }
            self.declare_timeline_state(&name);
        }
        // chased sources: one chase per source key, configured by the first timeline using it
        let mut wanted: BTreeMap<String, ChaseConfig> = BTreeMap::new();
        for rt in tl.rts.values() {
            if rt.def.source.chased() {
                wanted.entry(rt.def.source.key()).or_insert(rt.def.chase);
            }
        }
        tl.sources.retain(|k, _| wanted.contains_key(k));
        for (k, cfg) in wanted {
            match tl.sources.get_mut(&k) {
                Some(s) => s.chase.cfg = cfg,
                None => {
                    tl.sources.insert(k, SourceRt { chase: Chase::new(cfg), jumps: 0 });
                }
            }
        }
        self.timelines = tl;
        errs
    }

    fn declare_timeline_state(&mut self, name: &str) {
        let a = |f: &str| format!("timeline.{name}.{f}");
        let ro = |m: Meta, d: &str| m.readonly().owner("timelines").describe(d);
        self.state.declare(&a("time"), ro(Meta::float(0.0, [0.0, 86_400.0]).unit("s"), "Timeline position (source time)"));
        self.state.declare(&a("playing"), ro(Meta::boolean(false), "Position is advancing"));
        self.state.declare(&a("active"), ro(Meta::boolean(false), "Timeline is applying its cues/automation"));
        self.state.declare(&a("locked"), ro(Meta::boolean(false), "Locked to its timecode source"));
        self.state.declare(&a("recording"), ro(Meta::boolean(false), "Record mode"));
        self.state.declare(&a("status"), ro(Meta::enumeration("idle", STATUSES), "Transport / chase status"));
        self.state.declare(&a("source"), ro(Meta::string(""), "Timecode source"));
        self.state.declare(&a("timecode"), ro(Meta::string("00:00:00:00"), "Position as timecode"));
        self.state.declare(&a("length"), ro(Meta::float(0.0, [0.0, 86_400.0]).unit("s"), "Length (0 = open)"));
    }

    /// Queue a transport action (`timeline.play|pause|toggle|stop|locate|jog|record|tap`).
    pub(super) fn timeline_action(&mut self, name: &str, args: &Value) -> Result<(), String> {
        let target = args.get_path("name").or_else(|| args.get_path("args.0")).and_then(Value::as_str).map(String::from);
        if !self.timelines.rts.is_empty() {
            match target.as_deref() {
                None | Some("*") if name == "timeline.tap" || target.is_some() => {}
                None => return Err(format!("`{name}` needs a timeline name")),
                Some(t) if !self.timelines.rts.contains_key(t) => return Err(format!("unknown timeline `{t}`")),
                Some(t) => {
                    let rt = &self.timelines.rts[t];
                    let chased = rt.def.source.chased();
                    if chased && matches!(name, "timeline.locate" | "timeline.jog" | "timeline.pause" | "timeline.toggle") {
                        return Err(format!("timeline `{t}` follows {}; transport comes from the source", rt.def.source.label()));
                    }
                }
            }
        }
        self.timelines.pending.push((name.to_string(), args.clone()));
        Ok(())
    }

    fn run_timeline_action(&mut self, tl: &mut Timelines, name: &str, args: &Value, now: Ts) {
        let target = args.get_path("name").or_else(|| args.get_path("args.0")).and_then(Value::as_str).unwrap_or("*").to_string();
        let names: Vec<String> = if target == "*" { tl.rts.keys().cloned().collect() } else { vec![target.clone()] };
        if names.is_empty() || (target != "*" && !tl.rts.contains_key(&target)) {
            self.log("warn", format!("{name}: unknown timeline `{target}`"));
            return;
        }
        for n in names {
            let Some(rt) = tl.rts.get_mut(&n) else { continue };
            let chased = rt.def.source.chased();
            let rate = rt.def.rate;
            match name {
                "timeline.play" => {
                    if chased {
                        rt.armed = true;
                    } else {
                        if !rt.playing {
                            if rt.def.length.is_some_and(|l| rt.anchor_pos >= l - EPS) {
                                rt.anchor_pos = 0.0;
                                rt.relocated = true;
                            }
                            rt.anchor_ts = now;
                        }
                        rt.armed = true;
                        rt.playing = rt.def.source == SourceDef::Internal;
                        rt.ended = false;
                    }
                }
                "timeline.pause" if !chased => {
                    rt.anchor_pos = rt.transport_pos(now);
                    rt.playing = false;
                }
                "timeline.toggle" if !chased => {
                    if rt.playing {
                        rt.anchor_pos = rt.transport_pos(now);
                        rt.playing = false;
                    } else {
                        rt.anchor_ts = now;
                        rt.armed = true;
                        rt.playing = rt.def.source == SourceDef::Internal;
                    }
                }
                "timeline.stop" => {
                    rt.armed = false;
                    if !chased {
                        rt.playing = false;
                        rt.anchor_pos = 0.0;
                        rt.anchor_ts = now;
                    }
                }
                "timeline.locate" | "timeline.jog" if !chased => {
                    let cur = rt.transport_pos(now);
                    let jog = name == "timeline.jog";
                    let raw = args.get_path(if jog { "delta" } else { "time" }).or_else(|| args.get_path("args.1"));
                    let parsed = match raw {
                        // `jog` takes a signed delta: "-2s", -0.5
                        Some(Value::Str(s)) if jog && s.trim_start().starts_with('-') => {
                            parse_time_str(s.trim_start().trim_start_matches('-'), rate).map(|d| -d)
                        }
                        Some(v) => parse_time_value(v, rate),
                        None => Err("needs a time".into()),
                    };
                    let v = match parsed {
                        Ok(v) if jog => cur + v,
                        Ok(v) => v,
                        Err(e) => {
                            self.log("warn", format!("{name} {n}: {e}"));
                            continue;
                        }
                    };
                    let v = match rt.def.length {
                        Some(l) => v.clamp(0.0, l),
                        None => v.max(0.0),
                    };
                    rt.relocated |= (v - cur).abs() > rt.def.chase.jump || v < cur;
                    rt.anchor_pos = v;
                    rt.anchor_ts = now;
                    rt.ended = false;
                    if !rt.armed && rt.def.source == SourceDef::Manual {
                        rt.armed = true;
                    }
                }
                "timeline.record" => {
                    let on = args.get_path("on").map(Value::truthy).unwrap_or(rt.recording.is_none());
                    if on && rt.recording.is_none() {
                        let arm: Vec<String> = match args.get_path("arm") {
                            Some(Value::List(l)) => l.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
                            Some(Value::Str(s)) => s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
                            _ => rt
                                .def
                                .tracks
                                .iter()
                                .filter_map(|t| if let TrackKind::Automation { lane } = &t.kind { Some(lane.address.clone()) } else { None })
                                .collect(),
                        };
                        rt.recording = Some(Recording { from: rt.pos, arm, ids_gen: u64::MAX, ..Default::default() });
                        self.events
                            .push_back((Event::new("timeline.record.started", Origin::Timeline, Value::map().with("timeline", n.clone())), Ctx::default()));
                    } else if !on && let Some(rec) = rt.recording.take() {
                        self.commit_recording(rt, rec);
                    }
                }
                "timeline.tap" => {
                    let pos = rt.pos;
                    let active = rt.active;
                    if let Some(rec) = rt.recording.as_mut()
                        && active
                    {
                        let label = args.get_path("label").and_then(Value::as_str).map(String::from);
                        let cmds: Vec<String> = match args.get_path("do") {
                            Some(Value::Str(s)) => vec![s.clone()],
                            Some(Value::List(l)) => l.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
                            _ => Vec::new(),
                        };
                        rec.cues.push((pos, cmds, label.or_else(|| Some(format!("tap {}", rec.cues.len() + 1)))));
                    }
                }
                _ => self.log("warn", format!("{name}: not available for timeline `{n}` ({})", rt.def.source.label())),
            }
        }
    }

    /// Record-mode capture of a manual command (called before it executes).
    pub(super) fn timelines_capture(&mut self, cmd: &Command) {
        if !self.timelines.recording() || !recordable_origin(cmd.origin) {
            return;
        }
        if let Op::Action { name, .. } = &cmd.op
            && name.starts_with("timeline.")
        {
            return;
        }
        let Some(text) = op_text(&cmd.op) else { return };
        let target = match &cmd.op {
            Op::Set { address, .. } | Op::Animate { address, .. } => Some(address.as_str()),
            _ => None,
        };
        for rt in self.timelines.rts.values_mut() {
            let (pos, active) = (rt.pos, rt.active);
            let Some(rec) = rt.recording.as_mut() else { continue };
            if !active || target.is_some_and(|a| rec.arm.iter().any(|p| address::matches(p, a))) {
                continue;
            }
            rec.cues.push((pos, vec![text.clone()], None));
        }
    }

    fn commit_recording(&mut self, rt: &mut TimelineRt, rec: Recording) {
        let to = rt.pos;
        let mut lanes = Vec::new();
        let mut nkeys = 0;
        for (addr, pts) in &rec.keys {
            let numeric: Option<Vec<(f64, f64)>> =
                pts.iter().map(|(t, v)| if matches!(v, Value::Bool(_)) { None } else { v.as_f64().map(|x| (*t, x)) }).collect();
            let keep: Vec<usize> = match &numeric {
                Some(n) => {
                    let range =
                        self.state.id(addr).and_then(|i| self.state.param(i).meta.range).map(|r| (r[1] - r[0]).abs()).filter(|w| *w > 0.0).unwrap_or(1.0);
                    thin(n, range * 0.002)
                }
                None => (0..pts.len()).collect(),
            };
            nkeys += keep.len();
            let keys: Vec<Value> = keep.iter().map(|&i| Value::map().with("at", pts[i].0).with("value", pts[i].1.clone())).collect();
            lanes.push(Value::map().with("address", addr.clone()).with("keys", keys));
        }
        let cues: Vec<Value> = rec
            .cues
            .iter()
            .map(|(at, cmds, label)| Value::map().with("at", *at).with("do", cmds.clone()).with("label", label.clone().map(Value::Str).unwrap_or_default()))
            .collect();
        let payload = Value::map()
            .with("timeline", rt.def.name.clone())
            .with("file", rt.def.file.clone())
            .with("track", rt.def.record_track.clone())
            .with("from", rec.from)
            .with("to", to)
            .with("cues", cues.len())
            .with("keys", nkeys);
        let args = payload.clone().with("cues", cues).with("lanes", lanes);
        self.outbox.push(Output::Action(Command::new(Origin::Timeline, Op::Action { name: "timeline.record.commit".into(), args })));
        self.events.push_back((Event::new("timeline.recorded", Origin::Timeline, payload), Ctx::default()));
    }

    /// Turn signal-driven positions into recorded observations (signals are not replayed).
    fn timeline_signal_obs(&mut self, tl: &mut Timelines, now: Ts) {
        let mut out: Vec<(String, TcObs)> = Vec::new();
        if tl.sources.contains_key(MEDIA)
            && let Some(v) = self.signals.get(SONG_POSITION)
            && tl.sig_media != Some(v)
        {
            tl.sig_media = Some(v);
            let playing = self.state.get(SONG_STATE).and_then(Value::as_str).is_none_or(|s| s == "playing");
            let kind = if playing { ObsKind::Run } else { ObsKind::Locate };
            out.push((MEDIA.to_string(), TcObs { seconds: v as f64, ts: now, kind, rate: None }));
        }
        for rt in tl.rts.values() {
            let Some(sig) = &rt.def.scrub else { continue };
            let Some(v) = self.signals.get(sig) else { continue };
            if tl.sig_scrub.get(&rt.def.name) == Some(&v) {
                continue;
            }
            tl.sig_scrub.insert(rt.def.name.clone(), v);
            out.push((
                format!("scrub:{}", rt.def.name),
                TcObs { seconds: v.clamp(0.0, 1.0) as f64 * rt.def.scrub_range, ts: now, kind: ObsKind::Locate, rate: None },
            ));
        }
        for (source, obs) in out {
            self.applied.push((self.tick, Input::Timecode { source: source.clone(), obs }));
            tl.observe(&source, &obs);
        }
    }

    /// One timeline pass (every tick, after signals, before bindings).
    pub(super) fn tick_timelines(&mut self, now: Ts) {
        if self.timelines.rts.is_empty() && self.timelines.pending.is_empty() {
            return;
        }
        let mut tl = std::mem::take(&mut self.timelines);
        self.timeline_signal_obs(&mut tl, now);
        for (name, args) in std::mem::take(&mut tl.pending) {
            self.run_timeline_action(&mut tl, &name, &args, now);
        }
        // media pauses come from replayed state, so they're derived here, not recorded
        if let Some(src) = tl.sources.get_mut(MEDIA)
            && src.chase.running()
            && self.state.get(SONG_STATE).and_then(Value::as_str).is_some_and(|s| s != "playing")
        {
            src.chase.observe(&TcObs { seconds: tl.media_pos, ts: now, kind: ObsKind::Stop, rate: None });
        }
        for src in tl.sources.values_mut() {
            src.chase.tick(now);
        }
        let media_now = self.state.get(SONG_MEDIA).and_then(Value::as_str).unwrap_or("").to_string();
        let names: Vec<String> = tl.rts.keys().cloned().collect();
        for n in names {
            let Some(mut rt) = tl.rts.remove(&n) else { continue };
            let src = tl.sources.get(&rt.def.source.key());
            self.tick_timeline(&mut rt, src, &media_now, now);
            tl.rts.insert(n, rt);
        }
        // actions queued by cues during this pass run next tick
        let queued = std::mem::take(&mut self.timelines.pending);
        tl.pending.extend(queued);
        self.timelines = tl;
    }

    fn tick_timeline(&mut self, rt: &mut TimelineRt, src: Option<&SourceRt>, media_now: &str, now: Ts) {
        let def_source = rt.def.source.clone();
        let mut jumped = std::mem::take(&mut rt.relocated);
        let mut ended = false;
        let mut wrapped_from = None;
        let (pos, running, status, want) = match &def_source {
            SourceDef::Internal => {
                let mut pos = rt.transport_pos(now);
                if rt.playing
                    && let Some(len) = rt.def.length
                    && pos >= len
                {
                    if rt.def.looping && len > 0.0 {
                        wrapped_from = Some(len);
                        pos = pos.rem_euclid(len);
                        rt.anchor_pos = pos;
                        rt.anchor_ts = now;
                    } else {
                        pos = len;
                        rt.anchor_pos = len;
                        rt.playing = false;
                        ended = !rt.ended;
                        rt.ended = true;
                    }
                }
                let status = if !rt.armed {
                    "stopped"
                } else if rt.playing {
                    "playing"
                } else {
                    "paused"
                };
                (pos, rt.playing, status, rt.armed)
            }
            SourceDef::Manual => (rt.anchor_pos, false, if rt.armed { "paused" } else { "stopped" }, rt.armed),
            SourceDef::Media(id) | SourceDef::Mtc(id) | SourceDef::Ltc(id) => {
                let Some(src) = src else { return };
                let st = src.chase.status();
                rt.rate = src.chase.rate().unwrap_or(rt.def.rate);
                if src.jumps != rt.seen_jumps {
                    rt.seen_jumps = src.jumps;
                    jumped = true;
                }
                let bound = !matches!(def_source, SourceDef::Media(_)) || media_now == id;
                let holding = matches!(st, ChaseStatus::Stopped | ChaseStatus::Lost);
                let want = rt.armed && bound && st != ChaseStatus::Idle && !(holding && rt.def.release_on_stop);
                let status = if !rt.armed {
                    "disabled"
                } else if !bound {
                    "waiting"
                } else {
                    st.as_str()
                };
                (src.chase.position(now), src.chase.running(), status, want)
            }
        };
        rt.status = status;
        let prev = rt.pos;
        rt.pos = pos;
        if !want {
            if rt.active {
                self.tl_release(rt);
                rt.active = false;
                self.tl_event(rt, "timeline.stopped", Value::map());
            }
            self.publish_timeline(rt, running);
            return;
        }
        if !rt.active {
            rt.active = true;
            self.tl_event(rt, "timeline.started", Value::map().with("time", pos));
            if pos <= rt.def.chase.jump.max(0.25) && !jumped {
                // started from (about) the top: everything up to here fires normally
                rt.fired_upto = f64::NEG_INFINITY;
            } else {
                self.apply_tracked(rt, pos);
            }
        } else if jumped {
            self.tl_event(rt, "timeline.jump", Value::map().with("from", prev).with("to", pos));
            self.apply_tracked(rt, pos);
        }
        if let Some(len) = wrapped_from {
            self.fire_cues(rt, len);
            rt.fired_upto = f64::NEG_INFINITY;
            self.tl_event(rt, "timeline.looped", Value::map());
        }
        if pos > rt.fired_upto {
            self.fire_cues(rt, pos);
        }
        self.tick_regions(rt, pos);
        self.tick_lanes(rt, pos);
        if running || rt.def.source == SourceDef::Manual {
            self.record_sample(rt, pos);
        }
        if ended {
            self.tl_event(rt, "timeline.ended", Value::map().with("time", pos));
        }
        self.publish_timeline(rt, running);
    }

    fn tl_ctx(&self, rt: &TimelineRt, parent: Option<se_proto::Id>) -> Ctx {
        Ctx { key: Some(rt.key()), priority: Some(rt.def.priority), parent, ..Default::default() }
    }

    fn tl_event(&mut self, rt: &TimelineRt, ty: &str, payload: Value) {
        let e = Event::new(ty, Origin::Timeline, payload.with("timeline", rt.def.name.clone()));
        self.events.push_back((e, Ctx::default()));
    }

    /// Fire every cue in (fired_upto, upto].
    fn fire_cues(&mut self, rt: &mut TimelineRt, upto: f64) {
        let from = rt.fired_upto;
        let mut due: Vec<(f64, usize, usize)> = Vec::new();
        for (ti, t) in rt.def.tracks.iter().enumerate() {
            if t.mute {
                continue;
            }
            if let TrackKind::Cues { cues } = &t.kind {
                let lo = cues.partition_point(|c| c.at <= from);
                for (ci, c) in cues.iter().enumerate().skip(lo) {
                    if c.at > upto {
                        break;
                    }
                    due.push((c.at, ti, ci));
                }
            }
        }
        rt.fired_upto = upto;
        due.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        let now = self.now();
        for (at, ti, ci) in due {
            let (track, cue) = match &rt.def.tracks[ti].kind {
                TrackKind::Cues { cues } => (rt.def.tracks[ti].name.clone(), cues[ci].clone()),
                _ => continue,
            };
            let id = next_id();
            self.trace.add(
                id,
                None,
                now,
                "timeline",
                format!("{} {} @{}{}", rt.def.name, track, format_time(at), cue.label.as_ref().map(|l| format!(" ({l})")).unwrap_or_default()),
            );
            for text in &cue.commands {
                if let Ok(op) = self.template_op(text, None)
                    && let Some(k) = track_key(&op, text, cue.track, &self.config)
                {
                    if is_release(&op) {
                        rt.applied.remove(&k);
                    } else {
                        rt.applied.insert(k);
                    }
                }
            }
            let ctx = self.tl_ctx(rt, Some(id));
            self.run_list(&cue.commands, Origin::Timeline, ctx);
            let payload = Value::map()
                .with("timeline", rt.def.name.clone())
                .with("track", track)
                .with("cue", ci as i64)
                .with("at", at)
                .with("label", cue.label.clone().map(Value::Str).unwrap_or_default());
            let e = Event::new("timeline.cue", Origin::Timeline, payload).with_causal(Some(id));
            self.events.push_back((e, Ctx { parent: Some(id), ..Default::default() }));
        }
    }

    /// Jump semantics: re-apply the latest stateful command per target before `pos`, revert
    /// targets no earlier cue touched, and leave cues at exactly `pos` to fire normally.
    fn apply_tracked(&mut self, rt: &mut TimelineRt, pos: f64) {
        let mut all: Vec<(f64, usize, usize, usize, String, Op)> = Vec::new();
        for (ti, t) in rt.def.tracks.iter().enumerate() {
            if t.mute {
                continue;
            }
            let TrackKind::Cues { cues } = &t.kind else { continue };
            for (ci, c) in cues.iter().enumerate() {
                if c.at >= pos - EPS {
                    break;
                }
                for (k, text) in c.commands.iter().enumerate() {
                    let Ok(op) = self.template_op(text, None) else { continue };
                    if let Some(key) = track_key(&op, text, c.track, &self.config) {
                        all.push((c.at, ti, ci, k, key, op));
                    }
                }
            }
        }
        all.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
        let mut winners: BTreeMap<String, (usize, Op)> = BTreeMap::new();
        for (i, (_, _, _, _, key, op)) in all.into_iter().enumerate() {
            winners.insert(key, (i, op));
        }
        let id = next_id();
        let now = self.now();
        self.trace.add(id, None, now, "timeline", format!("{} tracked state at {}", rt.def.name, format_time(pos)));
        let ctx = self.tl_ctx(rt, Some(id));
        // revert what the timeline set that no longer applies (or whose latest op is a release)
        let stale: Vec<String> = rt.applied.iter().filter(|k| winners.get(*k).is_none_or(|(_, op)| is_release(op))).cloned().collect();
        for k in stale {
            self.revert_key(rt, &k, &ctx);
            rt.applied.remove(&k);
        }
        let mut order: Vec<(usize, String, Op)> = winners.into_iter().map(|(k, (i, op))| (i, k, op)).collect();
        order.sort_by_key(|x| x.0);
        for (_, key, op) in order {
            if is_release(&op) {
                continue;
            }
            let op = snap_op(op);
            self.exec_traced(&op, Origin::Timeline, &ctx);
            rt.applied.insert(key);
        }
        rt.fired_upto = pos - EPS;
        // lanes rewrite their value next pass
        rt.lane_last.fill(None);
    }

    fn revert_key(&mut self, rt: &TimelineRt, key: &str, ctx: &Ctx) {
        if let Some(a) = key.strip_prefix("addr:") {
            if let Some(i) = self.state.id(a) {
                self.state.remove_override(i, &rt.key());
            }
        } else if let Some(cl) = key.strip_prefix("lights:") {
            let args = Value::map().with("cuelist", cl).with("fade", 0);
            self.exec_traced(&Op::Action { name: "lights.release".into(), args }, Origin::Timeline, ctx);
        } else if let Some(p) = key.strip_prefix("preset:")
            && self.presets.iter().any(|x| x.name == p)
        {
            self.release_preset(p, ctx.parent, true);
        }
        // scene, preview, mode, mixer snapshot, and forced momentary commands have no "off"
    }

    fn tick_regions(&mut self, rt: &mut TimelineRt, pos: f64) {
        for ti in 0..rt.def.tracks.len() {
            let t = &rt.def.tracks[ti];
            let TrackKind::Regions { regions } = &t.kind else { continue };
            let mute = t.mute;
            for ri in 0..regions.len() {
                let r = match &rt.def.tracks[ti].kind {
                    TrackKind::Regions { regions } => regions[ri].clone(),
                    _ => continue,
                };
                let want = !mute && pos >= r.start && pos < r.end;
                if rt.regions_on[ti][ri] != want {
                    rt.regions_on[ti][ri] = want;
                    self.region_edge(rt, &r, want);
                }
            }
        }
    }

    fn region_edge(&mut self, rt: &TimelineRt, r: &RegionDef, on: bool) {
        let id = next_id();
        let now = self.now();
        self.trace.add(
            id,
            None,
            now,
            "timeline",
            format!("{} region {} {}", rt.def.name, r.label.clone().unwrap_or_else(|| format_time(r.start)), if on { "on" } else { "off" }),
        );
        let ctx = self.tl_ctx(rt, Some(id));
        match (&r.action, on) {
            (RegionAction::Preset { preset }, true) => self.exec_traced(&Op::PresetFire { name: preset.clone(), payload: Value::Null }, Origin::Timeline, &ctx),
            (RegionAction::Preset { preset }, false) => {
                if self.presets.iter().any(|p| &p.name == preset) {
                    self.release_preset(preset, Some(id), true);
                }
            }
            (RegionAction::Cuelist { cuelist, cue }, true) => {
                let args = match cue {
                    Some(c) => Value::map().with("cuelist", cuelist.clone()).with("cue", c.clone()),
                    None => Value::map().with("cue", cuelist.clone()),
                };
                self.exec_traced(&Op::Action { name: "lights.cue".into(), args }, Origin::Timeline, &ctx);
            }
            (RegionAction::Cuelist { cuelist, .. }, false) => {
                self.exec_traced(&Op::Action { name: "lights.release".into(), args: Value::map().with("cuelist", cuelist.clone()) }, Origin::Timeline, &ctx)
            }
            (RegionAction::Fx { fx }, on) => {
                let a = if fx.contains('.') { fx.clone() } else { format!("fx.{fx}") };
                let op = if on { Op::Trigger { address: a, payload: Value::map().with("hold", FX_HOLD_MS) } } else { Op::Release { address: a } };
                self.exec_traced(&op, Origin::Timeline, &ctx);
            }
            (RegionAction::Commands { on: cmds, .. }, true) | (RegionAction::Commands { off: cmds, .. }, false) => {
                let cmds = cmds.clone();
                self.run_list(&cmds, Origin::Timeline, ctx);
            }
        }
        let payload = Value::map().with("label", r.label.clone().map(Value::Str).unwrap_or_default()).with("start", r.start).with("end", r.end);
        self.tl_event(rt, if on { "timeline.region.on" } else { "timeline.region.off" }, payload);
    }

    fn tick_lanes(&mut self, rt: &mut TimelineRt, pos: f64) {
        let key = rt.key();
        for ti in 0..rt.def.tracks.len() {
            let TrackKind::Automation { lane } = &rt.def.tracks[ti].kind else { continue };
            if rt.def.tracks[ti].mute {
                continue;
            }
            let Some(v) = lane_value(&lane.keys, pos) else { continue };
            if rt.lane_ids[ti].0 != self.state.generation {
                let ids: Vec<usize> = if address::is_pattern(&lane.address) {
                    self.state.matching(&lane.address).collect()
                } else {
                    vec![self.state.ensure(&lane.address, &super::zero_of(&v))]
                };
                rt.lane_ids[ti] = (self.state.generation, ids);
                rt.lane_last[ti] = None;
            }
            if rt.lane_last[ti].as_ref() == Some(&v) {
                continue;
            }
            let armed = |a: &str| rt.recording.as_ref().is_some_and(|r| r.arm.iter().any(|p| address::matches(p, a)));
            for &i in &rt.lane_ids[ti].1 {
                if armed(&self.state.param(i).addr) {
                    // recording this address: the operator's control must be audible
                    self.state.remove_override(i, &key);
                    continue;
                }
                self.state.put_override(
                    i,
                    Override {
                        key: key.clone(),
                        priority: rt.def.priority,
                        value: v.clone(),
                        seq: 0,
                        expires: None,
                        anim: None,
                        origin: Origin::Timeline,
                        causal: None,
                    },
                );
            }
            rt.lane_last[ti] = Some(v);
        }
    }

    fn record_sample(&mut self, rt: &mut TimelineRt, pos: f64) {
        let Some(rec) = rt.recording.as_mut() else { return };
        if rec.ids_gen != self.state.generation {
            let mut ids: Vec<usize> = Vec::new();
            for p in &rec.arm {
                ids.extend(self.state.matching(p));
            }
            ids.sort_unstable();
            ids.dedup();
            // keep last values for ids that survive
            let old: BTreeMap<String, Value> =
                rec.ids.iter().zip(&rec.last).filter(|(i, _)| **i < self.state.len()).map(|(i, v)| (self.state.param(*i).addr.clone(), v.clone())).collect();
            rec.last = ids.iter().map(|i| old.get(&self.state.param(*i).addr).cloned().unwrap_or(Value::Null)).collect();
            rec.ids = ids;
            rec.ids_gen = self.state.generation;
        }
        for (k, &i) in rec.ids.iter().enumerate() {
            let v = self.state.value(i);
            if &rec.last[k] != v {
                rec.last[k] = v.clone();
                rec.keys.entry(self.state.param(i).addr.clone()).or_default().push((pos, v.clone()));
            }
        }
    }

    /// Remove everything a timeline applied (overrides, regions, tracked lights/presets).
    fn tl_release(&mut self, rt: &mut TimelineRt) {
        let key = rt.key();
        self.state.remove_overrides_where(|o| o.key == key);
        let id = next_id();
        let ctx = self.tl_ctx(rt, Some(id));
        for ti in 0..rt.regions_on.len() {
            for ri in 0..rt.regions_on[ti].len() {
                if rt.regions_on[ti][ri] {
                    rt.regions_on[ti][ri] = false;
                    if let TrackKind::Regions { regions } = &rt.def.tracks[ti].kind {
                        let r = regions[ri].clone();
                        self.region_edge(rt, &r, false);
                    }
                }
            }
        }
        for k in std::mem::take(&mut rt.applied) {
            self.revert_key(rt, &k, &ctx);
        }
        // triggers held by the timeline (fx regions/cues)
        let now = self.now();
        for t in self.triggers.values_mut() {
            t.release(Some(&key), now);
        }
        rt.fired_upto = f64::NEG_INFINITY;
        rt.lane_last.fill(None);
    }

    fn publish_timeline(&mut self, rt: &TimelineRt, running: bool) {
        let n = &rt.def.name;
        let tc = Timecode::from_seconds(rt.pos, rt.rate).to_string();
        let vals = [
            ("time", Value::Float((rt.pos * 1000.0).round() / 1000.0)),
            ("playing", Value::Bool(running && rt.active)),
            ("active", Value::Bool(rt.active)),
            ("locked", Value::Bool(rt.status == "locked")),
            ("recording", Value::Bool(rt.recording.is_some())),
            ("status", Value::Str(rt.status.into())),
            ("source", Value::Str(rt.def.source.label())),
            ("timecode", Value::Str(tc)),
            ("length", Value::Float(rt.def.length.unwrap_or(0.0))),
        ];
        for (f, v) in vals {
            self.set_sys(&format!("timeline.{n}.{f}"), v);
        }
    }

    /// Panic: every timeline stops following its source (state is released next pass).
    pub(super) fn timelines_panic(&mut self) {
        for rt in self.timelines.rts.values_mut() {
            rt.armed = false;
            rt.playing = false;
        }
    }

    pub(super) fn timelines_persist(&self) -> Vec<PersistedTimeline> {
        let now = self.now();
        self.timelines
            .rts
            .values()
            .map(|r| PersistedTimeline {
                name: r.def.name.clone(),
                time: if r.def.source.chased() { r.pos } else { r.transport_pos(now) },
                playing: r.playing,
                armed: r.armed,
            })
            .collect()
    }

    pub(super) fn timelines_restore(&mut self, saved: &[PersistedTimeline]) {
        let now = self.now();
        for p in saved {
            let Some(rt) = self.timelines.rts.get_mut(&p.name) else { continue };
            rt.armed = p.armed;
            if !rt.def.source.chased() {
                rt.playing = p.playing && rt.def.source == SourceDef::Internal;
                rt.anchor_pos = p.time;
                rt.anchor_ts = now;
                rt.relocated = p.armed && p.time > 0.0;
            }
        }
    }

    /// `timelines` (all) and `timeline {name}` (one) queries for the UI.
    pub(super) fn query_timelines(&self, name: &str, args: &Value) -> Result<Value, String> {
        let one = |rt: &TimelineRt| -> Value {
            let mut v = Value::from(serde_json::to_value(&rt.def).unwrap_or_default());
            let src = self.timelines.sources.get(&rt.def.source.key());
            v = v
                .with("time", rt.pos)
                .with("status", rt.status)
                .with("active", rt.active)
                .with("playing", rt.playing || src.is_some_and(|s| s.chase.running()))
                .with("recording", rt.recording.is_some())
                .with("extent", rt.def.extent())
                .with("speed", src.map(|s| s.chase.speed()).unwrap_or(if rt.playing { 1.0 } else { 0.0 }))
                .with("source_rate", src.and_then(|s| s.chase.rate()).map(|r| Value::Str(r.as_str().into())).unwrap_or_default())
                .with("regions_on", Value::List(rt.regions_on.iter().map(|t| Value::List(t.iter().map(|b| Value::Bool(*b)).collect())).collect()));
            if let Some(rec) = &rt.recording {
                v = v.with(
                    "record",
                    Value::map()
                        .with("from", rec.from)
                        .with("arm", rec.arm.clone())
                        .with("cues", rec.cues.len())
                        .with("keys", rec.keys.values().map(Vec::len).sum::<usize>()),
                );
            }
            v
        };
        if name == "timeline" {
            let n = args.get_path("name").or_else(|| args.get_path("args.0")).and_then(Value::as_str).ok_or("needs name")?;
            return self.timelines.rts.get(n).map(one).ok_or_else(|| format!("unknown timeline `{n}`"));
        }
        Ok(Value::List(self.timelines.rts.values().map(one).collect()))
    }
}

/// Cue-list playbacks take their priority from the action's `priority` argument: timeline
/// `lights.*` actions carry the timeline's priority unless the command sets one.
pub fn playback_args(name: &str, args: &Value, priority: u16) -> Value {
    if !name.starts_with("lights.") || args.get_path("priority").is_some() {
        return args.clone();
    }
    let base = if args.is_null() { Value::map() } else { args.clone() };
    base.with("priority", priority as i64)
}

fn is_release(op: &Op) -> bool {
    matches!(op, Op::Release { .. } | Op::PresetRelease { .. }) || matches!(op, Op::Action { name, .. } if name == "lights.release")
}

/// On a jump, stateful commands land instantly: animations become sets, cue-list cues
/// become `lights.locate` (tracked, fade 0, no follow timers).
fn snap_op(op: Op) -> Op {
    match op {
        Op::Animate { address, to, .. } => Op::Set { address, value: to },
        Op::Action { name, args } if matches!(name.as_str(), "lights.cue" | "lights.goto") => {
            let cl = args.get_path("cuelist").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
            let cue = args.get_path("cue").or_else(|| args.get_path("args.0")).cloned();
            match (cl, cue) {
                (Some(cl), Some(cue)) => Op::Action { name: "lights.locate".into(), args: Value::map().with("cuelist", cl).with("cue", cue) },
                (_, _) => Op::Action { name, args: args.with("fade", 0) },
            }
        }
        op => op,
    }
}

/// Target key for tracked-state recall (`None` = momentary).
fn track_key(op: &Op, text: &str, mode: TrackMode, config: &Config) -> Option<String> {
    if mode == TrackMode::Never {
        return None;
    }
    let latching = |name: &str| config.presets.get(name).is_some_and(|p| p.hold.is_none() && (!p.set.is_empty() || p.toggle));
    let k = match op {
        Op::Set { address, .. } | Op::Animate { address, .. } | Op::Release { address } => Some(format!("addr:{address}")),
        Op::SceneCut { .. } => Some("scene".into()),
        Op::SceneGo { .. } => Some("preview".into()),
        Op::ModeSet { .. } => Some("mode".into()),
        Op::PresetFire { name, .. } | Op::PresetRelease { name } if latching(name) => Some(format!("preset:{name}")),
        Op::Action { name, args } if matches!(name.as_str(), "lights.cue" | "lights.goto" | "lights.locate" | "lights.release") => {
            Some(format!("lights:{}", cuelist_of(args)))
        }
        Op::Action { name, .. } if name == "mixer.snapshot.recall" => Some("mixer.snapshot".into()),
        _ => None,
    };
    match (k, mode) {
        (None, TrackMode::Always) => Some(format!("cmd:{text}")),
        (k, _) => k,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(src: &str) -> TimelineDef {
        TimelineDef::parse("t", "timelines/t.toml", &src.parse().unwrap()).unwrap()
    }

    #[test]
    fn plan_example_parses() {
        let d = def(
            "media = \"yt:VIDEO_ID\"\ncues = [\n  { at = \"1:12.400\", do = [\"preset.fire chorus_blast\"] },\n  { at = \"1:43.000\", do = [\"lights.cue blackout\"] },\n]",
        );
        assert_eq!(d.source, SourceDef::Media("yt:VIDEO_ID".into()));
        assert_eq!(d.source.label(), "media:yt:VIDEO_ID");
        let TrackKind::Cues { cues } = &d.tracks[0].kind else { panic!() };
        assert_eq!(cues[0].at, 72.4);
        assert_eq!(cues[1].at, 103.0);
    }

    #[test]
    fn times_parse_as_durations_and_timecode() {
        assert_eq!(parse_time_str("1:12.400", FrameRate::Fps30).unwrap(), 72.4);
        assert_eq!(parse_time_str("72.4s", FrameRate::Fps30).unwrap(), 72.4);
        assert_eq!(parse_time_str("01:00:00:15", FrameRate::Fps30).unwrap(), 3600.5);
        assert!((parse_time_str("00:01:00;02", FrameRate::Fps2997Df).unwrap() - 1800.0 / (30000.0 / 1001.0)).abs() < 1e-9);
        assert!(parse_time_str("00:01:00;00", FrameRate::Fps2997Df).is_err(), "dropped label");
        assert_eq!(format_time(72.4), "1:12.400");
        assert_eq!(format_time(3723.5), "1:02:03.500");
        assert_eq!(parse_time_str(&format_time(3723.5), FrameRate::Fps30).unwrap(), 3723.5);
    }

    #[test]
    fn bad_files_report_errors() {
        let bad = |s: &str| TimelineDef::parse("t", "f", &s.parse().unwrap()).unwrap_err();
        assert!(bad("source = \"spotify:x\"").contains("unknown source"));
        assert!(bad("cues = [{ at = \"1s\", do = [\"\"] }]").contains("empty command"));
        assert!(bad("[[track]]\ntype = \"automation\"").contains("address"));
        assert!(bad("[[track]]\nregions = [{ start = \"2s\", end = \"1s\", preset = \"x\" }]").contains("after start"));
        assert!(bad("fps = \"23.976\"").contains("fps"));
    }

    #[test]
    fn lane_interpolation_uses_segment_curves() {
        let keys = vec![
            KeyDef { at: 0.0, value: Value::Float(0.0), curve: Ease::Linear },
            KeyDef { at: 10.0, value: Value::Float(1.0), curve: Ease::Step },
            KeyDef { at: 20.0, value: Value::Float(0.0), curve: Ease::Linear },
        ];
        assert_eq!(lane_value(&keys, -1.0), Some(Value::Float(0.0)));
        assert_eq!(lane_value(&keys, 5.0), Some(Value::Float(0.5)));
        assert_eq!(lane_value(&keys, 15.0), Some(Value::Float(1.0)), "step holds until the next key");
        assert_eq!(lane_value(&keys, 25.0), Some(Value::Float(0.0)));
        let colors = vec![
            KeyDef { at: 0.0, value: Value::from(vec![1.0f64, 0.0, 0.0]), curve: Ease::Linear },
            KeyDef { at: 2.0, value: Value::from(vec![0.0f64, 0.0, 1.0]), curve: Ease::Linear },
        ];
        assert_eq!(lane_value(&colors, 1.0), Some(Value::from(vec![0.5f64, 0.0, 0.5])));
    }

    #[test]
    fn op_text_roundtrips() {
        for t in [
            "preset.fire hype",
            "set fx.rgb_split.amount 0.5",
            "set lights.group.front.color [1.0, 0.5, 0.0]",
            "scene.cut wide morph",
            "lights.cue blackout",
            "lights.cue cuelist=main cue=\"3\"",
            "trigger patch.confetti amount=3",
            "animate fx.vhs.amount 1.0 500ms out_quad",
            "set show.title \"two words\"",
            "bot.say \"hello there\"",
        ] {
            let op = Op::parse(t).unwrap();
            let back = Op::parse(&op_text(&op).unwrap()).unwrap();
            assert_eq!(back, op, "{t} → {}", op_text(&op).unwrap());
        }
        assert!(op_text(&Op::Panic).is_none());
    }

    #[test]
    fn thinning_keeps_shape() {
        let pts: Vec<(f64, f64)> = (0..=100).map(|i| (i as f64 / 10.0, if i <= 50 { i as f64 / 50.0 } else { 1.0 })).collect();
        let keep = thin(&pts, 0.002);
        assert_eq!(keep, vec![0, 50, 100], "ramp + hold collapse to 3 keys");
    }
}
