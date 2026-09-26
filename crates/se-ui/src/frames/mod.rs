//! frames.sock client (docs/frames-protocol.md): imports engine canvases (dmabuf or shm) as egui
//! textures.
//!
//! * An IO thread ([`io`]) owns the socket: hello/release out, canvases/frames/goodbyes in (with
//!   their `SCM_RIGHTS` fds), reconnecting with backoff. It keeps at most one pending canvas
//!   description and one pending frame per canvas and releases superseded frames immediately, so
//!   a slow or hidden UI never makes the engine stall.
//! * The UI thread ([`Frames::update`]) imports dmabufs into the eframe wgpu device (Vulkan,
//!   `VK_EXT_image_drm_format_modifier`), waits for each frame's sync_file without blocking,
//!   acquires the image from the foreign queue family and swaps the displayed buffer at most
//!   `max_hz` times per second. The shm fallback uploads memfd rows with `Queue::write_texture`.
//!   Nothing is ever read back from the GPU.

mod core;
mod gpu;
mod io;
pub mod proto;
mod shm;

#[cfg(test)]
mod gpu_tests;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use eframe::egui_wgpu;

pub use gpu::{device_descriptor, open_device, wgpu_options};

/// An engine output canvas (`se_hello.want` bit = [`Canvas::index`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Canvas {
    Wide,
    Tall,
    Preview,
    Atlas,
}

impl Canvas {
    pub const ALL: [Canvas; 4] = [Canvas::Wide, Canvas::Tall, Canvas::Preview, Canvas::Atlas];

    /// Protocol index (0 wide, 1 tall, 2 preview, 3 atlas).
    pub fn index(self) -> usize {
        self as usize
    }

    pub fn name(self) -> &'static str {
        match self {
            Canvas::Wide => "wide",
            Canvas::Tall => "tall",
            Canvas::Preview => "preview",
            Canvas::Atlas => "atlas",
        }
    }

    pub fn from_index(index: usize) -> Option<Canvas> {
        Self::ALL.get(index).copied()
    }
}

/// How a canvas reaches the UI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Transport {
    #[default]
    None,
    /// Zero-copy dmabuf import.
    Dmabuf,
    /// memfd rows uploaded with `Queue::write_texture`.
    Shm,
}

impl Transport {
    pub fn name(self) -> &'static str {
        match self {
            Transport::None => "none",
            Transport::Dmabuf => "dmabuf",
            Transport::Shm => "shm",
        }
    }
}

/// The frame currently displayed for a canvas. Fetch it every UI frame (after
/// [`Frames::update`]): `id` changes whenever a newer engine buffer is swapped in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameTexture {
    pub id: egui::TextureId,
    pub size: [u32; 2],
    pub seq: u64,
    /// Time since this frame was swapped in.
    pub age: Duration,
    pub transport: Transport,
    /// The source is gone (engine disconnected or said goodbye); this is the last frame it sent.
    pub stale: bool,
}

/// Per-canvas counters for the Performance view.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CanvasStats {
    pub wanted: bool,
    pub max_hz: f32,
    pub transport: Transport,
    pub width: u32,
    pub height: u32,
    pub generation: u32,
    pub drm_fourcc: u32,
    pub modifier: u64,
    pub frames_received: u64,
    pub frames_presented: u64,
    /// Frames replaced by a newer one before they were displayed (rate limit or slow UI).
    pub frames_dropped_rate: u64,
    /// Frames of an outdated generation (ignored).
    pub frames_dropped_stale: u64,
    /// Newest `se_frame.seq` received.
    pub last_seq: Option<u64>,
    /// `seq` of the displayed frame.
    pub presented_seq: Option<u64>,
    /// Time since the displayed frame was swapped in.
    pub age_ms: Option<f64>,
    /// The newest received frame is still waiting for its GPU fence.
    pub fence_pending: bool,
    pub stale: bool,
}

/// Snapshot for the Performance view; also the proof of the zero-readback path (there is no
/// GPU→CPU copy anywhere in this module; `shm_bytes_uploaded` is the only host→GPU copy).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameStats {
    pub connected: bool,
    pub socket: PathBuf,
    /// The UI device can import dmabufs and no import has failed (else hello asks for shm).
    pub dmabuf_enabled: bool,
    /// Indexed by [`Canvas::index`].
    pub canvases: [CanvasStats; 4],
    /// dmabuf buffers imported (each se_canvas imports all of its buffers).
    pub dmabuf_imports: u64,
    pub dmabuf_frames_presented: u64,
    pub shm_frames_presented: u64,
    pub shm_bytes_uploaded: u64,
    pub releases_sent: u64,
    pub hellos_sent: u64,
    pub reconnects: u64,
    pub last_error: Option<String>,
}

/// The frames.sock client. Create once with [`Frames::start`]; per UI frame call
/// [`Frames::update`] first, then [`Frames::want`] + [`Frames::texture`] for every canvas drawn.
pub struct Frames {
    socket: PathBuf,
    core: core::Core<gpu::GpuBuffer>,
    caps: Option<gpu::DeviceCaps>,
    io_thread: Option<JoinHandle<()>>,
}

/// Socket path: explicit, else `$SE_FRAMES_SOCKET`, else `<runtime dir>/frames.sock`.
pub fn socket_path(explicit: Option<PathBuf>) -> PathBuf {
    explicit
        .or_else(|| std::env::var_os("SE_FRAMES_SOCKET").filter(|v| !v.is_empty()).map(PathBuf::from))
        .unwrap_or_else(|| se_proto::wire::runtime_dir().join("frames.sock"))
}

impl Frames {
    /// Starts the IO thread (never blocks; connects in the background with backoff).
    /// `repaint` is woken when a frame for a wanted canvas becomes presentable.
    pub fn start(socket: Option<PathBuf>, repaint: egui::Context) -> Frames {
        let socket = socket_path(socket);
        let (core, io_thread) = core::Core::start(Box::new(io::PathConnector(socket.clone())), repaint);
        Frames { socket, core, caps: None, io_thread }
    }

    /// Declares that `canvas` is drawn this UI frame and should update at most `max_hz` times
    /// per second. Canvases not declared for 1 s stop being requested from the engine.
    pub fn want(&mut self, canvas: Canvas, max_hz: f32) {
        self.core.want(canvas.index(), max_hz, Instant::now());
    }

    /// Once per UI frame (main viewport), before any [`Frames::texture`] call: imports new
    /// canvases, swaps in presentable frames and sends hello/release updates.
    pub fn update(&mut self, rs: &egui_wgpu::RenderState) {
        let caps = self.caps.get_or_insert_with(|| gpu::DeviceCaps::probe(&rs.device));
        let mut gpu = gpu::WgpuGpu::new(rs, caps);
        self.core.step(&mut gpu, Instant::now());
    }

    pub fn texture(&self, canvas: Canvas) -> Option<FrameTexture> {
        self.core.texture(canvas.index(), Instant::now())
    }

    pub fn stats(&self) -> FrameStats {
        self.core.stats(&self.socket, Instant::now())
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        self.core.shutdown();
        if let Some(t) = self.io_thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::os::fd::{FromRawFd, OwnedFd};

    /// A memfd named `name` holding `data` (named so leaks are countable in /proc/self/fd).
    pub fn memfd_with(name: &str, data: &[u8]) -> OwnedFd {
        let cname = std::ffi::CString::new(name).unwrap();
        // SAFETY: plain memfd_create/write calls on a fresh fd.
        let fd = unsafe { libc::memfd_create(cname.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create failed");
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if !data.is_empty() {
            let n = unsafe { libc::pwrite(std::os::fd::AsRawFd::as_raw_fd(&fd), data.as_ptr().cast(), data.len(), 0) };
            assert_eq!(n as usize, data.len());
        }
        fd
    }

    pub fn memfd(name: &str, size: usize) -> OwnedFd {
        memfd_with(name, &vec![0u8; size])
    }

    /// Number of this process's open fds whose target is `link` (as shown by /proc/self/fd).
    pub fn open_fds_linking(link: &str) -> usize {
        std::fs::read_dir("/proc/self/fd").unwrap().filter_map(|e| std::fs::read_link(e.ok()?.path()).ok()).filter(|t| t.as_os_str() == link).count()
    }

    /// Number of open fds of memfds named `name`.
    pub fn open_fds_named(name: &str) -> usize {
        open_fds_linking(&format!("/memfd:{name} (deleted)"))
    }
}
