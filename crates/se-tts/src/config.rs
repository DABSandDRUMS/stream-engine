//! `[tts]` in `project.toml` and voice selection.
//!
//! ```toml
//! [tts]
//! enabled   = true                  # subsystem on/off (the live toggle is the `tts.enabled` state)
//! model_dir = "~/.local/share/stream-engine/models/kokoro"   # default: <data dir>/models/kokoro
//! model     = "model_quantized.onnx"
//! voice     = "af_heart"
//! speed     = 1.0                   # 0.5–2.0
//! volume    = 0.9                   # 0–2 (linear gain into the `tts` bus)
//! max_chars = 300                   # text is cut at a word boundary
//! max_queue = 20
//! lang      = "en-us"               # espeak voice; default: from the voice prefix (af_ → en-us, bf_ → en-gb, …)
//!
//! [[tts.voices]]                    # first match wins; `when` sees the request args
//! when  = "kind == 'twitch.cheer' && amount >= 1000 or tier >= 2"
//! voice = "am_michael"
//! speed = 1.1
//! ```

use crate::kokoro::SPEED_RANGE;
use crate::phonemize::{lang_for_voice, valid_lang};
use se_expr::{Expr, Scope};
use se_proto::Value;
use std::path::{Path, PathBuf};

pub const DEFAULT_MODEL: &str = "model_quantized.onnx";
pub const DEFAULT_VOICE: &str = "af_heart";
pub const DEFAULT_LANG: &str = "en-us";

#[derive(Clone, Debug)]
pub struct VoiceRule {
    pub when: Expr,
    pub voice: Option<String>,
    pub speed: Option<f32>,
    pub lang: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TtsConfig {
    pub enabled: bool,
    pub model_dir: PathBuf,
    pub model: String,
    pub voice: String,
    pub speed: f32,
    pub volume: f32,
    pub max_chars: usize,
    pub max_queue: usize,
    /// Explicit espeak language for every voice; `None` = derive from the voice prefix.
    pub lang: Option<String>,
    pub voices: Vec<VoiceRule>,
}

/// Voice, speed, and espeak language chosen for one request.
#[derive(Clone, Debug, PartialEq)]
pub struct Selected {
    pub voice: String,
    pub speed: f32,
    pub lang: String,
}

/// Rule `when` expressions see the request args (`kind`, `amount`, `tier`, `user`, …).
pub struct ArgsScope<'a>(pub &'a Value);

impl Scope for ArgsScope<'_> {
    fn lookup(&self, path: &str) -> Value {
        self.0.get_path(path).cloned().unwrap_or(Value::Null)
    }
}

pub fn valid_voice_name(v: &str) -> bool {
    !v.is_empty() && v.len() <= 64 && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn valid_model_name(m: &str) -> bool {
    m.ends_with(".onnx") && !m.contains('/') && !m.contains('\\') && !m.starts_with('.')
}

impl TtsConfig {
    pub fn defaults(data_dir: &Path) -> TtsConfig {
        TtsConfig {
            enabled: true,
            model_dir: data_dir.join("models").join("kokoro"),
            model: DEFAULT_MODEL.into(),
            voice: DEFAULT_VOICE.into(),
            speed: 1.0,
            volume: 0.9,
            max_chars: 300,
            max_queue: 20,
            lang: None,
            voices: Vec::new(),
        }
    }

    pub fn model_path(&self) -> PathBuf {
        self.model_dir.join(&self.model)
    }

    pub fn voices_dir(&self) -> PathBuf {
        self.model_dir.join("voices")
    }

    /// Parse the `[tts]` section (absent = defaults). Unknown keys and out-of-range values are
    /// errors, so typos don't silently fall back.
    pub fn parse(section: Option<&toml::Value>, data_dir: &Path) -> Result<TtsConfig, String> {
        let mut c = TtsConfig::defaults(data_dir);
        let Some(section) = section else { return Ok(c) };
        let t = section.as_table().ok_or("[tts] must be a table")?;
        for (k, v) in t {
            let bad = |what: &str| format!("[tts] {k}: expected {what}, got `{v}`");
            match k.as_str() {
                "enabled" => c.enabled = v.as_bool().ok_or_else(|| bad("true/false"))?,
                "model_dir" => c.model_dir = expand_path(v.as_str().ok_or_else(|| bad("a path"))?, data_dir),
                "model" => {
                    let m = v.as_str().ok_or_else(|| bad("a file name"))?;
                    if !valid_model_name(m) {
                        return Err(bad("an .onnx file name inside model_dir"));
                    }
                    c.model = m.to_string();
                }
                "voice" => {
                    let s = v.as_str().filter(|s| valid_voice_name(s)).ok_or_else(|| bad("a voice name such as af_heart"))?;
                    c.voice = s.to_string();
                }
                "speed" => c.speed = ranged(v, SPEED_RANGE.0 as f64, SPEED_RANGE.1 as f64).ok_or_else(|| bad("a number 0.5–2.0"))? as f32,
                "volume" => c.volume = ranged(v, 0.0, 2.0).ok_or_else(|| bad("a number 0–2"))? as f32,
                "max_chars" => c.max_chars = ranged(v, 1.0, 5000.0).filter(|f| f.fract() == 0.0).ok_or_else(|| bad("an integer 1–5000"))? as usize,
                "max_queue" => c.max_queue = ranged(v, 1.0, 500.0).filter(|f| f.fract() == 0.0).ok_or_else(|| bad("an integer 1–500"))? as usize,
                "lang" => {
                    let s = v.as_str().filter(|s| valid_lang(s)).ok_or_else(|| bad("an espeak-ng language such as en-us"))?;
                    c.lang = Some(s.to_string());
                }
                "voices" => {
                    let list = v.as_array().ok_or_else(|| bad("an array of tables ([[tts.voices]])"))?;
                    c.voices = list.iter().enumerate().map(|(i, r)| parse_rule(i, r)).collect::<Result<_, _>>()?;
                }
                _ => return Err(format!("[tts] unknown key `{k}`")),
            }
        }
        Ok(c)
    }

    /// Voice for a request: an explicit (existing) override wins for the voice; otherwise the
    /// first matching rule, else the default. `lang`: rule → `[tts] lang` → voice prefix.
    pub fn select(&self, args: &Value, explicit_voice: Option<&str>) -> Selected {
        let rule = self.voices.iter().find(|r| r.when.eval_bool(&ArgsScope(args)));
        let voice = explicit_voice.or_else(|| rule.and_then(|r| r.voice.as_deref())).unwrap_or(&self.voice).to_string();
        let speed = rule.and_then(|r| r.speed).unwrap_or(self.speed);
        let lang = rule
            .and_then(|r| r.lang.clone())
            .or_else(|| self.lang.clone())
            .or_else(|| lang_for_voice(&voice).map(str::to_string))
            .unwrap_or_else(|| DEFAULT_LANG.to_string());
        Selected { voice, speed, lang }
    }
}

fn ranged(v: &toml::Value, lo: f64, hi: f64) -> Option<f64> {
    let f = v.as_float().or_else(|| v.as_integer().map(|i| i as f64))?;
    (lo..=hi).contains(&f).then_some(f)
}

fn expand_path(p: &str, data_dir: &Path) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    let p = PathBuf::from(p);
    if p.is_absolute() { p } else { data_dir.join(p) }
}

fn parse_rule(i: usize, v: &toml::Value) -> Result<VoiceRule, String> {
    let t = v.as_table().ok_or_else(|| format!("[[tts.voices]] #{}: expected a table", i + 1))?;
    let at = |k: &str| format!("[[tts.voices]] #{} {k}", i + 1);
    let mut when = None;
    let mut rule = VoiceRule { when: Expr::parse("true").expect("literal"), voice: None, speed: None, lang: None };
    for (k, v) in t {
        match k.as_str() {
            "when" => {
                let src = v.as_str().ok_or_else(|| format!("{}: expected an expression string", at(k)))?;
                when = Some(Expr::parse(src).map_err(|e| format!("{}: {e}", at(k)))?);
            }
            "voice" => {
                let s = v.as_str().filter(|s| valid_voice_name(s)).ok_or_else(|| format!("{}: expected a voice name", at(k)))?;
                rule.voice = Some(s.to_string());
            }
            "speed" => rule.speed = Some(ranged(v, SPEED_RANGE.0 as f64, SPEED_RANGE.1 as f64).ok_or_else(|| format!("{}: expected 0.5–2.0", at(k)))? as f32),
            "lang" => {
                let s = v.as_str().filter(|s| valid_lang(s)).ok_or_else(|| format!("{}: expected an espeak-ng language", at(k)))?;
                rule.lang = Some(s.to_string());
            }
            _ => return Err(format!("{}: unknown key", at(k))),
        }
    }
    rule.when = when.ok_or_else(|| format!("[[tts.voices]] #{}: missing `when`", i + 1))?;
    if rule.voice.is_none() && rule.speed.is_none() && rule.lang.is_none() {
        return Err(format!("[[tts.voices]] #{}: set at least one of voice, speed, lang", i + 1));
    }
    Ok(rule)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(src: &str) -> Result<TtsConfig, String> {
        let t: toml::Table = src.parse().unwrap();
        TtsConfig::parse(t.get("tts"), Path::new("/data"))
    }

    fn args(kind: &str, amount: i64, tier: i64) -> Value {
        Value::map().with("kind", kind).with("amount", amount).with("tier", tier).with("user", "drumfan")
    }

    #[test]
    fn defaults_and_paths() {
        let c = cfg("").unwrap();
        assert_eq!(c.model_path(), PathBuf::from("/data/models/kokoro/model_quantized.onnx"));
        assert_eq!((c.max_chars, c.max_queue, c.voice.as_str(), c.lang.clone()), (300, 20, "af_heart", None));
        assert_eq!(c.select(&Value::Null, None), Selected { voice: "af_heart".into(), speed: 1.0, lang: "en-us".into() });
        let c = cfg("[tts]\nmodel_dir = \"kokoro\"").unwrap();
        assert_eq!(c.model_dir, PathBuf::from("/data/kokoro"));
    }

    #[test]
    fn first_matching_rule_wins() {
        let c = cfg(r#"
            [tts]
            voice = "af_heart"
            speed = 0.9
            [[tts.voices]]
            when = "kind == 'twitch.cheer' && amount >= 1000 or tier >= 2"
            voice = "am_michael"
            speed = 1.1
            [[tts.voices]]
            when = "kind == 'twitch.cheer'"
            voice = "bf_emma"
            [[tts.voices]]
            when = "user == 'drumfan'"
            speed = 1.3
        "#)
        .unwrap();
        let big = c.select(&args("twitch.cheer", 1000, 0), None);
        assert_eq!(big, Selected { voice: "am_michael".into(), speed: 1.1, lang: "en-us".into() });
        assert_eq!(c.select(&args("twitch.sub", 0, 2), None).voice, "am_michael", "`or tier >= 2` branch");
        // second rule: voice from the rule, speed from [tts], lang from the British prefix
        assert_eq!(c.select(&args("twitch.cheer", 999, 1), None), Selected { voice: "bf_emma".into(), speed: 0.9, lang: "en-gb".into() });
        // speed-only rule keeps the default voice
        assert_eq!(c.select(&args("twitch.follow", 0, 0), None), Selected { voice: "af_heart".into(), speed: 1.3, lang: "en-us".into() });
        // explicit override replaces only the voice
        assert_eq!(c.select(&args("twitch.cheer", 5000, 0), Some("bm_george")), Selected { voice: "bm_george".into(), speed: 1.1, lang: "en-gb".into() });
        // missing fields are null → no match
        assert_eq!(c.select(&Value::map().with("text", "hi"), None).voice, "af_heart");
    }

    #[test]
    fn explicit_lang_overrides_prefix() {
        let c = cfg("[tts]\nlang = \"en-us\"\n[[tts.voices]]\nwhen = \"tier >= 3\"\nvoice = \"ef_dora\"\nlang = \"es\"").unwrap();
        assert_eq!(c.select(&args("x", 0, 0), Some("bf_emma")).lang, "en-us");
        assert_eq!(c.select(&args("x", 0, 3), None).lang, "es");
    }

    #[test]
    fn rejects_broken_config() {
        for (src, needle) in [
            ("[tts]\nspeeed = 1.0", "unknown key `speeed`"),
            ("[tts]\nspeed = 3.0", "speed"),
            ("[tts]\nmax_queue = 0", "max_queue"),
            ("[tts]\nmax_chars = 12.5", "max_chars"),
            ("[tts]\nmodel = \"../evil.onnx\"", "model"),
            ("[tts]\nvoice = \"af heart\"", "voice"),
            ("[tts]\nlang = \"--help\"", "lang"),
            ("[[tts.voices]]\nwhen = \"amount >=\"\nvoice = \"am_adam\"", "#1 when"),
            ("[[tts.voices]]\nvoice = \"am_adam\"", "missing `when`"),
            ("[[tts.voices]]\nwhen = \"true\"", "at least one"),
            ("tts = 5", "must be a table"),
        ] {
            let e = cfg(src).unwrap_err();
            assert!(e.contains(needle), "{src:?} → {e}");
        }
    }
}
