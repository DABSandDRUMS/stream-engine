//! Patches (§6): hot-reloaded folders that add sources, overlays, effects, transitions, and
//! DSP. This crate owns manifests and the generated shader header shared by every kind;
//! the loader, Lua runtime, and templates build on them.

pub mod manifest;
pub mod wgsl;

pub use manifest::{Kind, Layer, Manifest, ManifestError, ParamSpec, scan};
