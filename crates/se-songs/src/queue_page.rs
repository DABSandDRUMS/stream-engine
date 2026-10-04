//! `health.queue_page`: is the public queue page (`queue.url`, served by the local queue
//! service behind the Cloudflare tunnel, or by the Worker relay) reachable from the internet?
//!
//! `health.relay` only covers the engine → relay WebSocket. A dead tunnel or broken DNS leaves
//! that link green while viewers get an error page, so this task fetches `<origin>/queue.json`
//! through the public hostname every minute. It runs on its own task: a slow or hanging network
//! never stalls the song actor. `queue.url` is re-read from the hub snapshot every cycle.

use se_hub::Hub;
use se_proto::{Meta, Value, ValueType};
use std::sync::Arc;
use std::time::Duration;

pub const KEY: &str = "health.queue_page";
const EVERY: Duration = Duration::from_secs(60);
const TIMEOUT: Duration = Duration::from_secs(10);
/// Let the song actor publish `queue.url` (and the tunnel come up at boot) before the first probe.
const FIRST_AFTER: Duration = Duration::from_secs(15);
/// Consecutive failed probes before the check turns `fail` (one or two are `warn`).
pub const FAIL_AFTER: u32 = 3;
const STILL_WORKS: &str = "song requests in chat still work";

/// What one probe of the public page found.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// `queue.url` empty or not `https://`: nothing public to check.
    Unconfigured,
    /// HTTP 200 with `{online: true}`.
    Online,
    /// The page answered but says the engine is not connected to it.
    EngineOffline,
    /// Unreachable, wrong status, or not the queue page; plain-language reason.
    Failed(String),
}

/// The URL to probe and the host to name in details, from a `queue.url` value.
pub fn target(queue_url: &str) -> Option<(String, String)> {
    let url = reqwest::Url::parse(queue_url.trim()).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    let host = url.host_str()?.to_string();
    let origin = url.origin().ascii_serialization();
    Some((host, format!("{origin}/queue.json")))
}

/// Classify a completed HTTP response.
pub fn classify(status: u16, body: &str) -> Outcome {
    if status != 200 {
        let hint = match status {
            502..=504 | 530 => " (the tunnel or the local queue service is down)",
            404 => " (the hostname does not serve the queue page)",
            _ => "",
        };
        return Outcome::Failed(format!("HTTP {status}{hint}"));
    }
    match serde_json::from_str::<serde_json::Value>(body).ok().and_then(|j| j.get("online").and_then(serde_json::Value::as_bool)) {
        Some(true) => Outcome::Online,
        Some(false) => Outcome::EngineOffline,
        None => Outcome::Failed("the address answered, but not with the queue page".into()),
    }
}

fn describe_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        format!("no answer within {} s", TIMEOUT.as_secs())
    } else if e.is_connect() {
        "could not connect (DNS lookup or connection failed)".into()
    } else {
        "the request failed".into()
    }
}

/// Turns probe outcomes into the published `{status, detail}`.
#[derive(Debug, Default)]
pub struct Tracker {
    failures: u32,
}

impl Tracker {
    pub fn observe(&mut self, host: &str, outcome: &Outcome) -> (&'static str, String) {
        match outcome {
            Outcome::Unconfigured => {
                self.failures = 0;
                ("pass", "public queue page not configured".into())
            }
            Outcome::Online => {
                self.failures = 0;
                ("pass", format!("public queue page reachable ({host})"))
            }
            Outcome::EngineOffline => {
                self.failures = 0;
                (
                    "warn",
                    format!(
                        "public queue page ({host}) is up but shows the stream as offline: the engine's link to the queue service is down (see the relay check); {STILL_WORKS}"
                    ),
                )
            }
            Outcome::Failed(reason) => {
                self.failures = self.failures.saturating_add(1);
                if self.failures >= FAIL_AFTER {
                    (
                        "fail",
                        format!(
                            "viewers cannot open the public queue page ({host}): {reason}, {} checks in a row; check the tunnel or DNS (stream-engine-queue-tunnel.service, stream-engine-queue.service); {STILL_WORKS}",
                            self.failures
                        ),
                    )
                } else {
                    ("warn", format!("public queue page ({host}) did not answer: {reason}; checking again in a minute; {STILL_WORKS}"))
                }
            }
        }
    }
}

/// Declare `health.queue_page` and start the probe task.
pub fn spawn(hub: Arc<Hub>) {
    hub.declare(
        KEY,
        Meta { ty: ValueType::Map, readonly: true, ..Default::default() }.owner("songs").describe("Public queue page reachable from the internet"),
    );
    tokio::spawn(run(hub));
}

async fn run(hub: Arc<Hub>) {
    let mut http: Option<reqwest::Client> = None;
    let mut tracker = Tracker::default();
    let mut last_status: Option<&'static str> = None;
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + FIRST_AFTER, EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let queue_url = hub.snapshot.load().get("queue.url").and_then(Value::as_str).map(str::to_string).unwrap_or_default();
        let (host, outcome) = match target(&queue_url) {
            None => (String::new(), Outcome::Unconfigured),
            Some((host, url)) => {
                if http.is_none() {
                    http = reqwest::Client::builder()
                        .timeout(TIMEOUT)
                        .connect_timeout(TIMEOUT)
                        .user_agent(concat!("stream-engine/", env!("CARGO_PKG_VERSION"), " (queue page check)"))
                        .build()
                        .map_err(|e| tracing::warn!("queue page check: HTTP client: {e}"))
                        .ok();
                }
                let outcome = match &http {
                    None => Outcome::Failed("the engine could not create an HTTP client".into()),
                    Some(c) => probe(c, &url).await,
                };
                (host, outcome)
            }
        };
        let (status, detail) = tracker.observe(&host, &outcome);
        if last_status != Some(status) {
            if last_status.is_some() || status != "pass" {
                let level = if status == "pass" { "info" } else { "warn" };
                hub.log(level, "relay", format!("queue page: {detail}"));
            }
            last_status = Some(status);
        }
        hub.publish(KEY, Value::map().with("status", status).with("detail", detail));
    }
}

async fn probe(http: &reqwest::Client, url: &str) -> Outcome {
    let resp = match http.get(url).send().await {
        Ok(r) => r,
        Err(e) => return Outcome::Failed(describe_error(&e)),
    };
    let status = resp.status().as_u16();
    match resp.text().await {
        Ok(body) => classify(status, &body),
        Err(e) => Outcome::Failed(describe_error(&e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_uses_the_origin_and_requires_https() {
        assert_eq!(target("https://queue.dabsanddrums.com/queue"), Some(("queue.dabsanddrums.com".into(), "https://queue.dabsanddrums.com/queue.json".into())));
        assert_eq!(target("https://q.example.com:8443/x/queue?a=1").map(|t| t.1), Some("https://q.example.com:8443/queue.json".into()));
        assert_eq!(target(""), None);
        assert_eq!(target("http://127.0.0.1:8787/queue"), None);
        assert_eq!(target("not a url"), None);
    }

    #[test]
    fn only_online_true_json_is_healthy() {
        assert_eq!(classify(200, r#"{"online":true,"snapshot":null}"#), Outcome::Online);
        assert_eq!(classify(200, r#"{"online":false,"snapshot":null}"#), Outcome::EngineOffline);
        assert!(matches!(classify(200, "<html>Cloudflare</html>"), Outcome::Failed(_)));
        assert!(matches!(classify(200, r#"{"snapshot":{}}"#), Outcome::Failed(_)));
        assert!(matches!(classify(530, r#"{"online":true}"#), Outcome::Failed(r) if r.contains("tunnel")));
        assert!(matches!(classify(404, ""), Outcome::Failed(_)));
    }

    #[test]
    fn warns_on_first_failure_and_fails_on_the_third_in_a_row() {
        let mut t = Tracker::default();
        let bad = Outcome::Failed("HTTP 530".into());
        assert_eq!(t.observe("q.example", &Outcome::Online).0, "pass");
        assert_eq!(t.observe("q.example", &bad).0, "warn");
        assert_eq!(t.observe("q.example", &bad).0, "warn");
        let (st, detail) = t.observe("q.example", &bad);
        assert_eq!(st, "fail");
        assert!(detail.contains("tunnel or DNS") && detail.contains("chat still work"), "{detail}");
        assert_eq!(t.observe("q.example", &bad).0, "fail");
        // One good probe clears the streak; the next failure starts over at warn.
        assert_eq!(t.observe("q.example", &Outcome::Online), ("pass", "public queue page reachable (q.example)".to_string()));
        assert_eq!(t.observe("q.example", &bad).0, "warn");
    }

    #[test]
    fn engine_offline_warns_and_resets_the_failure_streak() {
        let mut t = Tracker::default();
        let bad = Outcome::Failed("no answer".into());
        t.observe("q", &bad);
        t.observe("q", &bad);
        let (st, detail) = t.observe("q", &Outcome::EngineOffline);
        assert_eq!(st, "warn");
        assert!(detail.contains("offline"), "{detail}");
        // The page itself answered, so the next outage starts from the first strike.
        assert_eq!(t.observe("q", &bad).0, "warn");
    }

    #[test]
    fn unconfigured_passes_and_resets() {
        let mut t = Tracker::default();
        let bad = Outcome::Failed("x".into());
        t.observe("q", &bad);
        t.observe("q", &bad);
        assert_eq!(t.observe("", &Outcome::Unconfigured), ("pass", "public queue page not configured".to_string()));
        assert_eq!(t.observe("q", &bad).0, "warn");
    }
}
