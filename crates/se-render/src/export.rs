//! Canvas export as dmabufs (§4.5): exportable `VkImage`s with a DRM format modifier created
//! through `ash`, wrapped into wgpu with wgpu-hal `texture_from_raw`, explicit sync through a
//! sync_file exported from a binary semaphore signalled by the frame's submission, and queue
//! family ownership transfers to/from `VK_QUEUE_FAMILY_FOREIGN_EXT` around each write.

use crate::gpu::{Gpu, Vk};
use crate::plan::ModifierPref;
use anyhow::{Context, Result, anyhow, bail};
use ash::vk;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use wgpu::hal::api::Vulkan;

/// DRM_FORMAT_ABGR8888: RGBA8 unorm in memory order.
pub const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241;
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const VENDOR_NVIDIA: u64 = 0x03;

/// NVIDIA block-linear modifiers carry a compression field (bits 23..25); compressed layouts are
/// not safe for every importer, so `auto` avoids them.
pub fn nvidia_compressed(m: u64) -> bool {
    m >> 56 == VENDOR_NVIDIA && (m >> 23) & 0x7 != 0
}

pub struct ExportImage {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub image: vk::Image,
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
    pub size: u64,
}

pub struct DmabufRing {
    pub images: Vec<ExportImage>,
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
}

const USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::COLOR_ATTACHMENT.as_raw()
        | vk::ImageUsageFlags::SAMPLED.as_raw()
        | vk::ImageUsageFlags::TRANSFER_SRC.as_raw()
        | vk::ImageUsageFlags::TRANSFER_DST.as_raw(),
);
const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// Modifiers the driver can render to, sample from, and export as a single-plane dmabuf at
/// this size, in driver order.
pub fn exportable_modifiers(vk: &Vk, width: u32, height: u32) -> Vec<u64> {
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    unsafe {
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        vk.instance.get_physical_device_format_properties2(vk.physical, FORMAT, &mut props);
    }
    let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); list.drm_format_modifier_count as usize];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut mods);
    unsafe {
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        vk.instance.get_physical_device_format_properties2(vk.physical, FORMAT, &mut props);
    }
    let n = list.drm_format_modifier_count as usize;
    let need = vk::FormatFeatureFlags::COLOR_ATTACHMENT | vk::FormatFeatureFlags::SAMPLED_IMAGE;
    mods[..n]
        .iter()
        .filter(|m| m.drm_format_modifier_plane_count == 1 && m.drm_format_modifier_tiling_features.contains(need))
        .map(|m| m.drm_format_modifier)
        .filter(|&m| supports_export(vk, m, width, height))
        .collect()
}

fn supports_export(vk: &Vk, modifier: u64, width: u32, height: u32) -> bool {
    let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default().drm_format_modifier(modifier).sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut ext_info = vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(FORMAT)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(USAGE)
        .push_next(&mut ext_info)
        .push_next(&mut drm);
    let mut ext_props = vk::ExternalImageFormatProperties::default();
    let mut props = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
    let ok = unsafe { vk.instance.get_physical_device_image_format_properties2(vk.physical, &info, &mut props) }.is_ok();
    let max = props.image_format_properties.max_extent;
    ok && ext_props.external_memory_properties.external_memory_features.contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
        && max.width >= width
        && max.height >= height
}

/// Candidate list passed to the driver for `pref`.
pub fn choose_modifiers(available: &[u64], pref: ModifierPref) -> Result<Vec<u64>> {
    match pref {
        ModifierPref::Linear => {
            if available.contains(&DRM_FORMAT_MOD_LINEAR) {
                Ok(vec![DRM_FORMAT_MOD_LINEAR])
            } else {
                bail!("the driver cannot export LINEAR RGBA8 render targets")
            }
        }
        ModifierPref::Explicit(m) => {
            if available.contains(&m) {
                Ok(vec![m])
            } else {
                bail!("modifier {m:#x} is not exportable (available: {})", available.iter().map(|m| format!("{m:#x}")).collect::<Vec<_>>().join(", "))
            }
        }
        ModifierPref::Auto => {
            let tiled: Vec<u64> = available.iter().copied().filter(|&m| m != DRM_FORMAT_MOD_LINEAR && !nvidia_compressed(m)).collect();
            if !tiled.is_empty() {
                Ok(tiled)
            } else if available.contains(&DRM_FORMAT_MOD_LINEAR) {
                Ok(vec![DRM_FORMAT_MOD_LINEAR])
            } else if !available.is_empty() {
                Ok(available.to_vec())
            } else {
                bail!("no exportable RGBA8 modifier")
            }
        }
    }
}

fn memory_type(vk: &Vk, bits: u32) -> Option<u32> {
    let props = unsafe { vk.instance.get_physical_device_memory_properties(vk.physical) };
    (0..props.memory_type_count)
        .find(|&i| bits & (1 << i) != 0 && props.memory_types[i as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL))
}

impl DmabufRing {
    pub fn new(gpu: &Gpu, label: &str, width: u32, height: u32, count: u32, pref: ModifierPref) -> Result<DmabufRing> {
        let vk = gpu.vk.as_ref().filter(|v| v.can_export()).ok_or_else(|| anyhow!("dmabuf export extensions unavailable"))?;
        let available = exportable_modifiers(vk, width, height);
        let candidates = choose_modifiers(&available, pref)?;
        let mut images = Vec::with_capacity(count as usize);
        let mut modifier = None;
        for i in 0..count {
            let img = create_image(gpu, vk, &format!("{label}#{i}"), width, height, &candidates)?;
            // all buffers of a ring share one modifier (the driver picks deterministically)
            match modifier {
                None => modifier = Some(img.1),
                Some(m) if m != img.1 => bail!("driver chose different modifiers within one ring"),
                _ => {}
            }
            images.push(img.0);
        }
        Ok(DmabufRing { images, width, height, modifier: modifier.unwrap_or(DRM_FORMAT_MOD_LINEAR) })
    }

    /// Duplicated fds for a `se_canvas` message.
    pub fn dup_fds(&self) -> std::io::Result<Vec<OwnedFd>> {
        self.images.iter().map(|i| i.fd.as_fd().try_clone_to_owned()).collect()
    }
}

fn create_image(gpu: &Gpu, vk: &Vk, label: &str, width: u32, height: u32, candidates: &[u64]) -> Result<(ExportImage, u64)> {
    let dev = &vk.device;
    let drm = vk.drm_modifier.as_ref().expect("checked by can_export");
    let mem_fd = vk.mem_fd.as_ref().expect("checked by can_export");
    let mut mods = vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(candidates);
    let mut ext = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(FORMAT)
        .extent(vk::Extent3D { width, height, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(USAGE)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut ext)
        .push_next(&mut mods);
    let image = unsafe { dev.create_image(&info, None) }.context("vkCreateImage (dmabuf canvas)")?;
    let cleanup_image = |e: anyhow::Error| {
        unsafe { dev.destroy_image(image, None) };
        e
    };
    let mut mp = vk::ImageDrmFormatModifierPropertiesEXT::default();
    unsafe { drm.get_image_drm_format_modifier_properties(image, &mut mp) }.map_err(|e| cleanup_image(anyhow!("modifier query: {e}")))?;
    let layout = unsafe {
        dev.get_image_subresource_layout(image, vk::ImageSubresource { aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT, mip_level: 0, array_layer: 0 })
    };
    let reqs = unsafe { dev.get_image_memory_requirements(image) };
    let Some(type_index) = memory_type(vk, reqs.memory_type_bits) else {
        return Err(cleanup_image(anyhow!("no device-local memory type for the canvas image")));
    };
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let alloc = vk::MemoryAllocateInfo::default().allocation_size(reqs.size).memory_type_index(type_index).push_next(&mut export).push_next(&mut dedicated);
    let memory = unsafe { dev.allocate_memory(&alloc, None) }.map_err(|e| cleanup_image(anyhow!("vkAllocateMemory: {e}")))?;
    let fail = |e: anyhow::Error| {
        unsafe {
            dev.free_memory(memory, None);
            dev.destroy_image(image, None);
        }
        e
    };
    unsafe { dev.bind_image_memory(image, memory, 0) }.map_err(|e| fail(anyhow!("vkBindImageMemory: {e}")))?;
    let raw_fd = unsafe { mem_fd.get_memory_fd(&vk::MemoryGetFdInfoKHR::default().memory(memory).handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)) }
        .map_err(|e| fail(anyhow!("vkGetMemoryFdKHR: {e}")))?;
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

    let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
    let hal_desc = wgpu::hal::TextureDescriptor {
        label: Some(label),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUses::COLOR_TARGET | wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC | wgpu::TextureUses::COPY_DST,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: Vec::new(),
    };
    let owner = dev.clone();
    let drop_cb: wgpu::hal::DropCallback = Box::new(move || unsafe {
        owner.destroy_image(image, None);
        owner.free_memory(memory, None);
    });
    let texture = unsafe {
        let hal_dev = gpu.device.as_hal::<Vulkan>().ok_or_else(|| anyhow!("device is not Vulkan"))?;
        let hal_tex = hal_dev.texture_from_raw(image, &hal_desc, Some(drop_cb), wgpu::hal::vulkan::TextureMemory::External);
        drop(hal_dev);
        gpu.device.create_texture_from_hal::<Vulkan>(
            hal_tex,
            &wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            wgpu::TextureUses::UNINITIALIZED,
        )
    };
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Ok((ExportImage { texture, view, image, fd, offset: layout.offset as u32, stride: layout.row_pitch as u32, size: reqs.size }, mp.drm_format_modifier))
}

/// Up to this many images get an ownership barrier per submission.
pub const MAX_BARRIERS: usize = 8;

/// A raw command buffer transferring `images` between our queue family and the foreign one:
/// `acquire` = FOREIGN → ours (UNDEFINED → COLOR_ATTACHMENT, content discarded),
/// release = ours → FOREIGN (COLOR_ATTACHMENT → GENERAL).
pub fn ownership_barrier(gpu: &Gpu, images: &[vk::Image], acquire: bool) -> Option<wgpu::CommandBuffer> {
    let vk = gpu.vk.as_ref()?;
    if images.is_empty() {
        return None;
    }
    let external = if vk.foreign_queue { vk::QUEUE_FAMILY_FOREIGN_EXT } else { vk::QUEUE_FAMILY_EXTERNAL };
    let range = vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 };
    let mut barriers = [vk::ImageMemoryBarrier::default(); MAX_BARRIERS];
    let n = images.len().min(MAX_BARRIERS);
    for (b, img) in barriers.iter_mut().zip(images) {
        *b = if acquire {
            vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .src_queue_family_index(external)
                .dst_queue_family_index(vk.queue_family)
        } else {
            vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_access_mask(vk::AccessFlags::empty())
                .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk.queue_family)
                .dst_queue_family_index(external)
        }
        .image(*img)
        .subresource_range(range);
    }
    let (src, dst) = if acquire {
        (vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
    } else {
        (vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT, vk::PipelineStageFlags::BOTTOM_OF_PIPE)
    };
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some(if acquire { "export acquire" } else { "export release" }) });
    let recorded = unsafe {
        enc.as_hal_mut::<Vulkan, _, _>(|e| {
            let Some(e) = e else { return false };
            vk.device.cmd_pipeline_barrier(e.raw_handle(), src, dst, vk::DependencyFlags::empty(), &[], &[], &barriers[..n]);
            true
        })
    };
    recorded.then(|| enc.finish())
}

/// Binary semaphore exported as a sync_file after each frame's submission.
pub struct FenceExport {
    semaphore: vk::Semaphore,
    device: ash::Device,
    sem_fd: ash::khr::external_semaphore_fd::Device,
}

impl FenceExport {
    pub fn new(gpu: &Gpu) -> Result<FenceExport> {
        let vk = gpu.vk.as_ref().filter(|v| v.sem_fd.is_some()).ok_or_else(|| anyhow!("VK_KHR_external_semaphore_fd unavailable"))?;
        let mut props = vk::ExternalSemaphoreProperties::default();
        let info = vk::PhysicalDeviceExternalSemaphoreInfo::default().handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        unsafe { vk.instance.get_physical_device_external_semaphore_properties(vk.physical, &info, &mut props) };
        if !props.external_semaphore_features.contains(vk::ExternalSemaphoreFeatureFlags::EXPORTABLE) {
            bail!("sync_file semaphore export unsupported");
        }
        let mut exp = vk::ExportSemaphoreCreateInfo::default().handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        let semaphore = unsafe { vk.device.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut exp), None) }.context("vkCreateSemaphore")?;
        Ok(FenceExport { semaphore, device: vk.device.clone(), sem_fd: vk.sem_fd.clone().expect("filtered") })
    }

    /// Have the next `queue.submit` signal the semaphore. Call right before the frame's final
    /// submission (other submissions in between would consume it).
    pub fn arm(&self, queue: &wgpu::Queue) -> bool {
        match unsafe { queue.as_hal::<Vulkan>() } {
            Some(q) => {
                q.add_signal_semaphore(self.semaphore, None);
                true
            }
            None => false,
        }
    }

    /// Undo [`arm`](Self::arm) when the submission did not happen.
    pub fn disarm(&self, queue: &wgpu::Queue) {
        if let Some(q) = unsafe { queue.as_hal::<Vulkan>() } {
            q.remove_signal_semaphore(self.semaphore);
        }
    }

    /// Export the pending signal as a sync_file (resets the semaphore). `None` = already
    /// signalled (the driver may return -1) — the frame is complete.
    pub fn export(&self) -> Result<Option<OwnedFd>> {
        let fd = unsafe {
            self.sem_fd
                .get_semaphore_fd(&vk::SemaphoreGetFdInfoKHR::default().semaphore(self.semaphore).handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD))
        }
        .map_err(|e| anyhow!("vkGetSemaphoreFdKHR: {e}"))?;
        Ok((fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) }))
    }
}

impl Drop for FenceExport {
    fn drop(&mut self) {
        // Callers drop this only after the device went idle (renderer teardown waits).
        unsafe { self.device.destroy_semaphore(self.semaphore, None) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NV_BL: u64 = 0x0300_0000_0060_6014;
    const NV_BL_COMPRESSED: u64 = NV_BL | (1 << 23);

    #[test]
    fn modifier_preference() {
        assert!(nvidia_compressed(NV_BL_COMPRESSED));
        assert!(!nvidia_compressed(NV_BL));
        assert!(!nvidia_compressed(DRM_FORMAT_MOD_LINEAR));
        let avail = [NV_BL_COMPRESSED, NV_BL, DRM_FORMAT_MOD_LINEAR];
        assert_eq!(choose_modifiers(&avail, ModifierPref::Auto).unwrap(), vec![NV_BL]);
        assert_eq!(choose_modifiers(&avail, ModifierPref::Linear).unwrap(), vec![0]);
        assert_eq!(choose_modifiers(&avail, ModifierPref::Explicit(NV_BL_COMPRESSED)).unwrap(), vec![NV_BL_COMPRESSED]);
        assert!(choose_modifiers(&avail, ModifierPref::Explicit(42)).is_err());
        assert_eq!(choose_modifiers(&[DRM_FORMAT_MOD_LINEAR, NV_BL_COMPRESSED], ModifierPref::Auto).unwrap(), vec![0]);
        assert_eq!(choose_modifiers(&[NV_BL_COMPRESSED], ModifierPref::Auto).unwrap(), vec![NV_BL_COMPRESSED]);
        assert!(choose_modifiers(&[], ModifierPref::Auto).is_err());
    }
}
