//! Alert routing: event → the first matching `[[alert]]` → variation (by expression) →
//! rendered title/message/TTS with the viewer's text content-filtered.

use crate::config::{AlertDef, AlertsConfig};
use crate::queue::{Alert, Tts};
use crate::stats::{event_user, tier};
use se_core::policy::{FilterCfg, filter_text};
use se_core::rng::Rng;
use se_expr::Scope;
use se_proto::{Event, Origin, Role, Value, address};

/// Read access to engine state for expressions and templates.
pub trait StateView {
    fn get(&self, address: &str) -> Option<Value>;
    fn mode(&self) -> String;
}

/// The amount an alert is "about": bits, tip amount, raid viewers, gift count, sub months.
pub fn amount(e: &Event) -> f64 {
    let n = |k: &str| e.payload.get_path(k).and_then(Value::as_f64);
    match e.ty.as_str() {
        "twitch.cheer" => n("bits"),
        "twitch.raid" => n("viewers"),
        "twitch.gift" => n("count"),
        "twitch.sub" | "twitch.resub" => n("months"),
        "twitch.redeem" => n("cost"),
        _ => n("amount"),
    }
    .unwrap_or(0.0)
}

/// `$5.00`, `€3.50`, `12.00 SEK`.
pub fn money(amount: f64, currency: &str) -> String {
    let sym = match currency.to_ascii_uppercase().as_str() {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        "CAD" => "CA$",
        "AUD" => "A$",
        "BRL" => "R$",
        "" => "",
        _ => return format!("{amount:.2} {}", currency.to_ascii_uppercase()),
    };
    format!("{sym}{amount:.2}")
}

/// Numbers print without a trailing `.0`.
fn num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 { format!("{}", v as i64) } else { format!("{v:.2}") }
}

struct Ctx<'a> {
    e: &'a Event,
    user: String,
    amount: f64,
    message: String,
    state: &'a dyn StateView,
    filter: &'a FilterCfg,
}

impl Ctx<'_> {
    /// Normalized fields first (`user`, `amount`, `tier`, `money`…), then payload fields
    /// (viewer text is filtered), then state.
    fn field(&self, name: &str) -> Option<Value> {
        let p = &self.e.payload;
        Some(match name {
            "user" => Value::Str(self.user.clone()),
            "amount" => Value::Float(self.amount),
            "message" => Value::Str(self.message.clone()),
            "tier" => tier(self.e).map(Value::Int).unwrap_or_default(),
            "kind" | "type" => Value::Str(self.e.ty.clone()),
            "money" => Value::Str(money(self.amount, p.get_path("currency").and_then(Value::as_str).unwrap_or(""))),
            "role" => Value::Str(self.e.actor.as_ref().map(|a| se_core::policy::role_name(a.top_role())).unwrap_or_else(|| "everyone".into())),
            "mode" => Value::Str(self.state.mode()),
            _ => {
                if let Some(v) = p.get_path(name) {
                    return Some(match v {
                        Value::Str(s) => Value::Str(filter_text(s, self.filter).unwrap_or_default()),
                        other => other.clone(),
                    });
                }
                if name.contains('.') {
                    return self.state.get(name);
                }
                return None;
            }
        })
    }

    fn text(&self, name: &str) -> Option<String> {
        self.field(name).map(|v| match v {
            Value::Float(f) => num(f),
            Value::Str(s) => s,
            Value::Null => String::new(),
            other => other.to_string(),
        })
    }
}

impl Scope for Ctx<'_> {
    fn lookup(&self, path: &str) -> Value {
        if path == "event" {
            return self.e.payload.clone();
        }
        if let Some(rest) = path.strip_prefix("event.") {
            return self.e.payload.get_path(rest).cloned().unwrap_or_default();
        }
        self.field(path).unwrap_or_default()
    }
}

/// Build the alert for `e`, if an enabled alert matches. `id` is assigned by the caller.
pub fn build(cfg: &AlertsConfig, e: &Event, state: &dyn StateView, filter: &FilterCfg, rng: &mut Rng, id: u64) -> Option<Alert> {
    let base = Ctx { e, user: event_user(e), amount: amount(e), message: String::new(), state, filter };
    let def: &AlertDef = cfg.alerts.iter().find(|d| d.enabled && address::matches(&d.when, &e.ty) && d.cond.as_ref().is_none_or(|c| c.eval_bool(&base)))?;
    let vetoed = e.payload.get_path("vetoed").is_some_and(Value::truthy);
    let raw = e.payload.get_path(&def.user_text).and_then(Value::as_str).unwrap_or("");
    let message = if vetoed || raw.trim().is_empty() {
        String::new()
    } else {
        match filter_text(raw, filter) {
            Ok(t) => se_bot::text::truncate_chars(&t, cfg.queue.max_message),
            Err(reason) => {
                tracing::info!(target: "alerts", "{} text withheld by the content filter ({reason})", e.ty);
                String::new()
            }
        }
    };
    let ctx = Ctx { message, ..base };
    let r = def.resolve(|c| c.eval_bool(&ctx));
    let mut lookup = |n: &str| ctx.text(n);
    let title = se_bot::template::render(&r.title, &mut lookup, rng);
    let msg = se_bot::template::render(&r.message, &mut lookup, rng);
    let tts = if r.tts {
        let t = se_bot::template::render(&r.tts_text, &mut lookup, rng);
        (!t.trim().is_empty()).then(|| Tts { text: t, voice: r.voice.clone() })
    } else {
        None
    };
    // commands are rendered per token so viewer text can't add tokens
    let cmds = r
        .cmds
        .iter()
        .filter_map(|c| {
            let toks = se_proto::command::tokenize(c).ok()?;
            let rendered: Vec<String> = toks.iter().map(|t| se_bot::template::render(t, &mut lookup, rng)).collect();
            Some(rendered)
        })
        .collect();
    let vetted = e.payload.get_path("vetted").is_some_and(Value::truthy);
    let viewer = e.actor.as_ref().is_some_and(|a| a.top_role() < Role::Owner);
    Some(Alert {
        id,
        kind: def.name.clone(),
        variation: r.variation,
        event_type: e.ty.clone(),
        title,
        needs_veto: def.veto && !ctx.message.is_empty() && !vetted && viewer,
        message: msg,
        user: ctx.user.clone(),
        user_id: e.actor.as_ref().map(|a| a.id.clone()).or_else(|| e.payload.get_path("user_id").and_then(Value::as_str).map(String::from)).unwrap_or_default(),
        message_id: e.payload.get_path("message_id").and_then(Value::as_str).map(String::from),
        amount: ctx.amount,
        currency: e.payload.get_path("currency").and_then(Value::as_str).map(String::from),
        tier: tier(e),
        sound: r.sound,
        image: r.image,
        duration_ms: r.duration_ms,
        priority: r.priority,
        interrupt: r.interrupt,
        tts,
        cmds,
        actor: e.actor.clone(),
        cause: Some(e.id),
        recipients: Vec::new(),
        sim: e.origin == Origin::Sim,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_file;
    use se_proto::Actor;
    use std::collections::HashMap;

    struct S(HashMap<String, Value>);
    impl StateView for S {
        fn get(&self, a: &str) -> Option<Value> {
            self.0.get(a).cloned()
        }
        fn mode(&self) -> String {
            "live".into()
        }
    }

    fn cfg(src: &str) -> AlertsConfig {
        let (q, alerts) = parse_file("t", "alerts/t.toml", &src.parse().unwrap()).unwrap();
        AlertsConfig { queue: q.unwrap_or_default(), alerts }
    }

    fn ev(ty: &str, payload: Value) -> Event {
        Event::new(ty, Origin::Twitch, payload).with_actor(Actor {
            platform: "twitch".into(),
            id: "u1".into(),
            name: "ana".into(),
            roles: vec![Role::Everyone],
        })
    }

    const SRC: &str = r#"
[[alert]]
name = "cheer"
when = "twitch.cheer"
title = "{user} cheered {amount} bits"
sound = "cheer"
tts = true
[[alert.variation]]
name = "big"
if = "amount >= 1000"
title = "{user} cheered {amount} BITS!"
priority = 80
voice = "bf_emma"

[[alert]]
name = "tip"
when = "tip"
title = "{user} tipped {money}"
do = ["patch.confetti.trigger count={amount}", "bot.say 'thanks {user}: {message}'"]

[[alert]]
name = "resub"
when = "twitch.resub"
if = "event.months >= 12"
title = "{user}: {months} months!"
"#;

    #[test]
    fn picks_alert_and_variation_and_renders() {
        let c = cfg(SRC);
        let st = S(HashMap::new());
        let f = FilterCfg { blocklist: vec!["badword".into()], ..Default::default() };
        let mut rng = Rng::new(1);
        let a = build(&c, &ev("twitch.cheer", Value::map().with("bits", 100).with("message", "Cheer100 hi")), &st, &f, &mut rng, 1).unwrap();
        assert_eq!((a.title.as_str(), a.variation.as_str(), a.priority), ("ana cheered 100 bits", "", 50));
        assert_eq!(a.message, "Cheer100 hi");
        assert!(a.needs_veto, "viewer text, not vetted by policy");
        assert_eq!(a.tts, Some(Tts { text: "Cheer100 hi".into(), voice: None }));
        let big = build(&c, &ev("twitch.cheer", Value::map().with("bits", 5000).with("message", "yo").with("vetted", true)), &st, &f, &mut rng, 2).unwrap();
        assert_eq!((big.title.as_str(), big.variation.as_str(), big.priority), ("ana cheered 5000 BITS!", "big", 80));
        assert!(!big.needs_veto, "already vetted by the policy hold");
        assert_eq!(big.tts.unwrap().voice.as_deref(), Some("bf_emma"));
        // blocked text is removed, the alert still shows
        let b = build(&c, &ev("twitch.cheer", Value::map().with("bits", 100).with("message", "badword")), &st, &f, &mut rng, 3).unwrap();
        assert!(b.message.is_empty() && b.tts.is_none() && !b.needs_veto);
        // condition on the alert itself
        assert!(build(&c, &ev("twitch.resub", Value::map().with("months", 3)), &st, &f, &mut rng, 4).is_none());
        assert_eq!(build(&c, &ev("twitch.resub", Value::map().with("months", 12)), &st, &f, &mut rng, 5).unwrap().title, "ana: 12 months!");
        assert!(build(&c, &ev("twitch.follow", Value::map()), &st, &f, &mut rng, 6).is_none());
    }

    #[test]
    fn money_and_commands_are_safe() {
        let c = cfg(SRC);
        let st = S(HashMap::new());
        let f = FilterCfg::default();
        let mut rng = Rng::new(1);
        let a = build(&c, &ev("tip", Value::map().with("amount", 5.5).with("currency", "EUR").with("message", "x; panic")), &st, &f, &mut rng, 1).unwrap();
        assert_eq!(a.title, "ana tipped €5.50");
        assert_eq!(a.cmds[0], vec!["patch.confetti.trigger".to_string(), "count=5.50".to_string()]);
        // viewer text stays inside its one token
        assert_eq!(a.cmds[1], vec!["bot.say".to_string(), "thanks ana: x; panic".to_string()]);
        assert!(se_proto::Op::from_tokens(&a.cmds[1]).is_ok());
        assert_eq!(money(12.0, "sek"), "12.00 SEK");
    }
}
