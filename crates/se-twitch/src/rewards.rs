//! Channel point rewards we own (§11): created/updated on Twitch from `rewards/*.toml`. Only
//! rewards created by our Client ID can have redemptions fulfilled or refunded, so the app owns
//! them; the file stem ↔ Twitch reward id mapping lives in the runtime DB.

use crate::auth::Account;
use crate::helix::{Helix, Method, first};
use se_core::policy::RewardDef;
use se_proto::Value;
use se_store::Db;
use serde_json::{Value as J, json};
use std::collections::HashMap;

const NS: &str = "twitch.rewards";

/// Helix body for a reward (create or update).
pub fn body(r: &RewardDef) -> J {
    let mut b = json!({
        "title": r.title,
        "cost": r.cost,
        "prompt": r.prompt,
        "is_enabled": r.enabled,
        "is_paused": r.paused,
        "is_user_input_required": r.input_required,
        "is_max_per_stream_enabled": r.max_per_stream.is_some(),
        "max_per_stream": r.max_per_stream.unwrap_or(0),
        "is_max_per_user_per_stream_enabled": r.max_per_user_per_stream.is_some(),
        "max_per_user_per_stream": r.max_per_user_per_stream.unwrap_or(0),
        "is_global_cooldown_enabled": r.cooldown_ms.is_some(),
        "global_cooldown_seconds": r.cooldown_ms.map(|ms| ms.div_ceil(1000)).unwrap_or(0),
        // redemptions must stay UNFULFILLED so we can fulfill or refund them
        "should_redemptions_skip_request_queue": false,
    });
    if let Some(c) = &r.color {
        b["background_color"] = json!(c.to_uppercase());
    }
    b
}

/// Does the reward on Twitch (`GET` shape) already match our body?
pub fn matches(remote: &J, want: &J) -> bool {
    let g = |v: &J, path: &str| v.pointer(path).cloned().unwrap_or(J::Null);
    let norm = |v: J| match v {
        J::String(s) => J::String(s.to_uppercase()),
        o => o,
    };
    [
        ("/title", "/title"),
        ("/cost", "/cost"),
        ("/prompt", "/prompt"),
        ("/is_enabled", "/is_enabled"),
        ("/is_paused", "/is_paused"),
        ("/is_user_input_required", "/is_user_input_required"),
        ("/max_per_stream_setting/is_enabled", "/is_max_per_stream_enabled"),
        ("/max_per_user_per_stream_setting/is_enabled", "/is_max_per_user_per_stream_enabled"),
        ("/global_cooldown_setting/is_enabled", "/is_global_cooldown_enabled"),
        ("/should_redemptions_skip_request_queue", "/should_redemptions_skip_request_queue"),
    ]
    .iter()
    .all(|(r, w)| g(remote, r) == g(want, w))
        && (!g(want, "/is_max_per_stream_enabled").as_bool().unwrap_or(false)
            || g(remote, "/max_per_stream_setting/max_per_stream") == g(want, "/max_per_stream"))
        && (!g(want, "/is_max_per_user_per_stream_enabled").as_bool().unwrap_or(false)
            || g(remote, "/max_per_user_per_stream_setting/max_per_user_per_stream") == g(want, "/max_per_user_per_stream"))
        && (!g(want, "/is_global_cooldown_enabled").as_bool().unwrap_or(false)
            || g(remote, "/global_cooldown_setting/global_cooldown_seconds") == g(want, "/global_cooldown_seconds"))
        && (want.get("background_color").is_none() || norm(g(remote, "/background_color")) == norm(g(want, "/background_color")))
}

/// One reward after a sync.
#[derive(Clone, Debug, PartialEq)]
pub struct Synced {
    pub key: String,
    pub title: String,
    pub id: String,
    pub cost: i64,
    pub enabled: bool,
    /// `created | updated | unchanged | disabled | error: …`
    pub status: String,
}

impl Synced {
    pub fn value(&self) -> Value {
        Value::map()
            .with("key", self.key.clone())
            .with("title", self.title.clone())
            .with("id", self.id.clone())
            .with("cost", self.cost)
            .with("enabled", self.enabled)
            .with("status", self.status.clone())
    }
}

/// Twitch reward id → (file key, managed by us).
#[derive(Clone, Debug, Default)]
pub struct RewardMap {
    pub by_id: HashMap<String, String>,
    /// Every reward our Client ID can manage (includes ones without a file).
    pub manageable: std::collections::HashSet<String>,
}

pub fn load_map(db: &Db) -> HashMap<String, String> {
    db.kv_list(NS).unwrap_or_default().into_iter().filter_map(|(k, v)| v.as_str().map(|id| (k, id.to_string()))).collect()
}

/// Create/update every reward from the project; disable ones whose file was removed.
pub async fn sync(helix: &Helix, db: &Db, broadcaster: &str, defs: &[RewardDef]) -> Result<(Vec<Synced>, RewardMap), String> {
    let q = [("broadcaster_id", broadcaster.to_string()), ("only_manageable_rewards", "true".to_string())];
    let remote = helix.get(Account::Broadcaster, "/channel_points/custom_rewards", &q).await.map_err(|e| e.to_string())?;
    let remote: Vec<J> = remote.get("data").and_then(J::as_array).cloned().unwrap_or_default();
    let by_remote_id: HashMap<String, &J> = remote.iter().filter_map(|r| Some((r.get("id")?.as_str()?.to_string(), r))).collect();
    let mut mapping = load_map(db);
    let mut out = Vec::new();
    let mut map = RewardMap { manageable: by_remote_id.keys().cloned().collect(), ..Default::default() };
    for def in defs {
        let want = body(def);
        let existing = mapping
            .get(&def.key)
            .and_then(|id| by_remote_id.get(id).copied())
            .or_else(|| remote.iter().find(|r| r.get("title").and_then(J::as_str).is_some_and(|t| t.eq_ignore_ascii_case(&def.title))));
        let res = match existing {
            Some(r) => {
                let id = r.get("id").and_then(J::as_str).unwrap_or_default().to_string();
                if matches(r, &want) {
                    Ok((id, "unchanged"))
                } else {
                    let q = [("broadcaster_id", broadcaster.to_string()), ("id", id.clone())];
                    helix.call(Account::Broadcaster, Method::Patch, "/channel_points/custom_rewards", &q, Some(&want)).await.map(|_| (id, "updated"))
                }
            }
            None => {
                let q = [("broadcaster_id", broadcaster.to_string())];
                helix
                    .call(Account::Broadcaster, Method::Post, "/channel_points/custom_rewards", &q, Some(&want))
                    .await
                    .map(|v| (first(&v).and_then(|r| r.get("id")).and_then(J::as_str).unwrap_or_default().to_string(), "created"))
            }
        };
        match res {
            Ok((id, status)) => {
                if mapping.get(&def.key) != Some(&id) {
                    let _ = db.kv_set(NS, &def.key, &Value::Str(id.clone()));
                    mapping.insert(def.key.clone(), id.clone());
                }
                map.by_id.insert(id.clone(), def.key.clone());
                map.manageable.insert(id.clone());
                out.push(Synced { key: def.key.clone(), title: def.title.clone(), id, cost: def.cost, enabled: def.enabled, status: status.into() });
            }
            Err(e) => {
                let msg = if e.message.contains("DUPLICATE") || e.message.to_lowercase().contains("duplicate") {
                    format!("a reward titled `{}` exists but was not created by this app; rename one of them", def.title)
                } else {
                    e.to_string()
                };
                out.push(Synced {
                    key: def.key.clone(),
                    title: def.title.clone(),
                    id: String::new(),
                    cost: def.cost,
                    enabled: def.enabled,
                    status: format!("error: {msg}"),
                });
            }
        }
    }
    // files removed since the last sync: disable their rewards (reversible, keeps history)
    let gone: Vec<(String, String)> = mapping.iter().filter(|(k, _)| !defs.iter().any(|d| &d.key == *k)).map(|(k, v)| (k.clone(), v.clone())).collect();
    for (key, id) in gone {
        if let Some(r) = by_remote_id.get(&id)
            && r.get("is_enabled").and_then(J::as_bool).unwrap_or(false)
        {
            let q = [("broadcaster_id", broadcaster.to_string()), ("id", id.clone())];
            let status =
                match helix.call(Account::Broadcaster, Method::Patch, "/channel_points/custom_rewards", &q, Some(&json!({ "is_enabled": false }))).await {
                    Ok(_) => "disabled".to_string(),
                    Err(e) => format!("error: {e}"),
                };
            out.push(Synced {
                key: key.clone(),
                title: r.get("title").and_then(J::as_str).unwrap_or_default().into(),
                id: id.clone(),
                cost: r.get("cost").and_then(J::as_i64).unwrap_or(0),
                enabled: false,
                status,
            });
        }
        let _ = db.kv_del(NS, &key);
    }
    Ok((out, map))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(src: &str) -> RewardDef {
        RewardDef::parse("hype", &toml::from_str(src).unwrap()).unwrap()
    }

    #[test]
    fn body_and_remote_comparison() {
        let d = def("title = \"HYPE\"\ncost = 2000\ncooldown = \"5m\"\nmax_per_user_per_stream = 3\nfires = \"preset.hype\"\ncolor = \"#e82424\"");
        let b = body(&d);
        assert_eq!(b["global_cooldown_seconds"], 300);
        assert_eq!(b["is_max_per_user_per_stream_enabled"], true);
        assert_eq!(b["should_redemptions_skip_request_queue"], false);
        let remote = json!({
            "id": "r1", "title": "HYPE", "cost": 2000, "prompt": "", "is_enabled": true, "is_paused": false, "is_user_input_required": false,
            "background_color": "#E82424",
            "max_per_stream_setting": {"is_enabled": false, "max_per_stream": 0},
            "max_per_user_per_stream_setting": {"is_enabled": true, "max_per_user_per_stream": 3},
            "global_cooldown_setting": {"is_enabled": true, "global_cooldown_seconds": 300},
            "should_redemptions_skip_request_queue": false
        });
        assert!(matches(&remote, &b));
        let mut changed = remote.clone();
        changed["global_cooldown_setting"]["global_cooldown_seconds"] = json!(60);
        assert!(!matches(&changed, &b));
        let mut skip = remote;
        skip["should_redemptions_skip_request_queue"] = json!(true);
        assert!(!matches(&skip, &b), "a reward that skips the queue can't be refunded: must be fixed");
    }
}
