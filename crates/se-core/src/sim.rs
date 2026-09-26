//! Event simulator (§15.5): realistic payloads for testing rules, alerts, and patches.
//!
//! The payload shapes here are the normalized event contract that `se-twitch`, the relay
//! (Ko-fi), and other adapters also produce.

use crate::rng::Rng;
use se_proto::{Actor, Event, Origin, Role, Value};

pub const PRESETS: &[(&str, &str)] = &[
    ("cheer", "bits=1000 message='…'"),
    ("sub", "tier=1 months=1"),
    ("resub", "tier=1 months=12"),
    ("gift_bomb", "count=50 tier=1"),
    ("follow", ""),
    ("raid", "viewers=300"),
    ("redeem", "reward=HYPE cost=2000 input=''"),
    ("chat", "message='hello'"),
    ("tip", "amount=5 currency=USD message=''"),
    ("ad_break", "duration=90"),
    ("hype_train", "level=2"),
];

const NAMES: &[&str] =
    &["pixelwizard", "drumfan42", "lofi_larry", "snarequeen", "kickdrum_kid", "cymbalcrash", "groovebot", "tomtom", "hihat_hero", "bassdrop"];

fn actor(rng: &mut Rng, roles: Vec<Role>) -> Actor {
    let name = NAMES[rng.below(NAMES.len() as u64) as usize].to_string();
    Actor { platform: "twitch".into(), id: format!("sim-{}", rng.below(1_000_000)), name, roles }
}

fn arg<'a>(args: &'a Value, k: &str) -> Option<&'a Value> {
    args.get_path(k)
}

fn num(args: &Value, k: &str, d: i64) -> i64 {
    arg(args, k).and_then(Value::as_i64).unwrap_or(d)
}

fn text(args: &Value, k: &str, d: &str) -> String {
    arg(args, k).map(|v| v.to_string()).unwrap_or_else(|| d.to_string())
}

/// Build the events for a simulator preset.
pub fn events(name: &str, args: &Value, rng: &mut Rng) -> Result<Vec<Event>, String> {
    let mk = |ty: &str, payload: Value, a: Option<Actor>| {
        let mut e = Event::new(ty, Origin::Sim, payload);
        e.actor = a;
        e
    };
    let user = arg(args, "user").map(|v| v.to_string());
    let who = |rng: &mut Rng, roles: Vec<Role>| {
        let mut a = actor(rng, roles);
        if let Some(u) = &user {
            a.name = u.clone();
        }
        a
    };
    Ok(match name {
        "cheer" => {
            let a = who(rng, vec![Role::Everyone]);
            vec![mk(
                "twitch.cheer",
                Value::map().with("bits", num(args, "bits", 1000)).with("message", text(args, "message", "Cheer1000 let's go!")).with("user", a.name.clone()),
                Some(a),
            )]
        }
        "sub" | "resub" => {
            let a = who(rng, vec![Role::Sub]);
            let months = num(args, "months", if name == "resub" { 12 } else { 1 });
            vec![mk(
                if name == "resub" { "twitch.resub" } else { "twitch.sub" },
                Value::map()
                    .with("tier", num(args, "tier", 1))
                    .with("months", months)
                    .with("is_gift", false)
                    .with("message", text(args, "message", ""))
                    .with("user", a.name.clone()),
                Some(a),
            )]
        }
        "gift_bomb" => {
            let gifter = who(rng, vec![Role::Sub]);
            let count = num(args, "count", 50).clamp(1, 1000);
            let tier = num(args, "tier", 1);
            let gift_id = format!("sim-gift-{}", rng.below(1 << 40));
            let mut out = vec![mk(
                "twitch.gift",
                Value::map()
                    .with("count", count)
                    .with("tier", tier)
                    .with("total", num(args, "total", count * 3))
                    .with("gift_id", gift_id.clone())
                    .with("user", gifter.name.clone()),
                Some(gifter.clone()),
            )];
            for _ in 0..count {
                let r = actor(rng, vec![Role::Sub]);
                out.push(mk(
                    "twitch.sub",
                    Value::map()
                        .with("tier", tier)
                        .with("months", 1)
                        .with("is_gift", true)
                        .with("gift_id", gift_id.clone())
                        .with("gifter", gifter.name.clone())
                        .with("user", r.name.clone()),
                    Some(r),
                ));
            }
            out
        }
        "follow" => {
            let a = who(rng, vec![Role::Follower]);
            vec![mk("twitch.follow", Value::map().with("user", a.name.clone()), Some(a))]
        }
        "raid" => {
            let a = who(rng, vec![Role::Everyone]);
            vec![mk("twitch.raid", Value::map().with("viewers", num(args, "viewers", 300)).with("from", a.name.clone()).with("user", a.name.clone()), Some(a))]
        }
        "redeem" => {
            let a = who(rng, vec![Role::Follower]);
            let reward = text(args, "reward", "HYPE");
            vec![mk(
                "twitch.redeem",
                Value::map()
                    .with("reward", reward.clone())
                    .with("reward_id", format!("sim-reward-{reward}"))
                    .with("redemption_id", format!("sim-red-{}", rng.below(1 << 40)))
                    .with("cost", num(args, "cost", 2000))
                    .with("input", text(args, "input", ""))
                    .with("user", a.name.clone()),
                Some(a),
            )]
        }
        "chat" => {
            let role = arg(args, "role").and_then(|v| v.as_str()).and_then(Role::parse).unwrap_or(Role::Everyone);
            let a = who(rng, vec![role]);
            vec![mk(
                "twitch.chat",
                Value::map()
                    .with("message", text(args, "message", "hello"))
                    .with("message_id", format!("sim-msg-{}", rng.below(1 << 40)))
                    .with("user", a.name.clone()),
                Some(a),
            )]
        }
        "tip" => {
            let a = Actor { platform: "kofi".into(), id: format!("sim-{}", rng.below(1 << 30)), name: text(args, "user", "Supporter"), roles: vec![] };
            vec![mk(
                "tip",
                Value::map()
                    .with("amount", arg(args, "amount").and_then(Value::as_f64).unwrap_or(5.0))
                    .with("currency", text(args, "currency", "USD"))
                    .with("message", text(args, "message", "Great stream!"))
                    .with("is_public", true)
                    .with("provider", "kofi")
                    .with("user", a.name.clone()),
                Some(a),
            )]
        }
        "ad_break" => vec![mk("twitch.ad_break", Value::map().with("duration", num(args, "duration", 90)).with("automatic", false), None)],
        "hype_train" => vec![mk("twitch.hype_train.begin", Value::map().with("level", num(args, "level", 1)), None)],
        other => return Err(format!("unknown simulator preset `{other}`")),
    })
}
