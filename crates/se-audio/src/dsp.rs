//! Real-time building blocks used by the graph: A/V delay with crossfaded delay changes,
//! the adaptive-rate slot reader (jitter buffer for producers on other clocks), meters.

use se_dsp::{MAX_BLOCK, Smoother};
use std::sync::atomic::{AtomicU32, Ordering};

/// Stereo delay line for A/V alignment. Changing the delay crossfades between the old and
/// new read positions (no clicks, no pitch glide).
pub struct AvDelay {
    l: Vec<f32>,
    r: Vec<f32>,
    pos: usize,
    cur: usize,
    next: usize,
    xf_left: u32,
    xf_len: u32,
}

impl AvDelay {
    pub fn new(max_samples: usize, xfade: u32) -> AvDelay {
        let cap = (max_samples + MAX_BLOCK + 1).next_power_of_two();
        AvDelay { l: vec![0.0; cap], r: vec![0.0; cap], pos: 0, cur: 0, next: 0, xf_left: 0, xf_len: xfade.max(1) }
    }

    pub fn max(&self) -> usize {
        self.l.len() - MAX_BLOCK - 1
    }

    pub fn set(&mut self, samples: usize) {
        let s = samples.min(self.max());
        if s == self.next {
            return;
        }
        if self.xf_left > 0 {
            // finish the running fade instantly at its destination, then start the new one
            self.cur = self.next;
        }
        self.next = s;
        self.xf_left = self.xf_len;
    }

    pub fn delay(&self) -> usize {
        self.next
    }

    pub fn clear(&mut self) {
        self.l.fill(0.0);
        self.r.fill(0.0);
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        if self.cur == 0 && self.next == 0 && self.xf_left == 0 {
            // keep the history warm so a later delay change has real audio to read
            let mask = self.l.len() - 1;
            for i in 0..l.len() {
                self.l[self.pos] = l[i];
                self.r[self.pos] = r[i];
                self.pos = (self.pos + 1) & mask;
            }
            return;
        }
        let mask = self.l.len() - 1;
        let len = self.l.len();
        for i in 0..l.len() {
            self.l[self.pos] = l[i];
            self.r[self.pos] = r[i];
            let a = (self.pos + len - self.cur) & mask;
            if self.xf_left > 0 {
                let b = (self.pos + len - self.next) & mask;
                let t = 1.0 - self.xf_left as f32 / self.xf_len as f32;
                l[i] = self.l[a] + (self.l[b] - self.l[a]) * t;
                r[i] = self.r[a] + (self.r[b] - self.r[a]) * t;
                self.xf_left -= 1;
                if self.xf_left == 0 {
                    self.cur = self.next;
                }
            } else {
                l[i] = self.l[a];
                r[i] = self.r[a];
            }
            self.pos = (self.pos + 1) & mask;
        }
    }
}

/// Reads an interleaved producer ring (a `hub.audio` slot) at the graph rate. The producer
/// runs on another clock (CEF, TTS, file decoders), so the reader keeps the ring near a
/// target fill with a PI-controlled resampling ratio (±0.5 %, inaudible) and cubic
/// interpolation. Underruns fade out and re-prime; nothing here allocates.
pub struct SlotReader {
    pub consumer: rtrb::Consumer<f32>,
    channels: usize,
    nominal: f64,
    hist_l: [f32; 4],
    hist_r: [f32; 4],
    frac: f64,
    primed: bool,
    target: usize,
    integ: f64,
    fade: Smoother,
    pub underruns: u32,
}

impl SlotReader {
    /// `target_ms` of buffering is kept in the ring (jitter margin).
    pub fn new(consumer: rtrb::Consumer<f32>, channels: u16, src_rate: u32, graph_rate: u32, target_ms: f32) -> SlotReader {
        let channels = channels.max(1) as usize;
        let cap_frames = consumer.buffer().capacity() / channels;
        let target = ((target_ms * 0.001 * src_rate as f32) as usize).clamp(64, (cap_frames / 3).max(64));
        SlotReader {
            consumer,
            channels,
            nominal: src_rate as f64 / graph_rate as f64,
            hist_l: [0.0; 4],
            hist_r: [0.0; 4],
            frac: 0.0,
            primed: false,
            target,
            integ: 0.0,
            fade: Smoother::new(0.0, 64),
            underruns: 0,
        }
    }

    pub fn fill_frames(&self) -> usize {
        self.consumer.slots() / self.channels
    }

    pub fn is_playing(&self) -> bool {
        self.primed
    }

    fn pop_frame(&mut self) -> Option<(f32, f32)> {
        if self.consumer.slots() < self.channels {
            return None;
        }
        let l = self.consumer.pop().unwrap_or(0.0);
        let r = if self.channels >= 2 { self.consumer.pop().unwrap_or(0.0) } else { l };
        for _ in 2..self.channels {
            let _ = self.consumer.pop();
        }
        Some((l, r))
    }

    /// Produce `l.len()` frames into `l`/`r` (overwrites).
    pub fn read(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len();
        let fill = self.fill_frames();
        if !self.primed {
            if fill >= self.target {
                self.primed = true;
                self.integ = 0.0;
                self.fade.set(1.0);
                // start the interpolator on the first frame (no step from stale history)
                if let Some((a, b)) = self.pop_frame() {
                    self.hist_l = [a; 4];
                    self.hist_r = [b; 4];
                }
            } else if self.fade.settled() && self.fade.value() == 0.0 {
                l.fill(0.0);
                r.fill(0.0);
                return;
            }
            // else: still fading out after an underrun (holding the last frame)
        }
        let err = (fill as f64 - self.target as f64) / self.target as f64;
        self.integ = (self.integ + err * n as f64 * 1e-6).clamp(-0.004, 0.004);
        let corr = (err * 0.002 + self.integ).clamp(-0.005, 0.005);
        let ratio = self.nominal * (1.0 + corr);
        for i in 0..n {
            self.frac += ratio;
            while self.frac >= 1.0 {
                self.frac -= 1.0;
                match self.pop_frame() {
                    Some((a, b)) => {
                        self.hist_l = [self.hist_l[1], self.hist_l[2], self.hist_l[3], a];
                        self.hist_r = [self.hist_r[1], self.hist_r[2], self.hist_r[3], b];
                    }
                    None => {
                        if self.primed {
                            self.primed = false;
                            self.underruns += 1;
                            self.fade.set(0.0);
                        }
                        // hold the last frame while fading out
                        self.hist_l = [self.hist_l[1], self.hist_l[2], self.hist_l[3], self.hist_l[3]];
                        self.hist_r = [self.hist_r[1], self.hist_r[2], self.hist_r[3], self.hist_r[3]];
                    }
                }
            }
            let t = self.frac as f32;
            let g = self.fade.next();
            l[i] = hermite(&self.hist_l, t) * g;
            r[i] = hermite(&self.hist_r, t) * g;
        }
    }
}

/// 4-point, 3rd-order Hermite interpolation between `h[1]` and `h[2]`.
#[inline]
fn hermite(h: &[f32; 4], t: f32) -> f32 {
    let c0 = h[1];
    let c1 = 0.5 * (h[2] - h[0]);
    let c2 = h[0] - 2.5 * h[1] + 2.0 * h[2] - 0.5 * h[3];
    let c3 = 0.5 * (h[3] - h[0]) + 1.5 * (h[1] - h[2]);
    ((c3 * t + c2) * t + c1) * t + c0
}

/// Lock-free meter slot: RT writes peak (max) and mean square; readers take and reset.
#[derive(Default)]
pub struct Meter {
    peak: AtomicU32,
    ms: AtomicU32,
}

impl Meter {
    /// Record one block (RT).
    #[inline]
    pub fn record(&self, l: &[f32], r: &[f32]) {
        let mut pk = 0.0f32;
        let mut sq = 0.0f32;
        for i in 0..l.len() {
            let a = l[i].abs().max(r[i].abs());
            pk = pk.max(a);
            sq += 0.5 * (l[i] * l[i] + r[i] * r[i]);
        }
        let ms = if l.is_empty() { 0.0 } else { sq / l.len() as f32 };
        if pk.is_finite() {
            // non-negative f32 bit patterns order like the floats
            self.peak.fetch_max(pk.to_bits(), Ordering::Relaxed);
        }
        let prev = f32::from_bits(self.ms.load(Ordering::Relaxed));
        // smooth mean square towards the block value (~50 ms at 256-frame blocks)
        let next = prev + (ms - prev) * 0.2;
        self.ms.store(if next.is_finite() { next } else { 0.0 }.to_bits(), Ordering::Relaxed);
    }

    /// (rms, peak since last take), linear.
    pub fn take(&self) -> (f32, f32) {
        let pk = f32::from_bits(self.peak.swap(0, Ordering::Relaxed));
        let ms = f32::from_bits(self.ms.load(Ordering::Relaxed));
        (ms.max(0.0).sqrt(), pk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn av_delay_delays_and_crossfades_changes() {
        let mut d = AvDelay::new(4800, 480);
        d.set(100);
        let input: Vec<f32> = (0..4096).map(|i| i as f32).collect();
        let mut l = input.clone();
        let mut r = input.clone();
        for (cl, cr) in l.chunks_mut(256).zip(r.chunks_mut(256)) {
            d.process(cl, cr);
        }
        // after the initial 480-sample fade from 0 to 100 samples of delay
        for i in 1000..4096 {
            assert_eq!(l[i], input[i - 100]);
        }
        // change: output stays continuous (ramp input: slope 1/sample; crossfade adds < 1)
        d.set(300);
        let mut l2: Vec<f32> = (4096..8192).map(|i| i as f32).collect();
        let mut r2 = l2.clone();
        for (cl, cr) in l2.chunks_mut(256).zip(r2.chunks_mut(256)) {
            d.process(cl, cr);
        }
        let jumps = l2.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(jumps <= 1.0 + 200.0 / 480.0 + 1e-3, "jump {jumps}");
        assert_eq!(l2[4095], (8191 - 300) as f32);
    }

    #[test]
    fn slot_reader_tracks_a_faster_producer_without_overflow() {
        // producer 0.3 % fast: the reader must speed up and keep the fill near target
        let (mut p, c) = rtrb::RingBuffer::new(48000 * 2);
        let mut rd = SlotReader::new(c, 2, 48000, 48000, 30.0);
        let mut phase = 0.0f64;
        let mut out_l = vec![0.0; 256];
        let mut out_r = vec![0.0; 256];
        let mut produced = 0.0f64;
        for _block in 0..6000 {
            produced += 256.0 * 1.003;
            while produced >= 1.0 {
                produced -= 1.0;
                let s = (phase * std::f64::consts::TAU).sin() as f32 * 0.5;
                phase += 440.0 / 48000.0;
                let _ = p.push(s);
                let _ = p.push(s);
            }
            rd.read(&mut out_l, &mut out_r);
        }
        let fill = rd.fill_frames() as f64;
        assert!(rd.is_playing());
        assert!((fill - 1440.0).abs() < 600.0, "fill {fill}");
        assert_eq!(rd.underruns, 0);
        // output is a clean 440 Hz sine: bounded second difference
        let d2 = out_l.windows(3).map(|w| (w[2] - 2.0 * w[1] + w[0]).abs()).fold(0.0, f32::max);
        let expected = 0.5 * (std::f32::consts::TAU * 440.0 / 48000.0).powi(2);
        assert!(d2 < expected * 1.5, "d2 {d2} vs {expected}");
    }

    #[test]
    fn slot_reader_fades_out_on_underrun_and_reprimes() {
        let (mut p, c) = rtrb::RingBuffer::new(9600);
        let mut rd = SlotReader::new(c, 1, 48000, 48000, 10.0);
        for _ in 0..1000 {
            p.push(0.5).unwrap();
        }
        let mut l = vec![0.0; 256];
        let mut r = vec![0.0; 256];
        let mut all = Vec::new();
        for _ in 0..6 {
            rd.read(&mut l, &mut r);
            all.extend_from_slice(&l);
        }
        assert_eq!(rd.underruns, 1);
        assert!(!rd.is_playing());
        let max_step = all.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(max_step < 0.5 / 32.0, "fade-in/out must be ramped: {max_step}");
        assert!(all.last().unwrap().abs() < 1e-6);
    }

    #[test]
    fn meter_peak_and_rms() {
        let m = Meter::default();
        let l = vec![0.5f32; 256];
        for _ in 0..50 {
            m.record(&l, &l);
        }
        let (rms, pk) = m.take();
        assert!((rms - 0.5).abs() < 1e-3 && pk == 0.5);
        assert_eq!(m.take().1, 0.0, "peak resets on take");
    }
}
