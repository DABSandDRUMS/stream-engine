//! dmabuf export for the test producer: RGBA8 Vulkan images with a DRM format modifier,
//! exported as dmabufs, cleared to the test pattern on the GPU, fenced with sync_files.

use std::ffi::{CStr, c_char};
use std::os::fd::{FromRawFd, OwnedFd};

use anyhow::{Context, Result, anyhow, bail};
use ash::vk;
use se_frames::RingDesc;
use se_frames::proto::DRM_FORMAT_ABGR8888;

const EXTENSIONS: [&CStr; 5] = [
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
    ash::khr::external_semaphore_fd::NAME,
];
const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

pub struct Gpu {
    _entry: ash::Entry,
    instance: ash::Instance,
    pdev: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    queue_family: u32,
    pool: vk::CommandPool,
    memory_fd: ash::khr::external_memory_fd::Device,
    semaphore_fd: ash::khr::external_semaphore_fd::Device,
    drm: ash::ext::image_drm_format_modifier::Device,
    /// sync_file export works: frames carry a fence instead of a CPU wait.
    sync_fd: bool,
    modifiers: Vec<u64>,
}

struct Buf {
    image: vk::Image,
    memory: vk::DeviceMemory,
    fd: OwnedFd,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    semaphore: vk::Semaphore,
    initialized: bool,
}

pub struct Ring {
    width: u32,
    height: u32,
    modifier: u64,
    offset: u64,
    stride: u64,
    bufs: Vec<Buf>,
}

impl Ring {
    pub fn desc(&self) -> Result<RingDesc> {
        Ok(RingDesc {
            width: self.width,
            height: self.height,
            drm_fourcc: DRM_FORMAT_ABGR8888,
            modifier: self.modifier,
            offsets: [u32::try_from(self.offset)?, 0, 0, 0],
            strides: [u32::try_from(self.stride)?, 0, 0, 0],
            planes: 1,
            fds: self.bufs.iter().map(|b| b.fd.try_clone()).collect::<std::io::Result<_>>()?,
        })
    }
}

impl Gpu {
    pub fn new() -> Result<Gpu> {
        // SAFETY: loading the system Vulkan loader.
        let entry = unsafe { ash::Entry::load() }.context("loading libvulkan")?;
        let app = vk::ApplicationInfo::default().application_name(c"se-frames-test-producer").api_version(vk::API_VERSION_1_3);
        // SAFETY: valid create info.
        let instance = unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None) }?;
        let wanted = std::env::var("SE_GPU").unwrap_or_else(|_| "NVIDIA".into());
        // SAFETY: valid instance.
        let pdev = unsafe { instance.enumerate_physical_devices() }?
            .into_iter()
            .find(|&p| {
                // SAFETY: valid physical device.
                let props = unsafe { instance.get_physical_device_properties(p) };
                props.device_name_as_c_str().is_ok_and(|n| n.to_string_lossy().to_lowercase().contains(&wanted.to_lowercase()))
            })
            .ok_or_else(|| anyhow!("no Vulkan device matching {wanted:?}"))?;
        // SAFETY: valid physical device.
        let queue_family = unsafe { instance.get_physical_device_queue_family_properties(pdev) }
            .iter()
            .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .ok_or_else(|| anyhow!("no graphics queue"))? as u32;
        let priorities = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(queue_family).queue_priorities(&priorities)];
        let ext: Vec<*const c_char> = EXTENSIONS.iter().map(|e| e.as_ptr()).collect();
        // SAFETY: valid create info.
        let device = unsafe { instance.create_device(pdev, &vk::DeviceCreateInfo::default().queue_create_infos(&queues).enabled_extension_names(&ext), None) }
            .context("vkCreateDevice (needs dmabuf/modifier/foreign-queue/sync_fd extensions)")?;
        // SAFETY: valid device.
        let pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(queue_family).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
        }?;
        let mut sem_props = vk::ExternalSemaphoreProperties::default();
        // SAFETY: valid physical device.
        unsafe {
            instance.get_physical_device_external_semaphore_properties(
                pdev,
                &vk::PhysicalDeviceExternalSemaphoreInfo::default().handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
                &mut sem_props,
            )
        };
        let sync_fd = sem_props.external_semaphore_features.contains(vk::ExternalSemaphoreFeatureFlags::EXPORTABLE);
        let mut gpu = Gpu {
            memory_fd: ash::khr::external_memory_fd::Device::new(&instance, &device),
            semaphore_fd: ash::khr::external_semaphore_fd::Device::new(&instance, &device),
            drm: ash::ext::image_drm_format_modifier::Device::new(&instance, &device),
            // SAFETY: the queue family was requested with one queue.
            queue: unsafe { device.get_device_queue(queue_family, 0) },
            _entry: entry,
            instance,
            pdev,
            device,
            queue_family,
            pool,
            sync_fd,
            modifiers: Vec::new(),
        };
        gpu.modifiers = gpu.exportable_modifiers()?;
        if gpu.modifiers.is_empty() {
            bail!("no single-plane exportable DRM modifier for R8G8B8A8_UNORM");
        }
        println!("dmabuf export: modifiers {:x?}, sync_file fences {}", gpu.modifiers, if sync_fd { "on" } else { "off (CPU wait)" });
        Ok(gpu)
    }

    fn exportable_modifiers(&self) -> Result<Vec<u64>> {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        {
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            // SAFETY: valid output chain.
            unsafe { self.instance.get_physical_device_format_properties2(self.pdev, FORMAT, &mut props) };
        }
        let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); list.drm_format_modifier_count as usize];
        {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut mods);
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            // SAFETY: output chain sized by the first call.
            unsafe { self.instance.get_physical_device_format_properties2(self.pdev, FORMAT, &mut props) };
        }
        let need = vk::FormatFeatureFlags::TRANSFER_DST | vk::FormatFeatureFlags::TRANSFER_SRC;
        let mut out = Vec::new();
        for m in mods {
            if m.drm_format_modifier_plane_count != 1 || !m.drm_format_modifier_tiling_features.contains(need) {
                continue;
            }
            let mut ext_info = vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let mut drm_info =
                vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default().drm_format_modifier(m.drm_format_modifier).sharing_mode(vk::SharingMode::EXCLUSIVE);
            let info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(FORMAT)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
                .push_next(&mut ext_info)
                .push_next(&mut drm_info);
            let mut ext_props = vk::ExternalImageFormatProperties::default();
            let ok = {
                let mut props = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
                // SAFETY: valid chains.
                unsafe { self.instance.get_physical_device_image_format_properties2(self.pdev, &info, &mut props) }.is_ok()
            };
            let features = ext_props.external_memory_properties.external_memory_features;
            if ok && features.contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE | vk::ExternalMemoryFeatureFlags::IMPORTABLE) {
                out.push(m.drm_format_modifier);
            }
        }
        Ok(out)
    }

    pub fn create_ring(&mut self, width: u32, height: u32, count: usize) -> Result<Ring> {
        let mut ring = Ring { width, height, modifier: 0, offset: 0, stride: 0, bufs: Vec::new() };
        for i in 0..count {
            let (buf, modifier, layout) = self.create_buf(width, height)?;
            if i == 0 {
                ring.modifier = modifier;
                ring.offset = layout.offset;
                ring.stride = layout.row_pitch;
            } else if (modifier, layout.offset, layout.row_pitch) != (ring.modifier, ring.offset, ring.stride) {
                bail!("the driver picked different layouts for buffers of one ring");
            }
            ring.bufs.push(buf);
        }
        Ok(ring)
    }

    fn create_buf(&self, width: u32, height: u32) -> Result<(Buf, u64, vk::SubresourceLayout)> {
        let d = &self.device;
        let mut list = vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&self.modifiers);
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external)
            .push_next(&mut list);
        // SAFETY: every call below uses handles created from `d` in this function; errors
        // leak them only on the (fatal) error path of this example.
        unsafe {
            let image = d.create_image(&info, None).context("vkCreateImage")?;
            let mut props = vk::ImageDrmFormatModifierPropertiesEXT::default();
            self.drm.get_image_drm_format_modifier_properties(image, &mut props)?;
            let layout = d.get_image_subresource_layout(
                image,
                vk::ImageSubresource { aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT, mip_level: 0, array_layer: 0 },
            );
            let req = d.get_image_memory_requirements(image);
            let mem_props = self.instance.get_physical_device_memory_properties(self.pdev);
            let type_index = (0..mem_props.memory_type_count)
                .filter(|&i| req.memory_type_bits & (1 << i) != 0)
                .find(|&i| mem_props.memory_types[i as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL))
                .or_else(|| (0..32).find(|&i| req.memory_type_bits & (1 << i) != 0))
                .ok_or_else(|| anyhow!("no memory type for the image"))?;
            let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let memory = d.allocate_memory(
                &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(type_index).push_next(&mut export).push_next(&mut dedicated),
                None,
            )?;
            d.bind_image_memory(image, memory, 0)?;
            let raw =
                self.memory_fd.get_memory_fd(&vk::MemoryGetFdInfoKHR::default().memory(memory).handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT))?;
            let fd = OwnedFd::from_raw_fd(raw);
            let cmd = d.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default().command_pool(self.pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(1),
            )?[0];
            let fence = d.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None)?;
            let mut export_sem = vk::ExportSemaphoreCreateInfo::default().handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
            let mut sem_info = vk::SemaphoreCreateInfo::default();
            if self.sync_fd {
                sem_info = sem_info.push_next(&mut export_sem);
            }
            let semaphore = d.create_semaphore(&sem_info, None)?;
            Ok((Buf { image, memory, fd, cmd, fence, semaphore, initialized: false }, props.drm_format_modifier, layout))
        }
    }

    /// Clears buffer `b` to `color`; returns the sync_file fence of the GPU work (or waits
    /// on the CPU when sync_file export is unavailable).
    pub fn render(&mut self, ring: &mut Ring, b: usize, color: [u8; 4]) -> Result<Option<OwnedFd>> {
        let d = &self.device;
        let buf = &mut ring.bufs[b];
        let range =
            vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 };
        let acquire = if buf.initialized {
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .dst_queue_family_index(self.queue_family)
        } else {
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        }
        .new_layout(vk::ImageLayout::GENERAL)
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .image(buf.image)
        .subresource_range(range);
        let release = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::empty())
            .src_queue_family_index(self.queue_family)
            .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .image(buf.image)
            .subresource_range(range);
        let clear = vk::ClearColorValue { float32: color.map(|c| f32::from(c) / 255.0) };
        let cmds = [buf.cmd];
        let signal = [buf.semaphore];
        let mut submit = vk::SubmitInfo::default().command_buffers(&cmds);
        if self.sync_fd {
            submit = submit.signal_semaphores(&signal);
        }
        // SAFETY: handles belong to this device; the buffer's previous submission is
        // waited for before its command buffer is re-recorded.
        unsafe {
            d.wait_for_fences(&[buf.fence], true, 2_000_000_000).context("previous frame of this buffer did not finish")?;
            d.reset_fences(&[buf.fence])?;
            d.reset_command_buffer(buf.cmd, vk::CommandBufferResetFlags::empty())?;
            d.begin_command_buffer(buf.cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
            d.cmd_pipeline_barrier(
                buf.cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[acquire],
            );
            d.cmd_clear_color_image(buf.cmd, buf.image, vk::ImageLayout::GENERAL, &clear, &[range]);
            d.cmd_pipeline_barrier(
                buf.cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[release],
            );
            d.end_command_buffer(buf.cmd)?;
            d.queue_submit(self.queue, &[submit], buf.fence)?;
            buf.initialized = true;
            if self.sync_fd {
                let raw = self.semaphore_fd.get_semaphore_fd(
                    &vk::SemaphoreGetFdInfoKHR::default().semaphore(buf.semaphore).handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
                )?;
                // -1 means "already signalled".
                Ok((raw >= 0).then(|| OwnedFd::from_raw_fd(raw)))
            } else {
                d.wait_for_fences(&[buf.fence], true, 2_000_000_000)?;
                Ok(None)
            }
        }
    }

    pub fn destroy_ring(&mut self, ring: Ring) {
        // SAFETY: the device is idle; each handle is destroyed once.
        unsafe {
            let _ = self.device.device_wait_idle();
            for b in ring.bufs {
                self.device.destroy_semaphore(b.semaphore, None);
                self.device.destroy_fence(b.fence, None);
                self.device.free_command_buffers(self.pool, &[b.cmd]);
                self.device.destroy_image(b.image, None);
                self.device.free_memory(b.memory, None);
            }
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: rings were destroyed; tear down in reverse creation order.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
