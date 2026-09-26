//! Timeline I/O glue (§2.7, §9.2). Timelines themselves run inside the deterministic core
//! (`se_core::timeline`); this crate connects them to the outside:
//!
//! * **MTC in** (`source = "mtc[:port]"`): raw MIDI from `se_input::midi` → decoder →
//!   `Input::Timecode` observations (replayable).
//! * **LTC in** (`source = "ltc[:tap]"`): an audio tap from `se_audio_taps` → decoder →
//!   observations stamped with the tap's block clock.
//! * **MTC/LTC out** (`[timecode_out] mtc = "<port>" / ltc = true` in a timeline file):
//!   generators that follow the timeline from the state snapshot on their own threads.
//! * **Record commits and editor writes**: `timeline.record.commit`, `timeline.edit.*`, and
//!   `timeline.create` rewrite `timelines/*.toml` with `toml_edit` (comments preserved); the
//!   project watcher reloads them.
//! * **Beat grid** (`timeline.grid` query) from the Audio slice's offline analysis, used for
//!   snapping.
//!
//! Project settings (`project.toml`):
//! ```toml
//! [timecode]
//! mtc_in = "studio24c"     # MIDI port (device id or ALSA name glob) for `source = "mtc"`
//! ltc_in = "input.ltc.0"    # audio tap for `source = "ltc"` (see [audio.inputs])
//! ltc_level = -12.0         # LTC out level, dBFS
//! ltc_latency = "0ms"       # audio output latency compensation for LTC out
//! mtc_latency = "0ms"       # receiver latency compensation for MTC out
//! ```

pub mod edit;
pub mod follow;
pub mod grid;
pub mod ltc;
pub mod mtc;

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use se_clock::timecode::FrameRate;
use se_core::Config;
use se_core::timeline::{self, Snap, SourceDef, TimelineDef};
use se_hub::{EngineCtx, Hub};
use se_proto::{Op, Value};
use se_store::Project;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Timecode I/O health, published as `health.timecode` for preflight.
pub struct Health {
    hub: Arc<Hub>,
    items: Mutex<BTreeMap<String, (String, String)>>,
    last: Mutex<Option<Value>>,
}

impl Health {
    pub fn new(hub: Arc<Hub>) -> Self {
        Health { hub, items: Mutex::new(BTreeMap::new()), last: Mutex::new(None) }
    }

    pub fn set(&self, key: &str, status: &str, detail: String) {
        let changed = self.items.lock().insert(key.to_string(), (status.to_string(), detail.clone())) != Some((status.to_string(), detail));
        if changed {
            self.publish();
        }
    }

    pub fn clear(&self, key: &str) {
        if self.items.lock().remove(key).is_some() {
            self.publish();
        }
    }

    fn publish(&self) {
        let items = self.items.lock();
        let rank = |s: &str| match s {
            "fail" => 2,
            "warn" => 1,
            _ => 0,
        };
        let worst = items.values().map(|(s, _)| rank(s)).max().unwrap_or(0);
        let status = ["pass", "warn", "fail"][worst];
        let detail =
            if items.is_empty() { "no MTC/LTC in use".to_string() } else { items.iter().map(|(k, (_, d))| format!("{k}: {d}")).collect::<Vec<_>>().join("; ") };
        let v = Value::map().with("status", status).with("detail", detail);
        let mut last = self.last.lock();
        if last.as_ref() != Some(&v) {
            *last = Some(v.clone());
            self.hub.publish("health.timecode", v);
        }
    }
}

/// `[timecode]` settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub mtc_in: String,
    pub ltc_in: String,
    pub ltc_level: f64,
    pub ltc_latency: f64,
    pub mtc_latency: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { mtc_in: "*".into(), ltc_in: "input.ltc.0".into(), ltc_level: -12.0, ltc_latency: 0.0, mtc_latency: 0.0 }
    }
}

impl Settings {
    pub fn from_config(cfg: &Config) -> Result<Settings, String> {
        let mut s = Settings::default();
        let Some(v) = cfg.project.extra.get("timecode") else { return Ok(s) };
        let t = v.as_table().ok_or("[timecode] must be a table")?;
        let secs = |k: &str| -> Result<Option<f64>, String> {
            match t.get(k) {
                None => Ok(None),
                Some(toml::Value::String(x)) => {
                    se_proto::parse_duration_ms(x).map(|ms| Some(ms as f64 / 1000.0)).ok_or_else(|| format!("[timecode] bad {k} `{x}`"))
                }
                Some(toml::Value::Integer(i)) => Ok(Some(*i as f64 / 1000.0)),
                Some(x) => Err(format!("[timecode] bad {k} `{x}`")),
            }
        };
        if let Some(x) = t.get("mtc_in").and_then(|v| v.as_str()) {
            s.mtc_in = x.to_string();
        }
        if let Some(x) = t.get("ltc_in").and_then(|v| v.as_str()) {
            s.ltc_in = x.to_string();
        }
        match t.get("ltc_level") {
            None => {}
            Some(toml::Value::Float(f)) if *f <= 0.0 => s.ltc_level = *f,
            Some(toml::Value::Integer(i)) if *i <= 0 => s.ltc_level = *i as f64,
            Some(x) => return Err(format!("[timecode] ltc_level must be ≤ 0 dBFS, not {x}")),
        }
        s.ltc_latency = secs("ltc_latency")?.unwrap_or(0.0);
        s.mtc_latency = secs("mtc_latency")?.unwrap_or(0.0);
        Ok(s)
    }
}

/// One timecode I/O worker's specification (a change restarts the worker).
#[derive(Clone, Debug, PartialEq)]
pub enum Io {
    MtcIn { key: String, pattern: String },
    LtcIn { key: String, tap: String, rate: FrameRate },
    MtcOut { timeline: String, port: String, rate: FrameRate, offset: f64 },
    LtcOut { timeline: String, rate: FrameRate, level: f64, latency: f64 },
}

/// The workers a configuration needs, by id; plus warnings.
pub fn desired(defs: &BTreeMap<String, TimelineDef>, s: &Settings) -> (BTreeMap<String, Io>, Vec<String>) {
    let mut io = BTreeMap::new();
    let mut warn = Vec::new();
    for d in defs.values() {
        let key = d.source.key();
        match &d.source {
            SourceDef::Mtc(p) => {
                let pattern = if p.is_empty() { s.mtc_in.clone() } else { p.clone() };
                io.entry(format!("in:{key}")).or_insert(Io::MtcIn { key, pattern });
            }
            SourceDef::Ltc(p) => {
                let tap = if p.is_empty() { s.ltc_in.clone() } else { p.clone() };
                io.entry(format!("in:{key}")).or_insert(Io::LtcIn { key, tap, rate: d.rate });
            }
            _ => {}
        }
        if let Some(port) = &d.mtc_out {
            io.insert(format!("mtcout:{}", d.name), Io::MtcOut { timeline: d.name.clone(), port: port.clone(), rate: d.rate, offset: s.mtc_latency });
        }
        if d.ltc_out {
            if let Some(Io::LtcOut { timeline, .. }) = io.get("ltcout") {
                warn.push(format!("timeline `{}` also asks for LTC out; only `{timeline}` drives the single `timecode.ltc` output", d.name));
            } else {
                io.insert("ltcout".into(), Io::LtcOut { timeline: d.name.clone(), rate: d.rate, level: s.ltc_level, latency: s.ltc_latency });
            }
        }
    }
    (io, warn)
}

struct Worker {
    spec: Io,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn spawn(hub: &Arc<Hub>, health: &Arc<Health>, spec: &Io) -> std::io::Result<Worker> {
    let stop = Arc::new(AtomicBool::new(false));
    let (h, s) = (hub.clone(), health.clone());
    let handle = match spec.clone() {
        Io::MtcIn { key, pattern } => mtc::spawn_input(h, s, key, pattern, stop.clone())?,
        Io::LtcIn { key, tap, rate } => ltc::spawn_input(h, s, key, tap, rate, stop.clone())?,
        Io::MtcOut { timeline, port, rate, offset } => mtc::spawn_output(h, s, timeline, port, rate, offset, stop.clone())?,
        Io::LtcOut { timeline, rate, level, latency } => ltc::spawn_output(h, s, timeline, rate, level, latency, stop.clone())?,
    };
    Ok(Worker { spec: spec.clone(), stop, handle: Some(handle) })
}

struct Shared {
    ctx: EngineCtx,
    project: Project,
    grids: Mutex<BTreeMap<String, Value>>,
}

/// Start the timeline I/O subsystem.
pub async fn start(ctx: EngineCtx) -> Result<()> {
    let project = Project::open(&ctx.project_root)?;
    let health = Arc::new(Health::new(ctx.hub.clone()));
    health.publish();
    let sh = Arc::new(Shared { ctx: ctx.clone(), project, grids: Mutex::new(BTreeMap::new()) });

    // record commits, editor writes, new timelines
    let mut rx = ctx.hub.route_actions("timeline");
    {
        let sh = sh.clone();
        tokio::spawn(async move {
            while let Some(c) = rx.recv().await {
                let Op::Action { name, args } = &c.op else { continue };
                if let Err(e) = handle_action(&sh, name, args).await {
                    sh.ctx.hub.log("error", "timeline", format!("{name}: {e:#}"));
                }
            }
        });
    }

    {
        let sh = sh.clone();
        let f: se_hub::QueryFn = Arc::new(move |_name, args| {
            let sh = sh.clone();
            Box::pin(async move { grid_query(&sh, &args).await.map_err(|e| format!("{e:#}")) })
        });
        ctx.hub.register_query("timeline.grid", f);
    }
    {
        let ctx2 = ctx.clone();
        let f: se_hub::QueryFn = Arc::new(move |_name, _args| {
            let ctx = ctx2.clone();
            Box::pin(async move {
                let s = Settings::from_config(&ctx.config.borrow()).unwrap_or_default();
                let midi: Vec<Value> = se_input::midi::ports().into_iter().map(|p| Value::from(serde_json::to_value(p).unwrap_or_default())).collect();
                Ok(Value::map().with("midi", midi).with("mtc_in", s.mtc_in).with("ltc_in", s.ltc_in).with("ltc_slot", ltc::SLOT))
            })
        });
        ctx.hub.register_query("timeline.ports", f);
    }

    // timecode I/O workers follow the configuration
    let mut cfg_rx = ctx.config.clone();
    let hub = ctx.hub.clone();
    tokio::spawn(async move {
        let mut workers: BTreeMap<String, Worker> = BTreeMap::new();
        loop {
            let cfg = cfg_rx.borrow_and_update().clone();
            let settings = Settings::from_config(&cfg).unwrap_or_else(|e| {
                hub.log("error", "timeline", e);
                Settings::default()
            });
            let (defs, _) = timeline::parse_all(&cfg);
            let (want, warnings) = desired(&defs, &settings);
            for w in warnings {
                hub.log("warn", "timeline", w);
            }
            let stale: Vec<String> = workers.iter().filter(|(k, w)| want.get(*k) != Some(&w.spec)).map(|(k, _)| k.clone()).collect();
            let health2 = health.clone();
            let dropped: Vec<Worker> = stale.iter().filter_map(|k| workers.remove(k)).collect();
            // joining threads blocks: do it off the async runtime
            let _ = tokio::task::spawn_blocking(move || drop(dropped)).await;
            for (k, spec) in want {
                if workers.contains_key(&k) {
                    continue;
                }
                match spawn(&hub, &health2, &spec) {
                    Ok(w) => {
                        workers.insert(k, w);
                    }
                    Err(e) => hub.log("error", "timeline", format!("timecode worker `{k}`: {e}")),
                }
            }
            health2.publish();
            if cfg_rx.changed().await.is_err() {
                break;
            }
        }
        let _ = tokio::task::spawn_blocking(move || drop(workers)).await;
    });
    Ok(())
}

fn def_of(cfg: &Config, name: &str) -> Result<TimelineDef> {
    let t = cfg.other.get("timelines").and_then(|m| m.get(name)).ok_or_else(|| anyhow!("unknown timeline `{name}`"))?;
    let file = cfg.files.get(&format!("timelines/{name}")).cloned().unwrap_or_else(|| format!("timelines/{name}.toml"));
    TimelineDef::parse(name, &file, t).map_err(|e| anyhow!("{file}: {e}"))
}

/// Beat grid for a timeline: a `file:` media item's cached analysis, or the timeline's
/// `analysis = "<audio file>"` (analyzed and cached by the Audio slice).
async fn grid_for(sh: &Shared, def: &TimelineDef) -> Result<Option<Value>> {
    let args = match (&def.source, &def.analysis) {
        (_, Some(path)) => {
            let p = std::path::Path::new(path);
            let abs = if p.is_absolute() { p.to_path_buf() } else { sh.ctx.project_root.join(p) };
            Value::map().with("path", abs.to_string_lossy().to_string())
        }
        (SourceDef::Media(id), None) if id.starts_with("file:") => Value::map().with("media", id.clone()),
        _ => return Ok(None),
    };
    let key = args.to_string();
    if let Some(v) = sh.grids.lock().get(&key) {
        return Ok(Some(v.clone()));
    }
    let v = sh.ctx.hub.query("analysis.grid", args).await.map_err(|e| anyhow!("analysis.grid: {e}"))?;
    if v.is_null() {
        return Ok(None);
    }
    sh.grids.lock().insert(key, v.clone());
    Ok(Some(v))
}

async fn grid_query(sh: &Shared, args: &Value) -> Result<Value> {
    let name = args.get_path("name").or_else(|| args.get_path("args.0")).and_then(Value::as_str).ok_or_else(|| anyhow!("needs `name`"))?;
    let def = def_of(&sh.ctx.config.borrow(), name)?;
    Ok(grid_for(sh, &def).await?.unwrap_or_default())
}

async fn handle_action(sh: &Shared, name: &str, args: &Value) -> Result<()> {
    if name == "timeline.create" {
        let n = args.get_path("name").or_else(|| args.get_path("args.0")).and_then(Value::as_str).ok_or_else(|| anyhow!("needs `name`"))?;
        if !se_proto::address::is_valid(&format!("timeline.{n}"), false) || n.contains('.') {
            bail!("`{n}` is not a valid timeline name (letters, digits, `_`, `-`)");
        }
        let rel = format!("timelines/{n}.toml");
        if sh.project.root().join(&rel).exists() {
            bail!("{rel} already exists");
        }
        let src = args.get_path("source").and_then(Value::as_str).unwrap_or("internal");
        let fps = args.get_path("fps").map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string()));
        let length = args.get_path("length").map(|v| timeline::parse_time_value(v, FrameRate::Fps30)).transpose().map_err(|e| anyhow!(e))?;
        let text = edit::new_file(n, src, fps.as_deref(), length)?;
        sh.project.write_file(&rel, &text)?;
        sh.ctx.hub.log("info", "timeline", format!("created {rel}"));
        return Ok(());
    }
    let tl = args.get_path("timeline").and_then(Value::as_str).ok_or_else(|| anyhow!("needs `timeline`"))?;
    let def = def_of(&sh.ctx.config.borrow(), tl)?;
    let fmt = edit::TimeFmt::of(&def);
    let snap_mode = match args.get_path("snap") {
        Some(Value::Bool(true)) => def.snap,
        Some(Value::Str(s)) if s == "beat" => Snap::Beat,
        Some(Value::Str(s)) if s == "bar" => Snap::Bar,
        _ if name == "timeline.record.commit" => def.snap,
        _ => Snap::Off,
    };
    let grid = if snap_mode != Snap::Off { grid_for(sh, &def).await?.and_then(|v| grid::Grid::from_value(&v)) } else { None };
    let snap = |t: f64| grid.as_ref().map(|g| g.snap(t, snap_mode)).unwrap_or(t);
    match name {
        "timeline.record.commit" => {
            let mut done = edit::Committed::default();
            sh.project.edit(&def.file, |doc| {
                done = edit::commit_recording(doc, &fmt, args, snap)?;
                Ok(())
            })?;
            sh.ctx.hub.log("info", "timeline", format!("{}: recorded {} cues, {} keyframes into {}", def.name, done.cues, done.keys, def.file));
        }
        n if n.starts_with("timeline.edit.track.") => {
            let track = args.get_path("name").and_then(Value::as_str).ok_or_else(|| anyhow!("needs `name`"))?.to_string();
            sh.project.edit(&def.file, |doc| match &n["timeline.edit.track.".len()..] {
                "add" => {
                    let kind = match args.get_path("type").and_then(Value::as_str).unwrap_or("cues") {
                        "cues" | "cue" => edit::Kind::Cues,
                        "automation" | "lane" => edit::Kind::Keys,
                        "regions" | "region" => edit::Kind::Regions,
                        other => bail!("unknown track type `{other}`"),
                    };
                    edit::add_track(doc, &track, kind, args.get_path("address").and_then(Value::as_str))
                }
                "remove" => edit::remove_track(doc, &track),
                "mute" => edit::set_mute(doc, &track, args.get_path("mute").is_none_or(Value::truthy)),
                other => bail!("unknown track edit `{other}`"),
            })?;
        }
        n if n.starts_with("timeline.edit.") => {
            let op = &n["timeline.edit.".len()..];
            let mut a = args.clone();
            if grid.is_some() {
                for k in ["at", "start", "end"] {
                    if let Some(v) = a.get_path(k).cloned() {
                        let t = timeline::parse_time_value(&v, def.rate).map_err(|e| anyhow!(e))?;
                        a = a.with(k, snap(t));
                    }
                }
            }
            sh.project.edit(&def.file, |doc| edit::apply_item_op(doc, &fmt, op, &a)).with_context(|| format!("{} ({op})", def.file))?;
        }
        other => bail!("unknown timeline action `{other}`"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_follows_timeline_sources_and_outputs() {
        let t = |src: &str| -> TimelineDef { TimelineDef::parse("x", "f", &src.parse().unwrap()).unwrap() };
        let mut defs = BTreeMap::new();
        defs.insert("a".to_string(), TimelineDef { name: "a".into(), ..t("source = \"mtc\"\n[timecode_out]\nltc = true") });
        defs.insert("b".to_string(), TimelineDef { name: "b".into(), ..t("source = \"mtc\"\nfps = \"25\"\n[timecode_out]\nmtc = \"xtouch\"\nltc = true") });
        defs.insert("c".to_string(), TimelineDef { name: "c".into(), ..t("source = \"ltc:input.tc.1\"") });
        let (io, warn) = desired(&defs, &Settings { mtc_in: "studio24c".into(), ..Default::default() });
        assert_eq!(io["in:mtc"], Io::MtcIn { key: "mtc".into(), pattern: "studio24c".into() });
        assert_eq!(io["in:ltc:input.tc.1"], Io::LtcIn { key: "ltc:input.tc.1".into(), tap: "input.tc.1".into(), rate: FrameRate::Fps30 });
        assert_eq!(io["mtcout:b"], Io::MtcOut { timeline: "b".into(), port: "xtouch".into(), rate: FrameRate::Fps25, offset: 0.0 });
        assert!(matches!(&io["ltcout"], Io::LtcOut { timeline, .. } if timeline == "a"));
        assert_eq!(warn.len(), 1, "one LTC output only");
    }

    #[test]
    fn settings_parse() {
        let files = vec![se_core::SourceFile {
            kind: "project".into(),
            name: "project".into(),
            path: "project.toml".into(),
            table: "schema = 1\n[timecode]\nmtc_in = \"*24c*\"\nltc_level = -18\nltc_latency = \"12ms\"".parse().unwrap(),
        }];
        let s = Settings::from_config(&Config::build(&files)).unwrap();
        assert_eq!((s.mtc_in.as_str(), s.ltc_level, s.ltc_latency), ("*24c*", -18.0, 0.012));
    }
}
