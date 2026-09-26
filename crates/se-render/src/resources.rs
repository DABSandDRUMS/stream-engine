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
    pub sim: wgpu::BindGroupLayout,
    pub node: wgpu::BindGroupLayout,
    pub node_tex: wgpu::BindGroupLayout,
    pub convert: wgpu::BindGroupLayout,
    pub blit: wgpu::BindGroupLayout,
    pub flash: wgpu::BindGroupLayout,
    pub fx_pipeline: wgpu::PipelineLayout,
    pub patch_pipeline: wgpu::PipelineLayout,
    pub sim_pipeline: wgpu::PipelineLayout,
    pub linear: wgpu::Sampler,
    pub nearest: wgpu::Sampler,
    /// 1×1 transparent 2D texture and 2×2×2 identity 3D LUT (unused bindings).
    pub dummy_2d: wgpu::TextureView,
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
            sim,
            node,
            node_tex,
            convert,
            blit,
            flash,
            fx_pipeline,
            patch_pipeline,
            sim_pipeline,
            linear: samp("linear", wgpu::FilterMode::Linear),
            nearest: samp("nearest", wgpu::FilterMode::Nearest),
            dummy_2d: dummy2.create_view(&Default::default()),
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

/// Host-visible buffers cycled between CPU and GPU with `map_async` (no per-frame buffer
/// creation). `MAP_WRITE` rings feed uploads; `MAP_READ` rings carry small readbacks.
pub struct MappedRing {
    pub buffers: Vec<wgpu::Buffer>,
    state: Vec<Arc<AtomicU8>>,
    /// Buffer index handed out this frame (re-mapped after submit).
    in_flight: Vec<bool>,
    next: usize,
    pub size: u64,
    write: bool,
    /// Frame counter each readback buffer was filled on.
    pub tags: Vec<u64>,
}

impl MappedRing {
    pub fn new(device: &wgpu::Device, label: &str, count: usize, size: u64, write: bool) -> MappedRing {
        let usage =
            if write { wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC } else { wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST };
        let buffers: Vec<wgpu::Buffer> = (0..count)
            .map(|i| device.create_buffer(&wgpu::BufferDescriptor { label: Some(&format!("{label}#{i}")), size, usage, mapped_at_creation: write }))
            .collect();
        MappedRing {
            buffers,
            state: (0..count).map(|_| Arc::new(AtomicU8::new(if write { READY } else { IDLE }))).collect(),
            in_flight: vec![false; count],
            next: 0,
            size,
            write,
            tags: vec![0; count],
        }
    }

    /// Write ring: a mapped buffer ready for CPU writes.
    pub fn acquire_write(&mut self) -> Option<usize> {
        let n = self.buffers.len();
        for k in 0..n {
            let i = (self.next + k) % n;
            if !self.in_flight[i] && self.state[i].load(Ordering::Acquire) == READY {
                self.next = (i + 1) % n;
                self.in_flight[i] = true;
                return Some(i);
            }
        }
        None
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
        let mode = if self.write { wgpu::MapMode::Write } else { wgpu::MapMode::Read };
        for i in 0..self.buffers.len() {
            if !self.in_flight[i] {
                continue;
            }
            self.in_flight[i] = false;
            let st = self.state[i].clone();
            st.store(PENDING, Ordering::Release);
            let _p = se_alloc::Pause::new();
            self.buffers[i].slice(..).map_async(mode, move |r| st.store(if r.is_ok() { READY } else { FAILED }, Ordering::Release));
        }
    }

    /// Give back a write buffer that was acquired but not used (still mapped).
    pub fn cancel(&mut self, i: usize) {
        self.in_flight[i] = false;
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
