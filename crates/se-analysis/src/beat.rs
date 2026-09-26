//! Real-time beat tracking from an onset-strength (novelty) curve sampled once per hop.
//!
//! * **Tempo:** a leaky autocorrelation of the mean-removed onset curve (≈5 s memory) is scored
//!   with a comb over the first four multiples of each candidate period (0.1 BPM grid inside the
//!   configured range). The winner's octave family inside the range is then resolved by metrical
//!   evenness: the fastest candidate whose alternate beats carry comparable onset energy is the
//!   beat (a candidate whose every other "beat" is much weaker is the subdivision). The chosen
//!   period is refined by a joint period/phase comb search (±1.5 %) over the last 8 s, and only
//!   replaced by a different tempo after it has won consistently for ~1 s.
//! * **Phase:** every hop a comb over the most recent beats (recency weighted) locates the last
//!   beat; a phase-locked loop nudges a continuous beat clock towards it, so beat positions are
//!   smooth and extrapolate between hops (`beat_at`, `phase_at`).
//! * **Tap tempo:** ≥ 2 taps within 2 s of each other set the period (least squares over the last
//!   taps) and put a beat on the last tap. The tapped tempo holds until [`BeatTracker::clear_tap`]
//!   or a new tap series; the phase keeps locking to onsets, but only to those within ±4 % of a
//!   period of the predicted beat.
//!
//! Nothing allocates after [`BeatTracker::new`].

/// Absolute limits for [`BeatTracker::set_range`].
pub const MIN_BPM_LIMIT: f32 = 30.0;
pub const MAX_BPM_LIMIT: f32 = 300.0;

const HISTORY_S: f64 = 8.0;
const ACF_MEMORY_S: f64 = 5.0;
const MEAN_MEMORY_S: f64 = 3.0;
const COMB_MULTIPLES: usize = 4;
const TEMPO_EVERY: u32 = 8;
const WARMUP_S: f64 = 2.0;
const SWITCH_S: f64 = 1.0;
const EVEN_MIN: f64 = 0.45;
const PHASE_GAIN: f64 = 0.08;
const TAP_WINDOW_NS: u64 = 2_000_000_000;
const TAP_LOCK: f64 = 0.04;
const MAX_TAPS: usize = 8;
const CONF_LOCK: f32 = 0.3;
const CONF_UNLOCK: f32 = 0.18;

/// A beat emitted by [`BeatTracker::push`] (and forwarded by the live analyzer).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeatEvent {
    /// Master-clock ns of the beat (the clock's exact crossing time, not the hop boundary).
    pub ts: u64,
    /// Running beat count since the tracker first locked.
    pub index: u64,
    pub bpm: f32,
    /// Best-effort bar start (4/4, strongest average onset accent among the four bar phases).
    pub downbeat: bool,
    pub confidence: f32,
}

#[derive(Clone, Copy, Debug)]
struct Clock {
    b0: f64,
    t0: u64,
    period_ns: f64,
}

impl Clock {
    fn beat_at(&self, ts: u64) -> f64 {
        self.b0 + (ts as f64 - self.t0 as f64) / self.period_ns
    }

    fn time_of(&self, beat: f64) -> u64 {
        (self.t0 as f64 + (beat - self.b0) * self.period_ns).max(0.0) as u64
    }
}

/// Real-time tempo + phase tracker (see the module docs).
#[derive(Clone, Debug)]
pub struct BeatTracker {
    fr: f64,
    hop_ns: f64,
    min_bpm: f32,
    max_bpm: f32,
    // Onset history (rings indexed by age: 0 = newest).
    raw: Vec<f32>,
    smooth: Vec<f32>,
    cen: Vec<f32>,
    head: usize,
    frames: u64,
    mean: f64,
    a_mean: f64,
    acf: Vec<f64>,
    lambda: f64,
    // Tempo.
    period: f64,
    cand: f64,
    cand_hits: u32,
    ticks: u32,
    salience: f64,
    // Clock and output.
    clock: Option<Clock>,
    next_index: i64,
    conf: f32,
    a_conf: f32,
    locked: bool,
    // Taps.
    taps: [u64; MAX_TAPS],
    n_taps: usize,
    tap_period_ns: Option<f64>,
    // Bar phase.
    accent: [f32; 4],
    accents_seen: u32,
    last_beat: Option<(i64, u64)>,
}

impl BeatTracker {
    /// `sample_rate`/`hop` define the rate at which [`push`](Self::push) is called
    /// (`sample_rate / hop` values per second). Default range 70–180 BPM.
    pub fn new(sample_rate: f32, hop: usize) -> Self {
        let fr = sample_rate.max(1.0) as f64 / hop.max(1) as f64;
        let cap = (HISTORY_S * fr).ceil() as usize + 4;
        let max_lag = ((60.0 / MIN_BPM_LIMIT as f64) * fr * COMB_MULTIPLES as f64).ceil() as usize + 2;
        let cap = cap.max(max_lag + 2);
        BeatTracker {
            fr,
            hop_ns: 1e9 / fr,
            min_bpm: 70.0,
            max_bpm: 180.0,
            raw: vec![0.0; cap],
            smooth: vec![0.0; cap],
            cen: vec![0.0; cap],
            head: 0,
            frames: 0,
            mean: 0.0,
            a_mean: 1.0 - (-1.0 / (MEAN_MEMORY_S * fr)).exp(),
            acf: vec![0.0; max_lag + 1],
            lambda: (-1.0 / (ACF_MEMORY_S * fr)).exp(),
            period: 0.0,
            cand: 0.0,
            cand_hits: 0,
            ticks: 0,
            salience: 0.0,
            clock: None,
            next_index: 0,
            conf: 0.0,
            a_conf: 1.0 - (-1.0 / fr as f32).exp(),
            locked: false,
            taps: [0; MAX_TAPS],
            n_taps: 0,
            tap_period_ns: None,
            accent: [0.0; 4],
            accents_seen: 0,
            last_beat: None,
        }
    }

    /// Restrict the tempo search (clamped to 30–300 BPM). Octave candidates outside the range are
    /// never reported, so e.g. `set_range(90, 180)` turns a 70 BPM groove into 140.
    pub fn set_range(&mut self, min_bpm: f32, max_bpm: f32) {
        let lo = min_bpm.clamp(MIN_BPM_LIMIT, MAX_BPM_LIMIT);
        let hi = max_bpm.clamp(MIN_BPM_LIMIT, MAX_BPM_LIMIT);
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        self.min_bpm = lo;
        self.max_bpm = hi.max(lo + 1.0).min(MAX_BPM_LIMIT);
        if self.period > 0.0 {
            let bpm = self.frames_to_bpm(self.period);
            if bpm < self.min_bpm || bpm > self.max_bpm {
                // Re-acquire inside the new range on the next tempo tick.
                self.period = 0.0;
                self.cand_hits = 0;
            }
        }
    }

    pub fn range(&self) -> (f32, f32) {
        (self.min_bpm, self.max_bpm)
    }

    /// Current tempo (tapped tempo while tap mode is active); 0 before the first estimate.
    pub fn bpm(&self) -> f32 {
        if let Some(p) = self.tap_period_ns {
            return (60e9 / p) as f32;
        }
        if self.period > 0.0 { self.frames_to_bpm(self.period) } else { 0.0 }
    }

    /// 0–1 lock confidence (1 while tapped).
    pub fn confidence(&self) -> f32 {
        if self.tap_period_ns.is_some() { 1.0 } else { self.conf }
    }

    /// True while a tapped tempo overrides the estimator.
    pub fn tapped(&self) -> bool {
        self.tap_period_ns.is_some()
    }

    /// Running beat position at `ts` (integer part = beat index, fraction = phase), extrapolated
    /// from the beat clock; 0 before the first lock.
    pub fn beat_at(&self, ts: u64) -> f64 {
        self.clock.map_or(0.0, |c| c.beat_at(ts))
    }

    /// Beat phase 0–1 at `ts`.
    pub fn phase_at(&self, ts: u64) -> f32 {
        let b = self.beat_at(ts);
        (b - b.floor()) as f32
    }

    /// Register a tap at master-clock `ts`. Taps more than 2 s apart start a new series.
    pub fn tap(&mut self, ts: u64) {
        if self.n_taps > 0 {
            let last = self.taps[self.n_taps - 1];
            if ts <= last || ts - last > TAP_WINDOW_NS {
                self.n_taps = 0;
            }
        }
        if self.n_taps == MAX_TAPS {
            self.taps.copy_within(1.., 0);
            self.n_taps -= 1;
        }
        self.taps[self.n_taps] = ts;
        self.n_taps += 1;
        if self.n_taps < 2 {
            return;
        }
        // Least-squares slope of tap time against tap index.
        let n = self.n_taps as f64;
        let t0 = self.taps[0] as f64;
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
        for (i, &t) in self.taps[..self.n_taps].iter().enumerate() {
            let (x, y) = (i as f64, t as f64 - t0);
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
        let period = (n * sxy - sx * sy) / (n * sxx - sx * sx);
        let bpm = 60e9 / period;
        if !(MIN_BPM_LIMIT as f64..=MAX_BPM_LIMIT as f64).contains(&bpm) {
            return;
        }
        self.tap_period_ns = Some(period);
        let beat = self.clock.map_or(0.0, |c| c.beat_at(ts).round());
        self.clock = Some(Clock { b0: beat, t0: ts, period_ns: period });
        self.next_index = beat as i64 + 1;
    }

    /// Leave tap mode; the estimator's tempo (tracked all along) takes over again.
    pub fn clear_tap(&mut self) {
        self.tap_period_ns = None;
        self.n_taps = 0;
    }

    /// Forget all history (tempo, clock, taps); keeps the range. Does not allocate.
    pub fn reset(&mut self) {
        self.raw.fill(0.0);
        self.smooth.fill(0.0);
        self.cen.fill(0.0);
        self.acf.fill(0.0);
        self.head = 0;
        self.frames = 0;
        self.mean = 0.0;
        self.period = 0.0;
        self.cand = 0.0;
        self.cand_hits = 0;
        self.ticks = 0;
        self.salience = 0.0;
        self.clock = None;
        self.next_index = 0;
        self.conf = 0.0;
        self.locked = false;
        self.n_taps = 0;
        self.tap_period_ns = None;
        self.accent = [0.0; 4];
        self.accents_seen = 0;
        self.last_beat = None;
    }

    /// Feed one onset-strength sample; `ts` = master-clock ns the sample refers to (i.e. when
    /// the onsets it measures happened). Returns a beat when the clock crossed a beat since the
    /// previous call.
    pub fn push(&mut self, onset_strength: f32, ts: u64) -> Option<BeatEvent> {
        let o = if onset_strength.is_finite() { onset_strength.max(0.0) } else { 0.0 };
        self.store(o);
        self.update_acf();
        self.ticks += 1;
        let warm = self.frames as f64 >= WARMUP_S * self.fr;
        if warm && (self.ticks >= TEMPO_EVERY || self.period == 0.0) {
            self.ticks = 0;
            self.update_tempo();
        }
        let period_frames = match self.tap_period_ns {
            Some(p) => p / self.hop_ns,
            None => self.period,
        };
        if warm && period_frames > 0.0 {
            self.update_phase(period_frames, ts);
        } else if self.tap_period_ns.is_none() {
            self.conf += self.a_conf * (0.0 - self.conf);
        }
        self.emit(ts)
    }

    fn frames_to_bpm(&self, p: f64) -> f32 {
        (60.0 * self.fr / p) as f32
    }

    fn bpm_to_frames(&self, bpm: f64) -> f64 {
        60.0 * self.fr / bpm
    }

    fn cap(&self) -> usize {
        self.raw.len()
    }

    /// Frames of valid history.
    fn avail(&self) -> usize {
        (self.frames as usize).min(self.cap())
    }

    #[inline]
    fn idx(&self, age: usize) -> usize {
        (self.head + self.cap() - age) % self.cap()
    }

    fn store(&mut self, o: f32) {
        self.head = (self.head + 1) % self.cap();
        self.frames += 1;
        self.mean += self.a_mean * (o as f64 - self.mean);
        let h = self.head;
        self.raw[h] = o;
        self.cen[h] = (o as f64 - self.mean) as f32;
        // 3-tap smoothing, finalised one frame late; the newest value is one-sided.
        let i1 = self.idx(1);
        let i2 = self.idx(2);
        self.smooth[i1] = 0.25 * self.raw[i2] + 0.5 * self.raw[i1] + 0.25 * o;
        self.smooth[h] = 0.5 * o + 0.25 * self.raw[i1];
    }

    fn update_acf(&mut self) {
        let lags = (self.acf.len() - 1).min(self.avail().saturating_sub(1));
        let c = self.cen[self.head] as f64;
        let lambda = self.lambda;
        self.acf[0] = lambda * self.acf[0] + c * c;
        // Walk ages 1..=lags over the (at most two) contiguous ring segments.
        let cap = self.cap();
        let mut age = 1;
        while age <= lags {
            let i = (self.head + cap - age) % cap;
            let run = (i + 1).min(lags - age + 1);
            for k in 0..run {
                let a = age + k;
                self.acf[a] = lambda * self.acf[a] + c * self.cen[i - k] as f64;
            }
            age += run;
        }
        for a in lags + 1..self.acf.len() {
            self.acf[a] *= lambda;
        }
    }

    fn acf_at(&self, lag: f64) -> f64 {
        let i = lag.floor() as usize;
        if i + 1 >= self.acf.len() {
            return 0.0;
        }
        let f = lag - i as f64;
        self.acf[i] * (1.0 - f) + self.acf[i + 1] * f
    }

    /// Normalised comb salience of period `p` (frames).
    fn comb_salience(&self, p: f64) -> f64 {
        let limit = (self.acf.len() - 2).min(self.avail().saturating_sub(2)) as f64;
        let (mut s, mut n) = (0.0, 0);
        for m in 1..=COMB_MULTIPLES {
            let lag = p * m as f64;
            if lag > limit {
                break;
            }
            s += self.acf_at(lag);
            n += 1;
        }
        if n == 0 || self.acf[0] <= 1e-12 { 0.0 } else { s / n as f64 / self.acf[0] }
    }

    /// Smoothed onset value at fractional `age` (frames back from newest), 0 outside history.
    fn odf_at(&self, age: f64) -> f64 {
        if age < 0.0 {
            return 0.0;
        }
        let i = age.floor() as usize;
        if i + 1 >= self.avail() {
            return 0.0;
        }
        let f = age - i as f64;
        self.smooth[self.idx(i)] as f64 * (1.0 - f) + self.smooth[self.idx(i + 1)] as f64 * f
    }

    fn beats_in_history(&self, p: f64) -> usize {
        (((self.avail() as f64 - 2.0) / p).floor() as usize).clamp(1, 32)
    }

    /// Comb score of beats at ages `phi + j·p`, `j < k`, weights `w^j`.
    fn comb(&self, p: f64, phi: f64, k: usize, w: f64) -> f64 {
        let mut s = 0.0;
        let mut g = 1.0;
        for j in 0..k {
            s += g * self.odf_at(phi + j as f64 * p);
            g *= w;
        }
        s
    }

    /// Best phase (age of the last beat) for period `p`: (phi, best score, mean score).
    fn best_phase(&self, p: f64, k: usize, w: f64) -> (f64, f64, f64) {
        let n = p.ceil() as usize;
        let (mut best, mut best_i, mut sum) = (f64::MIN, 0usize, 0.0);
        for i in 0..n {
            let s = self.comb(p, i as f64, k, w);
            sum += s;
            if s > best {
                best = s;
                best_i = i;
            }
        }
        let mean = sum / n as f64;
        // Parabolic refinement on the (cyclic) phase grid.
        let l = self.comb(p, if best_i == 0 { p - 1.0 } else { best_i as f64 - 1.0 }, k, w);
        let r = self.comb(p, best_i as f64 + 1.0, k, w);
        let den = l - 2.0 * best + r;
        let off = if den < -1e-12 { (0.5 * (l - r) / den).clamp(-0.5, 0.5) } else { 0.0 };
        ((best_i as f64 + off).rem_euclid(p), best, mean)
    }

    /// Ratio (≤ 1) of onset energy on alternate beats of the best grid for period `p`.
    fn evenness(&self, p: f64) -> f64 {
        let k = self.beats_in_history(p) & !1;
        if k < 2 {
            return 1.0;
        }
        let (phi, _, _) = self.best_phase(p, k, 1.0);
        let (mut even, mut odd) = (0.0, 0.0);
        for j in 0..k {
            let v = self.odf_at(phi + j as f64 * p);
            if j % 2 == 0 { even += v } else { odd += v }
        }
        let hi = even.max(odd);
        if hi <= 1e-12 { 1.0 } else { even.min(odd) / hi }
    }

    fn update_tempo(&mut self) {
        let (lo, hi) = (self.min_bpm as f64, self.max_bpm as f64);
        let steps = ((hi - lo) / 0.1).round() as usize;
        let (mut best, mut best_i) = (f64::MIN, 0usize);
        for i in 0..=steps {
            let s = self.comb_salience(self.bpm_to_frames(lo + i as f64 * 0.1));
            if s > best {
                best = s;
                best_i = i;
            }
        }
        if best <= 0.0 {
            self.salience = 0.0;
            return;
        }
        let bpm_at = |i: usize| lo + i as f64 * 0.1;
        let mut bpm = bpm_at(best_i);
        if best_i > 0 && best_i < steps {
            let l = self.comb_salience(self.bpm_to_frames(bpm_at(best_i - 1)));
            let r = self.comb_salience(self.bpm_to_frames(bpm_at(best_i + 1)));
            let den = l - 2.0 * best + r;
            if den < -1e-12 {
                bpm += 0.1 * (0.5 * (l - r) / den).clamp(-0.5, 0.5);
            }
        }
        let p_star = self.bpm_to_frames(bpm);
        let (p_min, p_max) = (self.bpm_to_frames(hi), self.bpm_to_frames(lo));
        // Octave family inside the range, fastest first.
        let mut chosen = p_star;
        for j in (-3..=3).rev() {
            let p = p_star * 2f64.powi(-j);
            if p < p_min * 0.999 || p > p_max * 1.001 {
                continue;
            }
            if j != 0 && self.comb_salience(p) < 0.2 * best {
                continue;
            }
            if self.evenness(p) >= EVEN_MIN {
                chosen = p;
                break;
            }
        }
        let p_new = self.refine_period(chosen);
        self.salience = self.comb_salience(p_new).max(0.0);
        if self.period == 0.0 {
            self.period = p_new;
            self.cand_hits = 0;
            return;
        }
        let ratio = p_new / self.period;
        if (ratio - 1.0).abs() <= 0.04 {
            self.period += 0.3 * (p_new - self.period);
            self.cand_hits = 0;
        } else {
            if self.cand_hits > 0 && (p_new / self.cand - 1.0).abs() <= 0.04 {
                self.cand_hits += 1;
            } else {
                self.cand_hits = 1;
            }
            self.cand = p_new;
            let need = (SWITCH_S * self.fr / TEMPO_EVERY as f64).ceil() as u32;
            if self.cand_hits >= need {
                self.period = p_new;
                self.cand_hits = 0;
            }
        }
    }

    /// Joint period/phase comb maximisation within ±1.5 % of `p`.
    fn refine_period(&self, p: f64) -> f64 {
        let k = self.beats_in_history(p);
        if k < 3 {
            return p;
        }
        let (phi0, _, _) = self.best_phase(p, k, 1.0);
        let score = |q: f64| {
            let mut best = f64::MIN;
            for d in -6..=6 {
                best = best.max(self.comb(q, (phi0 + d as f64 * 0.5).max(0.0), k, 1.0));
            }
            best
        };
        let steps = 15i32;
        let step = 0.001;
        let (mut best, mut best_i) = (f64::MIN, 0i32);
        let mut scores = [0.0f64; 31];
        for i in -steps..=steps {
            let s = score(p * (1.0 + i as f64 * step));
            scores[(i + steps) as usize] = s;
            if s > best {
                best = s;
                best_i = i;
            }
        }
        let mut f = best_i as f64;
        if best_i > -steps && best_i < steps {
            let l = scores[(best_i + steps - 1) as usize];
            let r = scores[(best_i + steps + 1) as usize];
            let den = l - 2.0 * best + r;
            if den < -1e-12 {
                f += (0.5 * (l - r) / den).clamp(-0.5, 0.5);
            }
        }
        p * (1.0 + f * step)
    }

    fn update_phase(&mut self, p: f64, ts: u64) {
        let k = self.beats_in_history(p).min(12);
        let (phi, best, mean) = self.best_phase(p, k, 0.85);
        if self.tap_period_ns.is_none() {
            let contrast = if best > 1e-9 { (best - mean) / best } else { 0.0 };
            let raw = (((contrast - 0.25) / 0.45).clamp(0.0, 1.0) * (self.salience * 3.0).clamp(0.0, 1.0)) as f32;
            self.conf += self.a_conf * (raw - self.conf);
        }
        let period_ns = p * self.hop_ns;
        let beat_ts = (ts as f64 - phi * self.hop_ns).max(0.0) as u64;
        match &mut self.clock {
            None => {
                if best > 1e-9 {
                    self.clock = Some(Clock { b0: 0.0, t0: beat_ts, period_ns });
                    self.next_index = 1;
                }
            }
            Some(c) => {
                let x = c.beat_at(beat_ts);
                let err = x - x.round();
                let tapped = self.tap_period_ns.is_some();
                let apply = best > 1e-9 && (!tapped || err.abs() <= TAP_LOCK);
                let now = c.beat_at(ts) - if apply { PHASE_GAIN * err } else { 0.0 };
                *c = Clock { b0: now, t0: ts, period_ns };
            }
        }
    }

    fn emit(&mut self, ts: u64) -> Option<BeatEvent> {
        let tapped = self.tap_period_ns.is_some();
        if self.locked {
            self.locked = self.conf >= CONF_UNLOCK;
        } else {
            self.locked = self.conf >= CONF_LOCK;
        }
        let clock = self.clock?;
        let b = clock.beat_at(ts);
        let floor = b.floor() as i64;
        if !(self.locked || tapped) {
            self.next_index = self.next_index.max(floor + 1);
            return None;
        }
        if floor < self.next_index || floor < 0 {
            return None;
        }
        let index = floor;
        self.next_index = index + 1;
        let beat_ts = clock.time_of(index as f64);
        // Accent of the previous beat (its onsets are fully in the history by now).
        if let Some((prev, prev_ts)) = self.last_beat
            && prev + 1 == index
        {
            let age = (ts as f64 - prev_ts as f64) / self.hop_ns;
            let win = (0.08 * clock.period_ns / self.hop_ns).max(2.0);
            let mut a = 0.0f64;
            let mut d = -win;
            while d <= win {
                a = a.max(self.odf_at(age + d));
                d += 1.0;
            }
            let slot = prev.rem_euclid(4) as usize;
            self.accent[slot] = 0.8 * self.accent[slot] + 0.2 * a as f32;
            self.accents_seen += 1;
        }
        self.last_beat = Some((index, beat_ts));
        let bar_phase = if self.accents_seen >= 8 { (0..4).max_by(|&a, &b| self.accent[a].total_cmp(&self.accent[b])).unwrap_or(0) as i64 } else { 0 };
        Some(BeatEvent { ts: beat_ts, index: index as u64, bpm: self.bpm(), downbeat: index.rem_euclid(4) == bar_phase, confidence: self.confidence() })
    }
}
