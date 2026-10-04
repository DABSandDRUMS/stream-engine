//! A small upload must not reserve most of a discrete GPU's CPU-visible VRAM window.
//! Kept in its own test binary: Vulkan heap usage is process-wide.

use ash::vk;
use se_render::gpu::{Gpu, GpuOptions};

#[test]
fn small_upload_leaves_small_host_visible_vram_heap_available() {
    const MIB: u64 = 1024 * 1024;
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let Some(adapter) = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN))
        .into_iter().find(|a| a.get_info().vendor == 0x10de)
    else {
        return;
    };
    let hal = unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }.unwrap();
    let raw = hal.shared_instance().raw_instance().clone();
    let physical = hal.raw_physical_device();
    drop(hal);
    let extensions = unsafe { raw.enumerate_device_extension_properties(physical) }.unwrap();
    if !extensions.iter().any(|e| e.extension_name_as_c_str().is_ok_and(|n| n == ash::ext::memory_budget::NAME)) {
        return;
    }
    let props = unsafe { raw.get_physical_device_memory_properties(physical) };
    let Some(heap) = props.memory_types_as_slice().iter().find_map(|ty| {
        let heap = ty.heap_index as usize;
        (ty.property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL | vk::MemoryPropertyFlags::HOST_VISIBLE)
            && props.memory_heaps[heap].size <= 256 * MIB)
            .then_some(heap)
    }) else {
        return; // Unified-memory and large-BAR GPUs do not have this constrained window.
    };
    let usage = || {
        let mut budget = vk::PhysicalDeviceMemoryBudgetPropertiesEXT::default();
        let mut props = vk::PhysicalDeviceMemoryProperties2::default().push_next(&mut budget);
        unsafe { raw.get_physical_device_memory_properties2(physical, &mut props) };
        budget.heap_usage[heap]
    };
    let before = usage();
    // Device creation itself initializes a staging pool; measure before opening it.
    let gpu = Gpu::new(&GpuOptions { adapter: Some(adapter.get_info().name), debug: false }).expect("GPU");
    let upload = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("small BAR upload"),
        size: MIB,
        usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: true,
    });
    {
        let mut data = upload.slice(..).get_mapped_range_mut().unwrap();
        data.slice(0..1).copy_from_slice(&[0x35]);
        data.slice(MIB as usize - 1..MIB as usize).copy_from_slice(&[0xc7]);
    }
    upload.unmap();
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("upload result"),
        size: MIB,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    encoder.copy_buffer_to_buffer(&upload, 0, &readback, 0, MIB);
    gpu.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    readback.slice(..).map_async(wgpu::MapMode::Read, move |result| tx.send(result).unwrap());
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    rx.recv().unwrap().unwrap();
    {
        let data = readback.slice(..).get_mapped_range().unwrap();
        assert_eq!(data[0], 0x35);
        assert_eq!(data[MIB as usize - 1], 0xc7);
    }
    readback.unmap();
    let reserved = usage().saturating_sub(before);
    println!("1 MiB upload reserves {} MiB of CPU-visible VRAM", reserved / MIB);
    assert!(reserved <= 16 * MIB, "small upload reserved {reserved} bytes of the constrained VRAM window");

    // Ten full-HD browser/camera rings used to request ~237 MiB of CPU-visible VRAM.
    // Hold every production ring alive at once; upload pixels, not just empty reservations.
    let before_sources = usage();
    let mut videos: Vec<se_render::sources::VideoGpu> = (0..10).map(|_| Default::default()).collect();
    let mut binds = se_render::resources::BindCache::default();
    let frame = se_hub::media::VideoFrame {
        width: 1920, height: 1080, stride: 1920 * 4, format: se_hub::media::PixelFormat::Rgba8,
        data: vec![0x7b; 1920 * 1080 * 4], seq: 1, ts: 0,
    };
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    for video in &mut videos {
        assert!(video.upload(&gpu, &mut encoder, &mut binds, "full-HD source", Some(&frame)));
    }
    gpu.queue.submit([encoder.finish()]);
    for video in &mut videos {
        video.after_submit(&gpu.queue);
    }
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let source_reserved = usage().saturating_sub(before_sources);
    println!("Ten full-HD upload rings add {} MiB of CPU-visible VRAM", source_reserved / MIB);
    assert!(source_reserved <= 4 * MIB, "full-HD source uploads consumed {source_reserved} bytes of the constrained heap");
}
