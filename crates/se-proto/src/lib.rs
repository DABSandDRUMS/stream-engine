//! Shared vocabulary of stream-engine: addresses, values, metadata, events, commands, and
//! the API wire format. Every other crate speaks these types.

pub mod address;
pub mod command;
pub mod palette;
pub mod types;
pub mod value;
pub mod wire;

pub use command::{Command, Ease, Op, ParseError, parse_duration_ms, parse_ms};
pub use types::*;
pub use value::Value;
