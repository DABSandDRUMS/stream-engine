//! Post-stream clip job: marker and requested-song candidates → deterministic ranking
//! (optionally assisted by a rank_command) → wide/tall cuts → review queue.
//! Musical passages retain their song audio and don't use Whisper lyrics as captions.
//! Everything here is blocking; the engine runs it on a niced worker thread.

use crate::captions::{self, Layout};
use crate::config::{ClipsConfig, Encoder, TallSource};
use crate::ffmpeg::{self, Codec, Cut, Frame, Probe};
use crate::select::{self, RankIn, Trim};
use crate::session::{self, Candidate, Recording, SessionFiles};
use crate::show;
use crate::store::{self, ClipRow};
use crate::tracks::{self, AudioPlan};
use crate::transcribe::{self, Transcriber, Word};
use se_proto::Value;
use se_store::Db;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct JobEnv {
    pub db: Db,
    pub project_root: PathBuf,
    pub data_dir: PathBuf,
    pub cfg: ClipsConfig,
}

/// A progress report (→ `clips.progress` events and `clips.job.*` state).
#[derive(Clone, Debug)]
pub struct Progress {
    pub session: String,
    pub stage: String,
    pub done: usize,
    pub total: usize,
    pub detail: String,
}

impl Progress {
    pub fn fraction(&self) -> f64 {
        let base = match self.stage.as_str() {
            "load" => 0.0,
            "model" => 0.05,
            "transcribe" => 0.1,
            "rank" => 0.4,
            "render" => 0.45,
            _ => 1.0,
        };
        let span = match self.stage.as_str() {
            "transcribe" => 0.3,
            "render" => 0.55,
            _ => 0.0,
        };
        let f = if self.total > 0 { self.done as f64 / self.total as f64 } else { 0.0 };
        (base + span * f).min(1.0)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub clips: usize,
    pub failed: usize,
    pub skipped: Vec<String>,
    pub timings: Vec<(String, f64)>,
}

impl Report {
    pub fn timings_value(&self) -> Value {
        self.timings.iter().fold(Value::map(), |m, (k, v)| m.with(k.clone(), (v * 100.0).round() / 100.0))
    }
}

/// `sessions/<id>/` from the sessions index, else under the project.
pub fn session_dir(env: &JobEnv, session: &str) -> PathBuf {
    let from_db: Option<String> = env
        .db
        .with(|c| {
            use rusqlite::OptionalExtension;
            c.query_row("SELECT dir FROM sessions WHERE id = ?1", [session], |r| r.get(0)).optional()
        })
        .ok()
        .flatten();
    from_db.map(PathBuf::from).unwrap_or_else(|| env.project_root.join("sessions").join(session))
}
fn output_dir(dir: &Path, cfg: &ClipsConfig) -> PathBuf {
    if show::show_dir_from_meta(dir).is_some() { show::paths_for(dir).clips() } else { dir.join(&cfg.output_dir) }
}

/// Keep the session directory away from retention while clips still need it.
pub fn update_keep(env: &JobEnv, session: &str) {
    let keep = session_dir(env, session).join(".keep");
    match store::session_needs_files(&env.db, session) {
        Ok(true) => {
            if !keep.exists()
                && let Some(d) = keep.parent()
                && d.exists()
            {
                let _ = std::fs::write(&keep, "clips pending review (se-clips)\n");
            }
        }
        Ok(false) => {
            let _ = std::fs::remove_file(&keep);
        }
        Err(e) => tracing::warn!("clips keep {session}: {e:#}"),
    }
}

/// A recording with its probe (cached per job).
struct Rec {
    rec: Recording,
    probe: Probe,
    start_ns: u64,
}

impl Rec {
    fn covers(&self, t: u64) -> bool {
        let off = (t as f64 - self.start_ns as f64) / 1e9;
        off >= 0.0 && off <= self.probe.duration
    }
    fn at(&self, t: u64) -> f64 {
        (t as f64 - self.start_ns as f64) / 1e9
    }
}

/// Wait (up to `max`) until files OBS may still be finalizing stop changing: a file counts as
/// settled once its size is unchanged for 3 s and it wasn't modified in the last 3 s.
pub fn wait_settled(paths: &[PathBuf], max: std::time::Duration) -> bool {
    let deadline = Instant::now() + max;
    let recent = std::time::Duration::from_secs(3);
    let state = |p: &Path| std::fs::metadata(p).ok().map(|m| (m.len(), m.modified().ok().and_then(|t| t.elapsed().ok()).unwrap_or(recent)));
    let mut sizes: Vec<Option<(u64, std::time::Duration)>> = paths.iter().map(|p| state(p)).collect();
    let mut stable_since = Instant::now();
    loop {
        let settled = sizes.iter().all(|s| s.is_none_or(|(_, age)| age >= recent)) && stable_since.elapsed() >= recent;
        if settled {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        let now: Vec<_> = paths.iter().map(|p| state(p)).collect();
        if now.iter().zip(&sizes).any(|(a, b)| a.map(|x| x.0) != b.map(|x| x.0)) {
            stable_since = Instant::now();
        }
        sizes = now;
    }
}

fn load_recordings(files: &SessionFiles) -> (Vec<Rec>, Vec<String>) {
    let mut out = Vec::new();
    let mut problems = Vec::new();
    let paths: Vec<PathBuf> = files.meta.recordings.iter().map(|r| r.path.clone()).collect();
    if !wait_settled(&paths, std::time::Duration::from_secs(120)) {
        problems.push("recordings were still being written after 2 min; cutting what is on disk".into());
    }
    for r in &files.meta.recordings {
        let Some(start_ns) = r.start_ns else {
            problems.push(format!("{}: no master-clock start (OBS mapping missing)", r.path.display()));
            continue;
        };
        match ffmpeg::probe(&r.path) {
            Ok(probe) => out.push(Rec { rec: r.clone(), probe, start_ns }),
            Err(e) => problems.push(e),
        }
    }
    (out, problems)
}

/// Wide (canonical, multitrack) recording for a moment: the master recording covering `t`.
/// ISOs (cameras, extra canvases) are video-only and never chosen. Older shows without roles
/// prefer the `wide` canvas, then any recording that is not `tall`.
fn wide_for(recs: &[Rec], t: u64) -> Option<usize> {
    let rank = |r: &Rec| match (r.rec.role.as_str(), r.rec.canvas.as_str()) {
        ("master", _) | ("", "wide") => 0,
        _ => 1,
    };
    recs.iter().enumerate().filter(|(_, r)| r.rec.is_master() && r.covers(t)).min_by_key(|(_, r)| rank(r)).map(|(i, _)| i)
}

fn tall_for(recs: &[Rec], t0: u64, t1: u64) -> Option<&Rec> {
    recs.iter().find(|r| r.rec.is_tall() && r.covers(t0) && r.covers(t1))
}

/// Everything needed to render one clip.
struct Plan {
    key: String,
    cand: Candidate,
    wide: usize,
    audio: AudioPlan,
    words: Vec<Word>,
    transcript: (f64, f64),
    trim: Trim,
    score: f64,
    title: Option<String>,
    song: Option<session::Song>,
}

/// One stretch of one audio stream to transcribe, shared by the plans inside it.
#[derive(Debug, PartialEq)]
struct Span {
    rec: usize,
    stream: usize,
    from: f64,
    to: f64,
    plans: Vec<usize>,
}

fn merge_spans(plans: &[Plan]) -> Vec<Span> {
    let mut order: Vec<usize> = (0..plans.len()).filter(|k| plans[*k].audio.transcribe.is_some() && plans[*k].words.is_empty()).collect();
    order.sort_by(|a, b| {
        (plans[*a].wide, plans[*a].audio.transcribe, plans[*a].transcript.0)
            .partial_cmp(&(plans[*b].wide, plans[*b].audio.transcribe, plans[*b].transcript.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut spans: Vec<Span> = Vec::new();
    for k in order {
        let p = &plans[k];
        let stream = p.audio.transcribe.unwrap_or(0);
        match spans.last_mut() {
            Some(s) if s.rec == p.wide && s.stream == stream && p.transcript.0 <= s.to => {
                s.to = s.to.max(p.transcript.1);
                s.plans.push(k);
            }
            _ => spans.push(Span { rec: p.wide, stream, from: p.transcript.0, to: p.transcript.1, plans: vec![k] }),
        }
    }
    spans
}

fn transcribe_window(tx: &Transcriber, rec: &Rec, stream: usize, from: f64, to: f64, nice: i32) -> Result<Vec<Word>, String> {
    let pcm = transcribe::extract_pcm(&rec.rec.path, stream, from, to, nice)?;
    tx.words(&pcm, from)
}

/// Select the strongest marker, an existing drop event, an indexed hype peak, or
/// the song's middle. No new musical-performance analyzer is needed.
fn song_peak(song: &session::Song, markers: &[session::Marker], drops: &[u64], features: &[(u64, f64)]) -> u64 {
    markers
        .iter()
        .filter(|m| m.peak >= song.start && m.peak < song.end)
        .max_by(|a, b| a.score.total_cmp(&b.score))
        .map(|m| m.peak)
        .or_else(|| drops.iter().copied().find(|t| *t >= song.start && *t < song.end))
        .or_else(|| features.iter().filter(|(t, h)| *t >= song.start && *t < song.end && *h > 0.0).max_by(|a, b| a.1.total_cmp(&b.1)).map(|(t, _)| *t))
        .unwrap_or(song.start + (song.end - song.start) / 2)
}

fn indexed_features(paths: &show::ShowPaths) -> Vec<(u64, f64)> {
    let Some(manifest) = show::read_manifest(paths) else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(paths.features()) else { return Vec::new() };
    let mut lines = text.lines();
    let columns: Vec<_> = lines.next().unwrap_or("").split(',').collect();
    let Some(hi) = columns.iter().position(|c| matches!(c.trim(), "hype.score" | "hype")) else { return Vec::new() };
    let Some(ti) = columns.iter().position(|c| c.trim() == "t") else { return Vec::new() };
    lines
        .filter_map(|l| {
            let cols: Vec<_> = l.split(',').collect();
            let t: f64 = cols.get(ti)?.trim().parse().ok()?;
            let hype: f64 = cols.get(hi)?.trim().parse().ok()?;
            (t >= 0.0 && t.is_finite() && hype.is_finite()).then_some(((manifest.t0_ns as f64 + t * 1e9).max(0.0) as u64, hype))
        })
        .collect()
}

fn indexed_drops(paths: &show::ShowPaths) -> Vec<u64> {
    let Some(manifest) = show::read_manifest(paths) else { return Vec::new() };
    show::read_lane(&paths.lane("moments"))
        .iter()
        .filter(|v| matches!(v.get("label").and_then(serde_json::Value::as_str), Some("music.drop" | "band.drop")))
        .filter_map(|v| v.get("t").and_then(serde_json::Value::as_f64))
        .filter(|t| *t >= 0.0 && t.is_finite())
        .map(|t| (manifest.t0_ns as f64 + t * 1e9).max(0.0) as u64)
        .collect()
}

fn indexed_beats(paths: &show::ShowPaths) -> Vec<u64> {
    let Some(manifest) = show::read_manifest(paths) else { return Vec::new() };
    // Prefer downbeats; individual beats are useful when there is no downbeat lane.
    let lane = ["downbeats", "beats"].iter().map(|name| show::read_lane(&paths.lane(name))).find(|items| !items.is_empty()).unwrap_or_default();
    lane.iter()
        .filter_map(|v| v.get("t").and_then(serde_json::Value::as_f64))
        .filter(|t| *t >= 0.0 && t.is_finite())
        .map(|t| (manifest.t0_ns as f64 + t * 1e9).max(0.0) as u64)
        .collect()
}

fn overlapping_song(songs: &[session::Song], from: u64, to: u64) -> Option<&session::Song> {
    songs.iter().filter(|s| s.start < to && s.end > from).max_by_key(|s| s.end.min(to) - s.start.max(from))
}

/// One speech-rich candidate per five-minute bucket, capped across long sessions.
/// Manual/hype markers remain separate candidates and retain their scores.
fn indexed_talk_candidates(manifest: &show::Manifest, segments: &[serde_json::Value], songs: &[session::Song], cfg: &ClipsConfig) -> Vec<Candidate> {
    let mut buckets: std::collections::BTreeMap<u64, (u64, usize)> = std::collections::BTreeMap::new();
    for seg in segments {
        let (Some(t0), Some(t1)) = (seg.get("t0").and_then(serde_json::Value::as_f64), seg.get("t1").and_then(serde_json::Value::as_f64)) else { continue };
        if !t0.is_finite() || !t1.is_finite() || t0 < 0.0 || t1 <= t0 {
            continue;
        }
        let t = (manifest.t0_ns as f64 + (t0 + t1) * 0.5 * 1e9).max(0.0) as u64;
        if songs.iter().any(|s| s.start <= t && t < s.end) {
            continue;
        }
        let count = seg.get("text").and_then(serde_json::Value::as_str).unwrap_or("").split_whitespace().count();
        if count == 0 {
            continue;
        }
        let bucket = (t0 / 300.0) as u64;
        let group = buckets.entry(bucket).or_insert((t, 0));
        if count > group.1 {
            *group = (t, count);
        }
    }
    let candidates: Vec<_> = buckets.into_iter().filter(|(_, (_, words))| *words >= 8).collect();
    let stride = candidates.len().div_ceil(96).max(1);
    let half = (cfg.max_len.0 * 1_000_000).min(45_000_000_000) / 2;
    candidates
        .into_iter()
        .step_by(stride)
        .map(|(bucket, (peak, count))| Candidate {
            key: format!("talk-{bucket}"),
            start: peak.saturating_sub(half),
            peak,
            end: peak.saturating_add(half),
            score: cfg.manual_score * 0.5 + (count.min(40) as f64) * 0.02,
            reasons: vec!["spoken passage".into()],
            labels: Vec::new(),
            markers: Vec::new(),
        })
        .collect()
}
/// Reuse the indexed show transcript's word-level times for clip-local ranking.
fn indexed_words(manifest: &show::Manifest, segments: &[serde_json::Value], recs: &[Rec]) -> Vec<Vec<Word>> {
    let mut per_rec = vec![Vec::new(); recs.len()];
    for segment in segments {
        let Some(words) = segment.get("words").and_then(serde_json::Value::as_array) else { continue };
        for word in words {
            let (Some(t0), Some(t1), Some(text)) = (
                word.get("t0").and_then(serde_json::Value::as_f64),
                word.get("t1").and_then(serde_json::Value::as_f64),
                word.get("w").and_then(serde_json::Value::as_str),
            ) else {
                continue;
            };
            if !t0.is_finite() || !t1.is_finite() || t0 < 0.0 || t1 <= t0 {
                continue;
            }
            let master_ns = manifest.t0_ns as f64 + t0 * 1e9;
            for (rec, dest) in recs.iter().zip(&mut per_rec) {
                let from = (master_ns - rec.start_ns as f64) / 1e9;
                let to = from + t1 - t0;
                if from >= 0.0 && to <= rec.probe.duration {
                    dest.push(Word {
                        t0: from,
                        t1: to,
                        text: text.into(),
                        p: word.get("p").and_then(serde_json::Value::as_f64).unwrap_or(0.0) as f32,
                        annotation: word.get("annotation").and_then(serde_json::Value::as_bool).unwrap_or(false),
                    });
                }
            }
        }
    }
    for words in &mut per_rec {
        words.sort_by(|a, b| a.t0.total_cmp(&b.t0));
    }
    per_rec
}

fn default_rank_command(env: &JobEnv) -> Vec<String> {
    if !env.cfg.auto_rank {
        return Vec::new();
    }
    let has_omp = std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("omp").is_file()));
    if !has_omp {
        return Vec::new();
    }
    let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/rank-clips-omp.py");
    let installed = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("../share/stream-engine/scripts/rank-clips-omp.py")));
    [Some(env.project_root.join("scripts/rank-clips-omp.py")), installed, Some(bundled)]
        .into_iter()
        .flatten()
        .find(|path| path.is_file())
        .map(|path| vec![path.to_string_lossy().into_owned()])
        .unwrap_or_default()
}

/// Keep the highest-ranked candidates while reserving room for both kinds on
/// long mixed shows; `max_clips` still limits the render workload.
fn select_ranked(plans: &mut Vec<Plan>, max_clips: usize) {
    plans.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.cand.peak.cmp(&b.cand.peak)));
    let mut keep = vec![false; plans.len()];
    let quota = if max_clips >= 2 { (max_clips / 4).max(1) } else { 0 };
    for musical in [true, false] {
        for (i, _) in plans.iter().enumerate().filter(|(_, p)| p.song.is_some() == musical).take(quota) {
            keep[i] = true;
        }
    }
    let mut selected = keep.iter().filter(|yes| **yes).count();
    for yes in &mut keep {
        if selected >= max_clips {
            break;
        }
        if !*yes {
            *yes = true;
            selected += 1;
        }
    }
    let mut i = 0;
    plans.retain(|_| {
        let selected = keep[i];
        i += 1;
        selected
    });
}

/// Run the job for one session.
pub fn process(env: &JobEnv, session: &str, progress: &dyn Fn(Progress)) -> Result<Report, String> {
    let cfg = &env.cfg;
    let t_all = Instant::now();
    let mut report = Report::default();
    let say = |stage: &str, done: usize, total: usize, detail: String| {
        progress(Progress { session: session.to_string(), stage: stage.into(), done, total, detail });
    };
    say("load", 0, 0, "reading markers and recordings".into());
    let t = Instant::now();
    let dir = session_dir(env, session);
    let files = session::load(&dir, cfg)?;
    if files.meta.recordings.is_empty() {
        if files.markers.is_empty() {
            return Ok(report);
        }
        return Err(format!("no recordings in {}/meta.toml", dir.display()));
    }
    let (recs, problems) = load_recordings(&files);
    report.skipped.extend(problems);
    let paths = show::paths_for(&dir);
    let last_ns = recs.iter().map(|r| r.start_ns.saturating_add((r.probe.duration * 1e9) as u64)).max().unwrap_or(0);
    let songs = session::songs(&dir, &paths, last_ns);
    let features = indexed_features(&paths);
    let drops = indexed_drops(&paths);
    let beats = indexed_beats(&paths);
    let mut cands = session::candidates(&files.markers, cfg);
    let indexed = show::read_manifest(&paths);
    let transcript = if indexed.is_some() { show::read_lane(&paths.transcript()) } else { Vec::new() };
    if let Some(manifest) = &indexed {
        cands.extend(indexed_talk_candidates(manifest, &transcript, &songs, cfg));
    }
    for (song, mut c) in songs.iter().zip(session::song_candidates(&songs, cfg)) {
        c.peak = song_peak(song, &files.markers, &drops, &features);
        let span = (cfg.max_len.0 * 1_000_000).min(45_000_000_000);
        c.start = c.peak.saturating_sub(span / 2).max(song.start);
        c.end = c.peak.saturating_add(span / 2).min(song.end);
        cands.push(c);
    }
    if cands.is_empty() {
        report.timings.push(("total_s".into(), t_all.elapsed().as_secs_f64()));
        return Ok(report);
    }
    report.timings.push(("probe_s".into(), t.elapsed().as_secs_f64()));

    // map each candidate to a recording
    let mut plans: Vec<Plan> = Vec::new();
    for c in cands {
        let Some(wi) = wide_for(&recs, c.peak) else {
            report.skipped.push(format!("{}: not inside any recording", c.key));
            continue;
        };
        let song = overlapping_song(&songs, c.start, c.end)
            .filter(|s| {
                let overlap = s.end.min(c.end).saturating_sub(s.start.max(c.start));
                (s.start <= c.peak && c.peak < s.end) || overlap >= (cfg.min_len.0 * 1_000_000).max((c.end - c.start) * 3 / 10)
            })
            .cloned();
        let audio = if song.is_some() {
            tracks::plan_song(recs[wi].probe.audio_streams, &recs[wi].rec.tracks, &cfg.audio)
        } else {
            tracks::plan(recs[wi].probe.audio_streams, &recs[wi].rec.tracks, &cfg.audio)
        };
        plans.push(Plan {
            key: c.key.clone(),
            wide: wi,
            audio,
            words: Vec::new(),
            transcript: (0.0, 0.0),
            trim: Trim { t_in: 0.0, t_out: 0.0, clean_in: false, clean_out: false },
            score: c.score,
            title: None,
            song,
            cand: c,
        });
    }
    if plans.is_empty() {
        return Err(format!("no candidate lies inside a recording ({})", report.skipped.join("; ")));
    }

    // Reuse the completed index when present; it is already on the same master
    // clock as the recordings. Only unindexed talk windows require Whisper.
    let t = Instant::now();
    let indexed_words = indexed.as_ref().map(|m| indexed_words(m, &transcript, &recs)).unwrap_or_default();
    let pad = cfg.whisper.pad.0 as f64 / 1000.0;
    for p in &mut plans {
        let rec = &recs[p.wide];
        p.transcript = if p.song.is_some() {
            (0.0, rec.probe.duration)
        } else {
            ((rec.at(p.cand.start) - pad).max(0.0), (rec.at(p.cand.end) + pad).min(rec.probe.duration))
        };
        if p.song.is_none()
            && let Some(words) = indexed_words.get(p.wide)
        {
            p.words = words.iter().filter(|w| w.t1 > p.transcript.0 && w.t0 < p.transcript.1).cloned().collect();
        }
    }
    let spans = merge_spans(&plans);
    let tx = if !spans.is_empty() {
        say("model", 0, 0, format!("Whisper {}", cfg.whisper.model));
        let model = transcribe::ensure_model(&env.data_dir, &cfg.whisper.model, |m| say("model", 0, 0, m.to_string()))?;
        Some(Transcriber::load(&model, &cfg.whisper)?)
    } else {
        None
    };
    report.timings.push(("model_s".into(), t.elapsed().as_secs_f64()));
    let t = Instant::now();
    let n = spans.len();
    for (i, span) in spans.iter().enumerate() {
        say("transcribe", i, n, format!("{:.0}–{:.0} s", span.from, span.to));
        let words = transcribe_window(tx.as_ref().expect("talk span needs model"), &recs[span.rec], span.stream, span.from, span.to, cfg.nice)?;
        for &k in &span.plans {
            let (a, b) = plans[k].transcript;
            plans[k].words = words.iter().filter(|w| w.t1 > a && w.t0 < b).cloned().collect();
        }
    }
    report.timings.push(("transcribe_s".into(), t.elapsed().as_secs_f64()));

    // in/out + ranking
    say("rank", 0, n, String::new());
    let t = Instant::now();
    for p in plans.iter_mut() {
        let rec = &recs[p.wide];
        let (start, peak, end) = (rec.at(p.cand.start), rec.at(p.cand.peak), rec.at(p.cand.end));
        if p.song.is_some() {
            let nearby: Vec<f64> = beats.iter().filter(|t| rec.covers(**t)).map(|t| rec.at(*t)).collect();
            p.trim = select::trim_music(start.max(0.0), peak, end.min(rec.probe.duration), 0.0, rec.probe.duration, &nearby, cfg);
            p.score = p.cand.score * cfg.ranking.marker;
        } else {
            p.trim = select::trim(&p.words, start.max(0.0), peak, end.min(rec.probe.duration), 0.0, rec.probe.duration, cfg);
            p.score = select::rank_score(p.cand.score, &p.words, &p.trim, peak, &cfg.ranking);
        }
        let from = (rec.start_ns as f64 + p.trim.t_in * 1e9) as u64;
        let to = (rec.start_ns as f64 + p.trim.t_out * 1e9) as u64;
        if let Some(song) = overlapping_song(&songs, from, to)
            && song.end.min(to).saturating_sub(song.start.max(from)) >= 2_000_000_000
        {
            let song_overlap = song.end.min(to).saturating_sub(song.start.max(from));
            if p.song.is_none() && song_overlap >= cfg.min_len.0 * 1_000_000 {
                let nearby: Vec<f64> = beats.iter().filter(|t| rec.covers(**t)).map(|t| rec.at(*t)).collect();
                p.trim = select::trim_music(start.max(0.0), peak, end.min(rec.probe.duration), 0.0, rec.probe.duration, &nearby, cfg);
                p.transcript = (0.0, rec.probe.duration);
                p.score = p.cand.score * cfg.ranking.marker;
            }
            p.song = Some(song.clone());
            p.audio = tracks::plan_song(rec.probe.audio_streams, &rec.rec.tracks, &cfg.audio);
        }
    }
    let rank_command = if cfg.rank_command.is_empty() { default_rank_command(env) } else { cfg.rank_command.clone() };
    if !rank_command.is_empty() {
        let cands: Vec<RankIn> = plans
            .iter()
            .map(|p| RankIn {
                key: p.key.clone(),
                score: p.score,
                marker_score: p.cand.score,
                reasons: p.cand.reasons.clone(),
                kind: if p.song.is_some() { "song" } else { "talk" }.into(),
                song: p.song.as_ref().map(|s| s.title.clone()),
                requester: p.song.as_ref().map(|s| s.user.clone()),
                dmca_risk: p.song.as_ref().is_some_and(|s| s.dmca),
                context: p.song.as_ref().map(session::Song::context).unwrap_or(serde_json::Value::Null),
                labels: p.cand.labels.clone(),
                t_in: p.trim.t_in,
                out: p.trim.t_out,
                peak: recs[p.wide].at(p.cand.peak),
                transcript_from: p.transcript.0,
                transcript_to: p.transcript.1,
                transcript: p.words.iter().filter(|w| !w.annotation).map(|w| w.text.as_str()).collect::<Vec<_>>().join(" "),
                words: p.words.clone(),
            })
            .collect();
        match select::external_rank(&rank_command, session, &cands, cfg) {
            Ok(answers) => {
                plans.retain_mut(|p| {
                    let rec = &recs[p.wide];
                    let bounds = if let Some(song) = &p.song {
                        let a = rec.at(song.start).max(0.0);
                        let b = rec.at(song.end).min(rec.probe.duration);
                        if b - a >= cfg.min_len.0 as f64 / 1000.0 { (a, b) } else { (0.0, rec.probe.duration) }
                    } else {
                        (p.transcript.0.max(0.0), p.transcript.1)
                    };
                    // The AI may prioritize songs, but not erase the only candidate for one.
                    let mut answers = answers.iter().filter(|a| a.key == p.key).cloned().collect::<Vec<_>>();
                    if p.key.starts_with("song-") {
                        for a in &mut answers {
                            a.drop = false;
                        }
                    }
                    select::apply_rank(&p.key, &mut p.trim, &mut p.score, &mut p.title, &answers, bounds, cfg)
                });
            }
            Err(e) => {
                tracing::warn!("rank_command failed ({e}); using the deterministic ranking");
                report.skipped.push(format!("rank_command: {e}"));
            }
        }
    }
    for p in &mut plans {
        let rec = &recs[p.wide];
        let from = (rec.start_ns as f64 + p.trim.t_in * 1e9) as u64;
        let to = (rec.start_ns as f64 + p.trim.t_out * 1e9) as u64;
        p.song = overlapping_song(&songs, from, to).filter(|s| s.end.min(to).saturating_sub(s.start.max(from)) >= 2_000_000_000).cloned();
        p.audio = if p.song.is_some() {
            tracks::plan_song(rec.probe.audio_streams, &rec.rec.tracks, &cfg.audio)
        } else {
            tracks::plan(rec.probe.audio_streams, &rec.rec.tracks, &cfg.audio)
        };
    }
    // Every song and marker reached the ranker; reserve a few rendered clips for
    // music and speech so hours of one kind do not eclipse the other.
    select_ranked(&mut plans, cfg.max_clips);
    report.timings.push(("rank_s".into(), t.elapsed().as_secs_f64()));

    // render
    let t = Instant::now();
    let out_dir = output_dir(&dir, cfg);
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
    let n = plans.len();
    for (i, p) in plans.iter().enumerate() {
        say("render", i, n, format!("clip {} of {n}", i + 1));
        let rec = &recs[p.wide];
        let mut row = ClipRow {
            session: session.to_string(),
            key: p.key.clone(),
            rank: i as i64 + 1,
            score: p.score,
            marker_score: p.cand.score,
            reasons: p.cand.reasons.clone(),
            labels: p.cand.labels.clone(),
            title: p.title.clone().or_else(|| p.song.as_ref().map(|s| s.title.clone())),
            kind: if p.song.is_some() { "song" } else { "talk" }.into(),
            song: p.song.as_ref().map(|s| s.title.clone()),
            requester: p.song.as_ref().map(|s| s.user.clone()),
            dmca_risk: p.song.as_ref().is_some_and(|s| s.dmca),
            context: p.song.as_ref().map(session::Song::context).unwrap_or(serde_json::Value::Null).to_string(),
            start_ns: p.cand.start as i64,
            peak_ns: p.cand.peak as i64,
            end_ns: p.cand.end as i64,
            recording: rec.rec.path.to_string_lossy().into_owned(),
            rec_start_ns: rec.start_ns as i64,
            in_s: p.trim.t_in,
            out_s: p.trim.t_out,
            peak_s: rec.at(p.cand.peak),
            words: serde_json::to_string(&p.words).unwrap_or_else(|_| "[]".into()),
            transcript_from: p.transcript.0,
            transcript_to: p.transcript.1,
            audio_note: p.audio.note.clone(),
            music_dropped: p.audio.music_dropped,
            status: "ready".into(),
            ..Default::default()
        };
        let tc = Instant::now();
        match render(env, &recs, p.wide, &p.audio, &p.words, &mut row, &out_dir) {
            Ok(()) => report.clips += 1,
            Err(e) => {
                tracing::error!("clip {} ({session}): {e}", p.key);
                row.status = "failed".into();
                row.error = Some(e);
                report.failed += 1;
            }
        }
        report.timings.push((format!("clip_{}_s", i + 1), tc.elapsed().as_secs_f64()));
        store::upsert(&env.db, &row).map_err(|e| format!("db: {e:#}"))?;
    }
    store::rerank(&env.db, session).map_err(|e| format!("db: {e:#}"))?;
    report.timings.push(("render_s".into(), t.elapsed().as_secs_f64()));
    report.timings.push(("total_s".into(), t_all.elapsed().as_secs_f64()));
    Ok(report)
}

fn encoders(cfg: &ClipsConfig) -> (Codec, bool) {
    match cfg.encoder {
        Encoder::Auto => (Codec::Nvenc, true),
        Encoder::Nvenc => (Codec::Nvenc, false),
        Encoder::X264 => (Codec::X264, false),
    }
}

/// Render every configured canvas for `row` (in/out already set) and fill in paths,
/// thumbnails, captions, and the encoder used.
fn render(env: &JobEnv, recs: &[Rec], wide: usize, audio: &AudioPlan, words: &[Word], row: &mut ClipRow, out_dir: &Path) -> Result<(), String> {
    let cfg = &env.cfg;
    let rec = &recs[wide];
    let dur = row.out_s - row.in_s;
    if dur <= 0.5 {
        return Err(format!("empty clip window {:.2}–{:.2}", row.in_s, row.out_s));
    }
    let (prefer, fallback) = encoders(cfg);
    let in_ns = rec.start_ns as f64 + row.in_s * 1e9;
    let out_ns = rec.start_ns as f64 + row.out_s * 1e9;
    let mut used = Vec::new();
    let mut plain = String::new();
    row.wide_path = None;
    row.tall_path = None;
    row.wide_thumb = None;
    row.tall_thumb = None;
    for canvas in &cfg.canvases {
        let size = if canvas == "tall" { cfg.tall_size } else { cfg.wide_size };
        // source video for this canvas
        let (video, video_at, audio_src, frame, fps) = if canvas == "tall" {
            let tall = match cfg.tall_source {
                TallSource::Crop => None,
                _ => tall_for(recs, in_ns as u64, out_ns as u64),
            };
            match (tall, cfg.tall_source) {
                (Some(t), _) => {
                    (t.rec.path.clone(), t.at(in_ns as u64), Some((rec.rec.path.clone(), row.in_s)), Frame::Fit { w: size[0], h: size[1] }, t.probe.fps)
                }
                (None, TallSource::Recording) => return Err("no tall recording covers this clip ([clips] tall_source = \"recording\")".into()),
                (None, _) => (rec.rec.path.clone(), row.in_s, None, Frame::Crop { w: size[0], h: size[1], center: cfg.tall_crop_center }, rec.probe.fps),
            }
        } else {
            (rec.rec.path.clone(), row.in_s, None, Frame::Fit { w: size[0], h: size[1] }, rec.probe.fps)
        };
        let base = format!("{}_{canvas}", row.key);
        let subtitles = if cfg.captions.enabled && row.kind != "song" {
            let layout = Layout::for_canvas(canvas, size, &cfg.captions);
            let cues = captions::cues(words, row.in_s, row.out_s, layout.max_chars, cfg.captions.max_caption.0 as f64 / 1000.0);
            if plain.is_empty() {
                plain = captions::plain(&cues);
            }
            let name = format!("{base}.ass");
            std::fs::write(out_dir.join(&name), captions::ass(&cues, &layout, &cfg.captions)).map_err(|e| format!("{name}: {e}"))?;
            (!cues.is_empty()).then_some(name)
        } else {
            None
        };
        let cut = Cut {
            video,
            video_at,
            audio: audio_src,
            tracks: audio.mix.clone(),
            duration: dur,
            frame,
            fps,
            subtitles,
            out: PathBuf::from(format!("{base}.mp4")),
        };
        let codec = ffmpeg::cut(&cut, prefer, fallback, &cfg.video, out_dir, cfg.nice).map_err(|e| format!("{canvas}: {e}"))?;
        used.push(format!("{canvas}:{}", codec.name()));
        let mp4 = out_dir.join(&cut.out);
        let thumb = out_dir.join(format!("{base}.jpg"));
        let at = (row.peak_s - row.in_s).clamp(0.0, (dur - 0.1).max(0.0));
        ffmpeg::run(&ffmpeg::thumb_args(&mp4, at, cfg.video.thumb_width, &thumb), out_dir, cfg.nice).map_err(|e| format!("thumbnail: {e}"))?;
        let (p, t) = (Some(mp4.to_string_lossy().into_owned()), Some(thumb.to_string_lossy().into_owned()));
        if canvas == "tall" {
            (row.tall_path, row.tall_thumb) = (p, t);
        } else {
            (row.wide_path, row.wide_thumb) = (p, t);
        }
    }
    row.captions = plain;
    row.encoder = used.join(" ");
    row.error = None;
    Ok(())
}

/// Re-cut a clip with new in/out (recording seconds). Re-transcribes when the new window
/// leaves the stored transcript.
pub fn retrim(env: &JobEnv, id: i64, t_in: f64, t_out: f64, progress: &dyn Fn(Progress)) -> Result<ClipRow, String> {
    let cfg = &env.cfg;
    let mut row = store::get(&env.db, id).map_err(|e| format!("db: {e:#}"))?.ok_or_else(|| format!("no clip {id}"))?;
    let say = |stage: &str, detail: String| progress(Progress { session: row.session.clone(), stage: stage.into(), done: 0, total: 1, detail });
    let dir = session_dir(env, &row.session);
    let files = session::load(&dir, cfg)?;
    let (recs, _) = load_recordings(&files);
    let wide = recs.iter().position(|r| r.rec.path.to_string_lossy() == row.recording).ok_or_else(|| format!("recording {} is gone", row.recording))?;
    let rec = &recs[wide];
    let (min_len, max_len) = (cfg.min_len.0 as f64 / 1000.0, cfg.max_len.0 as f64 / 1000.0);
    if !(t_in >= 0.0 && t_out <= rec.probe.duration + 0.01 && t_out > t_in) {
        return Err(format!("in/out {t_in:.2}–{t_out:.2} outside the recording (0–{:.2})", rec.probe.duration));
    }
    if t_out - t_in < min_len - 1e-6 || t_out - t_in > max_len + 1e-6 {
        return Err(format!("clip length {:.1} s outside {min_len:.0}–{max_len:.0} s", t_out - t_in));
    }
    let last_ns = rec.start_ns.saturating_add((rec.probe.duration * 1e9) as u64);
    let paths = show::paths_for(&dir);
    let songs = session::songs(&dir, &paths, last_ns);
    let from_ns = (rec.start_ns as f64 + t_in * 1e9) as u64;
    let to_ns = (rec.start_ns as f64 + t_out * 1e9) as u64;
    let song = overlapping_song(&songs, from_ns, to_ns).filter(|s| s.end.min(to_ns).saturating_sub(s.start.max(from_ns)) >= 2_000_000_000);
    let previously_musical = row.kind == "song";
    row.kind = if song.is_some() { "song" } else { "talk" }.into();
    row.song = song.map(|s| s.title.clone());
    row.requester = song.map(|s| s.user.clone());
    row.dmca_risk = song.is_some_and(|s| s.dmca);
    row.context = song.map(session::Song::context).unwrap_or(serde_json::Value::Null).to_string();
    let audio = if song.is_some() {
        tracks::plan_song(rec.probe.audio_streams, &rec.rec.tracks, &cfg.audio)
    } else {
        tracks::plan(rec.probe.audio_streams, &rec.rec.tracks, &cfg.audio)
    };
    row.audio_note = audio.note.clone();
    row.music_dropped = audio.music_dropped;
    let mut words: Vec<Word> = serde_json::from_str(&row.words).unwrap_or_default();
    if row.kind != "song" && (previously_musical || t_in < row.transcript_from - 0.01 || t_out > row.transcript_to + 0.01) {
        say("transcribe", "extending the transcript".into());
        let pad = cfg.whisper.pad.0 as f64 / 1000.0;
        let model = transcribe::ensure_model(&env.data_dir, &cfg.whisper.model, |m| say("model", m.to_string()))?;
        let tx = Transcriber::load(&model, &cfg.whisper)?;
        let from = (t_in - pad).max(0.0);
        let to = (t_out + pad).min(rec.probe.duration);
        words = match audio.transcribe {
            Some(s) => transcribe_window(&tx, rec, s, from, to, cfg.nice)?,
            None => Vec::new(),
        };
        row.transcript_from = from;
        row.transcript_to = to;
        row.words = serde_json::to_string(&words).unwrap_or_else(|_| "[]".into());
    }
    row.in_s = t_in;
    row.out_s = t_out;
    row.peak_s = row.peak_s.clamp(t_in, t_out);
    say("render", format!("re-cutting clip {id}"));
    let out_dir = output_dir(&dir, cfg);
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    render(env, &recs, wide, &audio, &words, &mut row, &out_dir)?;
    row.status = match row.status.as_str() {
        "approved" => "approved".into(),
        _ => "ready".into(),
    };
    store::update_cut(&env.db, &row).map_err(|e| format!("db: {e:#}"))?;
    store::get(&env.db, id).map_err(|e| format!("db: {e:#}"))?.ok_or_else(|| "clip vanished".into())
}

/// Make a reviewer-selected clip from show-relative seconds (`recording.timeline`'s
/// t0), including its song context/audio when the selection intersects a request.
pub fn make(env: &JobEnv, session: &str, t_in: f64, t_out: f64, progress: &dyn Fn(Progress)) -> Result<ClipRow, String> {
    let cfg = &env.cfg;
    let (min, max) = (cfg.min_len.0 as f64 / 1000.0, cfg.max_len.0 as f64 / 1000.0);
    if !t_in.is_finite() || !t_out.is_finite() || t_in < 0.0 || !(min..=max).contains(&(t_out - t_in)) {
        return Err(format!("manual clip needs a show-time in/out of {min:.0}–{max:.0} seconds"));
    }
    let dir = session_dir(env, session);
    let files = session::load(&dir, cfg)?;
    if files.meta.recordings.is_empty() {
        return Err(format!("session {session} has no recorded video for a manual clip"));
    }
    let (recs, errors) = load_recordings(&files);
    let paths = show::paths_for(&dir);
    let origin = show::read_manifest(&paths)
        .map(|m| m.t0_ns.max(0) as u64)
        .or_else(|| recs.iter().filter(|r| r.rec.is_master()).map(|r| r.start_ns).min())
        .ok_or_else(|| format!("no master recording for session {session}: {}", errors.join("; ")))?;
    let from = origin.saturating_add((t_in * 1e9) as u64);
    let to = origin.saturating_add((t_out * 1e9) as u64);
    let wide = wide_for(&recs, from)
        .filter(|&i| recs[i].covers(to))
        .or_else(|| recs.iter().position(|r| r.rec.is_master() && r.covers(from) && r.covers(to)))
        .ok_or_else(|| "manual selection must fit inside one master recording segment".to_string())?;
    let rec = &recs[wide];
    let (local_in, local_out) = (rec.at(from), rec.at(to));
    let last_ns = recs.iter().map(|r| r.start_ns.saturating_add((r.probe.duration * 1e9) as u64)).max().unwrap_or(to);
    let songs = session::songs(&dir, &paths, last_ns);
    let song = overlapping_song(&songs, from, to).filter(|s| s.end.min(to).saturating_sub(s.start.max(from)) >= 2_000_000_000);
    let audio = if song.is_some() {
        tracks::plan_song(rec.probe.audio_streams, &rec.rec.tracks, &cfg.audio)
    } else {
        tracks::plan(rec.probe.audio_streams, &rec.rec.tracks, &cfg.audio)
    };
    let words = if let Some(stream) = audio.transcribe {
        progress(Progress { session: session.into(), stage: "transcribe".into(), done: 0, total: 1, detail: "transcribing manual cut".into() });
        let model = transcribe::ensure_model(&env.data_dir, &cfg.whisper.model, |_| {})?;
        let tx = Transcriber::load(&model, &cfg.whisper)?;
        transcribe_window(&tx, rec, stream, local_in, local_out, cfg.nice)?
    } else {
        Vec::new()
    };
    let peak = (from + to) / 2;
    let mut row = ClipRow {
        session: session.into(),
        key: format!("manual-{}-{}", from / 1_000_000, to / 1_000_000),
        score: cfg.manual_score,
        marker_score: cfg.manual_score,
        reasons: vec!["manual selection".into()],
        title: song.map(|s| s.title.clone()),
        kind: if song.is_some() { "song" } else { "talk" }.into(),
        song: song.map(|s| s.title.clone()),
        requester: song.map(|s| s.user.clone()),
        dmca_risk: song.is_some_and(|s| s.dmca),
        context: song.map(session::Song::context).unwrap_or(serde_json::Value::Null).to_string(),
        start_ns: from as i64,
        peak_ns: peak as i64,
        end_ns: to as i64,
        recording: rec.rec.path.to_string_lossy().into_owned(),
        rec_start_ns: rec.start_ns as i64,
        in_s: local_in,
        out_s: local_out,
        peak_s: rec.at(peak),
        words: serde_json::to_string(&words).unwrap_or_else(|_| "[]".into()),
        transcript_from: local_in,
        transcript_to: local_out,
        audio_note: audio.note.clone(),
        music_dropped: audio.music_dropped,
        status: "ready".into(),
        ..Default::default()
    };
    progress(Progress { session: session.into(), stage: "render".into(), done: 0, total: 1, detail: "cutting manual clip".into() });
    let out_dir = output_dir(&dir, cfg);
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    render(env, &recs, wide, &audio, &words, &mut row, &out_dir)?;
    let id = store::upsert(&env.db, &row).map_err(|e| format!("db: {e:#}"))?;
    store::record_feedback(&env.db, id, "manual", serde_json::json!({"in": t_in, "out": t_out})).map_err(|e| format!("db: {e:#}"))?;
    store::rerank(&env.db, session).map_err(|e| format!("db: {e:#}"))?;
    store::get(&env.db, id).map_err(|e| format!("db: {e:#}"))?.ok_or_else(|| "manual clip vanished".into())
}

/// Run `[clips] upload_command` for a clip; returns the URL it reported (if any).
pub fn upload(env: &JobEnv, id: i64) -> Result<Option<String>, String> {
    let cfg = &env.cfg;
    let (prog, args) = cfg.upload_command.split_first().ok_or("no [clips] upload_command configured")?;
    let row = store::get(&env.db, id).map_err(|e| format!("db: {e:#}"))?.ok_or_else(|| format!("no clip {id}"))?;
    if row.status == "failed" {
        return Err(format!("clip {id} failed to render; retrim or re-run the job first"));
    }
    let input = serde_json::to_value(row.to_value()).map_err(|e| e.to_string())?;
    let out = select::run_json(prog, args, &input, std::time::Duration::from_millis(cfg.upload_timeout.0))?;
    let url = if out.iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        let v: serde_json::Value = serde_json::from_slice(&out).map_err(|e| format!("upload_command reply: {e}"))?;
        v.get("url").and_then(|u| u.as_str()).map(String::from)
    };
    store::set_status(&env.db, id, "uploaded", url.as_deref()).map_err(|e| format!("db: {e:#}"))?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(wide: usize, stream: Option<usize>, from: f64, to: f64) -> Plan {
        Plan {
            key: format!("k{from}"),
            cand: Candidate { key: String::new(), start: 0, peak: 0, end: 0, score: 1.0, reasons: vec![], labels: vec![], markers: vec![] },
            wide,
            audio: AudioPlan { mix: vec![0], transcribe: stream, music_dropped: true, note: String::new() },
            words: Vec::new(),
            transcript: (from, to),
            trim: Trim { t_in: 0.0, t_out: 0.0, clean_in: false, clean_out: false },
            score: 0.0,
            title: None,
            song: None,
        }
    }

    #[test]
    fn settle_waits_for_a_growing_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("rec.mkv");
        std::fs::write(&p, b"x").unwrap();
        let writer = {
            let p = p.clone();
            std::thread::spawn(move || {
                for i in 0..4 {
                    std::thread::sleep(std::time::Duration::from_millis(700));
                    std::fs::write(&p, vec![0u8; 10 * (i + 2)]).unwrap();
                }
            })
        };
        let t = Instant::now();
        assert!(wait_settled(&[p.clone(), d.path().join("missing.mkv")], std::time::Duration::from_secs(20)));
        writer.join().unwrap();
        // the last write lands ~2.8 s in; settled means ≥ 3 s after it
        assert!(t.elapsed().as_secs_f64() >= 5.5, "{:?}", t.elapsed());
        // a file still being written is reported as unsettled when time runs out
        std::fs::write(&p, b"fresh").unwrap();
        assert!(!wait_settled(&[p], std::time::Duration::from_millis(600)));
    }

    #[test]
    fn overlapping_windows_share_one_transcription_per_track() {
        let plans = vec![
            plan(0, Some(0), 70.0, 132.0),
            plan(0, Some(0), 33.0, 80.0),
            plan(0, Some(0), 118.0, 164.0),
            plan(0, Some(0), 300.0, 340.0),
            plan(1, Some(0), 100.0, 120.0),
            plan(0, None, 10.0, 20.0),
        ];
        let spans = merge_spans(&plans);
        assert_eq!(spans.len(), 3, "{spans:?}");
        assert_eq!((spans[0].from, spans[0].to, spans[0].plans.clone()), (33.0, 164.0, vec![1, 0, 2]));
        assert_eq!((spans[1].from, spans[1].plans.clone()), (300.0, vec![3]));
        assert_eq!((spans[2].rec, spans[2].plans.clone()), (1, vec![4]));
    }
    #[test]
    fn ranked_clips_include_talk_on_song_heavy_shows() {
        let song = session::Song { start: 1, end: 100, title: "A".into(), user: "dj".into(), video: "v".into(), channel: "c".into(), dmca: true };
        let mut plans: Vec<_> = (0..20)
            .map(|i| {
                let mut p = plan(0, None, i as f64, 0.0);
                p.song = Some(song.clone());
                p.score = 100.0 - i as f64;
                p
            })
            .collect();
        plans.extend((0..6).map(|i| {
            let mut p = plan(0, None, 100.0 + i as f64, 0.0);
            p.score = 10.0 - i as f64;
            p
        }));
        select_ranked(&mut plans, 8);
        assert_eq!(plans.len(), 8);
        assert_eq!(plans.iter().filter(|p| p.song.is_none()).count(), 2);
        assert!(plans.windows(2).all(|w| w[0].score >= w[1].score));
    }

    #[test]
    fn song_peak_prefers_marker_then_drop_then_indexed_hype_then_middle() {
        let song = session::Song {
            start: 10_000_000_000,
            end: 130_000_000_000,
            title: "A".into(),
            user: "dj".into(),
            video: "v".into(),
            channel: "c".into(),
            dmca: true,
        };
        assert_eq!(song_peak(&song, &[], &[], &[]), 70_000_000_000);
        assert_eq!(song_peak(&song, &[], &[], &[(80_000_000_000, 1.2), (90_000_000_000, 3.0)]), 90_000_000_000);
        assert_eq!(song_peak(&song, &[], &[80_000_000_000], &[(90_000_000_000, 3.0)]), 80_000_000_000);
        let marker = session::Marker {
            index: 0,
            ts: 60_000_000_000,
            wall_ms: 0,
            label: "drop".into(),
            origin: "user".into(),
            hype: true,
            start: 50_000_000_000,
            peak: 60_000_000_000,
            end: 70_000_000_000,
            score: 5.0,
            reasons: vec![],
        };
        assert_eq!(song_peak(&song, &[marker], &[80_000_000_000], &[(90_000_000_000, 3.0)]), 60_000_000_000);
    }

    #[test]
    fn indexed_hype_and_drop_events_use_show_clock() {
        let d = tempfile::tempdir().unwrap();
        let paths = show::ShowPaths::new(d.path());
        std::fs::create_dir_all(paths.lanes()).unwrap();
        std::fs::write(paths.manifest(), serde_json::to_string(&show::Manifest { t0_ns: 1_000_000_000, ..Default::default() }).unwrap()).unwrap();
        std::fs::write(paths.features(), "t,hype.score,band.level\n4,0.2,0\n12,2.5,0\n").unwrap();
        std::fs::write(paths.lane("moments"), "{\"t\":9,\"label\":\"music.drop\"}\n").unwrap();
        assert_eq!(indexed_features(&paths), vec![(5_000_000_000, 0.2), (13_000_000_000, 2.5)]);
        assert_eq!(indexed_drops(&paths), vec![10_000_000_000]);
    }

    #[test]
    fn indexed_talk_candidates_cover_distant_spoken_intervals_without_song_lyrics() {
        let d = tempfile::tempdir().unwrap();
        let paths = show::ShowPaths::new(d.path());
        std::fs::create_dir_all(paths.data()).unwrap();
        std::fs::write(paths.manifest(), serde_json::to_string(&show::Manifest { t0_ns: 1_000_000_000, ..Default::default() }).unwrap()).unwrap();
        let lines: Vec<_> = [20, 650, 3700]
            .into_iter()
            .map(|t| {
                serde_json::json!({
                    "t0": t, "t1": t + 20, "text": "these spoken words are part of a long passage worth reviewing later"
                })
                .to_string()
            })
            .collect();
        std::fs::write(paths.transcript(), lines.join("\n")).unwrap();
        let song = session::Song {
            start: 601_000_000_000,
            end: 901_000_000_000,
            title: "A".into(),
            user: "dj".into(),
            video: "v".into(),
            channel: "c".into(),
            dmca: true,
        };
        let manifest = show::read_manifest(&paths).unwrap();
        let transcript = show::read_lane(&paths.transcript());
        let candidates = indexed_talk_candidates(&manifest, &transcript, &[song], &ClipsConfig::default());
        assert_eq!(candidates.len(), 2);
        assert!(candidates[1].peak > 3_600_000_000_000);
        assert!(candidates.iter().all(|c| c.key.starts_with("talk-")));
    }

    #[test]
    fn clip_master_is_chosen_by_role_never_an_iso() {
        let rec = |canvas: &str, role: &str, source: &str, start_s: u64, duration: f64| Rec {
            rec: Recording { canvas: canvas.into(), role: role.into(), source: source.into(), start_ns: Some(start_s * 1_000_000_000), ..Default::default() },
            probe: Probe { duration, ..Default::default() },
            start_ns: start_s * 1_000_000_000,
        };
        // An ISO listed first and named "wide" must never become the clip master.
        let recs = [
            rec("wide", "iso", "camera:cam_kit", 0, 100.0),
            rec("tall", "iso", "canvas:tall", 0, 100.0),
            rec("main", "master", "canvas:wide", 0, 40.0),
            rec("main", "master", "canvas:wide", 45, 55.0),
        ];
        assert_eq!(wide_for(&recs, 10_000_000_000), Some(2));
        assert_eq!(wide_for(&recs, 50_000_000_000), Some(3), "a restarted master segment covers later moments");
        assert_eq!(wide_for(&recs, 42_000_000_000), None, "the gap between master segments has no master");
        assert!(tall_for(&recs, 1_000_000_000, 2_000_000_000).is_some_and(|r| r.rec.source == "canvas:tall"));
        // Older shows without roles: wide first, tall never.
        let legacy = [rec("tall", "", "", 0, 100.0), rec("cam", "", "", 0, 100.0), rec("wide", "", "", 0, 100.0)];
        assert_eq!(wide_for(&legacy, 1_000_000_000), Some(2));
        assert_eq!(wide_for(&legacy[..2], 1_000_000_000), Some(1));
        assert!(tall_for(&legacy, 0, 1_000_000_000).is_some_and(|r| r.rec.canvas == "tall"));
    }

    #[test]
    fn indexed_words_map_show_time_to_recording_without_retranscription() {
        let manifest = show::Manifest { t0_ns: 5_000_000_000, ..Default::default() };
        let rec = Rec {
            rec: Recording { canvas: "wide".into(), path: PathBuf::new(), start_ns: Some(3_000_000_000), ..Default::default() },
            probe: Probe { duration: 60.0, ..Default::default() },
            start_ns: 3_000_000_000,
        };
        let segments = [serde_json::json!({"t0": 10, "t1": 12,
            "words": [{"t0": 10, "t1": 10.5, "w": "Hello"}, {"t0": 11, "t1": 11.5, "w": "world."}]})];
        let words = indexed_words(&manifest, &segments, &[rec]);
        assert_eq!((words[0][0].t0, words[0][1].t1), (12.0, 13.5));
        let mut p = plan(0, Some(0), 10.0, 20.0);
        p.words = words[0].clone();
        assert!(merge_spans(&[p]).is_empty(), "indexed speech needs no second Whisper pass");
    }

    #[test]
    #[ignore = "on-demand ffmpeg smoke run (real multitrack cut)"]
    fn manual_song_cut_keeps_music_and_writes_review_feedback() {
        let root = tempfile::tempdir().unwrap();
        let rec = root.path().join("recording.mkv");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=24:duration=12",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=200:duration=12",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=320:duration=12",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=12",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=110:duration=12",
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:a",
                "-map",
                "3:a",
                "-map",
                "4:a",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
            ])
            .arg(&rec)
            .status()
            .unwrap();
        assert!(status.success());
        let project = root.path().join("project");
        let dir = project.join("sessions/test");
        let show_dir = root.path().join("show");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meta.toml"),
            format!(
                "show = {{ dir = {:?}, name = \"test\" }}\nrecordings = [{{ canvas = \"wide\", path = {:?}, start_ns = 1000000000, tracks = [
             {{ name = \"program\" }}, {{ name = \"mic\" }}, {{ name = \"music\" }}, {{ name = \"band\" }} ] }}]\n",
                show_dir.to_string_lossy(),
                rec.to_string_lossy()
            ),
        )
        .unwrap();
        let paths = show::ShowPaths::new(&show_dir);
        std::fs::create_dir_all(paths.lanes()).unwrap();
        std::fs::write(paths.manifest(), serde_json::to_string(&show::Manifest { t0_ns: 1_000_000_000, ..Default::default() }).unwrap()).unwrap();
        std::fs::write(paths.lane("songs"), r#"{"t0":1,"t1":10,"title":"Live Song","user":"viewer","video":"v123","channel":"band","dmca":true}"#).unwrap();
        let db = Db::memory().unwrap();
        store::migrate(&db).unwrap();
        let cfg = ClipsConfig { canvases: vec!["wide".into()], wide_size: [320, 240], encoder: Encoder::X264, auto_rank: false, ..ClipsConfig::default() };
        let env = JobEnv { db: db.clone(), project_root: project, data_dir: root.path().join("data"), cfg };
        let row = make(&env, "test", 1.0, 9.0, &|_| {}).unwrap();
        assert_eq!((row.kind.as_str(), row.song.as_deref(), row.requester.as_deref()), ("song", Some("Live Song"), Some("viewer")));
        assert!(row.dmca_risk && !row.music_dropped);
        assert!(row.captions.is_empty());
        let cut = Path::new(row.wide_path.as_deref().unwrap());
        assert!(cut.starts_with(paths.clips()) && cut.exists());
        let p = ffmpeg::probe(cut).unwrap();
        assert!((p.duration - 8.0).abs() < 0.3);
        let band_db = |file: &Path, hz| {
            let output = std::process::Command::new("ffmpeg")
                .args(["-v", "info", "-nostdin", "-i"])
                .arg(file)
                .args(["-af", &format!("bandpass=f={hz}:width_type=h:w=20,astats=measure_perchannel=none"), "-f", "null", "-"])
                .output()
                .unwrap();
            assert!(output.status.success());
            let log = String::from_utf8_lossy(&output.stderr);
            log.lines().filter_map(|line| line.split("RMS level dB:").nth(1)).filter_map(|v| v.trim().parse::<f64>().ok()).next_back().unwrap()
        };
        for hz in [200, 320, 440, 110] {
            assert!(band_db(cut, hz) > -45.0, "selected source {hz} Hz missing from manual clip");
        }
        assert_eq!(store::feedback(&db, "test").unwrap()[0].get_path("action").and_then(Value::as_str), Some("manual"));
        let report = process(&env, "test", &|_| {}).unwrap();
        assert_eq!((report.clips, report.failed), (1, 0), "a song needs a candidate without markers or Whisper");
        let generated = store::list(&db, Some("test"), None, 10).unwrap().into_iter().find(|c| c.key.starts_with("song-")).unwrap();
        assert_eq!((generated.kind.as_str(), generated.song.as_deref(), generated.dmca_risk), ("song", Some("Live Song"), true));
        assert!(!generated.music_dropped && generated.captions.is_empty());
        let generated_cut = Path::new(generated.wide_path.as_deref().unwrap());
        for hz in [200, 320, 440, 110] {
            assert!(band_db(generated_cut, hz) > -45.0, "selected source {hz} Hz missing from automatic clip");
        }
    }
}
