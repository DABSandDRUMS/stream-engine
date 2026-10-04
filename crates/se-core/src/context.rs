//! Context layer and operator effects control (docs/context.md).
//!
//! * **Operator control:** `fx.enabled` (`fx.on|off|toggle`), `fx.auto`
//!   (`fx.auto.on|off|toggle`) and `lights.auto` (`lights.auto.on|off|toggle`) persist.
//!   `fx.auto` defaults off and gates only the musical director, not notifications/viewer effects.
//!   The independent effects kill switch still refuses non-operator effects while off.
//! * **Context signals:** `context.song|drums|chat|energy|talking|budget`, computed every tick
//!   from the analysis signals with fixed-size state only (no per-tick allocation).
//! * **Context events:** `context.peak|settle|song_peak|fill_landed|mood`, each with hysteresis
//!   and a cooldown so they stay occasional; `context.mood` is also state.
//!
//! Every threshold that shapes *when* something fires is in `[context]` ([`ContextDef`]); the
//! constants below only shape the 0–1 scales and are documented where they are used.

use super::{Core, Ctx, MS};
use crate::config::{Config, ContextDef, PresetDef};
use se_proto::{Event, Meta, Origin, PRIORITY_CHAT, Role, Ts, Value};
use std::collections::{BTreeMap, VecDeque};

/// Operator effects switch.
pub const FX_ENABLED: &str = "fx.enabled";
/// Operator context-driven video automation switch (opt-in).
pub const FX_AUTO: &str = "fx.auto";
/// Operator lights automation switch (rules check it before driving lights).
pub const LIGHTS_AUTO: &str = "lights.auto";
/// Current mood (state).
pub const MOOD: &str = "context.mood";
/// Owner token of the lighting layers automation drives.
pub const LIGHTS_OWNER: &str = "context";
/// Fade when `lights.auto.off` hands the automation's layers back (a release, never a cut).
const LIGHTS_AUTO_OFF_FADE: &str = "3s";
/// The error a refused effect gets (rewards refund on it).
pub const FX_OFF: &str = "effects are off";
/// A context-driven video effect was refused by the automation master.
pub const FX_AUTO_OFF: &str = "automatic effects are off";
pub const FX_PRESET: &str = "context.fx.preset";
pub const FX_REASON: &str = "context.fx.reason";
pub const FX_NEXT_AT: &str = "context.fx.next_at";
const FX_HEALTH: &str = "health.context.fx";
const MUSICAL_RELEASE: Ts = 3_000 * MS;
const GRID_STALL: Ts = 2_000 * MS;
const FX_RETRY: Ts = 16_000 * MS;


/// Actions handled here.
pub fn is_action(name: &str) -> bool {
    matches!(name, "fx.on" | "fx.off" | "fx.toggle" | "fx.auto.on" | "fx.auto.off" | "fx.auto.toggle" | "lights.auto.on" | "lights.auto.off" | "lights.auto.toggle" | "context.spend")
}

// ---- inputs ------------------------------------------------------------------------------

const IN: [&str; 20] = [
    "music.level",
    "music.lufs",
    "music.flux",
    "music.kick",
    "music.snare",
    "music.hat",
    "music.bass",
    "music.mid",
    "music.high",
    "music.centroid",
    "band.level",
    "band.kick",
    "band.snare",
    "band.hat",
    "beat.bpm",
    "beat.position",
    "beat.confidence",
    "mic.level",
    "mic.talking",
    "twitch.chat_rate",
];
const M_LEVEL: usize = 0;
const M_LUFS: usize = 1;
const M_FLUX: usize = 2;
const M_KICK: usize = 3;
const M_SNARE: usize = 4;
const M_HAT: usize = 5;
const M_BASS: usize = 6;
const M_MID: usize = 7;
const M_HIGH: usize = 8;
const M_CENTROID: usize = 9;
const B_LEVEL: usize = 10;
const B_KICK: usize = 11;
const B_SNARE: usize = 12;
const B_HAT: usize = 13;
const BPM: usize = 14;
const BEAT_POS: usize = 15;
const BEAT_CONF: usize = 16;
const MIC_LEVEL: usize = 17;
const MIC_TALKING: usize = 18;
const CHAT_RATE: usize = 19;

/// Published signals, in the order `tick_context` writes them.
const OUT: [&str; 6] = ["context.song", "context.drums", "context.chat", "context.energy", "context.talking", "context.budget"];
const O_BUDGET: usize = 5;

// ---- scale constants (see docs/context.md) -------------------------------------------------

/// Song audio counts as playing above this loudness (LUFS short-term, else RMS dBFS)…
const SONG_FLOOR_DB: f32 = -45.0;
/// …after it has stayed on that side this long (s).
const SONG_DEBOUNCE: f32 = 1.0;
/// A song coming back after less than this off (a break, a talk-over) keeps its baseline;
/// a longer gap (or `queue.song_started`) counts as a new song.
const SONG_GAP: Ts = 10_000 * MS;
/// The drummer counts as playing above this band level (dBFS).
const BAND_FLOOR_DB: f32 = -45.0;
/// The band is quiet enough for talking below this level (dBFS, smoothed).
const BAND_QUIET_DB: f32 = -40.0;
/// Fast feature smoothing (s) and the song's own running baseline (s).
const FAST_TAU: f32 = 1.5;
const SONG_BASE_TAU: f32 = 45.0;
/// The drummer's session baseline (s), updated only while playing.
const DRUM_BASE_TAU: f32 = 300.0;
/// Chat rate baseline (~10 min, s), the floor it never drops under (msgs/min), and the
/// multiple of the baseline that reads as full heat.
const CHAT_BASE_TAU: f32 = 600.0;
const CHAT_FLOOR: f32 = 3.0;
const CHAT_FULL: f32 = 3.0;
/// Event bumps on `context.chat` decay with this time constant (s).
const BUMP_TAU: f32 = 45.0;
/// Output smoothing (s).
const OUT_TAU: f32 = 0.4;
const ENERGY_TAU: f32 = 1.0;
/// Loudness above the baseline that reads as "full" (dB), and the relative flux/density rise.
const LOUD_SPAN_DB: f32 = 6.0;
/// Talking: condition held this long turns it on, its absence this long turns it off (s).
const TALK_ON: f32 = 0.5;
const TALK_OFF: f32 = 2.5;
/// Without a `mic.talking` signal, mic RMS above this counts as speech.
const MIC_SPEECH: f32 = 0.03;
/// Mood features smoothing (s), and the wait after a song starts before the first verdict (s).
const MOOD_TAU: f32 = 8.0;
const MOOD_SETTLE: Ts = 8_000 * MS;
/// Mean kick/snare/hat envelope and spectral flux of a dense, driving mix (full scale for the
/// mood's absolute energy).
const DENSE_ENVELOPES: f32 = 0.3;
const DENSE_FLUX: f32 = 1.5;
/// A beat grid is trusted for downbeats above this confidence.
const GRID_CONFIDENCE: f32 = 0.3;
const HOUR: Ts = 3_600_000 * MS;
const FILL_RING: usize = 16;
/// Snare/tom hits softer than this don't count toward a fill.
const FILL_HIT_VELOCITY: f32 = 0.15;

/// `context.mood` values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mood {
    #[default]
    None,
    Chill,
    Groove,
    Bright,
    Heavy,
    Hype,
}

impl Mood {
    pub const ALL: [&'static str; 6] = ["none", "chill", "groove", "bright", "heavy", "hype"];
    pub fn as_str(self) -> &'static str {
        match self {
            Mood::None => "none",
            Mood::Chill => "chill",
            Mood::Groove => "groove",
            Mood::Bright => "bright",
            Mood::Heavy => "heavy",
            Mood::Hype => "hype",
        }
    }
}

/// Genre keywords, checked in order against each genre (substring match, lowercase): the first
/// genre of `song.current.genres` with a match decides. More specific keywords come first
/// (`pop punk` before `punk` before `pop`).
pub const GENRE_MOODS: &[(&str, Mood)] = &[
    ("pop punk", Mood::Hype),
    ("metal", Mood::Heavy),
    ("hardcore", Mood::Heavy),
    ("djent", Mood::Heavy),
    ("grunge", Mood::Heavy),
    ("hard rock", Mood::Heavy),
    ("stoner", Mood::Heavy),
    ("punk", Mood::Hype),
    ("ska", Mood::Hype),
    ("drum and bass", Mood::Hype),
    ("lo-fi", Mood::Chill),
    ("lofi", Mood::Chill),
    ("ambient", Mood::Chill),
    ("jazz", Mood::Chill),
    ("acoustic", Mood::Chill),
    ("folk", Mood::Chill),
    ("singer-songwriter", Mood::Chill),
    ("funk", Mood::Groove),
    ("disco", Mood::Groove),
    ("hip hop", Mood::Groove),
    ("hip-hop", Mood::Groove),
    ("rap", Mood::Groove),
    ("r&b", Mood::Groove),
    ("rnb", Mood::Groove),
    ("soul", Mood::Groove),
    ("reggae", Mood::Groove),
    ("pop", Mood::Bright),
    ("edm", Mood::Bright),
    ("dance", Mood::Bright),
    ("house", Mood::Bright),
    ("electro", Mood::Bright),
    ("synth", Mood::Bright),
];

/// Mood of a genre list (`None` when no genre is known to the table).
pub fn genre_mood<S: AsRef<str>>(genres: &[S]) -> Option<Mood> {
    genres.iter().find_map(|g| {
        let g = g.as_ref().to_ascii_lowercase();
        GENRE_MOODS.iter().find(|(k, _)| g.contains(k)).map(|(_, m)| *m)
    })
}

/// Slow-moving song features a mood verdict is made from.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MoodFeatures {
    pub bpm: f32,
    /// Spectral centroid, 0–1 log-mapped.
    pub centroid: f32,
    /// Bass share of bass+mid+high.
    pub bass_ratio: f32,
    /// Sustained absolute energy 0–1 (onset density and flux, not loudness: masters are all loud).
    pub energy: f32,
}

/// Mood from the audio alone. In order: quiet → chill; fast and driving → hype; bass-heavy and
/// driving → heavy; slow and soft → chill; bright spectrum → bright; anything else → groove.
pub fn audio_mood(f: &MoodFeatures) -> Mood {
    if f.energy < 0.3 {
        Mood::Chill
    } else if f.bpm >= 150.0 && f.energy >= 0.6 {
        Mood::Hype
    } else if f.bass_ratio >= 0.55 && f.energy >= 0.55 {
        Mood::Heavy
    } else if f.bpm > 0.0 && f.bpm < 90.0 && f.energy < 0.45 {
        Mood::Chill
    } else if f.centroid >= 0.62 {
        Mood::Bright
    } else {
        Mood::Groove
    }
}

/// The verdict: a known genre wins, except that a near-silent passage (ballad, breakdown) of
/// any genre reads chill; without a known genre the audio decides.
pub fn decide_mood(f: &MoodFeatures, genre: Option<Mood>) -> Mood {
    match genre {
        Some(_) if f.energy < 0.2 => Mood::Chill,
        Some(g) => g,
        None => audio_mood(f),
    }
}

/// Musical scheduling, not wall-clock rule timers. All counters stay bounded over long shows.
#[derive(Default)]
struct LightDirector {
    enabled: bool,
    source: &'static str,
    source_since: Option<Ts>,
    mood: Mood,
    song_started: bool,
    song_grace: Ts,
    songs: u8,
    palette_span: u8,
    palette_turn: u8,
    motion_turn: u8,
    motion_due: Option<f64>,
    last_beat: f64,
    energy: &'static str,
    energy_target: &'static str,
    energy_since: Option<Ts>,
    family: &'static str,
    family_target: &'static str,
    family_since: Option<Ts>,
    build_until: f64,
    section: bool,
    drop: bool,
    peak_until: Ts,
    special: &'static str,
    special_until: Ts,
    special_pending: bool,
    controls_at: Ts,
    last_level: f32,
}

#[derive(Default, Debug)]
struct LightDecision {
    palette: bool,
    motion: bool,
    controls: bool,
    idle: bool,
    special: bool,
    drop: bool,
}

impl LightDirector {
    fn update(&mut self, now: Ts, enabled: bool, source: &'static str, mood: Mood, level: f32, rising: bool, beat: f64) -> LightDecision {
        let beat = if beat.is_finite() { beat } else { self.last_beat };
        let mut out = LightDecision::default();
        if !enabled {
            // Do not replay a raid, drop or queued song start when automation is re-enabled.
            *self = Self { last_beat: beat, ..Self::default() };
            return out;
        }
        if beat < self.last_beat && self.family == "build" {
            self.build_until = beat + 32.0;
        }
        let first = !self.enabled;
        self.enabled = true;
        let restored = !self.special.is_empty() && now >= self.special_until;
        if restored {
            self.special = "";
        }
        let special = self.special_pending && !self.special.is_empty() && now < self.special_until;
        self.special_pending = false;
        let source = if !self.special.is_empty() && source != "talking" { "celebration" } else { source };
        let source_change = source != self.source;
        // Enter music/talking promptly; wait two seconds before abandoning live drums for idle.
        let source_ready = source != "idle" || held(&mut self.source_since, source_change, now) >= 2_000 * MS;
        if !source_change {
            self.source_since = None;
        }
        let changed = source_change && (source_ready || first || restored);
        if changed {
            self.source = source;
            self.source_since = None;
        }
        let mood = if self.source == "talking" { Mood::Chill } else if mood == Mood::None { Mood::Groove } else { mood };
        let mood_change = self.mood != mood;
        self.mood = mood;
        let song = self.song_started;
        self.song_started = false;
        if song {
            self.songs = self.songs.saturating_add(1);
        }
        let palette_due = self.palette_span == 0 || self.songs >= self.palette_span;
        out.palette = self.source != "idle" && (first || changed || mood_change || palette_due || restored);
        if out.palette && self.special.is_empty() {
            self.songs = 0;
            self.palette_turn = (self.palette_turn + 1) % 6;
            self.palette_span = 2 + self.palette_turn % 2;
        }
        if special {
            out.special = true;
            out.palette = false;
        } else if !self.special.is_empty() {
            out.palette = false;
        }

        let drop = self.drop && self.source == "track";
        self.drop = false;
        let section = self.section && self.source == "track";
        self.section = false;
        if drop && level >= 0.85 {
            self.peak_until = now + 4_000 * MS;
        }
        let wanted = if self.source == "talking" {
            "low"
        } else if self.source == "celebration" {
            "high"
        } else if drop {
            if level >= 0.85 { "peak" } else { "high" }
        } else if now < self.peak_until && level >= 0.85 {
            "peak"
        } else if level < 0.35 {
            "low"
        } else if level < 0.65 {
            "mid"
        } else {
            "high"
        };
        let energy_change = wanted != self.energy;
        if wanted != self.energy_target {
            self.energy_target = wanted;
            self.energy_since = None;
        }
        let immediate = first || changed || section || drop || self.energy == "peak";
        let stable = held(&mut self.energy_since, energy_change, now) >= 3_000 * MS;
        if energy_change && (immediate || stable) {
            self.energy = wanted;
            self.energy_since = None;
            out.motion = true;
        }
        let family = if self.energy == "peak" {
            "strobe"
        } else if self.source == "talking" {
            "ambient"
        } else if self.source == "track" && self.energy == "low" {
            "breakdown"
        } else if self.source == "track" && !drop && self.energy == "high"
            && (rising || (self.family == "build" && beat < self.build_until)) {
            "build"
        } else {
            "motion"
        };
        let family_change = family != self.family;
        if family != self.family_target {
            self.family_target = family;
            self.family_since = None;
        }
        let family_stable = held(&mut self.family_since, family_change, now) >= 3_000 * MS;
        if family_change && (out.motion || immediate || family_stable) {
            if family == "build" {
                // Allow the queued sixteen-beat entrance and one full phrase to play before
                // a fading slope alone replaces it. Sections, drops and energy changes win.
                self.build_until = beat + 32.0;
            }
            self.family = family;
            self.family_since = None;
            out.motion = true;
        }
        // A repositioned clock rebases the deadline; it never sprays catch-up picks.
        if beat < self.last_beat {
            self.motion_due = Some(beat + if self.motion_turn == 0 { 64.0 } else { 128.0 });
        }
        self.last_beat = beat;
        let due = self.motion_due.is_some_and(|at| beat >= at);
        out.motion |= first || changed || song || section || drop || special || restored || due;
        out.drop = drop;
        if self.source == "idle" {
            out.idle = first || changed || restored;
            out.palette = false;
            out.motion = false;
            out.drop = false;
        } else if out.motion {
            self.motion_turn = (self.motion_turn + 1) % 2;
            // 16 or 32 four-beat bars, using the same authoritative position as effects.
            self.motion_due = Some(beat + if self.motion_turn == 0 { 64.0 } else { 128.0 });
        }
        out.controls = self.source != "idle" && !out.motion && now >= self.controls_at && (level - self.last_level).abs() >= 0.08;
        if out.motion || out.controls {
            self.controls_at = now + 2_000 * MS;
            self.last_level = level;
        }
        out
    }

    fn character(&self) -> &'static str {
        match self.mood {
            Mood::Heavy => if self.palette_turn % 3 == 0 { "rich" } else { "solid" },
            Mood::Chill => if self.palette_turn % 3 == 0 { "solid" } else { "rich" },
            Mood::Hype => if self.palette_turn % 3 == 0 { "rainbow" } else { "rich" },
            _ => if self.palette_turn % 2 == 0 { "rich" } else { "duo" },
        }
    }
}

struct AutoEffect {
    enabled: String,
    interval: String,
    last_start: Option<Ts>,
}

/// Canonical effect identity, shared by its trigger and settings (no allocation).
pub(super) fn effect_root(address: &str) -> Option<&str> {
    let rest = address.strip_prefix("fx.").or_else(|| address.strip_prefix("patch."))?;
    let name = rest.split('.').next()?;
    if name.is_empty() || matches!(name, "auto" | "enabled") { return None; }
    Some(&address[..address.len() - rest.len() + name.len()])
}

pub(super) fn auto_control(address: &str) -> bool {
    effect_root(address).is_some_and(|root| {
        address.strip_prefix(root).is_some_and(|suffix| matches!(suffix, ".auto.enabled" | ".auto.interval"))
    })
}

struct MusicalCandidate {
    name: String,
    moods: u8,
    energy: [f32; 2],
    weight: f64,
    effects: Vec<String>,
}

struct MusicalOwned {
    inst: se_proto::Id,
    tail_until: Option<Ts>,
}

/// Only scheduling and recent-history scalars change on ordinary ticks. Library metadata is
/// rebuilt on configuration changes; candidates are examined only at musical opportunities.
struct MusicalDirector {
    library: Vec<MusicalCandidate>,
    recent: [Option<usize>; 2],
    spends: [Ts; 48],
    spend_count: usize,
    spend_next: usize,
    enabled: bool,
    song_since: Option<Ts>,
    quiet_until: Ts,
    retry_at: Ts,
    phrase_due: Option<f64>,
    last_beat: f64,
    beat_changed: Option<Ts>,
    trusted: bool,
    owned: Option<MusicalOwned>,
    accent: f32,
    accent_until: Ts,
    reason: &'static str,
    published_next: Ts,
    failed: bool,
    config_valid: bool,
}

impl Default for MusicalDirector {
    fn default() -> Self {
        Self {
            library: Vec::new(), recent: [None; 2], spends: [0; 48], spend_count: 0, spend_next: 0,
            enabled: false, song_since: None, quiet_until: 0, retry_at: 0, phrase_due: None,
            last_beat: 0.0, beat_changed: None, trusted: false, owned: None, accent: 0.0,
            accent_until: 0, reason: "", published_next: 0, failed: false, config_valid: true,
        }
    }
}

impl MusicalDirector {
    fn remember(&mut self, candidate: usize, now: Ts) {
        self.recent = [Some(candidate), self.recent[0]];
        self.spends[self.spend_next] = now;
        self.spend_next = (self.spend_next + 1) % self.spends.len();
        self.spend_count = (self.spend_count + 1).min(self.spends.len());
    }

    fn hour_due(&self, now: Ts, max: usize) -> Ts {
        let mut live = 0;
        let mut oldest = Ts::MAX;
        for &at in &self.spends[..self.spend_count] {
            if now < at.saturating_add(HOUR) {
                live += 1;
                oldest = oldest.min(at);
            }
        }
        if live >= max { oldest.saturating_add(HOUR) } else { 0 }
    }
}

/// Runtime of the context layer and the effects switch's configuration.
#[derive(Default)]
pub struct Context {
    cfg: ContextDef,
    exempt: Vec<String>,
    /// Stable effect identities survive configuration reloads, including their last auto start.
    auto_effects: BTreeMap<String, AutoEffect>,
    in_ids: [Option<usize>; IN.len()],
    in_gen: u64,
    out_ids: [usize; OUT.len()],
    // song
    song_on: bool,
    song_flip: Option<Ts>,
    song_off_at: Option<Ts>,
    song_reset: bool,
    loud_fast: f32,
    flux_fast: f32,
    dens_fast: f32,
    loud_base: f32,
    flux_base: f32,
    dens_base: f32,
    song: f32,
    // drums
    drum_db_fast: f32,
    drum_dens_fast: f32,
    drum_db_base: f32,
    drum_dens_base: f32,
    drum_base_init: bool,
    drums: f32,
    // chat
    chat_base: f32,
    chat_base_init: bool,
    bump: f32,
    chat: f32,
    energy: f32,
    // talking
    talking: bool,
    talk_on_since: Option<Ts>,
    talk_off_since: Option<Ts>,
    // budget: times of the spends of the last hour
    spends: VecDeque<Ts>,
    // peak / settle
    peak_since: Option<Ts>,
    settle_since: Option<Ts>,
    peak_latched: bool,
    in_peak: bool,
    last_peak: Option<Ts>,
    // song peak
    arm_until: Ts,
    arm_strength: f32,
    song_high_since: Option<Ts>,
    last_song_peak: Option<Ts>,
    // fill: ring of recent snare/tom hit times (0 = empty)
    hits: [Ts; FILL_RING],
    hit_next: usize,
    last_fill: Option<Ts>,
    // mood
    mood: Mood,
    mood_due: Option<Ts>,
    mood_f: MoodFeatures,
    director: LightDirector,
    musical: MusicalDirector,
    song_trend: f32,
}

fn ema(prev: f32, x: f32, dt: f32, tau: f32) -> f32 {
    prev + (x - prev) * (1.0 - (-dt / tau).exp())
}

fn db(lin: f32) -> f32 {
    20.0 * lin.max(1e-6).log10()
}

/// Deviation of `fast` from `base` in units of `span`, clamped to ±1.
fn rel(fast: f32, base: f32, span: f32) -> f32 {
    ((fast - base) / span.max(1e-6)).clamp(-1.0, 1.0)
}

/// How long `cond` has held (0 while it doesn't).
fn held(since: &mut Option<Ts>, cond: bool, now: Ts) -> Ts {
    if cond {
        now - *since.get_or_insert(now)
    } else {
        *since = None;
        0
    }
}

fn cooled(last: Option<Ts>, now: Ts, cooldown: Ts) -> bool {
    last.is_none_or(|t| now >= t + cooldown)
}

fn secs(t: Ts) -> f32 {
    t as f32 / 1e9
}

/// Trigger address of a preset `fx` entry.
fn fx_address(name: &str) -> String {
    if name.contains('.') { name.to_string() } else { format!("fx.{name}") }
}

/// An effect trigger the switch governs (`patch.*`, `fx.*`).
fn effect_address(address: &str) -> bool {
    address.starts_with("patch.") || address.starts_with("fx.")
}

/// `[fx] exempt` names a patch id (`win31_alerts`) or a full trigger address.
fn exempt(list: &[String], address: &str) -> bool {
    let patch = address.strip_prefix("patch.").and_then(|r| r.split('.').next());
    list.iter().any(|e| e == address || patch == Some(e.as_str()))
}

/// A preset the effects switch governs: a roulette, a lane member, or one with a non-exempt effect.
fn effect_preset(def: &PresetDef, list: &[String]) -> bool {
    !def.pick.is_empty() || def.lane.is_some() || def.fx.iter().any(|f| !exempt(list, &fx_address(&f.name)))
}

/// Unlike the overall kill switch, a lane alone does not make a lighting job a video effect.
fn automatic_effect_preset(def: &PresetDef, config: &Config, depth: u32) -> bool {
    if !def.fx.is_empty() || def.set.keys().any(|a| effect_address(a)) {
        return true;
    }
    if depth >= super::MAX_CHAIN_DEPTH { return false; }
    let child = |name: &str| config.presets.get(name).is_some_and(|p| automatic_effect_preset(p, config, depth + 1));
    def.pick.iter().any(|p| child(&p.name)) || def.commands.iter().any(|text| {
        match se_proto::Op::parse(text) {
            Ok(se_proto::Op::Trigger { address, .. }) => effect_address(&address),
            Ok(se_proto::Op::PresetFire { name, .. }) => child(&name),
            _ => false,
        }
    })
}

/// Keep the originating event through delays and preset queues; viewer rules are independent.
fn automatic_context(ctx: &Ctx) -> bool {
    ctx.event.as_ref().is_some_and(|e| e.ty == "context.musical_fx")
}

impl Core {
    // ---- setup -------------------------------------------------------------------------

    pub(super) fn declare_context_state(&mut self) {
        let sw = |d: &str| Meta::boolean(true).readonly().owner("core").describe(d);
        self.state.declare(FX_ENABLED, sw("Automatic and viewer effects may run (operator switch: fx.on / fx.off)"));
        self.state.declare(FX_AUTO, Meta::boolean(false).readonly().owner("core").describe("Context-driven video moments may run (operator switch: fx.auto.on / fx.auto.off)"));
        self.state.declare(LIGHTS_AUTO, sw("Automation may drive the lights (operator switch: lights.auto.on / lights.auto.off)"));
        let mood = Meta::enumeration("none", &Mood::ALL).readonly().owner("context").describe("Mood of the current song");
        self.state.declare(MOOD, mood);
        self.state.declare(FX_PRESET, Meta::string("").readonly().owner("context").describe("Director-owned musical preset, including its release tail"));
        self.state.declare(FX_REASON, Meta::string("off").readonly().owner("context").describe("Musical FX state: active, quiet, off, no-song, speech, no-match or error"));
        self.state.declare(FX_NEXT_AT, Meta::int(0, [0.0, 9.2e18]).unit("ns").readonly().owner("context").describe("Earliest musical opportunity on the core clock (0 when suppressed)"));
        self.state.declare(FX_HEALTH, Meta::string("").readonly().owner("context").describe("Musical FX configuration or firing failure; empty when healthy"));
        for (k, n) in OUT.iter().enumerate() {
            self.context.out_ids[k] = self.signals.ensure(n);
        }
    }

    pub(super) fn declare_auto_effect(&mut self, address: &str) {
        let Some(root) = effect_root(address) else { return };
        if self.context.auto_effects.get(root).is_some_and(|effect|
            self.state.id(&effect.enabled).is_some() && self.state.id(&effect.interval).is_some()) { return; }
        let enabled = format!("{root}.auto.enabled");
        let interval = format!("{root}.auto.interval");
        self.state.declare(&enabled, Meta::boolean(true).readonly().owner("context")
            .describe("Allow automatic starts only; manual and viewer triggers are unaffected"));
        self.state.declare(&interval, Meta::float(0.0, [0.0, 86_400.0]).unit("s").readonly().owner("context")
            .describe("Minimum seconds between successful automatic starts; not a periodic timer"));
        self.context.auto_effects.entry(root.to_string()).or_insert(AutoEffect { enabled, interval, last_start: None });
    }

    pub(super) fn auto_effect_ready(&self, address: &str) -> bool {
        let Some(effect) = effect_root(address).and_then(|root| self.context.auto_effects.get(root)) else { return true };
        let base = |address: &str| self.state.id(address).map(|id| &self.state.param(id).base);
        let interval = base(&effect.interval).and_then(Value::as_f64).unwrap_or(0.0);
        base(&effect.enabled).is_none_or(Value::truthy)
            && effect.last_start.is_none_or(|last| self.now() >= last.saturating_add((interval * 1e9) as Ts))
    }

    pub(super) fn note_auto_start(&mut self, address: &str, ctx: &Ctx) {
        if !Self::automatic_effect_context(ctx) { return; }
        self.declare_auto_effect(address);
        let now = self.now();
        if let Some(effect) = effect_root(address).and_then(|root| self.context.auto_effects.get_mut(root)) {
            effect.last_start = Some(now);
        }
    }

    /// Roulettes need at least one eligible child; every constituent of a regular preset must pass.
    pub(super) fn auto_preset_ready(&self, def: &PresetDef, depth: u32) -> bool {
        if depth >= super::MAX_CHAIN_DEPTH { return false; }
        if !def.pick.is_empty() {
            return def.pick.iter().any(|entry| entry.w > 0.0 && self.config.presets.get(&entry.name)
                .is_some_and(|child| self.auto_preset_ready(child, depth + 1)));
        }
        def.fx.iter().all(|fx| self.auto_effect_ready(&fx_address(&fx.name)))
            && def.set.keys().all(|address| self.auto_effect_ready(address))
            && def.commands.iter().all(|text| match se_proto::Op::parse(text) {
                Ok(se_proto::Op::Trigger { address, .. }) => self.auto_effect_ready(&address),
                Ok(se_proto::Op::PresetFire { name, .. }) => self.config.presets.get(&name)
                    .is_some_and(|child| self.auto_preset_ready(child, depth + 1)),
                _ => true,
            })
    }

    pub(super) fn automatic_effect_context(ctx: &Ctx) -> bool {
        // Only autonomous musical opportunities, not context notifications or operator rituals.
        !ctx.operator && ctx.event.as_ref().is_some_and(|e| matches!(e.ty.as_str(),
            "context.musical_fx" | "context.peak" | "context.settle" | "context.song_peak" | "context.fill_landed" | "context.mood"))
    }

    /// `[context]` and `[fx]` (errors are reported by config validation; defaults apply).
    pub(super) fn configure_context(&mut self, config: &Config) {
        self.stop_musical_fx(None);
        for preset in config.presets.values() {
            for fx in &preset.fx { self.declare_auto_effect(&fx_address(&fx.name)); }
            for address in preset.set.keys() { self.declare_auto_effect(address); }
        }
        let cfg = config.context();
        let error = cfg.as_ref().err().cloned().or_else(|| config.errors.iter().find(|e| e.msg.contains("auto_fx")).map(|e| e.msg.clone()))
            .or_else(|| config.presets.values().find_map(|p| p.check_auto_fx().err()));
        self.context.musical.failed = error.is_some();
        self.context.musical.config_valid = error.is_none();
        self.set_sys(FX_HEALTH, Value::Str(error.unwrap_or_default()));
        self.context.cfg = cfg.unwrap_or_default();
        self.context.exempt = config.fx_control().map(|f| f.exempt).unwrap_or_default();
        let d = &mut self.context.musical;
        d.library.clear();
        d.recent = [None; 2];
        d.phrase_due = None;
        d.song_since = None;
        d.enabled = false;
        if let Some(tail) = d.owned.as_ref().and_then(|p| p.tail_until) {
            d.quiet_until = d.quiet_until.max(tail.saturating_add(self.context.cfg.musical_fx.quiet_gap.ns()));
        }
        for p in config.presets.values().filter(|p| p.auto_fx.is_some() && p.check_auto_fx().is_ok()) {
            let a = p.auto_fx.as_ref().unwrap();
            let moods = if a.moods.is_empty() { 0x3f } else {
                a.moods.iter().fold(0, |mask, m| mask | (1 << Mood::ALL.iter().position(|s| *s == m.as_str()).unwrap()))
            };
            d.library.push(MusicalCandidate { name: p.name.clone(), moods, energy: a.energy, weight: a.weight,
                effects: p.fx.iter().map(|fx| fx_address(&fx.name)).collect() });
        }
    }

    // ---- operator control --------------------------------------------------------------

    pub(super) fn fx_on(&self) -> bool {
        self.state.get(FX_ENABLED).is_none_or(Value::truthy)
    }

    pub(super) fn fx_auto_on(&self) -> bool {
        self.state.get(FX_AUTO).is_some_and(Value::truthy)
    }

    /// The operator (or content they authored: scenes, their own button presses and what they
    /// trigger). Viewers, patches, rules on automatic events and timelines are not.
    pub(super) fn is_operator(&self, origin: Origin, ctx: &Ctx) -> bool {
        if ctx.operator {
            return true;
        }
        if let Some(a) = &ctx.actor {
            return a.platform == "local" || a.top_role() >= Role::Owner;
        }
        operator_origin(origin) && self.priority_for(origin, ctx) > PRIORITY_CHAT
    }

    /// `preset.fire` of an effect preset while effects are off.
    pub(super) fn gate_preset(&self, def: &PresetDef, origin: Origin, ctx: &Ctx) -> Result<(), String> {
        if !self.fx_auto_on() && automatic_context(ctx) && automatic_effect_preset(def, &self.config, 0) {
            return Err(FX_AUTO_OFF.into());
        }
        if Self::automatic_effect_context(ctx) && !self.auto_preset_ready(def, 0) {
            return Err("preset contains an automatic effect that is disabled or cooling down".into());
        }
        if self.fx_on() || !effect_preset(def, &self.context.exempt) || self.is_operator(origin, ctx) {
            return Ok(());
        }
        Err(FX_OFF.into())
    }

    /// `patch.<id>.trigger` / `fx.<name>.trigger` while effects are off.
    pub(super) fn gate_trigger(&self, address: &str, origin: Origin, ctx: &Ctx) -> Result<(), String> {
        if !self.fx_auto_on() && automatic_context(ctx) && effect_address(address) {
            return Err(FX_AUTO_OFF.into());
        }
        if Self::automatic_effect_context(ctx) && !self.auto_effect_ready(address) {
            return Err(format!("`{address}` automatic starts are disabled or cooling down"));
        }
        if Self::automatic_effect_context(ctx) && self.triggers.get(address).is_some_and(|t|
            t.spec.retrigger == crate::config::Conflict::Queue && !t.instances.is_empty()) {
            return Err(format!("`{address}` is busy; automatic starts are not queued"));
        }
        if self.fx_on() || !effect_address(address) || exempt(&self.context.exempt, address) || self.is_operator(origin, ctx) {
            return Ok(());
        }
        Err(FX_OFF.into())
    }

    /// `fx.*`, `fx.auto.*`, `lights.auto.*`, `context.spend`.
    pub(super) fn context_action(&mut self, name: &str, origin: Origin, ctx: &Ctx) -> Result<(), String> {
        match name {
            "context.spend" => {
                let left = self.context_budget();
                if left < 1 {
                    return Err("no automatic moments left this hour".into());
                }
                self.context.spends.push_back(self.now());
                let id = self.context.out_ids[O_BUDGET];
                self.signals.set_id(id, (left - 1) as f32, self.now());
                Ok(())
            }
            "fx.on" | "fx.off" | "fx.toggle" => {
                if !self.is_operator(origin, ctx) {
                    return Err("only the operator can switch effects on or off".into());
                }
                let on = match name {
                    "fx.on" => true,
                    "fx.off" => false,
                    _ => !self.fx_on(),
                };
                self.set_fx_enabled(on, ctx);
                Ok(())
            }
            "fx.auto.on" | "fx.auto.off" | "fx.auto.toggle" => {
                if !self.is_operator(origin, ctx) {
                    return Err("only the operator can switch automatic effects".into());
                }
                let cur = self.fx_auto_on();
                let on = match name {
                    "fx.auto.on" => true,
                    "fx.auto.off" => false,
                    _ => !cur,
                };
                if let Some(id) = self.state.id(FX_AUTO) {
                    while let Some(key) = self.state.param(id).overrides.first().map(|o| o.key.clone()) {
                        self.state.remove_override(id, &key);
                    }
                }
                self.set_sys(FX_AUTO, Value::Bool(on));
                if let Some(id) = self.state.id(FX_AUTO) {
                    let now = self.now();
                    self.state.refresh(id, now);
                }
                if !on {
                    self.stop_musical_fx(ctx.parent);
                    self.context.musical.enabled = false;
                    self.musical_status("off", 0);
                    // Cancel only new context-origin video starts, never viewer effects or lights.
                    let cfg = &self.config;
                    let is_fx = |name: &str| cfg.presets.get(name).is_some_and(|p| automatic_effect_preset(p, cfg, 0));
                    self.pending_presets.retain(|p| !automatic_context(&p.ctx) || !is_fx(&p.name));
                    self.preset_queue.retain(|(name, _, ctx)| !automatic_context(ctx) || !is_fx(name));
                    self.scheduled.retain(|s| {
                        if !automatic_context(&s.0.ctx) { return true; }
                        match &s.0.op {
                            se_proto::Op::Trigger { address, .. } => !effect_address(address),
                            se_proto::Op::PresetFire { name, .. } => !is_fx(name),
                            _ => true,
                        }
                    });
                }
                if on != cur {
                    self.context_emit("fx.auto.changed", Value::map().with("enabled", on), ctx);
                }
                self.runtime_dirty = true;
                Ok(())
            }
            _ => {
                if !self.is_operator(origin, ctx) {
                    return Err("only the operator can switch light automation".into());
                }
                let cur = self.state.get(LIGHTS_AUTO).is_none_or(Value::truthy);
                let on = match name {
                    "lights.auto.on" => true,
                    "lights.auto.off" => false,
                    _ => !cur,
                };
                if let Some(id) = self.state.id(LIGHTS_AUTO) {
                    while let Some(key) = self.state.param(id).overrides.first().map(|o| o.key.clone()) {
                        self.state.remove_override(id, &key);
                    }
                }
                self.set_sys(LIGHTS_AUTO, Value::Bool(on));
                if let Some(id) = self.state.id(LIGHTS_AUTO) {
                    // set_sys skips refresh when the base already matches, but a removed legacy
                    // override can still have supplied the resolved value.
                    let now = self.now();
                    self.state.refresh(id, now);
                }
                if !on {
                    // fade back to the operator's lights rather than cutting
                    for layer in ["base", "rhythm", "accent"] {
                        let args = Value::map().with("layer", layer).with("owner", LIGHTS_OWNER).with("fade", LIGHTS_AUTO_OFF_FADE);
                        self.exec_traced(&se_proto::Op::Action { name: "lights.layer.release".into(), args }, origin, ctx);
                    }
                }
                if on != cur {
                    self.context_emit("lights.auto.changed", Value::map().with("enabled", on), ctx);
                }
                self.runtime_dirty = true;
                Ok(())
            }
        }
    }

    fn set_fx_enabled(&mut self, on: bool, ctx: &Ctx) {
        if self.fx_on() == on {
            return;
        }
        self.set_sys(FX_ENABLED, Value::Bool(on));
        if !on {
            self.stop_musical_fx(ctx.parent);
            self.context.musical.enabled = false;
            self.musical_status("off", 0);
            self.release_effects(ctx);
        }
        self.context_emit("fx.changed", Value::map().with("enabled", on), ctx);
        self.runtime_dirty = true;
    }

    /// Effects off: every running effect preset and effect trigger envelope lets go, and lane
    /// queues, quantized starts and conflict queues of effect presets are dropped. Exempt
    /// patches keep running.
    fn release_effects(&mut self, ctx: &Ctx) {
        let list = &self.context.exempt;
        let cfg = &self.config;
        let is_fx = |name: &str| cfg.presets.get(name).is_some_and(|d| effect_preset(d, list));
        let mut names: Vec<String> = self.presets.iter().filter(|p| is_fx(&p.name)).map(|p| p.name.clone()).collect();
        self.pending_presets.retain(|p| !is_fx(&p.name));
        self.preset_queue.retain(|(n, _, _)| !is_fx(n));
        names.sort();
        names.dedup();
        for n in names {
            self.release_preset(&n, ctx.parent, true);
        }
        let now = self.now();
        let list = &self.context.exempt;
        for (a, t) in self.triggers.iter_mut() {
            if effect_address(a) && !exempt(list, a) {
                t.release(None, now);
            }
        }
    }

    fn context_emit(&mut self, ty: &str, payload: Value, ctx: &Ctx) {
        let mut e = Event::new(ty, Origin::System, payload).with_causal(ctx.parent);
        e.ts = self.now();
        self.events.push_back((e, Ctx { depth: ctx.depth, parent: ctx.parent, ..Default::default() }));
    }

    // ---- persistence -------------------------------------------------------------------

    /// Runtime switches kept across restarts.
    pub(super) fn context_bases() -> [&'static str; 3] {
        [FX_ENABLED, FX_AUTO, LIGHTS_AUTO]
    }

    // ---- events ------------------------------------------------------------------------

    /// Observe a delivered event (drops, drum hits, channel activity, song changes).
    pub(super) fn context_event(&mut self, ev: &Event) {
        let num = |k: &str| ev.payload.get_path(k).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let now = self.now();
        let mut landing = None;
        let mut reset_musical = false;
        let c = &mut self.context;
        match ev.ty.as_str() {
            "music.drop" if !c.talking => {
                c.arm_until = now + c.cfg.song_peak_window.ns();
                c.arm_strength = num("strength").clamp(0.0, 1.0);
                c.director.drop = true;
                c.musical.accent = c.arm_strength;
                c.musical.accent_until = now + FX_RETRY;
            }
            "music.section" if !c.talking && num("novelty") >= c.cfg.section_novelty => {
                c.arm_until = now + c.cfg.song_peak_window.ns();
                c.arm_strength = num("novelty").clamp(0.0, 1.0);
                c.director.section = true;
                c.musical.accent = c.arm_strength * 0.7;
                c.musical.accent_until = now + FX_RETRY;
            }
            "band.snare" | "band.tom" | "band.tom_hi" | "band.tom_mid" | "band.tom_lo" | "band.tom_floor" => {
                if num("velocity") >= FILL_HIT_VELOCITY {
                    c.hits[c.hit_next] = now;
                    c.hit_next = (c.hit_next + 1) % FILL_RING;
                }
            }
            "band.kick" | "band.crash" | "band.cymbal" => {
                let v = num("velocity");
                let crash = ev.ty != "band.kick";
                if crash || v >= c.cfg.fill_land_velocity {
                    landing = Some(if crash { v.max(c.cfg.fill_land_velocity) } else { v });
                }
            }
            "twitch.cheer" => c.bump += (0.1 + num("bits") / 2500.0).min(0.5),
            "twitch.sub" | "twitch.resub" => c.bump += 0.2,
            "twitch.gift" => c.bump += (0.15 * num("count").max(1.0)).min(0.6),
            "twitch.raid" => {
                c.bump += (0.3 + num("viewers") / 200.0).min(0.7);
                c.director.special = "raid";
                c.director.special_until = now + 20_000 * MS;
                c.director.special_pending = true;
            }
            "twitch.hype_train.begin" | "twitch.hype_train.progress" => {
                c.bump += 0.25;
                if ev.ty == "twitch.hype_train.begin" {
                    c.director.special = "hypetrain";
                    c.director.special_until = now + 32_000 * MS;
                    c.director.special_pending = true;
                }
            }
            "twitch.hype_train.end" if c.director.special == "hypetrain" => c.director.special_until = now,
            "queue.song_started" => {
                c.song_reset = true;
                c.mood_due = Some(now + MOOD_SETTLE);
                c.director.song_started = true;
                c.director.song_grace = now + SONG_GAP;
                reset_musical = true;
            }
            "queue.song_ended" => {
                c.director.song_grace = 0;
                reset_musical = true;
            }
            "context.peak" | "context.song_peak" if !c.talking && c.song >= 0.85 => {
                c.director.peak_until = now + 4_000 * MS;
                c.director.section = true;
            }
            "context.fill_landed" if !c.talking && c.song_on => {
                c.musical.accent = num("strength") * 0.5;
                c.musical.accent_until = now + FX_RETRY;
            }
            _ => {}
        }
        self.context.bump = self.context.bump.min(1.0);
        if reset_musical { self.stop_musical_fx(Some(ev.id)); }
        if let Some(v) = landing {
            self.fill_landing(now, v);
        }
    }

    /// A kick/crash: a fill landed if enough snare/tom hits came just before, near a downbeat.
    fn fill_landing(&mut self, now: Ts, velocity: f32) {
        let c = &self.context;
        let last = c.hits.iter().copied().max().unwrap_or(0);
        if last == 0 || now.saturating_sub(last) > c.cfg.fill_land.ns() || !cooled(c.last_fill, now, c.cfg.fill_cooldown.ns()) {
            return;
        }
        let from = last.saturating_sub(c.cfg.fill_window.ns());
        let n = c.hits.iter().filter(|&&t| t != 0 && t >= from).count();
        if n < c.cfg.fill_hits as usize || !self.near_downbeat() {
            return;
        }
        let strength = (0.5 * velocity.clamp(0.0, 1.0) + 0.5 * (n as f32 / 8.0).min(1.0)).clamp(0.0, 1.0);
        let c = &mut self.context;
        c.hits = [0; FILL_RING];
        c.last_fill = Some(now);
        self.context_emit("context.fill_landed", Value::map().with("strength", strength as f64), &Ctx::default());
    }

    /// Within `fill_downbeat` beats of a bar start on a trusted 4/4 grid; anywhere without one.
    fn near_downbeat(&self) -> bool {
        let tol = self.context.cfg.fill_downbeat;
        if tol <= 0.0 || self.ctx_sig(BEAT_CONF) < GRID_CONFIDENCE || self.ctx_sig(BPM) <= 0.0 {
            return true;
        }
        let b = (self.ctx_sig(BEAT_POS) as f64).rem_euclid(4.0);
        b.min(4.0 - b) <= tol as f64
    }

    fn ctx_sig(&self, k: usize) -> f32 {
        self.context.in_ids[k].map_or(0.0, |i| self.signals.value(i))
    }

    fn context_budget(&mut self) -> i64 {
        let now = self.now();
        while self.context.spends.front().is_some_and(|t| now >= t + HOUR) {
            self.context.spends.pop_front();
        }
        self.context.cfg.auto_per_hour as i64 - self.context.spends.len() as i64
    }

    // ---- tick --------------------------------------------------------------------------

    pub(super) fn tick_context(&mut self, now: Ts) {
        if self.context.in_gen != self.signals.generation {
            for (k, n) in IN.iter().enumerate() {
                self.context.in_ids[k] = self.signals.id(n);
            }
            self.context.in_gen = self.signals.generation;
        }
        let dt = secs(self.period);
        let s = |k: usize| self.ctx_sig(k);
        let lufs = s(M_LUFS);
        let loud = if self.context.in_ids[M_LUFS].is_some() && lufs < -0.01 { lufs } else { db(s(M_LEVEL)) };
        let flux = s(M_FLUX).max(0.0);
        let dens = (s(M_KICK) + s(M_SNARE) + s(M_HAT)) / 3.0;
        let band_db = db(s(B_LEVEL));
        let band_dens = (s(B_KICK) + s(B_SNARE) + s(B_HAT)) / 3.0;
        let (bass, mid, high, centroid) = (s(M_BASS), s(M_MID), s(M_HIGH), s(M_CENTROID));
        let bpm = if s(BEAT_CONF) >= GRID_CONFIDENCE { s(BPM) } else { 0.0 };
        let mic_talk = match self.context.in_ids[MIC_TALKING] {
            Some(_) => s(MIC_TALKING) >= 0.5,
            None => s(MIC_LEVEL) > MIC_SPEECH,
        };
        let chat_rate = s(CHAT_RATE).max(0.0);
        let (hype_on, hype_level) = match self.state.get("twitch.hype_train").and_then(Value::as_map) {
            Some(m) if !m.is_empty() && m.get("active").is_none_or(Value::truthy) => (true, m.get("level").and_then(Value::as_f64).unwrap_or(1.0) as f32),
            _ => (false, 0.0),
        };
        let budget = self.context_budget();

        let c = &mut self.context;
        // song: playing (debounced), then loudness/flux/density against its own baseline
        let raw_on = loud > SONG_FLOOR_DB;
        if raw_on != c.song_on {
            if secs(held(&mut c.song_flip, true, now)) >= SONG_DEBOUNCE {
                c.song_on = raw_on;
                c.song_flip = None;
                if !raw_on {
                    c.song_off_at = Some(now);
                } else if c.song_off_at.is_none_or(|t| now - t >= SONG_GAP) {
                    c.song_reset = true;
                    c.mood_due = Some(now + MOOD_SETTLE);
                }
            }
        } else {
            c.song_flip = None;
        }
        if c.song_reset && c.song_on {
            c.song_reset = false;
            (c.loud_fast, c.flux_fast, c.dens_fast) = (loud, flux, dens);
            (c.loud_base, c.flux_base, c.dens_base) = (loud, flux, dens);
        }
        c.loud_fast = ema(c.loud_fast, loud, dt, FAST_TAU);
        c.flux_fast = ema(c.flux_fast, flux, dt, FAST_TAU);
        c.dens_fast = ema(c.dens_fast, dens, dt, FAST_TAU);
        let song_raw = if c.song_on {
            c.loud_base = ema(c.loud_base, c.loud_fast, dt, SONG_BASE_TAU);
            c.flux_base = ema(c.flux_base, c.flux_fast, dt, SONG_BASE_TAU);
            c.dens_base = ema(c.dens_base, c.dens_fast, dt, SONG_BASE_TAU);
            // 0.5 = typical for this song; louder, busier, more changing sections push it to 1
            let x = 0.5 * rel(c.loud_fast, c.loud_base, LOUD_SPAN_DB)
                + 0.25 * rel(c.flux_fast, c.flux_base, c.flux_base.max(0.1))
                + 0.25 * rel(c.dens_fast, c.dens_base, c.dens_base.max(0.05));
            (0.5 + 0.5 * x).clamp(0.0, 1.0)
        } else {
            0.0
        };
        c.song_trend = ema(c.song_trend, song_raw, dt, 8.0);
        c.song = ema(c.song, song_raw, dt, OUT_TAU);

        // drums: band level and onset density against the session's playing baseline
        c.drum_db_fast = ema(c.drum_db_fast, band_db, dt, FAST_TAU);
        c.drum_dens_fast = ema(c.drum_dens_fast, band_dens, dt, FAST_TAU);
        let playing = band_db > BAND_FLOOR_DB;
        let drums_raw = if playing {
            if !c.drum_base_init {
                c.drum_base_init = true;
                (c.drum_db_fast, c.drum_dens_fast) = (band_db, band_dens);
                (c.drum_db_base, c.drum_dens_base) = (band_db, band_dens);
            }
            c.drum_db_base = ema(c.drum_db_base, c.drum_db_fast, dt, DRUM_BASE_TAU);
            c.drum_dens_base = ema(c.drum_dens_base, c.drum_dens_fast, dt, DRUM_BASE_TAU);
            let x = 0.6 * rel(c.drum_db_fast, c.drum_db_base, LOUD_SPAN_DB) + 0.4 * rel(c.drum_dens_fast, c.drum_dens_base, c.drum_dens_base.max(0.05));
            (0.5 + 0.5 * x).clamp(0.0, 1.0)
        } else {
            0.0
        };
        c.drums = ema(c.drums, drums_raw, dt, OUT_TAU);

        // chat: rate against a ~10 minute baseline, plus decaying bumps from channel events
        if !c.chat_base_init {
            c.chat_base_init = true;
            c.chat_base = chat_rate.max(CHAT_FLOOR);
        }
        c.chat_base = ema(c.chat_base, chat_rate, dt, CHAT_BASE_TAU);
        c.bump = ema(c.bump, 0.0, dt, BUMP_TAU);
        let heat = (chat_rate / c.chat_base.max(CHAT_FLOOR) / CHAT_FULL).clamp(0.0, 1.0);
        c.chat = ema(c.chat, (heat + c.bump).clamp(0.0, 1.0), dt, OUT_TAU);

        // energy: the music (song and/or drummer) dominates, chat and a hype train add
        let music = match (c.song_on, playing) {
            (true, true) => 0.5 * (c.song + c.drums),
            (true, false) => c.song,
            _ => c.drums,
        };
        let hype = if hype_on { (0.1 + 0.03 * hype_level).min(0.25) } else { 0.0 };
        c.energy = ema(c.energy, (0.7 * music + 0.3 * c.chat + hype).clamp(0.0, 1.0), dt, ENERGY_TAU);

        // talking: mic speech while song and band are quiet, with on/off hysteresis
        let talk_cond = mic_talk && !c.song_on && c.drum_db_fast < BAND_QUIET_DB;
        if c.talking {
            if secs(held(&mut c.talk_off_since, !talk_cond, now)) >= TALK_OFF {
                c.talking = false;
            }
        } else if secs(held(&mut c.talk_on_since, talk_cond, now)) >= TALK_ON {
            c.talking = true;
            c.arm_until = 0;
        }
        if c.talking {
            c.talk_on_since = None;
        } else {
            c.talk_off_since = None;
        }

        // mood features (while a song plays)
        if c.song_on {
            let f = &mut c.mood_f;
            f.bpm = if bpm > 0.0 { ema(f.bpm, bpm, dt, MOOD_TAU) } else { f.bpm };
            f.centroid = ema(f.centroid, centroid, dt, MOOD_TAU);
            f.bass_ratio = ema(f.bass_ratio, bass / (bass + mid + high).max(1e-6), dt, MOOD_TAU);
            let abs = 0.5 * (dens / DENSE_ENVELOPES).min(1.0) + 0.5 * (flux / DENSE_FLUX).min(1.0);
            f.energy = ema(f.energy, abs, dt, MOOD_TAU);
        }

        let out = c.out_ids;
        let vals = [c.song, c.drums, c.chat, c.energy, if c.talking { 1.0 } else { 0.0 }, budget as f32];
        for k in 0..OUT.len() {
            self.signals.set_id(out[k], vals[k], now);
        }
        self.context_moments(now);
        if self.context.song_on && self.context.mood_due.is_some_and(|d| now >= d) {
            self.context.mood_due = Some(now + self.context.cfg.mood_every.ns());
            self.evaluate_mood();
        }
        self.context_lights(now, playing);
        self.context_musical_fx(now, loud > SONG_FLOOR_DB);
    }

    fn musical_status(&mut self, reason: &'static str, next_at: Ts) {
        if self.context.musical.reason != reason {
            self.context.musical.reason = reason;
            self.set_sys(FX_REASON, Value::Str(reason.into()));
        }
        if self.context.musical.published_next != next_at {
            self.context.musical.published_next = next_at;
            self.set_sys(FX_NEXT_AT, Value::Int(next_at.min(i64::MAX as u64) as i64));
        }
    }

    pub(super) fn musical_started(&mut self, name: &str, inst: se_proto::Id) {
        self.context.musical.owned = Some(MusicalOwned { inst, tail_until: None });
        self.set_sys(FX_PRESET, Value::Str(name.into()));
        self.context.musical.phrase_due = None;
        self.context.musical.failed = false;
        self.set_sys(FX_HEALTH, Value::Str(String::new()));
        self.musical_status("active", 0);
    }

    pub(super) fn musical_failure(&mut self, error: String) {
        self.context.musical.failed = true;
        self.set_sys(FX_HEALTH, Value::Str(error));
    }

    pub(super) fn musical_released(&mut self, inst: se_proto::Id, now: Ts) {
        let d = &mut self.context.musical;
        if let Some(owned) = &mut d.owned
            && owned.inst == inst && owned.tail_until.is_none()
        {
            owned.tail_until = Some(now.saturating_add(MUSICAL_RELEASE));
            d.quiet_until = now.saturating_add(MUSICAL_RELEASE).saturating_add(self.context.cfg.musical_fx.quiet_gap.ns());
            d.phrase_due = None;
            d.retry_at = 0;
        }
    }

    pub(super) fn stop_musical_fx(&mut self, parent: Option<se_proto::Id>) {
        let inst = self.context.musical.owned.as_ref().filter(|p| p.tail_until.is_none()).map(|p| p.inst);
        if let Some(inst) = inst {
            self.release_preset_instance(inst, parent);
        }
        let d = &mut self.context.musical;
        d.phrase_due = None;
        d.song_since = None;
        d.retry_at = 0;
        d.accent = 0.0;
        d.accent_until = 0;
    }

    fn context_musical_fx(&mut self, now: Ts, audio_on: bool) {
        if self.context.musical.owned.as_ref().is_some_and(|p| p.tail_until.is_some_and(|at| now >= at)) {
            self.context.musical.owned = None;
            self.set_sys(FX_PRESET, Value::Str(String::new()));
        }
        let enabled = self.fx_on() && self.fx_auto_on();
        // An actual VAD can hear speech over music. Raw drum-mic RMS is not a VAD.
        let speech = self.context.talking || self.context.in_ids[MIC_TALKING].is_some_and(|id| self.signals.value(id) >= 0.5);
        let suppressed = if !enabled { Some("off") } else if !audio_on || !self.context.song_on {
            Some("no-song")
        } else if speech { Some("speech") } else { None };
        if let Some(reason) = suppressed {
            self.stop_musical_fx(None);
            self.context.musical.enabled = enabled;
            self.musical_status(reason, 0);
            return;
        }
        if !self.context.musical.config_valid {
            self.stop_musical_fx(None);
            self.musical_status("error", 0);
            return;
        }
        let beat = self.ctx_sig(BEAT_POS) as f64;
        let bpm = self.ctx_sig(BPM) as f64;
        let grid = beat.is_finite() && bpm.is_finite() && bpm > 0.0 && self.ctx_sig(BEAT_CONF) >= GRID_CONFIDENCE;
        let d = &mut self.context.musical;
        if !d.enabled {
            d.song_since = None;
            d.phrase_due = None;
            d.accent = 0.0;
            d.accent_until = 0;
        }
        d.enabled = true;
        let song_since = *d.song_since.get_or_insert(now);
        let moved = grid && (beat - d.last_beat).abs() > 1e-5;
        let repositioned = grid && (beat < d.last_beat || beat - d.last_beat > 4.0);
        if moved { d.beat_changed = Some(now); }
        let trusted = grid && d.beat_changed.is_some_and(|at| now.saturating_sub(at) < GRID_STALL);
        if repositioned || trusted != d.trusted {
            d.phrase_due = None;
        }
        d.last_beat = beat;
        d.trusted = trusted;
        if let Some(owned) = &d.owned {
            let fading = owned.tail_until.is_some();
            let next = if fading { d.quiet_until } else { 0 };
            let reason = if d.failed { "error" } else if fading { "quiet" } else { "active" };
            self.musical_status(reason, next);
            return;
        }
        let cfg = &self.context.cfg.musical_fx;
        let eligible_at = song_since.saturating_add(cfg.start_delay.ns()).max(d.quiet_until).max(d.retry_at);
        if now < eligible_at {
            let reason = if d.failed { "error" } else if d.reason == "no-match" { "no-match" } else { "quiet" };
            self.musical_status(reason, eligible_at);
            return;
        }
        let hour_due = d.hour_due(now, cfg.max_per_hour as usize);
        if hour_due > now {
            d.retry_at = hour_due;
            self.musical_status("quiet", hour_due);
            return;
        }
        if trusted {
            let phrase = cfg.phrase_beats as f64;
            let due = *d.phrase_due.get_or_insert_with(|| {
                let phase = beat.rem_euclid(phrase);
                if phase <= 0.08 { beat } else { beat + phrase - phase }
            });
            if beat < due || repositioned {
                let next = now.saturating_add(((due - beat).max(0.0) * 60.0 / bpm * 1e9) as Ts);
                // Publish the estimate once per scheduled phrase, not a changing per-frame clock.
                if d.published_next <= now || repositioned { self.musical_status("quiet", next); }
                return;
            }
        }
        self.select_musical_fx(now);
    }

    fn select_musical_fx(&mut self, now: Ts) {
        let random = self.rng.f64();
        let mood = 1u8 << (self.context.mood as u8);
        let energy = self.context.song;
        let d = &self.context.musical;
        let accent = if now < d.accent_until { d.accent } else { 0.0 };
        let activity = (0.8 * energy + 0.15 * self.context.drums + 0.05 * self.context.chat + 0.05 * accent).clamp(0.0, 1.0);
        let eligible = |index: usize, avoid: usize| {
            let p = &d.library[index];
            if p.moods & mood == 0 || energy < p.energy[0] || energy > p.energy[1] || d.recent[..avoid].contains(&Some(index)) {
                return false;
            }
            // Do not replace, queue behind or merge with manually-fired video envelopes.
            let Some(def) = self.config.presets.get(&p.name) else { return false };
            if !p.effects.iter().all(|address| self.auto_effect_ready(address)) { return false; }
            !self.presets.iter().any(|a| a.name == p.name)
                && !self.pending_presets.iter().any(|a| a.name == p.name)
                && !self.preset_queue.iter().any(|(name, _, _)| name == &p.name)
                && def.lane.as_deref().is_none_or(|lane| !self.lane_busy(lane))
                && p.effects.iter().all(|address| self.triggers.get(address).is_none_or(|t| t.instances.is_empty() && t.queued.is_empty()))
        };
        let weight = |index: usize| {
            let p = &d.library[index];
            let center = 0.5 * (p.energy[0] + p.energy[1]);
            p.weight * (1.0 + 0.2 * (1.0 - (center - activity).abs() as f64))
        };
        let mut avoid = 2;
        let total = loop {
            let total: f64 = (0..d.library.len()).filter(|&i| eligible(i, avoid)).map(weight).sum();
            if total > 0.0 || avoid == 0 { break total; }
            // Small pools relax the oldest exclusion first, preserving the immediately last
            // pick whenever any other matching choice remains.
            avoid -= 1;
        };
        if total == 0.0 {
            let d = &mut self.context.musical;
            d.phrase_due = None;
            d.retry_at = now + FX_RETRY;
            self.musical_status(if self.context.musical.failed { "error" } else { "no-match" }, now + FX_RETRY);
            return;
        }
        let mut roll = random * total;
        let mut selected = None;
        for i in 0..d.library.len() {
            if !eligible(i, avoid) { continue; }
            selected = Some(i);
            roll -= weight(i);
            if roll <= 0.0 { break; }
        }
        let selected = selected.unwrap();
        let name = d.library[selected].name.clone();
        let payload = Value::map().with("preset", name.clone()).with("mood", self.context.mood.as_str())
            .with("energy", energy as f64).with("level", (0.18 + 0.22 * activity) as f64)
            .with("attack", 2_000i64).with("release", 3_000i64);
        let mut event = Event::new("context.musical_fx", Origin::System, payload.clone());
        event.ts = now;
        let ctx = Ctx { key: Some("musical_fx".into()), event: Some(event.clone()), parent: Some(event.id), ..Default::default() };
        let result = self.config.presets.get(&name).cloned().ok_or_else(|| format!("musical preset `{name}` disappeared"))
            .and_then(|def| {
                self.gate_preset(&def, Origin::System, &ctx)?;
                let priority = def.priority.unwrap_or(se_proto::PRIORITY_PRESET).min(se_proto::PRIORITY_MANUAL - 1);
                let trigger_payload = Value::map().with("level", (0.18 + 0.22 * activity) as f64).with("attack", 2_000i64).with("release", 3_000i64);
                self.start_preset(&def, &trigger_payload, priority, Origin::System, &ctx);
                if self.context.musical.failed {
                    Err(self.state.get(FX_HEALTH).and_then(Value::as_str).unwrap_or("musical effect failed").to_string())
                } else { Ok(()) }
            });
        self.context.musical.phrase_due = None;
        match result {
            Ok(()) => {
                self.context.musical.remember(selected, now);
                self.events.push_back((event, Ctx::default()));
            }
            Err(error) => {
                self.context.musical.failed = true;
                self.context.musical.retry_at = now + FX_RETRY;
                self.set_sys(FX_HEALTH, Value::Str(error.clone()));
                self.log("error", error);
                self.musical_status("error", now + FX_RETRY);
            }
        }
    }

    fn context_lights(&mut self, now: Ts, playing: bool) {
        let enabled = self.state.get(LIGHTS_AUTO).is_none_or(Value::truthy);
        let beat = self.ctx_sig(BEAT_POS) as f64;
        let c = &self.context;
        let source = if c.talking {
            "talking"
        } else if c.song_on || now < c.director.song_grace {
            "track"
        } else if playing || c.drums >= 0.15 {
            "drums"
        } else {
            "idle"
        };
        let level = if source == "track" { c.song } else if source == "drums" { c.drums } else { 0.0 };
        let rising = c.song >= c.song_trend + 0.12;
        let mood = c.mood;
        let decision = self.context.director.update(now, enabled, source, mood, level, rising, beat);
        if !decision.idle && !decision.palette && !decision.motion && !decision.controls && !decision.special && !decision.drop {
            return;
        }
        let d = &self.context.director;
        let calm = d.source == "talking";
        let celebration = d.source == "celebration";
        let brightness = if calm { 0.55 } else { 0.7 + 0.3 * level };
        let depth = if calm { 0.35 } else if celebration { 1.0 } else { 0.6 + 0.4 * level };
        let payload = Value::map()
            .with("mood", d.mood.as_str())
            .with("character", d.character())
            .with("energy", d.energy)
            .with("family", d.family)
            .with("source", if d.source == "drums" { "drums" } else { "motion" })
            .with("prefer", if d.source == "track" { "track" } else { "clock" })
            .with("brightness", brightness as f64)
            .with("depth", depth as f64)
            .with("rhythm", if calm { 2.0 } else { 1.0 })
            .with("special", d.special);
        for (fire, event) in [
            (decision.idle, "context.lights_idle"),
            (decision.palette, "context.lights_palette"),
            (decision.motion, "context.lights_motion"),
            (decision.controls, "context.lights_controls"),
            (decision.special, "context.lights_special"),
            (decision.drop, "context.lights_drop"),
        ] {
            if fire {
                self.context_emit(event, payload.clone(), &Ctx::default());
            }
        }
    }

    /// `context.peak`/`settle` and `context.song_peak`.
    fn context_moments(&mut self, now: Ts) {
        let c = &mut self.context;
        let cfg = &c.cfg;
        let mut emit: [Option<(&'static str, f32)>; 3] = [None; 3];
        // peak: high for `peak_hold`, then (cooled down) one event per crossing
        let high = held(&mut c.peak_since, c.energy >= cfg.peak_on, now);
        if high >= cfg.peak_hold.ns() && !c.peak_latched {
            c.peak_latched = true;
            if cooled(c.last_peak, now, cfg.peak_cooldown.ns()) {
                c.last_peak = Some(now);
                c.in_peak = true;
                c.settle_since = None;
                emit[0] = Some(("context.peak", c.energy));
            }
        }
        if c.energy < cfg.peak_off {
            c.peak_latched = false;
        }
        if c.in_peak && held(&mut c.settle_since, c.energy < cfg.peak_off, now) >= cfg.settle_hold.ns() {
            c.in_peak = false;
            emit[1] = Some(("context.settle", c.energy));
        }
        // song peak: an armed (drop/section) run of high song energy, never while talking
        let armed = now < c.arm_until || c.song_high_since.is_some();
        let run = held(&mut c.song_high_since, armed && !c.talking && c.song >= cfg.song_peak_on, now);
        if run >= cfg.song_peak_hold.ns() {
            c.song_high_since = None;
            c.arm_until = 0;
            if cooled(c.last_song_peak, now, cfg.song_peak_cooldown.ns()) {
                c.last_song_peak = Some(now);
                emit[2] = Some(("context.song_peak", (0.5 * c.arm_strength + 0.5 * c.song).clamp(0.0, 1.0)));
            }
        }
        if emit.iter().all(Option::is_none) {
            return;
        }
        let mood = self.context.mood.as_str();
        for (ty, v) in emit.into_iter().flatten() {
            let p = match ty {
                "context.peak" => Value::map().with("energy", v as f64).with("mood", mood),
                "context.settle" => Value::map().with("mood", mood),
                _ => Value::map().with("strength", v as f64).with("mood", mood),
            };
            self.context_emit(ty, p, &Ctx::default());
        }
    }

    fn evaluate_mood(&mut self) {
        let genre = match self.state.get("song.current.genres").and_then(Value::as_list) {
            Some(l) => genre_mood(&l.iter().filter_map(Value::as_str).collect::<Vec<_>>()),
            None => None,
        };
        let mood = decide_mood(&self.context.mood_f, genre);
        let previous = self.context.mood;
        if mood == previous {
            return;
        }
        self.context.mood = mood;
        self.set_sys(MOOD, Value::Str(mood.as_str().into()));
        self.context_emit("context.mood", Value::map().with("mood", mood.as_str()).with("previous", previous.as_str()), &Ctx::default());
    }
}

/// Origins a person at the desk drives directly.
fn operator_origin(o: Origin) -> bool {
    matches!(o, Origin::Ui | Origin::Cli | Origin::Api | Origin::Deck | Origin::Midi | Origin::Voice | Origin::Osc | Origin::Mixer | Origin::Binding)
}

/// An event a person at the desk caused (rules reacting to it run as operator content).
pub(super) fn operator_event(ev: &Event) -> bool {
    match &ev.actor {
        Some(a) => a.platform == "local" || a.top_role() >= Role::Owner,
        None => operator_origin(ev.origin),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn director_deck_switch_canonicalizes_legacy_override_and_survives_restart() {
        use crate::config::SourceFile;
        use crate::state::Override;
        use crate::Input;
        use se_proto::{Command, Op};

        let config = || Config::build(&[SourceFile {
            kind: "project".into(), name: "project".into(), path: "project.toml".into(),
            table: toml::from_str("schema = 1\nstart_mode = \"offline\"").unwrap(),
        }]);
        let mut c = Core::new(config(), 10 * MS);
        let id = c.state.id(LIGHTS_AUTO).unwrap();
        c.state.put_override(id, Override {
            key: "manual".into(), priority: 299, value: Value::Bool(false), seq: 1,
            expires: None, anim: None, origin: Origin::Deck, causal: None,
        });
        let now = c.now();
        c.state.refresh(id, now);
        assert_eq!(c.get(LIGHTS_AUTO), Some(&Value::Bool(false)));
        let legacy_snapshot = c.runtime_state();
        let mut legacy_restored = Core::new(config(), 10 * MS);
        legacy_restored.restore(&legacy_snapshot);
        legacy_restored.step();
        assert_eq!(legacy_restored.get(LIGHTS_AUTO), Some(&Value::Bool(false)));
        legacy_restored.submit(Input::Command { cmd: Command::new(Origin::Deck, Op::Set { address: LIGHTS_AUTO.into(), value: Value::Bool(true) }) });
        legacy_restored.step();
        assert_eq!(legacy_restored.get(LIGHTS_AUTO), Some(&Value::Bool(true)));
        // The base is already true: removing the old override must still refresh resolution.
        c.submit(Input::Command { cmd: Command::new(Origin::Deck, Op::Set { address: LIGHTS_AUTO.into(), value: Value::Bool(true) }) });
        c.step();
        assert_eq!(c.get(LIGHTS_AUTO), Some(&Value::Bool(true)));
        c.submit(Input::Command { cmd: Command::new(Origin::Deck, Op::Set { address: LIGHTS_AUTO.into(), value: Value::Bool(false) }) });
        c.step();
        assert_eq!(c.get(LIGHTS_AUTO), Some(&Value::Bool(false)));
        let snapshot = c.runtime_state();
        let mut restored = Core::new(config(), 10 * MS);
        restored.restore(&snapshot);
        restored.step();
        assert_eq!(restored.get(LIGHTS_AUTO), Some(&Value::Bool(false)));
        // Repeated explicit off is safe; the durable switch must remain off.
        restored.submit(Input::Command { cmd: Command::new(Origin::Deck, Op::Action { name: "lights.auto.off".into(), args: Value::Null }) });
        restored.step();
        assert_eq!(restored.get(LIGHTS_AUTO), Some(&Value::Bool(false)));
        restored.submit(Input::Command { cmd: Command::new(Origin::Deck, Op::Set { address: LIGHTS_AUTO.into(), value: Value::Bool(true) }) });
        restored.step();
        assert_eq!(restored.get(LIGHTS_AUTO), Some(&Value::Bool(true)));
    }

    #[test]
    fn director_varies_motion_without_recoloring_three_hour_song() {
        let mut d = LightDirector::default();
        let mut previous = 0.0;
        let mut intervals = [0usize; 2];
        for second in 0..=10_800u64 {
            let beat = second as f64 * 2.0;
            let out = d.update(second * 1_000 * MS, true, "track", Mood::Groove, 0.5, false, beat);
            assert_eq!(out.palette, second == 0);
            if out.motion && second != 0 {
                let interval = beat - previous;
                assert!(interval == 64.0 || interval == 128.0, "motion interval: {interval}");
                intervals[usize::from(interval == 128.0)] += 1;
                previous = beat;
            }
        }
        assert!(intervals[0] >= 100 && intervals[1] >= 100);
        // A player/grid reset rebases the next pick instead of ending musical variety.
        assert!(!d.update(10_801_000 * MS, true, "track", Mood::Groove, 0.5, false, 0.0).motion);
        let deadline = d.motion_due.unwrap();
        assert!(d.update(10_865_000 * MS, true, "track", Mood::Groove, 0.5, false, deadline).motion);
    }

    #[test]
    fn director_palette_song_cadence_and_mood_are_independent() {
        let mut d = LightDirector::default();
        assert!(d.update(0, true, "track", Mood::Groove, 0.5, false, 0.0).palette);
        for song in 1..=9u64 {
            d.song_started = true;
            let out = d.update(song * 1_000 * MS, true, "track", Mood::Groove, 0.5, false, song as f64);
            assert!(out.motion);
            assert_eq!(out.palette, matches!(song, 3 | 5 | 8));
        }
        let out = d.update(10_000 * MS, true, "track", Mood::Heavy, 0.5, false, 10.0);
        assert!(out.palette);
        assert!(!out.motion);
        assert!(matches!(d.character(), "solid" | "rich"));
    }

    #[test]
    fn director_energy_sections_and_peaks_respect_boundaries() {
        let mut d = LightDirector::default();
        d.update(0, true, "track", Mood::Heavy, 0.5, false, 0.0);
        assert!(!d.update(1_000 * MS, true, "track", Mood::Heavy, 0.9, false, 2.0).motion);
        assert!(d.update(4_000 * MS, true, "track", Mood::Heavy, 0.9, false, 8.0).motion);
        assert_eq!(d.energy, "high"); // high energy alone must not strobe
        d.drop = true;
        assert!(d.update(5_000 * MS, true, "track", Mood::Heavy, 0.9, false, 10.0).drop);
        assert_eq!(d.energy, "peak");
        assert!(d.update(9_000 * MS, true, "track", Mood::Heavy, 0.9, false, 18.0).motion);
        assert_eq!(d.energy, "high");
        d.section = true;
        assert!(d.update(10_000 * MS, true, "track", Mood::Heavy, 0.2, false, 20.0).motion);
        assert_eq!((d.energy, d.family), ("low", "breakdown"));
        d.section = true;
        d.update(11_000 * MS, true, "track", Mood::Heavy, 0.7, true, 22.0);
        assert_eq!((d.energy, d.family), ("high", "build"));
    }

    #[test]
    fn director_build_survives_phrase_entrance_but_drop_can_interrupt() {
        let mut d = LightDirector::default();
        d.update(0, true, "track", Mood::Hype, 0.7, true, 4.0);
        assert_eq!(d.family, "build");
        assert!(!d.update(6_000 * MS, true, "track", Mood::Hype, 0.7, false, 16.0).motion);
        assert_eq!(d.family, "build");
        assert!(!d.update(13_000 * MS, true, "track", Mood::Hype, 0.7, false, 30.0).motion);
        d.drop = true;
        let out = d.update(14_000 * MS, true, "track", Mood::Hype, 0.7, false, 32.0);
        assert!(out.drop && out.motion);
        assert_eq!((d.energy, d.family), ("high", "motion"));
    }

    #[test]
    fn director_unstable_energy_cannot_accumulate_a_stable_band() {
        let mut d = LightDirector::default();
        d.update(0, true, "track", Mood::Groove, 0.5, false, 0.0);
        for second in 1..=10u64 {
            let level = if second % 2 == 0 { 0.2 } else { 0.8 };
            let out = d.update(second * 1_000 * MS, true, "track", Mood::Groove, level, false, second as f64 * 2.0);
            assert!(!out.motion);
            assert_eq!(d.energy, "mid");
        }
        d.update(11_000 * MS, true, "track", Mood::Groove, 0.8, false, 22.0);
        assert!(d.update(14_000 * MS, true, "track", Mood::Groove, 0.8, false, 28.0).motion);
        assert_eq!(d.energy, "high");
    }

    #[test]
    fn director_drums_talking_idle_and_toggle_do_not_replay_specials() {
        let mut d = LightDirector::default();
        let out = d.update(0, true, "drums", Mood::None, 0.5, false, 0.0);
        assert!(out.palette && out.motion);
        assert_eq!(d.source, "drums");
        assert!(!d.update(1_000 * MS, true, "idle", Mood::None, 0.0, false, 2.0).idle);
        assert!(d.update(3_000 * MS, true, "idle", Mood::None, 0.0, false, 6.0).idle);
        assert!(!d.update(4_000 * MS, true, "idle", Mood::None, 0.0, false, 8.0).idle);
        let out = d.update(5_000 * MS, true, "talking", Mood::Heavy, 0.9, false, 10.0);
        assert!(out.motion && out.palette);
        assert_eq!((d.energy, d.family, d.mood), ("low", "ambient", Mood::Chill));
        d.special = "raid";
        d.special_until = 25_000 * MS;
        d.special_pending = true;
        d.drop = true;
        let out = d.update(6_000 * MS, false, "track", Mood::Hype, 0.9, false, 12.0);
        assert!(!out.special && !out.palette && !out.motion && !out.drop);
        let out = d.update(7_000 * MS, true, "track", Mood::Hype, 0.9, false, 14.0);
        assert!(out.palette && out.motion);
        assert!(!out.special && !out.drop);
        assert_eq!(d.energy, "high");
    }

    #[test]
    fn director_celebration_is_finite_and_restores_current_context() {
        let mut d = LightDirector::default();
        d.update(0, true, "track", Mood::Groove, 0.5, false, 0.0);
        d.special = "raid";
        d.special_until = 21_000 * MS;
        d.special_pending = true;
        let out = d.update(1_000 * MS, true, "track", Mood::Groove, 0.5, false, 2.0);
        assert!(out.special && out.motion && !out.palette);
        assert_eq!(d.energy, "high");
        assert!(!d.update(2_000 * MS, true, "track", Mood::Heavy, 0.3, false, 4.0).palette);
        let out = d.update(21_000 * MS, true, "track", Mood::Heavy, 0.3, false, 42.0);
        assert!(out.palette && out.motion);
        assert_eq!((d.mood, d.energy, d.family), (Mood::Heavy, "low", "breakdown"));
        d.special = "hypetrain";
        d.special_until = 54_000 * MS;
        d.special_pending = true;
        d.update(22_000 * MS, true, "idle", Mood::Heavy, 0.0, false, 44.0);
        // Ending a train early restores idle immediately, not at its old timeout.
        d.special_until = 23_000 * MS;
        let out = d.update(23_000 * MS, true, "idle", Mood::Heavy, 0.0, false, 46.0);
        assert!(out.idle && !out.palette && !out.motion);
    }

    #[test]
    fn genres_map_to_moods_most_specific_first() {
        let m = |g: &[&str]| genre_mood(g);
        assert_eq!(m(&["pop punk"]), Some(Mood::Hype));
        assert_eq!(m(&["punk"]), Some(Mood::Hype));
        assert_eq!(m(&["heavy metal"]), Some(Mood::Heavy));
        assert_eq!(m(&["metalcore", "pop"]), Some(Mood::Heavy));
        assert_eq!(m(&["lo-fi", "hip hop"]), Some(Mood::Chill));
        assert_eq!(m(&["funk"]), Some(Mood::Groove));
        assert_eq!(m(&["r&b"]), Some(Mood::Groove));
        assert_eq!(m(&["synth-pop"]), Some(Mood::Bright));
        assert_eq!(m(&["edm"]), Some(Mood::Bright));
        assert_eq!(m(&["Jazz"]), Some(Mood::Chill));
        assert_eq!(m(&["polka"]), None);
        assert_eq!(m(&[]), None);
        // the first genre with a known keyword decides
        assert_eq!(m(&["polka", "disco"]), Some(Mood::Groove));
    }

    #[test]
    fn audio_alone_and_quiet_passages() {
        let f = |bpm, centroid, bass_ratio, energy| MoodFeatures { bpm, centroid, bass_ratio, energy };
        assert_eq!(audio_mood(&f(170.0, 0.5, 0.3, 0.8)), Mood::Hype);
        assert_eq!(audio_mood(&f(120.0, 0.4, 0.6, 0.7)), Mood::Heavy);
        assert_eq!(audio_mood(&f(80.0, 0.4, 0.3, 0.4)), Mood::Chill);
        assert_eq!(audio_mood(&f(120.0, 0.7, 0.3, 0.5)), Mood::Bright);
        assert_eq!(audio_mood(&f(110.0, 0.5, 0.4, 0.5)), Mood::Groove);
        assert_eq!(audio_mood(&f(110.0, 0.5, 0.4, 0.1)), Mood::Chill);
        // a known genre wins over the audio, except in a near-silent passage
        assert_eq!(decide_mood(&f(110.0, 0.7, 0.3, 0.5), Some(Mood::Heavy)), Mood::Heavy);
        assert_eq!(decide_mood(&f(110.0, 0.7, 0.3, 0.1), Some(Mood::Heavy)), Mood::Chill);
    }

    #[test]
    fn exempt_patches_and_effect_presets() {
        let list = vec!["win31_alerts".to_string(), "fx.grade".to_string()];
        assert!(exempt(&list, "patch.win31_alerts"));
        assert!(exempt(&list, "fx.grade"));
        assert!(!exempt(&list, "patch.win31_alerts_tall"));
        assert!(!exempt(&list, "patch.confetti"));
        let p = |src: &str| -> PresetDef { toml::from_str(src).unwrap() };
        assert!(effect_preset(&p("lane = \"moment\""), &list));
        assert!(effect_preset(&p("fx = [{ name = \"patch.confetti\" }]"), &list));
        assert!(effect_preset(&p("fx = [{ name = \"glitch\" }]"), &list));
        assert!(!effect_preset(&p("fx = [{ name = \"patch.win31_alerts\" }]"), &list));
        assert!(!effect_preset(&p("lights = { look = \"warm\" }"), &list));
    }
}
