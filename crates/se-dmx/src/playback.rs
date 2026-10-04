//! Playbacks (§9.2): run cue lists into the core as overrides keyed `cuelist:<cl>` at the
//! playback's priority, so intensity merges HTP and everything else LTP through the normal
//! resolver. Fades are core animations; delays, follows, and waits are control-task timers.
//! Intensities (and effect sizes) are scaled by the playback master
//! (`lights.cuelist.<cl>.master`).

use crate::cuelist::{self, CueList, State, Tracked};
use crate::foundation::Controls;
use crate::palette::{self, Spec};
use crate::rig::AttrKind;
use crate::show::Show;
use se_proto::{Actor, Command, Ease, Id, Op, Origin, PRIORITY_CHAT, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// Override key for a cue list's layer.
pub fn key(list: &str) -> String {
    format!("cuelist:{list}")
}

/// Playback id and override key of a held look (a palette run as a one-cue playback). Cue list
/// names are plain address segments, so a `:` never collides with them.
pub fn look_id(palette: &str) -> String {
    format!("look:{palette}")
}

/// What a playback currently holds on one address.
#[derive(Clone, Debug)]
pub struct Applied {
    /// Resolved value before master scaling.
    pub value: Value,
    pub scaled: bool,
    pub spec: Spec,
    pub head: Option<usize>,
    pub attr: String,
    pub kind: Option<AttrKind>,
    /// Live source address (stream palette / `@address`).
    pub live: Option<String>,
    /// When the current fade ends (for master re-scaling mid-fade).
    pub fade_end: Instant,
    pub ease: Ease,
    /// Waiting for its delay; the fade to run when it fires.
    pub pending: Option<u64>,
    pub token: u64,
}

pub struct Playback {
    /// Cue list name, or [`look_id`] for a held look (timers refer to the playback by it).
    pub list: String,
    /// Override key of the playback's layer.
    pub key: String,
    pub priority: u16,
    pub origin: Origin,
    pub actor: Option<Actor>,
    pub cause: Option<Id>,
    pub current: Option<usize>,
    pub applied: BTreeMap<String, Applied>,
    pub go_at: Instant,
    pub total_ms: u64,
    /// Invalidates advance timers after manual navigation.
    pub epoch: u64,
    pub master: f32,
    pub last_master_apply: Option<Instant>,
    /// Shared controls apply only to foundation-owned playbacks.
    pub controls: Controls,
    pub coverage: Option<Vec<bool>>,
    pub beat_rates: BTreeSet<String>,
    pub scoped_chat_key: bool,
    /// An auto-advance carries the prior exact musical boundary, not its delayed poll time.
    pub beat_anchor: Option<f64>,
}

/// Timers owned by the control task.
#[derive(Clone, Debug)]
pub enum Timer {
    /// A delayed entry starts its fade.
    Entry {
        list: String,
        addr: String,
        token: u64,
    },
    /// Follow/wait auto-advance.
    Advance {
        list: String,
        epoch: u64,
    },
    /// Autorelease at the end of a non-looping list.
    AutoRelease {
        list: String,
        epoch: u64,
    },
    /// Musical auto-advance (or terminal autorelease), polled on the shared beat clock.
    BeatAdvance { list: String, epoch: u64, beats: f64, boundary: Option<f64>, release: bool },
    /// Release overrides after a fade-out (skipped per address if re-applied since).
    Release {
        list: String,
        cmds: Vec<(String, u64, Command)>,
    },
    /// Apply a pending master change (throttled).
    Master {
        list: String,
    },
    /// Flash: fade out after the hold, then release.
    FlashFade {
        addrs: Vec<String>,
        ms: u32,
        token: u64,
        origin: Origin,
        actor: Option<Actor>,
        priority: u16,
    },
    FlashEnd {
        addrs: Vec<String>,
        token: u64,
        origin: Origin,
        actor: Option<Actor>,
        priority: u16,
    },
    /// Poll a quantized selection against the shared musical clock.
    LayerStart { slot: crate::foundation::Slot, generation: u64 },
    /// Finite layer lifetime; never releases a replacement generation.
    LayerExpire { slot: crate::foundation::Slot, generation: u64 },
}

/// How a cue is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    /// Next cue: only this cue's changes are applied (tracking).
    Go,
    /// Jump with the full tracked state; arms follow/wait.
    Goto,
    /// Previous cue with the full tracked state.
    Back,
    /// Full tracked state, no follow/wait (timelines own progression).
    Locate,
}

/// Commands produced by playback operations plus timers to arm.
#[derive(Default)]
pub struct Out {
    pub cmds: Vec<Command>,
    pub timers: Vec<(Instant, Timer)>,
    pub errors: Vec<String>,
}

impl Playback {
    pub fn new(list: &str, priority: u16, origin: Origin, actor: Option<Actor>, cause: Option<Id>, master: f32) -> Playback {
        Playback {
            list: list.into(),
            key: key(list),
            priority,
            origin,
            actor,
            cause,
            current: None,
            applied: BTreeMap::new(),
            go_at: Instant::now(),
            total_ms: 0,
            epoch: 0,
            master,
            last_master_apply: None,
            controls: Controls::default(),
            coverage: None,
            beat_rates: BTreeSet::new(),
            scoped_chat_key: false,
            beat_anchor: None,
        }
    }

    /// A held look: the one-cue list from [`CueList::look`] run at full master under the
    /// override key `look:<palette>`.
    pub fn look(palette: &str, priority: u16, origin: Origin, actor: Option<Actor>, cause: Option<Id>) -> Playback {
        let id = look_id(palette);
        Playback { key: id.clone(), ..Playback::new(&id, priority, origin, actor, cause, 1.0) }
    }

    /// A uniquely owned layer; chat keys remain actor-scoped by the core resolver.
    pub fn layer(id: &str, priority: u16, origin: Origin, actor: Option<Actor>, cause: Option<Id>, controls: Controls, show: &Show) -> Self {
        let coverage = controls.mask(show).expect("controls validated before playback");
        let beat_rates = show.effects.iter().filter(|e| e.unit == crate::effects::Unit::Beats).map(|e| format!("lights.effect.{}.rate", e.name)).collect();
        Self { key: id.into(), controls, coverage: Some(coverage), beat_rates, scoped_chat_key: true, ..Self::new(id, priority, origin, actor, cause, 1.0) }
    }

    fn filter_values(&self, values: &mut BTreeMap<String, Tracked>, _show: &Show) {
        let Some(mask) = &self.coverage else { return };
        values.retain(|_, tr| tr.head.is_none_or(|(h, _)| mask.get(h).copied().unwrap_or(false)));
    }

    fn effect_values(&self, name: &str, size: f32, rate: Option<f32>, cue: usize, show: &Show) -> Vec<(String, Tracked)> {
        let mut entries = effect_entries(name, size, rate, cue);
        if let Some(mask) = &self.coverage
            && let Some(effect) = show.effects.iter().find(|e| e.name == name)
            && let Ok(heads) = effect.layout(&show.rig)
        {
            for (h, _) in heads {
                let capable = match effect.kind {
                    crate::effects::Kind::Circle => show.rig.heads[h].attr("pan").is_some() && show.rig.heads[h].attr("tilt").is_some(),
                    crate::effects::Kind::ColorChase | crate::effects::Kind::ColorWave | crate::effects::Kind::Rainbow | crate::effects::Kind::FollowColor => show.rig.heads[h].attr("color").is_some(),
                    _ => show.rig.heads[h].attr("intensity").is_some(),
                };
                let attr = format!("lights.effect.{name}.coverage.{}", show.rig.heads[h].id);
                entries.push((attr.clone(), Tracked { entry: cuelist::Entry { spec: Spec::Literal(Value::Bool(capable && mask.get(h).copied().unwrap_or(false))), fade: Some(0), delay: Some(0) }, head: None, attr, cue }));
            }
        }
        entries
    }

    fn cmd(&self, op: Op) -> Command {
        let mut c = Command::new(self.origin, op).with_priority(Some(self.priority)).with_actor(self.actor.clone()).caused_by(self.cause);
        if self.priority > PRIORITY_CHAT || self.scoped_chat_key {
            c = c.with_key(self.key.clone());
        }
        c
    }

    fn send(&self, addr: &str, value: &Value, fade_ms: u64, ease: Ease) -> Command {
        if fade_ms == 0 || (self.scoped_chat_key || addr.starts_with("lights.effect.")) && matches!(value, Value::Bool(_)) {
            self.cmd(Op::Set { address: addr.into(), value: value.clone() })
        } else {
            self.cmd(Op::Animate { address: addr.into(), to: value.clone(), ms: fade_ms.min(u32::MAX as u64) as u32, ease })
        }
    }

    fn scaled_value(&self, a: &Applied) -> Value {
        let factor = if a.kind == Some(AttrKind::Intensity) {
            self.master * self.controls.brightness
        } else if a.scaled {
            self.master * self.controls.energy
        } else if self.beat_rates.contains(&a.attr) {
            self.controls.rhythm
        } else {
            return a.value.clone();
        };
        Value::Float(a.value.as_f64().unwrap_or(0.0) * factor as f64)
    }

    pub fn progress(&self, now: Instant) -> (f32, u64) {
        let el = now.saturating_duration_since(self.go_at).as_millis() as u64;
        if self.total_ms == 0 {
            return (1.0, el);
        }
        ((el as f32 / self.total_ms as f32).min(1.0), el)
    }

    /// Late polls retain the authored boundary, preventing musical loop drift.
    pub fn beat_due(&mut self, epoch: u64, boundary: f64, position: f64) -> Option<bool> {
        if self.current.is_none() || self.epoch != epoch { return None; }
        let due = position >= boundary;
        if due { self.beat_anchor = Some(boundary); }
        Some(due)
    }

    /// Move to cue `k` of `list`.
    #[allow(clippy::too_many_arguments)]
    pub fn goto(
        &mut self,
        list: &CueList,
        k: usize,
        nav: Nav,
        fade_override: Option<u64>,
        show: &Show,
        get: &dyn Fn(&str) -> Option<Value>,
        tokens: &mut u64,
        now: Instant,
    ) -> Out {
        let mut out = Out::default();
        let cue = &list.cues[k];
        let target: State = cuelist::tracked(list, k, &show.rig, &show.palettes);
        let prev = self.current;
        let sequential = nav == Nav::Go && prev.is_some_and(|p| p + 1 == k || (list.looped && p + 1 == list.cues.len() && k == 0));
        // Only this cue's changes on a sequential go (tracking); otherwise the full state.
        let explicit: Option<BTreeMap<String, Tracked>> = (sequential && list.tracking && !cue.block).then(|| {
            let mut sink = Vec::new();
            let mut m = cuelist::expand(cue, k, &show.rig, &show.palettes, &mut sink);
            for (e, size, rate) in &cue.effects {
                for (a, t) in self.effect_values(e, size.unwrap_or_else(|| default_size(show, e)), *rate, k, show) {
                    m.insert(a, t);
                }
            }
            self.filter_values(&mut m, show);
            m
        });
        let released: Vec<String> = if explicit.is_some() {
            let mut r = cuelist::released(cue, &show.rig);
            for s in &cue.stop_effects {
                let names: Vec<String> = if s == "all" {
                    self.applied.keys().filter_map(|a| a.strip_prefix("lights.effect.").and_then(|x| x.split('.').next()).map(String::from)).collect()
                } else {
                    vec![s.clone()]
                };
                for n in names {
                    for p in ["active", "size", "rate"] {
                        r.push(format!("lights.effect.{n}.{p}"));
                    }
                    r.extend(self.applied.keys().filter(|a| a.starts_with(&format!("lights.effect.{n}.coverage."))).cloned());
                }
            }
            r
        } else {
            Vec::new()
        };
        let mut values = target.values.clone();
        for (e, (size, rate)) in &target.effects {
            values.extend(self.effect_values(e, size.unwrap_or_else(|| default_size(show, e)), *rate, k, show));
        }
        self.filter_values(&mut values, show);
        let to_apply: Vec<(String, Tracked)> = match &explicit {
            Some(ex) => ex.clone().into_iter().collect(),
            None => values.clone().into_iter().collect(),
        };
        let to_release: Vec<String> = match &explicit {
            Some(_) => released.into_iter().filter(|a| self.applied.contains_key(a) && !values.contains_key(a)).collect(),
            None => self.applied.keys().filter(|a| !values.contains_key(*a)).cloned().collect(),
        };
        let back_fade = if nav == Nav::Back { list.back_fade_ms } else { None };
        let fade_override = fade_override.or(back_fade);
        let mut end = 0u64;
        for (addr, tr) in to_apply {
            let kind = tr.head.map(|(_, k)| k).or_else(|| effect_kind(&addr));
            let value = match &tr.head {
                Some((h, k)) => palette::resolve(&tr.entry.spec, &show.palettes, &show.rig, *h, &tr.attr, *k, get),
                None => match &tr.entry.spec {
                    Spec::Literal(v) => Ok(v.clone()),
                    Spec::Address(a) => get(a).ok_or_else(|| format!("`{a}` has no value")),
                    Spec::Palette(p) => Err(format!("palette `{p}` on a plain address")),
                },
            };
            let live = match &tr.head {
                Some((h, _)) => tr.entry.spec.live(&show.palettes, &show.rig, *h, &tr.attr).map(String::from),
                None => match &tr.entry.spec {
                    Spec::Address(a) => Some(a.clone()),
                    _ => None,
                },
            };
            let value = match value {
                Ok(v) => v,
                Err(e) => {
                    out.errors.push(format!("cue list `{}` cue `{}`: {addr}: {e}", list.name, cue.id));
                    if let Some(src) = live {
                        // a live source (stream palette, `@address`) without a value yet: keep
                        // the entry and apply it as soon as the source publishes
                        *tokens += 1;
                        let scaled = matches!(kind, Some(AttrKind::Intensity));
                        self.applied.insert(
                            addr,
                            Applied {
                                value: Value::Null,
                                scaled,
                                spec: tr.entry.spec.clone(),
                                head: tr.head.map(|(h, _)| h),
                                attr: tr.attr.clone(),
                                kind,
                                live: Some(src),
                                fade_end: now,
                                ease: cue.ease,
                                pending: None,
                                token: *tokens,
                            },
                        );
                    }
                    continue;
                }
            };
            let before = self.applied.get(&addr).map(|a| a.value.clone()).or_else(|| get(&addr));
            let down = kind == Some(AttrKind::Intensity) && before.and_then(|b| b.as_f64()).zip(value.as_f64()).is_some_and(|(b, v)| v < b);
            let delay = if fade_override.is_some() { 0 } else { tr.entry.delay.unwrap_or(cue.delay_ms) };
            let fade = fade_override.unwrap_or_else(|| tr.entry.fade.unwrap_or(cue.fade_for(down)));
            if fade > 0 && addr.starts_with("lights.effect.") && addr.ends_with(".size")
                && !self.applied.contains_key(&addr)
            {
                let active = addr.strip_suffix("size").map(|prefix| format!("{prefix}active")).expect("effect size");
                let from = if get(&active).is_some_and(|v| v.truthy()) {
                    get(&addr).unwrap_or(Value::Float(0.0))
                } else {
                    Value::Float(0.0)
                };
                out.cmds.push(self.send(&addr, &from, 0, cue.ease));
            }
            end = end.max(delay + fade);
            *tokens += 1;
            let a = Applied {
                value,
                scaled: matches!(kind, Some(AttrKind::Intensity)) || addr.ends_with(".size") && addr.starts_with("lights.effect."),
                spec: tr.entry.spec.clone(),
                head: tr.head.map(|(h, _)| h),
                attr: tr.attr.clone(),
                kind,
                live,
                fade_end: now + Duration::from_millis(delay + fade),
                ease: cue.ease,
                pending: (delay > 0).then_some(fade),
                token: *tokens,
            };
            if delay > 0 {
                out.timers.push((now + Duration::from_millis(delay), Timer::Entry { list: self.list.clone(), addr: addr.clone(), token: a.token }));
            } else {
                out.cmds.push(self.send(&addr, &self.scaled_value(&a), if kind == Some(AttrKind::Index) { 0 } else { fade }, a.ease));
            }
            self.applied.insert(addr, a);
        }
        let release_fade = fade_override.unwrap_or(cue.fade_for(true));
        let mut later = Vec::new();
        let retiring_effects = to_release.iter().filter_map(|addr| addr.strip_prefix("lights.effect.").and_then(|s| s.strip_suffix(".size"))).collect::<BTreeSet<_>>();
        for addr in &to_release {
            if let Some(a) = self.applied.remove(addr) {
                let effect_tail = addr.strip_prefix("lights.effect.").and_then(|s| s.split('.').next())
                    .is_some_and(|name| retiring_effects.contains(name));
                if release_fade > 0 && (a.scaled || effect_tail) {
                    if a.scaled { out.cmds.push(self.send(&addr, &Value::Float(0.0), release_fade, cue.ease)); }
                    later.push((addr.clone(), a.token, self.cmd(Op::Release { address: addr.clone() })));
                } else {
                    out.cmds.push(self.cmd(Op::Release { address: addr.clone() }));
                }
            }
        }
        if !later.is_empty() {
            out.timers.push((now + Duration::from_millis(release_fade), Timer::Release { list: self.list.clone(), cmds: later }));
            end = end.max(release_fade);
        }
        self.current = Some(k);
        self.go_at = now;
        self.total_ms = end;
        self.epoch += 1;
        if nav != Nav::Locate {
            let last = k + 1 == list.cues.len();
            if let Some(beats) = cue.wait_beats {
                if !last || list.looped || list.autorelease {
                    out.timers.push((now + Duration::from_millis(10), Timer::BeatAdvance {
                        list: self.list.clone(), epoch: self.epoch, beats,
                        boundary: self.beat_anchor.take().map(|start| start + beats),
                        release: last && !list.looped,
                    }));
                }
            } else if let Some(w) = cue.wait_ms {
                if !last || list.looped {
                    out.timers.push((now + Duration::from_millis(w), Timer::Advance { list: self.list.clone(), epoch: self.epoch }));
                }
            } else if let Some(f) = cue.follow_ms
                && (!last || list.looped)
            {
                out.timers.push((now + Duration::from_millis(end + f), Timer::Advance { list: self.list.clone(), epoch: self.epoch }));
            }
            if last && !list.looped && list.autorelease && cue.wait_beats.is_none() {
                out.timers
                    .push((now + Duration::from_millis(end + cue.follow_ms.unwrap_or(0)), Timer::AutoRelease { list: self.list.clone(), epoch: self.epoch }));
            }
        }
        out
    }

    /// A delayed entry's time has come.
    pub fn fire_entry(&mut self, addr: &str, token: u64) -> Option<Command> {
        let a = self.applied.get_mut(addr)?;
        if a.token != token {
            return None;
        }
        let fade = a.pending.take()?;
        let a = a.clone();
        Some(self.send(addr, &self.scaled_value(&a), if a.kind == Some(AttrKind::Index) { 0 } else { fade }, a.ease))
    }

    /// Re-issue scaled values after a master change (keeps fades in progress).
    pub fn apply_master(&mut self, now: Instant) -> Vec<Command> {
        self.last_master_apply = Some(now);
        let mut v = Vec::new();
        for (addr, a) in &self.applied {
            if !a.scaled || a.pending.is_some() || a.value.is_null() {
                continue;
            }
            let remaining = a.fade_end.saturating_duration_since(now).as_millis() as u64;
            v.push(self.send(addr, &self.scaled_value(a), remaining, a.ease));
        }
        v
    }

    /// Re-resolve values after a palette/cue edit (a knob) or a live source change; returns
    /// commands for values that changed (short crossfade). `list` is the playback's (current)
    /// list. Effect rates/sizes and other plain addresses the cue now holds but didn't before
    /// (a knob giving the effect its own rate) are taken on as well.
    pub fn refresh(
        &mut self,
        list: &CueList,
        show: &Show,
        only_live: Option<&str>,
        get: &dyn Fn(&str) -> Option<Value>,
        tokens: &mut u64,
        now: Instant,
    ) -> Out {
        let mut out = Out::default();
        let Some(k) = self.current else { return out };
        if k >= list.cues.len() {
            return out;
        }
        let target = cuelist::tracked(list, k, &show.rig, &show.palettes);
        let mut values = target.values;
        for (e, (size, rate)) in &target.effects {
            values.extend(self.effect_values(e, size.unwrap_or_else(|| default_size(show, e)), *rate, k, show));
        }
        self.filter_values(&mut values, show);
        if only_live.is_none() {
            let obsolete: Vec<_> = self.applied.keys().filter(|addr| !values.contains_key(*addr)).cloned().collect();
            for addr in obsolete {
                self.applied.remove(&addr);
                out.cmds.push(self.cmd(Op::Release { address: addr }));
            }
        }
        for (addr, tr) in values {
            let held = self.applied.get(&addr);
            if held.is_some_and(|a| a.pending.is_some()) {
                continue;
            }
            if let Some(src) = only_live
                && held.and_then(|a| a.live.as_deref()) != Some(src)
            {
                continue;
            }
            let (v, live) = match tr.head {
                Some((h, kind)) => {
                    if held.is_none() {
                        continue;
                    }
                    let Ok(v) = palette::resolve(&tr.entry.spec, &show.palettes, &show.rig, h, &tr.attr, kind, get) else { continue };
                    (v, tr.entry.spec.live(&show.palettes, &show.rig, h, &tr.attr).map(String::from))
                }
                None => match &tr.entry.spec {
                    Spec::Literal(v) => (v.clone(), None),
                    _ => continue,
                },
            };
            let na = match held {
                Some(a) if v == a.value && tr.entry.spec == a.spec => continue,
                Some(a) => Applied { value: v, spec: tr.entry.spec.clone(), live, fade_end: now + Duration::from_millis(REFRESH_FADE_MS), ..a.clone() },
                None => {
                    *tokens += 1;
                    Applied {
                        value: v,
                        scaled: addr.ends_with(".size") && addr.starts_with("lights.effect."),
                        spec: tr.entry.spec.clone(),
                        head: None,
                        attr: tr.attr.clone(),
                        kind: effect_kind(&addr),
                        live,
                        fade_end: now + Duration::from_millis(REFRESH_FADE_MS),
                        ease: Ease::Smoothstep,
                        pending: None,
                        token: *tokens,
                    }
                }
            };
            if held.is_none() && addr.starts_with("lights.effect.") && addr.ends_with(".size") {
                let active = format!("{}active", addr.strip_suffix("size").expect("effect size"));
                let from = if get(&active).is_some_and(|v| v.truthy()) { get(&addr).unwrap_or(Value::Float(0.0)) } else { Value::Float(0.0) };
                out.cmds.push(self.send(&addr, &from, 0, Ease::Smoothstep));
            }
            out.cmds.push(self.send(&addr, &self.scaled_value(&na), if na.kind == Some(AttrKind::Index) { 0 } else { REFRESH_FADE_MS }, Ease::Smoothstep));
            self.applied.insert(addr, na);
        }
        out
    }

    /// Shared effects have one oscillator/size address. Transfer those controls instead
    /// of letting the outgoing generation's fade replace the incoming contribution.
    pub fn replace_by(&mut self, replacement: &Playback, fade: u64, now: Instant) -> Out {
        let shared = self.applied.keys().filter(|addr| addr.starts_with("lights.effect.") && replacement.applied.contains_key(*addr)).cloned().collect::<Vec<_>>();
        let mut cmds = Vec::new();
        for addr in shared {
            self.applied.remove(&addr);
            cmds.push(self.cmd(Op::Release { address: addr }));
        }
        let mut out = self.release(fade, now);
        out.cmds.splice(0..0, cmds);
        out
    }

    /// Release the playback: intensities fade out over `fade`, then every override goes.
    pub fn release(&mut self, fade: u64, now: Instant) -> Out {
        let mut out = Out::default();
        let mut later = Vec::new();
        let applied = std::mem::take(&mut self.applied);
        for (addr, a) in applied {
            if fade > 0 && a.scaled {
                out.cmds.push(self.send(&addr, &Value::Float(0.0), fade, Ease::Linear));
            }
            if fade > 0 {
                later.push((addr.clone(), a.token, self.cmd(Op::Release { address: addr })));
            } else {
                out.cmds.push(self.cmd(Op::Release { address: addr }));
            }
        }
        // A generation may also own values already removed from `applied` during a cue
        // transition. Its final retirement releases the entire key, including those tails.
        if self.list.starts_with("layer:") {
            let cleanup = self.cmd(Op::Release { address: "**".into() });
            if fade > 0 { later.push(("**".into(), 0, cleanup)); } else { out.cmds.push(cleanup); }
        }
        if !later.is_empty() {
            out.timers.push((now + Duration::from_millis(fade), Timer::Release { list: self.list.clone(), cmds: later }));
        }
        self.current = None;
        self.epoch += 1;
        out
    }
}

/// Crossfade used when palettes/live colours change under a running cue.
pub const REFRESH_FADE_MS: u64 = 300;

fn effect_kind(addr: &str) -> Option<AttrKind> {
    if addr.starts_with("lights.effect.") && (addr.ends_with(".size") || addr.ends_with(".rate")) { Some(AttrKind::Unit) } else { None }
}

fn default_size(show: &Show, effect: &str) -> f32 {
    show.effects.iter().find(|e| e.name == effect).map(|e| e.size).unwrap_or(1.0)
}

/// Addresses a cue-started effect holds: active, size (scaled by the playback master), and
/// rate when given.
fn effect_entries(name: &str, size: f32, rate: Option<f32>, cue: usize) -> Vec<(String, Tracked)> {
    let t = |attr: &str, v: Value| {
        (
            format!("lights.effect.{name}.{attr}"),
            Tracked {
                entry: cuelist::Entry { spec: Spec::Literal(v), fade: Some(0), delay: Some(0) },
                head: None,
                attr: format!("lights.effect.{name}.{attr}"),
                cue,
            },
        )
    };
    let mut v = vec![t("active", Value::Bool(true))];
    let mut depth = t("size", Value::Float(size.clamp(0.0, 1.0) as f64));
    depth.1.entry.fade = None;
    v.push(depth);
    if let Some(r) = rate {
        v.push(t("rate", Value::Float(r as f64)));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cuelist::CueList;
    use crate::engine::tests::rig;
    use crate::palette::Palette;
    use std::collections::BTreeMap;

    fn show(lists: &[(&str, &str)], pals: &[(&str, &str)]) -> Show {
        let r = rig(
            "[fixtures.a]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 1\n[fixtures.b]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 4\n[groups]\nfront = [\"a\"]",
        );
        let mut s = Show::empty();
        s.rig = r;
        s.palettes = pals.iter().map(|(n, src)| (n.to_string(), Palette::parse(n, &toml::from_str(src).unwrap()).unwrap())).collect();
        s.lists = lists.iter().map(|(n, src)| (n.to_string(), CueList::parse(n, &toml::from_str(src).unwrap()).unwrap())).collect();
        s.effects = vec![crate::effects::EffectDef::parse("pulse", &toml::from_str("kind = \"dimmer_sine\"\nsize = 0.6\norder = \"index\"").unwrap()).unwrap()];
        s
    }

    struct RenderHarness {
        core: se_core::Core,
        engine: crate::engine::Engine,
        tokens: u64,
        at: Instant,
        time: u64,
    }

    impl RenderHarness {
        fn new(show: &Show) -> Self {
            let mut core = se_core::Core::new(se_core::Config::build(&[]), 1_000_000_000);
            for (address, meta) in show.declarations() {
                core.submit(se_core::Input::Declare { address, meta });
            }
            for (address, value) in [("lights.a.intensity", Value::Float(0.8)), ("lights.a.color", Value::from("#00ff00"))] {
                core.submit(se_core::Input::Publish { address: address.into(), value });
            }
            core.step();
            let (plan, errors) = crate::engine::Plan::build(show.rig.clone(), show.effects.clone());
            assert!(errors.is_empty(), "{errors:?}");
            Self { time: core.now(), core, engine: crate::engine::Engine::new(std::sync::Arc::new(plan)), tokens: 0, at: Instant::now() }
        }

        fn submit(&mut self, out: &Out) {
            for cmd in &out.cmds {
                self.core.submit(se_core::Input::Command { cmd: cmd.clone() });
            }
        }

        fn goto(&mut self, pb: &mut Playback, show: &Show, cue: usize, fade: u64) -> Out {
            let out = pb.goto(&show.lists["main"], cue, Nav::Goto, Some(fade), show,
                &|a| self.core.get(a).cloned(), &mut self.tokens, self.at);
            self.submit(&out);
            out
        }

        fn frame(&mut self, ms: u64) -> crate::engine::HeadView {
            self.time += ms * 1_000_000;
            self.core.advance_to(self.time);
            let params = self.core.state().params();
            let values = params.iter().map(|p| (p.addr.as_str(), p.resolved.clone())).collect::<Vec<_>>();
            let mut snapshot = crate::engine::tests::snapshot(&values, &[]);
            snapshot.tick = self.core.tick_index();
            snapshot.priorities = params.iter().map(|p| p.overrides.iter().map(|o| o.priority).max().unwrap_or(0)).collect();
            self.engine.render(&snapshot, self.core.now()).heads[0]
        }

        fn retire(&mut self, out: &Out, replacement: Option<&Playback>) {
            for (_, timer) in &out.timers {
                if let Timer::Release { cmds, .. } = timer {
                    for (addr, token, cmd) in cmds {
                        if replacement.is_none_or(|p| p.applied.get(addr).is_none_or(|a| a.token <= *token)) {
                            self.core.submit(se_core::Input::Command { cmd: cmd.clone() });
                        }
                    }
                }
            }
        }
    }

    fn effect_show() -> Show {
        let mut s = show(&[("main", "tracking=false\n[[cue]]\nfade='1s'\neffects={ tint={} }\n[[cue]]\nstop_effects=['tint']\neffects={ pulse={} }\n[[cue]]\nstop_effects=['all']")], &[]);
        s.effects = vec![
            crate::effects::EffectDef::parse("tint", &toml::from_str("kind='color_wave'\nrate=0.01\nsize=1\norder='index'\ncolors=['#ff0000','#0000ff']").unwrap()).unwrap(),
            crate::effects::EffectDef::parse("pulse", &toml::from_str("kind='dimmer_triangle'\nrate=0.01\nsize=1\norder='index'").unwrap()).unwrap(),
        ];
        s
    }

    #[test]
    fn rendered_regular_effect_entrance_handoff_and_retirement_are_continuous() {
        let s = effect_show();
        let mut h = RenderHarness::new(&s);
        let mut pb = Playback::layer("layer:motion:1", 200, Origin::Rule, None, None, Controls::default(), &s);
        let mut before = h.frame(20);
        let entering = pb.goto(&s.lists["main"], 0, Nav::Goto, None, &s,
            &|a| h.core.get(a).cloned(), &mut h.tokens, h.at);
        h.submit(&entering);
        for _ in 0..55 {
            let after = h.frame(20);
            for channel in 0..3 { assert!((after.color[channel] - before.color[channel]).abs() < 0.04, "color entrance jumped: {before:?} -> {after:?}"); }
            before = after;
        }
        assert!(before.color[0] > 0.95 && before.color[1] < 0.01, "{before:?}");
        let leaving = h.goto(&mut pb, &s, 1, 1000);
        for _ in 0..55 {
            let after = h.frame(20);
            assert!((after.intensity - before.intensity).abs() < 0.04, "dimmer entrance jumped: {before:?} -> {after:?}");
            for channel in 0..3 { assert!((after.color[channel] - before.color[channel]).abs() < 0.04, "outgoing color jumped: {before:?} -> {after:?}"); }
            before = after;
        }
        assert!(before.color[1] > 0.99 && before.intensity < 0.1, "{before:?}");
        h.retire(&leaving, Some(&pb));
        let after = h.frame(20);
        assert!((after.intensity - before.intensity).abs() < 0.04, "{before:?} -> {after:?}");
        assert_eq!(h.core.get("lights.effect.tint.active"), Some(&Value::Bool(false)));
        let leaving = h.goto(&mut pb, &s, 2, 1000);
        before = after;
        for _ in 0..55 {
            let after = h.frame(20);
            assert!((after.intensity - before.intensity).abs() < 0.04, "dimmer exit jumped: {before:?} -> {after:?}");
            before = after;
        }
        h.retire(&leaving, Some(&pb));
        assert!((h.frame(20).intensity - 0.8).abs() < 0.01);
        assert_eq!(h.core.get("lights.effect.pulse.active"), Some(&Value::Bool(false)));
    }

    #[test]
    fn immediate_effect_hits_remain_immediate_and_operator_color_wins() {
        let s = effect_show();
        let mut h = RenderHarness::new(&s);
        let mut pb = Playback::layer("layer:accent:1", 300, Origin::Rule, None, None, Controls::default(), &s);
        h.goto(&mut pb, &s, 0, 0);
        assert!(h.frame(20).color[0] > 0.99);
        h.core.submit(se_core::Input::Command { cmd: Command::new(Origin::Ui, Op::Set { address: "lights.a.color".into(), value: Value::from("#0000ff") }).with_priority(Some(400)).with_key("operator") });
        assert!(h.frame(20).color[2] > 0.99);
        let out = pb.release(1000, h.at);
        h.submit(&out);
        for _ in 0..55 { assert!(h.frame(20).color[2] > 0.99); }
        h.retire(&out, None);
        assert!(h.frame(20).color[2] > 0.99);
    }

    #[test]
    fn same_effect_replacement_continues_depth_and_old_retirement_cannot_release_it() {
        let s = effect_show();
        let mut h = RenderHarness::new(&s);
        let mut old = Playback::layer("layer:motion:1", 300, Origin::Rule, None, None, Controls::default(), &s);
        h.goto(&mut old, &s, 0, 1000);
        let before = h.frame(200);
        let mut new = Playback::layer("layer:motion:2", 300, Origin::Rule, None, None, Controls::default(), &s);
        let entering = new.goto(&s.lists["main"], 0, Nav::Goto, Some(1000), &s,
            &|a| h.core.get(a).cloned(), &mut h.tokens, h.at);
        let leaving = old.replace_by(&new, 1000, h.at);
        h.submit(&leaving);
        h.submit(&entering);
        let mut previous = before;
        for _ in 0..55 {
            let after = h.frame(20);
            for channel in 0..3 {
                assert!((after.color[channel] - previous.color[channel]).abs() < 0.04, "shared oscillator jumped: {previous:?} -> {after:?}");
            }
            previous = after;
        }
        assert!(previous.color[0] > 0.95, "{previous:?}");
        h.retire(&leaving, None);
        assert!(h.frame(20).color[0] > 0.95, "old wildcard retirement killed the replacement");
        assert_eq!(h.core.get("lights.effect.tint.active"), Some(&Value::Bool(true)));
        assert!(!h.core.explain("lights.effect.tint.active").unwrap().layers.iter().any(|l| l.source == "layer:motion:1"));
        let leaving = new.release(1000, h.at);
        h.submit(&leaving);
        for _ in 0..55 { h.frame(20); }
        h.retire(&leaving, None);
        assert!(h.frame(20).color[1] > 0.99);
        assert_eq!(h.core.get("lights.effect.tint.active"), Some(&Value::Bool(false)));
    }

    #[test]
    fn different_layer_effects_overlap_without_a_flash_and_retire_separately() {
        let s = effect_show();
        let mut h = RenderHarness::new(&s);
        let mut old = Playback::layer("layer:motion:1", 200, Origin::Rule, None, None, Controls::default(), &s);
        h.goto(&mut old, &s, 0, 0);
        let mut previous = h.frame(20);
        let mut new = Playback::layer("layer:motion:2", 200, Origin::Rule, None, None, Controls::default(), &s);
        let entering = new.goto(&s.lists["main"], 1, Nav::Goto, Some(1000), &s,
            &|a| h.core.get(a).cloned(), &mut h.tokens, h.at);
        let leaving = old.replace_by(&new, 1000, h.at);
        h.submit(&leaving);
        h.submit(&entering);
        for _ in 0..55 {
            let after = h.frame(20);
            for channel in 0..3 { assert!((after.color[channel] - previous.color[channel]).abs() < 0.04, "{previous:?} -> {after:?}"); }
            assert!((after.intensity - previous.intensity).abs() < 0.04, "{previous:?} -> {after:?}");
            previous = after;
        }
        h.retire(&leaving, None);
        let after = h.frame(20);
        assert!(after.color[1] > 0.99 && after.intensity < 0.1, "{after:?}");
        assert_eq!(h.core.get("lights.effect.tint.active"), Some(&Value::Bool(false)));
        assert_eq!(h.core.get("lights.effect.pulse.active"), Some(&Value::Bool(true)));
    }

    #[test]
    fn reentering_an_effect_before_its_tail_retires_keeps_the_new_contribution() {
        let s = effect_show();
        let mut h = RenderHarness::new(&s);
        let mut pb = Playback::layer("layer:motion:1", 200, Origin::Rule, None, None, Controls::default(), &s);
        h.goto(&mut pb, &s, 0, 0);
        h.frame(20);
        let first_tail = h.goto(&mut pb, &s, 1, 1000);
        let mut previous = h.frame(300);
        let second_tail = h.goto(&mut pb, &s, 0, 1000);
        for _ in 0..55 {
            let after = h.frame(20);
            for channel in 0..3 { assert!((after.color[channel] - previous.color[channel]).abs() < 0.04, "{previous:?} -> {after:?}"); }
            assert!((after.intensity - previous.intensity).abs() < 0.04, "{previous:?} -> {after:?}");
            previous = after;
        }
        h.retire(&first_tail, Some(&pb));
        h.retire(&second_tail, Some(&pb));
        let after = h.frame(20);
        assert!(after.color[0] > 0.95 && (after.intensity - 0.8).abs() < 0.01, "{after:?}");
        assert_eq!(h.core.get("lights.effect.tint.active"), Some(&Value::Bool(true)));
        assert_eq!(h.core.get("lights.effect.pulse.active"), Some(&Value::Bool(false)));
    }

    #[test]
    fn scanner_repositions_with_closed_shutter_and_snaps_wheel_before_revealing() {
        let mut s = show(&[("main", "tracking=false\n[[cue]]\n[cue.set]\na={intensity=0.8,pan=0.25,tilt=0.3,gobo=5}\n[[cue]]\n[cue.set]\na={intensity=0.8,pan=0.75,tilt=0.6,gobo=0}")], &[]);
        let profiles = crate::profile::library(&BTreeMap::from([("scanner".into(),
            toml::from_str(include_str!("../../../project-example/lights/fixtures/adj_inno_pocket_scan.toml")).unwrap())]), &mut Vec::new());
        s.rig = std::sync::Arc::new(crate::rig::Rig::compile(&toml::from_str(
            "[fixtures.a]\nprofile='scanner'\nmode='6ch'\naddress=10\nlayout_verified=true\n[fixtures.b]\nprofile='generic_rgb'\nmode='3ch'\naddress=1\nlayout_verified=true").unwrap(), profiles, Vec::new()).unwrap());
        let mut h = RenderHarness::new(&s);
        h.core.submit(se_core::Input::Publish { address: "lights.b.intensity".into(), value: Value::Float(0.6) });
        let mut pb = Playback::layer("layer:motion:1", 200, Origin::Rule, None, None, Controls::default(), &s);
        h.goto(&mut pb, &s, 0, 0);
        for _ in 0..35 {
            h.frame(20);
            let bytes = &h.engine.frame.universes[0];
            assert_eq!((bytes[11], bytes[13]), (0, 0), "first positioning must be dark");
            assert_eq!(bytes[12], 36);
            assert_eq!(&bytes[..3], &[153, 153, 153], "wash must stay visible");
        }
        for _ in 0..45 { h.frame(20); }
        assert_eq!(h.engine.frame.universes[0][11], 8, "settled scanner is steady open");
        assert!(h.engine.frame.universes[0][13] >= 203);
        h.goto(&mut pb, &s, 1, 1000);
        for _ in 0..85 {
            h.frame(20);
            let bytes = &h.engine.frame.universes[0];
            assert_eq!((bytes[11], bytes[13]), (0, 0), "pan/tilt travel and settling must be dark");
            assert_eq!(bytes[12], 0, "combined wheel must never traverse colored intermediate indices");
        }
        let mut previous = 0;
        for _ in 0..50 {
            h.frame(20);
            let bytes = &h.engine.frame.universes[0];
            assert!(bytes[13] >= previous && bytes[13] - previous < 16, "spot reveal must be smooth");
            previous = bytes[13];
        }
        assert!(previous >= 203);
        assert_eq!(h.engine.frame.universes[0][11], 8);
    }

    #[test]
    fn palette_edit_and_live_stream_colour_refresh_render_smoothly() {
        let mut s = show(&[("main", "[[cue]]\n[cue.set]\nall={intensity=1.0,color='palette:warm'}")],
            &[("warm", "[set]\nall={color='stream:accent'}")]);
        let mut h = RenderHarness::new(&s);
        h.core.submit(se_core::Input::Publish { address: "palette.accent".into(), value: Value::from("#ff0000") });
        h.frame(20);
        let mut pb = Playback::new("main", 200, Origin::Deck, None, None, 1.0);
        h.goto(&mut pb, &s, 0, 0);
        assert!(h.frame(20).color[0] > 0.99);
        h.core.submit(se_core::Input::Publish { address: "palette.accent".into(), value: Value::from("#0000ff") });
        h.frame(20);
        let out = pb.refresh(&s.lists["main"], &s, Some("palette.accent"),
            &|a| h.core.get(a).cloned(), &mut h.tokens, h.at);
        h.submit(&out);
        let halfway = h.frame(150);
        assert!(halfway.color[0] > 0.25 && halfway.color[0] < 0.75 && halfway.color[2] > 0.25 && halfway.color[2] < 0.75, "{halfway:?}");
        assert!(h.frame(250).color[2] > 0.99);
        s.palettes.insert("warm".into(), Palette::parse("warm", &toml::from_str("[set]\nall={color='#00ff00'}").unwrap()).unwrap());
        let out = pb.refresh(&s.lists["main"], &s, None,
            &|a| h.core.get(a).cloned(), &mut h.tokens, h.at);
        h.submit(&out);
        assert!(h.frame(350).color[1] > 0.99);
    }

    #[test]
    fn live_source_arrival_preserves_master_and_reveals_its_color() {
        let s = show(&[("main", "[[cue]]\n[cue.set]\na={intensity=1.0,color='stream:accent'}")], &[]);
        let mut h = RenderHarness::new(&s);
        h.core.submit(se_core::Input::Publish { address: "lights.a.intensity".into(), value: Value::Float(0.0) });
        h.core.submit(se_core::Input::Publish { address: "lights.a.color".into(), value: Value::from("#ff0000") });
        h.frame(20);
        let mut pb = Playback::new("main", 200, Origin::Deck, None, None, 1.0);
        let entering = h.goto(&mut pb, &s, 0, 0);
        assert_eq!(entering.errors.len(), 1);
        assert!(h.frame(20).color[0] > 0.99);
        pb.master = 0.5;
        for cmd in pb.apply_master(h.at) { h.core.submit(se_core::Input::Command { cmd }); }
        assert!((h.frame(20).intensity - 0.5).abs() < 1e-6);
        h.core.submit(se_core::Input::Publish { address: "palette.accent".into(), value: Value::from("#0000ff") });
        h.frame(20);
        let out = pb.refresh(&s.lists["main"], &s, Some("palette.accent"),
            &|a| h.core.get(a).cloned(), &mut h.tokens, h.at);
        h.submit(&out);
        let after = h.frame(350);
        assert!(after.color[2] > 0.99 && (after.intensity - 0.5).abs() < 1e-6, "{after:?}");
    }

    #[test]
    fn releasing_one_chat_playback_preserves_the_other_actor() {
        let s = show(&[("main", "[[cue]]\n[cue.set]\na={color='palette:warm'}")],
            &[("warm", "[set]\na={color='#ff0000'}")]);
        let mut h = RenderHarness::new(&s);
        h.core.submit(se_core::Input::Command { cmd: Command::new(Origin::Ui, Op::ModeSet { mode: "live".into() }) });
        h.frame(20);
        let actor = |id: &str| Some(Actor { platform: "twitch".into(), id: id.into(), name: id.into(), roles: vec![] });
        let mut first = Playback::new("main", 100, Origin::Chat, actor("first"), None, 1.0);
        h.goto(&mut first, &s, 0, 0);
        assert!(h.frame(20).color[0] > 0.99);
        let mut second = Playback::new("main", 100, Origin::Chat, actor("second"), None, 1.0);
        let mut blue = s.clone();
        blue.palettes.insert("warm".into(), Palette::parse("warm", &toml::from_str("[set]\na={color='#0000ff'}").unwrap()).unwrap());
        h.goto(&mut second, &blue, 0, 0);
        assert!(h.frame(20).color[2] > 0.99);
        let out = first.release(0, h.at);
        h.submit(&out);
        assert!(h.frame(20).color[2] > 0.99, "first actor's retirement must not release the second actor's look");
        let out = second.release(0, h.at);
        h.submit(&out);
        assert!(h.frame(20).color[1] > 0.99, "both released restores the underlying green");
    }
}
