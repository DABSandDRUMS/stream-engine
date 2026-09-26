//! The lights control task: config (hot reload with last-good fallback), declarations,
//! actions (`lights.*`), the `lights.flash` event, playback timers, playback masters and fader
//! start, held looks (palettes run as one-cue playbacks), live palette following, health
//! (`health.dmx`), persistence, and UI queries.

use crate::cuelist::{self, CueList};
use crate::engine::Plan;
use crate::knobs;
use crate::output::{self, Monitor, Shared};
use crate::playback::{self, Nav, Playback, Timer};
use crate::programmer::{self, Programmer};
use crate::show::Show;
use parking_lot::RwLock;
use se_hub::{Bus, EngineCtx, Hub};
use se_proto::{Actor, Command, Event, Id, Op, Origin, PRIORITY_CHAT, PRIORITY_MANUAL, PRIORITY_PRESET, Value};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Cue list used as the safe look by Panic (falls back to `[safety] safe_look`).
const SAFE_LIST: &str = "safe";

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
}

/// A held look: a palette run as a one-cue playback. The synthetic list is built here on the
/// control task (never on the output thread) and rebuilt when the palette changes.
struct Look {
    pb: Playback,
    list: CueList,
}

pub struct Ctl {
    pub hub: Arc<Hub>,
    ctx: EngineCtx,
    pub show: Arc<Show>,
    shared: Arc<Shared>,
    view: Arc<RwLock<View>>,
    playbacks: BTreeMap<String, Playback>,
    /// Running looks by palette name.
    looks: BTreeMap<String, Look>,
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
        Some(Value::Float(f)) if *f >= 0.0 => Ok(Some(f.round() as u64)),
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
        for (at, t) in o.timers {
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
    }

    /// The playback a timer refers to (cue list name or [`playback::look_id`]).
    fn running_mut(&mut self, id: &str) -> Option<&mut Playback> {
        match id.strip_prefix("look:") {
            Some(n) => self.looks.get_mut(n).map(|l| &mut l.pb),
            None => self.playbacks.get_mut(id),
        }
    }

    fn publish_playbacks(&mut self) {
        self.live = self.playbacks.values().chain(self.looks.values().map(|l| &l.pb)).flat_map(|p| p.applied.values().filter_map(|a| a.live.clone())).collect();
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
        self.persist();
        self.sync_view();
    }

    fn publish_programmer(&mut self) {
        self.publish("lights.programmer.selection", Value::List(self.prog.selection.iter().cloned().map(Value::Str).collect()));
        self.publish("lights.programmer.highlight", Value::Bool(!self.prog.highlight.is_empty()));
        self.publish("lights.programmer.active", Value::Bool(!self.prog.values.is_empty()));
        self.sync_view();
    }

    /// Remember running playbacks for restart (§17.3).
    fn persist(&mut self) {
        let m: BTreeMap<String, Value> = self
            .playbacks
            .iter()
            .filter_map(|(n, p)| {
                let l = self.show.lists.get(n)?;
                let c = l.cues.get(p.current?)?;
                Some((n.clone(), Value::map().with("cue", c.id.clone()).with("priority", p.priority as i64)))
            })
            .collect();
        let v = Value::Map(m);
        if v != self.persisted {
            if let Err(e) = self.ctx.db.kv_set("lights", "playbacks", &v) {
                self.hub.log("warn", "lights", format!("persisting playbacks: {e}"));
            }
            self.persisted = v;
        }
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
            pb.origin = origin;
            pb.actor = actor;
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
        let o = pb.goto(&l, k, nav, fade, &show, &get, &mut tokens, Instant::now());
        self.tokens = tokens;
        self.out(o);
        self.hub.emit(Event::new("lights.cue.go", origin, Value::map().with("cuelist", list).with("cue", l.cues[k].id.clone())).with_causal(cause));
        self.publish_playbacks();
        Ok(())
    }

    fn release_list(&mut self, list: &str, fade: Option<u64>, cause: Option<Id>) -> bool {
        let Some(mut p) = self.playbacks.remove(list) else { return false };
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
        self.publish_playbacks();
    }

    /// Panic: everything off, programmer cleared, safe look on top (§2.8, §9.1).
    fn panic(&mut self, cause: Option<Id>) {
        self.panic_latched = true;
        self.release_all(Some(0), cause);
        let mut cmds = self.prog.release(Origin::System);
        cmds.extend(self.prog.highlight(&self.show.rig.clone(), false, Origin::System));
        self.prog.selection.clear();
        self.flash_token += 1;
        self.submit(cmds);
        if self.show.lists.contains_key(SAFE_LIST) {
            let p = self.priority(SAFE_LIST, false, None).max(PRIORITY_MANUAL);
            if let Err(e) = self.nav(SAFE_LIST, None, Nav::Goto, Some(0), p, Origin::System, None, cause) {
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
        match name {
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
                self.nav(&list, cue.as_deref(), Nav::Goto, fade, p, cmd.origin, cmd.actor.clone(), cause)
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
                self.nav(&list, if nav == Nav::Go || nav == Nav::Back { None } else { cue.as_deref() }, nav, fade, p, cmd.origin, cmd.actor.clone(), cause)
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
                    Some(l) if l == SAFE_LIST && !self.show.lists.contains_key(SAFE_LIST) => self.release_builtin_safe(cause),
                    Some(l) => {
                        if !self.release_list(&l, fade, cause) && !self.show.lists.contains_key(&l) {
                            return Err(format!("unknown cue list `{l}`"));
                        }
                        self.publish_playbacks();
                    }
                    None => self.release_all(fade, cause),
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

    pub fn bus(&mut self, b: &Bus) {
        match b {
            Bus::Changes(changes) => {
                // live sources resolve from this change set: the snapshot may not include it yet
                let mut live: BTreeMap<&str, &Value> = BTreeMap::new();
                for (addr, v) in changes {
                    if let Some(list) = addr.strip_prefix("lights.cuelist.").and_then(|r| r.strip_suffix(".master")) {
                        let m = v.as_f64().unwrap_or(1.0).clamp(0.0, 1.0) as f32;
                        self.master_changed(list, m);
                    } else if self.live.contains(addr) {
                        live.insert(addr, v);
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
            if let Err(e) = self.nav(list, None, Nav::Go, None, p, Origin::System, None, None) {
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
        match t {
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
                if let Err(e) = self.nav(&list, None, Nav::Go, None, p, o, a, None) {
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
        }
    }

    // ---- once per second ---------------------------------------------------------------

    pub fn tick(&mut self) {
        self.second += 1;
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

    /// Restore playbacks that were running before a restart (§17.3).
    fn restore(&mut self, saved: Option<Value>) {
        let Some(Value::Map(m)) = saved else { return };
        for (list, v) in m {
            let Some(cue) = v.get_path("cue").and_then(Value::as_str) else { continue };
            let p = v.get_path("priority").and_then(Value::as_i64).unwrap_or(PRIORITY_PRESET as i64) as u16;
            match self.nav(&list, Some(cue), Nav::Goto, Some(0), p, Origin::System, None, None) {
                Ok(()) => self.hub.log("info", "lights", format!("restored cue list `{list}` at cue `{cue}`")),
                Err(e) => self.hub.log("warn", "lights", format!("restore `{list}`: {e}")),
            }
        }
    }
}

/// Handle for a running lights subsystem: stops the control task and the output thread
/// (sACN announces stream termination) on [`Lights::stop`] or drop.
pub struct Lights {
    pub shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
    shutdown: Arc<tokio::sync::Notify>,
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
    let l = run(ctx).await?;
    if let Some(mut old) = RUNNING.lock().replace(l) {
        old.stop();
    }
    Ok(())
}

/// Stop the engine's lights (daemon shutdown).
pub async fn stop() {
    let l = RUNNING.lock().take();
    if let Some(mut l) = l {
        let _ = tokio::task::spawn_blocking(move || l.stop()).await;
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
        alloc_violations: Default::default(),
        alive: std::sync::atomic::AtomicBool::new(true),
        cid: cid(&ctx.db),
        retired: parking_lot::Mutex::new(retired),
    });
    let thread = output::spawn(hub.clone(), shared.clone(), monitor_in, retire)?;
    let view = Arc::new(RwLock::new(View { show: show.clone(), plan_errors, playbacks: BTreeMap::new(), looks: BTreeMap::new(), prog: Programmer::default() }));
    crate::query::register(&hub, view.clone(), shared.clone());
    let mut ctl = Ctl {
        hub: hub.clone(),
        ctx: ctx.clone(),
        show,
        shared: shared.clone(),
        view,
        playbacks: BTreeMap::new(),
        looks: BTreeMap::new(),
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
        panic_latched: false,
    };
    // read before the first publish persists the (still empty) playback set
    let saved = ctx.db.kv_get("lights", "playbacks").ok().flatten();
    ctl.declare();
    ctl.publish_playbacks();
    ctl.publish_programmer();
    let mut actions = hub.route_actions("lights");
    let mut bus = hub.subscribe();
    let mut config = ctx.config.clone();
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let stop = shutdown.clone();
    tokio::spawn(async move {
        // let declarations land before restoring overrides onto them
        tokio::time::sleep(Duration::from_millis(100)).await;
        {
            let snap = ctl.hub.snapshot.load();
            for l in ctl.show.lists.keys() {
                if let Some(m) = snap.f32(&format!("lights.cuelist.{l}.master")) {
                    ctl.masters.insert(l.clone(), m);
                }
            }
        }
        ctl.restore(saved);
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
                    let mut r = ctl.shared.retired.lock();
                    while r.pop().is_ok() {}
                }
                _ = tick.tick() => ctl.tick(),
                _ = stop.notified() => break,
            }
        }
        tracing::info!("lights control task stopped");
    });
    Ok(Lights { shared, thread: Some(thread), shutdown })
}
