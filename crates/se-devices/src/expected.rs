//! Expected devices (`project.toml [devices.expected]`) and matching them to present devices.
//!
//! ```toml
//! [devices.expected]
//! vc42_1 = { kind = "camera", label = "VC42 HDMI 1 (kit)", identity = "pci-0000:05:00.0-video-index0" }
//! deck   = { kind = "hid", label = "Stream Deck", usb = "0fd9:006d" }
//! dmx    = { kind = "serial", label = "ENTTEC DMX USB PRO", identity = "usb-ENTTEC_DMX_USB_PRO_*" }
//! ```
//!
//! Criteria (all given ones must match): `identity` (glob), `usb` (`vvvv:pppp`), `serial`,
//! `port` (udev `ID_PATH` glob), `name` (case-insensitive glob). `optional = true` downgrades a
//! missing device from fail to warn.

use crate::identity::{DeviceInfo, Kind, glob};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Expected {
    pub id: String,
    pub kind: Option<Kind>,
    pub label: String,
    pub identity: Option<String>,
    pub usb: Option<(u16, u16)>,
    pub serial: Option<String>,
    pub port: Option<String>,
    pub name: Option<String>,
    pub optional: bool,
}

impl Expected {
    pub fn matches(&self, d: &DeviceInfo) -> bool {
        self.kind.is_none_or(|k| k == d.kind)
            && self.identity.as_ref().is_none_or(|p| glob(p, &d.identity))
            && self.usb.is_none_or(|u| d.usb == Some(u))
            && self.serial.as_ref().is_none_or(|s| d.serial.as_deref() == Some(s.as_str()))
            && self.port.as_ref().is_none_or(|p| d.port.as_deref().is_some_and(|dp| glob(p, dp)))
            && self.name.as_ref().is_none_or(|n| glob(&n.to_lowercase(), &d.name.to_lowercase()))
    }

    fn specificity(&self) -> u32 {
        let exact_identity = self.identity.as_deref().is_some_and(|i| !i.contains('*') && !i.contains('?'));
        (exact_identity as u32) * 8
            + self.serial.is_some() as u32 * 4
            + self.port.is_some() as u32 * 2
            + (self.usb.is_some() || self.name.is_some() || self.identity.is_some()) as u32
    }
}

pub fn parse_usb(s: &str) -> Option<(u16, u16)> {
    let (v, p) = s.trim().split_once(':')?;
    Some((u16::from_str_radix(v, 16).ok()?, u16::from_str_radix(p, 16).ok()?))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Parse the `[devices]` section of `project.toml`. Bad entries are skipped with an error message.
pub fn parse(section: Option<&toml::Value>) -> (Vec<Expected>, Vec<String>) {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    let Some(table) = section.and_then(|s| s.get("expected")).and_then(toml::Value::as_table) else {
        return (out, errors);
    };
    for (id, v) in table {
        let Some(t) = v.as_table() else {
            errors.push(format!("devices.expected.{id}: expected a table"));
            continue;
        };
        if !valid_id(id) {
            errors.push(format!("devices.expected.{id}: id must be [A-Za-z0-9_-]"));
            continue;
        }
        let s = |k: &str| t.get(k).and_then(toml::Value::as_str).map(str::to_string);
        let kind = match s("kind") {
            Some(k) => match Kind::parse(&k) {
                Some(k) => Some(k),
                None => {
                    errors.push(format!("devices.expected.{id}: unknown kind `{k}`"));
                    continue;
                }
            },
            None => None,
        };
        let usb = match s("usb") {
            Some(u) => match parse_usb(&u) {
                Some(u) => Some(u),
                None => {
                    errors.push(format!("devices.expected.{id}: usb must be \"vvvv:pppp\" (hex), got `{u}`"));
                    continue;
                }
            },
            None => None,
        };
        let e = Expected {
            id: id.clone(),
            kind,
            label: s("label").unwrap_or_else(|| id.clone()),
            identity: s("identity"),
            usb,
            serial: s("serial"),
            port: s("port"),
            name: s("name"),
            optional: t.get("optional").and_then(toml::Value::as_bool).unwrap_or(false),
        };
        if e.identity.is_none() && e.usb.is_none() && e.serial.is_none() && e.port.is_none() && e.name.is_none() {
            errors.push(format!("devices.expected.{id}: needs at least one of identity, usb, serial, port, name"));
            continue;
        }
        out.push(e);
    }
    // `toml::Table` iteration order depends on the crate's `preserve_order` feature (unified
    // across the workspace), so sort explicitly for a stable order.
    out.sort_by(|a, b| a.id.cmp(&b.id));
    (out, errors)
}

/// Bind expected entries to present devices: most specific entries choose first; each device
/// binds at most one entry; ties go to the lowest identity. Returns expected id → device index.
pub fn assign(expected: &[Expected], devices: &[DeviceInfo]) -> BTreeMap<String, usize> {
    let mut order: Vec<&Expected> = expected.iter().collect();
    order.sort_by(|a, b| b.specificity().cmp(&a.specificity()).then_with(|| a.id.cmp(&b.id)));
    let mut by_identity: Vec<usize> = (0..devices.len()).collect();
    by_identity.sort_by(|&a, &b| devices[a].identity.cmp(&devices[b].identity));
    let mut taken = BTreeSet::new();
    let mut out = BTreeMap::new();
    for e in order {
        if let Some(&i) = by_identity.iter().find(|&&i| !taken.contains(&i) && e.matches(&devices[i])) {
            taken.insert(i);
            out.insert(e.id.clone(), i);
        }
    }
    out
}

/// Preflight status for expected devices: (status, detail).
pub fn health(expected: &[Expected], bound: &BTreeMap<String, usize>) -> (&'static str, String) {
    if expected.is_empty() {
        return ("warn", "no expected devices declared in project.toml [devices.expected]".into());
    }
    let missing: Vec<&Expected> = expected.iter().filter(|e| !bound.contains_key(&e.id)).collect();
    if missing.is_empty() {
        return ("pass", format!("all {} expected devices present", expected.len()));
    }
    let list = missing.iter().map(|e| format!("{} ({})", e.label, e.id)).collect::<Vec<_>>().join(", ");
    let status = if missing.iter().all(|e| e.optional) { "warn" } else { "fail" };
    (status, format!("missing {}/{}: {list}", missing.len(), expected.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(kind: Kind, identity: &str, name: &str, usb: Option<(u16, u16)>) -> DeviceInfo {
        DeviceInfo {
            kind,
            identity: identity.into(),
            name: name.into(),
            path: String::new(),
            syspath: identity.into(),
            bus: "usb".into(),
            usb,
            serial: None,
            port: None,
            driver: String::new(),
            card: String::new(),
            extra: Default::default(),
        }
    }

    fn cfg(src: &str) -> toml::Value {
        toml::from_str::<toml::Table>(src).unwrap().get("devices").cloned().unwrap()
    }

    #[test]
    fn parses_entries_and_reports_bad_ones() {
        let v = cfg(r#"
            [devices.expected]
            vc42_1 = { kind = "camera", label = "HDMI 1", identity = "pci-0000:05:00.0-video-index0" }
            deck = { kind = "hid", usb = "0fd9:006D" }
            bad_kind = { kind = "toaster", usb = "0fd9:006d" }
            bad_usb = { usb = "zz" }
            nothing = { kind = "midi" }
            "bad id" = { usb = "0fd9:006d" }
        "#);
        let (e, errs) = parse(Some(&v));
        let ids: Vec<&str> = e.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["deck", "vc42_1"]);
        assert_eq!(e[0].usb, Some((0x0fd9, 0x006d)));
        assert_eq!(e[0].label, "deck");
        assert_eq!(errs.len(), 4, "{errs:?}");
        assert!(parse(None).0.is_empty());
    }

    #[test]
    fn assignment_prefers_specific_entries_and_binds_each_device_once() {
        let devices = vec![
            dev(Kind::Midi, "usb-Behringer_X-TOUCH_MINI_1.0.1-00-midi0", "X-TOUCH MINI", Some((0x1397, 0x00b3))),
            dev(Kind::Camera, "pci-0000:05:00.0-video-index1", "AVMatrix HWS Capture 2", None),
            dev(Kind::Camera, "pci-0000:05:00.0-video-index0", "AVMatrix HWS Capture 1", None),
            dev(Kind::Serial, "usb-ENTTEC_DMX_USB_PRO_EN405589-if00-port0", "ENTTEC DMX USB PRO", Some((0x0403, 0x6001))),
        ];
        let v = cfg(r#"
            [devices.expected]
            any_hws = { kind = "camera", name = "avmatrix*" }
            hdmi1 = { kind = "camera", identity = "pci-0000:05:00.0-video-index0" }
            xtouch = { kind = "midi", usb = "1397:00b3" }
            dmx = { kind = "serial", identity = "usb-ENTTEC_DMX_USB_PRO_*" }
            deck = { kind = "hid", usb = "0fd9:006d" }
            wrong_kind = { kind = "camera", usb = "1397:00b3", optional = true }
        "#);
        let (e, errs) = parse(Some(&v));
        assert!(errs.is_empty());
        let b = assign(&e, &devices);
        assert_eq!(b["hdmi1"], 2, "exact identity wins over the name glob");
        assert_eq!(b["any_hws"], 1, "the glob takes the remaining VC42 input");
        assert_eq!(b["xtouch"], 0);
        assert_eq!(b["dmx"], 3);
        assert!(!b.contains_key("deck"));
        assert!(!b.contains_key("wrong_kind"));
        let (st, detail) = health(&e, &b);
        assert_eq!(st, "fail");
        assert!(detail.contains("deck") && detail.contains("missing 2/6"), "{detail}");
    }

    #[test]
    fn optional_only_missing_is_a_warning() {
        let v = cfg(r#"
            [devices.expected]
            cam = { kind = "camera", usb = "0c45:636b", optional = true }
        "#);
        let (e, _) = parse(Some(&v));
        assert_eq!(health(&e, &BTreeMap::new()).0, "warn");
        let devices = vec![dev(Kind::Camera, "usb-Sonix-video-index0", "USB Camera", Some((0x0c45, 0x636b)))];
        let b = assign(&e, &devices);
        assert_eq!(health(&e, &b), ("pass", "all 1 expected devices present".to_string()));
        assert_eq!(health(&[], &b).0, "warn");
    }

    #[test]
    fn matching_criteria_combine() {
        let mut d = dev(Kind::Camera, "usb-MSI-video-index0", "MSI USB Device: MSI STREAMING B", Some((0x0db0, 0x9098)));
        d.serial = Some("20000130041415".into());
        d.port = Some("pci-0000:07:00.0-usb-0:1:1.0".into());
        let e =
            Expected { id: "msi".into(), kind: Some(Kind::Camera), usb: Some((0x0db0, 0x9098)), serial: Some("20000130041415".into()), ..Default::default() };
        assert!(e.matches(&d));
        let e2 = Expected { port: Some("pci-0000:07:00.0-usb-0:2:*".into()), ..e.clone() };
        assert!(!e2.matches(&d));
        let e3 = Expected { name: Some("*streaming*".into()), ..Default::default() };
        assert!(e3.matches(&d));
    }
}
