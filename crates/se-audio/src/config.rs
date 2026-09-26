//! Audio configuration: `[audio]` in `project.toml` deep-merged with every `audio/*.toml`
//! fragment (file-name order). The format is documented in `docs/audio.md`.

use se_proto::parse_duration_ms;
use serde::Deserialize;
use std::collections::BTreeMap;

/// Buses that always exist (in processing order; `program` is the sum and comes last).
pub const BUILTIN_BUSES: &[&str] = &["band", "music", "sfx", "tts", "game", "program"];
/// Stem buses created when an input routes to them (M12).
pub const STEM_BUSES: &[&str] = &["drums", "mic"];

/// Default `hub.audio` slot routing (pattern → bus); config entries take precedence.
pub const DEFAULT_SLOT_ROUTES: &[(&str, &str)] =
    &[("youtube", "music"), ("web.*", "music"), ("tts", "tts"), ("tts.*", "tts"), ("sfx.*", "sfx"), ("game", "game"), ("game.*", "game"), ("patch.*", "sfx")];

/// Slots with this prefix are never mixed into buses; they only go to `[audio.direct]` routes.
pub const DIRECT_ONLY_PREFIX: &str = "timecode.";

#[derive(Clone, Debug, PartialEq)]
pub enum FxKind {
    /// Built-in `se-dsp` effect.
    Builtin(String),
    /// `dsp` WebAssembly patch (`patches/<id>/`).
    Patch(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct FxDef {
    /// Id within its chain (address segment).
    pub name: String,
    pub kind: FxKind,
    /// One-shot effect following the core trigger envelope at its address.
    pub trigger: bool,
    /// Trigger envelope spec (`{ attack, hold, release, retrigger }`) declared to the core.
    pub trigger_spec: Option<toml::Value>,
    pub wet: Option<f32>,
    pub dry: Option<f32>,
    pub bypass: bool,
    /// Expression (same language as rules); the effect is bypassed while false.
    pub when: Option<String>,
    /// Sidechain key bus (`ctx.key` = mono sum of that bus's input).
    pub key: Option<String>,
    /// Initial (base) parameter values.
    pub params: BTreeMap<String, toml::Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InputDef {
    pub name: String,
    /// PipeWire source node: exact `node.name`, or a case-insensitive substring of
    /// `node.name` / `node.description`.
    pub target: String,
    /// 1-based device channels (1 or 2 → mono or stereo input).
    pub channels: Vec<u32>,
    pub bus: String,
    pub gain_db: f32,
    pub mute: bool,
    pub delay_ms: f32,
    pub fx: Vec<FxDef>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BusDef {
    pub name: String,
    pub gain_db: f32,
    pub mute: bool,
    pub limiter: bool,
    pub ceiling_db: f32,
    pub fx: Vec<FxDef>,
    /// Summed into `program` (all buses except `program` itself by default).
    pub to_program: bool,
    /// Publish as a PipeWire node `se-<name>` for OBS.
    pub node: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DuckDef {
    /// Buses that duck.
    pub targets: Vec<String>,
    pub depth_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    /// Signals that duck while > 0.5 (e.g. `mic.talking`).
    pub signals: Vec<String>,
    /// Buses whose level ducks the targets while above `threshold_db` (e.g. `tts`).
    pub keys: Vec<String>,
    pub threshold_db: f32,
    /// Keep ducking this long after the last key activity (avoid pumping between words).
    pub hold_ms: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LayerCfg {
    pub vel: [f32; 2],
    pub files: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SoundCfg {
    pub name: String,
    pub layers: Vec<LayerCfg>,
    pub random: bool,
    pub gain_db: f32,
    pub bus: String,
    pub choke: Option<u32>,
    pub max_voices: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MonitorDef {
    /// PipeWire sink node (exact name or substring).
    pub target: String,
    pub channels: Vec<u32>,
    /// Buses mixed into the monitor (post-chain, pre-fader) and inputs (post input chain).
    pub buses: Vec<String>,
    pub inputs: Vec<String>,
    pub gain_db: f32,
    pub fx: Vec<FxDef>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DirectDef {
    pub slot: String,
    pub target: String,
    /// 1-based sink channel.
    pub channel: u32,
    pub gain_db: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PadDef {
    pub name: String,
    pub input: String,
    /// 0-based channel within the input.
    pub channel: usize,
    pub band: [f32; 2],
    pub threshold_db: f32,
    pub retrigger_ms: f32,
    pub scan_ms: f32,
    pub bleed_db: f32,
    pub velocity_curve: f32,
    pub velocity_range_db: [f32; 2],
    /// Sampler layer fired on the audio thread for every hit (`sound`, gain dB).
    pub layer: Option<(String, f32)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SourceDef {
    /// `dsp` patch id with layer `audio-source`.
    pub patch: String,
    pub bus: String,
    pub gain_db: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeatSource {
    /// Band when it has a confident beat, else music.
    Auto,
    Bus(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnalysisDef {
    /// `auto`, or a bus name whose tracker drives `beat.*`.
    pub beat_source: String,
    pub min_bpm: f32,
    pub max_bpm: f32,
    /// Buses analysed into `<bus>.*` signals (default band + music).
    pub buses: Vec<String>,
    /// Input analysed as the mic (hype level, `mic.talking` from the vocal stem).
    pub mic: Option<String>,
    /// `mic.talking` threshold (dBFS RMS) and hangover.
    pub talk_threshold_db: f32,
    pub talk_hold_ms: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioConfig {
    pub rate: u32,
    /// PipeWire quantum requested by the engine node (frames).
    pub quantum: u32,
    pub inputs: Vec<InputDef>,
    pub buses: Vec<BusDef>,
    /// Slot pattern → bus (config first, then defaults).
    pub slot_routes: Vec<(String, String)>,
    /// Per-slot A/V delay defaults (ms) by slot pattern.
    pub slot_delay_ms: Vec<(String, f32)>,
    pub duck: DuckDef,
    pub sounds: Vec<SoundCfg>,
    pub monitor: Option<MonitorDef>,
    pub direct: Vec<DirectDef>,
    pub pads: Vec<PadDef>,
    pub sources: Vec<SourceDef>,
    pub analysis: AnalysisDef,
    /// Max. seconds of A/V delay per input/slot (buffer size).
    pub max_delay_ms: f32,
}

impl AudioConfig {
    pub fn bus_index(&self, name: &str) -> Option<usize> {
        self.buses.iter().position(|b| b.name == name)
    }

    pub fn input_index(&self, name: &str) -> Option<usize> {
        self.inputs.iter().position(|b| b.name == name)
    }

    /// Bus for a `hub.audio` slot (None = not mixed).
    pub fn route_slot(&self, slot: &str) -> Option<usize> {
        if slot.starts_with(DIRECT_ONLY_PREFIX) {
            return None;
        }
        self.slot_routes.iter().find(|(p, _)| se_proto::address::matches(p, slot)).and_then(|(_, b)| self.bus_index(b))
    }

    pub fn slot_delay(&self, slot: &str) -> f32 {
        self.slot_delay_ms.iter().find(|(p, _)| se_proto::address::matches(p, slot)).map(|(_, d)| *d).unwrap_or(0.0)
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        parse(&toml::Table::new(), &BTreeMap::new()).expect("default audio config is valid")
    }
}

// ---- raw (serde) -------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Raw {
    rate: Option<u32>,
    quantum: Option<u32>,
    max_delay: Option<toml::Value>,
    inputs: BTreeMap<String, RawInput>,
    buses: BTreeMap<String, RawBus>,
    slots: BTreeMap<String, RawSlot>,
    duck: RawDuck,
    sounds: BTreeMap<String, RawSound>,
    monitor: Option<RawMonitor>,
    direct: BTreeMap<String, RawDirect>,
    drums: RawDrums,
    sources: BTreeMap<String, RawSource>,
    analysis: RawAnalysis,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawSlot {
    Bus(String),
    Full {
        #[serde(default)]
        bus: Option<String>,
        #[serde(default)]
        delay: Option<toml::Value>,
    },
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawInput {
    target: Option<String>,
    channels: Option<Vec<u32>>,
    bus: Option<String>,
    gain: f32,
    mute: bool,
    delay: Option<toml::Value>,
    fx: Vec<toml::Table>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawBus {
    gain: f32,
    mute: bool,
    limiter: Option<bool>,
    ceiling: Option<f32>,
    fx: Vec<toml::Table>,
    to_program: Option<bool>,
    node: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawDuck {
    targets: Option<Vec<String>>,
    depth: Option<f32>,
    attack: Option<toml::Value>,
    release: Option<toml::Value>,
    hold: Option<toml::Value>,
    signals: Option<Vec<String>>,
    keys: Option<Vec<String>>,
    threshold: Option<f32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLayer {
    #[serde(default)]
    vel: Option<[f32; 2]>,
    files: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawSound {
    file: Option<String>,
    files: Vec<String>,
    layers: Vec<RawLayer>,
    pick: Option<String>,
    gain: f32,
    bus: Option<String>,
    choke: Option<u32>,
    voices: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawMonitor {
    target: Option<String>,
    channels: Option<Vec<u32>>,
    buses: Vec<String>,
    inputs: Vec<String>,
    gain: f32,
    fx: Vec<toml::Table>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDirect {
    target: String,
    channel: u32,
    #[serde(default)]
    gain: f32,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawDrums {
    pads: BTreeMap<String, RawPad>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPad {
    input: String,
    #[serde(default)]
    channel: Option<usize>,
    #[serde(default)]
    band: Option<[f32; 2]>,
    #[serde(default)]
    threshold: Option<f32>,
    #[serde(default)]
    retrigger: Option<toml::Value>,
    #[serde(default)]
    scan: Option<toml::Value>,
    #[serde(default)]
    bleed: Option<f32>,
    #[serde(default)]
    velocity_curve: Option<f32>,
    #[serde(default)]
    velocity_range: Option<[f32; 2]>,
    #[serde(default)]
    layer: Option<RawPadLayer>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPadLayer {
    sound: String,
    #[serde(default)]
    gain: f32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSource {
    bus: String,
    #[serde(default)]
    gain: f32,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawAnalysis {
    beat_source: Option<String>,
    min_bpm: Option<f32>,
    max_bpm: Option<f32>,
    buses: Option<Vec<String>>,
    mic: Option<String>,
    talk_threshold: Option<f32>,
    talk_hold: Option<toml::Value>,
}

// ---- parsing -----------------------------------------------------------------------------

/// Deep-merge `src` into `dst` (tables merge recursively; everything else replaces).
pub fn deep_merge(dst: &mut toml::Table, src: &toml::Table) {
    for (k, v) in src {
        match (dst.get_mut(k), v) {
            (Some(toml::Value::Table(d)), toml::Value::Table(s)) => deep_merge(d, s),
            _ => {
                dst.insert(k.clone(), v.clone());
            }
        }
    }
}

fn ms(v: &Option<toml::Value>, default: f32, what: &str) -> Result<f32, String> {
    match v {
        None => Ok(default),
        Some(toml::Value::String(s)) => parse_duration_ms(s).map(|x| x as f32).ok_or_else(|| format!("{what}: bad duration `{s}`")),
        Some(toml::Value::Integer(i)) => Ok(*i as f32),
        Some(toml::Value::Float(f)) => Ok(*f as f32),
        Some(other) => Err(format!("{what}: expected a duration, got `{other}`")),
    }
}

fn segment_ok(s: &str) -> bool {
    !s.is_empty() && !s.contains('.') && se_proto::address::is_valid(s, false)
}

fn parse_fx(list: &[toml::Table], owner: &str) -> Result<Vec<FxDef>, String> {
    let mut out: Vec<FxDef> = Vec::new();
    for t in list {
        let mut params = BTreeMap::new();
        let mut name = None;
        let mut kind = None;
        let mut patch = None;
        let mut fx = FxDef {
            name: String::new(),
            kind: FxKind::Builtin(String::new()),
            trigger: false,
            trigger_spec: None,
            wet: None,
            dry: None,
            bypass: false,
            when: None,
            key: None,
            params: BTreeMap::new(),
        };
        for (k, v) in t {
            let num = || v.as_float().or_else(|| v.as_integer().map(|i| i as f64)).map(|f| f as f32);
            match k.as_str() {
                "name" => name = v.as_str().map(str::to_string),
                "kind" => kind = v.as_str().map(str::to_string),
                "patch" => patch = v.as_str().map(str::to_string),
                "trigger" => match v {
                    toml::Value::Boolean(b) => fx.trigger = *b,
                    toml::Value::Table(_) => {
                        fx.trigger = true;
                        fx.trigger_spec = Some(v.clone());
                    }
                    _ => return Err(format!("{owner}: `trigger` must be true/false or {{ attack, hold, release }}")),
                },
                "wet" => fx.wet = Some(num().ok_or_else(|| format!("{owner}: `wet` must be a number"))?),
                "dry" => fx.dry = Some(num().ok_or_else(|| format!("{owner}: `dry` must be a number"))?),
                "bypass" => fx.bypass = v.as_bool().ok_or_else(|| format!("{owner}: `bypass` must be a bool"))?,
                "when" => fx.when = Some(v.as_str().ok_or_else(|| format!("{owner}: `when` must be a string"))?.to_string()),
                "key" => fx.key = Some(v.as_str().ok_or_else(|| format!("{owner}: `key` must be a bus name"))?.to_string()),
                _ => {
                    params.insert(k.clone(), v.clone());
                }
            }
        }
        fx.kind = match (kind, patch) {
            (Some(_), Some(_)) => return Err(format!("{owner}: an effect has either `kind` or `patch`, not both")),
            (None, Some(p)) => FxKind::Patch(p),
            (Some(k), None) => match k.strip_prefix("patch.") {
                Some(p) => FxKind::Patch(p.to_string()),
                None => FxKind::Builtin(k),
            },
            (None, None) => match &name {
                Some(n) if se_dsp::params_of(n).is_some() => FxKind::Builtin(n.clone()),
                _ => return Err(format!("{owner}: effect needs `kind` (one of {}) or `patch`", se_dsp::kinds().join(", "))),
            },
        };
        fx.name = name.unwrap_or_else(|| match &fx.kind {
            FxKind::Builtin(k) => k.clone(),
            FxKind::Patch(p) => p.clone(),
        });
        if !segment_ok(&fx.name) || ["wet", "dry", "bypass", "env", "active", "trigger"].contains(&fx.name.as_str()) {
            return Err(format!("{owner}: bad effect name `{}`", fx.name));
        }
        if out.iter().any(|o| o.name == fx.name) {
            return Err(format!("{owner}: duplicate effect `{}` (give one a `name`)", fx.name));
        }
        if let Some(w) = &fx.when {
            se_expr::Expr::parse(w).map_err(|e| format!("{owner}.{}: when: {e}", fx.name))?;
        }
        if let Some(spec) = &fx.trigger_spec {
            se_core::triggers::TriggerSpec::from_value(&se_proto::Value::from(spec.clone())).map_err(|e| format!("{owner}.{}: trigger: {e}", fx.name))?;
        }
        if let FxKind::Builtin(k) = &fx.kind {
            let specs =
                se_dsp::params_of(k).ok_or_else(|| format!("{owner}.{}: unknown effect kind `{k}` (known: {})", fx.name, se_dsp::kinds().join(", ")))?;
            for (p, v) in &params {
                let spec = specs.iter().find(|s| s.name == p).ok_or_else(|| {
                    format!("{owner}.{}: `{k}` has no parameter `{p}` (has: {})", fx.name, specs.iter().map(|s| s.name).collect::<Vec<_>>().join(", "))
                })?;
                param_value(spec, v).map_err(|e| format!("{owner}.{}.{p}: {e}", fx.name))?;
            }
        } else if let FxKind::Patch(p) = &fx.kind
            && !segment_ok(p)
        {
            return Err(format!("{owner}: bad patch id `{p}`"));
        }
        fx.params = params;
        out.push(fx);
    }
    Ok(out)
}

/// Convert a config/state value into the effect's numeric domain (choices by name or index).
pub fn param_value(spec: &se_dsp::ParamSpec, v: &toml::Value) -> Result<f32, String> {
    match (spec.kind, v) {
        (se_dsp::ParamKind::Choice(opts), toml::Value::String(s)) => {
            opts.iter().position(|o| o == s).map(|i| i as f32).ok_or_else(|| format!("`{s}` is not one of {}", opts.join(", ")))
        }
        (_, toml::Value::Boolean(b)) => Ok(if *b { 1.0 } else { 0.0 }),
        (_, toml::Value::Integer(i)) => Ok(spec.clamp(*i as f32)),
        (_, toml::Value::Float(f)) => Ok(spec.clamp(*f as f32)),
        (_, other) => Err(format!("unsupported value `{other}`")),
    }
}

/// Build the audio config from `project.toml [audio]` and the `audio/*.toml` fragments.
pub fn parse(section: &toml::Table, fragments: &BTreeMap<String, toml::Table>) -> Result<AudioConfig, String> {
    let mut merged = section.clone();
    for t in fragments.values() {
        deep_merge(&mut merged, t);
    }
    let raw: Raw = toml::Value::Table(merged).try_into().map_err(|e: toml::de::Error| e.message().to_string())?;

    let rate = raw.rate.unwrap_or(48000);
    if !(8000..=192000).contains(&rate) {
        return Err(format!("rate {rate} out of range"));
    }
    let quantum = raw.quantum.unwrap_or(256);
    if !(32..=2048).contains(&quantum) {
        return Err(format!("quantum {quantum} must be 32–2048 frames"));
    }
    if quantum as usize > se_dsp::MAX_BLOCK * 2 {
        return Err(format!("quantum {quantum} too large"));
    }
    let max_delay_ms = ms(&raw.max_delay, 2000.0, "max_delay")?.clamp(10.0, 10000.0);

    // inputs (default: the Studio 24c stereo input = 16R main mix → band)
    let mut raw_inputs = raw.inputs;
    if raw_inputs.is_empty() {
        raw_inputs.insert("band".into(), RawInput::default());
    }
    let mut inputs = Vec::new();
    for (name, ri) in raw_inputs {
        if !segment_ok(&name) {
            return Err(format!("bad input name `{name}`"));
        }
        let channels = ri.channels.unwrap_or_else(|| vec![1, 2]);
        if channels.is_empty() || channels.len() > 2 || channels.contains(&0) {
            return Err(format!("inputs.{name}: channels must be 1 or 2 one-based device channels"));
        }
        inputs.push(InputDef {
            target: ri.target.unwrap_or_else(|| "Studio 24c".into()),
            channels,
            bus: ri.bus.unwrap_or_else(|| if name == "band" { "band".into() } else { name.clone() }),
            gain_db: ri.gain,
            mute: ri.mute,
            delay_ms: ms(&ri.delay, 0.0, &format!("inputs.{name}.delay"))?.clamp(0.0, max_delay_ms),
            fx: parse_fx(&ri.fx, &format!("inputs.{name}"))?,
            name,
        });
    }

    // buses: builtins, stems used by inputs, and explicitly configured ones
    let mut names: Vec<String> = BUILTIN_BUSES.iter().filter(|b| **b != "program").map(|s| s.to_string()).collect();
    let add = |n: &str, names: &mut Vec<String>| {
        if n != "program" && !names.iter().any(|x| x == n) {
            names.push(n.to_string());
        }
    };
    for s in STEM_BUSES {
        if inputs.iter().any(|i| i.bus == *s) || raw.buses.contains_key(*s) {
            add(s, &mut names);
        }
    }
    for i in &inputs {
        add(&i.bus, &mut names);
    }
    for n in raw.buses.keys() {
        add(n, &mut names);
    }
    names.push("program".into());
    let mut buses = Vec::new();
    for n in names {
        if !segment_ok(&n) {
            return Err(format!("bad bus name `{n}`"));
        }
        let rb = raw.buses.get(&n);
        let is_prog = n == "program";
        buses.push(BusDef {
            gain_db: rb.map(|b| b.gain).unwrap_or(0.0).clamp(-60.0, 12.0),
            mute: rb.is_some_and(|b| b.mute),
            limiter: rb.and_then(|b| b.limiter).unwrap_or(is_prog),
            ceiling_db: rb.and_then(|b| b.ceiling).unwrap_or(-1.0).clamp(-24.0, 0.0),
            fx: match rb {
                Some(b) => parse_fx(&b.fx, &format!("buses.{n}"))?,
                None => Vec::new(),
            },
            to_program: !is_prog && rb.and_then(|b| b.to_program).unwrap_or(true),
            node: rb.and_then(|b| b.node).unwrap_or(true),
            name: n,
        });
    }
    let has_bus = |b: &str| buses.iter().any(|x| x.name == b);

    let mut slot_routes = Vec::new();
    let mut slot_delay_ms = Vec::new();
    for (pat, rs) in &raw.slots {
        let (bus, delay) = match rs {
            RawSlot::Bus(b) => (Some(b.clone()), None),
            RawSlot::Full { bus, delay } => (bus.clone(), delay.clone()),
        };
        if let Some(b) = bus {
            if b != "none" && !has_bus(&b) {
                return Err(format!("slots.\"{pat}\": unknown bus `{b}`"));
            }
            slot_routes.push((pat.clone(), b));
        }
        if delay.is_some() {
            slot_delay_ms.push((pat.clone(), ms(&delay, 0.0, &format!("slots.\"{pat}\".delay"))?.clamp(0.0, max_delay_ms)));
        }
    }
    for (p, b) in DEFAULT_SLOT_ROUTES {
        slot_routes.push((p.to_string(), b.to_string()));
    }

    for i in &inputs {
        if !has_bus(&i.bus) || i.bus == "program" {
            return Err(format!("inputs.{}: bad bus `{}`", i.name, i.bus));
        }
    }
    let d = raw.duck;
    let duck = DuckDef {
        targets: d.targets.unwrap_or_else(|| vec!["music".into()]),
        depth_db: d.depth.unwrap_or(-12.0).clamp(-60.0, 0.0),
        attack_ms: ms(&d.attack, 40.0, "duck.attack")?.max(1.0),
        release_ms: ms(&d.release, 400.0, "duck.release")?.max(1.0),
        hold_ms: ms(&d.hold, 250.0, "duck.hold")?.max(0.0),
        signals: d.signals.unwrap_or_else(|| vec!["mic.talking".into()]),
        keys: d.keys.unwrap_or_else(|| vec!["tts".into()]),
        threshold_db: d.threshold.unwrap_or(-45.0),
    };
    for b in duck.targets.iter().chain(&duck.keys) {
        if !has_bus(b) {
            return Err(format!("duck: unknown bus `{b}`"));
        }
    }

    let mut sounds = Vec::new();
    for (name, rs) in raw.sounds {
        if !segment_ok(&name) {
            return Err(format!("bad sound name `{name}`"));
        }
        let mut layers: Vec<LayerCfg> = rs.layers.into_iter().map(|l| LayerCfg { vel: l.vel.unwrap_or([0.0, 1.0]), files: l.files }).collect();
        let mut flat = rs.files;
        if let Some(f) = rs.file {
            flat.insert(0, f);
        }
        if !flat.is_empty() {
            layers.push(LayerCfg { vel: [0.0, 1.0], files: flat });
        }
        if layers.is_empty() || layers.iter().any(|l| l.files.is_empty()) {
            return Err(format!("sounds.{name}: needs `file`, `files`, or `layers` with files"));
        }
        let bus = rs.bus.unwrap_or_else(|| "sfx".into());
        if !has_bus(&bus) || bus == "program" {
            return Err(format!("sounds.{name}: bad bus `{bus}`"));
        }
        let random = match rs.pick.as_deref() {
            None | Some("round_robin") => false,
            Some("random") => true,
            Some(x) => return Err(format!("sounds.{name}: pick must be round_robin or random, not `{x}`")),
        };
        sounds.push(SoundCfg { name, layers, random, gain_db: rs.gain, bus, choke: rs.choke, max_voices: rs.voices.unwrap_or(4).clamp(1, 32) });
    }

    let monitor = match raw.monitor {
        None => None,
        Some(m) => {
            let target = m.target.ok_or("monitor: needs `target` (sink node)")?;
            for b in &m.buses {
                if !has_bus(b) {
                    return Err(format!("monitor: unknown bus `{b}`"));
                }
            }
            for i in &m.inputs {
                if !inputs.iter().any(|x| &x.name == i) {
                    return Err(format!("monitor: unknown input `{i}`"));
                }
            }
            let channels = m.channels.unwrap_or_else(|| vec![1, 2]);
            if channels.is_empty() || channels.len() > 2 || channels.contains(&0) {
                return Err("monitor: channels must be 1 or 2 one-based device channels".into());
            }
            Some(MonitorDef { target, channels, buses: m.buses, inputs: m.inputs, gain_db: m.gain.clamp(-60.0, 12.0), fx: parse_fx(&m.fx, "monitor")? })
        }
    };

    let direct = raw
        .direct
        .into_iter()
        .map(|(slot, d)| {
            if d.channel == 0 {
                return Err(format!("direct.\"{slot}\": channel is 1-based"));
            }
            Ok(DirectDef { slot, target: d.target, channel: d.channel, gain_db: d.gain })
        })
        .collect::<Result<Vec<_>, String>>()?;

    let mut pads = Vec::new();
    for (name, p) in raw.drums.pads {
        if !segment_ok(&name) {
            return Err(format!("bad pad name `{name}`"));
        }
        let Some(inp) = inputs.iter().find(|i| i.name == p.input) else {
            return Err(format!("drums.pads.{name}: unknown input `{}`", p.input));
        };
        let channel = p.channel.unwrap_or(0);
        if channel >= inp.channels.len() {
            return Err(format!("drums.pads.{name}: input `{}` has {} channel(s)", p.input, inp.channels.len()));
        }
        // the sound may also come from assets/sounds/ (resolved when the graph is built)
        let layer = p.layer.map(|l| (l.sound, l.gain));
        pads.push(PadDef {
            band: p.band.unwrap_or_else(|| default_band(&name)),
            threshold_db: p.threshold.unwrap_or(-30.0),
            retrigger_ms: ms(&p.retrigger, 40.0, &format!("drums.pads.{name}.retrigger"))?,
            scan_ms: ms(&p.scan, 3.0, &format!("drums.pads.{name}.scan"))?.clamp(0.5, 20.0),
            bleed_db: p.bleed.unwrap_or(6.0),
            velocity_curve: p.velocity_curve.unwrap_or(1.0).clamp(0.2, 5.0),
            velocity_range_db: p.velocity_range.unwrap_or([-40.0, -6.0]),
            layer,
            channel,
            input: p.input,
            name,
        });
    }

    let mut sources = Vec::new();
    for (key, s) in raw.sources {
        let patch = key.strip_prefix("patch.").unwrap_or(&key).to_string();
        if !segment_ok(&patch) {
            return Err(format!("sources: bad patch id `{key}`"));
        }
        if !has_bus(&s.bus) || s.bus == "program" {
            return Err(format!("sources.\"{key}\": bad bus `{}`", s.bus));
        }
        sources.push(SourceDef { patch, bus: s.bus, gain_db: s.gain });
    }

    let a = raw.analysis;
    let analysis = AnalysisDef {
        beat_source: a.beat_source.unwrap_or_else(|| "auto".into()),
        min_bpm: a.min_bpm.unwrap_or(70.0),
        max_bpm: a.max_bpm.unwrap_or(180.0),
        buses: a.buses.unwrap_or_else(|| vec!["band".into(), "music".into()]),
        mic: a.mic,
        talk_threshold_db: a.talk_threshold.unwrap_or(-42.0),
        talk_hold_ms: ms(&a.talk_hold, 600.0, "analysis.talk_hold")?,
    };
    if analysis.beat_source != "auto" && !analysis.buses.contains(&analysis.beat_source) {
        return Err(format!("analysis.beat_source `{}` must be `auto` or one of the analysed buses", analysis.beat_source));
    }
    for b in &analysis.buses {
        if !has_bus(b) {
            return Err(format!("analysis: unknown bus `{b}`"));
        }
    }
    if analysis.min_bpm < 30.0 || analysis.max_bpm > 300.0 || analysis.min_bpm * 1.9 > analysis.max_bpm {
        return Err("analysis: tempo range must span about an octave within 30–300 BPM".into());
    }
    if let Some(m) = &analysis.mic
        && !inputs.iter().any(|i| &i.name == m)
    {
        return Err(format!("analysis.mic: unknown input `{m}`"));
    }

    Ok(AudioConfig { rate, quantum, inputs, buses, slot_routes, slot_delay_ms, duck, sounds, monitor, direct, pads, sources, analysis, max_delay_ms })
}

fn default_band(pad: &str) -> [f32; 2] {
    match pad {
        p if p.contains("kick") => [40.0, 150.0],
        p if p.contains("snare") => [150.0, 4000.0],
        p if p.contains("hat") || p.contains("cym") || p.contains("ride") => [5000.0, 16000.0],
        p if p.contains("tom") => [70.0, 800.0],
        _ => [60.0, 8000.0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> toml::Table {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn defaults_are_the_24c_band_and_builtin_buses() {
        let c = AudioConfig::default();
        assert_eq!(c.quantum, 256);
        assert_eq!(c.inputs.len(), 1);
        assert_eq!((c.inputs[0].name.as_str(), c.inputs[0].bus.as_str(), c.inputs[0].channels.clone()), ("band", "band", vec![1, 2]));
        let names: Vec<&str> = c.buses.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, BUILTIN_BUSES);
        assert!(c.buses.last().unwrap().limiter, "program limiter on by default");
        assert_eq!(c.route_slot("youtube"), c.bus_index("music"));
        assert_eq!(c.route_slot("web.player"), c.bus_index("music"));
        assert_eq!(c.route_slot("tts"), c.bus_index("tts"));
        assert_eq!(c.route_slot("sfx.alert"), c.bus_index("sfx"));
        assert_eq!(c.route_slot("timecode.ltc"), None, "timecode slots are direct-only");
        assert_eq!(c.duck.targets, vec!["music"]);
    }

    #[test]
    fn fragments_deep_merge_after_project_section() {
        let section = t("quantum = 128\n[buses.music]\ngain = -3.0\n");
        let mut frags = BTreeMap::new();
        frags.insert(
            "chains".to_string(),
            t(r#"
[buses.music]
fx = [{ name = "stutter", kind = "stutter", trigger = { attack = "5ms", hold = "2s", release = "50ms" } }]
[slots]
"web.radio" = "game"
"#),
        );
        let c = parse(&section, &frags).unwrap();
        assert_eq!(c.quantum, 128);
        let m = &c.buses[c.bus_index("music").unwrap()];
        assert_eq!(m.gain_db, -3.0, "project value survives the fragment merge");
        assert_eq!(m.fx[0].name, "stutter");
        assert!(m.fx[0].trigger && m.fx[0].trigger_spec.is_some());
        assert_eq!(c.route_slot("web.radio"), c.bus_index("game"), "config routes win over defaults");
        assert_eq!(c.route_slot("web.other"), c.bus_index("music"));
    }

    #[test]
    fn stems_and_pads() {
        let c = parse(
            &t(r#"
[inputs.band]
target = "16R"
channels = [17, 18]
[inputs.kick]
target = "16R"
channels = [1]
bus = "drums"
[inputs.vox]
target = "16R"
channels = [9]
bus = "mic"
[sounds.kick_layer]
layers = [{ vel = [0.0, 0.5], files = ["a.wav"] }, { vel = [0.5, 1.0], files = ["b.wav", "c.wav"] }]
bus = "drums"
[drums.pads.kick]
input = "kick"
threshold = -24
retrigger = "35ms"
layer = { sound = "kick_layer", gain = -6 }
"#),
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(c.bus_index("drums").is_some() && c.bus_index("mic").is_some());
        assert_eq!(c.pads[0].band, [40.0, 150.0]);
        assert_eq!(c.pads[0].retrigger_ms, 35.0);
        assert_eq!(c.pads[0].layer, Some(("kick_layer".into(), -6.0)));
        assert_eq!(c.sounds[0].layers.len(), 2);
    }

    #[test]
    fn errors_are_specific() {
        let bad = |s: &str| parse(&t(s), &BTreeMap::new()).unwrap_err();
        assert!(bad("quantum = 7").contains("quantum"));
        assert!(bad("[inputs.x]\nbus = \"program\"").contains("bad bus"));
        assert!(bad("[duck]\ntargets = [\"nope\"]").contains("unknown bus"));
        assert!(bad("[sounds.x]\ngain = 1.0").contains("needs `file`"));
        assert!(bad("[drums.pads.k]\ninput = \"nope\"").contains("unknown input"));
        assert!(bad("[buses.music]\nfx = [{ kind = \"svf\", when = \"mode ==\" }]").contains("when"));
        assert!(bad("[buses.music]\nfx = [{ kind = \"svf\" }, { kind = \"svf\" }]").contains("duplicate"));
        assert!(bad("bogus = 1").contains("unknown field"));
        assert!(bad("[buses.music]\nfx = [{ patch = \"a.b\" }]").contains("bad"));
    }

    #[test]
    fn effect_params_are_validated_against_the_registry() {
        let bad = |s: &str| parse(&t(s), &BTreeMap::new()).unwrap_err();
        assert!(bad("[buses.music]\nfx = [{ kind = \"no_such_fx\" }]").contains("unknown effect kind"));
        let kinds = se_dsp::kinds();
        let Some(k) = kinds.first() else { return };
        let e = bad(&format!("[buses.music]\nfx = [{{ kind = \"{k}\", no_such_param = 1 }}]"));
        assert!(e.contains("no parameter"), "{e}");
    }
}
