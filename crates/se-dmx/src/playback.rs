//! Playbacks (§9.2): run cue lists into the core as overrides keyed `cuelist:<cl>` at the
//! playback's priority, so intensity merges HTP and everything else LTP through the normal
//! resolver. Fades are core animations; delays, follows, and waits are control-task timers.
//! Intensities (and effect sizes) are scaled by the playback master
//! (`lights.cuelist.<cl>.master`).

use crate::cuelist::{self, CueList, State, Tracked};
use crate::palette::{self, Spec};
use crate::rig::AttrKind;
use crate::show::Show;
use se_proto::{Actor, Command, Ease, Id, Op, Origin, PRIORITY_CHAT, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Override key for a cue list's layer.
pub fn key(list: &str) -> String {
    format!("cuelist:{list}")
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
    pub list: String,
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
        }
    }

    fn cmd(&self, op: Op) -> Command {
        let mut c = Command::new(self.origin, op).with_priority(Some(self.priority)).with_actor(self.actor.clone()).caused_by(self.cause);
        if self.priority > PRIORITY_CHAT {
            c = c.with_key(key(&self.list));
        }
        c
    }

    fn send(&self, addr: &str, value: &Value, fade_ms: u64, ease: Ease) -> Command {
        if fade_ms == 0 {
            self.cmd(Op::Set { address: addr.into(), value: value.clone() })
        } else {
            self.cmd(Op::Animate { address: addr.into(), to: value.clone(), ms: fade_ms.min(u32::MAX as u64) as u32, ease })
        }
    }

    fn scaled_value(&self, a: &Applied) -> Value {
        if a.scaled { Value::Float(a.value.as_f64().unwrap_or(0.0) * self.master.clamp(0.0, 1.0) as f64) } else { a.value.clone() }
    }

    pub fn progress(&self, now: Instant) -> (f32, u64) {
        let el = now.saturating_duration_since(self.go_at).as_millis() as u64;
        if self.total_ms == 0 {
            return (1.0, el);
        }
        ((el as f32 / self.total_ms as f32).min(1.0), el)
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
                for (a, t) in effect_entries(e, size.unwrap_or_else(|| default_size(show, e)), *rate, k) {
                    m.insert(a, t);
                }
            }
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
                }
            }
            r
        } else {
            Vec::new()
        };
        let mut values = target.values.clone();
        for (e, (size, rate)) in &target.effects {
            values.extend(effect_entries(e, size.unwrap_or_else(|| default_size(show, e)), *rate, k));
        }
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
                out.cmds.push(self.send(&addr, &self.scaled_value(&a), fade, a.ease));
            }
            self.applied.insert(addr, a);
        }
        let release_fade = fade_override.unwrap_or(cue.fade_for(true));
        let mut later = Vec::new();
        for addr in to_release {
            if let Some(a) = self.applied.remove(&addr) {
                if a.scaled && release_fade > 0 {
                    out.cmds.push(self.send(&addr, &Value::Float(0.0), release_fade, cue.ease));
                    later.push((addr.clone(), a.token, self.cmd(Op::Release { address: addr })));
                } else {
                    out.cmds.push(self.cmd(Op::Release { address: addr }));
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
            if let Some(w) = cue.wait_ms {
                if !last || list.looped {
                    out.timers.push((now + Duration::from_millis(w), Timer::Advance { list: self.list.clone(), epoch: self.epoch }));
                }
            } else if let Some(f) = cue.follow_ms
                && (!last || list.looped)
            {
                out.timers.push((now + Duration::from_millis(end + f), Timer::Advance { list: self.list.clone(), epoch: self.epoch }));
            }
            if last && !list.looped && list.autorelease {
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
        Some(self.send(addr, &self.scaled_value(&a), fade, a.ease))
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

    /// Re-resolve values after a palette/cue edit or a live source change; returns commands
    /// for values that changed (short crossfade).
    pub fn refresh(&mut self, show: &Show, only_live: Option<&str>, get: &dyn Fn(&str) -> Option<Value>, now: Instant) -> Out {
        let mut out = Out::default();
        let Some(k) = self.current else { return out };
        let Some(list) = show.lists.get(&self.list) else { return out };
        if k >= list.cues.len() {
            return out;
        }
        let target = cuelist::tracked(list, k, &show.rig, &show.palettes);
        for (addr, tr) in target.values {
            let Some(a) = self.applied.get(&addr) else { continue };
            if a.pending.is_some() {
                continue;
            }
            if let Some(src) = only_live
                && a.live.as_deref() != Some(src)
            {
                continue;
            }
            let Some((h, kind)) = tr.head else { continue };
            let Ok(v) = palette::resolve(&tr.entry.spec, &show.palettes, &show.rig, h, &tr.attr, kind, get) else { continue };
            if v == a.value && tr.entry.spec == a.spec {
                continue;
            }
            let mut na = a.clone();
            na.value = v;
            na.spec = tr.entry.spec.clone();
            na.live = tr.entry.spec.live(&show.palettes, &show.rig, h, &tr.attr).map(String::from);
            na.fade_end = now + Duration::from_millis(REFRESH_FADE_MS);
            out.cmds.push(self.send(&addr, &self.scaled_value(&na), REFRESH_FADE_MS, Ease::Smoothstep));
            self.applied.insert(addr, na);
        }
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
    v.push(t("size", Value::Float(size.clamp(0.0, 1.0) as f64)));
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
        s.effects = vec![crate::effects::EffectDef::parse("pulse", &toml::from_str("kind = \"dimmer_sine\"\nsize = 0.6").unwrap()).unwrap()];
        s
    }

    fn describe(cmds: &[Command]) -> Vec<String> {
        cmds.iter().map(|c| format!("{} [{}@{}]", c.op.describe(), c.key.clone().unwrap_or_default(), c.priority.unwrap_or(0))).collect()
    }

    const MAIN: &str = r##"
[[cue]]
fade = "2s"
fade_out = "1s"
[cue.set]
all = { intensity = 1.0, color = "palette:warm" }
[[cue]]
follow = "1s"
[cue.set]
a = { intensity = { value = 0.4, fade = "3s", delay = "500ms" } }
[[cue]]
effects = { pulse = {} }
release = ["b"]
"##;

    #[test]
    fn go_tracks_changes_and_goto_restores_full_state() {
        let s = show(&[("main", MAIN)], &[("warm", "[set]\nall = { color = \"#ff8000\" }")]);
        let l = &s.lists["main"];
        let get = |_: &str| None;
        let mut t = 0;
        let now = Instant::now();
        let mut pb = Playback::new("main", 200, Origin::Deck, None, None, 1.0);
        let o = pb.goto(l, 0, Nav::Go, None, &s, &get, &mut t, now);
        let d = describe(&o.cmds);
        assert!(d.contains(&"animate lights.a.intensity 1.0 2000ms [cuelist:main@200]".to_string()), "{d:?}");
        assert!(d.iter().any(|x| x.starts_with("animate lights.b.color [1.0,0.50196") && x.ends_with("2000ms [cuelist:main@200]")), "{d:?}");
        // cue 2: only `a.intensity` changes, delayed 500 ms, fading down over its own 3 s
        let o = pb.goto(l, 1, Nav::Go, None, &s, &get, &mut t, now);
        assert!(o.cmds.is_empty(), "delayed: {:?}", describe(&o.cmds));
        let (at, timer) = o.timers.iter().find(|(_, t)| matches!(t, Timer::Entry { .. })).unwrap().clone();
        assert_eq!(at, now + Duration::from_millis(500));
        let Timer::Entry { addr, token, .. } = timer else { unreachable!() };
        let c = pb.fire_entry(&addr, token).unwrap();
        assert_eq!(c.op.describe(), "animate lights.a.intensity 0.4 3000ms");
        // follow: advance 1 s after the 3.5 s completion
        assert!(o.timers.iter().any(|(at, t)| matches!(t, Timer::Advance { .. }) && *at == now + Duration::from_millis(4500)));
        // cue 3: effect starts, b released (intensity fades out with fade_out, then released)
        let o = pb.goto(l, 2, Nav::Go, None, &s, &get, &mut t, now);
        let d = describe(&o.cmds);
        assert!(d.contains(&"set lights.effect.pulse.active true [cuelist:main@200]".to_string()), "{d:?}");
        assert!(d.contains(&"release lights.b.color [cuelist:main@200]".to_string()), "{d:?}");
        assert!(!pb.applied.contains_key("lights.b.intensity"));
        // back to cue 1: full tracked state, effect stopped, b restored
        let o = pb.goto(l, 0, Nav::Back, None, &s, &get, &mut t, now);
        let d = describe(&o.cmds);
        assert!(d.contains(&"release lights.effect.pulse.active [cuelist:main@200]".to_string()), "{d:?}");
        assert!(d.iter().any(|x| x.starts_with("animate lights.b.intensity 1.0")), "{d:?}");
        // master scales intensities, keeping fades in progress
        pb.master = 0.5;
        let d = describe(&pb.apply_master(now));
        assert!(d.contains(&"animate lights.a.intensity 0.5 2000ms [cuelist:main@200]".to_string()), "{d:?}");
        // release: intensities fade over the release time, then everything is released
        let o = pb.release(1000, now);
        let d = describe(&o.cmds);
        assert!(d.contains(&"animate lights.a.intensity 0.0 1000ms [cuelist:main@200]".to_string()), "{d:?}");
        let Timer::Release { cmds, .. } = &o.timers[0].1 else { panic!() };
        assert!(cmds.iter().any(|(a, _, c)| a == "lights.b.color" && c.op.describe() == "release lights.b.color"));
        assert!(pb.applied.is_empty() && pb.current.is_none());
    }

    #[test]
    fn palette_edit_and_live_stream_colour_refresh_running_cue() {
        let mut s = show(&[("main", MAIN)], &[("warm", "[set]\nall = { color = \"stream:accent\" }")]);
        let accent = std::cell::RefCell::new(Value::from([1.0f32, 0.0, 0.0, 1.0]));
        let get = |a: &str| (a == "palette.accent").then(|| accent.borrow().clone());
        let mut t = 0;
        let now = Instant::now();
        let mut pb = Playback::new("main", 200, Origin::Deck, None, None, 1.0);
        pb.goto(&s.lists["main"].clone(), 0, Nav::Goto, None, &s, &get, &mut t, now);
        assert_eq!(pb.applied["lights.a.color"].live.as_deref(), Some("palette.accent"));
        // the stream palette changes → only live entries re-resolve
        *accent.borrow_mut() = Value::from([0.0f32, 0.0, 1.0, 1.0]);
        let o = pb.refresh(&s, Some("palette.accent"), &get, now);
        assert_eq!(describe(&o.cmds).len(), 2, "{:?}", describe(&o.cmds));
        assert!(describe(&o.cmds)[0].starts_with("animate lights.a.color [0.0,0.0,1.0,1.0] 300ms"));
        // editing the palette file updates the running cue
        s.palettes.insert("warm".into(), Palette::parse("warm", &toml::from_str("[set]\nall = { color = \"#00ff00\" }").unwrap()).unwrap());
        let o = pb.refresh(&s, None, &get, now);
        assert!(describe(&o.cmds).iter().all(|c| c.contains("[0.0,1.0,0.0,1.0]")), "{:?}", describe(&o.cmds));
        assert_eq!(o.cmds.len(), 2);
        let _ = BTreeMap::<String, String>::new();
    }

    #[test]
    fn live_source_without_a_value_applies_once_it_publishes() {
        let s = show(&[("main", "[[cue]]\n[cue.set]\na = { intensity = 1.0, color = \"stream:accent\" }")], &[]);
        let accent: std::cell::RefCell<Option<Value>> = std::cell::RefCell::new(None);
        let get = |a: &str| if a == "palette.accent" { accent.borrow().clone() } else { None };
        let mut t = 0;
        let now = Instant::now();
        let mut pb = Playback::new("main", 200, Origin::Deck, None, None, 1.0);
        let o = pb.goto(&s.lists["main"], 0, Nav::Goto, None, &s, &get, &mut t, now);
        assert_eq!(describe(&o.cmds).len(), 1, "only the intensity: {:?}", describe(&o.cmds));
        assert_eq!(o.errors.len(), 1);
        pb.master = 0.5;
        assert_eq!(pb.apply_master(now).len(), 1, "no bogus value for the unresolved colour");
        *accent.borrow_mut() = Some(Value::from([0.0f32, 1.0, 0.0, 1.0]));
        let o = pb.refresh(&s, Some("palette.accent"), &get, now);
        let d = describe(&o.cmds);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].starts_with("animate lights.a.color [0.0,1.0,0.0,1.0]"), "{d:?}");
    }

    #[test]
    fn chat_playbacks_use_chat_priority_without_a_key() {
        let s = show(&[("main", MAIN)], &[("warm", "[set]\nall = { color = \"#ff8000\" }")]);
        let get = |_: &str| None;
        let mut t = 0;
        let mut pb =
            Playback::new("main", 100, Origin::Chat, Some(Actor { platform: "twitch".into(), id: "42".into(), name: "x".into(), roles: vec![] }), None, 1.0);
        let o = pb.goto(&s.lists["main"], 0, Nav::Go, None, &s, &get, &mut t, Instant::now());
        assert!(o.cmds.iter().all(|c| c.key.is_none() && c.priority == Some(100) && c.origin == Origin::Chat));
    }
}
