//! Typed project configuration (§3.3, §7). Built from parsed TOML files; every file parses
//! independently so one broken file never takes down the rest (last-good is kept per file by
//! [`Config::merge_last_good`]).

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
            extra: BTreeMap::new(),
        }
    }
}

/// Effect reference on a source, node, scene, canvas, or output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FxRef {
    pub name: String,
    /// Conditional attachment (`mode == 'chill'`).
    pub when: Option<String>,
    /// Exclusive group (only one active per group).
    pub group: Option<String>,
    pub hold: Option<Dur>,
    /// Always on (true) or only while triggered (false).
    pub enabled: Option<bool>,
    #[serde(flatten)]
    pub params: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeDef {
    /// Node id within the scene; defaults to `src`.
    pub id: String,
    pub src: String,
    /// Normalized x, y, w, h.
    pub rect: [f32; 4],
    /// Normalized crop: left, top, right, bottom insets.
    pub crop: [f32; 4],
    pub radius: f32,
    pub opacity: f32,
    pub z: i32,
    pub blend: String,
    pub mask: Option<String>,
    pub fx: Vec<FxRef>,
    pub when: Option<String>,
    /// Enter/exit style for morph transitions: `fade | scale | slide_left | slide_right | slide_up | slide_down | none`.
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
            crop: [0.0; 4],
            radius: 0.0,
            opacity: 1.0,
            z: 0,
            blend: "normal".into(),
            mask: None,
            fx: Vec::new(),
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SceneCanvas {
    pub nodes: Vec<NodeDef>,
    pub fx: Vec<FxRef>,
    pub background: Option<Value>,
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
}

impl TransitionPool {
    pub fn ms_range(&self) -> Option<(u32, u32)> {
        match &self.ms {
            Some(Value::List(l)) if l.len() == 2 => Some((l[0].as_f64()? as u32, l[1].as_f64()? as u32)),
            Some(v) => v.as_f64().map(|m| (m as u32, m as u32)),
            None => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LightsRef {
    pub cue: Option<String>,
    pub cuelist: Option<String>,
    pub hold: Option<Dur>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
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
    pub on_enter: Vec<String>,
    pub on_exit: Vec<String>,
    /// Scene-layer values applied to any address while this scene is on program.
    pub set: BTreeMap<String, Value>,
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
    /// `morph | shader | combined | cut`
    pub kind: String,
    /// WGSL file (relative to the project) for shader/combined.
    pub shader: Option<String>,
    pub ms: Option<Dur>,
    pub ease: se_proto::Ease,
    pub enter: String,
    pub exit: String,
    #[serde(flatten)]
    pub params: BTreeMap<String, Value>,
}

impl Default for TransitionDef {
    fn default() -> Self {
        TransitionDef {
            name: String::new(),
            kind: "morph".into(),
            shader: None,
            ms: None,
            ease: se_proto::Ease::InOutCubic,
            enter: "fade".into(),
            exit: "fade".into(),
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PresetFx {
    pub name: String,
    pub hold: Option<Dur>,
    #[serde(flatten)]
    pub params: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MixRef {
    pub snapshot: String,
    pub fade: Option<Dur>,
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
    /// Pressing again while active releases it.
    pub toggle: bool,
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
                let p: ProjectDef = parse(&f.table)?;
                if p.schema > SCHEMA_VERSION {
                    return Err(format!("schema {} is newer than this engine ({SCHEMA_VERSION})", p.schema));
                }
                self.project = p;
            }
            "scenes" => {
                let mut s: SceneDef = parse(&f.table)?;
                s.name = f.name.clone();
                for canvas in s.canvas.values_mut() {
                    for n in &mut canvas.nodes {
                        if n.src.is_empty() {
                            return Err("node without `src`".into());
                        }
                        if n.id.is_empty() {
                            n.id = n.src.clone();
                        }
                    }
                }
                self.scenes.insert(s.name.clone(), s);
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
                self.other.entry(other.to_string()).or_default().insert(f.name.clone(), f.table.clone());
            }
        }
        self.files.insert(format!("{}/{}", f.kind, f.name), f.path.clone());
        Ok(())
    }

    /// Cross-file checks (reported, never fatal).
    fn validate(&mut self) {
        let mut errs = Vec::new();
        for s in self.scenes.values() {
            for e in &s.transitions.pool {
                if !self.transitions.contains_key(&e.name) && !BUILTIN_TRANSITIONS.contains(&e.name.as_str()) {
                    errs.push(ConfigError {
                        file: self.files.get(&format!("scenes/{}", s.name)).cloned().unwrap_or_default(),
                        msg: format!("unknown transition `{}`", e.name),
                    });
                }
            }
        }
        self.errors.extend(errs);
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
    fn broken_file_reported_and_last_good_kept() {
        let good = Config::build(&[file("presets", "hype", "sound = \"airhorn\"")]);
        let broken = [file("presets", "hype", "do = [\"$(boom)\"]")];
        let next = Config::build(&broken);
        assert_eq!(next.errors.len(), 1);
        let merged = next.merge_last_good(&good, &["presets/hype.toml".to_string()]);
        assert_eq!(merged.presets["hype"].sound.as_deref(), Some("airhorn"));
    }
}
