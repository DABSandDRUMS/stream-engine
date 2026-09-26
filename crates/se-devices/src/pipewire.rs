//! PipeWire audio nodes, listed and followed live through `pw-dump --monitor` (read-only; the
//! audio subsystem owns the graph).

use crate::identity::{DeviceInfo, Kind};
use serde_json::Value as Json;
use std::collections::{BTreeMap, HashMap};
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Debug)]
pub enum PwChange {
    /// Complete node list from a fresh connection (first dump).
    Snapshot(Vec<DeviceInfo>),
    Upsert(Box<DeviceInfo>),
    Removed(String),
    /// The connection dropped: every PipeWire node is gone until the next snapshot.
    Reset,
}

/// Tracks node objects across partial monitor updates.
#[derive(Default)]
pub struct PwState {
    /// PipeWire object id → last full props.
    props: HashMap<u64, serde_json::Map<String, Json>>,
    /// PipeWire object id → our syspath key (`pipewire:<node.name>`) for audio nodes.
    audio: HashMap<u64, String>,
}

fn s(p: &serde_json::Map<String, Json>, k: &str) -> Option<String> {
    match p.get(k)? {
        Json::String(s) if !s.is_empty() => Some(s.clone()),
        Json::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl PwState {
    /// Apply one monitor object; returns the resulting change, if any.
    pub fn apply(&mut self, obj: &Json) -> Option<PwChange> {
        let id = obj.get("id")?.as_u64()?;
        let info = obj.get("info");
        if info.is_none_or(Json::is_null) {
            self.props.remove(&id);
            return self.audio.remove(&id).map(PwChange::Removed);
        }
        let ty = obj.get("type").and_then(Json::as_str);
        if ty.is_some_and(|t| t != "PipeWire:Interface:Node") {
            return None;
        }
        if let Some(p) = info.and_then(|i| i.get("props")).and_then(Json::as_object) {
            let entry = self.props.entry(id).or_default();
            for (k, v) in p {
                entry.insert(k.clone(), v.clone());
            }
        }
        let props = self.props.get(&id)?;
        let class = s(props, "media.class")?;
        if !class.starts_with("Audio/") {
            return None;
        }
        let name = s(props, "node.name")?;
        let key = format!("pipewire:{name}");
        let mut extra = BTreeMap::new();
        extra.insert("media_class".into(), class.clone());
        for (k, dst) in
            [("audio.channels", "channels"), ("audio.rate", "rate"), ("api.alsa.path", "alsa_path"), ("object.serial", "serial"), ("device.id", "pw_device")]
        {
            if let Some(v) = s(props, k) {
                extra.insert(dst.into(), v);
            }
        }
        extra.insert("pw_id".into(), id.to_string());
        if let Some(state) = info.and_then(|i| i.get("state")).and_then(Json::as_str) {
            extra.insert("state".into(), state.to_string());
        }
        let bus = s(props, "device.bus").unwrap_or_else(|| if name.contains(".usb-") { "usb".into() } else { "pipewire".into() });
        let d = DeviceInfo {
            kind: Kind::AudioNode,
            identity: name.clone(),
            name: s(props, "node.description").or_else(|| s(props, "node.nick")).unwrap_or_else(|| name.clone()),
            path: name,
            syspath: key.clone(),
            bus,
            usb: None,
            serial: None,
            port: s(props, "device.bus-path"),
            driver: s(props, "factory.name").unwrap_or_default(),
            card: s(props, "api.alsa.card").or_else(|| s(props, "alsa.card")).unwrap_or_default(),
            extra,
        };
        self.audio.insert(id, key);
        Some(PwChange::Upsert(Box::new(d)))
    }
}

fn runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
        // SAFETY: getuid never fails.
        format!("/run/user/{}", unsafe { libc::getuid() })
    })
}

/// Follow PipeWire until `tx` closes; reconnects when PipeWire restarts.
pub fn monitor(tx: tokio::sync::mpsc::UnboundedSender<PwChange>) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("se-pw-dump".into()).spawn(move || {
        let mut warned = false;
        while !tx.is_closed() {
            let child = Command::new("pw-dump")
                .args(["--monitor", "--no-colors"])
                .env("XDG_RUNTIME_DIR", runtime_dir())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();
            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    if !warned {
                        tracing::error!("pw-dump not available ({e}); PipeWire nodes are not listed");
                        warned = true;
                    }
                    std::thread::sleep(Duration::from_secs(30));
                    continue;
                }
            };
            let Some(out) = child.stdout.take() else { continue };
            let mut state = PwState::default();
            let stream = serde_json::Deserializer::from_reader(BufReader::with_capacity(1 << 16, out)).into_iter::<Json>();
            let mut first = true;
            for batch in stream {
                let Ok(batch) = batch else { break };
                let Some(items) = batch.as_array() else { continue };
                let changes: Vec<PwChange> = items.iter().filter_map(|obj| state.apply(obj)).collect();
                let msgs = if first {
                    first = false;
                    let nodes = changes.into_iter().filter_map(|c| if let PwChange::Upsert(d) = c { Some(*d) } else { None }).collect();
                    vec![PwChange::Snapshot(nodes)]
                } else {
                    changes
                };
                for m in msgs {
                    if tx.send(m).is_err() {
                        let _ = child.kill();
                        return;
                    }
                }
            }
            let _ = child.kill();
            let status = child.wait();
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                use std::io::Read;
                let _ = e.read_to_string(&mut err);
            }
            let _ = tx.send(PwChange::Reset);
            tracing::warn!("pw-dump exited ({status:?}: {}); reconnecting in 3 s", err.trim());
            std::thread::sleep(Duration::from_secs(3));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nodes_upsert_merge_and_remove() {
        let mut st = PwState::default();
        let add = json!({"id": 138, "type": "PipeWire:Interface:Node", "info": {"state": "suspended", "props": {
            "media.class": "Audio/Source", "node.name": "alsa_input.usb-PreSonus_Studio_24c_SC1M19120661-00.analog-stereo",
            "node.description": "Studio 24c Analog Stereo", "audio.channels": 2, "api.alsa.path": "front:2", "device.bus-path": "pci-0000:0d:00.4-usb-0:2.1:1.0"}}});
        let Some(PwChange::Upsert(d)) = st.apply(&add) else { panic!() };
        let d = *d;
        assert_eq!(d.kind, Kind::AudioNode);
        assert_eq!(d.identity, "alsa_input.usb-PreSonus_Studio_24c_SC1M19120661-00.analog-stereo");
        assert_eq!(d.name, "Studio 24c Analog Stereo");
        assert_eq!(d.extra["channels"], "2");
        assert_eq!(d.bus, "usb");
        // a partial update (state only) keeps the props
        let upd = json!({"id": 138, "type": "PipeWire:Interface:Node", "info": {"state": "running"}});
        let Some(PwChange::Upsert(d2)) = st.apply(&upd) else { panic!() };
        assert_eq!(d2.identity, d.identity);
        assert_eq!(d2.extra["state"], "running");
        // removal
        let rm = json!({"id": 138, "info": null});
        let Some(PwChange::Removed(k)) = st.apply(&rm) else { panic!() };
        assert_eq!(k, format!("pipewire:{}", d.identity));
        assert!(st.apply(&rm).is_none());
    }

    #[test]
    fn non_audio_objects_are_ignored() {
        let mut st = PwState::default();
        let video = json!({"id": 42, "type": "PipeWire:Interface:Node", "info": {"props": {"media.class": "Video/Source", "node.name": "v4l2_input.pci-0000_05_00.0"}}});
        assert!(st.apply(&video).is_none());
        let dev = json!({"id": 124, "type": "PipeWire:Interface:Device", "info": {"props": {"media.class": "Audio/Device"}}});
        assert!(st.apply(&dev).is_none());
        assert!(st.apply(&json!({"id": 42, "info": null})).is_none());
    }
}
