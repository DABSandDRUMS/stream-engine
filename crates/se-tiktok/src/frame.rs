//! WebSocket frame codec: `WebcastPushFrame` in/out, gzip payloads, and the three frames the
//! client sends (enter-room, heartbeat, ack) — mirroring TikTok-Live-Connector's
//! `ws-client.ts`/`proto-utils.ts` and TikTokLive's `ws_client.py`/`ws_utils.py`.

use crate::proto::{HeartBeatMessage, ProtoMessageFetchResult, PushHeader, WebcastImEnterRoomMessage, WebcastPushFrame};
use prost::Message;
use std::io::Read;

/// Decompressed payloads larger than this are rejected (a normal batch is a few KiB).
pub const MAX_PAYLOAD: usize = 8 << 20;

#[derive(Debug)]
pub enum FrameError {
    Decode(prost::DecodeError),
    Gzip(std::io::Error),
    TooLarge,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Decode(e) => write!(f, "protobuf: {e}"),
            FrameError::Gzip(e) => write!(f, "gzip: {e}"),
            FrameError::TooLarge => write!(f, "payload larger than {MAX_PAYLOAD} bytes"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<prost::DecodeError> for FrameError {
    fn from(e: prost::DecodeError) -> Self {
        FrameError::Decode(e)
    }
}

pub fn decode_push_frame(bytes: &[u8]) -> Result<WebcastPushFrame, FrameError> {
    Ok(WebcastPushFrame::decode(bytes)?)
}

fn header<'a>(frame: &'a WebcastPushFrame, key: &str) -> Option<&'a str> {
    frame.headers.iter().find(|h| h.key == key).map(|h| h.value.as_str())
}

fn is_gzip(b: &[u8]) -> bool {
    b.len() > 2 && b[0] == 0x1f && b[1] == 0x8b && b[2] == 0x08
}

fn gunzip(b: &[u8]) -> Result<Vec<u8>, FrameError> {
    let mut out = Vec::with_capacity(b.len() * 4);
    flate2::read::GzDecoder::new(b).take(MAX_PAYLOAD as u64 + 1).read_to_end(&mut out).map_err(FrameError::Gzip)?;
    if out.len() > MAX_PAYLOAD {
        return Err(FrameError::TooLarge);
    }
    Ok(out)
}

/// Decode the `ProtoMessageFetchResult` carried by a `msg` frame. The payload is gzip when
/// the frame says `compress_type: gzip` (TikTokLive) or starts with the gzip magic
/// (TikTok-Live-Connector); both checks are applied.
pub fn decode_fetch_result(frame: &WebcastPushFrame) -> Result<ProtoMessageFetchResult, FrameError> {
    let gz = header(frame, "compress_type") == Some("gzip") || is_gzip(&frame.payload);
    if gz {
        let raw = gunzip(&frame.payload)?;
        Ok(ProtoMessageFetchResult::decode(raw.as_slice())?)
    } else {
        Ok(ProtoMessageFetchResult::decode(frame.payload.as_slice())?)
    }
}

fn frame(payload_type: &str, log_id: i64, payload: Vec<u8>) -> Vec<u8> {
    WebcastPushFrame { log_id, payload_encoding: "pb".into(), payload_type: payload_type.into(), payload, ..Default::default() }.encode_to_vec()
}

/// Join the room after the socket opens (`live_id` 12 and `identity` audience are the values
/// every web client sends; `enter_unique_id` is a random positive id the server echoes).
pub fn enter_room(room_id: i64, enter_unique_id: i64) -> Vec<u8> {
    let msg =
        WebcastImEnterRoomMessage { room_id, live_id: 12, identity: "audience".into(), enter_unique_id, filter_welcome_msg: "0".into(), ..Default::default() };
    frame("im_enter_room", 0, msg.encode_to_vec())
}

pub fn heartbeat(room_id: i64, seq: i64) -> Vec<u8> {
    frame("hb", 0, HeartBeatMessage { room_id, send_packet_seq_id: seq }.encode_to_vec())
}

/// Acknowledge a `msg` frame that asked for it: the frame's `log_id` plus the batch's
/// `internal_ext` as raw bytes (`-` when empty, as TikTokLive does).
pub fn ack(log_id: i64, internal_ext: &str) -> Vec<u8> {
    let body = if internal_ext.is_empty() { b"-".to_vec() } else { internal_ext.as_bytes().to_vec() };
    frame("ack", log_id, body)
}

/// Build a `msg` frame (tests and fixtures): gzip-compressed with the `compress_type` header.
pub fn msg_frame(log_id: i64, result: &ProtoMessageFetchResult, gzip: bool) -> Vec<u8> {
    let raw = result.encode_to_vec();
    let (payload, headers) = if gzip {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        // writing into a Vec cannot fail
        let _ = enc.write_all(&raw);
        let gz = enc.finish().unwrap_or_default();
        (gz, vec![PushHeader { key: "compress_type".into(), value: "gzip".into() }])
    } else {
        (raw, Vec::new())
    };
    WebcastPushFrame { log_id, headers, payload_encoding: "pb".into(), payload_type: "msg".into(), payload, ..Default::default() }.encode_to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::BaseProtoMessage;

    fn batch() -> ProtoMessageFetchResult {
        ProtoMessageFetchResult {
            messages: vec![BaseProtoMessage { method: "WebcastChatMessage".into(), payload: vec![1, 2, 3], msg_id: 7, ..Default::default() }],
            cursor: "c1".into(),
            internal_ext: "ext".into(),
            need_ack: true,
            ..Default::default()
        }
    }

    #[test]
    fn gzip_and_plain_payloads_decode() {
        for gz in [true, false] {
            let bytes = msg_frame(42, &batch(), gz);
            let f = decode_push_frame(&bytes).unwrap();
            assert_eq!(f.log_id, 42);
            assert_eq!(decode_fetch_result(&f).unwrap(), batch());
        }
    }

    #[test]
    fn gzip_detected_by_magic_without_header() {
        let mut f = decode_push_frame(&msg_frame(1, &batch(), true)).unwrap();
        f.headers.clear();
        assert_eq!(decode_fetch_result(&f).unwrap(), batch());
    }

    #[test]
    fn corrupt_gzip_is_an_error_not_a_panic() {
        let f = WebcastPushFrame {
            payload_type: "msg".into(),
            headers: vec![PushHeader { key: "compress_type".into(), value: "gzip".into() }],
            payload: vec![0x1f, 0x8b, 0x08, 0, 0, 0],
            ..Default::default()
        };
        assert!(matches!(decode_fetch_result(&f), Err(FrameError::Gzip(_))));
    }

    #[test]
    fn oversized_payload_is_rejected() {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(&vec![0u8; MAX_PAYLOAD + 10]).unwrap();
        let f = WebcastPushFrame { payload: enc.finish().unwrap(), ..Default::default() };
        assert!(matches!(decode_fetch_result(&f), Err(FrameError::TooLarge)));
    }

    #[test]
    fn outgoing_frames_carry_room_log_id_and_ext() {
        let f = decode_push_frame(&ack(99, "")).unwrap();
        assert_eq!((f.payload_type.as_str(), f.log_id, f.payload.as_slice()), ("ack", 99, b"-".as_slice()));
        let f = decode_push_frame(&ack(5, "abc")).unwrap();
        assert_eq!(f.payload, b"abc");
        let f = decode_push_frame(&heartbeat(7_140_000_000_000_000_001, 3)).unwrap();
        assert_eq!(f.payload_type, "hb");
        let hb = HeartBeatMessage::decode(f.payload.as_slice()).unwrap();
        assert_eq!((hb.room_id, hb.send_packet_seq_id), (7_140_000_000_000_000_001, 3));
        let f = decode_push_frame(&enter_room(11, 22)).unwrap();
        assert_eq!(f.payload_type, "im_enter_room");
        let m = WebcastImEnterRoomMessage::decode(f.payload.as_slice()).unwrap();
        assert_eq!((m.room_id, m.live_id, m.identity.as_str(), m.enter_unique_id), (11, 12, "audience", 22));
    }
}
