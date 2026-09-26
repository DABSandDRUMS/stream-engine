//! Chat-text safety (§19): everything the bot sends goes through [`sanitize_chat`].
//!
//! * control characters and bidi overrides are dropped, line breaks fold into spaces;
//! * runs of whitespace collapse;
//! * the result is capped (Twitch rejects messages over 500 characters);
//! * a leading `/`, `.`, `\`, or `!` gets a zero-width space in front, so a reply can never be
//!   read as a chat command — by Twitch, by other bots, or by this bot seeing its own echo.

/// Twitch's hard limit for one chat message (characters).
pub const TWITCH_MAX: usize = 500;

/// Invisible prefix that defuses command-looking replies.
pub const ZWSP: char = '\u{200B}';

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}')
}

/// Clean text for one outgoing chat message, capped at `max_chars` (≤ [`TWITCH_MAX`]).
pub fn sanitize_chat(s: &str, max_chars: usize) -> String {
    let max_chars = max_chars.clamp(1, TWITCH_MAX);
    let mut out = String::with_capacity(s.len().min(max_chars * 4));
    let mut last_space = true;
    for c in s.chars() {
        let c = if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c };
        if (c.is_control() && c != ' ') || is_bidi_control(c) {
            continue;
        }
        if c.is_whitespace() {
            if last_space {
                continue;
            }
            last_space = true;
            out.push(' ');
        } else {
            last_space = false;
            out.push(c);
        }
    }
    let trimmed = out.trim_end();
    let needs_guard = trimmed.starts_with(['/', '.', '\\', '!']);
    let budget = if needs_guard { max_chars - 1 } else { max_chars };
    let mut t = truncate_chars(trimmed, budget);
    if needs_guard && !t.is_empty() {
        t.insert(0, ZWSP);
    }
    t
}

/// Cap `s` at `max` characters, ending with `…` when cut.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    match s.char_indices().nth(max) {
        None => s.to_string(),
        Some(_) => {
            let cut = s.char_indices().nth(max - 1).map(|(i, _)| i).unwrap_or(s.len());
            let mut t = s[..cut].trim_end().to_string();
            t.push('…');
            t
        }
    }
}

/// Text as it would look after [`sanitize_chat`], for comparing a chat line against what the
/// bot sent (echo detection).
pub fn echo_key(s: &str) -> String {
    s.trim_start_matches(ZWSP).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A command/counter name usable as one address segment: lowercase `[a-z0-9_-]`.
pub fn segment(s: &str) -> String {
    s.chars()
        .filter_map(|c| {
            let c = c.to_ascii_lowercase();
            (c.is_ascii_alphanumeric() || c == '_' || c == '-').then_some(c)
        })
        .take(48)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_controls_and_folds_lines() {
        assert_eq!(sanitize_chat("hi\nthere\t\u{7}you\r\n", 500), "hi there you");
        assert_eq!(sanitize_chat("a\u{202E}evil\u{2066}b", 500), "aevilb");
        assert_eq!(sanitize_chat("   lots    of   space  ", 500), "lots of space");
    }

    #[test]
    fn caps_length_on_char_boundaries() {
        let s = "é".repeat(600);
        let out = sanitize_chat(&s, 500);
        assert_eq!(out.chars().count(), 500);
        assert!(out.ends_with('…'));
        assert_eq!(sanitize_chat("abcdef", 4), "abc…");
        assert_eq!(sanitize_chat("abcd", 4), "abcd");
    }

    #[test]
    fn defuses_command_prefixes() {
        for s in ["/ban someone", ".timeout x 10", "!addcom !x y", "\\me hi"] {
            let out = sanitize_chat(s, 500);
            assert!(out.starts_with(ZWSP), "{s}");
            assert_eq!(out.trim_start_matches(ZWSP), s);
        }
        assert_eq!(sanitize_chat("hello !discord", 500), "hello !discord");
        // the guard counts toward the cap
        assert_eq!(sanitize_chat("!abcdef", 5).chars().count(), 5);
    }

    #[test]
    fn echo_key_ignores_guard_and_spacing() {
        assert_eq!(echo_key(&sanitize_chat("!sr  is open", 500)), "!sr is open");
    }

    #[test]
    fn segments() {
        assert_eq!(segment("!Deaths"), "deaths");
        assert_eq!(segment("so-rry_2 x"), "so-rry_2x");
    }
}
