//! Shared GPU resources: bind group layouts, samplers, placeholder textures, render targets,
//! the per-frame uniform arena, bind group cache, and mapped staging/readback rings.
//!
//! Everything that allocates (textures, buffers, bind groups) happens at load, on resize, or on
//! first use of a new texture combination; steady-state frames only reuse.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

pub const COLOR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Bind group layouts shared by the render thread and the pipeline loader.
pub struct Layouts {
    pub fx: wgpu::BindGroupLayout,
    pub patch: wgpu::BindGroupLayout,
    /// `patch` plus `se_prev` (binding 5, 2D) and `se_history` (binding 6, 2D array) for effect
    /// shaders with `feedback`/`history`; other patches keep the plain layout.
    pub patch_frames: wgpu::BindGroupLayout,
    pub sim: wgpu::BindGroupLayout,
    pub node: wgpu::BindGroupLayout,
    pub node_tex: wgpu::BindGroupLayout,
    pub convert: wgpu::BindGroupLayout,
    pub blit: wgpu::BindGroupLayout,
    pub flash: wgpu::BindGroupLayout,
    pub fx_pipeline: wgpu::PipelineLayout,
    pub patch_pipeline: wgpu::PipelineLayout,
    pub patch_frames_pipeline: wgpu::PipelineLayout,
    pub sim_pipeline: wgpu::PipelineLayout,
    pub linear: wgpu::Sampler,
    pub nearest: wgpu::Sampler,
    /// 1×1 transparent 2D texture (also viewed as a 1-layer array) and 2×2×2 identity 3D LUT
    /// (unused bindings).
    pub dummy_2d: wgpu::TextureView,
    pub dummy_2d_array: wgpu::TextureView,
    pub dummy_3d: wgpu::TextureView,
    /// Placeholder storage buffer for shader patches (binding 4 unused).
    pub dummy_storage: wgpu::Buffer,
}

fn uniform(binding: u32, vis: wgpu::ShaderStages, dynamic: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: vis,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: dynamic, min_binding_size: None },
        count: None,
    }
}

fn tex(binding: u32, vis: wgpu::ShaderStages, dim: wgpu::TextureViewDimension) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: vis,
        ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: dim, multisampled: false },
        count: None,
    }
}

fn sampler(binding: u32, vis: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry { binding, visibility: vis, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None }
}

fn storage(binding: u32, vis: wgpu::ShaderStages, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: vis,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }
}

impl Layouts {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Arc<Layouts> {
        use wgpu::ShaderStages as S;
        use wgpu::TextureViewDimension as D;
        let bgl = |label: &str, entries: &[wgpu::BindGroupLayoutEntry]| {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some(label), entries })
        };
        let vf = S::VERTEX_FRAGMENT;
        let fx = bgl("fx", &[uniform(0, vf, true), sampler(1, vf), tex(2, vf, D::D2), tex(3, vf, D::D2), tex(4, vf, D::D3)]);
        let patch = bgl("patch", &[uniform(0, vf, true), sampler(1, vf), tex(2, vf, D::D2), tex(3, vf, D::D2), storage(4, S::VERTEX_FRAGMENT, true)]);
        let patch_frames = bgl(
            "patch frames",
            &[uniform(0, vf, true), sampler(1, vf), tex(2, vf, D::D2), tex(3, vf, D::D2), storage(4, vf, true), tex(5, vf, D::D2), tex(6, vf, D::D2Array)],
        );
        let sim = bgl(
            "patch sim",
            &[uniform(0, S::COMPUTE, true), sampler(1, S::COMPUTE), tex(2, S::COMPUTE, D::D2), tex(3, S::COMPUTE, D::D2), storage(4, S::COMPUTE, false)],
        );
        let node = bgl("node", &[uniform(0, vf, true)]);
        let node_tex = bgl("node tex", &[tex(0, vf, D::D2), sampler(1, vf), tex(2, vf, D::D2)]);
        let convert = bgl("convert", &[uniform(0, vf, true), tex(1, vf, D::D2), tex(2, vf, D::D2), tex(3, vf, D::D3), sampler(4, vf)]);
        let blit = bgl("blit", &[uniform(0, vf, true), tex(1, vf, D::D2), tex(2, vf, D::D2), sampler(3, vf)]);
        let flash = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("flash"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: S::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: D::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                storage(1, S::COMPUTE, false),
            ],
        });
        let pl = |label: &str, l: &wgpu::BindGroupLayout| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some(label), bind_group_layouts: &[Some(l)], immediate_size: 0 })
        };
        let fx_pipeline = pl("fx", &fx);
        let patch_pipeline = pl("patch", &patch);
        let patch_frames_pipeline = pl("patch frames", &patch_frames);
        let sim_pipeline = pl("patch sim", &sim);
        let samp = |label: &str, f: wgpu::FilterMode| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some(label),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: f,
                min_filter: f,
                mipmap_filter: wgpu::MipmapFilterMode::Nearest,
                ..Default::default()
            })
        };
        let dummy2 = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dummy 2d"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: COLOR,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            dummy2.as_image_copy(),
            &[0u8; 4],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4), rows_per_image: None },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        let identity = crate::lut::Lut::identity(2);
        let dummy_3d = crate::lut::upload(device, queue, &identity);
        let dummy_storage = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy storage"),
            size: 64,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        Arc::new(Layouts {
            fx,
            patch,
            patch_frames,
            sim,
            node,
            node_tex,
            convert,
            blit,
            flash,
            fx_pipeline,
            patch_pipeline,
            patch_frames_pipeline,
            sim_pipeline,
            linear: samp("linear", wgpu::FilterMode::Linear),
            nearest: samp("nearest", wgpu::FilterMode::Nearest),
            dummy_2d: dummy2.create_view(&Default::default()),
            dummy_2d_array: dummy2.create_view(&wgpu::TextureViewDescriptor { dimension: Some(D::D2Array), ..Default::default() }),
            dummy_3d,
            dummy_storage,
        })
    }
}

static NEXT_TEX: AtomicU32 = AtomicU32::new(1);

/// A render target / sampled texture with a stable id for bind group caching.
pub struct Tex {
    pub id: u32,
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub size: [u32; 2],
}

impl Tex {
    pub fn new(device: &wgpu::Device, label: &str, size: [u32; 2], format: wgpu::TextureFormat, usage: wgpu::TextureUsages) -> Tex {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: size[0].max(1), height: size[1].max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        Tex { id: NEXT_TEX.fetch_add(1, Ordering::Relaxed), texture, view, size }
    }

    /// Standard render target: render attachment + sampled + copy source.
    pub fn target(device: &wgpu::Device, label: &str, size: [u32; 2]) -> Tex {
        Tex::new(
            device,
            label,
            size,
            COLOR,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
        )
    }

    /// Wrap an externally created texture (dmabuf export images).
    pub fn wrap(texture: wgpu::Texture, view: wgpu::TextureView, size: [u32; 2]) -> Tex {
        Tex { id: NEXT_TEX.fetch_add(1, Ordering::Relaxed), texture, view, size }
    }

    pub fn bytes(&self) -> u64 {
        let bpp = self.texture.format().block_copy_size(None).unwrap_or(4) as u64;
        self.size[0] as u64 * self.size[1] as u64 * bpp
    }
}

/// Size of every dynamic uniform binding window (largest uniform block: patch headers).
pub const WINDOW: usize = 2048;

/// Per-frame uniform data, uploaded with one `write_buffer` before submission and bound with
/// dynamic offsets.
pub struct Arena {
    pub buffer: wgpu::Buffer,
    cpu: Vec<u8>,
    used: usize,
    align: usize,
    overflowed: bool,
}

impl Arena {
    pub fn new(device: &wgpu::Device, capacity: usize) -> Arena {
        let align = device.limits().min_uniform_buffer_offset_alignment as usize;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniform arena"),
            size: capacity as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Arena { buffer, cpu: vec![0u8; capacity], used: 0, align, overflowed: false }
    }

    pub fn reset(&mut self) {
        self.used = 0;
        self.overflowed = false;
    }

    /// Append `data` (≤ [`WINDOW`] bytes), returning its dynamic offset.
    pub fn push(&mut self, data: &[u8]) -> u32 {
        let off = self.used;
        if off + WINDOW > self.cpu.len() {
            // Out of room: reuse the last slot (visually wrong but never out of bounds); the
            // arena is sized for the plan, so this is reported as a bug.
            self.overflowed = true;
            return (self.cpu.len() - WINDOW) as u32;
        }
        let n = data.len().min(WINDOW);
        self.cpu[off..off + n].copy_from_slice(&data[..n]);
        self.used = off + n.div_ceil(self.align) * self.align;
        off as u32
    }

    pub fn push_pod<T: bytemuck::Pod>(&mut self, v: &T) -> u32 {
        self.push(bytemuck::bytes_of(v))
    }

    pub fn push_f32(&mut self, v: &[f32]) -> u32 {
        self.push(bytemuck::cast_slice(v))
    }

    pub fn upload(&self, queue: &wgpu::Queue) {
        if self.used > 0 {
            queue.write_buffer(&self.buffer, 0, &self.cpu[..self.used.min(self.cpu.len())]);
        }
    }

    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Binding window for dynamic-offset uniform bindings.
    pub fn binding(&self) -> wgpu::BindingResource<'_> {
        wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer: &self.buffer, offset: 0, size: wgpu::BufferSize::new(WINDOW as u64) })
    }
}

/// Bind groups keyed by (kind, texture ids); cleared whenever textures are recreated.
#[derive(Default)]
pub struct BindCache {
    map: HashMap<(u8, u32, u32, u32), wgpu::BindGroup>,
}

impl BindCache {
    pub fn get_or(&mut self, key: (u8, u32, u32, u32), make: impl FnOnce() -> wgpu::BindGroup) -> &wgpu::BindGroup {
        self.map.entry(key).or_insert_with(make)
    }

    /// Drop groups referencing texture `id`.
    pub fn forget(&mut self, id: u32) {
        self.map.retain(|k, _| k.1 != id && k.2 != id && k.3 != id);
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

const READY: u8 = 0;
const PENDING: u8 = 1;
const FAILED: u8 = 2;
const IDLE: u8 = 3;

/// Host-visible readback buffers cycled between CPU and GPU with `map_async`, without
/// per-frame buffer creation. Large uploads use the system-RAM ring in `crate::upload`.
pub struct MappedRing {
    pub buffers: Vec<wgpu::Buffer>,
    state: Vec<Arc<AtomicU8>>,
    /// Buffer index handed out this frame (re-mapped after submit).
    in_flight: Vec<bool>,
    next: usize,
    pub size: u64,
    /// Frame counter each readback buffer was filled on.
    pub tags: Vec<u64>,
}

impl MappedRing {
    pub fn new(device: &wgpu::Device, label: &str, count: usize, size: u64) -> MappedRing {
        let usage = wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST;
        let buffers: Vec<wgpu::Buffer> = (0..count)
            .map(|i| device.create_buffer(&wgpu::BufferDescriptor { label: Some(&format!("{label}#{i}")), size, usage, mapped_at_creation: false }))
            .collect();
        MappedRing {
            buffers,
            state: (0..count).map(|_| Arc::new(AtomicU8::new(IDLE))).collect(),
            in_flight: vec![false; count],
            next: 0,
            size,
            tags: vec![0; count],
        }
    }

    /// Read ring: an unmapped buffer the GPU can copy into.
    pub fn acquire_read(&mut self, tag: u64) -> Option<usize> {
        let n = self.buffers.len();
        for k in 0..n {
            let i = (self.next + k) % n;
            if !self.in_flight[i] && self.state[i].load(Ordering::Acquire) == IDLE {
                self.next = (i + 1) % n;
                self.in_flight[i] = true;
                self.tags[i] = tag;
                return Some(i);
            }
        }
        None
    }

    /// After the frame's submit: request mapping of every buffer used this frame.
    pub fn after_submit(&mut self) {
        for i in 0..self.buffers.len() {
            if !self.in_flight[i] {
                continue;
            }
            self.in_flight[i] = false;
            let st = self.state[i].clone();
            st.store(PENDING, Ordering::Release);
            let _p = se_alloc::Pause::new();
            self.buffers[i].slice(..).map_async(wgpu::MapMode::Read, move |r| st.store(if r.is_ok() { READY } else { FAILED }, Ordering::Release));
        }
    }

    /// Read ring: a mapped buffer with data, oldest tag first. Call [`MappedRing::release_read`]
    /// after reading.
    pub fn ready_read(&self) -> Option<usize> {
        (0..self.buffers.len()).filter(|&i| self.state[i].load(Ordering::Acquire) == READY).min_by_key(|&i| self.tags[i])
    }

    pub fn release_read(&mut self, i: usize) {
        self.buffers[i].unmap();
        self.state[i].store(IDLE, Ordering::Release);
    }

    pub fn is_ready(&self, i: usize) -> bool {
        self.state[i].load(Ordering::Acquire) == READY
    }

    pub fn failed(&self) -> bool {
        self.state.iter().any(|s| s.load(Ordering::Acquire) == FAILED)
    }
}

/// Freeze photos share a bounded GPU staging pool; PNG encoding and file IO never run on the
/// render thread. The shader's state alpha contains bookkeeping, so photos store only RGB.
const PHOTO_SLOTS: usize = 8;
const PHOTO_REQUESTS: usize = 64;
const PHOTO_BYTES: u64 = 256 << 20;

#[derive(Default)]
pub struct FreezePhotoStatus {
    pub pending: std::sync::atomic::AtomicU64,
    pub saved: std::sync::atomic::AtomicU64,
    pub failed: std::sync::atomic::AtomicU64,
    pub directory: parking_lot::Mutex<String>,
    pub last_path: parking_lot::Mutex<String>,
    pub error: parking_lot::Mutex<String>,
}

impl FreezePhotoStatus {
    pub(crate) fn fail(&self, count: u64, detail: String) {
        self.failed.fetch_add(count, Ordering::Relaxed);
        *self.error.lock() = detail;
    }
}

#[derive(Clone, Copy)]
struct PhotoRequest {
    seconds: u64,
    nanos: u32,
    sequence: u64,
}

struct PhotoSlot {
    buffer: Option<wgpu::Buffer>,
    bytes: u64,
    /// IDLE only after the worker has unmapped the buffer.
    state: Arc<AtomicU8>,
}

struct PhotoJob {
    device: wgpu::Device,
    buffer: wgpu::Buffer,
    state: Arc<AtomicU8>,
    size: [u32; 2],
    row: u32,
    context: usize,
    requests: Vec<PhotoRequest>,
}

/// One pool per renderer, across every freeze-frame attachment and canvas. Multiple accepted
/// triggers before a rendered pass share that exact state readback, but each writes a photo.
pub struct FreezePhotos {
    status: Arc<FreezePhotoStatus>,
    slots: [PhotoSlot; PHOTO_SLOTS],
    requests: std::collections::VecDeque<PhotoRequest>,
    submitted: Vec<PhotoJob>,
    tx: Option<crossbeam_channel::Sender<PhotoJob>>,
    rx: crossbeam_channel::Receiver<PhotoJob>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    wake: crossbeam_channel::Sender<()>,
    wake_rx: crossbeam_channel::Receiver<()>,
    worker: Option<std::thread::JoinHandle<()>>,
    retry_at: u64,
}

impl FreezePhotos {
    pub fn new(status: Arc<FreezePhotoStatus>) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(PHOTO_SLOTS);
        let (wake, wake_rx) = crossbeam_channel::bounded(1);
        let mut photos = Self {
            status,
            slots: std::array::from_fn(|_| PhotoSlot { buffer: None, bytes: 0, state: Arc::new(AtomicU8::new(IDLE)) }),
            requests: std::collections::VecDeque::with_capacity(PHOTO_REQUESTS),
            submitted: Vec::with_capacity(PHOTO_SLOTS),
            tx: Some(tx),
            rx,
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            wake,
            wake_rx,
            worker: None,
            retry_at: 0,
        };
        photos.retry_worker(se_clock::now());
        photos
    }

    /// Thread-creation failure must not take rendering down; retry independently.
    pub fn retry_worker(&mut self, now: u64) {
        if self.worker.is_some() || now < self.retry_at {
            return;
        }
        self.retry_at = now.saturating_add(2_000_000_000);
        let (rx, status, stop, wake) = (self.rx.clone(), self.status.clone(), self.stop.clone(), self.wake_rx.clone());
        match std::thread::Builder::new().name("se-freeze-photos".into()).spawn(move || photo_worker(rx, status, stop, wake)) {
            Ok(worker) => self.worker = Some(worker),
            Err(error) => {
                self.status.fail(0, format!("Cannot start freeze-photo worker: {error}; check process/thread limits; retrying every 2 seconds"));
            }
        }
    }

    pub fn request(&mut self) {
        if self.requests.len() == PHOTO_REQUESTS {
            self.status.fail(1, "Freeze-photo trigger backlog is full (64 requests); this photo could not be saved. Reduce trigger bursts and check the save-directory permissions/free space".into());
            return;
        }
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        self.requests.push_back(PhotoRequest { seconds: stamp.as_secs(), nanos: stamp.subsec_nanos(), sequence: SEQUENCE.fetch_add(1, Ordering::Relaxed) });
        self.status.pending.fetch_add(1, Ordering::Relaxed);
    }

    /// Called immediately after the freeze shader writes its clean state, before that texture
    /// can be reused. Never capture the decorated output or a later program/preview frame.
    pub fn capture(&mut self, device: &wgpu::Device, enc: &mut wgpu::CommandEncoder, state: &Tex, context: usize) {
        if self.requests.is_empty() {
            return;
        }
        let _p = se_alloc::Pause::new();
        let size = state.size;
        let Some((row, bytes)) = photo_layout(size) else {
            self.fail_requests("Freeze-photo dimensions exceed the 256 MiB staging budget; reduce the freeze target resolution".into());
            return;
        };
        if bytes > device.limits().max_buffer_size {
            self.fail_requests("Freeze-photo dimensions exceed the GPU readback-buffer limit; reduce the freeze target resolution".into());
            return;
        }
        let Some(i) = self.slots.iter().position(|slot| slot.state.load(Ordering::Acquire) == IDLE) else {
            self.fail_requests("Freeze-photo staging pool is full (8 captures); this photo could not be saved. Check save-directory permissions/free space and reduce trigger frequency".into());
            return;
        };
        let allocated: u64 = self.slots.iter().map(|slot| slot.bytes).sum();
        if allocated - self.slots[i].bytes + bytes > PHOTO_BYTES {
            self.fail_requests("Freeze-photo staging memory limit reached (256 MiB); this photo could not be saved. Reduce freeze resolution or trigger frequency".into());
            return;
        }
        let slot = &mut self.slots[i];
        if slot.bytes != bytes {
            slot.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("freeze photo readback"),
                size: bytes,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }));
            slot.bytes = bytes;
        }
        let buffer = slot.buffer.as_ref().expect("photo buffer allocated");
        slot.state.store(PENDING, Ordering::Release);
        enc.copy_texture_to_buffer(
            state.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(size[1]) },
            },
            wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
        );
        self.submitted.push(PhotoJob {
            device: device.clone(),
            buffer: buffer.clone(),
            state: slot.state.clone(),
            size,
            row,
            context,
            requests: self.requests.drain(..).collect(),
        });
    }

    fn fail_requests(&mut self, detail: String) {
        let count = self.requests.len() as u64;
        self.requests.clear();
        self.status.pending.fetch_sub(count, Ordering::Relaxed);
        self.status.fail(count, detail);
    }

    /// Queue only after GPU submit. A worker owns each staging slot until unmap, so the queue
    /// cannot fill while any slot is available; a disconnected worker is still health-visible.
    pub fn after_submit(&mut self) {
        for job in self.submitted.drain(..) {
            if let Err(error) = self.tx.as_ref().expect("photo sender alive").try_send(job) {
                let job = error.into_inner();
                let count = job.requests.len() as u64;
                job.state.store(IDLE, Ordering::Release);
                self.status.pending.fetch_sub(count, Ordering::Relaxed);
                self.status.fail(count, "Freeze-photo worker queue unavailable; photos could not be saved. Check engine logs and process/thread limits".into());
            }
        }
    }

    pub fn bytes(&self) -> u64 {
        self.slots.iter().map(|slot| slot.bytes).sum()
    }
}

impl Drop for FreezePhotos {
    fn drop(&mut self) {
        if !self.requests.is_empty() {
            self.fail_requests("Renderer stopped before a freeze-frame pass could capture pending photos; check the freeze target and patch compilation health".into());
        }
        self.after_submit();
        self.stop.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
        self.tx.take();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                self.status.fail(0, "Freeze-photo worker panicked; check engine logs and restart the engine when not live".into());
            }
        } else {
            while let Ok(job) = self.rx.try_recv() {
                let count = job.requests.len() as u64;
                job.state.store(IDLE, Ordering::Release);
                self.status.pending.fetch_sub(count, Ordering::Relaxed);
                self.status.fail(count, "Renderer stopped while the freeze-photo worker was unavailable; captured photos could not be saved. Check process/thread limits".into());
            }
        }
    }
}

fn photo_layout(size: [u32; 2]) -> Option<(u32, u64)> {
    if size.contains(&0) {
        return None;
    }
    let row = size[0].checked_mul(4)?.checked_add(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT - 1)?
        / wgpu::COPY_BYTES_PER_ROW_ALIGNMENT * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let bytes = u64::from(row).checked_mul(u64::from(size[1]))?;
    (bytes <= PHOTO_BYTES).then_some((row, bytes))
}

fn photo_rgb(mapped: &[u8], size: [u32; 2], row: u32) -> Result<Vec<u8>, String> {
    let (expected_row, bytes) = photo_layout(size).ok_or("Invalid freeze-photo dimensions")?;
    if row != expected_row || mapped.len() < bytes as usize {
        return Err("Freeze-photo readback is truncated or has an invalid row stride".into());
    }
    let mut rgb = Vec::with_capacity(size[0] as usize * size[1] as usize * 3);
    for y in 0..size[1] as usize {
        for pixel in mapped[y * row as usize..y * row as usize + size[0] as usize * 4].chunks_exact(4) {
            rgb.extend_from_slice(&pixel[..3]);
        }
    }
    Ok(rgb)
}

fn photo_worker(
    rx: crossbeam_channel::Receiver<PhotoJob>,
    status: Arc<FreezePhotoStatus>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    wake: crossbeam_channel::Receiver<()>,
) {
    let mut directory_ready = false;
    let mut directory_retry = std::time::Instant::now();
    let mut directory_backoff = std::time::Duration::from_secs(1);
    loop {
        if !directory_ready && std::time::Instant::now() >= directory_retry {
            match prepare_photo_directory(&status) {
                Ok(_) => {
                    directory_ready = true;
                    status.error.lock().clear();
                }
                Err(error) => {
                    *status.error.lock() = format!("{error}; check HOME, directory permissions and free space; retrying");
                    directory_retry = std::time::Instant::now() + directory_backoff;
                    directory_backoff = (directory_backoff * 2).min(std::time::Duration::from_secs(30));
                }
            }
        }
        let job = match rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(job) => job,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        let result = read_photo(&job, &stop).and_then(|rgb| encode_photo(&rgb, job.size));
        match result {
            Ok(png) => {
                for request in &job.requests {
                    let mut backoff = std::time::Duration::from_secs(1);
                    loop {
                        match save_photo(&png, *request, job.context, &status) {
                            Ok(path) => {
                                *status.last_path.lock() = path.display().to_string();
                                status.saved.fetch_add(1, Ordering::Relaxed);
                                status.pending.fetch_sub(1, Ordering::Relaxed);
                                status.error.lock().clear();
                                tracing::info!(target: "render", "freeze photo saved: {}", path.display());
                                break;
                            }
                            Err(error) => {
                                *status.error.lock() = format!("{error}; check HOME, directory permissions and free space; retaining photo and retrying");
                                if stop.load(Ordering::Acquire) {
                                    status.pending.fetch_sub(1, Ordering::Relaxed);
                                    status.fail(1, format!("{error}; renderer is stopping, this photo could not be saved"));
                                    break;
                                }
                                let _ = wake.recv_timeout(backoff);
                                backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
                            }
                        }
                    }
                }
            }
            Err(error) => {
                let count = job.requests.len() as u64;
                status.pending.fetch_sub(count, Ordering::Relaxed);
                status.fail(count, format!("{error}; photos could not be saved; check health.render and GPU device health"));
            }
        }
        job.buffer.unmap();
        job.state.store(IDLE, Ordering::Release);
    }
}

fn encode_photo(rgb: &[u8], size: [u32; 2]) -> Result<Vec<u8>, String> {
    use image::ImageEncoder;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(&mut png, image::codecs::png::CompressionType::Fast, image::codecs::png::FilterType::Adaptive)
        .write_image(rgb, size[0], size[1], image::ExtendedColorType::Rgb8)
        .map_err(|error| format!("Cannot encode freeze photo: {error}"))?;
    Ok(png)
}

fn read_photo(job: &PhotoJob, stop: &std::sync::atomic::AtomicBool) -> Result<Vec<u8>, String> {
    let mapped = Arc::new(AtomicU8::new(PENDING));
    let callback = mapped.clone();
    job.buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        callback.store(if result.is_ok() { READY } else { FAILED }, Ordering::Release);
    });
    let mut stopping_since = None;
    loop {
        let _ = job.device.poll(wgpu::PollType::Poll);
        match mapped.load(Ordering::Acquire) {
            READY => {
                let view = job.buffer.slice(..).get_mapped_range().map_err(|error| format!("Cannot read mapped freeze photo: {error}"))?;
                return photo_rgb(&view, job.size, job.row);
            }
            FAILED => return Err("GPU freeze-photo readback failed".into()),
            _ => {}
        }
        if stop.load(Ordering::Acquire) {
            let since = stopping_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() >= std::time::Duration::from_secs(5) {
                return Err("GPU freeze-photo readback did not finish within 5 seconds of shutdown".into());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn prepare_photo_directory(status: &FreezePhotoStatus) -> Result<std::path::PathBuf, String> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty()).ok_or("HOME is unset; cannot locate Pictures/Stream Engine/Freeze Frames")?;
    let directory = std::path::PathBuf::from(home).join("Pictures/Stream Engine/Freeze Frames");
    *status.directory.lock() = directory.display().to_string();
    std::fs::create_dir_all(&directory).map_err(|error| format!("Cannot create freeze-photo directory {}: {error}", directory.display()))?;
    Ok(directory)
}

fn save_photo(png: &[u8], request: PhotoRequest, context: usize, status: &FreezePhotoStatus) -> Result<std::path::PathBuf, String> {
    write_photo(&prepare_photo_directory(status)?, png, request, context)
}

fn write_photo(directory: &std::path::Path, png: &[u8], request: PhotoRequest, context: usize) -> Result<std::path::PathBuf, String> {
    use std::io::Write;
    std::fs::create_dir_all(directory).map_err(|error| format!("Cannot create freeze-photo directory {}: {error}", directory.display()))?;
    for collision in 0..100 {
        let path = directory.join(format!("freeze-{}-{:09}-{}-{:06}-ctx{context}-{collision}.png", request.seconds, request.nanos, std::process::id(), request.sequence));
        let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Cannot create freeze photo {}: {error}", path.display())),
        };
        if let Err(error) = file.write_all(png).and_then(|_| file.flush()) {
            drop(file);
            let cleanup = std::fs::remove_file(&path).err();
            return Err(format!("Cannot write freeze photo {}: {error}; partial-file cleanup: {cleanup:?}", path.display()));
        }
        return Ok(path);
    }
    Err(format!("Cannot choose a unique freeze-photo filename in {}", directory.display()))
}

#[cfg(test)]
mod photo_tests {
    use super::*;

    #[test]
    fn photo_layout_rejects_invalid_or_unbounded_dimensions() {
        assert_eq!(photo_layout([0, 1080]), None);
        assert_eq!(photo_layout([u32::MAX, 1]), None);
        assert_eq!(photo_layout([16384, 16384]), None);
        assert_eq!(photo_layout([65, 2]), Some((512, 1024)));
    }

    #[test]
    fn photo_rgb_removes_gpu_padding_and_shader_bookkeeping_alpha() {
        let mut bytes = vec![0xEE; 512];
        bytes[..8].copy_from_slice(&[10, 20, 30, 0xA5, 40, 50, 60, 0]);
        bytes[256..264].copy_from_slice(&[70, 80, 90, 255, 100, 110, 120, 3]);
        assert_eq!(photo_rgb(&bytes, [2, 2], 256).unwrap(), vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120]);
        assert!(photo_rgb(&bytes[..511], [2, 2], 256).is_err());
        assert!(photo_rgb(&bytes, [2, 2], 8).is_err());
    }

    #[test]
    fn photo_png_roundtrips_and_never_overwrites_an_existing_photo() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("Freeze Frames");
        let rgb = [15, 25, 35, 45, 55, 65];
        let png = encode_photo(&rgb, [2, 1]).unwrap();
        let request = PhotoRequest { seconds: 123, nanos: 456, sequence: 7 };
        let first = write_photo(&directory, &png, request, 0).unwrap();
        let second = write_photo(&directory, &png, request, 0).unwrap();
        assert_ne!(first, second);
        assert_eq!(image::open(first).unwrap().to_rgb8().as_raw(), &rgb);
        assert_eq!(image::open(second).unwrap().to_rgb8().as_raw(), &rgb);
    }

    #[test]
    fn photo_directory_failure_names_the_path() {
        let root = tempfile::tempdir().unwrap();
        let blocker = root.path().join("not-a-directory");
        std::fs::write(&blocker, b"occupied").unwrap();
        let request = PhotoRequest { seconds: 0, nanos: 0, sequence: 0 };
        let error = write_photo(&blocker, b"", request, 0).unwrap_err();
        assert!(error.contains(&blocker.display().to_string()));
        assert!(error.contains("Cannot create freeze-photo directory"));
    }
}
