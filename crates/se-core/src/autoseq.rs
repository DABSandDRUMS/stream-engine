//! Auto sequence presets (`autoseq/<id>.toml`): the program scene cycles through a list of
//! scenes on a timer, in order or at random, with a dwell time and a transition per step.
//!
//! * **One at a time:** program is single, so one preset runs (`show.autoseq.*`). Selecting
//!   another while running hands over to it.
//! * **Timer:** the dwell restarts whenever `show.scene.program` changes — a sequence switch
//!   or a manual cut. The next scene is picked when the dwell starts, so it can be shown, and
//!   the switch waits while a transition is still running.
//! * **Switch:** preview ← scene, then a take (as `scene.cut`) with the step's transition and
//!   duration (else the preset's, else the engine's normal pick).
//! * **Replay:** core time and the core RNG only; the switches are regenerated from the
//!   logged `autoseq.*` commands.

use super::{Core, Ctx, MS, addr};
use crate::config::{BUILTIN_TRANSITIONS, Config, ConfigError, Dur};
use se_proto::{Event, Id, Meta, Origin, Ts, Value, next_id};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Project kind (directory) of auto sequence presets.
pub const KIND: &str = "autoseq";

/// Selected preset id (`""` = none).
pub const PRESET: &str = "show.autoseq.preset";
/// Running.
pub const ACTIVE: &str = "show.autoseq.active";
/// Index of the program scene in the steps (-1: not a step, or stopped).
pub const STEP: &str = "show.autoseq.step";
/// Scene that comes next (`""` when stopped).
pub const NEXT: &str = "show.autoseq.next";
/// Core/master-clock ns of the next switch (same clock as `show.transition.start`; 0 when stopped).
pub const NEXT_AT: &str = "show.autoseq.next_at";
/// Length of the current dwell (ms).
pub const DWELL_MS: &str = "show.autoseq.dwell_ms";

const MIN_DWELL_MS: u64 = 1_000;

fn default_dwell() -> Dur {
    Dur(30_000)
}

fn default_avoid_repeat() -> usize {
    1
}

const MAX_HISTORY: usize = 32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    #[default]
    InOrder,
    Random,
}

impl Order {
    pub fn as_str(self) -> &'static str {
        match self {
            Order::InOrder => "in_order",
            Order::Random => "random",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutoSeqStep {
    /// Scene id (file stem in `scenes/`).
    pub scene: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dwell: Option<Dur>,
    /// Transition into this scene.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<Dur>,
}

/// One `autoseq/<id>.toml` file; `name` (the id) is the file stem, not a key in the file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutoSeqDef {
    #[serde(skip)]
    pub name: String,
    /// Display name (defaults to the id).
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub order: Order,
    /// Random mode excludes this many recently shown scenes, oldest first if exhausted.
    #[serde(default = "default_avoid_repeat")]
    pub avoid_repeat: usize,
    /// Default time on each scene.
    #[serde(default = "default_dwell")]
    pub dwell: Dur,
    /// Default transition into each scene (absent = the engine's normal pick).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<String>,
    /// Default transition duration (absent = the transition's own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<Dur>,
    #[serde(default, rename = "step")]
    pub steps: Vec<AutoSeqStep>,
}

impl AutoSeqDef {
    /// Parse + self-contained validation (>=1 step, dwell >= 1s, no duplicate scenes).
    pub fn parse(name: &str, t: &toml::Table) -> Result<AutoSeqDef, String> {
        let mut d: AutoSeqDef = toml::Value::Table(t.clone()).try_into().map_err(|e: toml::de::Error| e.message().to_string())?;
        d.name = name.to_string();
        if d.label.trim().is_empty() {
            d.label = name.to_string();
        }
        if d.steps.is_empty() {
            return Err("needs at least one [[step]]".into());
        }
        if d.dwell.ms() < MIN_DWELL_MS {
            return Err(format!("`dwell` must be at least 1s (is {})", String::from(d.dwell)));
        }
        if d.avoid_repeat > MAX_HISTORY {
            return Err(format!("`avoid_repeat` must be at most {MAX_HISTORY}"));
        }
        let mut seen = BTreeSet::new();
        for (i, s) in d.steps.iter().enumerate() {
            if s.scene.trim().is_empty() {
                return Err(format!("step {} has no `scene`", i + 1));
            }
            if let Some(dw) = s.dwell.filter(|dw| dw.ms() < MIN_DWELL_MS) {
                return Err(format!("step `{}`: `dwell` must be at least 1s (is {})", s.scene, String::from(dw)));
            }
            if !seen.insert(s.scene.as_str()) {
                return Err(format!("scene `{}` is in the sequence twice", s.scene));
            }
        }
        Ok(d)
    }

    /// File text for this def (what the UI writes via `project.write {path, text}`).
    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("an auto sequence always serializes to TOML")
    }

    /// Time on step `i`: its own `dwell`, else the preset's.
    pub fn step_dwell(&self, i: usize) -> Dur {
        self.steps.get(i).and_then(|s| s.dwell).unwrap_or(self.dwell)
    }
}

/// Parse all auto sequence presets of a configuration. Broken files are left out (one error
/// each); unknown scene/transition references are reported but keep the preset.
pub fn parse_all(config: &Config) -> (BTreeMap<String, AutoSeqDef>, Vec<ConfigError>) {
    let mut defs = BTreeMap::new();
    let mut errs = Vec::new();
    let known_transition = |n: &str| config.transitions.contains_key(n) || BUILTIN_TRANSITIONS.contains(&n);
    for (name, t) in config.other.get(KIND).into_iter().flatten() {
        let file = config.files.get(&format!("{KIND}/{name}")).cloned().unwrap_or_else(|| format!("{KIND}/{name}.toml"));
        match AutoSeqDef::parse(name, t) {
            Ok(d) => {
                let mut err = |msg: String| errs.push(ConfigError { file: file.clone(), msg });
                if let Some(n) = d.transition.as_deref().filter(|n| !known_transition(n)) {
                    err(format!("unknown transition `{n}`"));
                }
                for s in &d.steps {
                    if !config.scenes.contains_key(&s.scene) {
                        err(format!("unknown scene `{}`", s.scene));
                    }
                    if let Some(n) = s.transition.as_deref().filter(|n| !known_transition(n)) {
                        err(format!("step `{}`: unknown transition `{n}`", s.scene));
                    }
                }
                defs.insert(name.clone(), d);
            }
            Err(msg) => errs.push(ConfigError { file, msg }),
        }
    }
    (defs, errs)
}

/// Actions handled inside the core.
pub fn is_action(name: &str) -> bool {
    matches!(name, "autoseq.play" | "autoseq.stop" | "autoseq.toggle" | "autoseq.select" | "autoseq.next")
}

/// Steps whose scene exists (a deleted scene is skipped).
fn playable(def: &AutoSeqDef, config: &Config) -> Vec<usize> {
    def.steps.iter().enumerate().filter(|(_, s)| config.scenes.contains_key(&s.scene)).map(|(i, _)| i).collect()
}

// ---- runtime ------------------------------------------------------------------------------

/// Persisted auto sequence state (crash recovery).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PersistedAutoSeq {
    /// Selected preset (`""` = none).
    pub preset: String,
    pub active: bool,
    /// Last step the sequence (or a cut) put on program.
    pub last: Option<usize>,
    #[serde(default)]
    pub recent: Vec<String>,
}

#[derive(Default)]
pub struct AutoSeq {
    defs: BTreeMap<String, AutoSeqDef>,
    selected: String,
    active: bool,
    /// Last step shown on program (in order continues after it from a scene outside the list).
    last: Option<usize>,
    /// Program scene the current dwell belongs to.
    program: String,
    /// Scene names rather than step indices, so reordering a preset keeps its history valid.
    recent: Vec<String>,
    /// End of the current dwell (core ns) and its length.
    due: Ts,
    dwell_ms: u64,
    /// Step that comes next.
    next: Option<usize>,
    /// Definitions or scenes changed: pick the next step again (the dwell keeps running).
    replan: bool,
}

impl Core {
    pub(super) fn declare_autoseq_state(&mut self) {
        let ro = |m: Meta, d: &str| m.readonly().owner("autoseq").describe(d);
        self.state.declare(PRESET, ro(Meta::string(""), "Selected auto sequence preset"));
        self.state.declare(ACTIVE, ro(Meta::boolean(false), "Auto sequence is running"));
        self.state.declare(STEP, ro(Meta::int(-1, [-1.0, 1e6]), "Step of the program scene (-1: not a step, or stopped)"));
        self.state.declare(NEXT, ro(Meta::string(""), "Scene the auto sequence switches to next"));
        self.state.declare(NEXT_AT, ro(Meta::int(0, [0.0, 9.2e18]).unit("ns"), "Master-clock time of the next switch (0 when stopped)"));
        self.state.declare(DWELL_MS, ro(Meta::int(0, [0.0, 1e12]).unit("ms"), "Length of the current dwell"));
    }

    /// (Re)load auto sequence presets; a broken file keeps its last good version. A deleted
    /// selected preset stops the sequence (reason `removed`).
    pub(super) fn configure_autoseq(&mut self, config: &Config) -> Vec<ConfigError> {
        let (mut defs, errs) = parse_all(config);
        for e in &errs {
            self.log("error", format!("{}: {}", e.file, e.msg));
        }
        for name in config.other.get(KIND).into_iter().flat_map(|m| m.keys()) {
            if !defs.contains_key(name)
                && let Some(d) = self.autoseq.defs.get(name)
            {
                defs.insert(name.clone(), d.clone());
            }
        }
        self.autoseq.defs = defs;
        if !self.autoseq.selected.is_empty() && !self.autoseq.defs.contains_key(&self.autoseq.selected) {
            self.autoseq_stop("removed", None);
            self.autoseq.selected.clear();
            self.autoseq.last = None;
            self.autoseq.recent.clear();
            self.runtime_dirty = true;
        }
        // scenes or steps may have changed: re-pick the next step once the new config is live
        self.autoseq.replan = self.autoseq.active;
        self.publish_autoseq();
        errs
    }

    /// `autoseq.play|stop|toggle|select|next` (`name` or the first positional argument).
    pub(super) fn autoseq_action(&mut self, name: &str, args: &Value, ctx: &Ctx) -> Result<(), String> {
        let target = args
            .get_path("name")
            .or_else(|| args.get_path("args.0"))
            .map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))
            .filter(|s| !s.is_empty());
        match name {
            "autoseq.play" => self.autoseq_play(target, ctx.parent),
            "autoseq.stop" => {
                self.autoseq_stop("stop", ctx.parent);
                Ok(())
            }
            "autoseq.toggle" => {
                if self.autoseq.active {
                    self.autoseq_stop("stop", ctx.parent);
                    Ok(())
                } else {
                    self.autoseq_play(target, ctx.parent)
                }
            }
            "autoseq.select" => {
                let n = target.ok_or("`autoseq.select` needs a preset name")?;
                if !self.autoseq.defs.contains_key(&n) {
                    return Err(format!("unknown auto sequence `{n}`"));
                }
                if self.autoseq.active {
                    return self.autoseq_play(Some(n), ctx.parent);
                }
                if self.autoseq.selected != n {
                    self.autoseq.selected = n;
                    self.autoseq.last = None;
                    self.autoseq.recent.clear();
                    self.runtime_dirty = true;
                }
                self.publish_autoseq();
                Ok(())
            }
            "autoseq.next" => {
                if !self.autoseq.active {
                    return Err("the auto sequence is not running".into());
                }
                let program = self.program_scene();
                match self.autoseq_next_scene() {
                    Some((i, scene)) if scene != program => self.autoseq_switch(i, ctx.parent),
                    _ => Err("no other scene to switch to".into()),
                }
            }
            _ => Err(format!("unknown action `{name}`")),
        }
    }

    fn autoseq_play(&mut self, name: Option<String>, parent: Option<Id>) -> Result<(), String> {
        let name = name.unwrap_or_else(|| self.autoseq.selected.clone());
        if name.is_empty() {
            return Err("no auto sequence selected".into());
        }
        let def = self.autoseq.defs.get(&name).cloned().ok_or_else(|| format!("unknown auto sequence `{name}`"))?;
        let playable = playable(&def, &self.config);
        if playable.is_empty() {
            return Err(format!("none of the scenes of auto sequence `{name}` exist"));
        }
        let already_running = self.autoseq.active && self.autoseq.selected == name;
        if self.autoseq.selected != name {
            self.autoseq.last = None;
            self.autoseq.recent.clear();
        }
        self.autoseq.selected = name.clone();
        self.autoseq.active = true;
        self.runtime_dirty = true;
        if !already_running {
            self.autoseq_event("autoseq.started", Value::map().with("preset", name), parent);
        }
        let program = self.program_scene();
        let step = def.steps.iter().position(|s| s.scene == program);
        if let Some(i) = step {
            if already_running {
                self.autoseq_plan(true);
                return Ok(());
            }
            self.autoseq.last = Some(i);
            self.autoseq_remember(&program);
        }
        let i = match def.order {
            Order::InOrder => step
                .and_then(|i| (1..=def.steps.len()).map(|k| (i + k) % def.steps.len()).find(|j| playable.contains(j)))
                .unwrap_or(playable[0]),
            Order::Random => self.autoseq_random(&def, &playable, step).expect("at least one playable scene"),
        };
        if def.steps[i].scene == program {
            self.autoseq_plan(true);
            return Ok(());
        }
        if let Err(e) = self.autoseq_switch(i, parent) {
            self.autoseq_plan(true);
            return Err(e);
        }
        Ok(())
    }

    fn autoseq_stop(&mut self, reason: &str, parent: Option<Id>) {
        if !self.autoseq.active {
            return;
        }
        self.autoseq.active = false;
        self.autoseq.next = None;
        self.autoseq.replan = false;
        let preset = self.autoseq.selected.clone();
        self.autoseq_event("autoseq.stopped", Value::map().with("preset", preset).with("reason", reason), parent);
        self.publish_autoseq();
        self.runtime_dirty = true;
    }

    /// Panic: the sequence stops (program stays where it is).
    pub(super) fn autoseq_panic(&mut self, parent: Option<Id>) {
        self.autoseq_stop("panic", parent);
    }

    /// One pass (every tick, after timelines): follow program changes, switch when due.
    pub(super) fn tick_autoseq(&mut self, now: Ts) {
        if !self.autoseq.active {
            return;
        }
        if self.state.get(addr::PROGRAM).and_then(Value::as_str).unwrap_or("") != self.autoseq.program {
            self.autoseq_plan(true);
        } else if self.autoseq.replan {
            self.autoseq_plan(false);
        }
        if now < self.autoseq.due || self.transition.is_some() {
            return;
        }
        let program = self.program_scene();
        match self.autoseq_next_scene() {
            Some((i, scene)) if scene != program => {
                if let Err(e) = self.autoseq_switch(i, None) {
                    self.trace.add(next_id(), None, now, "error", format!("auto sequence: {e}"));
                    self.autoseq_plan(true);
                }
            }
            // nothing else to show (a single step, or no scene left): start another dwell
            _ => self.autoseq_plan(true),
        }
    }

    /// Put step `i` on program (preview, then a take as `scene.cut` does) and start its dwell.
    fn autoseq_switch(&mut self, i: usize, parent: Option<Id>) -> Result<(), String> {
        let def = self.autoseq.defs.get(&self.autoseq.selected).cloned().ok_or("no auto sequence selected")?;
        let step = def.steps.get(i).cloned().ok_or_else(|| format!("auto sequence `{}` has no step {i}", def.name))?;
        let transition = step.transition.clone().or_else(|| def.transition.clone());
        let ms = step.ms.or(def.ms).map(|d| d.ms().min(u32::MAX as u64) as u32);
        let id = next_id();
        self.trace.add(id, parent, self.now(), "autoseq", format!("{} → {} (step {})", def.name, step.scene, i + 1));
        self.set_sys(addr::PREVIEW, Value::Str(step.scene.clone()));
        self.take(transition, ms, se_proto::Origin::Rule, &Ctx { parent: Some(id), ..Default::default() })?;
        self.autoseq.last = Some(i);
        let used = self.state.get(addr::TR_NAME).cloned().unwrap_or_default();
        let payload = Value::map().with("preset", def.name.clone()).with("scene", step.scene).with("step", i as i64).with("transition", used);
        self.autoseq_event("autoseq.step", payload, Some(id));
        self.autoseq_plan(true);
        self.runtime_dirty = true;
        Ok(())
    }

    /// Start a fresh dwell on the program scene (`restart`, or when program moved since the
    /// last plan) and pick the step that comes next.
    fn autoseq_plan(&mut self, restart: bool) {
        self.autoseq.replan = false;
        let Some(def) = self.autoseq.defs.get(&self.autoseq.selected).cloned() else { return };
        let program = self.program_scene();
        let step = def.steps.iter().position(|s| s.scene == program);
        if step.is_some() {
            self.autoseq.last = step;
        }
        let fresh = restart || self.autoseq.program != program;
        if fresh {
            self.autoseq_remember(&program);
            let dwell = step.map_or(def.dwell, |i| def.step_dwell(i)).ms();
            self.autoseq.dwell_ms = dwell;
            self.autoseq.due = self.now() + dwell * MS;
            self.autoseq.program = program;
        }
        let playable = playable(&def, &self.config);
        self.autoseq.next = match def.order {
            Order::InOrder => {
                let n = def.steps.len();
                match step.or(self.autoseq.last) {
                    Some(i) => (1..=n).map(|k| (i + k) % n).find(|j| playable.contains(j)),
                    None => playable.first().copied(),
                }
            }
            Order::Random => {
                // Reloading does not change an already promised next scene if it is still
                // playable and respects the same recent-scene exclusion.
                if !fresh && self.autoseq.next.is_some_and(|j| self.autoseq_random_allowed(&def, &playable, step, j)) {
                    self.autoseq.next
                } else {
                    self.autoseq_random(&def, &playable, step)
                }
            }
        };
        self.publish_autoseq();
    }

    fn autoseq_remember(&mut self, program: &str) {
        if self.autoseq.recent.last().is_some_and(|scene| scene == program) {
            return;
        }
        if self.autoseq.recent.len() == MAX_HISTORY {
            self.autoseq.recent.remove(0);
        }
        self.autoseq.recent.push(program.to_string());
        self.runtime_dirty = true;
    }

    fn autoseq_random_window(&self, def: &AutoSeqDef, playable: &[usize], step: Option<usize>) -> usize {
        let mut avoid = def.avoid_repeat.min(self.autoseq.recent.len());
        loop {
            let recent = &self.autoseq.recent[self.autoseq.recent.len() - avoid..];
            if avoid == 0 || playable.iter().any(|&j| {
                (playable.len() < 2 || Some(j) != step) && !recent.contains(&def.steps[j].scene)
            }) {
                return avoid;
            }
            avoid -= 1;
        }
    }

    fn autoseq_random_allowed(&self, def: &AutoSeqDef, playable: &[usize], step: Option<usize>, j: usize) -> bool {
        let avoid = self.autoseq_random_window(def, playable, step);
        playable.contains(&j) && (playable.len() < 2 || Some(j) != step)
            && !self.autoseq.recent[self.autoseq.recent.len() - avoid..].contains(&def.steps[j].scene)
    }

    fn autoseq_random(&mut self, def: &AutoSeqDef, playable: &[usize], step: Option<usize>) -> Option<usize> {
        let avoid = self.autoseq_random_window(def, playable, step);
        let recent = &self.autoseq.recent[self.autoseq.recent.len() - avoid..];
        let allowed = |j: &usize| (playable.len() < 2 || Some(*j) != step) && !recent.contains(&def.steps[*j].scene);
        let count = playable.iter().copied().filter(allowed).count();
        if count == 0 {
            None
        } else {
            let pick = self.rng.below(count as u64) as usize;
            playable.iter().copied().filter(allowed).nth(pick)
        }
    }

    fn autoseq_next_scene(&self) -> Option<(usize, String)> {
        let i = self.autoseq.next?;
        let scene = self.autoseq.defs.get(&self.autoseq.selected)?.steps.get(i)?.scene.clone();
        Some((i, scene))
    }

    fn program_scene(&self) -> String {
        self.state.get(addr::PROGRAM).and_then(Value::as_str).unwrap_or("").to_string()
    }

    fn autoseq_event(&mut self, ty: &str, payload: Value, parent: Option<Id>) {
        let e = Event::new(ty, Origin::System, payload).with_causal(parent);
        self.events.push_back((e, Ctx { parent, ..Default::default() }));
    }

    fn publish_autoseq(&mut self) {
        let a = &self.autoseq;
        let program = self.program_scene();
        let def = a.defs.get(&a.selected).filter(|_| a.active);
        let step = def.and_then(|d| d.steps.iter().position(|s| s.scene == program)).map_or(-1, |i| i as i64);
        let next = def.zip(a.next).and_then(|(d, i)| d.steps.get(i)).map(|s| s.scene.clone());
        let vals = [
            (PRESET, Value::Str(a.selected.clone())),
            (ACTIVE, Value::Bool(a.active)),
            (STEP, Value::Int(step)),
            (NEXT_AT, Value::Int(if next.is_some() { a.due as i64 } else { 0 })),
            (NEXT, Value::Str(next.unwrap_or_default())),
            (DWELL_MS, Value::Int(if a.active { a.dwell_ms as i64 } else { 0 })),
        ];
        for (k, v) in vals {
            self.set_sys(k, v);
        }
    }

    pub(super) fn autoseq_persist(&self) -> PersistedAutoSeq {
        PersistedAutoSeq { preset: self.autoseq.selected.clone(), active: self.autoseq.active, last: self.autoseq.last, recent: self.autoseq.recent.clone() }
    }

    /// Restore after a restart: an active sequence resumes with a fresh dwell from now.
    pub(super) fn autoseq_restore(&mut self, p: &PersistedAutoSeq) {
        let Some(def) = self.autoseq.defs.get(&p.preset) else { return };
        let steps = def.steps.len();
        let runnable = !playable(def, &self.config).is_empty();
        self.autoseq.selected = p.preset.clone();
        self.autoseq.last = p.last.filter(|i| *i < steps);
        self.autoseq.recent = p.recent.iter().rev().take(MAX_HISTORY).cloned().collect();
        self.autoseq.recent.reverse();
        self.autoseq.active = p.active && runnable;
        if self.autoseq.active {
            self.autoseq_plan(true);
        } else {
            self.publish_autoseq();
        }
    }

    /// `autoseq` query: every preset, sorted by name.
    pub(super) fn query_autoseq(&self) -> Value {
        let ms = |d: Option<Dur>| d.map(|d| Value::Int(d.ms() as i64)).unwrap_or_default();
        let text = |t: &Option<String>| t.clone().map(Value::Str).unwrap_or_default();
        Value::List(
            self.autoseq
                .defs
                .values()
                .map(|d| {
                    let steps = d
                        .steps
                        .iter()
                        .map(|s| {
                            Value::map()
                                .with("scene", s.scene.clone())
                                .with("dwell_ms", ms(s.dwell))
                                .with("transition", text(&s.transition))
                                .with("ms", ms(s.ms))
                        })
                        .collect();
                    Value::map()
                        .with("name", d.name.clone())
                        .with("label", d.label.clone())
                        .with("order", d.order.as_str())
                        .with("avoid_repeat", d.avoid_repeat as i64)
                        .with("dwell_ms", d.dwell.ms() as i64)
                        .with("transition", text(&d.transition))
                        .with("ms", ms(d.ms))
                        .with("steps", Value::List(steps))
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(src: &str) -> Result<AutoSeqDef, String> {
        AutoSeqDef::parse("drums", &toml::from_str(src).unwrap())
    }

    #[test]
    fn defaults_and_overrides() {
        let d = def("[[step]]\nscene = \"a\"\n[[step]]\nscene = \"b\"\ndwell = \"60s\"\ntransition = \"morph\"\nms = \"1200ms\"").unwrap();
        assert_eq!((d.name.as_str(), d.label.as_str(), d.order), ("drums", "drums", Order::InOrder));
        assert_eq!((d.dwell, d.transition.clone(), d.ms), (Dur(30_000), None, None));
        assert_eq!((d.step_dwell(0), d.step_dwell(1)), (Dur(30_000), Dur(60_000)));
        assert_eq!((d.steps[1].transition.as_deref(), d.steps[1].ms), (Some("morph"), Some(Dur(1_200))));
    }

    #[test]
    fn rejects_bad_files() {
        assert!(def("order = \"random\"").unwrap_err().contains("at least one"));
        assert!(def("[[step]]\nscene = \"a\"\n[[step]]\nscene = \"a\"").unwrap_err().contains("twice"));
        assert!(def("dwell = \"500ms\"\n[[step]]\nscene = \"a\"").unwrap_err().contains("at least 1s"));
        assert!(def("[[step]]\nscene = \"a\"\ndwell = \"999ms\"").unwrap_err().contains("at least 1s"));
        assert!(def("order = \"shuffle\"\n[[step]]\nscene = \"a\"").is_err());
        assert!(def("[[step]]\ndwell = \"5s\"").is_err(), "step without scene");
    }

    #[test]
    fn to_toml_round_trips() {
        let src = "label = \"Drum cycle\"\norder = \"random\"\ndwell = \"45s\"\ntransition = \"fade\"\nms = \"800ms\"\n\n[[step]]\nscene = \"drum_cams\"\ndwell = \"60s\"\ntransition = \"morph\"\nms = \"1200ms\"\n\n[[step]]\nscene = \"wide\"\n";
        let d = def(src).unwrap();
        let text = d.to_toml();
        assert_eq!(def(&text).unwrap(), d, "{text}");
        assert!(text.contains("[[step]]") && text.contains("dwell = \"45s\"") && text.contains("order = \"random\""), "{text}");
        assert!(!text.contains("name"), "the id is the file name: {text}");
    }
}
