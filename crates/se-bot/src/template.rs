//! Reply templates (§14.3): `{user}`, `{count}`, `{uptime}`, `{song}`, `{args}`, `{1}`…,
//! `{touser}`, any state address (`{goals.subs.current}`), and `{random:a|b|c}`.
//!
//! One left-to-right pass: substituted values are never scanned again, so text typed by a
//! viewer can't smuggle in placeholders. Unknown names are left as written so mistakes show.

use se_core::rng::Rng;

/// Longest text one placeholder may insert.
pub const MAX_VALUE_CHARS: usize = 400;

fn is_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

/// Render `tpl`. `lookup` resolves a placeholder name; `None` keeps `{name}` literally.
pub fn render(tpl: &str, lookup: &mut dyn FnMut(&str) -> Option<String>, rng: &mut Rng) -> String {
    let mut out = String::with_capacity(tpl.len() + 16);
    let mut rest = tpl;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = &after[..end];
        if let Some(choices) = inner.strip_prefix("random:") {
            let opts: Vec<&str> = choices.split('|').collect();
            let pick = opts[rng.below(opts.len() as u64) as usize];
            out.push_str(pick);
            rest = &after[end + 1..];
        } else if is_name(inner) {
            match lookup(inner) {
                Some(v) => {
                    out.extend(v.chars().filter(|c| !c.is_control()).take(MAX_VALUE_CHARS));
                }
                None => {
                    out.push('{');
                    out.push_str(inner);
                    out.push('}');
                }
            }
            rest = &after[end + 1..];
        } else {
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Placeholder names used by a template (for validation and "does this reply echo user text?").
pub fn names(tpl: &str) -> Vec<&str> {
    let mut v = Vec::new();
    let mut rest = tpl;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        let inner = &after[..end];
        if is_name(inner) {
            v.push(inner);
            rest = &after[end + 1..];
        } else if inner.starts_with("random:") {
            rest = &after[end + 1..];
        } else {
            rest = after;
        }
    }
    v
}

/// Human uptime: `2h 5m`, `42m`, `35s`.
pub fn format_duration(secs: i64) -> String {
    let s = secs.max(0);
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{sec}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(tpl: &str, vars: &[(&str, &str)]) -> String {
        let mut rng = Rng::new(7);
        render(tpl, &mut |n| vars.iter().find(|(k, _)| *k == n).map(|(_, v)| v.to_string()), &mut rng)
    }

    #[test]
    fn substitutes_known_names() {
        assert_eq!(r("Hi {user}, you said {args}", &[("user", "ana"), ("args", "hello there")]), "Hi ana, you said hello there");
        assert_eq!(r("{goals.subs.current}/{goals.subs.target}", &[("goals.subs.current", "3"), ("goals.subs.target", "10")]), "3/10");
    }

    #[test]
    fn unknown_names_stay_literal_and_braces_survive() {
        assert_eq!(r("{nope} {user}", &[("user", "x")]), "{nope} x");
        assert_eq!(r("a { b } {", &[]), "a { b } {");
        assert_eq!(r("json {\"k\": 1}", &[]), "json {\"k\": 1}");
    }

    #[test]
    fn user_text_is_never_rescanned() {
        let out = r("echo: {args}", &[("args", "{user} {random:a|b} {args}"), ("user", "SHOULD_NOT_APPEAR")]);
        assert_eq!(out, "echo: {user} {random:a|b} {args}");
    }

    #[test]
    fn random_picks_one_option() {
        for seed in 0..50 {
            let mut rng = Rng::new(seed);
            let out = render("{random:rock|paper|scissors}!", &mut |_| None, &mut rng);
            assert!(["rock!", "paper!", "scissors!"].contains(&out.as_str()), "{out}");
        }
        let mut seen = std::collections::HashSet::new();
        let mut rng = Rng::new(1);
        for _ in 0..200 {
            seen.insert(render("{random:a|b|c}", &mut |_| None, &mut rng));
        }
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn values_are_capped_and_control_free() {
        let long = "x".repeat(1000);
        assert_eq!(r("{args}", &[("args", &long)]).len(), MAX_VALUE_CHARS);
        assert_eq!(r("{args}", &[("args", "a\u{0}b\nc")]), "abc");
    }

    #[test]
    fn lists_names() {
        assert_eq!(names("{user} {random:a|b} {1} {x y}"), vec!["user", "1"]);
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(35), "35s");
        assert_eq!(format_duration(42 * 60 + 5), "42m");
        assert_eq!(format_duration(2 * 3600 + 5 * 60 + 9), "2h 5m");
    }
}
