//! TikTok LIVE events (PLAN §11, M11) — **best-effort and unofficial**.
//!
//! TikTok has no public LIVE events API. This client speaks the reverse-engineered webcast
//! protocol the community libraries use (zerodytrash/TikTok-Live-Connector,
//! isaackogan/TikTokLive): resolve `@unique_id` → room id on TikTok web, ask an
//! Euler-Stream-compatible sign provider for a signed WebSocket URL + cookies, then decode
//! gzip'd protobuf push frames (acks + heartbeats, reconnect with capped exponential backoff and
//! jitter, slow polling while offline). Expect breakage when TikTok changes things (§27): the
//! client is isolated — its own supervised task (a panic is logged and restarted, never
//! propagated), no locks shared with other subsystems, bounded event rates — and it is
//! **disabled by default**: without `[tiktok] enabled = true` (or `tiktok.connect`) only a
//! config/action watcher runs and nothing touches the network.
//!
//! * Config: `[tiktok] enabled, unique_id, sign_url, poll_offline` (see [`config`]); API key =
//!   keyring secret `tiktok.sign_api_key`.
//! * Events (origin `chat`, actor `{platform: "tiktok", id, name, roles}`; every payload also
//!   has `user` and `user_id`): `tiktok.chat {message (≤ 500 chars), message_id}`,
//!   `tiktok.gift {gift, gift_id, count, diamond_count, diamonds = diamond_count × count, streak, to_user?}`
//!   (once per finished streak), `tiktok.like {count, total, likers}` (≤ 1/s, attributed to the
//!   top liker), `tiktok.follow`, `tiktok.share`, `tiktok.join`, `tiktok.sub {months}`.
//! * Signal: `tiktok.viewers`. State (readonly): `tiktok.connected`, `tiktok.room_id`,
//!   `tiktok.status` (`disabled|resolving|connecting|connected|offline|backoff|failed`).
//!   Preflight: `health.tiktok`.
//! * Actions: `tiktok.connect [unique_id]` / `tiktok.disconnect` (session override of
//!   `enabled`), `tiktok.key.set <key>` (stores the sign key; empty removes it).
//! * Query `tiktok` → `{enabled, manual, connected, state, room_id, unique_id, detail,
//!   last_error, viewers, sign_auth, counts}`.

mod backoff;
#[cfg(feature = "client")]
pub mod client;
pub mod config;
mod controller;
pub mod frame;
pub mod normalize;
pub mod process;
pub mod proto;
pub mod session;
pub mod transport;

use controller::ClientFactory;

/// Start the TikTok subsystem: declares its state, registers the `tiktok` query and action
/// router, and spawns the config watcher. Returns immediately; never fails the engine.
pub async fn start(ctx: se_hub::EngineCtx) -> anyhow::Result<()> {
    controller::start(ctx, default_factory(), controller::Tuning::default());
    Ok(())
}

#[cfg(feature = "client")]
fn default_factory() -> Option<ClientFactory> {
    use std::sync::Arc;
    let f: ClientFactory = Arc::new(|settings: config::Settings, out: session::Out, mut stop| {
        Box::pin(async move {
            match client::WebTransport::new(&settings.sign_url) {
                Ok(t) => session::run(t, settings, out, stop).await,
                Err(e) => {
                    out.error("error", format!("TikTok client setup failed: {e}"));
                    out.set_state(session::LinkState::Failed, "", format!("client setup failed: {e}"));
                    session::wait_stop(&mut stop).await;
                }
            }
        })
    });
    Some(f)
}

#[cfg(not(feature = "client"))]
fn default_factory() -> Option<ClientFactory> {
    None
}

#[cfg(test)]
mod fixture_tests {
    //! Frames encoded by `protoc` from the upstream schema (`fixtures/gen_fixtures.py`),
    //! decoded by our hand-written types and normalized.

    use crate::frame;
    use crate::process::{Output, Processor};
    use se_proto::{Event, Origin, Role, Value};
    use std::time::{Duration, Instant};

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn s<'a>(e: &'a Event, k: &str) -> &'a str {
        e.payload.get_path(k).and_then(Value::as_str).unwrap_or_default()
    }

    fn i(e: &Event, k: &str) -> i64 {
        e.payload.get_path(k).and_then(Value::as_i64).unwrap_or(-1)
    }

    #[test]
    fn upstream_encoded_frame_normalizes_to_engine_events() {
        let f = frame::decode_push_frame(&fixture("push_frame_events.bin")).unwrap();
        assert_eq!((f.payload_type.as_str(), f.log_id), ("msg", 7_400_000_000_000_000_123));
        let batch = frame::decode_fetch_result(&f).unwrap();
        assert!(batch.need_ack);
        assert_eq!(batch.messages.len(), 14);
        let mut p = Processor::new();
        let t = Instant::now();
        let mut out = p.handle(&batch, true, t);
        out.extend(p.tick(t + Duration::from_secs(1)));
        let mut events = Vec::new();
        let mut viewers = Vec::new();
        for o in out {
            match o {
                Output::Event(_, e) => events.push(e),
                Output::Viewers(n) => viewers.push(n),
                Output::DecodeError(m) => panic!("fixture message failed to decode: {m}"),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(viewers, vec![1234]);
        let ty: Vec<&str> = events.iter().map(|e| e.ty.as_str()).collect();
        assert_eq!(ty, ["tiktok.chat", "tiktok.gift", "tiktok.follow", "tiktok.share", "tiktok.join", "tiktok.sub", "tiktok.gift", "tiktok.like"]);
        assert!(events.iter().all(|e| e.origin == Origin::Chat && e.actor.as_ref().is_some_and(|a| a.platform == "tiktok")));

        let chat = &events[0];
        let a = chat.actor.as_ref().unwrap();
        assert_eq!((a.id.as_str(), a.name.as_str()), ("6800000000000000001", "Drum Fan 🥁"));
        assert_eq!(a.roles, vec![Role::Follower, Role::Sub]);
        assert_eq!((s(chat, "message"), s(chat, "message_id"), s(chat, "user")), ("hello from tiktok", "7400000000000000001", "Drum Fan 🥁"));
        assert_eq!(s(chat, "user_id"), "6800000000000000001");

        let lion = &events[1];
        assert_eq!((s(lion, "gift"), i(lion, "gift_id"), i(lion, "count"), i(lion, "diamonds")), ("Lion", 6369, 1, 29999));
        assert_eq!(lion.actor.as_ref().unwrap().roles, vec![Role::Mod]);

        let follow = events[2].actor.as_ref().unwrap();
        assert_eq!((follow.name.as_str(), follow.roles.as_slice()), ("Snare Queen", [Role::Follower, Role::Mod].as_slice()));
        assert_eq!(s(&events[3], "user"), "Drum Fan 🥁");
        assert_eq!(s(&events[4], "user"), "Snare Queen");
        assert_eq!(i(&events[5], "months"), 3);
        assert!(events[5].actor.as_ref().unwrap().roles.contains(&Role::Sub));

        let rose = &events[6];
        assert_eq!((s(rose, "gift"), i(rose, "count"), i(rose, "diamonds"), rose.payload.get_path("streak")), ("Rose", 5, 5, Some(&Value::Bool(true))));

        let like = &events[7];
        assert_eq!((i(like, "count"), i(like, "total"), i(like, "likers")), (25, 12010, 2));
        assert_eq!(s(like, "user"), "Drum Fan 🥁");
        assert_eq!(p.handle(&batch, true, t).iter().filter(|o| matches!(o, Output::Event(..))).count(), 0, "re-delivery is deduplicated");
    }

    #[test]
    fn upstream_encoded_stream_end_and_sign_response() {
        let f = frame::decode_push_frame(&fixture("push_frame_end.bin")).unwrap();
        let batch = frame::decode_fetch_result(&f).unwrap();
        assert!(!batch.need_ack);
        assert_eq!(Processor::new().handle(&batch, true, Instant::now()), vec![Output::StreamEnded]);

        use prost::Message;
        let sign = crate::proto::ProtoMessageFetchResult::decode(fixture("sign_response.bin").as_slice()).unwrap();
        assert_eq!(sign.push_server, "wss://webcast-ws.tiktok.com/webcast/im/ws_proxy/ws_reuse_supplement/");
        assert_eq!(sign.route_params.get("wrss").map(String::as_str), Some("Zm9vYmFy"));
        assert_eq!((sign.cursor.as_str(), sign.fetch_interval, sign.is_first), ("t-1758835200000_r-1", 1000, true));
        assert_eq!(Processor::new().handle(&sign, false, Instant::now()), vec![Output::Viewers(99)]);
    }
}
