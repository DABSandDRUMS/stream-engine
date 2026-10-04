//! In/out selection on sentence boundaries and deterministic ranking (marker score +
//! transcript features), with an optional external ranker (`[clips] rank_command`: JSON in →
//! JSON out, e.g. an LLM script).

use crate::config::{ClipsConfig, RankingConfig};
use crate::transcribe::Word;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A sentence (or pause-delimited phrase) in recording seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sentence {
    pub t0: f64,
    pub t1: f64,
    pub first: usize,
    pub last: usize,
}

const PAUSE: f64 = 0.8;

fn ends_sentence(w: &str) -> bool {
    w.trim_end_matches(['"', '\'', ')', ']']).ends_with(['.', '!', '?'])
}

/// Split spoken words into sentences at terminal punctuation or pauses ≥ 0.8 s.
pub fn sentences(words: &[Word]) -> Vec<Sentence> {
    let spoken: Vec<usize> = (0..words.len()).filter(|i| !words[*i].annotation).collect();
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (k, &i) in spoken.iter().enumerate() {
        let s = *start.get_or_insert(i);
        let next = spoken.get(k + 1).map(|j| &words[*j]);
        let pause = next.is_none_or(|n| n.t0 - words[i].t1 >= PAUSE);
        if ends_sentence(&words[i].text) || pause {
            out.push(Sentence { t0: words[s].t0, t1: words[i].t1, first: s, last: i });
            start = None;
        }
    }
    out
}

/// Chosen in/out (recording seconds).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Trim {
    pub t_in: f64,
    pub t_out: f64,
    pub clean_in: bool,
    pub clean_out: bool,
}

/// Is `t` inside a spoken word?
fn inside_word(words: &[Word], t: f64) -> Option<&Word> {
    words.iter().find(|w| !w.annotation && w.t0 < t && t < w.t1)
}

/// Pick in/out near the marker window on sentence boundaries, within `[lo, hi]` (the
/// recording's bounds) and the configured clip length.
pub fn trim(words: &[Word], start: f64, peak: f64, end: f64, lo: f64, hi: f64, cfg: &ClipsConfig) -> Trim {
    let slack = cfg.boundary_slack.0 as f64 / 1000.0;
    let min_len = cfg.min_len.0 as f64 / 1000.0;
    let max_len = cfg.max_len.0 as f64 / 1000.0;
    let sents = sentences(words);
    let prev_end = |i: usize| (0..i).rev().find(|j| !words[*j].annotation).map(|j| words[j].t1);
    let next_start = |i: usize| (i + 1..words.len()).find(|j| !words[*j].annotation).map(|j| words[j].t0);

    // in: a sentence start near the window start, before the peak
    let latest_in = (peak - 2.0).max(start);
    let in_pick = sents
        .iter()
        .filter(|s| s.t0 >= start - slack && s.t0 <= latest_in && s.t0 >= lo)
        .min_by(|a, b| (a.t0 - start).abs().total_cmp(&(b.t0 - start).abs()).then(a.t0.total_cmp(&b.t0)));
    let (mut t_in, clean_in) = match in_pick {
        Some(s) => {
            let lead = prev_end(s.first).map(|p| (s.t0 - p) / 2.0).unwrap_or(0.25).min(0.25);
            (s.t0 - lead, true)
        }
        None => match inside_word(words, start) {
            // never start mid-word
            Some(w) => (w.t0 - 0.1, false),
            None => (start, false),
        },
    };
    // out: a sentence end near the window end, after the peak
    let earliest_out = (peak + 2.0).min(end);
    let out_pick = sents
        .iter()
        .filter(|s| s.t1 >= earliest_out && s.t1 <= end + slack && s.t1 <= hi)
        .min_by(|a, b| (a.t1 - end).abs().total_cmp(&(b.t1 - end).abs()).then(a.t1.total_cmp(&b.t1)));
    let (mut t_out, clean_out) = match out_pick {
        Some(s) => {
            let tail = next_start(s.last).map(|n| (n - s.t1) / 2.0).unwrap_or(0.4).min(0.4);
            (s.t1 + tail, true)
        }
        None => match inside_word(words, end) {
            Some(w) => (w.t1 + 0.1, false),
            None => (end, false),
        },
    };
    t_in = t_in.max(lo);
    t_out = t_out.min(hi);
    // length limits: extend to the next sentence end, or trim from the start
    if t_out - t_in < min_len {
        let want = t_in + min_len;
        t_out = sents.iter().map(|s| s.t1 + 0.3).find(|t| *t >= want && *t <= hi).unwrap_or(want).min(hi);
        if t_out - t_in < min_len {
            t_in = (t_out - min_len).max(lo);
        }
    }
    if t_out - t_in > max_len {
        let earliest = t_out - max_len;
        t_in = sents.iter().map(|s| s.t0 - 0.2).find(|t| *t >= earliest && *t <= peak).unwrap_or(earliest);
    }
    Trim { t_in, t_out, clean_in, clean_out }
}

/// Musical clips use time and (when present) nearby downbeats/beat points, not
/// Whisper's unreliable song lyrics or spoken-sentence boundaries.
pub fn trim_music(start: f64, peak: f64, end: f64, lo: f64, hi: f64, beats: &[f64], cfg: &ClipsConfig) -> Trim {
    let min = cfg.min_len.0 as f64 / 1000.0;
    let max = cfg.max_len.0 as f64 / 1000.0;
    let lo = lo.max(0.0);
    let hi = hi.max(lo);
    let target = (end - start).clamp(min, max).min(45.0_f64.max(min)).min(hi - lo);
    let t_in = (peak - target / 2.0).max(start).clamp(lo, (hi - target).max(lo));
    let t_out = (t_in + target).min(hi);
    let snap =
        |t: f64| beats.iter().copied().filter(|b| b.is_finite() && (*b - t).abs() <= 0.4).min_by(|a, b| (a - t).abs().total_cmp(&(b - t).abs())).unwrap_or(t);
    let (a, b) = (snap(t_in), snap(t_out));
    let (t_in, t_out) = if a >= lo && b <= hi && b - a >= min && b - a <= max && a <= peak && b >= peak { (a, b) } else { (t_in, t_out) };
    Trim { t_in, t_out, clean_in: false, clean_out: false }
}

const EXCITED: &[&str] =
    &["let's go", "lets go", "no way", "oh my god", "oh my gosh", "holy", "what", "insane", "crazy", "wow", "yes", "haha", "lol", "hahaha"];

/// Deterministic transcript features → ranking score.
pub fn rank_score(marker_score: f64, words: &[Word], trim: &Trim, peak: f64, cfg: &RankingConfig) -> f64 {
    let near = words.iter().filter(|w| !w.annotation && w.t0 >= peak - 5.0 && w.t1 <= peak + 5.0).count() as f64;
    let speech = (near / 10.0).min(4.0);
    let inside: Vec<&Word> = words.iter().filter(|w| w.t0 >= trim.t_in && w.t1 <= trim.t_out).collect();
    let text = inside.iter().filter(|w| !w.annotation).map(|w| w.text.to_lowercase()).collect::<Vec<_>>().join(" ");
    let mut excite = inside.iter().filter(|w| w.text.contains('!')).count();
    excite += inside.iter().filter(|w| w.annotation && (w.text.to_lowercase().contains("laugh") || w.text.to_lowercase().contains("cheer"))).count();
    for p in EXCITED.iter().copied().chain(cfg.keywords.iter().map(String::as_str)) {
        let p = p.to_lowercase();
        if p.is_empty() {
            continue;
        }
        excite += text.match_indices(&p).filter(|(i, _)| *i == 0 || !text.as_bytes()[i - 1].is_ascii_alphanumeric()).count();
    }
    let len = trim.t_out - trim.t_in;
    let clean = (trim.clean_in as u8 + trim.clean_out as u8) as f64 / 2.0;
    marker_score * cfg.marker + cfg.speech * speech + cfg.excitement * (excite.min(8) as f64) - cfg.length_penalty * (len - 45.0).max(0.0)
        + cfg.clean_edges * clean
}

/// What the external ranker sees for each candidate.
#[derive(Clone, Debug, Serialize)]
pub struct RankIn {
    pub key: String,
    pub score: f64,
    pub marker_score: f64,
    pub reasons: Vec<String>,
    pub labels: Vec<String>,
    pub kind: String,
    pub song: Option<String>,
    pub requester: Option<String>,
    pub dmca_risk: bool,
    pub context: serde_json::Value,
    #[serde(rename = "in")]
    pub t_in: f64,
    pub out: f64,
    pub peak: f64,
    /// Transcript bounds: in/out suggestions must stay inside.
    pub transcript_from: f64,
    pub transcript_to: f64,
    pub transcript: String,
    pub words: Vec<Word>,
}

/// What it may answer, per candidate (all optional).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct RankOut {
    pub key: String,
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(default, rename = "in")]
    pub t_in: Option<f64>,
    #[serde(default)]
    pub out: Option<f64>,
    #[serde(default)]
    pub title: Option<String>,
    /// Leave this candidate out.
    #[serde(default)]
    pub drop: bool,
}

#[derive(Deserialize)]
struct RankReply {
    clips: Vec<RankOut>,
}

/// Run `[clips] rank_command` (stdin: `{session, min_len, max_len, candidates}`, stdout:
/// `{clips: [{key, score?, in?, out?, title?, drop?}]}`).
pub fn external_rank(argv: &[String], session: &str, cands: &[RankIn], cfg: &ClipsConfig) -> Result<Vec<RankOut>, String> {
    let (prog, args) = argv.split_first().ok_or("rank_command is empty")?;
    let input = serde_json::json!({
        "session": session,
        "min_len": cfg.min_len.0 as f64 / 1000.0,
        "max_len": cfg.max_len.0 as f64 / 1000.0,
        "candidates": cands,
    });
    let reply = run_json(prog, args, &input, Duration::from_millis(cfg.rank_timeout.0))?;
    let r: RankReply = serde_json::from_slice(&reply).map_err(|e| format!("rank_command reply: {e}"))?;
    Ok(r.clips)
}

/// Apply external answers, keeping deterministic values wherever an answer is invalid.
pub fn apply_rank(key: &str, trim: &mut Trim, score: &mut f64, title: &mut Option<String>, answers: &[RankOut], bounds: (f64, f64), cfg: &ClipsConfig) -> bool {
    let Some(a) = answers.iter().find(|a| a.key == key) else { return true };
    if a.drop {
        return false;
    }
    if let Some(s) = a.score.filter(|s| s.is_finite()) {
        *score = s;
    }
    let (min_len, max_len) = (cfg.min_len.0 as f64 / 1000.0, cfg.max_len.0 as f64 / 1000.0);
    let t_in = a.t_in.unwrap_or(trim.t_in);
    let t_out = a.out.unwrap_or(trim.t_out);
    if t_in >= bounds.0 && t_out <= bounds.1 && t_out - t_in >= min_len && t_out - t_in <= max_len {
        trim.t_in = t_in;
        trim.t_out = t_out;
    } else if a.t_in.is_some() || a.out.is_some() {
        tracing::warn!("rank_command: ignoring in/out {t_in:.2}–{t_out:.2} for {key} (outside the transcript or length limits)");
    }
    if let Some(t) = &a.title {
        *title = Some(t.chars().take(140).collect());
    }
    true
}

/// Run a hook: JSON on stdin, JSON on stdout, killed after `timeout`.
pub fn run_json(prog: &str, args: &[String], input: &serde_json::Value, timeout: Duration) -> Result<Vec<u8>, String> {
    let mut child = Command::new(expand_home(prog))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{prog}: {e}"))?;
    let body = serde_json::to_vec(input).map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().expect("piped");
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&body);
    });
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut v);
        v
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    let halt = crate::live::current();
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{prog}: timed out after {} s", timeout.as_secs()));
            }
            // ranking and uploads are post-show work: they stop when the show goes live
            None if halt.as_ref().is_some_and(crate::live::Halt::stop_now) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(halt.as_ref().map(crate::live::Halt::error).unwrap_or_default());
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let _ = writer.join();
    let out = reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    if !status.success() {
        return Err(format!("{prog}: exit {status}: {}", err.trim()));
    }
    Ok(out)
}

fn expand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(h)) => format!("{h}/{rest}"),
        _ => p.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_core::config::Dur;

    fn w(t0: f64, t1: f64, s: &str) -> Word {
        Word { t0, t1, text: s.into(), p: 0.9, annotation: false }
    }

    /// Sentences every ~4 s: "Sentence n starts here now." with 0.3 s gaps.
    fn speech(from: f64, to: f64) -> Vec<Word> {
        let mut out = Vec::new();
        let mut t = from;
        let mut n = 0;
        while t < to {
            for (k, s) in ["Sentence", "starts", "here", "now."].iter().enumerate() {
                let text = if k == 0 { format!("{s}{n}") } else { s.to_string() };
                out.push(w(t, t + 0.7, &text));
                t += 0.9;
            }
            t += 0.3;
            n += 1;
        }
        out
    }

    #[test]
    fn music_trim_keeps_drop_and_snaps_only_when_safe() {
        let cfg = ClipsConfig::default();
        let plain = trim_music(10.0, 30.0, 50.0, 0.0, 120.0, &[], &cfg);
        assert_eq!((plain.t_in, plain.t_out), (10.0, 50.0));
        let snapped = trim_music(10.0, 30.0, 50.0, 0.0, 120.0, &[9.9, 50.2], &cfg);
        assert_eq!((snapped.t_in, snapped.t_out), (9.9, 50.2));
        let unsafe_snap = trim_music(10.0, 30.0, 50.0, 10.0, 50.0, &[9.9, 50.2], &cfg);
        assert_eq!((unsafe_snap.t_in, unsafe_snap.t_out), (10.0, 50.0));
    }

    #[test]
    fn sentences_split_on_punctuation_and_pauses() {
        let words = vec![w(0.0, 0.3, "Hey"), w(0.4, 0.8, "there."), w(1.0, 1.3, "so"), w(1.3, 1.6, "um"), w(3.0, 3.5, "anyway")];
        let s = sentences(&words);
        assert_eq!(s.len(), 3);
        assert_eq!((s[0].first, s[0].last), (0, 1));
        assert_eq!((s[1].t0, s[1].t1), (1.0, 1.6));
        assert_eq!(s[2].first, 4);
    }

    #[test]
    fn trim_snaps_to_sentence_boundaries_near_the_window() {
        let cfg = ClipsConfig::default();
        let words = speech(0.0, 120.0);
        let t = trim(&words, 40.0, 55.0, 62.0, 0.0, 180.0, &cfg);
        assert!(t.clean_in && t.clean_out);
        let starts: Vec<f64> = sentences(&words).iter().map(|s| s.t0).collect();
        let ends: Vec<f64> = sentences(&words).iter().map(|s| s.t1).collect();
        // in = a sentence start (minus a short lead) close to 40 s; out = a sentence end close to 62 s
        let s_in = starts.iter().copied().min_by(|a, b| (a - t.t_in).abs().total_cmp(&(b - t.t_in).abs())).unwrap();
        assert!(t.t_in < s_in && s_in - t.t_in <= 0.25 + 1e-9 && (s_in - 40.0).abs() <= 2.5, "in {} vs {s_in}", t.t_in);
        let e_out = ends.iter().copied().min_by(|a, b| (a - t.t_out).abs().total_cmp(&(b - t.t_out).abs())).unwrap();
        assert!(t.t_out > e_out && t.t_out - e_out <= 0.4 + 1e-9 && (e_out - 62.0).abs() <= 2.5, "out {} vs {e_out}", t.t_out);
        // never mid-word
        assert!(inside_word(&words, t.t_in).is_none() && inside_word(&words, t.t_out).is_none());
    }

    #[test]
    fn trim_without_speech_respects_bounds_and_lengths() {
        let cfg = ClipsConfig { min_len: Dur(10_000), max_len: Dur(20_000), ..ClipsConfig::default() };
        let t = trim(&[], 5.0, 8.0, 9.0, 0.0, 100.0, &cfg);
        assert_eq!((t.t_in, t.t_out, t.clean_in), (5.0, 15.0, false));
        let t = trim(&[], 10.0, 40.0, 45.0, 0.0, 100.0, &cfg);
        assert_eq!((t.t_in, t.t_out), (25.0, 45.0));
        // window at the end of the recording: extend backwards
        let t = trim(&[], 95.0, 97.0, 99.0, 0.0, 100.0, &cfg);
        assert_eq!((t.t_in, t.t_out), (90.0, 100.0));
        // a sentence starting just before the window start is taken whole (with a lead-in)
        let words = vec![w(4.5, 5.5, "hello")];
        assert_eq!(trim(&words, 5.0, 8.0, 20.0, 0.0, 100.0, &cfg).t_in, 4.25);
        // a word straddling the start outside the search slack is still never cut
        let words = vec![w(0.2, 0.4, "earlier."), w(4.5, 5.5, "hello")];
        let cfg0 = ClipsConfig { boundary_slack: Dur(0), ..cfg.clone() };
        assert_eq!(trim(&words, 5.0, 8.0, 20.0, 0.0, 100.0, &cfg0).t_in, 4.4);
    }

    #[test]
    fn ranking_prefers_score_speech_and_excitement() {
        let cfg = RankingConfig::default();
        let tr = Trim { t_in: 0.0, t_out: 30.0, clean_in: true, clean_out: true };
        let quiet = rank_score(1.0, &[], &tr, 15.0, &cfg);
        let words = vec![w(12.0, 12.5, "No"), w(12.5, 13.0, "way!"), w(13.0, 13.5, "Let's"), w(13.5, 14.0, "go!")];
        let loud = rank_score(1.0, &words, &tr, 15.0, &cfg);
        assert!(loud > quiet + 0.5, "{loud} vs {quiet}");
        let long = Trim { t_out: 60.0, ..tr };
        assert!(rank_score(1.0, &[], &long, 15.0, &cfg) < quiet);
        assert!(rank_score(2.0, &[], &tr, 15.0, &cfg) > quiet);
    }

    #[test]
    fn external_ranker_round_trip_and_validation() {
        let d = tempfile::tempdir().unwrap();
        let script = d.path().join("rank.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\ncat > \"$(dirname \"$0\")/in.json\"\necho '{\"clips\":[{\"key\":\"a\",\"score\":9.5,\"in\":11,\"out\":30,\"title\":\"Big moment\"},{\"key\":\"b\",\"drop\":true},{\"key\":\"c\",\"in\":0,\"out\":500}]}'\n",
        )
        .unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
        let cfg = ClipsConfig { rank_command: vec![script.to_string_lossy().into()], ..ClipsConfig::default() };
        let cand = RankIn {
            key: "a".into(),
            score: 1.0,
            marker_score: 1.0,
            reasons: vec!["chat".into()],
            kind: "talk".into(),
            song: None,
            requester: None,
            dmca_risk: false,
            context: serde_json::Value::Null,
            labels: vec![],
            t_in: 10.0,
            out: 31.0,
            peak: 20.0,
            transcript_from: 0.0,
            transcript_to: 60.0,
            transcript: "hi".into(),
            words: vec![w(1.0, 2.0, "hi")],
        };
        let ans = external_rank(&cfg.rank_command, "s1", std::slice::from_ref(&cand), &cfg).unwrap();
        let sent: serde_json::Value = serde_json::from_slice(&std::fs::read(d.path().join("in.json")).unwrap()).unwrap();
        assert_eq!(sent["candidates"][0]["in"], 10.0);
        assert_eq!(sent["session"], "s1");
        let (mut tr, mut score, mut title) = (Trim { t_in: 10.0, t_out: 31.0, clean_in: true, clean_out: true }, 1.0, None);
        assert!(apply_rank("a", &mut tr, &mut score, &mut title, &ans, (0.0, 60.0), &cfg));
        assert_eq!((tr.t_in, tr.t_out, score, title.as_deref()), (11.0, 30.0, 9.5, Some("Big moment")));
        assert!(!apply_rank("b", &mut tr, &mut score, &mut title, &ans, (0.0, 60.0), &cfg));
        let mut tr2 = Trim { t_in: 10.0, t_out: 31.0, clean_in: true, clean_out: true };
        assert!(apply_rank("c", &mut tr2, &mut score, &mut title, &ans, (0.0, 60.0), &cfg));
        assert_eq!((tr2.t_in, tr2.t_out), (10.0, 31.0), "out-of-bounds answer ignored");
        // a failing hook reports its stderr
        std::fs::write(&script, "#!/bin/sh\necho nope >&2\nexit 3\n").unwrap();
        assert!(external_rank(&cfg.rank_command, "s1", &[cand], &cfg).unwrap_err().contains("nope"));
    }
}
