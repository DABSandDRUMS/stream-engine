//! One client run: resolve room → sign → open socket → pump frames, with capped exponential
//! backoff on errors and a slow poll while the stream is offline. Everything the run reports
//! goes through [`Out`] (hub events/state/signals + the private status behind the `tiktok`
//! query), so tests can capture it with a recording [`Sink`].

use crate::backoff::{Backoff, Rng};
use crate::config::Settings;
use crate::frame;
use crate::process::{Kind, Output, Processor};
use crate::transport::{RoomStatus, Socket, Transport, TransportError};
use parking_lot::Mutex;
use se_hub::Hub;
use se_proto::{Event, Value};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior};

pub const RETRY_BASE: Duration = Duration::from_secs(2);
pub const RETRY_CAP: Duration = Duration::from_secs(300);
/// Floor for retries after the sign provider refused the key (a new key restarts the client).
pub const AUTH_RETRY: Duration = Duration::from_secs(900);
/// Default wait after a 429 without `Retry-After`.
pub const RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);
pub const HEARTBEAT: Duration = Duration::from_secs(10);
/// No frame at all for this long → the socket is dead (the server answers every heartbeat).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// A connection that lasted this long resets the backoff.
pub const STABLE_AFTER: Duration = Duration::from_secs(60);
const HOUSEKEEPING: Duration = Duration::from_millis(250);

/// Where a run's output goes (the hub in production).
pub trait Sink: Send + Sync + 'static {
    fn emit(&self, e: Event);
    fn publish(&self, address: &str, v: Value);
    fn signal(&self, name: &str, v: f32);
    fn log(&self, level: &str, msg: String);
}

impl Sink for Hub {
    fn emit(&self, e: Event) {
        Hub::emit(self, e);
    }
    fn publish(&self, address: &str, v: Value) {
        Hub::publish(self, address, v);
    }
    fn signal(&self, name: &str, v: f32) {
        Hub::signal(self, name, v);
    }
    fn log(&self, level: &str, msg: String) {
        Hub::log(self, level, "tiktok", msg);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkState {
    #[default]
    Disabled,
    Resolving,
    Connecting,
    Connected,
    Offline,
    Backoff,
    Failed,
}

impl LinkState {
    pub const OPTIONS: [&'static str; 7] = ["disabled", "resolving", "connecting", "connected", "offline", "backoff", "failed"];

    pub fn as_str(self) -> &'static str {
        Self::OPTIONS[self as usize]
    }

    fn health(self) -> &'static str {
        match self {
            LinkState::Disabled | LinkState::Connected => "pass",
            LinkState::Failed => "fail",
            _ => "warn",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub enabled: bool,
    /// Session override from `tiktok.connect` / `tiktok.disconnect`.
    pub manual: Option<bool>,
    pub unique_id: String,
    pub state: LinkState,
    pub room_id: String,
    pub detail: String,
    pub last_error: String,
    pub viewers: u64,
    /// "", "anonymous" or "api_key" (last sign request).
    pub sign_auth: &'static str,
}

#[derive(Default)]
pub struct Counts {
    events: [AtomicU64; 7],
    pub dropped: AtomicU64,
    pub frames: AtomicU64,
    pub decode_errors: AtomicU64,
    pub connects: AtomicU64,
    pub reconnects: AtomicU64,
    pub panics: AtomicU64,
}

impl Counts {
    fn event(&self, k: Kind) {
        self.events[k as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub fn get(&self, k: Kind) -> u64 {
        self.events[k as usize].load(Ordering::Relaxed)
    }

    pub fn to_value(&self) -> Value {
        let mut v = Value::map();
        for k in Kind::ALL {
            v = v.with(k.as_str(), self.get(k));
        }
        let n = |a: &AtomicU64| a.load(Ordering::Relaxed);
        v.with("dropped", n(&self.dropped))
            .with("frames", n(&self.frames))
            .with("decode_errors", n(&self.decode_errors))
            .with("connects", n(&self.connects))
            .with("reconnects", n(&self.reconnects))
            .with("panics", n(&self.panics))
    }
}

/// Private to this crate: status for the `tiktok` query and counters. The mutex is never
/// held across an await and never shared with another subsystem.
#[derive(Default)]
pub struct Shared {
    pub status: Mutex<Status>,
    pub counts: Counts,
}

impl Shared {
    pub fn to_value(&self) -> Value {
        let s = self.status.lock().clone();
        Value::map()
            .with("enabled", s.enabled)
            .with("manual", s.manual.map(Value::Bool).unwrap_or_default())
            .with("connected", s.state == LinkState::Connected)
            .with("state", s.state.as_str())
            .with("room_id", s.room_id)
            .with("unique_id", s.unique_id)
            .with("detail", s.detail)
            .with("last_error", s.last_error)
            .with("viewers", s.viewers)
            .with("sign_auth", s.sign_auth)
            .with("counts", self.counts.to_value())
    }
}

#[derive(Clone)]
pub struct Out {
    pub sink: Arc<dyn Sink>,
    pub shared: Arc<Shared>,
}

impl Out {
    pub fn new(sink: Arc<dyn Sink>, shared: Arc<Shared>) -> Self {
        Out { sink, shared }
    }

    /// Publish the full state once (startup).
    pub fn publish_all(&self) {
        let s = self.shared.status.lock().clone();
        self.sink.publish("tiktok.status", s.state.as_str().into());
        self.sink.publish("tiktok.connected", (s.state == LinkState::Connected).into());
        self.sink.publish("tiktok.room_id", s.room_id.clone().into());
        self.publish_health(s.state, &s.detail);
    }

    fn publish_health(&self, state: LinkState, detail: &str) {
        self.sink.publish("health.tiktok", Value::map().with("status", state.health()).with("detail", detail));
    }

    /// Update the link state; publishes only what changed.
    pub fn set_state(&self, state: LinkState, room_id: &str, detail: impl Into<String>) {
        let detail = detail.into();
        let (old_state, old_room, old_detail) = {
            let mut s = self.shared.status.lock();
            let old = (s.state, std::mem::replace(&mut s.room_id, room_id.to_string()), std::mem::replace(&mut s.detail, detail.clone()));
            s.state = state;
            old
        };
        if old_state != state {
            self.sink.publish("tiktok.status", state.as_str().into());
            if (old_state == LinkState::Connected) != (state == LinkState::Connected) {
                self.sink.publish("tiktok.connected", (state == LinkState::Connected).into());
            }
        }
        if old_room != room_id {
            self.sink.publish("tiktok.room_id", room_id.into());
        }
        if old_state != state || old_detail != detail {
            self.publish_health(state, &detail);
        }
    }

    pub fn state(&self) -> LinkState {
        self.shared.status.lock().state
    }

    pub fn error(&self, level: &str, msg: String) {
        self.shared.status.lock().last_error = msg.clone();
        self.sink.log(level, msg);
    }

    pub fn viewers(&self, n: u64) {
        let changed = {
            let mut s = self.shared.status.lock();
            std::mem::replace(&mut s.viewers, n) != n
        };
        if changed {
            self.sink.signal("tiktok.viewers", n as f32);
        }
    }

    fn set_sign_auth(&self, authenticated: bool) {
        self.shared.status.lock().sign_auth = if authenticated { "api_key" } else { "anonymous" };
    }

    /// Deliver processor outputs; true when the stream ended.
    pub fn apply(&self, outs: Vec<Output>) -> bool {
        let mut ended = false;
        for o in outs {
            match o {
                Output::Event(k, e) => {
                    self.shared.counts.event(k);
                    self.sink.emit(e);
                }
                Output::Viewers(n) => self.viewers(n),
                Output::StreamEnded => ended = true,
                Output::Log(level, msg) => self.sink.log(level, msg),
                Output::Dropped(_) => {
                    self.shared.counts.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Output::DecodeError(msg) => {
                    self.shared.counts.decode_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(target: "tiktok", "undecodable message: {msg}");
                }
            }
        }
        ended
    }
}

/// Resolve when stop is requested (or the controller is gone).
pub async fn wait_stop(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() {
            return;
        }
        if stop.changed().await.is_err() {
            return;
        }
    }
}

pub async fn until_stop<F: Future>(stop: &mut watch::Receiver<bool>, f: F) -> Option<F::Output> {
    if *stop.borrow() {
        return None;
    }
    tokio::select! {
        r = f => Some(r),
        _ = wait_stop(stop) => None,
    }
}

/// Sleep; false when stopped meanwhile.
pub async fn sleep_or_stop(stop: &mut watch::Receiver<bool>, d: Duration) -> bool {
    until_stop(stop, tokio::time::sleep(d)).await.is_some()
}

enum End {
    Stopped,
    StreamEnded,
    Lost(String),
}

fn now_std() -> std::time::Instant {
    Instant::now().into_std()
}

/// Run the client for `settings.unique_id` until `stop`.
pub async fn run<T: Transport>(t: T, settings: Settings, out: Out, mut stop: watch::Receiver<bool>) {
    let mut rng = Rng::from_time();
    let mut backoff = Backoff::new(RETRY_BASE, RETRY_CAP, Rng::new(rng.next_u64()));
    let mut proc = Processor::new();
    let uid = settings.unique_id.clone();
    let poll = settings.poll_offline;
    loop {
        // while offline the periodic re-check stays "offline" (no status flapping every poll)
        if out.state() != LinkState::Offline {
            out.set_state(LinkState::Resolving, "", format!("looking up @{uid}"));
        }
        let room = match until_stop(&mut stop, t.room_status(&uid)).await {
            None => break,
            Some(Ok(RoomStatus::Live { room_id })) => room_id,
            Some(Ok(RoomStatus::Offline)) => {
                backoff.reset();
                out.viewers(0);
                out.set_state(LinkState::Offline, "", format!("@{uid} is not live; checking every {}s", poll.as_secs()));
                if !sleep_or_stop(&mut stop, poll).await {
                    break;
                }
                continue;
            }
            Some(Err(e)) => {
                if !retry(&out, &mut stop, &mut backoff, poll, e).await {
                    break;
                }
                continue;
            }
        };
        out.set_state(LinkState::Connecting, &room, format!("signing a connection to room {room}"));
        let signed = match until_stop(&mut stop, t.sign(&room)).await {
            None => break,
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                if !retry(&out, &mut stop, &mut backoff, poll, e).await {
                    break;
                }
                continue;
            }
        };
        out.set_sign_auth(signed.authenticated);
        let room_num = match signed.room_id.parse::<i64>() {
            Ok(n) if n > 0 => n,
            _ => {
                let e = TransportError::Protocol(format!("sign provider returned room id `{}`", signed.room_id));
                if !retry(&out, &mut stop, &mut backoff, poll, e).await {
                    break;
                }
                continue;
            }
        };
        let ended = out.apply(proc.handle(&signed.initial, false, now_std()));
        if !ended {
            let mut sock = match until_stop(&mut stop, t.open(&signed)).await {
                None => break,
                Some(Ok(s)) => s,
                Some(Err(e)) => {
                    if !retry(&out, &mut stop, &mut backoff, poll, e).await {
                        break;
                    }
                    continue;
                }
            };
            out.shared.counts.connects.fetch_add(1, Ordering::Relaxed);
            out.set_state(LinkState::Connected, &signed.room_id, format!("connected to @{uid} (room {})", signed.room_id));
            let started = Instant::now();
            let end = pump(&mut sock, room_num, &mut proc, &out, &mut stop, &mut rng).await;
            out.apply(proc.flush_all(now_std()));
            sock.close().await;
            match end {
                End::Stopped => break,
                End::StreamEnded => {}
                End::Lost(msg) => {
                    if started.elapsed() >= STABLE_AFTER {
                        backoff.reset();
                    }
                    out.shared.counts.reconnects.fetch_add(1, Ordering::Relaxed);
                    if !retry(&out, &mut stop, &mut backoff, poll, TransportError::Network(msg)).await {
                        break;
                    }
                    continue;
                }
            }
        }
        backoff.reset();
        out.viewers(0);
        out.set_state(LinkState::Offline, "", format!("@{uid} ended the stream; checking every {}s", poll.as_secs()));
        if !sleep_or_stop(&mut stop, poll).await {
            break;
        }
    }
}

async fn retry(out: &Out, stop: &mut watch::Receiver<bool>, backoff: &mut Backoff, poll: Duration, e: TransportError) -> bool {
    let (state, delay) = match &e {
        TransportError::RateLimited { retry_after, .. } => {
            (LinkState::Backoff, backoff.next_delay().max(retry_after.unwrap_or(RATE_LIMIT_WAIT)).min(Duration::from_secs(3600)))
        }
        TransportError::Auth(_) => (LinkState::Failed, backoff.next_delay().max(AUTH_RETRY)),
        TransportError::NotFound(_) => (LinkState::Failed, poll),
        _ => (LinkState::Backoff, backoff.next_delay()),
    };
    out.error("warn", e.to_string());
    out.set_state(state, "", format!("{e}; retrying in {}s", delay.as_secs().max(1)));
    sleep_or_stop(stop, delay).await
}

async fn pump<S: Socket>(sock: &mut S, room: i64, proc: &mut Processor, out: &Out, stop: &mut watch::Receiver<bool>, rng: &mut Rng) -> End {
    if let Err(e) = sock.send(frame::enter_room(room, rng.positive_i64())).await {
        return End::Lost(format!("joining the room failed: {e}"));
    }
    let every = sock.ping_interval().unwrap_or(HEARTBEAT).clamp(Duration::from_secs(1), Duration::from_secs(60));
    let mut hb = tokio::time::interval(every);
    hb.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut housekeeping = tokio::time::interval(HOUSEKEEPING);
    housekeeping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut seq = 1i64;
    let mut last_rx = Instant::now();
    loop {
        tokio::select! {
            _ = wait_stop(stop) => return End::Stopped,
            _ = hb.tick() => {
                if let Err(e) = sock.send(frame::heartbeat(room, seq)).await {
                    return End::Lost(format!("heartbeat failed: {e}"));
                }
                seq += 1;
            }
            _ = housekeeping.tick() => {
                out.apply(proc.tick(now_std()));
                if last_rx.elapsed() >= IDLE_TIMEOUT {
                    return End::Lost(format!("no data from TikTok for {}s", IDLE_TIMEOUT.as_secs()));
                }
            }
            m = sock.recv() => {
                let bytes = match m {
                    None => return End::Lost("connection closed by the server".into()),
                    Some(Err(e)) => return End::Lost(e.to_string()),
                    Some(Ok(b)) => b,
                };
                last_rx = Instant::now();
                let f = match frame::decode_push_frame(&bytes) {
                    Ok(f) => f,
                    Err(e) => {
                        out.apply(vec![Output::DecodeError(format!("push frame: {e}"))]);
                        continue;
                    }
                };
                out.shared.counts.frames.fetch_add(1, Ordering::Relaxed);
                if f.payload_type != "msg" {
                    continue;
                }
                let batch = match frame::decode_fetch_result(&f) {
                    Ok(b) => b,
                    Err(e) => {
                        out.apply(vec![Output::DecodeError(format!("fetch result: {e}"))]);
                        continue;
                    }
                };
                if batch.need_ack
                    && let Err(e) = sock.send(frame::ack(f.log_id, &batch.internal_ext)).await
                {
                    return End::Lost(format!("ack failed: {e}"));
                }
                if out.apply(proc.handle(&batch, true, now_std())) {
                    return End::StreamEnded;
                }
            }
        }
    }
}

#[cfg(test)]
pub mod testing {
    //! In-memory transport and recording sink shared by the unit tests.

    use super::*;
    use crate::proto::ProtoMessageFetchResult;
    use crate::transport::SignedConnect;
    use std::collections::VecDeque;
    use tokio::sync::mpsc;

    #[derive(Debug, Clone, PartialEq)]
    pub enum Rec {
        Event(Event),
        Publish(String, Value),
        Signal(String, f32),
        Log(String, String),
    }

    #[derive(Default)]
    pub struct Recorder(pub Mutex<Vec<Rec>>);

    impl Sink for Recorder {
        fn emit(&self, e: Event) {
            self.0.lock().push(Rec::Event(e));
        }
        fn publish(&self, address: &str, v: Value) {
            self.0.lock().push(Rec::Publish(address.into(), v));
        }
        fn signal(&self, name: &str, v: f32) {
            self.0.lock().push(Rec::Signal(name.into(), v));
        }
        fn log(&self, level: &str, msg: String) {
            self.0.lock().push(Rec::Log(level.into(), msg));
        }
    }

    impl Recorder {
        pub fn events(&self) -> Vec<Event> {
            self.0.lock().iter().filter_map(|r| if let Rec::Event(e) = r { Some(e.clone()) } else { None }).collect()
        }
        pub fn published(&self, addr: &str) -> Vec<Value> {
            self.0.lock().iter().filter_map(|r| if let Rec::Publish(a, v) = r { (a == addr).then(|| v.clone()) } else { None }).collect()
        }
        pub fn signals(&self, name: &str) -> Vec<f32> {
            self.0.lock().iter().filter_map(|r| if let Rec::Signal(n, v) = r { (n == name).then_some(*v) } else { None }).collect()
        }
    }

    /// Scripted transport: room lookups and sign results are popped in order (the last one
    /// repeats); each opened socket is fed from a channel the test holds.
    pub struct Fake {
        pub rooms: Mutex<VecDeque<Result<RoomStatus, TransportError>>>,
        pub signs: Mutex<VecDeque<Result<SignedConnect, TransportError>>>,
        pub sockets: Mutex<VecDeque<FakeSocket>>,
        pub calls: Arc<Mutex<Vec<&'static str>>>,
    }

    pub struct FakeSocket {
        pub rx: mpsc::UnboundedReceiver<Option<Vec<u8>>>,
        pub sent: mpsc::UnboundedSender<Vec<u8>>,
        pub ping: Option<Duration>,
    }

    pub fn signed(room: &str) -> SignedConnect {
        SignedConnect {
            ws_url: "wss://push.example/ws".into(),
            cookie: "ttwid=x".into(),
            user_agent: "ua".into(),
            room_id: room.into(),
            initial: ProtoMessageFetchResult { cursor: "c".into(), ..Default::default() },
            authenticated: false,
        }
    }

    fn pop<T: Clone>(q: &Mutex<VecDeque<T>>) -> Option<T> {
        let mut q = q.lock();
        if q.len() > 1 { q.pop_front() } else { q.front().cloned() }
    }

    impl Transport for Fake {
        type Socket = FakeSocket;
        async fn room_status(&self, _: &str) -> Result<RoomStatus, TransportError> {
            self.calls.lock().push("room_status");
            pop(&self.rooms).unwrap_or(Ok(RoomStatus::Offline))
        }
        async fn sign(&self, _: &str) -> Result<SignedConnect, TransportError> {
            self.calls.lock().push("sign");
            pop(&self.signs).unwrap_or_else(|| Err(TransportError::Network("no sign scripted".into())))
        }
        async fn open(&self, _: &SignedConnect) -> Result<FakeSocket, TransportError> {
            self.calls.lock().push("open");
            self.sockets.lock().pop_front().ok_or_else(|| TransportError::Network("no socket scripted".into()))
        }
    }

    impl Socket for FakeSocket {
        async fn recv(&mut self) -> Option<Result<Vec<u8>, TransportError>> {
            self.rx.recv().await.flatten().map(Ok)
        }
        async fn send(&mut self, frame: Vec<u8>) -> Result<(), TransportError> {
            self.sent.send(frame).map_err(|_| TransportError::Network("closed".into()))
        }
        async fn close(&mut self) {}
        fn ping_interval(&self) -> Option<Duration> {
            self.ping
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::proto::*;
    use prost::Message;
    use std::collections::VecDeque;
    use tokio::sync::mpsc;

    fn settings() -> Settings {
        Settings { enabled: true, unique_id: "someone".into(), ..Default::default() }
    }

    fn out() -> (Out, Arc<Recorder>) {
        let rec = Arc::new(Recorder::default());
        (Out::new(rec.clone(), Arc::new(Shared::default())), rec)
    }

    fn frame_of<M: Message>(log_id: i64, method: &str, id: i64, m: &M) -> Vec<u8> {
        let batch = ProtoMessageFetchResult {
            messages: vec![BaseProtoMessage { method: method.into(), payload: m.encode_to_vec(), msg_id: id, ..Default::default() }],
            need_ack: true,
            internal_ext: format!("ext{log_id}"),
            ..Default::default()
        };
        frame::msg_frame(log_id, &batch, true)
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn live_session_joins_acks_heartbeats_and_ends_offline() {
        let (tx_in, rx_in) = mpsc::unbounded_channel();
        let (tx_out, mut rx_out) = mpsc::unbounded_channel();
        let fake = Fake {
            rooms: Mutex::new(VecDeque::from([Ok(RoomStatus::Live { room_id: "7140000000000000001".into() })])),
            signs: Mutex::new(VecDeque::from([Ok(signed("7140000000000000001"))])),
            sockets: Mutex::new(VecDeque::from([FakeSocket { rx: rx_in, sent: tx_out, ping: None }])),
            calls: Arc::default(),
        };
        let (o, rec) = out();
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(run(fake, settings(), o.clone(), stop_rx));
        settle().await;
        assert_eq!(o.state(), LinkState::Connected);
        let first = frame::decode_push_frame(&rx_out.recv().await.unwrap()).unwrap();
        assert_eq!(first.payload_type, "im_enter_room");
        let hb = frame::decode_push_frame(&rx_out.recv().await.unwrap()).unwrap();
        assert_eq!(hb.payload_type, "hb");

        let chat =
            WebcastChatMessage { user: Some(User { id: 1, nickname: "Ann".into(), ..Default::default() }), content: "hello".into(), ..Default::default() };
        tx_in.send(Some(frame_of(555, "WebcastChatMessage", 1, &chat))).unwrap();
        settle().await;
        let ack = frame::decode_push_frame(&rx_out.recv().await.unwrap()).unwrap();
        assert_eq!((ack.payload_type.as_str(), ack.log_id, ack.payload.as_slice()), ("ack", 555, b"ext555".as_slice()));
        let ev = rec.events();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].ty, "tiktok.chat");
        assert_eq!(ev[0].payload.get_path("message").and_then(Value::as_str), Some("hello"));

        // heartbeats keep coming at the default interval
        tokio::time::advance(HEARTBEAT).await;
        settle().await;
        let hb2 = frame::decode_push_frame(&rx_out.recv().await.unwrap()).unwrap();
        assert_eq!(HeartBeatMessage::decode(hb2.payload.as_slice()).unwrap().send_packet_seq_id, 2);

        tx_in.send(Some(frame_of(556, "WebcastControlMessage", 2, &WebcastControlMessage { action: CONTROL_ENDED, ..Default::default() }))).unwrap();
        settle().await;
        assert_eq!(o.state(), LinkState::Offline);
        assert_eq!(rec.published("tiktok.connected"), vec![Value::Bool(true), Value::Bool(false)]);
        stop_tx.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn offline_polls_slowly_and_errors_back_off() {
        let fake = Fake {
            rooms: Mutex::new(VecDeque::from([
                Err(TransportError::Network("dns".into())),
                Err(TransportError::Network("dns".into())),
                Ok(RoomStatus::Offline),
            ])),
            signs: Mutex::new(VecDeque::new()),
            sockets: Mutex::new(VecDeque::new()),
            calls: Arc::default(),
        };
        let calls = fake.calls.clone();
        let (o, rec) = out();
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(run(fake, settings(), o.clone(), stop_rx));
        settle().await;
        assert_eq!(o.state(), LinkState::Backoff);
        // first retry ≤ 2 s, second ≤ 4 s
        tokio::time::advance(Duration::from_secs(2)).await;
        settle().await;
        tokio::time::advance(Duration::from_secs(4)).await;
        settle().await;
        assert_eq!(o.state(), LinkState::Offline);
        assert_eq!(calls.lock().len(), 3);
        // offline: next lookup only after poll_offline (60 s)
        tokio::time::advance(Duration::from_secs(59)).await;
        settle().await;
        assert_eq!(calls.lock().len(), 3);
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(calls.lock().len(), 4);
        assert!(calls.lock().iter().all(|c| *c == "room_status"));
        let health = rec.published("health.tiktok");
        assert_eq!(health.last().and_then(|h| h.get_path("status")).and_then(Value::as_str), Some("warn"));
        stop_tx.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limit_waits_for_retry_after_and_auth_failure_is_a_health_fail() {
        let fake = Fake {
            rooms: Mutex::new(VecDeque::from([Ok(RoomStatus::Live { room_id: "1".into() })])),
            signs: Mutex::new(VecDeque::from([
                Err(TransportError::RateLimited { message: "too many".into(), retry_after: Some(Duration::from_secs(120)) }),
                Err(TransportError::Auth("invalid key".into())),
            ])),
            sockets: Mutex::new(VecDeque::new()),
            calls: Arc::default(),
        };
        let calls = fake.calls.clone();
        let (o, rec) = out();
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(run(fake, settings(), o.clone(), stop_rx));
        settle().await;
        assert_eq!(calls.lock().as_slice(), &["room_status", "sign"]);
        tokio::time::advance(Duration::from_secs(119)).await;
        settle().await;
        assert_eq!(calls.lock().len(), 2);
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(calls.lock().as_slice(), &["room_status", "sign", "room_status", "sign"]);
        assert_eq!(o.state(), LinkState::Failed);
        let h = rec.published("health.tiktok").pop().unwrap();
        assert_eq!(h.get_path("status").and_then(Value::as_str), Some("fail"));
        assert!(h.get_path("detail").and_then(Value::as_str).unwrap().contains("invalid key"));
        stop_tx.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn silent_socket_is_dropped_and_pending_streak_flushed() {
        let (tx_in, rx_in) = mpsc::unbounded_channel();
        let (tx_out, _rx_out) = mpsc::unbounded_channel();
        let fake = Fake {
            rooms: Mutex::new(VecDeque::from([Ok(RoomStatus::Live { room_id: "9".into() })])),
            signs: Mutex::new(VecDeque::from([Ok(signed("9"))])),
            sockets: Mutex::new(VecDeque::from([FakeSocket { rx: rx_in, sent: tx_out, ping: Some(Duration::from_secs(5)) }])),
            calls: Arc::default(),
        };
        let (o, rec) = out();
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(run(fake, settings(), o.clone(), stop_rx));
        settle().await;
        let g = WebcastGiftMessage {
            gift_id: 1,
            repeat_count: 4,
            user: Some(User { id: 3, nickname: "G".into(), ..Default::default() }),
            gift: Some(Gift { id: 1, r#type: 1, diamond_count: 10, name: "Rose".into(), ..Default::default() }),
            ..Default::default()
        };
        tx_in.send(Some(frame_of(1, "WebcastGiftMessage", 1, &g))).unwrap();
        settle().await;
        assert!(rec.events().is_empty());
        // the streak idles out after 10 s and is emitted once
        tokio::time::advance(Duration::from_secs(11)).await;
        settle().await;
        let ev = rec.events();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].payload.get_path("diamonds").and_then(Value::as_i64), Some(40));
        tokio::time::advance(IDLE_TIMEOUT).await;
        settle().await;
        assert_eq!(o.state(), LinkState::Backoff);
        assert_eq!(o.shared.counts.reconnects.load(Ordering::Relaxed), 1);
        stop_tx.send(true).unwrap();
        task.await.unwrap();
    }
}
