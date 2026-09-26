//! The post-stream clip job (§18, §15.8 "the session closes and the clip job is queued"):
//! markers → candidate windows → recording time → Whisper words → in/out on sentence
//! boundaries → ranking (deterministic, optional external ranker) → wide + tall cuts with
//! burned-in captions and without the music track → thumbnails → `clips` rows.
//! Also retrims (re-cut) and the upload hook. Everything here is blocking; the engine runs it
//! on a niced worker thread.

use crate::captions::{self, Layout};
use crate::config::{ClipsConfig, Encoder, TallSource};
use crate::ffmpeg::{self, Codec, Cut, Frame, Probe};
use crate::select::{self, RankIn, Trim};
use crate::session::{self, Candidate, Recording, SessionFiles};
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

/// Wide (canonical, multitrack) recording for a moment: the `wide` canvas when present.
fn wide_for(recs: &[Rec], t: u64) -> Option<usize> {
    recs.iter().position(|r| r.rec.canvas == "wide" && r.covers(t)).or_else(|| recs.iter().position(|r| r.rec.canvas != "tall" && r.covers(t)))
}

fn tall_for(recs: &[Rec], t0: u64, t1: u64) -> Option<&Rec> {
    recs.iter().find(|r| r.rec.canvas == "tall" && r.covers(t0) && r.covers(t1))
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
    let mut order: Vec<usize> = (0..plans.len()).filter(|k| plans[*k].audio.transcribe.is_some()).collect();
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
    let cands = session::candidates(&files.markers, cfg);
    if cands.is_empty() {
        report.timings.push(("total_s".into(), t_all.elapsed().as_secs_f64()));
        return Ok(report);
    }
    if files.meta.recordings.is_empty() {
        return Err(format!("{} marker window(s) but no OBS recordings in {}/meta.toml", cands.len(), dir.display()));
    }
    let (recs, problems) = load_recordings(&files);
    report.skipped.extend(problems);
    report.timings.push(("probe_s".into(), t.elapsed().as_secs_f64()));

    // map each candidate to a recording
    let mut plans: Vec<Plan> = Vec::new();
    for c in cands {
        let Some(wi) = wide_for(&recs, c.peak) else {
            report.skipped.push(format!("{}: not inside any recording", c.key));
            continue;
        };
        let audio = tracks::plan(recs[wi].probe.audio_streams, &recs[wi].rec.tracks, &cfg.audio);
        plans.push(Plan {
            key: c.key.clone(),
            wide: wi,
            audio,
            words: Vec::new(),
            transcript: (0.0, 0.0),
            trim: Trim { t_in: 0.0, t_out: 0.0, clean_in: false, clean_out: false },
            score: c.score,
            title: None,
            cand: c,
        });
    }
    if plans.is_empty() {
        return Err(format!("no marker lies inside a recording ({})", report.skipped.join("; ")));
    }

    // transcripts
    let t = Instant::now();
    say("model", 0, 0, format!("Whisper {}", cfg.whisper.model));
    let model = transcribe::ensure_model(&env.data_dir, &cfg.whisper.model, |m| say("model", 0, 0, m.to_string()))?;
    let tx = Transcriber::load(&model, &cfg.whisper)?;
    report.timings.push(("model_s".into(), t.elapsed().as_secs_f64()));
    let t = Instant::now();
    let pad = cfg.whisper.pad.0 as f64 / 1000.0;
    for p in plans.iter_mut() {
        let rec = &recs[p.wide];
        p.transcript = ((rec.at(p.cand.start) - pad).max(0.0), (rec.at(p.cand.end) + pad).min(rec.probe.duration));
    }
    // overlapping windows of the same track are transcribed once
    let spans = merge_spans(&plans);
    let n = spans.len();
    for (i, span) in spans.iter().enumerate() {
        say("transcribe", i, n, format!("{:.0}–{:.0} s", span.from, span.to));
        let words = transcribe_window(&tx, &recs[span.rec], span.stream, span.from, span.to, cfg.nice)?;
        for &k in &span.plans {
            let (a, b) = plans[k].transcript;
            plans[k].words = words.iter().filter(|w| w.t1 > a && w.t0 < b).cloned().collect();
        }
    }
    drop(tx);
    report.timings.push(("transcribe_s".into(), t.elapsed().as_secs_f64()));

    // in/out + ranking
    say("rank", 0, n, String::new());
    let t = Instant::now();
    for p in plans.iter_mut() {
        let rec = &recs[p.wide];
        let (start, peak, end) = (rec.at(p.cand.start), rec.at(p.cand.peak), rec.at(p.cand.end));
        p.trim = select::trim(&p.words, start.max(0.0), peak, end.min(rec.probe.duration), 0.0, rec.probe.duration, cfg);
        p.score = select::rank_score(p.cand.score, &p.words, &p.trim, peak, &cfg.ranking);
    }
    if !cfg.rank_command.is_empty() {
        let cands: Vec<RankIn> = plans
            .iter()
            .map(|p| RankIn {
                key: p.key.clone(),
                score: p.score,
                marker_score: p.cand.score,
                reasons: p.cand.reasons.clone(),
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
        match select::external_rank(&cfg.rank_command, session, &cands, cfg) {
            Ok(answers) => {
                plans.retain_mut(|p| {
                    let bounds = (p.transcript.0.max(0.0), p.transcript.1);
                    select::apply_rank(&p.key, &mut p.trim, &mut p.score, &mut p.title, &answers, bounds, cfg)
                });
            }
            Err(e) => {
                tracing::warn!("rank_command failed ({e}); using the deterministic ranking");
                report.skipped.push(format!("rank_command: {e}"));
            }
        }
    }
    plans.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.cand.peak.cmp(&b.cand.peak)));
    plans.truncate(cfg.max_clips);
    report.timings.push(("rank_s".into(), t.elapsed().as_secs_f64()));

    // render
    let t = Instant::now();
    let out_dir = dir.join(&cfg.output_dir);
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
            title: p.title.clone(),
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
        let subtitles = if cfg.captions.enabled {
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
    let audio = tracks::plan(rec.probe.audio_streams, &rec.rec.tracks, &cfg.audio);
    let mut words: Vec<Word> = serde_json::from_str(&row.words).unwrap_or_default();
    if t_in < row.transcript_from - 0.01 || t_out > row.transcript_to + 0.01 {
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
    let out_dir = dir.join(&cfg.output_dir);
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    render(env, &recs, wide, &audio, &words, &mut row, &out_dir)?;
    row.status = match row.status.as_str() {
        "approved" => "approved".into(),
        _ => "ready".into(),
    };
    store::update_cut(&env.db, &row).map_err(|e| format!("db: {e:#}"))?;
    store::get(&env.db, id).map_err(|e| format!("db: {e:#}"))?.ok_or_else(|| "clip vanished".into())
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
}
