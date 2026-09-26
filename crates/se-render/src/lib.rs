//! Compositor (§4): a dedicated 60 fps render thread on the NVIDIA GPU (wgpu/Vulkan) that turns
//! sources (cameras, web pages, patches, draw lists) into the `wide`, `tall`, `preview`, and
//! multiview `atlas` canvases — scene graph, morph/shader transitions, the effect library,
//! overlay layers, the video flash limiter — and exports them as dmabufs with explicit sync
//! over `frames.sock` (docs/frames-protocol.md), with a shared-memory fallback.

pub mod addr;
pub mod compose;
pub mod effects;
pub mod export;
pub mod gpu;
pub mod limiter;
pub mod loader;
pub mod lut;
pub mod patches;
pub mod perf;
pub mod pipelines;
pub mod plan;
pub mod renderer;
pub mod resources;
pub mod scene;
pub mod service;
pub mod sources;

pub use service::{RenderHandle, spawn, start, stop};

#[cfg(test)]
#[global_allocator]
static ALLOC: se_alloc::Counting = se_alloc::Counting;
