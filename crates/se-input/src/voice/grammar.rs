//! Fixed voice command grammar: scene / preset / release / mode / take / panic / clean /
//! next / previous / next song / marker / page, plus confirm / cancel. Names are matched
//! fuzzily against the project's scenes, presets, modes, and deck pages.

use se_proto::{Op, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum Intent {
    Scene { name: String, cut: bool },
    Preset { name: String },
    Release { name: String },
    Mode { name: String },
    Take,
    Panic,
    Clean,
    Next,
    Prev,
    NextSong,
    Marker { label: Option<String> },
    Page { name: String },
    Confirm,
    Cancel,
}

impl Intent {
    /// Grammar category (used for `[voice] confirm = [...]`).
    pub fn kind(&self) -> &'static str {
        match self {
            Intent::Scene { .. } => "scene",
            Intent::Preset { .. } => "preset",
            Intent::Release { .. } => "release",
            Intent::Mode { .. } => "mode",
            Intent::Take => "take",
            Intent::Panic => "panic",
            Intent::Clean => "clean",
            Intent::Next => "next",
            Intent::Prev => "prev",
            Intent::NextSong => "next_song",
            Intent::Marker { .. } => "marker",
            Intent::Page { .. } => "page",
            Intent::Confirm => "confirm",
            Intent::Cancel => "cancel",
        }
    }

    pub fn arg(&self) -> Option<&str> {
        match self {
            Intent::Scene { name, .. } | Intent::Preset { name } | Intent::Release { name } | Intent::Mode { name } | Intent::Page { name } => Some(name),
            Intent::Marker { label } => label.as_deref(),
            _ => None,
        }
    }

    /// The command this intent runs (`None` for confirm/cancel).
    pub fn op(&self) -> Option<Op> {
        let action = |name: &str, args: Value| Op::Action { name: name.into(), args };
        Some(match self {
            Intent::Scene { name, cut: false } => Op::SceneGo { scene: name.clone() },
            Intent::Scene { name, cut: true } => Op::SceneCut { scene: name.clone(), transition: None },
            Intent::Preset { name } => Op::PresetFire { name: name.clone(), payload: Value::Null },
            Intent::Release { name } => Op::PresetRelease { name: name.clone() },
            Intent::Mode { name } => Op::ModeSet { mode: name.clone() },
            Intent::Take => Op::SceneTake { transition: None, ms: None },
            Intent::Panic => Op::Panic,
            Intent::Clean => Op::Clean,
            Intent::Next => action("scene.next", Value::Null),
            Intent::Prev => action("scene.prev", Value::Null),
            Intent::NextSong => action("queue.skip", Value::Null),
            Intent::Marker { label } => action("session.marker", Value::map().with("label", label.clone().unwrap_or_else(|| "voice".into()))),
            Intent::Page { name } => action("deck.page", Value::map().with("args", vec![Value::Str(name.clone())])),
            Intent::Confirm | Intent::Cancel => return None,
        })
    }

    pub fn describe(&self) -> String {
        match self.op() {
            Some(op) => op.describe(),
            None => self.kind().into(),
        }
    }
}

/// Names the grammar can refer to: `(canonical name, spoken forms)`.
#[derive(Clone, Debug, Default)]
pub struct Vocab {
    pub scenes: Vec<(String, Vec<String>)>,
    pub presets: Vec<(String, Vec<String>)>,
    pub modes: Vec<(String, Vec<String>)>,
    pub pages: Vec<(String, Vec<String>)>,
}

impl Vocab {
    /// Spoken forms for a config name and optional label (`sub_big` → "sub big").
    pub fn forms(name: &str, label: Option<&str>) -> Vec<String> {
        let mut v = vec![normalize(name)];
        if let Some(l) = label {
            let n = normalize(l);
            if !n.is_empty() && !v.contains(&n) {
                v.push(n);
            }
        }
        v
    }

    /// Words to bias the recognizer toward (whisper initial prompt).
    pub fn prompt(&self) -> String {
        let mut words: Vec<String> = Vec::new();
        for (_, f) in self.scenes.iter().chain(&self.presets).chain(&self.modes) {
            if let Some(x) = f.first() {
                words.push(x.clone());
            }
        }
        format!("Stream commands: scene, preset, take, panic, clean, next scene, mode, marker, confirm, cancel. Names: {}.", words.join(", "))
    }
}

const NUMBERS: &[(&str, &str)] = &[
    ("zero", "0"),
    ("one", "1"),
    ("two", "2"),
    ("three", "3"),
    ("four", "4"),
    ("five", "5"),
    ("six", "6"),
    ("seven", "7"),
    ("eight", "8"),
    ("nine", "9"),
    ("ten", "10"),
];

/// Lowercase, punctuation → spaces, `_`/`-` → spaces, number words → digits.
pub fn normalize(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' }).collect();
    cleaned.split_whitespace().map(|w| NUMBERS.iter().find(|(a, _)| *a == w).map(|(_, d)| *d).unwrap_or(w)).collect::<Vec<_>>().join(" ")
}

const FILLER: &[&str] = &[
    "please", "the", "a", "an", "uh", "um", "okay", "ok", "hey", "now", "can", "you", "could", "would", "let", "lets", "s", "just", "and", "then", "engine",
    "stream",
];

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// 0–1 similarity (1 = equal).
pub fn similarity(a: &str, b: &str) -> f32 {
    let n = a.chars().count().max(b.chars().count());
    if n == 0 {
        return 1.0;
    }
    1.0 - levenshtein(a, b) as f32 / n as f32
}

/// Accept fuzzy name matches at or above this similarity (short names get more slack, since
/// one wrong letter is a large fraction of them).
pub const MATCH: f32 = 0.72;
const MATCH_SHORT: f32 = 0.6;
/// The best name must beat the runner-up by this much, else the phrase is ambiguous.
const MARGIN: f32 = 0.08;

fn best<'a>(phrase: &str, names: &'a [(String, Vec<String>)]) -> Option<(&'a str, f32)> {
    let compact = phrase.replace(' ', "");
    let mut scored: Vec<(&str, f32, usize)> = names
        .iter()
        .map(|(name, forms)| {
            let (s, len) = forms
                .iter()
                .map(|f| (similarity(phrase, f).max(similarity(&compact, &f.replace(' ', ""))), f.chars().count()))
                .fold((0.0f32, 0usize), |a, b| if b.0 > a.0 { b } else { a });
            (name.as_str(), s, len)
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    let (name, s, len) = *scored.first()?;
    let need = if len <= 4 { MATCH_SHORT } else { MATCH };
    let runner_up = scored.get(1).map(|x| x.1).unwrap_or(0.0);
    (s >= need && (s - runner_up >= MARGIN || s >= 0.999)).then_some((name, s))
}

fn has_seq(w: &[&str], seq: &[&str]) -> Option<usize> {
    if seq.is_empty() || w.len() < seq.len() {
        return None;
    }
    (0..=w.len() - seq.len()).find(|&i| w[i..i + seq.len()] == *seq)
}

/// Parse a transcript. Returns the intent and a confidence (1.0 for keyword-only commands).
pub fn parse(text: &str, v: &Vocab) -> Option<(Intent, f32)> {
    let norm = normalize(text);
    let all: Vec<&str> = norm.split_whitespace().collect();
    let w: Vec<&str> = all.iter().copied().filter(|x| !FILLER.contains(x)).collect();
    if w.is_empty() {
        return None;
    }
    let rest = |from: usize| w[from..].join(" ");
    let phrase_without = |drop: &[&str]| w.iter().filter(|x| !drop.contains(x)).copied().collect::<Vec<_>>().join(" ");

    // confirmation words (short utterances only)
    if w.len() <= 3 {
        if ["confirm", "confirmed", "yes", "yeah", "yep", "affirmative", "correct"].contains(&w[0]) || has_seq(&w, &["do", "it"]).is_some() {
            return Some((Intent::Confirm, 1.0));
        }
        if ["cancel", "no", "nope", "abort", "negative"].contains(&w[0]) || has_seq(&w, &["never", "mind"]).is_some() {
            return Some((Intent::Cancel, 1.0));
        }
    }
    if w.contains(&"panic") {
        return Some((Intent::Panic, 1.0));
    }
    if w[0] == "clean" || w[0] == "cleanup" || has_seq(&w, &["clear", "chat"]).is_some() || has_seq(&w, &["clear", "effects"]).is_some() {
        return Some((Intent::Clean, 1.0));
    }
    if has_seq(&w, &["next", "song"]).is_some() || has_seq(&w, &["skip", "song"]).is_some() || has_seq(&w, &["skip", "track"]).is_some() {
        return Some((Intent::NextSong, 1.0));
    }
    if w[0] == "marker"
        || w[0] == "mark"
        || has_seq(&w, &["add", "marker"]).is_some()
        || has_seq(&w, &["set", "marker"]).is_some()
        || has_seq(&w, &["drop", "marker"]).is_some()
    {
        let i = w.iter().position(|x| *x == "marker" || *x == "mark").map(|i| i + 1).unwrap_or(w.len());
        let label = rest(i);
        return Some((Intent::Marker { label: (!label.is_empty()).then_some(label) }, 1.0));
    }
    // modes
    if has_seq(&w, &["go", "live"]).is_some() {
        return Some((Intent::Mode { name: "live".into() }, 1.0));
    }
    if has_seq(&w, &["be", "right", "back"]).is_some() || w == ["brb"] || has_seq(&w, &["take", "a", "break"]).is_some() {
        return Some((Intent::Mode { name: "brb".into() }, 1.0));
    }
    if has_seq(&w, &["go", "offline"]).is_some() || has_seq(&w, &["end", "stream"]).is_some() {
        return Some((Intent::Mode { name: "offline".into() }, 1.0));
    }
    if let Some(i) = w.iter().position(|x| *x == "mode") {
        let phrase = if i + 1 < w.len() { rest(i + 1) } else { w[..i].join(" ") };
        return best(&phrase, &v.modes).map(|(n, s)| (Intent::Mode { name: n.into() }, s));
    }
    // take / next / previous
    if w.len() <= 2 && (w[0] == "take" || w[0] == "go" || w == ["cut"]) {
        return Some((Intent::Take, 1.0));
    }
    if w[0] == "next" && (w.len() == 1 || w[1..] == ["scene"] || w[1..] == ["shot"] || w[1..] == ["camera"]) {
        return Some((Intent::Next, 1.0));
    }
    if (w[0] == "previous" || w[0] == "prev" || w[0] == "last" || w[0] == "back") && w.len() <= 2 {
        return Some((Intent::Prev, 1.0));
    }
    // page
    if let Some(i) = w.iter().position(|x| *x == "page") {
        let phrase = if i + 1 < w.len() { rest(i + 1) } else { w[..i].join(" ") };
        return best(&phrase, &v.pages).map(|(n, s)| (Intent::Page { name: n.into() }, s));
    }
    // release a preset
    if ["release", "stop", "end", "kill"].contains(&w[0]) && w.len() > 1 {
        let phrase = phrase_without(&["release", "stop", "end", "kill", "preset", "effect"]);
        return best(&phrase, &v.presets).map(|(n, s)| (Intent::Release { name: n.into() }, s));
    }
    // scenes: "scene X", "X scene", "switch to X", "show X", "camera X", "cut to X"
    let cut = has_seq(&w, &["cut", "to"]).is_some();
    let scene_kw = ["scene", "switch", "show", "camera", "cam", "shot", "to", "cut", "go"];
    if w.iter().any(|x| ["scene", "switch", "show", "camera", "cam", "shot"].contains(x)) || cut {
        let phrase = phrase_without(&scene_kw);
        return best(&phrase, &v.scenes).map(|(n, s)| (Intent::Scene { name: n.into(), cut }, s));
    }
    // presets: "preset X", "fire X", "trigger X", "play X", "hit X"
    let preset_kw = ["preset", "fire", "trigger", "play", "hit", "start", "do"];
    if w.iter().any(|x| preset_kw.contains(x)) {
        let phrase = phrase_without(&preset_kw);
        return best(&phrase, &v.presets).map(|(n, s)| (Intent::Preset { name: n.into() }, s));
    }
    // a bare name: unambiguous preset or scene
    let phrase = w.join(" ");
    let p = best(&phrase, &v.presets);
    let s = best(&phrase, &v.scenes);
    match (p, s) {
        (Some((pn, ps)), Some((_, ss))) if ps > ss + 0.1 => Some((Intent::Preset { name: pn.into() }, ps)),
        (Some((_, ps)), Some((sn, ss))) if ss > ps + 0.1 => Some((Intent::Scene { name: sn.into(), cut: false }, ss)),
        (Some((pn, ps)), None) => Some((Intent::Preset { name: pn.into() }, ps)),
        (None, Some((sn, ss))) => Some((Intent::Scene { name: sn.into(), cut: false }, ss)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab() -> Vocab {
        let n = |xs: &[(&str, Option<&str>)]| xs.iter().map(|(a, l)| (a.to_string(), Vocab::forms(a, *l))).collect();
        Vocab {
            scenes: n(&[("duo", None), ("wide", None), ("kit", None), ("drums", None), ("room", None), ("brb", Some("Be right back"))]),
            presets: n(&[("hype", Some("HYPE")), ("sub_big", Some("SUB!")), ("chorus_blast", None), ("confetti", None), ("blackout", None)]),
            modes: n(&[("offline", None), ("preshow", None), ("live", None), ("brb", None), ("ad_break", None), ("rehearsal", None)]),
            pages: n(&[("show", None), ("mix", None), ("fx", None), ("songs", None)]),
        }
    }

    #[test]
    fn keywords_and_names() {
        let v = vocab();
        let p = |t: &str| parse(t, &v).map(|(i, _)| i);
        assert_eq!(p("Preset hype."), Some(Intent::Preset { name: "hype".into() }));
        assert_eq!(p("fire the chorus blast"), Some(Intent::Preset { name: "chorus_blast".into() }));
        assert_eq!(p("Scene, wide!"), Some(Intent::Scene { name: "wide".into(), cut: false }));
        assert_eq!(p("cut to the kit"), Some(Intent::Scene { name: "kit".into(), cut: true }));
        assert_eq!(p("Take."), Some(Intent::Take));
        assert_eq!(p("PANIC!"), Some(Intent::Panic));
        assert_eq!(p("clean up please"), Some(Intent::Clean));
        assert_eq!(p("next scene"), Some(Intent::Next));
        assert_eq!(p("next song"), Some(Intent::NextSong));
        assert_eq!(p("add marker great fill"), Some(Intent::Marker { label: Some("great fill".into()) }));
        assert_eq!(p("mode rehearsal"), Some(Intent::Mode { name: "rehearsal".into() }));
        assert_eq!(p("let's go live"), Some(Intent::Mode { name: "live".into() }));
        assert_eq!(p("release hype"), Some(Intent::Release { name: "hype".into() }));
        assert_eq!(p("page effects"), None);
        assert_eq!(p("page fx"), Some(Intent::Page { name: "fx".into() }));
        assert_eq!(p("yes"), Some(Intent::Confirm));
        assert_eq!(p("never mind"), Some(Intent::Cancel));
    }

    #[test]
    fn fuzzy_transcription_errors_and_rejections() {
        let v = vocab();
        // typical ASR slips
        assert_eq!(parse("preset hipe", &v).map(|x| x.0), Some(Intent::Preset { name: "hype".into() }));
        assert_eq!(parse("preset sub big", &v).map(|x| x.0), Some(Intent::Preset { name: "sub_big".into() }));
        assert_eq!(parse("scene due", &v).map(|x| x.0), Some(Intent::Scene { name: "duo".into(), cut: false }));
        // unknown names are rejected rather than guessed
        assert_eq!(parse("preset fireworks", &v), None);
        assert_eq!(parse("what time is it", &v), None);
        // bare unambiguous name
        assert_eq!(parse("confetti", &v).map(|x| x.0), Some(Intent::Preset { name: "confetti".into() }));
    }

    #[test]
    fn intents_map_to_commands() {
        assert_eq!(Intent::Preset { name: "hype".into() }.op(), Some(Op::PresetFire { name: "hype".into(), payload: Value::Null }));
        assert_eq!(Intent::Take.describe(), "scene.take ");
        assert!(Intent::Confirm.op().is_none());
        assert_eq!(normalize("Sub_Big-ONE!"), "sub big 1");
    }
}
