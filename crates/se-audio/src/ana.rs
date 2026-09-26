//! Live analysis thread (§8.3): reads the analysis taps (bus input sums and the mic input)
//! from lock-free rings, runs `se_analysis::LiveAnalyzer` per source, and publishes
//! `<bus>.*` signals, onset/drop/section events, the global beat clock (`beat.bpm`,
//! `beat.phase`, `beat` events, tap tempo), and `mic.*` (level, hype, `mic.talking`).

use crate::taps::BlockStamp;
use parking_lot::Mutex;
use se_analysis::{AnalysisEvent, Features, LiveAnalyzer, LiveConfig, N_BANDS, OnsetConfig};
use se_hub::Hub;
use se_proto::{Event, Origin, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// RMS below which a bus is treated as silent (≈ −50 dBFS): no beat clock from it.
const SILENCE: f32 = 0.003;

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
        self.bpm.store(bpm.to_bits(), Ordering::Relaxed);
        self.beat.store(beat.to_bits(), Ordering::Relaxed);
        self.ts.store(ts, Ordering::Relaxed);
        self.confidence.store(conf.to_bits(), Ordering::Relaxed);
        self.seq.fetch_add(1, Ordering::Release);
    }

    /// (seq, bpm, beat, ts, confidence)
    pub fn load(&self) -> (u64, f32, f64, u64, f32) {
        let seq = self.seq.load(Ordering::Acquire);
        (
            seq,
            f32::from_bits(self.bpm.load(Ordering::Relaxed)),
            f64::from_bits(self.beat.load(Ordering::Relaxed)),
            self.ts.load(Ordering::Relaxed),
            f32::from_bits(self.confidence.load(Ordering::Relaxed)),
        )
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
    last_level: f32,
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
    let mut beat_pref = String::from("auto");
    let mut beat_src: Option<usize> = None;
    let mut beat_switch_since: Option<Instant> = None;
    let mut talk_thr = se_dsp::db_to_gain(-42.0);
    let mut talk_hold_ns = 600_000_000u64;
    let mut l = vec![0.0f32; 4096];
    let mut r = vec![0.0f32; 4096];
    let mut inter = vec![0.0f32; 8192];
    let mut last_latest = Instant::now();
    let mut pending_taps: Vec<u64> = Vec::new();
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
                                last_level: 0.0,
                                vad: src.is_mic.then(|| Box::new(se_analysis::vad::Vad::new(s.rate as f32))),
                            }
                        })
                        .collect();
                    beat_src = None;
                    latest.lock().clear();
                }
                Ok(Msg::Tap(ts)) => pending_taps.push(ts),
                Ok(Msg::ClearTap) => {
                    for run in &mut runs {
                        run.an.clear_tap();
                    }
                }
                Ok(Msg::Quit) | Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }
        for ts in pending_taps.drain(..) {
            for run in &mut runs {
                run.an.tap(ts);
            }
        }
        // pick the beat source
        let conf = |runs: &[Run], i: usize| runs[i].an.beat().map(|b| b.confidence()).unwrap_or(0.0) * if runs[i].last_level > 0.003 { 1.0 } else { 0.0 };
        let desired = if beat_pref == "auto" {
            let mut best: Option<(usize, f32)> = None;
            for (i, run) in runs.iter().enumerate() {
                if run.is_mic {
                    continue;
                }
                let c = conf(&runs, i);
                if best.is_none_or(|(_, b)| c > b + 1e-6) {
                    best = Some((i, c));
                }
            }
            best.map(|(i, _)| i)
        } else {
            runs.iter().position(|r| r.prefix == beat_pref)
        };
        match (beat_src, desired) {
            (None, d) => beat_src = d,
            (Some(cur), Some(d)) if cur != d => {
                // hysteresis: switch only after the other source is clearly better for 2 s
                if cur >= runs.len() || conf(&runs, d) > conf(&runs, cur) + 0.15 {
                    let since = *beat_switch_since.get_or_insert_with(Instant::now);
                    if cur >= runs.len() || since.elapsed() > Duration::from_secs(2) {
                        beat_src = Some(d);
                        beat_switch_since = None;
                    }
                } else {
                    beat_switch_since = None;
                }
            }
            _ => beat_switch_since = None,
        }

        let mut did = false;
        for (ri, run) in runs.iter_mut().enumerate() {
            let is_beat = beat_src == Some(ri);
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
                // timestamp of the first sample of this chunk
                while let Ok(s) = run.clock.peek().copied() {
                    if s.sample <= run.read {
                        run.stamp = Some(s);
                        let _ = run.clock.pop();
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
                let last_level = &mut run.last_level;
                let mut out = |f: &Features, events: &[AnalysisEvent]| {
                    *hop += 1;
                    *last_level = f.level;
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
                    // no beat clock from silence/noise floor
                    let beat_live = is_beat && f.level > SILENCE;
                    if beat_live {
                        sig.push(("beat.bpm".into(), f.bpm));
                        sig.push(("beat.phase".into(), f.phase));
                        sig.push(("beat.confidence".into(), f.beat_confidence));
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
                            AnalysisEvent::Beat(b) if beat_live => {
                                let mut ev = Event::new(
                                    "beat",
                                    Origin::System,
                                    Value::map()
                                        .with("index", b.index as i64)
                                        .with("bpm", b.bpm as f64)
                                        .with("downbeat", b.downbeat)
                                        .with("source", names.scalars[0].trim_end_matches(".level").to_string()),
                                );
                                ev.ts = b.ts;
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
                if is_beat
                    && run.last_level > SILENCE
                    && let Some(bt) = run.an.beat()
                {
                    let now = se_clock::now();
                    beat.store(bt.bpm(), bt.beat_at(now), now, bt.confidence());
                }
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
            if let Some(i) = beat_src
                && let Some(run) = runs.get(i)
            {
                lt.insert("beat".into(), Value::map().with("source", run.prefix.clone()));
            }
        }
        if !did {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
