//! GPU context: the NVIDIA adapter (chosen explicitly — this machine also exposes the Ryzen
//! iGPU), a wgpu device opened through wgpu-hal so the dmabuf/sync-file Vulkan extensions can be
//! enabled, raw `ash` handles for export, and device-loss detection.

use anyhow::{Context, Result, anyhow};
use ash::{ext, khr, vk};
use parking_lot::Mutex;
use std::ffi::CStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use wgpu::hal::api::Vulkan;

/// Raw Vulkan handles of the wgpu device (valid while the [`Gpu`] lives).
#[derive(Clone)]
pub struct Vk {
    pub instance: ash::Instance,
    pub device: ash::Device,
    pub physical: vk::PhysicalDevice,
    pub queue_family: u32,
    pub mem_fd: Option<khr::external_memory_fd::Device>,
    pub drm_modifier: Option<ext::image_drm_format_modifier::Device>,
    pub sem_fd: Option<khr::external_semaphore_fd::Device>,
    pub dma_buf: bool,
    pub foreign_queue: bool,
    pub memory_budget: bool,
}

impl Vk {
    /// Everything needed to export canvases as dmabufs with explicit sync.
    pub fn can_export(&self) -> bool {
        self.mem_fd.is_some() && self.drm_modifier.is_some() && self.sem_fd.is_some() && self.dma_buf
    }

    /// Device-local heap usage and budget of this process in bytes (VK_EXT_memory_budget).
    pub fn memory_budget(&self) -> Option<(u64, u64)> {
        if !self.memory_budget {
            return None;
        }
        let mut budget = vk::PhysicalDeviceMemoryBudgetPropertiesEXT::default();
        let mut props = vk::PhysicalDeviceMemoryProperties2::default().push_next(&mut budget);
        unsafe { self.instance.get_physical_device_memory_properties2(self.physical, &mut props) };
        let heaps = props.memory_properties.memory_heap_count as usize;
        let heap_props = props.memory_properties.memory_heaps;
        let (mut used, mut total) = (0u64, 0u64);
        for (i, heap) in heap_props.iter().enumerate().take(heaps) {
            if heap.flags.contains(vk::MemoryHeapFlags::DEVICE_LOCAL) {
                used += budget.heap_usage[i];
                total += budget.heap_budget[i];
            }
        }
        Some((used, total))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Caps {
    /// Timestamp queries anywhere in an encoder (per-pass GPU timing).
    pub timestamps: bool,
    pub timestamp_period_ns: f32,
    pub dmabuf_export: bool,
}

pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub info: wgpu::AdapterInfo,
    pub caps: Caps,
    pub vk: Option<Vk>,
    pub lost: Arc<AtomicBool>,
    pub lost_reason: Arc<Mutex<String>>,
    /// Uncaptured validation/OOM errors (logged by the device callback).
    pub errors: Arc<AtomicU64>,
    /// Set when we destroy the device on purpose (simulated loss): recover anyway.
    pub simulate: Arc<AtomicBool>,
}

#[derive(Clone, Debug, Default)]
pub struct GpuOptions {
    /// Adapter name substring (`[render] adapter`, env `SE_GPU`); default: the NVIDIA GPU.
    pub adapter: Option<String>,
    /// Instance validation + debug labels (`--dev`).
    pub debug: bool,
}

const NVIDIA: u32 = 0x10de;

fn pick_adapter(adapters: Vec<wgpu::Adapter>, want: Option<&str>) -> Result<wgpu::Adapter> {
    let infos: Vec<wgpu::AdapterInfo> = adapters.iter().map(|a| a.get_info()).collect();
    let listing = infos.iter().map(|i| format!("{} ({:?}, {:?})", i.name, i.device_type, i.backend)).collect::<Vec<_>>().join(", ");
    let idx = if let Some(w) = want {
        let w = w.to_lowercase();
        infos.iter().position(|i| i.name.to_lowercase().contains(&w)).ok_or_else(|| anyhow!("no GPU matching `{w}` (have: {listing})"))?
    } else {
        infos
            .iter()
            .position(|i| i.vendor == NVIDIA)
            .or_else(|| infos.iter().position(|i| i.device_type == wgpu::DeviceType::DiscreteGpu))
            .or_else(|| infos.iter().position(|i| i.device_type != wgpu::DeviceType::Cpu))
            .ok_or_else(|| anyhow!("no usable Vulkan GPU (have: {listing})"))?
    };
    Ok(adapters.into_iter().nth(idx).expect("index from the same list"))
}

/// Device extensions we enable on top of wgpu's set when the driver has them.
const EXTRA_EXTENSIONS: &[&CStr] = &[
    khr::external_memory_fd::NAME,
    ext::external_memory_dma_buf::NAME,
    ext::image_drm_format_modifier::NAME,
    khr::external_semaphore_fd::NAME,
    ext::queue_family_foreign::NAME,
    ext::memory_budget::NAME,
];

impl Gpu {
    pub fn new(opts: &GpuOptions) -> Result<Gpu> {
        let flags = if opts.debug { wgpu::InstanceFlags::DEBUG | wgpu::InstanceFlags::VALIDATION } else { wgpu::InstanceFlags::empty() };
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN, flags, ..wgpu::InstanceDescriptor::new_without_display_handle() });
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
        let env = std::env::var("SE_GPU").ok();
        let adapter = pick_adapter(adapters, env.as_deref().or(opts.adapter.as_deref()))?;
        let info = adapter.get_info();
        let af = adapter.features();
        let mut features = wgpu::Features::empty();
        let timestamps = af.contains(wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS);
        if timestamps {
            features |= wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
        }
        let limits = adapter.limits();
        // Performance reserves 64 MiB of CPU-visible VRAM even for tiny uploads.
        // Renderer + UI share the discrete GPU's small BAR1 window with OBS/CEF.
        let memory_hints = wgpu::MemoryHints::MemoryUsage;

        // Open through wgpu-hal to add the export extensions.
        let supported: Vec<std::ffi::CString> = unsafe {
            let hal = adapter.as_hal::<Vulkan>().ok_or_else(|| anyhow!("adapter is not Vulkan"))?;
            let inst = hal.shared_instance().raw_instance();
            inst.enumerate_device_extension_properties(hal.raw_physical_device())
                .context("enumerating device extensions")?
                .iter()
                .filter_map(|p| p.extension_name_as_c_str().ok().map(|c| c.to_owned()))
                .collect()
        };
        let extra: Vec<&'static CStr> = EXTRA_EXTENSIONS.iter().copied().filter(|e| supported.iter().any(|s| s.as_c_str() == *e)).collect();
        let enabled = extra.clone();
        let open = unsafe {
            let hal = adapter.as_hal::<Vulkan>().ok_or_else(|| anyhow!("adapter is not Vulkan"))?;
            hal.open_with_callback(
                features,
                &limits,
                &memory_hints,
                Some(Box::new(move |args: wgpu::hal::vulkan::CreateDeviceCallbackArgs| {
                    for e in &extra {
                        if !args.extensions.contains(e) {
                            args.extensions.push(e);
                        }
                    }
                })),
            )
            .map_err(|e| anyhow!("opening Vulkan device: {e}"))?
        };
        let (device, queue) = unsafe {
            adapter.create_device_from_hal(
                open,
                &wgpu::DeviceDescriptor {
                    label: Some("se-render"),
                    required_features: features,
                    required_limits: limits,
                    experimental_features: wgpu::ExperimentalFeatures::disabled(),
                    memory_hints,
                    trace: wgpu::Trace::Off,
                },
            )
        }
        .context("creating wgpu device")?;

        let vk = unsafe {
            let hal_dev = device.as_hal::<Vulkan>().ok_or_else(|| anyhow!("device is not Vulkan"))?;
            let instance = hal_dev.shared_instance().raw_instance().clone();
            let raw = hal_dev.raw_device().clone();
            let has = |n: &CStr| hal_dev.enabled_device_extensions().contains(&n) || enabled.contains(&n);
            Vk {
                mem_fd: has(khr::external_memory_fd::NAME).then(|| khr::external_memory_fd::Device::new(&instance, &raw)),
                drm_modifier: has(ext::image_drm_format_modifier::NAME).then(|| ext::image_drm_format_modifier::Device::new(&instance, &raw)),
                sem_fd: has(khr::external_semaphore_fd::NAME).then(|| khr::external_semaphore_fd::Device::new(&instance, &raw)),
                dma_buf: has(ext::external_memory_dma_buf::NAME),
                foreign_queue: has(ext::queue_family_foreign::NAME),
                memory_budget: has(ext::memory_budget::NAME),
                physical: hal_dev.raw_physical_device(),
                queue_family: hal_dev.queue_family_index(),
                instance,
                device: raw,
            }
        };

        let lost = Arc::new(AtomicBool::new(false));
        let lost_reason = Arc::new(Mutex::new(String::new()));
        let simulate = Arc::new(AtomicBool::new(false));
        {
            let (lost, why, simulate) = (lost.clone(), lost_reason.clone(), simulate.clone());
            device.set_device_lost_callback(move |reason, msg| {
                // `Destroyed` is our own teardown unless we destroyed it to simulate a loss.
                if reason != wgpu::DeviceLostReason::Destroyed || simulate.load(Ordering::Acquire) {
                    *why.lock() = format!("{reason:?}: {msg}");
                    lost.store(true, Ordering::Release);
                }
            });
        }
        let errors = Arc::new(AtomicU64::new(0));
        {
            let (errors, lost) = (errors.clone(), lost.clone());
            device.on_uncaptured_error(Arc::new(move |e: wgpu::Error| {
                let n = errors.fetch_add(1, Ordering::Relaxed);
                if n < 20 || n.is_power_of_two() {
                    tracing::error!(target: "render", "GPU error #{}: {e}", n + 1);
                }
                if matches!(e, wgpu::Error::OutOfMemory { .. }) {
                    lost.store(true, Ordering::Release);
                }
            }));
        }
        let caps = Caps { timestamps, timestamp_period_ns: queue.get_timestamp_period(), dmabuf_export: vk.can_export() };
        tracing::info!(
            target: "render",
            "GPU: {} ({:?}, driver {} {}), timestamps: {timestamps}, dmabuf export: {}",
            info.name,
            info.device_type,
            info.driver,
            info.driver_info,
            caps.dmabuf_export
        );
        Ok(Gpu { instance, adapter, device, queue, info, caps, vk: Some(vk), lost, lost_reason, errors, simulate })
    }

    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    /// Destroy the device to exercise the recovery path (dev action).
    pub fn simulate_loss(&self) {
        self.simulate.store(true, Ordering::Release);
        self.device.destroy();
    }
}

/// Validate WGSL with naga first (precise line/column errors), then create the module inside a
/// validation error scope so a bad shader never reaches the uncaptured-error handler.
pub fn shader_module(device: &wgpu::Device, label: &str, src: &str) -> Result<wgpu::ShaderModule, ShaderError> {
    validate_wgsl(src)?;
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let m = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(src.into()) });
    if let Some(e) = pollster::block_on(scope.pop()) {
        return Err(ShaderError { line: None, column: None, message: e.to_string() });
    }
    Ok(m)
}

/// Run `f` (pipeline creation) in a validation error scope.
pub fn scoped<T>(device: &wgpu::Device, f: impl FnOnce() -> T) -> Result<T, String> {
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let v = f();
    match pollster::block_on(scope.pop()) {
        Some(e) => Err(e.to_string()),
        None => Ok(v),
    }
}

/// A shader compile error with its location in the compiled source (1-based).
#[derive(Clone, Debug, PartialEq)]
pub struct ShaderError {
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub message: String,
}

impl std::fmt::Display for ShaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.line, self.column) {
            (Some(l), Some(c)) => write!(f, "{l}:{c}: {}", self.message),
            (Some(l), None) => write!(f, "{l}: {}", self.message),
            _ => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ShaderError {}

/// Parse + validate with naga, mapping errors to line/column.
pub fn validate_wgsl(src: &str) -> Result<naga::Module, ShaderError> {
    let module = naga::front::wgsl::parse_str(src).map_err(|e| {
        let loc = e.location(src);
        ShaderError { line: loc.map(|l| l.line_number as usize), column: loc.map(|l| l.line_position as usize), message: e.message().to_string() }
    })?;
    naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all()).validate(&module).map_err(|e| {
        let loc = e.location(src);
        ShaderError {
            line: loc.map(|l| l.line_number as usize),
            column: loc.map(|l| l.line_position as usize),
            message: e.emit_to_string(src).lines().next().unwrap_or("validation error").trim_start_matches("error: ").to_string(),
        }
    })?;
    Ok(module)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naga_errors_carry_lines() {
        let e = validate_wgsl("fn a() -> f32 {\n  return 1.0\n}\n").unwrap_err();
        assert_eq!(e.line, Some(3), "{e}");
        let e = validate_wgsl("fn a() -> f32 {\n  let x: u32 = 1.0;\n  return 1.0;\n}\n").unwrap_err();
        assert_eq!(e.line, Some(2), "{e}");
        assert!(validate_wgsl("fn a() -> f32 { return 1.0; }").is_ok());
    }
}
