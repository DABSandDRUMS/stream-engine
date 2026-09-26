//! UI-thread state machine: per-canvas buffer sets, frame swaps, hold/release bookkeeping, want
//! hysteresis. GPU work goes through [`Gpu`] so the logic runs against the real wgpu device in
//! the app and a recording fake in tests.

use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::io::{self, Cmd, IoTx, MailFrame, NewCanvas, Shared, bit};
use super::proto::{self, CanvasDesc};
use super::shm::ShmMap;
use super::{Canvas, CanvasStats, FrameStats, FrameTexture, Transport};

/// A canvas stays wanted this long after the UI last drew it.
pub(crate) const WANT_HOLD: Duration = Duration::from_secs(1);
/// After the connection is gone this long, the last frames are dropped.
pub(crate) const STALE_FREE: Duration = Duration::from_secs(3);
const DEFAULT_HZ: f32 = 60.0;
const MAX_HZ: f32 = 1000.0;
/// A swap is allowed once this fraction of the `1 / max_hz` interval has passed, so vsync jitter
/// does not halve the rate (60 Hz UI, 30 Hz canvas → exactly every second vsync).
const RATE_SLACK: f32 = 0.8;

pub(crate) trait GpuBuffer {
    fn texture_id(&self) -> egui::TextureId;
}

/// GPU operations needed by [`Core`]; one instance per `update()`.
pub(crate) trait Gpu {
    type Buffer: GpuBuffer;
    /// Runs completed-work callbacks without blocking.
    fn maintain(&mut self);
    fn dmabuf_capable(&self) -> bool;
    /// Imports every buffer of a dmabuf canvas (takes the fds).
    fn import_dmabuf(&mut self, desc: &CanvasDesc, fds: Vec<OwnedFd>) -> Result<Vec<Self::Buffer>, String>;
    fn create_shm(&mut self, width: u32, height: u32) -> Result<Self::Buffer, String>;
    /// Records the foreign → UI queue family acquire of an imported buffer.
    fn acquire(&mut self, buffer: &Self::Buffer);
    fn upload(&mut self, buffer: &Self::Buffer, rows: &[u8], stride: u32, size: [u32; 2]);
    /// Submits recorded acquires (before egui renders this frame).
    fn submit(&mut self);
    /// Runs `callback` once all work submitted so far has completed on the GPU.
    fn on_work_done(&mut self, callback: Box<dyn FnOnce() + Send + 'static>);
    fn free(&mut self, buffer: Self::Buffer);
}

#[derive(Clone, Copy, Debug)]
struct ReleaseCmd {
    canvas: u32,
    buffer: u32,
    seq: u64,
    lease: u64,
}

impl ReleaseCmd {
    fn cmd(self) -> Cmd {
        Cmd::Release { canvas: self.canvas, buffer: self.buffer, seq: self.seq, lease: self.lease }
    }
}

struct Shown {
    buffer: u32,
    seq: u64,
    at: Instant,
    /// The engine counts this buffer as held by us (dmabuf only; shm is released after upload).
    held: bool,
}

enum Res<B> {
    Dmabuf(Vec<B>),
    Shm { maps: Vec<ShmMap>, texture: B },
}

/// One imported buffer set (one se_canvas).
struct Gen<B> {
    lease: u64,
    epoch: u64,
    desc: CanvasDesc,
    res: Res<B>,
    shown: Option<Shown>,
}

impl<B: GpuBuffer> Gen<B> {
    fn transport(&self) -> Transport {
        match self.res {
            Res::Dmabuf(_) => Transport::Dmabuf,
            Res::Shm { .. } => Transport::Shm,
        }
    }

    fn free<G: Gpu<Buffer = B>>(self, gpu: &mut G) {
        match self.res {
            Res::Dmabuf(buffers) => buffers.into_iter().for_each(|b| gpu.free(b)),
            Res::Shm { texture, .. } => gpu.free(texture),
        }
    }

    fn texture(&self, now: Instant, stale: bool) -> Option<FrameTexture> {
        let shown = self.shown.as_ref()?;
        let id = match &self.res {
            Res::Dmabuf(buffers) => buffers.get(shown.buffer as usize)?.texture_id(),
            Res::Shm { texture, .. } => texture.texture_id(),
        };
        Some(FrameTexture {
            id,
            size: [self.desc.width, self.desc.height],
            seq: shown.seq,
            age: now.saturating_duration_since(shown.at),
            transport: self.transport(),
            stale,
        })
    }
}

struct Slot<B> {
    /// The newest buffer set.
    cur: Option<Gen<B>>,
    /// The previous set, kept on screen until `cur` presents its first frame.
    old: Option<Gen<B>>,
    stale: bool,
    last_swap: Option<Instant>,
    presented: u64,
    fence_pending: bool,
}

impl<B> Default for Slot<B> {
    fn default() -> Self {
        Self { cur: None, old: None, stale: false, last_swap: None, presented: 0, fence_pending: false }
    }
}

impl<B: GpuBuffer> Slot<B> {
    fn displayed(&self) -> Option<&Gen<B>> {
        [&self.cur, &self.old].into_iter().flatten().find(|g| g.shown.is_some())
    }

    fn clear<G: Gpu<Buffer = B>>(&mut self, gpu: &mut G) {
        for g in [self.cur.take(), self.old.take()].into_iter().flatten() {
            g.free(gpu);
        }
        self.fence_pending = false;
    }
}

#[derive(Default)]
struct Totals {
    dmabuf_imports: u64,
    dmabuf_frames_presented: u64,
    shm_frames_presented: u64,
    shm_bytes_uploaded: u64,
}

enum FenceState {
    Signaled,
    Pending,
    Failed,
}

fn fence_state(fd: &OwnedFd) -> FenceState {
    let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd, zero timeout.
    let n = unsafe { libc::poll(&raw mut p, 1, 0) };
    if n <= 0 {
        FenceState::Pending
    } else if p.revents & libc::POLLIN != 0 {
        FenceState::Signaled
    } else {
        FenceState::Failed
    }
}

fn clamp_hz(hz: f32) -> f32 {
    if hz.is_finite() && hz > 0.0 { hz.min(MAX_HZ) } else { DEFAULT_HZ }
}

fn rate_allows(last: Option<Instant>, hz: f32, now: Instant) -> bool {
    last.is_none_or(|t| now.saturating_duration_since(t).as_secs_f32() * clamp_hz(hz) >= RATE_SLACK)
}

pub(crate) fn fourcc_name(fourcc: u32) -> String {
    if fourcc == 0 {
        return "shm".into();
    }
    let b = fourcc.to_le_bytes();
    if b.iter().all(|c| c.is_ascii_graphic() || *c == b' ') { String::from_utf8_lossy(&b).trim_end().to_string() } else { format!("{fourcc:#010x}") }
}

pub(crate) struct Core<B> {
    shared: Arc<Shared>,
    io: Option<IoTx>,
    slots: [Slot<B>; 4],
    wanted_at: [Option<Instant>; 4],
    /// Highest max_hz requested per canvas since the last step.
    frame_hz: [f32; 4],
    rates: [f32; 4],
    rates_sent: Option<[f32; 4]>,
    hello_sent: Option<(u32, u32)>,
    mask: u32,
    dmabuf_capable: bool,
    dmabuf_failed: Option<String>,
    totals: Totals,
    /// Displayed dmabuf buffers replaced during this step: released once the GPU is done.
    retire: [Option<ReleaseCmd>; 4],
}

impl<B: GpuBuffer> Core<B> {
    pub fn start(connector: Box<dyn io::Connector>, repaint: egui::Context) -> (Self, Option<JoinHandle<()>>) {
        let shared = Arc::new(Shared::new(Instant::now()));
        let (io, thread) = match io::spawn(connector, shared.clone(), repaint) {
            Ok((tx, thread)) => (Some(tx), Some(thread)),
            Err(e) => {
                shared.set_error(format!("frames: cannot start the IO thread: {e}"));
                (None, None)
            }
        };
        let core = Self {
            shared,
            io,
            slots: Default::default(),
            wanted_at: [None; 4],
            frame_hz: [0.0; 4],
            rates: [DEFAULT_HZ; 4],
            rates_sent: None,
            hello_sent: None,
            mask: 0,
            dmabuf_capable: false,
            dmabuf_failed: None,
            totals: Totals::default(),
            retire: [None; 4],
        };
        (core, thread)
    }

    pub fn shutdown(&mut self) {
        if let Some(io) = self.io.take() {
            io.send(Cmd::Shutdown);
        }
    }

    fn send(&self, cmd: Cmd) {
        if let Some(io) = &self.io {
            io.send(cmd);
        }
    }

    fn error(&mut self, msg: String) {
        self.shared.set_error(msg);
    }

    pub fn want(&mut self, canvas: usize, max_hz: f32, now: Instant) {
        if canvas < 4 {
            self.wanted_at[canvas] = Some(now);
            self.frame_hz[canvas] = self.frame_hz[canvas].max(clamp_hz(max_hz));
        }
    }

    fn dmabuf_enabled(&self) -> bool {
        self.dmabuf_capable && self.dmabuf_failed.is_none()
    }

    pub fn step<G: Gpu<Buffer = B>>(&mut self, gpu: &mut G, now: Instant) {
        self.shared.touch(now);
        gpu.maintain();
        self.dmabuf_capable = gpu.dmabuf_capable();

        let mut mask = 0;
        for c in 0..4 {
            if self.frame_hz[c] > 0.0 {
                self.rates[c] = self.frame_hz[c];
                self.frame_hz[c] = 0.0;
            }
            if self.wanted_at[c].is_some_and(|t| now.saturating_duration_since(t) <= WANT_HOLD) {
                mask |= bit(c);
            }
        }

        let mut news: [Option<NewCanvas>; 4] = Default::default();
        let mut leases = [0u64; 4];
        let (connected, changed_at, suspended) = {
            let mut st = self.shared.lock();
            for (c, m) in st.canvases.iter_mut().enumerate() {
                news[c] = m.desc.take();
                leases[c] = m.lease;
                if m.goodbye.take().is_some() {
                    self.slots[c].stale = true;
                }
            }
            (st.connected, st.changed_at, std::mem::take(&mut st.suspended))
        };

        if suspended {
            // The engine dropped every hold of ours and may be rewriting those buffers.
            self.hello_sent = None;
            self.slots.iter_mut().for_each(|s| s.clear(gpu));
        }
        let gone_too_long = !connected && now.saturating_duration_since(changed_at) > STALE_FREE;
        for (c, slot) in self.slots.iter_mut().enumerate() {
            if mask & bit(c) == 0 || gone_too_long {
                slot.clear(gpu);
            }
            if !connected {
                slot.stale = true;
            }
            // Holds of an ended lease are void: never release them.
            for g in [&mut slot.cur, &mut slot.old].into_iter().flatten() {
                if g.lease != leases[c]
                    && let Some(s) = &mut g.shown
                {
                    s.held = false;
                }
            }
        }

        for new in news.into_iter().flatten() {
            if mask & bit(new.desc.canvas as usize) != 0 {
                self.install(gpu, new);
            }
        }

        let mut swaps: [Option<MailFrame>; 4] = Default::default();
        let mut failed: [Option<ReleaseCmd>; 4] = [None; 4];
        {
            let mut st = self.shared.lock();
            for (c, slot) in self.slots.iter_mut().enumerate() {
                slot.fence_pending = false;
                let Some(cur) = &slot.cur else { continue };
                let m = &mut st.canvases[c];
                let Some(f) = &m.frame else { continue };
                if f.lease != cur.lease || f.frame.generation != cur.desc.generation {
                    continue; // its description is not imported yet
                }
                match f.fence.as_deref().map(fence_state) {
                    Some(FenceState::Pending) => {
                        slot.fence_pending = true;
                        continue;
                    }
                    Some(FenceState::Failed) => {
                        failed[c] = Some(ReleaseCmd { canvas: c as u32, buffer: f.frame.buffer, seq: f.frame.seq, lease: f.lease });
                        m.frame = None;
                        continue;
                    }
                    Some(FenceState::Signaled) | None => {}
                }
                if rate_allows(slot.last_swap, self.rates[c], now) {
                    swaps[c] = m.frame.take();
                }
            }
        }
        for r in failed.into_iter().flatten() {
            self.error(format!("frames: {} frame {} fence reported an error; dropped", Canvas::ALL[r.canvas as usize].name(), r.seq));
            self.send(r.cmd());
        }
        for (c, f) in swaps.into_iter().enumerate() {
            if let Some(f) = f {
                self.swap(gpu, c, f, now);
            }
        }
        gpu.submit();
        if self.retire.iter().any(Option::is_some)
            && let Some(io) = self.io.clone()
        {
            let retire = std::mem::take(&mut self.retire);
            gpu.on_work_done(Box::new(move || {
                for r in retire.into_iter().flatten() {
                    io.send(r.cmd());
                }
            }));
        }
        self.retire = [None; 4];

        let flags = if self.dmabuf_enabled() { proto::FLAG_DMABUF } else { 0 };
        if self.hello_sent != Some((mask, flags)) {
            self.send(Cmd::Hello { want: mask, flags });
            self.hello_sent = Some((mask, flags));
        }
        if self.rates_sent != Some(self.rates) {
            self.send(Cmd::Rates(self.rates));
            self.rates_sent = Some(self.rates);
        }
        self.mask = mask;
    }

    fn install<G: Gpu<Buffer = B>>(&mut self, gpu: &mut G, new: NewCanvas) {
        let NewCanvas { lease, epoch, desc, fds } = new;
        let c = desc.canvas as usize;
        let name = Canvas::ALL[c].name();
        let res = if desc.is_shm() {
            let maps: Result<Vec<ShmMap>, _> =
                fds.iter().map(|fd| ShmMap::new(fd.as_fd(), desc.offsets[0] as usize, desc.strides[0] as usize, desc.height as usize)).collect();
            drop(fds); // the mappings stay valid without the descriptors
            let maps = match maps {
                Ok(m) => m,
                Err(e) => return self.error(format!("frames: {name}: cannot map shm buffers: {e}")),
            };
            match gpu.create_shm(desc.width, desc.height) {
                Ok(texture) => Res::Shm { maps, texture },
                Err(e) => return self.error(format!("frames: {name}: cannot create shm texture: {e}")),
            }
        } else {
            if !self.dmabuf_enabled() {
                // Sent before the engine processed our switch to shm; a shm description follows.
                tracing::debug!(target: "se_ui::frames", "{name}: ignoring dmabuf canvas while shm is requested");
                return;
            }
            let imported = if desc.drm_fourcc == proto::DRM_FORMAT_ABGR8888 {
                gpu.import_dmabuf(&desc, fds)
            } else {
                Err(format!("unsupported DRM format {}", fourcc_name(desc.drm_fourcc)))
            };
            match imported {
                Ok(buffers) => {
                    self.totals.dmabuf_imports += buffers.len() as u64;
                    Res::Dmabuf(buffers)
                }
                Err(e) => {
                    // Ask for the shm transport from now on (next hello carries flags = 0).
                    self.dmabuf_failed = Some(e.clone());
                    return self.error(format!("frames: {name}: dmabuf import failed ({e}); switching to shm"));
                }
            }
        };
        let set = Gen { lease, epoch, desc, res, shown: None };
        tracing::info!(
            target: "se_ui::frames",
            canvas = name,
            transport = set.transport().name(),
            width = desc.width,
            height = desc.height,
            generation = desc.generation,
            buffers = desc.buffer_count,
            fourcc = %fourcc_name(desc.drm_fourcc),
            modifier = %format_args!("{:#x}", desc.modifier),
            "frames: canvas imported"
        );
        let slot = &mut self.slots[c];
        if let Some(mut prev) = slot.cur.take() {
            // Keep the previous last frame on screen until the new set presents — but only if
            // the engine can no longer write those buffers (other generation or connection).
            let keep = prev.shown.is_some() && (prev.desc.generation != desc.generation || prev.epoch != epoch);
            if keep {
                if let Some(s) = &mut prev.shown {
                    s.held = false;
                }
                if let Some(older) = slot.old.replace(prev) {
                    older.free(gpu);
                }
            } else {
                prev.free(gpu);
            }
        }
        slot.cur = Some(set);
    }

    fn swap<G: Gpu<Buffer = B>>(&mut self, gpu: &mut G, c: usize, f: MailFrame, now: Instant) {
        let slot = &mut self.slots[c];
        let Some(set) = &mut slot.cur else { return };
        let (buffer, seq) = (f.frame.buffer, f.frame.seq);
        let held = match &set.res {
            Res::Dmabuf(buffers) => {
                let Some(b) = buffers.get(buffer as usize) else { return };
                gpu.acquire(b);
                self.totals.dmabuf_frames_presented += 1;
                true
            }
            Res::Shm { maps, texture } => {
                let Some(map) = maps.get(buffer as usize) else { return };
                let (w, h) = (set.desc.width, set.desc.height);
                gpu.upload(texture, map.rows(), set.desc.strides[0], [w, h]);
                self.totals.shm_frames_presented += 1;
                self.totals.shm_bytes_uploaded += u64::from(w) * u64::from(h) * 4;
                // write_texture copied the rows into wgpu's staging memory already.
                if let Some(io) = &self.io {
                    io.send(Cmd::Release { canvas: c as u32, buffer, seq, lease: f.lease });
                }
                false
            }
        };
        let prev = set.shown.replace(Shown { buffer, seq, at: now, held });
        if let Some(p) = prev
            && p.held
            && p.buffer != buffer
        {
            // Earlier submissions may still sample it: release once they have completed.
            self.retire[c] = Some(ReleaseCmd { canvas: c as u32, buffer: p.buffer, seq: p.seq, lease: set.lease });
        }
        slot.last_swap = Some(now);
        slot.presented += 1;
        slot.stale = false;
        if let Some(old) = slot.old.take() {
            old.free(gpu);
        }
    }

    pub fn texture(&self, canvas: usize, now: Instant) -> Option<FrameTexture> {
        let slot = self.slots.get(canvas)?;
        slot.displayed()?.texture(now, slot.stale)
    }

    pub fn stats(&self, socket: &Path, now: Instant) -> FrameStats {
        let st = self.shared.lock();
        let mut canvases: [CanvasStats; 4] = Default::default();
        for (c, out) in canvases.iter_mut().enumerate() {
            let slot = &self.slots[c];
            let m = &st.canvases[c];
            let set = slot.cur.as_ref().or(slot.old.as_ref());
            let shown = slot.displayed().and_then(|g| g.shown.as_ref());
            *out = CanvasStats {
                wanted: self.mask & bit(c) != 0,
                max_hz: self.rates[c],
                transport: set.map_or(Transport::None, Gen::transport),
                width: set.map_or(0, |g| g.desc.width),
                height: set.map_or(0, |g| g.desc.height),
                generation: set.map_or(0, |g| g.desc.generation),
                drm_fourcc: set.map_or(0, |g| g.desc.drm_fourcc),
                modifier: set.map_or(0, |g| g.desc.modifier),
                frames_received: m.frames_received,
                frames_presented: slot.presented,
                frames_dropped_rate: m.frames_superseded,
                frames_dropped_stale: m.frames_stale,
                last_seq: m.last_seq,
                presented_seq: shown.map(|s| s.seq),
                age_ms: shown.map(|s| now.saturating_duration_since(s.at).as_secs_f64() * 1000.0),
                fence_pending: slot.fence_pending,
                stale: slot.stale,
            };
        }
        FrameStats {
            connected: st.connected,
            socket: socket.to_path_buf(),
            dmabuf_enabled: self.dmabuf_enabled(),
            canvases,
            dmabuf_imports: self.totals.dmabuf_imports,
            dmabuf_frames_presented: self.totals.dmabuf_frames_presented,
            shm_frames_presented: self.totals.shm_frames_presented,
            shm_bytes_uploaded: self.totals.shm_bytes_uploaded,
            releases_sent: self.shared.releases_sent.load(Ordering::Relaxed),
            hellos_sent: self.shared.hellos_sent.load(Ordering::Relaxed),
            reconnects: self.shared.reconnects.load(Ordering::Relaxed),
            last_error: st.last_error.clone(),
        }
    }
}
