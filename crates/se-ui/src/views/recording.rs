//! Settings → Recording: what gets recorded (the main show picture with its sound tracks, plus
//! picture-only camera files), where it is kept, how small finished shows get in the archive, and
//! what the recorder and the archive are doing right now. Also the compact status on Clipping.
//!
//! Settings are a draft of `[recording]` / `[recording.archive]` in project.toml, saved with one
//! `project.write` `set` (the engine edits the file in place, keeping comments and other keys).

use crate::app::App;
use crate::views::status::mode_label;
use crossbeam_channel::{Receiver, TryRecvError};
use egui::{RichText, Ui};
use se_proto::Value;
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_medium, font_mono, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, Tone, icon};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

const DEFAULT_DIR: &str = "~/Videos/Stream Engine";
/// The archive size is chosen per show of this many hours.
const SHOW_HOURS: f64 = 3.0;
/// Archive size choices, GB per 3-hour show.
const GB_PER_SHOW_MIN: f64 = 3.0;
const GB_PER_SHOW_MAX: f64 = 12.0;
const DEFAULT_GB_PER_HOUR: f64 = 1.8;
/// Rough FLAC rate of one stereo 48 kHz track while recording.
const FLAC_MBPS: f64 = 1.0;
/// Show modes that can start automatic recording (it then runs until the show is off air).
const START_MODES: [&str; 6] = ["preshow", "live", "brb", "ad_break", "outro", "rehearsal"];
/// Main recording quality: (label, cq, max Mbps). Lower cq = better picture, bigger files.
const QUALITY: [(&str, i64, f64); 3] = [("Standard", 23, 25.0), ("High", 19, 35.0), ("Maximum", 16, 50.0)];
/// Camera file sizes: (label, picture height, Mbps).
const CAMERA_SIZES: [(&str, i64, f64); 2] = [("720p", 720, 3.5), ("1080p", 1080, 8.0)];
/// How long a folder's free-space reading stays fresh.
const FOLDER_FRESH_SECS: u64 = 10;

/// Engine actions on the archive queue.
const ARCHIVE_RUN: &str = "archive.run";
const ARCHIVE_PAUSE: &str = "archive.pause";
const ARCHIVE_RETRY: &str = "archive.retry";

// ---- draft ----------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Role {
    Master,
    Iso,
}

/// One `[[recording.video]]` entry.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Feed {
    pub name: String,
    pub source: String,
    /// Input format for external inputs (blank for engine sources).
    pub format: String,
    pub role: Role,
    pub codec: String,
    /// Master only: constant quality and its bitrate cap.
    pub cq: i64,
    pub max_mbps: f64,
    /// Iso only: target bitrate.
    pub mbps: f64,
    /// Output picture height (never upscaled); `None` keeps the input size.
    pub height: Option<i64>,
    /// Keys this screen doesn't know, written back unchanged.
    pub extra: Vec<(String, Value)>,
}

impl Feed {
    fn master(name: &str, source: &str) -> Self {
        let (_, cq, max_mbps) = QUALITY[1];
        Self { name: name.into(), source: source.into(), format: String::new(), role: Role::Master, codec: "hevc".into(), cq, max_mbps, mbps: 8.0, height: None, extra: Vec::new() }
    }

    fn iso(name: &str, source: &str, height: Option<i64>, mbps: f64) -> Self {
        Self { role: Role::Iso, height, mbps, ..Self::master(name, source) }
    }

    /// `camera:<id>` → `<id>`.
    pub fn camera(&self) -> Option<&str> {
        self.source.strip_prefix("camera:")
    }

    /// (feed, role written in the file).
    fn from_toml(row: &toml::Value) -> (Self, Option<Role>) {
        let s = |k: &str| row.get(k).and_then(toml::Value::as_str).unwrap_or("").to_string();
        let num = |k: &str| row.get(k).and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)));
        let role = match row.get("role").and_then(toml::Value::as_str) {
            Some("master") => Some(Role::Master),
            Some("iso") => Some(Role::Iso),
            _ => None,
        };
        let base = Self::master("", "");
        let known = ["name", "source", "format", "role", "codec", "cq", "max_mbps", "mbps", "height"];
        let feed = Self {
            name: s("name"),
            source: s("source"),
            format: s("format"),
            role: role.unwrap_or(Role::Iso),
            codec: row.get("codec").and_then(toml::Value::as_str).unwrap_or("hevc").into(),
            cq: row.get("cq").and_then(toml::Value::as_integer).unwrap_or(base.cq),
            max_mbps: num("max_mbps").unwrap_or(base.max_mbps),
            mbps: num("mbps").unwrap_or(base.mbps),
            height: row.get("height").and_then(toml::Value::as_integer),
            extra: extras(row, &known),
        };
        (feed, role)
    }

    fn value(&self) -> Value {
        let mut v = Value::map().with("name", self.name.trim()).with("source", self.source.trim());
        if !self.format.trim().is_empty() {
            v = v.with("format", self.format.trim());
        }
        v = match self.role {
            Role::Master => v.with("role", "master").with("codec", self.codec.as_str()).with("cq", self.cq).with("max_mbps", number(self.max_mbps)),
            Role::Iso => {
                let v = v.with("role", "iso");
                let v = if self.codec == "hevc" { v } else { v.with("codec", self.codec.as_str()) };
                v.with("mbps", number(self.mbps))
            }
        };
        if let Some(h) = self.height {
            v = v.with("height", h);
        }
        for (k, x) in &self.extra {
            v = v.with(k.as_str(), x.clone());
        }
        v
    }

    /// Index into [`QUALITY`] matching this master's settings.
    fn quality(&self) -> Option<usize> {
        QUALITY.iter().position(|(_, cq, max)| self.cq == *cq && (self.max_mbps - max).abs() < 1e-9)
    }

    /// Index into [`CAMERA_SIZES`] matching this camera file.
    fn size(&self) -> Option<usize> {
        CAMERA_SIZES.iter().position(|(_, h, mbps)| self.height == Some(*h) && (self.mbps - mbps).abs() < 1e-9)
    }
}

/// One `[[recording.audio]]` entry.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Track {
    pub name: String,
    pub source: String,
    pub format: String,
    pub extra: Vec<(String, Value)>,
}

impl Track {
    fn new(name: &str, source: &str, format: &str) -> Self {
        Self { name: name.into(), source: source.into(), format: format.into(), extra: Vec::new() }
    }

    fn value(&self) -> Value {
        let mut v = Value::map().with("name", self.name.trim()).with("source", self.source.trim()).with("format", self.format.trim());
        for (k, x) in &self.extra {
            v = v.with(k.as_str(), x.clone());
        }
        v
    }
}

/// `[recording.archive]`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Archive {
    pub enabled: bool,
    pub dir: String,
    pub gb_per_hour: f64,
    pub audio_kbps: i64,
    pub iso_keep_days: i64,
    pub min_offline_minutes: i64,
}

/// `[recording]` as edited on screen.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Draft {
    pub dir: String,
    pub fallback_dir: String,
    pub min_free_gb: f64,
    pub auto: bool,
    pub modes: Vec<String>,
    pub fps: i64,
    pub encoder: String,
    pub frames_socket: String,
    pub video: Vec<Feed>,
    pub audio: Vec<Track>,
    pub snapshot_project: bool,
    pub index_enabled: bool,
    pub index_transcript: bool,
    pub index_skip: Vec<String>,
    pub archive: Archive,
}

fn extras(row: &toml::Value, known: &[&str]) -> Vec<(String, Value)> {
    row.as_table().map(|t| t.iter().filter(|(k, _)| !known.contains(&k.as_str())).map(|(k, v)| (k.clone(), Value::from(v.clone()))).collect()).unwrap_or_default()
}

/// Whole numbers as integers (as written by hand), others as floats.
fn number(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 1e15 { Value::Int(x as i64) } else { Value::Float(x) }
}

/// Archive size per 3-hour show → `gb_per_hour` (clamped to the offered range).
pub(crate) fn gb_per_hour(gb_per_show: f64) -> f64 {
    (gb_per_show.clamp(GB_PER_SHOW_MIN, GB_PER_SHOW_MAX) / SHOW_HOURS * 10_000.0).round() / 10_000.0
}

/// `gb_per_hour` → archive size per 3-hour show.
pub(crate) fn gb_per_show(gb_per_hour: f64) -> f64 {
    gb_per_hour * SHOW_HOURS
}

/// Picture bitrate left in the archive once every sound track has its share.
pub(crate) fn archive_video_mbps(gb_per_hour: f64, audio_tracks: usize, audio_kbps: i64) -> f64 {
    (gb_per_hour * 8000.0 / 3600.0 - audio_tracks as f64 * audio_kbps as f64 / 1000.0).max(0.0)
}

/// Most space the recording can take per hour while a show runs (sound goes into the main file).
pub(crate) fn recording_gb_per_hour(video: &[Feed], audio_tracks: usize) -> f64 {
    let picture: f64 = video.iter().map(|f| if f.role == Role::Master { f.max_mbps } else { f.mbps }).sum();
    let sound = if video.iter().any(|f| f.role == Role::Master) { audio_tracks as f64 * FLAC_MBPS } else { 0.0 };
    (picture + sound) * 3600.0 / 8000.0
}

impl Draft {
    pub fn from_project(doc: &toml::Value) -> Self {
        let rec = doc.get("recording");
        let get = |key: &str| rec.and_then(|r| r.get(key));
        let text = |key: &str, default: &str| get(key).and_then(toml::Value::as_str).unwrap_or(default).to_string();
        let float = |v: Option<&toml::Value>, default: f64| v.and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64))).unwrap_or(default);
        let video = match get("video").and_then(toml::Value::as_array) {
            Some(rows) => resolve_roles(rows.iter().map(Feed::from_toml).collect()),
            None => vec![Feed::master("main", "canvas:wide")],
        };
        let audio = match get("audio").and_then(toml::Value::as_array) {
            Some(rows) => rows
                .iter()
                .map(|row| {
                    let s = |key| row.get(key).and_then(toml::Value::as_str).unwrap_or("");
                    Track { extra: extras(row, &["name", "source", "format"]), ..Track::new(s("name"), s("source"), s("format")) }
                })
                .collect(),
            None => vec![Track::new("Mix", "se-band", "pulse"), Track::new("Music", "se-music", "pulse")],
        };
        let index = get("index");
        let archive = get("archive");
        let a = |key: &str| archive.and_then(|v| v.get(key));
        let encoder = text("encoder", "auto");
        Self {
            dir: text("dir", DEFAULT_DIR),
            fallback_dir: text("fallback_dir", ""),
            min_free_gb: float(get("min_free_gb"), 20.0),
            auto: get("auto").and_then(toml::Value::as_bool).unwrap_or(true),
            modes: get("modes")
                .and_then(toml::Value::as_array)
                .map(|v| v.iter().filter_map(toml::Value::as_str).map(String::from).collect())
                .unwrap_or_else(|| vec!["preshow".into(), "live".into()]),
            fps: get("fps").and_then(toml::Value::as_integer).unwrap_or(60),
            // "x264" is the older name of the software encoder
            encoder: if encoder == "x264" { "software".into() } else { encoder },
            frames_socket: text("frames_socket", ""),
            video,
            audio,
            snapshot_project: get("snapshot_project").and_then(toml::Value::as_bool).unwrap_or(true),
            index_enabled: index.and_then(|v| v.get("enabled")).and_then(toml::Value::as_bool).unwrap_or(true),
            index_transcript: index.and_then(|v| v.get("transcript")).and_then(toml::Value::as_bool).unwrap_or(true),
            index_skip: index
                .and_then(|v| v.get("skip"))
                .and_then(toml::Value::as_array)
                .map(|v| v.iter().filter_map(toml::Value::as_str).map(String::from).collect())
                .unwrap_or_default(),
            archive: Archive {
                enabled: a("enabled").and_then(toml::Value::as_bool).unwrap_or(true),
                dir: a("dir").and_then(toml::Value::as_str).unwrap_or("").into(),
                gb_per_hour: float(a("gb_per_hour"), DEFAULT_GB_PER_HOUR),
                audio_kbps: a("audio_kbps").and_then(toml::Value::as_integer).unwrap_or(128),
                iso_keep_days: a("iso_keep_days").and_then(toml::Value::as_integer).unwrap_or(14),
                min_offline_minutes: a("min_offline_minutes").and_then(toml::Value::as_integer).unwrap_or(20),
            },
        }
    }

    pub fn normalize(&mut self) {
        self.dir = self.dir.trim().into();
        self.fallback_dir = self.fallback_dir.trim().into();
        self.frames_socket = self.frames_socket.trim().into();
        self.archive.dir = self.archive.dir.trim().into();
        for f in &mut self.video {
            f.name = f.name.trim().into();
            f.source = f.source.trim().into();
            f.format = f.format.trim().into();
        }
        for a in &mut self.audio {
            a.name = a.name.trim().into();
            a.source = a.source.trim().into();
            a.format = a.format.trim().into();
        }
    }

    /// `project.write` `set` map for project.toml.
    pub fn changes(&self) -> Value {
        let strs = |v: &[String]| Value::List(v.iter().map(|s| Value::from(s.trim())).filter(|v| v.as_str().is_some_and(|s| !s.is_empty())).collect());
        Value::map()
            .with("recording.dir", self.dir.trim())
            .with("recording.fallback_dir", self.fallback_dir.trim())
            .with("recording.min_free_gb", number(self.min_free_gb))
            .with("recording.auto", self.auto)
            .with("recording.modes", strs(&self.modes))
            .with("recording.fps", self.fps)
            .with("recording.encoder", self.encoder.as_str())
            .with("recording.frames_socket", self.frames_socket.trim())
            .with("recording.video", Value::List(self.video.iter().map(Feed::value).collect()))
            .with("recording.audio", Value::List(self.audio.iter().map(Track::value).collect()))
            .with("recording.snapshot_project", self.snapshot_project)
            .with("recording.index.enabled", self.index_enabled)
            .with("recording.index.transcript", self.index_transcript)
            .with("recording.index.skip", strs(&self.index_skip))
            .with("recording.archive.enabled", self.archive.enabled)
            .with("recording.archive.dir", self.archive.dir.trim())
            .with("recording.archive.gb_per_hour", number(self.archive.gb_per_hour))
            .with("recording.archive.audio_kbps", self.archive.audio_kbps)
            .with("recording.archive.iso_keep_days", self.archive.iso_keep_days)
            .with("recording.archive.min_offline_minutes", self.archive.min_offline_minutes)
    }

    fn masters(&self) -> usize {
        self.video.iter().filter(|f| f.role == Role::Master).count()
    }

    fn master_mut(&mut self) -> Option<&mut Feed> {
        self.video.iter_mut().find(|f| f.role == Role::Master)
    }

    fn camera_feed(&self, id: &str) -> Option<&Feed> {
        self.video.iter().find(|f| f.camera() == Some(id))
    }

    /// Record camera `id` at `size` (index into [`CAMERA_SIZES`]), or stop recording it (`None`).
    pub fn set_camera(&mut self, id: &str, size: Option<usize>) {
        let source = format!("camera:{id}");
        match size.and_then(|i| CAMERA_SIZES.get(i)) {
            None => self.video.retain(|f| f.source != source),
            Some(&(_, height, mbps)) => {
                if let Some(f) = self.video.iter_mut().find(|f| f.source == source) {
                    f.height = Some(height);
                    f.mbps = mbps;
                } else {
                    let name = self.free_name(id.strip_prefix("cam_").filter(|s| !s.is_empty()).unwrap_or(id));
                    self.video.push(Feed::iso(&name, &source, Some(height), mbps));
                }
            }
        }
    }

    /// `base`, or `base-2`, `base-3`… when another feed already uses the name.
    fn free_name(&self, base: &str) -> String {
        let taken = |n: &str| self.video.iter().any(|f| f.name.trim() == n);
        if !taken(base) {
            return base.into();
        }
        (2..).map(|i| format!("{base}-{i}")).find(|n| !taken(n)).unwrap_or_default()
    }

    /// Problems that block saving. `cameras` = the project's camera source ids, when known.
    pub fn errors(&self, cameras: Option<&[String]>) -> Vec<String> {
        let mut out = Vec::new();
        if self.dir.trim().is_empty() {
            out.push("Choose a recording folder.".into());
        }
        match self.masters() {
            0 => out.push("Choose the main recording: nothing would be recorded with sound.".into()),
            1 => {}
            n => out.push(format!("{n} feeds are set as the main recording. Keep exactly one.")),
        }
        let mut names: Vec<&str> = Vec::new();
        for f in &self.video {
            let name = f.name.trim();
            if name.is_empty() {
                out.push(format!("Give the recording of {} a name.", f.source.trim()));
            } else if names.contains(&name) {
                out.push(format!("Two recordings are called “{name}”. Names must differ."));
            }
            names.push(name);
            if f.source.trim().is_empty() {
                out.push(format!("The recording “{name}” has no source."));
            }
            if let (Some(id), Some(known)) = (f.camera(), cameras)
                && !known.iter().any(|c| c == id)
            {
                out.push(format!("Camera “{id}” isn't one of your sources anymore. Turn it off or add it back in Sources."));
            }
            if f.role == Role::Iso && f.mbps <= 0.0 {
                out.push(format!("Give the recording “{name}” a size above 0 Mbps."));
            }
        }
        for a in &self.audio {
            if a.source.trim().is_empty() {
                out.push(format!("The sound track “{}” has no input.", a.name.trim()));
            }
        }
        out
    }
}

/// Explicit roles win; with no explicit master, the first canvas (else first non-camera) input
/// without a role is the master and everything else records picture-only.
fn resolve_roles(rows: Vec<(Feed, Option<Role>)>) -> Vec<Feed> {
    let explicit_master = rows.iter().any(|(_, r)| *r == Some(Role::Master));
    let implied = if explicit_master {
        None
    } else {
        rows.iter()
            .position(|(f, r)| r.is_none() && f.source.starts_with("canvas:"))
            .or_else(|| rows.iter().position(|(f, r)| r.is_none() && f.camera().is_none()))
    };
    rows.into_iter()
        .enumerate()
        .map(|(i, (mut f, r))| {
            f.role = r.unwrap_or(if implied == Some(i) { Role::Master } else { Role::Iso });
            f
        })
        .collect()
}

// ---- folders ----------------------------------------------------------------------------------------

/// What the app can tell about a folder on this computer.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Folder {
    pub path: PathBuf,
    pub exists: bool,
    pub writable: bool,
    pub free_gb: Option<f64>,
    pub system_drive: bool,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub(crate) fn expand(path: &str, home: &Path) -> PathBuf {
    let path = path.trim();
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

fn archive_path(d: &Draft, home: &Path) -> PathBuf {
    if d.archive.dir.trim().is_empty() { expand(&d.dir, home).join("Archive") } else { expand(&d.archive.dir, home) }
}

fn fallback_path(d: &Draft, home: &Path) -> PathBuf {
    if d.fallback_dir.trim().is_empty() { home.join("Videos").join("Stream Engine (fallback)") } else { expand(&d.fallback_dir, home) }
}

fn c_path(p: &Path) -> Option<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes()).ok()
}

fn free_gb(p: &Path) -> Option<f64> {
    let c = c_path(p)?;
    // SAFETY: `statvfs` only writes the struct we own; a zeroed `statvfs` is a valid value.
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        (libc::statvfs(c.as_ptr(), &mut s) == 0).then(|| s.f_bavail as f64 * s.f_frsize as f64 / 1e9)
    }
}

fn writable(p: &Path) -> bool {
    // SAFETY: `access` only reads the NUL-terminated path.
    c_path(p).is_some_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
}

/// `\040` style escapes in /proc mount paths.
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && let Ok(code) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 4]).unwrap_or(""), 8)
        {
            out.push(code);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The device behind the mount holding `path` (a btrfs subvolume reports its disk).
pub(crate) fn mount_source_in(mountinfo: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        let Some((left, right)) = line.split_once(" - ") else { continue };
        let (Some(point), Some(source)) = (left.split(' ').nth(4), right.split(' ').nth(1)) else { continue };
        let point = unescape_mount(point);
        if path.starts_with(&point) && best.as_ref().is_none_or(|(n, _)| point.len() >= *n) {
            best = Some((point.len(), source.to_string()));
        }
    }
    best.map(|(_, s)| s)
}

fn inspect(path: &Path) -> Folder {
    let exists = path.is_dir();
    let probe = path.ancestors().find(|p| p.exists()).unwrap_or(Path::new("/"));
    let real = probe.canonicalize().unwrap_or_else(|_| probe.to_path_buf());
    let system_drive = std::fs::read_to_string("/proc/self/mountinfo")
        .ok()
        .and_then(|info| Some((mount_source_in(&info, &real)?, mount_source_in(&info, Path::new("/"))?)))
        .is_some_and(|(a, b)| a == b);
    Folder { path: path.to_path_buf(), exists, writable: exists && writable(path), free_gb: free_gb(probe), system_drive }
}

// ---- live guard -------------------------------------------------------------------------------------

/// Why background archive work can't be started now (`None` = the show is off air). Unknown
/// counts as live, like the engine's guard.
pub(crate) fn live_reason(connected: bool, get: impl Fn(&str) -> Option<Value>) -> Option<String> {
    let on = |k: &str| get(k).is_some_and(|v| v.truthy());
    if !connected {
        return Some("Connect to Stream Engine first.".into());
    }
    if ["recording.active", "recording.starting", "recording.stopping"].iter().any(|k| on(k)) {
        return Some("Recording is running. Archive work waits until it stops.".into());
    }
    if on("twitch.stream.live") {
        return Some("You're live on Twitch. Archive work waits until you're off air.".into());
    }
    if on("obs.stream.active") {
        return Some("OBS is streaming. Archive work waits until it stops.".into());
    }
    match get("show.mode").and_then(|v| v.as_str().map(String::from)).unwrap_or_default().as_str() {
        "offline" => None,
        "" => Some("Waiting to hear whether the show is on air.".into()),
        mode => Some(format!("The show is in {}. Archive work waits until you're off air.", mode_label(mode))),
    }
}

fn app_live_reason(app: &App) -> Option<String> {
    live_reason(app.m.connected, |k| app.m.get(k).cloned())
}

// ---- formatting -------------------------------------------------------------------------------------

pub(crate) fn gb(x: f64) -> String {
    if x >= 1000.0 {
        format!("{:.1} TB", x / 1000.0)
    } else if x >= 10.0 {
        format!("{x:.0} GB")
    } else {
        format!("{x:.1} GB")
    }
}

fn feed_state_words(state: &str) -> &'static str {
    match state {
        "starting" => "starting",
        "recording" => "recording",
        "retrying" => "trying again",
        "shed" => "paused: computer busy",
        "low_disk" => "stopped: disk almost full",
        "failed" => "failed",
        "stopped" => "stopped",
        _ => "",
    }
}

/// Archive steps, in the engine's order.
fn stage_words(stage: &str) -> String {
    match stage {
        "wait_offline" => "Waiting".into(),
        "iso_cleanup" => "Camera cleanup".into(),
        "encode" => "Shrinking video".into(),
        "verify" => "Checking".into(),
        "hold" => "Keeping full quality".into(),
        "package" => "Packing data".into(),
        "commit" | "cleanup" => "Moving".into(),
        "done" => "Done".into(),
        other => {
            let mut c = other.replace('_', " ");
            if let Some(f) = c.get_mut(0..1) {
                f.make_ascii_uppercase();
            }
            c
        }
    }
}

fn row_state(row: &Value) -> &str {
    row.get_path("state").and_then(Value::as_str).unwrap_or("")
}

/// 0–1 progress (the engine may report 0–1 or a percentage).
fn fraction(p: f64) -> f32 {
    (if p > 1.0 { p / 100.0 } else { p }).clamp(0.0, 1.0) as f32
}

/// A left-aligned, truncated label of fixed width (lines rows up as columns).
fn column(ui: &mut Ui, width: f32, text: RichText) {
    ui.allocate_ui_with_layout(egui::vec2(width, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_width(width);
        ui.add(egui::Label::new(text).truncate());
    });
}

fn shows_to_archive(n: usize) -> String {
    format!("{n} show{} to archive", if n == 1 { "" } else { "s" })
}

fn health_color(t: &Theme, status: &str) -> egui::Color32 {
    match status {
        "pass" => t.green,
        "warn" => t.yellow,
        "fail" => t.bright_red,
        _ => t.text_dim,
    }
}

fn health(app: &App, key: &str) -> (String, String) {
    let v = app.m.get(key);
    let s = |k: &str| v.and_then(|v| v.get_path(k)).and_then(Value::as_str).unwrap_or("").to_string();
    (s("status"), s("detail"))
}

/// Recorded bytes so far across every feed of the running recording.
fn recorded_bytes(status: &Value) -> i64 {
    status.get_path("feeds").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|f| f.get_path("bytes").and_then(Value::as_i64)).sum()
}

fn recording_free_gb(app: &App, status: Option<&Value>) -> Option<f64> {
    app.m
        .get("recording.dir_free_gb")
        .and_then(Value::as_f64)
        .or_else(|| status.and_then(|s| s.get_path("dir_free_gb").or_else(|| s.get_path("free_gb"))).and_then(Value::as_f64))
}

fn archive_queue(app: &App) -> Vec<Value> {
    app.m.get("archive.queue").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default()
}

// ---- view state -------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Chooser {
    Recording,
    Archive,
    Fallback,
}

#[derive(Default)]
pub struct RecordingState {
    draft: Option<Draft>,
    saved: Option<Draft>,
    asked: Option<Instant>,
    status_asked: Option<Instant>,
    seq: u64,
    conn: u64,
    chooser: Option<(Chooser, Receiver<Result<Option<PathBuf>, String>>)>,
    folders: HashMap<PathBuf, (Instant, Folder)>,
}

impl RecordingState {
    fn folder(&mut self, path: &Path) -> Folder {
        if let Some((at, f)) = self.folders.get(path)
            && at.elapsed().as_secs() < FOLDER_FRESH_SECS
        {
            return f.clone();
        }
        let f = inspect(path);
        if self.folders.len() > 32 {
            self.folders.clear();
        }
        self.folders.insert(path.to_path_buf(), (Instant::now(), f.clone()));
        f
    }
}

/// The portal's file:// URI is percent-encoded; keep filesystem bytes intact.
fn folder_uri(uri: &str) -> Option<PathBuf> {
    let raw = uri.strip_prefix("file://")?;
    let raw = if raw.starts_with('/') { raw } else { raw.strip_prefix("localhost")? };
    if !raw.starts_with('/') {
        return None;
    }
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut n = 0;
    while n < bytes.len() {
        if bytes[n] == b'%'
            && n + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[n + 1..n + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            n += 3;
        } else {
            out.push(bytes[n]);
            n += 1;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// The desktop's folder picker (xdg portal), answered on a background thread.
fn choose_folder(title: &'static str) -> Receiver<Result<Option<PathBuf>, String>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("se-ui-recording-folder".into()).spawn({
        let tx = tx.clone();
        move || {
            let result = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt.block_on(async {
                    use ashpd::desktop::ResponseError;
                    use ashpd::desktop::file_chooser::SelectedFiles;
                    let answer =
                        SelectedFiles::open_file().title(title).accept_label("Use this folder").directory(true).modal(true).send().await.map_err(|e| e.to_string())?;
                    match answer.response() {
                        Ok(files) => files
                            .uris()
                            .first()
                            .map(|uri| folder_uri(uri.as_str()).ok_or_else(|| "This folder isn't on your computer. Choose a local folder instead.".to_string()))
                            .transpose(),
                        Err(ashpd::Error::Response(ResponseError::Cancelled)) => Ok(None),
                        Err(e) => Err(e.to_string()),
                    }
                }),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(result);
        }
    });
    if let Err(e) = spawned {
        let _ = tx.send(Err(e.to_string()));
    }
    rx
}

// ---- Settings page ----------------------------------------------------------------------------------

fn refresh(app: &mut App) {
    let st = &mut app.settings.recording;
    if st.conn != app.m.conn_gen {
        st.conn = app.m.conn_gen;
        st.asked = None;
        st.status_asked = None;
    }
    if !app.m.connected {
        return;
    }
    if st.asked.is_none_or(|at| at.elapsed().as_secs() >= 10) {
        st.asked = Some(Instant::now());
        app.m.query_as("settings.recording.project", "project.read", Value::map().with("path", "project.toml"));
        app.m.query("recording.sources", Value::Null);
        app.m.query("sources", Value::Null);
    }
    let st = &mut app.settings.recording;
    if st.status_asked.is_none_or(|at| at.elapsed().as_secs() >= 3) {
        st.status_asked = Some(Instant::now());
        app.m.query("recording.status", Value::Null);
    }
    let seq = app.m.q_seq("settings.recording.project");
    let st = &mut app.settings.recording;
    if seq != st.seq {
        st.seq = seq;
        if let Some(doc) = app
            .m
            .q("settings.recording.project")
            .and_then(|v| v.get_path("text"))
            .and_then(Value::as_str)
            .and_then(|text| toml::from_str::<toml::Value>(text).ok())
        {
            let loaded = Draft::from_project(&doc);
            if st.draft.is_none() || st.draft == st.saved {
                st.draft = Some(loaded.clone());
            }
            st.saved = Some(loaded);
        }
    }
    let Some((target, rx)) = &app.settings.recording.chooser else { return };
    let target = *target;
    let answer = match rx.try_recv() {
        Err(TryRecvError::Empty) => return,
        Ok(answer) => answer,
        Err(TryRecvError::Disconnected) => Err("the folder chooser closed".into()),
    };
    app.settings.recording.chooser = None;
    match answer {
        Ok(Some(folder)) => {
            if let Some(d) = &mut app.settings.recording.draft {
                let path = folder.to_string_lossy().into_owned();
                match target {
                    Chooser::Recording => d.dir = path,
                    Chooser::Archive => d.archive.dir = path,
                    Chooser::Fallback => d.fallback_dir = path,
                }
            }
        }
        Ok(None) => {}
        Err(e) => app.m.toast(format!("Couldn't choose a folder: {e}. Type its path instead."), true),
    }
}

/// Camera sources of the project: (id, label, connected). `None` while unknown.
fn camera_sources(app: &App) -> Option<Vec<(String, String, bool)>> {
    let list = app.m.q("sources")?.as_list().unwrap_or(&[]);
    let mut cams: Vec<(String, String, bool)> = list
        .iter()
        .filter(|s| s.get_path("kind").and_then(Value::as_str) == Some("camera"))
        .filter_map(|s| {
            let id = s.get_path("name").and_then(Value::as_str)?.to_string();
            let label = s.get_path("label").and_then(Value::as_str).filter(|l| !l.is_empty()).unwrap_or(&id).to_string();
            Some((id, label, !s.get_path("missing").is_some_and(Value::truthy)))
        })
        .collect();
    cams.sort();
    Some(cams)
}

/// Settings → Recording: the status card, then the settings card.
pub fn settings(app: &mut App, ui: &mut Ui, t: &Theme) {
    refresh(app);
    status_card(app, ui, t);
    ui.add_space(spacing::L);
    settings_card(app, ui, t);
}

fn settings_card(app: &mut App, ui: &mut Ui, t: &Theme) {
    widgets::titled(ui, t, "Recording", "Every show is recorded and kept. Choose what gets recorded and where it goes.", |_| {}, |ui| {
        ui.set_width(ui.available_width());
        let Some(mut d) = app.settings.recording.draft.take() else {
            let failed = app.m.query_errors.contains_key("settings.recording.project");
            let body = if !app.m.connected {
                "Connect to Stream Engine to load this project's settings."
            } else if failed {
                "Couldn't read project.toml. Try again or check Troubleshooting."
            } else {
                "Loading project.toml…"
            };
            if widgets::empty_state(ui, t, icon::FILM, "Recording settings unavailable", body, Some("Reload")) {
                app.settings.recording.asked = None;
            }
            return;
        };
        let cameras = camera_sources(app);
        let home = home();

        // -- automatic recording
        widgets::toggle_row(ui, t, "Record every stream automatically", "Recording starts when the show starts and stops when you go off air.", &mut d.auto);
        if !d.auto {
            widgets::callout(ui, t, Tone::Warn, icon::WARN, "Streams won't be recorded", "Turn this back on so no show is lost. You can still start a recording by hand on Clipping.", None);
        }
        ui.add_space(spacing::M);

        // -- what gets recorded
        widgets::section(ui, t, icon::FILM, "What gets recorded");
        feeds(app, ui, t, &mut d, cameras.as_deref());
        ui.add_space(spacing::S);
        sound_tracks(app, ui, t, &mut d);
        let hot = recording_gb_per_hour(&d.video, d.audio.len());
        if hot > 0.0 {
            widgets::hint(ui, t, &format!("While a show records this uses up to about {} per hour ({} for a 3-hour show).", gb(hot), gb(hot * SHOW_HOURS)));
        }
        ui.add_space(spacing::M);

        // -- folders
        widgets::section(ui, t, icon::SAVE, "Where recordings go");
        let rec_folder = app.settings.recording.folder(&expand(&d.dir, &home));
        folder_row(app, ui, t, "Recording folder", "Shows are recorded here at full quality.", &mut d.dir, DEFAULT_DIR, Chooser::Recording, Some(&rec_folder));
        if let Some(free) = rec_folder.free_gb.filter(|_| hot > 0.0) {
            let hours = free / hot;
            if hours < SHOW_HOURS {
                widgets::callout(ui, t, Tone::Warn, icon::WARN, "Not enough space for a full show", &format!("{} free is about {:.1} hours of recording. Free up space or choose another drive.", gb(free), hours), None);
            }
        }
        ui.add_space(spacing::S);
        let archive_folder = app.settings.recording.folder(&archive_path(&d, &home));
        folder_row(app, ui, t, "Archive folder", "Finished shows are shrunk and kept here for good. Blank = “Archive” inside the recording folder.", &mut d.archive.dir, "Inside the recording folder", Chooser::Archive, Some(&archive_folder));
        ui.add_space(spacing::M);

        // -- archive
        widgets::section(ui, t, icon::SESSION, "Archive");
        widgets::toggle_row(ui, t, "Shrink finished shows into the archive", "After the show (never while you're live) the full-quality files are made smaller and moved to the archive folder.", &mut d.archive.enabled);
        ui.add_enabled_ui(d.archive.enabled, |ui| archive_size(ui, t, &mut d, &archive_folder));
        ui.add_space(spacing::S);
        ui.horizontal_wrapped(|ui| {
            ui.label("Delete camera files after their clips are reviewed, or after");
            ui.add(egui::DragValue::new(&mut d.archive.iso_keep_days).range(1..=365).suffix(" days"));
        });
        widgets::hint(ui, t, "Only the separate camera files are deleted. The main recording is always kept (in the archive). Shows marked to keep are never touched.");
        ui.add_space(spacing::M);

        // -- advanced
        widgets::details(ui, t, "recording-advanced", "More options", |ui| advanced(app, ui, t, &mut d));
        ui.add_space(spacing::M);

        // -- save
        let errors = d.errors(cameras.as_ref().map(|c| c.iter().map(|(id, _, _)| id.clone()).collect::<Vec<_>>()).as_deref());
        for e in &errors {
            ui.label(RichText::new(format!("{}  {e}", icon::WARN)).color(t.bright_red));
        }
        let dirty = app.settings.recording.saved.as_ref() != Some(&d);
        ui.horizontal_wrapped(|ui| {
            if widgets::button_ex(ui, t, None, "Save recording settings", Kind::Primary, Size::Medium, 0.0, app.m.connected && dirty && errors.is_empty()).clicked() {
                d.normalize();
                app.m.action("project.write", Value::map().with("path", "project.toml").with("set", d.changes()));
                app.settings.recording.asked = None;
            }
            if widgets::button_ex(ui, t, None, "Discard changes", Kind::Ghost, Size::Medium, 0.0, dirty).clicked()
                && let Some(saved) = &app.settings.recording.saved
            {
                d = saved.clone();
            }
            widgets::hint(ui, t, if dirty { "Not saved yet. Changes apply to the next recording." } else { "Saved." });
        });
        app.settings.recording.draft = Some(d);
    });
}

/// Main recording, the tall picture and the cameras.
fn feeds(app: &App, ui: &mut Ui, t: &Theme, d: &mut Draft, cameras: Option<&[(String, String, bool)]>) {
    // main recording
    ui.label(RichText::new("Main recording").font(font_medium(type_scale::BODY)).color(t.fg));
    widgets::hint(ui, t, "The whole show picture with every sound track. Clips are made from this.");
    if d.masters() == 0 && widgets::button(ui, t, "Record the show picture", Kind::Secondary).clicked() {
        let name = d.free_name("main");
        d.video.insert(0, Feed::master(&name, "canvas:wide"));
    }
    if let Some(m) = d.master_mut() {
        ui.horizontal_wrapped(|ui| {
            ui.label("Picture quality");
            let custom = m.quality().is_none();
            let mut labels: Vec<&str> = QUALITY.iter().map(|(l, _, _)| *l).collect();
            if custom {
                labels.push("Custom");
            }
            let mut sel = m.quality().unwrap_or(QUALITY.len());
            if widgets::segmented(ui, t, &mut sel, &labels)
                && let Some(&(_, cq, max)) = QUALITY.get(sel)
            {
                m.cq = cq;
                m.max_mbps = max;
            }
        });
        widgets::hint(ui, t, "High is recommended: full quality for clips. Maximum makes much bigger files.");
    }
    // the tall (phone) picture
    let tall = d.video.iter().position(|f| f.source == "canvas:tall" && f.role == Role::Iso);
    let mut on = tall.is_some();
    if widgets::toggle_row(ui, t, "Also record the tall picture", "A separate picture-only file of the phone-shaped canvas.", &mut on).changed() {
        match tall {
            Some(i) if !on => {
                d.video.remove(i);
            }
            None if on => {
                let name = d.free_name("tall");
                d.video.push(Feed::iso(&name, "canvas:tall", None, 8.0));
            }
            _ => {}
        }
    }
    ui.add_space(spacing::S);

    // cameras
    ui.label(RichText::new("Cameras").font(font_medium(type_scale::BODY)).color(t.fg));
    widgets::hint(ui, t, "Each camera can also be kept as its own picture-only file, for clips from another angle.");
    let mut rows: Vec<(String, String, Option<bool>)> = cameras.unwrap_or(&[]).iter().map(|(id, label, ok)| (id.clone(), label.clone(), Some(*ok))).collect();
    for f in &d.video {
        if let Some(id) = f.camera()
            && !rows.iter().any(|(r, _, _)| r == id)
        {
            rows.push((id.to_string(), id.to_string(), None));
        }
    }
    if rows.is_empty() {
        widgets::hint(ui, t, if cameras.is_none() && !app.m.connected { "Cameras show up when Stream Engine is running." } else { "No camera sources yet. Add one in Sources." });
    }
    for (id, label, state) in rows {
        ui.push_id(("rec-camera", &id), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(icon::CAMERA).color(t.text_dim));
                column(ui, 200.0, RichText::new(&label).color(t.fg));
                let feed = d.camera_feed(&id);
                let custom = feed.is_some_and(|f| f.size().is_none());
                let mut labels = vec!["Off"];
                labels.extend(CAMERA_SIZES.iter().map(|(l, _, _)| *l));
                if custom {
                    labels.push("Custom");
                }
                let mut sel = match feed {
                    None => 0,
                    Some(f) => f.size().map_or(labels.len() - 1, |i| i + 1),
                };
                if widgets::segmented(ui, t, &mut sel, &labels) && (sel == 0 || sel <= CAMERA_SIZES.len()) {
                    d.set_camera(&id, sel.checked_sub(1));
                }
                match state {
                    None => {
                        ui.label(RichText::new("not a source anymore").color(t.bright_red));
                    }
                    Some(false) => {
                        ui.label(RichText::new("not connected").color(t.yellow));
                    }
                    Some(true) => {}
                }
            });
        });
    }
    let big = d.video.iter().filter(|f| f.camera().is_some() && f.height.is_some_and(|h| h >= 1080)).count();
    if big > 1 {
        widgets::hint(ui, t, "Tip: one main camera at 1080p and the others at 720p saves a lot of space and keeps the graphics card relaxed.");
    } else if d.video.iter().any(|f| f.camera().is_some()) {
        widgets::hint(ui, t, "Recommended: your main camera at 1080p, the others at 720p.");
    }
}

/// Sound tracks of the main recording.
fn sound_tracks(app: &App, ui: &mut Ui, t: &Theme, d: &mut Draft) {
    ui.label(RichText::new("Sound tracks").font(font_medium(type_scale::BODY)).color(t.fg));
    widgets::hint(ui, t, "Each one becomes its own track in the main recording, so clips can leave the music out. Camera files have no sound.");
    let choices: Vec<(String, String, String)> = app
        .m
        .q("recording.sources")
        .and_then(|v| v.get_path("audio"))
        .and_then(Value::as_list)
        .unwrap_or(&[])
        .iter()
        .map(|c| {
            let s = |k| c.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
            (s("name"), s("source"), s("format"))
        })
        .collect();
    let mut remove = None;
    for (i, row) in d.audio.iter_mut().enumerate() {
        ui.push_id(("rec-audio", i), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(icon::VOLUME).color(t.text_dim));
                ui.add(widgets::field(&mut row.name).desired_width(130.0).hint_text("Track name"));
                let shown = if row.source.is_empty() { "Choose input…".to_string() } else { row.source.clone() };
                egui::ComboBox::from_id_salt("input").width(240.0).selected_text(shown).show_ui(ui, |ui| {
                    let mut choose = |label: &str, source: &str, format: &str| {
                        if ui.selectable_label(row.source == source && row.format == format, label).clicked() {
                            if row.name.trim().is_empty() {
                                row.name = label.split(" · ").next().unwrap_or("").into();
                            }
                            row.source = source.into();
                            row.format = format.into();
                        }
                    };
                    choose("Mix · your band (se-band)", "se-band", "pulse");
                    choose("Music · song requests (se-music)", "se-music", "pulse");
                    for (name, source, format) in &choices {
                        if source != "se-band" && source != "se-music" {
                            choose(&format!("{name} · {source}"), source, format);
                        }
                    }
                });
                if widgets::icon_button(ui, t, icon::TRASH, "Remove this track").clicked() {
                    remove = Some(i);
                }
            });
        });
    }
    if let Some(i) = remove {
        d.audio.remove(i);
    }
    if d.audio.is_empty() {
        widgets::hint(ui, t, "No sound tracks: the main recording will be silent.");
    }
    if widgets::button(ui, t, "Add sound track", Kind::Secondary).clicked() {
        d.audio.push(Track::new("", "", "pulse"));
    }
    if app.m.query_errors.contains_key("recording.sources") {
        widgets::hint(ui, t, "Can't list sound inputs right now. Type the input name instead (More options).");
    }
}

#[allow(clippy::too_many_arguments)]
fn folder_row(app: &mut App, ui: &mut Ui, t: &Theme, title: &str, help: &str, path: &mut String, placeholder: &str, target: Chooser, info: Option<&Folder>) {
    ui.label(RichText::new(title).font(font_medium(type_scale::BODY)).color(t.fg));
    widgets::hint(ui, t, help);
    ui.horizontal_wrapped(|ui| {
        ui.add(widgets::field(path).desired_width(ui.available_width().min(520.0)).hint_text(placeholder));
        let busy = app.settings.recording.chooser.is_some();
        if widgets::button_ex(ui, t, None, "Browse…", Kind::Secondary, Size::Medium, 0.0, !busy).clicked() {
            let title = match target {
                Chooser::Recording => "Choose where to record shows",
                Chooser::Archive => "Choose where to keep the archive",
                Chooser::Fallback => "Choose a backup recording folder",
            };
            app.settings.recording.chooser = Some((target, choose_folder(title)));
        }
    });
    let Some(f) = info else { return };
    let free = f.free_gb.map(|g| format!("{} free", gb(g))).unwrap_or_default();
    if !f.exists {
        let line = format!("{}  This folder doesn't exist yet. It will be created when it's needed{}.", icon::INFO, if free.is_empty() { String::new() } else { format!(" ({free} on that drive)") });
        ui.label(RichText::new(line).color(t.yellow));
    } else if !f.writable {
        ui.label(RichText::new(format!("{}  The app can't write to this folder. Choose another one.", icon::WARN)).color(t.bright_red));
    } else {
        ui.label(RichText::new(format!("{}  {free}", icon::CHECK)).color(t.text_dim));
    }
    if f.system_drive {
        ui.label(RichText::new(format!("{}  This is on the system drive. If it fills up the computer slows down; a separate drive is safer for recordings.", icon::WARN)).color(t.yellow));
    }
    ui.label(RichText::new(f.path.display().to_string()).font(font_mono(type_scale::SMALL)).color(t.text_faint));
}

fn archive_size(ui: &mut Ui, t: &Theme, d: &mut Draft, folder: &Folder) {
    ui.add_space(spacing::S);
    ui.label(RichText::new("Archive size per 3-hour show").font(font_medium(type_scale::BODY)).color(t.fg));
    let mut show = gb_per_show(d.archive.gb_per_hour);
    ui.spacing_mut().slider_width = 320.0;
    let r = ui.add(egui::Slider::new(&mut show, GB_PER_SHOW_MIN..=GB_PER_SHOW_MAX).step_by(0.1).fixed_decimals(1).suffix(" GB"));
    if r.changed() {
        d.archive.gb_per_hour = gb_per_hour(show);
    }
    let mbps = archive_video_mbps(d.archive.gb_per_hour, d.audio.len(), d.archive.audio_kbps);
    let words = if mbps < 2.5 {
        "small files; fast drumming may look a little soft"
    } else if mbps < 5.0 {
        "good for watching shows back"
    } else {
        "very close to the original"
    };
    widgets::hint(ui, t, &format!("About {} per hour of show · picture about {mbps:.1} Mbps: {words}. 5.4 GB is the recommended size.", gb(d.archive.gb_per_hour)));
    if let Some(free) = folder.free_gb {
        let shows = (free / gb_per_show(d.archive.gb_per_hour).max(0.1)).floor();
        widgets::hint(ui, t, &format!("The archive drive has room for about {shows:.0} more shows ({} free).", gb(free)));
    }
}

fn advanced(app: &mut App, ui: &mut Ui, t: &Theme, d: &mut Draft) {
    ui.label(RichText::new("Start recording when the show goes to").color(t.fg));
    ui.horizontal_wrapped(|ui| {
        for mode in START_MODES {
            let on = d.modes.iter().any(|m| m == mode);
            if widgets::chip(ui, t, if on { icon::CHECK } else { icon::PLUS }, &mode_label(mode), on).clicked() {
                if on {
                    d.modes.retain(|m| m != mode);
                } else {
                    d.modes.push(mode.into());
                }
            }
        }
    });
    widgets::hint(ui, t, "Once started, recording keeps going until the show is off air.");
    ui.add_space(spacing::S);
    ui.horizontal_wrapped(|ui| {
        ui.label("Frames per second");
        let mut sel = match d.fps {
            30 => 0,
            60 => 1,
            _ => 2,
        };
        let labels: &[&str] = if sel == 2 { &["30", "60", "Other"] } else { &["30", "60"] };
        if widgets::segmented(ui, t, &mut sel, labels) && sel < 2 {
            d.fps = [30, 60][sel];
        }
        if sel == 2 {
            ui.add(egui::DragValue::new(&mut d.fps).range(1..=120));
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.label("Video encoding");
        let encoders = [("auto", "Automatic"), ("nvenc", "Graphics card"), ("software", "Processor")];
        let shown = encoders.iter().find(|(k, _)| *k == d.encoder).map_or(d.encoder.as_str(), |(_, l)| l).to_string();
        egui::ComboBox::from_id_salt("recording.encoder").selected_text(shown).show_ui(ui, |ui| {
            for (key, label) in encoders {
                ui.selectable_value(&mut d.encoder, key.into(), label);
            }
        });
    });
    if let Some(m) = d.master_mut() {
        ui.horizontal_wrapped(|ui| {
            ui.label("Main recording");
            egui::ComboBox::from_id_salt("recording.master.source")
                .selected_text(match m.source.as_str() {
                    "canvas:wide" => "Wide picture",
                    "canvas:tall" => "Tall picture",
                    other => other,
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut m.source, "canvas:wide".into(), "Wide picture");
                    ui.selectable_value(&mut m.source, "canvas:tall".into(), "Tall picture");
                });
            egui::ComboBox::from_id_salt("recording.master.codec").selected_text(m.codec.to_uppercase()).show_ui(ui, |ui| {
                ui.selectable_value(&mut m.codec, "hevc".into(), "HEVC (smaller)");
                ui.selectable_value(&mut m.codec, "h264".into(), "H264 (plays anywhere)");
            });
            ui.label("Quality (lower = better)");
            ui.add(egui::DragValue::new(&mut m.cq).range(0..=51));
            ui.label("At most");
            ui.add(egui::DragValue::new(&mut m.max_mbps).range(1.0..=200.0).suffix(" Mbps"));
        });
    }
    ui.add_space(spacing::S);
    ui.horizontal_wrapped(|ui| {
        ui.label("Keep at least");
        ui.add(egui::DragValue::new(&mut d.min_free_gb).range(1.0..=2000.0).suffix(" GB"));
        ui.label("free: camera files stop first, then the main recording.");
    });
    let home = home();
    let fallback = app.settings.recording.folder(&fallback_path(d, &home));
    folder_row(app, ui, t, "Backup recording folder", "Used for the main recording only if the recording folder is missing when a show starts. Blank = Videos/Stream Engine (fallback).", &mut d.fallback_dir, "Videos/Stream Engine (fallback)", Chooser::Fallback, Some(&fallback));
    ui.add_space(spacing::S);
    ui.horizontal_wrapped(|ui| {
        ui.label("Start archiving");
        ui.add(egui::DragValue::new(&mut d.archive.min_offline_minutes).range(0..=1440).suffix(" min"));
        ui.label("after the show ends · sound in the archive");
        ui.add(egui::DragValue::new(&mut d.archive.audio_kbps).range(32..=512).suffix(" kbps"));
        ui.label("per track");
    });
    ui.add_space(spacing::S);
    other_inputs(ui, t, d);
    ui.add_space(spacing::S);
    widgets::toggle_row(ui, t, "Keep project snapshot", "Copy this project's setup into each show folder.", &mut d.snapshot_project);
    widgets::toggle_row(ui, t, "Build show index", "Index finished recordings for timeline review and clipping.", &mut d.index_enabled);
    let mut journal = !d.index_skip.iter().any(|s| s == "journal");
    if widgets::toggle_row(ui, t, "Show context", "Songs, scenes, modes, lights, effects, chat, moments and markers.", &mut journal).changed() {
        d.index_skip.retain(|s| s != "journal");
        if !journal {
            d.index_skip.push("journal".into());
        }
    }
    let mut signals = !d.index_skip.iter().any(|s| s == "signals");
    if widgets::toggle_row(ui, t, "Sampled signals", "Recorded hype, audio levels, chat rate and queue position.", &mut signals).changed() {
        d.index_skip.retain(|s| s != "signals");
        if !signals {
            d.index_skip.push("signals".into());
        }
    }
    let mut transcript = d.index_transcript && !d.index_skip.iter().any(|s| s == "transcript");
    if widgets::toggle_row(ui, t, "Speech transcript", "Analyze recorded speech for captions and review.", &mut transcript).changed() {
        d.index_transcript = transcript;
        d.index_skip.retain(|s| s != "transcript");
    }
    ui.horizontal_wrapped(|ui| {
        ui.label("Frames socket override (blank = engine default)");
        ui.add(widgets::field(&mut d.frames_socket).desired_width(320.0));
    });
}

/// Feeds that are neither the main recording, a camera nor the tall picture (external inputs),
/// and exact sound input names.
fn other_inputs(ui: &mut Ui, t: &Theme, d: &mut Draft) {
    ui.label(RichText::new("Other inputs").font(font_medium(type_scale::BODY)).color(t.fg));
    widgets::hint(ui, t, "Exact capture inputs (an FFmpeg source and its format, for example v4l2). Picture only.");
    let mut remove = None;
    for (i, f) in d.video.iter_mut().enumerate() {
        if f.role == Role::Master || f.camera().is_some() || f.source == "canvas:tall" {
            continue;
        }
        ui.push_id(("rec-other", i), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.add(widgets::field(&mut f.name).desired_width(110.0).hint_text("Name"));
                ui.add(widgets::field(&mut f.source).desired_width(240.0).hint_text("Source"));
                ui.add(widgets::field(&mut f.format).desired_width(90.0).hint_text("Format"));
                ui.add(egui::DragValue::new(&mut f.mbps).range(0.5..=100.0).speed(0.1).suffix(" Mbps"));
                if widgets::icon_button(ui, t, icon::TRASH, "Stop recording this input").clicked() {
                    remove = Some(i);
                }
            });
        });
    }
    if let Some(i) = remove {
        d.video.remove(i);
    }
    if widgets::button(ui, t, "Add input", Kind::Ghost).clicked() {
        let name = d.free_name("input");
        d.video.push(Feed { format: "v4l2".into(), ..Feed::iso(&name, "", None, 8.0) });
    }
    ui.add_space(spacing::S);
    ui.label(RichText::new("Sound inputs (exact names)").color(t.fg));
    for (i, a) in d.audio.iter_mut().enumerate() {
        ui.push_id(("rec-audio-exact", i), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(if a.name.is_empty() { "(unnamed)" } else { a.name.as_str() }).color(t.text_dim));
                ui.add(widgets::field(&mut a.source).desired_width(240.0).hint_text("Source"));
                ui.add(widgets::field(&mut a.format).desired_width(90.0).hint_text("Format"));
            });
        });
    }
}

// ---- status -----------------------------------------------------------------------------------------

/// "main + 2 cameras" for the feeds now recording.
fn feeds_summary(status: &Value) -> String {
    let feeds = status.get_path("feeds").and_then(Value::as_list).unwrap_or(&[]);
    let live: Vec<&Value> = feeds.iter().filter(|f| matches!(f.get_path("state").and_then(Value::as_str), Some("recording" | "starting" | "retrying"))).collect();
    let main = live.iter().find(|f| f.get_path("role").and_then(Value::as_str) == Some("master")).and_then(|f| f.get_path("name").and_then(Value::as_str));
    let others = live.iter().filter(|f| f.get_path("role").and_then(Value::as_str) != Some("master")).count();
    match (main, others) {
        (Some(m), 0) => m.to_string(),
        (Some(m), 1) => format!("{m} + 1 camera"),
        (Some(m), n) => format!("{m} + {n} cameras"),
        (None, 0) => String::new(),
        (None, n) => format!("{n} camera file{}", if n == 1 { "" } else { "s" }),
    }
}

fn status_card(app: &mut App, ui: &mut Ui, t: &Theme) {
    let status = app.m.q("recording.status").cloned();
    let (rec_status, rec_detail) = health(app, "health.recording");
    let (arc_status, arc_detail) = health(app, "health.archive");
    let active = app.m.b("recording.active") || status.as_ref().is_some_and(|s| s.get_path("active").is_some_and(Value::truthy));
    widgets::titled(
        ui,
        t,
        "Recording now",
        "What the recorder and the archive are doing.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if !app.m.connected {
                widgets::hint(ui, t, "Shows up when Stream Engine is running.");
                return;
            }
            // recorder
            ui.horizontal_wrapped(|ui| {
                widgets::badge(ui, t, if active { "Recording" } else { "Not recording" }, health_color(t, &rec_status));
                if !rec_detail.is_empty() {
                    ui.label(RichText::new(&rec_detail).color(t.fg));
                }
            });
            if let Some(s) = &status {
                let bytes = recorded_bytes(s);
                let mut facts = Vec::new();
                if active && bytes > 0 {
                    facts.push(format!("{} so far", gb(bytes as f64 / 1e9)));
                }
                if let Some(free) = recording_free_gb(app, Some(s)) {
                    facts.push(format!("{} free in the recording folder", gb(free)));
                }
                if !facts.is_empty() {
                    widgets::hint(ui, t, &facts.join(" · "));
                }
                if s.get_path("fallback").is_some_and(Value::truthy) {
                    let why = s.get_path("fallback_reason").and_then(Value::as_str).filter(|r| !r.is_empty()).map(|r| format!(" ({r})")).unwrap_or_default();
                    let body = format!("The recording folder couldn't be used when the show started{why}. Check the drive, then look in the backup folder for this show.");
                    widgets::callout(ui, t, Tone::Warn, icon::WARN, "Recording to the backup folder", &body, None);
                }
                for f in s.get_path("feeds").and_then(Value::as_list).unwrap_or(&[]) {
                    let g = |k: &str| f.get_path(k).and_then(Value::as_str).unwrap_or("");
                    let words = feed_state_words(g("state"));
                    let size = f.get_path("bytes").and_then(Value::as_i64).filter(|b| *b > 0).map(|b| format!(" · {}", gb(b as f64 / 1e9))).unwrap_or_default();
                    let dropped = f.get_path("dropped").and_then(Value::as_i64).filter(|d| *d > 0).map(|d| format!(" · {d} frames skipped")).unwrap_or_default();
                    let detail = if g("detail").is_empty() { String::new() } else { format!(" · {}", g("detail")) };
                    let bad = matches!(g("state"), "failed" | "low_disk" | "shed" | "retrying");
                    let kind = if g("role") == "master" { "main" } else { "camera" };
                    ui.label(RichText::new(format!("{}  {} ({kind}): {words}{size}{dropped}{detail}", if bad { icon::WARN } else { icon::REC }, g("name"))).color(if bad { t.yellow } else { t.text_dim }));
                }
            } else if app.m.query_errors.contains_key("recording.status") {
                widgets::hint(ui, t, "Recorder status is unavailable right now.");
            }
            ui.add_space(spacing::M);

            // archive
            let queue = archive_queue(app);
            let paused = app.m.b("archive.paused");
            let failed = queue.iter().filter(|r| row_state(r) == "failed").count();
            ui.horizontal_wrapped(|ui| {
                let label = match arc_status.as_str() {
                    _ if paused => "Archive paused",
                    "pass" => "Archive up to date",
                    "warn" if !queue.is_empty() => "Waiting to archive",
                    "warn" => "Archive needs a look",
                    "fail" => "Archive problem",
                    _ if !queue.is_empty() => "Archive working",
                    _ => "Archive",
                };
                widgets::badge(ui, t, label, if paused { t.yellow } else { health_color(t, &arc_status) });
                if !arc_detail.is_empty() {
                    ui.label(RichText::new(&arc_detail).color(t.fg));
                }
            });
            // 0 = the folder is missing (health says so)
            if let Some(free) = app.m.get("archive.dir_free_gb").and_then(Value::as_f64).filter(|g| *g > 0.0) {
                widgets::hint(ui, t, &format!("{} free in the archive folder", gb(free)));
            }
            let blocked = app_live_reason(app);
            let enabled = blocked.is_none();
            let tip = blocked.clone().unwrap_or_default();
            for row in &queue {
                let g = |k: &str| row.get_path(k).and_then(Value::as_str).unwrap_or("");
                let state = row_state(row);
                ui.push_id(("archive-row", g("session"), g("show")), |ui| {
                    ui.horizontal(|ui| {
                        column(ui, 220.0, RichText::new(g("show")).color(t.fg));
                        let step = if state == "failed" { format!("Failed: {}", stage_words(g("stage")).to_lowercase()) } else { stage_words(g("stage")) };
                        column(ui, 170.0, RichText::new(step).color(if state == "failed" { t.bright_red } else { t.text_dim }));
                        let p = row.get_path("progress").and_then(Value::as_f64).unwrap_or(0.0);
                        ui.add(egui::ProgressBar::new(fraction(p)).desired_width(160.0).show_percentage());
                        if state == "failed" {
                            let retry = widgets::button_ex(ui, t, Some(icon::UNDO), "Retry", Kind::Ghost, Size::Small, 0.0, enabled).on_disabled_hover_text(&tip);
                            if retry.clicked() {
                                app.m.action(ARCHIVE_RETRY, Value::map().with("show", g("show")));
                            }
                        }
                    });
                    if !g("detail").is_empty() {
                        ui.label(RichText::new(g("detail")).size(type_scale::SMALL + 0.5).color(if state == "failed" { t.bright_red } else { t.text_dim }));
                    }
                });
            }
            ui.horizontal_wrapped(|ui| {
                let (label, help) = if paused {
                    ("Resume archiving", "Carry on where it stopped.")
                } else {
                    ("Archive now", "Start now instead of waiting after the show (still waits while you're live).")
                };
                let run = widgets::button_ex(ui, t, Some(icon::PLAY), label, Kind::Secondary, Size::Small, 0.0, enabled).on_hover_text(help).on_disabled_hover_text(&tip);
                if run.clicked() {
                    app.m.action(ARCHIVE_RUN, Value::Null);
                }
                if !paused {
                    let pause = widgets::button_ex(ui, t, Some(icon::PAUSE), "Pause", Kind::Ghost, Size::Small, 0.0, enabled)
                        .on_hover_text("Stop archive work until you resume it.")
                        .on_disabled_hover_text(&tip);
                    if pause.clicked() {
                        app.m.action(ARCHIVE_PAUSE, Value::Null);
                    }
                }
                if failed > 0 {
                    let retry = widgets::button_ex(ui, t, Some(icon::UNDO), "Retry failed", Kind::Ghost, Size::Small, 0.0, enabled)
                        .on_hover_text("Try every show that failed to archive again.")
                        .on_disabled_hover_text(&tip);
                    if retry.clicked() {
                        app.m.action(ARCHIVE_RETRY, Value::Null);
                    }
                }
            });
            if let Some(why) = blocked {
                widgets::hint(ui, t, &why);
            }
        },
    );
}

/// Compact recording + archive status for the Clipping header (call inside a right-to-left row).
pub fn header_status(app: &App, ui: &mut Ui, t: &Theme, status: Option<&Value>) {
    let (arc_status, arc_detail) = health(app, "health.archive");
    let queue = archive_queue(app);
    let running = queue.iter().find(|r| row_state(r) == "running");
    let label = if app.m.b("archive.paused") {
        "Archive paused".into()
    } else if let Some(r) = running {
        let p = r.get_path("progress").and_then(Value::as_f64).unwrap_or(0.0);
        format!("Archiving {:.0}%", fraction(p) * 100.0)
    } else {
        match arc_status.as_str() {
            "pass" => "Archive up to date".into(),
            "warn" if !queue.is_empty() => shows_to_archive(queue.len()),
            "warn" => "Archive needs a look".into(),
            "fail" => "Archive problem".into(),
            _ if !queue.is_empty() => shows_to_archive(queue.len()),
            _ => String::new(),
        }
    };
    if !label.is_empty() {
        let b = widgets::badge(ui, t, &label, if running.is_some() { t.accent } else { health_color(t, &arc_status) });
        if !arc_detail.is_empty() {
            b.on_hover_text(arc_detail);
        }
    }
    let Some(s) = status else { return };
    let active = s.get_path("active").is_some_and(Value::truthy);
    let mut parts = Vec::new();
    if active {
        let feeds = feeds_summary(s);
        if !feeds.is_empty() {
            parts.push(feeds);
        }
        let bytes = recorded_bytes(s);
        if bytes > 0 {
            parts.push(gb(bytes as f64 / 1e9));
        }
    }
    if let Some(free) = recording_free_gb(app, Some(s)) {
        parts.push(format!("{} free", gb(free)));
    }
    if !parts.is_empty() {
        ui.label(RichText::new(parts.join(" · ")).size(type_scale::SMALL + 0.5).color(t.text_dim));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(text: &str) -> Draft {
        Draft::from_project(&toml::from_str(text).unwrap())
    }

    /// Apply a `set` map the way project.write does (dotted keys into the document).
    fn apply(changes: &Value) -> toml::Value {
        let mut doc = toml::Table::new();
        let Value::Map(m) = changes else { panic!("set must be a map") };
        for (key, v) in m {
            let parts: Vec<&str> = key.split('.').collect();
            let mut t = &mut doc;
            for p in &parts[..parts.len() - 1] {
                t = t.entry(p.to_string()).or_insert_with(|| toml::Value::Table(Default::default())).as_table_mut().unwrap();
            }
            if let Some(v) = se_proto::value::to_toml(v) {
                t.insert(parts[parts.len() - 1].to_string(), v);
            }
        }
        toml::Value::Table(doc)
    }

    const CONTRACT: &str = r#"
[recording]
auto = true
modes = ["preshow", "live"]
dir = "/media/rec"
fallback_dir = ""
min_free_gb = 20
fps = 60
encoder = "auto"
video = [
  { name = "main", source = "canvas:wide", role = "master", codec = "hevc", cq = 19, max_mbps = 35 },
  { name = "kit",  source = "camera:cam_kit",  role = "iso", height = 1080, mbps = 8 },
  { name = "kick", source = "camera:cam_kick", role = "iso", height = 720,  mbps = 3.5 },
]
audio = [
  { name = "Mix",   source = "se-band",  format = "pulse" },
  { name = "Music", source = "se-music", format = "pulse" },
]
[recording.archive]
enabled = true
dir = ""
gb_per_hour = 1.8
audio_kbps = 128
iso_keep_days = 14
min_offline_minutes = 20
"#;

    #[test]
    fn contract_schema_round_trips_through_a_save() {
        let d = draft(CONTRACT);
        assert_eq!(d.video.iter().map(|f| (f.name.as_str(), f.role)).collect::<Vec<_>>(), [("main", Role::Master), ("kit", Role::Iso), ("kick", Role::Iso)]);
        assert_eq!((d.video[2].height, d.video[2].mbps), (Some(720), 3.5));
        assert_eq!((d.archive.gb_per_hour, d.archive.iso_keep_days), (1.8, 14));
        let saved = apply(&d.changes());
        assert_eq!(Draft::from_project(&saved), d);
        // written exactly per the schema: master quality keys, iso size keys, whole numbers stay integers
        let video = saved["recording"]["video"].as_array().unwrap();
        assert_eq!(video[0]["max_mbps"].as_integer(), Some(35));
        assert!(video[0].get("mbps").is_none() && video[0].get("format").is_none());
        assert_eq!(video[1]["role"].as_str(), Some("iso"));
        assert!(video[1].get("cq").is_none() && video[1].get("codec").is_none());
        assert_eq!(video[2]["mbps"].as_float(), Some(3.5));
        assert_eq!(saved["recording"]["min_free_gb"].as_integer(), Some(20));
    }

    #[test]
    fn old_projects_load_with_wide_as_master_and_keep_their_choices() {
        let d = draft(
            r#"[recording]
fps = 30
encoder = "x264"
video = [{ name = "wide", source = "canvas:wide", format = "" }, { name = "tall", source = "canvas:tall", format = "", gop = 2 }]
audio = [{ name = "Mix", source = "default", format = "pulse" }]
"#,
        );
        assert!(d.auto, "auto-record is on unless turned off");
        assert_eq!((d.fps, d.encoder.as_str()), (30, "software"));
        assert_eq!(d.video[0].role, Role::Master);
        assert_eq!(d.video[1].role, Role::Iso);
        assert_eq!(d.audio, vec![Track::new("Mix", "default", "pulse")]);
        assert!(d.errors(None).is_empty());
        let saved = apply(&d.changes());
        let video = saved["recording"]["video"].as_array().unwrap();
        assert_eq!(video[0]["role"].as_str(), Some("master"));
        assert_eq!(video[1]["role"].as_str(), Some("iso"));
        assert_eq!(video[1]["gop"].as_integer(), Some(2), "unknown keys survive a save");
        assert_eq!(Draft::from_project(&saved), d);
    }

    #[test]
    fn explicit_roles_win_and_cameras_are_never_implied_masters() {
        let d = draft(r#"[recording]
video = [{ name = "kit", source = "camera:cam_kit" }, { name = "tall", source = "canvas:tall", role = "iso" }, { name = "wide", source = "canvas:wide" }]"#);
        assert_eq!(d.video.iter().map(|f| f.role).collect::<Vec<_>>(), [Role::Iso, Role::Iso, Role::Master]);
        let d = draft(r#"[recording]
video = [{ name = "wide", source = "canvas:wide" }, { name = "x", source = "/dev/video9", format = "v4l2", role = "master" }]"#);
        assert_eq!(d.video.iter().map(|f| f.role).collect::<Vec<_>>(), [Role::Iso, Role::Master]);
    }

    #[test]
    fn missing_section_gives_the_new_defaults() {
        let d = draft("");
        assert!(d.auto);
        assert_eq!((d.fps, d.dir.as_str(), d.min_free_gb), (60, DEFAULT_DIR, 20.0));
        assert_eq!(d.video, vec![Feed::master("main", "canvas:wide")]);
        assert_eq!(d.audio.iter().map(|a| a.source.as_str()).collect::<Vec<_>>(), ["se-band", "se-music"]);
        assert_eq!(d.archive.gb_per_hour, 1.8);
        assert_eq!(Draft::from_project(&apply(&d.changes())), d);
    }

    #[test]
    fn archive_size_per_show_maps_to_gb_per_hour() {
        assert_eq!(gb_per_hour(5.4), 1.8);
        assert!((gb_per_show(1.8) - 5.4).abs() < 1e-9);
        assert_eq!(gb_per_hour(gb_per_show(1.8)), 1.8);
        assert_eq!(gb_per_hour(3.0), 1.0);
        assert_eq!(gb_per_hour(12.0), 4.0);
        // outside the offered range clamps to it
        assert_eq!(gb_per_hour(1.0), 1.0);
        assert_eq!(gb_per_hour(30.0), 4.0);
        // 1.8 GB/h ≈ 4 Mbps total; two 128 kbps tracks leave ≈ 3.74 Mbps for the picture
        assert!((archive_video_mbps(1.8, 2, 128) - 3.744).abs() < 1e-9);
        assert_eq!(archive_video_mbps(0.05, 4, 512), 0.0);
    }

    #[test]
    fn validation_requires_one_master_and_known_cameras() {
        let mut d = draft(CONTRACT);
        let cams = vec!["cam_kit".to_string(), "cam_kick".to_string()];
        assert!(d.errors(Some(&cams)).is_empty());
        // unknown source list: no camera complaints
        assert!(d.errors(None).is_empty());
        let only_kit = vec!["cam_kit".to_string()];
        let errors = d.errors(Some(&only_kit));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("cam_kick"));
        d.video[1].role = Role::Master;
        assert!(d.errors(Some(&cams)).iter().any(|e| e.contains("exactly one")));
        d.video.retain(|f| f.role != Role::Master);
        assert!(d.errors(Some(&cams)).iter().any(|e| e.contains("main recording")));
        let mut d = draft(CONTRACT);
        d.video[2].name = "kit".into();
        assert!(d.errors(Some(&cams)).iter().any(|e| e.contains("“kit”")));
    }

    #[test]
    fn camera_sizes_add_resize_and_remove_iso_feeds() {
        let mut d = draft("");
        d.set_camera("cam_kit", Some(1));
        d.set_camera("cam_kick", Some(0));
        let kit = d.camera_feed("cam_kit").unwrap();
        assert_eq!((kit.name.as_str(), kit.role, kit.height, kit.mbps), ("kit", Role::Iso, Some(1080), 8.0));
        assert_eq!(d.camera_feed("cam_kick").map(|f| (f.height, f.mbps)), Some((Some(720), 3.5)));
        d.set_camera("cam_kit", Some(0));
        assert_eq!(d.camera_feed("cam_kit").map(|f| (f.name.as_str(), f.height)), Some(("kit", Some(720))));
        d.set_camera("cam_kit", None);
        assert!(d.camera_feed("cam_kit").is_none());
        // a camera whose short name is taken gets a unique one
        d.video.push(Feed::iso("kit", "canvas:tall", None, 8.0));
        d.set_camera("cam_kit", Some(1));
        assert_eq!(d.camera_feed("cam_kit").unwrap().name, "kit-2");
        assert!(d.errors(None).is_empty());
    }

    #[test]
    fn unknown_show_state_counts_as_live() {
        let state = |pairs: Vec<(&'static str, Value)>| move |k: &str| pairs.iter().find(|(a, _)| *a == k).map(|(_, v)| v.clone());
        assert!(live_reason(true, state(vec![])).is_some());
        assert!(live_reason(false, state(vec![("show.mode", "offline".into())])).is_some());
        assert!(live_reason(true, state(vec![("show.mode", "offline".into())])).is_none());
        assert!(live_reason(true, state(vec![("show.mode", "brb".into())])).is_some());
        assert!(live_reason(true, state(vec![("show.mode", "offline".into()), ("obs.stream.active", true.into())])).is_some());
        assert!(live_reason(true, state(vec![("show.mode", "offline".into()), ("twitch.stream.live", true.into())])).is_some());
        assert!(live_reason(true, state(vec![("show.mode", "offline".into()), ("recording.active", true.into())])).is_some());
    }

    #[test]
    fn mount_lookup_uses_whole_path_components_and_the_device_not_the_subvolume() {
        let info = "\
22 1 0:21 /@ / rw,relatime shared:1 - btrfs /dev/mapper/root rw\n\
23 22 0:22 /@home /home rw,relatime shared:2 - btrfs /dev/mapper/root rw\n\
40 22 8:17 / /mnt/media\\040drive rw,relatime shared:3 - ext4 /dev/sdb1 rw\n\
41 22 8:33 / /home2 rw,relatime shared:4 - ext4 /dev/sdc1 rw\n";
        let root = mount_source_in(info, Path::new("/"));
        assert_eq!(root.as_deref(), Some("/dev/mapper/root"));
        assert_eq!(mount_source_in(info, Path::new("/home/me/Videos")), root);
        assert_eq!(mount_source_in(info, Path::new("/mnt/media drive/rec")).as_deref(), Some("/dev/sdb1"));
        assert_eq!(mount_source_in(info, Path::new("/home2/rec")).as_deref(), Some("/dev/sdc1"));
        assert_eq!(mount_source_in(info, Path::new("/home22")), root);
    }

    #[test]
    fn folders_expand_home_and_default_inside_the_recording_folder() {
        let home = Path::new("/home/me");
        let mut d = draft("");
        assert_eq!(expand(&d.dir, home), Path::new("/home/me/Videos/Stream Engine"));
        assert_eq!(archive_path(&d, home), Path::new("/home/me/Videos/Stream Engine/Archive"));
        assert_eq!(fallback_path(&d, home), Path::new("/home/me/Videos/Stream Engine (fallback)"));
        d.archive.dir = "/mnt/archive".into();
        assert_eq!(archive_path(&d, home), Path::new("/mnt/archive"));
    }
}
