//! `frames.sock`: zero-copy canvas frames from the engine to the OBS plugin and the UI.
//!
//! The wire contract is `docs/frames-protocol.md`. This crate provides the message codec
//! ([`proto`]), the engine-side [`FramesServer`], the [`FramesClient`] library, shm
//! buffers for the CPU fallback ([`ShmBuffer`], [`ShmView`]), the libc plumbing ([`sys`])
//! and the validating `se-frames-client` binary.

pub mod client;
pub mod proto;
pub mod server;
pub mod shm;
pub mod sys;

use std::ffi::OsString;
use std::path::PathBuf;

pub use client::{ClientMsg, FramesClient, wait_fence};
pub use proto::{CanvasMsg, FrameMsg, Goodbye, Hello, ProtoError, Release};
pub use server::{Demand, FramesServer, RingDesc, RingKind, ServerOptions, ServerStats};
pub use shm::{ShmBuffer, ShmView};

// The render thread, the server thread and the Render agent's copy thread share these.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<FramesServer>();
    send_sync::<ShmBuffer>();
    send_sync::<ShmView>();
};

// Test builds count allocations to check the render-thread API (§21).
#[cfg(test)]
#[global_allocator]
static ALLOC: se_alloc::Counting = se_alloc::Counting;

/// Environment variable overriding the socket path.
pub const SOCKET_ENV: &str = "SE_FRAMES_SOCKET";
/// Environment variable overriding the socket directory (dev instances; also used by obs.sock).
pub const RUNTIME_DIR_ENV: &str = "SE_RUNTIME_DIR";

/// `$SE_FRAMES_SOCKET`, else `$SE_RUNTIME_DIR/frames.sock`, else
/// `$XDG_RUNTIME_DIR/stream-engine/frames.sock`, else `/run/user/<uid>/stream-engine/frames.sock`.
pub fn default_socket_path() -> PathBuf {
    socket_path_from(
        std::env::var_os(SOCKET_ENV),
        std::env::var_os(RUNTIME_DIR_ENV),
        std::env::var_os("XDG_RUNTIME_DIR"),
        // SAFETY: getuid has no preconditions.
        unsafe { libc::getuid() },
    )
}

fn socket_path_from(explicit: Option<OsString>, se_dir: Option<OsString>, runtime_dir: Option<OsString>, uid: u32) -> PathBuf {
    if let Some(path) = explicit.filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(dir) = se_dir.filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("frames.sock");
    }
    runtime_dir
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")))
        .join("stream-engine")
        .join("frames.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_precedence() {
        assert_eq!(socket_path_from(Some("/tmp/x.sock".into()), Some("/se".into()), Some("/run/user/7".into()), 7), PathBuf::from("/tmp/x.sock"));
        assert_eq!(socket_path_from(None, Some("/tmp/se-dev".into()), Some("/xdg".into()), 7), PathBuf::from("/tmp/se-dev/frames.sock"));
        assert_eq!(socket_path_from(Some("".into()), None, Some("/xdg".into()), 7), PathBuf::from("/xdg/stream-engine/frames.sock"));
        assert_eq!(socket_path_from(None, Some("".into()), Some("".into()), 1000), PathBuf::from("/run/user/1000/stream-engine/frames.sock"));
        assert_eq!(socket_path_from(None, None, None, 1000), PathBuf::from("/run/user/1000/stream-engine/frames.sock"));
    }
}
