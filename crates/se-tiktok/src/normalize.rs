//! Decode the messages inside a fetch result into typed [`Item`]s with engine [`Actor`]s.
//! Unknown `method`s are ignored; a message that fails to decode is reported, never fatal.

use crate::proto::*;
use prost::Message;
use se_proto::{Actor, Role};

/// Chat text is data only and capped to this many characters.
pub const MAX_CHAT_CHARS: usize = 500;
const MAX_NAME_CHARS: usize = 64;

#[derive(Clone, Debug, PartialEq)]
pub struct GiftItem {
    pub actor: Actor,
    pub gift_id: i64,
    pub name: String,
    pub diamond_count: i64,
    pub streakable: bool,
    pub group_id: i64,
    /// Running count of the streak (or the count of a one-shot gift).
    pub repeat_count: i64,
    pub repeat_end: bool,
    /// Recipient when it is not the streamer (multi-guest rooms).
    pub to_user: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Chat { actor: Actor, text: String, message_id: String },
    Gift(GiftItem),
    Like { actor: Actor, count: u64, total: u64 },
    Follow { actor: Actor },
    Share { actor: Actor },
    Join { actor: Actor },
    Sub { actor: Actor, months: i64 },
    Viewers(u64),
    Control(i32),
}

/// Strip control characters and cap to `max` characters (not bytes).
pub fn clean_text(s: &str, max: usize) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).take(max).collect::<String>().trim().to_string()
}

fn display_name(u: &User) -> String {
    let nick = clean_text(&u.nickname, MAX_NAME_CHARS);
    if !nick.is_empty() {
        return nick;
    }
    let handle = clean_text(&u.display_id, MAX_NAME_CHARS);
    if !handle.is_empty() { handle } else { u.id.to_string() }
}

/// Engine actor for a TikTok user. Roles: `Mod` (moderator identity or room admin), `Sub`
/// (subscriber identity), `Follower` (follower/mutual identity or `follow_status ≥ 1`),
/// `Owner` (the streamer); `Everyone` when none apply.
pub fn actor(user: Option<&User>, identity: Option<&UserIdentity>) -> Actor {
    let Some(u) = user else {
        return Actor { platform: "tiktok".into(), id: String::new(), name: String::new(), roles: vec![Role::Everyone] };
    };
    let id = identity.copied_flags();
    let attr = u.user_attr.as_ref();
    let follower = id.follower || u.follow_info.as_ref().is_some_and(|f| f.follow_status >= 1);
    let moderator = id.moderator || attr.is_some_and(|a| a.is_admin || a.is_super_admin);
    let mut roles = Vec::new();
    if follower {
        roles.push(Role::Follower);
    }
    if id.subscriber {
        roles.push(Role::Sub);
    }
    if moderator {
        roles.push(Role::Mod);
    }
    if id.anchor {
        roles.push(Role::Owner);
    }
    if roles.is_empty() {
        roles.push(Role::Everyone);
    }
    Actor { platform: "tiktok".into(), id: u.id.to_string(), name: display_name(u), roles }
}

#[derive(Default)]
struct Flags {
    follower: bool,
    subscriber: bool,
    moderator: bool,
    anchor: bool,
}

trait IdentityFlags {
    fn copied_flags(&self) -> Flags;
}

impl IdentityFlags for Option<&UserIdentity> {
    fn copied_flags(&self) -> Flags {
        match self {
            Some(i) => Flags {
                follower: i.is_follower_of_anchor || i.is_mutual_following_with_anchor,
                subscriber: i.is_subscriber_of_anchor,
                moderator: i.is_moderator_of_anchor,
                anchor: i.is_anchor,
            },
            None => Flags::default(),
        }
    }
}

fn with_role(mut a: Actor, r: Role) -> Actor {
    if !a.roles.contains(&r) {
        a.roles.retain(|x| *x != Role::Everyone);
        a.roles.push(r);
        a.roles.sort();
    }
    a
}

fn display_key(common: Option<&CommonMessageData>) -> String {
    common.and_then(|c| c.display_text.as_ref()).map(|t| t.key.to_ascii_lowercase()).unwrap_or_default()
}

/// The message id used for de-duplication (envelope id, else the common block's).
pub fn message_id(m: &BaseProtoMessage, common_id: i64) -> i64 {
    if m.msg_id != 0 { m.msg_id } else { common_id }
}

/// Decode one message. `Ok(None)` = a method we don't use (or an irrelevant variant).
pub fn decode(m: &BaseProtoMessage) -> Result<Option<(i64, Item)>, prost::DecodeError> {
    let p = m.payload.as_slice();
    let common_id = |c: &Option<CommonMessageData>| c.as_ref().map_or(0, |c| c.msg_id);
    Ok(match m.method.as_str() {
        "WebcastChatMessage" => {
            let x = WebcastChatMessage::decode(p)?;
            let id = message_id(m, common_id(&x.common));
            let item = Item::Chat {
                actor: actor(x.user.as_ref(), x.user_identity.as_ref()),
                text: clean_text(&x.content, MAX_CHAT_CHARS),
                message_id: if id != 0 { id.to_string() } else { String::new() },
            };
            Some((id, item))
        }
        "WebcastGiftMessage" => {
            let x = WebcastGiftMessage::decode(p)?;
            let gift = x.gift.clone().unwrap_or_default();
            let gift_id = if x.gift_id != 0 { x.gift_id } else { gift.id };
            let name = clean_text(&gift.name, MAX_NAME_CHARS);
            let to_user = x.to_user.as_ref().filter(|u| u.id != 0).map(display_name);
            let item = GiftItem {
                actor: actor(x.user.as_ref(), x.user_identity.as_ref()),
                gift_id,
                name: if name.is_empty() { format!("gift {gift_id}") } else { name },
                diamond_count: i64::from(gift.diamond_count.max(0)),
                streakable: gift.r#type == GIFT_TYPE_STREAKABLE,
                group_id: x.group_id,
                repeat_count: i64::from(x.repeat_count.max(1)),
                repeat_end: x.repeat_end != 0,
                to_user,
            };
            Some((message_id(m, common_id(&x.common)), Item::Gift(item)))
        }
        "WebcastLikeMessage" => {
            let x = WebcastLikeMessage::decode(p)?;
            let item = Item::Like { actor: actor(x.user.as_ref(), None), count: x.count.max(0) as u64, total: x.total.max(0) as u64 };
            Some((message_id(m, common_id(&x.common)), item))
        }
        "WebcastMemberMessage" => {
            let x = WebcastMemberMessage::decode(p)?;
            (x.action == MEMBER_JOINED).then(|| (message_id(m, common_id(&x.common)), Item::Join { actor: actor(x.user.as_ref(), None) }))
        }
        "WebcastSocialMessage" => {
            let x = WebcastSocialMessage::decode(p)?;
            let key = display_key(x.common.as_ref());
            let a = actor(x.user.as_ref(), None);
            let id = message_id(m, common_id(&x.common));
            if key.contains("follow") {
                Some((id, Item::Follow { actor: with_role(a, Role::Follower) }))
            } else if key.contains("share") {
                Some((id, Item::Share { actor: a }))
            } else {
                None
            }
        }
        "WebcastSubNotifyMessage" => {
            let x = WebcastSubNotifyMessage::decode(p)?;
            let a = with_role(actor(x.user.as_ref(), None), Role::Sub);
            Some((message_id(m, common_id(&x.common)), Item::Sub { actor: a, months: x.sub_month.max(1) }))
        }
        "WebcastRoomUserSeqMessage" => {
            let x = WebcastRoomUserSeqMessage::decode(p)?;
            Some((0, Item::Viewers(x.total.max(0) as u64)))
        }
        "WebcastControlMessage" => {
            let x = WebcastControlMessage::decode(p)?;
            Some((0, Item::Control(x.action)))
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: i64, nick: &str) -> User {
        User { id, nickname: nick.into(), display_id: format!("h{id}"), ..Default::default() }
    }

    fn base<M: Message>(method: &str, m: &M) -> BaseProtoMessage {
        BaseProtoMessage { method: method.into(), payload: m.encode_to_vec(), msg_id: 1, ..Default::default() }
    }

    #[test]
    fn roles_from_identity_attr_and_follow_info() {
        let mut u = user(1, "a");
        assert_eq!(actor(Some(&u), None).roles, vec![Role::Everyone]);
        u.follow_info = Some(FollowInfo { follow_status: 1, ..Default::default() });
        u.user_attr = Some(UserAttr { is_admin: true, ..Default::default() });
        let ident = UserIdentity { is_subscriber_of_anchor: true, ..Default::default() };
        let a = actor(Some(&u), Some(&ident));
        assert_eq!(a.roles, vec![Role::Follower, Role::Sub, Role::Mod]);
        assert_eq!(a.top_role(), Role::Mod);
        let ident = UserIdentity { is_moderator_of_anchor: true, is_mutual_following_with_anchor: true, ..Default::default() };
        assert_eq!(actor(Some(&user(2, "b")), Some(&ident)).roles, vec![Role::Follower, Role::Mod]);
    }

    #[test]
    fn names_fall_back_to_handle_then_id_and_are_sanitized() {
        assert_eq!(actor(Some(&user(5, "  ")), None).name, "h5");
        let u = User { id: 9, ..Default::default() };
        assert_eq!(actor(Some(&u), None).name, "9");
        assert_eq!(actor(Some(&user(5, "a\u{0007}b")), None).name, "a b");
    }

    #[test]
    fn chat_is_capped_to_500_chars() {
        let long: String = "é".repeat(800);
        let m = base("WebcastChatMessage", &WebcastChatMessage { user: Some(user(1, "x")), content: long, ..Default::default() });
        let Some((_, Item::Chat { text, .. })) = decode(&m).unwrap() else { panic!() };
        assert_eq!(text.chars().count(), MAX_CHAT_CHARS);
    }

    #[test]
    fn social_splits_follow_and_share_by_display_key() {
        let mk = |key: &str| {
            let common = CommonMessageData { display_text: Some(Text { key: key.into(), ..Default::default() }), ..Default::default() };
            base("WebcastSocialMessage", &WebcastSocialMessage { common: Some(common), user: Some(user(3, "c")), ..Default::default() })
        };
        let Some((_, Item::Follow { actor })) = decode(&mk("pm_main_follow_message_viewer_2")).unwrap() else { panic!() };
        assert!(actor.roles.contains(&Role::Follower) && !actor.roles.contains(&Role::Everyone));
        assert!(matches!(decode(&mk("pm_mt_guidance_share")).unwrap(), Some((_, Item::Share { .. }))));
        assert_eq!(decode(&mk("pm_mt_something_else")).unwrap(), None);
    }

    #[test]
    fn only_joins_count_as_member_events() {
        let join = base("WebcastMemberMessage", &WebcastMemberMessage { user: Some(user(4, "d")), action: 1, ..Default::default() });
        assert!(matches!(decode(&join).unwrap(), Some((_, Item::Join { .. }))));
        let other = base("WebcastMemberMessage", &WebcastMemberMessage { user: Some(user(4, "d")), action: 3, ..Default::default() });
        assert_eq!(decode(&other).unwrap(), None);
    }

    #[test]
    fn unknown_methods_are_ignored_and_garbage_is_an_error() {
        let m = BaseProtoMessage { method: "WebcastLinkMicBattle".into(), payload: vec![0xff; 8], ..Default::default() };
        assert_eq!(decode(&m).unwrap(), None);
        let m = BaseProtoMessage { method: "WebcastChatMessage".into(), payload: vec![0x0a, 0xff, 0xff], ..Default::default() };
        assert!(decode(&m).is_err());
    }
}
