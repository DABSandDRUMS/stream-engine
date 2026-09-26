//! `[clips]` settings in `project.toml` (all optional; defaults are production values).
//!
//! ```toml
//! [clips]
//! auto_process = true          # queue the clip job when a session closes (§15.8)
//! encoder = "auto"             # auto (NVENC, x264 when NVENC is busy) | nvenc | x264
//! tall_source = "auto"         # auto (tall recording if present, else crop) | recording | crop
//! rank_command = []            # optional external ranker (argv); JSON on stdin → JSON on stdout
//! upload_command = []          # optional upload hook (argv); JSON on stdin → {"url": …}
//!
//! [clips.hype]                 # hype detector (§18)
//! threshold = 1.0
//! [clips.hype.weights]
//! chat = 0.8
//!
//! [clips.audio]                # which OBS tracks make up clip audio
//! drop = ["music", "program"]
//! [clips.audio.roles]
//! music = ["se-music", "*music*"]
//! ```

use se_core::config::Dur;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ClipsConfig {
    /// Queue the post-stream job when a session closes.
    pub auto_process: bool,
    /// Sub-directory of `sessions/<id>/` for rendered clips.
    pub output_dir: String,
    pub encoder: Encoder,
    /// Canvases to render (`wide`, `tall`).
    pub canvases: Vec<String>,
    pub tall_source: TallSource,
    /// Horizontal center (0 = left, 1 = right) of the 9:16 crop taken from the wide recording.
    pub tall_crop_center: f64,
    pub wide_size: [u32; 2],
    pub tall_size: [u32; 2],
    pub min_len: Dur,
    pub max_len: Dur,
    /// Window for markers without a hype window (`stream marker`, deck "clip that").
    pub manual_preroll: Dur,
    pub manual_postroll: Dur,
    /// Marker score given to manual markers (hype markers carry their own).
    pub manual_score: f64,
    /// Most clips rendered per session (best ranked first).
    pub max_clips: usize,
    /// Extra seconds searched around the marker window for sentence boundaries.
    pub boundary_slack: Dur,
    /// Optional external ranker: argv, receives the candidates as JSON on stdin.
    pub rank_command: Vec<String>,
    pub rank_timeout: Dur,
    /// Optional upload hook: argv, receives the clip as JSON on stdin.
    pub upload_command: Vec<String>,
    pub upload_timeout: Dur,
    /// Run the upload hook as soon as a clip is approved.
    pub upload_on_approve: bool,
    /// Nice level for the job's ffmpeg and Whisper threads (the show keeps priority).
    pub nice: i32,
    pub video: VideoConfig,
    pub captions: CaptionConfig,
    pub whisper: WhisperConfig,
    pub audio: AudioConfig,
    pub ranking: RankingConfig,
    pub hype: HypeConfig,
}

impl Default for ClipsConfig {
    fn default() -> Self {
        ClipsConfig {
            auto_process: true,
            output_dir: "clips".into(),
            encoder: Encoder::Auto,
            canvases: vec!["wide".into(), "tall".into()],
            tall_source: TallSource::Auto,
            tall_crop_center: 0.5,
            wide_size: [1920, 1080],
            tall_size: [1080, 1920],
            min_len: Dur(8_000),
            max_len: Dur(60_000),
            manual_preroll: Dur(30_000),
            manual_postroll: Dur(8_000),
            manual_score: 3.0,
            max_clips: 20,
            boundary_slack: Dur(4_000),
            rank_command: Vec::new(),
            rank_timeout: Dur(120_000),
            upload_command: Vec::new(),
            upload_timeout: Dur(600_000),
            upload_on_approve: false,
            nice: 10,
            video: VideoConfig::default(),
            captions: CaptionConfig::default(),
            whisper: WhisperConfig::default(),
            audio: AudioConfig::default(),
            ranking: RankingConfig::default(),
            hype: HypeConfig::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Encoder {
    /// NVENC; libx264 when NVENC can't open a session (OBS holds them, driver limit).
    Auto,
    Nvenc,
    X264,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TallSource {
    /// The tall canvas recording when one covers the clip, else a crop of the wide one.
    Auto,
    Recording,
    Crop,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct VideoConfig {
    /// NVENC constant-quality target (lower = better).
    pub nvenc_cq: u32,
    pub nvenc_preset: String,
    pub x264_crf: u32,
    pub x264_preset: String,
    pub max_bitrate: String,
    pub audio_bitrate: String,
    /// Loudness-normalize clip audio (social platforms target about -14 LUFS).
    pub loudnorm: bool,
    pub loudness_lufs: f64,
    /// Thumbnail width in pixels (height follows the aspect ratio).
    pub thumb_width: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        VideoConfig {
            nvenc_cq: 21,
            nvenc_preset: "p5".into(),
            x264_crf: 20,
            x264_preset: "veryfast".into(),
            max_bitrate: "20M".into(),
            audio_bitrate: "192k".into(),
            loudnorm: true,
            loudness_lufs: -14.0,
            thumb_width: 480,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CaptionConfig {
    pub enabled: bool,
    pub font: String,
    /// Font size in pixels of the output frame.
    pub size_wide: u32,
    pub size_tall: u32,
    /// Longest caption line (characters) before a new caption starts.
    pub max_chars_wide: usize,
    pub max_chars_tall: usize,
    /// Longest time one caption stays up.
    pub max_caption: Dur,
    /// Distance from the bottom edge in pixels.
    pub margin_wide: u32,
    pub margin_tall: u32,
    /// Text, spoken-word highlight, and outline colors (`#rrggbb`).
    pub color: String,
    pub highlight: String,
    pub outline: String,
    pub uppercase: bool,
}

impl Default for CaptionConfig {
    fn default() -> Self {
        CaptionConfig {
            enabled: true,
            font: "sans-serif".into(),
            size_wide: 64,
            size_tall: 78,
            max_chars_wide: 42,
            max_chars_tall: 22,
            max_caption: Dur(3_500),
            margin_wide: 70,
            margin_tall: 520,
            color: "#ffffff".into(),
            highlight: "#ffd83d".into(),
            outline: "#000000".into(),
            uppercase: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct WhisperConfig {
    /// ggml model name (`small.en`, `base.en`, `small.en-q5_1`, …) or an absolute path.
    pub model: String,
    pub language: String,
    /// CPU threads for Whisper (0 = half the cores).
    pub threads: u32,
    /// Transcript window padding around each clip window (room for retrims).
    pub pad: Dur,
    /// Words the model should expect (names, slang) as an initial prompt.
    pub prompt: String,
}

impl Default for WhisperConfig {
    fn default() -> Self {
        WhisperConfig { model: "small.en".into(), language: "en".into(), threads: 0, pad: Dur(12_000), prompt: String::new() }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    /// Roles whose tracks are left out of clip audio (music is dropped by default).
    pub drop: Vec<String>,
    /// Roles to transcribe, in order of preference (the first role with a track wins).
    pub transcribe: Vec<String>,
    /// Fallback role names by audio-stream index when the recording has no track info.
    pub tracks: Vec<String>,
    /// Role → glob patterns matched against a track's name, OBS sources, and PipeWire nodes.
    pub roles: BTreeMap<String, Vec<String>>,
    /// What to do when every track carries music (single mixed track): `keep` (flag the
    /// clip) or `mute`.
    pub mixed: MixedPolicy,
}

impl Default for AudioConfig {
    fn default() -> Self {
        let roles = [
            ("music", &["se-music", "*music*", "*youtube*", "*song*"][..]),
            ("program", &["se-program", "*program*"][..]),
            ("mic", &["se-mic", "*mic*", "*voice*", "*vocal*"][..]),
            ("band", &["se-band", "*band*", "*studio 24c*", "*24c*"][..]),
            ("drums", &["se-drums", "*drum*"][..]),
            ("tts", &["se-tts", "*tts*"][..]),
            ("sfx", &["se-sfx", "*sfx*", "*alert*"][..]),
            ("game", &["se-game", "*game*"][..]),
        ];
        AudioConfig {
            drop: vec!["music".into(), "program".into()],
            transcribe: vec!["mic".into(), "band".into()],
            tracks: Vec::new(),
            roles: roles.iter().map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect())).collect(),
            mixed: MixedPolicy::Keep,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MixedPolicy {
    Keep,
    Mute,
}

/// Deterministic ranking weights (score = marker score × `marker` + transcript features).
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RankingConfig {
    pub marker: f64,
    /// Per word/second of speech around the peak (up to 4 w/s).
    pub speech: f64,
    /// Per excited token (`!`, laughter, "let's go", keywords).
    pub excitement: f64,
    /// Penalty per second beyond 45 s (short clips travel better).
    pub length_penalty: f64,
    /// Bonus when the clip starts and ends on sentence boundaries.
    pub clean_edges: f64,
    /// Extra phrases that count as excitement.
    pub keywords: Vec<String>,
}

impl Default for RankingConfig {
    fn default() -> Self {
        RankingConfig { marker: 1.0, speech: 0.15, excitement: 0.25, length_penalty: 0.02, clean_edges: 0.2, keywords: Vec::new() }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct HypeConfig {
    pub enabled: bool,
    /// Score that opens a hype episode (also live-adjustable at `clips.hype.threshold`).
    pub threshold: f64,
    /// An episode ends when the score stays below `threshold × release` for `hold`.
    pub release: f64,
    pub hold: Dur,
    /// Pre-roll bounds: windows start 10–30 s before the peak (§18).
    pub preroll_min: Dur,
    pub preroll_max: Dur,
    pub postroll: Dur,
    pub max_len: Dur,
    /// Episodes closer than this merge into one marker.
    pub merge_gap: Dur,
    /// Extra lag for chat reactions after the stream delay before a moment is scored.
    pub settle: Dur,
    /// Stream delay used until the Twitch adapter has measured it.
    pub fallback_delay: Dur,
    /// Short window for chat/emote rates.
    pub chat_window: Dur,
    /// Baseline time constant for chat/emote/mic baselines.
    pub baseline: Dur,
    /// Floors so a quiet channel's baseline never divides by ~0 (per second).
    pub min_chat_rate: f64,
    pub min_emote_rate: f64,
    /// Unique `!clip` voters within `vote_window` that force a marker.
    pub clip_votes: usize,
    pub vote_window: Dur,
    pub clip_command: String,
    /// Decay time constant of event impulses (bits, subs, raids, drops).
    pub impulse_tau: Dur,
    /// Emote names counted in chat text (in addition to emotes the adapter reports).
    pub emotes: Vec<String>,
    /// Inputs shifted back by the stream delay (viewer reactions).
    pub shift: Vec<String>,
    /// Mic/music signals sampled for spikes, laughter, and drops.
    pub mic_signal: String,
    pub music_signals: Vec<String>,
    pub chat_rate_signal: String,
    pub weights: HypeWeights,
}

impl Default for HypeConfig {
    fn default() -> Self {
        HypeConfig {
            enabled: true,
            threshold: 1.0,
            release: 0.6,
            hold: Dur(2_000),
            preroll_min: Dur(10_000),
            preroll_max: Dur(30_000),
            postroll: Dur(5_000),
            max_len: Dur(60_000),
            merge_gap: Dur(8_000),
            settle: Dur(2_000),
            fallback_delay: Dur(3_000),
            chat_window: Dur(5_000),
            baseline: Dur(300_000),
            min_chat_rate: 0.2,
            min_emote_rate: 0.1,
            clip_votes: 3,
            vote_window: Dur(30_000),
            clip_command: "!clip".into(),
            impulse_tau: Dur(8_000),
            emotes: DEFAULT_EMOTES.iter().map(|s| s.to_string()).collect(),
            shift: vec!["chat".into(), "emotes".into(), "votes".into(), "bits".into()],
            mic_signal: "mic.level".into(),
            music_signals: vec!["music.level".into(), "band.level".into()],
            chat_rate_signal: "twitch.chat_rate".into(),
            weights: HypeWeights::default(),
        }
    }
}

pub const DEFAULT_EMOTES: &[&str] = &[
    "LUL",
    "LULW",
    "KEKW",
    "OMEGALUL",
    "Pog",
    "PogU",
    "PogChamp",
    "POGGERS",
    "Kreygasm",
    "HYPERS",
    "PepeLaugh",
    "monkaS",
    "catJAM",
    "EZ",
    "Clap",
    "WAYTOODANK",
    "PauseChamp",
    "KomodoHype",
    "SeemsGood",
    "Kappa",
    "LETSGO",
    "GIGACHAD",
];

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct HypeWeights {
    /// × ln(chat rate / baseline).
    pub chat: f64,
    /// × ln(emote rate / baseline).
    pub emotes: f64,
    /// Per repeated message (copypasta/spam wave) in the chat window.
    pub copypasta: f64,
    /// Per unique `!clip` voter.
    pub votes: f64,
    /// × log10(1 + bits/100).
    pub bits: f64,
    /// Per sub (× tier: 1, 2, 4).
    pub sub: f64,
    /// × log2(1 + gifted subs).
    pub gift: f64,
    /// × log10(1 + raiders/10).
    pub raid: f64,
    /// × log10(1 + amount).
    pub tip: f64,
    pub hype_train: f64,
    /// Per `band.drop` / `music.drop` event.
    pub drop: f64,
    /// × mic level z-score above 2.5 σ.
    pub mic: f64,
    /// × laughter strength (bursty mic energy at 3–8 Hz).
    pub laughter: f64,
    /// × music/band level novelty.
    pub novelty: f64,
}

impl Default for HypeWeights {
    fn default() -> Self {
        HypeWeights {
            chat: 0.8,
            emotes: 0.5,
            copypasta: 0.08,
            votes: 0.4,
            bits: 0.6,
            sub: 0.25,
            gift: 0.4,
            raid: 0.5,
            tip: 0.5,
            hype_train: 0.5,
            drop: 0.5,
            mic: 0.5,
            laughter: 0.6,
            novelty: 0.3,
        }
    }
}

impl ClipsConfig {
    /// Parse `[clips]` (missing section = defaults).
    pub fn from_section(v: Option<&toml::Value>) -> Result<ClipsConfig, String> {
        let Some(v) = v else { return Ok(ClipsConfig::default()) };
        let c: ClipsConfig = v.clone().try_into().map_err(|e: toml::de::Error| format!("[clips]: {}", e.message()))?;
        c.validate()?;
        Ok(c)
    }

    fn validate(&self) -> Result<(), String> {
        let h = &self.hype;
        if h.threshold.is_nan() || h.threshold <= 0.0 {
            return Err("[clips.hype] threshold must be > 0".into());
        }
        if !(0.0..1.0).contains(&h.release) {
            return Err("[clips.hype] release must be in [0, 1)".into());
        }
        if h.preroll_min.0 > h.preroll_max.0 {
            return Err("[clips.hype] preroll_min must be ≤ preroll_max".into());
        }
        if self.min_len.0 == 0 || self.min_len.0 > self.max_len.0 {
            return Err("[clips] min_len must be > 0 and ≤ max_len".into());
        }
        if !(0.0..=1.0).contains(&self.tall_crop_center) {
            return Err("[clips] tall_crop_center must be in [0, 1]".into());
        }
        for c in &self.canvases {
            if c != "wide" && c != "tall" {
                return Err(format!("[clips] unknown canvas `{c}` (wide | tall)"));
            }
        }
        for s in [self.wide_size, self.tall_size] {
            if s[0] < 16 || s[1] < 16 || s[0] % 2 == 1 || s[1] % 2 == 1 {
                return Err("[clips] clip sizes must be even and ≥ 16".into());
            }
        }
        for c in [&self.captions.color, &self.captions.highlight, &self.captions.outline] {
            if crate::captions::ass_color(c).is_none() {
                return Err(format!("[clips.captions] bad color `{c}` (use #rrggbb)"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_partial_sections_and_rejects_bad_values() {
        let v: toml::Value = toml::from_str(
            r#"
            encoder = "x264"
            min_len = "5s"
            [hype]
            threshold = 1.5
            preroll_max = "20s"
            [hype.weights]
            chat = 1.2
            [audio.roles]
            music = ["yt-*"]
            "#,
        )
        .unwrap();
        let c = ClipsConfig::from_section(Some(&v)).unwrap();
        assert_eq!(c.encoder, Encoder::X264);
        assert_eq!(c.min_len, Dur(5_000));
        assert_eq!(c.hype.threshold, 1.5);
        assert_eq!(c.hype.preroll_max, Dur(20_000));
        assert_eq!(c.hype.weights.chat, 1.2);
        assert_eq!(c.hype.weights.bits, HypeWeights::default().bits);
        // a table given for roles replaces the defaults
        assert_eq!(c.audio.roles.get("music").unwrap(), &vec!["yt-*".to_string()]);

        let bad: toml::Value = toml::from_str("[hype]\npreroll_min = \"40s\"").unwrap();
        assert!(ClipsConfig::from_section(Some(&bad)).unwrap_err().contains("preroll_min"));
        let typo: toml::Value = toml::from_str("encodr = \"x264\"").unwrap();
        assert!(ClipsConfig::from_section(Some(&typo)).is_err());
        assert_eq!(ClipsConfig::from_section(None).unwrap(), ClipsConfig::default());
    }
}
