//! Device registry (§3.5): udev hotplug for V4L2 cameras/capture cards, ALSA cards and MIDI
//! ports, hidraw, and USB serial; PipeWire audio nodes; stable identities; expected devices
//! from `project.toml [devices.expected]` with preflight (`health.devices`).
//!
//! Published state: `devices.<id>.{present,name,kind,path,identity}` where `<id>` is the
//! expected-entry key, or a slug of the identity for devices nobody declared.
//! Events: `devices.added` / `devices.removed` `{id, kind, name, identity, path}`.
//! Query: `devices` (full registry incl. camera modes). Actions: `devices.rename`,
//! `devices.expect`, `devices.forget`, `devices.rescan`.

pub mod expected;
pub mod identity;
pub mod pipewire;
pub mod registry;
pub mod udev_scan;
pub mod v4l2;

pub use identity::{DeviceInfo, Kind};
pub use udev_scan::{find_camera, scan_cameras};

use anyhow::{Context, Result, anyhow};
use parking_lot::Mutex;
use registry::{Published, Registry};
use se_hub::{EngineCtx, Hub};
use se_proto::{Event, Meta, Origin, Value, ValueType};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::mpsc;

const TARGET: &str = "devices";

struct Shared {
    reg: Mutex<Registry>,
}

/// Start the registry: initial scan, hotplug monitors, publishing, query and actions.
pub async fn start(ctx: EngineCtx) -> Result<()> {
    let initial = tokio::task::spawn_blocking(udev_scan::scan).await?.context("udev scan")?;
    let shared = Arc::new(Shared { reg: Mutex::new(Registry::default()) });
    {
        let mut reg = shared.reg.lock();
        for d in initial {
            reg.devices.insert(d.syspath.clone(), d);
        }
        reg.expected = load_expected(&ctx, &mut None);
    }
    let (hp_tx, hp_rx) = mpsc::unbounded_channel();
    udev_scan::monitor(hp_tx).context("udev monitor")?;
    let (pw_tx, pw_rx) = mpsc::unbounded_channel();
    pipewire::monitor(pw_tx).context("pw-dump monitor")?;

    register_query(&ctx, shared.clone());
    let actions = ctx.hub.route_actions("devices");
    let task = Task { ctx: ctx.clone(), shared, published: BTreeMap::new(), health: None, pw_ready: false, expected_errors: None };
    tokio::spawn(task.run(hp_rx, pw_rx, actions));
    Ok(())
}

fn load_expected(ctx: &EngineCtx, last_errors: &mut Option<Vec<String>>) -> Vec<expected::Expected> {
    let section = ctx.project_section("devices");
    let (list, errors) = expected::parse(section.as_ref());
    if last_errors.as_ref() != Some(&errors) {
        for e in &errors {
            ctx.hub.log("error", TARGET, format!("project.toml: {e}"));
        }
        *last_errors = Some(errors);
    }
    list
}

fn register_query(ctx: &EngineCtx, shared: Arc<Shared>) {
    let ctx2 = ctx.clone();
    ctx.hub.register_query(
        "devices",
        Arc::new(move |_name, _args| {
            let shared = shared.clone();
            let ctx = ctx2.clone();
            Box::pin(async move {
                let cams: Vec<String> = shared.reg.lock().devices.values().filter(|d| d.kind == Kind::Camera).map(|d| d.path.clone()).collect();
                let inputs = tokio::task::spawn_blocking(move || {
                    cams.into_iter()
                        .filter_map(|p| v4l2::Device::open(&p, true).and_then(|d| d.input_status()).ok().map(|st| (p, st)))
                        .collect::<HashMap<_, _>>()
                })
                .await
                .unwrap_or_default();
                let sources = source_devices(&ctx, &shared.reg.lock());
                Ok(shared.reg.lock().to_value(&sources, &inputs))
            })
        }),
    );
}

/// Device identity → name of the source (`sources/<name>.toml`) that uses it.
fn source_devices(ctx: &EngineCtx, reg: &Registry) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (name, t) in ctx.kind("sources") {
        let Some(spec) = t.get("device").and_then(toml::Value::as_str) else { continue };
        let canon = spec.starts_with('/').then(|| std::fs::canonicalize(spec).ok()).flatten();
        let hit = reg.devices.values().filter(|d| d.kind == Kind::Camera).find(|d| match &canon {
            Some(c) => c.to_string_lossy() == d.path,
            None => d.identity == spec || identity::glob(spec, &d.identity),
        });
        if let Some(d) = hit {
            out.insert(d.identity.clone(), name);
        }
    }
    out
}

struct Task {
    ctx: EngineCtx,
    shared: Arc<Shared>,
    published: BTreeMap<String, Published>,
    health: Option<(String, String)>,
    /// Suppress added/removed events for the first PipeWire snapshot (startup).
    pw_ready: bool,
    expected_errors: Option<Vec<String>>,
}

impl Task {
    async fn run(
        mut self,
        mut hp: mpsc::UnboundedReceiver<udev_scan::Hotplug>,
        mut pw: mpsc::UnboundedReceiver<pipewire::PwChange>,
        mut actions: mpsc::UnboundedReceiver<se_proto::Command>,
    ) {
        let mut config = self.ctx.config.clone();
        self.refresh_camera_details(None).await;
        self.sync(&[]);
        loop {
            tokio::select! {
                Some(h) = hp.recv() => {
                    let changes = self.apply_hotplug(h);
                    for (_, d) in changes.iter().filter(|(added, d)| *added && d.kind == Kind::Camera) {
                        self.refresh_camera_details(Some(d.clone())).await;
                    }
                    self.sync(&changes);
                }
                Some(c) = pw.recv() => {
                    let changes = self.apply_pw(c);
                    self.sync(&changes);
                }
                Ok(()) = config.changed() => {
                    let list = load_expected(&self.ctx, &mut self.expected_errors);
                    self.shared.reg.lock().expected = list;
                    self.sync(&[]);
                }
                Some(cmd) = actions.recv() => {
                    if let se_proto::Op::Action { name, args } = &cmd.op {
                        match self.action(name, args).await {
                            Ok(msg) => self.ctx.hub.log("info", TARGET, msg),
                            Err(e) => self.ctx.hub.log("error", TARGET, format!("{name}: {e}")),
                        }
                    }
                }
                else => break,
            }
        }
    }

    /// Returns (added?, device) for real presence changes.
    fn apply_hotplug(&mut self, h: udev_scan::Hotplug) -> Vec<(bool, DeviceInfo)> {
        let mut reg = self.shared.reg.lock();
        match h {
            udev_scan::Hotplug::Added(d) => {
                let d = *d;
                let key = d.syspath.clone();
                let prev = reg.devices.insert(key, d.clone());
                match prev {
                    None => vec![(true, d)],
                    // identity became known (e.g. a card finished initializing): treat as re-add
                    Some(p) if p.identity != d.identity => vec![(false, p), (true, d)],
                    Some(_) => Vec::new(),
                }
            }
            udev_scan::Hotplug::Removed(key) => reg.devices.remove(&key).map(|d| vec![(false, d)]).unwrap_or_default(),
        }
    }

    fn apply_pw(&mut self, c: pipewire::PwChange) -> Vec<(bool, DeviceInfo)> {
        let mut reg = self.shared.reg.lock();
        let mut out = Vec::new();
        match c {
            pipewire::PwChange::Snapshot(nodes) => {
                let old: Vec<String> = reg.devices.iter().filter(|(_, d)| d.kind == Kind::AudioNode).map(|(k, _)| k.clone()).collect();
                let new_keys: std::collections::HashSet<&str> = nodes.iter().map(|d| d.syspath.as_str()).collect();
                for k in old {
                    if !new_keys.contains(k.as_str())
                        && let Some(d) = reg.devices.remove(&k)
                    {
                        out.push((false, d));
                    }
                }
                for d in nodes {
                    if reg.devices.insert(d.syspath.clone(), d.clone()).is_none() {
                        out.push((true, d));
                    }
                }
                if !self.pw_ready {
                    self.pw_ready = true;
                    out.clear();
                }
            }
            pipewire::PwChange::Upsert(d) => {
                let d = *d;
                if reg.devices.insert(d.syspath.clone(), d.clone()).is_none() {
                    out.push((true, d));
                }
            }
            pipewire::PwChange::Removed(k) => {
                if let Some(d) = reg.devices.remove(&k) {
                    out.push((false, d));
                }
            }
            pipewire::PwChange::Reset => {
                let keys: Vec<String> = reg.devices.iter().filter(|(_, d)| d.kind == Kind::AudioNode).map(|(k, _)| k.clone()).collect();
                for k in keys {
                    if let Some(d) = reg.devices.remove(&k) {
                        out.push((false, d));
                    }
                }
            }
        }
        out
    }

    /// Read modes/controls for one camera (or all when `only` is None) off the async runtime.
    async fn refresh_camera_details(&mut self, only: Option<DeviceInfo>) {
        let targets: Vec<(String, String)> = match only {
            Some(d) => vec![(d.identity, d.path)],
            None => self.shared.reg.lock().devices.values().filter(|d| d.kind == Kind::Camera).map(|d| (d.identity.clone(), d.path.clone())).collect(),
        };
        let res = tokio::task::spawn_blocking(move || targets.into_iter().map(|(id, path)| (id, registry::camera_details(&path))).collect::<Vec<_>>())
            .await
            .unwrap_or_default();
        let mut reg = self.shared.reg.lock();
        for (id, det) in res {
            if let Some(e) = &det.error {
                tracing::warn!("camera {id}: {e}");
            }
            reg.details.insert(id, det);
        }
    }

    /// Publish differences and emit hotplug events.
    fn sync(&mut self, changes: &[(bool, DeviceInfo)]) {
        let hub: &Hub = &self.ctx.hub;
        let (now, health, ids) = {
            let reg = self.shared.reg.lock();
            let ids: HashMap<String, String> = reg.rows().into_iter().filter_map(|r| r.device.map(|d| (d.syspath.clone(), r.id))).collect();
            (reg.published(), reg.health(), ids)
        };
        for (id, p) in &now {
            let old = self.published.get(id);
            if old == Some(p) {
                continue;
            }
            let base = format!("devices.{id}");
            if old.is_none() {
                hub.declare(&format!("{base}.present"), Meta::boolean(false).readonly().owner(TARGET).describe("device is connected"));
                hub.declare(&format!("{base}.name"), Meta::string("").readonly().owner(TARGET));
                hub.declare(&format!("{base}.kind"), Meta::string("").readonly().owner(TARGET));
                hub.declare(&format!("{base}.path"), Meta::string("").readonly().owner(TARGET).describe("device node or PipeWire node name"));
                hub.declare(&format!("{base}.identity"), Meta::string("").readonly().owner(TARGET).describe("stable identity (udev by-id/by-path name)"));
            }
            let o = old.cloned().unwrap_or_default();
            if old.is_none() || o.present != p.present {
                hub.publish(&format!("{base}.present"), Value::Bool(p.present));
            }
            for (field, a, b) in [("name", &o.name, &p.name), ("kind", &o.kind, &p.kind), ("path", &o.path, &p.path), ("identity", &o.identity, &p.identity)] {
                if old.is_none() || a != b {
                    hub.publish(&format!("{base}.{field}"), Value::Str(b.clone()));
                }
            }
        }
        for id in self.published.keys() {
            if !now.contains_key(id) {
                hub.submit(se_core::Input::Remove { prefix: format!("devices.{id}") });
            }
        }
        self.published = now;

        let h = (health.0.to_string(), health.1);
        if self.health.as_ref() != Some(&h) {
            if self.health.is_none() {
                hub.declare(
                    "health.devices",
                    Meta {
                        ty: ValueType::Map,
                        readonly: true,
                        owner: Some(TARGET.into()),
                        description: Some("expected devices present".into()),
                        ..Default::default()
                    },
                );
            }
            hub.publish("health.devices", Value::map().with("status", h.0.as_str()).with("detail", h.1.as_str()));
            self.health = Some(h);
        }

        for (added, d) in changes {
            let id = ids.get(&d.syspath).cloned().unwrap_or_else(|| identity::slug(d.kind, &d.identity));
            let ty = if *added { "devices.added" } else { "devices.removed" };
            let payload = Value::map()
                .with("id", id.as_str())
                .with("kind", d.kind.as_str())
                .with("name", d.name.as_str())
                .with("identity", d.identity.as_str())
                .with("path", d.path.as_str());
            tracing::info!("{ty}: {} ({}) {}", d.name, d.identity, d.path);
            hub.emit(Event::new(ty, Origin::System, payload));
        }
    }

    async fn action(&mut self, name: &str, args: &Value) -> Result<String> {
        let arg = |key: &str, pos: usize| -> Option<String> {
            args.get_path(key).or_else(|| args.get_path("args").and_then(|a| a.as_list()).and_then(|l| l.get(pos))).map(|v| match v {
                Value::Str(s) => s.clone(),
                other => other.to_string(),
            })
        };
        match name {
            "devices.rescan" => {
                let list = tokio::task::spawn_blocking(udev_scan::scan).await??;
                let changes = {
                    let mut reg = self.shared.reg.lock();
                    let mut changes = Vec::new();
                    let keep: std::collections::HashSet<String> = list.iter().map(|d| d.syspath.clone()).collect();
                    let gone: Vec<String> =
                        reg.devices.iter().filter(|(k, d)| d.kind != Kind::AudioNode && !keep.contains(*k)).map(|(k, _)| k.clone()).collect();
                    for k in gone {
                        if let Some(d) = reg.devices.remove(&k) {
                            changes.push((false, d));
                        }
                    }
                    for d in list {
                        if reg.devices.insert(d.syspath.clone(), d.clone()).is_none() {
                            changes.push((true, d));
                        }
                    }
                    changes
                };
                self.refresh_camera_details(None).await;
                self.sync(&changes);
                Ok(format!("rescan: {} change(s)", changes.len()))
            }
            "devices.rename" => {
                let id = arg("id", 0).ok_or_else(|| anyhow!("needs id"))?;
                let label = arg("label", 1).ok_or_else(|| anyhow!("needs label"))?;
                let existing = self.shared.reg.lock().expected.iter().any(|e| e.id == id);
                if existing {
                    edit_expected(&self.ctx, |t| {
                        let item = t.get_mut(&id).ok_or_else(|| anyhow!("no expected device `{id}`"))?;
                        let tl = item.as_table_like_mut().ok_or_else(|| anyhow!("devices.expected.{id} is not a table"))?;
                        tl.insert("label", toml_edit::value(label.as_str()));
                        Ok(())
                    })?;
                    return Ok(format!("renamed {id} → {label}"));
                }
                let d = self.device_by_id(&id).ok_or_else(|| anyhow!("unknown device `{id}`"))?;
                let new_id = self.expect_device(&d, None, Some(&label))?;
                Ok(format!("{} is now expected as {new_id} ({label})", d.identity))
            }
            "devices.expect" => {
                let ident = arg("identity", 0).ok_or_else(|| anyhow!("needs identity"))?;
                let d = self
                    .shared
                    .reg
                    .lock()
                    .devices
                    .values()
                    .find(|d| d.identity == ident)
                    .cloned()
                    .ok_or_else(|| anyhow!("no present device with identity `{ident}`"))?;
                let new_id = self.expect_device(&d, arg("id", 1).as_deref(), arg("label", 2).as_deref())?;
                Ok(format!("{ident} is now expected as {new_id}"))
            }
            "devices.forget" => {
                let id = arg("id", 0).ok_or_else(|| anyhow!("needs id"))?;
                edit_expected(&self.ctx, |t| t.remove(&id).map(|_| ()).ok_or_else(|| anyhow!("no expected device `{id}`")))?;
                Ok(format!("{id} is no longer expected"))
            }
            other => Err(anyhow!("unknown action `{other}` (devices.rescan|rename|expect|forget)")),
        }
    }

    fn device_by_id(&self, id: &str) -> Option<DeviceInfo> {
        let reg = self.shared.reg.lock();
        reg.rows().into_iter().find(|r| r.id == id).and_then(|r| r.device.cloned())
    }

    /// Add a present device to `[devices.expected]`; returns the new entry id.
    fn expect_device(&self, d: &DeviceInfo, id: Option<&str>, label: Option<&str>) -> Result<String> {
        let label = label.map(str::to_string).unwrap_or_else(|| d.name.clone());
        let taken: Vec<String> = self.shared.reg.lock().expected.iter().map(|e| e.id.clone()).collect();
        let base = id.map(str::to_string).unwrap_or_else(|| label_id(&label));
        if base.is_empty() || !base.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(anyhow!("invalid id `{base}`"));
        }
        let mut new_id = base.clone();
        let mut n = 2;
        while taken.contains(&new_id) {
            new_id = format!("{base}_{n}");
            n += 1;
        }
        let mut entry = toml_edit::InlineTable::new();
        entry.insert("kind", d.kind.as_str().into());
        entry.insert("label", label.as_str().into());
        entry.insert("identity", d.identity.as_str().into());
        let key = new_id.clone();
        edit_expected(&self.ctx, move |t| {
            t.insert(&key, toml_edit::value(entry));
            Ok(())
        })?;
        Ok(new_id)
    }
}

/// `"Kit cam (HDMI 1)"` → `kit_cam_hdmi_1`.
fn label_id(label: &str) -> String {
    let mut s = String::new();
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            s.push(ch.to_ascii_lowercase());
        } else if !s.is_empty() && !s.ends_with('_') {
            s.push('_');
        }
    }
    s.trim_end_matches('_').to_string()
}

/// Edit `project.toml [devices.expected]` with comments preserved (the watcher reloads it).
fn edit_expected(ctx: &EngineCtx, f: impl FnOnce(&mut toml_edit::Table) -> Result<()>) -> Result<()> {
    let project = se_store::Project::open(&ctx.project_root)?;
    project.edit("project.toml", |doc| {
        let devices = doc.entry("devices").or_insert_with(|| {
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            toml_edit::Item::Table(t)
        });
        let dt = devices.as_table_mut().ok_or_else(|| anyhow!("[devices] is not a table"))?;
        let exp = dt.entry("expected").or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        let et = exp.as_table_mut().ok_or_else(|| anyhow!("[devices.expected] is not a table"))?;
        f(et)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_ids() {
        assert_eq!(label_id("Kit cam (HDMI 1)"), "kit_cam_hdmi_1");
        assert_eq!(label_id("  X-TOUCH MINI "), "x_touch_mini");
        assert_eq!(label_id("!!!"), "");
    }
}
