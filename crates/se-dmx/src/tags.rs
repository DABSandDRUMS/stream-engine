//! Lighting tags (docs/lights.md §2.9): free lowercase labels on palettes, cue lists and
//! effects, the `lights.tags` index, and the candidate choice behind `lights.layer.pick`.

use crate::show::Show;
use se_proto::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Picks remembered per layer; the ceiling for `avoid_repeat`.
pub const HISTORY: usize = 32;

/// Default `avoid_repeat`.
pub const AVOID_REPEAT: usize = 2;

/// Normalise authored or requested tags: trimmed, lowercase, unique, non-empty.
pub fn parse(raw: Vec<String>) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for t in raw {
        let t = t.trim().to_lowercase();
        if t.is_empty() {
            return Err("`tags` entries must be non-empty".into());
        }
        if !out.contains(&t) {
            out.push(t);
        }
    }
    Ok(out)
}

/// `lights.tags`: tag → names of the palettes, cue lists and effects carrying it (sorted, unique).
pub fn index(show: &Show) -> Value {
    let mut by: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let items = show
        .palettes
        .values()
        .map(|p| (p.name.as_str(), &p.tags))
        .chain(show.lists.values().map(|l| (l.name.as_str(), &l.tags)))
        .chain(show.effects.iter().map(|e| (e.name.as_str(), &e.tags)));
    for (name, tags) in items {
        for t in tags {
            by.entry(t).or_default().insert(name);
        }
    }
    Value::Map(by.into_iter().map(|(t, names)| (t.to_string(), Value::List(names.into_iter().map(|n| Value::Str(n.into())).collect()))).collect())
}

/// What a pick can select.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Palette,
    CueList,
}

impl Source {
    /// The `lights.layer.select` field naming this kind of content.
    pub fn field(self) -> &'static str {
        match self {
            Source::Palette => "palette",
            Source::CueList => "cuelist",
        }
    }

    /// `kind` argument: `palette` | `cuelist` | `any` (default).
    pub fn parse_kind(kind: Option<&str>) -> Result<&'static [Source], String> {
        match kind {
            None | Some("any") => Ok(&[Source::Palette, Source::CueList]),
            Some("palette") => Ok(&[Source::Palette]),
            Some("cuelist") => Ok(&[Source::CueList]),
            Some(o) => Err(format!("unknown kind `{o}` (palette | cuelist | any)")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub source: Source,
    pub name: String,
    /// Number of preferred (`any`) tags it carries.
    pub score: usize,
}

impl Candidate {
    /// History key; a palette and a cue list may share a name.
    pub fn key(&self) -> String {
        format!("{}:{}", self.source.field(), self.name)
    }
}

/// Content of the given kinds carrying every `required` tag, scored by `any` matches.
pub fn candidates(show: &Show, kinds: &[Source], required: &[String], any: &[String]) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut consider = |source: Source, name: &str, tags: &[String]| {
        if required.iter().all(|t| tags.contains(t)) {
            out.push(Candidate { source, name: name.to_string(), score: any.iter().filter(|t| tags.contains(t)).count() });
        }
    };
    for &source in kinds {
        match source {
            Source::Palette => show.palettes.values().for_each(|p| consider(source, &p.name, &p.tags)),
            Source::CueList => show.lists.values().for_each(|l| consider(source, &l.name, &l.tags)),
        }
    }
    out
}

/// Choose one candidate. `recent` holds earlier picks' keys, most recent first. Candidates among
/// the last `avoid` picks are skipped while any other candidate remains; among the rest the
/// highest score wins and `roll` breaks ties. When every candidate was picked recently, the
/// top-scoring one picked longest ago is reused.
pub fn choose<'a>(cands: &'a [Candidate], recent: &VecDeque<String>, avoid: usize, roll: u64) -> Option<&'a Candidate> {
    let age = |c: &Candidate| {
        let key = c.key();
        recent.iter().take(avoid).position(|k| *k == key)
    };
    let fresh: Vec<&Candidate> = cands.iter().filter(|c| age(c).is_none()).collect();
    if fresh.is_empty() {
        let best = cands.iter().map(|c| c.score).max()?;
        return cands.iter().filter(|c| c.score == best).max_by_key(|c| age(c));
    }
    let best = fresh.iter().map(|c| c.score).max()?;
    let top: Vec<&Candidate> = fresh.into_iter().filter(|c| c.score == best).collect();
    Some(top[(roll % top.len() as u64) as usize])
}

/// Remember `key` as the latest pick (moved to the front, history capped).
pub fn remember(recent: &mut VecDeque<String>, key: String) {
    recent.retain(|k| *k != key);
    recent.push_front(key);
    recent.truncate(HISTORY);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, score: usize) -> Candidate {
        Candidate { source: Source::Palette, name: name.into(), score }
    }

    #[test]
    fn tags_are_normalised_and_empty_rejected() {
        assert_eq!(parse(vec![" Heavy".into(), "heavy".into(), "RED".into()]).unwrap(), ["heavy", "red"]);
        assert!(parse(vec![" ".into()]).is_err());
    }

    #[test]
    fn choice_prefers_score_then_avoids_recent() {
        let cands = [c("a", 1), c("b", 0), c("c", 1)];
        let none = VecDeque::new();
        for roll in 0..8 {
            assert_ne!(choose(&cands, &none, 2, roll).unwrap().name, "b", "the lower score never wins while a fresh top score exists");
        }
        let mut recent = VecDeque::new();
        remember(&mut recent, "palette:a".into());
        for roll in 0..8 {
            assert_eq!(choose(&cands, &recent, 2, roll).unwrap().name, "c");
        }
        remember(&mut recent, "palette:c".into());
        assert_eq!(choose(&cands, &recent, 2, 0).unwrap().name, "b", "recent picks are skipped while others remain");
        assert_eq!(choose(&cands, &recent, 0, 0).unwrap().score, 1, "avoid_repeat = 0 disables the history");
        remember(&mut recent, "palette:b".into());
        // Every candidate is recent (window 3): the oldest top-scoring pick returns.
        assert_eq!(choose(&cands, &recent, 3, 5).unwrap().name, "a");
        assert!(choose(&[], &recent, 2, 0).is_none());
    }
}
