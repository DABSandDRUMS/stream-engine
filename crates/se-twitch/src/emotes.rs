//! Chat badges and emotes for overlays: Twitch (Helix) plus the public 7TV, BTTV, and FFZ
//! APIs. Parsed sets are cached in the runtime DB so overlays have emotes before the network
//! answers; the `emotes` query exposes `{badges: {"set/id": url}, emotes: {code: emote}}`.

use crate::normalize::{Emote, twitch_emote_url};
use se_proto::Value;
use serde_json::Value as J;
use std::collections::{BTreeMap, HashMap};

/// BTTV modifier emotes drawn over the previous emote.
const BTTV_ZERO_WIDTH: &[&str] = &["SoSnowy", "IceCold", "SantaHat", "TopHat", "ReinDeer", "CandyCane", "cvMask", "cvHazmat"];

pub const SEVENTV_GLOBAL: &str = "https://7tv.io/v3/emote-sets/global";
pub const BTTV_GLOBAL: &str = "https://api.betterttv.net/3/cached/emotes/global";
pub const FFZ_GLOBAL: &str = "https://api.frankerfacez.com/v1/set/global";

pub fn seventv_user(twitch_id: &str) -> String {
    format!("https://7tv.io/v3/users/twitch/{twitch_id}")
}
pub fn bttv_user(twitch_id: &str) -> String {
    format!("https://api.betterttv.net/3/cached/users/twitch/{twitch_id}")
}
pub fn ffz_room(twitch_id: &str) -> String {
    format!("https://api.frankerfacez.com/v1/room/id/{twitch_id}")
}

fn s(v: &J, k: &str) -> String {
    match v.get(k) {
        Some(J::String(x)) => x.clone(),
        Some(J::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// 7TV emote list (`emote_set.emotes` or a set's `emotes`).
pub fn parse_7tv(set: &J) -> Vec<(String, Emote)> {
    let list = set.get("emotes").or_else(|| set.pointer("/emote_set/emotes")).and_then(J::as_array);
    list.map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter_map(|e| {
            let code = s(e, "name");
            let data = e.get("data")?;
            let host = data.get("host")?;
            let base = s(host, "url");
            if code.is_empty() || base.is_empty() {
                return None;
            }
            let files: Vec<String> = host.get("files").and_then(J::as_array).map(|f| f.iter().map(|x| s(x, "name")).collect()).unwrap_or_default();
            let file = ["4x.webp", "3x.webp", "2x.webp", "1x.webp"].iter().find(|f| files.iter().any(|x| x == *f)).copied().unwrap_or("2x.webp");
            let url = if base.starts_with("//") { format!("https:{base}/{file}") } else { format!("{base}/{file}") };
            let flags = e.get("flags").and_then(J::as_i64).unwrap_or(0);
            let data_flags = data.get("flags").and_then(J::as_i64).unwrap_or(0);
            Some((
                code,
                Emote {
                    id: s(e, "id"),
                    url,
                    provider: "7tv".into(),
                    animated: data.get("animated").and_then(J::as_bool).unwrap_or(false),
                    zero_width: flags & 1 != 0 || data_flags & (1 << 8) != 0,
                },
            ))
        })
        .collect()
}

/// BTTV global list, or a user's `channelEmotes` + `sharedEmotes`.
pub fn parse_bttv(v: &J) -> Vec<(String, Emote)> {
    let lists: Vec<&J> = match v {
        J::Array(_) => vec![v],
        _ => ["channelEmotes", "sharedEmotes"].iter().filter_map(|k| v.get(*k)).collect(),
    };
    lists
        .into_iter()
        .filter_map(J::as_array)
        .flatten()
        .filter_map(|e| {
            let (id, code) = (s(e, "id"), s(e, "code"));
            if id.is_empty() || code.is_empty() {
                return None;
            }
            let animated = e.get("animated").and_then(J::as_bool).unwrap_or(false) || s(e, "imageType") == "gif";
            let zero_width = BTTV_ZERO_WIDTH.contains(&code.as_str());
            Some((code, Emote { url: format!("https://cdn.betterttv.net/emote/{id}/3x"), id, provider: "bttv".into(), animated, zero_width }))
        })
        .collect()
}

/// FFZ `/set/global` (default sets only) or `/room/id/<id>` (the room's set).
pub fn parse_ffz(v: &J) -> Vec<(String, Emote)> {
    let sets = v.get("sets").and_then(J::as_object);
    let wanted: Vec<String> = if let Some(d) = v.get("default_sets").and_then(J::as_array) {
        d.iter().map(|x| x.to_string()).collect()
    } else if let Some(r) = v.pointer("/room/set") {
        vec![r.to_string()]
    } else {
        sets.map(|m| m.keys().cloned().collect()).unwrap_or_default()
    };
    let mut out = Vec::new();
    for (k, set) in sets.into_iter().flatten() {
        if !wanted.contains(k) {
            continue;
        }
        for e in set.get("emoticons").and_then(J::as_array).map(Vec::as_slice).unwrap_or(&[]) {
            let (id, code) = (s(e, "id"), s(e, "name"));
            let pick = |urls: Option<&J>| urls.and_then(|u| ["4", "2", "1"].iter().find_map(|k| u.get(*k).and_then(J::as_str).map(String::from)));
            let animated_url = pick(e.get("animated").filter(|a| a.is_object()));
            let Some(url) = animated_url.clone().or_else(|| pick(e.get("urls"))) else { continue };
            let url = if url.starts_with("//") { format!("https:{url}") } else { url };
            out.push((
                code,
                Emote {
                    id,
                    url,
                    provider: "ffz".into(),
                    animated: animated_url.is_some(),
                    zero_width: e.get("modifier").and_then(J::as_bool).unwrap_or(false),
                },
            ));
        }
    }
    out
}

/// Helix `/chat/emotes` (global or channel).
pub fn parse_twitch_emotes(v: &J) -> Vec<(String, Emote)> {
    v.get("data")
        .and_then(J::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter_map(|e| {
            let (id, code) = (s(e, "id"), s(e, "name"));
            if id.is_empty() {
                return None;
            }
            let animated = e.get("format").and_then(J::as_array).is_some_and(|f| f.iter().any(|x| x.as_str() == Some("animated")));
            Some((code, Emote { url: twitch_emote_url(&id, animated), id, provider: "twitch".into(), animated, zero_width: false }))
        })
        .collect()
}

/// Helix `/chat/badges` (global or channel) → `"set_id/id"` → image url.
pub fn parse_badges(v: &J) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for set in v.get("data").and_then(J::as_array).map(Vec::as_slice).unwrap_or(&[]) {
        let set_id = s(set, "set_id");
        for ver in set.get("versions").and_then(J::as_array).map(Vec::as_slice).unwrap_or(&[]) {
            let url = ["image_url_4x", "image_url_2x", "image_url_1x"].iter().map(|k| s(ver, k)).find(|u| !u.is_empty());
            if let Some(url) = url {
                out.push((format!("{set_id}/{}", s(ver, "id")), url));
            }
        }
    }
    out
}

/// Raw API responses, one per source; kept so a failed refresh keeps the last good set.
#[derive(Default, Clone)]
pub struct Sources {
    pub responses: BTreeMap<String, J>,
}

/// Source keys in increasing precedence (later entries override earlier codes).
pub const ORDER: &[&str] = &["twitch.global", "ffz.global", "bttv.global", "7tv.global", "twitch.channel", "ffz.channel", "bttv.channel", "7tv.channel"];

/// Merged, ready-to-use caches.
#[derive(Default, Clone)]
pub struct EmoteSet {
    pub badges: HashMap<String, String>,
    /// Third-party emotes by code (matched against chat words).
    pub third_party: HashMap<String, Emote>,
    /// Twitch emotes by code (for the query; chat messages carry their ids).
    pub twitch: HashMap<String, Emote>,
}

impl EmoteSet {
    pub fn build(src: &Sources) -> EmoteSet {
        let mut set = EmoteSet::default();
        for key in ORDER {
            let Some(v) = src.responses.get(*key) else { continue };
            let list = match key.split('.').next().unwrap_or("") {
                "7tv" => parse_7tv(v),
                "bttv" => parse_bttv(v),
                "ffz" => parse_ffz(v),
                _ => parse_twitch_emotes(v),
            };
            for (code, e) in list {
                if e.provider == "twitch" {
                    set.twitch.insert(code, e);
                } else {
                    set.third_party.insert(code, e);
                }
            }
        }
        for key in ["badges.global", "badges.channel"] {
            if let Some(v) = src.responses.get(key) {
                set.badges.extend(parse_badges(v));
            }
        }
        set
    }

    /// `{badges: {"set/id": url}, emotes: {code: {id, url, provider, animated, zero_width}}}`.
    pub fn value(&self) -> Value {
        let badges: BTreeMap<String, Value> = self.badges.iter().map(|(k, v)| (k.clone(), Value::Str(v.clone()))).collect();
        let mut emotes: BTreeMap<String, Value> = self.twitch.iter().map(|(k, e)| (k.clone(), e.value())).collect();
        emotes.extend(self.third_party.iter().map(|(k, e)| (k.clone(), e.value())));
        Value::map().with("badges", Value::Map(badges)).with("emotes", Value::Map(emotes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn providers_parse_and_channel_overrides_global() {
        let seventv = json!({"emote_set": {"emotes": [
            {"id": "7a", "name": "catJAM", "flags": 0, "data": {"animated": true, "flags": 0, "host": {"url": "//cdn.7tv.app/emote/7a", "files": [{"name": "1x.webp"}, {"name": "4x.webp"}]}}},
            {"id": "7b", "name": "RainTime", "flags": 1, "data": {"animated": true, "host": {"url": "//cdn.7tv.app/emote/7b", "files": []}}}
        ]}});
        let bttv_user = json!({"channelEmotes": [{"id": "b1", "code": "catJAM", "imageType": "gif", "animated": true}], "sharedEmotes": [{"id": "b2", "code": "IceCold", "imageType": "png"}]});
        let bttv_global = json!([{"id": "bg", "code": "FeelsGoodMan", "imageType": "png"}]);
        let ffz = json!({"default_sets": [3], "sets": {"3": {"emoticons": [{"id": 1, "name": "ZreknarF", "urls": {"1": "https://cdn.frankerfacez.com/emote/1/1", "4": "https://cdn.frankerfacez.com/emote/1/4"}, "modifier": false}]}, "4": {"emoticons": [{"id": 9, "name": "NotDefault", "urls": {"1": "x"}}]}}});
        let tw = json!({"data": [{"id": "25", "name": "Kappa", "format": ["static"]}], "template": "…"});
        let badges = json!({"data": [{"set_id": "subscriber", "versions": [{"id": "0", "image_url_1x": "a", "image_url_4x": "https://b/4x"}]}]});
        let mut src = Sources::default();
        src.responses.insert("7tv.channel".into(), seventv);
        src.responses.insert("bttv.channel".into(), bttv_user);
        src.responses.insert("bttv.global".into(), bttv_global);
        src.responses.insert("ffz.global".into(), ffz);
        src.responses.insert("twitch.global".into(), tw);
        src.responses.insert("badges.global".into(), badges);
        let set = EmoteSet::build(&src);
        let cat = &set.third_party["catJAM"];
        assert_eq!((cat.provider.as_str(), cat.url.as_str(), cat.animated), ("7tv", "https://cdn.7tv.app/emote/7a/4x.webp", true), "7TV channel wins");
        assert!(set.third_party["RainTime"].zero_width);
        assert!(set.third_party["IceCold"].zero_width);
        assert_eq!(set.third_party["ZreknarF"].url, "https://cdn.frankerfacez.com/emote/1/4");
        assert!(!set.third_party.contains_key("NotDefault"));
        assert_eq!(set.third_party["FeelsGoodMan"].url, "https://cdn.betterttv.net/emote/bg/3x");
        assert_eq!(set.twitch["Kappa"].url, "https://static-cdn.jtvnw.net/emoticons/v2/25/static/dark/3.0");
        assert_eq!(set.badges["subscriber/0"], "https://b/4x");
        let v = set.value();
        assert_eq!(v.get_path("emotes.Kappa.provider").and_then(Value::as_str), Some("twitch"));
        assert_eq!(v.get_path("badges").and_then(Value::as_map).map(|m| m.len()), Some(1));
    }
}
