//! Client-side mirror of engine state, events, logs, and query results.

use se_client::{Live, LiveEvent};
use se_proto::wire::{ServerMsg, Subscription, TraceRec};
use se_proto::{Event, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Instant;

pub const EVENT_CAP: usize = 500;
pub const LOG_CAP: usize = 2000;
pub const TRACE_CAP: usize = 5000;

#[derive(Clone, Debug)]
pub struct LogLine {
    pub level: String,
    pub target: String,
    pub msg: String,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub error: bool,
    pub at: Instant,
}

pub struct Model {
    pub live: Live,
    pub connected: bool,
    pub session: String,
    pub engine_pid: u32,
    pub last_disconnect: Option<String>,
    pub state: BTreeMap<String, Value>,
    pub events: VecDeque<Event>,
    pub logs: VecDeque<LogLine>,
    pub trace: VecDeque<TraceRec>,
    pub signals: HashMap<String, VecDeque<f32>>,
    pub queries: HashMap<String, Value>,
    pending: HashMap<u64, String>,
    pub toasts: Vec<Toast>,
    pub last_refresh: Instant,
    pub trace_view: Vec<TraceRec>,
    pub explain: Option<se_proto::wire::Provenance>,
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
            "audio.bus.**".into(),
            "mixer.**".into(),
            "queue.**".into(),
            "song.**".into(),
            "twitch.**".into(),
            "alerts.**".into(),
            "devices.**".into(),
            "lights.**".into(),
            "timeline.**".into(),
            "fx.**".into(),
            "patch.**".into(),
            "stats.**".into(),
            "goals.**".into(),
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
            events: VecDeque::new(),
            logs: VecDeque::new(),
            trace: VecDeque::new(),
            signals: HashMap::new(),
            queries: HashMap::new(),
            pending: HashMap::new(),
            toasts: Vec::new(),
            last_refresh: Instant::now() - std::time::Duration::from_secs(60),
            trace_view: Vec::new(),
            explain: None,
        }
    }

    pub fn get(&self, a: &str) -> Option<&Value> {
        self.state.get(a)
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
    pub fn q(&self, name: &str) -> Option<&Value> {
        self.queries.get(name)
    }
    pub fn q_list(&self, name: &str) -> &[Value] {
        self.q(name).and_then(Value::as_list).unwrap_or(&[])
    }

    pub fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toasts.push(Toast { text: text.into(), error, at: Instant::now() });
    }

    pub fn query(&mut self, name: &str, args: Value) {
        let req = self.live.query(name, args);
        self.pending.insert(req, name.to_string());
    }

    pub fn text(&mut self, cmd: &str) {
        let req = self.live.text(cmd);
        self.pending.insert(req, format!("cmd:{cmd}"));
    }

    pub fn command(&mut self, op: se_proto::Op) {
        let label = op.describe();
        let req = self.live.command(se_proto::Command::new(se_proto::Origin::Ui, op));
        self.pending.insert(req, format!("cmd:{label}"));
    }

    pub fn trace_of(&mut self, id: u64) {
        let req = self.live.req();
        self.live.send(se_proto::wire::ClientMsg::Trace { req, id });
        self.pending.insert(req, "trace".into());
    }

    pub fn explain_of(&mut self, address: &str) {
        let req = self.live.req();
        self.live.send(se_proto::wire::ClientMsg::Explain { req, address: address.into() });
        self.pending.insert(req, "explain".into());
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
                    self.last_refresh = Instant::now() - std::time::Duration::from_secs(60);
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

    fn apply(&mut self, m: ServerMsg) {
        match m {
            ServerMsg::State { changes } => {
                for (a, v) in changes {
                    self.state.insert(a, v);
                }
            }
            ServerMsg::Event { event } => {
                if self.events.len() >= EVENT_CAP {
                    self.events.pop_front();
                }
                if matches!(event.ty.as_str(), "preset.released" | "scene.changed" | "project.reloaded") || event.ty.starts_with("preset.") {
                    self.last_refresh = Instant::now() - std::time::Duration::from_secs(60);
                }
                self.events.push_back(event);
            }
            ServerMsg::Signals { values, .. } => {
                for (n, v) in values {
                    let h = self.signals.entry(n).or_default();
                    if h.len() >= 256 {
                        h.pop_front();
                    }
                    h.push_back(v);
                }
            }
            ServerMsg::Log { level, target, msg, .. } => {
                if level == "error" {
                    self.toast(msg.clone(), true);
                }
                if self.logs.len() >= LOG_CAP {
                    self.logs.pop_front();
                }
                self.logs.push_back(LogLine { level, target, msg });
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
                if let Some(name) = self.pending.remove(&req) {
                    match error {
                        Some(e) => tracing::debug!("query {name}: {e}"),
                        None => {
                            self.queries.insert(name, value);
                        }
                    }
                }
            }
            ServerMsg::Ack { req, ok, error, .. } => {
                let label = req.and_then(|r| self.pending.remove(&r)).unwrap_or_default();
                if !ok {
                    self.toast(format!("{}: {}", label.trim_start_matches("cmd:"), error.unwrap_or_default()), true);
                }
                self.last_refresh = Instant::now() - std::time::Duration::from_millis(400);
            }
            ServerMsg::Error { msg } => self.toast(msg, true),
            _ => {}
        }
    }
}
