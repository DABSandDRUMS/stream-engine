//! API messages shared by the Unix socket (length-prefixed MessagePack), the WebSocket
//! (JSON text frames), and client libraries (UI, CLI, web patches).

use crate::{Command, Event, Id, Meta, Ts, Value};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
/// Frames above this size are rejected (protects the engine from bad clients).
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Subscription {
    /// Event type patterns (`twitch.*`, `**`).
    pub events: Vec<String>,
    /// State address patterns; matching changes are pushed.
    pub state: Vec<String>,
    /// Signal name patterns; pushed at `signal_hz`.
    pub signals: Vec<String>,
    pub signal_hz: Option<f32>,
    /// Receive engine log lines.
    pub logs: bool,
    /// Receive trace records as they happen.
    pub trace: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        client: String,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        version: u32,
    },
    Cmd {
        #[serde(default)]
        req: Option<u64>,
        cmd: Command,
    },
    /// Parse and submit one-line command text.
    CmdText {
        #[serde(default)]
        req: Option<u64>,
        text: String,
    },
    Subscribe {
        sub: Subscription,
    },
    /// Current values (and metadata) for addresses matching `pattern`.
    Get {
        req: u64,
        pattern: String,
        #[serde(default)]
        meta: bool,
    },
    /// Provenance for one address.
    Explain {
        req: u64,
        address: String,
    },
    /// Cause chain (ancestors and descendants) for a trace id.
    Trace {
        req: u64,
        id: Id,
    },
    /// Named queries served by subsystems (`presets`, `rules`, `scenes`, `sessions`, `devices`, `preflight`, …).
    Query {
        req: u64,
        name: String,
        #[serde(default)]
        args: Value,
    },
    Ping {
        stamp: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateEntry {
    pub address: String,
    pub value: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Layer {
    /// `base`, `scene`, `binding`, `override`, `animation`, `envelope`, `clamp`.
    pub kind: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u16>,
    pub value: Value,
    /// Whether this layer contributed to the final value.
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub address: String,
    pub value: Value,
    pub layers: Vec<Layer>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceRec {
    pub id: Id,
    pub parent: Option<Id>,
    pub ts: Ts,
    /// `event`, `rule`, `command`, `change`, `preset`, `error`, `policy`.
    pub kind: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        version: u32,
        engine: String,
        session: String,
        now: Ts,
        pid: u32,
    },
    Ack {
        req: Option<u64>,
        id: Id,
        ok: bool,
        #[serde(default)]
        error: Option<String>,
    },
    State {
        changes: Vec<(String, Value)>,
    },
    Values {
        req: u64,
        entries: Vec<StateEntry>,
    },
    Event {
        event: Event,
    },
    Signals {
        ts: Ts,
        values: Vec<(String, f32)>,
    },
    Explain {
        req: u64,
        provenance: Option<Provenance>,
    },
    Trace {
        req: Option<u64>,
        records: Vec<TraceRec>,
    },
    Reply {
        req: u64,
        #[serde(default)]
        value: Value,
        #[serde(default)]
        error: Option<String>,
    },
    Log {
        level: String,
        target: String,
        msg: String,
        ts: Ts,
    },
    Pong {
        stamp: u64,
        now: Ts,
    },
    Error {
        msg: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("decode: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("frame too large: {0} bytes")]
    TooLarge(usize),
}

/// Encode one frame: `u32` big-endian length + MessagePack body.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, WireError> {
    let body = rmp_serde::to_vec_named(msg)?;
    if body.len() > MAX_FRAME {
        return Err(WireError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

pub fn decode_body<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, WireError> {
    Ok(rmp_serde::from_slice(body)?)
}

/// Blocking frame read (CLI and tests).
pub fn read_frame_sync<R: std::io::Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<T, WireError> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_be_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(WireError::TooLarge(n));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    decode_body(&buf)
}

pub fn write_frame_sync<W: std::io::Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), WireError> {
    w.write_all(&encode_frame(msg)?)?;
    w.flush()?;
    Ok(())
}

#[cfg(feature = "tokio")]
pub mod async_io {
    use super::*;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    pub async fn read_frame<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<T, WireError> {
        let n = r.read_u32().await? as usize;
        if n > MAX_FRAME {
            return Err(WireError::TooLarge(n));
        }
        let mut buf = vec![0u8; n];
        r.read_exact(&mut buf).await?;
        decode_body(&buf)
    }

    pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<(), WireError> {
        w.write_all(&encode_frame(msg)?).await?;
        w.flush().await?;
        Ok(())
    }
}

/// Default Unix socket path: `$XDG_RUNTIME_DIR/stream-engine/engine.sock`.
pub fn default_socket_path() -> std::path::PathBuf {
    runtime_dir().join("engine.sock")
}

pub fn runtime_dir() -> std::path::PathBuf {
    let base =
        std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(|| std::path::PathBuf::from(format!("/run/user/{}", unsafe_uid())));
    base.join("stream-engine")
}

fn unsafe_uid() -> u32 {
    // Parse from /proc to avoid a libc dependency in the protocol crate.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()))
        .unwrap_or(1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Op, Origin};

    #[test]
    fn frame_roundtrip() {
        let msg = ClientMsg::Cmd { req: Some(7), cmd: Command::new(Origin::Cli, Op::Panic) };
        let f = encode_frame(&msg).unwrap();
        let back: ClientMsg = read_frame_sync(&mut &f[..]).unwrap();
        assert_eq!(back, msg);
        let s = ServerMsg::State { changes: vec![("a.b".into(), Value::Float(0.5))] };
        let f = encode_frame(&s).unwrap();
        let back: ServerMsg = read_frame_sync(&mut &f[..]).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn json_shape() {
        let m: ClientMsg = serde_json::from_str(r#"{"t":"cmd_text","text":"preset.fire hype"}"#).unwrap();
        assert!(matches!(m, ClientMsg::CmdText { .. }));
    }
}
