//! Registry state: present devices + expected entries → ids, published state, query values.

use crate::expected::{self, Expected};
use crate::identity::{DeviceInfo, Kind, slug};
use crate::v4l2;
use se_proto::Value;
use std::collections::{BTreeMap, HashMap};

/// Camera capabilities read once when the camera appears.
#[derive(Clone, Debug, Default)]
pub struct CameraDetails {
    pub caps: v4l2::Caps,
    pub modes: Vec<v4l2::Mode>,
    pub current: Option<v4l2::PixFormat>,
    pub fps: Option<f64>,
    pub dv: Option<v4l2::DvTiming>,
    pub controls: Vec<v4l2::Control>,
    pub error: Option<String>,
}

pub fn camera_details(path: &str) -> CameraDetails {
    let dev = match v4l2::Device::open(path, true) {
        Ok(d) => d,
        Err(e) => return CameraDetails { error: Some(format!("open {path}: {e}")), ..Default::default() },
    };
    let caps = dev.caps().unwrap_or_default();
    let modes = dev.modes().unwrap_or_default();
    CameraDetails {
        caps,
        modes,
        current: dev.format().ok(),
        fps: dev.fps().ok().flatten(),
        dv: dev.current_dv_timing().ok().filter(|t| t.width > 0),
        controls: dev.controls().unwrap_or_default(),
        error: None,
    }
}

/// Fields published under `devices.<id>.*`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Published {
    pub present: bool,
    pub name: String,
    pub kind: String,
    pub path: String,
    pub identity: String,
}

/// One row of the registry view.
#[derive(Clone, Debug)]
pub struct Row<'a> {
    pub id: String,
    pub device: Option<&'a DeviceInfo>,
    pub expected: Option<&'a Expected>,
}

#[derive(Default)]
pub struct Registry {
    /// syspath (or `pipewire:<name>`) → device.
    pub devices: BTreeMap<String, DeviceInfo>,
    pub expected: Vec<Expected>,
    pub details: HashMap<String, CameraDetails>,
}

impl Registry {
    pub fn sorted(&self) -> Vec<&DeviceInfo> {
        let mut v: Vec<&DeviceInfo> = self.devices.values().collect();
        v.sort_by(|a, b| (a.kind, &a.identity).cmp(&(b.kind, &b.identity)));
        v
    }

    /// Expected entries (bound or missing) then unexpected devices, with their ids.
    pub fn rows(&self) -> Vec<Row<'_>> {
        let devs = self.sorted();
        let owned: Vec<DeviceInfo> = devs.iter().map(|d| (*d).clone()).collect();
        let bound = expected::assign(&self.expected, &owned);
        let mut rows = Vec::new();
        let mut used: Vec<bool> = vec![false; devs.len()];
        for e in &self.expected {
            let dev = bound.get(&e.id).map(|&i| {
                used[i] = true;
                devs[i]
            });
            rows.push(Row { id: e.id.clone(), device: dev, expected: Some(e) });
        }
        let mut taken: std::collections::HashSet<String> = rows.iter().map(|r| r.id.clone()).collect();
        for (i, d) in devs.iter().enumerate() {
            if used[i] {
                continue;
            }
            let base = slug(d.kind, &d.identity);
            let mut id = base.clone();
            let mut n = 2;
            while taken.contains(&id) {
                id = format!("{base}-{n}");
                n += 1;
            }
            taken.insert(id.clone());
            rows.push(Row { id, device: Some(d), expected: None });
        }
        rows
    }

    /// Id of a present device (by syspath key).
    pub fn id_of(&self, key: &str) -> Option<String> {
        let d = self.devices.get(key)?;
        self.rows().into_iter().find(|r| r.device.is_some_and(|x| x.syspath == d.syspath)).map(|r| r.id)
    }

    pub fn published(&self) -> BTreeMap<String, Published> {
        self.rows()
            .into_iter()
            .map(|r| {
                let p = match (r.device, r.expected) {
                    (Some(d), e) => Published {
                        present: true,
                        name: e.map(|e| e.label.clone()).unwrap_or_else(|| d.name.clone()),
                        kind: d.kind.as_str().into(),
                        path: d.path.clone(),
                        identity: d.identity.clone(),
                    },
                    (None, Some(e)) => Published {
                        present: false,
                        name: e.label.clone(),
                        kind: e.kind.map(|k| k.as_str().to_string()).unwrap_or_default(),
                        path: String::new(),
                        identity: e.identity.clone().unwrap_or_default(),
                    },
                    (None, None) => Published::default(),
                };
                (r.id, p)
            })
            .collect()
    }

    pub fn health(&self) -> (&'static str, String) {
        let devs: Vec<DeviceInfo> = self.sorted().into_iter().cloned().collect();
        let bound = expected::assign(&self.expected, &devs);
        expected::health(&self.expected, &bound)
    }

    /// Full registry for the `devices` query. `sources` maps device identity → source name;
    /// `inputs` maps camera path → V4L2 input status flags.
    pub fn to_value(&self, sources: &HashMap<String, String>, inputs: &HashMap<String, u32>) -> Value {
        let mut devices = Vec::new();
        let mut expected_list = Vec::new();
        for r in self.rows() {
            if let Some(e) = r.expected {
                expected_list.push(
                    Value::map()
                        .with("id", r.id.as_str())
                        .with("label", e.label.as_str())
                        .with("kind", e.kind.map(|k| k.as_str()).unwrap_or(""))
                        .with("present", r.device.is_some())
                        .with("optional", e.optional)
                        .with(
                            "identity",
                            r.device.map(|d| Value::from(d.identity.as_str())).or_else(|| e.identity.as_deref().map(Value::from)).unwrap_or(Value::Null),
                        ),
                );
            }
            let Some(d) = r.device else { continue };
            let mut v = Value::map()
                .with("id", r.id.as_str())
                .with("kind", d.kind.as_str())
                .with("name", d.name.as_str())
                .with("label", r.expected.map(|e| e.label.as_str()).unwrap_or(d.name.as_str()))
                .with("identity", d.identity.as_str())
                .with("path", d.path.as_str())
                .with("present", true)
                .with("expected", r.expected.map(|e| Value::from(e.id.as_str())).unwrap_or(Value::Null))
                .with("driver", d.driver.as_str())
                .with("card", d.card.as_str())
                .with("bus", d.bus.as_str())
                .with("usb", d.usb_str().map(Value::from).unwrap_or(Value::Null))
                .with("serial", d.serial.as_deref().map(Value::from).unwrap_or(Value::Null))
                .with("port", d.port.as_deref().map(Value::from).unwrap_or(Value::Null))
                .with("source", sources.get(&d.identity).map(|s| Value::from(s.as_str())).unwrap_or(Value::Null));
            if !d.extra.is_empty() {
                v = v.with("extra", Value::Map(d.extra.iter().map(|(k, x)| (k.clone(), Value::from(x.as_str()))).collect()));
            }
            if d.kind == Kind::Camera {
                let det = self.details.get(&d.identity);
                v = v.with("modes", det.map(|x| modes_value(&x.modes)).unwrap_or(Value::List(Vec::new())));
                if let Some(det) = det {
                    if let Some(f) = det.current {
                        v = v.with(
                            "current",
                            Value::map()
                                .with("format", v4l2::format_name(f.fourcc))
                                .with("width", f.width as i64)
                                .with("height", f.height as i64)
                                .with("fps", det.fps.map(Value::Float).unwrap_or(Value::Null))
                                .with("matrix", f.matrix())
                                .with("range", f.range()),
                        );
                    }
                    if let Some(t) = det.dv {
                        v = v.with("dv_timing", Value::map().with("width", t.width as i64).with("height", t.height as i64).with("fps", t.fps));
                    }
                    v = v.with("controls", Value::List(det.controls.iter().map(|c| c.name.as_str().into()).collect()));
                    if let Some(e) = &det.error {
                        v = v.with("error", e.as_str());
                    }
                }
                match inputs.get(&d.path) {
                    Some(&st) => {
                        v = v.with("input_status", v4l2::input_status_str(st)).with("signal", !v4l2::input_has_no_signal(st));
                    }
                    None => v = v.with("signal", Value::Null),
                }
            }
            devices.push(v);
        }
        let (status, detail) = self.health();
        Value::map()
            .with("devices", Value::List(devices))
            .with("expected", Value::List(expected_list))
            .with("health", Value::map().with("status", status).with("detail", detail))
    }
}

pub fn modes_value(modes: &[v4l2::Mode]) -> Value {
    Value::List(
        modes
            .iter()
            .map(|m| {
                Value::map()
                    .with("format", v4l2::format_name(m.fourcc))
                    .with("width", m.width as i64)
                    .with("height", m.height as i64)
                    .with("fps", Value::List(m.fps.iter().map(|f| Value::Float(*f)).collect()))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(kind: Kind, identity: &str, key: &str) -> DeviceInfo {
        DeviceInfo {
            kind,
            identity: identity.into(),
            name: format!("dev {identity}"),
            path: format!("/dev/{key}"),
            syspath: key.into(),
            bus: "usb".into(),
            usb: None,
            serial: None,
            port: None,
            driver: String::new(),
            card: String::new(),
            extra: Default::default(),
        }
    }

    #[test]
    fn rows_ids_and_published_state() {
        let mut r = Registry::default();
        r.devices.insert("a".into(), dev(Kind::Camera, "pci-0000:05:00.0-video-index0", "a"));
        r.devices.insert("b".into(), dev(Kind::Hid, "usb-Logitech-if00", "b"));
        r.expected = vec![
            Expected {
                id: "kit".into(),
                kind: Some(Kind::Camera),
                label: "Kit cam".into(),
                identity: Some("pci-0000:05:00.0-video-index0".into()),
                ..Default::default()
            },
            Expected { id: "deck".into(), kind: Some(Kind::Hid), label: "Stream Deck".into(), usb: Some((0x0fd9, 0x006d)), ..Default::default() },
        ];
        let p = r.published();
        assert_eq!(
            p["kit"],
            Published { present: true, name: "Kit cam".into(), kind: "camera".into(), path: "/dev/a".into(), identity: "pci-0000:05:00.0-video-index0".into() }
        );
        assert!(!p["deck"].present);
        assert_eq!(p["deck"].kind, "hid");
        assert!(p["hid_usb-logitech-if00"].present);
        assert_eq!(r.id_of("a").as_deref(), Some("kit"));
        assert_eq!(r.id_of("b").as_deref(), Some("hid_usb-logitech-if00"));
        assert_eq!(r.health().0, "fail");
        let v =
            r.to_value(&HashMap::from([("pci-0000:05:00.0-video-index0".to_string(), "cam_kit".to_string())]), &HashMap::from([("/dev/a".to_string(), 2u32)]));
        let cams: Vec<&Value> =
            v.get_path("devices").unwrap().as_list().unwrap().iter().filter(|d| d.get_path("kind").and_then(Value::as_str) == Some("camera")).collect();
        assert_eq!(cams[0].get_path("source").and_then(Value::as_str), Some("cam_kit"));
        assert_eq!(cams[0].get_path("signal"), Some(&Value::Bool(false)));
        assert_eq!(v.get_path("expected").unwrap().as_list().unwrap().len(), 2);
    }

    #[test]
    fn duplicate_identities_get_distinct_ids() {
        let mut r = Registry::default();
        r.devices.insert("x".into(), dev(Kind::Midi, "usb-X-midi0", "x"));
        r.devices.insert("y".into(), dev(Kind::Midi, "usb-X-midi0", "y"));
        let ids: Vec<String> = r.rows().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["midi_usb-x-midi0", "midi_usb-x-midi0-2"]);
    }
}
