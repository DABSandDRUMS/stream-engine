//! stream-engine core: state tree, provenance, command processing, events, signals, rules,
//! bindings, presets, modes, triggers, scenes, and the simulator. Deterministic and
//! single-threaded; the engine runs it on its own thread at a fixed tick.

pub mod bindings;
pub mod config;
pub mod core;
pub mod rng;
pub mod signals;
pub mod sim;
pub mod state;
pub mod trace;
pub mod triggers;

pub use crate::core::{Core, Input, Output, RuntimeState, addr};
pub use config::{Config, SourceFile};
