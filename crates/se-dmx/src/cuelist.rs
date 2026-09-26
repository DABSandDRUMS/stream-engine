//! Cue lists (§9.2): ordered cues with per-cue and per-attribute fade in/out and delay,
//! wait/follow auto-advance, and tracking (only changes are stored; unchanged values carry
//! forward). A cue expands against the rig into per-address entries; the tracked state at any
//! cue is the fold of all earlier cues (block cues and releases reset it).

use crate::palette::{Palette, Spec, attr_group};
use crate::rig::{AttrKind, Rig};
use se_proto::Ease;
use serde::Deserialize;
use std::collections::BTreeMap;

/// A value with optional per-attribute timing.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub spec: Spec,
    pub fade: Option<u64>,
    pub delay: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cue {
    pub id: String,
    pub label: String,
    pub fade_ms: u64,
    pub fade_out_ms: Option<u64>,
    pub delay_ms: u64,
    pub follow_ms: Option<u64>,
    pub wait_ms: Option<u64>,
    pub ease: Ease,
    pub block: bool,
    pub release: Vec<String>,
    /// Effects started by this cue: (name, size, rate).
    pub effects: Vec<(String, Option<f32>, Option<f32>)>,
    /// Effects stopped (`all` = every effect this playback runs).
    pub stop_effects: Vec<String>,
    /// target → (attribute → entry); target-level `palette(s)` under the pseudo attribute `*`.
    pub set: Vec<(String, Vec<(String, Entry)>, Vec<String>)>,
    /// Arbitrary state addresses held by the playback.
    pub addresses: Vec<(String, Entry)>,
}

impl Cue {
    /// Fade for a change: intensity going down uses `fade_out`.
    pub fn fade_for(&self, down: bool) -> u64 {
        if down { self.fade_out_ms.unwrap_or(self.fade_ms) } else { self.fade_ms }
    }
    /// Longest delay + fade of any entry (completion time).
    pub fn duration(&self) -> u64 {
        let mut d = self.delay_ms + self.fade_ms.max(self.fade_out_ms.unwrap_or(0));
        for (_, attrs, _) in &self.set {
            for (_, e) in attrs {
                d = d.max(e.delay.unwrap_or(self.delay_ms) + e.fade.unwrap_or(self.fade_ms.max(self.fade_out_ms.unwrap_or(0))));
            }
        }
        for (_, e) in &self.addresses {
            d = d.max(e.delay.unwrap_or(self.delay_ms) + e.fade.unwrap_or(self.fade_ms));
        }
        d
    }
    pub fn palettes(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        let mut push = |p: &str| {
            if !v.iter().any(|x| x == p) {
                v.push(p.to_string());
            }
        };
        for (_, attrs, pals) in &self.set {
            for p in pals {
                push(p);
            }
            for (_, e) in attrs {
                if let Spec::Palette(p) = &e.spec {
                    push(p);
                }
            }
        }
        v
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CueList {
    pub name: String,
    pub label: String,
    /// Explicit priority from the file (a floor for callers' priorities).
    pub priority: Option<u16>,
    pub looped: bool,
    pub autorelease: bool,
    pub release_ms: u64,
    pub fader_start: bool,
    pub tracking: bool,
    pub back_fade_ms: Option<u64>,
    pub cues: Vec<Cue>,
}

impl CueList {
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.cues.iter().position(|c| c.id == id)
    }
}

fn dur(v: &toml::Value, what: &str) -> Result<u64, String> {
    match v {
        toml::Value::String(s) => se_proto::parse_duration_ms(s).ok_or_else(|| format!("{what}: bad duration `{s}`")),
        toml::Value::Integer(i) if *i >= 0 => Ok(*i as u64),
        toml::Value::Float(f) if *f >= 0.0 => Ok((*f * 1000.0).round() as u64),
        _ => Err(format!("{what}: expected a duration like \"2s\" or \"500ms\"")),
    }
}

fn opt_dur(v: &Option<toml::Value>, what: &str) -> Result<Option<u64>, String> {
    v.as_ref().map(|v| dur(v, what)).transpose()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawList {
    label: Option<String>,
    priority: Option<u16>,
    #[serde(default, rename = "loop")]
    looped: bool,
    #[serde(default)]
    autorelease: bool,
    release: Option<toml::Value>,
    fader_start: Option<bool>,
    tracking: Option<bool>,
    back_fade: Option<toml::Value>,
    #[serde(default)]
    cue: Vec<RawCue>,
    #[serde(default)]
    notes: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCue {
    id: Option<toml::Value>,
    label: Option<String>,
    fade: Option<toml::Value>,
    fade_out: Option<toml::Value>,
    delay: Option<toml::Value>,
    follow: Option<toml::Value>,
    wait: Option<toml::Value>,
    ease: Option<String>,
    #[serde(default)]
    block: bool,
    #[serde(default)]
    release: Vec<String>,
    #[serde(default)]
    effects: toml::map::Map<String, toml::Value>,
    #[serde(default)]
    stop_effects: Vec<String>,
    #[serde(default)]
    set: toml::map::Map<String, toml::Value>,
    #[serde(default)]
    addresses: toml::map::Map<String, toml::Value>,
    #[serde(default)]
    notes: Option<String>,
}

fn entry(v: &toml::Value, what: &str) -> Result<Entry, String> {
    if let toml::Value::Table(t) = v {
        let value = t.get("value").ok_or_else(|| format!("{what}: timed values need `value`"))?;
        for k in t.keys() {
            if !["value", "fade", "delay"].contains(&k.as_str()) {
                return Err(format!("{what}: unknown key `{k}`"));
            }
        }
        return Ok(Entry {
            spec: Spec::parse(value).map_err(|e| format!("{what}: {e}"))?,
            fade: t.get("fade").map(|f| dur(f, what)).transpose()?,
            delay: t.get("delay").map(|f| dur(f, what)).transpose()?,
        });
    }
    Ok(Entry { spec: Spec::parse(v).map_err(|e| format!("{what}: {e}"))?, fade: None, delay: None })
}

impl CueList {
    pub fn parse(name: &str, t: &toml::Table) -> Result<CueList, String> {
        let r: RawList = toml::Value::Table(t.clone()).try_into().map_err(|e: toml::de::Error| e.message().to_string())?;
        let _ = r.notes;
        let mut cues = Vec::new();
        for (i, c) in r.cue.into_iter().enumerate() {
            let _ = c.notes;
            let id = match c.id {
                None => (i + 1).to_string(),
                Some(toml::Value::String(s)) => s,
                Some(toml::Value::Integer(n)) => n.to_string(),
                Some(toml::Value::Float(f)) => f.to_string(),
                Some(_) => return Err(format!("cue {}: `id` must be a string or number", i + 1)),
            };
            let w = |k: &str| format!("cue `{id}` {k}");
            let ease = match c.ease.as_deref() {
                None => Ease::Linear,
                Some(e) => Ease::parse(e).ok_or_else(|| format!("{}: unknown ease `{e}`", w("ease")))?,
            };
            let mut set = Vec::new();
            for (target, attrs) in &c.set {
                let attrs = attrs.as_table().ok_or_else(|| format!("{}: `set.{target}` must be a table", w("set")))?;
                let mut out = Vec::new();
                let mut pals = Vec::new();
                for (a, v) in attrs {
                    match a.as_str() {
                        "palette" => pals.push(v.as_str().ok_or_else(|| format!("{}: `palette` must be a name", w("set")))?.to_string()),
                        "palettes" => {
                            for p in v.as_array().ok_or_else(|| format!("{}: `palettes` must be a list", w("set")))? {
                                pals.push(p.as_str().ok_or_else(|| format!("{}: `palettes` must be names", w("set")))?.to_string());
                            }
                        }
                        _ => out.push((a.clone(), entry(v, &format!("cue `{id}` set.{target}.{a}"))?)),
                    }
                }
                set.push((target.clone(), out, pals));
            }
            let mut addresses = Vec::new();
            for (a, v) in &c.addresses {
                if !se_proto::address::is_valid(a, false) {
                    return Err(format!("{}: bad address `{a}`", w("addresses")));
                }
                addresses.push((a.clone(), entry(v, &format!("cue `{id}` addresses.{a}"))?));
            }
            let mut effects = Vec::new();
            for (e, p) in &c.effects {
                let t = p.as_table().ok_or_else(|| format!("{}: `effects.{e}` must be a table (use {{}} for defaults)", w("effects")))?;
                let num = |k: &str| t.get(k).and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64))).map(|x| x as f32);
                for k in t.keys() {
                    if !["size", "rate"].contains(&k.as_str()) {
                        return Err(format!("{}: unknown key `{k}`", w("effects")));
                    }
                }
                effects.push((e.clone(), num("size"), num("rate")));
            }
            cues.push(Cue {
                label: c.label.unwrap_or_else(|| format!("Cue {id}")),
                fade_ms: opt_dur(&c.fade, &w("fade"))?.unwrap_or(0),
                fade_out_ms: opt_dur(&c.fade_out, &w("fade_out"))?,
                delay_ms: opt_dur(&c.delay, &w("delay"))?.unwrap_or(0),
                follow_ms: opt_dur(&c.follow, &w("follow"))?,
                wait_ms: opt_dur(&c.wait, &w("wait"))?,
                ease,
                block: c.block,
                release: c.release,
                effects,
                stop_effects: c.stop_effects,
                set,
                addresses,
                id,
            });
        }
        for (i, c) in cues.iter().enumerate() {
            if cues[..i].iter().any(|x| x.id == c.id) {
                return Err(format!("duplicate cue id `{}`", c.id));
            }
        }
        Ok(CueList {
            name: name.into(),
            label: r.label.unwrap_or_else(|| name.to_string()),
            priority: r.priority,
            looped: r.looped,
            autorelease: r.autorelease,
            release_ms: opt_dur(&r.release, "release")?.unwrap_or(1000),
            fader_start: r.fader_start.unwrap_or(true),
            tracking: r.tracking.unwrap_or(true),
            back_fade_ms: opt_dur(&r.back_fade, "back_fade")?,
            cues,
        })
    }
}

/// What an address in a tracked state holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Tracked {
    pub entry: Entry,
    /// Head + attribute kind for fixture attributes (`None` for free addresses).
    pub head: Option<(usize, AttrKind)>,
    pub attr: String,
    /// Cue index that last set it.
    pub cue: usize,
}

/// The full look at a cue: addresses → values, and running effects.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub values: BTreeMap<String, Tracked>,
    pub effects: BTreeMap<String, (Option<f32>, Option<f32>)>,
}

/// Expand one cue's `set`/`addresses` into address entries (general targets first so specific
/// ones win). Errors are collected, not fatal.
pub fn expand(cue: &Cue, idx: usize, rig: &Rig, palettes: &BTreeMap<String, Palette>, errors: &mut Vec<String>) -> BTreeMap<String, Tracked> {
    let mut out = BTreeMap::new();
    let mut targets: Vec<&(String, Vec<(String, Entry)>, Vec<String>)> = cue.set.iter().collect();
    targets.sort_by_key(|(t, _, _)| std::cmp::Reverse(rig.specificity(t)));
    for (target, attrs, pals) in targets {
        if target != "all" && rig.group(target).is_none() && rig.head(target).is_none() {
            errors.push(format!("cue `{}`: unknown fixture or group `{target}`", cue.id));
            continue;
        }
        let put = |attr: &str, e: &Entry, out: &mut BTreeMap<String, Tracked>, errors: &mut Vec<String>, strict: bool| {
            let heads = rig.heads_for(target, attr).unwrap_or_default();
            if heads.is_empty() && strict {
                errors.push(format!("cue `{}`: `{target}` has no attribute `{attr}`", cue.id));
            }
            for h in heads {
                let Some(a) = rig.heads[h].attr(attr) else { continue };
                if let Spec::Palette(p) = &e.spec {
                    match palettes.get(p) {
                        None => {
                            errors.push(format!("cue `{}`: unknown palette `{p}`", cue.id));
                            return;
                        }
                        // a palette without a value for this head leaves it untouched
                        Some(pal) if pal.value_for(rig, h, attr).is_none() => continue,
                        _ => {}
                    }
                }
                out.insert(rig.heads[h].addr(attr), Tracked { entry: e.clone(), head: Some((h, a.kind)), attr: attr.into(), cue: idx });
            }
        };
        for p in pals {
            match palettes.get(p) {
                Some(pal) => {
                    for attr in pal.attrs() {
                        put(attr, &Entry { spec: Spec::Palette(p.clone()), fade: None, delay: None }, &mut out, errors, false);
                    }
                }
                None => errors.push(format!("cue `{}`: unknown palette `{p}`", cue.id)),
            }
        }
        for (attr, e) in attrs {
            match (attr_group(attr), &e.spec) {
                (Some(group), Spec::Palette(_)) => {
                    for a in group {
                        put(a, e, &mut out, errors, false);
                    }
                }
                (Some(_), _) => errors.push(format!("cue `{}`: `{attr}` only takes a palette reference", cue.id)),
                (None, _) => put(attr, e, &mut out, errors, true),
            }
        }
    }
    for (a, e) in &cue.addresses {
        if matches!(e.spec, Spec::Palette(_)) {
            errors.push(format!("cue `{}`: addresses.{a} cannot use a palette", cue.id));
            continue;
        }
        out.insert(a.clone(), Tracked { entry: e.clone(), head: None, attr: a.clone(), cue: idx });
    }
    out
}

/// Addresses a cue's `release` list removes from the tracked state.
pub fn released(cue: &Cue, rig: &Rig) -> Vec<String> {
    let mut v = Vec::new();
    for t in &cue.release {
        let heads: Vec<usize> = match rig.group(t) {
            Some(g) => rig.groups[g].roots.iter().chain(rig.groups[g].leaves.iter()).copied().collect(),
            None => match rig.head(t) {
                Some(h) => std::iter::once(h).chain(rig.heads[h].children.iter().copied()).collect(),
                None => continue,
            },
        };
        for h in heads {
            for a in &rig.heads[h].attrs {
                v.push(rig.heads[h].addr(&a.name));
            }
        }
    }
    v
}

/// Tracked state after cue `k` (0-based).
pub fn tracked(list: &CueList, k: usize, rig: &Rig, palettes: &BTreeMap<String, Palette>) -> State {
    let mut st = State::default();
    let mut sink = Vec::new();
    for (i, cue) in list.cues.iter().enumerate().take(k + 1) {
        apply_cue(&mut st, cue, i, !list.tracking, rig, palettes, &mut sink);
    }
    st
}

/// Fold one cue into a state.
pub fn apply_cue(st: &mut State, cue: &Cue, idx: usize, cue_only: bool, rig: &Rig, palettes: &BTreeMap<String, Palette>, errors: &mut Vec<String>) {
    if cue.block || cue_only {
        st.values.clear();
    }
    for a in released(cue, rig) {
        st.values.remove(&a);
    }
    st.values.extend(expand(cue, idx, rig, palettes, errors));
    for s in &cue.stop_effects {
        if s == "all" {
            st.effects.clear();
        } else {
            st.effects.remove(s);
        }
    }
    for (e, size, rate) in &cue.effects {
        st.effects.insert(e.clone(), (*size, *rate));
    }
}

/// Validate a list against the rig, palettes and effects (for load-time error reporting).
pub fn validate(list: &CueList, rig: &Rig, palettes: &BTreeMap<String, Palette>, effects: &[String]) -> Vec<String> {
    let mut errors = Vec::new();
    for (i, c) in list.cues.iter().enumerate() {
        expand(c, i, rig, palettes, &mut errors);
        for t in &c.release {
            if rig.group(t).is_none() && rig.head(t).is_none() {
                errors.push(format!("cue `{}`: release of unknown `{t}`", c.id));
            }
        }
        for (e, _, _) in &c.effects {
            if !effects.contains(e) {
                errors.push(format!("cue `{}`: unknown effect `{e}`", c.id));
            }
        }
        for e in &c.stop_effects {
            if e != "all" && !effects.contains(e) {
                errors.push(format!("cue `{}`: stop of unknown effect `{e}`", c.id));
            }
        }
    }
    errors.sort();
    errors.dedup();
    errors.into_iter().map(|e| format!("cue list `{}`: {e}", list.name)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::rig;

    const RIG: &str = "[fixtures.a]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 1\n[fixtures.b]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 4\n[groups]\nfront = [\"a\"]";

    fn list(src: &str) -> CueList {
        CueList::parse("main", &toml::from_str(src).unwrap()).unwrap()
    }

    #[test]
    fn tracking_block_release_and_specific_targets() {
        let r = rig(RIG);
        let mut pals = BTreeMap::new();
        pals.insert("warm".into(), Palette::parse("warm", &toml::from_str("[set]\nall = { color = \"#ffaa00\" }").unwrap()).unwrap());
        let l = list(
            r##"
[[cue]]
fade = "2s"
[cue.set]
all = { intensity = 1.0, palette = "warm" }
a = { intensity = { value = 0.5, fade = "4s", delay = "1s" } }
[[cue]]
id = "2"
[cue.set]
b = { color = "#0000ff" }
[[cue]]
release = ["front"]
effects = { chase = { size = 0.5 } }
[[cue]]
block = true
stop_effects = ["all"]
[cue.set]
b = { intensity = 0.3 }
"##,
        );
        assert_eq!(l.cues[0].duration(), 5000, "delay 1 s + attribute fade 4 s");
        let s0 = tracked(&l, 0, &r, &pals);
        assert_eq!(s0.values["lights.a.intensity"].entry.spec, Spec::Literal(se_proto::Value::Float(0.5)), "specific target wins over `all`");
        assert_eq!(s0.values["lights.b.color"].entry.spec, Spec::Palette("warm".into()));
        let s1 = tracked(&l, 1, &r, &pals);
        assert_eq!(s1.values["lights.a.color"].cue, 0, "tracked from cue 1");
        assert_eq!(s1.values["lights.b.color"].cue, 1);
        let s2 = tracked(&l, 2, &r, &pals);
        assert!(!s2.values.contains_key("lights.a.intensity"), "released");
        assert!(s2.values.contains_key("lights.b.intensity"));
        assert_eq!(s2.effects.get("chase"), Some(&(Some(0.5), None)));
        let s3 = tracked(&l, 3, &r, &pals);
        assert_eq!(s3.values.keys().collect::<Vec<_>>(), vec!["lights.b.intensity"], "block cue resets");
        assert!(s3.effects.is_empty());
        assert!(validate(&l, &r, &pals, &["chase".into()]).is_empty(), "{:?}", validate(&l, &r, &pals, &["chase".into()]));
    }

    #[test]
    fn validation_reports_unknowns() {
        let r = rig(RIG);
        let l = list("[[cue]]\neffects = { nope = {} }\n[cue.set]\nghost = { intensity = 1 }\na = { zoom = 0.5, color = \"palette:missing\" }");
        let e = validate(&l, &r, &BTreeMap::new(), &[]).join("\n");
        assert!(e.contains("unknown fixture or group `ghost`"), "{e}");
        assert!(e.contains("`a` has no attribute `zoom`"), "{e}");
        assert!(e.contains("unknown palette `missing`"), "{e}");
        assert!(e.contains("unknown effect `nope`"), "{e}");
        assert!(CueList::parse("x", &toml::from_str("[[cue]]\nid = 1\n[[cue]]\nid = 1").unwrap()).unwrap_err().contains("duplicate"));
        assert!(CueList::parse("x", &toml::from_str("[[cue]]\nfade = \"soon\"").unwrap()).unwrap_err().contains("bad duration"));
    }
}
