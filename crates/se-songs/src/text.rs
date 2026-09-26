//! Text helpers: folding for cache keys and blocklists, chat-safe display, durations, ids.

use se_proto::Value;

/// Fullwidth ASCII variants (U+FF01–U+FF5E) and the ideographic space map to plain ASCII.
fn fullwidth(c: char) -> char {
    let u = c as u32;
    if (0xFF01..=0xFF5E).contains(&u) {
        char::from_u32(u - 0xFEE0).unwrap_or(c)
    } else if c == '\u{3000}' {
        ' '
    } else {
        c
    }
}

/// Combining marks (zalgo, stacked diacritics).
fn is_combining(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

/// Latin lookalikes from Cyrillic/Greek used to dodge blocklists.
fn confusable(c: char) -> char {
    match c {
        'а' | 'α' => 'a',
        'в' | 'β' => 'b',
        'с' | 'ϲ' => 'c',
        'ԁ' => 'd',
        'е' | 'ε' => 'e',
        'ɡ' => 'g',
        'һ' => 'h',
        'і' | 'ι' | 'ı' => 'i',
        'ј' => 'j',
        'κ' | 'к' => 'k',
        'м' => 'm',
        'η' | 'п' => 'n',
        'о' | 'ο' | '0' => 'o',
        'р' | 'ρ' => 'p',
        'ѕ' => 's',
        'τ' | 'т' => 't',
        'υ' => 'u',
        'ν' => 'v',
        'ш' | 'ω' => 'w',
        'х' | 'χ' => 'x',
        'у' | 'γ' => 'y',
        // `1`, `l`, `I` and `|` are interchangeable in evasions; fold them all to `i`.
        '1' | 'l' | '|' => 'i',
        '3' => 'e',
        '4' => 'a',
        '5' => 's',
        '7' => 't',
        _ => c,
    }
}

fn fold_with(s: &str, strict: bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true;
    for c in s.chars() {
        let c = fullwidth(c);
        if is_combining(c) || c == '\'' || c == '’' || c == '\u{200B}' || c == '\u{200D}' || c == '\u{FEFF}' {
            continue;
        }
        let c = if strict { confusable(c) } else { c };
        if c.is_alphanumeric() {
            for l in c.to_lowercase() {
                out.push(if strict { confusable(l) } else { l });
            }
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Case/punctuation/width-insensitive form used for query cache keys and library search.
/// Keeps every script intact (a Cyrillic song title stays Cyrillic).
pub fn fold(s: &str) -> String {
    fold_with(s, false)
}

/// Aggressive form for blocklist matching: also maps homoglyphs and leetspeak to Latin.
pub fn fold_strict(s: &str) -> String {
    fold_with(s, true)
}

/// Maximum characters of a text query we look up (longer input is truncated).
pub const MAX_QUERY_CHARS: usize = 120;

/// Cache key for a text request.
pub fn query_key(s: &str) -> String {
    let f = fold(s);
    match f.char_indices().nth(MAX_QUERY_CHARS) {
        Some((i, _)) => f[..i].trim_end().to_string(),
        None => f,
    }
}

/// Whole-word phrase match on folded strings.
pub fn contains_phrase(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let (h, n) = (format!(" {hay} "), format!(" {needle} "));
    h.contains(&n)
}

/// Chat/UI-safe single line: control characters dropped, whitespace collapsed, capped at
/// `max` characters with an ellipsis.
pub fn display(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max * 4));
    let mut n = 0;
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if c.is_control() {
            continue;
        }
        if is_combining(c) {
            continue;
        }
        if space {
            // a space plus at least one more character must fit
            if n + 2 > max {
                if n >= max {
                    out.pop();
                }
                out.push('…');
                return out;
            }
            out.push(' ');
            n += 1;
            space = false;
        }
        if n >= max {
            // more text follows: the last kept character becomes the ellipsis
            out.pop();
            out.push('…');
            return out;
        }
        out.push(c);
        n += 1;
    }
    out
}

/// `m:ss` or `h:mm:ss`.
pub fn fmt_duration(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Entry ids arrive as `12`, `"12"`, or `"#12"`.
pub fn parse_id(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        Value::Float(f) if f.fract() == 0.0 => Some(*f as i64),
        Value::Str(s) => s.trim().trim_start_matches('#').parse().ok(),
        _ => None,
    }
}

/// Lowercased login without a leading `@`.
pub fn login(s: &str) -> String {
    s.trim().trim_start_matches('@').to_lowercase()
}

/// Fraction of query tokens present in the candidate (both folded), weighted toward longer
/// tokens so "the" matters less than "bohemian".
pub fn match_score(query: &str, cand: &str) -> f32 {
    let cand_tokens: Vec<&str> = cand.split(' ').collect();
    let (mut hit, mut total) = (0usize, 0usize);
    for q in query.split(' ').filter(|t| !t.is_empty()) {
        let w = q.chars().count().min(8);
        total += w;
        if cand_tokens.contains(&q) {
            hit += w;
        }
    }
    if total == 0 { 0.0 } else { hit as f32 / total as f32 }
}

/// Unix time in milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_normalizes_case_width_and_marks() {
        assert_eq!(fold("  Bohemian   RHAPSODY!! (Official) "), "bohemian rhapsody official");
        assert_eq!(fold("Ｑｕｅｅｎ"), "queen");
        assert_eq!(fold("Z̷a̶l̵g̴o̷"), "zalgo");
        assert_eq!(fold("Don't Stop Me Now"), "dont stop me now");
        assert_eq!(fold("Кино — Группа крови"), "кино группа крови");
    }

    #[test]
    fn strict_fold_defeats_homoglyphs() {
        // Cyrillic а/е/о and leetspeak
        let plain = fold_strict("Nickelback");
        assert_eq!(fold_strict("Nіckеlbаck"), plain);
        assert_eq!(fold_strict("n1ckelb4ck"), plain);
        assert_eq!(fold_strict("NICKEIBACK"), plain);
        assert!(contains_phrase(&fold_strict("Best of NICKELBАCK live"), &fold_strict("nickelback")));
    }

    #[test]
    fn phrase_match_is_whole_word() {
        assert!(contains_phrase("rick astley never gonna", "never gonna"));
        assert!(!contains_phrase("classical music", "ass"));
        assert!(!contains_phrase("anything", ""));
    }

    #[test]
    fn display_caps_and_strips() {
        assert_eq!(display("a\u{0007}b\n\n c", 50), "ab c");
        assert_eq!(display("abcdefghij", 5), "abcd…");
        assert_eq!(display("abc", 3), "abc");
    }

    #[test]
    fn durations_and_ids() {
        assert_eq!(fmt_duration(214), "3:34");
        assert_eq!(fmt_duration(3723), "1:02:03");
        assert_eq!(parse_id(&Value::Str("#12".into())), Some(12));
        assert_eq!(parse_id(&Value::Int(7)), Some(7));
        assert_eq!(parse_id(&Value::Str("x".into())), None);
        assert_eq!(login("@DrumFan42"), "drumfan42");
    }

    #[test]
    fn query_key_truncates() {
        let long = "word ".repeat(100);
        assert!(query_key(&long).chars().count() <= MAX_QUERY_CHARS);
        assert!(!query_key(&long).ends_with(' '));
    }

    #[test]
    fn score_prefers_full_matches() {
        let c = fold("Queen - Bohemian Rhapsody (Official Video Remastered)");
        assert!(match_score(&fold("bohemian rhapsody"), &c) > 0.99);
        assert!(match_score(&fold("bohemian like you"), &c) < 0.7);
    }
}
