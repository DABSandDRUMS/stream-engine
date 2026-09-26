//! Text → Kokoro phoneme tokens.
//!
//! `espeak-ng` (GPL-3.0) runs as a **separate process** and is never linked. Text reaches it
//! only on stdin (never argv), without `[`/`]` (its `[[…]]` raw-phoneme syntax) and without
//! clause punctuation: like phonemizer's `preserve_punctuation`, the text is split at
//! punctuation, the word runs are phonemized, and the marks are re-inserted (espeak drops them,
//! Kokoro's vocabulary keeps `;:,.!?—…"()“”` for prosody). The IPA is then mapped to the
//! phoneme set Kokoro v1.0 was trained on (misaki's espeak fallback: `a^ɪ`→`I`, `r`→`ɹ`,
//! `x`→`k`, `ɬ`→`l`, flap `ɾ`→`T`, …), filtered to the vocabulary, and chunked at clause
//! boundaries to ≤ 510 tokens.

use crate::text::is_mark;
use crate::vocab;
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Longest espeak run we wait for before killing it.
const ESPEAK_TIMEOUT: Duration = Duration::from_secs(10);
/// Parallel espeak processes in the per-line fallback.
const FALLBACK_PARALLEL: usize = 8;

/// The `espeak-ng` executable.
#[derive(Clone, Debug)]
pub struct Espeak {
    program: OsString,
}

impl Default for Espeak {
    fn default() -> Self {
        Espeak { program: "espeak-ng".into() }
    }
}

impl Espeak {
    pub fn with_program(program: impl Into<OsString>) -> Self {
        Espeak { program: program.into() }
    }

    /// `espeak-ng --version` → e.g. `1.52.0`; errors when it isn't installed.
    pub fn version(&self) -> Result<String> {
        let out = Command::new(&self.program)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("cannot run {} (install espeak-ng)", self.program.to_string_lossy()))?;
        let s = String::from_utf8_lossy(&out.stdout);
        // "eSpeak NG text-to-speech: 1.52.0  Data at: /usr/share/espeak-ng-data"
        let v = s.split(':').nth(1).and_then(|r| r.split_whitespace().next()).unwrap_or("").to_string();
        if !out.status.success() || v.is_empty() {
            bail!("unexpected `espeak-ng --version` output: {}", s.trim());
        }
        Ok(v)
    }

    /// IPA (with `^` ties) for each input line, one string per line.
    fn ipa_lines(&self, lang: &str, lines: &[&str]) -> Result<Vec<String>> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        // espeak reads stdin line by line and prints one IPA line per clause. The lines carry no
        // clause punctuation, so normally that is one output line per input line; very long
        // lines are split by espeak itself, in which case each line is phonemized on its own.
        let out = self.run(lang, &lines.join("\n"))?;
        if out.len() == lines.len() {
            return Ok(out);
        }
        let mut res = vec![String::new(); lines.len()];
        for (batch_lines, batch_res) in lines.chunks(FALLBACK_PARALLEL).zip(res.chunks_mut(FALLBACK_PARALLEL)) {
            std::thread::scope(|s| -> Result<()> {
                let handles: Vec<_> = batch_lines.iter().map(|l| s.spawn(move || self.run(lang, l))).collect();
                for (h, slot) in handles.into_iter().zip(batch_res.iter_mut()) {
                    let parts = h.join().map_err(|_| anyhow::anyhow!("espeak worker panicked"))??;
                    *slot = parts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ");
                }
                Ok(())
            })?;
        }
        Ok(res)
    }

    fn run(&self, lang: &str, input: &str) -> Result<Vec<String>> {
        let mut child = Command::new(&self.program)
            .args(["-q", "-b", "1", "--ipa", "--tie=^", "-v", lang])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("cannot run {} (install espeak-ng)", self.program.to_string_lossy()))?;
        let mut stdin = child.stdin.take().context("espeak stdin")?;
        let mut stdout = child.stdout.take().context("espeak stdout")?;
        let mut stderr = child.stderr.take().context("espeak stderr")?;
        let (out, err, status) = std::thread::scope(|s| {
            let w = s.spawn(move || {
                let r = stdin.write_all(input.as_bytes()).and_then(|_| stdin.write_all(b"\n"));
                drop(stdin);
                r
            });
            let r_out = s.spawn(move || {
                let mut b = Vec::new();
                stdout.read_to_end(&mut b).map(|_| b)
            });
            let r_err = s.spawn(move || {
                let mut b = Vec::new();
                let _ = stderr.read_to_end(&mut b);
                b
            });
            let deadline = Instant::now() + ESPEAK_TIMEOUT;
            let status = loop {
                match child.try_wait() {
                    Ok(Some(st)) => break Ok(st),
                    Ok(None) if Instant::now() >= deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Err(anyhow::anyhow!("espeak-ng timed out"));
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                    Err(e) => break Err(e.into()),
                }
            };
            let _ = w.join();
            let out = r_out.join().unwrap_or_else(|_| Ok(Vec::new()));
            let err = r_err.join().unwrap_or_default();
            (out, err, status)
        });
        let status = status?;
        let out = out.context("reading espeak output")?;
        if !status.success() {
            bail!("espeak-ng -v {lang} failed: {}", String::from_utf8_lossy(&err).trim());
        }
        Ok(String::from_utf8_lossy(&out).lines().map(str::to_string).collect())
    }
}

/// Kokoro-style `espeak-ng` voice name check (`en-us`, `en-gb-x-rp`, `pt-br`, `cmn`).
pub fn valid_lang(lang: &str) -> bool {
    let mut parts = lang.split('-');
    let first = parts.next().unwrap_or("");
    (2..=3).contains(&first.len())
        && first.chars().all(|c| c.is_ascii_lowercase())
        && parts.all(|p| (1..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// espeak language for a Kokoro voice by its prefix (`af_heart` → `en-us`, `bf_emma` → `en-gb`).
pub fn lang_for_voice(voice: &str) -> Option<&'static str> {
    Some(match voice.chars().next()? {
        'a' => "en-us",
        'b' => "en-gb",
        'e' => "es",
        'f' => "fr-fr",
        'h' => "hi",
        'i' => "it",
        'j' => "ja",
        'p' => "pt-br",
        'z' => "cmn",
        _ => return None,
    })
}

#[derive(Debug, PartialEq)]
enum Part {
    Words(String),
    /// Punctuation run with its surrounding whitespace collapsed to single spaces.
    Mark(String),
}

/// A decimal point, digit-group comma, or clock colon between digits is part of the number.
fn splits_at(chars: &[char], i: usize) -> bool {
    let c = chars[i];
    if !is_mark(c) {
        return false;
    }
    let digit_before = i > 0 && chars[i - 1].is_ascii_digit();
    let digit_after = chars.get(i + 1).is_some_and(|d| d.is_ascii_digit());
    !(matches!(c, '.' | ',' | ':') && digit_before && digit_after)
}

fn split_marks(text: &str) -> Vec<Part> {
    let chars: Vec<char> = text.chars().collect();
    let mut parts = Vec::new();
    let mut words = String::new();
    let mut i = 0;
    while i < chars.len() {
        if !splits_at(&chars, i) {
            words.push(chars[i]);
            i += 1;
            continue;
        }
        let lead_space = words.ends_with(char::is_whitespace);
        let w = words.trim();
        if !w.is_empty() {
            parts.push(Part::Words(w.to_string()));
        }
        words.clear();
        let mut mark = String::new();
        if lead_space && !parts.is_empty() {
            mark.push(' ');
        }
        while i < chars.len() && (chars[i].is_whitespace() || splits_at(&chars, i)) {
            let c = chars[i];
            if c.is_whitespace() {
                if !mark.ends_with(' ') {
                    mark.push(' ');
                }
            } else {
                mark.push(c);
            }
            i += 1;
        }
        parts.push(Part::Mark(mark));
    }
    let w = words.trim();
    if !w.is_empty() {
        parts.push(Part::Words(w.to_string()));
    }
    parts
}

/// Remove espeak language-switch flags such as `(en)` / `(fr)` from an IPA line (the input
/// carried no parentheses, so every parenthesized run in the output is a flag).
fn strip_lang_flags(ipa: &str) -> String {
    let mut out = String::with_capacity(ipa.len());
    let mut rest = ipa;
    while let Some(open) = rest.find('(') {
        out.push_str(&rest[..open]);
        match rest[open..].find(')') {
            Some(close) => rest = &rest[open + close + 1..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Phonemize normalized text: the Kokoro phoneme string (every char in the vocabulary).
pub fn phonemize(espeak: &Espeak, text: &str, lang: &str) -> Result<String> {
    if !valid_lang(lang) {
        bail!("invalid espeak language `{lang}`");
    }
    let parts = split_marks(text);
    let lines: Vec<&str> = parts
        .iter()
        .filter_map(|p| match p {
            Part::Words(w) => Some(w.as_str()),
            Part::Mark(_) => None,
        })
        .collect();
    let ipa = espeak.ipa_lines(lang, &lines)?;
    let mut ipa = ipa.into_iter();
    let mut joined = String::new();
    for p in &parts {
        match p {
            Part::Words(_) => {
                let line = ipa.next().unwrap_or_default();
                joined.push_str(strip_lang_flags(&line).trim());
            }
            Part::Mark(m) => joined.push_str(m),
        }
    }
    Ok(finish(&joined, lang))
}

/// Map raw espeak IPA (with `^` ties) + marks to the Kokoro phoneme string.
fn finish(raw: &str, lang: &str) -> String {
    let lang = lang.to_ascii_lowercase();
    let ps = if lang == "en" || lang.starts_with("en-") { english(raw, !lang.starts_with("en-us")) } else { other(raw) };
    let ps = kokoro_fixups(&ps);
    let mut out = String::with_capacity(ps.len());
    for c in ps.chars().filter(|c| vocab::contains(*c)) {
        if c == ' ' && (out.is_empty() || out.ends_with(' ')) {
            continue;
        }
        out.push(c);
    }
    out.trim_end().to_string()
}

/// misaki `EspeakFallback` (the G2P Kokoro v1.0's English voices were trained with), longest
/// keys first.
const EN_E2M: &[(&str, &str)] = &[
    ("ʔˌn\u{329}", "ʔn"),
    ("ʔn\u{329}", "ʔn"),
    ("a^ɪ", "I"),
    ("a^ʊ", "W"),
    ("d^ʒ", "ʤ"),
    ("e^ɪ", "A"),
    ("t^ʃ", "ʧ"),
    ("ɔ^ɪ", "Y"),
    ("ə^l", "ᵊl"),
    ("ʲo", "jo"),
    ("ʲə", "jə"),
    ("e", "A"),
    ("ʲ", ""),
    ("ɚ", "əɹ"),
    ("r", "ɹ"),
    ("x", "k"),
    ("ç", "k"),
    ("ɐ", "ə"),
    ("ɬ", "l"),
    ("\u{303}", ""),
];

fn english(raw: &str, british: bool) -> String {
    let mut ps = raw.to_string();
    if british {
        // misaki applies this after `e`→`A`, where it can no longer match; its British lexicon
        // (the training data) uses `ɛː` for this diphthong.
        ps = ps.replace("e^ə", "ɛː");
    }
    for (from, to) in EN_E2M {
        ps = ps.replace(from, to);
    }
    ps = syllabic(&ps);
    if british {
        ps = ps.replace("iə", "ɪə").replace("ə^ʊ", "Q");
    } else {
        ps = ps.replace("o^ʊ", "O").replace("ɜːɹ", "ɜɹ").replace("ɜː", "ɜɹ").replace("ɪə", "iə").replace('ː', "");
    }
    ps = ps.replace('o', "ɔ");
    // Kokoro v1.0 (misaki < 2.0): flap T and glottal stop
    ps = ps.replace('ɾ', "T").replace('ʔ', "t");
    ps.replace('^', "")
}

/// Syllabic-consonant mark U+0329 after `X` becomes `ᵊX`.
fn syllabic(ps: &str) -> String {
    let chars: Vec<char> = ps.chars().collect();
    let mut out = String::with_capacity(ps.len());
    let mut i = 0;
    while i < chars.len() {
        if i + 1 < chars.len() && chars[i + 1] == '\u{329}' && !chars[i].is_whitespace() {
            out.push('ᵊ');
            out.push(chars[i]);
            i += 2;
            continue;
        }
        if chars[i] != '\u{329}' {
            out.push(chars[i]);
        }
        i += 1;
    }
    out
}

/// misaki `EspeakG2P` for espeak languages other than English (keys in its sorted order).
const OTHER_E2M: &[(&str, &str)] =
    &[("a^ɪ", "I"), ("a^ʊ", "W"), ("d^z", "ʣ"), ("d^ʒ", "ʤ"), ("e^ɪ", "A"), ("o^ʊ", "O"), ("s^s", "S"), ("t^s", "ʦ"), ("t^ʃ", "ʧ"), ("ə^ʊ", "Q"), ("ɔ^ɪ", "Y")];

fn other(raw: &str) -> String {
    let mut ps = raw.to_string();
    for (from, to) in OTHER_E2M {
        ps = ps.replace(from, to);
    }
    ps.replace(['^', '-'], "")
}

/// Kokoro reference `phonemize` fixes for espeak word splitting: "…n hundred" gets a space
/// ("wˈʌnhˈʌndɹɪd" → "wˈʌn hˈʌndɹɪd"); a detached plural/possessive " z" joins its word.
fn kokoro_fixups(ps: &str) -> String {
    const HUNDRED: &str = "hˈʌndɹɪd";
    let mut s = String::with_capacity(ps.len() + 8);
    let mut rest = ps;
    while let Some(i) = rest.find(HUNDRED) {
        let before = rest[..i].chars().next_back().or_else(|| s.chars().next_back());
        s.push_str(&rest[..i]);
        if before.is_some_and(|c| c.is_ascii_alphabetic() || c == 'ɹ' || c == 'ː') {
            s.push(' ');
        }
        s.push_str(HUNDRED);
        rest = &rest[i + HUNDRED.len()..];
    }
    s.push_str(rest);
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == ' ' && chars.get(i + 1) == Some(&'z') && chars.get(i + 2).is_none_or(|n| *n == ' ' || is_mark(*n)) && i > 0 {
            continue;
        }
        out.push(c);
    }
    out
}

/// Token ids for a phoneme string (characters outside the vocabulary are skipped).
pub fn tokens(ps: &str) -> Vec<i64> {
    ps.chars().filter_map(vocab::id).collect()
}

/// Split a phoneme string into pieces of at most `max` tokens, preferring (latest first)
/// sentence ends `!.?…`, then `:;`, then `,—`, then a word gap; closing quotes/parens stay with
/// their clause. Pieces without any phoneme (only marks/stress) are dropped.
pub fn chunk(ps: &str, max: usize) -> Vec<String> {
    let chars: Vec<char> = ps.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        while start < chars.len() && chars[start] == ' ' {
            start += 1;
        }
        let rest = &chars[start..];
        if rest.is_empty() {
            break;
        }
        let cut = if rest.len() <= max { rest.len() } else { cut_point(rest, max) };
        let piece: String = rest[..cut].iter().collect::<String>().trim().to_string();
        if piece.chars().any(|c| !is_mark(c) && !matches!(c, ' ' | 'ˈ' | 'ˌ' | 'ː')) {
            out.push(piece);
        }
        start += cut;
    }
    out
}

fn cut_point(rest: &[char], max: usize) -> usize {
    let min = max / 5;
    for group in ["!.?…", ":;", ",—"] {
        if let Some(i) = (min..max).rev().find(|&i| group.contains(rest[i])) {
            let mut end = i + 1;
            while end < max && matches!(rest[end], ')' | '”' | '"') {
                end += 1;
            }
            return end;
        }
    }
    match (min..max).rev().find(|&i| rest[i] == ' ') {
        Some(i) => i,
        None => max,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_keeps_marks_and_numbers() {
        let p = split_marks("Thanks for 2.5 bits, let's go!");
        assert_eq!(p, vec![Part::Words("Thanks for 2.5 bits".into()), Part::Mark(", ".into()), Part::Words("let's go".into()), Part::Mark("!".into())]);
        let p = split_marks("“Hi” (you) … 10:30, 1,000");
        assert_eq!(
            p,
            vec![
                Part::Mark("“".into()),
                Part::Words("Hi".into()),
                Part::Mark("” (".into()),
                Part::Words("you".into()),
                Part::Mark(") … ".into()),
                Part::Words("10:30".into()),
                Part::Mark(", ".into()),
                Part::Words("1,000".into()),
            ]
        );
    }

    #[test]
    fn lang_names() {
        for ok in ["en-us", "en-gb-x-rp", "pt-br", "cmn", "es"] {
            assert!(valid_lang(ok), "{ok}");
        }
        for bad in ["", "-v", "--help", "en us", "EN", "e", "en-", "en-toolongsubtag", "en/../x"] {
            assert!(!valid_lang(bad), "{bad}");
        }
        assert_eq!(lang_for_voice("af_heart"), Some("en-us"));
        assert_eq!(lang_for_voice("bm_george"), Some("en-gb"));
        assert_eq!(lang_for_voice("custom"), None);
    }

    #[test]
    fn misaki_mapping() {
        // American: diphthongs to misaki letters, length marks dropped, flap → T
        assert_eq!(finish("fˈa^ɪv hˈʌndɹɪd bˈɪts, lˈɛts ɡˈo^ʊ!", "en-us"), "fˈIv hˈʌndɹɪd bˈɪts, lˈɛts ɡˈO!");
        assert_eq!(finish("wˈɔːɾɚ t^ʃˈiːz d^ʒˈʌmp", "en-us"), "wˈɔTəɹ ʧˈiz ʤˈʌmp");
        // British keeps length, ə^ʊ → Q, e^ə → ɛː
        assert_eq!(finish("ɡˌə^ʊ hˈe^ə nˈa^ɪnti", "en-gb"), "ɡˌQ hˈɛː nˈInti");
        // r/x/ɬ/ʲ fixups and syllabic n
        assert_eq!(finish("rxɬʲa bˈʌʔn\u{329}", "en-us"), "ɹkla bˈʌtn");
        assert_eq!(finish("bˈɒtl\u{329}", "en-gb"), "bˈɒtᵊl");
        // hundred spacing and detached plural z
        assert_eq!(finish("wˈʌnhˈʌndɹɪd nˈa^ɪnti z.", "en-us"), "wˈʌn hˈʌndɹɪd nˈIntiz.");
        // non-English: ties mapped/removed, hyphens dropped, vocabulary filter
        assert_eq!(finish("t^ʃˈaw pɾa-ʁa ¿ok?", "es"), "ʧˈaw pɾaʁa ok?");
    }

    #[test]
    fn language_flags_removed() {
        assert_eq!(strip_lang_flags("(en)hˈɛlo(es) ˈola"), "hˈɛlo ˈola");
        assert_eq!(strip_lang_flags("abc (x"), "abc ");
    }

    #[test]
    fn chunking_respects_limit_and_clauses() {
        let clause = "ðɪs ɪz ə vˈɛɹi lˈɔŋ sˈɛntəns";
        let text = [clause; 40].join(", ") + ".";
        assert!(text.chars().count() > 1000);
        let chunks = chunk(&text, vocab::MAX_TOKENS);
        assert!(chunks.len() >= 3);
        for c in &chunks {
            assert!(c.chars().count() <= vocab::MAX_TOKENS, "chunk of {}", c.chars().count());
            assert!(c.ends_with(',') || c.ends_with('.'), "cut at a clause mark: …{}", &c[c.len().saturating_sub(12)..]);
            assert!(!c.starts_with(' '));
        }
        let rejoined: String = chunks.join(" ");
        assert_eq!(rejoined, text);
        // sentence ends win over later commas
        let s = format!("{}. {}, {}", "a".repeat(300), "b".repeat(150), "c".repeat(200));
        let c = chunk(&s, vocab::MAX_TOKENS);
        assert_eq!(c[0], format!("{}.", "a".repeat(300)));
        // no punctuation: cut at a word gap; no gap at all: hard cut
        let words = ["wˈɜɹd"; 200].join(" ");
        assert!(chunk(&words, vocab::MAX_TOKENS).iter().all(|c| c.chars().count() <= vocab::MAX_TOKENS && !c.ends_with(' ')));
        let solid = "a".repeat(1200);
        assert_eq!(chunk(&solid, vocab::MAX_TOKENS).iter().map(|c| c.chars().count()).collect::<Vec<_>>(), vec![510, 510, 180]);
        // punctuation-only pieces are dropped
        assert!(chunk(" … , ", vocab::MAX_TOKENS).is_empty());
        // closing quote stays with its sentence
        let q = format!("“{}.” {}", "a".repeat(400), "b".repeat(300));
        assert!(chunk(&q, vocab::MAX_TOKENS)[0].ends_with(".”"));
    }

    #[test]
    fn tokens_map_vocab() {
        assert_eq!(tokens("hˈI!"), vec![50, 156, 25, 5]);
        assert_eq!(tokens("a^b"), vec![43, 44]);
    }
}
