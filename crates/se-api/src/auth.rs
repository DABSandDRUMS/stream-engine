//! API authorization (§19): the full-access token (keyring), per-device pairing tokens with
//! read/write scopes, scoped tokens for web patches, and the restricted mod scope.

use parking_lot::RwLock;
use se_proto::Op;
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    Full,
    ReadOnly,
    /// A web patch: writes only to `patch.<id>.*`, reads everything.
    Patch(String),
    /// Remote moderators: queue, moderation, alerts; never mixer/lights/scenes (§19).
    Mod,
}

impl Scope {
    pub fn allows(&self, op: &Op) -> bool {
        match self {
            Scope::Full => true,
            Scope::ReadOnly => false,
            Scope::Patch(id) => {
                let own = |a: &str| a == format!("patch.{id}") || a.starts_with(&format!("patch.{id}."));
                match op {
                    Op::Set { address, .. } | Op::Animate { address, .. } | Op::Trigger { address, .. } | Op::Release { address } => own(address),
                    Op::Emit { ty, .. } => own(ty),
                    Op::Action { name, .. } => own(name),
                    _ => false,
                }
            }
            Scope::Mod => match op {
                Op::Clean => true,
                Op::Action { name, .. } => ["queue.", "mod.", "alerts.", "bot.say"].iter().any(|p| name.starts_with(p)),
                _ => false,
            },
        }
    }

    pub fn may_query(&self, name: &str) -> bool {
        match self {
            Scope::Mod => ["queue", "mod", "alerts", "presets", "chat"].iter().any(|p| name == *p || name.starts_with(&format!("{p}."))),
            _ => true,
        }
    }
}

#[derive(Default)]
pub struct Auth {
    full: RwLock<Option<String>>,
    tokens: RwLock<HashMap<String, (String, Scope)>>,
}

impl Auth {
    pub fn new(full_token: Option<String>) -> Auth {
        Auth { full: RwLock::new(full_token), tokens: RwLock::new(HashMap::new()) }
    }

    pub fn set_full(&self, t: Option<String>) {
        *self.full.write() = t;
    }

    /// Register a scoped token (device pairing, web patch, mod session).
    pub fn add(&self, token: &str, name: &str, scope: Scope) {
        self.tokens.write().insert(token.into(), (name.into(), scope));
    }

    pub fn remove(&self, token: &str) {
        self.tokens.write().remove(token);
    }

    pub fn remove_named(&self, name: &str) {
        self.tokens.write().retain(|_, (n, _)| n != name);
    }

    pub fn check(&self, token: &str) -> Option<Scope> {
        if token.is_empty() {
            return None;
        }
        if self.full.read().as_deref().is_some_and(|f| constant_eq(f, token)) {
            return Some(Scope::Full);
        }
        self.tokens.read().iter().find(|(t, _)| constant_eq(t, token)).map(|(_, (_, s))| s.clone())
    }

    pub fn devices(&self) -> Vec<(String, Scope)> {
        self.tokens.read().values().cloned().collect()
    }
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_proto::Value;

    #[test]
    fn scopes() {
        let p = Scope::Patch("confetti".into());
        assert!(p.allows(&Op::Set { address: "patch.confetti.count".into(), value: Value::Int(1) }));
        assert!(!p.allows(&Op::Set { address: "patch.other.count".into(), value: Value::Int(1) }));
        assert!(!p.allows(&Op::Set { address: "mixer.16r.main.fader".into(), value: Value::Int(1) }));
        assert!(!p.allows(&Op::Panic));
        assert!(Scope::Mod.allows(&Op::Action { name: "queue.skip".into(), args: Value::Null }));
        assert!(!Scope::Mod.allows(&Op::Action { name: "lights.cue".into(), args: Value::Null }));
        assert!(!Scope::Mod.allows(&Op::SceneGo { scene: "x".into() }));
        assert!(!Scope::ReadOnly.allows(&Op::Clean));
    }

    #[test]
    fn tokens() {
        let a = Auth::new(Some("full".into()));
        a.add("dev1", "phone", Scope::ReadOnly);
        assert_eq!(a.check("full"), Some(Scope::Full));
        assert_eq!(a.check("dev1"), Some(Scope::ReadOnly));
        assert_eq!(a.check("nope"), None);
        assert_eq!(a.check(""), None);
    }
}
