//! Dot-path addresses and wildcard patterns.
//!
//! Pattern segments: `*` matches exactly one segment, `**` matches zero or more segments,
//! and a segment may contain `*` as a glob within the segment (`cam_*`).

/// True when `addr` matches `pattern`.
pub fn matches(pattern: &str, addr: &str) -> bool {
    if pattern == addr || pattern == "**" {
        return true;
    }
    if !pattern.contains('*') {
        return false;
    }
    match_segs(pattern.split('.'), addr.split('.'))
}

fn match_segs(mut p: std::str::Split<'_, char>, mut a: std::str::Split<'_, char>) -> bool {
    match p.next() {
        None => a.next().is_none(),
        Some("**") if p.clone().next().is_none() => true,
        Some("**") => {
            // Try zero or more segments without materializing either path.
            loop {
                if match_segs(p.clone(), a.clone()) {
                    return true;
                }
                if a.next().is_none() {
                    return false;
                }
            }
        }
        Some(ps) => a.next().is_some_and(|segment| glob_seg(ps, segment) && match_segs(p, a)),
    }
}

/// Glob within one segment (`*` = any run of characters).
pub fn glob_seg(p: &str, s: &str) -> bool {
    if p == "*" {
        return true;
    }
    if !p.contains('*') {
        return p == s;
    }
    let mut parts = p.split('*').enumerate().peekable();
    let mut rest = s;
    while let Some((i, part)) = parts.next() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if parts.peek().is_none() {
            return rest.ends_with(part);
        } else if let Some(pos) = rest.find(part) {
            rest = &rest[pos + part.len()..];
        } else {
            return false;
        }
    }
    true
}

pub fn is_pattern(s: &str) -> bool {
    s.contains('*')
}

/// Valid address: non-empty dot-separated segments of `[A-Za-z0-9_-]`, plus `*` in patterns.
pub fn is_valid(addr: &str, allow_wildcards: bool) -> bool {
    !addr.is_empty()
        && addr.split('.').all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || (allow_wildcards && c == '*')))
}

/// Parent of an address (`a.b.c` → `a.b`).
pub fn parent(addr: &str) -> Option<&str> {
    addr.rfind('.').map(|i| &addr[..i])
}

pub fn last(addr: &str) -> &str {
    addr.rsplit('.').next().unwrap_or(addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(matches("scene.*.node.cam_face.offset_y", "scene.duo.node.cam_face.offset_y"));
        assert!(!matches("scene.*.node.cam_face.offset_y", "scene.duo.x.node.cam_face.offset_y"));
        assert!(matches("twitch.**", "twitch.sub"));
        assert!(matches("twitch.**", "twitch.sub.gift"));
        assert!(matches("**", "anything.at.all"));
        assert!(matches("cam_*.fx", "cam_face.fx"));
        assert!(!matches("cam_*.fx", "mic.fx"));
        assert!(matches("a.**.z", "a.z"));
        assert!(matches("a.**.z", "a.b.c.z"));
        assert!(!matches("twitch.sub", "twitch.subgift"));
    }

    #[test]
    fn iterator_matching_preserves_segment_and_glob_boundaries() {
        // Independent materialized-path oracle, including empty segments and Unicode.
        fn glob(p: &str, s: &str) -> bool {
            if p == "*" { return true; }
            if !p.contains('*') { return p == s; }
            let parts: Vec<_> = p.split('*').collect();
            let mut rest = s;
            for (i, part) in parts.iter().enumerate() {
                if i == 0 {
                    let Some(r) = rest.strip_prefix(part) else { return false; };
                    rest = r;
                } else if i == parts.len() - 1 {
                    return rest.ends_with(part);
                } else if let Some(pos) = rest.find(part) {
                    rest = &rest[pos + part.len()..];
                } else {
                    return false;
                }
            }
            true
        }
        fn segments(p: &[&str], a: &[&str]) -> bool {
            match (p.first(), a.first()) {
                (None, None) => true,
                (Some(&"**"), _) => (0..=a.len()).any(|k| segments(&p[1..], &a[k..])),
                (Some(ps), Some(segment)) => glob(ps, segment) && segments(&p[1..], &a[1..]),
                _ => false,
            }
        }
        let mut patterns = vec!["**.a.**.b".to_string(), "a**b*c".to_string(), "*é*".to_string()];
        let mut addresses = vec!["a.b.a.b".to_string(), "aabbbc".to_string(), "préféré".to_string()];
        for p in ["", "a", "b", "*", "**", "a*", "*b", "a*b", "***"] {
            patterns.push(p.to_string());
            for q in ["", "a", "*", "**", "b*"] {
                patterns.push(format!("{p}.{q}"));
                patterns.push(format!("{p}.{q}.**"));
            }
        }
        for a in ["", "a", "b", "ab", "é", "*"] {
            addresses.push(a.to_string());
            for b in ["", "a", "b", "ab", "é"] {
                addresses.push(format!("{a}.{b}"));
                addresses.push(format!("{a}.{b}.a"));
            }
        }
        for p in &patterns {
            for a in &addresses {
                let want = p == a || p == "**" || (p.contains('*') && segments(&p.split('.').collect::<Vec<_>>(), &a.split('.').collect::<Vec<_>>()));
                assert_eq!(matches(p, a), want, "pattern={p:?}, address={a:?}");
            }
        }
    }

    #[test]
    fn validity() {
        assert!(is_valid("mixer.16r.ch.3.fader", false));
        assert!(!is_valid("a..b", false));
        assert!(!is_valid("a.*", false));
        assert!(is_valid("a.*", true));
    }
}
