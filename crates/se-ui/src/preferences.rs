//! Machine-local window preferences, separate from the stream project and screen layouts.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Preferences {
    pub reduce_motion: bool,
}

fn path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|dir| dir.join("stream-engine/ui.toml"))
}

impl Preferences {
    pub fn load() -> Self {
        path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(self) -> std::io::Result<()> {
        let path = path().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "No home or config directory"))?;
        self.save_to(&path)
    }

    fn save_to(self, path: &Path) -> std::io::Result<()> {
        let dir = path.parent().expect("preference file has parent");
        std::fs::create_dir_all(dir)?;
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        let data = toml::to_string(&self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, data)?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preference_round_trip_and_old_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/ui.toml");
        Preferences { reduce_motion: true }.save_to(&path).unwrap();
        assert!(toml::from_str::<Preferences>(&std::fs::read_to_string(&path).unwrap()).unwrap().reduce_motion);
        assert!(!toml::from_str::<Preferences>("").unwrap().reduce_motion);
    }
}
