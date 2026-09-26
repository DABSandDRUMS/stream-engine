//! LTC in/out through the Audio slice: an input tap (`se_audio_taps`) decoded into
//! replayable observations, and a generator writing the `timecode.ltc` audio slot (routed by
//! se-audio to a hardware output channel, never into program/OBS buses).

use crate::Health;
use crate::follow::Follower;
use se_clock::timecode::ltc::{LtcDecoder, LtcEncoder, LtcFrame};
use se_clock::timecode::{FrameRate, ObsKind, ObsThrottle, TcObs, Timecode};
use se_core::Input;
use se_hub::{Hub, Snapshot};
use se_proto::Ts;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Audio slot name for LTC out (se-audio routes `timecode.*` slots directly to hardware).
pub const SLOT: &str = "timecode.ltc";
pub const OUT_RATE: u32 = 48_000;

/// Decodes a mono input stream into observations; `stamp` maps stream samples to master
/// time (the tap's per-block clock).
pub struct LtcIn {
    dec: LtcDecoder,
    thr: ObsThrottle,
    stamps: VecDeque<(u64, Ts)>,
    rate: u32,
}

impl LtcIn {
    pub fn new(sample_rate: u32, rate_hint: FrameRate) -> Self {
        LtcIn { dec: LtcDecoder::new(sample_rate, rate_hint), thr: ObsThrottle::new(60_000_000, 0.02), stamps: VecDeque::with_capacity(512), rate: sample_rate }
    }

    /// Stream sample `sample` was captured at master time `ts`.
    pub fn stamp(&mut self, sample: u64, ts: Ts) {
        if self.stamps.len() == 512 {
            self.stamps.pop_front();
        }
        self.stamps.push_back((sample, ts));
    }

    fn ts_of(&self, s: f64) -> Option<Ts> {
        let (s0, t0) = self.stamps.iter().rev().find(|(x, _)| (*x as f64) <= s).or_else(|| self.stamps.front())?;
        let dt = (s - *s0 as f64) * 1e9 / self.rate as f64;
        Some((*t0 as f64 + dt).max(0.0) as Ts)
    }

    pub fn feed(&mut self, samples: &[f32], mut emit: impl FnMut(TcObs)) {
        let mut frames = Vec::new();
        self.dec.feed(samples, |f| frames.push(f));
        for f in frames {
            let Some(ts) = self.ts_of(f.position_sample()) else { continue };
            let o = TcObs { seconds: f.frame.tc.to_seconds(), ts, kind: ObsKind::Run, rate: Some(f.frame.tc.rate) };
            if self.thr.pass(&o) {
                emit(o);
            }
        }
    }

    pub fn quiet(&mut self) {
        self.thr.reset();
    }
}

pub fn spawn_input(
    hub: Arc<Hub>,
    health: Arc<Health>,
    key: String,
    tap_name: String,
    rate_hint: FrameRate,
    stop: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name(format!("se-ltc-in-{key}")).spawn(move || {
        let hkey = format!("ltc in `{tap_name}`");
        let mut tap = se_audio_taps::open(&tap_name, 1.0);
        let mut input = LtcIn::new(tap.rate, rate_hint);
        let mut buf = vec![0.0f32; 4096];
        let mut last_audio = se_clock::now();
        let mut last_frame: Option<Ts> = None;
        let mut state = "";
        health.set(&hkey, "warn", "waiting for audio from the tap".into());
        while !stop.load(Ordering::Relaxed) {
            while let Ok(s) = tap.clock.pop() {
                input.stamp(s.sample, s.ts);
            }
            let n = tap.samples.slots().min(buf.len());
            if n == 0 {
                std::thread::sleep(Duration::from_millis(5));
                let now = se_clock::now();
                if now.saturating_sub(last_audio) > 3_000_000_000 && state != "noaudio" {
                    state = "noaudio";
                    health.set(&hkey, "warn", "no audio on the tap (is the input configured in [audio.inputs]?)".into());
                }
                continue;
            }
            last_audio = se_clock::now();
            if let Ok(chunk) = tap.samples.read_chunk(n) {
                let (a, b) = chunk.as_slices();
                buf[..a.len()].copy_from_slice(a);
                buf[a.len()..a.len() + b.len()].copy_from_slice(b);
                chunk.commit_all();
            }
            let mut got = false;
            input.feed(&buf[..n], |obs| {
                got = true;
                hub.submit(Input::Timecode { source: key.clone(), obs });
            });
            let now = se_clock::now();
            if got {
                last_frame = Some(now);
                if state != "ok" {
                    state = "ok";
                    health.set(&hkey, "pass", format!("decoding at {} fps", input.dec.rate()));
                }
            } else if last_frame.is_none_or(|t| now.saturating_sub(t) > 500_000_000) {
                input.quiet();
                if state != "silent" && last_frame.is_none_or(|t| now.saturating_sub(t) > 3_000_000_000) {
                    state = "silent";
                    health.set(&hkey, "pass", "audio present, no timecode".into());
                }
            }
        }
        health.clear(&hkey);
    })
}

/// Generates LTC for one timeline into a ring, keeping `lead` seconds buffered and placing
/// each frame at the time it will be heard.
pub struct LtcOut {
    enc: LtcEncoder,
    follower: Follower,
    rate: FrameRate,
    sr: f64,
    buf: Vec<f32>,
    /// Frame count of the last frame written (consecutive while running).
    last: Option<u64>,
    /// Seconds between a sample entering the ring's read side and reaching the jack.
    pub latency: f64,
    /// Target fill of the ring in seconds.
    pub lead: f64,
}

impl LtcOut {
    pub fn new(timeline: &str, rate: FrameRate, amplitude: f32, latency: f64) -> Self {
        LtcOut {
            enc: LtcEncoder::new(OUT_RATE, amplitude),
            follower: Follower::new(timeline),
            rate,
            sr: OUT_RATE as f64,
            buf: Vec::with_capacity(4096),
            last: None,
            latency,
            lead: 0.06,
        }
    }

    /// Top up the ring. `buffered` = samples queued but not yet played; `push` writes samples.
    pub fn fill(&mut self, snap: &Snapshot, now: Ts, mut buffered: usize, free: usize, mut push: impl FnMut(&[f32])) {
        self.follower.update(snap);
        self.follower.tick(now);
        let fps = self.rate.fps();
        let frame_len = (self.sr / fps).ceil() as usize + 2;
        let target = (self.lead * self.sr) as usize;
        let mut free = free;
        while buffered < target && free >= frame_len {
            self.buf.clear();
            let t_play = now + ((buffered as f64 / self.sr + self.latency) * 1e9) as Ts;
            if !self.follower.running() {
                // stopped: silence (receivers hold their last frame)
                self.last = None;
                self.enc.silence((self.sr / fps) as usize, &mut self.buf);
            } else {
                let speed = self.follower.speed().clamp(0.5, 2.0);
                let p = self.follower.position(t_play) * fps;
                let n = match self.last {
                    Some(l) if (p - (l + 1) as f64).abs() < 0.5 => l + 1,
                    _ => {
                        // (re)sync: pad to the next frame boundary, then start there
                        let frac = p - p.floor();
                        if frac > 0.02 && self.last.is_none() {
                            let pad = ((1.0 - frac) / (fps * speed) * self.sr) as usize;
                            if pad > 0 {
                                self.enc.silence(pad, &mut self.buf);
                                push(&self.buf);
                                buffered += self.buf.len();
                                free = free.saturating_sub(self.buf.len());
                                self.last = Some(p.floor() as u64);
                                continue;
                            }
                        }
                        p.round().max(0.0) as u64
                    }
                };
                // servo the frame boundary onto the source position (±2 % max)
                let err = p - n as f64;
                let corr = (err * 0.1).clamp(-0.02, 0.02);
                let frame = LtcFrame::new(Timecode::from_frames(n, self.rate));
                self.enc.encode_frame(&frame, fps * speed * (1.0 + corr), &mut self.buf);
                self.last = Some(n);
            }
            push(&self.buf);
            buffered += self.buf.len();
            free = free.saturating_sub(self.buf.len());
        }
    }
}

pub fn spawn_output(
    hub: Arc<Hub>,
    health: Arc<Health>,
    timeline: String,
    rate: FrameRate,
    level_db: f64,
    latency: f64,
    stop: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name(format!("se-ltc-out-{timeline}")).spawn(move || {
        let hkey = format!("ltc out `{timeline}`");
        let mut prod = hub.audio.register(SLOT, 1, OUT_RATE, 0.5);
        let cap = prod.buffer().capacity();
        let amp = 10f64.powf(level_db / 20.0).clamp(0.0, 1.0) as f32;
        let mut out = LtcOut::new(&timeline, rate, amp, latency);
        health.set(&hkey, "pass", format!("{rate} fps on audio slot `{SLOT}` at {level_db:.0} dBFS"));
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
            let free = prod.slots();
            let snap = hub.snapshot.load();
            out.fill(&snap, se_clock::now(), cap - free, free, |s| {
                if let Ok(mut chunk) = prod.write_chunk_uninit(s.len()) {
                    let (a, b) = chunk.as_mut_slices();
                    for (d, v) in a.iter_mut().chain(b.iter_mut()).zip(s) {
                        d.write(*v);
                    }
                    // SAFETY: every slot of the chunk was written above.
                    unsafe { chunk.commit_all() };
                }
            });
        }
        health.clear(&hkey);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::follow::snapshot_for;

    #[test]
    fn ltc_in_maps_frames_to_master_time() {
        let r = FrameRate::Fps25;
        let mut enc = LtcEncoder::new(48_000, 0.4);
        let mut audio = Vec::new();
        for i in 0..50 {
            enc.encode_frame(&LtcFrame::new(Timecode::from_frames(90_000 + i, r)), 25.0, &mut audio);
        }
        let mut input = LtcIn::new(48_000, r);
        let t0: Ts = 5_000_000_000;
        let mut obs = Vec::new();
        for (k, block) in audio.chunks(256).enumerate() {
            input.stamp(k as u64 * 256, t0 + (k as u64 * 256) * 1_000_000_000 / 48_000);
            input.feed(block, |o| obs.push(o));
        }
        assert!(obs.len() >= 10, "{}", obs.len());
        for o in &obs {
            // frame n of the stream starts at sample n × 1920
            let n = (o.seconds * 25.0).round() - 90_000.0;
            let expect = t0 as f64 + n * 1920.0 * 1e9 / 48_000.0;
            assert!((o.ts as f64 - expect).abs() < 100_000.0, "{} vs {expect}", o.ts);
            assert_eq!(o.rate, Some(r));
        }
    }

    #[test]
    fn ltc_out_is_continuous_and_aligned() {
        let ms = 1_000_000u64;
        let mut out = LtcOut::new("show", FrameRate::Fps30, 0.5, 0.0);
        let mut ring: Vec<f32> = Vec::new();
        let mut played = 0usize;
        // 3 s of simulated time, 5 ms polls, timeline starting at 10 s
        for step in 0..600u64 {
            let now = step * 5 * ms;
            played = ((now as f64 / 1e9) * 48_000.0) as usize;
            let snap = snapshot_for("show", step, now, 10.0 + now as f64 / 1e9, true);
            let buffered = ring.len().saturating_sub(played);
            out.fill(&snap, now, buffered, 24_000usize.saturating_sub(buffered), |s| ring.extend_from_slice(s));
        }
        let mut dec = LtcDecoder::new(48_000, FrameRate::Fps30);
        let mut got = Vec::new();
        dec.feed(&ring[..played], |f| got.push(f));
        assert!(got.len() > 80, "{}", got.len());
        for w in got.windows(2) {
            assert_eq!(w[1].frame.tc.to_frames(), w[0].frame.tc.to_frames() + 1, "consecutive");
        }
        // each frame starts when the timeline is at that frame
        for g in got.iter().skip(5) {
            let t_audio = g.start / 48_000.0;
            let t_tl = 10.0 + t_audio;
            assert!((g.frame.tc.to_seconds() - t_tl).abs() < 0.004, "{} at {t_tl}", g.frame.tc);
        }
    }
}
