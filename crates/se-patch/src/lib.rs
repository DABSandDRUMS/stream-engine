//! Patches (§6): hot-reloaded folders that add sources, overlays, effects, transitions, and
//! DSP. This crate owns manifests and the generated shader header shared by every kind;
//! the loader, Lua runtime, and templates build on them.

pub mod loader;
pub mod manifest;
pub mod registry;
pub mod script;
pub mod templates;
pub mod wgsl;

pub use loader::start;
pub use manifest::{Feedback, Kind, Layer, Manifest, ManifestError, ParamSpec, scan};
pub use registry::{PatchInfo, PatchSet, Patches};
