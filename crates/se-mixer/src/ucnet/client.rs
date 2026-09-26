//! One control session with a console: TCP handshake (subscribe → state + reply), UDP
//! meters, keep-alive with liveness check, and a send queue. Port of `Client.ts`
//! (`connect`, `meterSubscribe`, `KeepAliveHelper`).
//!
//! Meters arrive from the console's UDP port 53000. Before announcing our meter port we send
//! an empty datagram to that port, so a stateful host firewall (ufw, default-deny input)
//! treats the meter stream as replies to our own traffic; it is repeated to keep the entry
//! alive.

use super::meters::{LevelFrame, MeterKind};
use super::msg::{self, Decoder, FaderGroup, Incoming};
use super::packet::{self, CONTROL_PORT, Reassembler};
use super::tree::ConsoleState;
use anyhow::{Context, Result, anyhow, bail};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Clone, Debug)]
pub struct ClientOptions {
    /// Shown in the console's client list.
    pub description: String,
    /// Stable 16-hex-digit client id.
    pub identifier: String,
    pub meters: bool,
    /// UDP port for meters (0 = ephemeral).
    pub meter_port: u16,
    pub connect_timeout: Duration,
    pub handshake_timeout: Duration,
    /// Drop the session when the console has been silent this long (it answers a liveness
    /// request every second).
    pub liveness_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            description: "stream-engine".into(),
            identifier: "5e0000000000000a".into(),
            meters: true,
            meter_port: 0,
            connect_timeout: Duration::from_secs(3),
            handshake_timeout: Duration::from_secs(10),
            liveness_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Debug)]
pub enum ClientEvent {
    /// Complete console state (after a scene/project recall the console resends it).
    State(Box<ConsoleState>),
    Param {
        path: String,
        value: f32,
    },
    Text {
        path: String,
        value: String,
    },
    Faders(Vec<FaderGroup>),
    Meters(LevelFrame),
    Json(serde_json::Value),
    /// A packet we could not decode (logged, the session continues).
    Error(String),
}

/// A live session. Dropping it (or all senders) unsubscribes and closes the connection.
pub struct Session {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    pub peer: SocketAddr,
    pub meter_port: Option<u16>,
    /// State received during the handshake.
    pub initial: Box<ConsoleState>,
}

impl Session {
    /// Queue raw packet bytes. Errors when the session has ended.
    pub fn send(&self, packet: Vec<u8>) -> Result<()> {
        self.tx.send(packet).map_err(|_| anyhow!("mixer session closed"))
    }

    pub fn set_param(&self, path: &str, value: f32) -> Result<()> {
        self.send(msg::set_param(path, value))
    }
}

async fn read_packets(stream: &mut TcpStream, re: &mut Reassembler, buf: &mut [u8]) -> Result<Vec<Vec<u8>>> {
    let n = stream.read(buf).await?;
    if n == 0 {
        bail!("console closed the connection");
    }
    re.push(&buf[..n]);
    let mut out = Vec::new();
    while let Some(p) = re.next_packet()? {
        out.push(p);
    }
    Ok(out)
}

fn to_event(dec: &mut Decoder, p: &[u8]) -> Option<ClientEvent> {
    let f = match packet::parse(p) {
        Ok(f) => f,
        Err(e) => return Some(ClientEvent::Error(format!("packet: {e}"))),
    };
    match dec.decode(f) {
        Ok(Incoming::Param { path, value }) => Some(ClientEvent::Param { path, value }),
        Ok(Incoming::Text { path, value }) => Some(ClientEvent::Text { path, value }),
        Ok(Incoming::Faders(g)) => Some(ClientEvent::Faders(g)),
        Ok(Incoming::State(s)) => Some(ClientEvent::State(s)),
        Ok(Incoming::Json(j)) => Some(ClientEvent::Json(j)),
        Ok(Incoming::FileData { .. }) | Ok(Incoming::Ignored(_)) => None,
        Err(e) => Some(ClientEvent::Error(format!("{} message: {e}", f.code.as_str()))),
    }
}

/// Connect, subscribe, and wait for the full state. Returns the session and the task that
/// runs it; the task resolves to the reason the session ended.
pub async fn connect(addr: SocketAddr, opts: &ClientOptions, events: mpsc::Sender<ClientEvent>) -> Result<(Session, JoinHandle<String>)> {
    let mut stream = tokio::time::timeout(opts.connect_timeout, TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow!("connect to {addr} timed out"))?
        .with_context(|| format!("connect to {addr}"))?;
    stream.set_nodelay(true)?;
    stream.write_all(&msg::subscribe(&opts.description, &opts.identifier)).await?;

    let meter = if opts.meters {
        let sock = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, opts.meter_port)))
            .await
            .with_context(|| format!("bind meter port {}", opts.meter_port))?;
        let port = sock.local_addr()?.port();
        let console_meter_src = SocketAddr::new(addr.ip(), CONTROL_PORT);
        sock.send_to(&[], console_meter_src).await?;
        stream.write_all(&msg::meter_hello(port)).await?;
        Some((sock, console_meter_src, port))
    } else {
        None
    };

    // handshake: the state payload and the subscription reply, in either order
    let mut re = Reassembler::default();
    let mut dec = Decoder::default();
    let mut buf = vec![0u8; 64 * 1024];
    let mut state: Option<Box<ConsoleState>> = None;
    let mut replied = false;
    // updates that arrive after the state (same read batch) must not be lost
    let mut early: Vec<ClientEvent> = Vec::new();
    let deadline = tokio::time::Instant::now() + opts.handshake_timeout;
    while state.is_none() || !replied {
        let packets = tokio::time::timeout_at(deadline, read_packets(&mut stream, &mut re, &mut buf))
            .await
            .map_err(|_| anyhow!("{addr}: no state from the console within {:?} (subscribed: {replied})", opts.handshake_timeout))??;
        for p in packets {
            match to_event(&mut dec, &p) {
                Some(ClientEvent::State(s)) => {
                    state = Some(s);
                    early.clear();
                }
                Some(ClientEvent::Json(j)) if j["id"] == "SubscriptionReply" => replied = true,
                Some(ClientEvent::Error(e)) => bail!("{addr}: handshake: {e}"),
                Some(ev) if state.is_some() => early.push(ev),
                _ => {}
            }
        }
    }
    let initial = state.expect("loop exits with a state");

    let (tx, rx) = mpsc::unbounded_channel();
    let meter_port = meter.as_ref().map(|m| m.2);
    let liveness = opts.liveness_timeout;
    let task = tokio::spawn(async move {
        for ev in early {
            if events.send(ev).await.is_err() {
                return "engine stopped".to_string();
            }
        }
        run(stream, re, dec, buf, meter, rx, events, liveness).await
    });
    Ok((Session { tx, peer: addr, meter_port, initial }, task))
}

#[allow(clippy::too_many_arguments)]
async fn run(
    mut stream: TcpStream,
    mut re: Reassembler,
    mut dec: Decoder,
    mut buf: Vec<u8>,
    meter: Option<(UdpSocket, SocketAddr, u16)>,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    events: mpsc::Sender<ClientEvent>,
    liveness: Duration,
) -> String {
    let mut ka = tokio::time::interval(Duration::from_secs(1));
    ka.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut punch = tokio::time::interval(Duration::from_secs(15));
    punch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_seen = Instant::now();
    let mut ka_id: u16 = 0x5e00;
    let mut ubuf = vec![0u8; 4096];
    let reason = loop {
        tokio::select! {
            r = read_packets(&mut stream, &mut re, &mut buf) => match r {
                Ok(packets) => {
                    last_seen = Instant::now();
                    for p in packets {
                        if let Some(ev) = to_event(&mut dec, &p)
                            && events.send(ev).await.is_err()
                        {
                            return "engine stopped".into();
                        }
                    }
                }
                Err(e) => break e.to_string(),
            },
            r = async {
                match &meter {
                    Some((s, _, _)) => s.recv_from(&mut ubuf).await,
                    None => std::future::pending().await,
                }
            } => match r {
                Ok((n, from)) => {
                    if meter.as_ref().is_some_and(|m| m.1.ip() == from.ip()) {
                        let mut f = LevelFrame::default();
                        match f.parse(&ubuf[..n]) {
                            // meters are lossy by nature: never block the TCP side on them
                            Ok(MeterKind::Level) => { let _ = events.try_send(ClientEvent::Meters(f)); }
                            Ok(_) => {}
                            Err(e) => { let _ = events.try_send(ClientEvent::Error(format!("meter datagram: {e}"))); }
                        }
                    }
                }
                Err(e) => break format!("meter socket: {e}"),
            },
            out = rx.recv() => match out {
                Some(bytes) => {
                    if let Err(e) = stream.write_all(&bytes).await {
                        break format!("write: {e}");
                    }
                }
                None => {
                    let _ = stream.write_all(&msg::unsubscribe()).await;
                    let _ = stream.shutdown().await;
                    return "closed".into();
                }
            },
            _ = ka.tick() => {
                if last_seen.elapsed() > liveness {
                    break format!("no reply from the console for {:.1}s", last_seen.elapsed().as_secs_f32());
                }
                ka_id = ka_id.wrapping_add(1);
                let mut pkt = msg::keep_alive();
                pkt.extend_from_slice(&msg::liveness_request(ka_id));
                if let Err(e) = stream.write_all(&pkt).await {
                    break format!("write: {e}");
                }
            },
            _ = punch.tick() => {
                if let Some((s, dst, _)) = &meter {
                    let _ = s.send_to(&[], *dst).await;
                }
            },
        }
    };
    let _ = stream.shutdown().await;
    reason
}
