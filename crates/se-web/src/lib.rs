//! Web sources (§4.2, §6.1 `web`, §13.3): web patches and the YouTube player page, rendered
//! off-screen by Chromium (CEF) into the engine's CPU video slots, with their audio in audio
//! slots.
//!
//! The engine never links libcef (it runs without CEF installed). [`start`] supervises the
//! separate `stream-engine-web` host process (crate `se-web-host`):
//!
//! ```text
//!  engine (se-web)                               stream-engine-web (CEF, --ozone-platform=headless)
//!  ─────────────────                             ─────────────────────────────────────────────
//!  supervisor task ── ToHost (SEQPACKET, JSON) ─▶ IPC thread → UI-thread tasks (open/resize/…)
//!  IPC thread ◀─ FromHost + memfds (SCM_RIGHTS) ─ on_paint → sealed memfd frame slots (BGRA)
//!    frame slot ─copy─▶ hub.video[<slot>]          AudioHandler → shm SPSC f32 ring (48 kHz ×2)
//!    audio ring ─copy─▶ hub.audio[<slot>]          renderer crash → report + reload with backoff
//! ```
//!
//! * Sources: every enabled `kind = "web"` patch (slot `patch.<id>`, URL
//!   `/patches/<id>/<entry>?token=…`) and the YouTube player page (slot `youtube`,
//!   `/web/player.html?token=…`, `[web.youtube]` overrides). Each page gets a random API token
//!   scoped to its own `patch.<id>.*` namespace (§19), removed when the source closes.
//! * Size follows the scene nodes showing the slot (largest node; overlays: largest canvas;
//!   manifest `size` wins), rounded to even, resized live on config changes.
//! * Status: `patch.<id>.error`, `web.<slot>.{fps,crashes,status,error}`, `health.cef`, the `web`
//!   query; actions `web.reload [slot]` and `web.login [url]` (windowed sign-in browser on the
//!   persistent profile, §13.3).
//! * Isolation: a renderer crash reloads the page; a host crash restarts the host with backoff
//!   while the slots keep their last frame; nothing here can take the engine down.

pub mod protocol;

#[cfg(feature = "engine")]
mod host;
#[cfg(feature = "engine")]
mod media;
#[cfg(feature = "engine")]
pub mod service;
#[cfg(feature = "engine")]
pub mod sources;

#[cfg(feature = "engine")]
pub use host::{HostPaths, INSTALL_HINT, find_host};
#[cfg(feature = "engine")]
pub use service::{start, start_with};
