//! Session files the clip job reads (§3.3): `markers.json` (hype + manual markers, master
//! clock) and `meta.toml` (OBS recordings with their master-clock start, track layout, clock
//! mappings — written by the OBS adapter through `session.meta`).

use crate::config::ClipsConfig;
use crate::show;
use se_proto::{Ts, Value};
use se_store::LogRec;
use std::path::{Path, PathBuf};

const S: f64 = 1e9;
/// A requested song on the master clock, from the indexed show or the session journal.
#[derive(Clone, Debug, PartialEq)]
pub struct Song {
    pub start: u64,
    pub end: u64,
    pub title: String,
    pub user: String,
    pub video: String,
    pub channel: String,
    pub dmca: bool,
}

impl Song {
    pub fn context(&self) -> serde_json::Value {
        serde_json::json!({"title": self.title, "user": self.user, "video": self.video, "channel": self.channel, "dmca": self.dmca})
    }
}

/// Prefer the durable show index, but jobs queued before indexing still see song events.
pub fn songs(dir: &Path, show_paths: &show::ShowPaths, last_ns: u64) -> Vec<Song> {
    if let Some(manifest) = show::read_manifest(show_paths) {
        let items = show::read_lane(&show_paths.lane("songs"));
        if items.iter().any(|v| v.get("t0").is_some()) {
            return items
                .iter()
                .filter_map(|v| {
                    let start = v.get("t0")?.as_f64()?;
                    let end = v.get("t1")?.as_f64()?;
                    if !start.is_finite() || !end.is_finite() || start < 0.0 || end <= start {
                        return None;
                    }
                    let ns = |t: f64| (manifest.t0_ns as f64 + t * S).max(0.0) as u64;
                    Some(Song {
                        start: ns(start),
                        end: ns(end),
                        title: v.get("title").or_else(|| v.get("label")).and_then(|x| x.as_str()).unwrap_or("").into(),
                        user: v.get("user").and_then(|x| x.as_str()).unwrap_or("").into(),
                        video: v.get("video").and_then(|x| x.as_str()).unwrap_or("").into(),
                        channel: v.get("channel").and_then(|x| x.as_str()).unwrap_or("").into(),
                        dmca: v.get("dmca").and_then(|x| x.as_bool()).unwrap_or(true),
                    })
                })
                .collect();
        }
    }
    let mut songs: Vec<Song> = Vec::new();
    if let Ok(log) = se_store::read_log(dir) {
        for entry in log {
            let LogRec::Ev { event } = entry else { continue };
            match event.ty.as_str() {
                "queue.song_started" => {
                    let get = |field| event.payload.get_path(field).and_then(Value::as_str).unwrap_or("").to_string();
                    songs.push(Song {
                        start: event.ts,
                        end: last_ns,
                        title: get("title"),
                        user: get("user"),
                        video: get("video"),
                        channel: get("channel"),
                        dmca: true,
                    });
                }
                "queue.song_ended" => {
                    let id = event.payload.get_path("id").and_then(Value::as_i64);
                    let video = event.payload.get_path("video").and_then(Value::as_str);
                    if let Some(song) = songs
                        .iter_mut()
                        .rev()
                        .find(|s| s.end == last_ns && s.start <= event.ts && (video == Some(s.video.as_str()) || id.is_none() && video.is_none()))
                    {
                        song.end = event.ts;
                    }
                }
                _ => {}
            }
        }
    }
    songs.retain(|s| s.end > s.start);
    songs
}

pub fn song_candidates(songs: &[Song], cfg: &ClipsConfig) -> Vec<Candidate> {
    let target = cfg.max_len.0.saturating_mul(1_000_000).min(45_000_000_000);
    songs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let peak = s.start + (s.end - s.start) / 2;
            let half = target / 2;
            Candidate {
                key: format!("song-{}-{i}", s.start / 1_000_000),
                start: peak.saturating_sub(half).max(s.start),
                peak,
                end: peak.saturating_add(half).min(s.end),
                score: cfg.manual_score,
                reasons: vec!["song request".into()],
                labels: vec![s.title.clone()],
                markers: Vec::new(),
            }
        })
        .collect()
}

/// A clip candidate window in master-clock ns.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// Stable identity within the session (derived from the peak).
    pub key: String,
    pub start: Ts,
    pub peak: Ts,
    pub end: Ts,
    pub score: f64,
    pub reasons: Vec<String>,
    pub labels: Vec<String>,
    /// Indices into `markers.json` that fed this candidate.
    pub markers: Vec<usize>,
}

/// One parsed marker (for the review timeline).
#[derive(Clone, Debug, PartialEq)]
pub struct Marker {
    pub index: usize,
    pub ts: Ts,
    pub wall_ms: i64,
    pub label: String,
    pub origin: String,
    pub hype: bool,
    pub start: Ts,
    pub peak: Ts,
    pub end: Ts,
    pub score: f64,
    pub reasons: Vec<String>,
}

impl Marker {
    /// Master ns → wall-clock ms, anchored at the marker's own `(ts, wall_ms)` pair.
    pub fn wall_of(&self, t: Ts) -> i64 {
        self.wall_ms + (t as i64 - self.ts as i64) / 1_000_000
    }

    pub fn to_value(&self) -> Value {
        Value::map()
            .with("index", self.index as i64)
            .with("label", self.label.clone())
            .with("origin", self.origin.clone())
            .with("kind", if self.hype { "hype" } else { "manual" })
            .with("score", self.score)
            .with("reasons", self.reasons.clone())
            .with("wall_ms", self.wall_ms)
            .with("start_wall_ms", self.wall_of(self.start))
            .with("peak_wall_ms", self.wall_of(self.peak))
            .with("end_wall_ms", self.wall_of(self.end))
            .with("start_ns", self.start as i64)
            .with("peak_ns", self.peak as i64)
            .with("end_ns", self.end as i64)
    }
}

fn ns(v: Option<&Value>) -> Option<Ts> {
    v.and_then(Value::as_f64).filter(|x| *x >= 0.0).map(|x| x as Ts)
}

/// Parse `markers.json` entries written by `session.marker`.
pub fn parse_markers(list: &[Value], cfg: &ClipsConfig) -> Vec<Marker> {
    let ms = 1_000_000u64;
    let mut out = Vec::new();
    for (index, m) in list.iter().enumerate() {
        let Some(ts) = ns(m.get_path("ts")) else { continue };
        let args = m.get_path("args").cloned().unwrap_or_default();
        let label = m.get_path("label").map(|v| v.to_string()).unwrap_or_else(|| "marker".into());
        let origin = m.get_path("origin").and_then(Value::as_str).unwrap_or("").to_string();
        let wall_ms = m.get_path("wall_ms").and_then(Value::as_i64).unwrap_or(0);
        let hype = args.get_path("kind").and_then(Value::as_str) == Some("hype");
        // absolute master ns, or seconds before the marker (script patches don't see the clock)
        let at = |abs: &str, ago: &str| {
            ns(args.get_path(abs)).or_else(|| args.get_path(ago).and_then(Value::as_f64).filter(|a| *a >= 0.0).map(|a| ts.saturating_sub((a * 1e9) as Ts)))
        };
        let (start, peak, end, score, reasons) = match (at("start", "start_ago"), at("peak", "peak_ago"), at("end", "end_ago")) {
            (Some(s), Some(p), Some(e)) if s <= p && p <= e => {
                let score = args.get_path("score").and_then(Value::as_f64).unwrap_or(cfg.manual_score);
                let reasons = args
                    .get_path("reasons")
                    .and_then(Value::as_list)
                    .map(|l| l.iter().filter_map(|r| r.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                (s, p, e, score, reasons)
            }
            // "clip that": the moment happened just before the press
            _ => {
                let peak = ts.saturating_sub(4_000 * ms);
                (ts.saturating_sub(cfg.manual_preroll.0 * ms), peak, ts + cfg.manual_postroll.0 * ms, cfg.manual_score, vec!["manual".to_string()])
            }
        };
        out.push(Marker { index, ts, wall_ms, label, origin, hype, start, peak, end, score, reasons });
    }
    out
}

/// Merge overlapping marker windows into candidates (combined evidence scores higher).
pub fn candidates(markers: &[Marker], cfg: &ClipsConfig) -> Vec<Candidate> {
    let max_len = cfg.max_len.0 * 1_000_000;
    let mut sorted: Vec<&Marker> = markers.iter().collect();
    sorted.sort_by_key(|m| (m.start, m.index));
    let mut out: Vec<Candidate> = Vec::new();
    for m in sorted {
        let label = if m.hype || matches!(m.label.as_str(), "marker" | "clip" | "") { None } else { Some(m.label.clone()) };
        if let Some(c) = out.last_mut()
            && m.start < c.end
        {
            let (hi, lo) = if m.score > c.score { (m.score, c.score) } else { (c.score, m.score) };
            if m.score > c.score {
                c.peak = m.peak;
            }
            c.score = hi + 0.25 * lo;
            c.start = c.start.min(m.start);
            c.end = c.end.max(m.end);
            for r in &m.reasons {
                if !c.reasons.contains(r) {
                    c.reasons.push(r.clone());
                }
            }
            c.labels.extend(label);
            c.markers.push(m.index);
            continue;
        }
        out.push(Candidate {
            key: String::new(),
            start: m.start,
            peak: m.peak,
            end: m.end,
            score: m.score,
            reasons: m.reasons.clone(),
            labels: label.into_iter().collect(),
            markers: vec![m.index],
        });
    }
    for c in &mut out {
        if c.end - c.start > max_len {
            // keep the part around the peak (up to 2/3 before it), as long as allowed
            let before = (max_len * 2 / 3).min(c.peak - c.start);
            c.start = c.start.max((c.peak - before).min(c.end - max_len));
            c.end = (c.start + max_len).min(c.end);
        }
        c.key = format!("p{}", c.peak / 1_000_000);
    }
    out
}

/// One audio stream of a recording (OBS track layout from `session.meta recordings`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackInfo {
    pub index: usize,
    pub mixer: Option<i64>,
    pub name: String,
    pub sources: Vec<String>,
    pub devices: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Recording {
    pub canvas: String,
    pub path: PathBuf,
    /// Master-clock ns of the file's t = 0.
    pub start_ns: Option<Ts>,
    pub end_ns: Option<Ts>,
    pub tracks: Vec<TrackInfo>,
}

impl Recording {
    /// Seconds into this file for a master time.
    pub fn offset_s(&self, t: Ts) -> Option<f64> {
        self.start_ns.map(|s| (t as f64 - s as f64) / S)
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionMeta {
    pub recordings: Vec<Recording>,
    pub clock: Option<se_clock::Mappings>,
}

fn strings(v: Option<&toml::Value>) -> Vec<String> {
    v.and_then(toml::Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// Parse `meta.toml`. Recordings without a master start fall back to the `clock.obs_record`
/// mapping (which maps master time to ns into the current recording).
pub fn parse_meta(text: &str) -> Result<SessionMeta, String> {
    let doc: toml::Table = toml::from_str(text).map_err(|e| format!("meta.toml: {}", e.message()))?;
    let clock = match doc.get("clock") {
        Some(toml::Value::String(s)) => serde_json::from_str::<se_clock::Mappings>(s).ok(),
        Some(v) => serde_json::to_value(v).ok().and_then(|j| serde_json::from_value::<se_clock::Mappings>(j).ok()),
        None => None,
    };
    let mut recordings = Vec::new();
    for r in doc.get("recordings").and_then(toml::Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
        let Some(path) = r.get("path").and_then(toml::Value::as_str) else { continue };
        let tracks = r
            .get("tracks")
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(i, t)| TrackInfo {
                        index: t.get("index").and_then(toml::Value::as_integer).map(|x| x.max(0) as usize).unwrap_or(i),
                        mixer: t.get("mixer").and_then(toml::Value::as_integer),
                        name: t.get("name").and_then(toml::Value::as_str).unwrap_or("").to_string(),
                        sources: strings(t.get("sources")),
                        devices: strings(t.get("devices")),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let int = |k: &str| r.get(k).and_then(toml::Value::as_integer).filter(|x| *x >= 0).map(|x| x as Ts);
        recordings.push(Recording {
            canvas: r.get("canvas").and_then(toml::Value::as_str).unwrap_or("wide").to_string(),
            path: PathBuf::from(path),
            start_ns: int("start_ns"),
            end_ns: int("end_ns"),
            tracks,
        });
    }
    if let Some(c) = &clock
        && let Some(start) = c.obs_record.to_master(0)
    {
        for r in recordings.iter_mut().filter(|r| r.start_ns.is_none()) {
            r.start_ns = Some(start);
        }
    }
    Ok(SessionMeta { recordings, clock })
}

/// Everything the job needs from `sessions/<id>/`.
pub struct SessionFiles {
    pub dir: PathBuf,
    pub markers: Vec<Marker>,
    pub meta: SessionMeta,
}

pub fn load(dir: &Path, cfg: &ClipsConfig) -> Result<SessionFiles, String> {
    let list: Vec<Value> = match std::fs::read_to_string(dir.join("markers.json")) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| format!("markers.json: {e}"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("markers.json: {e}")),
    };
    let meta = match std::fs::read_to_string(dir.join("meta.toml")) {
        Ok(s) => parse_meta(&s)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SessionMeta::default(),
        Err(e) => return Err(format!("meta.toml: {e}")),
    };
    Ok(SessionFiles { dir: dir.to_path_buf(), markers: parse_markers(&list, cfg), meta })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: i64 = 1_000_000_000;

    fn hype(ts: i64, start: i64, peak: i64, end: i64, score: f64, reasons: &[&str]) -> Value {
        Value::map().with("ts", ts).with("wall_ms", 1_700_000_000_000i64 + ts / 1_000_000).with("label", "hype").with("origin", "patch").with(
            "args",
            Value::map()
                .with("kind", "hype")
                .with("start", start)
                .with("peak", peak)
                .with("end", end)
                .with("score", score)
                .with("reasons", reasons.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
        )
    }

    #[test]
    fn manual_and_hype_markers_merge_when_they_overlap() {
        let cfg = ClipsConfig::default();
        let list = vec![
            hype(120 * SEC, 80 * SEC, 100 * SEC, 110 * SEC, 1.5, &["chat", "emotes"]),
            // "clip that" pressed at 112 s: window 82–120 s overlaps the hype window
            Value::map().with("ts", 112 * SEC).with("wall_ms", 5).with("label", "clip").with("origin", "deck"),
            hype(400 * SEC, 370 * SEC, 390 * SEC, 395 * SEC, 1.1, &["bits"]),
            Value::map().with("ts", 900 * SEC).with("label", "drum solo").with("origin", "cli"),
        ];
        let markers = parse_markers(&list, &cfg);
        assert_eq!(markers.len(), 4);
        assert!(!markers[1].hype);
        assert_eq!(markers[1].start as i64, 82 * SEC);
        let c = candidates(&markers, &cfg);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].start as i64, 80 * SEC);
        assert_eq!(c[0].end as i64, 120 * SEC);
        // manual scores 3.0 → peak follows the manual marker; evidence adds up
        assert_eq!(c[0].peak as i64, 108 * SEC);
        assert!((c[0].score - (3.0 + 0.25 * 1.5)).abs() < 1e-9);
        assert_eq!(c[0].reasons, vec!["chat", "emotes", "manual"]);
        assert_eq!(c[0].markers, vec![0, 1]);
        assert_eq!(c[2].labels, vec!["drum solo"]);
        assert_eq!(c[1].key, format!("p{}", 390_000));
    }

    #[test]
    fn script_markers_give_their_window_relative_to_the_marker() {
        let cfg = ClipsConfig::default();
        let list = vec![
            Value::map().with("ts", 100 * SEC).with("label", "hype").with(
                "args",
                Value::map()
                    .with("kind", "hype")
                    .with("start_ago", 25.0)
                    .with("peak_ago", 10.5)
                    .with("end_ago", 2)
                    .with("score", 1.4)
                    .with("reasons", vec!["chat"]),
            ),
        ];
        let m = &parse_markers(&list, &cfg)[0];
        assert!(m.hype);
        assert_eq!((m.start as i64, m.peak as i64, m.end as i64), (75 * SEC, 89 * SEC + SEC / 2, 98 * SEC));
        assert_eq!(m.reasons, vec!["chat"]);
    }

    #[test]
    fn overlong_merged_windows_keep_the_peak() {
        let cfg = ClipsConfig::default();
        let list = vec![hype(60 * SEC, 30 * SEC, 50 * SEC, 70 * SEC, 1.0, &["chat"]), hype(120 * SEC, 65 * SEC, 95 * SEC, 110 * SEC, 2.0, &["raid"])];
        let c = candidates(&parse_markers(&list, &cfg), &cfg);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].peak as i64, 95 * SEC);
        assert_eq!((c[0].end - c[0].start) as i64, 60 * SEC);
        assert!(c[0].start as i64 <= 95 * SEC - 30 * SEC);
    }

    #[test]
    fn meta_recordings_tracks_and_clock_fallback() {
        let text = r#"
recordings = [
  { canvas = "wide", path = "/v/a.mkv", start_ns = 5000000000, end_ns = 9000000000, tracks = [
      { index = 0, mixer = 1, name = "Mic", sources = ["Mic/Aux"], devices = ["se-mic"] },
      { index = 1, mixer = 2, name = "Track 2", sources = ["se-music"], devices = ["se-music"] } ] },
  { canvas = "tall", path = "/v/b.mkv" },
]
clock = { obs_record = { master_ref = 7000000000, other_ref = 0, rate = 1.0, valid = true }, twitch_delay_ms = 2500,
          wall = { master_ref = 0, other_ref = 0, rate = 1.0, valid = false }, obs_stream = { master_ref = 0, other_ref = 0, rate = 1.0, valid = false },
          audio = { master_ref = 0, other_ref = 0, rate = 1.0, valid = false } }
"#;
        let m = parse_meta(text).unwrap();
        assert_eq!(m.recordings.len(), 2);
        assert_eq!(m.recordings[0].tracks[1].devices, vec!["se-music"]);
        assert_eq!(m.recordings[0].offset_s(6_500_000_000), Some(1.5));
        assert_eq!(m.recordings[1].start_ns, Some(7_000_000_000), "clock fallback");
        assert_eq!(m.clock.unwrap().twitch_delay_ms, 2500);
        assert!(parse_meta("recordings = 3").unwrap().recordings.is_empty());
        assert!(parse_meta("= broken").is_err());
    }
    #[test]
    fn indexed_song_spans_become_candidates_even_without_markers() {
        let dir = tempfile::tempdir().unwrap();
        let paths = show::ShowPaths::new(dir.path());
        std::fs::create_dir_all(paths.lanes()).unwrap();
        let manifest = show::Manifest { t0_ns: 1_000_000_000, ..Default::default() };
        std::fs::write(paths.manifest(), serde_json::to_string(&manifest).unwrap()).unwrap();
        std::fs::write(
            paths.lane("songs"),
            concat!(
                r#"{"t0":10,"t1":130,"title":"One","user":"listener","video":"id1","channel":"artist","dmca":true}"#,
                "\n",
                r#"{"t0":150,"t1":210,"title":"Two","user":"dj","video":"id2","dmca":false}"#,
                "\n",
            ),
        )
        .unwrap();
        let songs = songs(dir.path(), &paths, 500 * SEC as u64);
        assert_eq!(songs.len(), 2);
        assert_eq!((songs[0].start, songs[0].end), (11 * SEC as u64, 131 * SEC as u64));
        assert_eq!(songs[0].context()["user"], "listener");
        assert!(!songs[1].dmca);
        let candidates = song_candidates(&songs, &ClipsConfig::default());
        assert_eq!(candidates.len(), 2);
        assert_ne!(candidates[0].key, candidates[1].key);
        assert_eq!(candidates[1].labels, vec!["Two"]);
    }
}
