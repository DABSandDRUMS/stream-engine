//! Helix REST client: bearer + Client-Id headers, token refresh on 401, rate-limit backoff on
//! 429 (`Ratelimit-Reset`), one retry on 5xx.

use crate::auth::{Account, Auth, AuthError};
use serde_json::Value as J;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct HelixError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for HelixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.status == 0 { f.write_str(&self.message) } else { write!(f, "Helix {}: {}", self.status, self.message) }
    }
}

impl std::error::Error for HelixError {}

impl From<AuthError> for HelixError {
    fn from(e: AuthError) -> Self {
        HelixError { status: if matches!(e, AuthError::Revoked(_)) { 401 } else { 0 }, message: e.to_string() }
    }
}

fn net(e: impl std::fmt::Display) -> HelixError {
    HelixError { status: 0, message: e.to_string() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Patch,
    Put,
    Delete,
}

pub struct Helix {
    http: reqwest::Client,
    auth: Arc<Auth>,
    base: parking_lot::Mutex<String>,
    /// Points left in the current rate-limit bucket (−1 = unknown).
    pub remaining: AtomicI64,
}

impl Helix {
    pub fn new(http: reqwest::Client, auth: Arc<Auth>, base: String) -> Helix {
        Helix { http, auth, base: parking_lot::Mutex::new(base), remaining: AtomicI64::new(-1) }
    }

    pub fn set_base(&self, base: String) {
        *self.base.lock() = base;
    }

    /// Call `path` (e.g. `/chat/messages`) as `account`. Returns the JSON body (`null` for 204).
    pub async fn call(&self, account: Account, method: Method, path: &str, query: &[(&str, String)], body: Option<&J>) -> Result<J, HelixError> {
        let url = if path.starts_with("http") { path.to_string() } else { format!("{}{path}", self.base.lock()) };
        self.call_url(account, method, &url, query, body).await
    }

    /// Same as [`Helix::call`] with an absolute URL (EventSub subscriptions on a mock server).
    pub async fn call_url(&self, account: Account, method: Method, url: &str, query: &[(&str, String)], body: Option<&J>) -> Result<J, HelixError> {
        let mut refreshed = false;
        let mut retried = false;
        loop {
            let token = self.auth.access(account).await?;
            let mut rb = match method {
                Method::Get => self.http.get(url),
                Method::Post => self.http.post(url),
                Method::Patch => self.http.patch(url),
                Method::Put => self.http.put(url),
                Method::Delete => self.http.delete(url),
            };
            rb = rb.bearer_auth(&token).header("Client-Id", self.auth.client_id()).timeout(Duration::from_secs(10));
            if !query.is_empty() {
                rb = rb.query(query);
            }
            if let Some(b) = body {
                rb = rb.json(b);
            }
            let resp = rb.send().await.map_err(net)?;
            let status = resp.status().as_u16();
            if let Some(r) = resp.headers().get("ratelimit-remaining").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok()) {
                self.remaining.store(r, Ordering::Relaxed);
            }
            let reset = resp.headers().get("ratelimit-reset").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok());
            let text = resp.text().await.map_err(net)?;
            match status {
                200..=299 => {
                    return if text.trim().is_empty() {
                        Ok(J::Null)
                    } else {
                        serde_json::from_str(&text).map_err(|e| net(format!("bad JSON from {url}: {e}")))
                    };
                }
                401 if !refreshed => {
                    refreshed = true;
                    self.auth.refresh_from(account, &token).await?;
                    continue;
                }
                429 if !retried => {
                    retried = true;
                    let wait = reset.map(|r| (r - crate::time::unix_s()).clamp(1, 10)).unwrap_or(2);
                    tokio::time::sleep(Duration::from_secs(wait as u64)).await;
                    continue;
                }
                500..=599 if !retried => {
                    retried = true;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                _ => {
                    let message = serde_json::from_str::<J>(&text)
                        .ok()
                        .and_then(|v| v.get("message").and_then(J::as_str).map(String::from))
                        .unwrap_or_else(|| text.chars().take(200).collect());
                    return Err(HelixError { status, message });
                }
            }
        }
    }

    pub async fn get(&self, account: Account, path: &str, query: &[(&str, String)]) -> Result<J, HelixError> {
        self.call(account, Method::Get, path, query, None).await
    }

    /// All pages of a paginated GET (`data` arrays concatenated), up to `max` items.
    pub async fn get_all(&self, account: Account, path: &str, query: &[(&str, String)], max: usize) -> Result<Vec<J>, HelixError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut q: Vec<(&str, String)> = query.to_vec();
            q.push(("first", "100".into()));
            if let Some(c) = &cursor {
                q.push(("after", c.clone()));
            }
            let v = self.get(account, path, &q).await?;
            if let Some(d) = v.get("data").and_then(J::as_array) {
                out.extend(d.iter().cloned());
            }
            cursor = v.pointer("/pagination/cursor").and_then(J::as_str).map(String::from).filter(|c| !c.is_empty());
            if cursor.is_none() || out.len() >= max {
                return Ok(out);
            }
        }
    }
}

/// First element of `data` (most Helix responses).
pub fn first(v: &J) -> Option<&J> {
    v.get("data").and_then(J::as_array).and_then(|a| a.first())
}
