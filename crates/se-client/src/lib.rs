//! Engine client over the Unix socket.
//!
//! [`Conn`] is a single blocking connection (CLI). [`Live`] keeps a connection alive in the
//! background, reconnecting with backoff and re-sending the subscription, and delivers
//! [`LiveEvent`]s on a channel (UI).

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use se_proto::wire::{ClientMsg, PROTOCOL_VERSION, ServerMsg, Subscription, WireError, default_socket_path, read_frame_sync, write_frame_sync};
use se_proto::{Command, Value};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

pub struct Conn {
    stream: UnixStream,
    next_req: u64,
    /// Messages received while waiting for a reply (events, state pushes).
    pub backlog: Vec<ServerMsg>,
    pub welcome: Option<ServerMsg>,
}

impl Conn {
    pub fn connect(path: Option<&Path>, client: &str) -> Result<Conn, WireError> {
        let p = path.map(Path::to_path_buf).unwrap_or_else(default_socket_path);
        let stream = UnixStream::connect(&p)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut c = Conn { stream, next_req: 1, backlog: Vec::new(), welcome: None };
        c.send(&ClientMsg::Hello { client: client.into(), token: None, version: PROTOCOL_VERSION })?;
        let w = c.recv()?;
        c.welcome = Some(w);
        Ok(c)
    }

    pub fn send(&mut self, m: &ClientMsg) -> Result<(), WireError> {
        write_frame_sync(&mut self.stream, m)
    }

    pub fn recv(&mut self) -> Result<ServerMsg, WireError> {
        read_frame_sync(&mut self.stream)
    }

    pub fn set_timeout(&self, d: Option<Duration>) {
        let _ = self.stream.set_read_timeout(d);
    }

    fn req(&mut self) -> u64 {
        self.next_req += 1;
        self.next_req
    }

    fn wait_for(&mut self, mut pred: impl FnMut(&ServerMsg) -> bool) -> Result<ServerMsg, WireError> {
        loop {
            let m = self.recv()?;
            if pred(&m) {
                return Ok(m);
            }
            self.backlog.push(m);
            if self.backlog.len() > 10_000 {
                self.backlog.drain(..5_000);
            }
        }
    }

    /// Submit a command and wait for its ack.
    pub fn exec(&mut self, cmd: Command) -> Result<Result<u64, String>, WireError> {
        let req = self.req();
        self.send(&ClientMsg::Cmd { req: Some(req), cmd })?;
        match self.wait_for(|m| matches!(m, ServerMsg::Ack { req: Some(r), .. } if *r == req))? {
            ServerMsg::Ack { ok: true, id, .. } => Ok(Ok(id)),
            ServerMsg::Ack { error, .. } => Ok(Err(error.unwrap_or_else(|| "failed".into()))),
            _ => unreachable!(),
        }
    }

    pub fn exec_text(&mut self, text: &str) -> Result<Result<u64, String>, WireError> {
        let req = self.req();
        self.send(&ClientMsg::CmdText { req: Some(req), text: text.into() })?;
        match self.wait_for(|m| matches!(m, ServerMsg::Ack { req: Some(r), .. } if *r == req))? {
            ServerMsg::Ack { ok: true, id, .. } => Ok(Ok(id)),
            ServerMsg::Ack { error, .. } => Ok(Err(error.unwrap_or_else(|| "failed".into()))),
            _ => unreachable!(),
        }
    }

    pub fn query(&mut self, name: &str, args: Value) -> Result<Result<Value, String>, WireError> {
        let req = self.req();
        self.send(&ClientMsg::Query { req, name: name.into(), args })?;
        match self.wait_for(|m| matches!(m, ServerMsg::Reply { req: r, .. } if *r == req))? {
            ServerMsg::Reply { error: Some(e), .. } => Ok(Err(e)),
            ServerMsg::Reply { value, .. } => Ok(Ok(value)),
            _ => unreachable!(),
        }
    }

    pub fn get(&mut self, pattern: &str, meta: bool) -> Result<Vec<se_proto::wire::StateEntry>, WireError> {
        let req = self.req();
        self.send(&ClientMsg::Get { req, pattern: pattern.into(), meta })?;
        match self.wait_for(|m| matches!(m, ServerMsg::Values { req: r, .. } if *r == req))? {
            ServerMsg::Values { entries, .. } => Ok(entries),
            _ => unreachable!(),
        }
    }

    pub fn explain(&mut self, address: &str) -> Result<Option<se_proto::wire::Provenance>, WireError> {
        let req = self.req();
        self.send(&ClientMsg::Explain { req, address: address.into() })?;
        match self.wait_for(|m| matches!(m, ServerMsg::Explain { req: r, .. } if *r == req))? {
            ServerMsg::Explain { provenance, .. } => Ok(provenance),
            _ => unreachable!(),
        }
    }

    pub fn trace(&mut self, id: u64) -> Result<Vec<se_proto::wire::TraceRec>, WireError> {
        let req = self.req();
        self.send(&ClientMsg::Trace { req, id })?;
        match self.wait_for(|m| matches!(m, ServerMsg::Trace { req: Some(r), .. } if *r == req))? {
            ServerMsg::Trace { records, .. } => Ok(records),
            _ => unreachable!(),
        }
    }

    pub fn subscribe(&mut self, sub: Subscription) -> Result<(), WireError> {
        self.send(&ClientMsg::Subscribe { sub })
    }
}

/// Connection status and pushed messages for a live client.
#[derive(Debug, Clone)]
pub enum LiveEvent {
    Connected { session: String, pid: u32 },
    Disconnected { reason: String },
    Msg(ServerMsg),
}

/// Background connection with reconnect.
pub struct Live {
    out_tx: Sender<ClientMsg>,
    pub events: Receiver<LiveEvent>,
    sub: Arc<Mutex<Subscription>>,
    connected: Arc<AtomicBool>,
    next_req: AtomicU64,
    stop: Arc<AtomicBool>,
}

impl Live {
    pub fn start(path: Option<PathBuf>, client: &str, sub: Subscription) -> Live {
        let (out_tx, out_rx) = crossbeam_channel::unbounded::<ClientMsg>();
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded::<LiveEvent>();
        let sub = Arc::new(Mutex::new(sub));
        let connected = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, c2, st2, name) = (sub.clone(), connected.clone(), stop.clone(), client.to_string());
        std::thread::Builder::new().name("se-client".into()).spawn(move || run(path, name, s2, c2, st2, out_rx, ev_tx)).expect("spawn client thread");
        Live { out_tx, events: ev_rx, sub, connected, next_req: AtomicU64::new(1), stop }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn send(&self, m: ClientMsg) {
        let _ = self.out_tx.send(m);
    }

    pub fn req(&self) -> u64 {
        self.next_req.fetch_add(1, Ordering::Relaxed)
    }

    pub fn command(&self, cmd: Command) -> u64 {
        let req = self.req();
        self.send(ClientMsg::Cmd { req: Some(req), cmd });
        req
    }

    pub fn text(&self, text: &str) -> u64 {
        let req = self.req();
        self.send(ClientMsg::CmdText { req: Some(req), text: text.into() });
        req
    }

    pub fn query(&self, name: &str, args: Value) -> u64 {
        let req = self.req();
        self.send(ClientMsg::Query { req, name: name.into(), args });
        req
    }

    pub fn set_subscription(&self, sub: Subscription) {
        *self.sub.lock() = sub.clone();
        self.send(ClientMsg::Subscribe { sub });
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run(
    path: Option<PathBuf>,
    client: String,
    sub: Arc<Mutex<Subscription>>,
    connected: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    out_rx: Receiver<ClientMsg>,
    ev_tx: Sender<LiveEvent>,
) {
    let mut backoff = Duration::from_millis(100);
    while !stop.load(Ordering::Relaxed) {
        let p = path.clone().unwrap_or_else(default_socket_path);
        let stream = match UnixStream::connect(&p) {
            Ok(s) => s,
            Err(e) => {
                let _ = ev_tx.send(LiveEvent::Disconnected { reason: e.to_string() });
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(2));
                continue;
            }
        };
        backoff = Duration::from_millis(100);
        let mut wr = match stream.try_clone() {
            Ok(w) => w,
            Err(_) => continue,
        };
        let mut rd = stream;
        if write_frame_sync(&mut wr, &ClientMsg::Hello { client: client.clone(), token: None, version: PROTOCOL_VERSION }).is_err() {
            continue;
        }
        match read_frame_sync::<_, ServerMsg>(&mut rd) {
            Ok(ServerMsg::Welcome { session, pid, .. }) => {
                connected.store(true, Ordering::Relaxed);
                let _ = ev_tx.send(LiveEvent::Connected { session, pid });
            }
            _ => continue,
        }
        let s = sub.lock().clone();
        let _ = write_frame_sync(&mut wr, &ClientMsg::Subscribe { sub: s });
        // drop commands queued while disconnected except subscriptions (UI re-sends intent)
        while out_rx.try_recv().is_ok() {}
        let dead = Arc::new(AtomicBool::new(false));
        let (d2, st2) = (dead.clone(), stop.clone());
        let orx = out_rx.clone();
        let writer = std::thread::spawn(move || {
            while !d2.load(Ordering::Relaxed) && !st2.load(Ordering::Relaxed) {
                match orx.recv_timeout(Duration::from_millis(200)) {
                    Ok(m) => {
                        if write_frame_sync(&mut wr, &m).is_err() {
                            d2.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(_) => break,
                }
            }
            let _ = wr.shutdown(std::net::Shutdown::Both);
        });
        let reason = loop {
            if stop.load(Ordering::Relaxed) {
                break "stopped".to_string();
            }
            match read_frame_sync::<_, ServerMsg>(&mut rd) {
                Ok(m) => {
                    if ev_tx.send(LiveEvent::Msg(m)).is_err() {
                        break "client dropped".into();
                    }
                }
                Err(e) => break e.to_string(),
            }
        };
        dead.store(true, Ordering::Relaxed);
        let _ = writer.join();
        connected.store(false, Ordering::Relaxed);
        let _ = ev_tx.send(LiveEvent::Disconnected { reason });
    }
}
