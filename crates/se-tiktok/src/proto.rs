//! The subset of TikTok's Webcast protobuf schema this client needs, defined by hand.
//!
//! Field numbers and types follow the community-maintained schema
//! `isaackogan/TikTok-Webcast-Protobuf`, `src/slim/v3/webcast/**` at commit `cf7bcd49`
//! (2026-07-22) — the schema behind `tiktok-live-proto` 0.2.4 (npm, used by
//! zerodytrash/TikTok-Live-Connector @ `8a923300`, 2026-09-22) and `TikTokLiveProto` 0.2.2
//! (PyPI, used by isaackogan/TikTokLive 7.0.1 @ `fc73b8f6`, 2026-09-09). Checked 2026-09-25.
//!
//! Only the fields we read are declared; prost skips every other field on the wire, so
//! additions on TikTok's side never break decoding. Enums are declared as `int32` (same wire
//! format) so unknown enum values decode instead of failing. Encoders are used for the three
//! frames we send (enter-room, heartbeat, ack) and for test fixtures.

use std::collections::BTreeMap;

// ---- transport envelope (webcast/synthetic_proto.proto, webcast/shared/message.proto) ----

/// Outermost WebSocket frame, both directions.
#[derive(Clone, PartialEq, prost::Message)]
pub struct WebcastPushFrame {
    #[prost(int64, tag = "1")]
    pub seq_id: i64,
    #[prost(int64, tag = "2")]
    pub log_id: i64,
    #[prost(int64, tag = "3")]
    pub service: i64,
    #[prost(int64, tag = "4")]
    pub method: i64,
    #[prost(message, repeated, tag = "5")]
    pub headers: Vec<PushHeader>,
    #[prost(string, tag = "6")]
    pub payload_encoding: String,
    /// `msg` (a [`ProtoMessageFetchResult`]), `hb`, `ack`, `im_enter_room`, `im_enter_room_resp`.
    #[prost(string, tag = "7")]
    pub payload_type: String,
    #[prost(bytes = "vec", tag = "8")]
    pub payload: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PushHeader {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

/// Client → server keep-alive (payload of a `hb` frame).
#[derive(Clone, PartialEq, prost::Message)]
pub struct HeartBeatMessage {
    #[prost(int64, tag = "1")]
    pub room_id: i64,
    #[prost(int64, tag = "2")]
    pub send_packet_seq_id: i64,
}

/// Client → server room join (payload of an `im_enter_room` frame).
#[derive(Clone, PartialEq, prost::Message)]
pub struct WebcastImEnterRoomMessage {
    #[prost(int64, tag = "1")]
    pub room_id: i64,
    #[prost(string, tag = "2")]
    pub room_tag: String,
    #[prost(string, tag = "3")]
    pub live_region: String,
    #[prost(int64, tag = "4")]
    pub live_id: i64,
    #[prost(string, tag = "5")]
    pub identity: String,
    #[prost(string, tag = "6")]
    pub cursor: String,
    #[prost(int64, tag = "7")]
    pub account_type: i64,
    #[prost(int64, tag = "8")]
    pub enter_unique_id: i64,
    #[prost(string, tag = "9")]
    pub filter_welcome_msg: String,
    #[prost(bool, tag = "10")]
    pub is_anchor_continue_keep_msg: bool,
}

/// One message inside a fetch result: `method` names the payload type (`WebcastChatMessage`, …).
#[derive(Clone, PartialEq, prost::Message)]
pub struct BaseProtoMessage {
    #[prost(string, tag = "1")]
    pub method: String,
    #[prost(bytes = "vec", tag = "2")]
    pub payload: Vec<u8>,
    #[prost(int64, tag = "3")]
    pub msg_id: i64,
    #[prost(int32, tag = "4")]
    pub msg_type: i32,
    #[prost(int64, tag = "5")]
    pub offset: i64,
    #[prost(bool, tag = "6")]
    pub is_history: bool,
}

/// Batch of messages: the body of a `msg` push frame and of the sign server's response.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ProtoMessageFetchResult {
    #[prost(message, repeated, tag = "1")]
    pub messages: Vec<BaseProtoMessage>,
    #[prost(string, tag = "2")]
    pub cursor: String,
    #[prost(int64, tag = "3")]
    pub fetch_interval: i64,
    #[prost(int64, tag = "4")]
    pub now: i64,
    #[prost(string, tag = "5")]
    pub internal_ext: String,
    #[prost(int32, tag = "6")]
    pub fetch_type: i32,
    #[prost(btree_map = "string, string", tag = "7")]
    pub route_params: BTreeMap<String, String>,
    #[prost(int64, tag = "8")]
    pub heartbeat_duration: i64,
    #[prost(bool, tag = "9")]
    pub need_ack: bool,
    #[prost(string, tag = "10")]
    pub push_server: String,
    #[prost(bool, tag = "11")]
    pub is_first: bool,
}

// ---- shared parts ----

/// `webcast.model.message.common.Text` (only the template key/pattern).
#[derive(Clone, PartialEq, prost::Message)]
pub struct Text {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(string, tag = "2")]
    pub default_pattern: String,
}

/// `webcast.shared.message.CommonMessageData` (field 1 of every event message).
#[derive(Clone, PartialEq, prost::Message)]
pub struct CommonMessageData {
    #[prost(string, tag = "1")]
    pub method: String,
    #[prost(int64, tag = "2")]
    pub msg_id: i64,
    #[prost(int64, tag = "3")]
    pub room_id: i64,
    #[prost(int64, tag = "4")]
    pub create_time: i64,
    #[prost(message, optional, tag = "8")]
    pub display_text: Option<Text>,
}

/// `webcast.model.base.user.User` (identity + role hints only).
#[derive(Clone, PartialEq, prost::Message)]
pub struct User {
    #[prost(int64, tag = "1")]
    pub id: i64,
    #[prost(string, tag = "3")]
    pub nickname: String,
    #[prost(message, optional, tag = "22")]
    pub follow_info: Option<FollowInfo>,
    #[prost(message, optional, tag = "32")]
    pub user_attr: Option<UserAttr>,
    /// The `@handle` (called `unique_id` by older schemas).
    #[prost(string, tag = "38")]
    pub display_id: String,
    #[prost(string, tag = "46")]
    pub sec_uid: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct FollowInfo {
    #[prost(int64, tag = "1")]
    pub following_count: i64,
    #[prost(int64, tag = "2")]
    pub follower_count: i64,
    /// 0 = not following, 1 = following, 2 = mutual ("friends").
    #[prost(int64, tag = "3")]
    pub follow_status: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct UserAttr {
    #[prost(bool, tag = "1")]
    pub is_muted: bool,
    #[prost(bool, tag = "2")]
    pub is_admin: bool,
    #[prost(bool, tag = "3")]
    pub is_super_admin: bool,
}

/// `webcast.model.data.UserIdentity`: the sender's relation to the streamer.
#[derive(Clone, PartialEq, prost::Message)]
pub struct UserIdentity {
    #[prost(bool, tag = "1")]
    pub is_gift_giver_of_anchor: bool,
    #[prost(bool, tag = "2")]
    pub is_subscriber_of_anchor: bool,
    #[prost(bool, tag = "3")]
    pub is_mutual_following_with_anchor: bool,
    #[prost(bool, tag = "4")]
    pub is_follower_of_anchor: bool,
    #[prost(bool, tag = "5")]
    pub is_moderator_of_anchor: bool,
    #[prost(bool, tag = "6")]
    pub is_anchor: bool,
}

/// `webcast.model.Gift` (the gift definition embedded in a gift message).
#[derive(Clone, PartialEq, prost::Message)]
pub struct Gift {
    #[prost(int64, tag = "5")]
    pub id: i64,
    #[prost(bool, tag = "10")]
    pub combo: bool,
    /// 1 = streakable (sent repeatedly; the final message carries `repeat_end`).
    #[prost(int32, tag = "11")]
    pub r#type: i32,
    #[prost(int32, tag = "12")]
    pub diamond_count: i32,
    #[prost(string, tag = "16")]
    pub name: String,
}

// ---- event messages (webcast/model/message/messages.proto) ----

#[derive(Clone, PartialEq, prost::Message)]
pub struct WebcastChatMessage {
    #[prost(message, optional, tag = "1")]
    pub common: Option<CommonMessageData>,
    #[prost(message, optional, tag = "2")]
    pub user: Option<User>,
    #[prost(string, tag = "3")]
    pub content: String,
    #[prost(string, tag = "14")]
    pub content_language: String,
    #[prost(message, optional, tag = "18")]
    pub user_identity: Option<UserIdentity>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WebcastGiftMessage {
    #[prost(message, optional, tag = "1")]
    pub common: Option<CommonMessageData>,
    #[prost(int64, tag = "2")]
    pub gift_id: i64,
    #[prost(int32, tag = "4")]
    pub group_count: i32,
    #[prost(int32, tag = "5")]
    pub repeat_count: i32,
    #[prost(int32, tag = "6")]
    pub combo_count: i32,
    #[prost(message, optional, tag = "7")]
    pub user: Option<User>,
    #[prost(message, optional, tag = "8")]
    pub to_user: Option<User>,
    #[prost(int32, tag = "9")]
    pub repeat_end: i32,
    #[prost(int64, tag = "11")]
    pub group_id: i64,
    #[prost(message, optional, tag = "15")]
    pub gift: Option<Gift>,
    #[prost(string, tag = "16")]
    pub log_id: String,
    #[prost(message, optional, tag = "32")]
    pub user_identity: Option<UserIdentity>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WebcastLikeMessage {
    #[prost(message, optional, tag = "1")]
    pub common: Option<CommonMessageData>,
    #[prost(int32, tag = "2")]
    pub count: i32,
    #[prost(int64, tag = "3")]
    pub total: i64,
    #[prost(message, optional, tag = "5")]
    pub user: Option<User>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WebcastMemberMessage {
    #[prost(message, optional, tag = "1")]
    pub common: Option<CommonMessageData>,
    #[prost(message, optional, tag = "2")]
    pub user: Option<User>,
    #[prost(int32, tag = "3")]
    pub member_count: i32,
    /// `MemberMessageAction`: 1 = joined, 3 = subscribed.
    #[prost(int32, tag = "10")]
    pub action: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WebcastSocialMessage {
    #[prost(message, optional, tag = "1")]
    pub common: Option<CommonMessageData>,
    #[prost(message, optional, tag = "2")]
    pub user: Option<User>,
    #[prost(int64, tag = "3")]
    pub share_type: i64,
    #[prost(int64, tag = "4")]
    pub action: i64,
    #[prost(int64, tag = "6")]
    pub follow_count: i64,
    #[prost(int32, tag = "8")]
    pub share_count: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WebcastRoomUserSeqMessage {
    #[prost(message, optional, tag = "1")]
    pub common: Option<CommonMessageData>,
    /// Current viewer count.
    #[prost(int64, tag = "3")]
    pub total: i64,
    #[prost(int64, tag = "6")]
    pub popularity: i64,
    /// Cumulative viewers.
    #[prost(int64, tag = "7")]
    pub total_user: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WebcastControlMessage {
    #[prost(message, optional, tag = "1")]
    pub common: Option<CommonMessageData>,
    /// `ControlAction`: 1 paused, 2 unpaused, 3 ended, 4 suspended.
    #[prost(int32, tag = "2")]
    pub action: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WebcastSubNotifyMessage {
    #[prost(message, optional, tag = "1")]
    pub common: Option<CommonMessageData>,
    #[prost(message, optional, tag = "2")]
    pub user: Option<User>,
    #[prost(int64, tag = "4")]
    pub sub_month: i64,
}

pub const CONTROL_PAUSED: i32 = 1;
pub const CONTROL_UNPAUSED: i32 = 2;
pub const CONTROL_ENDED: i32 = 3;
pub const CONTROL_SUSPENDED: i32 = 4;
pub const MEMBER_JOINED: i32 = 1;
pub const GIFT_TYPE_STREAKABLE: i32 = 1;
