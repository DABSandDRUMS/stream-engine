//! Typed project configuration (§3.3, §7). Built from parsed TOML files; every file parses
//! independently so one broken file never takes down the rest (last-good is kept per file by
//! [`Config::merge_last_good`]).

use crate::knob::{Knob, KnobKind};
use se_proto::{Value, parse_duration_ms};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: i64 = 1;

/// One parsed project file.
#[derive(Clone, Debug)]
pub struct SourceFile {
    /// Directory kind: `project`, `scenes`, `presets`, `rules`, `bindings`, `lights/cuelists`, …
    pub kind: String,
    /// File stem (`duo` for `scenes/duo.toml`).
    pub name: String,
    /// Path relative to the project root.
    pub path: String,
    pub table: toml::Table,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConfigError {
    pub file: String,
    pub msg: String,
}

/// Duration written as `"8s"`, `"100ms"`, `"1:12.400"`, or a number of milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "DurRepr", into = "String")]
pub struct Dur(pub u64);

#[derive(Deserialize)]
#[serde(untagged)]
enum DurRepr {
    N(u64),
    F(f64),
    S(String),
}

impl TryFrom<DurRepr> for Dur {
    type Error = String;
    fn try_from(r: DurRepr) -> Result<Self, String> {
        match r {
            DurRepr::N(n) => Ok(Dur(n)),
            DurRepr::F(f) if f >= 0.0 => Ok(Dur(f.round() as u64)),
            DurRepr::F(_) => Err("negative duration".into()),
            DurRepr::S(s) => parse_duration_ms(&s).map(Dur).ok_or_else(|| format!("bad duration `{s}`")),
        }
    }
}

impl From<Dur> for String {
    fn from(d: Dur) -> String {
        if d.0.is_multiple_of(1000) { format!("{}s", d.0 / 1000) } else { format!("{}ms", d.0) }
    }
}

impl Dur {
    pub fn ms(self) -> u64 {
        self.0
    }
    pub fn ns(self) -> u64 {
        self.0 * 1_000_000
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CanvasDef {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for CanvasDef {
    fn default() -> Self {
        CanvasDef { width: 1920, height: 1080, fps: 60 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LfoDef {
    /// `sine | tri | saw | square | random`
    pub shape: String,
    /// Hz (`"0.1hz"`, `0.1`) or beats (`"1beat"`, `"4beats"`).
    pub rate: Value,
    pub phase: f64,
}

impl Default for LfoDef {
    fn default() -> Self {
        LfoDef { shape: "sine".into(), rate: Value::Float(1.0), phase: 0.0 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectDef {
    pub schema: i64,
    pub name: String,
    pub canvas: BTreeMap<String, CanvasDef>,
    pub modes: Vec<String>,
    pub start_mode: String,
    /// Base values for arbitrary addresses.
    pub params: BTreeMap<String, Value>,
    pub lfo: BTreeMap<String, LfoDef>,
    /// Core tick rate (Hz).
    pub tick_hz: u32,
    /// Seed for deterministic randomness (transition pools, sim payloads).
    pub seed: u64,
    /// `[transitions]`: how scenes switch when the target scene (and its pair) gives no pool
    /// or `name` of its own; its `ms`/`lights` are the base the scene's override key by key.
    pub transitions: TransitionPool,
    /// Everything else (devices, safety, palette, api, …) for subsystems.
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

pub const DEFAULT_MODES: &[&str] = &["offline", "preshow", "live", "brb", "ad_break", "outro", "rehearsal"];

impl Default for ProjectDef {
    fn default() -> Self {
        let mut canvas = BTreeMap::new();
        canvas.insert("wide".into(), CanvasDef { width: 1920, height: 1080, fps: 60 });
        canvas.insert("tall".into(), CanvasDef { width: 1080, height: 1920, fps: 60 });
        ProjectDef {
            schema: SCHEMA_VERSION,
            name: "stream".into(),
            canvas,
            modes: DEFAULT_MODES.iter().map(|s| s.to_string()).collect(),
            start_mode: "offline".into(),
            params: BTreeMap::new(),
            lfo: BTreeMap::new(),
            tick_hz: 240,
            seed: 0x5eed,
            transitions: TransitionPool::default(),
            extra: BTreeMap::new(),
        }
    }
}

/// Effect reference on a source, node, scene, canvas, or output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FxRef {
    pub name: String,
    pub id: String,
    pub triggered: bool,
    /// Conditional attachment (`mode == 'chill'`).
    pub when: Option<String>,
    /// Exclusive group (only one active per group).
    pub group: Option<String>,
    pub hold: Option<Dur>,
    /// Bypass gate; trigger-envelope selection is independent.
    pub enabled: Option<bool>,
    #[serde(flatten)]
    pub params: BTreeMap<String, Value>,
}

impl FxRef {
    pub fn slot_id(&self) -> &str { if self.id.is_empty() { &self.name } else { &self.id } }
}

pub fn valid_fx_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.') && !id.starts_with('.') && !id.ends_with('.') && !id.contains("..")
}

pub fn normalize_fx(fx: &mut [FxRef]) -> Result<(), String> {
    let mut used = std::collections::BTreeSet::new();
    for f in fx.iter().filter(|f| !f.id.is_empty()) {
        if !valid_fx_id(&f.id) || !used.insert(f.id.clone()) { return Err(format!("invalid or duplicate FX slot `{}`", f.id)); }
    }
    for f in fx {
        if f.name.is_empty() { return Err("FX slot needs a name".into()); }
        if f.id.is_empty() {
            let base = f.name.clone();
            if !valid_fx_id(&base) { return Err(format!("invalid FX name `{}`", f.name)); }
            let mut id = base.clone();
            let mut n = 2;
            while used.contains(&id) { id = format!("{base}_{n}"); n += 1; }
            used.insert(id.clone());
            f.id = id;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FxChainDef {
    pub name: String,
    pub label: Option<String>,
    pub fx: Vec<FxRef>,
}

/// How a source fills its node window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeFit {
    #[default]
    Stretch,
    Native,
}

impl NodeFit {
    /// Place a source UV rectangle in a pixel window. Native pixels follow the canvas's
    /// resolution scale, not the window size: excess is clipped at the top/right, and
    /// unused space stays transparent. Returns the visible content rect and its UVs.
    pub fn placement(self, rect: [f32; 4], uv: [f32; 4], source_size: [f32; 2], canvas_scale: f32) -> ([f32; 4], [f32; 4]) {
        if self == Self::Stretch {
            return (rect, uv);
        }
        let pixel = [source_size[0] * canvas_scale, source_size[1] * canvas_scale];
        let available = [(uv[2] - uv[0]).max(0.0) * pixel[0], (uv[3] - uv[1]).max(0.0) * pixel[1]];
        if pixel[0] <= 0.0 || pixel[1] <= 0.0 || available[0] <= 0.0 || available[1] <= 0.0 {
            return ([rect[0], rect[1] + rect[3], 0.0, 0.0], uv);
        }
        let w = rect[2].max(0.0).min(available[0]);
        let h = rect[3].max(0.0).min(available[1]);
        ([rect[0], rect[1] + rect[3] - h, w, h], [uv[0], uv[3] - h / pixel[1], uv[0] + w / pixel[0], uv[3]])
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeDef {
    /// Node id within the scene; defaults to `src`.
    pub id: String,
    pub src: String,
    /// Normalized x, y, w, h.
    pub rect: [f32; 4],
    pub fit: NodeFit,
    /// Normalized crop: left, top, right, bottom insets.
    pub crop: [f32; 4],
    pub radius: f32,
    pub opacity: f32,
    pub z: i32,
    pub blend: String,
    pub mask: Option<String>,
    pub fx: Vec<FxRef>,
    pub fx_enabled: bool,
    pub when: Option<String>,
    /// Enter/exit style for morph/glide transitions: `fade | scale | slide | slide_left | slide_right | slide_up | slide_down | none`.
    pub enter: Option<String>,
    pub exit: Option<String>,
    pub offset_x: f32,
    pub offset_y: f32,
    pub scale: f32,
    pub rotation: f32,
    pub visible: bool,
}

impl Default for NodeDef {
    fn default() -> Self {
        NodeDef {
            id: String::new(),
            src: String::new(),
            rect: [0.0, 0.0, 1.0, 1.0],
            fit: NodeFit::default(),
            crop: [0.0; 4],
            radius: 0.0,
            opacity: 1.0,
            z: 0,
            blend: "normal".into(),
            mask: None,
            fx: Vec::new(),
            fx_enabled: true,
            when: None,
            enter: None,
            exit: None,
            offset_x: 0.0,
            offset_y: 0.0,
            scale: 1.0,
            rotation: 0.0,
            visible: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SceneCanvas {
    pub nodes: Vec<NodeDef>,
    pub fx: Vec<FxRef>,
    pub fx_enabled: bool,
    pub groups: Vec<GroupDef>,
    pub background: Option<Value>,
}

impl Default for SceneCanvas {
    fn default() -> Self { Self { nodes: Vec::new(), fx: Vec::new(), fx_enabled: true, groups: Vec::new(), background: None } }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupDef {
    pub id: String,
    pub nodes: Vec<String>,
    pub fx: Vec<FxRef>,
    pub fx_enabled: bool,
    pub z: i32,
    pub blend: String,
    pub opacity: f32,
    pub visible: bool,
    pub when: Option<String>,
}

impl Default for GroupDef {
    fn default() -> Self { Self { id: String::new(), nodes: Vec::new(), fx: Vec::new(), fx_enabled: true, z: 0, blend: "normal".into(), opacity: 1.0, visible: true, when: None } }
}

pub fn validate_groups(canvas: &SceneCanvas) -> Result<(), String> {
    let mut ids = std::collections::BTreeSet::new();
    let mut members = std::collections::BTreeSet::new();
    for g in &canvas.groups {
        if !valid_fx_id(&g.id) || !ids.insert(&g.id) { return Err(format!("invalid or duplicate group `{}`", g.id)); }
        for n in &g.nodes {
            if !canvas.nodes.iter().any(|node| &node.id == n) || !members.insert(n) {
                return Err(format!("group `{}` has unknown or repeated member `{n}`", g.id));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PoolEntry {
    pub name: String,
    #[serde(default = "one")]
    pub w: f64,
}

fn one() -> f64 {
    1.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TransitionPool {
    pub pool: Vec<PoolEntry>,
    pub avoid_repeat: usize,
    /// `[min, max]` ms or a single value.
    pub ms: Option<Value>,
    pub lights: Option<LightsRef>,
    /// Explicit transition (overrides the pool).
    pub name: Option<String>,
    /// Scene-pair pools (`[transitions.from.<scene>]`): used instead of this pool when the take
    /// comes from that scene.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub from: BTreeMap<String, TransitionPool>,
}

impl TransitionPool {
    pub fn ms_range(&self) -> Option<(u32, u32)> {
        match &self.ms {
            Some(Value::List(l)) if l.len() == 2 => Some((l[0].as_f64()? as u32, l[1].as_f64()? as u32)),
            Some(v) => v.as_f64().map(|m| (m as u32, m as u32)),
            None => None,
        }
    }

    /// The effective settings for a take from `from`: the pair's (`[transitions.from.<from>]`)
    /// where it gives them, else this (the target scene's). A pair with its own `pool` also
    /// brings its own `avoid_repeat` and ignores the scene's fixed `name`.
    pub fn for_pair(&self, from: &str) -> TransitionPool {
        let Some(p) = self.from.get(from) else { return TransitionPool { from: BTreeMap::new(), ..self.clone() } };
        let own_pool = !p.pool.is_empty();
        TransitionPool {
            pool: if own_pool { p.pool.clone() } else { self.pool.clone() },
            avoid_repeat: if own_pool { p.avoid_repeat } else { self.avoid_repeat },
            ms: p.ms.clone().or_else(|| self.ms.clone()),
            lights: p.lights.clone().or_else(|| self.lights.clone()),
            name: if own_pool { p.name.clone() } else { p.name.clone().or_else(|| self.name.clone()) },
            from: BTreeMap::new(),
        }
    }
}

/// `[transition_vote]` in project.toml (§4.4): chat votes (`transition.vote`, e.g. `!transition
/// fade`) collected over `window` pick the next take's transition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TransitionVoteDef {
    pub enabled: bool,
    /// Only votes from this long before the take count.
    pub window: Dur,
    /// What viewers may vote for (empty = every transition).
    pub choices: Vec<String>,
}

impl Default for TransitionVoteDef {
    fn default() -> Self {
        TransitionVoteDef { enabled: true, window: Dur(60_000), choices: Vec::new() }
    }
}

/// `[context.musical_fx]`: restrained musical video moments, independent of the novelty budget.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MusicalFxDef {
    /// Let the song establish itself before the first opportunity.
    pub start_delay: Dur,
    /// Silence between the end of a release tail and the next opportunity (at least 45s).
    pub quiet_gap: Dur,
    /// Trusted-grid phrase length in beats.
    pub phrase_beats: u32,
    /// Separate rolling-hour ceiling; fixed storage supports at most 48 moments.
    pub max_per_hour: u32,
}

impl Default for MusicalFxDef {
    fn default() -> Self {
        Self { start_delay: Dur(12_000), quiet_gap: Dur(45_000), phrase_beats: 32, max_per_hour: 24 }
    }
}

impl MusicalFxDef {
    fn validate(&self) -> Result<(), String> {
        if !(12_000..=120_000).contains(&self.start_delay.ms()) {
            return Err("[context.musical_fx] start_delay must be 12s..=120s".into());
        }
        if !(45_000..=600_000).contains(&self.quiet_gap.ms()) {
            return Err("[context.musical_fx] quiet_gap must be 45s..=600s".into());
        }
        if !(16..=128).contains(&self.phrase_beats) || self.phrase_beats % 4 != 0 {
            return Err("[context.musical_fx] phrase_beats must be a multiple of four in 16..=128".into());
        }
        if !(1..=48).contains(&self.max_per_hour) {
            return Err("[context.musical_fx] max_per_hour must be 1..=48".into());
        }
        Ok(())
    }
}

/// `[context]` in project.toml: tuning of the context layer (`context.*`, docs/context.md).
/// Every key is optional; the defaults suit a drum-cover stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextDef {
    /// Automatic moments per rolling hour (`context.budget`, spent by `context.spend`).
    pub auto_per_hour: u32,
    /// `context.peak`: `context.energy` ≥ `peak_on` for `peak_hold`, at most once per
    /// `peak_cooldown`; `context.settle` once it stays below `peak_off` for `settle_hold`.
    pub peak_on: f32,
    pub peak_off: f32,
    pub peak_hold: Dur,
    pub settle_hold: Dur,
    pub peak_cooldown: Dur,
    /// `context.song_peak`: after a `music.drop` (or a `music.section` with novelty ≥
    /// `section_novelty`), `context.song` ≥ `song_peak_on` for `song_peak_hold`, starting within
    /// `song_peak_window`; at most once per `song_peak_cooldown`, never while the host talks.
    pub song_peak_on: f32,
    pub song_peak_hold: Dur,
    pub song_peak_window: Dur,
    pub song_peak_cooldown: Dur,
    pub section_novelty: f32,
    /// `context.fill_landed`: ≥ `fill_hits` snare/tom hits within `fill_window`, then a kick of
    /// velocity ≥ `fill_land_velocity` (or a crash) within `fill_land` of the last hit, within
    /// `fill_downbeat` beats of a bar start (0 = anywhere); at most once per `fill_cooldown`.
    pub fill_hits: u32,
    pub fill_window: Dur,
    pub fill_land: Dur,
    pub fill_land_velocity: f32,
    pub fill_downbeat: f32,
    pub fill_cooldown: Dur,
    /// `context.mood` is re-evaluated at most this often while a song plays.
    pub mood_every: Dur,
    pub musical_fx: MusicalFxDef,
}

impl Default for ContextDef {
    fn default() -> Self {
        ContextDef {
            auto_per_hour: 6,
            peak_on: 0.72,
            peak_off: 0.5,
            peak_hold: Dur(6_000),
            settle_hold: Dur(3_000),
            peak_cooldown: Dur(120_000),
            song_peak_on: 0.7,
            song_peak_hold: Dur(4_000),
            song_peak_window: Dur(10_000),
            song_peak_cooldown: Dur(45_000),
            section_novelty: 0.35,
            fill_hits: 5,
            fill_window: Dur(1_200),
            fill_land: Dur(600),
            fill_land_velocity: 0.4,
            fill_downbeat: 0.5,
            fill_cooldown: Dur(20_000),
            mood_every: Dur(20_000),
            musical_fx: MusicalFxDef::default(),
        }
    }
}

/// `[fx]` in project.toml: the operator's effects switch (`fx.enabled`). Patch ids (or full
/// trigger addresses such as `fx.grade`) in `exempt` keep firing while effects are off.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FxControlDef {
    pub exempt: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LightsRef {
    /// A cue list to run (`cue` alone) or, with `cuelist`, one of its cues.
    pub cue: Option<String>,
    pub cuelist: Option<String>,
    /// A palette (`lights/palettes/<look>.toml`) selected in the musical base layer.
    pub look: Option<String>,
    /// Playback duration, bounded by chat TTL for chat-triggered presets.
    pub hold: Option<Dur>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SceneDef {
    pub name: String,
    pub label: Option<String>,
    /// Number key (1–9).
    pub key: Option<u8>,
    pub canvas: BTreeMap<String, SceneCanvas>,
    pub transitions: TransitionPool,
    pub lights: Option<LightsRef>,
    pub fx: Vec<FxRef>,
    pub fx_enabled: bool,
    pub on_enter: Vec<String>,
    pub on_exit: Vec<String>,
    /// Scene-layer values applied to any address while this scene is on program.
    pub set: BTreeMap<String, Value>,
}

impl Default for SceneDef {
    fn default() -> Self { Self { name: String::new(), label: None, key: None, canvas: BTreeMap::new(), transitions: TransitionPool::default(), lights: None, fx: Vec::new(), fx_enabled: true, on_enter: Vec::new(), on_exit: Vec::new(), set: BTreeMap::new() } }
}

impl SceneDef {
    /// Nodes of a canvas.
    pub fn nodes(&self, canvas: &str) -> &[NodeDef] {
        self.canvas.get(canvas).map(|c| c.nodes.as_slice()).unwrap_or(&[])
    }
    /// Unique node ids across canvases, in first-seen order.
    pub fn node_ids(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in self.canvas.values() {
            for n in &c.nodes {
                if !out.contains(&n.id) {
                    out.push(n.id.clone());
                }
            }
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransitionDef {
    pub name: String,
    /// Display name (the file name when absent).
    pub label: Option<String>,
    /// `morph | glide | shader | combined | cut`
    pub kind: String,
    /// For shader/combined: a built-in shader name (`crate::transitions::SHADERS`), a WGSL file
    /// relative to the project (`*.wgsl`), or `patch.<id>`.
    pub shader: Option<String>,
    pub ms: Option<Dur>,
    /// Omitted: `standard` for glide, `in_out_cubic` for other kinds.
    pub ease: Option<se_proto::Ease>,
    pub enter: String,
    pub exit: String,
    pub enter_window: Option<[f32; 2]>,
    pub exit_window: Option<[f32; 2]>,
    pub fade_window: Option<[f32; 2]>,
    #[serde(flatten)]
    pub params: BTreeMap<String, Value>,
}

impl Default for TransitionDef {
    fn default() -> Self {
        TransitionDef {
            name: String::new(),
            label: None,
            kind: "morph".into(),
            shader: None,
            ms: None,
            ease: None,
            enter: "fade".into(),
            exit: "fade".into(),
            enter_window: None,
            exit_window: None,
            fade_window: None,
            params: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Conflict {
    Stack,
    #[default]
    Replace,
    Queue,
    Reject,
}

/// When a preset starts: on the next beat or bar of the beat clock (`quantize`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quantize {
    Beat,
    Bar,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PresetFx {
    /// A built-in effect (`glitch` → trigger `fx.glitch`) or any trigger address (`patch.confetti`).
    pub name: String,
    pub hold: Option<Dur>,
    /// Trigger payload (`level`, `attack`, `release`, …). For built-in effects the other keys
    /// are the effect's settings (`speed = 2.0` → `fx.glitch.speed`), held until it has faded out.
    #[serde(flatten)]
    pub params: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MixRef {
    pub snapshot: String,
    pub fade: Option<Dur>,
}

/// Explicit opt-in to the musical video library. Empty moods admit every context mood.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutoFxDef {
    pub moods: Vec<String>,
    /// Inclusive range of the song's relative energy (typical passage = 0.5).
    pub energy: [f32; 2],
    pub weight: f64,
}

impl Default for AutoFxDef {
    fn default() -> Self { Self { moods: Vec::new(), energy: [0.0, 1.0], weight: 1.0 } }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PresetDef {
    pub name: String,
    pub label: Option<String>,
    pub color: Option<String>,
    pub icon: Option<String>,
    /// Auto-release after this long. Without `hold`, presets with `set` latch until released.
    pub hold: Option<Dur>,
    pub conflict: Conflict,
    pub priority: Option<u16>,
    /// One preset of a lane runs at a time; firings while it is busy wait their turn (FIFO).
    pub lane: Option<String>,
    /// Start on the next beat/bar boundary instead of at once.
    pub quantize: Option<Quantize>,
    /// Pressing again while active releases it.
    pub toggle: bool,
    /// Stays on until released (`preset.release`, a button let go), even with nothing in `set`.
    pub until_released: bool,
    /// Requires confirmation (hold-to-confirm in Show mode) and is chat-disabled.
    pub confirm: bool,
    pub chat: Option<bool>,
    pub fx: Vec<PresetFx>,
    pub lights: Option<LightsRef>,
    pub sound: Option<String>,
    pub mix: Option<MixRef>,
    /// Overrides held while active.
    pub set: BTreeMap<String, Value>,
    #[serde(rename = "do")]
    pub commands: Vec<String>,
    pub on_release: Vec<String>,
    pub scene: Option<String>,
    pub mode: Option<String>,
    /// The few premade controls the operator may turn (`[[knob]]`). Each drives an address this
    /// quick effect already changes; its value lives where firing picks it up ([`KnobSlot`]).
    #[serde(rename = "knob")]
    pub knobs: Vec<Knob>,
    /// A roulette: firing it picks one of these presets by weight (`w`, default 1) and fires that
    /// one instead, as `preset.fire <picked>` would. A roulette has no effects of its own.
    pub pick: Vec<PoolEntry>,
    /// A roulette skips its last N picks while others are left.
    pub avoid_repeat: usize,
    pub auto_fx: Option<AutoFxDef>,
}

/// Where a quick effect keeps a knob's value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KnobSlot {
    /// `set = { "<target>" = … }`: held while it runs.
    Set,
    /// Key `key` of `fx` entry `index`: the target is that entry's trigger address plus the key
    /// (`fx.shake.strength` → `{ name = "shake", strength = … }`, `patch.confetti.count` →
    /// `{ name = "patch.confetti", count = … }`). Built-in effect settings are held while it
    /// runs; other keys (an overlay's `count`, `level`) go with the trigger when it fires.
    Fx { index: usize, key: String },
}

/// Keys of an `fx` entry that are timings, not values a knob can turn.
const FX_TIMINGS: &[&str] = &["name", "hold", "attack", "release"];

impl PresetFx {
    /// The trigger address it fires: `fx.<name>` for a built-in effect, else the name itself.
    pub fn trigger(&self) -> String {
        if self.name.contains('.') { self.name.clone() } else { format!("fx.{}", self.name) }
    }
}

impl PresetDef {
    /// A musical candidate can only affect video, never lights, sound, scenes or persistent state.
    pub fn check_auto_fx(&self) -> Result<(), String> {
        let Some(a) = &self.auto_fx else { return Ok(()) };
        if a.moods.iter().any(|m| !crate::core::context::Mood::ALL.contains(&m.as_str())) {
            return Err("auto_fx moods must be none/chill/groove/bright/heavy/hype".into());
        }
        if a.energy.iter().any(|v| !v.is_finite() || !(0.0..=1.0).contains(v)) || a.energy[0] > a.energy[1] {
            return Err("auto_fx energy must be an ordered finite range within 0..=1".into());
        }
        if !a.weight.is_finite() || !(0.0..=1_000.0).contains(&a.weight) || a.weight == 0.0 {
            return Err("auto_fx weight must be finite and in 0..=1000 (exclusive of zero)".into());
        }
        if self.hold.is_none_or(|h| h.ms() == 0 || h.ms() > 30_000) {
            return Err("auto_fx requires a positive finite hold of at most 30s (20–28s recommended)".into());
        }
        if self.fx.is_empty() || self.fx.iter().any(|fx| {
            let video = !fx.name.contains('.') || fx.name.starts_with("patch.") || fx.name.starts_with("fx.");
            !video || fx.hold.is_some_and(|h| h.ms() == 0 || h.ms() > self.hold.unwrap().ms()) || fx.params.contains_key("hold")
        }) || self.lights.is_some() || self.sound.is_some() || self.mix.is_some() || !self.set.is_empty()
            || !self.commands.is_empty() || !self.on_release.is_empty() || self.scene.is_some() || self.mode.is_some()
            || !self.pick.is_empty() || self.toggle || self.until_released
        {
            return Err("auto_fx must be finite video-only effects without side effects or latching".into());
        }
        Ok(())
    }

    /// Where the value of a knob driving `target` lives, if this quick effect changes it.
    pub fn knob_slot(&self, target: &str) -> Option<KnobSlot> {
        if self.set.contains_key(target) {
            return Some(KnobSlot::Set);
        }
        self.fx.iter().enumerate().find_map(|(index, f)| {
            let key = target.strip_prefix(f.trigger().as_str())?.strip_prefix('.')?;
            (!key.is_empty() && !key.contains('.') && !FX_TIMINGS.contains(&key)).then(|| KnobSlot::Fx { index, key: key.to_string() })
        })
    }

    /// A knob's value as the file has it (`None` when its slot has no value yet: the effect's
    /// own default applies when it fires).
    pub fn knob_value(&self, target: &str) -> Option<&Value> {
        match self.knob_slot(target)? {
            KnobSlot::Set => self.set.get(target),
            KnobSlot::Fx { index, key } => self.fx[index].params.get(&key),
        }
    }

    /// Put a knob's value where firing picks it up. `false` when no knob drives `target`.
    pub fn set_knob_value(&mut self, target: &str, v: Value) -> bool {
        match self.knobs.iter().any(|k| k.target == target).then(|| self.knob_slot(target)).flatten() {
            Some(KnobSlot::Set) => {
                self.set.insert(target.to_string(), v);
                true
            }
            Some(KnobSlot::Fx { index, key }) => {
                self.fx[index].params.insert(key, v);
                true
            }
            None => false,
        }
    }

    /// Knobs are well formed and each drives something this quick effect changes, with a value
    /// of the right kind there.
    fn check_knobs(&self) -> Result<(), String> {
        crate::knob::check_all(&self.knobs)?;
        for k in &self.knobs {
            if self.knob_slot(&k.target).is_none() {
                return Err(format!(
                    "knob “{}”: this quick effect doesn't change `{}` — put it in `set`, or name a setting of one of its effects (like `fx.shake.strength`)",
                    k.label, k.target
                ));
            }
            if let Some(v) = self.knob_value(&k.target)
                && k.fit(v).is_none()
            {
                let want = match k.kind {
                    KnobKind::Number => "a number",
                    KnobKind::Color => "a colour written \"#rrggbb\"",
                    KnobKind::Choice => "one of its options' values",
                };
                return Err(format!("knob “{}”: the value this quick effect has for `{}` must be {want}", k.label, k.target));
            }
        }
        Ok(())
    }

    /// A roulette's own shape: usable weights and nothing of its own to run. Its targets are
    /// checked across files ([`Config::validate`]).
    fn check_pick(&self) -> Result<(), String> {
        if let Some(e) = self.pick.iter().find(|e| !(e.w.is_finite() && e.w >= 0.0)) {
            return Err(format!("pick `{}`: `w` must be a number ≥ 0", e.name));
        }
        if !self.pick.iter().any(|e| e.w > 0.0) {
            return Err("`pick` needs at least one entry with `w` > 0".into());
        }
        if self.pick.iter().any(|e| e.name == self.name) {
            return Err("`pick` can't name this preset itself".into());
        }
        let own = [
            ("fx", !self.fx.is_empty()),
            ("lights", self.lights.is_some()),
            ("sound", self.sound.is_some()),
            ("mix", self.mix.is_some()),
            ("set", !self.set.is_empty()),
            ("do", !self.commands.is_empty()),
            ("on_release", !self.on_release.is_empty()),
            ("scene", self.scene.is_some()),
            ("mode", self.mode.is_some()),
        ];
        if let Some((key, _)) = own.iter().find(|(_, has)| *has) {
            return Err(format!("a preset with `pick` fires the one it picks; it can't have `{key}` too"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Cooldown {
    pub global: Option<Dur>,
    pub per_actor: Option<Dur>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleDef {
    pub name: String,
    pub when: String,
    #[serde(rename = "if")]
    pub condition: Option<String>,
    #[serde(rename = "do")]
    pub commands: Vec<String>,
    pub cooldown: Cooldown,
    pub modes: Vec<String>,
    pub scenes: Vec<String>,
    pub enabled: bool,
    pub priority: Option<u16>,
    /// Stop evaluating later rules for this event when this one fires.
    #[serde(rename = "final")]
    pub is_final: bool,
    /// Policy (§12.1) for viewer-triggered firings: minimum role of the event's actor.
    pub role: Option<se_proto::Role>,
    /// Policy: viewer-triggered firings wait in the approval queue for a mod.
    pub approval: bool,
    /// Source file (set by the loader).
    #[serde(skip)]
    pub file: String,
}

impl Default for RuleDef {
    fn default() -> Self {
        RuleDef {
            name: String::new(),
            when: String::new(),
            condition: None,
            commands: Vec::new(),
            cooldown: Cooldown::default(),
            modes: Vec::new(),
            scenes: Vec::new(),
            enabled: true,
            priority: None,
            is_final: false,
            role: None,
            approval: false,
            file: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BindMode {
    #[default]
    Add,
    Multiply,
    Replace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Curve {
    #[default]
    Linear,
    Exp,
    Log,
    Smoothstep,
    FaderTaper,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Takeover {
    #[default]
    Jump,
    Pickup,
    Scale,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BindingDef {
    pub name: String,
    pub target: String,
    pub signal: String,
    pub gain: f64,
    pub offset: f64,
    pub gate: Option<f64>,
    pub attack: Option<Dur>,
    pub release: Option<Dur>,
    pub curve: Curve,
    pub invert: bool,
    /// Output range `[min, max]` the shaped 0–1 value maps into.
    pub range: Option<[f64; 2]>,
    /// Adaptive gain over a window (`true` = 10 s, or a duration).
    pub auto_normalize: Option<Value>,
    pub mode: BindMode,
    pub takeover: Option<Takeover>,
    /// `always`, `scene.<name>`, `preset.<name>`, `mode.<name>`, or an expression.
    pub scope: Option<String>,
    /// Send state changes back to the controller (MIDI/deck feedback), used by `se-input`.
    pub feedback: bool,
    pub enabled: bool,
    /// Source file (set by the loader).
    #[serde(skip)]
    pub file: String,
}

impl Default for BindingDef {
    fn default() -> Self {
        BindingDef {
            name: String::new(),
            target: String::new(),
            signal: String::new(),
            gain: 1.0,
            offset: 0.0,
            gate: None,
            attack: None,
            release: None,
            curve: Curve::Linear,
            invert: false,
            range: None,
            auto_normalize: None,
            mode: BindMode::Add,
            takeover: None,
            scope: None,
            feedback: false,
            enabled: true,
            file: String::new(),
        }
    }
}

/// The whole typed configuration.
#[derive(Clone, Debug, Default)]
pub struct Config {
    pub project: ProjectDef,
    pub scenes: BTreeMap<String, SceneDef>,
    pub transitions: BTreeMap<String, TransitionDef>,
    pub presets: BTreeMap<String, PresetDef>,
    pub fx_chains: BTreeMap<String, FxChainDef>,
    pub rules: Vec<RuleDef>,
    pub bindings: Vec<BindingDef>,
    /// Kinds owned by other subsystems (timelines, commands, alerts, mixes, controllers,
    /// rewards, layouts, lights/*), by kind then name.
    pub other: BTreeMap<String, BTreeMap<String, toml::Table>>,
    /// Per-file parse errors (the last good version of the file stays live).
    pub errors: Vec<ConfigError>,
    /// Which file each item came from (for write-back and error display): `kind/name` → path.
    pub files: BTreeMap<String, String>,
}

fn parse<T: for<'de> Deserialize<'de>>(t: &toml::Table) -> Result<T, String> {
    toml::Value::Table(t.clone()).try_into().map_err(|e: toml::de::Error| e.message().to_string())
}

fn normalize_fx_value(value: &mut toml::Value) -> Result<(), String> {
    let mut fx: Vec<FxRef> = value.clone().try_into().map_err(|e: toml::de::Error| e.to_string())?;
    normalize_fx(&mut fx)?;
    *value = toml::Value::try_from(fx).map_err(|e| e.to_string())?;
    Ok(())
}

fn normalize_table_fx(table: &mut toml::Table, key: &str) -> Result<(), String> {
    if let Some(value) = table.get_mut(key) { normalize_fx_value(value)?; }
    Ok(())
}

/// Tables in a file that holds either one item or `[[key]]` arrays.
fn items(t: &toml::Table, key: &str) -> Vec<toml::Table> {
    match t.get(key) {
        Some(toml::Value::Array(a)) => a.iter().filter_map(|v| v.as_table().cloned()).collect(),
        _ => vec![t.clone()],
    }
}

impl Config {
    pub fn build(files: &[SourceFile]) -> Config {
        let mut c = Config::default();
        for f in files {
            if let Err(msg) = c.add_file(f) {
                c.errors.push(ConfigError { file: f.path.clone(), msg });
            }
        }
        c.validate();
        c
    }

    fn add_file(&mut self, f: &SourceFile) -> Result<(), String> {
        match f.kind.as_str() {
            "project" => {
                let mut p: ProjectDef = parse(&f.table)?;
                if p.schema > SCHEMA_VERSION {
                    return Err(format!("schema {} is newer than this engine ({SCHEMA_VERSION})", p.schema));
                }
                if let Some(toml::Value::Table(render)) = p.extra.get_mut("render") {
                    for key in ["canvas_fx", "output_fx"] {
                        if let Some(toml::Value::Table(hosts)) = render.get_mut(key) {
                            for (_, value) in hosts.iter_mut() { normalize_fx_value(value)?; }
                        }
                    }
                }
                self.project = p;
            }
            "scenes" => {
                let mut s: SceneDef = parse(&f.table)?;
                s.name = f.name.clone();
                for canvas in s.canvas.values_mut() {
                    normalize_fx(&mut canvas.fx)?;
                    for n in &mut canvas.nodes {
                        if n.src.is_empty() {
                            return Err("node without `src`".into());
                        }
                        if n.id.is_empty() {
                            n.id = n.src.clone();
                        }
                        normalize_fx(&mut n.fx)?;
                    }
                    for g in &mut canvas.groups { normalize_fx(&mut g.fx)?; }
                    validate_groups(canvas)?;
                }
                normalize_fx(&mut s.fx)?;
                self.scenes.insert(s.name.clone(), s);
            }
            "fx_chains" => {
                let mut chain: FxChainDef = parse(&f.table)?;
                chain.name = f.name.clone();
                normalize_fx(&mut chain.fx)?;
                self.fx_chains.insert(chain.name.clone(), chain);
            }
            "transitions" => {
                let mut t: TransitionDef = parse(&f.table)?;
                t.name = f.name.clone();
                self.transitions.insert(t.name.clone(), t);
            }
            "presets" => {
                let mut p: PresetDef = parse(&f.table)?;
                p.name = f.name.clone();
                for c in p.commands.iter().chain(&p.on_release) {
                    se_proto::Op::parse(c).map_err(|e| format!("command `{c}`: {e}"))?;
                }
                p.check_knobs()?;
                p.check_auto_fx()?;
                if p.lane.as_deref().is_some_and(|l| l.trim().is_empty()) {
                    return Err("`lane` must name a lane".into());
                }
                if !p.pick.is_empty() {
                    p.check_pick()?;
                }
                self.presets.insert(p.name.clone(), p);
            }
            "rules" => {
                for (i, t) in items(&f.table, "rule").into_iter().enumerate() {
                    let mut r: RuleDef = parse(&t)?;
                    if r.name.is_empty() {
                        r.name = format!("{}#{}", f.name, i + 1);
                    }
                    r.file = f.path.clone();
                    if r.when.is_empty() {
                        return Err(format!("rule `{}` has no `when`", r.name));
                    }
                    if let Some(cond) = &r.condition {
                        se_expr::Expr::parse(cond).map_err(|e| format!("rule `{}` if: {e}", r.name))?;
                    }
                    for c in &r.commands {
                        se_proto::Op::parse(c).map_err(|e| format!("rule `{}` command `{c}`: {e}", r.name))?;
                    }
                    self.rules.push(r);
                }
            }
            "bindings" => {
                for (i, t) in items(&f.table, "binding").into_iter().enumerate() {
                    let mut b: BindingDef = parse(&t)?;
                    if b.name.is_empty() {
                        b.name = format!("{}#{}", f.name, i + 1);
                    }
                    b.file = f.path.clone();
                    if b.target.is_empty() || b.signal.is_empty() {
                        return Err(format!("binding `{}` needs `target` and `signal`", b.name));
                    }
                    self.bindings.push(b);
                }
            }
            other => {
                let mut table = f.table.clone();
                if other == "sources" {
                    normalize_table_fx(&mut table, "fx")?;
                }
                // Controller mappings (§7 `controllers/faders.toml`) carry `[[binding]]` entries
                // that are ordinary bindings (takeover, curves, scope) owned by the core.
                if other == "controllers"
                    && let Some(toml::Value::Array(arr)) = f.table.get("binding")
                {
                    for (i, t) in arr.iter().filter_map(|v| v.as_table()).enumerate() {
                        let mut b: BindingDef = parse(t)?;
                        if b.name.is_empty() {
                            b.name = format!("{}#{}", f.name, i + 1);
                        }
                        b.file = f.path.clone();
                        if b.target.is_empty() || b.signal.is_empty() {
                            return Err(format!("binding `{}` needs `target` and `signal`", b.name));
                        }
                        self.bindings.push(b);
                    }
                }
                self.other.entry(other.to_string()).or_default().insert(f.name.clone(), table);
            }
        }
        self.files.insert(format!("{}/{}", f.kind, f.name), f.path.clone());
        Ok(())
    }

    /// Cross-file checks (reported, never fatal).
    fn validate(&mut self) {
        let mut errs = Vec::new();
        let known = |n: &str| self.transitions.contains_key(n) || BUILTIN_TRANSITIONS.contains(&n);
        // a pool (scene's or the project default) and its `from` pairs
        let check = |p: &TransitionPool, err: &mut dyn FnMut(String)| {
            for n in p.pool.iter().map(|e| &e.name).chain(&p.name) {
                if !known(n) {
                    err(format!("unknown transition `{n}`"));
                }
            }
            for (from, pair) in &p.from {
                if !self.scenes.contains_key(from) {
                    err(format!("[transitions.from.{from}]: unknown scene `{from}`"));
                }
                if !pair.from.is_empty() {
                    err(format!("[transitions.from.{from}] can't have its own `from` pools"));
                }
                for n in pair.pool.iter().map(|e| &e.name).chain(&pair.name) {
                    if !known(n) {
                        err(format!("[transitions.from.{from}]: unknown transition `{n}`"));
                    }
                }
            }
        };
        for s in self.scenes.values() {
            let file = self.files.get(&format!("scenes/{}", s.name)).cloned().unwrap_or_default();
            check(&s.transitions, &mut |msg| errs.push(ConfigError { file: file.clone(), msg }));
        }
        let project_file = self.files.get("project/project").cloned().unwrap_or_else(|| "project.toml".into());
        check(&self.project.transitions, &mut |msg| errs.push(ConfigError { file: project_file.clone(), msg }));
        match self.transition_vote() {
            Ok(Some(v)) => {
                for n in v.choices.iter().filter(|n| !known(n)) {
                    errs.push(ConfigError { file: project_file.clone(), msg: format!("[transition_vote] choices: unknown transition `{n}`") });
                }
            }
            Ok(None) => {}
            Err(e) => errs.push(ConfigError { file: project_file.clone(), msg: e }),
        }
        for e in [self.context().err(), self.fx_control().err()].into_iter().flatten() {
            errs.push(ConfigError { file: project_file.clone(), msg: e });
        }
        // roulettes pick existing presets that aren't roulettes themselves
        for p in self.presets.values().filter(|p| !p.pick.is_empty()) {
            let file = self.files.get(&format!("presets/{}", p.name)).cloned().unwrap_or_default();
            for e in &p.pick {
                let msg = match self.presets.get(&e.name) {
                    None => format!("preset `{}` pick: unknown preset `{}`", p.name, e.name),
                    Some(t) if !t.pick.is_empty() => format!("preset `{}` pick: `{}` is itself a roulette (`pick`)", p.name, e.name),
                    Some(_) => continue,
                };
                errs.push(ConfigError { file: file.clone(), msg });
            }
        }
        self.errors.extend(errs);
    }

    /// `[transition_vote]` of project.toml (`None` when the section is absent).
    pub fn transition_vote(&self) -> Result<Option<TransitionVoteDef>, String> {
        match self.project.extra.get("transition_vote") {
            None => Ok(None),
            Some(v) => v.clone().try_into().map(Some).map_err(|e: toml::de::Error| format!("[transition_vote]: {}", e.message())),
        }
    }

    /// `[context]` of project.toml (defaults when the section is absent).
    pub fn context(&self) -> Result<ContextDef, String> {
        let def: ContextDef = match self.project.extra.get("context") {
            None => ContextDef::default(),
            Some(v) => v.clone().try_into().map_err(|e: toml::de::Error| format!("[context]: {}", e.message()))?,
        };
        def.musical_fx.validate()?;
        Ok(def)
    }

    /// `[fx]` of project.toml (nothing exempt when the section is absent).
    pub fn fx_control(&self) -> Result<FxControlDef, String> {
        match self.project.extra.get("fx") {
            None => Ok(FxControlDef::default()),
            Some(v) => v.clone().try_into().map_err(|e: toml::de::Error| format!("[fx]: {}", e.message())),
        }
    }

    /// Every transition name: built-ins first, then project files (a file overriding a
    /// built-in is listed once).
    pub fn transition_names(&self) -> Vec<String> {
        let mut names: Vec<String> = BUILTIN_TRANSITIONS.iter().map(|s| s.to_string()).collect();
        names.extend(self.transitions.keys().filter(|k| !BUILTIN_TRANSITIONS.contains(&k.as_str())).cloned());
        names
    }

    /// Keep the previous version of any file that failed to parse this time.
    pub fn merge_last_good(mut self, prev: &Config, failed_paths: &[String]) -> Config {
        for path in failed_paths {
            let Some(key) = prev.files.iter().find(|(_, p)| *p == path).map(|(k, _)| k.clone()) else { continue };
            let (kind, name) = key.split_once('/').unwrap_or((&key, ""));
            match kind {
                "project" => self.project = prev.project.clone(),
                "scenes" => {
                    if let Some(s) = prev.scenes.get(name) {
                        self.scenes.insert(name.into(), s.clone());
                    }
                }
                "fx_chains" => {
                    if let Some(chain) = prev.fx_chains.get(name) { self.fx_chains.insert(name.into(), chain.clone()); }
                }
                "transitions" => {
                    if let Some(s) = prev.transitions.get(name) {
                        self.transitions.insert(name.into(), s.clone());
                    }
                }
                "presets" => {
                    if let Some(s) = prev.presets.get(name) {
                        self.presets.insert(name.into(), s.clone());
                    }
                }
                "rules" => self.rules.extend(prev.rules.iter().filter(|r| &r.file == path).cloned()),
                "bindings" => self.bindings.extend(prev.bindings.iter().filter(|b| &b.file == path).cloned()),
                other => {
                    if other == "controllers" {
                        self.bindings.extend(prev.bindings.iter().filter(|b| &b.file == path).cloned());
                    }
                    if let Some(t) = prev.other.get(other).and_then(|m| m.get(name)) {
                        self.other.entry(other.into()).or_default().insert(name.into(), t.clone());
                    }
                }
            }
            self.files.insert(key.clone(), path.clone());
        }
        self
    }

    pub fn modes(&self) -> Vec<String> {
        if self.project.modes.is_empty() { DEFAULT_MODES.iter().map(|s| s.to_string()).collect() } else { self.project.modes.clone() }
    }
}

pub const BUILTIN_TRANSITIONS: &[&str] = &["cut", "morph", "fade"];

#[cfg(test)]
mod tests {
    use super::*;

    fn file(kind: &str, name: &str, src: &str) -> SourceFile {
        SourceFile { kind: kind.into(), name: name.into(), path: format!("{kind}/{name}.toml"), table: toml::from_str(src).unwrap() }
    }

    #[test]
    fn plan_examples_parse() {
        let files = vec![
            file(
                "scenes",
                "duo",
                r#"
[canvas.wide]
nodes = [
  { src = "cam_face", rect = [0.02, 0.05, 0.47, 0.90], radius = 24 },
  { src = "cam_desk", rect = [0.51, 0.05, 0.47, 0.90], radius = 24, fx = [{ name = "vhs", when = "mode == 'chill'" }] },
  { src = "youtube",  rect = [0.70, 0.70, 0.28, 0.25], when = "queue.playing" },
]
[canvas.tall]
nodes = [
  { src = "cam_face", rect = [0.0, 0.0, 1.0, 0.55] },
  { src = "cam_desk", rect = [0.0, 0.55, 1.0, 0.45] },
]
[transitions]
pool = [{ name = "morph", w = 3 }, { name = "morph+glitch", w = 1 }, { name = "zoomblur", w = 1 }]
avoid_repeat = 2
ms = [500, 900]
lights = { cue = "warm_duo" }
"#,
            ),
            file(
                "bindings",
                "kick_shake",
                "target = \"scene.*.node.cam_face.offset_y\"\nsignal = \"music.kick\"\nrange = [0, 12]\nattack = \"5ms\"\nrelease = \"120ms\"\nscope = \"preset.hype\"",
            ),
            file(
                "rules",
                "subs",
                "[[rule]]\nwhen = \"twitch.sub\"\nif = \"event.tier >= 2 && mode == 'live'\"\ndo = [\"preset.fire sub_big\"]\ncooldown = { global = \"10s\" }",
            ),
            file(
                "presets",
                "hype",
                "fx = [{ name = \"rgb_split\", hold = \"8s\" }, { name = \"patch.confetti\" }]\nlights = { cue = \"chase_fast\", hold = \"8s\" }\nsound = \"airhorn\"\nconflict = \"replace\"",
            ),
            file("transitions", "morph+glitch", "kind = \"combined\"\nshader = \"transitions/glitch.wgsl\""),
            file("transitions", "zoomblur", "kind = \"shader\"\nshader = \"transitions/zoomblur.wgsl\""),
        ];
        let c = Config::build(&files);
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        let duo = &c.scenes["duo"];
        assert_eq!(duo.nodes("wide").len(), 3);
        assert_eq!(duo.nodes("wide")[1].fx[0].when.as_deref(), Some("mode == 'chill'"));
        assert_eq!(duo.transitions.ms_range(), Some((500, 900)));
        assert_eq!(c.bindings[0].attack, Some(Dur(5)));
        assert_eq!(c.rules[0].cooldown.global, Some(Dur(10_000)));
        assert_eq!(c.presets["hype"].fx[0].hold, Some(Dur(8000)));
        assert_eq!(c.presets["hype"].conflict, Conflict::Replace);
    }

    #[test]
    fn native_fit_clips_top_right_and_preserves_cropped_pixels() {
        let fit = NodeFit::Native;
        let uv = [0.125, 0.25, 0.875, 0.75];
        assert_eq!(fit.placement([10.0, 20.0, 32.0, 16.0], uv, [128.0, 128.0], 1.0),
            ([10.0, 20.0, 32.0, 16.0], [0.125, 0.625, 0.375, 0.75]));
        assert_eq!(fit.placement([5.0, 10.0, 16.0, 8.0], uv, [128.0, 128.0], 0.5),
            ([5.0, 10.0, 16.0, 8.0], [0.125, 0.625, 0.375, 0.75]));
        assert_eq!(fit.placement([10.0, 20.0, 128.0, 96.0], uv, [128.0, 128.0], 1.0),
            ([10.0, 52.0, 96.0, 64.0], uv));
        let (rect, crop) = fit.placement([10.0, 20.0, 32.0, 16.0], uv, [0.0, 128.0], 1.0);
        assert_eq!(rect, [10.0, 36.0, 0.0, 0.0]);
        assert!(crop.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn controller_files_contribute_bindings() {
        let c = Config::build(&[file(
            "controllers",
            "faders",
            "[[binding]]\ntarget = \"mixer.16r.ch.3.fader\"\nsignal = \"midi.xtouch.fader.3\"\ncurve = \"fader_taper\"\ntakeover = \"pickup\"\nfeedback = true\n[page.show]\nkeys = []",
        )]);
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c.bindings.len(), 1);
        assert_eq!(c.bindings[0].takeover, Some(Takeover::Pickup));
        assert_eq!(c.bindings[0].file, "controllers/faders.toml");
        assert!(c.other["controllers"]["faders"].contains_key("page"));
    }

    #[test]
    fn broken_file_reported_and_last_good_kept() {
        let good = Config::build(&[file("presets", "hype", "sound = \"airhorn\"")]);
        let broken = [file("presets", "hype", "do = [\"$(boom)\"]")];
        let next = Config::build(&broken);
        assert_eq!(next.errors.len(), 1);
        let merged = next.merge_last_good(&good, &["presets/hype.toml".to_string()]);
        assert_eq!(merged.presets["hype"].sound.as_deref(), Some("airhorn"));
    }
    #[test]
    fn duplicate_slots_fail_and_last_good_chains_survive() {
        let good = Config::build(&[file("fx_chains","stage","fx=[{name='vhs'},{name='vhs',enabled=false,triggered=true}]")]);
        assert_eq!(good.fx_chains["stage"].fx[1].id,"vhs_2");
        assert_eq!(good.fx_chains["stage"].fx[1].enabled,Some(false));
        assert!(good.fx_chains["stage"].fx[1].triggered);
        let next = Config::build(&[file("fx_chains","stage","fx=[{id='same',name='vhs'},{id='same',name='grade'}]")]);
        assert!(!next.fx_chains.contains_key("stage"));
        let merged = next.merge_last_good(&good,&["fx_chains/stage.toml".into()]);
        assert_eq!(merged.fx_chains["stage"].fx,good.fx_chains["stage"].fx);
    }

    #[test]
    fn invalid_composite_membership_rejects_scene() {
        for groups in [
            "[{id='g',nodes=['missing']}]",
            "[{id='g',nodes=['cam']},{id='g',nodes=[]}]",
            "[{id='a',nodes=['cam']},{id='b',nodes=['cam']}]",
        ] {
            let text = format!("[canvas.wide]\nnodes=[{{src='cam'}}]\ngroups={groups}\n");
            let c = Config::build(&[file("scenes","show",&text)]);
            assert!(!c.scenes.contains_key("show"));
            assert_eq!(c.errors[0].file,"scenes/show.toml");
        }
    }
}
