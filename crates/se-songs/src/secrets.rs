//! Where the YouTube API key and the relay secret live (§19: only in the keyring).

use anyhow::Result;
use se_store::secrets as keyring;

pub const YOUTUBE_KEY: &str = keyring::names::YOUTUBE_KEY;
pub const RELAY_SECRET: &str = keyring::names::RELAY_SECRET;

/// Secret storage. Production uses the system keyring; tests supply an in-memory store.
pub trait SecretStore: Send + Sync {
    fn get(&self, name: &str) -> Result<Option<String>>;
    fn set(&self, name: &str, value: &str) -> Result<()>;
    fn delete(&self, name: &str) -> Result<()>;
}

/// The Secret Service keyring (service `stream-engine`). Calls block on D-Bus: use from
/// `spawn_blocking`.
pub struct Keyring;

impl SecretStore for Keyring {
    fn get(&self, name: &str) -> Result<Option<String>> {
        keyring::get(name)
    }
    fn set(&self, name: &str, value: &str) -> Result<()> {
        keyring::set(name, value)
    }
    fn delete(&self, name: &str) -> Result<()> {
        keyring::delete(name)
    }
}
