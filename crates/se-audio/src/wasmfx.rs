//! `dsp` patches (§8.6): WebAssembly modules from `patches/<id>/` compiled ahead of time on
//! load and instantiated as effects (inserts) or sources. Recompiled when `main.wasm`
//! changes; the graph swaps instances at a block boundary with a crossfade.

use crate::builder::Conv;
use se_dsp::wasm::{WASM_PARAM_SLOTS, WasmFx, WasmHost, WasmModule, WasmStatus};
use se_patch::{Kind, Manifest};
use se_proto::{Value, ValueType};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

/// A manifest param mapped onto the ABI's parameter slots.
#[derive(Clone, Debug)]
pub struct ParamSlot {
    /// `patch.<id>.<param>` suffix (component params keep the base name).
    pub param: String,
    pub slot: usize,
    pub conv: Conv,
    pub default: f32,
}

pub struct DspPatch {
    pub manifest: Manifest,
    pub module: WasmModule,
    pub stamp: (SystemTime, u64),
    pub slots: Vec<ParamSlot>,
}

pub struct DspPatches {
    host: Option<WasmHost>,
    pub host_error: Option<String>,
    pub patches: BTreeMap<String, DspPatch>,
    /// Load/compile errors by patch id (published as `patch.<id>.error`).
    pub errors: BTreeMap<String, String>,
    /// Default per-block budget (µs) when a manifest has none.
    pub default_budget_us: u32,
}

/// Map manifest params to ABI slots (BTreeMap = alphabetical order; vectors take several).
pub fn param_slots(m: &Manifest) -> Vec<ParamSlot> {
    let mut out = Vec::new();
    let mut slot = 0usize;
    for (name, spec) in &m.params {
        let n = spec.slots();
        let def = spec.default_value();
        match spec.value_type() {
            ValueType::String | ValueType::TextureRef => {}
            ValueType::Color | ValueType::Vec2 | ValueType::Vec4 => {
                for c in 0..n {
                    let d = def.as_list().and_then(|l| l.get(c)).and_then(Value::as_f32).unwrap_or(0.0);
                    out.push(ParamSlot { param: name.clone(), slot: slot + c, conv: Conv::Component(c), default: d });
                }
            }
            ValueType::Enum => {
                let d = spec.options.iter().position(|o| Some(o.as_str()) == def.as_str()).unwrap_or(0) as f32;
                out.push(ParamSlot { param: name.clone(), slot, conv: Conv::Options(spec.options.clone()), default: d });
            }
            ValueType::Bool => out.push(ParamSlot { param: name.clone(), slot, conv: Conv::Bool, default: if def.truthy() { 1.0 } else { 0.0 } }),
            _ => out.push(ParamSlot { param: name.clone(), slot, conv: Conv::Float, default: def.as_f32().unwrap_or(0.0) }),
        }
        slot += n;
    }
    out.retain(|p| p.slot < WASM_PARAM_SLOTS);
    out
}

fn stamp(p: &Path) -> Option<(SystemTime, u64)> {
    let md = std::fs::metadata(p).ok()?;
    Some((md.modified().ok()?, md.len()))
}

impl DspPatches {
    pub fn new(default_budget_us: u32) -> DspPatches {
        let (host, host_error) = match WasmHost::new() {
            Ok(h) => (Some(h), None),
            Err(e) => (None, Some(e)),
        };
        DspPatches { host, host_error, patches: BTreeMap::new(), errors: BTreeMap::new(), default_budget_us }
    }

    /// Rescan `patches/*/patch.toml` for `kind = "dsp"`; (re)compile new or changed modules.
    /// Returns the ids whose module changed (callers hot-swap their instances).
    pub fn scan(&mut self, project_root: &Path) -> Vec<String> {
        let mut changed = Vec::new();
        let mut seen = Vec::new();
        for r in se_patch::scan(project_root) {
            let m = match r {
                Ok(m) => m,
                Err(e) => {
                    // only report manifest errors for folders that look like dsp patches
                    let msg = e.to_string();
                    if msg.contains("dsp") {
                        tracing::warn!(target: "audio", "{msg}");
                    }
                    continue;
                }
            };
            if m.kind != Kind::Dsp {
                continue;
            }
            seen.push(m.id.clone());
            let path = m.entry_path();
            let Some(st) = stamp(&path) else {
                self.errors.insert(m.id.clone(), format!("missing {}", path.display()));
                self.patches.remove(&m.id);
                continue;
            };
            if let Some(p) = self.patches.get(&m.id)
                && p.stamp == st
                && p.manifest == m
            {
                continue;
            }
            let Some(host) = self.host.as_ref() else {
                self.errors.insert(m.id.clone(), format!("wasm host unavailable: {}", self.host_error.clone().unwrap_or_default()));
                continue;
            };
            let compiled = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display())).and_then(|b| host.compile(&b));
            match compiled {
                Ok(module) => {
                    let slots = param_slots(&m);
                    self.errors.remove(&m.id);
                    changed.push(m.id.clone());
                    self.patches.insert(m.id.clone(), DspPatch { manifest: m, module, stamp: st, slots });
                }
                Err(e) => {
                    // keep the last good module live
                    self.errors.insert(m.id.clone(), e);
                }
            }
        }
        self.patches.retain(|k, _| seen.contains(k));
        self.errors.retain(|k, _| seen.contains(k));
        changed
    }

    /// New instance of patch `id` (control thread; allocates).
    pub fn instantiate(&self, id: &str, sr: f32, source: bool) -> Result<(WasmFx, Arc<WasmStatus>, Vec<ParamSlot>), String> {
        let p = self.patches.get(id).ok_or_else(|| self.errors.get(id).cloned().unwrap_or_else(|| format!("no dsp patch `{id}` in patches/")))?;
        let budget = p.manifest.budget.cpu_ms.map(|ms| (ms * 1000.0) as u32).unwrap_or(self.default_budget_us).max(20);
        let fx = p.module.instantiate(sr, source, budget)?;
        let status = fx.status();
        Ok((fx, status, p.slots.clone()))
    }

    /// The manifest declares a trigger envelope (`patch.<id>` is triggerable).
    pub fn has_trigger(&self, id: &str) -> bool {
        self.patches.get(id).is_some_and(|p| p.manifest.has_trigger)
    }

    pub fn is_source(&self, id: &str) -> bool {
        self.patches.get(id).is_some_and(|p| p.manifest.layer == se_patch::Layer::AudioSource)
    }
}
