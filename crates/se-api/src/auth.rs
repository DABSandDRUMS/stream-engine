//! API authorization (§19): the full-access token (keyring), per-device pairing tokens with
//! read/write scopes, scoped tokens for web patches, and the restricted mod scope.

use parking_lot::RwLock;
use se_proto::{Op, address};
use std::collections::HashMap;

/// Namespaces and actions that stay full-access only whatever a web patch's grants say (§19):
/// API tokens and device pairing, secrets, project file writes, creating and removing patches and
/// launching an editor. Secret-carrying actions ([`Op::is_secret`]) and `set_base` are never grantable either.
const ADMIN_NAMESPACES: &[&str] = &["api", "secrets", "project"];
const ADMIN_ACTIONS: &[&str] = &["patch.new", "patch.open", "patch.remove"];
const ADMIN_ACTION_PREFIXES: &[&str] = &["fx.chain.", "fx.slot.", "fx.group."];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    Full,
    ReadOnly,
    /// A web patch `(id, grants)`: reads everything; writes its own `patch.<id>.*` plus whatever
    /// its manifest `grants` cover (see [`Scope::allows`]).
    Patch(String, Vec<String>),
    /// Remote moderators: queue, moderation, alerts; never mixer/lights/scenes (§19).
    Mod,
}

impl Scope {
    /// Whether this scope may submit `op`.
    ///
    /// Web patches: an op's target is its address (set/animate/trigger/release), event type
    /// (emit), action name, or the command word for scene/preset/mode ops (`scene.go`,
    /// `scene.cut`, `scene.take`, `preset.fire`, `preset.release`, `mode.set`). The patch's own
    /// `patch.<id>` subtree is always writable; a grant (an address pattern, `*`/`**` as in
    /// set/animate/release) covers every target it matches and everything beneath it. Admin
    /// targets (`api.*`, `secrets.*`, `project.*`, `patch.new`, `patch.open`, `patch.remove`, secret-carrying
    /// actions) and ops without a target (`set_base`, panic, clean, undo, redo) are never granted.
    pub fn allows(&self, op: &Op) -> bool {
        match self {
            Scope::Full => true,
            Scope::ReadOnly => false,
            Scope::Patch(id, grants) => {
                let Some(target) = patch_target(op) else { return false };
                let own = target.strip_prefix("patch.").and_then(|r| r.strip_prefix(id.as_str())).is_some_and(|r| r.is_empty() || r.starts_with('.'));
                // admin first: a patch folder named `new`/`open` must not reach `patch.new`/`patch.open`
                !admin_only(op, target) && (own || grants.iter().any(|g| covers(g, target)))
            }
            Scope::Mod => match op {
                Op::Clean => true,
                Op::Action { name, .. } => {
                    ["queue.", "mod.", "alerts.", "giveaway.", "bot.say"].iter().any(|p| name.starts_with(p))
                        || matches!(name.as_str(), "tts.skip" | "tts.clear")
                }
                _ => false,
            },
        }
    }

    pub fn may_query(&self, name: &str) -> bool {
        // These queries mutate state or durable metadata. Internal hub callers bypass API
        // authorization; external read/patch/mod clients must not reach those barriers.
        if name == "remote_mod" || name.starts_with("remote_mod.") || matches!(name, "recording.finalize" | "session.persist") {
            return *self == Scope::Full;
        }
        match self {
            Scope::Mod => ["queue", "mod", "alerts", "presets", "chat", "tts", "giveaway"].iter().any(|p| name == *p || name.starts_with(&format!("{p}."))),
            _ => true,
        }
    }
}

/// What a web patch's op writes to, for scope checks; `None` = never allowed for patches.
fn patch_target(op: &Op) -> Option<&str> {
    Some(match op {
        Op::Set { address, .. } | Op::Animate { address, .. } | Op::Trigger { address, .. } | Op::Release { address } => address,
        Op::Emit { ty, .. } => ty,
        Op::Action { name, .. } => name,
        Op::SceneGo { .. } => "scene.go",
        Op::SceneCut { .. } => "scene.cut",
        Op::SceneTake { .. } => "scene.take",
        Op::PresetFire { .. } => "preset.fire",
        Op::PresetRelease { .. } => "preset.release",
        Op::ModeSet { .. } => "mode.set",
        Op::SetBase { .. } | Op::Panic | Op::Clean | Op::Undo | Op::Redo | Op::Wait { .. } => return None,
    })
}

fn admin_only(op: &Op, target: &str) -> bool {
    let root = target.split('.').next().unwrap_or(target);
    op.is_secret()
        || ADMIN_NAMESPACES.contains(&root)
        || ADMIN_ACTIONS.contains(&target)
        || matches!(op, Op::Action { .. }) && ADMIN_ACTION_PREFIXES.iter().any(|prefix| target.starts_with(prefix))
}

/// `grant` matches `target` or one of its ancestors (`lights.*` covers `lights.cue` and
/// `lights.fixture.1.dimmer`; `source.cam1` covers `source.cam1.exposure`). A target that is
/// itself a pattern is covered only when its wildcards stay inside the granted subtree.
fn covers(grant: &str, target: &str) -> bool {
    target.match_indices('.').map(|(i, _)| &target[..i]).chain([target]).any(|prefix| address::matches(grant, prefix))
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
        let p = Scope::Patch("confetti".into(), Vec::new());
        assert!(p.allows(&Op::Set { address: "patch.confetti.count".into(), value: Value::Int(1) }));
        assert!(!p.allows(&Op::Set { address: "patch.other.count".into(), value: Value::Int(1) }));
        assert!(!p.allows(&Op::Set { address: "mixer.16r.main.fader".into(), value: Value::Int(1) }));
        assert!(!p.allows(&Op::Panic));
        assert!(Scope::Mod.allows(&Op::Action { name: "queue.skip".into(), args: Value::Null }));
        assert!(!Scope::Mod.allows(&Op::Action { name: "lights.cue".into(), args: Value::Null }));
        assert!(!Scope::Mod.allows(&Op::SceneGo { scene: "x".into() }));
        let act = |n: &str| Op::Action { name: n.into(), args: Value::Null };
        assert!(Scope::Mod.allows(&act("tts.skip")) && Scope::Mod.allows(&act("tts.clear")) && Scope::Mod.allows(&act("giveaway.draw")));
        assert!(!Scope::Mod.allows(&act("tts.say")), "mods may skip TTS, not speak through it");
        assert!(Scope::Mod.may_query("giveaway") && !Scope::Mod.may_query("sources"));
        assert!(!Scope::ReadOnly.allows(&Op::Clean));
    }

    fn set(a: &str) -> Op {
        Op::Set { address: a.into(), value: Value::Int(1) }
    }

    fn act(n: &str) -> Op {
        Op::Action { name: n.into(), args: Value::Null }
    }

    #[test]
    fn patch_own_namespace_is_a_whole_segment() {
        let p = Scope::Patch("confetti".into(), Vec::new());
        assert!(p.allows(&Op::Trigger { address: "patch.confetti".into(), payload: Value::Null }));
        assert!(p.allows(&Op::Emit { ty: "patch.confetti.clicked".into(), payload: Value::Null }));
        assert!(!p.allows(&set("patch.confetti_big.count")), "a longer id sharing the prefix is someone else");
        assert!(!p.allows(&act("lights.cue")) && !p.allows(&Op::SceneGo { scene: "duo".into() }));
        // a folder named like a loader action does not reach it
        assert!(!Scope::Patch("open".into(), Vec::new()).allows(&act("patch.open")));
    }

    #[test]
    fn patch_grants_cover_matching_subtrees() {
        let p = Scope::Patch("deck".into(), vec!["lights.*".into(), "source.cam1".into(), "scene.**".into()]);
        assert!(p.allows(&act("lights.cue")));
        assert!(p.allows(&Op::Animate { address: "lights.fixture.par1.dimmer".into(), to: Value::Float(0.5), ms: 200, ease: Default::default() }));
        assert!(p.allows(&Op::Release { address: "source.cam1.exposure".into() }));
        assert!(p.allows(&Op::SceneCut { scene: "duo".into(), transition: None }));
        assert!(p.allows(&set("lights.*")), "a pattern target inside the grant");
        assert!(!p.allows(&set("source.cam2.exposure")));
        assert!(!p.allows(&set("source.*.exposure")), "a pattern target reaching outside the grant");
        assert!(!p.allows(&set("lightsaber.on")));
        assert!(!p.allows(&act("mixer.mute")));
        assert!(!p.allows(&Op::PresetFire { name: "x".into(), payload: Value::Null }));
        assert!(!p.allows(&Op::SetBase { address: "lights.cue".into(), value: Value::Int(1) }), "grants never edit the project");
    }

    #[test]
    fn grants_never_reach_admin_ops() {
        // the manifest rejects `*`; the scope stays safe even if one got through
        let p = Scope::Patch("x".into(), vec!["*".into(), "**".into()]);
        assert!(p.allows(&act("lights.cue")) && p.allows(&Op::ModeSet { mode: "live".into() }));
        for name in [
            "api.token.rotate",
            "api.device.add",
            "secrets.set",
            "project.write",
            "patch.new",
            "patch.open",
            "patch.remove",
            "youtube.key.set",
            "twitch.token.set",
            "fx.chain.apply",
            "fx.chain.save",
            "fx.chain.set",
            "fx.slot.add",
            "fx.slot.remove",
            "fx.slot.move",
            "fx.slot.set",
            "fx.group.set",
        ] {
            assert!(!p.allows(&act(name)), "{name}");
        }
        // a patch folder named `remove` doesn't own `patch.remove`
        assert!(!Scope::Patch("remove".into(), vec![]).allows(&act("patch.remove")));
        assert!(!p.allows(&set("secrets.youtube")));
        for op in [Op::Panic, Op::Clean, Op::Undo, Op::Redo, Op::SetBase { address: "x.y".into(), value: Value::Null }] {
            assert!(!p.allows(&op), "{op:?}");
        }
    }

    #[test]
    fn remote_mod_queries_are_full_access_only() {
        // `remote_mod.request` would let a read, patch or mod token act as a moderator
        let granted = Scope::Patch("x".into(), vec!["remote_mod.*".into(), "**".into()]);
        for s in [Scope::ReadOnly, Scope::Patch("x".into(), Vec::new()), granted, Scope::Mod] {
            for q in ["remote_mod", "remote_mod.request", "remote_mod.status"] {
                assert!(!s.may_query(q), "{s:?} {q}");
            }
            assert!(!s.may_query("remote_mod.request.x"), "{s:?}");
        }
        assert!(Scope::Full.may_query("remote_mod") && Scope::Full.may_query("remote_mod.request"));
        assert!(Scope::ReadOnly.may_query("remote_modest"), "only the remote_mod namespace");
    }

    #[test]
    fn recorder_barriers_require_full_access() {
        for name in ["recording.finalize", "session.persist"] {
            for scope in [Scope::ReadOnly, Scope::Mod, Scope::Patch("x".into(), vec!["**".into()])] {
                assert!(!scope.may_query(name), "{scope:?} must not mutate recorder/session state");
            }
            assert!(Scope::Full.may_query(name));
        }
        assert!(Scope::ReadOnly.may_query("recording.status"));
        assert!(Scope::ReadOnly.may_query("recording.sources"));
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
