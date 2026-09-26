//! EventSub notifications → the engine's normalized events (the contract in `se_core::sim`).
//!
//! Pure except for the small [`Normalizer`] state (gift-bomb attribution for the `eventsub`
//! sub source); everything that needs caches (badges, emotes, follow ages, owned rewards) comes
//! through [`Lookup`].

use crate::time::parse_rfc3339;
use se_proto::{Actor, Event, Origin, Role, Value};
use serde_json::Value as J;
use std::collections::VecDeque;

/// An emote resolved for overlays.
#[derive(Clone, Debug, PartialEq)]
pub struct Emote {
    pub id: String,
    pub url: String,
    pub provider: String,
    pub animated: bool,
    pub zero_width: bool,
}

impl Emote {
    pub fn value(&self) -> Value {
        Value::map()
            .with("id", self.id.clone())
            .with("url", self.url.clone())
            .with("provider", self.provider.clone())
            .with("animated", self.animated)
            .with("zero_width", self.zero_width)
    }
}

/// Twitch CDN image for an emote id (`animated` when Twitch offers it).
pub fn twitch_emote_url(id: &str, animated: bool) -> String {
    format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/{}/dark/3.0", if animated { "animated" } else { "static" })
}

/// Caches the normalizer consults.
pub trait Lookup {
    fn broadcaster_id(&self) -> &str;
    /// Our bot account (its own messages are marked `bot: true`).
    fn bot_user_id(&self) -> Option<&str>;
    /// Seconds since the user followed; `None` when not following or unknown.
    fn follow_age_s(&self, user_id: &str) -> Option<i64>;
    /// Follow age needed for the follower role (`[policy] follower_min_age`).
    fn follower_min_age_s(&self) -> i64;
    /// Roles last seen in the user's chat badges (for events without badges: cheers, redeems).
    fn cached_roles(&self, user_id: &str) -> Option<Vec<Role>>;
    fn badge_url(&self, set_id: &str, id: &str) -> Option<String>;
    /// Third-party (7TV/BTTV/FFZ) emote by its code.
    fn third_party(&self, code: &str) -> Option<Emote>;
    /// `(reward key from rewards/*.toml, created by our client id)` for a Twitch reward id.
    fn reward(&self, reward_id: &str) -> (Option<String>, bool);
}

fn js(v: &J, k: &str) -> String {
    match v.get(k) {
        Some(J::String(s)) => s.clone(),
        Some(J::Number(n)) => n.to_string(),
        Some(J::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn ji(v: &J, k: &str) -> i64 {
    match v.get(k) {
        Some(J::Number(n)) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Some(J::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn jb(v: &J, k: &str) -> bool {
    match v.get(k) {
        Some(J::Bool(b)) => *b,
        Some(J::String(s)) => s == "true",
        _ => false,
    }
}

/// Unix seconds of an RFC 3339 field (0 when absent).
fn jt(v: &J, k: &str) -> i64 {
    v.get(k).and_then(J::as_str).and_then(parse_rfc3339).map(|ms| ms / 1000).unwrap_or(0)
}

/// `"1000" | "2000" | "3000" | "Prime"` → 1, 2, 3.
pub fn tier(s: &str) -> i64 {
    match s {
        "2000" => 2,
        "3000" => 3,
        _ => 1,
    }
}

/// Tier of a chat notification block (`sub_tier`; some payloads say `sub_plan`).
fn sub_tier(block: &J) -> i64 {
    let t = js(block, "sub_tier");
    tier(if t.is_empty() { js(block, "sub_plan") } else { t }.as_str())
}

/// Roles and subscriber months from chat badges.
pub fn roles_from_badges(badges: Option<&J>) -> (Vec<Role>, i64) {
    let mut roles = Vec::new();
    let mut months = 0;
    for b in badges.and_then(J::as_array).map(Vec::as_slice).unwrap_or(&[]) {
        let set = b.get("set_id").and_then(J::as_str).unwrap_or("");
        let role = match set {
            "broadcaster" => Some(Role::Owner),
            "moderator" | "lead_moderator" => Some(Role::Mod),
            "vip" => Some(Role::Vip),
            "subscriber" | "founder" => {
                let info = js(b, "info");
                months = months.max(info.parse().unwrap_or(0));
                Some(Role::Sub)
            }
            _ => None,
        };
        if let Some(r) = role
            && !roles.contains(&r)
        {
            roles.push(r);
        }
    }
    (roles, months)
}

fn with_follower(mut roles: Vec<Role>, user_id: &str, lk: &dyn Lookup) -> Vec<Role> {
    if !roles.contains(&Role::Follower) && lk.follow_age_s(user_id).is_some_and(|a| a >= lk.follower_min_age_s()) {
        roles.push(Role::Follower);
    }
    roles.sort();
    roles
}

fn actor(id: &str, name: &str, roles: Vec<Role>) -> Actor {
    Actor { platform: "twitch".into(), id: id.into(), name: name.into(), roles }
}

/// Anonymous gifters/cheerers still get an actor so the policy screens their text.
fn anonymous() -> Actor {
    actor("anonymous", "Anonymous", Vec::new())
}

/// A viewer from `<prefix>user_id/_login/_name` fields, roles from the role cache.
fn viewer(ev: &J, prefix: &str, lk: &dyn Lookup) -> Option<Actor> {
    let id = js(ev, &format!("{prefix}user_id"));
    if id.is_empty() {
        return None;
    }
    let mut name = js(ev, &format!("{prefix}user_name"));
    if name.is_empty() {
        name = js(ev, &format!("{prefix}user_login"));
    }
    let roles = if id == lk.broadcaster_id() { vec![Role::Owner] } else { lk.cached_roles(&id).unwrap_or_default() };
    Some(actor(&id, &name, with_follower(roles, &id, lk)))
}

fn badges_value(badges: Option<&J>, lk: &dyn Lookup) -> Value {
    Value::List(
        badges
            .and_then(J::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|b| {
                let (set, id) = (js(b, "set_id"), js(b, "id"));
                Value::map()
                    .with("url", lk.badge_url(&set, &id).map(Value::Str).unwrap_or_default())
                    .with("set_id", set)
                    .with("id", id)
                    .with("info", js(b, "info"))
            })
            .collect(),
    )
}

/// Split a text fragment on whitespace so third-party emote codes become emote fragments.
fn split_third_party(text: &str, lk: &dyn Lookup, out: &mut Vec<Value>) {
    let mut buf = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let ws = rest.find(|c: char| !c.is_whitespace()).unwrap_or(rest.len());
        buf.push_str(&rest[..ws]);
        rest = &rest[ws..];
        if rest.is_empty() {
            break;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..end];
        match lk.third_party(word) {
            Some(e) => {
                if !buf.is_empty() {
                    out.push(Value::map().with("type", "text").with("text", std::mem::take(&mut buf)));
                }
                out.push(Value::map().with("type", "emote").with("text", word).with("emote", e.value()));
            }
            None => buf.push_str(word),
        }
        rest = &rest[end..];
    }
    if !buf.is_empty() {
        out.push(Value::map().with("type", "text").with("text", buf));
    }
}

/// EventSub message fragments → overlay fragments (Twitch emotes get CDN urls, third-party
/// emote words are split out of text).
pub fn fragments(frags: Option<&J>, lk: &dyn Lookup) -> Value {
    let mut out = Vec::new();
    for f in frags.and_then(J::as_array).map(Vec::as_slice).unwrap_or(&[]) {
        let text = js(f, "text");
        match f.get("type").and_then(J::as_str).unwrap_or("text") {
            "emote" => {
                let e = f.get("emote").cloned().unwrap_or(J::Null);
                let id = js(&e, "id");
                let animated = e.get("format").and_then(J::as_array).is_some_and(|a| a.iter().any(|x| x.as_str() == Some("animated")));
                let emote = Emote { url: twitch_emote_url(&id, animated), id, provider: "twitch".into(), animated, zero_width: false };
                out.push(Value::map().with("type", "emote").with("text", text).with("emote", emote.value()));
            }
            "cheermote" => {
                let c = f.get("cheermote").cloned().unwrap_or(J::Null);
                let cm = Value::map().with("prefix", js(&c, "prefix")).with("bits", ji(&c, "bits")).with("tier", ji(&c, "tier"));
                out.push(Value::map().with("type", "cheermote").with("text", text).with("cheermote", cm));
            }
            "mention" => {
                let m = f.get("mention").cloned().unwrap_or(J::Null);
                let mv = Value::map().with("user_id", js(&m, "user_id")).with("user_login", js(&m, "user_login")).with("user_name", js(&m, "user_name"));
                out.push(Value::map().with("type", "mention").with("text", text).with("mention", mv));
            }
            _ => split_third_party(&text, lk, &mut out),
        }
    }
    Value::List(out)
}

/// Fragments for a plain message (resub messages carry emote offsets, not fragments).
fn text_fragments(text: &str, lk: &dyn Lookup) -> Value {
    let mut out = Vec::new();
    split_third_party(text, lk, &mut out);
    Value::List(out)
}

fn ev(ty: &str, payload: Value, a: Option<Actor>) -> Event {
    let mut e = Event::new(ty, Origin::Twitch, payload);
    e.actor = a;
    e
}

struct OpenGift {
    gift_id: String,
    gifter: String,
    tier: i64,
    remaining: i64,
    expires_ms: i64,
}

/// Notification → events, with gift attribution state for [`crate::config::SubSource::Eventsub`].
#[derive(Default)]
pub struct Normalizer {
    gifts: VecDeque<OpenGift>,
}

impl Normalizer {
    /// Events for one EventSub notification (`subscription.type`, `.version`, `event`).
    pub fn notification(&mut self, ty: &str, version: &str, e: &J, lk: &dyn Lookup, now_ms: i64) -> Vec<Event> {
        self.gifts.retain(|g| g.remaining > 0 && g.expires_ms > now_ms);
        match ty {
            "channel.chat.message" => chat_message(e, lk).into_iter().collect(),
            "channel.chat.message_delete" => {
                vec![ev(
                    "twitch.chat.delete",
                    Value::map().with("message_id", js(e, "message_id")).with("user_id", js(e, "target_user_id")).with("user", js(e, "target_user_name")),
                    None,
                )]
            }
            "channel.chat.clear_user_messages" => {
                vec![ev("twitch.user.purge", Value::map().with("user_id", js(e, "target_user_id")).with("user", js(e, "target_user_name")), None)]
            }
            "channel.chat.clear" => vec![ev("twitch.chat.clear", Value::map(), None)],
            "channel.chat.notification" => self.chat_notification(e, lk),
            "channel.subscribe" => {
                let a = viewer(e, "", lk);
                let is_gift = jb(e, "is_gift");
                let t = tier(&js(e, "tier"));
                let mut p = Value::map().with("tier", t).with("months", 1).with("is_gift", is_gift).with("message", "").with("user", js(e, "user_name"));
                if is_gift {
                    let g = self.gifts.iter_mut().find(|g| g.tier == t && g.remaining > 0);
                    let (gid, gifter) = match g {
                        Some(g) => {
                            g.remaining -= 1;
                            (g.gift_id.clone(), g.gifter.clone())
                        }
                        None => (String::new(), String::new()),
                    };
                    p = p.with("gift_id", gid).with("gifter", gifter);
                }
                vec![ev("twitch.sub", p, a.map(|mut a| add_sub(&mut a)))]
            }
            "channel.subscription.gift" => {
                let anon = jb(e, "is_anonymous");
                let a = if anon { Some(anonymous()) } else { viewer(e, "", lk) };
                let gifter = a.as_ref().map(|a| a.name.clone()).unwrap_or_default();
                let total = ji(e, "total").max(1);
                let t = tier(&js(e, "tier"));
                let gift_id = format!("gift-{}-{now_ms}", a.as_ref().map(|a| a.id.as_str()).unwrap_or("anonymous"));
                self.gifts.push_back(OpenGift { gift_id: gift_id.clone(), gifter: gifter.clone(), tier: t, remaining: total, expires_ms: now_ms + 60_000 });
                if self.gifts.len() > 32 {
                    self.gifts.pop_front();
                }
                vec![ev(
                    "twitch.gift",
                    Value::map()
                        .with("count", total)
                        .with("tier", t)
                        .with("total", ji(e, "cumulative_total"))
                        .with("gift_id", gift_id)
                        .with("anonymous", anon)
                        .with("user", gifter),
                    a,
                )]
            }
            "channel.subscription.message" => {
                let a = viewer(e, "", lk);
                let msg = e.get("message").map(|m| js(m, "text")).unwrap_or_default();
                vec![ev(
                    "twitch.resub",
                    Value::map()
                        .with("tier", tier(&js(e, "tier")))
                        .with("months", ji(e, "cumulative_months"))
                        .with("streak", ji(e, "streak_months"))
                        .with("duration_months", ji(e, "duration_months"))
                        .with("is_gift", false)
                        .with("fragments", text_fragments(&msg, lk))
                        .with("message", msg)
                        .with("user", js(e, "user_name")),
                    a.map(|mut a| add_sub(&mut a)),
                )]
            }
            "channel.cheer" => {
                let anon = jb(e, "is_anonymous") || js(e, "user_id").is_empty();
                let a = if anon { Some(anonymous()) } else { viewer(e, "", lk) };
                let msg = js(e, "message");
                vec![ev(
                    "twitch.cheer",
                    Value::map()
                        .with("bits", ji(e, "bits"))
                        .with("fragments", text_fragments(&msg, lk))
                        .with("message", msg)
                        .with("anonymous", anon)
                        .with("user", a.as_ref().map(|a| a.name.clone()).unwrap_or_default()),
                    a,
                )]
            }
            "channel.follow" => {
                let a = viewer(e, "", lk).map(|mut a| {
                    if lk.follower_min_age_s() <= 0 && !a.roles.contains(&Role::Follower) {
                        a.roles.push(Role::Follower);
                        a.roles.sort();
                    }
                    a
                });
                vec![ev("twitch.follow", Value::map().with("user", js(e, "user_name")).with("followed_at", jt(e, "followed_at")), a)]
            }
            "channel.raid" => {
                let a = viewer(e, "from_broadcaster_", lk);
                let from = js(e, "from_broadcaster_user_name");
                vec![ev(
                    "twitch.raid",
                    Value::map()
                        .with("viewers", ji(e, "viewers"))
                        .with("from", from.clone())
                        .with("from_login", js(e, "from_broadcaster_user_login"))
                        .with("user", from),
                    a,
                )]
            }
            "channel.channel_points_custom_reward_redemption.add" => {
                let a = viewer(e, "", lk);
                let r = e.get("reward").cloned().unwrap_or(J::Null);
                let reward_id = js(&r, "id");
                let (key, managed) = lk.reward(&reward_id);
                let user_id = js(e, "user_id");
                let mut p = Value::map()
                    .with("reward", js(&r, "title"))
                    .with("reward_id", reward_id)
                    .with("redemption_id", js(e, "id"))
                    .with("cost", ji(&r, "cost"))
                    .with("input", js(e, "user_input"))
                    .with("status", js(e, "status"))
                    .with("managed", managed)
                    .with("follow_age_s", lk.follow_age_s(&user_id).unwrap_or(-1))
                    .with("user", js(e, "user_name"));
                if let Some(k) = key {
                    p = p.with("reward_key", k);
                }
                vec![ev("twitch.redeem", p, a)]
            }
            "automod.message.hold" => {
                // v1: `message` is a string; v2: `{text, fragments}` plus `reason`/`automod`/`blocked_term`
                let (text, category, level, reason) = if version == "1" || e.get("message").is_some_and(J::is_string) {
                    (js(e, "message"), js(e, "category"), ji(e, "level"), "automod".to_string())
                } else {
                    let m = e.get("message").cloned().unwrap_or(J::Null);
                    let am = e.get("automod").cloned().unwrap_or(J::Null);
                    let reason = js(e, "reason");
                    let category = if reason == "blocked_term" { "blocked_term".to_string() } else { js(&am, "category") };
                    (js(&m, "text"), category, ji(&am, "level"), reason)
                };
                let text = se_core::policy::display_text(&text, 500, 2);
                vec![ev(
                    "twitch.automod.hold",
                    Value::map()
                        .with("message_id", js(e, "message_id"))
                        .with("user", js(e, "user_name"))
                        .with("user_id", js(e, "user_id"))
                        .with("text", text)
                        .with("category", category)
                        .with("level", level)
                        .with("reason", reason)
                        .with("held_at", jt(e, "held_at")),
                    None,
                )]
            }
            "automod.message.update" => {
                vec![ev(
                    "twitch.automod.update",
                    Value::map()
                        .with("message_id", js(e, "message_id"))
                        .with("status", js(e, "status").to_lowercase())
                        .with("user_id", js(e, "user_id"))
                        .with("moderator", js(e, "moderator_user_name")),
                    None,
                )]
            }
            "channel.ban" => {
                let (uid, user) = (js(e, "user_id"), js(e, "user_name"));
                let base = Value::map()
                    .with("user", user.clone())
                    .with("user_id", uid.clone())
                    .with("reason", js(e, "reason"))
                    .with("moderator", js(e, "moderator_user_name"));
                let main = if jb(e, "is_permanent") {
                    ev("twitch.ban", base, None)
                } else {
                    let secs = (jt(e, "ends_at") - jt(e, "banned_at")).max(0);
                    ev("twitch.timeout", base.with("duration_s", secs), None)
                };
                vec![main, ev("twitch.user.purge", Value::map().with("user_id", uid).with("user", user), None)]
            }
            "channel.unban" => {
                vec![ev(
                    "twitch.unban",
                    Value::map().with("user", js(e, "user_name")).with("user_id", js(e, "user_id")).with("moderator", js(e, "moderator_user_name")),
                    None,
                )]
            }
            t if t.starts_with("channel.poll.") => vec![ev(&format!("twitch.poll.{}", &t["channel.poll.".len()..]), poll(e), None)],
            t if t.starts_with("channel.prediction.") => vec![ev(&format!("twitch.prediction.{}", &t["channel.prediction.".len()..]), prediction(e), None)],
            t if t.starts_with("channel.hype_train.") => vec![ev(&format!("twitch.hype_train.{}", &t["channel.hype_train.".len()..]), hype_train(e), None)],
            "channel.ad_break.begin" => {
                vec![ev(
                    "twitch.ad_break",
                    Value::map().with("duration", ji(e, "duration_seconds")).with("automatic", jb(e, "is_automatic")).with("started_at", jt(e, "started_at")),
                    None,
                )]
            }
            "stream.online" => vec![ev("twitch.stream.online", Value::map().with("started_at", jt(e, "started_at")).with("type", js(e, "type")), None)],
            "stream.offline" => vec![ev("twitch.stream.offline", Value::map(), None)],
            "channel.update" => vec![ev(
                "twitch.channel.update",
                Value::map().with("title", js(e, "title")).with("category", js(e, "category_name")).with("category_id", js(e, "category_id")),
                None,
            )],
            _ => Vec::new(),
        }
    }

    fn chat_notification(&mut self, e: &J, lk: &dyn Lookup) -> Vec<Event> {
        let anon = jb(e, "chatter_is_anonymous");
        let chatter = if anon {
            Some(anonymous())
        } else {
            viewer(e, "chatter_", lk).map(|mut a| {
                let (mut roles, _) = roles_from_badges(e.get("badges"));
                if a.id == lk.broadcaster_id() {
                    roles.push(Role::Owner);
                }
                a.roles = with_follower(roles, &a.id.clone(), lk);
                a
            })
        };
        let name = chatter.as_ref().map(|a| a.name.clone()).unwrap_or_default();
        let msg = e.get("message").map(|m| js(m, "text")).unwrap_or_default();
        let frags = fragments(e.get("message").and_then(|m| m.get("fragments")), lk);
        let notice = js(e, "notice_type");
        let block = e.get(notice.as_str()).cloned().unwrap_or(J::Null);
        let notice_id = js(e, "message_id");
        match notice.as_str() {
            "sub" => vec![ev(
                "twitch.sub",
                Value::map()
                    .with("tier", sub_tier(&block))
                    .with("months", 1)
                    .with("duration_months", ji(&block, "duration_months"))
                    .with("is_prime", jb(&block, "is_prime"))
                    .with("is_gift", false)
                    .with("fragments", frags)
                    .with("message", msg)
                    .with("user", name),
                chatter.map(|mut a| add_sub(&mut a)),
            )],
            "resub" => vec![ev(
                "twitch.resub",
                Value::map()
                    .with("tier", sub_tier(&block))
                    .with("months", ji(&block, "cumulative_months"))
                    .with("streak", ji(&block, "streak_months"))
                    .with("duration_months", ji(&block, "duration_months"))
                    .with("is_prime", jb(&block, "is_prime"))
                    .with("is_gift", jb(&block, "is_gift"))
                    .with("gifter", js(&block, "gifter_user_name"))
                    .with("fragments", frags)
                    .with("message", msg)
                    .with("user", name),
                chatter.map(|mut a| add_sub(&mut a)),
            )],
            "sub_gift" => {
                let t = sub_tier(&block);
                let community = js(&block, "community_gift_id");
                let gift_id = if community.is_empty() { notice_id.clone() } else { community.clone() };
                let mut out = Vec::new();
                if community.is_empty() {
                    // a single direct gift: announce it like a bomb of one
                    out.push(ev(
                        "twitch.gift",
                        Value::map()
                            .with("count", 1)
                            .with("tier", t)
                            .with("total", ji(&block, "cumulative_total"))
                            .with("gift_id", gift_id.clone())
                            .with("anonymous", anon)
                            .with("user", name.clone()),
                        chatter.clone(),
                    ));
                }
                let rid = js(&block, "recipient_user_id");
                let rname = js(&block, "recipient_user_name");
                let recipient = (!rid.is_empty()).then(|| {
                    let roles = with_follower(lk.cached_roles(&rid).unwrap_or_default(), &rid, lk);
                    let mut a = actor(&rid, &rname, roles);
                    add_sub(&mut a)
                });
                out.push(ev(
                    "twitch.sub",
                    Value::map()
                        .with("tier", t)
                        .with("months", ji(&block, "duration_months").max(1))
                        .with("is_gift", true)
                        .with("gift_id", gift_id)
                        .with("gifter", name)
                        .with("user", rname),
                    recipient,
                ));
                out
            }
            "community_sub_gift" => vec![ev(
                "twitch.gift",
                Value::map()
                    .with("count", ji(&block, "total"))
                    .with("tier", sub_tier(&block))
                    .with("total", ji(&block, "cumulative_total"))
                    .with("gift_id", js(&block, "id"))
                    .with("anonymous", anon)
                    .with("user", name),
                chatter,
            )],
            "gift_paid_upgrade" | "prime_paid_upgrade" => vec![ev(
                "twitch.sub",
                Value::map()
                    .with("tier", sub_tier(&block))
                    .with("months", 1)
                    .with("is_gift", false)
                    .with("upgrade", notice.clone())
                    .with("gifter", js(&block, "gifter_user_name"))
                    .with("message", msg)
                    .with("user", name),
                chatter.map(|mut a| add_sub(&mut a)),
            )],
            "announcement" => vec![ev(
                "twitch.announcement",
                Value::map().with("color", js(&block, "color")).with("fragments", frags).with("message", msg).with("user", name),
                chatter,
            )],
            "bits_badge_tier" => vec![ev("twitch.bits_badge", Value::map().with("tier", ji(&block, "tier")).with("user", name), chatter)],
            "charity_donation" => {
                let amt = block.get("amount").cloned().unwrap_or(J::Null);
                let value = ji(&amt, "value") as f64 / 10f64.powi(ji(&amt, "decimal_place") as i32);
                vec![ev(
                    "twitch.charity",
                    Value::map().with("amount", value).with("currency", js(&amt, "currency")).with("charity", js(&block, "charity_name")).with("user", name),
                    chatter,
                )]
            }
            // raids come from `channel.raid`; shared-chat notices belong to other channels
            _ => Vec::new(),
        }
    }
}

fn add_sub(a: &mut Actor) -> Actor {
    if !a.roles.contains(&Role::Sub) && a.id != "anonymous" {
        a.roles.push(Role::Sub);
        a.roles.sort();
    }
    a.clone()
}

fn chat_message(e: &J, lk: &dyn Lookup) -> Option<Event> {
    let uid = js(e, "chatter_user_id");
    if uid.is_empty() {
        return None;
    }
    let (mut roles, sub_months) = roles_from_badges(e.get("badges"));
    if uid == lk.broadcaster_id() && !roles.contains(&Role::Owner) {
        roles.push(Role::Owner);
    }
    let roles = with_follower(roles, &uid, lk);
    let name = js(e, "chatter_user_name");
    let m = e.get("message").cloned().unwrap_or(J::Null);
    let mut p = Value::map()
        .with("message", js(&m, "text"))
        .with("message_id", js(e, "message_id"))
        .with("fragments", fragments(m.get("fragments"), lk))
        .with("user", name.clone())
        .with("user_id", uid.clone())
        .with("login", js(e, "chatter_user_login"))
        .with("color", js(e, "color"))
        .with("badges", badges_value(e.get("badges"), lk))
        .with("sub_months", sub_months)
        .with("follow_age_s", lk.follow_age_s(&uid).unwrap_or(-1))
        .with("message_type", js(e, "message_type"))
        .with("bot", lk.bot_user_id().is_some_and(|b| b == uid));
    if let Some(bits) = e.get("cheer").filter(|c| !c.is_null()).map(|c| ji(c, "bits")) {
        p = p.with("bits", bits);
    }
    if let Some(r) = e.get("reply").filter(|r| !r.is_null()) {
        p = p.with("reply_to", js(r, "parent_message_id")).with("reply_to_user", js(r, "parent_user_name"));
    }
    let reward = js(e, "channel_points_custom_reward_id");
    if !reward.is_empty() {
        p = p.with("reward_id", reward);
    }
    let source = js(e, "source_broadcaster_user_id");
    if !source.is_empty() && source != lk.broadcaster_id() {
        p = p.with("shared_from", js(e, "source_broadcaster_user_login"));
    }
    Some(ev("twitch.chat", p, Some(actor(&uid, &name, roles))))
}

fn poll(e: &J) -> Value {
    let choices: Vec<Value> = e
        .get("choices")
        .and_then(J::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|c| {
            Value::map()
                .with("id", js(c, "id"))
                .with("title", js(c, "title"))
                .with("votes", ji(c, "votes"))
                .with("points_votes", ji(c, "channel_points_votes"))
                .with("bits_votes", ji(c, "bits_votes"))
        })
        .collect();
    let total: i64 = choices.iter().filter_map(|c| c.get_path("votes").and_then(Value::as_i64)).sum();
    let winner = choices
        .iter()
        .filter(|_| e.get("status").is_some())
        .max_by_key(|c| c.get_path("votes").and_then(Value::as_i64).unwrap_or(0))
        .and_then(|c| c.get_path("id").cloned())
        .unwrap_or_default();
    let mut v = Value::map()
        .with("id", js(e, "id"))
        .with("title", js(e, "title"))
        .with("total_votes", total)
        .with("started_at", jt(e, "started_at"))
        .with("ends_at", jt(e, "ends_at").max(jt(e, "ended_at")))
        .with("choices", choices);
    if e.get("status").is_some() {
        v = v.with("status", js(e, "status")).with("winner", winner);
    }
    v
}

fn prediction(e: &J) -> Value {
    let outcomes: Vec<Value> = e
        .get("outcomes")
        .and_then(J::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|o| {
            Value::map()
                .with("id", js(o, "id"))
                .with("title", js(o, "title"))
                .with("color", js(o, "color"))
                .with("users", ji(o, "users"))
                .with("points", ji(o, "channel_points"))
        })
        .collect();
    let mut v = Value::map().with("id", js(e, "id")).with("title", js(e, "title")).with("locks_at", jt(e, "locks_at")).with("outcomes", outcomes);
    if e.get("winning_outcome_id").is_some() || e.get("status").is_some() {
        v = v.with("winner", js(e, "winning_outcome_id")).with("status", js(e, "status"));
    }
    v
}

fn hype_train(e: &J) -> Value {
    let goal = ji(e, "goal");
    let progress = ji(e, "progress");
    let top: Vec<Value> = e
        .get("top_contributions")
        .and_then(J::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|c| Value::map().with("user", js(c, "user_name")).with("type", js(c, "type")).with("total", ji(c, "total")))
        .collect();
    Value::map()
        .with("id", js(e, "id"))
        .with("level", ji(e, "level").max(1))
        .with("total", ji(e, "total"))
        .with("progress", progress)
        .with("goal", goal)
        .with("fraction", if goal > 0 { (progress as f64 / goal as f64).clamp(0.0, 1.0) } else { 0.0 })
        .with("type", js(e, "type"))
        .with("expires_at", jt(e, "expires_at"))
        .with("top", top)
}
