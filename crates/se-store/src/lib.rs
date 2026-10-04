//! Persistence: project directory (load, hot reload, round-trip writes, migrations),
//! runtime SQLite database, session logs, and keyring secrets.

pub mod db;
pub mod project;
pub mod fx;
pub mod secrets;
pub mod session;
pub mod versions;

pub use db::Db;
pub use project::{Loaded, Project};
pub use session::{LogRec, SessionWriter, read_log};

/// `$XDG_DATA_HOME/stream-engine` (runtime DB, backups).
pub fn data_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("stream-engine")
}
