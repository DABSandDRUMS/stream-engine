//! Live analysis thread (§8.3): reads the analysis taps (bus input sums and the mic input)
//! from lock-free rings, runs `se_analysis::LiveAnalyzer` per source, and publishes
//! `<bus>.*` signals, onset/drop/section events, the global beat clock (`beat.bpm`,
//! `beat.phase`, `beat` events, tap tempo), and `mic.*` (level, hype, `mic.talking`).

use crate::taps::BlockStamp;
use parking_lot::Mutex;
use se_analysis::{AnalysisEvent, Features, LiveAnalyzer, LiveConfig, N_BANDS, OnsetConfig};
use se_clock::musical::{MusicalClock, Observation, Source as ClockSource};
use se_hub::Hub;
use se_proto::{Event, Origin, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// RMS below which a bus is treated as silent (≈ −50 dBFS): no beat clock from it.
const SILENCE: f32 = 0.003;
/// A bus is live while its latest audible hop (above [`SILENCE`]) is at most this old, so the
/// gaps between hits of a sparse groove neither drop the clock nor reset the tracker.
const FRESH_NS: u64 = 250_000_000;

/// Beat clock shared with the control thread (feeds the RT transport). Written by the
/// analysis thread only.
#[derive(Default)]
pub struct BeatShared {
    pub bpm: AtomicU32,
    pub beat: AtomicU64,
    pub ts: AtomicU64,
    pub seq: AtomicU64,
    pub confidence: AtomicU32,
}

impl BeatShared {
    fn store(&self, bpm: f32, beat: f64, ts: u64, conf: f32) {
        self.seq.fetch_add(1, Ordering::AcqRel);
        self.bpm.store(bpm.to_bits(), Ordering::Relaxed);
        self.beat.store(beat.to_bits(), Ordering::Relaxed);
        self.ts.store(ts, Ordering::Relaxed);
        self.confidence.store(conf.to_bits(), Ordering::Relaxed);
        self.seq.fetch_add(1, Ordering::Release);
    }

    /// (seq, bpm, beat, ts, confidence)
    pub fn load(&self) -> (u64, f32, f64, u64, f32) {
        loop {
            let seq = self.seq.load(Ordering::Acquire);
            if seq & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let values = (
                seq,
                f32::from_bits(self.bpm.load(Ordering::Relaxed)),
                f64::from_bits(self.beat.load(Ordering::Relaxed)),
                self.ts.load(Ordering::Relaxed),
                f32::from_bits(self.confidence.load(Ordering::Relaxed)),
            );
            std::sync::atomic::fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == seq {
                return values;
            }
        }
    }
}

/// One analysed stereo stream.
pub struct Source {
    /// Signal prefix: bus name (`band`, `music`, …) or `mic`.
    pub prefix: String,
    pub samples: rtrb::Consumer<f32>,
    pub clock: rtrb::Consumer<BlockStamp>,
    pub is_mic: bool,
}

pub struct Setup {
    pub rate: u32,
    pub sources: Vec<Source>,
    /// `auto` or a prefix.
    pub beat_source: String,
    pub min_bpm: f32,
    pub max_bpm: f32,
    pub talk_threshold_db: f32,
    pub talk_hold_ms: f32,
}

pub enum Msg {
    Setup(Box<Setup>),
    Tap(u64),
    ClearTap,
    Bpm(f32),
    ResetSources,
    Quit,
}

/// Latest features per prefix (for the UI query), refreshed ~10 Hz.
pub type Latest = Arc<Mutex<BTreeMap<String, Value>>>;

struct Run {
    prefix: String,
    an: LiveAnalyzer,
    samples: rtrb::Consumer<f32>,
    clock: rtrb::Consumer<BlockStamp>,
    stamp: Option<BlockStamp>,
    read: u64,
    is_mic: bool,
    names: Names,
    hop: u64,
    talking: bool,
    talk_until: u64,
    /// Master ns of the latest hop above [`SILENCE`] (0 = none yet).
    last_audible_ts: u64,
    last_audio_ts: u64,
    audible: bool,
    /// Voice detector on the mic (`mic.talking` = voice and above the talk level).
    vad: Option<Box<se_analysis::vad::Vad>>,
}

struct Names {
    scalars: Vec<String>,
    bands: Vec<String>,
    kick: String,
    snare: String,
    hat: String,
    drop: String,
    section: String,
    hype: String,
}

const SCALARS: &[&str] = &["level", "peak", "lufs", "lufs_m", "bass", "mid", "high", "centroid", "kick", "snare", "hat", "flux", "novelty", "hype"];

impl Names {
    fn new(p: &str) -> Names {
        Names {
            scalars: SCALARS.iter().map(|s| format!("{p}.{s}")).collect(),
            bands: (0..N_BANDS).map(|i| format!("{p}.b.{i}")).collect(),
            kick: format!("{p}.kick"),
            snare: format!("{p}.snare"),
            hat: format!("{p}.hat"),
            drop: format!("{p}.drop"),
            section: format!("{p}.section"),
            hype: format!("{p}.hype"),
        }
    }
}

fn scalars(f: &Features) -> [f32; 14] {
    [f.level, f.peak, f.lufs_s, f.lufs_m, f.bass, f.mid, f.high, f.centroid, f.kick, f.snare, f.hat, f.flux, f.novelty, f.hype]
}

pub struct Handle {
    pub tx: std::sync::mpsc::Sender<Msg>,
    pub beat: Arc<BeatShared>,
    pub latest: Latest,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Quit);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

pub fn spawn(hub: Arc<Hub>) -> anyhow::Result<Handle> {
    let (tx, rx) = std::sync::mpsc::channel();
    let beat = Arc::new(BeatShared::default());
    let latest: Latest = Arc::new(Mutex::new(BTreeMap::new()));
    let (b, l) = (beat.clone(), latest.clone());
    let join = std::thread::Builder::new().name("se-audio-ana".into()).spawn(move || thread(hub, rx, b, l))?;
    Ok(Handle { tx, beat, latest, join: Some(join) })
}

fn features_value(f: &Features) -> Value {
    Value::map()
        .with("level", f.level as f64)
        .with("peak", f.peak as f64)
        .with("lufs", f.lufs_s as f64)
        .with("bass", f.bass as f64)
        .with("mid", f.mid as f64)
        .with("high", f.high as f64)
        .with("centroid_hz", f.centroid_hz as f64)
        .with("bpm", f.bpm as f64)
        .with("confidence", f.beat_confidence as f64)
        .with("hype", f.hype as f64)
        .with("bands", Value::List(f.bands.iter().map(|b| Value::Float(*b as f64)).collect()))
}

fn thread(hub: Arc<Hub>, rx: std::sync::mpsc::Receiver<Msg>, beat: Arc<BeatShared>, latest: Latest) {
    let mut runs: Vec<Run> = Vec::new();
    let mut setup_rate = 48000u32;
    let mut last_audio_map = Instant::now() - Duration::from_secs(1);
    let mut beat_pref = String::from("auto");
    let mut beat_src: Option<usize> = None;
    let mut beat_switch_since: Option<(usize, Instant)> = None;
    let mut talk_thr = se_dsp::db_to_gain(-42.0);
    let mut talk_hold_ns = 600_000_000u64;
    let mut l = vec![0.0f32; 4096];
    let mut r = vec![0.0f32; 4096];
    let mut inter = vec![0.0f32; 8192];
    let mut last_latest = Instant::now();
    let mut musical = MusicalClock::new(se_clock::now());
    let mut last_clock_publish = Instant::now() - Duration::from_secs(1);
    let mut event_beat = 0u64;
    loop {
        loop {
            match rx.try_recv() {
                Ok(Msg::Setup(s)) => {
                    setup_rate = s.rate;
                    beat_pref = s.beat_source.clone();
                    talk_thr = se_dsp::db_to_gain(s.talk_threshold_db);
                    talk_hold_ns = (s.talk_hold_ms as u64) * 1_000_000;
                    runs = s
                        .sources
                        .into_iter()
                        .map(|src| {
                            let cfg = LiveConfig { sample_rate: s.rate as f32, hop: 256, onsets: OnsetConfig::default(), beat: !src.is_mic, hype: src.is_mic };
                            let mut an = LiveAnalyzer::new(cfg);
                            if let Some(bt) = an.beat_mut() {
                                bt.set_range(s.min_bpm, s.max_bpm);
                            }
                            Run {
                                names: Names::new(&src.prefix),
                                prefix: src.prefix,
                                an,
                                samples: src.samples,
                                clock: src.clock,
                                stamp: None,
                                read: 0,
                                is_mic: src.is_mic,
                                hop: 0,
                                talking: false,
                                talk_until: 0,
                                last_audible_ts: 0,
                                last_audio_ts: 0,
                                audible: false,
                                vad: src.is_mic.then(|| Box::new(se_analysis::vad::Vad::new(s.rate as f32))),
                            }
                        })
                        .collect();
                    beat_src = None;
                    beat_switch_since = None;
                    musical.invalidate_source(se_clock::now());
                    latest.lock().clear();
                }
                Ok(Msg::Tap(ts)) => musical.tap(ts),
                Ok(Msg::Bpm(bpm)) => { musical.set_bpm(se_clock::now(), bpm); }
                Ok(Msg::ClearTap) => musical.clear_override(se_clock::now()),
                Ok(Msg::ResetSources) => {
                    musical.invalidate_source(se_clock::now());
                    beat_switch_since = None;
                    for run in &mut runs {
                        if let Some(bt) = run.an.beat_mut() { bt.reset(); }
                        run.last_audible_ts = 0;
                        run.last_audio_ts = 0;
                        run.audible = false;
                    }
                }
                Ok(Msg::Quit) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    musical.clear_override(se_clock::now());
                    let clock = musical.snapshot();
                    beat.store(clock.bpm, clock.position, clock.ts, 0.0);
                    hub.signals(vec![
                        ("beat.confidence".into(), 0.0), ("beat.locked".into(), 0.0),
                        ("beat.freewheel".into(), 1.0), ("beat.source".into(), 0.0),
                    ]);
                    latest.lock().insert("beat".into(), Value::map()
                        .with("source", "fallback").with("status", "stopped")
                        .with("locked", false).with("freewheel", true).with("confidence", 0.0)
                        .with("bpm", clock.bpm as f64).with("position", clock.position));
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }
        // pick the beat source
        let now = se_clock::now();
        // Confidence is usable only with recent audible analysis. Zero is the "never heard"
        // sentinel, not a master-clock observation.
        let live = |runs: &[Run], i: usize| runs[i].last_audible_ts != 0
            && now.saturating_sub(runs[i].last_audible_ts) <= FRESH_NS;
        let conf = |runs: &[Run], i: usize| if live(runs, i) { runs[i].an.beat().map_or(0.0, |b| b.confidence()) } else { 0.0 };
        let desired = if beat_pref == "auto" {
            // The backing song owns timing while audible, even during acquisition. Letting
            // the kit win on confidence would make lights follow the drummer instead of the song.
            runs.iter().enumerate().find(|(i, run)| !run.is_mic && run.prefix == "music" && live(&runs, *i))
                .map(|(i, _)| i)
                .or_else(|| {
                    let mut best: Option<(usize, f32)> = None;
                    for (i, run) in runs.iter().enumerate() {
                        if run.is_mic || !live(&runs, i) {
                            continue;
                        }
                        let c = conf(&runs, i);
                        if best.is_none_or(|(_, b)| c > b + 1e-6) {
                            best = Some((i, c));
                        }
                    }
                    best.map(|(i, _)| i)
                })
        } else {
            runs.iter().position(|r| r.prefix == beat_pref && !r.is_mic)
        };
        let previous_source = beat_src;
        match (beat_src, desired) {
            (None, d) => beat_src = d,
            (Some(_), None) => {
                beat_src = None;
                beat_switch_since = None;
            }
            (Some(cur), Some(d)) if cur != d => {
                // Song priority and an inactive source yield immediately. Other live buses
                // switch only after a clearly better confidence persists for two seconds.
                let immediate = cur >= runs.len()
                    || (beat_pref == "auto" && runs[d].prefix == "music" && live(&runs, d))
                    || (live(&runs, d) && !live(&runs, cur))
                    || (conf(&runs, d) > 0.0 && conf(&runs, cur) <= 0.0);
                if immediate {
                    beat_src = Some(d);
                    beat_switch_since = None;
                } else if conf(&runs, d) > conf(&runs, cur) + 0.15 {
                    if beat_switch_since.is_none_or(|(candidate, _)| candidate != d) {
                        beat_switch_since = Some((d, Instant::now()));
                    }
                    if beat_switch_since.is_some_and(|(_, since)| since.elapsed() > Duration::from_secs(2)) {
                        beat_src = Some(d);
                        beat_switch_since = None;
                    }
                } else {
                    beat_switch_since = None;
                }
            }
            _ => beat_switch_since = None,
        }
        if previous_source != beat_src {
            musical.invalidate_source(now);
        }

        let mut did = false;
        for (ri, run) in runs.iter_mut().enumerate() {
            loop {
                let avail = run.samples.slots() / 2;
                if avail == 0 {
                    break;
                }
                let n = avail.min(4096);
                let Ok(chunk) = run.samples.read_chunk(n * 2) else { break };
                let (a, b) = chunk.as_slices();
                inter[..a.len()].copy_from_slice(a);
                inter[a.len()..a.len() + b.len()].copy_from_slice(b);
                chunk.commit_all();
                for i in 0..n {
                    l[i] = inter[2 * i];
                    r[i] = inter[2 * i + 1];
                }
                // timestamp of the first frame of this chunk (stereo tap: stamps count
                // interleaved samples, `read` counts frames)
                while let Ok(s) = run.clock.peek().map(|s| BlockStamp { sample: s.sample / 2, ts: s.ts }) {
                    if s.sample <= run.read {
                        run.stamp = Some(s);
                        let _ = run.clock.pop();
                        // master ↔ audio-device clock mapping (§3.2), from the first tap, 1 Hz
                        if ri == 0 && last_audio_map.elapsed() >= Duration::from_secs(1) {
                            last_audio_map = Instant::now();
                            let device_ns = (s.sample as f64 * 1e9 / setup_rate as f64) as i128;
                            hub.clock.update(|m| m.audio.observe(s.ts, device_ns));
                        }
                    } else {
                        break;
                    }
                }
                let ts = match run.stamp {
                    Some(s) => s.ts + ((run.read - s.sample) as f64 * 1e9 / setup_rate as f64) as u64,
                    None => se_clock::now(),
                };
                run.read += n as u64;
                did = true;
                if se_clock::now().saturating_sub(run.last_audio_ts) > FRESH_NS {
                    if let Some(bt) = run.an.beat_mut() { bt.reset(); }
                    run.audible = false;
                }
                let voice = run.vad.as_mut().map(|v| {
                    v.push(&l[..n], &r[..n]);
                    v.score()
                });
                let names = &run.names;
                let hub2 = &hub;
                let hop = &mut run.hop;
                let is_mic = run.is_mic;
                let talking = &mut run.talking;
                let talk_until = &mut run.talk_until;
                let last_audible_ts = &mut run.last_audible_ts;
                let last_audio_ts = &mut run.last_audio_ts;
                let mut out = |f: &Features, events: &[AnalysisEvent]| {
                    *hop += 1;
                    if f.level > SILENCE {
                        *last_audible_ts = f.ts;
                    }
                    *last_audio_ts = f.ts;
                    let mut sig: Vec<(String, f32)> = Vec::with_capacity(20 + N_BANDS);
                    for (n, v) in names.scalars.iter().zip(scalars(f)) {
                        sig.push((n.clone(), if v.is_finite() { v } else { 0.0 }));
                    }
                    if *hop % 2 == 0 {
                        for (n, v) in names.bands.iter().zip(f.bands.iter()) {
                            sig.push((n.clone(), *v));
                        }
                    }
                    if is_mic {
                        // speech (not drums, bleed or room noise) at talking level
                        let speech = voice.is_none_or(|v| v >= se_analysis::vad::VAD_THRESHOLD);
                        if speech && f.level > talk_thr {
                            *talk_until = f.ts + talk_hold_ns;
                        }
                        if let Some(v) = voice {
                            sig.push(("mic.voice".into(), v));
                        }
                        let now_talking = f.ts < *talk_until;
                        if now_talking != *talking {
                            *talking = now_talking;
                        }
                        sig.push(("mic.talking".into(), if *talking { 1.0 } else { 0.0 }));
                    }
                    hub2.signals(sig);
                    for e in events {
                        match *e {
                            AnalysisEvent::Onset { class, strength, ts } => {
                                let ty = match class {
                                    se_analysis::OnsetClass::Kick => &names.kick,
                                    se_analysis::OnsetClass::Snare => &names.snare,
                                    se_analysis::OnsetClass::Hat => &names.hat,
                                };
                                let mut ev = Event::new(ty.clone(), Origin::System, Value::map().with("velocity", strength as f64));
                                ev.ts = ts;
                                hub2.emit(ev);
                            }
                            AnalysisEvent::Beat(_) => {}
                            AnalysisEvent::Drop { strength, ts } => {
                                let mut ev = Event::new(names.drop.clone(), Origin::System, Value::map().with("strength", strength as f64));
                                ev.ts = ts;
                                hub2.emit(ev);
                            }
                            AnalysisEvent::Section { novelty, ts } => {
                                let mut ev = Event::new(names.section.clone(), Origin::System, Value::map().with("novelty", novelty as f64));
                                ev.ts = ts;
                                hub2.emit(ev);
                            }
                            AnalysisEvent::HypeSpike { level, ts } => {
                                let mut ev = Event::new(names.hype.clone(), Origin::System, Value::map().with("level", level as f64));
                                ev.ts = ts;
                                hub2.emit(ev);
                            }
                        }
                    }
                };
                run.an.process(&l[..n], &r[..n], ts, &mut out);
                let audible = run.last_audible_ts != 0 && run.last_audio_ts.saturating_sub(run.last_audible_ts) <= FRESH_NS;
                if run.audible && !audible {
                    if let Some(bt) = run.an.beat_mut() { bt.reset(); }
                }
                run.audible = audible;
            }
        }
        if last_clock_publish.elapsed() >= Duration::from_millis(10) {
            last_clock_publish = Instant::now();
            let now = se_clock::now();
            if let Some(run) = beat_src.and_then(|i| runs.get(i))
                && run.last_audible_ts != 0
                && now.saturating_sub(run.last_audible_ts) <= FRESH_NS
                && let Some(bt) = run.an.beat()
            {
                musical.observe(Observation { ts: now, bpm: bt.bpm(), phase: bt.phase_at(now), confidence: bt.confidence() });
            } else {
                musical.invalidate_source(now);
            }
            let clock = musical.advance(now);
            beat.store(clock.bpm, clock.position, clock.ts, clock.confidence);
            hub.signals(vec![
                ("beat.bpm".into(), clock.bpm),
                ("beat.phase".into(), clock.phase),
                ("beat.position".into(), clock.position as f32),
                ("beat.confidence".into(), clock.confidence),
                ("beat.locked".into(), if clock.locked { 1.0 } else { 0.0 }),
                ("beat.freewheel".into(), if clock.locked { 0.0 } else { 1.0 }),
                ("beat.source".into(), clock.source.code()),
            ]);
            let index = clock.position.floor() as u64;
            if index > event_beat {
                event_beat = index;
                let source = if clock.source == ClockSource::Live {
                    beat_src.and_then(|i| runs.get(i)).map_or("live", |r| r.prefix.as_str())
                } else {
                    clock.source.name()
                };
                let mut ev = Event::new("beat", Origin::System, Value::map()
                    .with("index", index as i64).with("bpm", clock.bpm as f64)
                    .with("downbeat", index % 4 == 0).with("source", source));
                ev.ts = now;
                hub.emit(ev);
            }
        }
        if last_latest.elapsed() > Duration::from_millis(100) {
            last_latest = Instant::now();
            let mut lt = latest.lock();
            for run in &runs {
                if let Some(f) = run.an.last_features() {
                    lt.insert(run.prefix.clone(), features_value(f));
                }
            }
            let clock = musical.snapshot();
            let live_source = beat_src.and_then(|i| runs.get(i));
            lt.insert("beat".into(), Value::map()
                .with("source", clock.source.name())
                .with("bus", live_source.map_or("", |r| r.prefix.as_str()))
                .with("age_ms", live_source.filter(|r| r.last_audio_ts != 0)
                    .map(|r| Value::Float(se_clock::now().saturating_sub(r.last_audio_ts) as f64 * 1e-6))
                    .unwrap_or(Value::Null))
                .with("status", if clock.locked { "locked" } else { "freewheel" })
                .with("locked", clock.locked).with("freewheel", !clock.locked)
                .with("bpm", clock.bpm as f64).with("position", clock.position)
                .with("phase", clock.phase as f64).with("confidence", clock.confidence as f64));
        }
        if !did {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
