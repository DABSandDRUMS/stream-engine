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
    let p: Vec<&str> = pattern.split('.').collect();
    let a: Vec<&str> = addr.split('.').collect();
    match_segs(&p, &a)
}

fn match_segs(p: &[&str], a: &[&str]) -> bool {
    match (p.first(), a.first()) {
        (None, None) => true,
        (Some(&"**"), _) => {
            // zero or more segments
            (0..=a.len()).any(|k| match_segs(&p[1..], &a[k..]))
        }
        (Some(ps), Some(as_)) => glob_seg(ps, as_) && match_segs(&p[1..], &a[1..]),
        _ => false,
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
    let parts: Vec<&str> = p.split('*').collect();
    let mut rest = s;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
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
    fn validity() {
        assert!(is_valid("mixer.16r.ch.3.fader", false));
        assert!(!is_valid("a..b", false));
        assert!(!is_valid("a.*", false));
        assert!(is_valid("a.*", true));
    }
}
