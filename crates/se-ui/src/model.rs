//! Client-side mirror of engine state, events, logs, and query results.

use se_client::{Live, LiveEvent};
use se_proto::wire::{ServerMsg, StateEntry, Subscription, TraceRec};
use se_proto::{Event, Meta, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Instant;

pub const EVENT_CAP: usize = 500;
pub const LOG_CAP: usize = 2000;
pub const TRACE_CAP: usize = 5000;
/// Samples of history kept per signal (30 Hz push → ~8.5 s).
pub const SIGNAL_HISTORY: usize = 256;

#[derive(Clone, Debug)]
pub struct LogLine {
    pub level: String,
    pub target: String,
    pub msg: String,
    pub ts: u64,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub error: bool,
    pub at: Instant,
}

/// What an outstanding request id is waiting for.
#[derive(Clone, Debug)]
enum Pending {
    /// Store the reply under this key in `queries`.
    Query(String),
    Cmd(String),
    Trace,
    Explain,
    Values,
}

pub struct Model {
    pub live: Live,
    pub connected: bool,
    pub session: String,
    pub engine_pid: u32,
    pub last_disconnect: Option<String>,
    /// Subscribed state (pushed by the engine).
    pub state: BTreeMap<String, Value>,
    /// Values fetched on demand (`Get`) for addresses outside the subscription (inspector).
    pub fetched: BTreeMap<String, Value>,
    /// Metadata by address (fetched on demand with `Get { meta: true }`).
    pub meta: HashMap<String, Meta>,
    pub events: VecDeque<Event>,
    /// Total events received (monotonic; lets views notice new ones cheaply).
    pub events_seen: u64,
    pub logs: VecDeque<LogLine>,
    pub trace: VecDeque<TraceRec>,
    pub signals: HashMap<String, VecDeque<f32>>,
    pub queries: HashMap<String, Value>,
    /// Last query error per key (for empty states that explain themselves).
    pub query_errors: HashMap<String, String>,
    /// Replies received per key (lets views rebuild caches only when a new reply arrived).
    query_seq: HashMap<String, u64>,
    pending: HashMap<u64, Pending>,
    pub toasts: Vec<Toast>,
    pub last_refresh: Instant,
    pub trace_view: Vec<TraceRec>,
    pub explain: Option<se_proto::wire::Provenance>,
    /// Change counter per first address segment (`queue`, `controllers`, …).
    prefix_gen: HashMap<String, u64>,
    /// Connection generation (increments on every (re)connect).
    pub conn_gen: u64,
}

pub fn subscription() -> Subscription {
    Subscription {
        events: vec!["**".into()],
        state: vec![
            "show.**".into(),
            "scene.**".into(),
            "preset.**".into(),
            "project.**".into(),
            "system.**".into(),
            "obs.**".into(),
            "stream.**".into(),
            "health.**".into(),
            "perf.**".into(),
            "audio.**".into(),
            "mixer.**".into(),
            "queue.**".into(),
            "song.**".into(),
            "twitch.**".into(),
            "alerts.**".into(),
            "policy.**".into(),
            "mod.**".into(),
            "devices.**".into(),
            "source.**".into(),
            "lights.**".into(),
            "timeline.**".into(),
            "fx.**".into(),
            "patch.**".into(),
            "stats.**".into(),
            "goals.**".into(),
            "clips.**".into(),
            "controllers.**".into(),
            "render.**".into(),
            "accounts.**".into(),
            "setup.**".into(),
            "giveaway.**".into(),
            "tts.**".into(),
            "retention.**".into(),
            "remote_mod.**".into(),
        ],
        signals: vec!["**".into()],
        signal_hz: Some(30.0),
        logs: true,
        trace: true,
    }
}

impl Model {
    pub fn new(socket: Option<std::path::PathBuf>) -> Model {
        Model {
            live: Live::start(socket, "ui", subscription()),
            connected: false,
            session: String::new(),
            engine_pid: 0,
            last_disconnect: None,
            state: BTreeMap::new(),
            fetched: BTreeMap::new(),
            meta: HashMap::new(),
            events: VecDeque::new(),
            events_seen: 0,
            logs: VecDeque::new(),
            trace: VecDeque::new(),
            signals: HashMap::new(),
            queries: HashMap::new(),
            query_errors: HashMap::new(),
            query_seq: HashMap::new(),
            pending: HashMap::new(),
            toasts: Vec::new(),
            last_refresh: Instant::now() - std::time::Duration::from_secs(60),
            trace_view: Vec::new(),
            explain: None,
            prefix_gen: HashMap::new(),
            conn_gen: 0,
        }
    }

    /// Subscribed value, else the last fetched one.
    pub fn get(&self, a: &str) -> Option<&Value> {
        self.state.get(a).or_else(|| self.fetched.get(a))
    }
    pub fn str(&self, a: &str) -> &str {
        self.get(a).and_then(Value::as_str).unwrap_or("")
    }
    pub fn f(&self, a: &str) -> f64 {
        self.get(a).and_then(Value::as_f64).unwrap_or(0.0)
    }
    pub fn b(&self, a: &str) -> bool {
        self.get(a).is_some_and(Value::truthy)
    }
    pub fn has(&self, a: &str) -> bool {
        self.get(a).is_some()
    }
    pub fn q(&self, name: &str) -> Option<&Value> {
        self.queries.get(name)
    }
    pub fn q_list(&self, name: &str) -> &[Value] {
        self.q(name).and_then(Value::as_list).unwrap_or(&[])
    }
    /// Number of replies received for `key` so far.
    pub fn q_seq(&self, key: &str) -> u64 {
        self.query_seq.get(key).copied().unwrap_or(0)
    }
    /// Latest sample of a signal.
    pub fn sig(&self, name: &str) -> Option<f32> {
        self.signals.get(name).and_then(|h| h.back().copied())
    }

    /// Subscribed addresses under `prefix.` (sorted).
    pub fn under<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = (&'a String, &'a Value)> + 'a {
        let p = format!("{prefix}.");
        self.state.range(p.clone()..).take_while(move |(a, _)| a.starts_with(&p))
    }

    /// Change counter for the first address segment (`queue` for `queue.length`).
    pub fn gen_of(&self, segment: &str) -> u64 {
        self.prefix_gen.get(segment).copied().unwrap_or(0)
    }

    pub fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toasts.push(Toast { text: text.into(), error, at: Instant::now() });
    }

    pub fn query(&mut self, name: &str, args: Value) {
        self.query_as(name, name, args);
    }

    /// Run a query and store the reply under `key` (several results of one query side by side).
    pub fn query_as(&mut self, key: &str, name: &str, args: Value) {
        let req = self.live.query(name, args);
        self.pending.insert(req, Pending::Query(key.to_string()));
    }

    pub fn text(&mut self, cmd: &str) {
        let req = self.live.text(cmd);
        self.pending.insert(req, Pending::Cmd(cmd.to_string()));
    }

    pub fn command(&mut self, op: se_proto::Op) {
        let label = op.describe();
        let req = self.live.command(se_proto::Command::new(se_proto::Origin::Ui, op));
        self.pending.insert(req, Pending::Cmd(label));
    }

    /// Subsystem action (`midi.learn`, `project.write`, `queue.approve`, …).
    pub fn action(&mut self, name: &str, args: Value) {
        self.command(se_proto::Op::Action { name: name.into(), args });
    }

    pub fn trace_of(&mut self, id: u64) {
        let req = self.live.req();
        self.live.send(se_proto::wire::ClientMsg::Trace { req, id });
        self.pending.insert(req, Pending::Trace);
    }

    pub fn explain_of(&mut self, address: &str) {
        let req = self.live.req();
        self.live.send(se_proto::wire::ClientMsg::Explain { req, address: address.into() });
        self.pending.insert(req, Pending::Explain);
    }

    /// Fetch values + metadata for `pattern` into `fetched`/`meta`.
    pub fn fetch(&mut self, pattern: &str) {
        let req = self.live.req();
        self.live.send(se_proto::wire::ClientMsg::Get { req, pattern: pattern.into(), meta: true });
        self.pending.insert(req, Pending::Values);
    }

    /// Periodic refresh of query-backed views.
    pub fn refresh(&mut self, names: &[&str]) {
        if self.connected && self.last_refresh.elapsed().as_millis() > 500 {
            self.last_refresh = Instant::now();
            for n in names {
                self.query(n, Value::Null);
            }
        }
    }

    /// Force the next `refresh` to run immediately.
    pub fn refresh_soon(&mut self) {
        self.last_refresh = Instant::now() - std::time::Duration::from_secs(60);
    }

    /// Drain the connection; returns true when anything changed.
    pub fn pump(&mut self) -> bool {
        let mut any = false;
        while let Ok(ev) = self.live.events.try_recv() {
            any = true;
            match ev {
                LiveEvent::Connected { session, pid } => {
                    self.connected = true;
                    self.session = session;
                    self.engine_pid = pid;
                    self.last_disconnect = None;
                    self.conn_gen += 1;
                    self.pending.clear();
                    self.refresh_soon();
                    self.toast("connected to engine", false);
                }
                LiveEvent::Disconnected { reason } => {
                    if self.connected {
                        self.toast(format!("engine disconnected: {reason}"), true);
                    }
                    self.connected = false;
                    self.last_disconnect = Some(reason);
                }
                LiveEvent::Msg(m) => self.apply(m),
            }
        }
        self.toasts.retain(|t| t.at.elapsed().as_secs() < 6);
        any
    }

    fn bump(&mut self, address: &str) {
        let seg = address.split('.').next().unwrap_or(address);
        if let Some(g) = self.prefix_gen.get_mut(seg) {
            *g += 1;
        } else {
            self.prefix_gen.insert(seg.to_string(), 1);
        }
    }

    pub(crate) fn apply(&mut self, m: ServerMsg) {
        match m {
            ServerMsg::State { changes } => {
                for (a, v) in changes {
                    self.bump(&a);
                    self.fetched.remove(&a);
                    self.state.insert(a, v);
                }
            }
            ServerMsg::Values { req, entries } => {
                self.pending.remove(&req);
                self.store_values(entries);
            }
            ServerMsg::Event { event } => {
                if self.events.len() >= EVENT_CAP {
                    self.events.pop_front();
                }
                if matches!(event.ty.as_str(), "scene.changed" | "project.reloaded") || event.ty.starts_with("preset.") {
                    self.refresh_soon();
                }
                self.events_seen += 1;
                self.events.push_back(event);
            }
            ServerMsg::Signals { values, .. } => {
                for (n, v) in values {
                    let h = self.signals.entry(n).or_default();
                    if h.len() >= SIGNAL_HISTORY {
                        h.pop_front();
                    }
                    h.push_back(v);
                }
            }
            ServerMsg::Log { level, target, msg, ts } => {
                if level == "error" {
                    self.toast(msg.clone(), true);
                }
                if self.logs.len() >= LOG_CAP {
                    self.logs.pop_front();
                }
                self.logs.push_back(LogLine { level, target, msg, ts });
            }
            ServerMsg::Trace { req: None, records } => {
                for r in records {
                    if self.trace.len() >= TRACE_CAP {
                        self.trace.pop_front();
                    }
                    self.trace.push_back(r);
                }
            }
            ServerMsg::Trace { req: Some(req), records } => {
                self.pending.remove(&req);
                self.trace_view = records;
            }
            ServerMsg::Explain { req, provenance } => {
                self.pending.remove(&req);
                self.explain = provenance;
            }
            ServerMsg::Reply { req, value, error } => {
                if let Some(Pending::Query(key)) = self.pending.remove(&req) {
                    match error {
                        Some(e) => {
                            tracing::debug!("query {key}: {e}");
                            self.query_errors.insert(key, e);
                        }
                        None => {
                            self.query_errors.remove(&key);
                            *self.query_seq.entry(key.clone()).or_default() += 1;
                            self.queries.insert(key, value);
                        }
                    }
                }
            }
            ServerMsg::Ack { req, ok, error, .. } => {
                let label = match req.and_then(|r| self.pending.remove(&r)) {
                    Some(Pending::Cmd(l)) => l,
                    _ => String::new(),
                };
                if !ok {
                    self.toast(format!("{label}: {}", error.unwrap_or_default()), true);
                }
                self.last_refresh = Instant::now() - std::time::Duration::from_millis(400);
            }
            ServerMsg::Error { msg } => self.toast(msg, true),
            _ => {}
        }
    }

    fn store_values(&mut self, entries: Vec<StateEntry>) {
        for e in entries {
            if let Some(m) = e.meta {
                self.meta.insert(e.address.clone(), m);
            }
            match self.state.get_mut(&e.address) {
                Some(v) => *v = e.value,
                None => {
                    self.fetched.insert(e.address, e.value);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        // A socket path that never exists: the client just keeps retrying in the background.
        Model::new(Some(std::path::PathBuf::from("/nonexistent/se-ui-test.sock")))
    }

    #[test]
    fn state_changes_bump_segment_generations_and_shadow_fetched_values() {
        let mut m = model();
        m.apply(ServerMsg::Values {
            req: 1,
            entries: vec![StateEntry { address: "band.gain".into(), value: Value::Float(0.5), meta: Some(Meta::float(1.0, [0.0, 2.0])) }],
        });
        assert_eq!(m.f("band.gain"), 0.5);
        assert_eq!(m.meta["band.gain"].range, Some([0.0, 2.0]));
        let g = m.gen_of("queue");
        m.apply(ServerMsg::State { changes: vec![("queue.length".into(), Value::Int(3)), ("band.gain".into(), Value::Float(0.7))] });
        assert_eq!(m.gen_of("queue"), g + 1);
        assert_eq!(m.f("band.gain"), 0.7);
        assert!(!m.fetched.contains_key("band.gain"), "pushed state supersedes the fetched copy");
        assert_eq!(m.under("queue").count(), 1);
    }

    #[test]
    fn replies_land_under_their_key_and_errors_are_kept() {
        let mut m = model();
        m.query_as("project.read:rules/a.toml", "project.read", Value::Null);
        let (req, _) = m.pending.iter().next().map(|(r, p)| (*r, p.clone())).unwrap();
        m.apply(ServerMsg::Reply { req, value: Value::map().with("text", "x = 1"), error: None });
        assert_eq!(m.q("project.read:rules/a.toml").and_then(|v| v.get_path("text")).and_then(Value::as_str), Some("x = 1"));
        m.query("nope", Value::Null);
        let req = *m.pending.keys().next().unwrap();
        m.apply(ServerMsg::Reply { req, value: Value::Null, error: Some("unknown query `nope`".into()) });
        assert!(m.query_errors["nope"].contains("unknown"));
    }

    #[test]
    fn signal_history_is_bounded() {
        let mut m = model();
        for i in 0..(SIGNAL_HISTORY + 10) {
            m.apply(ServerMsg::Signals { ts: 0, values: vec![("lfo.slow".into(), i as f32)] });
        }
        assert_eq!(m.signals["lfo.slow"].len(), SIGNAL_HISTORY);
        assert_eq!(m.sig("lfo.slow"), Some((SIGNAL_HISTORY + 9) as f32));
    }
}
