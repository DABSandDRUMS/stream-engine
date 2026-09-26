//! Mix snapshots (`mixes/*.toml`): named sets of mixer values that presets recall or
//! crossfade (PLAN §8.7). Independent of the console's own scenes.
//!
//! ```toml
//! # mixes/brb.toml
//! label = "BRB"
//! fade = "2s"                  # default crossfade when a recall gives none
//! [values]
//! "ch.10.fader" = 0.0          # relative to mixer.<name>, or full "mixer.16r.ch.10.fader"
//! "ch.10.mute" = true
//! "aux.1.ch.10.send" = 0.0
//! ```

use crate::map::{Control, Kind};
use se_proto::{Value, parse_duration_ms};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct MixSnapshot {
    pub name: String,
    pub label: Option<String>,
    pub fade_ms: Option<u32>,
    /// Full address → value.
    pub values: BTreeMap<String, Value>,
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Parse one `mixes/<name>.toml` for the console namespace `prefix` (`mixer.16r`).
pub fn parse(name: &str, t: &toml::Table, prefix: &str) -> Result<MixSnapshot, String> {
    let label = match t.get("label") {
        None => None,
        Some(toml::Value::String(s)) => Some(s.clone()),
        Some(v) => return Err(format!("label: expected a string, got {v}")),
    };
    let fade_ms = match t.get("fade") {
        None => None,
        Some(toml::Value::String(s)) => Some(parse_duration_ms(s).and_then(|v| u32::try_from(v).ok()).ok_or_else(|| format!("fade: bad duration `{s}`"))?),
        Some(toml::Value::Integer(i)) if *i >= 0 => Some(u32::try_from(*i).map_err(|_| "fade: too long".to_string())?),
        Some(v) => return Err(format!("fade: expected a duration, got {v}")),
    };
    let mut values = BTreeMap::new();
    if let Some(v) = t.get("values") {
        let vt = v.as_table().ok_or("[values] must be a table")?;
        for (k, v) in vt {
            let full = if k.starts_with("mixer.") { k.clone() } else { format!("{prefix}.{k}") };
            let rel = full.strip_prefix(&format!("{prefix}.")).ok_or_else(|| format!("`{k}` belongs to another mixer (expected {prefix}.*)"))?;
            let ctl = Control::from_rel_addr(rel).ok_or_else(|| format!("`{k}` is not a mixer control"))?;
            if !ctl.settable() {
                return Err(format!("`{k}` is readback-only"));
            }
            let val = match (ctl.kind(), v) {
                (Kind::Bool, toml::Value::Boolean(b)) => Value::Bool(*b),
                (Kind::Level, toml::Value::Float(f)) if (0.0..=1.0).contains(f) => Value::Float(*f),
                (Kind::Level, toml::Value::Integer(i)) if (0..=1).contains(i) => Value::Float(*i as f64),
                _ => return Err(format!("`{k}`: expected {}, got {v}", if ctl.kind() == Kind::Bool { "true/false" } else { "a number 0–1" })),
            };
            values.insert(full, val);
        }
    }
    Ok(MixSnapshot { name: name.to_string(), label, fade_ms, values })
}

/// Parse every snapshot of the `mixes` kind; broken files are reported, not fatal.
pub fn load_all(kind: &BTreeMap<String, toml::Table>, prefix: &str) -> (BTreeMap<String, MixSnapshot>, Vec<String>) {
    let mut ok = BTreeMap::new();
    let mut errors = Vec::new();
    for (name, t) in kind {
        match parse(name, t, prefix) {
            Ok(s) => {
                ok.insert(name.clone(), s);
            }
            Err(e) => errors.push(format!("mixes/{name}.toml: {e}")),
        }
    }
    (ok, errors)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Animate { addr: String, to: f64, ms: u32 },
    Set { addr: String, value: Value },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecallPlan {
    pub now: Vec<Step>,
    /// Applied when the fade ends (mutes that turn on: fade down first, then mute).
    pub at_end: Vec<Step>,
    /// Addresses not applied (not on this console or not writable).
    pub skipped: Vec<String>,
}

/// Plan a recall. `control` resolves addresses supported by the connected console;
/// `writable` applies the `[mixer] writable` policy.
pub fn plan_recall(s: &MixSnapshot, fade_ms: u32, control: impl Fn(&str) -> Option<Control>, writable: impl Fn(&str) -> bool) -> RecallPlan {
    let mut p = RecallPlan::default();
    for (addr, v) in &s.values {
        let Some(ctl) = control(addr).filter(|c| c.settable() && writable(addr)) else {
            p.skipped.push(addr.clone());
            continue;
        };
        match ctl.kind() {
            Kind::Bool => {
                let step = Step::Set { addr: addr.clone(), value: Value::Bool(v.truthy()) };
                if v.truthy() && fade_ms > 0 { p.at_end.push(step) } else { p.now.push(step) }
            }
            Kind::Level => {
                let to = v.as_f64().unwrap_or(0.0).clamp(0.0, 1.0);
                p.now.push(if fade_ms > 0 {
                    Step::Animate { addr: addr.clone(), to, ms: fade_ms }
                } else {
                    Step::Set { addr: addr.clone(), value: Value::Float(to) }
                });
            }
            Kind::Text => p.skipped.push(addr.clone()),
        }
    }
    p
}

/// Values to store: resolved values of supported, settable controls matching `patterns`
/// (relative to `prefix`).
pub fn collect<'a>(values: impl Iterator<Item = (&'a str, &'a Value)>, prefix: &str, patterns: &[String]) -> BTreeMap<String, Value> {
    let pre = format!("{prefix}.");
    let mut out = BTreeMap::new();
    for (addr, v) in values {
        let Some(rel) = addr.strip_prefix(&pre) else { continue };
        let Some(ctl) = Control::from_rel_addr(rel) else { continue };
        if !ctl.settable() || !patterns.iter().any(|p| se_proto::address::matches(p, rel)) {
            continue;
        }
        let v = match ctl.kind() {
            Kind::Bool => Value::Bool(v.truthy()),
            Kind::Level => match v.as_f64() {
                Some(f) => Value::Float((f.clamp(0.0, 1.0) * 1e5).round() / 1e5),
                None => continue,
            },
            Kind::Text => continue,
        };
        out.insert(rel.to_string(), v);
    }
    out
}

/// Write `mixes/<name>.toml`, replacing its `[values]` and keeping everything else
/// (comments, label, fade) intact.
pub fn write(project_root: &Path, name: &str, label: Option<&str>, values: &BTreeMap<String, Value>) -> anyhow::Result<String> {
    anyhow::ensure!(valid_name(name), "snapshot name `{name}` must be letters, digits, `_` or `-`");
    let project = se_store::Project::open(project_root)?;
    let rel = format!("mixes/{name}.toml");
    let fresh = !project_root.join(&rel).exists();
    project.edit(&rel, |doc| {
        if fresh {
            doc.decor_mut()
                .set_prefix(format!("# Mix snapshot `{name}` (stored by `mixer.snapshot.store`). Recall: `mixer.snapshot.recall snapshot={name} fade=2s`\n"));
        }
        if let Some(l) = label {
            doc["label"] = toml_edit::value(l);
        }
        let mut t = toml_edit::Table::new();
        for (k, v) in values {
            let item = match v {
                Value::Bool(b) => toml_edit::value(*b),
                other => toml_edit::value(other.as_f64().unwrap_or(0.0)),
            };
            t.insert(k, item);
        }
        doc["values"] = toml_edit::Item::Table(t);
        Ok(())
    })?;
    Ok(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "mixer.16r";

    fn snap(s: &str) -> Result<MixSnapshot, String> {
        parse("brb", &s.parse::<toml::Table>().unwrap(), P)
    }

    #[test]
    fn parses_relative_and_full_addresses() {
        let s = snap("label = \"BRB\"\nfade = \"2s\"\n[values]\n\"ch.10.fader\" = 0.0\n\"mixer.16r.ch.10.mute\" = true\n\"aux.1.ch.10.send\" = 1").unwrap();
        assert_eq!(s.fade_ms, Some(2000));
        assert_eq!(s.values.get("mixer.16r.ch.10.fader"), Some(&Value::Float(0.0)));
        assert_eq!(s.values.get("mixer.16r.ch.10.mute"), Some(&Value::Bool(true)));
        assert_eq!(s.values.get("mixer.16r.aux.1.ch.10.send"), Some(&Value::Float(1.0)));
    }

    #[test]
    fn rejects_bad_snapshots() {
        for bad in [
            "[values]\n\"ch.1.name\" = 1.0",
            "[values]\n\"ch.1.fader\" = 1.5",
            "[values]\n\"ch.1.mute\" = 0.5",
            "[values]\n\"lights.x.intensity\" = 0.5",
            "[values]\n\"mixer.24r.ch.1.fader\" = 0.5",
            "fade = \"later\"",
        ] {
            assert!(snap(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn crossfade_plan_orders_mutes_around_the_fade() {
        let s = snap(
            "[values]\n\"ch.1.fader\" = 0.0\n\"ch.1.mute\" = true\n\"ch.2.mute\" = false\n\"ch.2.fader\" = 0.7\n\"ch.99.fader\" = 0.5\n\"main.fader\" = 0.5",
        )
        .unwrap();
        let control =
            |a: &str| a.strip_prefix("mixer.16r.").and_then(Control::from_rel_addr).filter(|c| !matches!(c.strip, crate::map::Strip::Line(n) if n > 16));
        let writable = |a: &str| !a.ends_with("main.fader");
        let p = plan_recall(&s, 2000, control, writable);
        assert!(p.now.contains(&Step::Animate { addr: "mixer.16r.ch.1.fader".into(), to: 0.0, ms: 2000 }));
        assert!(p.now.contains(&Step::Set { addr: "mixer.16r.ch.2.mute".into(), value: Value::Bool(false) }), "unmute before fading in");
        assert_eq!(p.at_end, vec![Step::Set { addr: "mixer.16r.ch.1.mute".into(), value: Value::Bool(true) }], "mute after fading out");
        assert_eq!(p.skipped, vec!["mixer.16r.ch.99.fader".to_string(), "mixer.16r.main.fader".to_string()]);
        // instant recall: everything now
        let p0 = plan_recall(&s, 0, control, writable);
        assert!(p0.at_end.is_empty());
        assert!(p0.now.contains(&Step::Set { addr: "mixer.16r.ch.1.fader".into(), value: Value::Float(0.0) }));
    }

    #[test]
    fn store_collects_strips_not_outputs_and_round_trips() {
        let state = [
            ("mixer.16r.ch.1.fader", Value::Float(0.752_941_2)),
            ("mixer.16r.ch.1.mute", Value::Bool(false)),
            ("mixer.16r.ch.1.name", Value::Str("Kick".into())),
            ("mixer.16r.aux.1.ch.10.send", Value::Float(0.74)),
            ("mixer.16r.aux.1.fader", Value::Float(0.6)),
            ("mixer.16r.main.fader", Value::Float(0.72)),
            ("lights.par.intensity", Value::Float(1.0)),
        ];
        let v = collect(state.iter().map(|(a, v)| (*a, v)), P, &crate::config::default_store());
        assert_eq!(v.keys().cloned().collect::<Vec<_>>(), vec!["aux.1.ch.10.send", "ch.1.fader", "ch.1.mute"]);
        assert_eq!(v["ch.1.fader"], Value::Float(0.75294));

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("project.toml"), "schema = 1\n").unwrap();
        std::fs::create_dir(dir.path().join("mixes")).unwrap();
        std::fs::write(dir.path().join("mixes/brb.toml"), "# keep me\nfade = \"3s\"\n[values]\n\"ch.5.fader\" = 0.1\n").unwrap();
        write(dir.path(), "brb", Some("BRB"), &v).unwrap();
        let text = std::fs::read_to_string(dir.path().join("mixes/brb.toml")).unwrap();
        assert!(text.contains("# keep me"), "{text}");
        let back = parse("brb", &text.parse().unwrap(), P).unwrap();
        assert_eq!(back.fade_ms, Some(3000));
        assert_eq!(back.label.as_deref(), Some("BRB"));
        assert_eq!(back.values.len(), 3, "old values replaced: {text}");
        assert!(write(dir.path(), "../x", None, &v).is_err());
    }
}
