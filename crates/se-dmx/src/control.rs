//! The lights control task: config (hot reload with last-good fallback), declarations,
//! actions (`lights.*`), the `lights.flash` event, playback timers, playback masters and fader
//! start, held looks (palettes run as one-cue playbacks), live palette following, health
//! (`health.dmx`), persistence, and UI queries.

use crate::cuelist::{self, CueList};
use crate::engine::Plan;
use crate::foundation::{self, Controls, Selection, Slot};
use crate::knobs;
use crate::output::{self, Monitor, Shared};
use crate::playback::{self, Nav, Playback, Timer};
use crate::programmer::{self, Programmer};
use crate::show::Show;
use parking_lot::RwLock;
use se_hub::{Bus, EngineCtx, Hub};
use se_proto::{Actor, Command, Event, Id, Op, Origin, PRIORITY_CHAT, PRIORITY_MANUAL, PRIORITY_PRESET, Value};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Cue list used as the safe look by Panic (falls back to `[safety] safe_look`).
const SAFE_LIST: &str = "safe";

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;

/// Minimum spacing of master re-scaling commands per playback.
const MASTER_THROTTLE: Duration = Duration::from_millis(33);

/// Snapshot of control state for queries.
#[derive(Clone)]
pub struct PbView {
    pub current: Option<usize>,
    pub go_at: Instant,
    pub total_ms: u64,
    pub priority: u16,
    pub master: f32,
}

pub struct View {
    pub show: Arc<Show>,
    pub plan_errors: Vec<String>,
    pub playbacks: BTreeMap<String, PbView>,
    /// Held looks (palette name → priority).
    pub looks: BTreeMap<String, u16>,
    pub prog: Programmer,
    pub layers: Value,
    pub layer_expiry: BTreeMap<Slot, Instant>,
}

/// A held look: a palette run as a one-cue playback. The synthetic list is built here on the
/// control task (never on the output thread) and rebuilt when the palette changes.
struct Look {
    pb: Playback,
    list: CueList,
}

/// Restart intention, never a serialized playback or its override generations.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaybackIntent {
    cuelist: String,
    cue: String,
    origin: Origin,
    priority: u16,
}

fn manual_playback(origin: Origin, priority: u16) -> bool {
    origin.default_priority() >= PRIORITY_MANUAL && priority > PRIORITY_CHAT
}

struct Layer {
    selection: Selection,
    list: CueList,
    pb: Playback,
    generation: u64,
    expires: Option<Instant>,
    release_ms: u64,
    source_owner: Option<String>,
}

struct PendingLayer {
    selection: Selection,
    controls: Controls,
    generation: u64,
    boundary: f64,
    priority: u16,
    origin: Origin,
    actor: Option<Actor>,
    cause: Option<Id>,
    fade: Option<u64>,
    duration_ms: Option<u64>,
    release_ms: u64,
    source_owner: Option<String>,
    reserved_effects: BTreeSet<String>,
}

pub struct Ctl {
    pub hub: Arc<Hub>,
    ctx: EngineCtx,
    pub show: Arc<Show>,
    shared: Arc<Shared>,
    view: Arc<RwLock<View>>,
    playbacks: BTreeMap<String, Playback>,
    /// Lists explicitly navigated by an operator, not inherited preset command origins.
    operator_playbacks: BTreeSet<String>,
    /// Running looks by palette name.
    looks: BTreeMap<String, Look>,
    layers: BTreeMap<Slot, Layer>,
    pending_layers: BTreeMap<Slot, PendingLayer>,
    layer_seq: u64,
    clock: crate::effects::BeatClock,
    clock_at: Instant,
    prog: Programmer,
    timers: BinaryHeap<Reverse<(Instant, u64)>>,
    timer_data: HashMap<u64, Timer>,
    timer_seq: u64,
    tokens: u64,
    prefixes: BTreeSet<String>,
    masters: HashMap<String, f32>,
    master_pending: BTreeSet<String>,
    flash_token: u64,
    rdm_started: bool,
    rdm_logged: i64,
    health: Value,
    published: HashMap<String, Value>,
    persisted: Value,
    second: u64,
    /// Live sources (stream palette / `@address`) running playbacks follow.
    live: std::collections::HashSet<String>,
    /// After a panic the safe look is latched: only an operator (manual priority) may release
    /// it, never automation (a preset ending, a rule, a timeline).
    panic_latched: bool,
    /// Recent `lights.layer.pick` choices per layer (keys `palette:<n>` / `cuelist:<n>`, newest first).
    picks: BTreeMap<Slot, VecDeque<String>>,
    /// xorshift64 state breaking ties between equally good picks.
    pick_rng: u64,
}

fn arg<'a>(args: &'a Value, name: &str, pos: usize) -> Option<&'a Value> {
    args.get_path(name).filter(|v| !v.is_null()).or_else(|| args.get_path("args").and_then(Value::as_list).and_then(|l| l.get(pos)))
}

fn arg_str(args: &Value, name: &str, pos: usize) -> Option<String> {
    arg(args, name, pos).map(|v| match v {
        Value::Str(s) => s.clone(),
        other => other.to_string(),
    })
}

/// `fade`: number = ms, or a duration string.
fn arg_ms(args: &Value, name: &str) -> Result<Option<u64>, String> {
    match args.get_path(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Int(i)) if *i >= 0 => Ok(Some(*i as u64)),
        Some(Value::Float(f)) if f.is_finite() && *f >= 0.0 && *f <= u64::MAX as f64 => Ok(Some(f.round() as u64)),
        Some(Value::Str(s)) => se_proto::parse_duration_ms(s).map(Some).ok_or_else(|| format!("bad duration `{s}`")),
        Some(o) => Err(format!("bad duration `{o}`")),
    }
}

/// Effective priority from a caller's explicit `priority`, the item's own floor, and whether
/// the request came from chat (always clamped to chat priority).
fn effective_priority(own: Option<u16>, chat: bool, explicit: Option<u16>) -> u16 {
    let p = match (explicit, own) {
        (Some(e), Some(o)) => e.max(o),
        (Some(e), None) => e,
        (None, o) => o.unwrap_or(PRIORITY_PRESET),
    };
    if chat { p.min(PRIORITY_CHAT) } else { p }
}

impl Ctl {
    fn get(&self) -> impl Fn(&str) -> Option<Value> + use<> {
        let snap = self.hub.snapshot.load_full();
        move |a: &str| snap.get(a).cloned()
    }

    fn submit(&self, cmds: Vec<Command>) {
        for c in cmds {
            self.hub.command(c);
        }
    }

    fn arm(&mut self, at: Instant, t: Timer) {
        self.timer_seq += 1;
        self.timers.push(Reverse((at, self.timer_seq)));
        self.timer_data.insert(self.timer_seq, t);
    }

    fn out(&mut self, o: playback::Out) {
        for e in &o.errors {
            self.hub.log("warn", "lights", e.clone());
        }
        self.submit(o.cmds);
        for (at, mut t) in o.timers {
            if let Timer::BeatAdvance { beats, boundary, .. } = &mut t {
                if boundary.is_none() { *boundary = Some(self.beat_position() + *beats); }
            }
            self.arm(at, t);
        }
    }

    fn publish(&mut self, addr: &str, v: Value) {
        if self.published.get(addr) != Some(&v) {
            self.hub.publish(addr, v.clone());
            self.published.insert(addr.to_string(), v);
        }
    }

    fn sync_view(&mut self) {
        let mut v = self.view.write();
        v.show = self.show.clone();
        v.playbacks = self
            .playbacks
            .iter()
            .map(|(n, p)| (n.clone(), PbView { current: p.current, go_at: p.go_at, total_ms: p.total_ms, priority: p.priority, master: p.master }))
            .collect();
        v.looks = self.looks.iter().map(|(n, l)| (n.clone(), l.pb.priority)).collect();
        v.prog = self.prog.clone();
        v.layers = self.layer_status();
        v.layer_expiry = self.layers.iter().filter_map(|(slot, l)| Some((*slot, l.expires?))).collect();
    }

    /// The playback a timer refers to (cue list name or [`playback::look_id`]).
    fn running_mut(&mut self, id: &str) -> Option<&mut Playback> {
        if id.starts_with("layer:") {
            return self.layers.values_mut().find(|l| l.pb.list == id).map(|l| &mut l.pb);
        }
        match id.strip_prefix("look:") {
            Some(n) => self.looks.get_mut(n).map(|l| &mut l.pb),
            None => self.playbacks.get_mut(id),
        }
    }

    fn publish_playbacks(&mut self) {
        self.live = self.playbacks.values().chain(self.looks.values().map(|l| &l.pb)).chain(self.layers.values().map(|l| &l.pb)).flat_map(|p| p.applied.values().filter_map(|a| a.live.clone())).collect();
        let names: Vec<String> = self.show.lists.keys().cloned().collect();
        for n in names {
            let (cue, next, playing) = match self.playbacks.get(&n).and_then(|p| p.current.map(|c| (p, c))) {
                Some((_, c)) => {
                    let l = &self.show.lists[&n];
                    let next = if c + 1 < l.cues.len() {
                        l.cues[c + 1].id.clone()
                    } else if l.looped {
                        l.cues.first().map(|x| x.id.clone()).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    (l.cues.get(c).map(|x| x.id.clone()).unwrap_or_default(), next, true)
                }
                None => (String::new(), self.show.lists[&n].cues.first().map(|x| x.id.clone()).unwrap_or_default(), false),
            };
            self.publish(&format!("lights.cuelist.{n}.cue"), Value::Str(cue));
            self.publish(&format!("lights.cuelist.{n}.next"), Value::Str(next));
            self.publish(&format!("lights.cuelist.{n}.playing"), Value::Bool(playing));
        }
        let status = self.layer_status();
        for slot in Slot::ALL {
            self.publish(&format!("lights.layer.{}.state", slot.name()), status.get_path(slot.name()).cloned().unwrap_or(Value::Null));
        }
        self.publish("lights.panic_latched", Value::Bool(self.panic_latched));
        self.persist();
        self.sync_view();
    }

    fn publish_programmer(&mut self) {
        self.publish("lights.programmer.selection", Value::List(self.prog.selection.iter().cloned().map(Value::Str).collect()));
        self.publish("lights.programmer.highlight", Value::Bool(!self.prog.highlight.is_empty()));
        self.publish("lights.programmer.active", Value::Bool(!self.prog.values.is_empty()));
        self.sync_view();
    }

    /// Keep only running advanced operator cue-list intentions, plus the safety latch.
    fn persist(&mut self) {
        let playbacks = Value::List(self.playbacks.values().filter_map(|p| {
            if self.panic_latched || !self.operator_playbacks.contains(&p.list) || !manual_playback(p.origin, p.priority) { return None; }
            let cue = self.show.lists.get(&p.list)?.cues.get(p.current?)?;
            Some(Value::map().with("cuelist", p.list.clone()).with("cue", cue.id.clone())
                .with("origin", p.origin.as_str()).with("priority", p.priority as i64))
        }).collect());
        let v = Value::map().with("playbacks", playbacks).with("panic_latched", self.panic_latched);
        if v != self.persisted {
            match self.ctx.db.kv_set("lights", "playbacks", v.get_path("playbacks").expect("intention list"))
                .and_then(|_| self.ctx.db.kv_set("lights", "panic_latched", &Value::Bool(self.panic_latched))) {
                Ok(()) => self.persisted = v,
                Err(e) => self.hub.log("warn", "lights", format!("persisting operator playback intentions: {e}")),
            }
        }
    }

    fn restore_playbacks(&mut self, saved: Value) {
        // Legacy maps held playback internals; only the intention list is accepted.
        if !self.panic_latched && let Value::List(intents) = saved {
            for value in intents {
                let Ok(intent) = serde_json::from_value::<PlaybackIntent>((&value).into()) else { continue };
                if !manual_playback(intent.origin, intent.priority) || self.playbacks.contains_key(&intent.cuelist) { continue; }
                let Some(list) = self.show.lists.get(&intent.cuelist).cloned() else { continue };
                let Some(k) = list.index_of(&intent.cue) else { continue };
                let priority = self.priority(&intent.cuelist, false, Some(intent.priority));
                let mut pb = Playback::new(&intent.cuelist, priority, intent.origin, None, None, self.master_of(&intent.cuelist));
                let get = self.get();
                let out = pb.goto(&list, k, Nav::Goto, Some(0), &self.show, &get, &mut self.tokens, Instant::now());
                self.operator_playbacks.insert(intent.cuelist.clone());
                self.playbacks.insert(intent.cuelist, pb);
                self.out(out);
            }
        }
        self.publish_playbacks();
    }

    // ---- config ------------------------------------------------------------------------

    fn declare(&mut self) {
        for (a, m) in self.show.declarations() {
            self.hub.declare(&a, m);
        }
        let now: BTreeSet<String> = self.show.prefixes().into_iter().collect();
        for stale in self.prefixes.difference(&now) {
            self.hub.submit(se_core::Input::Remove { prefix: stale.clone() });
        }
        self.prefixes = now;
    }

    pub fn reload(&mut self) {
        let cfg = self.ctx.config.borrow().clone();
        let show = Arc::new(Show::load(&cfg, &self.show));
        for e in &show.errors {
            if !self.show.errors.contains(e) {
                self.hub.log("error", "lights", e.clone());
            }
        }
        // Preserve the held cue by ID across reorderings; deleted cues stop immediately.
        let gone: Vec<String> = self.playbacks.iter_mut().filter_map(|(name, pb)| {
            let cue = self.show.lists.get(name)?.cues.get(pb.current?)?;
            match show.lists.get(name).and_then(|list| list.index_of(&cue.id)) {
                Some(k) => { pb.current = Some(k); None }
                None => Some(name.clone()),
            }
        }).collect();
        for name in gone {
            self.release_list(&name, Some(0), None);
        }
        self.show = show;
        let (plan, errs) = Plan::build(self.show.rig.clone(), self.show.effects.clone());
        for e in &errs {
            self.hub.log("error", "lights", e.clone());
        }
        self.shared.plan.store(Arc::new(plan));
        self.view.write().plan_errors = errs;
        self.declare();
        // drop playbacks whose list vanished; refresh running cues (palette/cue edits)
        let gone: Vec<String> = self.playbacks.keys().filter(|n| !self.show.lists.contains_key(*n)).cloned().collect();
        for n in gone {
            if let Some(mut p) = self.playbacks.remove(&n) {
                let o = p.release(0, Instant::now());
                self.out(o);
            }
        }
        self.refresh_running();
    }

    /// Running cue lists and looks follow edits of their files (and knob moves) with a short
    /// crossfade.
    fn refresh_running(&mut self) {
        let get = self.get();
        let now = Instant::now();
        let show = self.show.clone();
        let tokens = &mut self.tokens;
        let outs: Vec<playback::Out> =
            self.playbacks.values_mut().filter_map(|p| Some(p.refresh(show.lists.get(&p.list)?, &show, None, &get, tokens, now))).collect();
        for o in outs {
            self.out(o);
        }
        self.reload_looks(&get, now);
        self.refresh_layers(&get, now);
        self.publish_playbacks();
    }

    /// Running looks follow their palette: values re-resolve with a short crossfade, a palette
    /// that now covers other heads/attributes re-applies the rebuilt look, and a deleted palette
    /// releases it.
    fn reload_looks(&mut self, get: &dyn Fn(&str) -> Option<Value>, now: Instant) {
        let show = self.show.clone();
        let names: Vec<String> = self.looks.keys().cloned().collect();
        for n in names {
            let Some(pal) = show.palettes.get(&n) else {
                self.look_off(&n, Some(0), None);
                self.hub.log("info", "lights", format!("look `{n}` released: its palette was removed"));
                continue;
            };
            let Some(look) = self.looks.get_mut(&n) else { continue };
            look.list = CueList::look(pal);
            let target = cuelist::tracked(&look.list, 0, &show.rig, &show.palettes);
            let o = if target.values.keys().eq(look.pb.applied.keys()) {
                look.pb.refresh(&look.list, &show, None, get, &mut self.tokens, now)
            } else {
                look.pb.goto(&look.list, 0, Nav::Goto, Some(playback::REFRESH_FADE_MS), &show, get, &mut self.tokens, now)
            };
            self.out(o);
        }
    }

    // ---- shared look layers ------------------------------------------------------------

    fn layer_status(&self) -> Value {
        let now = Instant::now();
        let mut out = Value::map();
        for slot in Slot::ALL {
            let mut state = Value::map().with("active", false).with("pending", Value::Null);
            if let Some(l) = self.layers.get(&slot) {
                state = state.with("active", true).with("generation", l.generation as i64).with("owner", l.pb.key.clone())
                    .with("source_owner", l.source_owner.clone().map(Value::Str).unwrap_or(Value::Null))
                    .with("priority", l.pb.priority as i64).with("origin", l.pb.origin.as_str()).with("selection", l.selection.value()).with("controls", l.pb.controls.value())
                    .with("cue", l.pb.current.and_then(|k| l.list.cues.get(k)).map(|c| Value::Str(c.id.clone())).unwrap_or(Value::Null))
                    .with("remaining_ms", l.expires.map(|at| Value::Int(at.saturating_duration_since(now).as_millis().min(i64::MAX as u128) as i64)).unwrap_or(Value::Null));
            }
            if let Some(p) = self.pending_layers.get(&slot) {
                state = state.with("pending", Value::map().with("generation", p.generation as i64).with("owner", format!("layer:{}:{}", slot.name(), p.generation)).with("beat", p.boundary).with("selection", p.selection.value()).with("controls", p.controls.value()).with("source_owner", p.source_owner.clone().map(Value::Str).unwrap_or(Value::Null)));
            }
            out = out.with(slot.name(), state);
        }
        let releasing = self.timer_data.values().filter_map(|t| match t {
            Timer::Release { list, .. } if list.starts_with("layer:") => Some(Value::Str(list.clone())),
            _ => None,
        }).collect();
        out.with("panic_latched", self.panic_latched).with("releasing", Value::List(releasing))
    }

    fn layer_priority(&self, slot: Slot, own: Option<u16>, cmd: &Command, args: &Value) -> u16 {
        let explicit = args.get_path("priority").and_then(Value::as_i64).map(|p| p.clamp(0, (PRIORITY_MANUAL - 1) as i64) as u16);
        if cmd.priority() <= PRIORITY_CHAT {
            explicit.unwrap_or(PRIORITY_CHAT - 2 + slot.rank()).min(PRIORITY_CHAT)
        } else {
            let default = if cmd.priority() >= PRIORITY_MANUAL { PRIORITY_MANUAL - 3 + slot.rank() } else { PRIORITY_PRESET + slot.rank() };
            explicit.unwrap_or(default).max(own.unwrap_or(0)).min(PRIORITY_MANUAL - 1)
        }
    }

    fn check_layer_owner(&self, slot: Slot, cmd: &Command, priority: u16) -> Result<(), String> {
        self.check_layer_owner_for(slot, cmd, priority, None)
    }

    fn check_layer_owner_for(&self, slot: Slot, cmd: &Command, priority: u16, owner: Option<&str>) -> Result<(), String> {
        let permitted = |p: u16, actor: &Option<Actor>| cmd.priority() >= PRIORITY_MANUAL
            || priority >= p && (p > PRIORITY_CHAT || cmd.priority() > PRIORITY_CHAT || actor == &cmd.actor);
        if self.layers.get(&slot).filter(|l| owner.is_none_or(|o| l.source_owner.as_deref() == Some(o) || l.pb.key == o)).is_some_and(|l| !permitted(l.pb.priority, &l.pb.actor))
            || self.pending_layers.get(&slot).filter(|p| owner.is_none_or(|o| p.source_owner.as_deref() == Some(o) || o == format!("layer:{}:{}", slot.name(), p.generation))).is_some_and(|p| !permitted(p.priority, &p.actor))
        { return Err(format!("{} layer belongs to a higher-priority or different chat owner", slot.name())); }
        Ok(())
    }

    fn matches_source_owner(&self, slot: Slot, args: &Value) -> Result<bool, String> {
        let Some(v) = args.get_path("owner") else { return Ok(true) };
        let owner = v.as_str().filter(|s| !s.is_empty()).ok_or("owner must be a non-empty token")?;
        let running = self.layers.get(&slot).is_some_and(|l| l.source_owner.as_deref() == Some(owner) || l.pb.key == owner);
        let pending = self.pending_layers.get(&slot).is_some_and(|l| l.source_owner.as_deref() == Some(owner) || owner == format!("layer:{}:{}", slot.name(), l.generation));
        Ok(running || pending)
    }

    fn check_effect_ownership(&self, slot: Slot, list: &CueList) -> Result<BTreeSet<String>, String> {
        let wanted = list.cues.iter().flat_map(|c| c.effects.iter().map(|(n, _, _)| n.clone())).collect::<BTreeSet<_>>();
        for (other, layer) in &self.layers {
            if *other != slot && let Some(name) = layer.list.cues.iter().flat_map(|c| &c.effects).map(|(n, _, _)| n).find(|n| wanted.contains(*n)) {
                return Err(format!("effect `{name}` is owned by the {} layer; each authored effect has one oscillator", other.name()));
            }
        }
        for (other, pending) in &self.pending_layers {
            if *other != slot && let Some(name) = wanted.intersection(&pending.reserved_effects).next() {
                return Err(format!("effect `{name}` is reserved by the pending {} layer", other.name()));
            }
        }
        for timer in self.timer_data.values() {
            if let Timer::Release { list, cmds } = timer
                && let Some(other) = list.strip_prefix("layer:").and_then(|s| s.split(':').next())
                && other != slot.name()
            {
                for (addr, _, _) in cmds {
                    if let Some(name) = addr.strip_prefix("lights.effect.").and_then(|s| s.strip_suffix(".active"))
                        && wanted.contains(name)
                    { return Err(format!("effect `{name}` is still releasing from the {other} layer")); }
                }
            }
        }
        Ok(wanted)
    }

    fn beat_position(&mut self) -> f64 {
        let now = Instant::now();
        let dt = now.saturating_duration_since(self.clock_at).as_secs_f64();
        self.clock_at = now;
        let snap = self.hub.snapshot.load();
        self.clock.update(dt, snap.signal("beat.phase"), snap.signal("beat.bpm"), snap.signal("beat.confidence"), snap.signal("beat.position"))
    }

    fn select_layer(&mut self, args: &Value, cmd: &Command) -> Result<(), String> {
        if self.panic_latched { return Err("safe look latched after panic; operator must release it before selecting layers".into()); }
        let slot = Slot::parse(args.get_path("layer").and_then(Value::as_str).ok_or("needs layer")?)?;
        let selection = Selection::parse(args)?;
        let controls = Controls::default().patched(args, &self.show)?;
        let (list, _) = selection.compose(&self.show, &controls)?;
        let priority = self.layer_priority(slot, list.priority, cmd, args);
        self.check_layer_owner(slot, cmd, priority)?;
        let reserved_effects = self.check_effect_ownership(slot, &list)?;
        let duration_ms = arg_ms(args, "duration")?;
        if duration_ms == Some(0) || slot == Slot::Accent && duration_ms.is_none() { return Err("accent needs a positive finite duration; other layer durations must also be positive".into()); }
        if let Some(ms) = duration_ms { Instant::now().checked_add(Duration::from_millis(ms)).ok_or("duration is too large")?; }
        let quantize = args.get_path("quantize").map(|v| v.as_f64().ok_or("quantize must be beats")).transpose()?;
        let boundary = match quantize {
            Some(grid) => {
                Some(foundation::boundary(self.beat_position(), grid)?)
            }
            None => None,
        };
        let source_owner = args.get_path("owner").map(|v| v.as_str().filter(|s| !s.is_empty()).map(String::from).ok_or("owner must be a non-empty token")).transpose()?;
        self.layer_seq += 1;
        let generation = self.layer_seq;
        let pending = PendingLayer { selection, controls, generation, boundary: boundary.unwrap_or(0.0), priority, origin: cmd.origin, actor: cmd.actor.clone(),
            cause: Some(cmd.id), fade: arg_ms(args, "fade")?, duration_ms, release_ms: arg_ms(args, "release")?.unwrap_or(list.release_ms), source_owner, reserved_effects };
        if boundary.is_some() {
            self.pending_layers.insert(slot, pending);
            self.arm(Instant::now() + Duration::from_millis(10), Timer::LayerStart { slot, generation });
        } else {
            self.pending_layers.remove(&slot);
            self.start_layer(slot, pending)?;
        }
        self.publish_playbacks();
        Ok(())
    }

    /// `lights.layer.pick`: choose a tagged palette or cue list, then run it through
    /// [`Self::select_layer`] so ownership, priority, quantize and fades are identical.
    fn pick_layer(&mut self, args: &Value, cmd: &Command) -> Result<(), String> {
        let slot = Slot::parse(args.get_path("layer").and_then(Value::as_str).ok_or("needs layer")?)?;
        for f in ["palette", "cuelist", "cue"] {
            if args.get_path(f).is_some() { return Err(format!("pick chooses the palette or cue list itself; drop `{f}` or use lights.layer.select")); }
        }
        let tag_list = |f: &str| -> Result<Vec<String>, String> {
            crate::tags::parse(args.get_path(f).filter(|v| !v.is_null()).map(|v| foundation::names(v, f)).transpose()?.unwrap_or_default())
        };
        let required = tag_list("tags")?;
        let any = tag_list("any")?;
        let kinds = crate::tags::Source::parse_kind(args.get_path("kind").filter(|v| !v.is_null()).map(|v| v.as_str().ok_or("kind must be palette, cuelist or any")).transpose()?)?;
        let avoid = match args.get_path("avoid_repeat").filter(|v| !v.is_null()) {
            None => crate::tags::AVOID_REPEAT,
            Some(v) => v.as_i64().filter(|n| (0..=crate::tags::HISTORY as i64).contains(n))
                .ok_or_else(|| format!("avoid_repeat must be a whole number 0..={}", crate::tags::HISTORY))? as usize,
        };
        let Value::Map(base) = args else { return Err("pick needs named arguments".into()) };
        let mut base = base.clone();
        for k in ["tags", "any", "kind", "avoid_repeat"] { base.remove(k); }
        let with = |c: &crate::tags::Candidate| {
            let mut m = base.clone();
            m.insert(c.source.field().into(), Value::Str(c.name.clone()));
            Value::Map(m)
        };
        // Only content that composes on this rig with these controls is a candidate.
        let controls = Controls::default().patched(args, &self.show)?;
        let usable: Vec<_> = crate::tags::candidates(&self.show, kinds, &required, &any).into_iter()
            .filter(|c| Selection::parse(&with(c)).and_then(|s| s.compose(&self.show, &controls)).is_ok())
            .collect();
        self.pick_rng ^= self.pick_rng << 13;
        self.pick_rng ^= self.pick_rng >> 7;
        self.pick_rng ^= self.pick_rng << 17;
        let empty = VecDeque::new();
        let recent = self.picks.get(&slot).unwrap_or(&empty);
        let Some(choice) = crate::tags::choose(&usable, recent, avoid, self.pick_rng).cloned() else {
            self.hub.log("info", "lights", format!("lights.layer.pick {}: no usable {} tagged {required:?}; layer unchanged", slot.name(), kinds.iter().map(|k| k.field()).collect::<Vec<_>>().join(" or ")));
            return Ok(());
        };
        self.select_layer(&with(&choice), cmd)?;
        crate::tags::remember(self.picks.entry(slot).or_default(), choice.key());
        self.hub.log("info", "lights", format!("lights.layer.pick {}: {} `{}` ({} of {} preferred tags)", slot.name(), choice.source.field(), choice.name, choice.score, any.len()));
        Ok(())
    }

    fn start_layer(&mut self, slot: Slot, pending: PendingLayer) -> Result<(), String> {
        if self.panic_latched { return Err("safe look latched after panic".into()); }
        // Validate again after a quantized wait: content/rig may have changed.
        let (list, k) = pending.selection.compose(&self.show, &pending.controls)?;
        self.check_effect_ownership(slot, &list)?;
        let now = Instant::now();
        let expires = pending.duration_ms.map(|ms| now.checked_add(Duration::from_millis(ms)).ok_or("duration is too large")).transpose()?;
        let id = format!("layer:{}:{}", slot.name(), pending.generation);
        for field in ["energy", "brightness", "rhythm"] {
            self.hub.publish(&format!("lights.layer.{}.{field}", slot.name()), pending.controls.value().get_path(field).cloned().expect("control field"));
        }
        let mut pb = Playback::layer(&id, pending.priority, pending.origin, pending.actor, pending.cause, pending.controls, &self.show);
        pb.beat_anchor = Some(if pending.boundary > 0.0 { pending.boundary } else { self.beat_position() });
        let get = self.get();
        let out = pb.goto(&list, k, Nav::Goto, pending.fade, &self.show, &get, &mut self.tokens, now);
        // Replacements keep a separate fading ownership key. Old timers can only release it.
        if let Some(mut old) = self.layers.remove(&slot) {
            let out = old.pb.replace_by(&pb, pending.fade.unwrap_or(0), now);
            self.out(out);
        }
        self.layers.insert(slot, Layer { selection: pending.selection, list, pb, generation: pending.generation, expires, release_ms: pending.release_ms, source_owner: pending.source_owner });
        self.out(out);
        if let Some(at) = expires { self.arm(at, Timer::LayerExpire { slot, generation: pending.generation }); }
        self.hub.emit(Event::new("lights.layer.selected", pending.origin, Value::map().with("layer", slot.name()).with("generation", pending.generation as i64).with("priority", pending.priority as i64).with("source_owner", self.layers[&slot].source_owner.clone().map(Value::Str).unwrap_or(Value::Null))).with_causal(pending.cause));
        Ok(())
    }

    fn poll_layer(&mut self, slot: Slot, generation: u64) {
        let Some(p) = self.pending_layers.get(&slot) else { return };
        if p.generation != generation { return; }
        let boundary = p.boundary;
        if self.beat_position() >= boundary {
            let pending = self.pending_layers.remove(&slot).expect("checked above");
            if let Err(e) = self.start_layer(slot, pending) { self.hub.log("warn", "lights", format!("quantized layer: {e}")); }
            self.publish_playbacks();
        } else {
            self.arm(Instant::now() + Duration::from_millis(10), Timer::LayerStart { slot, generation });
        }
    }

    fn release_layer(&mut self, slot: Slot, fade: Option<u64>, cause: Option<Id>) {
        self.release_layer_for(slot, fade, cause, None);
    }

    fn release_layer_for(&mut self, slot: Slot, fade: Option<u64>, cause: Option<Id>, owner: Option<&str>) {
        if self.pending_layers.get(&slot).is_some_and(|p| owner.is_none_or(|o| p.source_owner.as_deref() == Some(o) || o == format!("layer:{}:{}", slot.name(), p.generation))) {
            self.pending_layers.remove(&slot);
        }
        let running = self.layers.get(&slot).is_some_and(|l| owner.is_none_or(|o| l.source_owner.as_deref() == Some(o) || l.pb.key == o));
        if running { self.release_running_layer(slot, fade, cause); }
    }

    fn release_running_layer(&mut self, slot: Slot, fade: Option<u64>, cause: Option<Id>) {
        if let Some(mut layer) = self.layers.remove(&slot) {
            layer.pb.cause = cause;
            let out = layer.pb.release(fade.unwrap_or(layer.release_ms), Instant::now());
            self.out(out);
            self.hub.emit(Event::new("lights.layer.released", Origin::System, Value::map().with("layer", slot.name()).with("generation", layer.generation as i64)).with_causal(cause));
        }
    }

    fn set_layer(&mut self, args: &Value, cmd: &Command) -> Result<(), String> {
        self.update_layer(args, cmd, true)
    }

    fn update_layer(&mut self, args: &Value, cmd: &Command, authored: bool) -> Result<(), String> {
        if self.panic_latched { return Err("safe look latched after panic".into()); }
        let slot = Slot::parse(args.get_path("layer").and_then(Value::as_str).ok_or("needs layer")?)?;
        if !self.matches_source_owner(slot, args)? { return Ok(()); }
        let owner = args.get_path("owner").and_then(Value::as_str);
        let own = self.layers.get(&slot).filter(|l| owner.is_none_or(|o| l.source_owner.as_deref() == Some(o) || l.pb.key == o)).and_then(|l| l.list.priority)
            .or_else(|| self.pending_layers.get(&slot).and_then(|p| p.selection.cuelist.as_ref()).and_then(|n| self.show.lists.get(n)).and_then(|l| l.priority));
        self.check_layer_owner_for(slot, cmd, self.layer_priority(slot, own, cmd, args), owner)?;
        if let Some(layer) = self.layers.get(&slot).filter(|l| owner.is_none_or(|o| l.source_owner.as_deref() == Some(o) || l.pb.key == o)) {
            let controls = layer.pb.controls.patched(args, &self.show)?;
            let (list, _) = layer.selection.compose(&self.show, &controls)?;
            self.check_effect_ownership(slot, &list)?;
            let mask = controls.mask(&self.show)?;
            let get = self.get();
            let now = Instant::now();
            let layer = self.layers.get_mut(&slot).expect("checked above");
            layer.pb.controls = controls;
            layer.pb.coverage = Some(mask);
            layer.pb.cause = Some(cmd.id);
            layer.list = list;
            let epoch = layer.pb.epoch;
            let go_at = layer.pb.go_at;
            let k = layer.pb.current.unwrap_or(0);
            let out = layer.pb.goto(&layer.list, k, Nav::Locate, Some(arg_ms(args, "fade")?.unwrap_or(playback::REFRESH_FADE_MS)), &self.show, &get, &mut self.tokens, now);
            // Controls do not restart authored follow/wait progression.
            layer.pb.epoch = epoch;
            layer.pb.go_at = go_at;
            self.out(out);
        } else if let Some(pending) = self.pending_layers.get(&slot) {
            let controls = pending.controls.patched(args, &self.show)?;
            let (list, _) = pending.selection.compose(&self.show, &controls)?;
            let reserved_effects = self.check_effect_ownership(slot, &list)?;
            let pending = self.pending_layers.get_mut(&slot).expect("checked above");
            pending.controls = controls;
            pending.reserved_effects = reserved_effects;
        } else { return Err(format!("{} layer is not selected", slot.name())); }
        if authored {
            for field in ["energy", "brightness", "rhythm"] {
                if let Some(value) = args.get_path(field) { self.hub.publish(&format!("lights.layer.{}.{field}", slot.name()), value.clone()); }
            }
        }
        self.publish_playbacks();
        Ok(())
    }

    fn advance_layer(&mut self, slot: Slot) {
        let get = self.get();
        let Some(layer) = self.layers.get_mut(&slot) else { return };
        let k = match layer.pb.current {
            Some(k) if k + 1 < layer.list.cues.len() => k + 1,
            Some(_) if layer.list.looped => 0,
            _ => return,
        };
        let out = layer.pb.goto(&layer.list, k, Nav::Go, None, &self.show, &get, &mut self.tokens, Instant::now());
        self.out(out);
    }

    fn refresh_layers(&mut self, get: &dyn Fn(&str) -> Option<Value>, now: Instant) {
        let slots = self.layers.keys().copied().collect::<Vec<_>>();
        for slot in slots {
            let layer = &self.layers[&slot];
            let rebuilt = layer.selection.compose(&self.show, &layer.pb.controls).and_then(|(list, k)| { self.check_effect_ownership(slot, &list)?; Ok((list, k)) });
            match rebuilt {
                Ok((list, _)) => {
                    let layer = self.layers.get_mut(&slot).expect("collected above");
                    let cue = layer.pb.current.and_then(|k| layer.list.cues.get(k)).map(|c| c.id.clone());
                    let k = cue.as_ref().and_then(|id| list.index_of(id)).unwrap_or(0);
                    layer.list = list;
                    layer.pb.coverage = layer.pb.controls.mask(&self.show).ok();
                    layer.pb.beat_rates = self.show.effects.iter().filter(|e| e.unit == crate::effects::Unit::Beats).map(|e| format!("lights.effect.{}.rate", e.name)).collect();
                    let epoch = layer.pb.epoch;
                    let out = layer.pb.goto(&layer.list, k, Nav::Locate, Some(playback::REFRESH_FADE_MS), &self.show, get, &mut self.tokens, now);
                    layer.pb.epoch = epoch;
                    self.out(out);
                }
                Err(e) => {
                    self.hub.log("warn", "lights", format!("{} layer released after reload: {e}", slot.name()));
                    self.release_running_layer(slot, Some(0), None);
                }
            }
        }
    }

    // ---- playbacks ---------------------------------------------------------------------

    fn master_of(&self, list: &str) -> f32 {
        self.hub.snapshot.load().f32(&format!("lights.cuelist.{list}.master")).unwrap_or(1.0).clamp(0.0, 1.0)
    }

    /// Effective playback priority: the caller's explicit `priority` argument (presets,
    /// timelines) or the list's own priority (default 200), with the list's priority as a
    /// floor; chat-origin requests always run at chat priority.
    fn priority(&self, list: &str, chat: bool, explicit: Option<u16>) -> u16 {
        effective_priority(self.show.lists.get(list).and_then(|l| l.priority), chat, explicit)
    }

    #[allow(clippy::too_many_arguments)]
    fn nav(
        &mut self,
        list: &str,
        target: Option<&str>,
        nav: Nav,
        fade: Option<u64>,
        priority: u16,
        origin: Origin,
        actor: Option<Actor>,
        cause: Option<Id>,
        operator: bool,
    ) -> Result<(), String> {
        let l = self.show.lists.get(list).cloned().ok_or_else(|| format!("unknown cue list `{list}`"))?;
        if l.cues.is_empty() {
            return Err(format!("cue list `{list}` has no cues"));
        }
        let master = self.master_of(list);
        // Re-fired at another priority (or from chat vs. not): the old layer can't be moved,
        // so it is dropped and the playback starts over at the new priority.
        if self.playbacks.get(list).is_some_and(|p| p.current.is_some() && (p.priority != priority || (p.priority <= PRIORITY_CHAT && p.actor != actor))) {
            self.release_list(list, Some(0), cause);
        }
        let pb = self.playbacks.entry(list.to_string()).or_insert_with(|| Playback::new(list, priority, origin, actor.clone(), cause, master));
        if pb.current.is_none() {
            pb.priority = priority;
        }
        pb.cause = cause;
        pb.master = master;
        let k = match (target, nav) {
            (Some(id), _) => l.index_of(id).ok_or_else(|| format!("cue list `{list}` has no cue `{id}`"))?,
            (None, Nav::Go) => match pb.current {
                None => 0,
                Some(c) if c + 1 < l.cues.len() => c + 1,
                Some(_) if l.looped => 0,
                Some(_) => return Err(format!("cue list `{list}` is at its last cue")),
            },
            (None, Nav::Back) => match pb.current {
                Some(c) if c > 0 => c - 1,
                Some(_) if l.looped => l.cues.len() - 1,
                _ => return Err(format!("cue list `{list}` is at its first cue")),
            },
            (None, _) => 0,
        };
        let get = self.get();
        let show = self.show.clone();
        let mut tokens = self.tokens;
        let pb = self.playbacks.get_mut(list).expect("inserted above");
        // Successful navigation transfers intention to its caller; automation cannot
        // inherit restart eligibility from a previous operator at the same priority.
        pb.origin = origin;
        pb.actor = actor;
        let o = pb.goto(&l, k, nav, fade, &show, &get, &mut tokens, Instant::now());
        self.tokens = tokens;
        if operator { self.operator_playbacks.insert(list.to_string()); }
        else { self.operator_playbacks.remove(list); }
        self.out(o);
        self.hub.emit(Event::new("lights.cue.go", origin, Value::map().with("cuelist", list).with("cue", l.cues[k].id.clone())).with_causal(cause));
        self.publish_playbacks();
        Ok(())
    }

    fn release_list(&mut self, list: &str, fade: Option<u64>, cause: Option<Id>) -> bool {
        let Some(mut p) = self.playbacks.remove(list) else { return false };
        self.operator_playbacks.remove(list);
        let f = fade.unwrap_or_else(|| self.show.lists.get(list).map(|l| l.release_ms).unwrap_or(0));
        p.cause = cause;
        let o = p.release(f, Instant::now());
        self.out(o);
        self.hub.emit(Event::new("lights.cue.released", Origin::System, Value::map().with("cuelist", list)).with_causal(cause));
        true
    }

    /// Hold the look `name` (palette) at `priority`; re-firing at another priority (or from
    /// another chat actor) drops the old layer first, like cue lists.
    fn look_on(&mut self, name: &str, fade: Option<u64>, priority: u16, origin: Origin, actor: Option<Actor>, cause: Option<Id>) -> Result<(), String> {
        let pal = self.show.palettes.get(name).ok_or_else(|| format!("unknown look `{name}`"))?;
        let list = CueList::look(pal);
        if self.looks.get(name).is_some_and(|l| l.pb.priority != priority || (l.pb.priority <= PRIORITY_CHAT && l.pb.actor != actor)) {
            self.look_off(name, Some(0), cause);
        }
        let get = self.get();
        let show = self.show.clone();
        let now = Instant::now();
        let look = match self.looks.entry(name.to_string()) {
            std::collections::btree_map::Entry::Occupied(e) => {
                let l = e.into_mut();
                l.list = list;
                l
            }
            std::collections::btree_map::Entry::Vacant(e) => e.insert(Look { pb: Playback::look(name, priority, origin, actor, cause), list }),
        };
        look.pb.cause = cause;
        let o = look.pb.goto(&look.list, 0, Nav::Goto, fade, &show, &get, &mut self.tokens, now);
        self.out(o);
        self.publish_playbacks();
        Ok(())
    }

    /// Release a held look (fade: intensities fade out, then its overrides go); false if it
    /// wasn't running.
    fn look_off(&mut self, name: &str, fade: Option<u64>, cause: Option<Id>) -> bool {
        let Some(mut look) = self.looks.remove(name) else { return false };
        look.pb.cause = cause;
        let o = look.pb.release(fade.unwrap_or(0), Instant::now());
        self.out(o);
        true
    }

    /// Remove the built-in safe look (used when the project has no `safe` cue list).
    fn release_builtin_safe(&mut self, cause: Option<Id>) {
        let key = playback::key(SAFE_LIST);
        let mut addrs: Vec<String> = vec!["lights.master".into(), "lights.blackout".into()];
        for h in self.show.rig.heads.iter().filter(|h| h.parent.is_none()) {
            addrs.push(h.addr("intensity"));
            if h.attr("color").is_some() {
                addrs.push(h.addr("color"));
            }
        }
        let cmds = addrs
            .into_iter()
            .map(|a| Command::new(Origin::System, Op::Release { address: a }).with_key(key.clone()).with_priority(Some(1000)).caused_by(cause))
            .collect();
        self.submit(cmds);
    }

    fn release_all(&mut self, fade: Option<u64>, cause: Option<Id>) {
        let names: Vec<String> = self.playbacks.keys().cloned().collect();
        for n in names {
            self.release_list(&n, fade, cause);
        }
        let looks: Vec<String> = self.looks.keys().cloned().collect();
        for n in looks {
            self.look_off(&n, fade, cause);
        }
        let layers: Vec<Slot> = self.layers.keys().chain(self.pending_layers.keys()).copied().collect();
        for slot in layers { self.release_layer(slot, fade, cause); }
        self.publish_playbacks();
    }

    /// Panic: everything off, programmer cleared, safe look on top (§2.8, §9.1).
    fn panic(&mut self, cause: Option<Id>) {
        self.panic_latched = true;
        self.release_all(Some(0), cause);
        // Outgoing crossfades are no longer in the running slot map. Drop their ownership too.
        let outgoing = self.timer_data.iter().filter_map(|(id, t)| match t {
            Timer::Release { list, cmds } if list.starts_with("layer:") => Some((*id, cmds.iter().map(|(_, _, c)| c.clone()).collect::<Vec<_>>())),
            _ => None,
        }).collect::<Vec<_>>();
        for (id, cmds) in outgoing {
            self.timer_data.remove(&id);
            self.submit(cmds);
        }
        let mut cmds = self.prog.release(Origin::System);
        cmds.extend(self.prog.highlight(&self.show.rig.clone(), false, Origin::System));
        self.prog.selection.clear();
        self.flash_token += 1;
        // Core panic intentionally retains manual controls in other subsystems.
        // Lighting HTP must not retain a direct/manual level above the safe look.
        // No owner key: unlike playback retirement, this deliberately clears every owner.
        for head in &self.show.rig.heads {
            for attr in &head.attrs {
                cmds.push(Command::new(Origin::System, Op::Release { address: head.addr(&attr.name) }).with_priority(Some(u16::MAX)).caused_by(cause));
            }
        }
        self.submit(cmds);
        if self.show.lists.contains_key(SAFE_LIST) {
            let p = self.priority(SAFE_LIST, false, None).max(PRIORITY_MANUAL);
            if let Err(e) = self.nav(SAFE_LIST, None, Nav::Goto, Some(0), p, Origin::System, None, cause, false) {
                self.hub.log("error", "lights", format!("panic: {e}"));
            }
        } else {
            // built-in safe look from `[safety] safe_look`
            let s = self.show.rig.safety.clone();
            let mut cmds = Vec::new();
            let key = playback::key(SAFE_LIST);
            let c = |op: Op| Command::new(Origin::System, op).with_key(key.clone()).with_priority(Some(1000)).caused_by(cause);
            for h in &self.show.rig.heads {
                if h.parent.is_some() {
                    continue;
                }
                cmds.push(c(Op::Set { address: h.addr("intensity"), value: Value::Float(s.safe_intensity as f64) }));
                if h.attr("color").is_some() {
                    cmds.push(c(Op::Set { address: h.addr("color"), value: Value::from([s.safe_color[0], s.safe_color[1], s.safe_color[2], 1.0]) }));
                }
            }
            cmds.push(c(Op::Set { address: "lights.master".into(), value: Value::Float(1.0) }));
            cmds.push(c(Op::Set { address: "lights.blackout".into(), value: Value::Bool(false) }));
            self.submit(cmds);
        }
        self.publish_programmer();
        self.hub.log("warn", "lights", "panic: playbacks released, safe look on".to_string());
    }

    /// An explicit operator handoff to the configured idle scene, never a new authored look.
    fn default_lighting(&mut self, cause: Option<Id>) {
        self.panic_latched = false;
        self.release_all(Some(0), cause);
        self.release_builtin_safe(cause);
        // Retired looks, cue entries and outgoing layers can still own fading overrides.
        // Flush every release tail now; discard delayed entries, advances and flash timers.
        let timers = std::mem::take(&mut self.timer_data);
        for timer in timers.into_values() {
            if let Timer::Release { cmds, .. } = timer {
                self.submit(cmds.into_iter().map(|(_, _, c)| c).collect());
            }
        }
        self.timers.clear();
        self.master_pending.clear();
        self.flash_token += 1;
        let mut cmds = self.prog.release(Origin::System);
        cmds.extend(self.prog.highlight(&self.show.rig.clone(), false, Origin::System));
        self.prog.selection.clear();
        // Unkeyed manual releases clear all owners, including direct controls and HTP
        // levels that would otherwise keep output authored and prevent the idle handoff.
        let release = |address| Command::new(Origin::System, Op::Release { address })
            .with_priority(Some(u16::MAX)).caused_by(cause);
        for head in &self.show.rig.heads {
            for attr in &head.attrs { cmds.push(release(head.addr(&attr.name))); }
        }
        for group in &self.show.rig.groups {
            for attr in &group.attrs { cmds.push(release(format!("lights.group.{}.{}", group.name, attr.name))); }
            cmds.push(release(format!("lights.group.{}.master", group.name)));
        }
        let set = |address, value| Command::new(Origin::System, Op::Set { address, value })
            .with_priority(Some(PRIORITY_PRESET)).caused_by(cause);
        for effect in &self.show.effects {
            let address = format!("lights.effect.{}.active", effect.name);
            cmds.push(release(address.clone()));
            cmds.push(set(address, Value::Bool(false)));
        }
        for (address, value) in [("lights.master", Value::Float(1.0)), ("lights.blackout", Value::Bool(false))] {
            cmds.push(release(address.into()));
            cmds.push(set(address.into(), value));
        }
        self.submit(cmds);
        self.publish_programmer();
        self.publish_playbacks();
    }

    /// `lights.flash {color?, ms?, target?, intensity?}` (action or event).
    fn flash(&mut self, args: &Value, origin: Origin, actor: Option<Actor>, priority: u16, cause: Option<Id>) -> Result<(), String> {
        let target = arg_str(args, "target", 2).unwrap_or_else(|| "all".into());
        let ms = arg_ms(args, "ms")?.unwrap_or(250).clamp(20, 10_000);
        let intensity = args.get_path("intensity").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.0, 1.0);
        let color = match arg(args, "color", 0) {
            Some(v) => Some(crate::palette::Spec::from_value(v)?),
            None => None,
        };
        let rig = self.show.rig.clone();
        let heads_i = rig.heads_for(&target, "intensity")?;
        let heads_c = rig.heads_for(&target, "color")?;
        let key = (priority > PRIORITY_CHAT).then_some("flash");
        let c = |op: Op| {
            let mut c = Command::new(origin, op).with_priority(Some(priority)).with_actor(actor.clone()).caused_by(cause);
            if let Some(k) = key {
                c = c.with_key(k);
            }
            c
        };
        let get = self.get();
        let mut cmds = Vec::new();
        let mut addrs = Vec::new();
        for h in &heads_i {
            let a = rig.heads[*h].addr("intensity");
            cmds.push(c(Op::Set { address: a.clone(), value: Value::Float(intensity) }));
            addrs.push(a);
        }
        let mut all = addrs.clone();
        if let Some(spec) = &color {
            for h in &heads_c {
                let v = crate::palette::resolve(spec, &self.show.palettes, &rig, *h, "color", crate::rig::AttrKind::Color, &get)?;
                let a = rig.heads[*h].addr("color");
                cmds.push(c(Op::Set { address: a.clone(), value: v }));
                all.push(a);
            }
        }
        self.submit(cmds);
        self.flash_token += 1;
        let hold = (ms / 4).clamp(20, 60);
        let now = Instant::now();
        let t = self.flash_token;
        self.arm(now + Duration::from_millis(hold), Timer::FlashFade { addrs, ms: ms as u32, token: t, origin, actor: actor.clone(), priority });
        self.arm(now + Duration::from_millis(hold + ms), Timer::FlashEnd { addrs: all, token: t, origin, actor, priority });
        Ok(())
    }

    // ---- actions -----------------------------------------------------------------------

    pub fn action(&mut self, cmd: Command) {
        let Op::Action { name, args } = &cmd.op else { return };
        let res = self.handle(name, args, &cmd);
        if let Err(e) = res {
            self.hub.log("warn", "lights", format!("{name}: {e}"));
        }
    }

    fn handle(&mut self, name: &str, args: &Value, cmd: &Command) -> Result<(), String> {
        let chat = cmd.priority() <= PRIORITY_CHAT;
        let explicit = args.get_path("priority").and_then(Value::as_i64).map(|p| p.clamp(0, u16::MAX as i64) as u16);
        let cause = Some(cmd.id);
        let fade = arg_ms(args, "fade")?;
        let operator = cmd.priority() >= PRIORITY_MANUAL && cmd.origin.default_priority() >= PRIORITY_MANUAL;
        match name {
            "lights.default" => {
                if !operator {
                    return Err("only the operator can restore default lighting".into());
                }
                self.default_lighting(cause);
                Ok(())
            }
            "lights.layer.select" => self.select_layer(args, cmd),
            "lights.layer.pick" => self.pick_layer(args, cmd),
            "lights.layer.set" => self.set_layer(args, cmd),
            "lights.layer.release" => {
                let slot = Slot::parse(args.get_path("layer").and_then(Value::as_str).ok_or("needs layer")?)?;
                if !self.matches_source_owner(slot, args)? { return Ok(()); }
                self.check_layer_owner_for(slot, cmd, self.layer_priority(slot, None, cmd, args), args.get_path("owner").and_then(Value::as_str))?;
                self.release_layer_for(slot, fade, cause, args.get_path("owner").and_then(Value::as_str));
                self.publish_playbacks();
                Ok(())
            }
            "lights.layer.status" => {
                self.hub.emit(Event::new("lights.layer.status", Origin::System, self.layer_status()).with_causal(cause));
                Ok(())
            }
            "lights.cue" => {
                if let Some(look) = arg_str(args, "look", usize::MAX).filter(|s| !s.is_empty()) {
                    if arg_str(args, "cuelist", usize::MAX).or_else(|| arg_str(args, "cue", 0)).is_some_and(|s| !s.is_empty()) {
                        return Err("give either `look` or a cue list, not both".into());
                    }
                    return self.look_on(&look, fade, effective_priority(None, chat, explicit), cmd.origin, cmd.actor.clone(), cause);
                }
                let (list, cue) = match (arg_str(args, "cuelist", usize::MAX).filter(|s| !s.is_empty()), arg_str(args, "cue", 0).filter(|s| !s.is_empty())) {
                    (Some(l), c) => (l, c.or_else(|| arg_str(args, "cue", 1))),
                    (None, Some(c)) => (c, arg_str(args, "cue_id", 1)),
                    (None, None) => return Err("needs `cue` (a cue list) or `cuelist` + `cue`".into()),
                };
                let p = self.priority(&list, chat, explicit);
                // no cue: (re)start at the first cue; with a cue: that cue's tracked state
                self.nav(&list, cue.as_deref(), Nav::Goto, fade, p, cmd.origin, cmd.actor.clone(), cause, operator)
            }
            "lights.go" | "lights.back" | "lights.goto" | "lights.locate" => {
                let list = arg_str(args, "cuelist", 0).ok_or("needs `cuelist`")?;
                let cue = arg_str(args, "cue", 1);
                let nav = match name {
                    "lights.go" => Nav::Go,
                    "lights.back" => Nav::Back,
                    "lights.goto" => Nav::Goto,
                    _ => Nav::Locate,
                };
                if matches!(nav, Nav::Goto | Nav::Locate) && cue.is_none() {
                    return Err("needs `cue`".into());
                }
                let fade = if nav == Nav::Locate { Some(0) } else { fade };
                let p = match self.playbacks.get(&list) {
                    Some(pb) if explicit.is_none() && pb.current.is_some() => pb.priority,
                    _ => self.priority(&list, chat, explicit),
                };
                self.nav(&list, if nav == Nav::Go || nav == Nav::Back { None } else { cue.as_deref() }, nav, fade, p, cmd.origin, cmd.actor.clone(), cause, operator)
            }
            "lights.release" => {
                if let Some(look) = arg_str(args, "look", usize::MAX).filter(|s| !s.is_empty()) {
                    if arg_str(args, "cuelist", usize::MAX).or_else(|| arg_str(args, "cue", 0)).is_some_and(|s| !s.is_empty()) {
                        return Err("give either `look` or a cue list, not both".into());
                    }
                    if !self.look_off(&look, fade, cause) && !self.show.palettes.contains_key(&look) {
                        return Err(format!("unknown look `{look}`"));
                    }
                    self.publish_playbacks();
                    return Ok(());
                }
                // `lights.release all` is the text form of release-all (bare `lights.release`
                // text parses as the core's release op).
                let list = arg_str(args, "cuelist", usize::MAX).or_else(|| arg_str(args, "cue", 0)).filter(|s| !s.is_empty() && s != "all");
                let operator = cmd.priority() >= PRIORITY_MANUAL;
                if self.panic_latched && !operator && list.as_deref().is_none_or(|l| l == SAFE_LIST) {
                    return Err(format!("safe look latched after panic; ignoring release from {}", cmd.origin.as_str()));
                }
                if operator && list.as_deref().is_none_or(|l| l == SAFE_LIST) {
                    self.panic_latched = false;
                }
                match list {
                    Some(l) if l == SAFE_LIST && !self.show.lists.contains_key(SAFE_LIST) => {
                        self.release_builtin_safe(cause);
                        self.publish_playbacks();
                    }
                    Some(l) => {
                        if !self.release_list(&l, fade, cause) && !self.show.lists.contains_key(&l) {
                            return Err(format!("unknown cue list `{l}`"));
                        }
                        self.publish_playbacks();
                    }
                    None => {
                        if operator && !self.show.lists.contains_key(SAFE_LIST) { self.release_builtin_safe(cause); }
                        self.release_all(fade, cause);
                    }
                }
                Ok(())
            }
            "lights.panic" => {
                self.panic(cause);
                Ok(())
            }
            "lights.flash" => self.flash(args, cmd.origin, cmd.actor.clone(), cmd.priority(), cause),
            "lights.effect.start" | "lights.effect.stop" => {
                let e = arg_str(args, "name", 0).ok_or("needs `name`")?;
                if !self.show.effects.iter().any(|x| x.name == e) {
                    return Err(format!("unknown effect `{e}`"));
                }
                let mut cmds = Vec::new();
                let c = |op: Op| Command::new(cmd.origin, op).with_priority(cmd.priority).with_actor(cmd.actor.clone()).caused_by(cause);
                if name == "lights.effect.start" {
                    for (p, v) in [("size", args.get_path("size")), ("rate", args.get_path("rate"))] {
                        if let Some(v) = v.and_then(Value::as_f64) {
                            cmds.push(c(Op::Set { address: format!("lights.effect.{e}.{p}"), value: Value::Float(v) }));
                        }
                    }
                    cmds.push(c(Op::Set { address: format!("lights.effect.{e}.active"), value: Value::Bool(true) }));
                } else {
                    cmds.push(c(Op::Set { address: format!("lights.effect.{e}.active"), value: Value::Bool(false) }));
                }
                self.submit(cmds);
                Ok(())
            }
            n if n.starts_with("lights.programmer.") => {
                if chat {
                    return Err("the programmer is not available to chat".into());
                }
                self.programmer(&n["lights.programmer.".len()..], args, cmd.origin)
            }
            "lights.knob" => {
                if chat {
                    return Err("knobs are not available to chat".into());
                }
                self.knob(args)
            }
            "lights.rdm.discover" => {
                self.shared.rdm_request.store(true, Ordering::Release);
                self.rdm_started = true;
                Ok(())
            }
            other => Err(format!("unknown lights action `{other}`")),
        }
    }

    /// `lights.knob {look | cuelist, target, value, save?}`: move one of a look's or cue list's
    /// knobs. The loaded look / cue list changes at once (running ones follow with a short
    /// crossfade); with `save` (the default) the value is also written into its file, comments
    /// kept. Callers dragging a slider send `save = false` until it settles.
    fn knob(&mut self, args: &Value) -> Result<(), String> {
        let s = |k: &str| args.get_path(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
        let target = s("target").ok_or("needs `target` (which knob)")?;
        let value = args.get_path("value").filter(|v| !v.is_null()).ok_or("needs `value`")?;
        let save = args.get_path("save").is_none_or(Value::truthy);
        let mut show = (*self.show).clone();
        let (rel, at, v) = match (s("look"), s("cuelist")) {
            (Some(n), None) => {
                let p = show.palettes.get_mut(&n).ok_or_else(|| format!("unknown look `{n}`"))?;
                let b = p.knobs.iter().find(|b| b.knob.target == target).cloned().ok_or_else(|| format!("look `{n}` has no knob for `{target}`"))?;
                let v = b.knob.fit(value).ok_or_else(|| format!("knob “{}” can't take `{value}`", b.knob.label))?;
                knobs::set_look(p, &b.at, &v);
                (format!("lights/palettes/{n}.toml"), b.at, v)
            }
            (None, Some(n)) => {
                let l = show.lists.get_mut(&n).ok_or_else(|| format!("unknown cue list `{n}`"))?;
                let b = l.knobs.iter().find(|b| b.knob.target == target).cloned().ok_or_else(|| format!("cue list `{n}` has no knob for `{target}`"))?;
                let v = b.knob.fit(value).ok_or_else(|| format!("knob “{}” can't take `{value}`", b.knob.label))?;
                knobs::set_cue(l, &b.at, &v);
                (format!("lights/cuelists/{n}.toml"), b.at, v)
            }
            _ => return Err("needs `look` or `cuelist` (one of them)".into()),
        };
        if save {
            let project = se_store::Project::open(&self.ctx.project_root).map_err(|e| e.to_string())?;
            project.edit(&rel, |doc| knobs::write(doc, &at, &v)).map_err(|e| format!("{rel}: {e:#}"))?;
        }
        self.show = Arc::new(show);
        self.refresh_running();
        Ok(())
    }

    fn programmer(&mut self, op: &str, args: &Value, origin: Origin) -> Result<(), String> {
        let show = self.show.clone();
        let get = self.get();
        let cmds = match op {
            "select" => {
                let targets: Vec<String> = match args.get_path("targets") {
                    Some(Value::List(l)) => l.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
                    Some(Value::Str(s)) => vec![s.clone()],
                    _ => args
                        .get_path("args")
                        .and_then(Value::as_list)
                        .map(|l| l.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                        .unwrap_or_default(),
                };
                let add = args.get_path("add").is_some_and(Value::truthy);
                self.prog.select(&show.rig, &targets, add)?;
                // highlight follows the selection
                if !self.prog.highlight.is_empty() { self.prog.highlight(&show.rig, true, origin) } else { Vec::new() }
            }
            "set" => {
                let value = arg(args, "value", 1).cloned().ok_or("needs `value`")?;
                if let Some(addr) = args.get_path("address").and_then(Value::as_str) {
                    let v = value.clone();
                    self.prog.values.insert(addr.to_string(), v.clone());
                    vec![Command::new(origin, Op::Set { address: addr.into(), value: v }).with_key(programmer::KEY).with_priority(Some(PRIORITY_MANUAL))]
                } else {
                    let attr = arg_str(args, "attr", 0).ok_or("needs `attr` (or `address`)")?;
                    self.prog.set(&show, &attr, &value, origin, &get)?
                }
            }
            "nudge" => {
                let attr = arg_str(args, "attr", 0).ok_or("needs `attr`")?;
                let delta = arg(args, "delta", 1).and_then(Value::as_f64).ok_or("needs a numeric `delta`")?;
                self.prog.nudge(&show.rig, &attr, delta, origin, &get)?
            }
            "highlight" => {
                let on = args.get_path("on").map(Value::truthy).unwrap_or(self.prog.highlight.is_empty());
                self.prog.highlight(&show.rig, on, origin)
            }
            "locate" => self.prog.locate(&show, origin)?,
            "release" => self.prog.release(origin),
            "clear" => {
                let mut c = self.prog.release(origin);
                c.extend(self.prog.highlight(&show.rig, false, origin));
                self.prog.selection.clear();
                c
            }
            "store" => {
                let project = se_store::Project::open(&self.ctx.project_root).map_err(|e| e.to_string())?;
                let s = |k: &str| args.get_path(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
                let rel = if let Some(p) = s("palette") {
                    programmer::store_palette(&project, &self.prog, &show.rig, &p, s("kind").as_deref())?
                } else if let Some(l) = s("cuelist") {
                    let cue = s("cue").or_else(|| args.get_path("cue").map(|v| v.to_string())).ok_or("needs `cue`")?;
                    programmer::store_cue(&project, &self.prog, &show.rig, &l, &cue)?
                } else if let Some(p) = s("preset") {
                    programmer::store_preset(&project, &self.prog, &show.rig, &p)?
                } else {
                    return Err("store needs `palette`, `cuelist` + `cue`, or `preset`".into());
                };
                self.hub.log("info", "lights", format!("programmer stored to {rel}"));
                Vec::new()
            }
            other => return Err(format!("unknown programmer action `{other}`")),
        };
        self.submit(cmds);
        self.publish_programmer();
        Ok(())
    }

    // ---- bus ---------------------------------------------------------------------------

    /// Reconcile unchanged binding results after a new owner starts, and missed bus changes.
    fn sync_layer_controls(&mut self) {
        let snap = self.hub.snapshot.load_full();
        let changes = self.layers.keys().flat_map(|slot| {
            let snap = &snap;
            ["energy", "brightness", "rhythm"].into_iter().filter_map(move |field| {
                let address = format!("lights.layer.{}.{field}", slot.name());
                snap.get(&address).cloned().map(|value| (address, value))
            })
        }).collect();
        self.bus(&Bus::Changes(changes));
    }

    pub fn bus(&mut self, b: &Bus) {
        match b {
            Bus::Changes(changes) => {
                // live sources resolve from this change set: the snapshot may not include it yet
                let mut live: BTreeMap<&str, &Value> = BTreeMap::new();
                let mut controls: BTreeMap<Slot, Value> = BTreeMap::new();
                for (addr, v) in changes {
                    if let Some(list) = addr.strip_prefix("lights.cuelist.").and_then(|r| r.strip_suffix(".master")) {
                        let m = v.as_f64().unwrap_or(1.0).clamp(0.0, 1.0) as f32;
                        self.master_changed(list, m);
                    } else if let Some((slot, field)) = addr.strip_prefix("lights.layer.").and_then(|s| s.split_once('.')).and_then(|(slot, field)| Slot::parse(slot).ok().map(|s| (s, field)))
                        && ["energy", "brightness", "rhythm"].contains(&field)
                    {
                        let current = self.layers.get(&slot).map(|l| &l.pb.controls).or_else(|| self.pending_layers.get(&slot).map(|l| &l.controls));
                        if current.is_some_and(|c| c.value().get_path(field).and_then(Value::as_f64).zip(v.as_f64()).is_some_and(|(a, b)| (a - b).abs() > 1e-6)) {
                            let args = controls.entry(slot).or_insert_with(|| Value::map().with("layer", slot.name()));
                            *args = args.clone().with(field, v.clone());
                        }
                    } else if self.live.contains(addr) {
                        live.insert(addr, v);
                    }
                }
                for (slot, args) in controls {
                    let owner = self.layers.get(&slot).map(|l| (l.pb.priority, l.pb.origin, l.pb.actor.clone()))
                        .or_else(|| self.pending_layers.get(&slot).map(|l| (l.priority, l.origin, l.actor.clone())));
                    if let Some((priority, origin, actor)) = owner {
                        let args = args.with("priority", priority as i64);
                        let cmd = Command::new(origin, Op::Action { name: "lights.layer.set".into(), args: args.clone() }).with_priority(Some(priority)).with_actor(actor);
                        if let Err(e) = self.update_layer(&args, &cmd, false) { self.hub.log("warn", "lights", format!("live layer control: {e}")); }
                    }
                }
                let fresh: HashMap<String, Value> = live.iter().map(|(a, v)| (a.to_string(), (*v).clone())).collect();
                for src in live.keys() {
                    let snap = self.get();
                    let get = |a: &str| fresh.get(a).cloned().or_else(|| snap(a));
                    let show = self.show.clone();
                    let now = Instant::now();
                    let tokens = &mut self.tokens;
                    let mut outs: Vec<playback::Out> =
                        self.playbacks.values_mut().filter_map(|p| Some(p.refresh(show.lists.get(&p.list)?, &show, Some(src), &get, tokens, now))).collect();
                    outs.extend(self.looks.values_mut().map(|l| l.pb.refresh(&l.list, &show, Some(src), &get, tokens, now)));
                    outs.extend(self.layers.values_mut().map(|l| l.pb.refresh(&l.list, &show, Some(src), &get, tokens, now)));
                    for o in outs {
                        self.out(o);
                    }
                }
            }
            Bus::Event(e) if e.ty == "mode.changed" => {
                let on = e.payload.get_path("to").and_then(Value::as_str) == Some("rehearsal");
                self.shared.rehearsal.store(on, Ordering::Relaxed);
            }
            Bus::Event(e) if e.ty == "lights.flash" => {
                let p = e.origin.default_priority();
                if let Err(err) = self.flash(&e.payload, e.origin, e.actor.clone(), p, Some(e.id)) {
                    self.hub.log("warn", "lights", format!("lights.flash event: {err}"));
                }
            }
            _ => {}
        }
    }

    fn master_changed(&mut self, list: &str, m: f32) {
        let old = self.masters.insert(list.to_string(), m);
        let playing = self.playbacks.get(list).is_some_and(|p| p.current.is_some());
        // fader start: pulled up from zero while stopped
        if !playing && m > 0.0 && old.is_some_and(|o| o <= 0.0) && self.show.lists.get(list).is_some_and(|l| l.fader_start) {
            let p = self.priority(list, false, None);
            if let Err(e) = self.nav(list, None, Nav::Go, None, p, Origin::System, None, None, false) {
                self.hub.log("warn", "lights", format!("fader start `{list}`: {e}"));
            }
            return;
        }
        let Some(pb) = self.playbacks.get_mut(list) else { return };
        pb.master = m;
        let now = Instant::now();
        if pb.last_master_apply.is_none_or(|t| now.duration_since(t) >= MASTER_THROTTLE) {
            let cmds = pb.apply_master(now);
            self.submit(cmds);
        } else if self.master_pending.insert(list.to_string()) {
            let at = pb.last_master_apply.map(|t| t + MASTER_THROTTLE).unwrap_or(now);
            self.arm(at, Timer::Master { list: list.to_string() });
        }
        self.sync_view();
    }

    // ---- timers ------------------------------------------------------------------------

    pub fn next_timer(&self) -> Option<Instant> {
        self.timers.peek().map(|Reverse((at, _))| *at)
    }

    pub fn fire_timers(&mut self) {
        let now = Instant::now();
        while let Some(Reverse((at, id))) = self.timers.peek().copied() {
            if at > now {
                break;
            }
            self.timers.pop();
            let Some(t) = self.timer_data.remove(&id) else { continue };
            self.timer(t);
        }
    }

    fn timer(&mut self, t: Timer) {
        if let Timer::Advance { list, epoch } | Timer::AutoRelease { list, epoch } = &t
            && list.starts_with("layer:")
        {
            let slot = self.layers.iter().find(|(_, l)| l.pb.list == *list && l.pb.epoch == *epoch).map(|(slot, _)| *slot);
            let Some(slot) = slot else { return };
            if matches!(t, Timer::AutoRelease { .. }) {
                self.release_running_layer(slot, None, None);
            } else {
                self.advance_layer(slot);
            }
            self.publish_playbacks();
            return;
        }
        match t {
            Timer::BeatAdvance { list, epoch, beats, boundary, release } => {
                let Some(boundary) = boundary else { return };
                let position = self.beat_position();
                match self.running_mut(&list).and_then(|pb| pb.beat_due(epoch, boundary, position)) {
                    None => {}
                    Some(true) => self.timer(if release { Timer::AutoRelease { list, epoch } } else { Timer::Advance { list, epoch } }),
                    Some(false) => self.arm(Instant::now() + Duration::from_millis(10), Timer::BeatAdvance { list, epoch, beats, boundary: Some(boundary), release }),
                }
            }
            Timer::Entry { list, addr, token } => {
                if let Some(c) = self.running_mut(&list).and_then(|p| p.fire_entry(&addr, token)) {
                    self.hub.command(c);
                }
            }
            Timer::Advance { list, epoch } => {
                let Some(pb) = self.playbacks.get(&list) else { return };
                if pb.epoch != epoch {
                    return;
                }
                let (p, o, a) = (pb.priority, pb.origin, pb.actor.clone());
                let operator = self.operator_playbacks.contains(&list);
                if let Err(e) = self.nav(&list, None, Nav::Go, None, p, o, a, None, operator) {
                    self.hub.log("warn", "lights", format!("auto-advance `{list}`: {e}"));
                }
            }
            Timer::AutoRelease { list, epoch } => {
                if self.playbacks.get(&list).is_some_and(|p| p.epoch == epoch) {
                    self.release_list(&list, None, None);
                    self.publish_playbacks();
                }
            }
            Timer::Release { list, cmds } => {
                for (addr, token, c) in cmds {
                    // skip addresses a restarted playback holds again
                    let reapplied = self.running_mut(&list).and_then(|p| p.applied.get(&addr)).is_some_and(|a| a.token > token);
                    if !reapplied {
                        self.hub.command(c);
                    }
                }
                if list.starts_with("layer:") { self.publish_playbacks(); }
            }
            Timer::Master { list } => {
                self.master_pending.remove(&list);
                if let Some(pb) = self.playbacks.get_mut(&list) {
                    let cmds = pb.apply_master(Instant::now());
                    self.submit(cmds);
                }
            }
            Timer::FlashFade { addrs, ms, token, origin, actor, priority } => {
                if token != self.flash_token {
                    return;
                }
                for a in addrs {
                    let mut c = Command::new(origin, Op::Animate { address: a, to: Value::Float(0.0), ms, ease: se_proto::Ease::OutQuad })
                        .with_priority(Some(priority))
                        .with_actor(actor.clone());
                    if priority > PRIORITY_CHAT {
                        c = c.with_key("flash");
                    }
                    self.hub.command(c);
                }
            }
            Timer::FlashEnd { addrs, token, origin, actor, priority } => {
                if token != self.flash_token {
                    return;
                }
                for a in addrs {
                    let mut c = Command::new(origin, Op::Release { address: a }).with_priority(Some(priority)).with_actor(actor.clone());
                    if priority > PRIORITY_CHAT {
                        c = c.with_key("flash");
                    }
                    self.hub.command(c);
                }
            }
            Timer::LayerStart { slot, generation } => self.poll_layer(slot, generation),
            Timer::LayerExpire { slot, generation } => {
                if self.layers.get(&slot).is_some_and(|l| l.generation == generation) {
                    self.release_running_layer(slot, None, None);
                    self.publish_playbacks();
                }
            }
        }
    }

    // ---- once per second ---------------------------------------------------------------

    pub fn tick(&mut self) {
        self.second += 1;
        // Retry failed writes and capture intention changes even after a missed bus update.
        self.persist();
        // mode.changed sets it at once; this catches a restored or missed mode
        self.shared.rehearsal.store(self.hub.snapshot.load().str("show.mode") == Some("rehearsal"), Ordering::Relaxed);
        let status = self.shared.status.lock().clone();
        let (stats, limited) = {
            let mut m = self.shared.monitor.lock();
            let mon = m.read();
            (mon.stats, mon.frame.limited)
        };
        let enabled: Vec<&output::OutputStatus> = status.iter().filter(|s| s.enabled).collect();
        let failed: Vec<&&output::OutputStatus> = enabled.iter().filter(|s| s.state == "fail").collect();
        let errors = self.show.errors.len() + self.view.read().plan_errors.len();
        let jitter_ms = stats.jitter_p99_us / 1000.0;
        let mut detail: Vec<String> =
            enabled.iter().map(|s| format!("{} {}: {}{}", s.id, s.state, s.detail, if s.held { " (holding its look during rehearsal)" } else { "" })).collect();
        detail.push(format!(
            "{:.1} fps · jitter p99 {:.2} ms (max {:.2} ms) · {}",
            stats.fps,
            jitter_ms,
            stats.jitter_worst_us / 1000.0,
            self.shared.scheduling.lock()
        ));
        if errors > 0 {
            detail.push(format!("{errors} lights config problem(s) — see the Lights view"));
        }
        if !self.show.has_rig {
            detail.push("no lights/rig.toml".into());
        }
        let dead = !self.shared.alive.load(Ordering::Acquire);
        if dead {
            detail.insert(0, "DMX output thread stopped (see the engine log)".into());
        }
        let state = if dead || !failed.is_empty() {
            "fail"
        } else if enabled.is_empty() || errors > 0 || !self.show.has_rig || (stats.samples > 100 && jitter_ms > 2.0) {
            "warn"
        } else {
            "pass"
        };
        let health = Value::map().with("status", state).with("detail", detail.join(" | "));
        if health != self.health {
            if state != self.health.get_path("status").and_then(Value::as_str).unwrap_or("") {
                self.hub.log(
                    if state == "fail" { "error" } else { "info" },
                    "lights",
                    format!("health.dmx {state}: {}", health.get_path("detail").and_then(Value::as_str).unwrap_or("")),
                );
            }
            self.hub.publish("health.dmx", health.clone());
            self.health = health;
        }
        self.publish("lights.output.fps", Value::Float((stats.fps as f64 * 10.0).round() / 10.0));
        self.publish("lights.output.jitter_ms", Value::Float((jitter_ms as f64 * 100.0).round() / 100.0));
        self.publish("lights.output.limited", Value::Int(limited as i64));
        if self.second.is_multiple_of(10) {
            self.publish("lights.output.frames", Value::Int(self.shared.frames.load(Ordering::Relaxed) as i64));
            self.publish("lights.output.alloc_violations", Value::Int(self.shared.alloc_violations.load(Ordering::Relaxed) as i64));
        }
        // RDM discovery once the USB output is up (if enabled)
        if !self.rdm_started && self.show.rig.rdm_on_start && status.iter().any(|s| s.kind == "enttec_pro" && s.state == "ok") {
            self.rdm_started = true;
            self.shared.rdm_request.store(true, Ordering::Release);
        }
        let rdm = self.shared.rdm.lock().clone();
        if rdm.at != 0 && rdm.at != self.rdm_logged {
            self.rdm_logged = rdm.at;
            self.hub.log("info", "lights", format!("RDM: {} {}", rdm.status, rdm.devices.iter().map(|d| d.uid.to_string()).collect::<Vec<_>>().join(", ")));
        }
    }

}

/// Handle for a running lights subsystem: stops the control task and the output thread
/// (sACN announces stream termination) on [`Lights::stop`] or drop.
pub struct Lights {
    pub shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
    shutdown: Arc<tokio::sync::Notify>,
    main_light: Option<tokio::task::JoinHandle<()>>,
}

impl Lights {
    pub fn stop(&mut self) {
        self.shutdown.notify_one();
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Lights {
    fn drop(&mut self) {
        self.stop();
    }
}

static RUNNING: parking_lot::Mutex<Option<Lights>> = parking_lot::Mutex::new(None);

/// Start lights for the engine (kept running until [`stop`]). Returns quickly.
pub async fn start(ctx: EngineCtx) -> anyhow::Result<()> {
    let old = RUNNING.lock().take();
    if let Some(old) = old { stop_instance(old).await; }
    let lights = run(ctx).await?;
    *RUNNING.lock() = Some(lights);
    Ok(())
}

/// Stop the engine's lights (daemon shutdown).
pub async fn stop() {
    let lights = RUNNING.lock().take();
    if let Some(lights) = lights { stop_instance(lights).await; }
}

async fn stop_instance(mut lights: Lights) {
    let main_light = lights.main_light.take();
    let _ = tokio::task::spawn_blocking(move || lights.stop()).await;
    if let Some(main_light) = main_light && let Err(error) = main_light.await {
        tracing::error!("main-light shutdown: {error}");
    }
}

fn cid(db: &se_store::Db) -> [u8; 16] {
    if let Ok(Some(Value::Str(s))) = db.kv_get("lights", "sacn_cid")
        && s.len() == 32
    {
        let mut c = [0u8; 16];
        if (0..16).all(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map(|b| c[i] = b).is_ok()) {
            return c;
        }
    }
    let mut c = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut c)).is_err() {
        let t = se_clock::wall_now_ns() as u128;
        c.copy_from_slice(&t.to_le_bytes());
    }
    // RFC 4122 version 4 layout
    c[6] = (c[6] & 0x0F) | 0x40;
    c[8] = (c[8] & 0x3F) | 0x80;
    let hex: String = c.iter().map(|b| format!("{b:02x}")).collect();
    let _ = db.kv_set("lights", "sacn_cid", &Value::Str(hex));
    c
}

/// Start an independent lights instance (output thread, control task, queries) and return
/// its handle.
pub async fn run(ctx: EngineCtx) -> anyhow::Result<Lights> {
    let hub = ctx.hub.clone();
    let show = Arc::new(Show::load(&ctx.config.borrow().clone(), &Show::empty()));
    for e in &show.errors {
        hub.log("error", "lights", e.clone());
    }
    let (plan, plan_errors) = Plan::build(show.rig.clone(), show.effects.clone());
    let (monitor_in, monitor_out) = triple_buffer::TripleBuffer::new(&Monitor::default()).split();
    // ≥ 44 snapshots/s retire; drained every 250 ms
    let (retire, retired) = rtrb::RingBuffer::new(256);
    let shared = Arc::new(Shared {
        plan: arc_swap::ArcSwap::from_pointee(plan),
        monitor: parking_lot::Mutex::new(monitor_out),
        status: parking_lot::Mutex::new(Vec::new()),
        rdm: parking_lot::Mutex::new(output::RdmReport::default()),
        rdm_request: Default::default(),
        stop: Default::default(),
        scheduling: parking_lot::Mutex::new(String::new()),
        frames: Default::default(),
        rehearsal: Default::default(),
        output_sent: Default::default(),
        output_in_use: Default::default(),
        alloc_violations: Default::default(),
        alive: std::sync::atomic::AtomicBool::new(true),
        cid: cid(&ctx.db),
        retired: parking_lot::Mutex::new(retired),
    });
    let thread = output::spawn(hub.clone(), shared.clone(), monitor_in, retire)?;
    let view = Arc::new(RwLock::new(View { show: show.clone(), plan_errors, playbacks: BTreeMap::new(), looks: BTreeMap::new(), prog: Programmer::default(), layers: Value::map(), layer_expiry: BTreeMap::new() }));
    crate::query::register(&hub, view.clone(), shared.clone());
    let panic_latched = ctx.db.kv_get("lights", "panic_latched").ok().flatten().is_some_and(|v| v.truthy());
    let saved_playbacks = ctx.db.kv_get("lights", "playbacks").ok().flatten().unwrap_or(Value::Null);
    let mut ctl = Ctl {
        hub: hub.clone(),
        ctx: ctx.clone(),
        show,
        shared: shared.clone(),
        view,
        playbacks: BTreeMap::new(),
        operator_playbacks: BTreeSet::new(),
        looks: BTreeMap::new(),
        layers: BTreeMap::new(),
        pending_layers: BTreeMap::new(),
        layer_seq: 0,
        clock: crate::effects::BeatClock::default(),
        clock_at: Instant::now(),
        prog: Programmer::default(),
        timers: BinaryHeap::new(),
        timer_data: HashMap::new(),
        timer_seq: 0,
        tokens: 0,
        prefixes: BTreeSet::new(),
        masters: HashMap::new(),
        master_pending: BTreeSet::new(),
        flash_token: 0,
        rdm_started: false,
        rdm_logged: 0,
        health: Value::Null,
        published: HashMap::new(),
        persisted: Value::Null,
        second: 0,
        live: Default::default(),
        panic_latched,
        picks: BTreeMap::new(),
        pick_rng: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0) | 1,
    };
    ctl.declare();
    ctl.publish_programmer();
    let mut actions = hub.route_actions("lights");
    let mut bus = hub.subscribe();
    let mut config = ctx.config.clone();
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let stop = shutdown.clone();
    tokio::spawn(async move {
        // Let declarations land before initializing resolved playback masters.
        tokio::time::sleep(Duration::from_millis(100)).await;
        {
            let snap = ctl.hub.snapshot.load();
            for l in ctl.show.lists.keys() {
                if let Some(m) = snap.f32(&format!("lights.cuelist.{l}.master")) {
                    ctl.masters.insert(l.clone(), m);
                }
            }
        }
        if ctl.panic_latched { ctl.panic(None); }
        ctl.restore_playbacks(saved_playbacks);
        let mut sweep = tokio::time::interval(Duration::from_millis(250));
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let next = ctl.next_timer().map(tokio::time::Instant::from_std).unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));
            tokio::select! {
                a = actions.recv() => match a {
                    Some(cmd) => ctl.action(cmd),
                    None => break,
                },
                m = bus.recv() => match m {
                    Ok(b) => ctl.bus(&b),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => ctl.hub.log("warn", "lights", format!("bus lagged by {n} messages")),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                c = config.changed() => match c {
                    Ok(()) => ctl.reload(),
                    Err(_) => break,
                },
                _ = tokio::time::sleep_until(next) => ctl.fire_timers(),
                _ = sweep.tick() => {
                    {
                        let mut r = ctl.shared.retired.lock();
                        while r.pop().is_ok() {}
                    }
                    ctl.sync_layer_controls();
                }
                _ = tick.tick() => ctl.tick(),
                _ = stop.notified() => break,
            }
        }
        tracing::info!("lights control task stopped");
    });
    let main_light = crate::main_light::spawn(hub, shared.clone());
    Ok(Lights { shared, thread: Some(thread), shutdown, main_light: Some(main_light) })
}
