//! wgpu/Vulkan side: device creation with dmabuf import enabled, dmabuf import via
//! `VK_EXT_image_drm_format_modifier`, the foreign → UI queue family acquire barrier, and the
//! shm upload texture.

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use ash::vk;
use eframe::egui_wgpu;
use wgpu::hal::Device as _;
use wgpu::hal::api::Vulkan;

use super::core::{Gpu, GpuBuffer as GpuBufferTrait, fourcc_name};
use super::proto::{CanvasDesc, DRM_FORMAT_ABGR8888};

/// Texture format of imported and uploaded frames. The memory holds sRGB-encoded RGBA8 and
/// egui-wgpu 0.36 samples user textures as gamma-space `Rgba8Unorm` (its shader treats the sample
/// as gamma and converts for sRGB framebuffers itself), so the bytes are viewed as UNORM — which is
/// also exactly the format the engine exports.
const IMPORT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const IMPORT_VK_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// Device features used for dmabuf import (enabled when the adapter has them).
const IMPORT_FEATURES: wgpu::Features = wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF.union(wgpu::Features::VULKAN_EXTERNAL_MEMORY_FD);

/// egui-wgpu's default device request (see `WgpuSetupCreateNew::without_display_handle`) plus the
/// Vulkan external-memory features needed to import engine dmabufs, when the adapter has them.
pub fn device_descriptor(adapter: &wgpu::Adapter) -> wgpu::DeviceDescriptor<'static> {
    let base_limits = if adapter.get_info().backend == wgpu::Backend::Gl { wgpu::Limits::downlevel_webgl2_defaults() } else { wgpu::Limits::default() };
    wgpu::DeviceDescriptor {
        label: Some("se-ui device"),
        required_features: adapter.features() & IMPORT_FEATURES,
        required_limits: wgpu::Limits { max_texture_dimension_2d: 8192, ..base_limits },
        // Do not reserve a 64 MiB staging block from the renderer/OBS BAR1 window.
        memory_hints: wgpu::MemoryHints::MemoryUsage,
        ..Default::default()
    }
}

/// Opens a device on `adapter` with [`device_descriptor`]. On Vulkan it additionally enables
/// `VK_EXT_queue_family_foreign` (wgpu does not), which the acquire of engine images from
/// `VK_QUEUE_FAMILY_FOREIGN_EXT` requires.
pub fn open_device(adapter: &wgpu::Adapter) -> Result<(wgpu::Device, wgpu::Queue), wgpu::RequestDeviceError> {
    let desc = device_descriptor(adapter);
    // SAFETY: the hal adapter is only used to open a device with the same features/limits wgpu
    // would request, plus one extra extension that does not change wgpu-visible behavior.
    let opened = unsafe {
        adapter.as_hal::<Vulkan>().and_then(|hal| {
            let foreign = ash::ext::queue_family_foreign::NAME;
            if !hal.physical_device_capabilities().supports_extension(foreign) {
                return None;
            }
            let add_foreign: Box<wgpu::hal::vulkan::CreateDeviceCallback<'_>> = Box::new(|args| args.extensions.push(foreign));
            Some(hal.open_with_callback(desc.required_features, &desc.required_limits, &desc.memory_hints, Some(add_foreign)))
        })
    };
    match opened {
        // SAFETY: `open` was created from this adapter with exactly `desc`'s features/limits.
        Some(Ok(open)) => unsafe { adapter.create_device_from_hal(open, &desc) },
        Some(Err(e)) => {
            tracing::warn!(target: "se_ui::frames", "opening the Vulkan device with VK_EXT_queue_family_foreign failed ({e}); using wgpu defaults");
            block_on(adapter.request_device(&desc))
        }
        None => block_on(adapter.request_device(&desc)),
    }
}

/// eframe wgpu configuration for the UI: a Vulkan device from [`open_device`] (dmabuf import +
/// foreign queue family), falling back to egui's own device creation with [`device_descriptor`]
/// when no Vulkan adapter is usable (frames then arrive over the shm fallback).
pub fn wgpu_options() -> egui_wgpu::WgpuConfiguration {
    let wgpu_setup = match vulkan_setup() {
        Ok(existing) => egui_wgpu::WgpuSetup::Existing(existing),
        Err(e) => {
            tracing::warn!(target: "se_ui::frames", "{e}; letting egui pick the GPU (frames use the shm fallback)");
            egui_wgpu::WgpuSetup::CreateNew(egui_wgpu::WgpuSetupCreateNew {
                device_descriptor: Arc::new(device_descriptor),
                ..egui_wgpu::WgpuSetupCreateNew::without_display_handle()
            })
        }
    };
    egui_wgpu::WgpuConfiguration { wgpu_setup, ..Default::default() }
}

fn vulkan_setup() -> Result<egui_wgpu::WgpuSetupExisting, String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        flags: wgpu::InstanceFlags::from_build_config().with_env(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::HighPerformance),
        ..Default::default()
    }))
    .map_err(|e| format!("no Vulkan adapter: {e}"))?;
    let (device, queue) = open_device(&adapter).map_err(|e| format!("cannot open the Vulkan device: {e}"))?;
    Ok(egui_wgpu::WgpuSetupExisting { instance, adapter, device, queue })
}

/// Drives a wgpu future to completion (native wgpu futures are ready on first poll).
fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}

/// What the UI device can do, probed once.
pub(crate) struct DeviceCaps {
    dmabuf: bool,
    /// Queue family the engine released its images to.
    src_family: u32,
    max_dim: u32,
}

impl DeviceCaps {
    pub fn probe(device: &wgpu::Device) -> Self {
        let max_dim = device.limits().max_texture_dimension_2d;
        let off = |why: &str| {
            tracing::info!(target: "se_ui::frames", "dmabuf import unavailable ({why}); frames use the shm fallback");
            Self { dmabuf: false, src_family: vk::QUEUE_FAMILY_FOREIGN_EXT, max_dim }
        };
        if !device.features().contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF) {
            return off("device created without VULKAN_EXTERNAL_MEMORY_DMA_BUF; use frames::wgpu_options()");
        }
        // SAFETY: read-only queries of the device's Vulkan handles.
        let Some(hal) = (unsafe { device.as_hal::<Vulkan>() }) else { return off("not a Vulkan device") };
        if hal.shared_instance().instance_api_version() < vk::API_VERSION_1_1 {
            return off("Vulkan instance older than 1.1");
        }
        let src_family = if hal.enabled_device_extensions().contains(&ash::ext::queue_family_foreign::NAME) {
            vk::QUEUE_FAMILY_FOREIGN_EXT
        } else {
            tracing::warn!(
                target: "se_ui::frames",
                "VK_EXT_queue_family_foreign is not enabled on the UI device; acquiring engine images from VK_QUEUE_FAMILY_EXTERNAL (create the device with frames::wgpu_options())"
            );
            vk::QUEUE_FAMILY_EXTERNAL
        };
        tracing::info!(target: "se_ui::frames", foreign = src_family == vk::QUEUE_FAMILY_FOREIGN_EXT, "dmabuf import enabled");
        Self { dmabuf: true, src_family, max_dim }
    }
}

/// One egui-registered texture: an imported engine dmabuf or the shm upload target.
pub(crate) struct GpuBuffer {
    texture: wgpu::Texture,
    id: egui::TextureId,
    /// The imported VkImage (null for shm textures).
    raw: vk::Image,
}

impl GpuBufferTrait for GpuBuffer {
    fn texture_id(&self) -> egui::TextureId {
        self.id
    }
}

pub(crate) struct WgpuGpu<'a> {
    rs: &'a egui_wgpu::RenderState,
    caps: &'a DeviceCaps,
    encoder: Option<wgpu::CommandEncoder>,
}

impl<'a> WgpuGpu<'a> {
    pub fn new(rs: &'a egui_wgpu::RenderState, caps: &'a DeviceCaps) -> Self {
        Self { rs, caps, encoder: None }
    }

    fn register(&self, texture: wgpu::Texture, filter: wgpu::FilterMode, raw: vk::Image) -> GpuBuffer {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let id = self.rs.renderer.write().register_native_texture(&self.rs.device, &view, filter);
        GpuBuffer { texture, id, raw }
    }
}

/// Whether `format` with DRM `modifier` can be imported from a dmabuf and sampled at `extent`.
/// `Some(linear_filter)` when it can.
///
/// # Safety
/// `instance`/`pdev` must be live handles of the same Vulkan ≥ 1.1 instance.
pub(crate) unsafe fn modifier_support(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    format: vk::Format,
    modifier: u64,
    extent: vk::Extent2D,
) -> Option<bool> {
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut props = vk::FormatProperties2::default().push_next(&mut list);
    unsafe { instance.get_physical_device_format_properties2(pdev, format, &mut props) };
    let count = list.drm_format_modifier_count as usize;
    let mut entries = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut entries);
    let mut props = vk::FormatProperties2::default().push_next(&mut list);
    unsafe { instance.get_physical_device_format_properties2(pdev, format, &mut props) };
    let entry = entries.iter().find(|e| e.drm_format_modifier == modifier)?;
    let features = entry.drm_format_modifier_tiling_features;
    if entry.drm_format_modifier_plane_count != 1 || !features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE) {
        return None;
    }
    let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default().drm_format_modifier(modifier).sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::SAMPLED)
        .push_next(&mut drm)
        .push_next(&mut external);
    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut out = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    unsafe { instance.get_physical_device_image_format_properties2(pdev, &info, &mut out) }.ok()?;
    let max = out.image_format_properties.max_extent;
    if extent.width > max.width || extent.height > max.height {
        return None;
    }
    let importable = external_props.external_memory_properties.external_memory_features.contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE);
    importable.then_some(features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR))
}

impl Gpu for WgpuGpu<'_> {
    type Buffer = GpuBuffer;

    fn maintain(&mut self) {
        let _ = self.rs.device.poll(wgpu::PollType::Poll);
    }

    fn dmabuf_capable(&self) -> bool {
        self.caps.dmabuf
    }

    fn import_dmabuf(&mut self, desc: &CanvasDesc, fds: Vec<OwnedFd>) -> Result<Vec<GpuBuffer>, String> {
        if !self.caps.dmabuf {
            return Err("the UI device cannot import dmabufs".into());
        }
        if desc.drm_fourcc != DRM_FORMAT_ABGR8888 {
            return Err(format!("unsupported DRM format {}", fourcc_name(desc.drm_fourcc)));
        }
        if desc.width > self.caps.max_dim || desc.height > self.caps.max_dim {
            return Err(format!("{}x{} exceeds the device limit {}", desc.width, desc.height, self.caps.max_dim));
        }
        let device = &self.rs.device;
        let size = wgpu::Extent3d { width: desc.width, height: desc.height, depth_or_array_layers: 1 };
        let (linear, raw_textures) = {
            // SAFETY: the hal device outlives this block; only queries and dmabuf imports are
            // made, and every created texture is either handed to wgpu or destroyed.
            let hal = unsafe { device.as_hal::<Vulkan>() }.ok_or("not a Vulkan device")?;
            let instance = hal.shared_instance().raw_instance();
            let pdev = hal.raw_physical_device();
            let extent = vk::Extent2D { width: desc.width, height: desc.height };
            let linear = unsafe { modifier_support(instance, pdev, IMPORT_VK_FORMAT, desc.modifier, extent) }
                .ok_or_else(|| format!("DRM modifier {:#x} is not importable as R8G8B8A8_UNORM at {}x{}", desc.modifier, desc.width, desc.height))?;
            let hal_desc = wgpu::hal::TextureDescriptor {
                label: Some("se-frames dmabuf"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: IMPORT_FORMAT,
                usage: wgpu::TextureUses::RESOURCE,
                memory_flags: wgpu::hal::MemoryFlags::empty(),
                view_formats: Vec::new(),
            };
            let mut raw_textures = Vec::with_capacity(fds.len());
            for (i, fd) in fds.into_iter().enumerate() {
                // SAFETY: fd is a dmabuf described by `desc` (single plane at offsets[0]/strides[0]);
                // the call takes ownership of it. On error the remaining fds drop (close) with the
                // iterator and the textures imported so far are destroyed.
                match unsafe { hal.texture_from_dmabuf_fd(fd, &hal_desc, desc.modifier, u64::from(desc.strides[0]), u64::from(desc.offsets[0])) } {
                    Ok(t) => raw_textures.push(t),
                    Err(e) => {
                        for t in raw_textures {
                            unsafe { hal.destroy_texture(t) };
                        }
                        return Err(format!("import of buffer {i} failed: {e}"));
                    }
                }
            }
            (linear, raw_textures)
        };
        let wgpu_desc = wgpu::TextureDescriptor {
            label: Some("se-frames dmabuf"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: IMPORT_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        let filter = if linear { wgpu::FilterMode::Linear } else { wgpu::FilterMode::Nearest };
        let buffers = raw_textures
            .into_iter()
            .map(|t| {
                // SAFETY: the handle stays valid as long as the wgpu texture wrapping it.
                let raw = unsafe { t.raw_handle() };
                // wgpu tracks the image as RESOURCE (SHADER_READ_ONLY_OPTIMAL) from the start.
                // It is first sampled only after the acquire barrier recorded at swap time, which
                // ends in exactly that layout, so wgpu never records a transition of its own.
                // SAFETY: `t` was created on this device from `wgpu_desc`'s hal equivalent.
                let texture = unsafe { device.create_texture_from_hal::<Vulkan>(t, &wgpu_desc, wgpu::TextureUses::RESOURCE) };
                self.register(texture, filter, raw)
            })
            .collect();
        Ok(buffers)
    }

    fn create_shm(&mut self, width: u32, height: u32) -> Result<GpuBuffer, String> {
        if width > self.caps.max_dim || height > self.caps.max_dim {
            return Err(format!("{width}x{height} exceeds the device limit {}", self.caps.max_dim));
        }
        let texture = self.rs.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("se-frames shm"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: IMPORT_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        Ok(self.register(texture, wgpu::FilterMode::Linear, vk::Image::null()))
    }

    fn acquire(&mut self, buffer: &GpuBuffer) {
        let device = &self.rs.device;
        let encoder = self.encoder.get_or_insert_with(|| device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("se-frames acquire") }));
        // SAFETY: records one barrier on wgpu's open command buffer for an image this module
        // owns; the barrier leaves the image in the layout wgpu tracks for it (see import).
        unsafe {
            let Some(hal) = device.as_hal::<Vulkan>() else { return };
            let barrier = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(self.caps.src_family)
                .dst_queue_family_index(hal.queue_family_index())
                .image(buffer.raw)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });
            encoder.as_hal_mut::<Vulkan, _, _>(|enc| {
                if let Some(enc) = enc {
                    hal.raw_device().cmd_pipeline_barrier(
                        enc.raw_handle(),
                        vk::PipelineStageFlags::TOP_OF_PIPE,
                        vk::PipelineStageFlags::FRAGMENT_SHADER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[barrier],
                    );
                }
            });
        }
    }

    fn upload(&mut self, buffer: &GpuBuffer, rows: &[u8], stride: u32, size: [u32; 2]) {
        self.rs.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &buffer.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            rows,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: Some(size[1]) },
            wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
        );
    }

    fn submit(&mut self) {
        if let Some(encoder) = self.encoder.take() {
            self.rs.queue.submit([encoder.finish()]);
        }
    }

    fn on_work_done(&mut self, callback: Box<dyn FnOnce() + Send + 'static>) {
        self.rs.queue.on_submitted_work_done(callback);
    }

    fn free(&mut self, buffer: GpuBuffer) {
        self.rs.renderer.write().free_texture(&buffer.id);
        // wgpu destroys the texture (and its imported memory) once submitted work is done.
        drop(buffer);
    }
}
