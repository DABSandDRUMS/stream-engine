//! `se-mixer`: the PreSonus StudioLive III (16R) adapter (PLAN §8.7).
//!
//! * [`ucnet`]: the protocol — a Rust port of the MIT-licensed reverse-engineered reference
//!   [featherbear/presonus-studiolive-api](https://github.com/featherbear/presonus-studiolive-api)
//!   (see `LICENSE-THIRD-PARTY`).
//! * [`map`]: engine addresses (`mixer.16r.*`) ↔ console parameters, measured coverage.
//! * [`sync`]: two-way sync (echo suppression, latest-value-wins rate limit).
//! * [`snapshots`]: `mixes/*.toml` store / recall / crossfade.
//! * [`talk`]: `mic.talking` from the vocal channel meter.
//! * [`service`]: the subsystem started by the engine.

pub mod config;
pub mod map;
pub mod service;
pub mod snapshots;
pub mod sync;
pub mod talk;
pub mod ucnet;

pub use service::start;
