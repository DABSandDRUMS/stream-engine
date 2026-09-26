//! Hype detector (§18): folds chat rate vs baseline, emote spam, `!clip` votes, bits/subs/
//! raids/tips, mic spikes and laughter, and music drops/novelty into one score per 100 ms of
//! *screen time*. Viewer reactions (chat, emotes, votes, bits) are shifted back by the
//! measured stream delay so they line up with the moment they react to; a moment is scored
//! once `delay + settle` has passed. Crossing the threshold opens an episode; when it calms
//! down a marker `{start, peak, end, score, reasons}` comes out, with a pre-roll of 10–30 s.
//!
//! Pure and deterministic: the engine task feeds it events, samples signals at
//! [`SAMPLE_HZ`], and calls [`HypeDetector::advance`]; tests drive it with synthetic streams.

use crate::config::HypeConfig;
use se_proto::{Event, Ts, Value};
use std::collections::VecDeque;

/// Local signal sampling rate (mic laughter detection needs ≥ 40 Hz).
pub const SAMPLE_HZ: u32 = 50;
const BUCKET_NS: u64 = 100_000_000;
const MIC_PER_BUCKET: usize = 8;
/// Two minutes of buckets: covers any sane stream delay plus settle.
const RING: usize = 1200;
const LAUGH_BUCKETS: usize = 20;
const NS: f64 = 1e9;
/// Seconds of scored history before chat/emote ratios count.
const WARMUP_S: f64 = 30.0;

/// Score components, in report order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comp {
    Chat,
    Emotes,
    Copypasta,
    Votes,
    Bits,
    Subs,
    Gifts,
    Raid,
    Tip,
    HypeTrain,
    Drop,
    Mic,
    Laughter,
    Novelty,
}

pub const COMPS: [Comp; 14] = [
    Comp::Chat,
    Comp::Emotes,
    Comp::Copypasta,
    Comp::Votes,
    Comp::Bits,
    Comp::Subs,
    Comp::Gifts,
    Comp::Raid,
    Comp::Tip,
    Comp::HypeTrain,
    Comp::Drop,
    Comp::Mic,
    Comp::Laughter,
    Comp::Novelty,
];

impl Comp {
    pub fn name(self) -> &'static str {
        match self {
            Comp::Chat => "chat",
            Comp::Emotes => "emotes",
            Comp::Copypasta => "copypasta",
            Comp::Votes => "votes",
            Comp::Bits => "bits",
            Comp::Subs => "subs",
            Comp::Gifts => "gifts",
            Comp::Raid => "raid",
            Comp::Tip => "tip",
            Comp::HypeTrain => "hype_train",
            Comp::Drop => "drop",
            Comp::Mic => "mic",
            Comp::Laughter => "laughter",
            Comp::Novelty => "novelty",
        }
    }
    fn idx(self) -> usize {
        self as usize
    }
    /// Config name used by `[clips.hype] shift`.
    fn shift_key(self) -> &'static str {
        match self {
            Comp::Chat | Comp::Copypasta => "chat",
            Comp::Emotes => "emotes",
            Comp::Votes => "votes",
            Comp::Bits => "bits",
            Comp::Subs | Comp::Gifts => "subs",
            other => other.name(),
        }
    }
}

/// Impulse categories (decaying accumulators).
const IMPULSES: [Comp; 7] = [Comp::Bits, Comp::Subs, Comp::Gifts, Comp::Raid, Comp::Tip, Comp::HypeTrain, Comp::Drop];

/// A normalized hype-relevant input.
#[derive(Clone, Debug, PartialEq)]
pub enum HypeInput {
    Chat { user: String, text: String, emotes: Option<u32> },
    Bits(u64),
    Sub { tier: u32 },
    Gift { count: u32 },
    Raid { viewers: u64 },
    Tip { amount: f64 },
    HypeTrain,
    Drop,
}

/// Map an engine event to a hype input (normalized contract: `se-core/src/sim.rs`).
pub fn classify(e: &Event) -> Option<HypeInput> {
    let p = &e.payload;
    let num = |k: &str| p.get_path(k).and_then(Value::as_f64);
    Some(match e.ty.as_str() {
        "twitch.chat" => {
            let text = p.get_path("message").and_then(Value::as_str).unwrap_or("").to_string();
            let user = e
                .actor
                .as_ref()
                .map(|a| if a.id.is_empty() { a.name.clone() } else { a.id.clone() })
                .or_else(|| p.get_path("user").and_then(Value::as_str).map(String::from))
                .unwrap_or_default();
            HypeInput::Chat { user, text, emotes: adapter_emotes(p) }
        }
        "twitch.cheer" => HypeInput::Bits(num("bits").unwrap_or(0.0).max(0.0) as u64),
        // gifted subs arrive with the gift event's count; don't count them twice
        "twitch.sub" | "twitch.resub" if !p.get_path("is_gift").is_some_and(Value::truthy) => HypeInput::Sub { tier: tier(p.get_path("tier")) },
        "twitch.gift" => HypeInput::Gift { count: num("count").unwrap_or(1.0).max(1.0) as u32 },
        "twitch.raid" => HypeInput::Raid { viewers: num("viewers").unwrap_or(0.0).max(0.0) as u64 },
        "tip" => HypeInput::Tip { amount: num("amount").unwrap_or(0.0).max(0.0) },
        "twitch.hype_train.begin" | "twitch.hype_train.level" | "twitch.hype_train.level_up" => HypeInput::HypeTrain,
        "band.drop" | "music.drop" => HypeInput::Drop,
        _ => return None,
    })
}

/// Emote count the chat adapter reports (`emotes` list/count, `emote_count`, or EventSub
/// `fragments` of type `emote`).
fn adapter_emotes(p: &Value) -> Option<u32> {
    if let Some(v) = p.get_path("emote_count").and_then(Value::as_i64) {
        return Some(v.max(0) as u32);
    }
    match p.get_path("emotes") {
        Some(Value::List(l)) => return Some(l.len() as u32),
        Some(v) if v.as_i64().is_some() => return Some(v.as_i64().unwrap_or(0).max(0) as u32),
        _ => {}
    }
    if let Some(Value::List(frags)) = p.get_path("fragments") {
        return Some(frags.iter().filter(|f| f.get_path("type").and_then(Value::as_str) == Some("emote")).count() as u32);
    }
    None
}

fn tier(v: Option<&Value>) -> u32 {
    match v {
        Some(Value::Str(s)) => match s.as_str() {
            "2000" | "2" => 2,
            "3000" | "3" => 3,
            _ => 1,
        },
        Some(v) => match v.as_i64().unwrap_or(1) {
            2 | 2000 => 2,
            3 | 3000 => 3,
            _ => 1,
        },
        None => 1,
    }
}

/// A detected hype moment (master-clock ns, screen time).
#[derive(Clone, Debug, PartialEq)]
pub struct HypeMarker {
    pub start: Ts,
    pub peak: Ts,
    pub end: Ts,
    pub score: f64,
    /// Component names that carried the peak, strongest first.
    pub reasons: Vec<String>,
    /// Component values at the peak.
    pub components: Vec<(String, f64)>,
}

impl HypeMarker {
    /// `session.marker` action args.
    pub fn to_value(&self) -> Value {
        let comps = self.components.iter().fold(Value::map(), |m, (k, v)| m.with(k.clone(), round3(*v)));
        Value::map()
            .with("label", "hype")
            .with("kind", "hype")
            .with("start", self.start as i64)
            .with("peak", self.peak as i64)
            .with("end", self.end as i64)
            .with("score", round3(self.score))
            .with("reasons", self.reasons.clone())
            .with("components", comps)
    }

    /// Twitch stream marker description (≤ 140 characters).
    pub fn description(&self) -> String {
        let mut s = format!("hype {:.1}: {}", self.score, self.reasons.join(", "));
        if s.chars().count() > 140 {
            s = s.chars().take(139).collect::<String>() + "…";
        }
        s
    }
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

#[derive(Clone, Copy, Default)]
struct Bucket {
    /// Absolute bucket index held in this slot (`u64::MAX` = empty).
    idx: u64,
    chat: u32,
    emotes: u32,
    copypasta: u32,
    chat_sig: f32,
    chat_sig_n: u16,
    impulses: [f64; IMPULSES.len()],
    mic: [f32; MIC_PER_BUCKET],
    mic_n: u8,
    music: f32,
    music_n: u16,
}

impl Bucket {
    fn empty(idx: u64) -> Bucket {
        Bucket { idx, ..Default::default() }
    }
}

/// Exponential moving mean/variance with a warm-up (cumulative average until `tau`).
#[derive(Clone, Copy, Debug, Default)]
struct Ema {
    mean: f64,
    var: f64,
    n: u64,
}

impl Ema {
    fn update(&mut self, x: f64, dt: f64, tau: f64) {
        self.n += 1;
        let a = (dt / tau).max(1.0 / self.n as f64).min(1.0);
        let d = x - self.mean;
        self.mean += a * d;
        self.var = (1.0 - a) * (self.var + a * d * d);
    }
    fn std(&self) -> f64 {
        self.var.max(0.0).sqrt()
    }
}

#[derive(Clone, Debug)]
enum Episode {
    Calm,
    Active { rise: Ts, peak: Ts, peak_score: f64, comps: [f64; COMPS.len()], last_above: Ts, below_since: Option<Ts> },
    Closing { rise: Ts, peak: Ts, peak_score: f64, comps: [f64; COMPS.len()], last_above: Ts, closed_at: Ts },
}

pub struct HypeDetector {
    cfg: HypeConfig,
    threshold: f64,
    ring: Vec<Bucket>,
    next_eval: Option<u64>,
    shifted: [bool; COMPS.len()],
    chat_base: Ema,
    chat_sig_base: Ema,
    emote_base: Ema,
    mic_base: Ema,
    music_short: f64,
    music_long: Ema,
    impulse: [f64; IMPULSES.len()],
    votes: VecDeque<(Ts, String)>,
    recent: VecDeque<(Ts, String, String)>,
    laugh_scratch: Vec<f32>,
    episode: Episode,
    calm_since: Ts,
    evaluated: u64,
    /// Latest score and components (screen time of the last evaluated bucket).
    pub score: f64,
    pub components: [f64; COMPS.len()],
    pub last_eval: Ts,
}

impl HypeDetector {
    pub fn new(cfg: HypeConfig) -> HypeDetector {
        let mut d = HypeDetector {
            threshold: cfg.threshold,
            cfg,
            ring: vec![Bucket::empty(u64::MAX); RING],
            next_eval: None,
            shifted: [false; COMPS.len()],
            chat_base: Ema::default(),
            chat_sig_base: Ema::default(),
            emote_base: Ema::default(),
            mic_base: Ema::default(),
            music_short: 0.0,
            music_long: Ema::default(),
            impulse: [0.0; IMPULSES.len()],
            votes: VecDeque::new(),
            recent: VecDeque::new(),
            laugh_scratch: Vec::with_capacity(LAUGH_BUCKETS * MIC_PER_BUCKET),
            episode: Episode::Calm,
            calm_since: 0,
            evaluated: 0,
            score: 0.0,
            components: [0.0; COMPS.len()],
            last_eval: 0,
        };
        d.apply_shift();
        d
    }

    /// Apply a new configuration (hot reload) without losing baselines or an open episode.
    pub fn reconfigure(&mut self, cfg: HypeConfig) {
        self.threshold = cfg.threshold;
        self.cfg = cfg;
        self.apply_shift();
    }

    fn apply_shift(&mut self) {
        for c in COMPS {
            self.shifted[c.idx()] = self.cfg.shift.iter().any(|s| s == c.shift_key());
        }
    }

    /// Live threshold (`clips.hype.threshold`).
    pub fn set_threshold(&mut self, t: f64) {
        if t > 0.0 {
            self.threshold = t;
        }
    }

    pub fn threshold(&self) -> f64 {
        self.threshold
    }

    pub fn active(&self) -> bool {
        !matches!(self.episode, Episode::Calm)
    }

    fn screen_ts(&self, comp: Comp, ts: Ts, delay_ns: u64) -> Ts {
        if self.shifted[comp.idx()] { ts.saturating_sub(delay_ns) } else { ts }
    }

    /// The bucket for a screen time; late arrivals land in the oldest unscored bucket.
    fn bucket(&mut self, ts: Ts) -> Option<&mut Bucket> {
        let mut idx = ts / BUCKET_NS;
        if let Some(n) = self.next_eval {
            if idx < n {
                idx = n;
            }
            if idx >= n + RING as u64 {
                return None;
            }
        }
        let slot = &mut self.ring[(idx % RING as u64) as usize];
        if slot.idx != idx {
            *slot = Bucket::empty(idx);
        }
        Some(slot)
    }

    /// Feed one input observed at master time `ts`.
    pub fn input(&mut self, ts: Ts, input: &HypeInput, delay_ns: u64) {
        match input {
            HypeInput::Chat { user, text, emotes } => {
                let t_chat = self.screen_ts(Comp::Chat, ts, delay_ns);
                let t_emote = self.screen_ts(Comp::Emotes, ts, delay_ns);
                let t_vote = self.screen_ts(Comp::Votes, ts, delay_ns);
                let trimmed = text.trim();
                let cmd = self.cfg.clip_command.as_str();
                if !cmd.is_empty() && trimmed.split_whitespace().next().is_some_and(|w| w.eq_ignore_ascii_case(cmd)) {
                    if !self.votes.iter().any(|(_, u)| u == user) {
                        self.votes.push_back((t_vote, user.clone()));
                    }
                    return;
                }
                let n_emotes = emotes.unwrap_or_else(|| trimmed.split_whitespace().filter(|w| self.cfg.emotes.iter().any(|e| e == w)).count() as u32);
                let norm = normalize(trimmed);
                let window = self.cfg.chat_window.0 * 1_000_000;
                while self.recent.front().is_some_and(|(t, _, _)| t_chat.saturating_sub(*t) > window) {
                    self.recent.pop_front();
                }
                let repeat = norm.len() >= 2 && self.recent.iter().any(|(_, n, u)| *n == norm && u != user);
                if self.recent.len() < 2000 {
                    self.recent.push_back((t_chat, norm, user.clone()));
                }
                if let Some(b) = self.bucket(t_chat) {
                    b.chat += 1;
                    if repeat {
                        b.copypasta += 1;
                    }
                }
                if n_emotes > 0
                    && let Some(b) = self.bucket(t_emote)
                {
                    b.emotes += n_emotes;
                }
            }
            HypeInput::Bits(bits) => self.impulse_in(Comp::Bits, ts, delay_ns, (1.0 + *bits as f64 / 100.0).log10()),
            HypeInput::Sub { tier } => self.impulse_in(Comp::Subs, ts, delay_ns, [1.0, 1.0, 2.0, 4.0][(*tier).min(3) as usize]),
            HypeInput::Gift { count } => self.impulse_in(Comp::Gifts, ts, delay_ns, (1.0 + *count as f64).log2()),
            HypeInput::Raid { viewers } => self.impulse_in(Comp::Raid, ts, delay_ns, (1.0 + *viewers as f64 / 10.0).log10()),
            HypeInput::Tip { amount } => self.impulse_in(Comp::Tip, ts, delay_ns, (1.0 + amount).log10()),
            HypeInput::HypeTrain => self.impulse_in(Comp::HypeTrain, ts, delay_ns, 1.0),
            HypeInput::Drop => self.impulse_in(Comp::Drop, ts, delay_ns, 1.0),
        }
    }

    fn impulse_in(&mut self, comp: Comp, ts: Ts, delay_ns: u64, amount: f64) {
        let t = self.screen_ts(comp, ts, delay_ns);
        let k = IMPULSES.iter().position(|c| *c == comp).expect("impulse category");
        if let Some(b) = self.bucket(t) {
            b.impulses[k] += amount;
        }
    }

    /// One sample of local signals at master time `ts` (`None` = signal absent).
    pub fn sample(&mut self, ts: Ts, mic: Option<f32>, music: Option<f32>, chat_rate: Option<f32>, delay_ns: u64) {
        if let Some(b) = self.bucket(ts) {
            if let Some(m) = mic
                && (b.mic_n as usize) < MIC_PER_BUCKET
            {
                b.mic[b.mic_n as usize] = m.max(0.0);
                b.mic_n += 1;
            }
            if let Some(m) = music {
                b.music = b.music.max(m.max(0.0));
                b.music_n += 1;
            }
        }
        if let Some(r) = chat_rate {
            let t = self.screen_ts(Comp::Chat, ts, delay_ns);
            if let Some(b) = self.bucket(t) {
                b.chat_sig = b.chat_sig.max(r.max(0.0));
                b.chat_sig_n += 1;
            }
        }
    }

    /// Score every bucket whose screen time is at least `delay + settle` old. Returns markers
    /// that completed.
    pub fn advance(&mut self, now: Ts, delay_ns: u64) -> Vec<HypeMarker> {
        let lag = delay_ns + self.cfg.settle.0 * 1_000_000;
        let Some(ready) = now.checked_sub(lag) else { return Vec::new() };
        let last_complete = (ready / BUCKET_NS).checked_sub(1);
        let Some(last_complete) = last_complete else { return Vec::new() };
        let mut next = match self.next_eval {
            Some(n) => n,
            None => {
                self.calm_since = last_complete * BUCKET_NS;
                last_complete
            }
        };
        // after a long stall (suspend, debugger) skip ahead instead of scoring stale buckets
        if last_complete >= next + RING as u64 {
            next = last_complete + 1 - (RING as u64 / 2);
        }
        let mut out = Vec::new();
        while next <= last_complete {
            if let Some(m) = self.evaluate(next) {
                out.push(m);
            }
            next += 1;
            self.next_eval = Some(next);
        }
        out
    }

    fn slot(&self, idx: u64) -> Option<&Bucket> {
        let b = &self.ring[(idx % RING as u64) as usize];
        (b.idx == idx).then_some(b)
    }

    fn evaluate(&mut self, idx: u64) -> Option<HypeMarker> {
        let has_mic = self.slot(idx).is_some_and(|b| b.mic_n > 0);
        let laughter = if has_mic { self.laughter(idx) } else { 0.0 };
        let cfg = &self.cfg;
        let w = &cfg.weights;
        let dt = BUCKET_NS as f64 / NS;
        let s = (idx + 1) * BUCKET_NS; // screen time at the end of this bucket
        self.evaluated += 1;
        let cur = self.slot(idx).copied().unwrap_or_else(|| Bucket::empty(idx));
        let mut c = [0.0f64; COMPS.len()];

        // chat + emote rates over the short window vs slow baselines
        let win = (cfg.chat_window.0 * 1_000_000 / BUCKET_NS).max(1);
        let (mut chat, mut emotes, mut copy) = (0u64, 0u64, 0u64);
        for i in idx.saturating_sub(win - 1)..=idx {
            if let Some(b) = self.slot(i) {
                chat += b.chat as u64;
                emotes += b.emotes as u64;
                copy += b.copypasta as u64;
            }
        }
        let win_s = win as f64 * dt;
        let chat_rate = chat as f64 / win_s;
        let emote_rate = emotes as f64 / win_s;
        let base_tau = (cfg.baseline.0 as f64 / 1000.0).max(1.0);
        // baselines need a while before a ratio against them means anything
        let warm = self.evaluated as f64 * dt >= (3.0 * cfg.chat_window.0 as f64 / 1000.0).max(WARMUP_S);
        let ratio = |x: f64, base: &Ema, floor: f64| x / base.mean.max(floor);
        if warm {
            c[Comp::Chat.idx()] = w.chat * ratio(chat_rate, &self.chat_base, cfg.min_chat_rate).ln().clamp(0.0, 3.0);
            c[Comp::Emotes.idx()] = w.emotes * ratio(emote_rate, &self.emote_base, cfg.min_emote_rate).ln().clamp(0.0, 3.0);
            c[Comp::Copypasta.idx()] = (w.copypasta * copy as f64).min(1.5);
        }
        self.chat_base.update(chat_rate, dt, base_tau);
        self.emote_base.update(emote_rate, dt, base_tau);
        if cur.chat_sig_n > 0 {
            let r = cur.chat_sig as f64;
            if warm && self.chat_sig_base.n > 0 {
                let sig = w.chat * ratio(r, &self.chat_sig_base, 1e-3).ln().clamp(0.0, 3.0);
                // the adapter's rate and our own count measure the same thing: take the larger
                c[Comp::Chat.idx()] = c[Comp::Chat.idx()].max(sig);
            }
            self.chat_sig_base.update(r, dt, base_tau);
        }

        // `!clip` votes: unique voters within the vote window
        let vote_win = cfg.vote_window.0 * 1_000_000;
        while self.votes.front().is_some_and(|(t, _)| s.saturating_sub(*t) > vote_win) {
            self.votes.pop_front();
        }
        let voters = self.votes.iter().filter(|(t, _)| *t <= s).count();
        let mut forced = false;
        if voters > 0 {
            c[Comp::Votes.idx()] = w.votes * voters as f64;
            if cfg.clip_votes > 0 && voters >= cfg.clip_votes {
                c[Comp::Votes.idx()] = c[Comp::Votes.idx()].max(self.threshold * 1.01);
                forced = true;
            }
        }

        // event impulses decay with `impulse_tau`
        let decay = (-dt / (cfg.impulse_tau.0 as f64 / 1000.0).max(0.1)).exp();
        let iw = [w.bits, w.sub, w.gift, w.raid, w.tip, w.hype_train, w.drop];
        for (k, comp) in IMPULSES.iter().enumerate() {
            self.impulse[k] = self.impulse[k] * decay + cur.impulses[k];
            c[comp.idx()] = iw[k] * self.impulse[k];
        }

        // mic spike (z-score vs baseline) and laughter
        if cur.mic_n > 0 {
            let samples = &cur.mic[..cur.mic_n as usize];
            let peak = samples.iter().copied().fold(0.0f32, f32::max) as f64;
            let mean = samples.iter().map(|x| *x as f64).sum::<f64>() / samples.len() as f64;
            let sd = self.mic_base.std().max(0.02);
            if self.mic_base.n as f64 * dt > 5.0 {
                let z = (peak - self.mic_base.mean) / sd;
                c[Comp::Mic.idx()] = w.mic * ((z - 2.5) / 2.0).clamp(0.0, 2.0);
                c[Comp::Laughter.idx()] = w.laughter * laughter;
            }
            self.mic_base.update(mean, dt, (base_tau / 5.0).max(1.0));
        }

        // music novelty: short level vs long level jump (drops, big entrances)
        if cur.music_n > 0 {
            let x = cur.music as f64;
            let a = (dt / 0.4).min(1.0);
            self.music_short += a * (x - self.music_short);
            if self.music_long.n as f64 * dt > 4.0 && self.music_long.mean > 0.02 {
                let nov = self.music_short / self.music_long.mean - 1.0;
                c[Comp::Novelty.idx()] = w.novelty * ((nov - 0.6) / 1.0).clamp(0.0, 1.5);
            }
            self.music_long.update(x, dt, 8.0);
        }

        let score: f64 = c.iter().sum();
        self.score = score;
        self.components = c;
        self.last_eval = s;
        self.step_episode(s, score, c, forced)
    }

    /// Bursty mic energy at 3–8 Hz above the baseline over the last 2 s (0 … 1.5).
    fn laughter(&mut self, idx: u64) -> f64 {
        let mut xs = std::mem::take(&mut self.laugh_scratch);
        xs.clear();
        for i in idx.saturating_sub(LAUGH_BUCKETS as u64 - 1)..=idx {
            if let Some(b) = self.slot(i) {
                xs.extend_from_slice(&b.mic[..b.mic_n as usize]);
            }
        }
        let mut strength = 0.0;
        if xs.len() >= 20 {
            let n = xs.len() as f64;
            let mean = xs.iter().map(|x| *x as f64).sum::<f64>() / n;
            let (lo, hi) = xs.iter().fold((f32::MAX, f32::MIN), |(lo, hi), x| (lo.min(*x), hi.max(*x)));
            let range = (hi - lo) as f64;
            let elevated = mean > self.mic_base.mean + self.mic_base.std().max(0.02);
            if elevated && range > 0.05 {
                let period_ms = 1000.0 / SAMPLE_HZ as f64;
                let mut last_peak: Option<usize> = None;
                let mut regular = 0u32;
                for i in 1..xs.len() - 1 {
                    let x = xs[i];
                    if !(x > xs[i - 1] && x >= xs[i + 1]) {
                        continue;
                    }
                    let a = i.saturating_sub(3);
                    let b = (i + 4).min(xs.len());
                    let local_min = xs[a..b].iter().copied().fold(f32::MAX, f32::min);
                    if ((x - local_min) as f64) < 0.3 * range {
                        continue;
                    }
                    if let Some(p) = last_peak {
                        let gap = (i - p) as f64 * period_ms;
                        if (120.0..=350.0).contains(&gap) {
                            regular += 1;
                        }
                    }
                    last_peak = Some(i);
                }
                if regular >= 4 {
                    strength = (regular as f64 / 6.0).min(1.5);
                }
            }
        }
        self.laugh_scratch = xs;
        strength
    }

    fn step_episode(&mut self, s: Ts, score: f64, comps: [f64; COMPS.len()], forced: bool) -> Option<HypeMarker> {
        let th = self.threshold;
        let rel = th * self.cfg.release;
        let hold = self.cfg.hold.0 * 1_000_000;
        let gap = self.cfg.merge_gap.0 * 1_000_000;
        let max_len = self.cfg.max_len.0 * 1_000_000;
        let calm_level = th * 0.35;
        let mut done = None;
        self.episode = match std::mem::replace(&mut self.episode, Episode::Calm) {
            Episode::Calm => {
                if score >= th || forced {
                    Episode::Active { rise: self.calm_since, peak: s, peak_score: score, comps, last_above: s, below_since: None }
                } else {
                    if score < calm_level {
                        self.calm_since = s;
                    }
                    Episode::Calm
                }
            }
            Episode::Active { rise, mut peak, mut peak_score, comps: mut pc, mut last_above, below_since } => {
                if score > peak_score {
                    peak = s;
                    peak_score = score;
                    pc = comps;
                }
                if score >= rel {
                    last_above = s;
                }
                let below_since = if score < rel { below_since.or(Some(s)) } else { None };
                let too_long = s.saturating_sub(rise) >= max_len;
                if below_since.is_some_and(|b| s - b >= hold) || too_long {
                    Episode::Closing { rise, peak, peak_score, comps: pc, last_above, closed_at: s }
                } else {
                    Episode::Active { rise, peak, peak_score, comps: pc, last_above, below_since }
                }
            }
            Episode::Closing { rise, peak, peak_score, comps: pc, last_above, closed_at } => {
                let too_long = s.saturating_sub(rise) >= max_len;
                if (score >= th || forced) && !too_long {
                    let (peak, peak_score, pc) = if score > peak_score { (s, score, comps) } else { (peak, peak_score, pc) };
                    Episode::Active { rise, peak, peak_score, comps: pc, last_above: s, below_since: None }
                } else if s - closed_at >= gap || too_long {
                    done = Some(self.make_marker(rise, peak, peak_score, pc, last_above));
                    // votes that fed this marker are spent
                    self.votes.retain(|(t, _)| *t > s);
                    self.calm_since = s;
                    Episode::Calm
                } else {
                    Episode::Closing { rise, peak, peak_score, comps: pc, last_above, closed_at }
                }
            }
        };
        done
    }

    fn make_marker(&self, rise: Ts, peak: Ts, score: f64, comps: [f64; COMPS.len()], last_above: Ts) -> HypeMarker {
        let ms = 1_000_000u64;
        let pre_min = self.cfg.preroll_min.0 * ms;
        let pre_max = self.cfg.preroll_max.0 * ms;
        let post = self.cfg.postroll.0 * ms;
        let max_len = self.cfg.max_len.0 * ms;
        // start a little before the build-up, but always 10–30 s before the peak
        let lead_in = rise.saturating_sub(2_000 * ms);
        let start = lead_in.clamp(peak.saturating_sub(pre_max), peak.saturating_sub(pre_min));
        let end = (last_above + post).max(peak + post).min(start + max_len).max(peak + ms);
        let mut ranked: Vec<(Comp, f64)> = COMPS.iter().map(|c| (*c, comps[c.idx()])).filter(|(_, v)| *v > 0.0).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let cut = (score * 0.15).max(0.05);
        let reasons: Vec<String> = ranked.iter().filter(|(_, v)| *v >= cut).map(|(c, _)| c.name().to_string()).collect();
        HypeMarker {
            start,
            peak,
            end,
            score,
            reasons: if reasons.is_empty() { ranked.first().map(|(c, _)| vec![c.name().to_string()]).unwrap_or_default() } else { reasons },
            components: ranked.iter().map(|(c, v)| (c.name().to_string(), *v)).collect(),
        }
    }
}

/// Copypasta key: lowercase, words only, repeated letters collapsed ("LULLLL" = "lul").
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = '\0';
    for ch in s.chars().flat_map(char::to_lowercase) {
        let ch = if ch.is_alphanumeric() { ch } else { ' ' };
        if ch == last && ch != ' ' {
            continue;
        }
        if ch == ' ' && (last == ' ' || out.is_empty()) {
            last = ch;
            continue;
        }
        out.push(ch);
        last = ch;
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_core::config::Dur;

    const S: u64 = 1_000_000_000;

    /// Drive a detector through `secs` seconds of synthetic stream at 50 Hz sampling.
    struct Sim {
        d: HypeDetector,
        t: Ts,
        delay: u64,
        markers: Vec<HypeMarker>,
        max_score: f64,
    }

    impl Sim {
        fn new(cfg: HypeConfig, delay_s: f64) -> Sim {
            Sim { d: HypeDetector::new(cfg), t: 1_000 * S, delay: (delay_s * 1e9) as u64, markers: Vec::new(), max_score: 0.0 }
        }
        /// Run `secs`; `chat(t_rel)` returns messages per second at that moment, `mic` the level.
        fn run(&mut self, secs: f64, mut each: impl FnMut(&mut HypeDetector, Ts, f64, u64)) {
            let steps = (secs * SAMPLE_HZ as f64) as u64;
            let dt = S / SAMPLE_HZ as u64;
            for i in 0..steps {
                let rel = i as f64 / SAMPLE_HZ as f64;
                each(&mut self.d, self.t, rel, self.delay);
                self.markers.extend(self.d.advance(self.t, self.delay));
                self.max_score = self.max_score.max(self.d.score);
                self.t += dt;
            }
        }
    }

    fn chat(d: &mut HypeDetector, t: Ts, delay: u64, user: &str, text: &str) {
        d.input(t, &HypeInput::Chat { user: user.into(), text: text.into(), emotes: None }, delay);
    }

    /// Background chat of ~`rate` msgs/s from distinct users with distinct text.
    fn background(d: &mut HypeDetector, t: Ts, rel: f64, delay: u64, rate: f64) {
        let i = (rel * SAMPLE_HZ as f64) as u64;
        let every = (SAMPLE_HZ as f64 / rate).round().max(1.0) as u64;
        if i.is_multiple_of(every) {
            chat(d, t, delay, &format!("u{i}"), &format!("message number {i} about the song"));
        }
    }

    #[test]
    fn quiet_stream_never_fires_and_chat_burst_marks_the_moment_on_screen() {
        let mut sim = Sim::new(HypeConfig::default(), 4.0);
        // 3 minutes of normal chat (0.5 msg/s) and speech-level mic
        sim.run(180.0, |d, t, rel, delay| {
            background(d, t, rel, delay, 0.5);
            d.sample(t, Some(0.2 + 0.02 * ((rel * 3.0).sin() as f32)), Some(0.4), None, delay);
        });
        assert!(sim.markers.is_empty(), "false positive: {:?}", sim.markers);
        assert!(sim.max_score < 1.0, "quiet score {}", sim.max_score);

        // the moment happens on screen at T; chat reacts 4 s (delay) + 1 s (reaction) later
        let moment = sim.t + 20 * S;
        sim.run(80.0, |d, t, rel, delay| {
            background(d, t, rel, delay, 0.5);
            d.sample(t, Some(0.2), Some(0.4), None, delay);
            let react = moment + 5 * S;
            if t >= react && t < react + 8 * S {
                // 6 msgs/s with emotes
                let i = (rel * SAMPLE_HZ as f64) as u64;
                if i.is_multiple_of(8) {
                    chat(d, t, delay, &format!("hype{i}"), "LUL LUL KEKW that was insane");
                }
            }
        });
        assert_eq!(sim.markers.len(), 1, "{:?}", sim.markers);
        let m = &sim.markers[0];
        assert!(m.score >= 1.0);
        assert!(m.reasons.contains(&"chat".to_string()), "{:?}", m.reasons);
        assert!(m.reasons.contains(&"emotes".to_string()), "{:?}", m.reasons);
        // shifted back by the 4 s delay: the peak lies within the reaction window on screen
        let lo = moment + S; // reaction starts 1 s after the moment on screen
        assert!(m.peak >= lo && m.peak <= lo + 9 * S, "peak {} vs moment {}", (m.peak as f64 - moment as f64) / 1e9, moment);
        // windows start 10–30 s before the peak and end after it
        let pre = m.peak - m.start;
        assert!((10 * S..=30 * S).contains(&pre), "preroll {}", pre as f64 / 1e9);
        assert!(m.end > m.peak && m.end - m.start <= 60 * S);
    }

    #[test]
    fn stream_delay_moves_the_peak_back_by_exactly_the_delay() {
        let peaks: Vec<Ts> = [2.0, 12.0]
            .iter()
            .map(|delay_s| {
                let mut sim = Sim::new(HypeConfig::default(), *delay_s);
                let start = sim.t;
                sim.run(160.0, |d, t, rel, delay| {
                    background(d, t, rel, delay, 0.3);
                    // chat burst always arrives at start+100 s (wall time)
                    if t >= start + 100 * S && t < start + 106 * S && ((rel * SAMPLE_HZ as f64) as u64).is_multiple_of(5) {
                        chat(d, t, delay, &format!("x{rel}"), &format!("wow {rel}"));
                    }
                });
                assert_eq!(sim.markers.len(), 1, "delay {delay_s}: {:?}", sim.markers);
                sim.markers[0].peak - start
            })
            .collect();
        let diff = peaks[0] as i64 - peaks[1] as i64;
        assert!((diff - 10 * S as i64).abs() <= 100_000_000, "peak shift {} s", diff as f64 / 1e9);
    }

    #[test]
    fn events_and_votes_contribute_and_votes_force_a_marker() {
        let mut sim = Sim::new(HypeConfig::default(), 3.0);
        sim.run(90.0, |d, t, rel, delay| background(d, t, rel, delay, 0.4));
        // a lone 100-bit cheer is not a moment
        let t = sim.t;
        sim.d.input(t, &HypeInput::Bits(100), sim.delay);
        sim.run(30.0, |d, t, rel, delay| background(d, t, rel, delay, 0.4));
        assert!(sim.markers.is_empty(), "{:?}", sim.markers);
        // a 50-gift bomb is
        let t = sim.t;
        sim.d.input(t, &HypeInput::Gift { count: 50 }, sim.delay);
        sim.run(40.0, |d, t, rel, delay| background(d, t, rel, delay, 0.4));
        assert_eq!(sim.markers.len(), 1);
        assert_eq!(sim.markers[0].reasons[0], "gifts");
        // gifts are not viewer reactions: the peak is not shifted back
        assert!(sim.markers[0].peak >= t && sim.markers[0].peak <= t + S);
        // three `!clip` votes from different users force a marker; repeats don't count
        let t = sim.t;
        for u in ["a", "a", "b"] {
            chat(&mut sim.d, t, sim.delay, u, "!clip");
        }
        sim.run(40.0, |d, t, rel, delay| background(d, t, rel, delay, 0.4));
        assert_eq!(sim.markers.len(), 1, "two voters must not force a marker");
        let t = sim.t;
        for u in ["c", "d", "e"] {
            chat(&mut sim.d, t, sim.delay, u, "!clip that");
        }
        sim.run(40.0, |d, t, rel, delay| background(d, t, rel, delay, 0.4));
        assert_eq!(sim.markers.len(), 2);
        assert!(sim.markers[1].reasons.contains(&"votes".to_string()));
    }

    #[test]
    fn mic_spike_and_laughter_score_against_the_mic_baseline() {
        let mut sim = Sim::new(HypeConfig::default(), 3.0);
        sim.run(60.0, |d, t, rel, delay| d.sample(t, Some(0.15 + 0.01 * ((rel * 7.0).sin() as f32)), None, None, delay));
        assert!(sim.markers.is_empty());
        // 3 s of laughter: bursts at 5 Hz well above the baseline
        sim.run(3.0, |d, t, rel, delay| {
            let phase = (rel * 5.0).fract();
            d.sample(t, Some(if phase < 0.4 { 0.75 } else { 0.3 }), None, None, delay);
        });
        let laugh = sim.d.components[Comp::Laughter.idx()];
        let mic = sim.d.components[Comp::Mic.idx()];
        // advance past the settle lag so the laughter buckets are scored
        sim.run(20.0, |d, t, rel, delay| d.sample(t, Some(0.15 + 0.01 * ((rel * 7.0).sin() as f32)), None, None, delay));
        assert!(laugh >= 0.0 && mic >= 0.0);
        assert_eq!(sim.markers.len(), 1, "laughter + spike should mark: {:?}", sim.markers);
        let r = &sim.markers[0].reasons;
        assert!(r.contains(&"laughter".to_string()) && r.contains(&"mic".to_string()), "{r:?}");
    }

    #[test]
    fn long_hype_is_capped_and_close_episodes_merge() {
        let cfg = HypeConfig { max_len: Dur(40_000), ..HypeConfig::default() };
        let mut sim = Sim::new(cfg, 0.0);
        sim.run(60.0, |d, t, rel, delay| background(d, t, rel, delay, 0.3));
        // two raids 5 s apart → one merged marker
        let t0 = sim.t;
        sim.d.input(t0, &HypeInput::Raid { viewers: 500 }, 0);
        sim.run(5.0, |_, _, _, _| {});
        sim.d.input(sim.t, &HypeInput::Raid { viewers: 500 }, 0);
        sim.run(60.0, |_, _, _, _| {});
        assert_eq!(sim.markers.len(), 1, "{:?}", sim.markers);
        // a hype that never calms down is split at max_len
        sim.run(120.0, |d, t, rel, _| {
            if ((rel * SAMPLE_HZ as f64) as u64).is_multiple_of(50) {
                d.input(t, &HypeInput::Drop, 0);
                d.input(t, &HypeInput::Bits(2_000), 0);
            }
        });
        assert!(sim.markers.len() >= 3, "{:?}", sim.markers.len());
        for m in &sim.markers {
            assert!(m.end - m.start <= 40 * S);
        }
    }

    #[test]
    fn classifies_normalized_events_without_double_counting_gifts() {
        let gifted = Event::new("twitch.sub", se_proto::Origin::Twitch, Value::map().with("tier", 1).with("is_gift", true));
        assert_eq!(classify(&gifted), None);
        let sub = Event::new("twitch.sub", se_proto::Origin::Twitch, Value::map().with("tier", "3000"));
        assert_eq!(classify(&sub), Some(HypeInput::Sub { tier: 3 }));
        let frag = Value::List(vec![Value::map().with("type", "emote"), Value::map().with("type", "text"), Value::map().with("type", "emote")]);
        let chat = Event::new("twitch.chat", se_proto::Origin::Twitch, Value::map().with("message", "a b").with("user", "z").with("fragments", frag));
        assert_eq!(classify(&chat), Some(HypeInput::Chat { user: "z".into(), text: "a b".into(), emotes: Some(2) }));
        assert_eq!(normalize("LULLLL   lul!!"), "lul lul");
    }

    #[test]
    fn marker_value_and_description() {
        let m = HypeMarker { start: 1, peak: 2, end: 3, score: 1.23456, reasons: vec!["chat".into()], components: vec![("chat".into(), 1.23456)] };
        let v = m.to_value();
        assert_eq!(v.get_path("kind").and_then(Value::as_str), Some("hype"));
        assert_eq!(v.get_path("score").and_then(Value::as_f64), Some(1.235));
        assert_eq!(v.get_path("components.chat").and_then(Value::as_f64), Some(1.235));
        assert!(m.description().starts_with("hype 1.2: chat"));
    }
}
