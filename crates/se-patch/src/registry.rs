//! The live set of patches published by the loader to the runtimes that host non-script
//! kinds (`se-web` for `web`, `se-render` for `shader`/`particles`, `se-audio` for `dsp`).
//!
//! Ownership of the per-patch status addresses:
//! * the loader declares `patch.<id>.<param>`, the trigger `patch.<id>`, `patch.<id>.error`,
//!   and `patch.<id>.state` for every kind;
//! * `script` patches: the loader publishes both `error` and `state`;
//! * other kinds: the hosting runtime publishes `patch.<id>.error` (`""` when healthy) and the
//!   loader derives `patch.<id>.state` from it; the loader writes `error` itself only for
//!   manifest failures (and clears only what it wrote).

use crate::manifest::Manifest;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::watch;

/// One loaded patch (the last manifest that parsed).
#[derive(Clone, Debug)]
pub struct PatchInfo {
    pub manifest: Arc<Manifest>,
    /// `false` after `patch.disable` (persisted across restarts); hosts must stop output.
    pub enabled: bool,
    /// Bumps whenever any file in the patch folder changes (debounced) or on `patch.reload`;
    /// hosts reload the patch when it changes.
    pub generation: u64,
}

/// Patches by id, in id order.
pub type PatchSet = BTreeMap<String, PatchInfo>;

/// Handle returned by `se_patch::start`; clone freely.
#[derive(Clone)]
pub struct Patches {
    pub(crate) rx: watch::Receiver<Arc<PatchSet>>,
}

impl Patches {
    /// Receiver of the current set; `changed()` fires on every add/remove/reload/enable.
    pub fn subscribe(&self) -> watch::Receiver<Arc<PatchSet>> {
        self.rx.clone()
    }

    pub fn current(&self) -> Arc<PatchSet> {
        self.rx.borrow().clone()
    }
}

/// Values of `patch.<id>.state`.
pub mod state {
    pub const LOADED: &str = "loaded";
    pub const ERROR: &str = "error";
    pub const SUSPENDED: &str = "suspended";
    pub const DISABLED: &str = "disabled";
}
