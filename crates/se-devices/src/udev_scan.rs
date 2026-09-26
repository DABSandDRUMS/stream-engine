//! udev enumeration and hotplug monitoring for the subsystems we track.

use crate::identity::{DeviceInfo, UdevRecord, classify};
use std::collections::BTreeMap;
use std::os::fd::AsRawFd;

pub const SUBSYSTEMS: [&str; 4] = ["video4linux", "sound", "hidraw", "tty"];

#[derive(Debug)]
pub enum Hotplug {
    Added(Box<DeviceInfo>),
    /// A tracked device went away (by sysfs path).
    Removed(String),
}

fn os(s: Option<&std::ffi::OsStr>) -> Option<String> {
    s.map(|v| v.to_string_lossy().into_owned())
}

fn props_of(d: &udev::Device) -> BTreeMap<String, String> {
    d.properties().map(|p| (p.name().to_string_lossy().into_owned(), p.value().to_string_lossy().into_owned())).collect()
}

/// Build a record for a udev device, pulling parent card/HID properties where needed.
pub fn record(d: &udev::Device) -> UdevRecord {
    let subsystem = os(d.subsystem()).unwrap_or_default();
    let sysname = d.sysname().to_string_lossy().into_owned();
    let mut r = UdevRecord {
        subsystem: subsystem.clone(),
        sysname: sysname.clone(),
        syspath: d.syspath().to_string_lossy().into_owned(),
        devnode: d.devnode().map(|p| p.to_string_lossy().into_owned()),
        driver: os(d.driver()),
        props: props_of(d),
        ..Default::default()
    };
    match subsystem.as_str() {
        "video4linux" => {
            for a in ["index", "name"] {
                if let Some(v) = os(d.attribute_value(a)) {
                    r.attrs.insert(a.into(), v.trim().to_string());
                }
            }
            if r.driver.is_none() {
                r.driver = d.parent().and_then(|p| os(p.driver()));
            }
        }
        "sound" => {
            if sysname.starts_with("card") {
                for a in ["id", "number"] {
                    if let Some(v) = os(d.attribute_value(a)) {
                        r.attrs.insert(a.into(), v.trim().to_string());
                    }
                }
            } else if let Some(card) = d.parent() {
                // midiC*D*: the card holds the USB identity.
                r.parent_props = props_of(&card);
                if let Some(id) = os(card.attribute_value("id")) {
                    r.parent_props.insert("__CARD_ID".into(), id.trim().to_string());
                }
                if let Some((c, dnum)) = sysname.strip_prefix("midiC").and_then(|rest| rest.split_once('D'))
                    && let Ok(name) = std::fs::read_to_string(format!("/proc/asound/card{c}/midi{dnum}"))
                    && let Some(first) = name.lines().next().map(str::trim).filter(|s| !s.is_empty())
                {
                    r.parent_props.insert("__MIDI_NAME".into(), first.to_string());
                }
            }
        }
        "hidraw" => {
            if let Ok(Some(hid)) = d.parent_with_subsystem("hid") {
                r.parent_props = props_of(&hid);
            }
        }
        "tty" if r.driver.is_none() => {
            r.driver = d.parent().and_then(|p| os(p.driver()));
        }
        _ => {}
    }
    r
}

/// All currently present tracked devices.
pub fn scan() -> std::io::Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    for sub in SUBSYSTEMS {
        let mut e = udev::Enumerator::new()?;
        e.match_subsystem(sub)?;
        e.match_is_initialized()?;
        for d in e.scan_devices()? {
            if let Some(info) = classify(&record(&d)) {
                out.push(info);
            }
        }
    }
    out.sort_by(|a, b| a.identity.cmp(&b.identity));
    Ok(out)
}

/// Present cameras only.
pub fn scan_cameras() -> std::io::Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    let mut e = udev::Enumerator::new()?;
    e.match_subsystem("video4linux")?;
    for d in e.scan_devices()? {
        if let Some(info) = classify(&record(&d)) {
            out.push(info);
        }
    }
    out.sort_by(|a, b| a.identity.cmp(&b.identity));
    Ok(out)
}

/// Resolve a camera spec from a source file: a stable identity (glob allowed), a
/// `/dev/v4l/by-*/…` link, or a `/dev/videoN` path.
pub fn find_camera(spec: &str) -> Option<DeviceInfo> {
    let cams = scan_cameras().ok()?;
    if spec.starts_with('/') {
        let target = std::fs::canonicalize(spec).ok()?;
        let t = target.to_string_lossy();
        return cams.into_iter().find(|c| c.path == t);
    }
    if let Some(c) = cams.iter().find(|c| c.identity == spec) {
        return Some(c.clone());
    }
    cams.into_iter().find(|c| crate::identity::glob(spec, &c.identity))
}

/// Monitor thread: sends hotplug changes until `tx` is closed. udev sockets aren't `Send`,
/// so everything udev lives on this thread.
pub fn monitor(tx: tokio::sync::mpsc::UnboundedSender<Hotplug>) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("se-udev".into()).spawn(move || {
        let build = || -> std::io::Result<udev::MonitorSocket> {
            let mut b = udev::MonitorBuilder::new()?;
            for s in SUBSYSTEMS {
                b = b.match_subsystem(s)?;
            }
            b.listen()
        };
        let socket = match build() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("udev monitor failed: {e}");
                return;
            }
        };
        // syspath → tracked, so removals of untracked nodes (pcm, control, …) are ignored.
        let mut tracked: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Ok(list) = scan() {
            tracked.extend(list.into_iter().map(|d| d.syspath));
        }
        let fd = socket.as_raw_fd();
        loop {
            if tx.is_closed() {
                return;
            }
            let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            let r = unsafe { libc::poll(&mut p, 1, 1000) };
            if r <= 0 {
                continue;
            }
            for ev in socket.iter() {
                let dev = ev.device();
                let syspath = dev.syspath().to_string_lossy().into_owned();
                match ev.event_type() {
                    udev::EventType::Add | udev::EventType::Change | udev::EventType::Bind => {
                        if let Some(info) = classify(&record(&dev)) {
                            tracked.insert(syspath);
                            let _ = tx.send(Hotplug::Added(Box::new(info)));
                        }
                        // A card finishes initializing after its MIDI nodes appear: refresh them
                        // so they pick up the card's USB identity.
                        if dev.subsystem().is_some_and(|s| s == "sound") && dev.sysname().to_string_lossy().starts_with("card") {
                            for child in midi_children(&dev) {
                                tracked.insert(child.syspath.clone());
                                let _ = tx.send(Hotplug::Added(Box::new(child)));
                            }
                        }
                    }
                    udev::EventType::Remove | udev::EventType::Unbind if tracked.remove(&syspath) => {
                        let _ = tx.send(Hotplug::Removed(syspath));
                    }
                    _ => {}
                }
            }
        }
    })
}

fn midi_children(card: &udev::Device) -> Vec<DeviceInfo> {
    let Ok(mut e) = udev::Enumerator::new() else { return Vec::new() };
    if e.match_subsystem("sound").is_err() || e.match_parent(card).is_err() {
        return Vec::new();
    }
    let Ok(list) = e.scan_devices() else { return Vec::new() };
    list.filter(|d| d.sysname().to_string_lossy().starts_with("midiC")).filter_map(|d| classify(&record(&d))).collect()
}
