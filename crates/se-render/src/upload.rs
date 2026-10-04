//! Persistent, host-coherent upload buffers in system RAM, not a discrete GPU's BAR window.
//! wgpu's MAP_WRITE allocator prefers DEVICE_LOCAL | HOST_VISIBLE memory. Full-resolution
//! camera/browser rings must not compete with the desktop and encoders for that small heap.

use anyhow::{Context, Result, anyhow, ensure};
use ash::vk;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use wgpu::hal::api::Vulkan;

const SLOTS: usize = 3;

struct Upload {
    buffer: wgpu::Buffer,
    mapped: NonNull<u8>,
    size: usize,
}

// CPU writes require exclusive access to the ring and a completed submission. The Vulkan
// mapping is valid across threads; its lifetime is owned by the wgpu buffer's drop callback.
unsafe impl Send for Upload {}

impl Upload {
    fn new(device: &wgpu::Device, label: &str, size: u64) -> Result<Self> {
        ensure!(size > 0 && size <= device.limits().max_buffer_size && size <= isize::MAX as u64, "invalid upload buffer size {size}");
        let hal = unsafe { device.as_hal::<Vulkan>() }.ok_or_else(|| anyhow!("uploads require the renderer's Vulkan device"))?;
        let raw = hal.raw_device().clone();
        let instance = hal.shared_instance().raw_instance();
        let physical = hal.raw_physical_device();
        let props = unsafe { instance.get_physical_device_memory_properties(physical) };
        let unified = unsafe { instance.get_physical_device_properties(physical) }.device_type != vk::PhysicalDeviceType::DISCRETE_GPU;
        drop(hal);
        let info = vk::BufferCreateInfo::default().size(size).usage(vk::BufferUsageFlags::TRANSFER_SRC).sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { raw.create_buffer(&info, None) }.context("creating system-RAM upload buffer")?;
        let req = unsafe { raw.get_buffer_memory_requirements(buffer) };
        let coherent = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let compatible = |i: usize, ty: &vk::MemoryType| req.memory_type_bits & (1 << i) != 0 && ty.property_flags.contains(coherent);
        let types = props.memory_types_as_slice();
        let ty = types.iter().enumerate().find(|(i, ty)| compatible(*i, ty) && !ty.property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL))
            .or_else(|| unified.then(|| types.iter().enumerate().find(|(i, ty)| compatible(*i, ty))).flatten());
        let Some((index, _)) = ty else {
            unsafe { raw.destroy_buffer(buffer, None) };
            return Err(anyhow!("no coherent system-RAM upload memory on this GPU"));
        };
        let memory = match unsafe { raw.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(index as u32), None) } {
            Ok(memory) => memory,
            Err(error) => {
                unsafe { raw.destroy_buffer(buffer, None) };
                return Err(anyhow!("allocating system-RAM upload: {error}"));
            }
        };
        let cleanup = || unsafe {
            raw.destroy_buffer(buffer, None);
            raw.free_memory(memory, None);
        };
        if let Err(error) = unsafe { raw.bind_buffer_memory(buffer, memory, 0) } {
            cleanup();
            return Err(anyhow!("binding system-RAM upload: {error}"));
        }
        let mapped = match unsafe { raw.map_memory(memory, 0, req.size, vk::MemoryMapFlags::empty()) } {
            Ok(pointer) => NonNull::new(pointer.cast::<u8>()).expect("Vulkan successful map is non-null"),
            Err(error) => {
                cleanup();
                return Err(anyhow!("mapping system-RAM upload: {error}"));
            }
        };
        // create_buffer_from_hal requires initialized memory, including row padding.
        unsafe { mapped.as_ptr().write_bytes(0, size as usize) };
        let owner = raw.clone();
        let drop_callback: wgpu::hal::DropCallback = Box::new(move || unsafe {
            owner.unmap_memory(memory);
            owner.destroy_buffer(buffer, None);
            owner.free_memory(memory, None);
        });
        // COPY_SRC only: wgpu never maps this external buffer. Vulkan's coherent mapping is
        // written before submission; submit makes host writes available to the copy commands.
        let buffer = unsafe {
            device.create_buffer_from_hal::<Vulkan>(
                wgpu::hal::vulkan::Buffer::from_raw_externally_owned(buffer, drop_callback),
                &wgpu::BufferDescriptor { label: Some(label), size, usage: wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false },
            )
        };
        Ok(Self { buffer, mapped, size: size as usize })
    }
}

pub(crate) struct UploadRing {
    buffers: [Upload; SLOTS],
    ready: [Arc<AtomicBool>; SLOTS],
    used: [bool; SLOTS],
    next: usize,
}

impl UploadRing {
    pub(crate) fn new(device: &wgpu::Device, label: &str, size: u64) -> Result<Self> {
        Ok(Self {
            buffers: [Upload::new(device, label, size)?, Upload::new(device, label, size)?, Upload::new(device, label, size)?],
            ready: std::array::from_fn(|_| Arc::new(AtomicBool::new(true))),
            used: [false; SLOTS],
            next: 0,
        })
    }

    pub(crate) fn acquire(&mut self) -> Option<usize> {
        for offset in 0..SLOTS {
            let i = (self.next + offset) % SLOTS;
            if self.ready[i].swap(false, Ordering::Acquire) {
                self.used[i] = true;
                self.next = (i + 1) % SLOTS;
                return Some(i);
            }
        }
        None
    }

    pub(crate) fn write(&mut self, i: usize) -> &mut [u8] {
        assert!(self.used[i], "upload slot must be acquired before writing");
        let upload = &mut self.buffers[i];
        // The slot remains unavailable until GPU completion, and the ring owns the mapping.
        unsafe { std::slice::from_raw_parts_mut(upload.mapped.as_ptr(), upload.size) }
    }

    pub(crate) fn buffer(&self, i: usize) -> &wgpu::Buffer {
        &self.buffers[i].buffer
    }

    pub(crate) fn after_submit(&mut self, queue: &wgpu::Queue) {
        for i in 0..SLOTS {
            if std::mem::take(&mut self.used[i]) {
                let ready = self.ready[i].clone();
                // Not map_async: these buffers stay mapped. Reuse still requires completion
                // of the submission that reads them, not merely recording/submitting a copy.
                queue.on_submitted_work_done(move || ready.store(true, Ordering::Release));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::{Gpu, GpuOptions};

    #[test]
    fn ring_waits_for_gpu_completion_and_preserves_full_frame_uploads() {
        let gpu = Gpu::new(&GpuOptions { debug: true, ..Default::default() }).unwrap();
        let size = 1920 * 1080 * 4;
        let mut ring = UploadRing::new(&gpu.device, "full-HD system upload", size).unwrap();
        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("upload verification"), size: size * SLOTS as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false,
        });
        for round in 0..4u8 {
            let mut encoder = gpu.device.create_command_encoder(&Default::default());
            for i in 0..SLOTS {
                let slot = ring.acquire().unwrap();
                ring.write(slot).fill(round * 16 + i as u8 + 1);
                encoder.copy_buffer_to_buffer(ring.buffer(slot), 0, &readback, i as u64 * size, size);
            }
            assert!(ring.acquire().is_none(), "pending GPU reads must not be overwritten");
            gpu.queue.submit([encoder.finish()]);
            ring.after_submit(&gpu.queue);
            let (tx, rx) = std::sync::mpsc::channel();
            readback.slice(..).map_async(wgpu::MapMode::Read, move |result| tx.send(result).unwrap());
            gpu.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            rx.recv().unwrap().unwrap();
            {
                let data = readback.slice(..).get_mapped_range().unwrap();
                for (i, frame) in data.chunks_exact(size as usize).enumerate() {
                    assert!(frame.iter().all(|b| *b == round * 16 + i as u8 + 1), "frame {i} corrupted in round {round}");
                }
            }
            readback.unmap();
        }
        assert_eq!(gpu.errors.load(Ordering::Relaxed), 0);
    }
}
