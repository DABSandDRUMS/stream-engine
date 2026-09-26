//! Users/roles cache (§3.3 runtime DB, §12.1 roles): the roles last seen in each viewer's chat
//! badges plus the channel's moderators and VIPs from Helix (`/moderation/moderators`,
//! `/channels/vips`, refreshed periodically), kept in the runtime DB. Events that carry no
//! badges (cheers, redemptions, subs, raids, …) take their roles from here, so role gates hold
//! for them too.
//!
//! Badges and the lists disagree for a while after a change (a new VIP who hasn't chatted yet,
//! a removed mod still wearing yesterday's badge): for moderator and VIP, whichever was seen
//! last wins.

use crate::auth::Account;
use crate::helix::Helix;
use se_proto::Role;
use se_store::Db;
use serde_json::Value as J;
use std::collections::{HashMap, HashSet};

/// Viewers not seen in chat for this long are forgotten at startup (listed ones are kept).
const FORGET_S: i64 = 90 * 86_400;
/// In-memory cap; beyond it viewers not seen today (and not listed) are dropped.
const MAX_USERS: usize = 50_000;
/// Most entries read per Helix list (100 per page).
const LIST_MAX: usize = 5_000;
const KV_NS: &str = "twitch";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum List {
    Mods,
    Vips,
}

impl List {
    pub const ALL: [List; 2] = [List::Mods, List::Vips];

    fn path(self) -> &'static str {
        match self {
            List::Mods => "/moderation/moderators",
            List::Vips => "/channels/vips",
        }
    }

    fn role(self) -> Role {
        match self {
            List::Mods => Role::Mod,
            List::Vips => Role::Vip,
        }
    }

    fn kv_key(self) -> &'static str {
        match self {
            List::Mods => "roles.mods_at",
            List::Vips => "roles.vips_at",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            List::Mods => "moderators",
            List::Vips => "VIPs",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct User {
    pub login: String,
    /// Roles in the user's last chat badges (sub, VIP, mod, owner).
    pub badges: Vec<Role>,
    /// When those badges were seen (unix s; 0 = never chatted).
    pub seen_at: i64,
    /// On the moderator / VIP list at its last fetch.
    pub is_mod: bool,
    pub is_vip: bool,
}

impl User {
    fn listed(&self, l: List) -> bool {
        match l {
            List::Mods => self.is_mod,
            List::Vips => self.is_vip,
        }
    }

    fn set_listed(&mut self, l: List, on: bool) {
        match l {
            List::Mods => self.is_mod = on,
            List::Vips => self.is_vip = on,
        }
    }
}

#[derive(Debug, Default)]
pub struct Users {
    users: HashMap<String, User>,
    /// When the moderator and VIP lists were last fetched (unix s; 0 = never).
    mods_at: i64,
    vips_at: i64,
}

impl Users {
    fn list_at(&self, l: List) -> i64 {
        match l {
            List::Mods => self.mods_at,
            List::Vips => self.vips_at,
        }
    }

    pub fn len(&self) -> usize {
        self.users.len()
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    /// Roles for a user id, `None` when we know nothing about them.
    pub fn roles(&self, id: &str) -> Option<Vec<Role>> {
        let u = self.users.get(id)?;
        let mut roles: Vec<Role> = u.badges.iter().copied().filter(|r| !matches!(r, Role::Mod | Role::Vip)).collect();
        for l in List::ALL {
            let on = if u.seen_at > self.list_at(l) { u.badges.contains(&l.role()) } else { u.listed(l) };
            if on {
                roles.push(l.role());
            }
        }
        roles.sort();
        Some(roles)
    }

    /// Chat badges seen now; returns the row to persist when it changed (or disagrees with
    /// the lists, so "seen last wins" survives a restart).
    pub fn saw(&mut self, id: &str, login: &str, roles: &[Role], now: i64) -> Option<(String, User)> {
        if self.users.len() >= MAX_USERS && !self.users.contains_key(id) {
            self.users.retain(|_, u| u.is_mod || u.is_vip || now - u.seen_at < 86_400);
        }
        let mut badges: Vec<Role> = roles.iter().copied().filter(|r| matches!(r, Role::Sub | Role::Vip | Role::Mod | Role::Owner)).collect();
        badges.sort();
        badges.dedup();
        let u = self.users.entry(id.to_string()).or_default();
        let disagrees = List::ALL.iter().any(|l| u.listed(*l) != badges.contains(&l.role()));
        let changed = u.badges != badges || u.seen_at == 0 || (!login.is_empty() && u.login != login) || disagrees;
        u.badges = badges;
        u.seen_at = now;
        if !login.is_empty() {
            u.login = login.to_string();
        }
        changed.then(|| (id.to_string(), u.clone()))
    }

    /// A freshly fetched list: members `(id, login)`; returns the rows that changed.
    pub fn apply_list(&mut self, l: List, members: &[(String, String)], now: i64) -> Vec<(String, User)> {
        let ids: HashSet<&str> = members.iter().map(|(id, _)| id.as_str()).collect();
        let mut changed = Vec::new();
        for (id, login) in members {
            let u = self.users.entry(id.clone()).or_default();
            let named = u.login.is_empty() && !login.is_empty();
            if named {
                u.login = login.clone();
            }
            if !u.listed(l) || named {
                u.set_listed(l, true);
                changed.push((id.clone(), u.clone()));
            }
        }
        for (id, u) in self.users.iter_mut() {
            if u.listed(l) && !ids.contains(id.as_str()) {
                u.set_listed(l, false);
                changed.push((id.clone(), u.clone()));
            }
        }
        match l {
            List::Mods => self.mods_at = now,
            List::Vips => self.vips_at = now,
        }
        changed
    }

    /// `(id, login)` of a list's members, by login.
    pub fn listed(&self, l: List) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self.users.iter().filter(|(_, u)| u.listed(l)).map(|(id, u)| (id.clone(), u.login.clone())).collect();
        v.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    pub fn synced_at(&self, l: List) -> i64 {
        self.list_at(l)
    }
}

pub fn migrate(db: &Db) -> anyhow::Result<()> {
    db.migrate(
        "twitch_users_v1",
        "CREATE TABLE IF NOT EXISTS twitch_users (
           user_id TEXT PRIMARY KEY,
           login TEXT NOT NULL DEFAULT '',
           badges TEXT NOT NULL DEFAULT '',
           seen_at INTEGER NOT NULL DEFAULT 0,
           is_mod INTEGER NOT NULL DEFAULT 0,
           is_vip INTEGER NOT NULL DEFAULT 0
         );",
    )
}

fn badges_text(r: &[Role]) -> String {
    r.iter().map(|r| format!("{r:?}").to_lowercase()).collect::<Vec<_>>().join(",")
}

/// Load the cache (forgetting viewers not seen for [`FORGET_S`] who aren't listed).
pub fn load(db: &Db, now: i64) -> Users {
    let _ = db.with(|c| c.execute("DELETE FROM twitch_users WHERE seen_at < ?1 AND is_mod = 0 AND is_vip = 0", [now - FORGET_S]));
    let users = db
        .with(|c| {
            let mut st = c.prepare("SELECT user_id, login, badges, seen_at, is_mod, is_vip FROM twitch_users")?;
            let rows = st.query_map([], |r| {
                let badges: String = r.get(2)?;
                Ok((
                    r.get::<_, String>(0)?,
                    User {
                        login: r.get(1)?,
                        badges: badges.split(',').filter_map(Role::parse).collect(),
                        seen_at: r.get(3)?,
                        is_mod: r.get(4)?,
                        is_vip: r.get(5)?,
                    },
                ))
            })?;
            rows.collect::<Result<HashMap<_, _>, _>>()
        })
        .unwrap_or_default();
    let at = |l: List| db.kv_get(KV_NS, l.kv_key()).ok().flatten().and_then(|v| v.as_i64()).unwrap_or(0);
    Users { users, mods_at: at(List::Mods), vips_at: at(List::Vips) }
}

pub fn store(db: &Db, rows: &[(String, User)]) {
    if rows.is_empty() {
        return;
    }
    let _ = db.with(|c| {
        let tx = c.unchecked_transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO twitch_users (user_id, login, badges, seen_at, is_mod, is_vip) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(user_id) DO UPDATE SET login = excluded.login, badges = excluded.badges, seen_at = excluded.seen_at,
                   is_mod = excluded.is_mod, is_vip = excluded.is_vip",
            )?;
            for (id, u) in rows {
                st.execute((id, &u.login, badges_text(&u.badges), u.seen_at, u.is_mod, u.is_vip))?;
            }
        }
        tx.commit()
    });
}

pub fn store_synced(db: &Db, l: List, at: i64) {
    let _ = db.kv_set(KV_NS, l.kv_key(), &se_proto::Value::Int(at));
}

/// Every member of a list: `(user id, login)`.
pub async fn fetch(helix: &Helix, broadcaster: &str, l: List) -> Result<Vec<(String, String)>, String> {
    let all = helix.get_all(Account::Broadcaster, l.path(), &[("broadcaster_id", broadcaster.to_string())], LIST_MAX).await.map_err(|e| e.to_string())?;
    Ok(all.iter().filter_map(|u| Some((u.get("user_id")?.as_str()?.to_string(), u.get("user_login").and_then(J::as_str).unwrap_or("").to_string()))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[(String, User)]) -> Vec<&str> {
        let mut v: Vec<&str> = v.iter().map(|(id, _)| id.as_str()).collect();
        v.sort();
        v
    }

    #[test]
    fn lists_and_badges_whichever_was_seen_last_wins() {
        let mut u = Users::default();
        // a VIP who hasn't chatted: only the list knows
        u.apply_list(List::Vips, &[("1".into(), "ann".into())], 100);
        u.apply_list(List::Mods, &[("2".into(), "bob".into())], 100);
        assert_eq!(u.roles("1"), Some(vec![Role::Vip]));
        assert_eq!(u.roles("2"), Some(vec![Role::Mod]));
        assert_eq!(u.roles("3"), None);
        // bob chats after being unmodded: no mod badge, and that's newer than the list
        u.saw("2", "bob", &[Role::Sub], 150);
        assert_eq!(u.roles("2"), Some(vec![Role::Sub]));
        // cat chats with a VIP badge before the next fetch, which no longer lists her
        u.saw("3", "cat", &[Role::Follower, Role::Vip], 160);
        assert_eq!(u.roles("3"), Some(vec![Role::Vip]), "badge newer than the list");
        let changed = u.apply_list(List::Vips, &[("1".into(), "ann".into())], 200);
        assert!(changed.is_empty(), "nothing on the list changed: {:?}", ids(&changed));
        assert_eq!(u.roles("3"), Some(vec![]), "list newer than the badge");
        // ann leaves the VIP list
        let changed = u.apply_list(List::Vips, &[], 300);
        assert_eq!(ids(&changed), ["1"]);
        assert_eq!(u.roles("1"), Some(vec![]));
        assert_eq!(u.listed(List::Mods), vec![("2".to_string(), "bob".to_string())]);
    }

    #[test]
    fn badge_rows_persist_only_on_change_and_survive_a_restart() {
        let db = Db::memory().unwrap();
        migrate(&db).unwrap();
        let mut u = Users::default();
        let row = u.saw("7", "dee", &[Role::Follower, Role::Sub], 1_000).expect("new viewer");
        assert_eq!(row.1.badges, vec![Role::Sub], "follower comes from the follow cache, not badges");
        store(&db, &[row]);
        assert!(u.saw("7", "dee", &[Role::Sub], 1_010).is_none(), "same badges: no write");
        let listed = u.apply_list(List::Mods, &[("8".into(), "eve".into())], 1_020);
        store(&db, &listed);
        store_synced(&db, List::Mods, 1_020);
        let back = load(&db, 1_030);
        assert_eq!(back.roles("7"), Some(vec![Role::Sub]));
        assert_eq!(back.roles("8"), Some(vec![Role::Mod]));
        assert_eq!(back.synced_at(List::Mods), 1_020);
        // long-gone viewers are forgotten at startup, listed ones kept
        let later = load(&db, 1_000 + FORGET_S + 1);
        assert_eq!((later.roles("7"), later.roles("8")), (None, Some(vec![Role::Mod])));
    }
}
