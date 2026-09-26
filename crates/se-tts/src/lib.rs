//! Text-to-speech (PLAN §14.4): Kokoro-82M v1.0 (Apache-2.0 weights) on ONNX Runtime (CPU,
//! statically linked by `ort`), phonemes from `espeak-ng` run as a separate process (GPL-3.0,
//! never linked), audio to the `tts` slot (48 kHz mono → `tts` bus, which ducks music).
//!
//! Engine surface ([`start`]):
//! * config `[tts]` in `project.toml`: see [`config`].
//! * actions: `tts.say {text, id?, voice?, kind?, user?, user_id?, message_id?, amount?, tier?}`
//!   (policy-filtered text, sent after the veto window; `voice` = explicit override),
//!   `tts.skip {id?}` (no id = stop the current item), `tts.clear`, `tts.model.fetch`.
//! * state: `tts.enabled` (user toggle), `tts.speaking`, `tts.queue`,
//!   `tts.current.{id,user,voice,text}`, `tts.model.status`, `tts.ready`; `health.tts`.
//! * events: `tts.started {id, user, voice, duration_ms}`, `tts.finished {id, skipped, error?}`
//!   (also for requests dropped before playing).
//! * query `tts` → `{ready, enabled, current, queue, voices, voice, model_status}`.
//! * deletion sync: `twitch.user.purge {user_id, user}` drops that user's items (and stops
//!   their current one), `twitch.chat.delete {message_id}` drops the item from that message,
//!   `twitch.chat.clear` drops the queue.

pub mod config;
pub mod kokoro;
pub mod phonemize;
pub mod resample;
mod service;
pub mod text;
pub mod vocab;

pub use service::{INTRA_THREADS, start};
