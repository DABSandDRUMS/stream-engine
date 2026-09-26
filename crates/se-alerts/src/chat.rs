//! The chat feed for overlays (§14.1 chat box): policy-filtered, display-safe messages
//! (`chat.message`), deletion sync (`chat.delete`, `chat.purge`, `chat.clear`), and the recent
//! backlog (`chat.recent` query) so a freshly loaded chat box starts filled.

use se_core::policy::{FilterCfg, display_fragment, filter_text};
use se_proto::{Event, Value};
use std::collections::VecDeque;

/// Twitch emote CDN (dark theme, 2x) for fragments that carry only an id.
pub fn twitch_emote_url(id: &str) -> String {
    format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/default/dark/2.0")
}

pub struct ChatFeed {
    recent: VecDeque<Value>,
    cap: usize,
}

fn s(v: &Value, k: &str) -> String {
    v.get_path(k)
        .map(|x| match x {
            Value::Str(s) => s.clone(),
            Value::Null => String::new(),
            o => o.to_string(),
        })
        .unwrap_or_default()
}

impl ChatFeed {
    pub fn new(cap: usize) -> ChatFeed {
        ChatFeed { recent: VecDeque::new(), cap: cap.max(1) }
    }

    /// Filter and normalize a chat event. `Err(reason)` = withheld by the content filter;
    /// `Ok(None)` = nothing to show.
    pub fn on_chat(&mut self, e: &Event, cfg: &FilterCfg, now_unix_ms: i64) -> Result<Option<Value>, String> {
        let p = &e.payload;
        if p.get_path("vetoed").is_some_and(Value::truthy) {
            return Ok(None);
        }
        let text = s(p, "message");
        if text.trim().is_empty() {
            return Ok(None);
        }
        let clean = filter_text(&text, cfg).map_err(|r| r.to_string())?;
        let mut frags = Vec::new();
        let mut budget = cfg.max_len.max(1);
        match p.get_path("fragments").and_then(Value::as_list).filter(|l| !l.is_empty()) {
            Some(list) => {
                for f in list {
                    if budget == 0 {
                        frags.push(Value::map().with("type", "text").with("text", "…"));
                        break;
                    }
                    let ty = s(f, "type");
                    let raw = s(f, "text");
                    match ty.as_str() {
                        "emote" => {
                            let em = f.get_path("emote").cloned().unwrap_or_default();
                            let id = s(&em, "id");
                            let url = match s(&em, "url") {
                                u if !u.is_empty() => u,
                                _ if !id.is_empty() => twitch_emote_url(&id),
                                _ => String::new(),
                            };
                            if url.is_empty() {
                                let t = display_fragment(&raw, budget, cfg.max_marks);
                                budget = budget.saturating_sub(t.chars().count());
                                frags.push(Value::map().with("type", "text").with("text", t));
                                continue;
                            }
                            budget = budget.saturating_sub(1);
                            frags.push(
                                Value::map()
                                    .with("type", "emote")
                                    .with("text", display_fragment(&raw, 100, 0))
                                    .with("id", id)
                                    .with("url", url)
                                    .with("provider", em.get_path("provider").cloned().unwrap_or_else(|| "twitch".into()))
                                    .with("animated", em.get_path("animated").is_some_and(Value::truthy))
                                    .with("zero_width", em.get_path("zero_width").is_some_and(Value::truthy)),
                            );
                        }
                        other => {
                            let t = display_fragment(&raw, budget, cfg.max_marks);
                            budget = budget.saturating_sub(t.chars().count());
                            let kind = if matches!(other, "mention" | "cheermote") { other } else { "text" };
                            let mut v = Value::map().with("type", kind).with("text", t);
                            if kind == "cheermote"
                                && let Some(c) = f.get_path("cheermote")
                            {
                                v = v.with("cheermote", c.clone());
                            }
                            frags.push(v);
                        }
                    }
                }
            }
            None => frags.push(Value::map().with("type", "text").with("text", clean.clone())),
        }
        let actor = e.actor.as_ref();
        let msg = Value::map()
            .with("id", s(p, "message_id"))
            .with("user", actor.map(|a| a.name.clone()).unwrap_or_else(|| s(p, "user")))
            .with("user_id", actor.map(|a| a.id.clone()).unwrap_or_else(|| s(p, "user_id")))
            .with("login", s(p, "login"))
            .with("color", s(p, "color"))
            .with("badges", p.get_path("badges").cloned().unwrap_or_else(|| Value::List(Vec::new())))
            .with("roles", Value::List(actor.map(|a| a.roles.iter().map(|r| Value::Str(se_core::policy::role_name(*r))).collect()).unwrap_or_default()))
            .with("fragments", Value::List(frags))
            .with("text", clean)
            .with("reply_to", p.get_path("reply_to").cloned().unwrap_or_default())
            .with("bits", p.get_path("bits").cloned().unwrap_or_default())
            .with("platform", actor.map(|a| a.platform.clone()).unwrap_or_else(|| e.ty.split('.').next().unwrap_or("").to_string()))
            .with("ts", now_unix_ms);
        self.recent.push_back(msg.clone());
        while self.recent.len() > self.cap {
            self.recent.pop_front();
        }
        Ok(Some(msg))
    }

    /// Remove one message. Returns whether it was in the backlog.
    pub fn delete(&mut self, message_id: &str) -> bool {
        let before = self.recent.len();
        self.recent.retain(|m| s(m, "id") != message_id);
        before != self.recent.len()
    }

    /// Remove everything from a user (ban/timeout). Returns how many.
    pub fn purge(&mut self, user_id: &str, user: &str) -> usize {
        let before = self.recent.len();
        self.recent.retain(|m| !((!user_id.is_empty() && s(m, "user_id") == user_id) || (!user.is_empty() && s(m, "user").eq_ignore_ascii_case(user))));
        before - self.recent.len()
    }

    pub fn clear(&mut self) {
        self.recent.clear();
    }

    pub fn recent(&self) -> Value {
        Value::List(self.recent.iter().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_proto::{Actor, Origin, Role};

    fn chat(user: &str, id: &str, msg: &str, frags: Option<Value>) -> Event {
        let mut p = Value::map().with("message", msg).with("message_id", id).with("user", user).with("color", "#ff0000");
        if let Some(f) = frags {
            p = p.with("fragments", f);
        }
        Event::new("twitch.chat", Origin::Twitch, p).with_actor(Actor {
            platform: "twitch".into(),
            id: format!("id-{user}"),
            name: user.into(),
            roles: vec![Role::Sub],
        })
    }

    fn cfg() -> FilterCfg {
        FilterCfg { blocklist: vec!["badword".into()], ..Default::default() }
    }

    #[test]
    fn filters_and_normalizes() {
        let mut f = ChatFeed::new(10);
        let m = f.on_chat(&chat("ana", "m1", "hello   chat", None), &cfg(), 1).unwrap().unwrap();
        assert_eq!(m.get_path("text"), Some(&Value::Str("hello chat".into())));
        assert_eq!(m.get_path("fragments.0.text"), Some(&Value::Str("hello chat".into())));
        assert_eq!(m.get_path("roles.0"), Some(&Value::Str("sub".into())));
        assert!(f.on_chat(&chat("troll", "m2", "you b a d w o r d", None), &cfg(), 2).is_err());
        let zalgo = "h\u{0301}\u{0302}\u{0303}\u{0304}\u{0305}i";
        let m = f.on_chat(&chat("z", "m3", zalgo, None), &cfg(), 3).unwrap().unwrap();
        assert!(m.get_path("text").unwrap().as_str().unwrap().chars().count() <= 4);
        let mut vetoed = chat("v", "m4", "", None);
        vetoed.payload = vetoed.payload.with("vetoed", true);
        assert_eq!(f.on_chat(&vetoed, &cfg(), 4).unwrap(), None);
        assert_eq!(f.recent().as_list().unwrap().len(), 2);
    }

    #[test]
    fn fragments_keep_emotes_and_spacing() {
        let frags = Value::List(vec![
            Value::map().with("type", "text").with("text", "nice "),
            Value::map().with("type", "emote").with("text", "Kappa").with("emote", Value::map().with("id", "25")),
            Value::map().with("type", "text").with("text", " and "),
            Value::map().with("type", "emote").with("text", "catJAM").with(
                "emote",
                Value::map().with("id", "7tv1").with("url", "https://cdn.7tv.app/emote/x/2x.webp").with("provider", "7tv").with("animated", true),
            ),
        ]);
        let mut f = ChatFeed::new(10);
        let m = f.on_chat(&chat("ana", "m1", "nice Kappa and catJAM", Some(frags)), &cfg(), 1).unwrap().unwrap();
        let fr = m.get_path("fragments").unwrap().as_list().unwrap();
        assert_eq!(fr.len(), 4);
        assert_eq!(fr[0].get_path("text"), Some(&Value::Str("nice ".into())));
        assert_eq!(fr[1].get_path("url"), Some(&Value::Str(twitch_emote_url("25"))));
        assert_eq!(fr[2].get_path("text"), Some(&Value::Str(" and ".into())));
        assert_eq!(fr[3].get_path("provider"), Some(&Value::Str("7tv".into())));
        assert_eq!(fr[3].get_path("animated"), Some(&Value::Bool(true)));
    }

    #[test]
    fn deletion_sync_and_backlog_cap() {
        let mut f = ChatFeed::new(3);
        for (u, id) in [("a", "1"), ("b", "2"), ("a", "3"), ("c", "4")] {
            f.on_chat(&chat(u, id, "hi", None), &cfg(), 0).unwrap();
        }
        assert_eq!(f.recent().as_list().unwrap().len(), 3, "capped");
        assert!(f.delete("2"));
        assert!(!f.delete("2"));
        assert_eq!(f.purge("id-a", ""), 1);
        let ids: Vec<String> = f.recent().as_list().unwrap().iter().map(|m| s(m, "id")).collect();
        assert_eq!(ids, vec!["4"]);
        f.clear();
        assert!(f.recent().as_list().unwrap().is_empty());
    }
}
