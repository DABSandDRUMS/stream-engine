//! OAuth for a public client (§11): device code flow, refresh tokens in the keyring (Twitch
//! rotates them on every refresh), automatic refresh before expiry, and hourly validation.

use crate::config::TwitchCfg;
use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Account {
    Broadcaster,
    Bot,
}

impl Account {
    pub fn parse(s: &str) -> Option<Account> {
        match s {
            "" | "broadcaster" | "owner" => Some(Account::Broadcaster),
            "bot" => Some(Account::Bot),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Account::Broadcaster => "broadcaster",
            Account::Bot => "bot",
        }
    }
    fn idx(self) -> usize {
        self as usize
    }
}

/// Secret storage (the system keyring in the engine; memory in tests).
pub trait SecretStore: Send + Sync {
    fn get(&self, name: &str) -> Result<Option<String>>;
    fn set(&self, name: &str, value: &str) -> Result<()>;
    fn delete(&self, name: &str) -> Result<()>;
}

/// The Secret Service keyring (§3.3: secrets never live in project files).
pub struct Keyring;

impl SecretStore for Keyring {
    fn get(&self, name: &str) -> Result<Option<String>> {
        se_store::secrets::get(name)
    }
    fn set(&self, name: &str, value: &str) -> Result<()> {
        se_store::secrets::set(name, value)
    }
    fn delete(&self, name: &str) -> Result<()> {
        se_store::secrets::delete(name)
    }
}

/// In-memory store for tests and dry runs.
#[derive(Default)]
pub struct MemoryStore(pub Mutex<std::collections::HashMap<String, String>>);

impl SecretStore for MemoryStore {
    fn get(&self, name: &str) -> Result<Option<String>> {
        Ok(self.0.lock().get(name).cloned())
    }
    fn set(&self, name: &str, value: &str) -> Result<()> {
        self.0.lock().insert(name.into(), value.into());
        Ok(())
    }
    fn delete(&self, name: &str) -> Result<()> {
        self.0.lock().remove(name);
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    /// Unix seconds.
    pub expires_at: i64,
    pub scopes: Vec<String>,
    pub user_id: String,
    pub login: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: i64,
    pub interval: i64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    expires_in: i64,
    #[serde(default)]
    scope: Vec<String>,
}

#[derive(Deserialize)]
struct Validate {
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    login: String,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    expires_in: i64,
}

/// Outcome of a token request that may still be pending (device flow).
#[derive(Debug, PartialEq)]
pub enum Poll {
    Pending,
    SlowDown,
    Done(Tokens),
}

#[derive(Debug, PartialEq)]
pub enum AuthError {
    /// The refresh token is gone or invalid: the user must authorize again.
    Revoked(String),
    Other(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Revoked(m) => write!(f, "authorization revoked: {m}"),
            AuthError::Other(m) => f.write_str(m),
        }
    }
}

pub struct Auth {
    http: reqwest::Client,
    cfg: Mutex<TwitchCfg>,
    store: Arc<dyn SecretStore>,
    tokens: [Mutex<Option<Tokens>>; 2],
    refreshing: [tokio::sync::Mutex<()>; 2],
}

fn error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_else(|| body.chars().take(200).collect())
}

impl Auth {
    pub fn new(http: reqwest::Client, cfg: TwitchCfg, store: Arc<dyn SecretStore>) -> Auth {
        Auth {
            http,
            cfg: Mutex::new(cfg),
            store,
            tokens: [Mutex::new(None), Mutex::new(None)],
            refreshing: [tokio::sync::Mutex::new(()), tokio::sync::Mutex::new(())],
        }
    }

    pub fn set_cfg(&self, cfg: TwitchCfg) {
        *self.cfg.lock() = cfg;
    }

    fn cfg(&self) -> TwitchCfg {
        self.cfg.lock().clone()
    }

    pub fn client_id(&self) -> String {
        self.cfg.lock().client_id.clone()
    }

    pub fn tokens(&self, a: Account) -> Option<Tokens> {
        self.tokens[a.idx()].lock().clone()
    }

    pub fn scopes(a: Account) -> &'static [&'static str] {
        match a {
            Account::Broadcaster => crate::config::SCOPES,
            Account::Bot => crate::config::BOT_SCOPES,
        }
    }

    /// Scopes we asked for that the stored grant lacks (after a scope list change).
    pub fn missing_scopes(&self, a: Account) -> Vec<&'static str> {
        let Some(t) = self.tokens(a) else { return Vec::new() };
        Self::scopes(a).iter().copied().filter(|s| !t.scopes.iter().any(|x| x == s)).collect()
    }

    /// Start the device code flow: the user opens `verification_uri` and enters `user_code`.
    pub async fn device_start(&self, a: Account) -> Result<DeviceCode> {
        let cfg = self.cfg();
        if cfg.client_id.is_empty() {
            bail!("set [twitch] client_id in project.toml first");
        }
        let scopes = Self::scopes(a).join(" ");
        let r = self.http.post(format!("{}/device", cfg.auth_url)).form(&[("client_id", cfg.client_id.as_str()), ("scopes", scopes.as_str())]).send().await?;
        let status = r.status();
        let body = r.text().await?;
        if !status.is_success() {
            bail!("device code request failed ({status}): {}", error_message(&body));
        }
        serde_json::from_str(&body).context("device code response")
    }

    /// One token poll for a device code.
    pub async fn device_poll(&self, a: Account, dc: &DeviceCode) -> Result<Poll> {
        let cfg = self.cfg();
        let scopes = Self::scopes(a).join(" ");
        let r = self
            .http
            .post(format!("{}/token", cfg.auth_url))
            .form(&[
                ("client_id", cfg.client_id.as_str()),
                ("scopes", scopes.as_str()),
                ("device_code", dc.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?;
        let status = r.status();
        let body = r.text().await?;
        if status.is_success() {
            let t: TokenResponse = serde_json::from_str(&body).context("token response")?;
            let tokens = self.finish(a, t).await?;
            return Ok(Poll::Done(tokens));
        }
        match error_message(&body).as_str() {
            "authorization_pending" => Ok(Poll::Pending),
            "slow_down" => Ok(Poll::SlowDown),
            m => Err(anyhow!("{m}")),
        }
    }

    /// Validate a fresh grant, keep it, and store the refresh token.
    async fn finish(&self, a: Account, t: TokenResponse) -> Result<Tokens> {
        let v = self.validate_token(&t.access_token).await.map_err(|e| anyhow!("{e}"))?;
        let tokens = Tokens {
            access: t.access_token,
            refresh: t.refresh_token,
            expires_at: crate::time::unix_s() + if t.expires_in > 0 { t.expires_in } else { v.expires_in },
            scopes: if t.scope.is_empty() { v.scopes } else { t.scope },
            user_id: v.user_id,
            login: v.login,
        };
        self.store.set(&self.cfg().refresh_secret(a == Account::Bot), &tokens.refresh).context("store the refresh token in the keyring")?;
        *self.tokens[a.idx()].lock() = Some(tokens.clone());
        Ok(tokens)
    }

    async fn validate_token(&self, access: &str) -> Result<Validate, AuthError> {
        let cfg = self.cfg();
        let r = self
            .http
            .get(format!("{}/validate", cfg.auth_url))
            .header("Authorization", format!("OAuth {access}"))
            .send()
            .await
            .map_err(|e| AuthError::Other(e.to_string()))?;
        let status = r.status();
        let body = r.text().await.map_err(|e| AuthError::Other(e.to_string()))?;
        if status.as_u16() == 401 {
            return Err(AuthError::Revoked(error_message(&body)));
        }
        if !status.is_success() {
            return Err(AuthError::Other(format!("validate failed ({status}): {}", error_message(&body))));
        }
        let v: Validate = serde_json::from_str(&body).map_err(|e| AuthError::Other(e.to_string()))?;
        if !cfg.client_id.is_empty() && !v.client_id.is_empty() && v.client_id != cfg.client_id {
            return Err(AuthError::Revoked(format!("token belongs to client {} (configured {})", v.client_id, cfg.client_id)));
        }
        Ok(v)
    }

    /// Load the stored refresh token and exchange it. `Ok(false)` = nothing stored.
    pub async fn load(&self, a: Account) -> Result<bool, AuthError> {
        let name = self.cfg().refresh_secret(a == Account::Bot);
        let stored = self.store.get(&name).map_err(|e| AuthError::Other(format!("keyring: {e:#}")))?;
        let Some(refresh) = stored.filter(|s| !s.is_empty()) else { return Ok(false) };
        *self.tokens[a.idx()].lock() =
            Some(Tokens { access: String::new(), refresh, expires_at: 0, scopes: Vec::new(), user_id: String::new(), login: String::new() });
        self.refresh(a).await?;
        Ok(true)
    }

    /// Exchange the refresh token for a new pair (serialized per account; the new refresh
    /// token replaces the old one in the keyring).
    pub async fn refresh(&self, a: Account) -> Result<Tokens, AuthError> {
        let seen = self.tokens(a).map(|t| t.access).unwrap_or_default();
        self.refresh_from(a, &seen).await
    }

    /// Refresh unless another task already replaced the access token `seen` meanwhile.
    pub async fn refresh_from(&self, a: Account, seen: &str) -> Result<Tokens, AuthError> {
        let _g = self.refreshing[a.idx()].lock().await;
        let cur = self.tokens(a).ok_or_else(|| AuthError::Revoked("not authorized".into()))?;
        if !cur.access.is_empty() && cur.access != seen {
            return Ok(cur);
        }
        let cfg = self.cfg();
        let r = self
            .http
            .post(format!("{}/token", cfg.auth_url))
            .form(&[("client_id", cfg.client_id.as_str()), ("grant_type", "refresh_token"), ("refresh_token", cur.refresh.as_str())])
            .send()
            .await
            .map_err(|e| AuthError::Other(e.to_string()))?;
        let status = r.status();
        let body = r.text().await.map_err(|e| AuthError::Other(e.to_string()))?;
        if matches!(status.as_u16(), 400 | 401) {
            *self.tokens[a.idx()].lock() = None;
            // forget the grant, unless the keyring already holds a newer one
            let name = cfg.refresh_secret(a == Account::Bot);
            if self.store.get(&name).ok().flatten().as_deref() == Some(cur.refresh.as_str()) {
                let _ = self.store.delete(&name);
            }
            return Err(AuthError::Revoked(error_message(&body)));
        }
        if !status.is_success() {
            return Err(AuthError::Other(format!("refresh failed ({status}): {}", error_message(&body))));
        }
        let t: TokenResponse = serde_json::from_str(&body).map_err(|e| AuthError::Other(e.to_string()))?;
        self.finish(a, t).await.map_err(|e| AuthError::Other(format!("{e:#}")))
    }

    /// A usable access token, refreshed when it expires within a minute.
    pub async fn access(&self, a: Account) -> Result<String, AuthError> {
        let t = self.tokens(a).ok_or_else(|| AuthError::Revoked(format!("{} account not authorized", a.as_str())))?;
        if t.access.is_empty() || t.expires_at - crate::time::unix_s() < 60 {
            return Ok(self.refresh_from(a, &t.access).await?.access);
        }
        Ok(t.access)
    }

    /// Hourly validation (Twitch requirement); refreshes on 401.
    pub async fn validate(&self, a: Account) -> Result<(), AuthError> {
        let t = self.tokens(a).ok_or_else(|| AuthError::Revoked("not authorized".into()))?;
        match self.validate_token(&t.access).await {
            Ok(v) => {
                if let Some(cur) = self.tokens[a.idx()].lock().as_mut() {
                    cur.expires_at = crate::time::unix_s() + v.expires_in;
                    cur.scopes = v.scopes;
                }
                Ok(())
            }
            Err(AuthError::Revoked(_)) => self.refresh_from(a, &t.access).await.map(|_| ()),
            Err(e) => Err(e),
        }
    }

    pub fn logout(&self, a: Account) -> Result<()> {
        *self.tokens[a.idx()].lock() = None;
        self.store.delete(&self.cfg().refresh_secret(a == Account::Bot))
    }
}
