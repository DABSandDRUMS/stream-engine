//! Commands: the only way to mutate state (§2.2), plus the one-line text syntax used in
//! rules, presets, timelines, chat commands, and the CLI.

use crate::{Actor, Id, Origin, Ts, Value, next_id};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Ease {
    #[default]
    Linear,
    InQuad,
    OutQuad,
    InOutQuad,
    InCubic,
    OutCubic,
    InOutCubic,
    Smoothstep,
    OutBack,
    Step,
}

impl Ease {
    pub fn apply(self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Ease::Linear => t,
            Ease::InQuad => t * t,
            Ease::OutQuad => 1.0 - (1.0 - t) * (1.0 - t),
            Ease::InOutQuad => {
                if t < 0.5 {
                    2.0 * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
                }
            }
            Ease::InCubic => t * t * t,
            Ease::OutCubic => 1.0 - (1.0 - t).powi(3),
            Ease::InOutCubic => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
            Ease::Smoothstep => t * t * (3.0 - 2.0 * t),
            Ease::OutBack => {
                let c1 = 1.70158;
                let c3 = c1 + 1.0;
                1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
            }
            Ease::Step => {
                if t >= 1.0 {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }

    pub fn parse(s: &str) -> Option<Ease> {
        serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
    }
}

/// A state mutation or action request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Command {
    #[serde(default = "next_id")]
    pub id: Id,
    #[serde(default)]
    pub ts: Ts,
    #[serde(default)]
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<Actor>,
    /// Event or command that caused this command (trace chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causal: Option<Id>,
    /// Override priority; defaults from `origin`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u16>,
    /// Override layer key (e.g. `cuelist:<name>`, `timeline:<name>`): overrides with the same
    /// key replace each other and are released together. Defaults from origin/actor.
    /// Ignored for chat-priority commands (their key is always `chat:<actor>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub op: Op,
}

impl Command {
    pub fn new(origin: Origin, op: Op) -> Self {
        Command { id: next_id(), ts: 0, origin, actor: None, causal: None, priority: None, key: None, op }
    }
    pub fn priority(&self) -> u16 {
        self.priority.unwrap_or_else(|| self.origin.default_priority())
    }
    pub fn caused_by(mut self, id: Option<Id>) -> Self {
        self.causal = id;
        self
    }
    pub fn with_actor(mut self, a: Option<Actor>) -> Self {
        self.actor = a;
        self
    }
    pub fn with_priority(mut self, p: Option<u16>) -> Self {
        self.priority = p;
        self
    }
    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Op {
    /// Runtime override at the command's priority (manual/preset/chat layer).
    Set {
        address: String,
        value: Value,
    },
    /// Edit the base (project) value; persisted to the project file and undoable.
    SetBase {
        address: String,
        value: Value,
    },
    /// Animate the override for `address` to `to` over `ms`.
    Animate {
        address: String,
        to: Value,
        ms: u32,
        #[serde(default)]
        ease: Ease,
    },
    /// Fire a triggerable address (`patch.x`, `fx.rgb_split`, `audio.bus.music.fx.stutter`).
    Trigger {
        address: String,
        #[serde(default)]
        payload: Value,
    },
    /// Release an override or trigger held by this origin/actor on `address`.
    Release {
        address: String,
    },
    /// Scene to preview.
    SceneGo {
        scene: String,
    },
    /// Scene straight to program (with the selected transition).
    SceneCut {
        scene: String,
        #[serde(default)]
        transition: Option<String>,
    },
    /// Preview → program.
    SceneTake {
        #[serde(default)]
        transition: Option<String>,
        #[serde(default)]
        ms: Option<u32>,
    },
    PresetFire {
        name: String,
        #[serde(default)]
        payload: Value,
    },
    PresetRelease {
        name: String,
    },
    ModeSet {
        mode: String,
    },
    /// Inject an event into the core (simulator, patches, adapters).
    Emit {
        #[serde(rename = "type")]
        ty: String,
        #[serde(default)]
        payload: Value,
    },
    Panic,
    Clean,
    Undo,
    Redo,
    /// Delay inside a command list (rules/presets/timelines).
    Wait {
        ms: u32,
    },
    /// Subsystem action routed by name prefix (`lights.cue`, `queue.skip`, `bot.say`, `obs.stream.start`, …).
    Action {
        name: String,
        #[serde(default)]
        args: Value,
    },
}

impl Op {
    /// Actions that carry secrets (`youtube.key.set`, `relay.secret.set`, `*.token.set`): their
    /// args must never reach logs, traces, sessions, or the audit table (§19).
    pub fn is_secret(&self) -> bool {
        matches!(self, Op::Action { name, .. } if name.ends_with(".key.set") || name.ends_with(".secret.set") || name.ends_with(".token.set")
            // the UI's write-only secret entry and device pairing carry the secret in their args
            || name == "secrets.set" || name == "api.device.add")
    }

    /// Copy safe for persistence: secret-carrying args replaced by `{redacted: true}`.
    pub fn redacted(&self) -> Op {
        match self {
            Op::Action { name, .. } if self.is_secret() => Op::Action { name: name.clone(), args: Value::map().with("redacted", true) },
            other => other.clone(),
        }
    }

    /// Short display form (for traces and logs). Secret args are never shown.
    pub fn describe(&self) -> String {
        if self.is_secret()
            && let Op::Action { name, .. } = self
        {
            return format!("{name} (redacted)");
        }
        match self {
            Op::Set { address, value } => format!("set {address} {value}"),
            Op::SetBase { address, value } => format!("set_base {address} {value}"),
            Op::Animate { address, to, ms, .. } => format!("animate {address} {to} {ms}ms"),
            Op::Trigger { address, .. } => format!("trigger {address}"),
            Op::Release { address } => format!("release {address}"),
            Op::SceneGo { scene } => format!("scene.go {scene}"),
            Op::SceneCut { scene, .. } => format!("scene.cut {scene}"),
            Op::SceneTake { transition, .. } => format!("scene.take {}", transition.as_deref().unwrap_or("")),
            Op::PresetFire { name, .. } => format!("preset.fire {name}"),
            Op::PresetRelease { name } => format!("preset.release {name}"),
            Op::ModeSet { mode } => format!("mode.set {mode}"),
            Op::Emit { ty, .. } => format!("emit {ty}"),
            Op::Panic => "panic".into(),
            Op::Clean => "clean".into(),
            Op::Undo => "undo".into(),
            Op::Redo => "redo".into(),
            Op::Wait { ms } => format!("wait {ms}ms"),
            Op::Action { name, args } => {
                if args.is_null() {
                    name.clone()
                } else {
                    format!("{name} {args}")
                }
            }
        }
    }

    /// Parse the one-line command syntax.
    ///
    /// ```text
    /// set <addr> <value>            animate <addr> <to> <dur> [ease]
    /// trigger <addr> [k=v …]        <addr>.trigger [k=v …]      release <addr>
    /// scene.go <name>               scene.cut <name> [transition]
    /// scene.take [transition] [dur] preset.fire <name> [k=v …]  preset.release <name>
    /// mode.set <mode>               emit <type> [k=v …]        wait <dur>
    /// panic | clean | undo | redo   <any.action> [positional …] [k=v …]
    /// ```
    pub fn parse(text: &str) -> Result<Op, ParseError> {
        Op::from_tokens(&tokenize(text)?)
    }

    /// Build from already-split tokens (used after per-token templating so substituted text
    /// can never introduce new tokens).
    pub fn from_tokens(toks: &[String]) -> Result<Op, ParseError> {
        let Some((verb, rest)) = toks.split_first() else {
            return Err(ParseError("empty command".into()));
        };
        let verb = verb.as_str();
        let arg = |i: usize, what: &str| -> Result<&String, ParseError> { rest.get(i).ok_or_else(|| ParseError(format!("`{verb}` needs {what}"))) };
        Ok(match verb {
            "set" => Op::Set { address: arg(0, "an address")?.clone(), value: Value::parse_text(arg(1, "a value")?) },
            "set_base" => Op::SetBase { address: arg(0, "an address")?.clone(), value: Value::parse_text(arg(1, "a value")?) },
            "animate" => Op::Animate {
                address: arg(0, "an address")?.clone(),
                to: Value::parse_text(arg(1, "a target value")?),
                ms: parse_ms(arg(2, "a duration")?).ok_or_else(|| ParseError("bad duration".into()))?,
                ease: rest.get(3).and_then(|e| Ease::parse(e)).unwrap_or_default(),
            },
            "trigger" => Op::Trigger { address: arg(0, "an address")?.clone(), payload: kv_payload(&rest[1..]) },
            "release" => Op::Release { address: arg(0, "an address")?.clone() },
            "scene.go" => Op::SceneGo { scene: arg(0, "a scene")?.clone() },
            "scene.cut" => Op::SceneCut { scene: arg(0, "a scene")?.clone(), transition: rest.get(1).cloned() },
            "scene.take" => Op::SceneTake { transition: rest.iter().find(|t| parse_ms(t).is_none()).cloned(), ms: rest.iter().find_map(|t| parse_ms(t)) },
            "preset.fire" => Op::PresetFire { name: arg(0, "a preset")?.clone(), payload: kv_payload(&rest[1..]) },
            "preset.release" => Op::PresetRelease { name: arg(0, "a preset")?.clone() },
            "mode.set" => Op::ModeSet { mode: arg(0, "a mode")?.clone() },
            "emit" | "fire" => Op::Emit { ty: arg(0, "an event type")?.clone(), payload: kv_payload(&rest[1..]) },
            "wait" => Op::Wait { ms: parse_ms(arg(0, "a duration")?).ok_or_else(|| ParseError("bad duration".into()))? },
            "panic" => Op::Panic,
            "clean" => Op::Clean,
            "undo" => Op::Undo,
            "redo" => Op::Redo,
            v if v.ends_with(".trigger") && rest.iter().all(|t| t.contains('=')) => {
                Op::Trigger { address: v.trim_end_matches(".trigger").to_string(), payload: kv_payload(rest) }
            }
            v if v.ends_with(".release") && rest.is_empty() => Op::Release { address: v.trim_end_matches(".release").to_string() },
            v => {
                if !crate::address::is_valid(v, false) {
                    return Err(ParseError(format!("unknown command `{v}`")));
                }
                Op::Action { name: v.to_string(), args: action_args(rest) }
            }
        })
    }
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(pub String);

/// Split on whitespace honoring single/double quotes.
pub fn tokenize(s: &str) -> Result<Vec<String>, ParseError> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0i32;
    let mut has = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => {
                quote = None;
                if depth > 0 {
                    cur.push(c);
                }
            }
            Some(_) => cur.push(c),
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    has = true;
                    if depth > 0 {
                        cur.push(c);
                    }
                }
                '[' | '{' => {
                    depth += 1;
                    cur.push(c);
                    has = true;
                }
                ']' | '}' => {
                    depth -= 1;
                    cur.push(c);
                }
                c if c.is_whitespace() && depth == 0 => {
                    if has || !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                        has = false;
                    }
                }
                c => {
                    cur.push(c);
                    has = true;
                }
            },
        }
    }
    if quote.is_some() {
        return Err(ParseError("unterminated quote".into()));
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

fn kv_payload(toks: &[String]) -> Value {
    let mut m = BTreeMap::new();
    for t in toks {
        if let Some((k, v)) = t.split_once('=') {
            m.insert(k.to_string(), Value::parse_text(v));
        }
    }
    if m.is_empty() { Value::Null } else { Value::Map(m) }
}

/// Positional args under `"args"` plus `k=v` pairs.
fn action_args(toks: &[String]) -> Value {
    if toks.is_empty() {
        return Value::Null;
    }
    let mut m = BTreeMap::new();
    let mut pos = Vec::new();
    for t in toks {
        match t.split_once('=') {
            Some((k, v)) if crate::address::is_valid(k, false) => {
                m.insert(k.to_string(), Value::parse_text(v));
            }
            _ => pos.push(Value::parse_text(t)),
        }
    }
    if !pos.is_empty() {
        m.insert("args".into(), Value::List(pos));
    }
    Value::Map(m)
}

/// Parse durations: `8s`, `100ms`, `5m`, `1h`, `1:12.400` (m:ss.mmm), `1:02:03`, plain number = ms.
pub fn parse_ms(s: &str) -> Option<u32> {
    parse_duration_ms(s).and_then(|v| u32::try_from(v).ok())
}

pub fn parse_duration_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        let mut secs = 0.0f64;
        for p in &parts {
            secs = secs * 60.0 + p.parse::<f64>().ok()?;
        }
        return Some((secs * 1000.0).round() as u64);
    }
    let (num, mul) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000.0)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600_000.0)
    } else {
        (s, 1.0)
    };
    let v: f64 = num.trim().parse().ok()?;
    if v < 0.0 {
        return None;
    }
    Some((v * mul).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_forms() {
        assert_eq!(Op::parse("preset.fire sub_big").unwrap(), Op::PresetFire { name: "sub_big".into(), payload: Value::Null });
        assert_eq!(Op::parse("set lights.par_left.dimmer 0.5").unwrap(), Op::Set { address: "lights.par_left.dimmer".into(), value: Value::Float(0.5) });
        assert_eq!(
            Op::parse("patch.confetti.trigger count=40").unwrap(),
            Op::Trigger { address: "patch.confetti".into(), payload: Value::map().with("count", 40) }
        );
        assert_eq!(Op::parse("wait 2s").unwrap(), Op::Wait { ms: 2000 });
        assert_eq!(Op::parse("lights.cue blackout").unwrap(), Op::Action { name: "lights.cue".into(), args: Value::map().with("args", vec!["blackout"]) });
        assert_eq!(
            Op::parse("emit twitch.cheer bits=1000 message='hello there'").unwrap(),
            Op::Emit { ty: "twitch.cheer".into(), payload: Value::map().with("bits", 1000).with("message", "hello there") }
        );
        assert_eq!(Op::parse("set a.b [1, 2]").unwrap(), Op::Set { address: "a.b".into(), value: Value::parse_text("[1, 2]") });
        assert!(Op::parse("").is_err());
        assert!(Op::parse("$(boom) x").is_err());
    }

    #[test]
    fn secrets_redacted() {
        let op = Op::parse("youtube.key.set AIzaSECRET").unwrap();
        assert!(op.is_secret());
        assert!(!op.describe().contains("SECRET"));
        assert!(!serde_json::to_string(&op.redacted()).unwrap().contains("SECRET"));
        assert!(!Op::parse("preset.fire hype").unwrap().is_secret());
        for line in ["secrets.set name=youtube.key value=SECRET", "api.device.add name=phone token=SECRETSECRETSECRETSECRET scope=read"] {
            let op = Op::parse(line).unwrap();
            assert!(op.is_secret(), "{line}");
            assert!(!serde_json::to_string(&op.redacted()).unwrap().contains("SECRET"), "{line}");
            assert!(!op.describe().contains("SECRET"), "{line}");
        }
        assert!(!Op::parse("api.device.remove phone").unwrap().is_secret());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("8s"), Some(8000));
        assert_eq!(parse_duration_ms("100ms"), Some(100));
        assert_eq!(parse_duration_ms("5m"), Some(300_000));
        assert_eq!(parse_duration_ms("1:12.400"), Some(72_400));
        assert_eq!(parse_duration_ms("250"), Some(250));
        assert_eq!(parse_duration_ms("-1s"), None);
    }

    #[test]
    fn command_wire_json() {
        let c = Command::new(Origin::Cli, Op::PresetFire { name: "hype".into(), payload: Value::Null });
        let js = serde_json::to_string(&c).unwrap();
        let back: Command = serde_json::from_str(&js).unwrap();
        assert_eq!(back, c);
        let mp = rmp_serde::to_vec_named(&c).unwrap();
        assert_eq!(rmp_serde::from_slice::<Command>(&mp).unwrap(), c);
    }
}
