//! The show folder (PLAN §18): every recorded show gets one folder under `[recording] dir`
//! holding the app's video files, the rendered clips, and `data/` — the time-aligned show timeline
//! (songs, scenes, lights, chat, moments, audio features, transcript) that the clip ranker, the
//! UI, and later analyzers read to understand what was happening at any moment.
//!
//! ```text
//! <recording.dir>/<YYYY-MM-DD HH-MM> (<session id>)/
//!   *.mkv                    app recordings (one file per selected video source)
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
/// fallback_dir = ""                # master only, when `dir` is missing or unwritable at start;
///                                  # "" = ~/Videos/Stream Engine (fallback)
/// min_free_gb = 20                 # camera recordings stop under 1.5×, the master under 1× this
/// auto = true                      # begin in `modes`, continue through breaks until offline
/// modes = ["preshow", "live"]
/// snapshot_project = true          # copy the project's text files into the show folder
/// fps = 60
/// encoder = "auto"                 # auto (NVENC, else software) | nvenc | software
/// video = [
///   { name = "main", source = "canvas:wide", role = "master", codec = "hevc", cq = 19, max_mbps = 35 },
///   { name = "kit",  source = "camera:cam_kit",  role = "iso", height = 1080, mbps = 8 },
///   { name = "kick", source = "camera:cam_kick", role = "iso", height = 720,  mbps = 3.5 },
/// ]
/// audio = [                        # muxed into the master file only
///   { name = "Mix",   source = "se-band",  format = "pulse" },
///   { name = "Music", source = "se-music", format = "pulse" },
/// ]
///
/// [recording.archive]              # the compact forever copy made after the show
/// enabled = true
/// gb_per_hour = 1.8
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
///
/// Roles: exactly one video input is the `master` (the canonical show file with every audio
/// track); the rest are `iso` (video-only, own encoder, shed first under load or low disk).
/// Without an explicit `role = "master"`, the first `canvas:*` input (else the first input that
/// is not a camera) is the master and every other input is an ISO.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RecordingConfig {
    pub dir: String,
    pub fallback_dir: String,
    pub min_free_gb: f64,
    pub auto: bool,
    pub modes: Vec<String>,
    pub snapshot_project: bool,
    pub video: Vec<RecordingInput>,
    pub audio: Vec<RecordingInput>,
    pub fps: u32,
    pub encoder: String,
    pub frames_socket: String,
    pub index: IndexConfig,
    pub archive: ArchiveConfig,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        RecordingConfig {
            dir: "~/Videos/Stream Engine".into(),
            fallback_dir: String::new(),
            min_free_gb: 20.0,
            auto: true,
            modes: vec!["preshow".into(), "live".into()],
            snapshot_project: true,
            video: vec![RecordingInput { role: Some(Role::Master), ..RecordingInput::new("main", "canvas:wide", "") }],
            audio: vec![RecordingInput::new("Mix", "se-band", "pulse"), RecordingInput::new("Music", "se-music", "pulse")],
            fps: 60,
            encoder: "auto".into(),
            frames_socket: String::new(),
            index: IndexConfig::default(),
            archive: ArchiveConfig::default(),
        }
    }
}

/// What a video input is recorded as.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The canonical show file: constant quality, every audio track, never shed.
    Master,
    /// An isolated video-only angle at a target bitrate.
    Iso,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Master => "master",
            Role::Iso => "iso",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    #[default]
    Hevc,
    H264,
}

/// `[recording] encoder`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderPref {
    /// NVENC, falling back to software only when NVENC cannot start.
    Auto,
    Nvenc,
    Software,
}

/// Where a video input's pictures come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind<'a> {
    /// `canvas:wide` / `canvas:tall`: the engine's rendered canvas.
    Canvas(&'a str),
    /// `camera:<source id>`: the frames the engine already captures for that source.
    Camera(&'a str),
    /// Any other ffmpeg input URL/path with `format`.
    External,
}

pub const DEFAULT_CQ: u32 = 19;
pub const DEFAULT_MAX_MBPS: f64 = 35.0;
pub const DEFAULT_ISO_MBPS: f64 = 8.0;

/// One `[recording] video` or `audio` entry. Older configs with only `name`/`source`/`format`
/// keep working; the encoding fields apply to video inputs.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RecordingInput {
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<Codec>,
    /// Master constant quality (lower = better).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cq: Option<u32>,
    /// Master bitrate cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_mbps: Option<f64>,
    /// ISO target bitrate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mbps: Option<f64>,
    /// Scale down to this height keeping the aspect ratio (never up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

impl RecordingInput {
    pub fn new(name: &str, source: &str, format: &str) -> Self {
        RecordingInput { name: name.into(), source: source.into(), format: format.into(), ..Default::default() }
    }

    pub fn kind(&self) -> SourceKind<'_> {
        if let Some(canvas) = self.source.strip_prefix("canvas:") {
            SourceKind::Canvas(canvas)
        } else if let Some(id) = self.source.strip_prefix("camera:") {
            SourceKind::Camera(id)
        } else {
            SourceKind::External
        }
    }

    pub fn codec(&self) -> Codec {
        self.codec.unwrap_or_default()
    }

    pub fn cq(&self) -> u32 {
        self.cq.unwrap_or(DEFAULT_CQ)
    }

    pub fn max_mbps(&self) -> f64 {
        self.max_mbps.unwrap_or(DEFAULT_MAX_MBPS)
    }

    pub fn mbps(&self) -> f64 {
        self.mbps.unwrap_or(DEFAULT_ISO_MBPS)
    }

    fn has_video_settings(&self) -> bool {
        self.role.is_some() || self.codec.is_some() || self.cq.is_some() || self.max_mbps.is_some() || self.mbps.is_some() || self.height.is_some()
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

/// `[recording.archive]`: the compact forever copy of each show, made after the show.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ArchiveConfig {
    pub enabled: bool,
    /// "" = `<recording dir>/Archive`.
    pub dir: String,
    /// Size target for the whole show (video + audio + data).
    pub gb_per_hour: f64,
    /// Opus bitrate per audio track.
    pub audio_kbps: u32,
    /// Delete ISO files once the show's clips are reviewed, or after this many days.
    pub iso_keep_days: u32,
    /// Archive work starts only after the show has been offline this long.
    pub min_offline_minutes: u32,
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        ArchiveConfig { enabled: true, dir: String::new(), gb_per_hour: 1.8, audio_kbps: 128, iso_keep_days: 14, min_offline_minutes: 20 }
    }
}

impl ArchiveConfig {
    /// The archive root: `dir` with `~/` expanded, else `<recording_dir>/Archive`.
    pub fn dir_path(&self, recording_dir: &Path) -> PathBuf {
        if self.dir.trim().is_empty() { recording_dir.join("Archive") } else { expand_home(&self.dir) }
    }

    fn validate(&self) -> Result<(), String> {
        if !(0.2..=50.0).contains(&self.gb_per_hour) {
            return Err("[recording.archive] gb_per_hour must be between 0.2 and 50".into());
        }
        if !(32..=512).contains(&self.audio_kbps) {
            return Err("[recording.archive] audio_kbps must be between 32 and 512".into());
        }
        if self.dir.contains('\0') {
            return Err("[recording.archive] dir is not a valid folder path".into());
        }
        Ok(())
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
        if self.dir.contains('\0') || self.fallback_dir.contains('\0') {
            return Err("[recording] dir and fallback_dir must be valid folder paths".into());
        }
        if !self.min_free_gb.is_finite() || !(0.0..=100_000.0).contains(&self.min_free_gb) {
            return Err("[recording] min_free_gb must be a number of GB between 0 and 100000".into());
        }
        if !(1..=120).contains(&self.fps) {
            return Err("[recording] fps must be between 1 and 120".into());
        }
        if !matches!(self.encoder.as_str(), "auto" | "nvenc" | "software" | "x264") {
            return Err("[recording] encoder must be auto, nvenc, or software".into());
        }
        for (kind, inputs) in [("video", &self.video), ("audio", &self.audio)] {
            let mut names = std::collections::HashSet::new();
            for input in inputs {
                if input.name.trim().is_empty() || input.name.contains(['/', '\\', '\0']) || !names.insert(&input.name) {
                    return Err(format!("[recording.{kind}] names must be nonempty, unique, and contain no path separators"));
                }
                if input.source.trim().is_empty() || input.source.contains('\0') || input.format.contains('\0') {
                    return Err(format!("[recording.{kind}] {} needs a valid source", input.name));
                }
                if kind == "audio" {
                    if !matches!(input.kind(), SourceKind::External) {
                        return Err(format!("[recording.audio] {} must be an audio device or file, not a canvas or camera", input.name));
                    }
                    if input.has_video_settings() {
                        return Err(format!("[recording.audio] {}: role, codec, cq, max_mbps, mbps and height apply to video inputs only", input.name));
                    }
                    continue;
                }
                match input.kind() {
                    SourceKind::Canvas(name) if !matches!(name, "wide" | "tall") || !input.format.is_empty() => {
                        return Err("[recording.video] engine sources are canvas:wide or canvas:tall with an empty format".into());
                    }
                    SourceKind::Camera(id) if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') || !input.format.is_empty() => {
                        return Err(format!("[recording.video] {}: camera sources are camera:<source id> (letters, digits, `_` or `-`) with an empty format", input.name));
                    }
                    _ => {}
                }
            }
        }
        if !self.video.is_empty() {
            if self.video.iter().filter(|i| i.role == Some(Role::Master)).count() > 1 {
                return Err("[recording.video] only one video input can be the master recording".into());
            }
            if self.master_index().is_none() {
                return Err("[recording.video] mark one video input as the master recording (role = \"master\")".into());
            }
            for (index, input) in self.video.iter().enumerate() {
                validate_encoding(input, self.role(index))?;
            }
        }
        for e in &self.index.external {
            if e.name.is_empty() || !e.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                return Err(format!("[[recording.index.external]] name `{}` must be letters, digits, `_` or `-`", e.name));
            }
            if e.command.is_empty() {
                return Err(format!("[[recording.index.external]] `{}` needs a command", e.name));
            }
        }
        self.archive.validate()
    }

    /// The master video input: an explicit `role = "master"`, else the first canvas, else the
    /// first input that is not a camera (inputs marked `role = "iso"` never qualify).
    pub fn master_index(&self) -> Option<usize> {
        if let Some(i) = self.video.iter().position(|i| i.role == Some(Role::Master)) {
            return Some(i);
        }
        let open = |i: &RecordingInput| i.role.is_none();
        self.video
            .iter()
            .position(|i| open(i) && matches!(i.kind(), SourceKind::Canvas(_)))
            .or_else(|| self.video.iter().position(|i| open(i) && matches!(i.kind(), SourceKind::External)))
    }

    /// Resolved role of `video[index]`.
    pub fn role(&self, index: usize) -> Role {
        if self.master_index() == Some(index) { Role::Master } else { Role::Iso }
    }

    pub fn encoder_pref(&self) -> EncoderPref {
        match self.encoder.as_str() {
            "nvenc" => EncoderPref::Nvenc,
            "software" | "x264" => EncoderPref::Software,
            _ => EncoderPref::Auto,
        }
    }

    /// The recordings root with a leading `~/` expanded.
    pub fn dir_path(&self) -> PathBuf {
        expand_home(&self.dir)
    }

    /// Where the master goes when `dir` is missing or unwritable at start.
    pub fn fallback_path(&self) -> PathBuf {
        if self.fallback_dir.trim().is_empty() { expand_home("~/Videos/Stream Engine (fallback)") } else { expand_home(&self.fallback_dir) }
    }
}

fn validate_encoding(input: &RecordingInput, role: Role) -> Result<(), String> {
    let name = &input.name;
    if input.cq.is_some_and(|cq| cq > 51) {
        return Err(format!("[recording.video] {name}: cq must be between 0 (best) and 51"));
    }
    if input.max_mbps.is_some_and(|m| !(m.is_finite() && (1.0..=400.0).contains(&m))) {
        return Err(format!("[recording.video] {name}: max_mbps must be between 1 and 400"));
    }
    if input.mbps.is_some_and(|m| !(m.is_finite() && (0.2..=200.0).contains(&m))) {
        return Err(format!("[recording.video] {name}: mbps must be between 0.2 and 200"));
    }
    if input.height.is_some_and(|h| !(144..=4320).contains(&h) || h % 2 != 0) {
        return Err(format!("[recording.video] {name}: height must be an even number of pixels between 144 and 4320"));
    }
    match role {
        Role::Master if input.mbps.is_some() => {
            Err(format!("[recording.video] {name} is the master recording: set its quality with cq and max_mbps, not mbps"))
        }
        Role::Iso if input.cq.is_some() || input.max_mbps.is_some() => {
            Err(format!("[recording.video] {name} is an ISO recording: set its bitrate with mbps; cq and max_mbps apply to the master only"))
        }
        _ => Ok(()),
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
    /// Configured input name (`main`, `kit`, legacy `wide`/`tall`).
    pub canvas: String,
    /// `master` | `iso`; empty for shows recorded before roles existed.
    pub role: String,
    /// Configured source (`canvas:wide`, `camera:cam_kit`, …); empty for older shows.
    pub source: String,
    /// Show seconds at the file's t = 0.
    pub offset: f64,
    pub duration: Option<f64>,
    /// Configured audio track names in file order (empty for ISOs).
    pub tracks: Vec<String>,
}

impl ManifestRecording {
    /// A video-only isolated angle, never the canonical show file.
    pub fn is_iso(&self) -> bool {
        self.role == "iso"
    }
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

    fn parse(text: &str) -> Result<RecordingConfig, String> {
        let v: toml::Value = toml::from_str(text).unwrap();
        RecordingConfig::from_section(Some(&v))
    }

    #[test]
    fn recording_defaults_record_every_show_at_60_fps_with_one_hevc_master() {
        let c = RecordingConfig::from_section(None).unwrap();
        assert!(c.auto);
        assert_eq!(c.fps, 60);
        assert_eq!(c.min_free_gb, 20.0);
        assert_eq!(c.master_index(), Some(0));
        assert_eq!((c.video[0].source.as_str(), c.video[0].codec(), c.video[0].cq(), c.video[0].max_mbps()), ("canvas:wide", Codec::Hevc, 19, 35.0));
        assert_eq!(c.audio.iter().map(|a| a.source.as_str()).collect::<Vec<_>>(), ["se-band", "se-music"]);
        assert_eq!(c.archive, ArchiveConfig { enabled: true, dir: String::new(), gb_per_hour: 1.8, audio_kbps: 128, iso_keep_days: 14, min_offline_minutes: 20 });
        assert_eq!(c.archive.dir_path(Path::new("/v/SE")), PathBuf::from("/v/SE/Archive"));
        assert!(c.fallback_path().ends_with("Videos/Stream Engine (fallback)"));
        // An empty section is the defaults, too.
        assert_eq!(parse("").unwrap(), c);
    }

    #[test]
    fn contract_config_parses_with_roles_and_encoding() {
        let c = parse(
            r#"
            auto = true
            dir = "~/Videos/Stream Engine"
            fallback_dir = "/srv/rec"
            min_free_gb = 30
            encoder = "nvenc"
            video = [
              { name = "main", source = "canvas:wide", role = "master", codec = "hevc", cq = 19, max_mbps = 35 },
              { name = "kit",  source = "camera:cam_kit",  role = "iso", height = 1080, mbps = 8 },
              { name = "kick", source = "camera:cam_kick", role = "iso", height = 720,  mbps = 3.5 },
            ]
            audio = [{ name = "Mix", source = "se-band", format = "pulse" }]
            [archive]
            gb_per_hour = 2.5
            "#,
        )
        .unwrap();
        assert_eq!((c.role(0), c.role(1), c.role(2)), (Role::Master, Role::Iso, Role::Iso));
        assert_eq!(c.video[2].kind(), SourceKind::Camera("cam_kick"));
        assert_eq!((c.video[2].mbps(), c.video[2].height), (3.5, Some(720)));
        assert_eq!(c.encoder_pref(), EncoderPref::Nvenc);
        assert_eq!(c.fallback_path(), PathBuf::from("/srv/rec"));
        assert_eq!(c.archive.gb_per_hour, 2.5);
        assert_eq!(c.archive.audio_kbps, 128);
    }

    #[test]
    fn older_configs_keep_working_and_resolve_one_master() {
        // The old default: wide + tall, name/source/format only, x264 encoder name.
        let c = parse(
            r#"fps = 30
               encoder = "x264"
               video = [{ name = "wide", source = "canvas:wide", format = "" }, { name = "tall", source = "canvas:tall", format = "" }]
               audio = [{ name = "Mix", source = "default", format = "pulse" }]"#,
        )
        .unwrap();
        assert_eq!((c.role(0), c.role(1)), (Role::Master, Role::Iso));
        assert_eq!(c.encoder_pref(), EncoderPref::Software);
        // A camera listed first does not become the master; the canvas does.
        let c = parse(r#"video = [{ name = "kit", source = "camera:cam_kit" }, { name = "main", source = "canvas:wide" }]"#).unwrap();
        assert_eq!(c.master_index(), Some(1));
        // Explicit master wins over the canvas default.
        let c = parse(r#"video = [{ name = "main", source = "canvas:wide" }, { name = "cam", source = "/dev/video0", format = "v4l2", role = "master" }]"#).unwrap();
        assert_eq!(c.master_index(), Some(1));
        // No video at all is valid (auto-record waits for a master to be chosen).
        let c = parse("auto = false\nvideo = []\naudio = []").unwrap();
        assert_eq!(c.master_index(), None);
    }

    #[test]
    fn invalid_recording_settings_explain_themselves() {
        let err = |text: &str| parse(text).unwrap_err();
        assert!(err(r#"video = [{ name = "a", source = "canvas:wide", role = "master" }, { name = "b", source = "canvas:tall", role = "master" }]"#).contains("only one video input can be the master"));
        assert!(err(r#"video = [{ name = "kit", source = "camera:cam_kit" }]"#).contains("mark one video input as the master"));
        assert!(err(r#"video = [{ name = "main", source = "canvas:wide", mbps = 8 }]"#).contains("main is the master recording: set its quality with cq and max_mbps"));
        assert!(err(r#"video = [{ name = "main", source = "canvas:wide" }, { name = "kit", source = "camera:cam_kit", cq = 20 }]"#).contains("kit is an ISO recording"));
        assert!(err(r#"video = [{ name = "main", source = "canvas:wide", height = 721 }]"#).contains("height must be an even number"));
        assert!(err(r#"video = [{ name = "main", source = "canvas:wide", cq = 60 }]"#).contains("cq must be between 0 (best) and 51"));
        assert!(err(r#"video = [{ name = "main", source = "camera:" }]"#).contains("camera sources are camera:<source id>"));
        assert!(err(r#"video = [{ name = "main", source = "canvas:side" }]"#).contains("canvas:wide or canvas:tall"));
        assert!(err(r#"audio = [{ name = "Mix", source = "se-band", format = "pulse", mbps = 1 }]"#).contains("apply to video inputs only"));
        assert!(err(r#"audio = [{ name = "Mix", source = "camera:cam_kit" }]"#).contains("must be an audio device"));
        assert!(err(r#"encoder = "qsv""#).contains("encoder must be auto, nvenc, or software"));
        assert!(err("min_free_gb = -1").contains("min_free_gb"));
        assert!(err("[archive]\ngb_per_hour = 0.1").contains("[recording.archive] gb_per_hour must be between 0.2 and 50"));
        assert!(err("[archive]\naudio_kbps = 8").contains("audio_kbps must be between 32 and 512"));
        assert!(err(r#"video = [{ name = "main", source = "canvas:wide", role = "boss" }]"#).contains("unknown variant"));
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
