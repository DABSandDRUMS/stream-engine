//! What every engine subsystem receives at start (`se_<x>::start(ctx)`).

use crate::Hub;
use se_core::Config;
use se_store::Db;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Clone)]
pub struct EngineCtx {
    pub hub: Arc<Hub>,
    /// Runtime database (subsystems create their own tables with `db.migrate`).
    pub db: Db,
    /// Project directory (patches, assets, lights, …).
    pub project_root: PathBuf,
    /// `~/.local/share/stream-engine` (caches, models, backups).
    pub data_dir: PathBuf,
    /// Installed data (`web/`, `project-example/`, templates).
    pub share_dir: PathBuf,
    /// Latest applied configuration; changes on every hot reload.
    pub config: watch::Receiver<Arc<Config>>,
    /// HTTP/WS API address (web patches and the player page load from here).
    pub http: std::net::SocketAddr,
    /// `--dev`: verbose, validation layers, simulator conveniences.
    pub dev: bool,
}

impl EngineCtx {
    /// Tables of another config kind, e.g. `ctx.kind("lights/cuelists")`.
    pub fn kind(&self, kind: &str) -> std::collections::BTreeMap<String, toml::Table> {
        self.config.borrow().other.get(kind).cloned().unwrap_or_default()
    }

    /// A `[section]` of `project.toml` (subsystem settings such as `[twitch]`, `[audio]`).
    pub fn project_section(&self, key: &str) -> Option<toml::Value> {
        self.config.borrow().project.extra.get(key).cloned()
    }
}
