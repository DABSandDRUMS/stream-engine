//! The show folder (PLAN §18): every recorded show gets one folder under `[recording] dir`
//! holding the OBS video files, the rendered clips, and `data/` — the time-aligned show timeline
//! (songs, scenes, lights, chat, moments, audio features, transcript) that the clip ranker, the
//! UI, and later analyzers read to understand what was happening at any moment.
//!
//! ```text
//! <recording.dir>/<YYYY-MM-DD HH-MM> (<session id>)/
//!   *.mkv / *.mp4            OBS recordings (wide, and tall when OBS records it)
//!   clips/                   rendered clips, thumbnails, captions
//!   data/
//!     show.json              Manifest: time base, recordings, lanes, analyzer versions
//!     lanes/<name>.jsonl     one lane per file: spans {t0, t1, label, …} or points {t, label, …}
//!     features.csv           per-second numbers (t, loudness, chat rate, hype, …)
//!     transcript.jsonl       spoken segments {t0, t1, text, words: [{t0, t1, w}]}
//!     feedback.jsonl         every keep / skip / retrim / manual clip, with its context
//!     session/               copy of the engine journal (events, signals, markers, meta)
//!     project/               the project's text files as they were when the show started
//! ```
//!
//! Time base: every `t` in `data/` is **seconds since the show's `t0_ns`** (master clock). `t0_ns`
//! is the start of the first recording when there is one, so `t` is also the position in that
//! video; `Manifest.recordings[].offset` gives the other files' positions.
//!
//! The session's `meta.toml` points at the folder with `show = { dir = "…", name = "…" }`
//! (written through `session.meta` when recording starts). Sessions without it (never recorded)
//! keep their data and clips in `sessions/<id>/` itself.

use se_core::config::Dur;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `data/show.json` schema version.
pub const SCHEMA: u32 = 1;

/// `[recording]` in `project.toml`.
///
/// ```toml
/// [recording]
/// dir = "~/Videos/Stream Engine"   # one folder per show is made in here
/// auto = true                      # keep OBS recording while the show is in one of `modes`
/// modes = ["preshow", "live"]
/// snapshot_project = true          # copy the project's text files into the show folder
///
/// [recording.index]                # the show timeline built after the show
/// enabled = true
/// transcript = true                # full-show transcript of the speech track (Whisper)
/// skip = []                        # built-in analyzers to leave out
/// [[recording.index.external]]     # extra analyzers: argv run with SE_SHOW_DIR set
/// name = "tightness"
/// command = ["~/bin/tightness"]
/// timeout = "30m"
/// ```
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RecordingConfig {
    pub dir: String,
    pub auto: bool,
    pub modes: Vec<String>,
    pub snapshot_project: bool,
    pub index: IndexConfig,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        RecordingConfig {
            dir: "~/Videos/Stream Engine".into(),
            auto: true,
            modes: vec!["preshow".into(), "live".into()],
            snapshot_project: true,
            index: IndexConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct IndexConfig {
    pub enabled: bool,
    pub transcript: bool,
    pub skip: Vec<String>,
    pub external: Vec<ExternalAnalyzer>,
}

impl Default for IndexConfig {
    fn default() -> Self {
        IndexConfig { enabled: true, transcript: true, skip: Vec::new(), external: Vec::new() }
    }
}

/// An analyzer outside the engine: `command` runs with `SE_SHOW_DIR` (the show folder) and
/// `SE_SHOW_DATA` (its `data/`) set, writes `data/lanes/<name>.jsonl` (and may append columns to
/// nothing else), and exits 0.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ExternalAnalyzer {
    pub name: String,
    pub title: String,
    pub command: Vec<String>,
    pub timeout: Dur,
}

impl Default for ExternalAnalyzer {
    fn default() -> Self {
        ExternalAnalyzer { name: String::new(), title: String::new(), command: Vec::new(), timeout: Dur(30 * 60 * 1000) }
    }
}

impl RecordingConfig {
    pub fn from_section(v: Option<&toml::Value>) -> Result<RecordingConfig, String> {
        let Some(v) = v else { return Ok(RecordingConfig::default()) };
        let c: RecordingConfig = v.clone().try_into().map_err(|e: toml::de::Error| format!("[recording]: {}", e.message()))?;
        c.validate()?;
        Ok(c)
    }

    fn validate(&self) -> Result<(), String> {
        if self.dir.trim().is_empty() {
            return Err("[recording] dir must not be empty".into());
        }
        for e in &self.index.external {
            if e.name.is_empty() || !e.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                return Err(format!("[[recording.index.external]] name `{}` must be letters, digits, `_` or `-`", e.name));
            }
            if e.command.is_empty() {
                return Err(format!("[[recording.index.external]] `{}` needs a command", e.name));
            }
        }
        Ok(())
    }

    /// The recordings root with a leading `~/` expanded.
    pub fn dir_path(&self) -> PathBuf {
        expand_home(&self.dir)
    }
}

pub fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ if p == "~" => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(p)),
        _ => PathBuf::from(p),
    }
}

/// Where one show's files live.
#[derive(Clone, Debug, PartialEq)]
pub struct ShowPaths {
    pub root: PathBuf,
}

impl ShowPaths {
    pub fn new(root: impl Into<PathBuf>) -> ShowPaths {
        ShowPaths { root: root.into() }
    }
    pub fn data(&self) -> PathBuf {
        self.root.join("data")
    }
    pub fn clips(&self) -> PathBuf {
        self.root.join("clips")
    }
    pub fn manifest(&self) -> PathBuf {
        self.data().join("show.json")
    }
    pub fn lanes(&self) -> PathBuf {
        self.data().join("lanes")
    }
    pub fn lane(&self, name: &str) -> PathBuf {
        self.lanes().join(format!("{name}.jsonl"))
    }
    pub fn features(&self) -> PathBuf {
        self.data().join("features.csv")
    }
    pub fn transcript(&self) -> PathBuf {
        self.data().join("transcript.jsonl")
    }
    pub fn feedback(&self) -> PathBuf {
        self.data().join("feedback.jsonl")
    }
    pub fn session_copy(&self) -> PathBuf {
        self.data().join("session")
    }
    pub fn project_copy(&self) -> PathBuf {
        self.data().join("project")
    }
}

/// The show folder recorded in `<session_dir>/meta.toml` (`show.dir`), if any.
pub fn show_dir_from_meta(session_dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(session_dir.join("meta.toml")).ok()?;
    let doc: toml::Table = toml::from_str(&text).ok()?;
    let dir = doc.get("show")?.get("dir")?.as_str()?;
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

/// The show folder for a session: `meta.toml` `show.dir`, else the session directory itself.
pub fn paths_for(session_dir: &Path) -> ShowPaths {
    ShowPaths::new(show_dir_from_meta(session_dir).unwrap_or_else(|| session_dir.to_path_buf()))
}

/// Seconds on the show time base.
pub fn secs(ns: i64, t0_ns: i64) -> f64 {
    (ns - t0_ns) as f64 / 1e9
}

/// `data/show.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Manifest {
    pub schema: u32,
    pub session: String,
    /// Display name ("Sat 26 Sep, 8:00 pm").
    pub name: String,
    /// Master-clock ns of t = 0.
    pub t0_ns: i64,
    /// Wall clock (unix ms) of t = 0.
    pub t0_wall_ms: i64,
    /// Seconds from t = 0 to the end of the show.
    pub duration: f64,
    pub recordings: Vec<ManifestRecording>,
    pub lanes: Vec<LaneInfo>,
    pub features: Option<FeaturesInfo>,
    /// `transcript.jsonl` when the transcript analyzer ran.
    pub transcript: Option<String>,
    /// Analyzer name → version that produced the current files.
    pub analyzers: BTreeMap<String, u32>,
    /// Analyzers that failed or were skipped, with the reason (plain words).
    pub notes: Vec<String>,
    pub indexed_at_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ManifestRecording {
    /// Absolute path of the video file.
    pub path: String,
    pub canvas: String,
    /// Show seconds at the file's t = 0.
    pub offset: f64,
    pub duration: Option<f64>,
    /// Track names in order (as OBS reported them).
    pub tracks: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaneKind {
    /// Items `{t0, t1, label, …}`.
    #[default]
    Spans,
    /// Items `{t, label, …}`.
    Points,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LaneInfo {
    /// File stem under `data/lanes/` (`songs`, `scenes`, `chat`, …).
    pub name: String,
    /// Plain UI title ("Songs", "Scenes", "Chat").
    pub title: String,
    pub kind: LaneKind,
    /// Analyzer that wrote it.
    pub analyzer: String,
    pub count: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FeaturesInfo {
    /// Column names after `t` in `features.csv`.
    pub columns: Vec<String>,
    /// Rows per second.
    pub rate: f64,
}

/// Read `data/show.json`.
pub fn read_manifest(paths: &ShowPaths) -> Option<Manifest> {
    let text = std::fs::read_to_string(paths.manifest()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Read a lane file (one JSON object per line; bad lines are skipped).
pub fn read_lane(path: &Path) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// A span of a `Spans` lane covering show second `t`.
pub fn span_at(items: &[serde_json::Value], t: f64) -> Option<&serde_json::Value> {
    items.iter().find(|v| {
        let a = v.get("t0").and_then(serde_json::Value::as_f64);
        let b = v.get("t1").and_then(serde_json::Value::as_f64);
        matches!((a, b), (Some(a), Some(b)) if a <= t && t < b)
    })
}

/// Spans of a `Spans` lane overlapping `[t0, t1)`.
pub fn spans_overlapping(items: &[serde_json::Value], t0: f64, t1: f64) -> impl Iterator<Item = &serde_json::Value> {
    items.iter().filter(move |v| {
        let a = v.get("t0").and_then(serde_json::Value::as_f64).unwrap_or(f64::INFINITY);
        let b = v.get("t1").and_then(serde_json::Value::as_f64).unwrap_or(f64::NEG_INFINITY);
        a < t1 && b > t0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_config_rejects_bad_external_names() {
        let v: toml::Value = toml::from_str("[index]\n[[index.external]]\nname = \"a b\"\ncommand = [\"x\"]\n").unwrap();
        assert!(RecordingConfig::from_section(Some(&v)).is_err());
        let v: toml::Value = toml::from_str("[index]\n[[index.external]]\nname = \"tight\"\ncommand = [\"x\"]\n").unwrap();
        assert_eq!(RecordingConfig::from_section(Some(&v)).unwrap().index.external[0].name, "tight");
    }

    #[test]
    fn show_dir_comes_from_meta_else_session_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(paths_for(tmp.path()).root, tmp.path());
        std::fs::write(tmp.path().join("meta.toml"), "show = { dir = \"/v/show 1\", name = \"x\" }\n").unwrap();
        assert_eq!(paths_for(tmp.path()).root, PathBuf::from("/v/show 1"));
    }

    #[test]
    fn span_lookup_is_half_open() {
        let items: Vec<serde_json::Value> =
            vec![serde_json::json!({"t0": 0.0, "t1": 10.0, "label": "a"}), serde_json::json!({"t0": 10.0, "t1": 20.0, "label": "b"})];
        assert_eq!(span_at(&items, 10.0).unwrap()["label"], "b");
        assert_eq!(spans_overlapping(&items, 5.0, 12.0).count(), 2);
        assert_eq!(spans_overlapping(&items, 20.0, 30.0).count(), 0);
    }
}
