//! Loader thread: compiles user shaders (shader/particles patches, project transitions) and
//! decodes assets (LUTs, masks) off the render thread (§3.4, §21). A failed compile publishes the
//! error with the patch source line and sends nothing, so the renderer keeps the last good
//! pipeline; after a device loss the last good sources are recompiled for the new device.

use crate::lut::Lut;
use crate::pipelines::{UserPipes, compile_fullscreen, compile_particles, transition_manifest};
use crate::plan::{Plan, ShaderSource, TrKind};
use crate::renderer::{AssetRequest, Msg, PipeKey};
use crate::resources::{Layouts, WINDOW};
use crossbeam_channel::{Receiver, Sender};
use se_patch::Kind;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

pub enum LoaderCmd {
    Device {
        device: wgpu::Device,
        layouts: Arc<Layouts>,
        generation: u64,
    },
    Plan(Arc<Plan>),
    /// Source files changed (or a manual reload): recompile what differs.
    Recompile,
    Asset(AssetRequest),
    Shutdown,
}

/// Errors and results reported to the IO side (published as state/logs).
pub trait Report: Send {
    /// `Ok(())` clears the error.
    fn patch(&self, id: &str, result: Result<(), String>);
    fn transition(&self, name: &str, result: Result<(), String>);
}

/// One compile unit: sources + the layout its header comes from.
#[derive(Clone)]
struct Unit {
    key: PipeKey,
    layout: se_patch::wgsl::Layout,
    kind: UnitKind,
    /// File names for error messages + contents.
    sources: Vec<(String, String)>,
}

#[derive(Clone)]
enum UnitKind {
    Fullscreen,
    Particles(Arc<se_patch::Manifest>),
}

fn digest(u: &Unit) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    u.layout.header().hash(&mut h);
    for (n, s) in &u.sources {
        n.hash(&mut h);
        s.hash(&mut h);
    }
    h.finish()
}

/// Paths inside the project only (no absolute paths, no `..`).
pub fn project_path(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let p = Path::new(rel);
    if p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("`{rel}` must be a path inside the project"));
    }
    Ok(root.join(p))
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn units(plan: &Plan) -> Vec<Result<Unit, (PipeKey, String)>> {
    let mut out = Vec::new();
    for pp in &plan.patches {
        let m = &pp.manifest;
        let key = PipeKey::Patch(m.id.clone());
        if pp.layout.byte_size() > WINDOW {
            out.push(Err((key, format!("patch.toml: too many params/signals for the uniform block ({} bytes > {WINDOW})", pp.layout.byte_size()))));
            continue;
        }
        let unit = match m.kind {
            Kind::Shader => read(&m.entry_path()).map(|s| Unit {
                key: key.clone(),
                layout: pp.layout.clone(),
                kind: UnitKind::Fullscreen,
                sources: vec![(m.entry.clone(), s)],
            }),
            Kind::Particles => {
                let spec = m.particles.clone().unwrap_or_default();
                let sim = if spec.sim.is_empty() { m.entry.clone() } else { spec.sim.clone() };
                let draw = if spec.draw.is_empty() { "draw.wgsl".to_string() } else { spec.draw.clone() };
                read(&m.dir.join(&sim)).and_then(|s| read(&m.dir.join(&draw)).map(|d| (s, d))).map(|(s, d)| Unit {
                    key: key.clone(),
                    layout: pp.layout.clone(),
                    kind: UnitKind::Particles(m.clone()),
                    sources: vec![(sim, s), (draw, d)],
                })
            }
            _ => continue,
        };
        out.push(unit.map_err(|e| (key, e)));
    }
    for tp in &plan.transitions {
        if !matches!(tp.kind, TrKind::Shader | TrKind::Combined) {
            continue;
        }
        let Some(ShaderSource::File(rel)) = &tp.shader else { continue };
        let key = PipeKey::Transition(tp.name.clone());
        let unit = project_path(&plan.project_root, &rel.to_string_lossy()).and_then(|p| read(&p)).map(|s| Unit {
            key: key.clone(),
            layout: se_patch::wgsl::Layout::new(&transition_manifest(&tp.name, &tp.params)),
            kind: UnitKind::Fullscreen,
            sources: vec![(rel.to_string_lossy().to_string(), s)],
        });
        out.push(unit.map_err(|e| (key, e)));
    }
    out
}

fn compile(device: &wgpu::Device, layouts: &Layouts, u: &Unit) -> Result<UserPipes, String> {
    match &u.kind {
        UnitKind::Fullscreen => compile_fullscreen(device, layouts, &u.sources[0].0, &u.layout, &u.sources[0].1),
        UnitKind::Particles(m) => compile_particles(device, layouts, m, &u.layout, &u.sources[0].1, &u.sources[1].1),
    }
}

pub struct Loader {
    rx: Receiver<LoaderCmd>,
    out: Sender<Msg>,
    report: Box<dyn Report>,
    device: Option<(wgpu::Device, Arc<Layouts>, u64)>,
    plan: Option<Arc<Plan>>,
    /// Digest of what is live on the current device per key.
    live: HashMap<PipeKey, u64>,
    /// Last source that compiled, per key (recompiled after a device loss).
    last_good: HashMap<PipeKey, Unit>,
}

impl Loader {
    pub fn spawn(rx: Receiver<LoaderCmd>, out: Sender<Msg>, report: Box<dyn Report>) -> std::io::Result<std::thread::JoinHandle<()>> {
        std::thread::Builder::new().name("se-render-loader".into()).spawn(move || {
            let mut l = Loader { rx, out, report, device: None, plan: None, live: HashMap::new(), last_good: HashMap::new() };
            l.run();
        })
    }

    fn run(&mut self) {
        while let Ok(cmd) = self.rx.recv() {
            match cmd {
                LoaderCmd::Device { device, layouts, generation } => {
                    self.device = Some((device, layouts, generation));
                    self.live.clear();
                    // restore last good pipelines first (a broken current source must not
                    // leave the new device without the old version)
                    let good: Vec<Unit> = self.last_good.values().cloned().collect();
                    for u in good {
                        self.build(&u, false);
                    }
                    self.sync();
                }
                LoaderCmd::Plan(p) => {
                    self.plan = Some(p);
                    self.sync();
                }
                LoaderCmd::Recompile => self.sync(),
                LoaderCmd::Asset(a) => self.asset(a),
                LoaderCmd::Shutdown => break,
            }
        }
    }

    fn sync(&mut self) {
        let Some(plan) = self.plan.clone() else { return };
        for u in units(&plan) {
            match u {
                Ok(u) => {
                    if self.live.get(&u.key) != Some(&digest(&u)) {
                        self.build(&u, true);
                    }
                }
                Err((key, e)) => self.report(&key, Err(e)),
            }
        }
    }

    fn report(&self, key: &PipeKey, r: Result<(), String>) {
        match key {
            PipeKey::Patch(id) => self.report.patch(id, r),
            PipeKey::Transition(n) => self.report.transition(n, r),
        }
    }

    fn build(&mut self, u: &Unit, report: bool) {
        let Some((device, layouts, generation)) = &self.device else { return };
        let d = digest(u);
        match compile(device, layouts, u) {
            Ok(p) => {
                let _ = self.out.send(Msg::Pipes { key: u.key.clone(), device_gen: *generation, layout: u.layout.clone(), pipes: Arc::new(p) });
                self.live.insert(u.key.clone(), d);
                self.last_good.insert(u.key.clone(), u.clone());
                if report {
                    self.report(&u.key, Ok(()));
                }
            }
            Err(e) => {
                // remember the failure so an unchanged broken file is not recompiled every sync
                self.live.insert(u.key.clone(), d);
                if report {
                    self.report(&u.key, Err(e));
                }
            }
        }
    }

    fn asset(&self, a: AssetRequest) {
        let Some(plan) = &self.plan else { return };
        match a {
            AssetRequest::Lut(path) => {
                let lut = project_path(&plan.project_root, &path).and_then(|p| Lut::load(&p));
                let _ = self.out.send(Msg::Lut { path, lut });
            }
            AssetRequest::Mask(path) => {
                let mask = project_path(&plan.project_root, &path).and_then(|p| load_mask(&p));
                let _ = self.out.send(Msg::Mask { path, mask });
            }
        }
    }
}

/// Decode a mask image (white = visible) to RGBA8, at most 4096 px per side.
pub fn load_mask(path: &Path) -> Result<(u32, u32, Vec<u8>), String> {
    let img = image::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let img = if img.width() > 4096 || img.height() > 4096 { img.resize(4096, 4096, image::imageops::FilterType::Triangle) } else { img };
    let rgba = img.to_rgba8();
    Ok((rgba.width(), rgba.height(), rgba.into_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_paths_stay_inside() {
        let root = Path::new("/p");
        assert_eq!(project_path(root, "assets/a.cube").unwrap(), PathBuf::from("/p/assets/a.cube"));
        assert!(project_path(root, "../x").is_err());
        assert!(project_path(root, "/etc/passwd").is_err());
        assert!(project_path(root, "assets/../../x").is_err());
    }
}
