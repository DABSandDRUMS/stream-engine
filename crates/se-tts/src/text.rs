//! Chat-text normalization before phonemization.
//!
//! Chat text is data: nothing in it is interpreted (no SSML, no espeak phoneme input), it is
//! only cleaned so the voice says something sensible and bounded: Unicode folding, invisible
//! and zalgo characters, emoji, URLs/e-mails, spammy repetition, money/decade/time forms, and a
//! length cap.

use regex::{Captures, Regex};
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;

/// Normalize `text` for speech and cap it at `max_chars` characters (0 = no cap).
pub fn normalize(text: &str, max_chars: usize) -> String {
    let s = fold_chars(text);
    let s = strip_links(&s);
    let s = map_symbols(&s);
    let s = expand_forms(&s);
    let s = collapse_repeats(&s);
    let s = collapse_words(&s);
    let s = squash_spaces(&s);
    cap(&s, max_chars)
}

/// NFKC-fold and map characters: typographic quotes/dashes/brackets to the forms the
/// phonemizer understands, drop invisible/control/zalgo marks and pictographs. Symbols that
/// URLs contain (`&`, `=`, `~`, …) are mapped later, after links are removed whole.
fn fold_chars(text: &str) -> String {
    let mut s = String::with_capacity(text.len());
    for c in text.nfkc() {
        match c {
            '\u{2018}' | '\u{2019}' | '\u{201B}' | '\u{2032}' | '`' | '\u{B4}' => s.push('\''),
            '\u{201E}' | '\u{201F}' | '\u{2033}' => s.push('"'),
            '«' => s.push('“'),
            '»' => s.push('”'),
            // `[[…]]` is espeak's raw phoneme input syntax: brackets must never reach it.
            '[' | '{' => s.push('('),
            ']' | '}' => s.push(')'),
            '\u{2012}' | '\u{2013}' | '\u{2015}' | '\u{2E3A}' | '\u{2E3B}' => s.push('—'),
            '、' => s.push_str(", "),
            '。' => s.push_str(". "),
            c if is_invisible(c) || is_zalgo_mark(c) => {}
            c if c.is_control() || c.is_whitespace() => s.push(' '),
            c if is_pictograph(c) => s.push(' '),
            c => s.push(c),
        }
    }
    s
}

fn is_invisible(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x034F | 0x061C | 0x115F | 0x1160 | 0x17B4 | 0x17B5 | 0x180B..=0x180F
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0x3164 | 0xFE00..=0xFE0F
        | 0xFEFF | 0xFFA0 | 0xFFF0..=0xFFF8 | 0xE0000..=0xE0FFF)
}

/// Combining diacritics left over after NFKC composition (letters with accents compose into
/// precomposed characters, so what remains is stacked "zalgo" noise). Indic vowel signs live
/// in their own blocks and are kept.
fn is_zalgo_mark(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F | 0x0483..=0x0489 | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

fn is_pictograph(c: char) -> bool {
    matches!(c as u32,
        0x2190..=0x21FF | 0x2300..=0x23FF | 0x2460..=0x24FF | 0x2500..=0x27BF | 0x2900..=0x297F
        | 0x2B00..=0x2BFF | 0x3030 | 0x303D | 0x3297 | 0x3299 | 0xE000..=0xF8FF
        | 0x1F000..=0x1FAFF | 0xF0000..=0x10FFFF)
}

static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:[a-z][a-z0-9+.-]*://|www\.)\S+").expect("url regex"));
static EMAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\S+@\S+\.[A-Za-z]{2,}\S*").expect("email regex"));
static BARE_DOMAIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b[a-z0-9][a-z0-9-]*(?:\.[a-z0-9-]+)*\.(?:com|net|org|tv|gg|io|co|me|ly|be|us|uk|de|fr|ca|au|app|dev|xyz|info|biz|live|link|shop|site|online|store|club|ru|cn|in|to|eu|nl|es|it|pl|br|jp|fm)\b(?:/\S*)?",
    )
    .expect("domain regex")
});
static MENTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"@(\w)").expect("mention regex"));
static HASH_NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"#(\d)").expect("hash regex"));
static HASHTAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"#(\w)").expect("hashtag regex"));

/// Links are never read out (spam, doxxing, unreadable); `@name` and `#tag` lose their sigil.
fn strip_links(s: &str) -> String {
    let s = URL.replace_all(s, " ");
    let s = EMAIL.replace_all(&s, " ");
    let s = BARE_DOMAIN.replace_all(&s, " ");
    let s = MENTION.replace_all(&s, "$1");
    let s = HASH_NUM.replace_all(&s, "number $1");
    HASHTAG.replace_all(&s, "$1").into_owned()
}

/// `&` is read as "and"; markup-ish symbols espeak would name aloud ("asterisk hugs
/// asterisk") become spaces.
fn map_symbols(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str(" and "),
            '*' | '_' | '~' | '^' | '|' | '\\' | '<' | '>' | '=' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

static EG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\be\.g\.").expect("eg regex"));
static IE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bi\.e\.").expect("ie regex"));
static ABBREV: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(mr|mrs|ms|dr|st|jr|sr|vs|etc|prof|approx)\.").expect("abbrev regex"));
static INITIALISM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(?:[A-Za-z]\.){2,}").expect("initialism regex"));
static THOUSANDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d),(\d{3})\b").expect("thousands regex"));
static MONEY_PRE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([$£€¥])\s?(\d+)(?:\.(\d{1,2}))?\b").expect("money regex"));
static MONEY_POST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(\d+)(?:\.(\d{1,2}))?\s?([$£€¥])").expect("money regex"));
static DECADE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(1[1-9]|20)([1-9]0)s\b").expect("decade regex"));
static CLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(\d{1,2}):([0-5]\d)\b").expect("clock regex"));
static SPACED_DASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s-+\s").expect("dash regex"));

/// Written forms espeak reads badly: abbreviations whose period would end a clause, money
/// ("$5.50" is otherwise "dollar five point five zero"), digit-group commas, decades, clock times.
fn expand_forms(s: &str) -> String {
    let s = EG.replace_all(s, "for example");
    let s = IE.replace_all(&s, "that is");
    let s = ABBREV.replace_all(&s, "$1");
    let s = INITIALISM.replace_all(&s, |c: &Captures| c[0].replace('.', ""));
    let mut s = s.into_owned();
    loop {
        let next = THOUSANDS.replace_all(&s, "$1$2").into_owned();
        if next == s {
            break;
        }
        s = next;
    }
    let s = MONEY_PRE.replace_all(&s, |c: &Captures| money(&c[1], &c[2], c.get(3).map(|m| m.as_str())));
    let s = MONEY_POST.replace_all(&s, |c: &Captures| money(&c[3], &c[1], c.get(2).map(|m| m.as_str())));
    let s = DECADE.replace_all(&s, "$1 ${2}s");
    let s = clock(&s);
    SPACED_DASH.replace_all(&s, " — ").into_owned()
}

fn money(symbol: &str, whole: &str, frac: Option<&str>) -> String {
    let (unit, units, sub, subs) = match symbol {
        "£" => ("pound", "pounds", "penny", "pence"),
        "€" => ("euro", "euros", "cent", "cents"),
        "¥" => ("yen", "yen", "sen", "sen"),
        _ => ("dollar", "dollars", "cent", "cents"),
    };
    let n: u64 = whole.parse().unwrap_or(0);
    let mut out = format!("{whole} {}", if n == 1 { unit } else { units });
    if let Some(f) = frac {
        // "5.5" means fifty cents
        let cents: u64 = if f.len() == 1 { f.parse::<u64>().unwrap_or(0) * 10 } else { f.parse().unwrap_or(0) };
        if cents > 0 {
            out.push_str(&format!(" and {cents} {}", if cents == 1 { sub } else { subs }));
        }
    }
    format!(" {out} ")
}

/// `7:00` → "7 o'clock", `10:05` → "10 oh 5", `10:30` → "10 30" (not ratios like `1:2:3`).
fn clock(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for c in CLOCK.captures_iter(s) {
        let m = c.get(0).expect("whole match");
        let before = s[..m.start()].chars().next_back();
        let after = s[m.end()..].chars().next();
        let hour: u32 = c[1].parse().unwrap_or(99);
        if before == Some(':') || after == Some(':') || hour > 23 {
            continue;
        }
        let minute: u32 = c[2].parse().unwrap_or(0);
        out.push_str(&s[last..m.start()]);
        match minute {
            0 => out.push_str(&format!("{hour} o'clock")),
            1..=9 => out.push_str(&format!("{hour} oh {minute}")),
            _ => out.push_str(&format!("{hour} {minute}")),
        }
        last = m.end();
    }
    out.push_str(&s[last..]);
    out
}

/// Clause punctuation kept for prosody (Kokoro's vocabulary has these symbols).
pub fn is_mark(c: char) -> bool {
    matches!(c, ';' | ':' | ',' | '.' | '!' | '?' | '¡' | '¿' | '—' | '…' | '"' | '“' | '”' | '(' | ')')
}

/// Letters repeated 3+ times keep two ("sooooo" → "soo"); runs of clause punctuation keep one
/// mark (".." → "…", "?!?!" → "?"); runs of any other repeated symbol are dropped; digits are
/// left alone (numbers).
fn collapse_repeats(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if is_mark(c) && !matches!(c, '"' | '“' | '”' | '(' | ')') {
            let mut j = i;
            while j < chars.len() && is_mark(chars[j]) && !matches!(chars[j], '"' | '“' | '”' | '(' | ')') {
                j += 1;
            }
            let run = &chars[i..j];
            if run.len() >= 2 && run.iter().all(|&x| x == '.') {
                out.push('…');
            } else {
                out.push(c);
            }
            i = j;
            continue;
        }
        let mut j = i + 1;
        while j < chars.len() && chars[j] == c {
            j += 1;
        }
        let n = j - i;
        if c.is_alphabetic() {
            (0..n.min(2)).for_each(|_| out.push(c));
        } else if c.is_numeric() || c.is_whitespace() || n == 1 {
            (0..n).for_each(|_| out.push(c));
        } else if n >= 2 {
            out.push(' ');
        }
        i = j;
    }
    out
}

/// The same word more than three times in a row is spam ("LUL LUL LUL LUL" → three).
fn collapse_words(s: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut run = 0usize;
    for w in s.split_whitespace() {
        if out.last().is_some_and(|p| p.eq_ignore_ascii_case(w)) {
            run += 1;
        } else {
            run = 1;
        }
        if run <= 3 {
            out.push(w);
        }
    }
    out.join(" ")
}

fn squash_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for w in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

/// Cut at a word boundary when one exists in the last 40 % of the budget.
fn cap(s: &str, max_chars: usize) -> String {
    if max_chars == 0 || s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut = s.char_indices().nth(max_chars).map(|(i, _)| i).unwrap_or(s.len());
    let head = &s[..cut];
    if s[cut..].starts_with(char::is_whitespace) {
        return head.trim_end().to_string();
    }
    let min_keep = s.char_indices().nth(max_chars * 3 / 5).map(|(i, _)| i).unwrap_or(0);
    let end = match head.rfind(char::is_whitespace) {
        Some(i) if i >= min_keep => i,
        _ => cut,
    };
    head[..end].trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_and_sigils() {
        assert_eq!(normalize("check https://evil.example/x?y=1 now", 0), "check now");
        assert_eq!(normalize("go to www.foo.bar/baz or twitch.tv/dabs! or x.com.", 0), "go to or or .");
        assert_eq!(normalize("https://x.com/?a=1&b=2 tom&jerry", 0), "tom and jerry");
        assert_eq!(normalize("mail me@host.com pls", 0), "mail pls");
        assert_eq!(normalize("hi @drumfan #1 fan #hype", 0), "hi drumfan number 1 fan hype");
        assert_eq!(normalize("e.g. this, i.e. that", 0), "for example this, that is that");
    }

    #[test]
    fn money_numbers_times() {
        assert_eq!(normalize("$5.50 for 1,000,000 bits", 0), "5 dollars and 50 cents for 1000000 bits");
        assert_eq!(normalize("£1 and 3€ and $2.5", 0), "1 pound and 3 euros and 2 dollars and 50 cents");
        assert_eq!(normalize("the 1990s at 7:00 or 10:05, ratio 1:2:3", 0), "the 19 90s at 7 o'clock or 10 oh 5, ratio 1:2:3");
        assert_eq!(normalize("version 2.5 by Mr. Smith", 0), "version 2.5 by Mr Smith");
    }

    #[test]
    fn repetition_and_symbols() {
        assert_eq!(normalize("sooooo goooood!!!!!", 0), "soo good!");
        assert_eq!(normalize("wait.... what?!?! ok", 0), "wait… what? ok");
        assert_eq!(normalize("LUL LUL lul LUL LUL Kappa", 0), "LUL LUL lul Kappa");
        assert_eq!(normalize("*hugs* ===== ~~yay~~ 1000000", 0), "hugs yay 1000000");
        assert_eq!(normalize("a - b -- c", 0), "a — b — c");
    }

    #[test]
    fn unicode_cleanup() {
        // math-bold letters fold to ASCII; zero-width joiners, emoji and zalgo marks vanish
        assert_eq!(normalize("𝐡𝐞𝐥𝐥𝐨 w\u{200D}orld 😀🎉 z\u{0335}\u{0321}a\u{0336}lgo", 0), "hello world zalgo");
        // brackets never survive: `[[…]]` would be espeak phoneme input
        assert_eq!(normalize("“quoted” «guillemets» ‘tick’ [[h@'loU]] {x}", 0), "“quoted” “guillemets” 'tick' h@'loU (x)");
        assert_eq!(normalize("café naïve", 0), "café naïve");
        assert_eq!(normalize("line\nbreak\ttab\u{7}bell", 0), "line break tab bell");
    }

    #[test]
    fn caps_length_at_word_boundary() {
        let s = normalize(&"one two three four ".repeat(20), 22);
        assert_eq!(s, "one two three four one");
        assert_eq!(normalize("abcdefghijklmnopqrstuvwxyz", 10), "abcdefghij");
        assert_eq!(normalize("short", 10), "short");
        assert_eq!(normalize(&"éàü".repeat(20), 7), "éàüéàüé", "counts characters, not bytes");
    }
}
