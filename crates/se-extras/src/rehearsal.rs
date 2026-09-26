//! Rehearsal (§17.2): the `rehearsal` show mode runs everything with the simulator and test
//! outputs. The other subsystems do their part by watching `show.mode`: OBS never starts
//! streaming (`obs.stream.start` is skipped), light outputs not marked `rehearsal = true` hold
//! their look (the visualizer shows the rehearsal), and Twitch actions are dry runs. This module
//! supplies the practice viewers: while rehearsing it fires simulator presets at random, about
//! one every `every`, so alerts, overlays, rules and lights react as in a real show. Leaving
//! the mode stops it.
//!
//! ```toml
//! [rehearsal]
//! simulate = true          # simulated viewer events while rehearsing
//! every = "20s"            # about one event this often (random 0.5–1.5×), 1s..1h
//! presets = []             # simulator presets to draw from; empty = a built-in mix
//! ```
//!
//! State: `show.rehearsal.simulating` (bool).

use crate::util::Section;
use rand::{Rng, SeedableRng};
use se_hub::EngineCtx;
use se_proto::{Command, Meta, Op, Origin, Value};
use serde::Deserialize;
use std::time::Duration;

const TARGET: &str = "rehearsal";

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub simulate: bool,
    pub every: String,
    pub presets: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { simulate: true, every: "20s".into(), presets: Vec::new() }
    }
}

const CHAT: &[&str] = &["hello!", "let's gooo", "what snare is that?", "that fill was sick", "hi from the practice run", "turn the drums up!"];

/// Built-in mix: (preset, weight). Mostly chat and follows, now and then something big.
const MIX: &[(&str, u32)] = &[("chat", 10), ("follow", 4), ("sub", 2), ("resub", 1), ("cheer", 2), ("redeem", 2), ("tip", 1), ("raid", 1), ("gift_bomb", 1)];

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub simulate: bool,
    pub every: Duration,
    /// (preset, weight); custom lists weigh every preset the same.
    pub mix: Vec<(String, u32)>,
}

impl Plan {
    pub fn build(s: &Settings) -> Result<Plan, String> {
        let ms = se_proto::parse_duration_ms(&s.every).ok_or_else(|| format!("every = \"{}\" is not a duration like \"20s\"", s.every))?;
        let every = Duration::from_millis(ms);
        if !(Duration::from_secs(1)..=Duration::from_secs(3600)).contains(&every) {
            return Err(format!("every = \"{}\" must be between 1s and 1h", s.every));
        }
        let mix = if s.presets.is_empty() {
            MIX.iter().map(|(n, w)| (n.to_string(), *w)).collect()
        } else {
            let known: Vec<&str> = se_core::sim::PRESETS.iter().map(|(n, _)| *n).collect();
            for p in &s.presets {
                if !known.contains(&p.as_str()) {
                    return Err(format!("unknown simulator preset `{p}` (known: {})", known.join(", ")));
                }
            }
            s.presets.iter().map(|p| (p.clone(), 1)).collect()
        };
        Ok(Plan { simulate: s.simulate, every, mix })
    }

    /// Time until the next simulated event: `every` × 0.5..1.5.
    pub fn delay(&self, rng: &mut impl Rng) -> Duration {
        self.every.mul_f64(rng.random_range(0.5..1.5))
    }

    /// The next simulator action (`sim.<preset>` with some variety in its arguments).
    pub fn pick(&self, rng: &mut impl Rng) -> Op {
        let total: u32 = self.mix.iter().map(|(_, w)| w).sum();
        let mut roll = rng.random_range(0..total.max(1));
        let mut name = self.mix[0].0.as_str();
        for (n, w) in &self.mix {
            if roll < *w {
                name = n;
                break;
            }
            roll -= w;
        }
        let args = match name {
            "chat" => Value::map().with("message", CHAT[rng.random_range(0..CHAT.len())]),
            "cheer" => Value::map().with("bits", [100i64, 250, 500, 1000][rng.random_range(0..4)]).with("message", "Cheer from the practice run"),
            "raid" => Value::map().with("viewers", rng.random_range(5..60i64)),
            "gift_bomb" => Value::map().with("count", [3i64, 5, 10][rng.random_range(0..3)]),
            _ => Value::Null,
        };
        Op::Action { name: format!("sim.{name}"), args }
    }
}

pub fn start(ctx: EngineCtx) {
    ctx.hub.declare("show.rehearsal.simulating", Meta::boolean(false).readonly().owner(TARGET).describe("Practice viewers are being simulated (rehearsal)"));
    tokio::spawn(run(ctx));
}

async fn run(mut ctx: EngineCtx) {
    let mut section: Section<Settings> = Section::new("rehearsal", TARGET);
    section.reload(&ctx);
    let mut plan = Plan::build(&section.value).unwrap_or_else(|e| {
        ctx.hub.log("error", TARGET, format!("[rehearsal] in project.toml: {e}; using the defaults"));
        Plan::build(&Settings::default()).expect("default rehearsal settings are valid")
    });
    let mut rng = rand::rngs::StdRng::from_os_rng();
    let mut active = false;
    let mut next = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            r = ctx.config.changed() => {
                if r.is_err() { break; }
                if section.reload(&ctx) {
                    match Plan::build(&section.value) {
                        Ok(p) => {
                            if p.every != plan.every {
                                next = tokio::time::Instant::now() + p.delay(&mut rng);
                            }
                            plan = p;
                            if active {
                                ctx.hub.publish("show.rehearsal.simulating", Value::Bool(plan.simulate));
                            }
                        }
                        Err(e) => ctx.hub.log("error", TARGET, format!("[rehearsal] in project.toml: {e}; keeping the previous settings")),
                    }
                }
            }
            _ = tick.tick() => {
                let now = tokio::time::Instant::now();
                let (on, streaming) = {
                    let snap = ctx.hub.snapshot.load();
                    (snap.str("show.mode") == Some("rehearsal"), snap.bool("obs.stream.active"))
                };
                if on != active {
                    active = on;
                    ctx.hub.publish("show.rehearsal.simulating", Value::Bool(on && plan.simulate));
                    if on {
                        next = now + plan.delay(&mut rng);
                        let viewers = if plan.simulate {
                            format!("practice viewers about every {} s", plan.every.as_secs())
                        } else {
                            "no practice viewers ([rehearsal] simulate = false)".into()
                        };
                        ctx.hub.log("info", TARGET, format!("rehearsal started: {viewers}; OBS won't start streaming, lights hold their look unless an output has rehearsal = true, Twitch changes are dry runs"));
                        if streaming {
                            ctx.hub.log("warn", TARGET, "rehearsal started while OBS is streaming: viewers can see it; stop the stream if this is only a practice run");
                        }
                    } else {
                        ctx.hub.log("info", TARGET, "rehearsal over: outputs are back to normal");
                    }
                }
                if active && plan.simulate && now >= next {
                    next = now + plan.delay(&mut rng);
                    ctx.hub.command(Command::new(Origin::System, plan.pick(&mut rng)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(every: &str, presets: &[&str]) -> Settings {
        Settings { simulate: true, every: every.into(), presets: presets.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn interval_bounds_and_preset_names_are_checked() {
        assert_eq!(Plan::build(&Settings::default()).unwrap().every, Duration::from_secs(20));
        assert!(Plan::build(&settings("500ms", &[])).unwrap_err().contains("between 1s and 1h"));
        assert!(Plan::build(&settings("2h", &[])).is_err());
        assert!(Plan::build(&settings("soon", &[])).unwrap_err().contains("not a duration"));
        assert!(Plan::build(&settings("1s", &[])).is_ok());
        assert!(Plan::build(&settings("20s", &["cheer", "confetti"])).unwrap_err().contains("unknown simulator preset `confetti`"));
        assert!(toml::from_str::<Settings>("simulat = true").is_err(), "typos are reported");
    }

    #[test]
    fn delays_stay_within_half_to_one_and_a_half_intervals() {
        let plan = Plan::build(&settings("10s", &[])).unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(3);
        for _ in 0..1000 {
            let d = plan.delay(&mut rng);
            assert!(d >= Duration::from_secs(5) && d < Duration::from_secs(15), "{d:?}");
        }
    }

    #[test]
    fn picks_follow_the_mix() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(9);
        let custom = Plan::build(&settings("5s", &["follow", "raid"])).unwrap();
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..200 {
            let Op::Action { name, .. } = custom.pick(&mut rng) else { panic!("not an action") };
            seen.insert(name);
        }
        assert_eq!(seen.into_iter().collect::<Vec<_>>(), ["sim.follow", "sim.raid"]);
        let builtin = Plan::build(&Settings::default()).unwrap();
        let n = 5000;
        let chats = (0..n).filter(|_| matches!(builtin.pick(&mut rng), Op::Action { name, .. } if name == "sim.chat")).count();
        // chat weighs 10 of 24
        assert!((1800..2400).contains(&chats), "{chats}");
        for _ in 0..500 {
            let Op::Action { name, args } = builtin.pick(&mut rng) else { panic!() };
            assert!(se_core::sim::events(name.trim_start_matches("sim."), &args, &mut se_core::rng::Rng::new(1)).is_ok(), "{name} {args}");
        }
    }
}
