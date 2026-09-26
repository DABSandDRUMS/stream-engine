//! Real dmabuf import on the local GPU. A second Vulkan device (plain ash, like the engine)
//! exports DRM-format-modifier images filled with known colors, releases them to
//! `VK_QUEUE_FAMILY_FOREIGN_EXT` in `GENERAL` and exports a sync_file; the fds travel through a
//! fake engine socket into [`Frames`]. The imported textures are verified by drawing them with
//! egui's own renderer into an offscreen target — the only readback, and it lives in this test.
//! Skips (with a printed reason) when no suitable Vulkan device exists.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ash::vk;
use eframe::egui_wgpu;
use wgpu::hal::api::Vulkan;

use super::core::Core;
use super::gpu::modifier_support;
use super::proto::{self, CanvasDesc, Hello, Release};
use super::tests::{PairConnector, Server, desc, frame, socketpair};
use super::testutil::memfd_with;
use super::{Canvas, Frames, Transport};

const WAIT: Duration = Duration::from_secs(5);

struct Exporter {
    _entry: ash::Entry,
    instance: ash::Instance,
    pdev: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    family: u32,
    pool: vk::CommandPool,
    mem_fd: ash::khr::external_memory_fd::Device,
    sem_fd: ash::khr::external_semaphore_fd::Device,
    drm: ash::ext::image_drm_format_modifier::Device,
    images: Vec<(vk::Image, vk::DeviceMemory)>,
    semaphores: Vec<vk::Semaphore>,
    fences: Vec<vk::Fence>,
}

/// One exported buffer: the dmabuf plus its plane layout.
struct Exported {
    fd: OwnedFd,
    modifier: u64,
    offset: u32,
    stride: u32,
}

impl Exporter {
    /// A device on the same physical GPU as `adapter` (matched by vendor/device id).
    fn new(adapter: &wgpu::Adapter) -> Result<Self, String> {
        let info = adapter.get_info();
        // SAFETY: standard Vulkan bring-up; every handle is destroyed in Drop.
        unsafe {
            let entry = ash::Entry::load().map_err(|e| format!("no Vulkan loader: {e}"))?;
            let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
            let instance =
                entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None).map_err(|e| format!("vkCreateInstance: {e}"))?;
            let pdev = instance
                .enumerate_physical_devices()
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|&p| {
                    let props = instance.get_physical_device_properties(p);
                    props.vendor_id == info.vendor && props.device_id == info.device
                })
                .ok_or("exporter: no physical device matching the wgpu adapter")?;
            let family = instance
                .get_physical_device_queue_family_properties(pdev)
                .iter()
                .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                .ok_or("exporter: no graphics queue")? as u32;
            let supported: Vec<String> = instance
                .enumerate_device_extension_properties(pdev)
                .map_err(|e| e.to_string())?
                .iter()
                .map(|e| e.extension_name_as_c_str().unwrap_or_default().to_string_lossy().into_owned())
                .collect();
            let exts = [
                ash::khr::external_memory_fd::NAME,
                ash::ext::external_memory_dma_buf::NAME,
                ash::ext::image_drm_format_modifier::NAME,
                ash::khr::external_semaphore_fd::NAME,
                ash::ext::queue_family_foreign::NAME,
            ];
            for e in exts {
                if !supported.iter().any(|s| s.as_bytes() == e.to_bytes()) {
                    instance.destroy_instance(None);
                    return Err(format!("exporter: {} unsupported", e.to_string_lossy()));
                }
            }
            let ext_ptrs: Vec<_> = exts.iter().map(|e| e.as_ptr()).collect();
            let prio = [1.0];
            let qinfo = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&prio)];
            let device = instance
                .create_device(pdev, &vk::DeviceCreateInfo::default().queue_create_infos(&qinfo).enabled_extension_names(&ext_ptrs), None)
                .map_err(|e| format!("exporter vkCreateDevice: {e}"))?;
            let queue = device.get_device_queue(family, 0);
            let pool = device.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(family), None).map_err(|e| e.to_string())?;
            Ok(Self {
                mem_fd: ash::khr::external_memory_fd::Device::new(&instance, &device),
                sem_fd: ash::khr::external_semaphore_fd::Device::new(&instance, &device),
                drm: ash::ext::image_drm_format_modifier::Device::new(&instance, &device),
                _entry: entry,
                instance,
                pdev,
                device,
                queue,
                family,
                pool,
                images: Vec::new(),
                semaphores: Vec::new(),
                fences: Vec::new(),
            })
        }
    }

    /// Creates an exportable R8G8B8A8_UNORM image with one of `modifiers` (the driver picks),
    /// clears it to `rgba`, releases it to the foreign queue family in GENERAL and returns the
    /// dmabuf plus a sync_file signaled when that work completes (`None` if already signaled).
    fn export(&mut self, modifiers: &[u64], width: u32, height: u32, rgba: [u8; 4]) -> Result<(Exported, Option<OwnedFd>), String> {
        // SAFETY: all handles are created here and tracked for destruction in Drop.
        unsafe {
            let d = &self.device;
            let mut ext = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let mut list = vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(modifiers);
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk::Format::R8G8B8A8_UNORM)
                .extent(vk::Extent3D { width, height, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::COLOR_ATTACHMENT)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut ext)
                .push_next(&mut list);
            let image = d.create_image(&info, None).map_err(|e| format!("export vkCreateImage: {e}"))?;
            let req = d.get_image_memory_requirements(image);
            let mem_props = self.instance.get_physical_device_memory_properties(self.pdev);
            let type_index = (0..mem_props.memory_type_count)
                .find(|&i| {
                    req.memory_type_bits & (1 << i) != 0 && mem_props.memory_types[i as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                })
                .ok_or("no device-local memory type")?;
            let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let memory = d
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(type_index).push_next(&mut export).push_next(&mut dedicated),
                    None,
                )
                .map_err(|e| format!("export vkAllocateMemory: {e}"))?;
            self.images.push((image, memory));
            d.bind_image_memory(image, memory, 0).map_err(|e| e.to_string())?;
            let mut props = vk::ImageDrmFormatModifierPropertiesEXT::default();
            self.drm.get_image_drm_format_modifier_properties(image, &mut props).map_err(|e| e.to_string())?;
            let layout = d.get_image_subresource_layout(
                image,
                vk::ImageSubresource { aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT, mip_level: 0, array_layer: 0 },
            );
            let fd = self
                .mem_fd
                .get_memory_fd(&vk::MemoryGetFdInfoKHR::default().memory(memory).handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT))
                .map_err(|e| format!("vkGetMemoryFdKHR: {e}"))?;

            let cb = d
                .allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(self.pool).command_buffer_count(1))
                .map_err(|e| e.to_string())?[0];
            d.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))
                .map_err(|e| e.to_string())?;
            let range =
                vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 };
            let to_dst = vk::ImageMemoryBarrier::default()
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(range);
            d.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst],
            );
            let color = vk::ClearColorValue { float32: rgba.map(|c| f32::from(c) / 255.0) };
            d.cmd_clear_color_image(cb, image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &color, &[range]);
            // What the engine does after its final pass: release to FOREIGN in GENERAL.
            let release = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(self.family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .image(image)
                .subresource_range(range);
            d.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[release],
            );
            d.end_command_buffer(cb).map_err(|e| e.to_string())?;

            let mut export_sem = vk::ExportSemaphoreCreateInfo::default().handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
            let sem = d.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut export_sem), None).map_err(|e| e.to_string())?;
            self.semaphores.push(sem);
            let fence = d.create_fence(&vk::FenceCreateInfo::default(), None).map_err(|e| e.to_string())?;
            self.fences.push(fence);
            let cbs = [cb];
            let sems = [sem];
            d.queue_submit(self.queue, &[vk::SubmitInfo::default().command_buffers(&cbs).signal_semaphores(&sems)], fence)
                .map_err(|e| format!("vkQueueSubmit: {e}"))?;
            let sync = self
                .sem_fd
                .get_semaphore_fd(&vk::SemaphoreGetFdInfoKHR::default().semaphore(sem).handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD))
                .map_err(|e| format!("vkGetSemaphoreFdKHR(SYNC_FD): {e}"))?;
            let sync = (sync >= 0).then(|| OwnedFd::from_raw_fd(sync));
            Ok((
                Exported { fd: OwnedFd::from_raw_fd(fd), modifier: props.drm_format_modifier, offset: layout.offset as u32, stride: layout.row_pitch as u32 },
                sync,
            ))
        }
    }

    /// Modifiers of R8G8B8A8_UNORM that this device can export as a single-plane dmabuf usable
    /// for clears and sampling.
    fn exportable_modifiers(&self, width: u32, height: u32) -> Vec<u64> {
        let pdev = self.pdev;
        // SAFETY: queries on live handles; out-structs outlive the calls.
        unsafe {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            self.instance.get_physical_device_format_properties2(pdev, vk::Format::R8G8B8A8_UNORM, &mut props);
            let mut entries = vec![vk::DrmFormatModifierPropertiesEXT::default(); list.drm_format_modifier_count as usize];
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut entries);
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            self.instance.get_physical_device_format_properties2(pdev, vk::Format::R8G8B8A8_UNORM, &mut props);
            entries
                .iter()
                .filter(|e| e.drm_format_modifier_plane_count == 1 && e.drm_format_modifier_tiling_features.contains(vk::FormatFeatureFlags::TRANSFER_DST))
                .map(|e| e.drm_format_modifier)
                .filter(|&m| {
                    let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default().drm_format_modifier(m).sharing_mode(vk::SharingMode::EXCLUSIVE);
                    let mut ext = vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
                    let info = vk::PhysicalDeviceImageFormatInfo2::default()
                        .format(vk::Format::R8G8B8A8_UNORM)
                        .ty(vk::ImageType::TYPE_2D)
                        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                        .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::COLOR_ATTACHMENT)
                        .push_next(&mut drm)
                        .push_next(&mut ext);
                    let mut ext_props = vk::ExternalImageFormatProperties::default();
                    let mut out = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
                    let ok = self.instance.get_physical_device_image_format_properties2(pdev, &info, &mut out).is_ok();
                    let max = out.image_format_properties.max_extent;
                    ok && max.width >= width
                        && max.height >= height
                        && ext_props.external_memory_properties.external_memory_features.contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
                })
                .collect()
        }
    }
}

impl Drop for Exporter {
    fn drop(&mut self) {
        // SAFETY: waits for all work, then destroys everything created by this exporter.
        unsafe {
            let _ = self.device.device_wait_idle();
            for f in self.fences.drain(..) {
                self.device.destroy_fence(f, None);
            }
            for s in self.semaphores.drain(..) {
                self.device.destroy_semaphore(s, None);
            }
            for (i, m) in self.images.drain(..) {
                self.device.destroy_image(i, None);
                self.device.free_memory(m, None);
            }
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// Headless UI-side setup: Vulkan adapter, device from [`super::open_device`], egui renderer.
fn render_state() -> Result<egui_wgpu::RenderState, String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN, ..wgpu::InstanceDescriptor::new_without_display_handle() });
    let adapter = pollster_block_on(
        instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() }),
    )
    .map_err(|e| format!("no Vulkan adapter: {e}"))?;
    if !adapter.features().contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF) {
        return Err(format!("{} lacks VULKAN_EXTERNAL_MEMORY_DMA_BUF", adapter.get_info().name));
    }
    let desc = super::device_descriptor(&adapter);
    assert!(desc.required_features.contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF));
    assert_eq!(desc.required_limits.max_texture_dimension_2d, 8192);
    let (device, queue) = super::open_device(&adapter).map_err(|e| format!("open_device: {e}"))?;
    // eframe's preferred surface format; egui then outputs gamma-space values directly.
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let renderer = egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions { dithering: false, ..Default::default() });
    Ok(egui_wgpu::RenderState {
        adapter,
        available_adapters: Vec::new(),
        instance,
        device,
        queue,
        target_format: format,
        renderer: Arc::new(egui::mutex::RwLock::new(renderer)),
        surface_config: egui_wgpu::SurfaceConfig::HIGH_THROUGHPUT,
    })
}

fn pollster_block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}

/// Test-only readback: draws `id` full-screen with egui's renderer into a `target_format`
/// target and returns the center pixel (sRGB-encoded bytes, like the source).
fn draw_and_read(rs: &egui_wgpu::RenderState, id: egui::TextureId) -> [u8; 4] {
    let (device, queue) = (&rs.device, &rs.queue);
    let n = 16u32;
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test target"),
        size: wgpu::Extent3d { width: n, height: n, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: rs.target_format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(n as f32, n as f32));
    let mut mesh = egui::Mesh::with_texture(id);
    mesh.add_rect_with_uv(rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
    let jobs = [egui::ClippedPrimitive { clip_rect: rect, primitive: egui::epaint::Primitive::Mesh(mesh) }];
    let sd = egui_wgpu::ScreenDescriptor { size_in_pixels: [n, n], pixels_per_point: 1.0 };
    let mut enc = device.create_command_encoder(&Default::default());
    let mut renderer = rs.renderer.write();
    let extra = renderer.update_buffers(device, queue, &mut enc, &jobs, &sd);
    {
        let pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("test draw"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            ..Default::default()
        });
        renderer.render(&mut pass.forget_lifetime(), &jobs, &sd);
    }
    drop(renderer);
    let row = 256u32;
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(row * n),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    enc.copy_texture_to_buffer(
        target.as_image_copy(),
        wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(n) } },
        wgpu::Extent3d { width: n, height: n, depth_or_array_layers: 1 },
    );
    queue.submit(extra.into_iter().chain([enc.finish()]));
    buf.slice(..).map_async(wgpu::MapMode::Read, |r| r.unwrap());
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let data = buf.slice(..).get_mapped_range().expect("mapped");
    let at = (row * (n / 2) + 4 * (n / 2)) as usize;
    [data[at], data[at + 1], data[at + 2], data[at + 3]]
}

fn assert_close(got: [u8; 4], want: [u8; 4], what: &str) {
    let ok = got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 2);
    assert!(ok, "{what}: drew {got:?}, expected {want:?}");
}

/// Runs `update()` until `done` holds (the UI frame loop).
fn update_until(f: &mut Frames, rs: &egui_wgpu::RenderState, what: &str, mut done: impl FnMut(&Frames) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        for c in [Canvas::Wide, Canvas::Tall] {
            f.want(c, 60.0);
        }
        f.update(rs);
        if done(f) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn dmabuf_frames_import_and_sample_on_the_real_gpu() {
    let rs = match render_state() {
        Ok(rs) => rs,
        Err(why) => return eprintln!("skipping dmabuf GPU test: {why}"),
    };
    let mut exporter = match Exporter::new(&rs.adapter) {
        Ok(e) => e,
        Err(why) => return eprintln!("skipping dmabuf GPU test: {why}"),
    };
    // SAFETY: read-only queries of the UI device's Vulkan handles.
    let (foreign, importable) = unsafe {
        let hal = rs.device.as_hal::<Vulkan>().expect("Vulkan device");
        let foreign = hal.enabled_device_extensions().contains(&ash::ext::queue_family_foreign::NAME);
        let (instance, pdev) = (hal.shared_instance().raw_instance(), hal.raw_physical_device());
        let extent = vk::Extent2D { width: 64, height: 48 };
        let importable: Vec<u64> = exporter
            .exportable_modifiers(64, 48)
            .into_iter()
            .filter(|&m| modifier_support(instance, pdev, vk::Format::R8G8B8A8_UNORM, m, extent).is_some())
            .collect();
        (foreign, importable)
    };
    assert!(foreign, "open_device must enable VK_EXT_queue_family_foreign");
    let tiled: Vec<u64> = importable.iter().copied().filter(|&m| m != 0).collect();
    let mut runs = Vec::new();
    if !tiled.is_empty() {
        runs.push(tiled); // driver's choice among vendor-tiled modifiers (the engine's "auto")
    }
    if importable.contains(&0) {
        runs.push(vec![0]); // DRM_FORMAT_MOD_LINEAR
    }
    if runs.is_empty() {
        return eprintln!("skipping dmabuf GPU test: no modifier is both exportable and importable");
    }

    for (run, modifiers) in runs.iter().enumerate() {
        let (w, h) = (64, 48);
        let colors = [[200u8, 100, 50, 255], [20, 180, 240, 255]];
        let mut exported = Vec::new();
        let mut fences = Vec::new();
        for c in colors {
            let (e, fence) = exporter.export(modifiers, w, h, c).expect("export");
            exported.push(e);
            fences.push(fence);
        }
        let modifier = exported[0].modifier;
        assert!(exported.iter().all(|e| e.modifier == modifier && e.stride == exported[0].stride && e.offset == exported[0].offset));
        eprintln!(
            "run {run}: modifier {modifier:#x}, stride {}, offset {}, sync_file fences {}/2",
            exported[0].stride,
            exported[0].offset,
            fences.iter().flatten().count()
        );

        let (conns, rx) = crossbeam_channel::unbounded();
        let (client, server) = socketpair();
        conns.send(client).unwrap();
        let srv = Server(server);
        let (core, io_thread) = Core::start(Box::new(PairConnector(rx)), egui::Context::default());
        let mut frames = Frames { socket: "test".into(), core, caps: None, io_thread };
        assert_eq!(srv.expect_hello(), Hello { client: proto::CLIENT_UI, want: 0, flags: 0 });
        update_until(&mut frames, &rs, "hello", |_| true);
        assert_eq!(srv.expect_hello(), Hello { client: proto::CLIENT_UI, want: 0b11, flags: proto::FLAG_DMABUF });
        assert!(frames.stats().dmabuf_enabled);

        let d = CanvasDesc {
            modifier,
            offsets: [exported[0].offset, 0, 0, 0],
            strides: [exported[0].stride, 0, 0, 0],
            ..desc(0, 1, w, h, 0, proto::DRM_FORMAT_ABGR8888, 2)
        };
        srv.send_owned(&d.encode(), exported.into_iter().map(|e| e.fd).collect());
        update_until(&mut frames, &rs, "dmabuf import", |f| f.stats().canvases[0].transport == Transport::Dmabuf);
        let s = frames.stats();
        assert_eq!((s.dmabuf_imports, s.canvases[0].modifier, s.last_error.as_deref()), (2, modifier, None));

        for (i, fence) in fences.into_iter().enumerate() {
            let seq = i as u64 + 1;
            let bytes = frame(0, i as u32, seq, 1, fence.is_some());
            srv.send_owned(&bytes, fence.into_iter().collect());
            update_until(&mut frames, &rs, "dmabuf frame shown", |f| f.texture(Canvas::Wide).is_some_and(|t| t.seq == seq));
            let t = frames.texture(Canvas::Wide).unwrap();
            assert_eq!((t.transport, t.size, t.stale), (Transport::Dmabuf, [w, h], false));
            assert_close(draw_and_read(&rs, t.id), colors[i], &format!("run {run} buffer {i}"));
        }
        // Buffer 0 went back once the GPU finished the frames that sampled it.
        let deadline = Instant::now() + WAIT;
        let release = loop {
            frames.want(Canvas::Wide, 60.0);
            frames.update(&rs);
            if let Some(m) = srv.recv(Duration::from_millis(5)) {
                break m;
            }
            assert!(Instant::now() < deadline, "no release after the switch");
        };
        assert_eq!(release, proto::testing::ClientMsg::Release(Release { canvas: 0, buffer: 0, seq: 1 }));

        // The shm fallback on the same device: memfd rows uploaded with write_texture.
        let shm = desc(1, 1, 8, 4, 40, 0, 1);
        let color = [30u8, 60, 90, 255];
        let mut rows = vec![0u8; 40 * 4];
        for r in rows.chunks_mut(40) {
            for px in r[..32].chunks_mut(4) {
                px.copy_from_slice(&color);
            }
        }
        srv.send_owned(&shm.encode(), vec![memfd_with("se-gpu-shm", &rows)]);
        srv.send(&frame(1, 0, 1, 1, false), &[]);
        update_until(&mut frames, &rs, "shm frame shown", |f| f.texture(Canvas::Tall).is_some_and(|t| t.seq == 1));
        let t = frames.texture(Canvas::Tall).unwrap();
        assert_eq!(t.transport, Transport::Shm);
        assert_close(draw_and_read(&rs, t.id), color, "shm");
        let s = frames.stats();
        assert_eq!((s.dmabuf_frames_presented, s.shm_frames_presented, s.shm_bytes_uploaded), (2, 1, 8 * 4 * 4));
        drop(frames);
        assert!(srv.closed_within(WAIT));
    }
}
