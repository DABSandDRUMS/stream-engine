//! A console simulator built from the recorded 16R fixtures: it answers the subscription
//! with the captured handshake, streams the captured meter datagrams, answers liveness
//! requests, and echoes parameter changes the way the real console does (`PV` for
//! mutes/sends, an `MS fdrs` packet with every fader for volumes).

#![allow(dead_code)]

use parking_lot::Mutex;
use se_mixer::ucnet::msg::{self, Decoder, Incoming};
use se_mixer::ucnet::packet::{self, Code, Reassembler};
use se_mixer::ucnet::tree::{ConsoleState, Leaf};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::broadcast;

pub fn fixture(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Split a TCP capture into packets.
pub fn tcp_packets(raw: &[u8]) -> Vec<Vec<u8>> {
    let mut r = Reassembler::default();
    r.push(raw);
    let mut out = Vec::new();
    while let Some(p) = r.next_packet().unwrap() {
        out.push(p);
    }
    assert_eq!(r.pending(), 0, "capture ends mid-packet");
    out
}

/// Split a UDP capture (u32 LE length-prefixed datagrams).
pub fn udp_datagrams(raw: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= raw.len() {
        let n = u32::from_le_bytes(raw[i..i + 4].try_into().unwrap()) as usize;
        out.push(raw[i + 4..i + 4 + n].to_vec());
        i += 4 + n;
    }
    out
}

/// The console state contained in the handshake capture.
pub fn handshake_state() -> ConsoleState {
    let mut d = Decoder::default();
    for p in tcp_packets(&fixture("handshake.tcp")) {
        if let Incoming::State(s) = d.decode(packet::parse(&p).unwrap()).unwrap() {
            return *s;
        }
    }
    panic!("no state in handshake.tcp");
}

/// `MS fdrs` packet with every fader of `st`, in the console's group order.
pub fn fdrs_packet(st: &ConsoleState) -> Vec<u8> {
    let groups: [(u16, &str); 7] = [(0, "line"), (1, "return"), (2, "fxreturn"), (3, "talkback"), (4, "aux"), (5, "fxbus"), (7, "main")];
    let mut values: Vec<u16> = Vec::new();
    let mut descs: Vec<(u16, u16, u16)> = Vec::new();
    for (id, name) in groups {
        let chans = st.strips(name);
        let off = values.len() as u16;
        for c in &chans {
            let v = st.num(&format!("{name}/ch{c}/volume")).unwrap_or(0.0);
            values.push((v.clamp(0.0, 1.0) * 65535.0).round() as u16);
        }
        descs.push((id, off, chans.len() as u16));
    }
    let mut b = b"fdrs".to_vec();
    b.extend([0, 0]);
    b.extend((values.len() as u16).to_le_bytes());
    for v in &values {
        b.extend(v.to_le_bytes());
    }
    b.push(descs.len() as u8);
    for (id, off, n) in descs {
        b.extend(id.to_le_bytes());
        b.extend(off.to_le_bytes());
        b.extend(n.to_le_bytes());
    }
    packet::encode(Code::METER16, &b).unwrap()
}

pub struct FakeConsole {
    pub addr: SocketAddr,
    pub state: Arc<Mutex<ConsoleState>>,
    /// Every parameter set a client sent, in order.
    pub received: Arc<Mutex<Vec<(String, f32)>>>,
    /// Number of subscriptions seen (reconnect detection).
    pub subscriptions: Arc<Mutex<usize>>,
    out: broadcast::Sender<Vec<u8>>,
    kill: broadcast::Sender<()>,
}

impl FakeConsole {
    pub async fn start() -> FakeConsole {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(handshake_state()));
        let received = Arc::new(Mutex::new(Vec::new()));
        let subscriptions = Arc::new(Mutex::new(0));
        let (out, _) = broadcast::channel(1024);
        let (kill, _) = broadcast::channel(4);
        let fc =
            FakeConsole { addr, state: state.clone(), received: received.clone(), subscriptions: subscriptions.clone(), out: out.clone(), kill: kill.clone() };
        tokio::spawn(async move {
            loop {
                let Ok((sock, peer)) = listener.accept().await else { return };
                let (state, received, subscriptions, out, kill) = (state.clone(), received.clone(), subscriptions.clone(), out.clone(), kill.clone());
                tokio::spawn(async move { serve(sock, peer, state, received, subscriptions, out, kill).await });
            }
        });
        fc
    }

    /// A change made on the console itself (or UC Surface): update and notify clients.
    pub fn external(&self, path: &str, value: f64) {
        let pkt = {
            let mut st = self.state.lock();
            st.values.insert(path.to_string(), Leaf::Num(value));
            if path.ends_with("/volume") { fdrs_packet(&st) } else { msg::set_param(path, value as f32) }
        };
        let _ = self.out.send(pkt);
    }

    /// Drop every client connection (network blip).
    pub fn drop_clients(&self) {
        let _ = self.kill.send(());
    }

    pub fn num(&self, path: &str) -> Option<f64> {
        self.state.lock().num(path)
    }

    pub fn sets_of(&self, path: &str) -> Vec<f32> {
        self.received.lock().iter().filter(|(p, _)| p == path).map(|(_, v)| *v).collect()
    }
}

async fn serve(
    mut sock: tokio::net::TcpStream,
    peer: SocketAddr,
    state: Arc<Mutex<ConsoleState>>,
    received: Arc<Mutex<Vec<(String, f32)>>>,
    subscriptions: Arc<Mutex<usize>>,
    out: broadcast::Sender<Vec<u8>>,
    kill: broadcast::Sender<()>,
) {
    let mut re = Reassembler::default();
    let mut buf = vec![0u8; 65536];
    let mut rx = out.subscribe();
    let mut killed = kill.subscribe();
    let meters = udp_datagrams(&fixture("meters.udp"));
    let mut meter_task: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        tokio::select! {
            r = sock.read(&mut buf) => {
                let Ok(n) = r else { break };
                if n == 0 { break; }
                re.push(&buf[..n]);
                while let Ok(Some(p)) = re.next_packet() {
                    let f = packet::parse(&p).unwrap();
                    match f.code {
                        Code::JSON => {
                            let j: serde_json::Value = serde_json::from_slice(&f.body[4..]).unwrap();
                            if j["id"] == "Subscribe" {
                                *subscriptions.lock() += 1;
                                // the real handshake, but with the current state
                                let hs = fixture("handshake.tcp");
                                if sock.write_all(&hs).await.is_err() { return; }
                                let st = state.lock().clone();
                                if sock.write_all(&fdrs_packet(&st)).await.is_err() { return; }
                            }
                        }
                        Code::HELLO => {
                            let port = u16::from_le_bytes([f.body[0], f.body[1]]);
                            let dst = SocketAddr::new(peer.ip(), port);
                            let meters = meters.clone();
                            meter_task = Some(tokio::spawn(async move {
                                let u = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                                loop {
                                    for d in &meters {
                                        if u.send_to(d, dst).await.is_err() { return; }
                                        tokio::time::sleep(Duration::from_millis(13)).await;
                                    }
                                }
                            }));
                        }
                        Code::PARAM_VALUE => {
                            if let Incoming::Param { path, value } = Decoder::default().decode(f).unwrap() {
                                received.lock().push((path.clone(), value));
                                let echo = {
                                    let mut st = state.lock();
                                    st.values.insert(path.clone(), Leaf::Num(value as f64));
                                    if path.ends_with("/volume") { fdrs_packet(&st) } else { msg::set_param(&path, value) }
                                };
                                let _ = out.send(echo);
                            }
                        }
                        Code::FILE_REQUEST => {
                            let mut body = f.body[..2].to_vec();
                            body.extend([0u8; 12]);
                            let fd = packet::encode(Code::FILE_DATA, &body).unwrap();
                            if sock.write_all(&fd).await.is_err() { break; }
                        }
                        _ => {}
                    }
                }
            }
            m = rx.recv() => match m {
                Ok(p) => if sock.write_all(&p).await.is_err() { break; },
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            },
            _ = killed.recv() => break,
        }
    }
    if let Some(t) = meter_task {
        t.abort();
    }
}
