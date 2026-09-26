//! Headless GPU setup and readback shared by the GPU tests and the bench example.
#![allow(dead_code, reason = "each consumer uses a different subset")]

use std::sync::LazyLock;

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub name: String,
}

static GPU: LazyLock<Gpu> = LazyLock::new(|| {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN, ..wgpu::InstanceDescriptor::new_without_display_handle() });
    let want = std::env::var("SE_GPU").unwrap_or_else(|_| "NVIDIA".to_owned());
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
    let names: Vec<String> = adapters.iter().map(|a| a.get_info().name).collect();
    let adapter =
        adapters.into_iter().find(|a| a.get_info().name.contains(&want)).unwrap_or_else(|| panic!("no Vulkan adapter matching {want:?} (found {names:?})"));
    let name = adapter.get_info().name;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("se-vector tests"),
        required_features: adapter.features() & wgpu::Features::CLEAR_TEXTURE,
        required_limits: wgpu::Limits::default(),
        ..Default::default()
    }))
    .expect("request device");
    Gpu { device, queue, name }
});

/// The Vulkan adapter whose name contains `SE_GPU` (default "NVIDIA"), shared per process.
pub fn gpu() -> &'static Gpu {
    &GPU
}

pub struct Target {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub size: [u32; 2],
}

impl Target {
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Self {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("se-vector target"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: se_vector::required_texture_usage() | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self { texture, view, size: [width, height] }
    }

    pub fn read(&self, gpu: &Gpu) -> Pixels {
        let [w, h] = self.size;
        let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("se-vector readback"),
            size: u64::from(row * h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            self.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) } },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        gpu.queue.submit([encoder.finish()]);
        buffer.map_async(wgpu::MapMode::Read, .., |r| r.expect("map readback buffer"));
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let mapped = buffer.get_mapped_range(..).expect("mapped range");
        let mut data = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            let line = &mapped[(y * row) as usize..][..(w * 4) as usize];
            data.extend_from_slice(line.as_chunks::<4>().0);
        }
        Pixels { width: w, height: h, data }
    }
}

pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub data: Vec<[u8; 4]>,
}

impl Pixels {
    pub fn at(&self, x: u32, y: u32) -> [u8; 4] {
        self.data[(y * self.width + x) as usize]
    }

    /// Inclusive bounding box `[x0, y0, x1, y1]` of pixels with alpha > 0.
    pub fn ink_bbox(&self) -> Option<[u32; 4]> {
        let mut bbox: Option<[u32; 4]> = None;
        for y in 0..self.height {
            for x in 0..self.width {
                if self.at(x, y)[3] > 0 {
                    let b = bbox.get_or_insert([x, y, x, y]);
                    b[0] = b[0].min(x);
                    b[1] = b[1].min(y);
                    b[2] = b[2].max(x);
                    b[3] = b[3].max(y);
                }
            }
        }
        bbox
    }

    pub fn ink_count(&self) -> usize {
        self.data.iter().filter(|p| p[3] > 0).count()
    }
}
