//! Secrets (§3.3, §19) live only in the system keyring (Secret Service).

use anyhow::{Context, Result};

pub const SERVICE: &str = "stream-engine";

/// Well-known secret names.
pub mod names {
    pub const API_TOKEN: &str = "api.token";
    pub const TWITCH_REFRESH: &str = "twitch.refresh_token";
    pub const TWITCH_BOT_REFRESH: &str = "twitch.bot_refresh_token";
    pub const YOUTUBE_KEY: &str = "youtube.api_key";
    pub const RELAY_SECRET: &str = "relay.secret";
}

fn entry(name: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, name).with_context(|| format!("keyring entry {name}"))
}

pub fn get(name: &str) -> Result<Option<String>> {
    match entry(name)?.get_password() {
        Ok(s) => Ok(Some(s)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read secret {name}")),
    }
}

pub fn set(name: &str, value: &str) -> Result<()> {
    entry(name)?.set_password(value).with_context(|| format!("store secret {name}"))
}

pub fn delete(name: &str) -> Result<()> {
    match entry(name)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).with_context(|| format!("delete secret {name}")),
    }
}

/// Random URL-safe token (API tokens, relay secret).
pub fn random_token() -> String {
    let mut buf = [0u8; 32];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Get a secret, creating it with a random token on first use.
pub fn get_or_create(name: &str) -> Result<String> {
    if let Some(s) = get(name)? {
        return Ok(s);
    }
    let t = random_token();
    set(name, &t)?;
    Ok(t)
}
