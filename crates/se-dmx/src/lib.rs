//! Lights (§9): fixture profiles, rig patch, programmer, palettes, cue lists, effects, the
//! output-stage safety limiter, and DMX output (ENTTEC DMX USB PRO, sACN, Art-Net) with RDM.
//!
//! Every fixture attribute is a state address (`lights.<head>.<attr>`, groups under
//! `lights.group.<g>.<attr>`); cue list playbacks write overrides keyed `cuelist:<cl>` so
//! intensity merges HTP and everything else LTP through the core resolver. The output thread
//! reads the resolved snapshot, applies effects, masters, caps and the flash limiter, and
//! sends DMX at up to 44 Hz. Start with [`start`].

pub mod control;
pub mod cuelist;
pub mod effects;
pub mod engine;
pub mod limiter;
pub mod output;
pub mod palette;
pub mod playback;
pub mod profile;
pub mod programmer;
pub mod query;
pub mod rig;
pub mod show;

pub use control::{Lights, run, start, stop};

#[cfg(test)]
#[global_allocator]
static ALLOC: se_alloc::Counting = se_alloc::Counting;
