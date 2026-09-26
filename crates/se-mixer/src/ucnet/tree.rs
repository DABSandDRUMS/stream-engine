//! The console's full state (`Synchronize` payload) flattened to `group/chN/param` paths.
//! Port of `util/zlib/zlibNodeParser.ts` + `util/zlib/zlibUtil.ts`.

use super::ubjson::Ub;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq)]
pub enum Leaf {
    Num(f64),
    Text(String),
}

impl Leaf {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Leaf::Num(n) => Some(*n),
            Leaf::Text(_) => None,
        }
    }
}

/// Flattened console state. Paths use `/` like the protocol (`line/ch1/volume`).
#[derive(Debug, Clone, Default)]
pub struct ConsoleState {
    pub values: BTreeMap<String, Leaf>,
    /// Enumerations (`strings`): option labels for a value path (e.g. FX type names).
    pub enums: BTreeMap<String, Vec<String>>,
}

/// Identity of the console from `global/*`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConsoleInfo {
    pub model: String,
    pub name: String,
    pub firmware: String,
    pub serial: String,
}

impl ConsoleState {
    /// Build from a decoded `Synchronize` document.
    pub fn from_sync(doc: &Ub) -> Result<ConsoleState, String> {
        match doc.get("id").and_then(Ub::as_str) {
            Some("Synchronize") => {}
            other => return Err(format!("unexpected state payload id {other:?}")),
        }
        let shared: Vec<Vec<String>> = doc
            .get("shared")
            .and_then(|s| s.get("strings"))
            .and_then(|s| match s {
                Ub::Arr(a) => Some(a),
                _ => None,
            })
            .map(|a| {
                a.iter()
                    .map(|l| match l {
                        Ub::Arr(items) => items.iter().filter_map(|i| i.as_str().map(String::from)).collect(),
                        _ => Vec::new(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut st = ConsoleState::default();
        st.walk(doc, "", &shared);
        if st.values.is_empty() {
            return Err("state payload has no values".into());
        }
        Ok(st)
    }

    fn walk(&mut self, node: &Ub, prefix: &str, shared: &[Vec<String>]) {
        let join = |k: &str| if prefix.is_empty() { k.to_string() } else { format!("{prefix}/{k}") };
        if let Some(values) = node.get("values").and_then(Ub::as_obj) {
            for (k, v) in values {
                let leaf = match v {
                    Ub::Str(s) => Leaf::Text(s.clone()),
                    other => match other.as_f64() {
                        Some(n) => Leaf::Num(n),
                        None => continue,
                    },
                };
                self.values.insert(join(k), leaf);
            }
        }
        if let Some(strings) = node.get("strings").and_then(Ub::as_obj) {
            for (k, v) in strings {
                let opts = match v {
                    Ub::Arr(items) => items.iter().filter_map(|i| i.as_str().map(String::from)).collect(),
                    other => match other.as_f64() {
                        Some(i) if i >= 0.0 => shared.get(i as usize).cloned().unwrap_or_default(),
                        _ => Vec::new(),
                    },
                };
                if !opts.is_empty() {
                    self.enums.insert(join(k), opts);
                }
            }
        }
        if let Some(children) = node.get("children").and_then(Ub::as_obj) {
            for (k, child) in children {
                self.walk(child, &join(k), shared);
            }
        }
    }

    pub fn num(&self, path: &str) -> Option<f64> {
        self.values.get(path).and_then(Leaf::as_f64)
    }

    pub fn text(&self, path: &str) -> Option<&str> {
        match self.values.get(path) {
            Some(Leaf::Text(s)) => Some(s),
            _ => None,
        }
    }

    pub fn has(&self, path: &str) -> bool {
        self.values.contains_key(path)
    }

    /// Channel numbers present under a group (`line` → 1..=16 on a 16R), ascending.
    pub fn strips(&self, group: &str) -> Vec<u16> {
        let prefix = format!("{group}/ch");
        let mut set = BTreeSet::new();
        for k in self.values.range(prefix.clone()..) {
            let Some(rest) = k.0.strip_prefix(&prefix) else { break };
            if let Some((n, _)) = rest.split_once('/')
                && let Ok(n) = n.parse::<u16>()
            {
                set.insert(n);
            }
        }
        set.into_iter().collect()
    }

    pub fn info(&self) -> ConsoleInfo {
        let t = |p: &str| self.text(p).unwrap_or_default().to_string();
        ConsoleInfo { model: t("global/devicename"), name: t("global/mixer_name"), firmware: t("global/mixer_version"), serial: t("global/mixer_serial") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn obj(items: Vec<(&str, Ub)>) -> Ub {
        Ub::Obj(items.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<BTreeMap<_, _>>())
    }

    #[test]
    fn flattens_children_values_and_shared_enums() {
        let doc = obj(vec![
            ("id", Ub::Str("Synchronize".into())),
            ("shared", obj(vec![("strings", Ub::Arr(vec![Ub::Arr(vec![Ub::Str("Plate".into()), Ub::Str("Hall".into())])]))])),
            (
                "children",
                obj(vec![
                    ("global", obj(vec![("values", obj(vec![("devicename", Ub::Str("StudioLive 16R".into())), ("mixer_serial", Ub::Str("RA1".into()))]))])),
                    (
                        "line",
                        obj(vec![(
                            "children",
                            obj(vec![
                                ("ch1", obj(vec![("values", obj(vec![("volume", Ub::Float(0.75)), ("mute", Ub::Int(0))]))])),
                                ("ch10", obj(vec![("values", obj(vec![("volume", Ub::Float(0.5))]))])),
                            ]),
                        )]),
                    ),
                    (
                        "fx",
                        obj(vec![(
                            "children",
                            obj(vec![("ch1", obj(vec![("values", obj(vec![("type", Ub::Float(0.0))])), ("strings", obj(vec![("type", Ub::Int(0))]))]))]),
                        )]),
                    ),
                ]),
            ),
        ]);
        let st = ConsoleState::from_sync(&doc).unwrap();
        assert_eq!(st.num("line/ch1/volume"), Some(0.75));
        assert_eq!(st.num("line/ch1/mute"), Some(0.0));
        assert_eq!(st.strips("line"), vec![1, 10]);
        assert_eq!(st.strips("aux"), Vec::<u16>::new());
        assert_eq!(st.enums.get("fx/ch1/type").unwrap(), &vec!["Plate".to_string(), "Hall".to_string()]);
        assert_eq!(st.info().model, "StudioLive 16R");
        assert_eq!(st.info().serial, "RA1");
    }

    #[test]
    fn rejects_other_payloads() {
        assert!(ConsoleState::from_sync(&obj(vec![("id", Ub::Str("Other".into()))])).is_err());
    }
}
