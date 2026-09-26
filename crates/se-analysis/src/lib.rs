//! Audio analysis (PLAN §8.3, §8.4, §8.6).
//!
//! * [`LiveAnalyzer`]: per-bus live features (levels, LUFS, bands, centroid, onsets per drum
//!   class, beat, drops, sections, hype), one [`Features`] frame + events per hop, allocation-free
//!   after construction (runs on the analysis thread).
//! * [`BeatTracker`]: tempo/phase tracking with tap tempo (used by the live analyzer, usable on
//!   any onset-strength curve).
//! * [`DrumTriggers`]: per-drum close-mic triggers for the real-time audio thread.
//! * [`offline`]: decode files (symphonia) and compute beat grid, downbeats, sections and chorus
//!   candidates for the song library.

// Filter banks, similarity matrices and DP tables index several parallel arrays by the same
// position; index loops are the clearest form there.
#![allow(clippy::needless_range_loop)]

mod beat;
mod drums;
mod filters;
mod live;
pub mod offline;
mod stats;
pub mod vad;

pub use beat::{BeatEvent, BeatTracker, MAX_BPM_LIMIT, MIN_BPM_LIMIT};
pub use drums::{DrumHit, DrumPadConfig, DrumTriggers};
pub use live::{AnalysisEvent, BAND_NOMINAL, Features, LiveAnalyzer, LiveConfig, N_BANDS, OnsetBand, OnsetClass, OnsetConfig, band_center};
