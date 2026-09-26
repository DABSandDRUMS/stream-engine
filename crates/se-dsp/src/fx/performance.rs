//! Performance effects driven by trigger edges (`X.active`): `stutter`, `tapestop`,
//! `vinylbrake`, `reverse`, and `chopper`.
//!
//! While not triggered they pass the input through bit-exactly, so the default (1, 0) mix is
//! transparent. Every transition (start, loop edge, chunk edge, release) is a crossfade or a
//! continuous speed change, never a jump.

use super::defaults;
use crate::util::{DIVISIONS, DelayLine, division_beats, flush, smoothstep};
use crate::{Ctx, Effect, ParamSpec, Transport};

/// Grid choices for "wait until the next …" (`quantize`).
pub const QUANTIZE: &[&str] = &["off", "1/16", "1/8", "1/4", "1 bar"];
/// Loop-edge and chunk-edge crossfade.
pub const XFADE_MS: f32 = 4.0;
/// Return-to-live crossfade after release.
pub const RELEASE_MS: f32 = 10.0;
/// Longest stutter loop.
pub const MAX_LOOP_S: f32 = 4.0;
/// Longest reverse chunk.
pub const MAX_CHUNK_S: f32 = 2.5;
/// History kept by tape stop / vinyl brake (bounds how far playback may lag behind live).
pub const MAX_LAG_S: f32 = 5.4;

/// Grid length in beats for a `quantize` choice (`None` = start immediately).
pub fn quantize_beats(idx: usize, t: &Transport) -> Option<f64> {
    match idx {
        0 => None,
        1 => Some(0.25),
        2 => Some(0.5),
        3 => Some(1.0),
        _ => Some(t.beats_per_bar.max(1) as f64),
    }
}

fn samples(ms: f32, sr: f32) -> usize {
    ((ms * 0.001 * sr).round() as usize).max(1)
}

/// Waits for the frame nearest the next grid point at or after arming.
#[derive(Clone, Copy, Debug, Default)]
struct GridWait {
    pending: bool,
    target: Option<f64>,
}

impl GridWait {
    fn arm(&mut self) {
        self.pending = true;
        self.target = None;
    }

    fn cancel(&mut self) {
        self.pending = false;
    }

    /// True (once) at the frame whose beat position `beat` is within half a sample (`half`, in
    /// beats) of the grid point. `grid = None` fires immediately.
    #[inline]
    fn due(&mut self, grid: Option<f64>, beat: f64, half: f64) -> bool {
        if !self.pending {
            return false;
        }
        let Some(g) = grid else {
            self.pending = false;
            return true;
        };
        let next = || ((beat - half) / g).ceil() * g;
        let t = match self.target {
            // the transport jumped back by more than a grid step: aim at the next point again
            Some(t) if beat + g >= t => t,
            _ => next(),
        };
        self.target = Some(t);
        if beat + half >= t {
            self.pending = false;
            return true;
        }
        false
    }
}

// ---------------------------------------------------------------------------------------------

pub const STUTTER_PARAMS: &[ParamSpec] = &[
    ParamSpec::choice("division", DIVISIONS, 4, "length of the captured slice that repeats"),
    ParamSpec::choice("quantize", QUANTIZE, 1, "on trigger, wait for this grid before capturing"),
    ParamSpec::float("decay", 0.0, 1.0, 0.0, "", "level drop per repeat (0 = none, 0.5 = −6 dB per repeat)"),
    ParamSpec::float("gate", 0.05, 1.0, 1.0, "", "sounding portion of each repeat"),
];

/// One playing loop (two exist so a new capture can start while the last one fades out).
#[derive(Clone, Copy, Debug, Default)]
struct Loop {
    active: bool,
    fading: bool,
    /// 0–1 linear; shaped with smoothstep when mixing.
    mix: f32,
    len: usize,
    pos: usize,
    rep: u32,
    gain: f32,
    prev_gain: f32,
    gated: bool,
    gate_len: usize,
    prev_gated: bool,
    prev_gate_len: usize,
}

/// Envelope of a gated repeat at position `p` (fade-out of `xf` ending at `gl`).
#[inline]
fn gate_gain(p: usize, gated: bool, gl: usize, xf: usize) -> f32 {
    if !gated || p + xf < gl {
        1.0
    } else if p < gl {
        1.0 - smoothstep((p + xf - gl) as f32 / xf as f32)
    } else {
        0.0
    }
}

/// Beat repeat. On trigger it waits for the `quantize` grid, captures `division` of audio
/// (passing it through live), then loops it. Each loop start crossfades from the audio that
/// followed the slice (the natural continuation) into the slice start, so the loop seam is as
/// smooth as the source. Release crossfades back to live.
pub struct Stutter {
    sr: f32,
    division: usize,
    quantize: usize,
    decay: f32,
    gate: f32,
    xf: usize,
    rel_step: f32,
    max_len: usize,
    bufs: [[Vec<f32>; 2]; 2],
    loops: [Loop; 2],
    wait: GridWait,
    /// Capture in progress: (buffer, position, length).
    rec: Option<(usize, usize, usize)>,
}

impl Stutter {
    pub fn new(sr: f32) -> Stutter {
        let p: [f32; 4] = defaults(STUTTER_PARAMS);
        let xf = samples(XFADE_MS, sr);
        let max_len = (MAX_LOOP_S * sr) as usize;
        let buf = || [vec![0.0; max_len + xf], vec![0.0; max_len + xf]];
        Stutter {
            sr,
            division: p[0] as usize,
            quantize: p[1] as usize,
            decay: p[2],
            gate: p[3],
            xf,
            rel_step: 1.0 / samples(RELEASE_MS, sr) as f32,
            max_len,
            bufs: [buf(), buf()],
            loops: [Loop::default(); 2],
            wait: GridWait::default(),
            rec: None,
        }
    }

    fn latch_gate(&self, len: usize) -> (bool, usize) {
        let gl = ((self.gate * len as f32).round() as usize).max(2 * self.xf);
        (gl + self.xf < len, gl)
    }

    /// Buffer for a new capture: a free one, else the most faded-out one (cut hard; only
    /// reachable when retriggering faster than the release crossfade).
    fn free_buf(&mut self) -> usize {
        if let Some(b) = self.loops.iter().position(|l| !l.active) {
            return b;
        }
        let b = if self.loops[0].mix <= self.loops[1].mix { 0 } else { 1 };
        self.loops[b].active = false;
        b
    }

    fn start_loop(&mut self, b: usize, len: usize) {
        for lp in self.loops.iter_mut().filter(|l| l.active && !l.fading) {
            lp.fading = true;
        }
        let (gated, gl) = self.latch_gate(len);
        self.loops[b] = Loop {
            active: true,
            fading: false,
            mix: 1.0,
            len,
            pos: 0,
            rep: 1,
            gain: flush(1.0 - self.decay),
            prev_gain: 1.0,
            gated,
            gate_len: gl,
            prev_gated: false,
            prev_gate_len: len,
        };
    }
}

impl Effect for Stutter {
    fn kind(&self) -> &'static str {
        "stutter"
    }

    fn params(&self) -> &'static [ParamSpec] {
        STUTTER_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.division = (v as usize).min(DIVISIONS.len() - 1),
            1 => self.quantize = (v as usize).min(QUANTIZE.len() - 1),
            2 => self.decay = v.clamp(0.0, 1.0),
            3 => self.gate = v.clamp(0.05, 1.0),
            _ => {}
        }
    }

    fn trigger(&mut self, on: bool) {
        if on {
            self.wait.arm();
        } else {
            self.wait.cancel();
            self.rec = None;
            for lp in self.loops.iter_mut().filter(|l| l.active) {
                lp.fading = true;
            }
        }
    }

    fn process(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        if self.rec.is_none() && !self.wait.pending && !self.loops.iter().any(|l| l.active) {
            return;
        }
        let spb = ctx.transport.samples_per_beat(self.sr);
        let grid = quantize_beats(self.quantize, &ctx.transport);
        let xf = self.xf;
        for i in 0..l.len().min(r.len()) {
            let (xl, xr) = (l[i], r[i]);
            if self.wait.due(grid, ctx.transport.beat + i as f64 / spb, 0.5 / spb) {
                let len = ((division_beats(self.division) * spb).round() as usize).clamp(4 * xf, self.max_len);
                let b = self.free_buf();
                self.rec = Some((b, 0, len));
            }
            let mut start = None;
            if let Some((b, pos, len)) = self.rec.as_mut() {
                self.bufs[*b][0][*pos] = xl;
                self.bufs[*b][1][*pos] = xr;
                *pos += 1;
                if *pos == *len {
                    start = Some((*b, *len));
                }
            }
            let (mut ol, mut or) = (xl, xr);
            for (b, lp) in self.loops.iter_mut().enumerate() {
                if !lp.active {
                    continue;
                }
                let buf = &mut self.bufs[b];
                let (p, len) = (lp.pos, lp.len);
                if lp.rep == 1 && p < xf {
                    buf[0][len + p] = xl;
                    buf[1][len + p] = xr;
                }
                let g = gate_gain(p, lp.gated, lp.gate_len, xf) * lp.gain;
                let (mut yl, mut yr) = (buf[0][p] * g, buf[1][p] * g);
                if p < xf {
                    let gc = if lp.rep == 1 { 1.0 } else { gate_gain(len + p, lp.prev_gated, lp.prev_gate_len, xf) * lp.prev_gain };
                    let (cl, cr) = (buf[0][len + p] * gc, buf[1][len + p] * gc);
                    let f = smoothstep(p as f32 / xf as f32);
                    yl = cl + (yl - cl) * f;
                    yr = cr + (yr - cr) * f;
                }
                let m = smoothstep(lp.mix);
                ol += m * (yl - xl);
                or += m * (yr - xr);
                lp.pos += 1;
                if lp.pos == len {
                    lp.pos = 0;
                    lp.rep += 1;
                    lp.prev_gain = lp.gain;
                    lp.gain = flush(lp.gain * (1.0 - self.decay));
                    lp.prev_gated = lp.gated;
                    lp.prev_gate_len = lp.gate_len;
                    let gl = ((self.gate * len as f32).round() as usize).max(2 * xf);
                    lp.gated = gl + xf < len;
                    lp.gate_len = gl;
                }
                if lp.fading {
                    lp.mix -= self.rel_step;
                    if lp.mix <= 0.0 {
                        lp.active = false;
                    }
                }
            }
            if let Some((b, len)) = start {
                self.rec = None;
                self.start_loop(b, len);
            }
            l[i] = ol;
            r[i] = or;
        }
    }

    fn tail(&self) -> usize {
        let Some(lp) = self.loops.iter().filter(|l| l.active).max_by_key(|l| l.len) else { return 0 };
        let repeats = if self.decay <= 1e-3 { f32::INFINITY } else { (1e-3f32).ln() / (1.0 - self.decay).max(1e-6).ln() };
        (lp.len as f32 * repeats.max(1.0)).min(60.0 * self.sr) as usize
    }

    fn reset(&mut self) {
        self.loops = [Loop::default(); 2];
        self.wait.cancel();
        self.rec = None;
    }
}

// ---------------------------------------------------------------------------------------------

/// Playback-speed profile while stopping.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Profile {
    /// `(1 − u)^k`.
    Tape(f32),
    /// Exponential brake.
    Brake,
    /// Quick kick to reverse, then decaying backwards spin.
    Backspin,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Motion {
    Idle,
    Stopping { t: f32, total: f32, from: f32 },
    Stopped,
    Starting { t: f32, total: f32, from: f32 },
    Returning,
}

/// Reverse speed reached by the backspin kick.
const BACKSPIN_SPEED: f32 = 2.0;
const BACKSPIN_KICK_MS: f32 = 40.0;
/// Below this |speed| the playback level fades out (a stopped tape is silent, not DC).
const SILENT_SPEED: f32 = 0.15;

/// Variable-speed playback of the live history (shared by tape stop and vinyl brake).
struct Varispeed {
    sr: f32,
    dl: [DelayLine; 2],
    max_lag: f32,
    lag: f32,
    speed: f32,
    mix: f32,
    rel_step: f32,
    motion: Motion,
}

impl Varispeed {
    fn new(sr: f32) -> Varispeed {
        let n = (MAX_LAG_S * sr) as usize + 8;
        let dl = [DelayLine::new(n), DelayLine::new(n)];
        let max_lag = (dl[0].max_delay() - 2) as f32;
        Varispeed { sr, dl, max_lag, lag: 0.0, speed: 1.0, mix: 0.0, rel_step: 1.0 / samples(RELEASE_MS, sr) as f32, motion: Motion::Idle }
    }

    fn stop(&mut self, total: f32) {
        let from = if self.motion == Motion::Idle { 1.0 } else { self.speed };
        self.motion = Motion::Stopping { t: 0.0, total: total.max(1.0), from };
    }

    fn release(&mut self, restart: f32) {
        self.motion = match self.motion {
            Motion::Idle | Motion::Returning => return,
            _ if restart < 1.0 => Motion::Returning,
            Motion::Stopped => {
                // silent at standstill: restart from the live edge
                self.lag = 0.0;
                Motion::Starting { t: 0.0, total: restart, from: 0.0 }
            }
            Motion::Stopping { .. } => Motion::Starting { t: 0.0, total: restart, from: self.speed },
            m @ Motion::Starting { .. } => m,
        };
    }

    fn active(&self) -> bool {
        self.motion != Motion::Idle
    }

    #[inline]
    fn stop_speed(profile: Profile, t: f32, total: f32, from: f32, sr: f32) -> f32 {
        let u = (t / total).min(1.0);
        match profile {
            Profile::Tape(k) => from * (1.0 - u).powf(k),
            Profile::Brake => {
                const E: f32 = 0.018_315_64; // e^−4
                from * (((-4.0 * u).exp() - E) / (1.0 - E)).max(0.0)
            }
            Profile::Backspin => {
                let kick = (BACKSPIN_KICK_MS * 0.001 * sr).min(total * 0.25);
                if t < kick {
                    from + (-BACKSPIN_SPEED - from) * smoothstep(t / kick)
                } else {
                    let v = 1.0 - ((t - kick) / (total - kick)).min(1.0);
                    -BACKSPIN_SPEED * v * v
                }
            }
        }
    }

    fn process(&mut self, profile: Profile, l: &mut [f32], r: &mut [f32]) {
        for i in 0..l.len().min(r.len()) {
            let (xl, xr) = (l[i], r[i]);
            self.dl[0].push(xl);
            self.dl[1].push(xr);
            match &mut self.motion {
                Motion::Idle => continue,
                Motion::Stopping { t, total, from } => {
                    self.speed = Self::stop_speed(profile, *t, *total, *from, self.sr);
                    *t += 1.0;
                    if *t >= *total {
                        self.speed = 0.0;
                        self.motion = Motion::Stopped;
                    }
                }
                Motion::Stopped => self.speed = 0.0,
                Motion::Starting { t, total, from } => {
                    self.speed = *from + (1.0 - *from) * smoothstep(*t / *total);
                    *t += 1.0;
                    if *t >= *total {
                        self.speed = 1.0;
                        self.motion = Motion::Returning;
                    }
                }
                Motion::Returning => {}
            }
            if self.motion != Motion::Stopped {
                self.lag = (self.lag + 1.0 - self.speed).clamp(0.0, self.max_lag);
            }
            let level = smoothstep(self.speed.abs() / SILENT_SPEED);
            let (yl, yr) = (self.dl[0].read(self.lag) * level, self.dl[1].read(self.lag) * level);
            if self.motion == Motion::Returning {
                self.mix -= self.rel_step;
                if self.mix <= 0.0 {
                    self.motion = Motion::Idle;
                    self.mix = 0.0;
                    self.lag = 0.0;
                    self.speed = 1.0;
                    continue;
                }
            } else {
                self.mix = (self.mix + self.rel_step).min(1.0);
            }
            let m = smoothstep(self.mix);
            l[i] = xl + m * (yl - xl);
            r[i] = xr + m * (yr - xr);
        }
    }

    fn reset(&mut self) {
        self.dl[0].clear();
        self.dl[1].clear();
        self.motion = Motion::Idle;
        self.lag = 0.0;
        self.speed = 1.0;
        self.mix = 0.0;
    }
}

/// Stop time in samples from `sync`/`division`/`time`.
fn stop_samples(sync: bool, division: usize, ms: f32, ctx: &Ctx, sr: f32, max_s: f32) -> f32 {
    let s = if sync { (division_beats(division) * ctx.transport.samples_per_beat(sr)) as f32 } else { ms * 0.001 * sr };
    s.clamp(1.0, max_s * sr)
}

pub const TAPESTOP_PARAMS: &[ParamSpec] = &[
    ParamSpec::float("time", 50.0, 4000.0, 1000.0, "ms", "time to come to a stop (when not synced)"),
    ParamSpec::boolean("sync", false, "stop time from `division` at the current tempo"),
    ParamSpec::choice("division", DIVISIONS, 1, "stop time when synced"),
    ParamSpec::float("curve", -1.0, 1.0, 0.0, "", "speed curve: −1 slow start / abrupt end, 0 linear, +1 fast start / long crawl"),
    ParamSpec::float("restart", 0.0, 2000.0, 0.0, "ms", "on release, spin back up over this time (0 = crossfade straight back to live)"),
];

/// Tape stop: playback slows to a standstill; release spins up or crossfades back.
pub struct TapeStop {
    sr: f32,
    time: f32,
    sync: bool,
    division: usize,
    curve: f32,
    restart: f32,
    pending: bool,
    v: Varispeed,
}

impl TapeStop {
    pub fn new(sr: f32) -> TapeStop {
        let p: [f32; 5] = defaults(TAPESTOP_PARAMS);
        TapeStop { sr, time: p[0], sync: p[1] >= 0.5, division: p[2] as usize, curve: p[3], restart: p[4], pending: false, v: Varispeed::new(sr) }
    }
}

impl Effect for TapeStop {
    fn kind(&self) -> &'static str {
        "tapestop"
    }

    fn params(&self) -> &'static [ParamSpec] {
        TAPESTOP_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.time = v.max(1.0),
            1 => self.sync = v >= 0.5,
            2 => self.division = (v as usize).min(DIVISIONS.len() - 1),
            3 => self.curve = v.clamp(-1.0, 1.0),
            4 => self.restart = v.max(0.0),
            _ => {}
        }
    }

    fn trigger(&mut self, on: bool) {
        if on {
            self.pending = true;
        } else {
            self.pending = false;
            self.v.release(self.restart * 0.001 * self.sr);
        }
    }

    fn process(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        if self.pending {
            self.pending = false;
            self.v.stop(stop_samples(self.sync, self.division, self.time, ctx, self.sr, 4.0));
        }
        self.v.process(Profile::Tape(3f32.powf(self.curve)), l, r);
    }

    fn tail(&self) -> usize {
        if self.v.active() { self.v.max_lag as usize } else { 0 }
    }

    fn reset(&mut self) {
        self.pending = false;
        self.v.reset();
    }
}

pub const VINYL_PARAMS: &[ParamSpec] = &[
    ParamSpec::float("time", 100.0, 3000.0, 800.0, "ms", "brake (or backspin) duration when not synced"),
    ParamSpec::boolean("sync", false, "duration from `division` at the current tempo"),
    ParamSpec::choice("division", DIVISIONS, 2, "duration when synced"),
    ParamSpec::boolean("backspin", false, "spin the record backwards instead of braking"),
    ParamSpec::float("restart", 0.0, 2000.0, 0.0, "ms", "on release, spin back up over this time (0 = crossfade straight back to live)"),
];

/// Vinyl brake (exponential slow-down) or backspin.
pub struct VinylBrake {
    sr: f32,
    time: f32,
    sync: bool,
    division: usize,
    backspin: bool,
    restart: f32,
    pending: bool,
    profile: Profile,
    v: Varispeed,
}

impl VinylBrake {
    pub fn new(sr: f32) -> VinylBrake {
        let p: [f32; 5] = defaults(VINYL_PARAMS);
        VinylBrake {
            sr,
            time: p[0],
            sync: p[1] >= 0.5,
            division: p[2] as usize,
            backspin: p[3] >= 0.5,
            restart: p[4],
            pending: false,
            profile: Profile::Brake,
            v: Varispeed::new(sr),
        }
    }
}

impl Effect for VinylBrake {
    fn kind(&self) -> &'static str {
        "vinylbrake"
    }

    fn params(&self) -> &'static [ParamSpec] {
        VINYL_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.time = v.max(1.0),
            1 => self.sync = v >= 0.5,
            2 => self.division = (v as usize).min(DIVISIONS.len() - 1),
            3 => self.backspin = v >= 0.5,
            4 => self.restart = v.max(0.0),
            _ => {}
        }
    }

    fn trigger(&mut self, on: bool) {
        if on {
            self.pending = true;
        } else {
            self.pending = false;
            self.v.release(self.restart * 0.001 * self.sr);
        }
    }

    fn process(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        if self.pending {
            self.pending = false;
            // the profile is latched per stop so toggling `backspin` mid-stop has no effect
            self.profile = if self.backspin { Profile::Backspin } else { Profile::Brake };
            self.v.stop(stop_samples(self.sync, self.division, self.time, ctx, self.sr, 3.0));
        }
        self.v.process(self.profile, l, r);
    }

    fn tail(&self) -> usize {
        if self.v.active() { self.v.max_lag as usize } else { 0 }
    }

    fn reset(&mut self) {
        self.pending = false;
        self.v.reset();
    }
}

// ---------------------------------------------------------------------------------------------

pub const REVERSE_PARAMS: &[ParamSpec] = &[
    ParamSpec::choice("division", DIVISIONS, 2, "chunk length: each chunk plays the previous one backwards"),
    ParamSpec::choice("quantize", QUANTIZE, 0, "on trigger, wait for this grid before starting"),
];

/// Reverse buffer: from the start point on, every `division` chunk of output is the previous
/// chunk of input played backwards (a mirror at each chunk start), crossfaded at chunk edges.
pub struct Reverse {
    sr: f32,
    division: usize,
    quantize: usize,
    xf: usize,
    rel_step: f32,
    max_chunk: usize,
    dl: [DelayLine; 2],
    wait: GridWait,
    active: bool,
    releasing: bool,
    mix: f32,
    k: usize,
    len: usize,
    /// Previous chunk still fading out: (its position when the new chunk began, frames since).
    prev: Option<(usize, usize)>,
}

impl Reverse {
    pub fn new(sr: f32) -> Reverse {
        let p: [f32; 2] = defaults(REVERSE_PARAMS);
        let xf = samples(XFADE_MS, sr);
        let max_chunk = (MAX_CHUNK_S * sr) as usize;
        let hist = 2 * (max_chunk + xf) + 8;
        Reverse {
            sr,
            division: p[0] as usize,
            quantize: p[1] as usize,
            xf,
            rel_step: 1.0 / samples(RELEASE_MS, sr) as f32,
            max_chunk,
            dl: [DelayLine::new(hist), DelayLine::new(hist)],
            wait: GridWait::default(),
            active: false,
            releasing: false,
            mix: 0.0,
            k: 0,
            len: 1,
            prev: None,
        }
    }

    fn chunk_len(&self, spb: f64) -> usize {
        ((division_beats(self.division) * spb).round() as usize).clamp(2 * self.xf, self.max_chunk)
    }
}

impl Effect for Reverse {
    fn kind(&self) -> &'static str {
        "reverse"
    }

    fn params(&self) -> &'static [ParamSpec] {
        REVERSE_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.division = (v as usize).min(DIVISIONS.len() - 1),
            1 => self.quantize = (v as usize).min(QUANTIZE.len() - 1),
            _ => {}
        }
    }

    fn trigger(&mut self, on: bool) {
        if on {
            self.wait.arm();
        } else {
            self.wait.cancel();
            self.releasing = true;
        }
    }

    fn process(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        let spb = ctx.transport.samples_per_beat(self.sr);
        let grid = quantize_beats(self.quantize, &ctx.transport);
        let (xf, up) = (self.xf, 1.0 / self.xf as f32);
        for i in 0..l.len().min(r.len()) {
            let (xl, xr) = (l[i], r[i]);
            self.dl[0].push(xl);
            self.dl[1].push(xr);
            if self.wait.due(grid, ctx.transport.beat + i as f64 / spb, 0.5 / spb) {
                // restarting while still audible: the old stream fades out like a chunk edge
                self.prev = if self.active && self.mix > 0.0 { Some((self.k, 0)) } else { None };
                self.active = true;
                self.releasing = false;
                self.k = 0;
                self.len = self.chunk_len(spb);
            }
            if !self.active {
                continue;
            }
            if self.k == self.len {
                self.prev = Some((self.len, 0));
                self.k = 0;
                self.len = self.chunk_len(spb);
            }
            let d = 2 * self.k + 1;
            let (mut yl, mut yr) = (self.dl[0].tap(d), self.dl[1].tap(d));
            if let Some((k0, q)) = self.prev.as_mut() {
                let dp = 2 * (*k0 + *q) + 1;
                let (pl, pr) = (self.dl[0].tap(dp), self.dl[1].tap(dp));
                let f = smoothstep(*q as f32 / xf as f32);
                yl = pl + (yl - pl) * f;
                yr = pr + (yr - pr) * f;
                *q += 1;
                if *q >= xf {
                    self.prev = None;
                }
            }
            self.k += 1;
            if self.releasing {
                self.mix -= self.rel_step;
                if self.mix <= 0.0 {
                    self.mix = 0.0;
                    self.active = false;
                    self.releasing = false;
                    self.prev = None;
                    continue;
                }
            } else {
                self.mix = (self.mix + up).min(1.0);
            }
            let m = smoothstep(self.mix);
            l[i] = xl + m * (yl - xl);
            r[i] = xr + m * (yr - xr);
        }
    }

    fn tail(&self) -> usize {
        if self.active { 2 * self.len } else { 0 }
    }

    fn reset(&mut self) {
        self.dl[0].clear();
        self.dl[1].clear();
        self.wait.cancel();
        self.active = false;
        self.releasing = false;
        self.mix = 0.0;
        self.prev = None;
    }
}

// ---------------------------------------------------------------------------------------------

const PATTERNS: &[&str] = &["straight", "dotted", "triplet", "offbeat"];

pub const CHOPPER_PARAMS: &[ParamSpec] = &[
    ParamSpec::choice("division", DIVISIONS, 4, "gate step length"),
    ParamSpec::float("duty", 0.05, 1.0, 0.5, "", "open portion of each step"),
    ParamSpec::float("smooth", 0.0, 20.0, 3.0, "ms", "S-curve edge time (0 = hard gate)"),
    ParamSpec::float("depth", 0.0, 1.0, 1.0, "", "attenuation while closed (1 = silent)"),
    ParamSpec::choice("pattern", PATTERNS, 0, "step = division ×1 (straight), ×1.5 (dotted), ×2/3 (triplet), or straight shifted half a step (offbeat)"),
];

/// Tempo-synced amplitude gate, locked to the beat grid. Chops while always-on; in a
/// triggered slot `trigger(false)` opens the gate smoothly and `trigger(true)` resumes.
pub struct Chopper {
    sr: f32,
    division: usize,
    duty: f32,
    ramp: f32,
    depth: crate::Smoother,
    pattern: usize,
    active: bool,
    /// Openness 0–1 (linear ramp, shaped by smoothstep).
    open: f32,
}

impl Chopper {
    pub fn new(sr: f32) -> Chopper {
        let p: [f32; 5] = defaults(CHOPPER_PARAMS);
        Chopper {
            sr,
            division: p[0] as usize,
            duty: p[1],
            ramp: Self::ramp_of(p[2], sr),
            depth: crate::Smoother::with_ms(p[3], 20.0, sr),
            pattern: p[4] as usize,
            active: true,
            open: 1.0,
        }
    }

    fn ramp_of(ms: f32, sr: f32) -> f32 {
        1.0 / (ms * 0.001 * sr).max(1.0)
    }

    /// Step length in beats and phase offset for the current pattern.
    fn step(&self) -> (f64, f64) {
        let d = division_beats(self.division);
        match self.pattern {
            1 => (d * 1.5, 0.0),
            2 => (d * 2.0 / 3.0, 0.0),
            3 => (d, 0.5),
            _ => (d, 0.0),
        }
    }
}

impl Effect for Chopper {
    fn kind(&self) -> &'static str {
        "chopper"
    }

    fn params(&self) -> &'static [ParamSpec] {
        CHOPPER_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.division = (v as usize).min(DIVISIONS.len() - 1),
            1 => self.duty = v.clamp(0.0, 1.0),
            2 => self.ramp = Self::ramp_of(v, self.sr),
            3 => self.depth.set(v.clamp(0.0, 1.0)),
            4 => self.pattern = (v as usize).min(PATTERNS.len() - 1),
            _ => {}
        }
    }

    fn trigger(&mut self, on: bool) {
        self.active = on;
    }

    fn process(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        let spb = ctx.transport.samples_per_beat(self.sr);
        let (step, offset) = self.step();
        for i in 0..l.len().min(r.len()) {
            let depth = self.depth.next();
            let target = if self.active {
                let ph = ((ctx.transport.beat + i as f64 / spb) / step + offset).rem_euclid(1.0);
                if ph < self.duty as f64 { 1.0 } else { 0.0 }
            } else {
                1.0
            };
            if self.open < target {
                self.open = (self.open + self.ramp).min(target);
            } else if self.open > target {
                self.open = (self.open - self.ramp).max(target);
            }
            if self.open >= 1.0 {
                continue;
            }
            let g = 1.0 - depth * (1.0 - smoothstep(self.open));
            l[i] *= g;
            r[i] *= g;
        }
    }

    fn reset(&mut self) {
        self.open = 1.0;
        self.depth.reset(self.depth.target());
    }
}
