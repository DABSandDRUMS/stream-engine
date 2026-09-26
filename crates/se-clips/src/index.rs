//! Offline, versioned show index. All timestamps in the output are seconds since the first
//! recording's master-clock start; sessions without recordings use their first journal start.
use crate::{
    config::ClipsConfig,
    ffmpeg, session,
    show::{self, FeaturesInfo, LaneInfo, LaneKind, Manifest, ManifestRecording, RecordingConfig, SCHEMA, ShowPaths},
    tracks, transcribe,
};
use se_hub::{Bus, EngineCtx};
use se_proto::{Event, Origin, Value};
use se_store::session::LogRec;
use serde_json::{Value as Json, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const VERSION: u32 = 1;
const LANES: &[(&str, &str, LaneKind)] = &[
    ("songs", "Songs", LaneKind::Spans),
    ("scenes", "Scenes", LaneKind::Spans),
    ("modes", "Modes", LaneKind::Spans),
    ("lights", "Lights", LaneKind::Points),
    ("effects", "Quick effects", LaneKind::Points),
    ("chat", "Chat", LaneKind::Points),
    ("moments", "Moments", LaneKind::Points),
    ("markers", "Markers", LaneKind::Points),
];
const COLUMNS: &[&str] = &["hype.score", "mic.level", "music.level", "band.level", "twitch.chat_rate", "queue.position"];

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

// Decode concatenated zstd frames a line at a time, without retaining the entire journal.
fn decoder(path: &Path) -> Result<impl Read, String> {
    let file = File::open(path).map_err(err)?;
    zstd::stream::read::Decoder::new(file).map_err(err)
}
fn events(dir: &Path, mut visit: impl FnMut(LogRec) -> Result<(), String>) -> Result<(), String> {
    let path = dir.join("events.jsonl.zst");
    if !path.exists() {
        return Ok(());
    }
    let mut reader = BufReader::new(decoder(&path)?);
    let mut decoded = false;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => match serde_json::from_str::<LogRec>(&line) {
                Ok(rec) => {
                    decoded = true;
                    visit(rec)?;
                }
                Err(_) => break, // incomplete last line
            },
            Err(e) if decoded => {
                tracing::warn!("truncated session journal {}: {e}", path.display());
                break;
            }
            Err(e) => return Err(format!("decoding {}: {e}", path.display())),
        }
    }
    Ok(())
}

fn start_time(dir: &Path) -> Result<(i64, i64, i64), String> {
    let mut first = None;
    let mut first_event = None;
    let mut last = 0i64;
    events(dir, |rec| {
        match rec {
            LogRec::Start { t0, period, tick, wall_ns, .. } if first.is_none() => {
                // A rotated session keeps the daemon's original t0, but wall_ns refers to
                // this restart's tick. Map using that tick, not the daemon's first tick.
                let at = t0.saturating_add(period.saturating_mul(tick));
                first = Some((at.min(i64::MAX as u64) as i64, wall_ns / 1_000_000));
            }
            LogRec::Ev { event } => {
                first_event.get_or_insert(event.ts as i64);
                last = last.max(event.ts as i64);
            }
            _ => {}
        }
        Ok(())
    })?;
    let (t0, wall) = first.unwrap_or((first_event.unwrap_or(last), 0));
    Ok((t0, wall, last))
}

fn atomic(path: &Path, f: impl FnOnce(&mut BufWriter<File>) -> Result<(), String>) -> Result<(), String> {
    let tmp = path.with_extension(format!("{}.index.tmp", path.extension().and_then(|s| s.to_str()).unwrap_or("")));
    let result = (|| {
        let mut out = BufWriter::new(File::create(&tmp).map_err(err)?);
        f(&mut out)?;
        out.flush().map_err(err)?;
        out.get_ref().sync_all().map_err(err)?;
        fs::rename(&tmp, path).map_err(err)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
fn line(out: &mut impl Write, v: &Json) -> Result<(), String> {
    serde_json::to_writer(&mut *out, v).map_err(err)?;
    out.write_all(b"\n").map_err(err)
}
fn text(v: &Value, key: &str) -> String {
    v.get_path(key).and_then(Value::as_str).unwrap_or("").to_owned()
}
fn time(ns: i64, t0: i64) -> f64 {
    show::secs(ns, t0).max(0.0)
}
fn path_of(dir: &Path, session_dir: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_owned()
    } else if dir.join(p).exists() {
        dir.join(p)
    } else {
        session_dir.join(p)
    }
}

struct LaneWriter {
    name: &'static str,
    title: &'static str,
    kind: LaneKind,
    out: BufWriter<File>,
    tmp: PathBuf,
    count: usize,
}
impl LaneWriter {
    fn new(paths: &ShowPaths, name: &'static str, title: &'static str, kind: LaneKind) -> Result<Self, String> {
        let tmp = paths.lane(name).with_extension("jsonl.index.tmp");
        Ok(Self { name, title, kind, out: BufWriter::new(File::create(&tmp).map_err(err)?), tmp, count: 0 })
    }
    fn push(&mut self, item: Json) -> Result<(), String> {
        line(&mut self.out, &item)?;
        self.count += 1;
        Ok(())
    }
    fn finish(mut self, paths: &ShowPaths) -> Result<LaneInfo, String> {
        self.out.flush().map_err(err)?;
        self.out.get_ref().sync_all().map_err(err)?;
        fs::rename(&self.tmp, paths.lane(self.name)).map_err(err)?;
        Ok(LaneInfo { name: self.name.into(), title: self.title.into(), kind: self.kind, analyzer: "journal".into(), count: self.count })
    }
}

fn close_span(lane: &mut LaneWriter, active: &mut Option<(i64, Json)>, end: i64, t0: i64) -> Result<(), String> {
    if let Some((start, mut data)) = active.take()
        && end > t0
        && end > start
    {
        data["t0"] = json!(time(start, t0));
        data["t1"] = json!(time(end, t0));
        lane.push(data)?;
    }
    Ok(())
}

fn journal_lanes(dir: &Path, paths: &ShowPaths, t0: i64, end: i64, chat_delay_ms: i64) -> Result<Vec<LaneInfo>, String> {
    let mut lanes: Vec<LaneWriter> = LANES.iter().map(|&(n, title, kind)| LaneWriter::new(paths, n, title, kind)).collect::<Result<_, _>>()?;
    let mut song: Option<(i64, Json)> = None;
    let mut scene: Option<(i64, Json)> = None;
    let mut mode: Option<(i64, Json)> = None;
    events(dir, |rec| {
        let LogRec::Ev { event: e } = rec else { return Ok(()) };
        let ts = e.ts as i64;
        let p = &e.payload;
        match e.ty.as_str() {
            "queue.song_started" => {
                close_span(&mut lanes[0], &mut song, ts, t0)?;
                song = Some((
                    ts,
                    json!({"label": text(p, "title"), "title": text(p, "title"), "user": text(p, "user"), "video": text(p, "video"), "channel": text(p, "channel"), "dmca": true, "entry": p.get_path("id").and_then(Value::as_i64)}),
                ));
            }
            "queue.song_ended" => close_span(&mut lanes[0], &mut song, ts, t0)?,
            "scene.changed" => {
                close_span(&mut lanes[1], &mut scene, ts, t0)?;
                scene = Some((ts, json!({"label": text(p, "scene")})));
            }
            "mode.changed" => {
                close_span(&mut lanes[2], &mut mode, ts, t0)?;
                mode = Some((ts, json!({"label": text(p, "to")})));
            }
            _ => {}
        }
        if ts < t0 {
            return Ok(());
        }
        let t = time(ts, t0);
        match e.ty.as_str() {
            "lights.cue.go" | "lights.cue.released" => lanes[3].push(json!({"t": t, "label": text(p, "cue"), "cuelist": text(p, "cuelist"), "event": e.ty}))?,
            ty if ty.starts_with("preset.") && (ty.ends_with(".fired") || ty.ends_with(".released")) => {
                lanes[4].push(json!({"t": t, "label": ty.trim_start_matches("preset.").split('.').next().unwrap_or(""), "event": ty}))?;
            }
            "twitch.chat" | "tiktok.chat" => {
                let delay = if e.ty == "twitch.chat" { chat_delay_ms.max(0) as f64 / 1000.0 } else { 0.0 };
                lanes[5].push(json!({"t": (t - delay).max(0.0), "label": text(p, "message"), "user": e.actor.as_ref().map(|a| a.name.as_str()).unwrap_or(""), "source": e.ty}))?;
            }
            "twitch.sub" | "twitch.resub" | "twitch.gift" | "twitch.cheer" | "twitch.raid" | "twitch.redeem" | "tip" | "hype.peak" | "music.drop"
            | "band.drop" => {
                let label = e.ty.strip_prefix("twitch.").unwrap_or(&e.ty);
                lanes[6].push(json!({"t": t, "label": label, "user": e.actor.as_ref().map(|a| a.name.as_str()).unwrap_or(""), "payload": p}))?;
            }
            "session.marker" => lanes[7].push(json!({"t": t, "label": text(p, "label"), "kind": text(p, "args.kind"), "origin": text(p, "origin")}))?,
            _ => {}
        }
        Ok(())
    })?;
    close_span(&mut lanes[0], &mut song, end, t0)?;
    close_span(&mut lanes[1], &mut scene, end, t0)?;
    close_span(&mut lanes[2], &mut mode, end, t0)?;
    // Older journals may have markers.json but no marker event. Don't duplicate those already logged.
    if lanes[7].count == 0
        && let Ok(bytes) = fs::read(dir.join("markers.json"))
        && let Ok(markers) = serde_json::from_slice::<Vec<Value>>(&bytes)
    {
        for m in markers {
            if let Some(ts) = m.get_path("ts").and_then(Value::as_i64).filter(|v| *v >= t0) {
                lanes[7].push(json!({"t": time(ts, t0), "label": text(&m, "label"), "kind": text(&m, "args.kind")}))?;
            }
        }
    }
    lanes.into_iter().map(|l| l.finish(paths)).collect()
}

// Decode the binary signal format incrementally; retain at most one second of observations.
fn sampled_signals(dir: &Path, paths: &ShowPaths, t0: i64) -> Result<Option<FeaturesInfo>, String> {
    let path = dir.join("signals.bin");
    if !path.exists() {
        return Ok(None);
    }
    let mut input = decoder(&path)?;
    let tmp = paths.features().with_extension("csv.index.tmp");
    let result = (|| {
        let mut out = BufWriter::new(File::create(&tmp).map_err(err)?);
        writeln!(out, "t,{}", COLUMNS.join(",")).map_err(err)?;
        let mut names = Vec::new();
        let mut second = None;
        let mut sums = [0.0f64; COLUMNS.len()];
        let mut counts = [0u32; COLUMNS.len()];
        let mut rows = 0usize;
        let emit = |out: &mut BufWriter<File>, s: i64, sums: &[f64; COLUMNS.len()], counts: &[u32; COLUMNS.len()]| -> Result<(), String> {
            write!(out, "{s}").map_err(err)?;
            for (sum, count) in sums.iter().zip(counts) {
                if *count > 0 {
                    write!(out, ",{:.5}", sum / *count as f64).map_err(err)?;
                } else {
                    write!(out, ",").map_err(err)?;
                }
            }
            writeln!(out).map_err(err)
        };
        loop {
            let mut tag = [0u8];
            if input.read_exact(&mut tag).is_err() {
                break;
            }
            let mut u32buf = [0u8; 4];
            if input.read_exact(&mut u32buf).is_err() {
                break;
            }
            let n = u32::from_le_bytes(u32buf) as usize;
            if tag[0] == 1 && n > 4096 {
                break;
            }
            match tag[0] {
                1 => {
                    names.clear();
                    for _ in 0..n {
                        let mut b = [0u8; 2];
                        if input.read_exact(&mut b).is_err() {
                            break;
                        }
                        let len = u16::from_le_bytes(b) as usize;
                        let mut s = vec![0u8; len];
                        if input.read_exact(&mut s).is_err() {
                            break;
                        }
                        names.push(String::from_utf8_lossy(&s).into_owned());
                    }
                }
                2 => {
                    // For frame records the first four bytes above belong to the timestamp.
                    let mut ts = [0u8; 8];
                    ts[..4].copy_from_slice(&u32buf);
                    if input.read_exact(&mut ts[4..]).is_err() || input.read_exact(&mut u32buf).is_err() {
                        break;
                    }
                    let count = u32::from_le_bytes(u32buf) as usize;
                    if count > 4096 {
                        break;
                    }
                    let ns = u64::from_le_bytes(ts) as i64;
                    let sec = (ns - t0) / 1_000_000_000;
                    if let Some(prev) = second
                        && sec != prev
                    {
                        emit(&mut out, prev, &sums, &counts)?;
                        rows += 1;
                        sums.fill(0.0);
                        counts.fill(0);
                    }
                    second = (ns >= t0).then_some(sec).or(second);
                    for (i, name) in names.iter().enumerate().take(count) {
                        let mut b = [0u8; 4];
                        if input.read_exact(&mut b).is_err() {
                            return Ok(rows);
                        }
                        if ns >= t0
                            && let Some(j) = COLUMNS.iter().position(|col| col == name)
                        {
                            let v = f32::from_le_bytes(b);
                            if v.is_finite() {
                                sums[j] += v as f64;
                                counts[j] += 1;
                            }
                        }
                        if i + 1 == count {
                            break;
                        }
                    }
                    // Frames can report more values than the most recently declared names.
                    for _ in names.len()..count {
                        let mut b = [0u8; 4];
                        if input.read_exact(&mut b).is_err() {
                            return Ok(rows);
                        }
                    }
                }
                _ => break,
            }
        }
        if let Some(s) = second {
            emit(&mut out, s, &sums, &counts)?;
            rows += 1;
        }
        out.flush().map_err(err)?;
        out.get_ref().sync_all().map_err(err)?;
        Ok(rows)
    })();
    if result.is_ok() {
        fs::rename(tmp, paths.features()).map_err(err)?;
    } else {
        let _ = fs::remove_file(tmp);
    }
    result.map(|rows| (rows > 0).then(|| FeaturesInfo { columns: COLUMNS.iter().map(|s| (*s).to_string()).collect(), rate: 1.0 }))
}

fn copy_journal(dir: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(err)?;
    for name in ["events.jsonl.zst", "signals.bin", "markers.json", "meta.toml"] {
        let src = dir.join(name);
        if src.is_file() {
            let tmp = dst.join(format!("{name}.index.tmp"));
            fs::copy(&src, &tmp).map_err(err)?;
            fs::rename(tmp, dst.join(name)).map_err(err)?;
        }
    }
    Ok(())
}

fn snapshot_project(root: &Path, target: &Path) -> Result<(), String> {
    if !root.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(target).map_err(err)?;
    let mut todo = vec![(root.to_path_buf(), target.to_path_buf(), 0usize)];
    let mut files = 0usize;
    while let Some((src, dst, depth)) = todo.pop() {
        if depth > 8 {
            continue;
        }
        for item in fs::read_dir(src).map_err(err)? {
            let item = item.map_err(err)?;
            let ty = item.file_type().map_err(err)?;
            let origin = item.path();
            if ty.is_symlink() || target.starts_with(&origin) {
                continue;
            }
            let name = item.file_name().to_string_lossy().into_owned();
            let lower = name.to_ascii_lowercase();
            if lower.starts_with('.')
                || ["sessions", "assets", "data", "cache", "target", "recordings", "node_modules"].contains(&lower.as_str())
                || ["secret", "token", "credential", "password", "private", "auth", "keyring"].iter().any(|s| lower.contains(s))
            {
                continue;
            }
            if ty.is_dir() {
                fs::create_dir_all(dst.join(&name)).map_err(err)?;
                todo.push((origin, dst.join(name), depth + 1));
            } else if ty.is_file() && files < 4096 {
                let ext = origin.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
                if !["toml", "json", "yaml", "yml", "txt", "lua", "cue", "csv"].contains(&ext.as_str()) {
                    continue;
                }
                if item.metadata().map_err(err)?.len() > 1024 * 1024 {
                    continue;
                }
                let bytes = fs::read(&origin).map_err(err)?;
                let Ok(contents) = std::str::from_utf8(&bytes) else { continue };
                let content = contents.to_ascii_lowercase();
                // Reject the entire file if any key, section or PEM block may contain a
                // credential. Project snapshots are best effort, never a secret store.
                let sensitive = ["secret", "token", "password", "credential", "private_key", "api_key", "authorization"];
                if content.contains("-----begin private key")
                    || content.lines().any(|line| {
                        let line = line.trim_start();
                        if line.starts_with('#') {
                            return false;
                        }
                        let key = line.split_once(['=', ':']).map(|(k, _)| k).unwrap_or(line);
                        sensitive.iter().any(|s| key.contains(s))
                    })
                {
                    continue;
                }
                atomic(&dst.join(name), |out| out.write_all(&bytes).map_err(err))?;
                files += 1;
            }
        }
    }
    Ok(())
}

fn transcript(paths: &ShowPaths, recordings: &[(session::Recording, PathBuf, f64)], t0: i64, cfg: &ClipsConfig, data_dir: &Path) -> Result<bool, String> {
    let mut whisper = None;
    let mut wrote = false;
    atomic(&paths.transcript(), |out| {
        for (rec, path, duration) in recordings {
            let probe = ffmpeg::probe(path)?;
            let plan = tracks::plan(probe.audio_streams, &rec.tracks, &cfg.audio);
            let Some(stream) = plan.transcribe else { continue };
            // A missing OBS layout or a mixed program/music stream is not a speech track.
            // Never transcribe the musical performance as if it were dialogue.
            let track = rec
                .tracks
                .iter()
                .find(|t| t.index == stream)
                .cloned()
                .or_else(|| cfg.audio.tracks.get(stream).map(|name| session::TrackInfo { index: stream, name: name.clone(), ..Default::default() }));
            let Some(track) = track else { continue };
            let roles = tracks::roles(&track, &cfg.audio);
            if roles.iter().any(|r| ["music", "program", "band", "drums"].contains(&r.as_str())) || !roles.iter().any(|r| cfg.audio.transcribe.contains(r)) {
                continue;
            }
            if whisper.is_none() {
                let model = transcribe::ensure_model(data_dir, &cfg.whisper.model, |_| {})?;
                whisper = Some(transcribe::Transcriber::load(&model, &cfg.whisper)?);
            }
            let whisper = whisper.as_ref().unwrap();
            // Bound PCM and Whisper memory independently of show length; do not cross file ends.
            let offset = rec.start_ns.map(|s| show::secs(s as i64, t0)).unwrap_or(0.0);
            let mut from = 0.0;
            while from < *duration {
                let to = (from + 25.0).min(*duration);
                let pcm = transcribe::extract_pcm(path, stream, from, to, cfg.nice)?;
                let mut sentence = Vec::new();
                for word in whisper.words(&pcm, from + offset)? {
                    if word.annotation {
                        continue;
                    }
                    sentence.push(word);
                    let split = sentence.last().is_some_and(|w| w.text.ends_with(['.', '!', '?'])) || sentence.len() >= 30;
                    if split {
                        write_sentence(out, &sentence)?;
                        sentence.clear();
                        wrote = true;
                    }
                }
                if !sentence.is_empty() {
                    write_sentence(out, &sentence)?;
                    wrote = true;
                }
                from = to;
            }
        }
        Ok(())
    })?;
    Ok(wrote)
}
fn write_sentence(out: &mut impl Write, words: &[transcribe::Word]) -> Result<(), String> {
    line(
        out,
        &json!({"t0": words.first().unwrap().t0.max(0.0), "t1": words.last().unwrap().t1.max(0.0),
        "text": words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" "),
        "words": words.iter().map(|w| json!({"t0": w.t0.max(0.0), "t1": w.t1.max(0.0), "w": w.text})).collect::<Vec<_>>() }),
    )
}

/// Force a new index; serialized against automatic indexing and clip processing.
pub fn build(session_dir: &Path, session: &str, cfg: &RecordingConfig) -> Result<Manifest, String> {
    let project = project_for(session_dir);
    serialized_build(session_dir, session, cfg, &clips_for(&project), &project, &se_store::data_dir(), 0, false)
}

/// Return the current index or build it. A per-session advisory file lock ensures a clip worker
/// and the automatic indexer never write the same lane at once.
pub fn ensure(session_dir: &Path, session: &str, cfg: &RecordingConfig) -> Result<Manifest, String> {
    let project = project_for(session_dir);
    serialized_build(session_dir, session, cfg, &clips_for(&project), &project, &se_store::data_dir(), 0, true)
}

fn project_for(session_dir: &Path) -> PathBuf {
    session_dir
        .parent()
        .filter(|p| p.file_name().is_some_and(|name| name == "sessions"))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn clips_for(project: &Path) -> ClipsConfig {
    if project == Path::new(".") {
        return ClipsConfig::default();
    }
    fs::read_to_string(project.join("project.toml"))
        .ok()
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .and_then(|doc| ClipsConfig::from_section(doc.get("clips")).ok())
        .unwrap_or_default()
}

// A rebuild keeps the last published generation independently of the mutable current files.
// This is also a recovery point if a subsequent analyzer fails before show.json is committed.
fn backup_previous(paths: &ShowPaths, manifest: &Manifest) -> Result<(), String> {
    let versions = paths.data().join("versions");
    fs::create_dir_all(&versions).map_err(err)?;
    let stem = format!("{}-{}", manifest.indexed_at_ms, manifest.schema);
    let target = (0..10_000)
        .map(|n| versions.join(if n == 0 { stem.clone() } else { format!("{stem}-{n}") }))
        .find(|path| !path.exists())
        .ok_or("too many prior index versions")?;
    let stage = target.with_extension("index.tmp");
    fs::create_dir_all(stage.join("lanes")).map_err(err)?;
    fs::copy(paths.manifest(), stage.join("show.json")).map_err(err)?;
    for lane in &manifest.lanes {
        if lane.name.is_empty() || !lane.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            continue;
        }
        let source = paths.lane(&lane.name);
        if source.is_file() {
            fs::copy(source, stage.join("lanes").join(format!("{}.jsonl", lane.name))).map_err(err)?;
        }
    }
    for (source, name) in [(paths.features(), "features.csv"), (paths.transcript(), "transcript.jsonl")] {
        if source.is_file() {
            fs::copy(source, stage.join(name)).map_err(err)?;
        }
    }
    fs::rename(stage, target).map_err(err)
}

fn serialized_build(
    session_dir: &Path,
    session: &str,
    cfg: &RecordingConfig,
    clips: &ClipsConfig,
    project: &Path,
    data_dir: &Path,
    delay: i64,
    reuse: bool,
) -> Result<Manifest, String> {
    if !session_dir.is_dir() {
        return Err(format!("session {} does not exist", session_dir.display()));
    }
    let lock = OpenOptions::new().create(true).write(true).truncate(false).open(session_dir.join(".show-index.lock")).map_err(err)?;
    // SAFETY: flock operates on this owned open file descriptor. Closing it releases the lock.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(err(io::Error::last_os_error()));
    }
    let paths = show::paths_for(session_dir);
    if reuse && let Some(m) = show::read_manifest(&paths) {
        let lanes_ok = m.lanes.iter().all(|lane| paths.lane(&lane.name).exists());
        let has_journal = cfg.index.skip.iter().any(|s| s == "journal") || m.analyzers.get("journal") == Some(&VERSION);
        let has_recordings = fs::read_to_string(session_dir.join("meta.toml"))
            .ok()
            .and_then(|s| session::parse_meta(&s).ok())
            .is_none_or(|meta| m.recordings.len() == meta.recordings.len());
        let has_analyzers = cfg
            .index
            .external
            .iter()
            .filter(|e| !cfg.index.skip.contains(&e.name))
            .all(|e| m.analyzers.get(&e.name) == Some(&VERSION) && paths.lane(&e.name).exists());
        if m.schema == SCHEMA && m.session == session && lanes_ok && has_journal && has_recordings && has_analyzers {
            return Ok(m);
        }
    }
    if let Some(m) = show::read_manifest(&paths) {
        backup_previous(&paths, &m)?;
    }
    build_with(session_dir, session, cfg, clips, project, data_dir, delay)
}
fn build_with(
    session_dir: &Path,
    session: &str,
    cfg: &RecordingConfig,
    clips: &ClipsConfig,
    project: &Path,
    data_dir: &Path,
    chat_delay_ms: i64,
) -> Result<Manifest, String> {
    if !session_dir.is_dir() {
        return Err(format!("session {} does not exist", session_dir.display()));
    }
    let paths = show::paths_for(session_dir);
    fs::create_dir_all(paths.lanes()).map_err(err)?;
    let meta = fs::read_to_string(session_dir.join("meta.toml")).ok().map(|s| session::parse_meta(&s)).transpose()?.unwrap_or_default();
    let chat_delay_ms = meta.clock.as_ref().map(|clock| clock.twitch_delay_ms as i64).filter(|ms| *ms > 0).unwrap_or(if chat_delay_ms > 0 {
        chat_delay_ms
    } else {
        clips.hype.fallback_delay.0 as i64
    });
    let (start, wall, last) = start_time(session_dir)?;
    let t0 = meta.recordings.iter().filter_map(|r| r.start_ns).min().map(|n| n as i64).unwrap_or(start);
    let mut manifest = Manifest {
        schema: SCHEMA,
        session: session.into(),
        name: paths.root.file_name().unwrap_or_default().to_string_lossy().into_owned(),
        t0_ns: t0,
        t0_wall_ms: if wall == 0 { 0 } else { wall.saturating_add((t0 - start) / 1_000_000) },
        indexed_at_ms: now_ms(),
        ..Default::default()
    };
    let mut recorded = Vec::new();
    let mut end = last.max(t0.saturating_add(1_000_000_000));
    for rec in meta.recordings {
        let path = path_of(&paths.root, session_dir, &rec.path);
        if path.exists() && !crate::job::wait_settled(std::slice::from_ref(&path), Duration::from_secs(30)) {
            manifest.notes.push(format!("Recording {} did not settle before indexing", path.display()));
        }
        let offset = rec.start_ns.map(|s| show::secs(s as i64, t0)).unwrap_or(0.0);
        let duration = if path.exists() { ffmpeg::probe(&path).ok().map(|p| p.duration).filter(|d| *d > 0.0) } else { None }
            .or_else(|| rec.start_ns.zip(rec.end_ns).map(|(a, b)| (b.saturating_sub(a)) as f64 / 1e9));
        if let Some(d) = duration {
            end = end.max(t0.saturating_add(((offset + d) * 1e9) as i64));
        }
        manifest.recordings.push(ManifestRecording {
            path: path.to_string_lossy().into_owned(),
            canvas: rec.canvas.clone(),
            offset,
            duration,
            tracks: rec.tracks.iter().map(|t| t.name.clone()).collect(),
        });
        if let Some(d) = duration
            && path.exists()
        {
            recorded.push((rec, path, d));
        }
    }
    if manifest.recordings.is_empty() {
        manifest.notes.push("No recording metadata; showing the session journal".into());
    } else if recorded.is_empty() {
        manifest.notes.push("Recording files unavailable; showing the session journal".into());
    }
    manifest.duration = show::secs(end, t0).max(0.0);
    for name in &cfg.index.skip {
        manifest.notes.push(format!("{name} analyzer skipped by configuration"));
    }
    if !cfg.index.skip.iter().any(|s| s == "journal") {
        manifest.lanes = journal_lanes(session_dir, &paths, t0, end, chat_delay_ms)?;
        manifest.analyzers.insert("journal".into(), VERSION);
    }
    if !cfg.index.skip.iter().any(|s| s == "signals") {
        manifest.features = sampled_signals(session_dir, &paths, t0)?;
        if manifest.features.is_some() {
            manifest.analyzers.insert("signals".into(), VERSION);
        }
    }
    copy_journal(session_dir, &paths.session_copy())?;
    if cfg.snapshot_project && project.is_dir() && project != Path::new(".") && !paths.project_copy().exists() {
        snapshot_project(project, &paths.project_copy())?;
    }
    if cfg.index.transcript && !cfg.index.skip.iter().any(|s| s == "transcript") && !recorded.is_empty() {
        match transcript(&paths, &recorded, t0, clips, data_dir) {
            Ok(true) => {
                manifest.transcript = Some("transcript.jsonl".into());
                manifest.analyzers.insert("transcript".into(), VERSION);
            }
            Ok(false) => manifest.notes.push("No spoken audio was found on the speech track".into()),
            Err(e) => manifest.notes.push(e),
        }
    }
    for ext in &cfg.index.external {
        if cfg.index.skip.contains(&ext.name) {
            continue;
        }
        if ext.name.is_empty()
            || !ext.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            || ext.command.is_empty()
            || LANES.iter().any(|(name, _, _)| *name == ext.name)
            || ["journal", "signals", "transcript"].contains(&ext.name.as_str())
            || manifest.analyzers.contains_key(&ext.name)
        {
            manifest.notes.push(format!("Invalid or duplicate external analyzer {}", ext.name));
            continue;
        }
        let mut cmd = std::process::Command::new(show::expand_home(&ext.command[0]));
        cmd.args(&ext.command[1..])
            .env("SE_SHOW_DIR", &paths.root)
            .env("SE_SHOW_DATA", paths.data())
            .current_dir(&paths.root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(mut child) => {
                let deadline = std::time::Instant::now() + Duration::from_millis(ext.timeout.0);
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            if status.success() {
                                if let Ok(f) = File::open(paths.lane(&ext.name)) {
                                    let count = BufReader::new(f).lines().count();
                                    manifest.lanes.push(LaneInfo {
                                        name: ext.name.clone(),
                                        title: ext.title.clone(),
                                        kind: LaneKind::Points,
                                        analyzer: ext.name.clone(),
                                        count,
                                    });
                                    manifest.analyzers.insert(ext.name.clone(), VERSION);
                                } else {
                                    manifest.notes.push(format!("{} wrote no lane", ext.name));
                                }
                            } else {
                                manifest.notes.push(format!("{} exited with {status}", ext.name));
                            }
                            break;
                        }
                        Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
                        Ok(None) => {
                            let _ = child.kill();
                            let _ = child.wait();
                            manifest.notes.push(format!("{} timed out", ext.name));
                            break;
                        }
                        Err(e) => {
                            manifest.notes.push(format!("{}: {e}", ext.name));
                            break;
                        }
                    }
                }
            }
            Err(e) => manifest.notes.push(format!("{}: {e}", ext.name)),
        }
    }
    atomic(&paths.manifest(), |out| {
        serde_json::to_writer_pretty(&mut *out, &manifest).map_err(err)?;
        out.write_all(b"\n").map_err(err)
    })?;
    Ok(manifest)
}

fn session_dir(ctx: &EngineCtx, session: &str) -> Result<PathBuf, String> {
    if session.is_empty() || session.contains(['/', '\\']) || session == "." || session == ".." {
        return Err("invalid session id".into());
    }
    let from_db: Option<String> = ctx
        .db
        .with(|c| {
            use rusqlite::OptionalExtension;
            c.query_row("SELECT dir FROM sessions WHERE id = ?1", [session], |r| r.get(0)).optional()
        })
        .map_err(err)?;
    Ok(from_db.map(PathBuf::from).unwrap_or_else(|| ctx.project_root.join("sessions").join(session)))
}
fn wanted(item: &Json, from: f64, to: f64) -> bool {
    if let Some(t) = item.get("t").and_then(Json::as_f64) {
        t >= from && t < to
    } else {
        item.get("t0").and_then(Json::as_f64).unwrap_or(f64::INFINITY) < to && item.get("t1").and_then(Json::as_f64).unwrap_or(f64::NEG_INFINITY) > from
    }
}
fn bounded_jsonl(path: &Path, from: f64, to: f64, limit: usize) -> Vec<Json> {
    File::open(path)
        .ok()
        .map(|f| {
            BufReader::new(f)
                .lines()
                .map_while(Result::ok)
                .filter_map(|line| serde_json::from_str::<Json>(&line).ok())
                .filter(|v| wanted(v, from, to))
                .take(limit)
                .collect()
        })
        .unwrap_or_default()
}
fn timeline(ctx: &EngineCtx, args: &Value) -> Result<Value, String> {
    let id = args.get_path("session").and_then(Value::as_str).ok_or("needs session")?;
    let dir = session_dir(ctx, id)?;
    if !dir.exists() {
        return Err(format!("session {id} not found"));
    }
    let paths = show::paths_for(&dir);
    let manifest = match show::read_manifest(&paths) {
        Some(m) => m,
        None => {
            let cfg = RecordingConfig::from_section(ctx.project_section("recording").as_ref())?;
            let clips = ClipsConfig::from_section(ctx.project_section("clips").as_ref()).unwrap_or_default();
            serialized_build(&dir, id, &cfg, &clips, &ctx.project_root, &ctx.data_dir, ctx.hub.clock.mappings().twitch_delay_ms as i64, true)?
        }
    };
    let from = args.get_path("from").or_else(|| args.get_path("t0")).and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
    let to = args.get_path("to").or_else(|| args.get_path("t1")).and_then(Value::as_f64).unwrap_or(manifest.duration.max(from + 1.0)).max(from);
    let limit = args.get_path("limit").and_then(Value::as_i64).unwrap_or(500).clamp(1, 2000) as usize;
    let mut lanes = serde_json::Map::new();
    for lane in &manifest.lanes {
        // Never accept paths from a stored manifest; names are file stems only.
        if !lane.name.is_empty() && lane.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            lanes.insert(lane.name.clone(), json!(bounded_jsonl(&paths.lane(&lane.name), from, to, limit)));
        }
    }
    let mut features = Vec::new();
    if manifest.features.is_some()
        && let Ok(f) = File::open(paths.features())
    {
        let columns = manifest.features.as_ref().map(|f| f.columns.as_slice()).unwrap_or(&[]);
        for row in BufReader::new(f).lines().skip(1).filter_map(Result::ok) {
            let mut parts = row.split(',');
            let Some(t) = parts.next().and_then(|s| s.parse::<f64>().ok()) else { continue };
            if t < from || t >= to {
                continue;
            }
            let mut item = serde_json::Map::new();
            item.insert("t".into(), json!(t));
            for (name, raw) in columns.iter().zip(parts) {
                if let Ok(n) = raw.parse::<f64>() {
                    item.insert(name.clone(), json!(n));
                }
            }
            features.push(Json::Object(item));
            if features.len() == limit {
                break;
            }
        }
    }
    let transcript = if manifest.transcript.is_some() { bounded_jsonl(&paths.transcript(), from, to, limit) } else { Vec::new() };
    Ok(Value::from(json!({"manifest": manifest, "lanes": lanes, "features": features, "transcript": transcript})))
}

/// Register `recording.index` and `recording.timeline`; indexing never runs on live threads.
pub async fn start(ctx: EngineCtx) -> anyhow::Result<()> {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<String>(16);
    let worker = ctx.clone();
    tokio::spawn(async move {
        while let Some(id) = receiver.recv().await {
            let w = worker.clone();
            let key = id.clone();
            w.hub.emit(Event::new("recording.index.progress", Origin::System, Value::map().with("session", id.clone()).with("stage", "building")));
            match tokio::task::spawn_blocking(move || {
                let dir = session_dir(&w, &key)?;
                let cfg = RecordingConfig::from_section(w.project_section("recording").as_ref())?;
                let clips = ClipsConfig::from_section(w.project_section("clips").as_ref()).unwrap_or_default();
                serialized_build(&dir, &key, &cfg, &clips, &w.project_root, &w.data_dir, w.hub.clock.mappings().twitch_delay_ms as i64, true)
            })
            .await
            {
                Ok(Ok(m)) => {
                    let path = session_dir(&worker, &id).map(|dir| show::paths_for(&dir).manifest().display().to_string()).unwrap_or_default();
                    worker.hub.emit(Event::new(
                        "recording.index.done",
                        Origin::System,
                        Value::map().with("session", id).with("path", path).with("lanes", m.lanes.len() as i64),
                    ));
                }
                other => worker.hub.emit(Event::new(
                    "recording.index.failed",
                    Origin::System,
                    Value::map().with("session", id.clone()).with(
                        "error",
                        match other {
                            Ok(Err(e)) => e,
                            Err(e) => e.to_string(),
                            _ => unreachable!(),
                        },
                    ),
                )),
            }
        }
    });
    let action_tx = sender.clone();
    let action_hub = ctx.hub.clone();
    let action_ctx = ctx.clone();
    let mut actions = action_hub.route_actions("recording.index");
    tokio::spawn(async move {
        // The route was registered synchronously before returning from start().
        while let Some(c) = actions.recv().await {
            let se_proto::Op::Action { name, args } = &c.op else { continue };
            if name != "recording.index" {
                continue;
            }
            let selected = args.get_path("session").or_else(|| args.get_path("args.0")).and_then(Value::as_str).map(String::from);
            let id = selected.or_else(|| {
                action_ctx
                    .db
                    .with(|db| {
                        use rusqlite::OptionalExtension;
                        db.query_row("SELECT id FROM sessions WHERE ended_at IS NOT NULL ORDER BY ended_at DESC LIMIT 1", [], |r| r.get(0)).optional()
                    })
                    .ok()
                    .flatten()
            });
            let Some(id) = id else {
                action_hub.log("error", "recording.index", "no closed session to index");
                continue;
            };
            if sender.send(id).await.is_err() {
                break;
            }
        }
    });
    let event_hub = ctx.hub.clone();
    let event_ctx = ctx.clone();
    let mut events = event_hub.subscribe();
    tokio::spawn(async move {
        // Subscribe before returning from start() so the first close cannot be missed.
        loop {
            match events.recv().await {
                Ok(b) => {
                    if let Bus::Event(e) = &*b
                        && e.ty == "session.closed"
                        && RecordingConfig::from_section(event_ctx.project_section("recording").as_ref()).is_ok_and(|c| c.index.enabled)
                        && let Some(id) = e.payload.get_path("session").and_then(Value::as_str)
                        && action_tx.send(id.to_owned()).await.is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
    let query_ctx = ctx.clone();
    ctx.hub.register_query(
        "recording.timeline",
        Arc::new(move |_, args| {
            let ctx = query_ctx.clone();
            Box::pin(async move { tokio::task::spawn_blocking(move || timeline(&ctx, &args)).await.map_err(err)? })
        }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_store::session::SessionWriter;

    #[tokio::test]
    async fn show_index_aligns_song_markers_chat_and_sampled_signals() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = SessionWriter::open(root.path(), "s1").unwrap();
        writer.write(&LogRec::Start { t0: 1_000_000_000, period: 4_166_666, tick: 0, wall_ns: 1_700_000_000_000_000_000, restore: None }).unwrap();
        let mut event = |ty: &str, t: u64, payload: Value| {
            let mut e = Event::new(ty, Origin::System, payload);
            e.ts = t;
            writer.write(&LogRec::Ev { event: e }).unwrap();
        };
        event("mode.changed", 2_000_000_000, Value::map().with("to", "live"));
        event(
            "queue.song_started",
            3_000_000_000,
            Value::map().with("title", "Song").with("user", "viewer").with("video", "video-id").with("channel", "artist"),
        );
        event("twitch.chat", 5_000_000_000, Value::map().with("message", "great"));
        event("session.marker", 6_000_000_000, Value::map().with("label", "solo"));
        event("queue.song_ended", 8_000_000_000, Value::map());
        writer.write_signals(4_000_000_000, &["hype.score".into(), "mic.level".into()], &[0.5, 0.2]).unwrap();
        writer.write_signals(4_400_000_000, &["hype.score".into(), "mic.level".into()], &[0.9, 0.4]).unwrap();
        writer.close().unwrap();
        let dir = root.path().join("s1");
        let cfg = RecordingConfig { snapshot_project: false, ..Default::default() };
        let m = serialized_build(&dir, "s1", &cfg, &ClipsConfig::default(), root.path(), root.path(), 1500, true).unwrap();
        assert_eq!(m.t0_ns, 1_000_000_000);
        assert_eq!(m.duration, 7.0);
        let paths = show::paths_for(&dir);
        let songs = bounded_jsonl(&paths.lane("songs"), 0.0, 10.0, 30);
        assert_eq!(songs[0]["t0"], 2.0);
        assert_eq!(songs[0]["t1"], 7.0);
        assert_eq!(songs[0]["user"], "viewer");
        assert_eq!(songs[0]["video"], "video-id");
        assert_eq!(songs[0]["dmca"], true);
        assert_eq!(bounded_jsonl(&paths.lane("songs"), 3.0, 4.0, 30).len(), 1);
        assert!(bounded_jsonl(&paths.lane("songs"), 7.0, 8.0, 30).is_empty());
        let chat = bounded_jsonl(&paths.lane("chat"), 0.0, 10.0, 30);
        assert_eq!(chat[0]["t"], 2.5); // received at t=4, 1.5-second Twitch delay
        let csv = fs::read_to_string(paths.features()).unwrap();
        assert!(csv.lines().any(|row| row.starts_with("3,0.70000,0.30000,")), "{csv}");
        assert!(paths.session_copy().join("events.jsonl.zst").exists());
        let cached = ensure(&dir, "s1", &cfg).unwrap();
        assert_eq!(cached.indexed_at_ms, m.indexed_at_ms);
        let (hub, _rx) = se_hub::Hub::new(Arc::new(se_clock::Clock::new()));
        let (_tx, config) = tokio::sync::watch::channel(Arc::new(se_core::Config::default()));
        let db = se_store::Db::memory().unwrap();
        db.session_open("s1", &dir.to_string_lossy()).unwrap();
        let ctx = EngineCtx {
            hub: hub.clone(),
            db,
            project_root: root.path().to_path_buf(),
            data_dir: root.path().to_path_buf(),
            share_dir: root.path().to_path_buf(),
            config,
            http: "127.0.0.1:0".parse().unwrap(),
            dev: false,
        };
        start(ctx).await.unwrap();
        let window = hub.query("recording.timeline", Value::map().with("session", "s1").with("from", 2.0).with("to", 4.0).with("limit", 1)).await.unwrap();
        assert_eq!(window.get_path("lanes.songs").and_then(Value::as_list).unwrap().len(), 1);
        assert_eq!(window.get_path("lanes.markers").and_then(Value::as_list).unwrap().len(), 0);
        assert_eq!(window.get_path("features").and_then(Value::as_list).unwrap().len(), 1);
        let feature = window.get_path("features.0").unwrap().as_map().unwrap();
        assert_eq!(feature.get("hype.score").and_then(Value::as_f64), Some(0.7));
        build(&dir, "s1", &cfg).unwrap();
        let prior = paths.data().join("versions").join(format!("{}-{}", m.indexed_at_ms, m.schema));
        let before: Manifest = serde_json::from_slice(&fs::read(prior.join("show.json")).unwrap()).unwrap();
        assert_eq!(before, m);
        assert_eq!(bounded_jsonl(&prior.join("lanes/chat.jsonl"), 0.0, 10.0, 1)[0]["t"], 2.5);
    }

    #[tokio::test]
    async fn closing_a_session_indexes_in_background_and_reports_completion() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = SessionWriter::open(root.path(), "closed").unwrap();
        writer.write(&LogRec::Start { t0: 1_000_000_000, period: 4_166_666, tick: 0, wall_ns: 1_700_000_000_000_000_000, restore: None }).unwrap();
        writer.close().unwrap();
        let dir = root.path().join("closed");
        let (hub, rx) = se_hub::Hub::new(Arc::new(se_clock::Clock::new()));
        let (_tx, config) = tokio::sync::watch::channel(Arc::new(se_core::Config::default()));
        let db = se_store::Db::memory().unwrap();
        db.session_open("closed", &dir.to_string_lossy()).unwrap();
        let ctx = EngineCtx {
            hub: hub.clone(),
            db,
            project_root: root.path().to_path_buf(),
            data_dir: root.path().to_path_buf(),
            share_dir: root.path().to_path_buf(),
            config,
            http: "127.0.0.1:0".parse().unwrap(),
            dev: false,
        };
        assert!(RecordingConfig::from_section(ctx.project_section("recording").as_ref()).unwrap().index.enabled);
        start(ctx).await.unwrap();
        hub.publish_bus(Bus::Event(Event::new("session.closed", Origin::System, Value::map().with("session", "closed"))));
        // This fixture has no core runner: read the hub's outbound core inputs.
        let done = tokio::task::spawn_blocking(move || {
            loop {
                let message = rx.recv_timeout(Duration::from_secs(5)).unwrap();
                if let se_hub::CoreMsg::Input(se_core::Input::Event { event }) = message {
                    if event.ty == "recording.index.failed" {
                        panic!("index failed: {:?}", event.payload)
                    }
                    if event.ty == "recording.index.done" {
                        break event.payload;
                    }
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(done.get_path("session").and_then(Value::as_str), Some("closed"));
        assert!(show::paths_for(&dir).manifest().exists());
    }

    #[test]
    fn project_snapshot_excludes_secrets_and_session_data() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let snapshot = root.path().join("show/data/project");
        fs::create_dir_all(project.join("lights")).unwrap();
        fs::create_dir_all(project.join("sessions")).unwrap();
        fs::write(project.join("project.toml"), "name = \"Example\"\n").unwrap();
        fs::write(project.join("lights/cues.toml"), "cue = \"intro\"\n").unwrap();
        fs::write(project.join("lights/control.toml"), "access_token = \"hidden\"\n").unwrap();
        fs::write(project.join("sessions/private.toml"), "name = \"journal\"\n").unwrap();
        snapshot_project(&project, &snapshot).unwrap();
        assert!(snapshot.join("project.toml").exists());
        assert!(snapshot.join("lights/cues.toml").exists());
        assert!(!snapshot.join("lights/control.toml").exists());
        assert!(!snapshot.join("sessions/private.toml").exists());
        let nested = project.join("videos/show/data/project");
        snapshot_project(&project, &nested).unwrap();
        assert!(nested.join("lights/cues.toml").exists());
        assert!(!nested.join("videos").exists(), "do not recursively snapshot the show itself");
    }

    #[test]
    fn recording_start_and_rotated_session_start_share_the_master_clock() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = SessionWriter::open(root.path(), "recorded").unwrap();
        writer.write(&LogRec::Start { t0: 1_000_000_000, period: 100_000_000, tick: 20, wall_ns: 1_700_000_000_000_000_000, restore: None }).unwrap();
        for (kind, ns) in [("queue.song_started", 9_000_000_000), ("queue.song_ended", 12_000_000_000)] {
            let mut event = Event::new(kind, Origin::System, Value::map().with("title", "Solo"));
            event.ts = ns;
            writer.write(&LogRec::Ev { event }).unwrap();
        }
        writer.close().unwrap();
        let dir = root.path().join("recorded");
        fs::write(dir.join("meta.toml"), "[[recordings]]\ncanvas = \"wide\"\npath = \"missing.mkv\"\nstart_ns = 10000000000\nend_ns = 13000000000\n").unwrap();
        let manifest = build(&dir, "recorded", &RecordingConfig::default()).unwrap();
        assert_eq!(manifest.t0_ns, 10_000_000_000);
        assert_eq!(manifest.t0_wall_ms, 1_700_000_007_000);
        assert_eq!(manifest.duration, 3.0);
        assert_eq!(manifest.recordings[0].offset, 0.0);
        let songs = bounded_jsonl(&show::paths_for(&dir).lane("songs"), 0.0, 3.0, 10);
        assert_eq!(songs[0]["t0"], 0.0);
        assert_eq!(songs[0]["t1"], 2.0);
    }

    #[test]
    fn simultaneous_ensure_calls_publish_one_complete_generation() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = SessionWriter::open(root.path(), "shared").unwrap();
        writer.write(&LogRec::Start { t0: 1_000_000_000, period: 4_166_666, tick: 0, wall_ns: 1_700_000_000_000_000_000, restore: None }).unwrap();
        writer.close().unwrap();
        let dir = root.path().join("shared");
        let cfg = RecordingConfig::default();
        let first = {
            let dir = dir.clone();
            let cfg = cfg.clone();
            std::thread::spawn(move || ensure(&dir, "shared", &cfg).unwrap())
        };
        let second = {
            let dir = dir.clone();
            std::thread::spawn(move || ensure(&dir, "shared", &cfg).unwrap())
        };
        assert_eq!(first.join().unwrap(), second.join().unwrap());
        let paths = show::paths_for(&dir);
        assert!(paths.lane("songs").exists());
        assert!(!paths.data().join("versions").exists(), "the second caller must reuse the first generation");
    }

    #[test]
    fn legacy_session_without_recording_metadata_stays_browsable() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = SessionWriter::open(root.path(), "legacy").unwrap();
        let mut event = Event::new("scene.changed", Origin::System, Value::map().with("scene", "wide"));
        event.ts = 9_000_000_000;
        writer.write(&LogRec::Ev { event }).unwrap();
        writer.close().unwrap();
        let dir = root.path().join("legacy");
        let m = build(&dir, "legacy", &RecordingConfig::default()).unwrap();
        assert!(m.recordings.is_empty());
        assert_eq!(m.t0_ns, 9_000_000_000);
        assert_eq!(bounded_jsonl(&show::paths_for(&dir).lane("scenes"), 0.0, 1.0, 10)[0]["label"], "wide");
        assert!(show::paths_for(&dir).manifest().exists());
    }
}
