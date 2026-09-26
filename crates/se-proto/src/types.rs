//! Metadata, origins, actors, events, and ids.

use crate::Value;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

/// Master-clock timestamp in nanoseconds (CLOCK_MONOTONIC).
pub type Ts = u64;
/// Unique id for events, commands, and trace records.
pub type Id = u64;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Allocate a process-unique id that is also unique across restarts: the counter is seeded
/// from wall-clock microseconds on first use.
pub fn next_id() -> Id {
    let cur = NEXT_ID.load(Ordering::Relaxed);
    if cur == 0 {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(1);
        let _ = NEXT_ID.compare_exchange(0, seed, Ordering::Relaxed, Ordering::Relaxed);
    }
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ValueType {
    #[default]
    Any,
    Float,
    Int,
    Bool,
    Color,
    Enum,
    String,
    Vec2,
    Vec4,
    TextureRef,
    /// A triggerable address (`patch.x`, `fx.rgb_split`): has an envelope and fires events.
    Trigger,
    List,
    Map,
}

/// How competing overrides combine for one address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Merge {
    /// Latest-takes-precedence among the highest priority.
    #[default]
    Ltp,
    /// Highest-takes-precedence (numeric max) across all overrides (light intensity).
    Htp,
}

/// Metadata for an address. The UI is generated from it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Meta {
    #[serde(rename = "type")]
    pub ty: ValueType,
    pub range: Option<[f64; 2]>,
    pub default: Value,
    pub unit: Option<String>,
    pub description: Option<String>,
    /// Allowed values for `enum`.
    pub options: Vec<String>,
    pub readonly: bool,
    pub merge: Merge,
    /// Owning subsystem (for UI grouping and supported-address reporting).
    pub owner: Option<String>,
}

impl Meta {
    pub fn float(default: f64, range: [f64; 2]) -> Self {
        Meta { ty: ValueType::Float, range: Some(range), default: Value::Float(default), ..Default::default() }
    }
    pub fn int(default: i64, range: [f64; 2]) -> Self {
        Meta { ty: ValueType::Int, range: Some(range), default: Value::Int(default), ..Default::default() }
    }
    pub fn boolean(default: bool) -> Self {
        Meta { ty: ValueType::Bool, default: Value::Bool(default), ..Default::default() }
    }
    pub fn string(default: &str) -> Self {
        Meta { ty: ValueType::String, default: Value::Str(default.into()), ..Default::default() }
    }
    pub fn enumeration(default: &str, options: &[&str]) -> Self {
        Meta { ty: ValueType::Enum, default: Value::Str(default.into()), options: options.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }
    pub fn color(default: [f32; 4]) -> Self {
        Meta { ty: ValueType::Color, default: Value::from(default), ..Default::default() }
    }
    pub fn vec4(default: [f32; 4]) -> Self {
        Meta { ty: ValueType::Vec4, default: Value::from(default), ..Default::default() }
    }
    pub fn trigger() -> Self {
        Meta { ty: ValueType::Trigger, default: Value::Float(0.0), range: Some([0.0, 1.0]), ..Default::default() }
    }
    pub fn readonly(mut self) -> Self {
        self.readonly = true;
        self
    }
    pub fn owner(mut self, o: &str) -> Self {
        self.owner = Some(o.into());
        self
    }
    pub fn unit(mut self, u: &str) -> Self {
        self.unit = Some(u.into());
        self
    }
    pub fn describe(mut self, d: &str) -> Self {
        self.description = Some(d.into());
        self
    }
    pub fn htp(mut self) -> Self {
        self.merge = Merge::Htp;
        self
    }

    /// Coerce and clamp a value to this metadata (numeric ranges, enum options, type casts).
    pub fn coerce(&self, v: &Value) -> Value {
        match self.ty {
            ValueType::Float | ValueType::Trigger => match v.as_f64() {
                Some(mut f) => {
                    if let Some([lo, hi]) = self.range {
                        f = f.clamp(lo, hi);
                    }
                    Value::Float(f)
                }
                None => self.default.clone(),
            },
            ValueType::Int => match v.as_f64() {
                Some(f) => {
                    let mut i = f.round() as i64;
                    if let Some([lo, hi]) = self.range {
                        i = i.clamp(lo as i64, hi as i64);
                    }
                    Value::Int(i)
                }
                None => self.default.clone(),
            },
            ValueType::Bool => Value::Bool(v.truthy()),
            ValueType::Enum => match v.as_str() {
                Some(s) if self.options.is_empty() || self.options.iter().any(|o| o == s) => v.clone(),
                _ => self.default.clone(),
            },
            ValueType::String => match v {
                Value::Str(_) => v.clone(),
                Value::Null => self.default.clone(),
                other => Value::Str(other.to_string()),
            },
            _ => v.clone(),
        }
    }
}

/// Where a command or event came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Ui,
    Rule,
    Binding,
    Patch,
    Cli,
    Api,
    Deck,
    Midi,
    Voice,
    Chat,
    Mixer,
    Timeline,
    Osc,
    Twitch,
    Relay,
    Sim,
    Audio,
    Obs,
    #[default]
    System,
}

impl Origin {
    /// Default override priority for commands from this origin (§2.1).
    pub fn default_priority(self) -> u16 {
        match self {
            Origin::Ui | Origin::Cli | Origin::Api | Origin::Deck | Origin::Midi | Origin::Voice | Origin::Osc | Origin::Mixer => PRIORITY_MANUAL,
            Origin::Chat | Origin::Twitch | Origin::Relay => PRIORITY_CHAT,
            _ => PRIORITY_PRESET,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Ui => "ui",
            Origin::Rule => "rule",
            Origin::Binding => "binding",
            Origin::Patch => "patch",
            Origin::Cli => "cli",
            Origin::Api => "api",
            Origin::Deck => "deck",
            Origin::Midi => "midi",
            Origin::Voice => "voice",
            Origin::Chat => "chat",
            Origin::Mixer => "mixer",
            Origin::Timeline => "timeline",
            Origin::Osc => "osc",
            Origin::Twitch => "twitch",
            Origin::Relay => "relay",
            Origin::Sim => "sim",
            Origin::Audio => "audio",
            Origin::Obs => "obs",
            Origin::System => "system",
        }
    }
}

pub const PRIORITY_MANUAL: u16 = 300;
pub const PRIORITY_PRESET: u16 = 200;
pub const PRIORITY_CHAT: u16 = 100;

/// Viewer role ladder (§12.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    Everyone,
    Follower,
    Sub,
    Vip,
    Mod,
    Owner,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "everyone" => Role::Everyone,
            "follower" => Role::Follower,
            "sub" | "subscriber" => Role::Sub,
            "vip" => Role::Vip,
            "mod" | "moderator" => Role::Mod,
            "owner" | "broadcaster" => Role::Owner,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct Actor {
    pub platform: String,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub roles: Vec<Role>,
}

impl Actor {
    pub fn top_role(&self) -> Role {
        self.roles.iter().copied().max().unwrap_or(Role::Everyone)
    }
}

/// A discrete, timestamped occurrence (§2.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: Id,
    pub ts: Ts,
    #[serde(rename = "type")]
    pub ty: String,
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<Actor>,
    #[serde(default)]
    pub payload: Value,
    /// Id of the event/command that caused this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causal: Option<Id>,
}

impl Event {
    pub fn new(ty: impl Into<String>, origin: Origin, payload: Value) -> Self {
        Event { id: next_id(), ts: 0, ty: ty.into(), origin, actor: None, payload, causal: None }
    }
    pub fn with_actor(mut self, a: Actor) -> Self {
        self.actor = Some(a);
        self
    }
    pub fn with_causal(mut self, c: Option<Id>) -> Self {
        self.causal = c;
        self
    }
}
