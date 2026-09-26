//! EventSub payloads (fixtures from dev.twitch.tv/docs/eventsub, plus chat variants built from
//! the EventSub reference) → normalized events matching the simulator contract (`se_core::sim`).

use se_core::rng::Rng;
use se_proto::{Event, Role, Value};
use se_twitch::normalize::{Emote, Lookup, Normalizer};
use serde_json::Value as J;
use std::collections::{BTreeSet, HashMap};

struct Dir {
    broadcaster: String,
    follows: HashMap<String, i64>,
    min_follow: i64,
    roles: HashMap<String, Vec<Role>>,
    managed: HashMap<String, String>,
}

impl Dir {
    fn new(b: &str) -> Dir {
        Dir { broadcaster: b.into(), follows: HashMap::new(), min_follow: 0, roles: HashMap::new(), managed: HashMap::new() }
    }
}

impl Lookup for Dir {
    fn broadcaster_id(&self) -> &str {
        &self.broadcaster
    }
    fn bot_user_id(&self) -> Option<&str> {
        Some("bot-1")
    }
    fn follow_age_s(&self, user_id: &str) -> Option<i64> {
        self.follows.get(user_id).copied()
    }
    fn follower_min_age_s(&self) -> i64 {
        self.min_follow
    }
    fn cached_roles(&self, user_id: &str) -> Option<Vec<Role>> {
        self.roles.get(user_id).cloned()
    }
    fn badge_url(&self, set_id: &str, id: &str) -> Option<String> {
        Some(format!("https://badges.test/{set_id}/{id}"))
    }
    fn third_party(&self, code: &str) -> Option<Emote> {
        (code == "catJAM").then(|| Emote {
            id: "7a".into(),
            url: "https://cdn.7tv.app/emote/7a/4x.webp".into(),
            provider: "7tv".into(),
            animated: true,
            zero_width: false,
        })
    }
    fn reward(&self, reward_id: &str) -> (Option<String>, bool) {
        (self.managed.get(reward_id).cloned(), self.managed.contains_key(reward_id))
    }
}

fn fixture(name: &str) -> J {
    let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))).unwrap()
}

fn run(n: &mut Normalizer, doc: &J, dir: &Dir) -> Vec<Event> {
    let ty = doc["subscription"]["type"].as_str().unwrap();
    let ver = doc["subscription"]["version"].as_str().unwrap();
    n.notification(ty, ver, &doc["event"], dir, 1_700_000_000_000)
}

fn one(name: &str, dir: &Dir) -> Event {
    let evs = run(&mut Normalizer::default(), &fixture(name), dir);
    assert_eq!(evs.len(), 1, "{name}: {evs:#?}");
    evs.into_iter().next().unwrap()
}

fn p<'a>(e: &'a Event, k: &str) -> &'a Value {
    e.payload.get_path(k).unwrap_or_else(|| panic!("{} has no `{k}`: {:?}", e.ty, e.payload))
}

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_map().map(|m| m.keys().cloned().collect()).unwrap_or_default()
}

/// Every field the simulator produces for `preset` must be present in the normalized event.
fn assert_superset_of_sim(preset: &str, args: &str, real: &Event) {
    let mut rng = Rng::new(7);
    let args = se_proto::Op::parse(&format!("x {args}")).ok().and_then(|op| match op {
        se_proto::Op::Action { args, .. } => Some(args),
        _ => None,
    });
    let sims = se_core::sim::events(preset, &args.unwrap_or_default(), &mut rng).unwrap();
    let sim = sims.iter().find(|s| s.ty == real.ty).unwrap_or_else(|| panic!("simulator {preset} has no {}", real.ty));
    let missing: Vec<_> = keys(&sim.payload).difference(&keys(&real.payload)).cloned().collect();
    assert!(missing.is_empty(), "{} lacks simulator fields {missing:?}", real.ty);
    assert_eq!(sim.actor.is_some(), real.actor.is_some(), "{}: actor presence differs from the simulator", real.ty);
}

#[test]
fn chat_message_roles_badges_follower() {
    let mut dir = Dir::new("1971641");
    dir.follows.insert("4145994".into(), 86_400);
    dir.min_follow = 3600;
    let e = one("channel.chat.message", &dir);
    assert_eq!(e.ty, "twitch.chat");
    assert_eq!(p(&e, "message").as_str(), Some("Hi chat"));
    assert_eq!(p(&e, "message_id").as_str(), Some("cc106a89-1814-919d-454c-f4f2f970aae7"));
    assert_eq!(p(&e, "sub_months"), &Value::Int(16));
    assert_eq!(p(&e, "follow_age_s"), &Value::Int(86_400));
    assert_eq!(p(&e, "badges.0.set_id").as_str(), Some("moderator"));
    assert_eq!(p(&e, "badges.1.url").as_str(), Some("https://badges.test/subscriber/12"));
    assert_eq!(p(&e, "bot"), &Value::Bool(false));
    let a = e.actor.as_ref().unwrap();
    assert_eq!(a.roles, vec![Role::Follower, Role::Sub, Role::Mod]);
    assert_eq!(a.top_role(), Role::Mod);
    // too-young follow: no follower role
    dir.min_follow = 100_000;
    assert!(!one("channel.chat.message", &dir).actor.unwrap().roles.contains(&Role::Follower));
    assert_superset_of_sim("chat", "", &e);
}

#[test]
fn chat_fragments_emotes_cheer_reply_and_shared_chat() {
    let dir = Dir::new("1971641");
    let e = one("channel.chat.message.emotes", &dir);
    let types: Vec<&str> = p(&e, "fragments").as_list().unwrap().iter().map(|f| f.get_path("type").unwrap().as_str().unwrap()).collect();
    assert_eq!(types, ["cheermote", "text", "mention", "text", "emote", "text", "emote", "text"]);
    assert_eq!(p(&e, "fragments.4.emote.url").as_str(), Some("https://static-cdn.jtvnw.net/emoticons/v2/25/static/dark/3.0"));
    assert_eq!(p(&e, "fragments.4.emote.provider").as_str(), Some("twitch"));
    assert_eq!(p(&e, "fragments.6.emote.provider").as_str(), Some("7tv"), "third-party words become emote fragments");
    assert_eq!(p(&e, "fragments.5.text").as_str(), Some(" "));
    assert_eq!(p(&e, "fragments.7.text").as_str(), Some(" gg"));
    assert_eq!(p(&e, "fragments.0.cheermote.bits"), &Value::Int(100));
    assert_eq!(p(&e, "bits"), &Value::Int(100));
    assert_eq!(p(&e, "reply_to").as_str(), Some("parent-1"));
    assert_eq!(e.actor.unwrap().roles, vec![Role::Vip]);
    let shared = one("channel.chat.message.shared", &Dir::new("1971641"));
    assert!(shared.payload.get_path("shared_from").is_some(), "{:?}", shared.payload);
}

#[test]
fn deletion_sync_events() {
    let dir = Dir::new("1337");
    let d = one("channel.chat.message_delete", &dir);
    assert_eq!(d.ty, "twitch.chat.delete");
    assert!(!p(&d, "message_id").as_str().unwrap().is_empty());
    let c = one("channel.chat.clear_user_messages", &dir);
    assert_eq!(c.ty, "twitch.user.purge");
    assert!(!p(&c, "user_id").as_str().unwrap().is_empty());
    assert_eq!(one("channel.chat.clear", &dir).ty, "twitch.chat.clear");
    let ban = run(&mut Normalizer::default(), &fixture("channel.ban"), &dir);
    assert_eq!(ban.iter().map(|e| e.ty.as_str()).collect::<Vec<_>>(), ["twitch.timeout", "twitch.user.purge"]);
    assert_eq!(p(&ban[0], "duration_s"), &Value::Int(60));
    assert_eq!(p(&ban[0], "reason").as_str(), Some("Offensive language"));
    assert_eq!(p(&ban[1], "user_id").as_str(), Some("1234"));
    let mut perm = fixture("channel.ban");
    perm["event"]["is_permanent"] = J::Bool(true);
    assert_eq!(run(&mut Normalizer::default(), &perm, &dir)[0].ty, "twitch.ban");
    assert_eq!(one("channel.unban", &dir).ty, "twitch.unban");
}

#[test]
fn chat_notification_resub_and_gift_bomb_link_every_recipient() {
    let dir = Dir::new("1971641");
    let r = one("channel.chat.notification", &dir);
    assert_eq!(r.ty, "twitch.resub");
    assert_eq!(p(&r, "months"), &Value::Int(10));
    assert_eq!(p(&r, "tier"), &Value::Int(1));
    assert_superset_of_sim("resub", "", &r);
    let mut n = Normalizer::default();
    let gift = run(&mut n, &fixture("channel.chat.notification.community_sub_gift"), &dir);
    assert_eq!(gift.len(), 1);
    let g = &gift[0];
    assert_eq!(g.ty, "twitch.gift");
    assert_eq!((p(g, "count"), p(g, "gift_id").as_str(), p(g, "total")), (&Value::Int(3), Some("cg-555"), &Value::Int(120)));
    assert_superset_of_sim("gift_bomb", "count=3", g);
    let subs: Vec<Event> = fixture("channel.chat.notification.sub_gift_bomb").as_array().unwrap().iter().flat_map(|d| run(&mut n, d, &dir)).collect();
    assert_eq!(subs.len(), 3, "no extra gift event per recipient of a bomb");
    for (i, s) in subs.iter().enumerate() {
        assert_eq!(s.ty, "twitch.sub");
        assert_eq!(p(s, "gift_id").as_str(), Some("cg-555"));
        assert_eq!(p(s, "gifter").as_str(), Some("viewer23"));
        assert_eq!(p(s, "is_gift"), &Value::Bool(true));
        assert_eq!(p(s, "user").as_str(), Some(format!("Recipient{i}").as_str()));
        let a = s.actor.as_ref().unwrap();
        assert_eq!((a.id.as_str(), a.top_role()), (format!("r{i}").as_str(), Role::Sub));
        assert_superset_of_sim("gift_bomb", "count=3", s);
    }
    // a single direct gift announces itself as a bomb of one
    let single = run(&mut Normalizer::default(), &fixture("channel.chat.notification.sub_gift"), &dir);
    assert_eq!(single.iter().map(|e| e.ty.as_str()).collect::<Vec<_>>(), ["twitch.gift", "twitch.sub"]);
    assert_eq!(p(&single[0], "count"), &Value::Int(1));
    assert_eq!(p(&single[1], "tier"), &Value::Int(2));
    assert_eq!(p(&single[0], "gift_id"), p(&single[1], "gift_id"));
    // other channels' shared-chat notices are not ours
    assert!(run(&mut Normalizer::default(), &fixture("channel.chat.notification.shared_resub"), &dir).is_empty());
}

#[test]
fn eventsub_sub_source_attributes_gifts_to_the_open_bomb() {
    let dir = Dir::new("1337");
    let mut n = Normalizer::default();
    let g = run(&mut n, &fixture("channel.subscription.gift"), &dir);
    assert_eq!(p(&g[0], "count"), &Value::Int(2));
    let gid = p(&g[0], "gift_id").clone();
    let mut sub = fixture("channel.subscribe");
    sub["event"]["is_gift"] = J::Bool(true);
    let s1 = run(&mut n, &sub, &dir);
    let s2 = run(&mut n, &sub, &dir);
    let s3 = run(&mut n, &sub, &dir);
    assert_eq!(p(&s1[0], "gift_id"), &gid);
    assert_eq!(p(&s2[0], "gift_id"), &gid);
    assert_eq!(p(&s3[0], "gift_id").as_str(), Some(""), "the bomb had only 2 gifts");
    assert_eq!(p(&s1[0], "gifter").as_str(), Some("Cool_User"));
    let plain = one("channel.subscribe", &dir);
    assert_eq!((p(&plain, "is_gift"), p(&plain, "months")), (&Value::Bool(false), &Value::Int(1)));
    assert_superset_of_sim("sub", "", &plain);
    let resub = one("channel.subscription.message", &dir);
    assert_eq!((resub.ty.as_str(), p(&resub, "months"), p(&resub, "message").as_str()), ("twitch.resub", &Value::Int(15), Some("Love the stream! FevziGG")));
}

#[test]
fn cheers_follows_raids_redemptions() {
    let mut dir = Dir::new("1337");
    dir.roles.insert("1234".into(), vec![Role::Vip]);
    let c = one("channel.cheer", &dir);
    assert_eq!((p(&c, "bits"), p(&c, "message").as_str(), p(&c, "user").as_str()), (&Value::Int(1000), Some("pogchamp"), Some("Cool_User")));
    assert_eq!(c.actor.as_ref().unwrap().roles, vec![Role::Vip], "cached chat roles apply to badge-less events");
    assert_superset_of_sim("cheer", "", &c);
    let mut anon = fixture("channel.cheer");
    anon["event"]["is_anonymous"] = J::Bool(true);
    anon["event"]["user_id"] = J::Null;
    let a = run(&mut Normalizer::default(), &anon, &dir).remove(0);
    assert_eq!((a.actor.as_ref().unwrap().id.as_str(), p(&a, "anonymous")), ("anonymous", &Value::Bool(true)));
    let f = one("channel.follow", &dir);
    assert_eq!((f.ty.as_str(), p(&f, "user").as_str(), p(&f, "followed_at")), ("twitch.follow", Some("Cool_User"), &Value::Int(1_594_836_971)));
    assert!(f.actor.as_ref().unwrap().roles.contains(&Role::Follower));
    assert_superset_of_sim("follow", "", &f);
    let r = one("channel.raid", &dir);
    assert_eq!((p(&r, "viewers"), p(&r, "from").as_str()), (&Value::Int(9001), Some("Cool_User")));
    assert_superset_of_sim("raid", "", &r);
    dir.managed.insert("92af127c-7326-4483-a52b-b0da0be61c01".into(), "hype".into());
    let red = one("channel.channel_points_custom_reward_redemption.add", &dir);
    assert_eq!(red.ty, "twitch.redeem");
    assert_eq!(p(&red, "redemption_id").as_str(), Some("17fa2df1-ad76-4804-bfa5-a40ef63efe63"));
    assert_eq!((p(&red, "input").as_str(), p(&red, "cost"), p(&red, "status").as_str()), (Some("pogchamp"), &Value::Int(100), Some("unfulfilled")));
    assert_eq!((p(&red, "managed"), p(&red, "reward_key").as_str()), (&Value::Bool(true), Some("hype")));
    assert_superset_of_sim("redeem", "", &red);
}

#[test]
fn automod_polls_predictions_hype_trains_ads_stream() {
    let dir = Dir::new("1337");
    let h1 = one("automod.message.hold", &dir);
    assert_eq!((h1.ty.as_str(), p(&h1, "category").as_str(), p(&h1, "level")), ("twitch.automod.hold", Some("aggressive"), &Value::Int(5)));
    let h2 = one("automod.message.hold.v2", &dir);
    assert_eq!(
        (p(&h2, "message_id").as_str(), p(&h2, "text").as_str(), p(&h2, "category").as_str()),
        (Some("bad-message-id"), Some("This is a bad message… pogchamp"), Some("aggressive"))
    );
    let u = one("automod.message.update.v2", &dir);
    assert_eq!((u.ty.as_str(), p(&u, "status").as_str()), ("twitch.automod.update", Some("approved")));
    let pb = one("channel.poll.begin", &dir);
    assert_eq!((pb.ty.as_str(), p(&pb, "choices").as_list().unwrap().len()), ("twitch.poll.begin", 3));
    assert!(pb.payload.get_path("status").is_none());
    let pe = one("channel.poll.end", &dir);
    assert_eq!((p(&pe, "winner").as_str(), p(&pe, "total_votes"), p(&pe, "status").as_str()), (Some("124"), &Value::Int(340), Some("completed")));
    assert_eq!(one("channel.poll.progress", &dir).ty, "twitch.poll.progress");
    assert_eq!(one("channel.prediction.begin", &dir).ty, "twitch.prediction.begin");
    assert_eq!(one("channel.prediction.progress", &dir).ty, "twitch.prediction.progress");
    assert_eq!(one("channel.prediction.lock", &dir).ty, "twitch.prediction.lock");
    let pr = one("channel.prediction.end", &dir);
    assert_eq!((p(&pr, "winner").as_str(), p(&pr, "outcomes.0.points")), (Some("12345"), &Value::Int(15000)));
    let hb = one("channel.hype_train.begin", &dir);
    assert_eq!((hb.ty.as_str(), p(&hb, "level"), p(&hb, "goal")), ("twitch.hype_train.begin", &Value::Int(1), &Value::Int(500)));
    assert!((p(&hb, "fraction").as_f64().unwrap() - 0.274).abs() < 1e-9);
    assert_superset_of_sim("hype_train", "", &hb);
    assert_eq!(one("channel.hype_train.progress", &dir).ty, "twitch.hype_train.progress");
    assert_eq!(one("channel.hype_train.end", &dir).ty, "twitch.hype_train.end");
    let ad = one("channel.ad_break.begin", &dir);
    assert_eq!((ad.ty.as_str(), p(&ad, "duration"), p(&ad, "automatic")), ("twitch.ad_break", &Value::Int(60), &Value::Bool(false)));
    assert!(ad.actor.is_none(), "ad breaks are platform events");
    assert_superset_of_sim("ad_break", "", &ad);
    let on = one("stream.online", &dir);
    assert_eq!((on.ty.as_str(), p(&on, "started_at")), ("twitch.stream.online", &Value::Int(1_602_411_072)));
    assert_eq!(one("stream.offline", &dir).ty, "twitch.stream.offline");
}
