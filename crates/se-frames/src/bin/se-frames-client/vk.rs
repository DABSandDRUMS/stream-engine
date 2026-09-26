//! dmabuf import validation (`--import`): imports every canvas buffer as a Vulkan image
//! on the NVIDIA device (or `SE_GPU=<substring>`) and reads back the center pixels.

use std::ffi::{CStr, c_char};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};

use anyhow::{Context, Result, anyhow, bail};
use ash::vk;
use se_frames::CanvasMsg;

/// Side of the square read back from the canvas center.
pub const SAMPLE: u32 = 16;

const REQUIRED_EXTENSIONS: [&CStr; 4] = [
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
];

/// One imported canvas buffer.
pub struct Image {
    image: vk::Image,
    memory: vk::DeviceMemory,
    width: u32,
    height: u32,
}

/// Center readback: the pixel at the canvas center and an FNV-1a checksum of the
/// `SAMPLE × SAMPLE` block around it.
pub struct Sample {
    pub center: [u8; 4],
    pub checksum: u32,
}

pub struct Gpu {
    _entry: ash::Entry,
    instance: ash::Instance,
    pdev: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    queue_family: u32,
    memory_fd: ash::khr::external_memory_fd::Device,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    readback: vk::Buffer,
    readback_memory: vk::DeviceMemory,
    readback_ptr: *const u8,
    pub name: String,
}

fn find_memory_type(props: &vk::PhysicalDeviceMemoryProperties, bits: u32, flags: vk::MemoryPropertyFlags) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| bits & (1 << i) != 0 && props.memory_types[i as usize].property_flags.contains(flags))
}

impl Gpu {
    pub fn new() -> Result<Gpu> {
        // SAFETY: loading the system Vulkan loader.
        let entry = unsafe { ash::Entry::load() }.context("loading libvulkan")?;
        let app = vk::ApplicationInfo::default().application_name(c"se-frames-client").api_version(vk::API_VERSION_1_3);
        // SAFETY: valid create info.
        let instance = unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None) }.context("vkCreateInstance")?;
        match Gpu::with_instance(&instance) {
            Ok(parts) => Ok(Gpu::assemble(entry, instance, parts)),
            Err(e) => {
                // SAFETY: nothing else was created from the instance.
                unsafe { instance.destroy_instance(None) };
                Err(e)
            }
        }
    }

    fn with_instance(instance: &ash::Instance) -> Result<Parts> {
        let wanted = std::env::var("SE_GPU").unwrap_or_else(|_| "NVIDIA".into());
        // SAFETY: valid instance.
        let pdevs = unsafe { instance.enumerate_physical_devices() }?;
        let mut names = Vec::new();
        let mut chosen = None;
        for pdev in pdevs {
            // SAFETY: valid physical device.
            let props = unsafe { instance.get_physical_device_properties(pdev) };
            let name = props.device_name_as_c_str().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if chosen.is_none() && name.to_lowercase().contains(&wanted.to_lowercase()) {
                chosen = Some((pdev, name.clone()));
            }
            names.push(name);
        }
        let (pdev, name) = chosen.ok_or_else(|| anyhow!("no Vulkan device matching {wanted:?} (SE_GPU); found {names:?}"))?;

        // SAFETY: valid physical device.
        let exts = unsafe { instance.enumerate_device_extension_properties(pdev) }?;
        let missing: Vec<_> = REQUIRED_EXTENSIONS
            .iter()
            .filter(|req| !exts.iter().any(|e| e.extension_name_as_c_str().is_ok_and(|n| n == **req)))
            .map(|req| req.to_string_lossy().into_owned())
            .collect();
        if !missing.is_empty() {
            bail!("{name} lacks device extensions {missing:?}");
        }

        // SAFETY: valid physical device.
        let families = unsafe { instance.get_physical_device_queue_family_properties(pdev) };
        let queue_family = families
            .iter()
            .position(|f| f.queue_flags.intersects(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER))
            .ok_or_else(|| anyhow!("{name} has no queue family that can copy"))? as u32;

        let priorities = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(queue_family).queue_priorities(&priorities)];
        let ext_ptrs: Vec<*const c_char> = REQUIRED_EXTENSIONS.iter().map(|e| e.as_ptr()).collect();
        let info = vk::DeviceCreateInfo::default().queue_create_infos(&queues).enabled_extension_names(&ext_ptrs);
        // SAFETY: valid create info; extensions checked above.
        let device = unsafe { instance.create_device(pdev, &info, None) }.context("vkCreateDevice")?;
        match Gpu::device_objects(instance, pdev, &device, queue_family) {
            Ok(objects) => Ok(Parts { pdev, device, queue_family, objects, name }),
            Err(e) => {
                // SAFETY: device_objects cleaned up after itself.
                unsafe { device.destroy_device(None) };
                Err(e)
            }
        }
    }

    fn device_objects(instance: &ash::Instance, pdev: vk::PhysicalDevice, device: &ash::Device, queue_family: u32) -> Result<Objects> {
        // SAFETY: every handle below is created from `device` and destroyed on failure.
        unsafe {
            let pool = device.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(queue_family).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?;
            let cleanup_pool = |e: vk::Result| {
                device.destroy_command_pool(pool, None);
                anyhow!(e)
            };
            let cmd = device
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default().command_pool(pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(1),
                )
                .map_err(cleanup_pool)?[0];
            let fence = device.create_fence(&vk::FenceCreateInfo::default(), None).map_err(cleanup_pool)?;
            let size = u64::from(SAMPLE * SAMPLE * 4);
            let readback = match device.create_buffer(
                &vk::BufferCreateInfo::default().size(size).usage(vk::BufferUsageFlags::TRANSFER_DST).sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            ) {
                Ok(b) => b,
                Err(e) => {
                    device.destroy_fence(fence, None);
                    return Err(cleanup_pool(e));
                }
            };
            let fail = |e: anyhow::Error| {
                device.destroy_buffer(readback, None);
                device.destroy_fence(fence, None);
                device.destroy_command_pool(pool, None);
                e
            };
            let req = device.get_buffer_memory_requirements(readback);
            let mem_props = instance.get_physical_device_memory_properties(pdev);
            let Some(type_index) =
                find_memory_type(&mem_props, req.memory_type_bits, vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT)
            else {
                return Err(fail(anyhow!("no host-visible coherent memory type")));
            };
            let readback_memory = device
                .allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(type_index), None)
                .map_err(|e| fail(anyhow!(e)))?;
            let mapped =
                device.bind_buffer_memory(readback, readback_memory, 0).and_then(|()| device.map_memory(readback_memory, 0, size, vk::MemoryMapFlags::empty()));
            let ptr = match mapped {
                Ok(p) => p,
                Err(e) => {
                    device.free_memory(readback_memory, None);
                    return Err(fail(anyhow!(e)));
                }
            };
            Ok(Objects {
                queue: device.get_device_queue(queue_family, 0),
                pool,
                cmd,
                fence,
                readback,
                readback_memory,
                readback_ptr: ptr.cast::<u8>().cast_const(),
            })
        }
    }

    fn assemble(entry: ash::Entry, instance: ash::Instance, p: Parts) -> Gpu {
        let memory_fd = ash::khr::external_memory_fd::Device::new(&instance, &p.device);
        Gpu {
            _entry: entry,
            instance,
            pdev: p.pdev,
            memory_fd,
            queue: p.objects.queue,
            queue_family: p.queue_family,
            pool: p.objects.pool,
            cmd: p.objects.cmd,
            fence: p.objects.fence,
            readback: p.objects.readback,
            readback_memory: p.objects.readback_memory,
            readback_ptr: p.objects.readback_ptr,
            device: p.device,
            name: p.name,
        }
    }

    /// Checks that `modifier` is importable for `R8G8B8A8_UNORM` as a transfer source.
    fn check_modifier(&self, msg: &CanvasMsg) -> Result<()> {
        let format = vk::Format::R8G8B8A8_UNORM;
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        {
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            // SAFETY: valid physical device and output chain.
            unsafe { self.instance.get_physical_device_format_properties2(self.pdev, format, &mut props) };
        }
        let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); list.drm_format_modifier_count as usize];
        {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut mods);
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            // SAFETY: valid physical device and output chain sized by the first call.
            unsafe { self.instance.get_physical_device_format_properties2(self.pdev, format, &mut props) };
        }
        let Some(m) = mods.iter().find(|m| m.drm_format_modifier == msg.modifier) else {
            let supported: Vec<String> = mods.iter().map(|m| format!("{:#x}", m.drm_format_modifier)).collect();
            bail!("modifier {:#x} is not supported for R8G8B8A8_UNORM on {} (supported: {})", msg.modifier, self.name, supported.join(", "));
        };
        if m.drm_format_modifier_plane_count != 1 {
            bail!("modifier {:#x} needs {} memory planes; the protocol carries 1", msg.modifier, m.drm_format_modifier_plane_count);
        }
        if !m.drm_format_modifier_tiling_features.contains(vk::FormatFeatureFlags::TRANSFER_SRC) {
            bail!("modifier {:#x} does not support transfer reads", msg.modifier);
        }

        let mut ext_info = vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut drm_info =
            vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default().drm_format_modifier(msg.modifier).sharing_mode(vk::SharingMode::EXCLUSIVE);
        let info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC)
            .push_next(&mut ext_info)
            .push_next(&mut drm_info);
        let mut ext_props = vk::ExternalImageFormatProperties::default();
        let limits = {
            let mut props = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
            // SAFETY: valid chains.
            unsafe { self.instance.get_physical_device_image_format_properties2(self.pdev, &info, &mut props) }
                .with_context(|| format!("modifier {:#x} is not usable for dmabuf import", msg.modifier))?;
            props.image_format_properties.max_extent
        };
        if !ext_props.external_memory_properties.external_memory_features.contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE) {
            bail!("dmabufs with modifier {:#x} are not importable", msg.modifier);
        }
        if msg.width > limits.width || msg.height > limits.height {
            bail!("{}x{} exceeds the import limit {}x{}", msg.width, msg.height, limits.width, limits.height);
        }
        Ok(())
    }

    /// Imports one dmabuf described by `msg` (a dup of `fd` is handed to Vulkan).
    pub fn import(&self, fd: BorrowedFd<'_>, msg: &CanvasMsg) -> Result<Image> {
        self.check_modifier(msg)?;
        let layouts =
            [vk::SubresourceLayout { offset: u64::from(msg.offsets[0]), size: 0, row_pitch: u64::from(msg.strides[0]), array_pitch: 0, depth_pitch: 0 }];
        let mut drm = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default().drm_format_modifier(msg.modifier).plane_layouts(&layouts);
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D { width: msg.width, height: msg.height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external)
            .push_next(&mut drm);
        // SAFETY: valid create info.
        let image = unsafe { self.device.create_image(&info, None) }.context("vkCreateImage")?;
        match self.bind_import(image, fd) {
            Ok(memory) => Ok(Image { image, memory, width: msg.width, height: msg.height }),
            Err(e) => {
                // SAFETY: the image is unused.
                unsafe { self.device.destroy_image(image, None) };
                Err(e)
            }
        }
    }

    fn bind_import(&self, image: vk::Image, fd: BorrowedFd<'_>) -> Result<vk::DeviceMemory> {
        // SAFETY: valid image.
        let req = unsafe { self.device.get_image_memory_requirements(image) };
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        // SAFETY: fd is a live dmabuf.
        unsafe { self.memory_fd.get_memory_fd_properties(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT, fd.as_raw_fd(), &mut fd_props) }
            .context("vkGetMemoryFdPropertiesKHR")?;
        let bits = req.memory_type_bits & fd_props.memory_type_bits;
        if bits == 0 {
            bail!("no memory type fits both the image ({:#x}) and the dmabuf ({:#x})", req.memory_type_bits, fd_props.memory_type_bits);
        }
        let size = se_frames::sys::fd_size(fd).context("sizing the dmabuf")?;
        if size < req.size {
            bail!("dmabuf is {size} bytes, the image needs {}", req.size);
        }
        let raw = fd.try_clone_to_owned()?.into_raw_fd();
        let mut import = vk::ImportMemoryFdInfoKHR::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT).fd(raw);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(bits.trailing_zeros())
            .push_next(&mut import)
            .push_next(&mut dedicated);
        // SAFETY: valid allocate info; on success Vulkan owns `raw`.
        let memory = match unsafe { self.device.allocate_memory(&alloc, None) } {
            Ok(m) => m,
            Err(e) => {
                // SAFETY: a failed import leaves the fd with us.
                drop(unsafe { OwnedFd::from_raw_fd(raw) });
                return Err(anyhow!(e).context("importing the dmabuf (vkAllocateMemory)"));
            }
        };
        // SAFETY: fresh image and memory.
        if let Err(e) = unsafe { self.device.bind_image_memory(image, memory, 0) } {
            // SAFETY: unused memory.
            unsafe { self.device.free_memory(memory, None) };
            return Err(anyhow!(e).context("vkBindImageMemory"));
        }
        Ok(memory)
    }

    /// Copies the center `SAMPLE × SAMPLE` pixels to host memory (acquiring the image from
    /// `VK_QUEUE_FAMILY_FOREIGN_EXT` in layout GENERAL and releasing it back).
    pub fn sample(&self, img: &Image) -> Result<Sample> {
        let (w, h) = (SAMPLE.min(img.width), SAMPLE.min(img.height));
        let (x, y) = ((img.width - w) / 2, (img.height - h) / 2);
        let range =
            vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 };
        let acquire = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .dst_queue_family_index(self.queue_family)
            .image(img.image)
            .subresource_range(range);
        let release = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dst_access_mask(vk::AccessFlags::empty())
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(self.queue_family)
            .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .image(img.image)
            .subresource_range(range);
        let host = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.readback)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        let region = vk::BufferImageCopy::default()
            .image_subresource(vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 })
            .image_offset(vk::Offset3D { x: x as i32, y: y as i32, z: 0 })
            .image_extent(vk::Extent3D { width: w, height: h, depth: 1 });
        let cmds = [self.cmd];
        let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
        // SAFETY: all handles belong to this device; the fence is unsignalled and the
        // command buffer idle (every submission is waited for below).
        unsafe {
            let d = &self.device;
            d.reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())?;
            d.begin_command_buffer(self.cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
            d.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[acquire],
            );
            d.cmd_copy_image_to_buffer(self.cmd, img.image, vk::ImageLayout::GENERAL, self.readback, &[region]);
            d.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST | vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[host],
                &[release],
            );
            d.end_command_buffer(self.cmd)?;
            d.queue_submit(self.queue, &submit, self.fence)?;
            let waited = d.wait_for_fences(&[self.fence], true, 2_000_000_000);
            d.reset_fences(&[self.fence])?;
            waited.context("waiting for the readback copy")?;
        }
        let len = (w * h * 4) as usize;
        // SAFETY: the mapping is SAMPLE² × 4 bytes and coherent; the copy has completed.
        let bytes = unsafe { std::slice::from_raw_parts(self.readback_ptr, len) };
        let c = (((h / 2) * w + w / 2) * 4) as usize;
        let mut checksum: u32 = 0x811c_9dc5;
        for &b in bytes {
            checksum = (checksum ^ u32::from(b)).wrapping_mul(0x0100_0193);
        }
        Ok(Sample { center: [bytes[c], bytes[c + 1], bytes[c + 2], bytes[c + 3]], checksum })
    }

    pub fn destroy(&self, img: Image) {
        // SAFETY: every submission using the image was waited for.
        unsafe {
            self.device.destroy_image(img.image, None);
            self.device.free_memory(img.memory, None);
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: tear down in reverse creation order after the device is idle.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.unmap_memory(self.readback_memory);
            self.device.free_memory(self.readback_memory, None);
            self.device.destroy_buffer(self.readback, None);
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

struct Objects {
    queue: vk::Queue,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    readback: vk::Buffer,
    readback_memory: vk::DeviceMemory,
    readback_ptr: *const u8,
}

struct Parts {
    pdev: vk::PhysicalDevice,
    device: ash::Device,
    queue_family: u32,
    objects: Objects,
    name: String,
}
