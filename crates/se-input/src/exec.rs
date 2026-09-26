//! Running key/button actions against the core, and reading their state back for feedback.
//! Shared by Stream Deck keys, MIDI buttons, and the UI pad grid (`deck.press`).

use crate::config::{Action, Behavior};
use se_hub::{Hub, Snapshot};
use se_proto::{Command, Id, Op, Origin, Value};
use std::sync::Arc;
use std::time::Duration;

/// Lit state of a key/LED.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct KeyState {
    pub on: bool,
    pub program: bool,
    pub preview: bool,
}

/// Sends commands on behalf of a controller.
#[derive(Clone)]
pub struct Exec {
    pub hub: Arc<Hub>,
    pub rt: tokio::runtime::Handle,
}

impl Exec {
    pub fn op(&self, origin: Origin, op: Op, causal: Option<Id>) {
        self.hub.command(Command::new(origin, op).caused_by(causal));
    }

    /// Run a command list; `wait <dur>` delays the rest of the list.
    pub fn run_list(&self, origin: Origin, cmds: &[String], causal: Option<Id>) {
        let mut rest: Vec<Op> = Vec::new();
        for c in cmds {
            match Op::parse(c) {
                Ok(op) => rest.push(op),
                Err(e) => self.hub.log("error", "controllers", format!("command `{c}`: {e}")),
            }
        }
        let first_wait = rest.iter().position(|o| matches!(o, Op::Wait { .. }));
        let now: Vec<Op> = match first_wait {
            Some(i) => rest.drain(..i).collect(),
            None => std::mem::take(&mut rest),
        };
        for op in now {
            self.op(origin, op, causal);
        }
        if rest.is_empty() {
            return;
        }
        let me = self.clone();
        self.rt.spawn(async move {
            for op in rest {
                match op {
                    Op::Wait { ms } => tokio::time::sleep(Duration::from_millis(ms as u64)).await,
                    op => me.op(origin, op, causal),
                }
            }
        });
    }

    /// Press. `page` switches pages for deck-local page actions (returns the requested page).
    pub fn press(&self, a: &Action, origin: Origin, causal: Option<Id>) -> Option<String> {
        let snap = self.hub.snapshot.load();
        match a {
            Action::None => {}
            Action::Preset(p) => self.op(origin, Op::PresetFire { name: p.clone(), payload: Value::Null }, causal),
            Action::Scene { name, cut: false } => self.op(origin, Op::SceneGo { scene: name.clone() }, causal),
            Action::Scene { name, cut: true } => self.op(origin, Op::SceneCut { scene: name.clone(), transition: None }, causal),
            Action::Toggle(addr) => {
                let v = match snap.get(addr) {
                    Some(Value::Bool(b)) => Value::Bool(!b),
                    Some(v @ (Value::Int(_) | Value::Float(_))) => {
                        if v.as_f64().unwrap_or(0.0) > 0.5 {
                            Value::Float(0.0)
                        } else {
                            Value::Float(1.0)
                        }
                    }
                    _ => Value::Bool(true),
                };
                self.op(origin, Op::Set { address: addr.clone(), value: v }, causal);
            }
            Action::Momentary(addr) => {
                let v = match snap.get(addr) {
                    Some(Value::Int(_) | Value::Float(_)) => Value::Float(1.0),
                    _ => Value::Bool(true),
                };
                self.op(origin, Op::Set { address: addr.clone(), value: v }, causal);
            }
            Action::Commands { press, .. } => self.run_list(origin, press, causal),
            Action::Page(p) => return Some(p.clone()),
            Action::Ptt => self.op(origin, voice_op("start"), causal),
        }
        None
    }

    pub fn release(&self, a: &Action, origin: Origin, causal: Option<Id>) {
        match a {
            Action::Momentary(addr) => self.op(origin, Op::Release { address: addr.clone() }, causal),
            Action::Commands { release, .. } if !release.is_empty() => self.run_list(origin, release, causal),
            Action::Ptt => self.op(origin, voice_op("stop"), causal),
            _ => {}
        }
    }
}

fn voice_op(what: &str) -> Op {
    Op::Action { name: "voice.ptt".into(), args: Value::map().with("args", vec![Value::from(what)]) }
}

/// Current state of an action (for key colors and LEDs). `page` = the controller's page.
pub fn state(a: &Action, b: &Behavior, snap: &Snapshot, page: &str) -> KeyState {
    if let Some(addr) = &b.state {
        return KeyState { on: snap.bool(addr), ..Default::default() };
    }
    match a {
        Action::Preset(p) => KeyState { on: snap.bool(&format!("preset.{p}.active")), ..Default::default() },
        Action::Scene { name, .. } => {
            let program = snap.str("show.scene.program") == Some(name.as_str());
            let preview = snap.str("show.scene.preview") == Some(name.as_str());
            KeyState { on: program, program, preview }
        }
        Action::Toggle(addr) | Action::Momentary(addr) => KeyState { on: snap.bool(addr), ..Default::default() },
        Action::Page(p) => KeyState { on: p == page, ..Default::default() },
        Action::Ptt => KeyState { on: snap.bool("controllers.voice.active"), ..Default::default() },
        Action::Commands { .. } | Action::None => KeyState::default(),
    }
}

/// Presets marked `confirm` in their definition need confirmation on controllers too.
pub fn needs_confirm(a: &Action, b: &Behavior, cfg: &se_core::Config) -> bool {
    if let Some(c) = b.confirm {
        return c;
    }
    match a {
        Action::Preset(p) => cfg.presets.get(p).is_some_and(|d| d.confirm),
        Action::Commands { press, .. } => press.iter().any(|c| matches!(Op::parse(c), Ok(Op::Panic))) && b.hold_ms == 0,
        _ => false,
    }
}
