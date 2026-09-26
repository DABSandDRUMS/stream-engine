//! Message bodies: decoding what the console sends (`packetParser/*.ts`) and building what we
//! send (`Client.ts`, `util/subscriptionUtil.ts`, `util/KeepAliveHelper.ts`).

use super::packet::{self, CBYTES, Code, Frame, PacketError};
use super::tree::ConsoleState;
use super::ubjson;
use std::io::Read;
use thiserror::Error;

/// Upper bound for an inflated state payload (a 16R's is ~200 KiB).
const MAX_STATE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum MsgError {
    #[error("{0}: truncated body")]
    Truncated(&'static str),
    #[error("{0}: missing NUL after the parameter name")]
    NoName(&'static str),
    #[error("chunk out of order (offset {offset}, have {have})")]
    ChunkOrder { offset: usize, have: usize },
    #[error("state payload: {0}")]
    State(String),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Packet(#[from] PacketError),
}

/// One fader group of an `MS`/`fdrs` packet: positions 0–1 of consecutive channels.
#[derive(Debug, Clone, PartialEq)]
pub struct FaderGroup {
    pub group: u16,
    pub values: Vec<f64>,
}

impl FaderGroup {
    /// Protocol group name for a fader group id (`MS.ts` mapping).
    pub fn group_name(&self) -> Option<&'static str> {
        Some(match self.group {
            0 => "line",
            1 => "return",
            2 => "fxreturn",
            3 => "talkback",
            4 => "aux",
            5 => "fxbus",
            6 => "sub",
            7 => "main",
            8 => "mono",
            11 => "master",
            _ => return None,
        })
    }
}

#[derive(Debug)]
pub enum Incoming {
    /// `PV`: numeric parameter (floats; booleans are 0.0/1.0).
    Param { path: String, value: f32 },
    /// `PS`: string parameter (names).
    Text { path: String, value: String },
    /// `MS` `fdrs`: fader positions of every strip.
    Faders(Vec<FaderGroup>),
    /// `JM`: JSON message (`SubscriptionReply`, `UserLoggedIn`, …).
    Json(serde_json::Value),
    /// `ZB` or the last `CK` chunk: complete console state.
    State(Box<ConsoleState>),
    /// `FD`: file data / keep-alive reply, by request id.
    FileData { id: u16 },
    /// Understood but not needed (`BO`, `PL`, `PC`, partial `CK`, …).
    Ignored(Code),
}

/// Stateful decoder (chunked state payloads span several `CK` packets).
#[derive(Default)]
pub struct Decoder {
    chunk: Vec<u8>,
}

fn split_name<'a>(what: &'static str, body: &'a [u8]) -> Result<(String, &'a [u8]), MsgError> {
    let nul = body.iter().position(|b| *b == 0).ok_or(MsgError::NoName(what))?;
    let name = String::from_utf8_lossy(&body[..nul]).into_owned();
    // name NUL, then two bytes (usually 00 00; filter groups use 00 01), then the value
    let rest = body.get(nul + 3..).ok_or(MsgError::Truncated(what))?;
    Ok((name, rest))
}

fn inflate_state(z: &[u8]) -> Result<ConsoleState, MsgError> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(z).take(MAX_STATE_BYTES).read_to_end(&mut out).map_err(|e| MsgError::State(format!("inflate: {e}")))?;
    let doc = ubjson::decode(&out).map_err(|e| MsgError::State(e.to_string()))?;
    ConsoleState::from_sync(&doc).map_err(MsgError::State)
}

fn u16le(b: &[u8], at: usize) -> Option<u16> {
    b.get(at..at + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn u32le(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// `MS` `fdrs` body: "fdrs", u16, u16 count, count × u16 positions (÷65535), u8 group count,
/// groups of (u16 id, u16 offset, u16 count), all little-endian.
pub fn parse_faders(body: &[u8]) -> Result<Vec<FaderGroup>, MsgError> {
    const W: &str = "MS fdrs";
    if body.get(..4) != Some(b"fdrs") {
        return Err(MsgError::Truncated(W));
    }
    let n = u16le(body, 6).ok_or(MsgError::Truncated(W))? as usize;
    let vals_at = 8;
    let groups_at = vals_at + n * 2;
    let raw: Vec<u16> = (0..n).map(|i| u16le(body, vals_at + i * 2)).collect::<Option<_>>().ok_or(MsgError::Truncated(W))?;
    let gcount = *body.get(groups_at).ok_or(MsgError::Truncated(W))? as usize;
    let mut out = Vec::with_capacity(gcount);
    for g in 0..gcount {
        let at = groups_at + 1 + g * 6;
        let (id, off, cnt) = (u16le(body, at), u16le(body, at + 2), u16le(body, at + 4));
        let (Some(id), Some(off), Some(cnt)) = (id, off, cnt) else { return Err(MsgError::Truncated(W)) };
        let vals = raw.get(off as usize..off as usize + cnt as usize).ok_or(MsgError::Truncated(W))?;
        out.push(FaderGroup { group: id, values: vals.iter().map(|v| *v as f64 / 65535.0).collect() });
    }
    Ok(out)
}

impl Decoder {
    pub fn decode(&mut self, f: Frame<'_>) -> Result<Incoming, MsgError> {
        let b = f.body;
        Ok(match f.code {
            Code::PARAM_VALUE => {
                let (path, v) = split_name("PV", b)?;
                let bytes: [u8; 4] = v.get(..4).and_then(|s| s.try_into().ok()).ok_or(MsgError::Truncated("PV"))?;
                Incoming::Param { path, value: f32::from_le_bytes(bytes) }
            }
            Code::PARAM_STRING => {
                let (path, v) = split_name("PS", b)?;
                let end = v.iter().position(|c| *c == 0).unwrap_or(v.len());
                Incoming::Text { path, value: String::from_utf8_lossy(&v[..end]).into_owned() }
            }
            Code::METER16 if b.starts_with(b"fdrs") => Incoming::Faders(parse_faders(b)?),
            Code::JSON => {
                let json = b.get(4..).ok_or(MsgError::Truncated("JM"))?;
                let end = json.iter().rposition(|c| *c != 0).map(|i| i + 1).unwrap_or(0);
                Incoming::Json(serde_json::from_slice(&json[..end])?)
            }
            Code::ZLIB => {
                let z = b.get(4..).ok_or(MsgError::Truncated("ZB"))?;
                Incoming::State(Box::new(inflate_state(z)?))
            }
            Code::CHUNK => {
                let (Some(offset), Some(total), Some(size)) = (u32le(b, 4), u32le(b, 8), u32le(b, 12)) else {
                    return Err(MsgError::Truncated("CK"));
                };
                let (offset, total, size) = (offset as usize, total as usize, size as usize);
                let data = b.get(16..16 + size).ok_or(MsgError::Truncated("CK"))?;
                if offset == 0 {
                    self.chunk.clear();
                }
                if offset != self.chunk.len() {
                    let have = self.chunk.len();
                    self.chunk.clear();
                    return Err(MsgError::ChunkOrder { offset, have });
                }
                self.chunk.extend_from_slice(data);
                if offset + size < total {
                    return Ok(Incoming::Ignored(Code::CHUNK));
                }
                let full = std::mem::take(&mut self.chunk);
                if b.get(2..4) == Some(b"ZB") { Incoming::State(Box::new(inflate_state(&full)?)) } else { Incoming::Ignored(Code::CHUNK) }
            }
            Code::FILE_DATA => Incoming::FileData { id: b.get(..2).map(|s| u16::from_be_bytes([s[0], s[1]])).ok_or(MsgError::Truncated("FD"))? },
            other => Incoming::Ignored(other),
        })
    }
}

// ---- outgoing -----------------------------------------------------------------------------

fn json_packet(v: &serde_json::Value) -> Vec<u8> {
    let s = v.to_string();
    let mut body = Vec::with_capacity(s.len() + 4);
    body.extend_from_slice(&(s.len() as u16).to_le_bytes());
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(s.as_bytes());
    packet::encode(Code::JSON, &body).expect("JSON messages are small")
}

/// Subscribe as a UC Surface-compatible remote (`craftSubscribe`). `description` shows in the
/// console's client list; `identifier` is a stable 16-hex-digit client id.
pub fn subscribe(description: &str, identifier: &str) -> Vec<u8> {
    json_packet(&serde_json::json!({
        "id": "Subscribe",
        "clientName": "UC-Surface",
        "clientInternalName": "ucremoteapp",
        "clientType": "StudioLive API",
        "clientDescription": description,
        "clientIdentifier": identifier,
        "clientOptions": "perm users levl redu rtan",
        "clientEncoding": 23106,
    }))
}

pub fn unsubscribe() -> Vec<u8> {
    json_packet(&serde_json::json!({ "id": "Unsubscribe" }))
}

pub fn keep_alive() -> Vec<u8> {
    packet::encode(Code::KEEP_ALIVE, &[]).expect("empty body")
}

/// File request the console answers with an `FD` carrying `id` (liveness probe used by the
/// reference's keep-alive).
pub fn liveness_request(id: u16) -> Vec<u8> {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&id.to_be_bytes());
    body.extend_from_slice(b"Ftbr");
    body.extend_from_slice(&[0, 0]);
    packet::encode(Code::FILE_REQUEST, &body).expect("small body")
}

/// Ask the console to stream meters to our UDP `port`.
pub fn meter_hello(port: u16) -> Vec<u8> {
    let mut cb = CBYTES;
    cb[0] = 0x00;
    packet::encode_with(Code::HELLO, cb, &port.to_le_bytes()).expect("small body")
}

/// Set a numeric parameter (`PV`): name, NUL, 00 00, f32 LE. Booleans are 0.0/1.0.
pub fn set_param(path: &str, value: f32) -> Vec<u8> {
    let mut body = Vec::with_capacity(path.len() + 7);
    body.extend_from_slice(path.as_bytes());
    body.extend_from_slice(&[0, 0, 0]);
    body.extend_from_slice(&value.to_le_bytes());
    packet::encode(Code::PARAM_VALUE, &body).expect("parameter paths are short")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(p: &[u8]) -> Frame<'_> {
        packet::parse(p).unwrap()
    }

    #[test]
    fn set_param_round_trips_through_the_decoder() {
        let p = set_param("line/ch16/mute", 1.0);
        // the exact bytes the console echoed back in the capture
        assert!(p.ends_with(b"line/ch16/mute\0\0\0\x00\x00\x80\x3f"));
        match Decoder::default().decode(frame(&p)).unwrap() {
            Incoming::Param { path, value } => {
                assert_eq!(path, "line/ch16/mute");
                assert_eq!(value, 1.0);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn subscribe_is_a_length_prefixed_json_message() {
        let p = subscribe("stream-engine", "0123456789abcdef");
        let f = frame(&p);
        assert_eq!(f.code, Code::JSON);
        let len = u16::from_le_bytes([f.body[0], f.body[1]]) as usize;
        assert_eq!(len, f.body.len() - 4);
        match Decoder::default().decode(f).unwrap() {
            Incoming::Json(j) => {
                assert_eq!(j["id"], "Subscribe");
                assert_eq!(j["clientOptions"], "perm users levl redu rtan");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn meter_hello_uses_zeroed_first_identity_byte() {
        let p = meter_hello(49070);
        let f = frame(&p);
        assert_eq!(f.code, Code::HELLO);
        assert_eq!(f.cbytes, [0x00, 0x00, 0x65, 0x00]);
        assert_eq!(f.body, 49070u16.to_le_bytes());
    }

    #[test]
    fn faders_parse_groups() {
        let mut b = b"fdrs".to_vec();
        b.extend([0, 0]);
        b.extend(3u16.to_le_bytes());
        for v in [0u16, 32768, 65535] {
            b.extend(v.to_le_bytes());
        }
        b.push(2);
        b.extend([0, 0, 0, 0, 2, 0]); // line: offset 0, count 2
        b.extend([7, 0, 2, 0, 1, 0]); // main: offset 2, count 1
        let g = parse_faders(&b).unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].group_name(), Some("line"));
        assert_eq!(g[0].values.len(), 2);
        assert!((g[0].values[1] - 0.5).abs() < 1e-4);
        assert_eq!(g[1].group_name(), Some("main"));
        assert_eq!(g[1].values, vec![1.0]);
        // a descriptor pointing past the values is rejected, not a panic
        let mut bad = b.clone();
        let n = bad.len();
        bad[n - 2] = 9;
        assert!(parse_faders(&bad).is_err());
    }

    #[test]
    fn chunks_must_arrive_in_order() {
        let mut d = Decoder::default();
        let mut body = b"\0\0ZB".to_vec();
        body.extend(10u32.to_le_bytes()); // offset 10 without a first chunk
        body.extend(20u32.to_le_bytes());
        body.extend(2u32.to_le_bytes());
        body.extend([1, 2]);
        let p = packet::encode(Code::CHUNK, &body).unwrap();
        assert!(matches!(d.decode(frame(&p)), Err(MsgError::ChunkOrder { offset: 10, have: 0 })));
    }
}
