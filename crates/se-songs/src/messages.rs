//! Chat replies. Every message can be overridden (or silenced with `""`) in
//! `project.toml [songs.messages]`; placeholders are `{user}`, `{title}`, `{duration}`,
//! `{pos}`, `{id}`, `{reason}`, `{votes}`, `{needed}`, `{url}`.

use std::collections::BTreeMap;

pub const DEFAULTS: &[(&str, &str)] = &[
    ("added", "@{user} added \"{title}\" ({duration}) at #{pos}"),
    ("pending", "@{user} \"{title}\" is waiting for mod approval (#{id})"),
    ("approved", "@{user} your request \"{title}\" was approved (#{pos})"),
    ("rejected", "@{user} ✗ {reason}"),
    ("removed", "Removed \"{title}\" (requested by {user})"),
    ("skipped", "Skipped \"{title}\""),
    ("now_playing", ""),
    ("voteskip", "Vote-skip {votes}/{needed} for \"{title}\""),
    ("voteskip_passed", "Vote-skip passed — skipping \"{title}\""),
    ("voteskip_off", ""),
    ("error_skip", "\"{title}\" can't be played here ({reason}) — skipping"),
    ("opened", "Song requests are open! !sr <YouTube link or song name>"),
    ("closed", "Song requests are closed"),
    ("banned_user", "{user} can no longer request songs"),
    ("unbanned_user", "{user} can request songs again"),
    ("banned_song", "Banned \"{title}\" from requests"),
    ("quota_links", "YouTube search is used up for today — requests by link (youtube.com/…) and from the library still work until midnight Pacific"),
    ("quota_library", "YouTube lookups are used up for today — only songs already in the library can be requested until midnight Pacific"),
    ("quota_back", "Song search is back — !sr <YouTube link or song name>"),
];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Messages {
    overrides: BTreeMap<String, String>,
}

impl Messages {
    pub fn new(overrides: BTreeMap<String, String>) -> Messages {
        Messages { overrides }
    }

    pub fn template(&self, key: &str) -> &str {
        if let Some(t) = self.overrides.get(key) {
            return t;
        }
        DEFAULTS.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).unwrap_or("")
    }

    /// Render `key`; `None` when the message is silenced. Values are inserted verbatim (they are
    /// already display-sanitized) and never re-expanded, so user text can't inject placeholders.
    pub fn render(&self, key: &str, vars: &[(&str, &str)]) -> Option<String> {
        let t = self.template(key);
        if t.is_empty() {
            return None;
        }
        let mut out = String::with_capacity(t.len() + 32);
        let mut rest = t;
        while let Some(i) = rest.find('{') {
            out.push_str(&rest[..i]);
            let after = &rest[i + 1..];
            match after.find('}') {
                Some(j) if vars.iter().any(|(k, _)| *k == &after[..j]) => {
                    let name = &after[..j];
                    out.push_str(vars.iter().find(|(k, _)| *k == name).map(|(_, v)| *v).unwrap_or(""));
                    rest = &after[j + 1..];
                }
                _ => {
                    out.push('{');
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        // Twitch chat messages are capped at 500 characters.
        Some(crate::text::display(&out, 480))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_without_reexpanding_user_text() {
        let m = Messages::default();
        let s = m.render("added", &[("user", "a"), ("title", "{user} x"), ("duration", "3:00"), ("pos", "2")]).unwrap();
        assert_eq!(s, "@a added \"{user} x\" (3:00) at #2");
    }

    #[test]
    fn overrides_and_silencing() {
        let mut o = BTreeMap::new();
        o.insert("added".to_string(), "queued {title} {unknown}".to_string());
        o.insert("skipped".to_string(), String::new());
        let m = Messages::new(o);
        assert_eq!(m.render("added", &[("title", "T")]).unwrap(), "queued T {unknown}");
        assert_eq!(m.render("skipped", &[]), None);
        assert_eq!(m.render("now_playing", &[]), None, "silent by default");
    }
}
