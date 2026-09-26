//! Device kinds, records, and stable identities.
//!
//! An identity survives reboots and replugging: it is the udev `by-id` name when the device
//! has a USB serial (`usb-Sonix_Technology_Co.__Ltd._USB_Camera_SN0001-video-index0`), else
//! the `by-path` name (`pci-0000:05:00.0-video-index2` = AVMatrix VC42 input 3), else a name
//! built from `ID_PATH`. PipeWire nodes use their `node.name`. Network devices use their MAC
//! (`mac-00:50:c2:12:34:56`), consoles their serial (`ucnet-RA1E24110101`); see [`crate::network`].

use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Camera,
    AudioCard,
    Midi,
    Hid,
    Serial,
    AudioNode,
    /// PreSonus StudioLive console found by UCNET discovery.
    Ucnet,
    /// Art-Net node (DMX over Ethernet) found by ArtPoll.
    ArtNet,
}

impl Kind {
    pub const ALL: [Kind; 8] = [Kind::Camera, Kind::AudioCard, Kind::Midi, Kind::Hid, Kind::Serial, Kind::AudioNode, Kind::Ucnet, Kind::ArtNet];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Camera => "camera",
            Kind::AudioCard => "audio_card",
            Kind::Midi => "midi",
            Kind::Hid => "hid",
            Kind::Serial => "serial",
            Kind::AudioNode => "audio_node",
            Kind::Ucnet => "ucnet",
            Kind::ArtNet => "artnet",
        }
    }

    /// Found on the network (not by udev or PipeWire).
    pub fn is_network(self) -> bool {
        matches!(self, Kind::Ucnet | Kind::ArtNet)
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s).or(match s {
            "video" | "capture" => Some(Kind::Camera),
            "audio" | "sound" => Some(Kind::AudioCard),
            "hidraw" => Some(Kind::Hid),
            "tty" | "dmx" => Some(Kind::Serial),
            "pipewire" | "node" => Some(Kind::AudioNode),
            "mixer" | "presonus" => Some(Kind::Ucnet),
            "art-net" | "art_net" | "dmx_node" => Some(Kind::ArtNet),
            _ => None,
        })
    }
}

/// One present device.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeviceInfo {
    pub kind: Kind,
    pub identity: String,
    /// Human-readable name (`Studio 24c`, `AVMatrix HWS Capture 1`).
    pub name: String,
    /// Device node (`/dev/video0`, `/dev/snd/midiC2D0`, `/dev/hidraw8`), PipeWire node name, or
    /// network address (`10.0.0.187:53000`).
    pub path: String,
    /// sysfs path (udev devices), `pipewire:<id>`, or `net:<identity>`; the key for removal.
    pub syspath: String,
    /// `usb`, `pci`, `pipewire`, `network`, …
    pub bus: String,
    /// USB vendor/product ids.
    pub usb: Option<(u16, u16)>,
    /// USB serial (`ID_SERIAL_SHORT`) or a console's serial number.
    pub serial: Option<String>,
    /// Port path (`ID_PATH`).
    pub port: Option<String>,
    pub driver: String,
    /// ALSA card id (`S24c`) for sound devices; V4L2 bus info for cameras.
    pub card: String,
    /// Kind-specific extras (PipeWire media class/channels, ALSA card number, network ip/mac, …).
    pub extra: BTreeMap<String, String>,
}

impl DeviceInfo {
    pub fn usb_str(&self) -> Option<String> {
        self.usb.map(|(v, p)| format!("{v:04x}:{p:04x}"))
    }
}

/// What udev knows about one device node, plus the parent properties needed to classify it.
#[derive(Clone, Debug, Default)]
pub struct UdevRecord {
    pub subsystem: String,
    pub sysname: String,
    pub syspath: String,
    pub devnode: Option<String>,
    pub driver: Option<String>,
    pub props: BTreeMap<String, String>,
    /// Sound card properties (for `midiC*D*`), HID device properties (for `hidraw*`).
    pub parent_props: BTreeMap<String, String>,
    /// sysfs attributes: `index`/`name` (video4linux), `id`/`number` (sound cards).
    pub attrs: BTreeMap<String, String>,
}

impl UdevRecord {
    fn prop(&self, k: &str) -> Option<&str> {
        self.props.get(k).map(String::as_str).filter(|s| !s.is_empty())
    }
    fn pprop(&self, k: &str) -> Option<&str> {
        self.parent_props.get(k).map(String::as_str).filter(|s| !s.is_empty())
    }
    fn devlinks(&self) -> impl Iterator<Item = &str> {
        self.prop("DEVLINKS").unwrap_or("").split_whitespace()
    }
    /// Basename of the first devlink under `dir` (e.g. `v4l/by-id/`), skipping the
    /// USB-revision variants (`-usbv2-`, `-usbv3-`) of by-path links.
    fn link_in(&self, dir: &str) -> Option<String> {
        self.devlinks().filter(|l| l.contains(dir)).filter(|l| !l.contains("-usbv")).map(|l| l.rsplit('/').next().unwrap_or(l).to_string()).next()
    }
}

/// Decode udev's `\xNN` escapes (`Studio\x2024c` → `Studio 24c`).
pub fn decode_udev(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b.get(i + 1) == Some(&b'x')
            && let Some(v) = std::str::from_utf8(&b[i + 2..i + 4]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).trim().to_string()
}

fn hex16(s: Option<&str>) -> Option<u16> {
    u16::from_str_radix(s?.trim_start_matches("0x"), 16).ok()
}

/// Human name from vendor/model properties (`ID_VENDOR_ENC` + `ID_MODEL_ENC`).
fn vendor_model(p: &BTreeMap<String, String>) -> Option<String> {
    let get = |enc: &str, plain: &str| p.get(enc).map(|s| decode_udev(s)).or_else(|| p.get(plain).map(|s| s.replace('_', " "))).filter(|s| !s.is_empty());
    let model = get("ID_MODEL_ENC", "ID_MODEL");
    let vendor = get("ID_VENDOR_ENC", "ID_VENDOR").or_else(|| p.get("ID_VENDOR_FROM_DATABASE").cloned());
    match (vendor, model) {
        (Some(v), Some(m)) if !m.to_lowercase().contains(&v.to_lowercase()) => Some(format!("{v} {m}")),
        (_, Some(m)) => Some(m),
        (Some(v), None) => Some(v),
        _ => None,
    }
}

fn base_info(kind: Kind, r: &UdevRecord, p: &BTreeMap<String, String>) -> DeviceInfo {
    let get = |k: &str| p.get(k).cloned().filter(|s| !s.is_empty());
    let port = get("ID_PATH");
    let bus = get("ID_BUS").unwrap_or_else(|| match port.as_deref() {
        Some(pp) if pp.contains("-usb-") => "usb".into(),
        Some(pp) if pp.starts_with("pci-") => "pci".into(),
        Some(pp) if pp.starts_with("platform-") => "platform".into(),
        _ => String::new(),
    });
    DeviceInfo {
        kind,
        identity: String::new(),
        name: String::new(),
        path: r.devnode.clone().unwrap_or_default(),
        syspath: r.syspath.clone(),
        bus,
        usb: hex16(get("ID_VENDOR_ID").as_deref()).zip(hex16(get("ID_MODEL_ID").as_deref())),
        serial: get("ID_SERIAL_SHORT"),
        port,
        driver: get("ID_USB_DRIVER").or_else(|| r.driver.clone()).unwrap_or_default(),
        card: String::new(),
        extra: BTreeMap::new(),
    }
}

/// Identity of an ALSA card from its (own or parent) properties.
fn card_identity(p: &BTreeMap<String, String>, attrs_id: Option<&str>) -> Option<String> {
    let get = |k: &str| p.get(k).map(String::as_str).filter(|s| !s.is_empty());
    get("ID_ID")
        .map(|s| s.to_string())
        .or_else(|| get("ID_SERIAL").map(|s| format!("usb-{s}")))
        .or_else(|| get("ID_PATH").map(|s| s.to_string()))
        .or_else(|| attrs_id.map(|a| format!("card-{a}")))
}

/// Classify a udev record into a registry device; None for nodes we don't track (metadata
/// video nodes, PCM/control nodes, built-in serial ports, …).
pub fn classify(r: &UdevRecord) -> Option<DeviceInfo> {
    match r.subsystem.as_str() {
        "video4linux" => {
            let caps = r.prop("ID_V4L_CAPABILITIES").unwrap_or("");
            if !caps.contains(":capture:") || !r.sysname.starts_with("video") {
                return None;
            }
            let mut d = base_info(Kind::Camera, r, &r.props);
            let index = r.attrs.get("index").cloned().unwrap_or_else(|| "0".into());
            d.identity = r
                .link_in("/v4l/by-id/")
                .or_else(|| r.link_in("/v4l/by-path/"))
                .or_else(|| r.prop("ID_PATH").map(|p| format!("{p}-video-index{index}")))
                .unwrap_or_else(|| r.sysname.clone());
            d.name = r.prop("ID_V4L_PRODUCT").map(str::to_string).or_else(|| r.attrs.get("name").cloned()).unwrap_or_else(|| r.sysname.clone());
            d.extra.insert("index".into(), index);
            Some(d)
        }
        "sound" => {
            if let Some(num) = r.sysname.strip_prefix("card") {
                if num.parse::<u32>().is_err() {
                    return None;
                }
                let mut d = base_info(Kind::AudioCard, r, &r.props);
                let id = r.attrs.get("id").cloned().unwrap_or_default();
                d.identity = card_identity(&r.props, Some(&id).filter(|s| !s.is_empty()).map(String::as_str))?;
                d.name = vendor_model_short(&r.props).unwrap_or_else(|| id.clone());
                d.path = format!("/dev/snd/controlC{num}");
                d.card = id;
                d.extra.insert("card".into(), num.to_string());
                Some(d)
            } else if let Some(rest) = r.sysname.strip_prefix("midiC") {
                let (card, dev) = rest.split_once('D')?;
                let (card, dev): (u32, u32) = (card.parse().ok()?, dev.parse().ok()?);
                let mut d = base_info(Kind::Midi, r, &r.parent_props);
                let card_id = r.pprop("__CARD_ID").map(str::to_string);
                let cid = card_identity(&r.parent_props, card_id.as_deref()).unwrap_or_else(|| format!("card{card}"));
                d.identity = format!("{cid}-midi{dev}");
                let base =
                    r.pprop("__MIDI_NAME").map(str::to_string).or_else(|| vendor_model_short(&r.parent_props)).unwrap_or_else(|| format!("MIDI {card}:{dev}"));
                d.name = if dev == 0 { base } else { format!("{base} {}", dev + 1) };
                d.card = card_id.unwrap_or_default();
                d.extra.insert("card".into(), card.to_string());
                d.extra.insert("device".into(), dev.to_string());
                Some(d)
            } else {
                None
            }
        }
        "hidraw" => {
            let mut d = base_info(Kind::Hid, r, &r.props);
            let iface = r.prop("ID_USB_INTERFACE_NUM").unwrap_or("00");
            let mut identity = match (r.prop("ID_SERIAL"), r.prop("ID_BUS")) {
                (Some(serial), Some("usb")) => format!("usb-{serial}-if{iface}"),
                _ => r.prop("ID_PATH").map(str::to_string).unwrap_or_else(|| r.sysname.clone()),
            };
            // Composite receivers expose several HID devices on one interface: qualify by the
            // HID device's own vendor:product when it differs from the USB device.
            if let Some((hv, hp)) = r.pprop("HID_ID").and_then(parse_hid_id) {
                if d.usb.is_some_and(|u| u != (hv, hp)) {
                    identity = format!("{identity}-hid-{hv:04x}:{hp:04x}");
                }
                if d.usb.is_none() {
                    d.usb = Some((hv, hp));
                }
            }
            d.identity = identity;
            d.name = r.pprop("HID_NAME").map(str::to_string).or_else(|| vendor_model(&r.props)).unwrap_or_else(|| r.sysname.clone());
            Some(d)
        }
        "tty" => {
            if r.prop("ID_BUS") != Some("usb") {
                return None;
            }
            let mut d = base_info(Kind::Serial, r, &r.props);
            let iface = r.prop("ID_USB_INTERFACE_NUM").unwrap_or("00");
            d.identity = r
                .link_in("/serial/by-id/")
                .or_else(|| r.link_in("/serial/by-path/"))
                .or_else(|| r.prop("ID_SERIAL").map(|s| format!("usb-{s}-if{iface}-port0")))
                .or_else(|| r.prop("ID_PATH").map(|p| format!("{p}-port0")))
                .unwrap_or_else(|| r.sysname.clone());
            d.name = vendor_model(&r.props).unwrap_or_else(|| r.sysname.clone());
            Some(d)
        }
        _ => None,
    }
}

/// Model name alone when it's descriptive (`Studio 24c`, `X-TOUCH MINI`), else vendor+model.
fn vendor_model_short(p: &BTreeMap<String, String>) -> Option<String> {
    let model = p.get("ID_MODEL_ENC").map(|s| decode_udev(s)).or_else(|| p.get("ID_MODEL").map(|s| s.replace('_', " "))).filter(|s| !s.is_empty());
    match model {
        Some(m) if !m.eq_ignore_ascii_case("usb audio") && !m.eq_ignore_ascii_case("usb camera") && !m.eq_ignore_ascii_case("usb device") => Some(m),
        _ => vendor_model(p),
    }
}

/// `0003:00000FD9:0000006D` → (0x0fd9, 0x006d).
pub fn parse_hid_id(s: &str) -> Option<(u16, u16)> {
    let mut it = s.split(':');
    let _bus = it.next()?;
    let v = u32::from_str_radix(it.next()?, 16).ok()?;
    let p = u32::from_str_radix(it.next()?, 16).ok()?;
    Some((v as u16, p as u16))
}

/// Address-safe id segment for a discovered device (`camera_pci-0000_05_00_0-video-index0`).
pub fn slug(kind: Kind, identity: &str) -> String {
    let mut s = String::with_capacity(identity.len() + 12);
    s.push_str(kind.as_str());
    s.push('_');
    let mut last_us = true;
    for ch in identity.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            s.push(ch.to_ascii_lowercase());
            last_us = false;
        } else if !last_us {
            s.push('_');
            last_us = true;
        }
    }
    while s.ends_with('_') {
        s.pop();
    }
    s
}

/// Glob with `*` (any run) and `?` (one char).
pub fn glob(pattern: &str, s: &str) -> bool {
    fn rec(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => rec(&p[1..], s) || (!s.is_empty() && rec(p, &s[1..])),
            (Some(b'?'), Some(_)) => rec(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => rec(&p[1..], &s[1..]),
            _ => false,
        }
    }
    rec(pattern.as_bytes(), s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(subsystem: &str, sysname: &str, devnode: &str, props: &[(&str, &str)], attrs: &[(&str, &str)]) -> UdevRecord {
        UdevRecord {
            subsystem: subsystem.into(),
            sysname: sysname.into(),
            syspath: format!("/sys/test/{sysname}"),
            devnode: (!devnode.is_empty()).then(|| devnode.into()),
            props: props.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            attrs: attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn vc42_inputs_are_identified_by_pci_path_and_input_index() {
        for i in 0..4 {
            let link = format!("/dev/v4l/by-path/pci-0000:05:00.0-video-index{i}");
            let r = rec(
                "video4linux",
                &format!("video{i}"),
                &format!("/dev/video{i}"),
                &[
                    ("ID_V4L_CAPABILITIES", ":capture:"),
                    ("ID_V4L_PRODUCT", &format!("AVMatrix HWS Capture {}", i + 1)),
                    ("ID_PATH", "pci-0000:05:00.0"),
                    ("DEVLINKS", &link),
                ],
                &[("index", &i.to_string()), ("name", &format!("hws-hdmi{i}"))],
            );
            let d = classify(&r).unwrap();
            assert_eq!(d.kind, Kind::Camera);
            assert_eq!(d.identity, format!("pci-0000:05:00.0-video-index{i}"));
            assert_eq!(d.name, format!("AVMatrix HWS Capture {}", i + 1));
            assert_eq!(d.bus, "pci");
            assert_eq!(d.usb, None);
        }
    }

    #[test]
    fn vc42_without_devlinks_falls_back_to_id_path_and_index() {
        let r = rec("video4linux", "video2", "/dev/video2", &[("ID_V4L_CAPABILITIES", ":capture:"), ("ID_PATH", "pci-0000:05:00.0")], &[("index", "2")]);
        assert_eq!(classify(&r).unwrap().identity, "pci-0000:05:00.0-video-index2");
    }

    #[test]
    fn usb_camera_prefers_serial_by_id_over_port() {
        let r = rec(
            "video4linux",
            "video6",
            "/dev/video6",
            &[
                ("ID_V4L_CAPABILITIES", ":capture:"),
                ("ID_V4L_PRODUCT", "USB Camera: USB Camera"),
                ("ID_BUS", "usb"),
                ("ID_VENDOR_ID", "0c45"),
                ("ID_MODEL_ID", "636b"),
                ("ID_SERIAL_SHORT", "SN0001"),
                ("ID_PATH", "pci-0000:0d:00.4-usb-0:2.2:1.0"),
                (
                    "DEVLINKS",
                    "/dev/v4l/by-path/pci-0000:0d:00.4-usbv2-0:2.2:1.0-video-index0 /dev/v4l/by-path/pci-0000:0d:00.4-usb-0:2.2:1.0-video-index0 /dev/v4l/by-id/usb-Sonix_Technology_Co.__Ltd._USB_Camera_SN0001-video-index0",
                ),
            ],
            &[("index", "0")],
        );
        let d = classify(&r).unwrap();
        assert_eq!(d.identity, "usb-Sonix_Technology_Co.__Ltd._USB_Camera_SN0001-video-index0");
        assert_eq!(d.usb, Some((0x0c45, 0x636b)));
        assert_eq!(d.usb_str().unwrap(), "0c45:636b");
        assert_eq!(d.serial.as_deref(), Some("SN0001"));
        assert_eq!(d.port.as_deref(), Some("pci-0000:0d:00.4-usb-0:2.2:1.0"));
    }

    #[test]
    fn by_path_skips_usb_revision_variant() {
        let r = rec(
            "video4linux",
            "video4",
            "/dev/video4",
            &[
                ("ID_V4L_CAPABILITIES", ":capture:"),
                ("DEVLINKS", "/dev/v4l/by-path/pci-0000:07:00.0-usbv3-0:1:1.0-video-index0 /dev/v4l/by-path/pci-0000:07:00.0-usb-0:1:1.0-video-index0"),
            ],
            &[],
        );
        assert_eq!(classify(&r).unwrap().identity, "pci-0000:07:00.0-usb-0:1:1.0-video-index0");
    }

    #[test]
    fn metadata_video_nodes_are_ignored() {
        let r = rec("video4linux", "video5", "/dev/video5", &[("ID_V4L_CAPABILITIES", ":")], &[("index", "1")]);
        assert!(classify(&r).is_none());
    }

    #[test]
    fn sound_card_and_its_midi_port() {
        let card_props = [
            ("ID_ID", "usb-PreSonus_Studio_24c_SC1M19120661-00"),
            ("ID_BUS", "usb"),
            ("ID_MODEL", "Studio_24c"),
            ("ID_MODEL_ENC", "Studio\\x2024c"),
            ("ID_VENDOR", "PreSonus"),
            ("ID_VENDOR_ID", "194f"),
            ("ID_MODEL_ID", "0109"),
            ("ID_SERIAL", "PreSonus_Studio_24c_SC1M19120661"),
            ("ID_SERIAL_SHORT", "SC1M19120661"),
            ("ID_PATH", "pci-0000:0d:00.4-usb-0:2.1:1.0"),
        ];
        let card = rec("sound", "card2", "", &card_props, &[("id", "S24c"), ("number", "2")]);
        let d = classify(&card).unwrap();
        assert_eq!(d.kind, Kind::AudioCard);
        assert_eq!(d.identity, "usb-PreSonus_Studio_24c_SC1M19120661-00");
        assert_eq!(d.name, "Studio 24c");
        assert_eq!(d.path, "/dev/snd/controlC2");
        assert_eq!(d.card, "S24c");

        let mut midi = rec("sound", "midiC2D0", "/dev/snd/midiC2D0", &[], &[]);
        midi.parent_props = card_props.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        midi.parent_props.insert("__CARD_ID".into(), "S24c".into());
        let m = classify(&midi).unwrap();
        assert_eq!(m.kind, Kind::Midi);
        assert_eq!(m.identity, "usb-PreSonus_Studio_24c_SC1M19120661-00-midi0");
        assert_eq!(m.usb, Some((0x194f, 0x0109)));
        assert_eq!(m.name, "Studio 24c");
        assert_eq!(m.path, "/dev/snd/midiC2D0");

        // PCM and control nodes are not separate devices
        assert!(classify(&rec("sound", "pcmC2D0c", "/dev/snd/pcmC2D0c", &[], &[])).is_none());
        assert!(classify(&rec("sound", "controlC2", "/dev/snd/controlC2", &[], &[])).is_none());
    }

    #[test]
    fn card_without_serial_uses_port_path() {
        let card = rec("sound", "card3", "", &[("ID_PATH", "pci-0000:01:00.1")], &[("id", "NVidia")]);
        assert_eq!(classify(&card).unwrap().identity, "pci-0000:01:00.1");
        let bare = rec("sound", "card9", "", &[], &[("id", "Loopback")]);
        assert_eq!(classify(&bare).unwrap().identity, "card-Loopback");
    }

    #[test]
    fn stream_deck_hidraw() {
        let mut r = rec(
            "hidraw",
            "hidraw8",
            "/dev/hidraw8",
            &[
                ("ID_BUS", "usb"),
                ("ID_MODEL", "Stream_Deck"),
                ("ID_VENDOR", "Elgato"),
                ("ID_VENDOR_ID", "0fd9"),
                ("ID_MODEL_ID", "006d"),
                ("ID_SERIAL", "Elgato_Stream_Deck_AL46J2C62768"),
                ("ID_SERIAL_SHORT", "AL46J2C62768"),
                ("ID_USB_INTERFACE_NUM", "00"),
            ],
            &[],
        );
        r.parent_props.insert("HID_ID".into(), "0003:00000FD9:0000006D".into());
        r.parent_props.insert("HID_NAME".into(), "Elgato Stream Deck".into());
        let d = classify(&r).unwrap();
        assert_eq!(d.identity, "usb-Elgato_Stream_Deck_AL46J2C62768-if00");
        assert_eq!(d.name, "Elgato Stream Deck");
        assert_eq!(d.usb, Some((0x0fd9, 0x006d)));
    }

    #[test]
    fn nested_hid_devices_on_one_interface_get_distinct_identities() {
        let props =
            [("ID_BUS", "usb"), ("ID_VENDOR_ID", "046d"), ("ID_MODEL_ID", "c534"), ("ID_SERIAL", "Logitech_USB_Receiver"), ("ID_USB_INTERFACE_NUM", "01")];
        let mut a = rec("hidraw", "hidraw2", "/dev/hidraw2", &props, &[]);
        a.parent_props.insert("HID_ID".into(), "0003:0000046D:0000C534".into());
        let mut b = rec("hidraw", "hidraw3", "/dev/hidraw3", &props, &[]);
        b.parent_props.insert("HID_ID".into(), "0003:0000046D:00004054".into());
        let (a, b) = (classify(&a).unwrap(), classify(&b).unwrap());
        assert_eq!(a.identity, "usb-Logitech_USB_Receiver-if01");
        assert_eq!(b.identity, "usb-Logitech_USB_Receiver-if01-hid-046d:4054");
    }

    #[test]
    fn usb_serial_uses_serial_by_id_and_skips_builtin_ports() {
        let r = rec(
            "tty",
            "ttyUSB0",
            "/dev/ttyUSB0",
            &[
                ("ID_BUS", "usb"),
                ("ID_VENDOR", "ENTTEC"),
                ("ID_MODEL", "DMX_USB_PRO"),
                ("ID_VENDOR_ID", "0403"),
                ("ID_MODEL_ID", "6001"),
                ("ID_SERIAL_SHORT", "EN405589"),
                ("DEVLINKS", "/dev/serial/by-path/pci-0000:0d:00.4-usb-0:2.3:1.0-port0 /dev/serial/by-id/usb-ENTTEC_DMX_USB_PRO_EN405589-if00-port0"),
            ],
            &[],
        );
        let d = classify(&r).unwrap();
        assert_eq!(d.kind, Kind::Serial);
        assert_eq!(d.identity, "usb-ENTTEC_DMX_USB_PRO_EN405589-if00-port0");
        assert_eq!(d.name, "ENTTEC DMX USB PRO");
        assert!(classify(&rec("tty", "ttyS0", "/dev/ttyS0", &[], &[])).is_none());
    }

    #[test]
    fn udev_escapes_and_slugs() {
        assert_eq!(decode_udev("Sonix\\x20Technology\\x20Co.\\x2c\\x20Ltd."), "Sonix Technology Co., Ltd.");
        assert_eq!(decode_udev("plain\\x"), "plain\\x");
        assert_eq!(slug(Kind::Camera, "pci-0000:05:00.0-video-index0"), "camera_pci-0000_05_00_0-video-index0");
        assert_eq!(
            slug(Kind::AudioNode, "alsa_input.usb-PreSonus_Studio_24c-00.analog-stereo"),
            "audio_node_alsa_input_usb-presonus_studio_24c-00_analog-stereo"
        );
        assert!(se_proto::address::is_valid(&format!("devices.{}.present", slug(Kind::Hid, "usb-Logitech_USB_Receiver-if01-hid-046d:4054")), false));
    }

    #[test]
    fn globs() {
        assert!(glob("usb-ENTTEC_DMX_USB_PRO_*", "usb-ENTTEC_DMX_USB_PRO_EN405589-if00-port0"));
        assert!(glob("pci-0000:05:00.0-video-index?", "pci-0000:05:00.0-video-index3"));
        assert!(!glob("pci-0000:05:00.0-video-index?", "pci-0000:05:00.0-video-index10"));
        assert!(glob("*", ""));
        assert!(!glob("abc", "abcd"));
    }

    #[test]
    fn kinds_parse_with_aliases() {
        for k in Kind::ALL {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
        }
        assert_eq!(Kind::parse("dmx"), Some(Kind::Serial));
        assert_eq!(Kind::parse("bogus"), None);
    }
}
