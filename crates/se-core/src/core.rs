//! The deterministic core: a fixed-timestep tick that applies inputs, runs rules, presets,
//! triggers, scenes, bindings, and resolves the state tree (§2, §3.1).
//!
//! Time inside the core is `t0 + tick * period`, so a session replay that feeds the same
//! inputs at the same tick indices reproduces the same state.

use crate::bindings::{BindScope, BindingRt};
use crate::config::{Config, Conflict, KnobSlot, LightsRef, PresetDef, PresetFx, SceneDef};
use crate::rng::Rng;
use crate::signals::{Lfo, Signals, builtin_lfos};
use crate::state::{Anim, Mod, Override, StateTree};
use crate::trace::Trace;
use crate::triggers::{Instance, PAYLOAD_FIELDS, TriggerPayload, TriggerRt, TriggerSpec};
use se_expr::{Expr, Scope};
use se_proto::wire::{Provenance, TraceRec};
use se_proto::{Actor, Command, Event, Id, Meta, Op, Origin, PRIORITY_CHAT, PRIORITY_MANUAL, PRIORITY_PRESET, Role, Ts, Value, address, next_id};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap};

#[path = "timeline.rs"]
pub mod timeline;

const MS: Ts = 1_000_000;
const MAX_EVENTS_PER_TICK: usize = 5000;
const MAX_CHAIN_DEPTH: u32 = 16;

/// Something fed into the core.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Input {
    Command {
        cmd: Command,
    },
    Event {
        event: Event,
    },
    Signal {
        name: String,
        value: f32,
    },
    Signals {
        values: Vec<(String, f32)>,
    },
    Declare {
        address: String,
        meta: Meta,
    },
    DeclareTrigger {
        address: String,
        spec: TriggerSpec,
    },
    /// Adapter readback (meters, mixer state, stats): sets the base value, never persisted.
    Publish {
        address: String,
        value: Value,
    },
    Remove {
        prefix: String,
    },
    /// Timecode observation (MTC/LTC input, or a signal-derived media/scrub position recorded
    /// by the core itself) for the timelines following `source` (§2.7).
    Timecode {
        source: String,
        obs: se_clock::timecode::TcObs,
    },
    #[serde(skip)]
    Config {
        config: Box<Config>,
    },
}

impl Input {
    /// Inputs recorded in the session log for replay.
    pub fn is_replayable(&self) -> bool {
        matches!(
            self,
            Input::Command { .. }
                | Input::Event { .. }
                | Input::Publish { .. }
                | Input::Declare { .. }
                | Input::DeclareTrigger { .. }
                | Input::Remove { .. }
                | Input::Timecode { .. }
        )
    }
}

/// Something the core produced during a tick.
#[derive(Clone, Debug)]
pub enum Output {
    Changes(Vec<(String, Value)>),
    Event(Event),
    /// Subsystem action for adapters (routed by name prefix).
    Action(Command),
    Ack {
        id: Id,
        ok: bool,
        error: Option<String>,
    },
    /// Base value edited: write back to the project file.
    PersistBase {
        address: String,
        value: Value,
    },
    Trace(Vec<TraceRec>),
    RuntimeDirty,
    Log {
        level: &'static str,
        msg: String,
    },
}

/// Persisted live runtime state for crash recovery (§17.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimeState {
    pub mode: String,
    pub program: String,
    pub preview: String,
    pub overrides: Vec<(String, Override)>,
    pub bases: Vec<(String, Value)>,
    pub presets: Vec<PersistedPreset>,
    pub disabled_rules: Vec<String>,
    pub disabled_bindings: Vec<String>,
    pub transition_history: Vec<String>,
    pub rng: u64,
    #[serde(default)]
    pub timelines: Vec<timeline::PersistedTimeline>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersistedPreset {
    pub name: String,
    pub priority: u16,
    pub remaining_ms: Option<u64>,
    pub payload: Value,
}

#[derive(Clone, Debug)]
struct ActivePreset {
    name: String,
    inst: Id,
    started: Ts,
    release_at: Option<Ts>,
    priority: u16,
    key: String,
    payload: Value,
    trace: Id,
    fx: Vec<String>,
    lights: Option<LightsRef>,
    actor: Option<Actor>,
}

struct RuleRt {
    def: crate::config::RuleDef,
    cond: Option<Expr>,
    last: Option<Ts>,
    per_actor: HashMap<String, Ts>,
    enabled: bool,
}

/// Execution context for commands run from rules/presets/timelines.
#[derive(Clone, Debug, Default)]
struct Ctx {
    key: Option<String>,
    priority: Option<u16>,
    parent: Option<Id>,
    actor: Option<Actor>,
    depth: u32,
    event: Option<Event>,
}

struct Scheduled {
    due: Ts,
    seq: u64,
    op: Op,
    origin: Origin,
    ctx: Ctx,
}

impl PartialEq for Scheduled {
    fn eq(&self, o: &Self) -> bool {
        self.due == o.due && self.seq == o.seq
    }
}
impl Eq for Scheduled {}
impl PartialOrd for Scheduled {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Scheduled {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (self.due, self.seq).cmp(&(o.due, o.seq))
    }
}

#[derive(Clone, Debug)]
struct UndoEntry {
    address: String,
    old: Value,
    new: Value,
}

struct Transition {
    from: String,
    to: String,
    name: String,
    start: Ts,
    dur: Ts,
}

/// Well-known addresses.
pub mod addr {
    pub const MODE: &str = "show.mode";
    pub const PROGRAM: &str = "show.scene.program";
    pub const PREVIEW: &str = "show.scene.preview";
    pub const DIRECT: &str = "show.direct";
    pub const TR_NAME: &str = "show.transition.name";
    pub const TR_MS: &str = "show.transition.ms";
    pub const TR_ACTIVE: &str = "show.transition.active";
    pub const TR_PROGRESS: &str = "show.transition.progress";
    pub const TR_FROM: &str = "show.transition.from";
    pub const TR_START: &str = "show.transition.start";
    /// Live chat votes for the next take's transition: `{transition: votes}`.
    pub const TR_VOTES: &str = "show.transition.votes";
    pub const PANIC: &str = "show.panic";
}

pub struct Core {
    pub config: Config,
    state: StateTree,
    signals: Signals,
    trace: Trace,
    rng: Rng,
    t0: Ts,
    tick: u64,
    period: Ts,
    inputs: Vec<Input>,
    applied: Vec<(u64, Input)>,
    events: std::collections::VecDeque<(Event, Ctx)>,
    outbox: Vec<Output>,
    cause: HashMap<usize, Id>,
    rules: Vec<RuleRt>,
    bindings: Vec<BindingRt>,
    lfos: Vec<Lfo>,
    presets: Vec<ActivePreset>,
    preset_queue: Vec<(String, Command)>,
    triggers: BTreeMap<String, TriggerRt>,
    trigger_specs: HashMap<String, TriggerSpec>,
    scheduled: BinaryHeap<Reverse<Scheduled>>,
    sched_seq: u64,
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    transition: Option<Transition>,
    transition_history: Vec<String>,
    /// Chat votes for the next take's transition: (voter id, transition, expires at).
    transition_votes: Vec<(String, String, Ts)>,
    scene_layer: Vec<usize>,
    caps: Vec<(String, [f64; 2])>,
    chat_caps: Vec<(String, [f64; 2])>,
    chat_ttl: Ts,
    caps_gen: u64,
    scene_prefixes: Vec<String>,
    runtime_dirty: bool,
    beat_sig: usize,
    time_sig: usize,
    /// Wall-clock-free tick bookkeeping for `time.*` signals.
    pub history_every: u64,
    /// Policy pipeline for chat-, bits-, and points-originated activity (§12.1).
    policy: crate::policy::Policy,
    /// Timelines and the timecode sources they chase (§2.7).
    timelines: timeline::Timelines,
}

struct EvalScope<'a> {
    core: &'a Core,
    event: Option<&'a Event>,
}

impl Scope for EvalScope<'_> {
    fn lookup(&self, path: &str) -> Value {
        if let Some(ev) = self.event {
            if path == "event" {
                return ev.payload.clone();
            }
            if let Some(rest) = path.strip_prefix("event.") {
                return event_field(ev, rest);
            }
        }
        match path {
            "mode" => return self.core.mode_str().into(),
            "scene" => return self.core.state.get(addr::PROGRAM).cloned().unwrap_or_default(),
            "preview" => return self.core.state.get(addr::PREVIEW).cloned().unwrap_or_default(),
            _ => {}
        }
        if let Some(v) = self.core.state.get(path) {
            return v.clone();
        }
        if let Some(v) = self.core.signals.get(path) {
            return Value::Float(v as f64);
        }
        Value::Null
    }
}

fn event_field(ev: &Event, rest: &str) -> Value {
    match rest {
        "type" => return ev.ty.clone().into(),
        "origin" => return ev.origin.as_str().into(),
        "id" => return Value::Int(ev.id as i64),
        "user" | "actor.name" if ev.payload.get_path("user").is_none() => {
            return ev.actor.as_ref().map(|a| Value::Str(a.name.clone())).unwrap_or_default();
        }
        "actor.id" => return ev.actor.as_ref().map(|a| Value::Str(a.id.clone())).unwrap_or_default(),
        "actor.platform" => return ev.actor.as_ref().map(|a| Value::Str(a.platform.clone())).unwrap_or_default(),
        "role" | "actor.role" => {
            return ev.actor.as_ref().map(|a| Value::Str(format!("{:?}", a.top_role()).to_lowercase())).unwrap_or("everyone".into());
        }
        "roles" | "actor.roles" => {
            return Value::List(ev.actor.as_ref().map(|a| a.roles.iter().map(|r| Value::Str(format!("{r:?}").to_lowercase())).collect()).unwrap_or_default());
        }
        _ => {}
    }
    let p = rest.strip_prefix("payload.").unwrap_or(rest);
    ev.payload.get_path(p).cloned().unwrap_or_default()
}

/// Template lookup: bare names resolve to event fields first (`{user}`, `{bits}`), then
/// state and signals (`{song.title}`, `{show.mode}`).
struct TemplateScope<'a>(&'a EvalScope<'a>);

impl Scope for TemplateScope<'_> {
    fn lookup(&self, path: &str) -> Value {
        if self.0.event.is_some() && !path.starts_with("event") {
            let v = self.0.lookup(&format!("event.{path}"));
            if !v.is_null() {
                return v;
            }
        }
        self.0.lookup(path)
    }
}

/// Substitute `{path}` placeholders inside one token. A token that is exactly `{path}` becomes
/// the value's text; chat text can never introduce new tokens because this runs per token.
pub fn render_token(tok: &str, scope: &dyn Scope) -> String {
    if !tok.contains('{') {
        return tok.to_string();
    }
    let mut out = String::with_capacity(tok.len());
    let mut rest = tok;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) if is_placeholder(&after[..end]) => {
                let v = scope.lookup(&after[..end]);
                let s = v.to_string();
                out.extend(s.chars().filter(|c| !c.is_control()).take(500));
                rest = &after[end + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn is_placeholder(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

impl Core {
    pub fn new(config: Config, t0: Ts) -> Core {
        let hz = config.project.tick_hz.clamp(30, 1000) as u64;
        let seed = config.project.seed;
        let mut c = Core {
            config: Config::default(),
            state: StateTree::default(),
            signals: Signals::default(),
            trace: Trace::default(),
            rng: Rng::new(seed),
            t0,
            tick: 0,
            period: 1_000_000_000 / hz,
            inputs: Vec::new(),
            applied: Vec::new(),
            events: Default::default(),
            outbox: Vec::new(),
            cause: HashMap::new(),
            rules: Vec::new(),
            bindings: Vec::new(),
            lfos: Vec::new(),
            presets: Vec::new(),
            preset_queue: Vec::new(),
            triggers: BTreeMap::new(),
            trigger_specs: HashMap::new(),
            scheduled: BinaryHeap::new(),
            sched_seq: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            transition: None,
            transition_history: Vec::new(),
            transition_votes: Vec::new(),
            scene_layer: Vec::new(),
            caps: Vec::new(),
            chat_caps: Vec::new(),
            chat_ttl: 30_000 * MS,
            caps_gen: u64::MAX,
            scene_prefixes: Vec::new(),
            runtime_dirty: false,
            beat_sig: 0,
            time_sig: 0,
            history_every: (hz / 60).max(1),
            policy: crate::policy::Policy::default(),
            timelines: timeline::Timelines::default(),
        };
        c.declare_builtins();
        c.apply_config(config);
        c.state.resolve(c.now(), |_, _| {});
        c
    }

    // ---- accessors -------------------------------------------------------------------

    pub fn now(&self) -> Ts {
        self.t0 + self.tick * self.period
    }
    pub fn tick_index(&self) -> u64 {
        self.tick
    }
    pub fn period(&self) -> Ts {
        self.period
    }
    pub fn t0(&self) -> Ts {
        self.t0
    }
    pub fn state(&self) -> &StateTree {
        &self.state
    }
    pub fn signals(&self) -> &Signals {
        &self.signals
    }
    pub fn trace(&self) -> &Trace {
        &self.trace
    }
    pub fn mode_str(&self) -> &str {
        self.state.get(addr::MODE).and_then(Value::as_str).unwrap_or("offline")
    }
    pub fn get(&self, a: &str) -> Option<&Value> {
        self.state.get(a)
    }
    pub fn explain(&self, a: &str) -> Option<Provenance> {
        self.state.explain(a, self.now())
    }
    pub fn trace_chain(&self, id: Id) -> Vec<TraceRec> {
        self.trace.chain(id)
    }
    pub fn active_presets(&self) -> Vec<(String, Option<u64>)> {
        let now = self.now();
        self.presets.iter().map(|p| (p.name.clone(), p.release_at.map(|r| r.saturating_sub(now) / MS))).collect()
    }

    /// Jump the tick counter (replay of a session segment that started mid-run).
    pub fn seek_tick(&mut self, tick: u64) {
        self.tick = tick;
    }

    /// Queue an input for the next tick.
    pub fn submit(&mut self, input: Input) {
        self.inputs.push(input);
    }

    /// Replayable inputs applied since the last drain, with their tick index.
    pub fn drain_applied(&mut self) -> Vec<(u64, Input)> {
        std::mem::take(&mut self.applied)
    }

    pub fn drain_outputs(&mut self) -> Vec<Output> {
        std::mem::take(&mut self.outbox)
    }

    /// Run ticks until core time reaches `real_now`. Returns ticks run.
    pub fn advance_to(&mut self, real_now: Ts) -> u64 {
        let mut n = 0;
        while self.now() + self.period <= real_now {
            self.step();
            n += 1;
            if n > 2000 {
                // Far behind (suspend/resume): jump instead of spinning.
                let behind = (real_now - self.now()) / self.period;
                self.tick += behind;
                break;
            }
        }
        n
    }

    /// One fixed tick.
    pub fn step(&mut self) {
        self.tick += 1;
        let now = self.now();
        let inputs = std::mem::take(&mut self.inputs);
        for i in inputs {
            self.apply_input(i);
        }
        self.run_scheduled(now);
        self.tick_policy(now);
        self.process_events();
        self.tick_presets(now);
        self.tick_triggers(now);
        self.tick_transition(now);
        self.tick_signals(now);
        self.tick_timelines(now);
        self.tick_bindings();
        self.process_events();
        self.refresh_caps();
        self.resolve();
        if self.tick.is_multiple_of(self.history_every) {
            self.signals.sample_history();
        }
        let fresh = self.trace.drain_fresh();
        if !fresh.is_empty() {
            self.outbox.push(Output::Trace(fresh));
        }
        if self.runtime_dirty {
            self.runtime_dirty = false;
            self.outbox.push(Output::RuntimeDirty);
        }
    }

    fn log(&mut self, level: &'static str, msg: String) {
        match level {
            "error" => tracing::error!(target: "core", "{msg}"),
            "warn" => tracing::warn!(target: "core", "{msg}"),
            _ => tracing::info!(target: "core", "{msg}"),
        }
        self.outbox.push(Output::Log { level, msg });
    }

    // ---- builtins & config -----------------------------------------------------------

    fn declare_builtins(&mut self) {
        let modes: Vec<String> = crate::config::DEFAULT_MODES.iter().map(|s| s.to_string()).collect();
        let m = Meta { options: modes, ..Meta::enumeration("offline", &[]) };
        self.state.declare(addr::MODE, m.owner("core").describe("Global show mode"));
        self.state.declare(addr::PROGRAM, Meta::string("").owner("core").describe("Scene on program"));
        self.state.declare(addr::PREVIEW, Meta::string("").owner("core").describe("Scene on preview"));
        self.state.declare(addr::DIRECT, Meta::boolean(false).owner("core").describe("Scene changes go straight to program"));
        self.state.declare(addr::TR_NAME, Meta::string("").readonly().owner("core"));
        self.state.declare(addr::TR_MS, Meta::int(0, [0.0, 60_000.0]).readonly().owner("core"));
        self.state.declare(addr::TR_ACTIVE, Meta::boolean(false).readonly().owner("core"));
        self.state.declare(addr::TR_PROGRESS, Meta::float(1.0, [0.0, 1.0]).readonly().owner("core"));
        self.state.declare(addr::TR_FROM, Meta::string("").readonly().owner("core"));
        self.state.declare(addr::TR_START, Meta::int(0, [0.0, 9.2e18]).readonly().owner("core"));
        self.state.declare(addr::PANIC, Meta::boolean(false).readonly().owner("core"));
        self.beat_sig = self.signals.ensure("beat.phase");
        self.signals.ensure("beat.bpm");
        self.time_sig = self.signals.ensure("time.seconds");
        let pending = Meta { ty: se_proto::ValueType::List, default: Value::List(Vec::new()), ..Default::default() };
        self.state.declare(
            crate::policy::PENDING,
            pending.readonly().owner("policy").describe("Held chat/bits/points activity waiting for a mod (veto window, approvals)"),
        );
        self.state.declare(crate::policy::PENDING_COUNT, Meta::int(0, [0.0, 1e6]).readonly().owner("policy"));
    }

    /// Apply a new configuration (initial load and hot reload). Runtime state is kept.
    pub fn apply_config(&mut self, config: Config) {
        let now = self.now();
        // modes
        let mut mm = Meta::enumeration(&config.project.start_mode, &[]).owner("core").describe("Global show mode");
        mm.options = config.modes();
        let cur_mode = if self.tick == 0 { config.project.start_mode.clone() } else { self.mode_str().to_string() };
        self.state.declare(addr::MODE, mm);
        self.set_sys(addr::MODE, Value::Str(cur_mode));

        // Clear cached scene ids before removing addresses compacts the state tree.
        for i in std::mem::take(&mut self.scene_layer) {
            self.state.set_scene(i, None);
        }
        // Deleting a definition also retires its runtime work; surviving definitions keep theirs.
        self.preset_queue.retain(|(name, _)| config.presets.contains_key(name));
        let removed: Vec<String> = self.config.presets.keys().filter(|name| !config.presets.contains_key(*name)).cloned().collect();
        for name in &removed {
            self.release_preset(name, None, false);
        }
        self.state.remove_overrides_where(|o| !configured_owner(&config, &o.key));
        self.scheduled.retain(|s| s.0.ctx.key.as_deref().is_none_or(|key| configured_owner(&config, key)));
        for trigger in self.triggers.values_mut() {
            trigger.instances.retain(|i| configured_owner(&config, &i.key));
            trigger.queued.retain(|i| configured_owner(&config, &i.key));
        }
        for name in &removed {
            self.state.remove_prefix(&format!("preset.{name}"));
        }

        // scenes: declare node params, drop removed scenes
        let new_prefixes: Vec<String> = config.scenes.keys().map(|s| format!("scene.{s}")).collect();
        for old in std::mem::take(&mut self.scene_prefixes) {
            if !new_prefixes.contains(&old) {
                self.state.remove_prefix(&old);
            }
        }
        self.scene_prefixes = new_prefixes;
        for s in config.scenes.values() {
            declare_scene(&mut self.state, s);
        }
        let program = self.state.get(addr::PROGRAM).and_then(Value::as_str).unwrap_or("").to_string();
        if !config.scenes.contains_key(&program) {
            self.set_sys(addr::PROGRAM, Value::Str(first_scene(&config)));
        }
        if !config.scenes.contains_key(self.state.get(addr::PREVIEW).and_then(Value::as_str).unwrap_or("")) {
            let program = self.state.get(addr::PROGRAM).cloned().unwrap_or_else(|| Value::Str(String::new()));
            self.set_sys(addr::PREVIEW, program);
        }
        if self.transition.as_ref().is_some_and(|t| !config.scenes.contains_key(&t.from) || !config.scenes.contains_key(&t.to)) {
            self.transition = None;
            self.set_sys(addr::TR_ACTIVE, Value::Bool(false));
            self.set_sys(addr::TR_PROGRESS, Value::Float(1.0));
            self.set_sys(addr::TR_FROM, Value::Str(String::new()));
        }
        self.rebind_triggers();
        for trigger in self.triggers.values_mut() {
            let (level, active) = trigger.tick(now);
            self.state.set_base(trigger.env_id, Value::Float(level));
            self.state.set_base(trigger.active_id, Value::Bool(active));
        }

        // presets: `preset.<name>.active`
        for name in config.presets.keys() {
            self.state.declare(&format!("preset.{name}.active"), Meta::boolean(false).readonly().owner("presets"));
        }

        // project base params
        for (a, v) in &config.project.params {
            let i = self.state.ensure(a, v);
            if &self.state.param(i).base != v || !self.state.param(i).base_explicit {
                self.state.set_base(i, v.clone());
            }
        }

        // rules (keep cooldowns and enable flags by name)
        let old: HashMap<String, (Option<Ts>, HashMap<String, Ts>, bool)> =
            self.rules.drain(..).map(|r| (r.def.name.clone(), (r.last, r.per_actor, r.enabled))).collect();
        for def in &config.rules {
            let cond = match def.condition.as_deref().map(Expr::parse) {
                Some(Ok(e)) => Some(e),
                Some(Err(e)) => {
                    self.log("error", format!("rule `{}`: {e}", def.name));
                    continue;
                }
                None => None,
            };
            let (last, per_actor, enabled) = old.get(&def.name).cloned().unwrap_or((None, HashMap::new(), def.enabled));
            self.rules.push(RuleRt { def: def.clone(), cond, last, per_actor, enabled });
        }

        // bindings
        let was_enabled: HashMap<String, bool> = self.bindings.iter().map(|b| (b.def.name.clone(), b.def.enabled)).collect();
        self.bindings.clear();
        self.state.clear_all_mods();
        for def in &config.bindings {
            match BindingRt::new(def.clone()) {
                Ok(mut b) => {
                    if let Some(e) = was_enabled.get(&def.name) {
                        b.def.enabled = *e;
                    }
                    self.bindings.push(b)
                }
                Err(e) => self.log("error", e),
            }
        }

        // lfos: builtins + project
        self.lfos.clear();
        let mut defs: BTreeMap<String, crate::config::LfoDef> = builtin_lfos().into_iter().map(|(n, d)| (n.to_string(), d)).collect();
        defs.extend(config.project.lfo.clone());
        for (n, d) in &defs {
            match Lfo::from_def(n, d, &mut self.signals) {
                Ok(l) => self.lfos.push(l),
                Err(e) => self.log("error", e),
            }
        }

        // safety caps
        self.caps.clear();
        self.chat_caps.clear();
        if let Some(toml::Value::Table(s)) = config.project.extra.get("safety") {
            let read = |k: &str| -> Vec<(String, [f64; 2])> {
                s.get(k)
                    .and_then(|v| v.as_table())
                    .map(|t| {
                        t.iter()
                            .filter_map(|(p, v)| {
                                let a = v.as_array()?;
                                Some((p.clone(), [num(a.first()?)?, num(a.get(1)?)?]))
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            self.caps = read("caps");
            self.chat_caps = read("chat_caps");
            if let Some(ttl) = s.get("chat_ttl").and_then(|v| v.as_str()).and_then(se_proto::parse_duration_ms) {
                self.chat_ttl = ttl * MS;
            }
        }
        self.caps_gen = u64::MAX;
        for e in self.policy.configure(&config) {
            self.log("error", e);
        }

        let timeline_errors = self.configure_timelines(&config);
        for e in &config.errors {
            self.outbox.push(Output::Log { level: "error", msg: format!("{}: {}", e.file, e.msg) });
        }
        self.config = config;
        self.config.errors.extend(timeline_errors);
        self.apply_scene_layer();
        self.runtime_dirty = true;
    }

    fn refresh_caps(&mut self) {
        if self.caps_gen == self.state.generation {
            return;
        }
        self.caps_gen = self.state.generation;
        let n = self.state.len();
        for i in 0..n {
            let a = self.state.param(i).addr.clone();
            let cap = self.caps.iter().find(|(p, _)| address::matches(p, &a)).map(|(_, c)| *c);
            if self.state.param(i).cap != cap {
                self.state.set_cap(i, cap);
            }
        }
    }

    // ---- inputs ----------------------------------------------------------------------

    fn apply_input(&mut self, input: Input) {
        if input.is_replayable() {
            self.applied.push((self.tick, input.clone()));
        }
        match input {
            Input::Command { mut cmd } => {
                if cmd.ts == 0 {
                    cmd.ts = self.now();
                }
                let id = cmd.id;
                self.trace.add(id, cmd.causal, self.now(), "command", format!("{} ({})", cmd.op.describe(), cmd.origin.as_str()));
                let key = cmd.key.clone().filter(|_| cmd.priority() > PRIORITY_CHAT);
                let ctx = Ctx { parent: Some(id), actor: cmd.actor.clone(), priority: cmd.priority, key, ..Default::default() };
                let res = self.exec(&cmd.op, cmd.origin, &ctx);
                if res.is_ok() {
                    self.timelines_capture(&cmd);
                }
                if let Err(e) = &res {
                    self.trace.add(next_id(), Some(id), self.now(), "error", e.clone());
                }
                self.outbox.push(Output::Ack { id, ok: res.is_ok(), error: res.err() });
            }
            Input::Event { mut event } => {
                if event.ts == 0 {
                    event.ts = self.now();
                }
                self.events.push_back((event, Ctx::default()));
            }
            Input::Signal { name, value } => self.signals.set(&name, value, self.now()),
            Input::Signals { values } => {
                let now = self.now();
                for (n, v) in values {
                    self.signals.set(&n, v, now);
                }
            }
            Input::Declare { address, meta } => {
                self.state.declare(&address, meta);
            }
            Input::DeclareTrigger { address, spec } => {
                self.trigger_specs.insert(address.clone(), spec);
                self.ensure_trigger(&address);
            }
            Input::Publish { address, value } => {
                let i = self.state.ensure(&address, &value);
                if self.state.param(i).base != value || !self.state.param(i).base_explicit {
                    self.state.set_base(i, value);
                }
            }
            Input::Remove { prefix } => {
                self.state.remove_prefix(&prefix);
                let dotted = format!("{prefix}.");
                self.triggers.retain(|k, _| !(k == &prefix || k.starts_with(&dotted)));
                self.trigger_specs.retain(|k, _| !(k == &prefix || k.starts_with(&dotted)));
                self.rebind_triggers();
            }
            Input::Timecode { source, obs } => self.timelines.observe(&source, &obs),
            Input::Config { config } => self.apply_config(*config),
        }
    }

    /// Queue an event produced by the engine itself (not from an input).
    pub fn emit(&mut self, event: Event) {
        self.submit(Input::Event { event });
    }

    // ---- events & rules --------------------------------------------------------------

    fn process_events(&mut self) {
        let mut n = 0;
        let mut fx = Vec::new();
        while let Some((mut ev, ctx)) = self.events.pop_front() {
            n += 1;
            if n > MAX_EVENTS_PER_TICK {
                self.log("error", "event storm: dropping events this tick".into());
                self.events.clear();
                break;
            }
            if ev.ts == 0 {
                ev.ts = self.now();
            }
            // policy (§12.1) screens every event before it is published or rules see it
            let mode = self.mode_str().to_string();
            self.policy.screen(ev, self.now(), &mode, &mut fx);
            for e in fx.drain(..) {
                self.apply_policy(e, &ctx);
            }
        }
    }

    /// Publish an event and run its rules.
    fn deliver(&mut self, ev: Event, ctx: &Ctx) {
        let label = match &ev.actor {
            Some(a) => format!("{} by {}", ev.ty, a.name),
            None => ev.ty.clone(),
        };
        self.trace.add(ev.id, ev.causal.or(ctx.parent), self.now(), "event", label);
        self.outbox.push(Output::Event(ev.clone()));
        if ctx.depth > MAX_CHAIN_DEPTH {
            self.trace.add(next_id(), Some(ev.id), self.now(), "error", "rule chain too deep (loop?)".into());
            return;
        }
        self.run_rules(&ev, ctx.depth);
    }

    fn apply_policy(&mut self, e: crate::policy::Effect, ctx: &Ctx) {
        use crate::policy::Effect;
        match e {
            Effect::Deliver(ev) => self.deliver(ev, ctx),
            Effect::Emit(ev) => self.events.push_back((ev, Ctx { depth: ctx.depth, parent: ctx.parent, ..Default::default() })),
            Effect::Run { commands, actor, event } => {
                let id = next_id();
                let who = actor.as_ref().map(|a| a.name.clone()).unwrap_or_default();
                self.trace.add(id, event.as_ref().map(|e| e.id).or(ctx.parent), self.now(), "policy", format!("run for {who}"));
                let c = Ctx { key: None, priority: Some(PRIORITY_CHAT), parent: Some(id), actor, depth: ctx.depth + 1, event };
                self.run_list(&commands, Origin::Rule, c);
            }
            Effect::Redeem(r) => {
                let id = next_id();
                let who = r.actor().map(|a| a.name.clone()).unwrap_or_default();
                self.trace.add(id, Some(r.event.id), self.now(), "policy", format!("redeem for {who}"));
                let c = Ctx {
                    key: None,
                    priority: Some(PRIORITY_CHAT),
                    parent: Some(id),
                    actor: r.actor().cloned(),
                    depth: ctx.depth + 1,
                    event: Some(r.event.clone()),
                };
                let outcome = self.run_commands(&r.commands, Origin::Rule, c, true);
                let mut fx = Vec::new();
                self.policy.settle(*r, outcome, &mut fx);
                for e in fx {
                    self.apply_policy(e, ctx);
                }
            }
            Effect::Action { name, args, actor } => {
                let mut c = Command::new(Origin::System, Op::Action { name, args });
                c.actor = actor;
                c.causal = ctx.parent;
                c.ts = self.now();
                self.outbox.push(Output::Action(c));
            }
            Effect::SetMode(m) => {
                let c = Ctx { parent: ctx.parent, depth: ctx.depth, ..Default::default() };
                if let Err(e) = self.set_mode(&m, Origin::System, &c) {
                    self.log("error", format!("policy: {e}"));
                }
            }
            Effect::Pending => {
                self.set_sys(crate::policy::PENDING, self.policy.pending_value());
                self.set_sys(crate::policy::PENDING_COUNT, Value::Int(self.policy.pending_len() as i64));
            }
        }
    }

    fn tick_policy(&mut self, now: Ts) {
        if !self.policy.due(now) {
            return;
        }
        let mode = self.mode_str().to_string();
        let mut fx = Vec::new();
        self.policy.tick(now, &mode, &mut fx);
        for e in fx {
            self.apply_policy(e, &Ctx::default());
        }
    }

    fn event_priority(ev: &Event) -> Option<u16> {
        match &ev.actor {
            Some(a) if a.platform != "local" && a.top_role() < Role::Owner => Some(PRIORITY_CHAT),
            // platform events (stream online/offline, ad breaks) may change modes and scenes
            None if crate::policy::is_platform_event(&ev.ty) => None,
            _ if matches!(ev.origin, Origin::Chat | Origin::Twitch | Origin::Relay) => Some(PRIORITY_CHAT),
            _ => None,
        }
    }

    fn run_rules(&mut self, ev: &Event, depth: u32) {
        let now = self.now();
        let mode = self.mode_str().to_string();
        let scene = self.state.get(addr::PROGRAM).and_then(Value::as_str).unwrap_or("").to_string();
        let mut fired: Vec<(usize, Id)> = Vec::new();
        let viewer = Self::event_priority(ev) == Some(PRIORITY_CHAT);
        for (ri, r) in self.rules.iter().enumerate() {
            if !r.enabled || !address::matches(&r.def.when, &ev.ty) {
                continue;
            }
            if !r.def.modes.is_empty() && !r.def.modes.contains(&mode) {
                continue;
            }
            if !r.def.scenes.is_empty() && !r.def.scenes.contains(&scene) {
                continue;
            }
            if let (Some(cd), Some(last)) = (r.def.cooldown.global, r.last)
                && now < last + cd.ns()
            {
                continue;
            }
            if let (Some(cd), Some(a)) = (r.def.cooldown.per_actor, &ev.actor)
                && r.per_actor.get(&a.id).is_some_and(|t| now < *t + cd.ns())
            {
                continue;
            }
            if viewer
                && let Some(min) = r.def.role
                && !crate::policy::role_allows(ev.actor.as_ref(), min)
            {
                continue;
            }
            if let Some(c) = &r.cond {
                let scope = EvalScope { core: self, event: Some(ev) };
                if !c.eval_bool(&scope) {
                    continue;
                }
            }
            let id = next_id();
            fired.push((ri, id));
            if r.def.is_final {
                break;
            }
        }
        for (ri, id) in fired {
            let (name, cmds, prio, approval) = {
                let r = &mut self.rules[ri];
                r.last = Some(now);
                if let Some(a) = &ev.actor {
                    r.per_actor.insert(a.id.clone(), now);
                }
                (r.def.name.clone(), r.def.commands.clone(), r.def.priority, r.def.approval)
            };
            self.trace.add(id, Some(ev.id), now, "rule", name.clone());
            if approval && viewer && !crate::policy::role_allows(ev.actor.as_ref(), Role::Mod) {
                // viewer-triggered firing waits for a mod (§12.1 approval queue)
                let pid = crate::policy::hold_id(ev.id ^ (ri as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
                let mut fx = Vec::new();
                if let Err(e) = self.policy.hold_commands(pid, &format!("rule {name}"), cmds, ev.actor.clone(), Some(ev.clone()), None, now, &mut fx) {
                    self.trace.add(next_id(), Some(id), now, "error", e);
                }
                let c = Ctx { parent: Some(id), depth: depth + 1, ..Default::default() };
                for e in fx {
                    self.apply_policy(e, &c);
                }
                continue;
            }
            let ep = Self::event_priority(ev);
            let priority = match (prio, ep) {
                (Some(p), Some(c)) => Some(p.min(c)),
                (p, c) => p.or(c),
            };
            let ctx = Ctx { key: Some(format!("rule:{name}")), priority, parent: Some(id), actor: ev.actor.clone(), depth: depth + 1, event: Some(ev.clone()) };
            self.run_list(&cmds, Origin::Rule, ctx);
        }
    }

    /// Run a command list with `wait` delays and per-token templating.
    fn run_list(&mut self, cmds: &[String], origin: Origin, ctx: Ctx) {
        let _ = self.run_commands(cmds, origin, ctx, false);
    }

    /// [`Self::run_list`]; with `strict`, the first command before any `wait` that fails
    /// stops the list and its error is returned (commands after a `wait` are only scheduled,
    /// so they can't fail it).
    fn run_commands(&mut self, cmds: &[String], origin: Origin, ctx: Ctx, strict: bool) -> Result<(), String> {
        let mut delay: Ts = 0;
        for text in cmds {
            let op = match self.template_op(text, ctx.event.as_ref()) {
                Ok(op) => op,
                Err(e) => {
                    let msg = format!("`{text}`: {e}");
                    self.trace.add(next_id(), ctx.parent, self.now(), "error", msg.clone());
                    if strict && delay == 0 {
                        return Err(msg);
                    }
                    continue;
                }
            };
            if let Op::Wait { ms } = op {
                delay += ms as u64 * MS;
                continue;
            }
            if delay == 0 {
                let r = self.try_exec_traced(&op, origin, &ctx);
                if strict {
                    r?;
                }
            } else {
                self.schedule(self.now() + delay, op, origin, ctx.clone());
            }
        }
        Ok(())
    }

    fn template_op(&self, text: &str, ev: Option<&Event>) -> Result<Op, String> {
        let toks = se_proto::command::tokenize(text).map_err(|e| e.to_string())?;
        if !text.contains('{') {
            return Op::from_tokens(&toks).map_err(|e| e.to_string());
        }
        let scope = EvalScope { core: self, event: ev };
        let ts = TemplateScope(&scope);
        let toks: Vec<String> = toks.iter().map(|t| render_token(t, &ts)).collect();
        Op::from_tokens(&toks).map_err(|e| e.to_string())
    }

    fn schedule(&mut self, due: Ts, op: Op, origin: Origin, ctx: Ctx) {
        self.sched_seq += 1;
        self.scheduled.push(Reverse(Scheduled { due, seq: self.sched_seq, op, origin, ctx }));
    }

    fn run_scheduled(&mut self, now: Ts) {
        while self.scheduled.peek().is_some_and(|Reverse(s)| s.due <= now) {
            let Reverse(s) = self.scheduled.pop().unwrap();
            self.exec_traced(&s.op, s.origin, &s.ctx);
        }
    }

    fn exec_traced(&mut self, op: &Op, origin: Origin, ctx: &Ctx) {
        let _ = self.try_exec_traced(op, origin, ctx);
    }

    /// Execute with a trace entry; failures are traced and returned.
    fn try_exec_traced(&mut self, op: &Op, origin: Origin, ctx: &Ctx) -> Result<(), String> {
        let id = next_id();
        self.trace.add(id, ctx.parent, self.now(), "command", op.describe());
        let mut c = ctx.clone();
        c.parent = Some(id);
        let r = self.exec(op, origin, &c);
        if let Err(e) = &r {
            self.trace.add(next_id(), Some(id), self.now(), "error", e.clone());
        }
        r
    }

    // ---- command execution -----------------------------------------------------------

    fn priority_for(&self, origin: Origin, ctx: &Ctx) -> u16 {
        ctx.priority.unwrap_or_else(|| origin.default_priority())
    }

    fn override_key(&self, origin: Origin, ctx: &Ctx, prio: u16) -> String {
        if let Some(k) = &ctx.key {
            return k.clone();
        }
        if prio <= PRIORITY_CHAT {
            if let Some(a) = &ctx.actor {
                return format!("chat:{}", a.id);
            }
            return "chat".into();
        }
        if prio >= PRIORITY_MANUAL {
            return "manual".into();
        }
        origin.as_str().to_string()
    }

    fn note_cause(&mut self, i: usize, ctx: &Ctx) {
        if let Some(p) = ctx.parent {
            self.cause.insert(i, p);
        }
    }

    fn exec(&mut self, op: &Op, origin: Origin, ctx: &Ctx) -> Result<(), String> {
        let now = self.now();
        // chat effects only run in the policy's effect modes (§12.1); the simulator is exempt
        if prio_is_chat(self.priority_for(origin, ctx))
            && !ctx.event.as_ref().is_some_and(|e| e.origin == Origin::Sim)
            && self.policy.effect_paused(op, self.mode_str())
        {
            return Err(format!("chat effects are paused in mode `{}`", self.mode_str()));
        }
        // `set scene.*.node.cam_face.offset_y 5`: one command per matching address (§2.1)
        if let Some(pattern) = wildcard_target(op) {
            if !address::is_valid(pattern, true) {
                return Err(format!("bad address `{pattern}`"));
            }
            let mut addrs: Vec<String> = self.state.matching(pattern).map(|i| self.state.param(i).addr.clone()).collect();
            if matches!(op, Op::Release { .. }) {
                addrs.extend(self.triggers.keys().filter(|k| address::matches(pattern, k)).cloned());
                addrs.sort();
                addrs.dedup();
            }
            if addrs.is_empty() {
                return Err(format!("`{pattern}` matches nothing"));
            }
            let (mut done, mut first_err) = (0usize, None);
            for a in &addrs {
                match self.exec(&with_address(op, a), origin, ctx) {
                    Ok(()) => done += 1,
                    Err(e) => {
                        first_err.get_or_insert(e);
                    }
                }
            }
            return if done > 0 { Ok(()) } else { Err(first_err.unwrap_or_default()) };
        }
        match op {
            Op::Set { address, value } => {
                self.check_writable(address, origin)?;
                let prio = self.priority_for(origin, ctx);
                let i = self.state.ensure(address, &zero_of(value));
                let mut v = value.clone();
                let mut expires = None;
                if prio <= PRIORITY_CHAT {
                    if address.starts_with("mixer.") {
                        return Err("chat can never touch the mixer".into());
                    }
                    v = self.chat_clamp(address, v);
                    expires = Some(now + self.chat_ttl);
                }
                let key = self.override_key(origin, ctx, prio);
                self.state.put_override(i, Override { key, priority: prio, value: v, seq: 0, expires, anim: None, origin, causal: ctx.parent });
                self.note_cause(i, ctx);
                if prio > PRIORITY_CHAT {
                    self.runtime_dirty = true;
                }
                Ok(())
            }
            Op::SetBase { address, value } => {
                self.check_writable(address, origin)?;
                if matches!(origin, Origin::Chat | Origin::Twitch) {
                    return Err("chat cannot edit the project".into());
                }
                let i = self.state.ensure(address, value);
                let coerced = self.state.param(i).meta.coerce(value);
                let old = self.state.set_base(i, coerced.clone());
                if old != coerced {
                    self.undo.push(UndoEntry { address: address.clone(), old, new: coerced.clone() });
                    if self.undo.len() > 500 {
                        self.undo.remove(0);
                    }
                    self.redo.clear();
                    self.outbox.push(Output::PersistBase { address: address.clone(), value: coerced });
                }
                self.note_cause(i, ctx);
                Ok(())
            }
            Op::Animate { address, to, ms, ease } => {
                self.check_writable(address, origin)?;
                let prio = self.priority_for(origin, ctx);
                if prio <= PRIORITY_CHAT && address.starts_with("mixer.") {
                    return Err("chat can never touch the mixer".into());
                }
                let i = self.state.ensure(address, &zero_of(to));
                let from = self.state.value(i).clone();
                let to = if prio <= PRIORITY_CHAT { self.chat_clamp(address, to.clone()) } else { to.clone() };
                let key = self.override_key(origin, ctx, prio);
                let expires = (prio <= PRIORITY_CHAT).then(|| now + *ms as u64 * MS + self.chat_ttl);
                self.state.put_override(
                    i,
                    Override {
                        key,
                        priority: prio,
                        value: to.clone(),
                        seq: 0,
                        expires,
                        anim: Some(Anim { from, to, start: now, dur: *ms as u64 * MS, ease: *ease }),
                        origin,
                        causal: ctx.parent,
                    },
                );
                self.note_cause(i, ctx);
                Ok(())
            }
            Op::Trigger { address, payload } => self.fire_trigger(address, payload, origin, ctx),
            Op::Release { address } => {
                let prio = self.priority_for(origin, ctx);
                let key = self.override_key(origin, ctx, prio);
                let mut any = false;
                if let Some(t) = self.triggers.get_mut(address) {
                    any |= t.release(if prio >= PRIORITY_MANUAL { None } else { Some(&key) }, now);
                }
                if let Some(i) = self.state.id(address) {
                    any |= self.state.remove_override(i, &key);
                    if prio >= PRIORITY_MANUAL && !any {
                        let keys: Vec<String> = self.state.param(i).overrides.iter().map(|o| o.key.clone()).collect();
                        for k in keys {
                            any |= self.state.remove_override(i, &k);
                        }
                    }
                }
                self.runtime_dirty = true;
                if any { Ok(()) } else { Err(format!("nothing to release on `{address}`")) }
            }
            Op::SceneGo { scene } => {
                self.require_scene(scene)?;
                if prio_is_chat(self.priority_for(origin, ctx)) {
                    return Err("chat cannot change scenes".into());
                }
                let direct = self.state.get(addr::DIRECT).is_some_and(Value::truthy);
                self.set_sys(addr::PREVIEW, Value::Str(scene.clone()));
                self.runtime_dirty = true;
                if direct { self.take(None, None, ctx) } else { Ok(()) }
            }
            Op::SceneCut { scene, transition } => {
                self.require_scene(scene)?;
                if prio_is_chat(self.priority_for(origin, ctx)) {
                    return Err("chat cannot change scenes".into());
                }
                self.set_sys(addr::PREVIEW, Value::Str(scene.clone()));
                self.take(transition.clone(), None, ctx)
            }
            Op::SceneTake { transition, ms } => {
                if prio_is_chat(self.priority_for(origin, ctx)) {
                    return Err("chat cannot change scenes".into());
                }
                self.take(transition.clone(), *ms, ctx)
            }
            Op::PresetFire { name, payload } => self.fire_preset(name, payload, origin, ctx),
            Op::PresetRelease { name } => {
                if self.release_preset(name, ctx.parent, true) {
                    Ok(())
                } else {
                    Err(format!("preset `{name}` is not active"))
                }
            }
            Op::ModeSet { mode } => self.set_mode(mode, origin, ctx),
            Op::Emit { ty, payload } => {
                if !address::is_valid(ty, false) {
                    return Err(format!("bad event type `{ty}`"));
                }
                let mut e = Event::new(ty.clone(), if origin == Origin::Rule { Origin::Rule } else { origin }, payload.clone());
                e.actor = ctx.actor.clone();
                e.causal = ctx.parent;
                e.ts = now;
                self.events.push_back((e, Ctx { depth: ctx.depth, parent: ctx.parent, ..Default::default() }));
                Ok(())
            }
            Op::Panic => {
                self.panic(ctx);
                Ok(())
            }
            Op::Clean => {
                self.clean(ctx);
                Ok(())
            }
            Op::Undo => self.undo_redo(true),
            Op::Redo => self.undo_redo(false),
            Op::Wait { .. } => Ok(()),
            Op::Action { name, args } => self.action(name, args, origin, ctx),
        }
    }

    fn check_writable(&self, address: &str, origin: Origin) -> Result<(), String> {
        if !address::is_valid(address, false) {
            return Err(format!("bad address `{address}`"));
        }
        if let Some(i) = self.state.id(address)
            && self.state.param(i).meta.readonly
            && origin != Origin::System
        {
            return Err(format!("`{address}` is read-only"));
        }
        Ok(())
    }

    fn chat_clamp(&self, address: &str, v: Value) -> Value {
        match self.chat_caps.iter().find(|(p, _)| address::matches(p, address)) {
            Some((_, [lo, hi])) => match (&v, v.as_f64()) {
                // booleans are capped as 0/1 (`[0, 0]` = chat may only turn it off)
                (Value::Bool(_), Some(x)) => Value::Bool(x.clamp(*lo, *hi) >= 0.5),
                (Value::Int(_), Some(x)) => Value::Int(x.clamp(*lo, *hi).round() as i64),
                (_, Some(x)) => Value::Float(x.clamp(*lo, *hi)),
                _ => v,
            },
            None => v,
        }
    }

    fn set_sys(&mut self, a: &str, v: Value) {
        let i = self.state.ensure(a, &v);
        if self.state.param(i).base != v {
            self.state.set_base(i, v);
            let now = self.now();
            self.state.refresh(i, now);
        }
    }

    fn require_scene(&self, s: &str) -> Result<(), String> {
        if self.config.scenes.contains_key(s) { Ok(()) } else { Err(format!("unknown scene `{s}`")) }
    }

    // ---- triggers --------------------------------------------------------------------

    /// `X.payload.*` of patch triggers (what shader/dsp patches read of the last payload).
    fn declare_payload(&mut self, address: &str) -> Vec<usize> {
        if !address.starts_with("patch.") {
            return Vec::new();
        }
        let mut ids: Vec<usize> = PAYLOAD_FIELDS
            .iter()
            .map(|f| {
                self.state
                    .declare(&format!("{address}.payload.{f}"), Meta::float(0.0, [0.0, 1e12]).readonly().owner("triggers").describe("Last trigger payload"))
            })
            .collect();
        ids.push(
            self.state
                .declare(&format!("{address}.payload.user_color"), Meta::color([0.0; 4]).readonly().owner("triggers").describe("Last trigger's user colour")),
        );
        ids
    }

    fn ensure_trigger(&mut self, address: &str) -> &mut TriggerRt {
        if !self.triggers.contains_key(address) {
            let spec = self.trigger_specs.get(address).copied().unwrap_or_default();
            let env_id = self.state.declare(&format!("{address}.env"), Meta::float(0.0, [0.0, 1.0]).readonly().owner("triggers"));
            let active_id = self.state.declare(&format!("{address}.active"), Meta::boolean(false).readonly().owner("triggers"));
            let payload_ids = self.declare_payload(address);
            self.triggers.insert(address.to_string(), TriggerRt { spec, env_id, active_id, payload_ids, ..Default::default() });
        }
        let spec = self.trigger_specs.get(address).copied();
        let t = self.triggers.get_mut(address).unwrap();
        if let Some(s) = spec {
            t.spec = s;
        }
        t
    }

    fn rebind_triggers(&mut self) {
        let addrs: Vec<String> = self.triggers.keys().cloned().collect();
        for a in addrs {
            let env = self.state.declare(&format!("{a}.env"), Meta::float(0.0, [0.0, 1.0]).readonly().owner("triggers"));
            let act = self.state.declare(&format!("{a}.active"), Meta::boolean(false).readonly().owner("triggers"));
            let payload = self.declare_payload(&a);
            let t = self.triggers.get_mut(&a).unwrap();
            t.env_id = env;
            t.active_id = act;
            t.payload_ids = payload;
        }
    }

    fn fire_trigger(&mut self, address: &str, payload: &Value, origin: Origin, ctx: &Ctx) -> Result<(), String> {
        if !address::is_valid(address, false) {
            return Err(format!("bad address `{address}`"));
        }
        let prio = self.priority_for(origin, ctx);
        if prio <= PRIORITY_CHAT && address.starts_with("mixer.") {
            return Err("chat can never touch the mixer".into());
        }
        let key = self.override_key(origin, ctx, prio);
        let now = self.now();
        let spec = self.ensure_trigger(address).spec;
        let get = |k: &str| payload_ms(payload, k);
        // `hold = "latch"`: held until released (toggled presets)
        let latch = payload.get_path("hold").and_then(Value::as_str).is_some_and(|h| h == "latch" || h == "inf");
        let mut hold = if latch { None } else { get("hold").or(spec.hold_ms) };
        if prio <= PRIORITY_CHAT {
            // chat effects always auto-expire
            let ttl = self.chat_ttl / MS;
            hold = Some(hold.unwrap_or(ttl).min(ttl));
        }
        let inst = Instance {
            id: next_id(),
            start: now,
            attack: get("attack").unwrap_or(spec.attack_ms) * MS,
            hold: hold.map(|h| h * MS),
            release: get("release").unwrap_or(spec.release_ms) * MS,
            released_at: None,
            release_from: 0.0,
            key,
            payload: payload.clone(),
        };
        let t = self.triggers.get_mut(address).unwrap();
        if !t.fire(inst) {
            return Err(format!("`{address}` is busy (retrigger = reject)"));
        }
        // same tick as `.active`, so a patch never sees the new edge with the old payload
        if !t.payload_ids.is_empty() {
            let ids = t.payload_ids.clone();
            let p = TriggerPayload::from_event(payload, ctx.actor.as_ref());
            let f = p.floats();
            for (i, id) in ids[..PAYLOAD_FIELDS.len()].iter().enumerate() {
                self.state.set_base(*id, Value::Float(f[i] as f64));
            }
            self.state.set_base(ids[PAYLOAD_FIELDS.len()], Value::from(p.user_color));
        }
        let mut e = Event::new(format!("{address}.trigger"), origin, payload.clone());
        e.actor = ctx.actor.clone();
        e.causal = ctx.parent;
        e.ts = now;
        self.events.push_back((e, Ctx { depth: ctx.depth, parent: ctx.parent, ..Default::default() }));
        Ok(())
    }

    fn tick_triggers(&mut self, now: Ts) {
        let mut ended = Vec::new();
        for (a, t) in self.triggers.iter_mut() {
            if t.instances.is_empty() && t.queued.is_empty() && t.level == 0.0 {
                continue;
            }
            let was = !t.instances.is_empty();
            let (lvl, active) = t.tick(now);
            let (e, ac) = (t.env_id, t.active_id);
            self.state.set_base(e, Value::Float(lvl));
            if was != active || self.state.param(ac).base != Value::Bool(active) {
                self.state.set_base(ac, Value::Bool(active));
            }
            if was && !active {
                ended.push(a.clone());
            }
        }
        for a in ended {
            let e = Event::new(format!("{a}.ended"), Origin::System, Value::Null);
            self.events.push_back((e, Ctx::default()));
        }
    }

    // ---- presets ---------------------------------------------------------------------

    fn fire_preset(&mut self, name: &str, payload: &Value, origin: Origin, ctx: &Ctx) -> Result<(), String> {
        let def = self.config.presets.get(name).cloned().ok_or_else(|| format!("unknown preset `{name}`"))?;
        let caller = self.priority_for(origin, ctx);
        let chat = caller <= PRIORITY_CHAT;
        if chat && (def.confirm || def.chat == Some(false)) {
            return Err(format!("preset `{name}` is not available to chat"));
        }
        let active = self.presets.iter().any(|p| p.name == name);
        if active {
            if def.toggle && !chat {
                self.release_preset(name, ctx.parent, true);
                return Ok(());
            }
            match def.conflict {
                Conflict::Reject => return Err(format!("preset `{name}` is already active")),
                Conflict::Queue => {
                    let mut c = Command::new(origin, Op::PresetFire { name: name.into(), payload: payload.clone() });
                    c.actor = ctx.actor.clone();
                    c.priority = ctx.priority;
                    c.causal = ctx.parent;
                    self.preset_queue.push((name.into(), c));
                    return Ok(());
                }
                Conflict::Replace => {
                    self.release_preset(name, ctx.parent, true);
                }
                Conflict::Stack => {}
            }
        }
        let priority = {
            let base = def.priority.unwrap_or(PRIORITY_PRESET).min(PRIORITY_MANUAL - 1);
            if chat { base.min(PRIORITY_CHAT) } else { base }
        };
        self.start_preset(&def, payload, priority, origin, ctx);
        Ok(())
    }

    fn start_preset(&mut self, def: &PresetDef, payload: &Value, priority: u16, origin: Origin, ctx: &Ctx) {
        let now = self.now();
        let inst = next_id();
        let trace = next_id();
        self.trace.add(trace, ctx.parent, now, "preset", format!("{} (priority {priority})", def.name));
        let key = format!("preset:{}", def.name);
        let chat = priority <= PRIORITY_CHAT;
        let expires = chat.then(|| now + def.hold.map(|h| h.ns()).unwrap_or(self.chat_ttl).min(self.chat_ttl));
        for (a, v) in &def.set {
            if chat && a.starts_with("mixer.") {
                continue;
            }
            let v = if chat { self.chat_clamp(a, v.clone()) } else { v.clone() };
            let i = self.state.ensure(a, &zero_of(&v));
            self.state.put_override(i, Override { key: key.clone(), priority, value: v, seq: 0, expires, anim: None, origin, causal: Some(trace) });
            self.cause.insert(i, trace);
        }
        let pctx =
            Ctx { key: Some(key.clone()), priority: Some(priority), parent: Some(trace), actor: ctx.actor.clone(), depth: ctx.depth, event: ctx.event.clone() };
        let mut fx_addrs = Vec::new();
        let mut longest: u64 = 0;
        for fx in &def.fx {
            let a = if fx.name.contains('.') { fx.name.clone() } else { format!("fx.{}", fx.name) };
            let mut p = Value::Map(fx.params.clone());
            if let Some(h) = fx.hold.or(def.hold) {
                p = p.with("hold", h.ms() as i64);
                longest = longest.max(h.ms());
            } else if def.toggle && !chat {
                // a toggled preset keeps its effects up until it is pressed again
                p = p.with("hold", "latch");
            } else if let Some(s) = self.trigger_specs.get(&a) {
                longest = longest.max(s.attack_ms + s.hold_ms.unwrap_or(0));
            } else {
                let d = TriggerSpec::default();
                longest = longest.max(d.attack_ms + d.hold_ms.unwrap_or(0));
            }
            if let Some(m) = payload.as_map() {
                for (k, v) in m {
                    p = p.with(k.clone(), v.clone());
                }
            }
            if let Err(e) = self.fire_trigger(&a, &p, origin, &pctx) {
                self.trace.add(next_id(), Some(trace), now, "error", e);
            } else if !fx.name.contains('.') {
                self.hold_effect_settings(&a, fx, &p, &key, priority, origin, trace, expires);
            }
            fx_addrs.push(a);
        }
        if let Some(l) = &def.lights {
            let args = Value::map()
                .with("cue", l.cue.clone().map(Value::Str).unwrap_or_default())
                .with("cuelist", l.cuelist.clone().map(Value::Str).unwrap_or_default())
                .with("look", l.look.clone().map(Value::Str).unwrap_or_default())
                .with("priority", priority as i64);
            self.exec_traced(&Op::Action { name: "lights.cue".into(), args }, origin, &pctx);
            if let Some(h) = l.hold {
                longest = longest.max(h.ms());
            }
        }
        if let Some(s) = &def.sound {
            self.exec_traced(&Op::Action { name: "audio.play".into(), args: Value::map().with("sound", s.clone()) }, origin, &pctx);
        }
        if let Some(m) = &def.mix
            && !chat
        {
            let args = Value::map().with("snapshot", m.snapshot.clone()).with("fade", m.fade.map(|f| Value::Int(f.ms() as i64)).unwrap_or(Value::Null));
            self.exec_traced(&Op::Action { name: "mixer.snapshot.recall".into(), args }, origin, &pctx);
        }
        if let Some(s) = &def.scene
            && !chat
        {
            self.exec_traced(&Op::SceneCut { scene: s.clone(), transition: None }, origin, &pctx);
        }
        if let Some(m) = &def.mode
            && !chat
        {
            self.exec_traced(&Op::ModeSet { mode: m.clone() }, origin, &pctx);
        }
        let cmds = def.commands.clone();
        self.run_list(&cmds, origin, pctx);
        let release_at = match def.hold {
            Some(h) => Some(now + h.ns()),
            None if def.set.is_empty() && !def.toggle && !def.until_released => Some(now + longest * MS),
            None if chat => Some(now + self.chat_ttl),
            None => None,
        };
        self.presets.push(ActivePreset {
            name: def.name.clone(),
            inst,
            started: now,
            release_at,
            priority,
            key,
            payload: payload.clone(),
            trace,
            fx: fx_addrs,
            lights: held_lights(def),
            actor: ctx.actor.clone(),
        });
        self.set_sys(&format!("preset.{}.active", def.name), Value::Bool(true));
        let e = Event::new(format!("preset.{}.fired", def.name), origin, payload.clone()).with_causal(Some(trace));
        self.events.push_back((e, Ctx { depth: ctx.depth, parent: Some(trace), ..Default::default() }));
        self.runtime_dirty = true;
    }

    /// Settings of a built-in effect in a preset's `fx` entry (`{ name = "glitch", speed = 2.0 }`)
    /// hold `fx.<name>.<setting>` (key `<preset key>#fx`) until that effect has faded out, so it
    /// keeps its shape through its release tail even after the preset itself has ended.
    #[allow(clippy::too_many_arguments)]
    fn hold_effect_settings(&mut self, a: &str, fx: &PresetFx, p: &Value, key: &str, priority: u16, origin: Origin, trace: Id, chat_expires: Option<Ts>) {
        let settings: Vec<(&String, &Value)> = fx.params.iter().filter(|(k, _)| !EFFECT_TIMING.contains(&k.as_str())).collect();
        if settings.is_empty() {
            return;
        }
        let now = self.now();
        let spec = self.triggers.get(a).map(|t| t.spec).unwrap_or_default();
        let latch = p.get_path("hold").and_then(Value::as_str).is_some_and(|h| h == "latch" || h == "inf");
        let hold = if latch { None } else { payload_ms(p, "hold").or(spec.hold_ms) };
        let end = hold.map(|h| now + (payload_ms(p, "attack").unwrap_or(spec.attack_ms) + h + payload_ms(p, "release").unwrap_or(spec.release_ms)) * MS);
        let expires = match (end, chat_expires) {
            (Some(e), Some(c)) => Some(e.min(c)),
            (e, c) => e.or(c),
        };
        let chat = priority <= PRIORITY_CHAT;
        let fkey = format!("{key}#fx");
        for (k, v) in settings {
            let addr = format!("{a}.{k}");
            let v = if chat { self.chat_clamp(&addr, v.clone()) } else { v.clone() };
            let i = self.state.ensure(&addr, &zero_of(&v));
            self.state.put_override(i, Override { key: fkey.clone(), priority, value: v, seq: 0, expires, anim: None, origin, causal: Some(trace) });
            self.cause.insert(i, trace);
        }
    }

    /// A released preset's effect settings ([`Core::hold_effect_settings`]) end with the
    /// effects' release tail instead of snapping back while the effect still fades out.
    fn fade_effect_settings(&mut self, name: &str, key: &str, now: Ts) {
        let fkey = format!("{key}#fx");
        let Some(def) = self.config.presets.get(name) else {
            self.state.remove_overrides_where(|o| o.key == fkey);
            return;
        };
        let mut due: Vec<(String, Ts)> = Vec::new();
        for (index, fx) in def.fx.iter().enumerate().filter(|(_, f)| !f.name.contains('.')) {
            let a = format!("fx.{}", fx.name);
            let fx_params = Value::Map(fx.params.clone());
            let release =
                payload_ms(&fx_params, "release").or_else(|| self.triggers.get(&a).map(|t| t.spec.release_ms)).unwrap_or(TriggerSpec::default().release_ms);
            // the entry's own settings, and any a knob held while it ran (see `preset_knob`)
            let knobbed = def.knobs.iter().filter_map(|k| match def.knob_slot(&k.target) {
                Some(KnobSlot::Fx { index: i, key }) if i == index => Some(key),
                _ => None,
            });
            let mut keys: Vec<String> = fx.params.keys().cloned().chain(knobbed).filter(|k| !EFFECT_TIMING.contains(&k.as_str())).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                due.push((format!("{a}.{k}"), now + release * MS));
            }
        }
        for (addr, until) in due {
            let Some(i) = self.state.id(&addr) else { continue };
            if let Some(mut o) = self.state.param(i).overrides.iter().find(|o| o.key == fkey).cloned()
                && o.expires.is_none_or(|e| e > until)
            {
                o.expires = Some(until);
                self.state.put_override(i, o);
            }
        }
    }

    /// Turn a quick effect's knob (`preset.knob`): a running instance's held value follows at
    /// once (settings in `set` and built-in effect settings; an overlay's burst size and trigger
    /// strengths take effect on the next firing). A saved turn also goes into the definition,
    /// so the next firing uses it right away, before the written file has reloaded; an unsaved
    /// one (a slider still held) leaves the definition matching the file. Returns the fitted
    /// value.
    fn preset_knob(&mut self, name: &str, target: &str, value: &Value, save: bool) -> Result<Value, String> {
        let (v, live) = {
            let def = self.config.presets.get_mut(name).ok_or_else(|| format!("unknown preset `{name}`"))?;
            let knob = def.knobs.iter().find(|k| k.target == target).ok_or_else(|| format!("quick effect `{name}` has no knob for `{target}`"))?;
            let v = knob.fit(value).ok_or_else(|| format!("{value} doesn't fit knob “{}”", knob.label))?;
            let live = match def.knob_slot(target) {
                Some(KnobSlot::Set) => Some(""),
                Some(KnobSlot::Fx { index, key }) if !def.fx[index].name.contains('.') && !EFFECT_TIMING.contains(&key.as_str()) => Some("#fx"),
                _ => None,
            };
            if save {
                def.set_knob_value(target, v.clone());
            }
            (v, live)
        };
        let Some(suffix) = live else { return Ok(v) };
        let running: Vec<(String, u16, Option<Ts>, Id)> =
            self.presets.iter().filter(|p| p.name == name).map(|p| (format!("{}{suffix}", p.key), p.priority, p.release_at, p.trace)).collect();
        for (key, priority, release_at, trace) in running {
            let held = if prio_is_chat(priority) { self.chat_clamp(target, v.clone()) } else { v.clone() };
            let i = self.state.ensure(target, &zero_of(&v));
            let o = match self.state.param(i).overrides.iter().find(|o| o.key == key).cloned() {
                Some(o) => Override { value: held, ..o },
                // an effect setting the file didn't have when it fired: held like the others
                // (until it ends; a latched one fades with its effect on release)
                None if suffix == "#fx" => {
                    Override { key, priority, value: held, seq: 0, expires: release_at, anim: None, origin: Origin::Ui, causal: Some(trace) }
                }
                None => continue,
            };
            self.state.put_override(i, o);
        }
        Ok(v)
    }

    /// Release every active instance of a preset.
    fn release_preset(&mut self, name: &str, parent: Option<Id>, run_on_release: bool) -> bool {
        let now = self.now();
        let (gone, keep): (Vec<ActivePreset>, Vec<ActivePreset>) = std::mem::take(&mut self.presets).into_iter().partition(|p| p.name == name);
        self.presets = keep;
        if gone.is_empty() {
            return false;
        }
        for p in &gone {
            let key = p.key.clone();
            self.state.remove_overrides_where(|o| o.key == key);
            for a in &p.fx {
                if let Some(t) = self.triggers.get_mut(a) {
                    t.release(Some(&key), now);
                }
            }
            self.fade_effect_settings(name, &key, now);
            if let Some(l) = &p.lights {
                let args = Value::map()
                    .with("cue", l.cue.clone().map(Value::Str).unwrap_or_default())
                    .with("cuelist", l.cuelist.clone().map(Value::Str).unwrap_or_default())
                    .with("look", l.look.clone().map(Value::Str).unwrap_or_default());
                self.outbox.push(Output::Action(Command::new(Origin::Rule, Op::Action { name: "lights.release".into(), args }).caused_by(Some(p.trace))));
            }
            let id = next_id();
            self.trace.add(id, parent.or(Some(p.trace)), now, "preset", format!("{name} released"));
            if run_on_release && let Some(def) = self.config.presets.get(name).cloned() {
                let ctx = Ctx { key: Some(key.clone()), priority: Some(p.priority), parent: Some(id), actor: p.actor.clone(), ..Default::default() };
                self.run_list(&def.on_release, Origin::Rule, ctx);
            }
            let _ = (p.inst, p.started);
        }
        let still = self.presets.iter().any(|p| p.name == name);
        if !still {
            self.set_sys(&format!("preset.{name}.active"), Value::Bool(false));
            let e = Event::new(format!("preset.{name}.released"), Origin::System, Value::Null);
            self.events.push_back((e, Ctx::default()));
            // start a queued firing
            if let Some(pos) = self.preset_queue.iter().position(|(n, _)| n == name) {
                let (_, c) = self.preset_queue.remove(pos);
                self.submit(Input::Command { cmd: c });
            }
        }
        self.runtime_dirty = true;
        true
    }

    fn tick_presets(&mut self, now: Ts) {
        let expired = |p: &ActivePreset| p.release_at.is_some_and(|r| now >= r);
        let mut due: Vec<String> = self.presets.iter().filter(|p| expired(p)).map(|p| p.name.clone()).collect();
        due.dedup();
        for n in due {
            let live = self.presets.iter().filter(|p| p.name == n && !expired(p)).count();
            if live > 0 {
                // Stacked instances share the preset's override key; the layer stays until the last one ends.
                self.presets.retain(|p| !(p.name == n && expired(p)));
            } else {
                self.release_preset(&n, None, true);
            }
        }
    }

    // ---- scenes ----------------------------------------------------------------------

    fn take(&mut self, transition: Option<String>, ms: Option<u32>, ctx: &Ctx) -> Result<(), String> {
        let now = self.now();
        let to = self.state.get(addr::PREVIEW).and_then(Value::as_str).unwrap_or("").to_string();
        let from = self.state.get(addr::PROGRAM).and_then(Value::as_str).unwrap_or("").to_string();
        self.require_scene(&to)?;
        if to == from && self.transition.is_none() {
            return Ok(());
        }
        let def = self.config.scenes.get(&to).cloned().unwrap_or_default();
        let pool = def.transitions.for_pair(&from);
        let base = self.config.project.transitions.for_pair(&from);
        let (name, by) = match transition {
            Some(t) => (t, "command"),
            None => self.pick_transition(&pool, &base, now),
        };
        let tdef = self.config.transitions.get(&name);
        // a cut stays instant unless the command itself gives a duration
        let dur_ms = ms
            .or((name == "cut").then_some(0))
            .or_else(|| pool.ms_range().or_else(|| base.ms_range()).map(|(lo, hi)| self.rng.range(lo, hi)))
            .or_else(|| tdef.and_then(|t| t.ms).map(|d| d.ms() as u32))
            .unwrap_or(700);
        self.transition_history.push(name.clone());
        if self.transition_history.len() > 32 {
            self.transition_history.remove(0);
        }
        self.set_sys(addr::TR_NAME, Value::Str(name.clone()));
        self.set_sys(addr::TR_MS, Value::Int(dur_ms as i64));
        self.set_sys(addr::TR_FROM, Value::Str(from.clone()));
        self.set_sys(addr::TR_START, Value::Int(now as i64));
        self.set_sys(addr::TR_ACTIVE, Value::Bool(dur_ms > 0));
        self.set_sys(addr::TR_PROGRESS, Value::Float(if dur_ms > 0 { 0.0 } else { 1.0 }));
        self.set_sys(addr::PROGRAM, Value::Str(to.clone()));
        self.transition = (dur_ms > 0).then(|| Transition { from: from.clone(), to: to.clone(), name: name.clone(), start: now, dur: dur_ms as u64 * MS });
        self.apply_scene_layer();
        let payload = Value::map().with("from", from.clone()).with("to", to.clone()).with("transition", name).with("ms", dur_ms as i64).with("by", by);
        let e = Event::new("scene.take", Origin::System, payload).with_causal(ctx.parent);
        self.events.push_back((e, Ctx { depth: ctx.depth, parent: ctx.parent, ..Default::default() }));
        let sctx = Ctx { key: Some(format!("scene:{to}")), priority: Some(PRIORITY_PRESET), parent: ctx.parent, depth: ctx.depth, ..Default::default() };
        if let Some(old) = self.config.scenes.get(&from).cloned() {
            self.run_list(&old.on_exit, Origin::Rule, sctx.clone());
        }
        if let Some(l) = def.lights.clone().or(pool.lights.clone()).or(base.lights.clone()) {
            let args = Value::map().with("cue", l.cue.map(Value::Str).unwrap_or_default()).with("cuelist", l.cuelist.map(Value::Str).unwrap_or_default());
            self.exec_traced(&Op::Action { name: "lights.cue".into(), args }, Origin::Rule, &sctx);
        }
        self.run_list(&def.on_enter, Origin::Rule, sctx);
        if dur_ms == 0 {
            self.finish_transition(&to);
        }
        self.runtime_dirty = true;
        Ok(())
    }

    /// Transition for a take without one named: the pair's/scene's fixed `name`, else the chat
    /// vote winner, else a weighted pick from the pair's/scene's pool, else the project-wide
    /// default (`base`: its fixed `name`, else its pool), else `fade`. Returns the name and what
    /// chose it.
    fn pick_transition(&mut self, p: &crate::config::TransitionPool, base: &crate::config::TransitionPool, now: Ts) -> (String, &'static str) {
        if let Some(n) = &p.name {
            return (n.clone(), "fixed");
        }
        if let Some(n) = self.vote_winner(now) {
            self.transition_votes.clear();
            self.publish_votes(now);
            return (n, "vote");
        }
        if let Some(n) = self.pick_from_pool(p) {
            return (n, "pool");
        }
        if let Some(n) = base.name.clone().or_else(|| self.pick_from_pool(base)) {
            return (n, "project");
        }
        ("fade".into(), "default")
    }

    /// A weighted pick from `p`'s pool, skipping the last `avoid_repeat` transitions when others
    /// are left; `None` when nothing in it can be picked.
    fn pick_from_pool(&mut self, p: &crate::config::TransitionPool) -> Option<String> {
        let recent: Vec<&String> = self.transition_history.iter().rev().take(p.avoid_repeat).collect();
        let mut cands: Vec<&crate::config::PoolEntry> = p.pool.iter().filter(|e| !recent.contains(&&e.name) && e.w > 0.0).collect();
        if cands.is_empty() {
            cands = p.pool.iter().filter(|e| e.w > 0.0).collect();
        }
        if cands.is_empty() {
            return None;
        }
        let total: f64 = cands.iter().map(|e| e.w).sum();
        let mut r = self.rng.f64() * total;
        for e in &cands {
            if r < e.w {
                return Some(e.name.clone());
            }
            r -= e.w;
        }
        cands.last().map(|e| e.name.clone())
    }

    // ---- transition votes (§4.4) ------------------------------------------------------

    fn vote_cfg(&self) -> Option<crate::config::TransitionVoteDef> {
        self.config.transition_vote().ok().flatten().filter(|v| v.enabled)
    }

    /// What viewers may vote for (canonical names).
    fn vote_choices(&self, v: &crate::config::TransitionVoteDef) -> Vec<String> {
        if v.choices.is_empty() { self.config.transition_names() } else { v.choices.clone() }
    }

    /// Live votes per transition, in order of their first vote.
    fn vote_tally(&self, now: Ts) -> Vec<(String, i64)> {
        let Some(v) = self.vote_cfg() else { return Vec::new() };
        let choices = self.vote_choices(&v);
        let mut tally: Vec<(String, i64)> = Vec::new();
        for (_, name, expires) in &self.transition_votes {
            if now > *expires || !choices.contains(name) {
                continue;
            }
            match tally.iter_mut().find(|(n, _)| n == name) {
                Some((_, c)) => *c += 1,
                None => tally.push((name.clone(), 1)),
            }
        }
        tally
    }

    /// Most votes wins; a tie goes to the transition that got its first vote earliest.
    fn vote_winner(&self, now: Ts) -> Option<String> {
        let tally = self.vote_tally(now);
        let best = tally.iter().map(|(_, c)| *c).max()?;
        tally.into_iter().find(|(_, c)| *c == best).map(|(n, _)| n)
    }

    /// `show.transition.votes` = `{transition: votes}`; returns it.
    fn publish_votes(&mut self, now: Ts) -> Value {
        let m: std::collections::BTreeMap<String, Value> = self.vote_tally(now).into_iter().map(|(n, c)| (n, Value::Int(c))).collect();
        let v = Value::Map(m);
        self.set_sys(addr::TR_VOTES, v.clone());
        v
    }

    /// `transition.vote {name, user?}`: one vote per viewer (a new vote replaces theirs).
    fn transition_vote(&mut self, args: &Value, origin: Origin, ctx: &Ctx) -> Result<(), String> {
        let v = self.vote_cfg().ok_or("transition voting is off ([transition_vote] in project.toml)")?;
        let from_sim = ctx.event.as_ref().is_some_and(|e| e.origin == Origin::Sim);
        if prio_is_chat(self.priority_for(origin, ctx)) && !from_sim && !self.policy.cfg.effect_modes.iter().any(|m| m == self.mode_str()) {
            return Err(format!("chat effects are paused in mode `{}`", self.mode_str()));
        }
        let pos0 = args.get_path("args").and_then(Value::as_list).and_then(|l| l.first());
        let asked = args.get_path("name").or(pos0).map(|n| n.to_string()).unwrap_or_default();
        let asked = asked.trim().trim_start_matches('#');
        if asked.is_empty() {
            return Err("transition.vote needs a transition name".into());
        }
        let choices = self.vote_choices(&v);
        let name = choices
            .iter()
            .find(|c| c.eq_ignore_ascii_case(asked))
            .cloned()
            .ok_or_else(|| format!("`{asked}` isn't one of the choices ({})", choices.join(", ")))?;
        let voter = ctx
            .actor
            .as_ref()
            .map(|a| a.id.clone())
            .or_else(|| args.get_path("user").map(|u| u.to_string()))
            .filter(|u| !u.is_empty())
            .ok_or("transition.vote needs a viewer")?;
        let now = self.now();
        self.transition_votes.retain(|(who, _, expires)| *who != voter && now <= *expires);
        self.transition_votes.push((voter, name.clone(), now + v.window.ns()));
        let tally = self.publish_votes(now);
        let user = ctx.actor.as_ref().map(|a| a.name.clone()).or_else(|| args.get_path("user").map(|u| u.to_string())).unwrap_or_default();
        let mut e = Event::new("transition.vote", Origin::System, Value::map().with("user", user).with("name", name).with("votes", tally));
        e.actor = ctx.actor.clone();
        e.causal = ctx.parent;
        e.ts = now;
        self.events.push_back((e, Ctx { depth: ctx.depth, parent: ctx.parent, ..Default::default() }));
        Ok(())
    }

    fn tick_transition(&mut self, now: Ts) {
        // votes run out after the window (oldest first)
        if self.transition_votes.first().is_some_and(|(_, _, expires)| now > *expires) {
            self.transition_votes.retain(|(_, _, expires)| now <= *expires);
            self.publish_votes(now);
        }
        let Some(t) = &self.transition else { return };
        let p = ((now - t.start) as f64 / t.dur.max(1) as f64).min(1.0);
        let to = t.to.clone();
        self.set_sys(addr::TR_PROGRESS, Value::Float(p));
        if p >= 1.0 {
            self.finish_transition(&to);
        }
    }

    fn finish_transition(&mut self, to: &str) {
        let (from, name) = self.transition.take().map(|t| (t.from, t.name)).unwrap_or_default();
        self.set_sys(addr::TR_ACTIVE, Value::Bool(false));
        self.set_sys(addr::TR_PROGRESS, Value::Float(1.0));
        let e = Event::new("scene.changed", Origin::System, Value::map().with("scene", to).with("from", from).with("transition", name));
        self.events.push_back((e, Ctx::default()));
    }

    /// Scene-layer values from the program scene's `set` table.
    fn apply_scene_layer(&mut self) {
        for i in std::mem::take(&mut self.scene_layer) {
            if i < self.state.len() {
                self.state.set_scene(i, None);
            }
        }
        let prog = self.state.get(addr::PROGRAM).and_then(Value::as_str).unwrap_or("").to_string();
        if let Some(s) = self.config.scenes.get(&prog) {
            for (a, v) in s.set.clone() {
                let i = self.state.ensure(&a, &zero_of(&v));
                self.state.set_scene(i, Some(v));
                self.scene_layer.push(i);
            }
        }
    }

    // ---- modes -----------------------------------------------------------------------

    fn set_mode(&mut self, mode: &str, origin: Origin, ctx: &Ctx) -> Result<(), String> {
        if prio_is_chat(self.priority_for(origin, ctx)) {
            return Err("chat cannot change the mode".into());
        }
        let modes = self.config.modes();
        if !modes.iter().any(|m| m == mode) {
            return Err(format!("unknown mode `{mode}` (modes: {})", modes.join(", ")));
        }
        let from = self.mode_str().to_string();
        if from == mode {
            return Ok(());
        }
        self.set_sys(addr::MODE, Value::Str(mode.into()));
        let payload = Value::map().with("from", from.clone()).with("to", mode);
        let c = Ctx { depth: ctx.depth, parent: ctx.parent, ..Default::default() };
        self.events.push_back((Event::new("mode.changed", Origin::System, payload.clone()).with_causal(ctx.parent), c.clone()));
        self.events.push_back((Event::new(format!("mode.exit.{from}"), Origin::System, payload.clone()).with_causal(ctx.parent), c.clone()));
        self.events.push_back((Event::new(format!("mode.enter.{mode}"), Origin::System, payload).with_causal(ctx.parent), c));
        self.runtime_dirty = true;
        Ok(())
    }

    // ---- panic / clean / undo --------------------------------------------------------

    fn panic(&mut self, ctx: &Ctx) {
        let now = self.now();
        let names: Vec<String> = self.presets.iter().map(|p| p.name.clone()).collect();
        for n in names {
            self.release_preset(&n, ctx.parent, false);
        }
        self.preset_queue.clear();
        self.state.remove_overrides_where(|o| o.priority < PRIORITY_MANUAL);
        for t in self.triggers.values_mut() {
            t.release(None, now);
        }
        self.scheduled.clear();
        self.timelines_panic();
        self.set_sys(addr::PANIC, Value::Bool(true));
        if self.config.presets.contains_key("panic") {
            let _ = self.fire_preset("panic", &Value::Null, Origin::System, &Ctx { parent: ctx.parent, ..Default::default() });
        }
        for a in ["lights.panic", "mixer.panic", "audio.panic"] {
            self.outbox.push(Output::Action(Command::new(Origin::System, Op::Action { name: a.into(), args: Value::Null }).caused_by(ctx.parent)));
        }
        self.events.push_back((Event::new("panic", Origin::System, Value::Null).with_causal(ctx.parent), Ctx::default()));
        self.runtime_dirty = true;
    }

    fn clean(&mut self, ctx: &Ctx) {
        let now = self.now();
        self.state.remove_overrides_where(|o| o.priority <= PRIORITY_CHAT);
        for t in self.triggers.values_mut() {
            let keys: Vec<String> = t.instances.iter().filter(|i| i.key.starts_with("chat")).map(|i| i.key.clone()).collect();
            for k in keys {
                t.release(Some(&k), now);
            }
        }
        let chat_presets: Vec<String> = self.presets.iter().filter(|p| p.priority <= PRIORITY_CHAT).map(|p| p.name.clone()).collect();
        for n in chat_presets {
            self.release_preset(&n, ctx.parent, false);
        }
        self.set_sys(addr::PANIC, Value::Bool(false));
        self.events.push_back((Event::new("clean", Origin::System, Value::Null).with_causal(ctx.parent), Ctx::default()));
    }

    fn undo_redo(&mut self, undo: bool) -> Result<(), String> {
        let e = if undo { self.undo.pop() } else { self.redo.pop() }.ok_or_else(|| if undo { "nothing to undo" } else { "nothing to redo" }.to_string())?;
        let v = if undo { e.old.clone() } else { e.new.clone() };
        let i = self.state.ensure(&e.address, &v);
        self.state.set_base(i, v.clone());
        self.outbox.push(Output::PersistBase { address: e.address.clone(), value: v });
        if undo {
            self.redo.push(e)
        } else {
            self.undo.push(e)
        }
        Ok(())
    }

    // ---- actions ---------------------------------------------------------------------

    /// `policy.run {key, role, cooldown, approval, filter, event, do}`: a viewer's ad-hoc action
    /// (chatbot command) through the policy gate.
    fn policy_run(&mut self, args: &Value, origin: Origin, ctx: &Ctx) -> Result<(), String> {
        let key = args.get_path("key").and_then(Value::as_str).ok_or("policy.run needs a `key`")?.to_string();
        let spec = crate::policy::GateSpec::from_args(args)?;
        let commands: Vec<String> = match args.get_path("do") {
            Some(Value::Str(s)) => vec![s.clone()],
            Some(Value::List(l)) => l.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            _ => return Err("policy.run needs `do`".into()),
        };
        for c in &commands {
            Op::parse(c).map_err(|e| format!("`{c}`: {e}"))?;
        }
        let event = args.get_path("event").filter(|v| v.as_map().is_some()).map(|p| {
            let mut e = Event::new("policy.run", origin, p.clone());
            e.actor = ctx.actor.clone();
            e.causal = ctx.parent;
            e.ts = self.now();
            e
        });
        let id = crate::policy::hold_id(ctx.parent.unwrap_or_else(next_id));
        let mut fx = Vec::new();
        let res = self.policy.run_gated(id, &key, &spec, commands, ctx.actor.clone(), event, self.now(), &mut fx);
        for e in fx {
            self.apply_policy(e, ctx);
        }
        res
    }

    /// The value `toggle` sets: a switch flips; a number goes to 0, or from 0 to its declared
    /// max (1 when it has no range). Integers stay integers.
    fn toggled(&self, address: &str) -> Result<Value, String> {
        let i = self.state.id(address).ok_or_else(|| format!("unknown setting `{address}`"))?;
        let max = self.state.param(i).meta.range.map_or(1.0, |r| r[1]);
        Ok(match self.state.value(i) {
            Value::Bool(b) => Value::Bool(!b),
            Value::Int(n) => Value::Int(if *n != 0 { 0 } else { max.round() as i64 }),
            Value::Float(f) => Value::Float(if *f != 0.0 { 0.0 } else { max }),
            Value::Null => Value::Bool(true),
            _ => return Err(format!("`{address}` is not an on/off or number setting")),
        })
    }

    fn action(&mut self, name: &str, args: &Value, origin: Origin, ctx: &Ctx) -> Result<(), String> {
        let pos = |i: usize| args.get_path("args").and_then(|a| a.as_list()).and_then(|l| l.get(i)).cloned();
        let arg = |k: &str, i: usize| args.get_path(k).cloned().or_else(|| pos(i));
        let prio = self.priority_for(origin, ctx);
        if prio <= PRIORITY_CHAT && (name.starts_with("mixer.") || name.starts_with("obs.") || name.starts_with("rule.")) {
            return Err(format!("chat cannot run `{name}`"));
        }
        // viewers' own commands may not moderate or drive the channel (§12.3, §19)
        if matches!(origin, Origin::Chat | Origin::Relay) {
            crate::policy::chat_action_allowed(name, ctx.actor.as_ref())?;
        }
        if let Some(preset) = name.strip_prefix("sim.") {
            let evs = crate::sim::events(preset, args, &mut self.rng)?;
            for mut e in evs {
                e.ts = self.now();
                e.causal = ctx.parent;
                self.events.push_back((e, Ctx { depth: ctx.depth, parent: ctx.parent, ..Default::default() }));
            }
            return Ok(());
        }
        match name {
            n if timeline::is_transport(n) => self.timeline_action(n, args),
            "policy.run" => self.policy_run(args, origin, ctx),
            "mod.approve" | "mod.reject" => {
                let id = arg("id", 0).map(|v| v.to_string()).ok_or("needs the pending item's `id`")?;
                let reason = args.get_path("reason").and_then(Value::as_str).map(String::from);
                let by = ctx.actor.as_ref().map(|a| a.name.clone()).unwrap_or_else(|| origin.as_str().to_string());
                let mode = self.mode_str().to_string();
                let mut fx = Vec::new();
                self.policy.resolve(&id, name == "mod.approve", &by, reason.as_deref(), &mode, &mut fx)?;
                for e in fx {
                    self.apply_policy(e, ctx);
                }
                Ok(())
            }
            "signal.set" => {
                let n = arg("name", 0).and_then(|v| v.as_str().map(String::from)).ok_or("signal.set needs a name")?;
                let v = arg("value", 1).and_then(|v| v.as_f32()).ok_or("signal.set needs a value")?;
                self.signals.set(&n, v, self.now());
                Ok(())
            }
            "rule.enable" | "rule.disable" => {
                let n = arg("name", 0).map(|v| v.to_string()).ok_or("needs a rule name")?;
                let r = self.rules.iter_mut().find(|r| r.def.name == n).ok_or_else(|| format!("unknown rule `{n}`"))?;
                r.enabled = name == "rule.enable";
                self.runtime_dirty = true;
                Ok(())
            }
            "binding.enable" | "binding.disable" => {
                let n = arg("name", 0).map(|v| v.to_string()).ok_or("needs a binding name")?;
                let b = self.bindings.iter_mut().find(|b| b.def.name == n).ok_or_else(|| format!("unknown binding `{n}`"))?;
                b.def.enabled = name == "binding.enable";
                if !b.def.enabled {
                    b.deactivate();
                }
                self.runtime_dirty = true;
                Ok(())
            }
            "preset.toggle" => {
                let n = arg("name", 0).map(|v| v.to_string()).ok_or("needs a preset name")?;
                if self.presets.iter().any(|p| p.name == n) {
                    self.release_preset(&n, ctx.parent, true);
                    Ok(())
                } else {
                    self.fire_preset(&n, &Value::Null, origin, ctx)
                }
            }
            "toggle" => {
                let a = arg("address", 0).and_then(|v| v.as_str().map(String::from)).ok_or("toggle needs an address")?;
                if a.contains('*') && !address::is_valid(&a, true) {
                    return Err(format!("bad address `{a}`"));
                }
                let targets: Vec<String> = self.state.matching(&a).map(|i| self.state.param(i).addr.clone()).collect();
                if targets.is_empty() {
                    return Err(format!("`{a}` matches nothing"));
                }
                // each match flips from its own value, at the same priority a `set` would use
                for t in targets {
                    let v = self.toggled(&t)?;
                    self.exec(&Op::Set { address: t, value: v }, origin, ctx)?;
                }
                Ok(())
            }
            "preset.knob" => {
                if prio <= PRIORITY_CHAT {
                    return Err("chat cannot turn a quick effect's knobs".into());
                }
                let n = arg("name", 0).map(|v| v.to_string()).ok_or("needs a preset name")?;
                let target = arg("target", 1).and_then(|v| v.as_str().map(String::from)).ok_or("needs the knob's `target`")?;
                let value = arg("value", 2).ok_or("needs a `value`")?;
                let save = args.get_path("save").is_none_or(Value::truthy);
                let v = self.preset_knob(&n, &target, &value, save)?;
                if save {
                    // the file write-back belongs to the engine around the core
                    let mut c =
                        Command::new(origin, Op::Action { name: name.into(), args: Value::map().with("name", n).with("target", target).with("value", v) });
                    c.actor = ctx.actor.clone();
                    c.causal = ctx.parent;
                    c.priority = Some(prio);
                    c.ts = self.now();
                    self.outbox.push(Output::Action(c));
                }
                Ok(())
            }
            "transition.vote" => self.transition_vote(args, origin, ctx),
            "scene.next" | "scene.prev" => {
                let names = scene_order(&self.config);
                if names.is_empty() {
                    return Err("no scenes".into());
                }
                let cur = self.state.get(addr::PREVIEW).and_then(Value::as_str).unwrap_or("").to_string();
                let i = names.iter().position(|n| *n == cur).unwrap_or(0);
                let j = if name == "scene.next" { (i + 1) % names.len() } else { (i + names.len() - 1) % names.len() };
                self.exec(&Op::SceneGo { scene: names[j].clone() }, origin, ctx)
            }
            _ => {
                let args = if origin == Origin::Timeline { timeline::playback_args(name, args, prio) } else { args.clone() };
                let mut c = Command::new(origin, Op::Action { name: name.into(), args });
                c.actor = ctx.actor.clone();
                c.causal = ctx.parent;
                c.priority = Some(prio);
                c.ts = self.now();
                self.outbox.push(Output::Action(c));
                Ok(())
            }
        }
    }

    // ---- signals & bindings ----------------------------------------------------------

    fn tick_signals(&mut self, now: Ts) {
        let t = (now - self.t0) as f64 / 1e9;
        self.signals.set_id(self.time_sig, t as f32, now);
        let bp = self.signals.value(self.beat_sig);
        for l in &mut self.lfos {
            l.tick(t, bp, &mut self.signals, now);
        }
    }

    fn tick_bindings(&mut self) {
        let dt = self.period as f64 / 1e9;
        let sgen_state = self.state.generation;
        let sgen = self.signals.generation;
        let mode = self.mode_str().to_string();
        let program = self.state.get(addr::PROGRAM).and_then(Value::as_str).unwrap_or("").to_string();
        let mut mods: HashMap<usize, Vec<Mod>> = HashMap::new();
        let mut bindings = std::mem::take(&mut self.bindings);
        for b in bindings.iter_mut() {
            if b.targets_gen != sgen_state {
                b.targets = self.state.matching(&b.def.target).collect();
                b.targets_gen = sgen_state;
            }
            if b.signal_gen != sgen || b.signal.is_none() {
                b.signal = self.signals.id(&b.def.signal);
                b.signal_gen = sgen;
            }
            let in_scope = b.def.enabled
                && match &b.scope {
                    BindScope::Always => true,
                    BindScope::Scene(s) => *s == program,
                    BindScope::Preset(p) => self.presets.iter().any(|x| &x.name == p),
                    BindScope::Mode(m) => *m == mode,
                    BindScope::Expr(e) => e.eval_bool(&EvalScope { core: self, event: None }),
                };
            if !in_scope {
                if b.active {
                    b.deactivate();
                }
                continue;
            }
            b.active = true;
            let Some(sig) = b.signal else { continue };
            let raw = self.signals.value(sig) as f64;
            let out = b.shape(raw, dt);
            if b.def.takeover.is_some() {
                // Physical controls act like a hand on the control: only movement writes, as a
                // manual override the rest of the system (mixer readback, presets, UI) can see
                // and replace. Pickup compares against the value without this control.
                if !b.moved(out) {
                    continue;
                }
                for k in 0..b.targets.len() {
                    let t = b.targets[k];
                    let cur = self.state.value(t).as_f64();
                    if let Some(v) = b.takeover(out, cur) {
                        let key = format!("binding:{}", b.def.name);
                        self.state.put_override(
                            t,
                            Override {
                                key,
                                priority: PRIORITY_MANUAL,
                                value: Value::Float(v),
                                seq: 0,
                                expires: None,
                                anim: None,
                                origin: Origin::Binding,
                                causal: None,
                            },
                        );
                        let now = self.now();
                        self.state.refresh(t, now);
                        self.runtime_dirty = true;
                    }
                }
                continue;
            }
            for k in 0..b.targets.len() {
                mods.entry(b.targets[k]).or_default().push(Mod { source: b.def.name.clone(), mode: b.def.mode, value: out });
            }
        }
        self.bindings = bindings;
        // clear mods on params that no longer have active bindings
        let n = self.state.len();
        for i in 0..n {
            match mods.remove(&i) {
                Some(m) => self.state.set_mods(i, m),
                None => {
                    if !self.state.param(i).mods.is_empty() {
                        self.state.set_mods(i, Vec::new());
                    }
                }
            }
        }
    }

    fn resolve(&mut self) {
        let now = self.now();
        let mut changed: Vec<(usize, Value)> = Vec::new();
        self.state.resolve(now, |i, v| changed.push((i, v.clone())));
        if changed.is_empty() {
            self.cause.clear();
            return;
        }
        let mut out = Vec::with_capacity(changed.len());
        for (i, v) in changed {
            let a = self.state.param(i).addr.clone();
            if let Some(c) = self.cause.remove(&i) {
                self.trace.add(next_id(), Some(c), now, "change", format!("{a} = {v}"));
            }
            out.push((a, v));
        }
        self.cause.clear();
        self.outbox.push(Output::Changes(out));
    }

    // ---- persistence -----------------------------------------------------------------

    pub fn runtime_state(&self) -> RuntimeState {
        let now = self.now();
        let mut overrides = Vec::new();
        for p in self.state.params() {
            for o in &p.overrides {
                if o.priority > PRIORITY_CHAT && o.anim.is_none() && o.expires.is_none() {
                    overrides.push((p.addr.clone(), o.clone()));
                }
            }
        }
        let bases = [addr::DIRECT].iter().filter_map(|a| Some((a.to_string(), self.state.get(a)?.clone()))).collect();
        RuntimeState {
            mode: self.mode_str().into(),
            program: self.state.get(addr::PROGRAM).and_then(Value::as_str).unwrap_or("").into(),
            preview: self.state.get(addr::PREVIEW).and_then(Value::as_str).unwrap_or("").into(),
            overrides,
            bases,
            presets: self
                .presets
                .iter()
                .filter(|p| p.priority > PRIORITY_CHAT)
                .map(|p| PersistedPreset {
                    name: p.name.clone(),
                    priority: p.priority,
                    remaining_ms: p.release_at.map(|r| r.saturating_sub(now) / MS),
                    payload: p.payload.clone(),
                })
                .collect(),
            disabled_rules: self.rules.iter().filter(|r| !r.enabled).map(|r| r.def.name.clone()).collect(),
            disabled_bindings: self.bindings.iter().filter(|b| !b.def.enabled).map(|b| b.def.name.clone()).collect(),
            transition_history: self.transition_history.clone(),
            rng: self.rng.state(),
            timelines: self.timelines_persist(),
        }
    }

    /// Restore after a crash/restart without re-running preset side effects.
    pub fn restore(&mut self, rs: &RuntimeState) {
        let now = self.now();
        if self.config.modes().contains(&rs.mode) {
            self.set_sys(addr::MODE, Value::Str(rs.mode.clone()));
        }
        if self.config.scenes.contains_key(&rs.program) {
            self.set_sys(addr::PROGRAM, Value::Str(rs.program.clone()));
        }
        if self.config.scenes.contains_key(&rs.preview) {
            self.set_sys(addr::PREVIEW, Value::Str(rs.preview.clone()));
        }
        for (a, v) in &rs.bases {
            self.set_sys(a, v.clone());
        }
        for (a, o) in &rs.overrides {
            let missing_address = a.strip_prefix("scene.").and_then(|s| s.split('.').next()).is_some_and(|name| !self.config.scenes.contains_key(name))
                || a.strip_prefix("preset.").and_then(|s| s.split('.').next()).is_some_and(|name| !self.config.presets.contains_key(name));
            let missing_scene =
                matches!(a.as_str(), addr::PROGRAM | addr::PREVIEW) && o.value.as_str().is_none_or(|name| !self.config.scenes.contains_key(name));
            if missing_address || missing_scene || !configured_owner(&self.config, &o.key) {
                continue;
            }
            let i = self.state.ensure(a, &zero_of(&o.value));
            self.state.put_override(i, o.clone());
        }
        for p in &rs.presets {
            let Some(def) = self.config.presets.get(&p.name).cloned() else { continue };
            let fx: Vec<String> = def.fx.iter().map(|f| if f.name.contains('.') { f.name.clone() } else { format!("fx.{}", f.name) }).collect();
            self.presets.push(ActivePreset {
                name: p.name.clone(),
                inst: next_id(),
                started: now,
                release_at: p.remaining_ms.map(|r| now + r * MS),
                priority: p.priority,
                key: format!("preset:{}", p.name),
                payload: p.payload.clone(),
                trace: next_id(),
                fx,
                lights: held_lights(&def),
                actor: None,
            });
            self.set_sys(&format!("preset.{}.active", p.name), Value::Bool(true));
        }
        for r in &mut self.rules {
            r.enabled = !rs.disabled_rules.contains(&r.def.name);
        }
        for b in &mut self.bindings {
            if rs.disabled_bindings.contains(&b.def.name) {
                b.def.enabled = false;
            }
        }
        self.transition_history = rs.transition_history.clone();
        if rs.rng != 0 {
            self.rng.set_state(rs.rng);
        }
        self.timelines_restore(&rs.timelines);
        self.apply_scene_layer();
        let mode = self.mode_str().to_string();
        self.policy.restored(&mode, now);
        self.events.push_back((
            Event::new(
                "engine.restored",
                Origin::System,
                Value::map().with("mode", self.mode_str()).with("scene", self.state.get(addr::PROGRAM).cloned().unwrap_or_default()),
            ),
            Ctx::default(),
        ));
    }

    // ---- queries ---------------------------------------------------------------------

    /// Named queries answered by the core.
    pub fn query(&self, name: &str, args: &Value) -> Result<Value, String> {
        let now = self.now();
        Ok(match name {
            "presets" => Value::List(
                self.config
                    .presets
                    .values()
                    .map(|p| {
                        let active = self.presets.iter().find(|a| a.name == p.name);
                        Value::map()
                            .with("name", p.name.clone())
                            .with("label", p.label.clone().unwrap_or_else(|| p.name.to_uppercase()))
                            .with("color", p.color.clone().map(Value::Str).unwrap_or_default())
                            .with("active", active.is_some())
                            .with(
                                "remaining_ms",
                                active.and_then(|a| a.release_at).map(|r| Value::Int((r.saturating_sub(now) / MS) as i64)).unwrap_or_default(),
                            )
                            .with("toggle", p.toggle)
                            .with("confirm", p.confirm)
                            .with("chat", p.chat.unwrap_or(!p.confirm))
                            // stays on for a while or until released (so it can be stopped)
                            .with("held", p.hold.is_some() || p.toggle || p.until_released || !p.set.is_empty())
                            .with(
                                "knobs",
                                Value::List(
                                    p.knobs
                                        .iter()
                                        .map(|k| {
                                            // the file's value, else the knob's default, else what the address holds now
                                            let v = p
                                                .knob_value(&k.target)
                                                .or(k.default.as_ref())
                                                .or_else(|| self.state.get(&k.target))
                                                .cloned()
                                                .unwrap_or_default();
                                            k.to_value().with("value", v)
                                        })
                                        .collect(),
                                ),
                            )
                    })
                    .collect(),
            ),
            "rules" => Value::List(
                self.rules
                    .iter()
                    .map(|r| {
                        Value::map()
                            .with("name", r.def.name.clone())
                            .with("when", r.def.when.clone())
                            .with("if", r.def.condition.clone().map(Value::Str).unwrap_or_default())
                            .with("do", r.def.commands.clone())
                            .with("enabled", r.enabled)
                            .with("file", r.def.file.clone())
                            .with("last_fired_ms_ago", r.last.map(|l| Value::Int(((now - l) / MS) as i64)).unwrap_or_default())
                    })
                    .collect(),
            ),
            "bindings" => Value::List(
                self.bindings
                    .iter()
                    .map(|b| {
                        Value::map()
                            .with("name", b.def.name.clone())
                            .with("target", b.def.target.clone())
                            .with("signal", b.def.signal.clone())
                            .with("enabled", b.def.enabled)
                            .with("active", b.active)
                            .with("output", b.output)
                            .with("targets", b.targets.len())
                            .with("file", b.def.file.clone())
                            .with("mode", format!("{:?}", b.def.mode).to_lowercase())
                            .with("scope", b.def.scope.clone().map(Value::Str).unwrap_or_default())
                    })
                    .collect(),
            ),
            "scenes" => Value::List(
                scene_order(&self.config)
                    .into_iter()
                    .map(|n| {
                        let s = &self.config.scenes[&n];
                        Value::map()
                            .with("name", n.clone())
                            .with("label", s.label.clone().unwrap_or_else(|| n.clone()))
                            .with("key", s.key.map(|k| Value::Int(k as i64)).unwrap_or_default())
                            .with("nodes", s.node_ids())
                            .with("sources", s.canvas.values().flat_map(|c| c.nodes.iter().map(|n| Value::Str(n.src.clone()))).collect::<Vec<_>>())
                    })
                    .collect(),
            ),
            "transitions" => {
                // built-ins first; a project file with a built-in's name overrides it (listed once)
                Value::from(self.config.transition_names())
            }
            "modes" => Value::from(self.config.modes()),
            "signals" => {
                let pat = args.get_path("pattern").and_then(Value::as_str).unwrap_or("**");
                Value::List(
                    self.signals.iter().filter(|(n, _)| address::matches(pat, n)).map(|(n, v)| Value::map().with("name", n).with("value", v as f64)).collect(),
                )
            }
            "signal.history" => {
                let n = args.get_path("name").and_then(Value::as_str).ok_or("needs name")?;
                Value::List(self.signals.history(n).ok_or("unknown signal")?.into_iter().map(|v| Value::Float(v as f64)).collect())
            }
            "active" => {
                let mut items = Vec::new();
                for p in &self.presets {
                    items.push(
                        Value::map()
                            .with("kind", "preset")
                            .with("name", p.name.clone())
                            .with("priority", p.priority as i64)
                            .with("chat", p.priority <= PRIORITY_CHAT)
                            .with("remaining_ms", p.release_at.map(|r| Value::Int((r.saturating_sub(now) / MS) as i64)).unwrap_or_default()),
                    );
                }
                for (a, t) in &self.triggers {
                    if !t.instances.is_empty() {
                        let rem = t.instances.iter().filter_map(|i| i.hold.map(|h| (i.start + i.attack + h).saturating_sub(now) / MS)).max();
                        let chat = t.instances.iter().any(|i| i.key.starts_with("chat"));
                        items.push(
                            Value::map()
                                .with("kind", "trigger")
                                .with("name", a.clone())
                                .with("level", t.level)
                                .with("count", t.instances.len())
                                .with("chat", chat)
                                .with("remaining_ms", rem.map(|r| Value::Int(r as i64)).unwrap_or_default()),
                        );
                    }
                }
                Value::List(items)
            }
            "addresses" => {
                let pat = args.get_path("pattern").and_then(Value::as_str).unwrap_or("**");
                Value::List(self.state.matching(pat).map(|i| Value::Str(self.state.param(i).addr.clone())).collect())
            }
            "errors" => Value::List(self.config.errors.iter().map(|e| Value::map().with("file", e.file.clone()).with("msg", e.msg.clone())).collect()),
            "trace.recent" => {
                let n = args.get_path("n").and_then(Value::as_i64).unwrap_or(200).clamp(1, 5000) as usize;
                Value::List(
                    self.trace
                        .recent(n)
                        .into_iter()
                        .map(|r| {
                            Value::map()
                                .with("id", r.id as i64)
                                .with("parent", r.parent.map(|p| Value::Int(p as i64)).unwrap_or_default())
                                .with("kind", r.kind)
                                .with("label", r.label)
                                .with("ts", r.ts as i64)
                        })
                        .collect(),
                )
            }
            "timelines" | "timeline" => self.query_timelines(name, args)?,
            "sim.presets" => Value::List(crate::sim::PRESETS.iter().map(|(n, a)| Value::map().with("name", *n).with("args", *a)).collect()),
            "undo" => Value::map().with("undo", self.undo.len()).with("redo", self.redo.len()),
            "scene" => {
                let n = args.get_path("name").and_then(Value::as_str).ok_or("needs name")?;
                let s = self.config.scenes.get(n).ok_or("unknown scene")?;
                Value::from(serde_json::to_value(s).map_err(|e| e.to_string())?)
            }
            "config.scenes" => Value::from(serde_json::to_value(&self.config.scenes).map_err(|e| e.to_string())?),
            "config.presets" => Value::from(serde_json::to_value(&self.config.presets).map_err(|e| e.to_string())?),
            "config.transitions" => Value::from(serde_json::to_value(&self.config.transitions).map_err(|e| e.to_string())?),
            "config.rules" => Value::from(serde_json::to_value(&self.config.rules).map_err(|e| e.to_string())?),
            "config.bindings" => Value::from(serde_json::to_value(&self.config.bindings).map_err(|e| e.to_string())?),
            "config.project" => Value::from(serde_json::to_value(&self.config.project).map_err(|e| e.to_string())?),
            _ => return Err(format!("unknown query `{name}`")),
        })
    }
}

/// Neutral base for addresses implicitly created by an override (so releasing reverts).
pub fn zero_of(v: &Value) -> Value {
    match v {
        Value::Bool(_) => Value::Bool(false),
        Value::Int(_) => Value::Int(0),
        Value::Float(_) => Value::Float(0.0),
        Value::Str(_) => Value::Str(String::new()),
        Value::List(l) => Value::List(l.iter().map(zero_of).collect()),
        Value::Map(_) | Value::Null => Value::Null,
    }
}

fn prio_is_chat(p: u16) -> bool {
    p <= PRIORITY_CHAT
}

fn num(v: &toml::Value) -> Option<f64> {
    v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
}

/// Scenes ordered by number key, then name.
pub fn scene_order(c: &Config) -> Vec<String> {
    let mut v: Vec<(&String, &SceneDef)> = c.scenes.iter().collect();
    v.sort_by_key(|(n, s)| (s.key.unwrap_or(u8::MAX), (*n).clone()));
    v.into_iter().map(|(n, _)| n.clone()).collect()
}

fn first_scene(c: &Config) -> String {
    scene_order(c).into_iter().next().unwrap_or_default()
}

/// Runtime ownership keys outlive files in persisted state; never revive a deleted definition.
fn configured_owner(config: &Config, key: &str) -> bool {
    if let Some(name) = key.strip_prefix("preset:") {
        return config.presets.contains_key(name.split('#').next().unwrap_or(name));
    }
    if let Some(name) = key.strip_prefix("scene:") {
        return config.scenes.contains_key(name);
    }
    if let Some(name) = key.strip_prefix("rule:") {
        return config.rules.iter().any(|rule| rule.name == name);
    }
    true
}

/// Declare the addressable parameters of a scene's nodes (§4.3).
pub fn declare_scene(st: &mut StateTree, s: &SceneDef) {
    let base = format!("scene.{}", s.name);
    let mut shared_done: Vec<String> = Vec::new();
    for (canvas, c) in &s.canvas {
        for n in &c.nodes {
            let p = format!("{base}.node.{}", n.id);
            if !shared_done.contains(&n.id) {
                shared_done.push(n.id.clone());
                let set = |st: &mut StateTree, a: String, m: Meta, v: Value| {
                    let i = st.declare(&a, m);
                    if st.param(i).base != v {
                        st.set_base(i, v);
                    }
                };
                set(st, format!("{p}.opacity"), Meta::float(1.0, [0.0, 1.0]).owner("scene"), Value::Float(n.opacity as f64));
                set(st, format!("{p}.offset_x"), Meta::float(0.0, [-4000.0, 4000.0]).unit("px").owner("scene"), Value::Float(n.offset_x as f64));
                set(st, format!("{p}.offset_y"), Meta::float(0.0, [-4000.0, 4000.0]).unit("px").owner("scene"), Value::Float(n.offset_y as f64));
                set(st, format!("{p}.scale"), Meta::float(1.0, [0.01, 10.0]).owner("scene"), Value::Float(n.scale as f64));
                set(st, format!("{p}.rotation"), Meta::float(0.0, [-360.0, 360.0]).unit("deg").owner("scene"), Value::Float(n.rotation as f64));
                set(st, format!("{p}.visible"), Meta::boolean(true).owner("scene"), Value::Bool(n.visible));
                for fx in &n.fx {
                    let fp = format!("{p}.fx.{}", fx.name);
                    set(st, format!("{fp}.enabled"), Meta::boolean(true).owner("scene"), Value::Bool(fx.enabled.unwrap_or(true)));
                    for (k, v) in &fx.params {
                        let a = format!("{fp}.{k}");
                        let i = st.ensure(&a, v);
                        if st.param(i).base != *v {
                            st.set_base(i, v.clone());
                        }
                    }
                }
            }
            let set = |st: &mut StateTree, a: String, m: Meta, v: Value| {
                let i = st.declare(&a, m);
                if st.param(i).base != v {
                    st.set_base(i, v);
                }
            };
            set(st, format!("{p}.rect.{canvas}"), Meta::vec4([0.0, 0.0, 1.0, 1.0]).owner("scene"), Value::from(n.rect));
            set(st, format!("{p}.crop.{canvas}"), Meta::vec4([0.0; 4]).owner("scene"), Value::from(n.crop));
            set(st, format!("{p}.radius.{canvas}"), Meta::float(0.0, [0.0, 2000.0]).unit("px").owner("scene"), Value::Float(n.radius as f64));
            set(st, format!("{p}.z.{canvas}"), Meta::int(0, [-1000.0, 1000.0]).owner("scene"), Value::Int(n.z as i64));
        }
    }
}

/// The light cue a running preset owns (and releases when it ends). One-shot presets
/// (no hold/toggle/set) fire their cue and let it run its own course, so ending the
/// preset must not switch the look off again. A light look has no course of its own: it is
/// always held for as long as the preset runs.
fn held_lights(def: &PresetDef) -> Option<crate::config::LightsRef> {
    def.lights.clone().filter(|l| l.look.is_some() || l.hold.is_some() || def.hold.is_some() || def.toggle || !def.set.is_empty())
}

/// Keys of a preset `fx` entry that shape its trigger (timing, strength) rather than being
/// settings of the effect.
const EFFECT_TIMING: &[&str] = &["hold", "attack", "release", "level", "amount"];

/// A duration in a trigger payload: `"2s"`, `"500ms"`, or a number of milliseconds.
fn payload_ms(p: &Value, k: &str) -> Option<u64> {
    p.get_path(k).and_then(|v| match v {
        Value::Str(s) => se_proto::parse_duration_ms(s),
        v => v.as_f64().map(|f| f.max(0.0) as u64),
    })
}

/// The address pattern of a Set/Animate/Release that uses `*`.
fn wildcard_target(op: &Op) -> Option<&str> {
    match op {
        Op::Set { address, .. } | Op::Animate { address, .. } | Op::Release { address } if address.contains('*') => Some(address),
        _ => None,
    }
}

/// The same op aimed at one concrete address.
fn with_address(op: &Op, a: &str) -> Op {
    match op {
        Op::Set { value, .. } => Op::Set { address: a.into(), value: value.clone() },
        Op::Animate { to, ms, ease, .. } => Op::Animate { address: a.into(), to: to.clone(), ms: *ms, ease: *ease },
        Op::Release { .. } => Op::Release { address: a.into() },
        other => other.clone(),
    }
}
