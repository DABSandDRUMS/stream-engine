//! The compositor proper: owns every GPU resource for one device and encodes a frame —
//! sources, scene graph per canvas, node/scene/canvas/output effects, transitions, overlays,
//! the flash limiter, dmabuf/shm export, and the multiview atlas (§4.1).

use crate::addr::{Resolved, Whens};
use crate::compose::{
    self, BLIT_COPY, BLIT_LIMIT, BLIT_OPAQUE, BlitUniform, FusedUniform, FxContext, FxEval, FxStage, FxUniform, Gfx, KIND_FLASH, LutLookup, NodeUniform,
};
use crate::effects::{Exec, LIBRARY, Point};
use crate::export::{DRM_FORMAT_ABGR8888, DmabufRing, FenceExport, MAX_BARRIERS, ownership_barrier};
use crate::fuse::{FusedPipe, Step, Steps};
use crate::gpu::Gpu;
use crate::limiter::{CELLS, FlashDetector};
use crate::lut::Lut;
use crate::patches::{FrameInputs, KIND_PATCH, PatchRt};
use crate::perf::{P_ATLAS, P_OUTPUT, P_PREVIEW, P_SOURCES, P_TALL, P_WIDE, Stats, Timestamps};
use crate::pipelines::{Pipelines, UserPipes, glide_layout, transition_manifest};
use crate::plan::{ATLAS, Attach, CANVASES, EffectKind, LAYOUTS, PREVIEW, Plan, ShaderSource, SourceKind, TALL, TrKind, WIDE, atlas_tiles};
use crate::resources::{Arena, BindCache, FreezePhotos, Layouts, MappedRing, Tex};
use crate::scene::{self, Item, MorphScratch, TrState};
use crate::sources::{VideoGpu, color_settings};
use se_core::config::NodeFit;
use se_frames::{FramesServer, RingDesc, RingKind, ShmBuffer};
use se_hub::draw::DrawReader;
use se_hub::media::VideoReader;
use se_hub::{Hub, Snapshot};
use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Readers of media/draw slots; live across renderer (device) re-creation.
#[derive(Default)]
pub struct Inputs {
    pub video: HashMap<String, VideoReader>,
    pub draw: HashMap<String, DrawReader>,
    video_gen: u64,
    draw_gen: u64,
}

impl Inputs {
    /// Pick up newly registered slots (allocates only when a slot was (re-)registered).
    pub fn refresh(&mut self, hub: &Hub, plan: &Plan, force: bool) {
        let vg = hub.video.generation.load(Ordering::Acquire);
        if force || vg != self.video_gen {
            self.video_gen = vg;
            for s in &plan.sources {
                if let SourceKind::Video { slot } = &s.kind
                    && let Some(r) = hub.video.take_reader(slot)
                {
                    self.video.insert(slot.clone(), r);
                }
            }
        }
        let dg = hub.draw.generation.load(Ordering::Acquire);
        if force || dg != self.draw_gen {
            self.draw_gen = dg;
            for (n, r) in hub.draw.take_new() {
                self.draw.insert(n, r);
            }
        }
    }
}

/// Asset loads requested by the render thread (served by the loader thread).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetRequest {
    Lut(String),
    Mask(String),
}

/// Messages applied at the start of a frame.
pub enum Msg {
    Plan(Arc<Plan>),
    /// A compiled patch (`Patch`) or transition (`Transition`) pipeline for device `device_gen`.
    Pipes {
        key: PipeKey,
        device_gen: u64,
        layout: se_patch::wgsl::Layout,
        pipes: Arc<UserPipes>,
    },
    Lut {
        path: String,
        lut: Result<Lut, String>,
    },
    Mask {
        path: String,
        mask: Result<(u32, u32, Vec<u8>), String>,
    },
    /// A trigger payload set this effect's level (None = use the `level` param).
    TriggerLevel {
        effect: String,
        level: Option<f32>,
    },
    /// A patch was triggered with this payload (`se_core::triggers::TriggerPayload::floats`).
    PatchTrigger {
        patch: String,
        payload: [f32; se_core::triggers::PAYLOAD_FLOATS],
    },
    /// A file under assets/ changed: reload it if it is in use (project-relative path).
    AssetChanged(String),
    /// A fused pointwise effect chain compiled for device `device_gen` (`crate::fuse`).
    Fused {
        device_gen: u64,
        chain: Box<[u8]>,
        pipeline: wgpu::RenderPipeline,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PipeKey {
    Patch(String),
    Transition(String),
    /// Custom enter/exit style shader file (project-relative path).
    Style(String),
}

enum Slot {
    Pending,
    Ready(u32),
    /// Reload requested; keeps showing the previous version (its id) until the new one arrives.
    Reloading(u32),
    Failed,
}

struct LutTable<'a> {
    map: &'a mut HashMap<String, Slot>,
    assets: &'a crossbeam_channel::Sender<AssetRequest>,
}

impl LutLookup for LutTable<'_> {
    fn lut(&mut self, path: &str) -> Option<u32> {
        match self.map.get(path) {
            Some(Slot::Ready(id) | Slot::Reloading(id)) => Some(*id),
            Some(_) => None,
            None => {
                self.map.insert(path.to_string(), Slot::Pending);
                let _ = self.assets.try_send(AssetRequest::Lut(path.to_string()));
                None
            }
        }
    }
}

enum SourceRt {
    Video(Box<VideoGpu>),
    Draw { layers: [Option<Tex>; LAYOUTS], seq: [u64; LAYOUTS] },
    Patch { patch: Option<usize>, layers: [Option<Tex>; LAYOUTS] },
    Solid([f32; 4]),
}

struct Src {
    rt: SourceRt,
    /// Source-level effect ping-pong (video sources).
    fx: [Option<Tex>; 2],
    /// Which `fx` texture holds this frame's result (None = no active source effects).
    fx_out: Option<usize>,
    layer_fx: [[Option<Tex>; 2]; LAYOUTS],
    layer_fx_out: [Option<usize>; LAYOUTS],
    used: u8,
    last_lut: String,
}

impl Src {
    /// Texture placements sample for `layout`, and whether it is premultiplied.
    fn tex(&self, layout: usize) -> Option<(&Tex, bool)> {
        if let Some(i) = self.layer_fx_out[layout] {
            return self.layer_fx[layout][i].as_ref().map(|t| (t, true));
        }
        match &self.rt {
            SourceRt::Video(v) => {
                if !v.has_frame {
                    return None;
                }
                match self.fx_out {
                    Some(i) => self.fx[i].as_ref().map(|t| (t, true)),
                    None => v.out.as_ref().map(|t| (t, true)),
                }
            }
            SourceRt::Draw { layers, seq } => (seq[layout] != 0).then(|| layers[layout].as_ref().map(|t| (t, false))).flatten(),
            SourceRt::Patch { layers, .. } => layers[layout].as_ref().map(|t| (t, true)),
            SourceRt::Solid(_) => None,
        }
    }
}

/// shm readbacks in flight per canvas (≤ readback ring size).
const SHM_PENDING: usize = 3;

struct Shm {
    bufs: Vec<ShmBuffer>,
    readback: MappedRing,
    row: u32,
    /// (readback index, seq, monotonic ns) waiting for the mapping.
    pending: Vec<(usize, u64, u64)>,
}

struct Flash {
    cells: wgpu::Buffer,
    readback: MappedRing,
    detector: FlashDetector,
    /// Frame time per readback buffer.
    times: [u64; 4],
}

struct CanvasRt {
    size: [u32; 2],
    t: Vec<Tex>,
    fin: Vec<Tex>,
    cur_fin: usize,
    quarter: Vec<Tex>,
    pool: Vec<Tex>,
    dmabuf: Option<DmabufRing>,
    dmabuf_failed: bool,
    shm: Option<Shm>,
    /// Frame sequence sent to clients (continues across device re-creation).
    seq: u64,
    /// Frames rendered by this renderer.
    rendered: u64,
    items: Vec<Item>,
    items_b: Vec<Item>,
    slots: Vec<i32>,
    slots_b: Vec<i32>,
    flash: Option<Flash>,
    last_render: u64,
}

impl CanvasRt {
    fn new(size: [u32; 2]) -> CanvasRt {
        CanvasRt {
            size,
            t: Vec::new(),
            fin: Vec::new(),
            cur_fin: 0,
            quarter: Vec::new(),
            pool: Vec::new(),
            dmabuf: None,
            dmabuf_failed: false,
            shm: None,
            seq: 0,
            rendered: 0,
            items: Vec::with_capacity(64),
            items_b: Vec::with_capacity(64),
            slots: Vec::with_capacity(64),
            slots_b: Vec::with_capacity(64),
            flash: None,
            last_render: 0,
        }
    }

    fn ensure(&mut self, device: &wgpu::Device, name: &str, final_textures: usize) -> bool {
        if self.t.len() == 4 && self.fin.len() >= final_textures {
            return false;
        }
        let _p = se_alloc::Pause::new();
        let s = self.size;
        while self.t.len() < 4 {
            self.t.push(Tex::target(device, &format!("{name} target {}", self.t.len()), s));
        }
        while self.fin.len() < final_textures {
            self.fin.push(Tex::target(device, &format!("{name} final {}", self.fin.len()), s));
        }
        if self.quarter.is_empty() {
            let q = [(s[0] / 4).max(1), (s[1] / 4).max(1)];
            self.quarter.push(Tex::target(device, &format!("{name} quarter 0"), q));
            self.quarter.push(Tex::target(device, &format!("{name} quarter 1"), q));
        }
        true
    }

    fn bytes(&self) -> u64 {
        let t: u64 = self.t.iter().chain(&self.fin).chain(&self.quarter).chain(&self.pool).map(Tex::bytes).sum();
        let d: u64 = self.dmabuf.as_ref().map_or(0, |r| r.images.iter().map(|i| i.size).sum());
        t + d
    }
}

/// Per-transition (or style file) compiled shader with its static params.
struct TransitionRt {
    layout: se_patch::wgsl::Layout,
    pipes: Arc<UserPipes>,
    values: Vec<[f32; 4]>,
    buf: Vec<f32>,
}

pub struct Renderer {
    pub gpu: Gpu,
    pub device_gen: u64,
    layouts: Arc<Layouts>,
    pipes: Pipelines,
    plan: Arc<Plan>,
    res: Resolved,
    whens: Whens,
    arena: Arena,
    binds: BindCache,
    canvases: Vec<CanvasRt>,
    vertical_enabled: bool,
    sources: Vec<Src>,
    patches: Vec<PatchRt>,
    freeze_photos: FreezePhotos,
    transitions: HashMap<String, TransitionRt>,
    /// Compiled custom enter/exit style files by path.
    styles: HashMap<String, TransitionRt>,
    user_patch_pipes: HashMap<String, (se_patch::wgsl::Layout, Arc<UserPipes>)>,
    /// Fused effect chains compiled for this device (kept across plan swaps), shortest first.
    fused: Vec<FusedPipe>,
    /// Effect passes this frame, and effects that ran inside fused passes.
    fx_counts: FxCounts,
    fade_layout: se_patch::wgsl::Layout,
    fade_buf: Vec<f32>,
    glide_layout: se_patch::wgsl::Layout,
    glide_buf: Vec<f32>,
    luts: HashMap<String, Slot>,
    lut_views: Vec<wgpu::TextureView>,
    masks: HashMap<String, Slot>,
    mask_views: Vec<wgpu::TextureView>,
    assets: crossbeam_channel::Sender<AssetRequest>,
    trigger_levels: Vec<Option<f32>>,
    fx: Vec<FxEval>,
    fx_node: Vec<FxEval>,
    globals_canvas: Vec<Attach>,
    globals_output: Vec<Attach>,
    morph: MorphScratch,
    ts: Option<Timestamps>,
    fence: Option<FenceExport>,
    vector: Option<se_vector::VectorRenderer>,
    frames: Option<Arc<FramesServer>>,
    pub stats: Arc<Stats>,
    frame: u64,
    t0: u64,
    last_now: u64,
    barrier_images: Vec<ash::vk::Image>,
    presents: Vec<(usize, u32, u64)>,
    atlas_tiles: Vec<[f32; 4]>,
    atlas_sources: Vec<u32>,
    last_vram_query: u64,
    vector_error_logged: bool,
}

impl Renderer {
    pub fn new(
        gpu: Gpu,
        device_gen: u64,
        plan: Arc<Plan>,
        assets: crossbeam_channel::Sender<AssetRequest>,
        frames: Option<Arc<FramesServer>>,
        stats: Arc<Stats>,
    ) -> anyhow::Result<Renderer> {
        let layouts = Layouts::new(&gpu.device, &gpu.queue);
        let pipes = Pipelines::new(&gpu.device, &layouts).map_err(anyhow::Error::msg)?;
        let fence = if frames.is_some() && gpu.caps.dmabuf_export {
            match FenceExport::new(&gpu) {
                Ok(f) => Some(f),
                Err(e) => {
                    tracing::warn!(target: "render", "explicit sync unavailable, dmabuf export disabled: {e:#}");
                    None
                }
            }
        } else {
            None
        };
        let ts = gpu.caps.timestamps.then(|| Timestamps::new(&gpu.device, gpu.caps.timestamp_period_ns));
        let vector = match se_vector::VectorRenderer::new(
            &gpu.device,
            se_vector::VectorOptions { assets_root: plan.project_root.join("assets"), font_family: plan.settings.font.clone() },
        ) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::error!(target: "render", "vector renderer unavailable (script patches will not draw): {e:#}");
                None
            }
        };
        let fade_layout = se_patch::wgsl::Layout::new(&transition_manifest("fade", &[]));
        let glide_layout = glide_layout();
        let arena = Arena::new(&gpu.device, 4 << 20);
        stats.device_ok.store(true, Ordering::Relaxed);
        let mut r = Renderer {
            device_gen,
            layouts,
            pipes,
            plan: plan.clone(),
            res: Resolved::default(),
            whens: Whens::default(),
            arena,
            binds: BindCache::default(),
            canvases: Vec::new(),
            vertical_enabled: true,
            sources: Vec::new(),
            patches: Vec::new(),
            freeze_photos: FreezePhotos::new(stats.freeze_photos.clone()),
            transitions: HashMap::new(),
            styles: HashMap::new(),
            user_patch_pipes: HashMap::new(),
            fused: Vec::new(),
            fx_counts: FxCounts::default(),
            fade_buf: vec![0.0; fade_layout.floats],
            fade_layout,
            glide_buf: vec![0.0; glide_layout.floats],
            glide_layout,
            luts: HashMap::new(),
            lut_views: Vec::new(),
            masks: HashMap::new(),
            mask_views: Vec::new(),
            assets,
            trigger_levels: Vec::new(),
            fx: Vec::with_capacity(64),
            fx_node: Vec::with_capacity(32),
            globals_canvas: Vec::new(),
            globals_output: Vec::new(),
            morph: MorphScratch::with_capacity(64),
            ts,
            fence,
            vector,
            frames,
            stats,
            frame: 0,
            t0: se_clock::now(),
            last_now: 0,
            barrier_images: Vec::with_capacity(MAX_BARRIERS),
            presents: Vec::with_capacity(8),
            atlas_tiles: Vec::new(),
            atlas_sources: Vec::new(),
            last_vram_query: 0,
            vector_error_logged: false,
            gpu,
        };
        r.set_plan(plan);
        Ok(r)
    }

    pub fn layouts(&self) -> Arc<Layouts> {
        self.layouts.clone()
    }

    pub fn plan(&self) -> &Arc<Plan> {
        &self.plan
    }

    /// Swap in a new plan (hot reload). Textures of canvases whose size is unchanged are kept.
    pub fn set_plan(&mut self, plan: Arc<Plan>) {
        let _p = se_alloc::Pause::new();
        let device = &self.gpu.device;
        // canvases (keep resources when the size is unchanged)
        let mut old: Vec<CanvasRt> = std::mem::take(&mut self.canvases);
        for c in 0..CANVASES {
            let size = [plan.canvases[c].width, plan.canvases[c].height];
            match old.get_mut(c).map(|o| std::mem::replace(o, CanvasRt::new(size))) {
                Some(o) if o.size == size => self.canvases.push(o),
                Some(o) => {
                    // size changed: clients must re-import (new generation)
                    if let Some(f) = &self.frames {
                        if o.dmabuf.is_some() {
                            let _ = f.set_ring(c as u32, RingKind::Dmabuf, None);
                        }
                        if o.shm.is_some() {
                            let _ = f.set_ring(c as u32, RingKind::Shm, None);
                        }
                    }
                    self.canvases.push(CanvasRt::new(size));
                }
                None => self.canvases.push(CanvasRt::new(size)),
            }
        }
        // sources: keep GPU state of video sources that still exist
        let mut keep: HashMap<String, Src> = HashMap::new();
        for (i, s) in std::mem::take(&mut self.sources).into_iter().enumerate() {
            if let Some(sp) = self.plan.sources.get(i) {
                keep.insert(sp.name.clone(), s);
            }
        }
        for sp in &plan.sources {
            let reuse = keep
                .remove(&sp.name)
                .filter(|s| matches!((&s.rt, &sp.kind), (SourceRt::Video(_), SourceKind::Video { .. }) | (SourceRt::Draw { .. }, SourceKind::Draw { .. })));
            let src = match reuse {
                Some(s) => s,
                None => Src {
                    rt: match &sp.kind {
                        SourceKind::Video { .. } => SourceRt::Video(Box::default()),
                        SourceKind::Draw { .. } => SourceRt::Draw { layers: [None, None], seq: [0; LAYOUTS] },
                        SourceKind::Patch { id } => SourceRt::Patch { patch: plan.patch_index.get(id).copied(), layers: [None, None] },
                        SourceKind::Solid(c) => SourceRt::Solid(*c),
                    },
                    fx: [None, None],
                    fx_out: None,
                    layer_fx: std::array::from_fn(|_| [None, None]),
                    layer_fx_out: [None; LAYOUTS],
                    used: 0,
                    last_lut: String::new(),
                },
            };
            self.sources.push(src);
        }
        // generated sources: (re)allocate layer textures at the planned sizes
        for (sp, s) in plan.sources.iter().zip(self.sources.iter_mut()) {
            let usage = if matches!(sp.kind, SourceKind::Draw { .. }) {
                se_vector::required_texture_usage() | wgpu::TextureUsages::COPY_SRC
            } else {
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC
            };
            let layers = match &mut s.rt {
                SourceRt::Draw { layers, seq } => {
                    *seq = [0; LAYOUTS];
                    Some(layers)
                }
                SourceRt::Patch { layers, patch } => {
                    if let SourceKind::Patch { id } = &sp.kind {
                        *patch = plan.patch_index.get(id).copied();
                    }
                    Some(layers)
                }
                _ => None,
            };
            if let Some(layers) = layers {
                for (li, l) in layers.iter_mut().enumerate() {
                    let size = sp.sizes[li];
                    if l.as_ref().is_none_or(|t| t.size != size) {
                        if let Some(t) = l.take() {
                            self.binds.forget(t.id);
                        }
                        *l = Some(Tex::new(device, &format!("{} {}", sp.name, crate::plan::LAYOUT_NAMES[li]), size, crate::resources::COLOR, usage));
                    }
                }
            }
        }
        // patches: keep compiled pipelines across plan swaps
        self.patches = plan
            .patches
            .iter()
            .map(|pp| {
                let mut rt = PatchRt::new(device, pp);
                if let Some((layout, pipes)) = self.user_patch_pipes.get(&pp.manifest.id)
                    && *layout == pp.layout
                {
                    rt.set_pipes(layout.clone(), pipes.clone());
                }
                rt
            })
            .collect();
        // transitions: drop compiled ones that no longer exist; static params re-read
        self.transitions.retain(|name, _| plan.transition_index.contains_key(name));
        for (name, t) in self.transitions.iter_mut() {
            t.values = transition_values(&plan, name, &t.layout);
        }
        self.styles.retain(|path, _| plan.styles.iter().any(|s| matches!(s, ShaderSource::File(p) if p.to_str() == Some(path.as_str()))));
        self.trigger_levels = vec![None; plan.effects.len()];
        self.globals_canvas = plan.global_attaches(Point::Canvas);
        self.globals_output = plan.global_attaches(Point::Output);
        self.whens.reset(plan.whens.len());
        self.res.invalidate();
        self.atlas_sources = plan.atlas_sources();
        self.atlas_tiles = atlas_tiles(self.atlas_sources.len(), [plan.canvases[ATLAS].width, plan.canvases[ATLAS].height]);
        if let Some(v) = &mut self.vector {
            v.set_options(se_vector::VectorOptions { assets_root: plan.project_root.join("assets"), font_family: plan.settings.font.clone() });
        }
        self.binds.clear();
        self.plan = plan;
    }

    pub fn apply(&mut self, msg: Msg) {
        let _p = se_alloc::Pause::new();
        match msg {
            Msg::Plan(p) => self.set_plan(p),
            Msg::Pipes { key, device_gen, layout, pipes } => {
                if device_gen != self.device_gen {
                    return;
                }
                match key {
                    PipeKey::Patch(id) => {
                        if let Some(i) = self.plan.patch_index.get(&id) {
                            self.patches[*i].set_pipes(layout.clone(), pipes.clone());
                        }
                        self.user_patch_pipes.insert(id, (layout, pipes));
                    }
                    PipeKey::Transition(name) => {
                        let values = transition_values(&self.plan, &name, &layout);
                        let buf = vec![0.0; layout.floats];
                        self.transitions.insert(name, TransitionRt { layout, pipes, values, buf });
                    }
                    PipeKey::Style(path) => {
                        let buf = vec![0.0; layout.floats];
                        self.styles.insert(path, TransitionRt { layout, pipes, values: Vec::new(), buf });
                    }
                }
            }
            Msg::Lut { path, lut } => match lut {
                Ok(l) => {
                    let view = crate::lut::upload(&self.gpu.device, &self.gpu.queue, &l);
                    let id = match self.luts.get(&path) {
                        Some(Slot::Reloading(id)) => {
                            self.lut_views[*id as usize - 1] = view;
                            self.binds.clear();
                            *id
                        }
                        _ => {
                            self.lut_views.push(view);
                            self.lut_views.len() as u32
                        }
                    };
                    self.luts.insert(path, Slot::Ready(id));
                }
                Err(e) => {
                    tracing::error!(target: "render", "LUT {path}: {e}");
                    self.luts.insert(path, Slot::Failed);
                }
            },
            Msg::Mask { path, mask } => match mask {
                Ok((w, h, rgba)) => {
                    let t = self.gpu.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("mask"),
                        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: crate::resources::COLOR,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                    self.gpu.queue.write_texture(
                        t.as_image_copy(),
                        &rgba,
                        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
                        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    );
                    let view = t.create_view(&Default::default());
                    let id = match self.masks.get(&path) {
                        Some(Slot::Reloading(id)) => {
                            self.mask_views[(*id - 1_000_001) as usize] = view;
                            self.binds.clear();
                            *id
                        }
                        _ => {
                            self.mask_views.push(view);
                            1_000_000 + self.mask_views.len() as u32
                        }
                    };
                    self.masks.insert(path, Slot::Ready(id));
                }
                Err(e) => {
                    tracing::error!(target: "render", "mask {path}: {e}");
                    self.masks.insert(path, Slot::Failed);
                }
            },
            Msg::TriggerLevel { effect, level } => {
                if let Some(i) = self.plan.effect_index.get(&effect) {
                    self.trigger_levels[*i] = level;
                }
            }
            Msg::AssetChanged(path) => {
                for (map, kind) in [(&mut self.luts, 0), (&mut self.masks, 1)] {
                    if let Some(Slot::Ready(id)) = map.get(&path).map(|s| match s {
                        Slot::Ready(i) => Slot::Ready(*i),
                        _ => Slot::Failed,
                    }) {
                        map.insert(path.clone(), Slot::Reloading(id));
                        let req = if kind == 0 { AssetRequest::Lut(path.clone()) } else { AssetRequest::Mask(path.clone()) };
                        let _ = self.assets.try_send(req);
                    } else if matches!(map.get(&path), Some(Slot::Failed)) {
                        map.remove(&path);
                    }
                }
            }
            Msg::Fused { device_gen, chain, pipeline } => {
                if device_gen == self.device_gen {
                    self.fused.retain(|f| f.chain != chain);
                    let at = self.fused.partition_point(|f| f.chain.len() <= chain.len());
                    self.fused.insert(at, FusedPipe { chain, pipeline });
                }
            }
            Msg::PatchTrigger { patch, payload } => {
                if patch == "freeze_frame" {
                    self.freeze_photos.request();
                }
                if let Some(i) = self.plan.patch_index.get(&patch) {
                    let rt = &mut self.patches[*i];
                    rt.trigger_count = rt.trigger_count.wrapping_add(1);
                    rt.trigger = payload;
                    rt.trigger_at = None;
                }
            }
        }
    }

    fn mask(&mut self, path: &Option<String>) -> Option<(u32, usize)> {
        let p = path.as_deref()?;
        match self.masks.get(p) {
            Some(Slot::Ready(id) | Slot::Reloading(id)) => Some((*id, (*id - 1_000_001) as usize)),
            Some(_) => None,
            None => {
                let _pz = se_alloc::Pause::new();
                self.masks.insert(p.to_string(), Slot::Pending);
                let _ = self.assets.try_send(AssetRequest::Mask(p.to_string()));
                None
            }
        }
    }

    /// Render one frame representing master-clock time `now`.
    pub fn frame(&mut self, snap: &Snapshot, now: u64, inputs: &mut Inputs) {
        let started = Instant::now();
        let scope = se_alloc::Scope::begin();
        self.frame += 1;
        let plan = self.plan.clone();
        self.res.update(snap, &plan.state, &plan.signals);
        self.vertical_enabled = self.res.bool(snap, plan.vertical_enabled, true);
        {
            let _p = se_alloc::Pause::new();
            let _ = self.gpu.device.poll(wgpu::PollType::Poll);
            self.freeze_photos.retry_worker(now);
        }
        if let Some(ts) = &mut self.ts
            && ts.poll()
        {
            self.stats.set_gpu(ts.total, &ts.last);
        }
        self.read_flash();
        self.deliver_shm(now);

        let dt = if self.last_now == 0 { 1.0 / 60.0 } else { (now.saturating_sub(self.last_now)) as f32 / 1e9 };
        self.last_now = now;
        let mut palette = plan.settings.palette;
        for (i, a) in plan.palette.iter().enumerate() {
            palette[i] = self.res.vec4(snap, *a, palette[i]);
        }
        let fi = FrameInputs { time: (now.saturating_sub(self.t0)) as f32 / 1e9, dt, frame: self.frame as u32, palette };

        // what to render
        let demand = |c: usize| self.frames.as_ref().map_or(se_frames::Demand::default(), |f| f.demand(c as u32));
        let want_preview = demand(PREVIEW).any();
        let atlas_period = 1_000_000_000 / plan.canvases[ATLAS].fps.max(1) as u64;
        let want_atlas = demand(ATLAS).any() && now.saturating_sub(self.canvases[ATLAS].last_render) + 2_000_000 >= atlas_period;
        let wanted = [true, self.vertical_enabled, want_preview, want_atlas];
        let tr = scene::transition_state(&plan, snap, &self.res, now);
        let preview_scene = self.res.str(snap, plan.show.preview).and_then(|s| plan.scene_index.get(s).copied());

        // scene evaluation + source usage
        for s in &mut self.sources {
            s.used = 0;
        }
        for c in [WIDE, TALL, PREVIEW] {
            let cv = &mut self.canvases[c];
            cv.items.clear();
            cv.items_b.clear();
            if !wanted[c] {
                continue;
            }
            let layout = plan.canvases[c].layout;
            let size = [cv.size[0] as f32, cv.size[1] as f32];
            if c == PREVIEW {
                if let Some(s) = preview_scene {
                    scene::eval_layout(&plan, s, layout, size, snap, &self.res, &mut self.whens, &mut cv.items);
                }
            } else if let (true, Some(from), Some(to)) = (tr.active, tr.from, tr.to) {
                let tp = &plan.transitions[tr.transition];
                match tp.kind {
                    TrKind::Morph | TrKind::Combined => {
                        scene::eval_morph(&plan, tp, from, to, layout, size, tr.eased(&plan), snap, &self.res, &mut self.whens, &mut self.morph, &mut cv.items);
                    }
                    TrKind::Glide => {
                        // incoming side → items (drawn as the program), outgoing → items_b
                        let t = tr.progress;
                        scene::eval_glide(&plan, tp, from, to, layout, size, t, snap, &self.res, &mut self.whens, &mut self.morph, &mut cv.items_b, &mut cv.items);
                    }
                    TrKind::Shader => {
                        scene::eval_layout(&plan, to, layout, size, snap, &self.res, &mut self.whens, &mut cv.items);
                        scene::eval_layout(&plan, from, layout, size, snap, &self.res, &mut self.whens, &mut cv.items_b);
                    }
                    TrKind::Cut => scene::eval_layout(&plan, to, layout, size, snap, &self.res, &mut self.whens, &mut cv.items),
                }
            } else if let Some(to) = tr.to {
                scene::eval_layout(&plan, to, layout, size, snap, &self.res, &mut self.whens, &mut cv.items);
            }
            scene::finish(&mut cv.items, size);
            scene::finish(&mut cv.items_b, size);
            for it in cv.items.iter().chain(cv.items_b.iter()) {
                self.sources[it.source as usize].used |= 1 << layout;
            }
        }
        for o in &plan.overlays {
            for c in [WIDE, TALL, PREVIEW] {
                if o.canvases[c] && wanted[c] {
                    self.sources[o.source as usize].used |= 1 << plan.canvases[c].layout;
                }
            }
        }
        if want_atlas {
            for s in &self.atlas_sources {
                self.sources[*s as usize].used |= 1 << WIDE;
            }
        }
        let mut used_bits = [0u64; 4];
        for (i, s) in self.sources.iter().enumerate().take(256) {
            if s.used != 0 {
                used_bits[i / 64] |= 1 << (i % 64);
            }
        }
        self.stats.set_used(&used_bits);

        let mut enc = {
            let _p = se_alloc::Pause::new();
            self.gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") })
        };
        self.arena.reset();
        self.fx_counts = FxCounts::default();
        if let Some(ts) = &mut self.ts {
            ts.begin_frame();
            ts.start(&mut enc, P_SOURCES);
        }
        self.update_sources(&mut enc, snap, inputs, &fi);
        if let Some(ts) = &mut self.ts {
            ts.end(&mut enc, P_SOURCES);
        }

        let flash_scale = self.flash_scale();
        let passes = [(WIDE, P_WIDE), (TALL, P_TALL), (PREVIEW, P_PREVIEW)];
        for (c, pass) in passes {
            if !wanted[c] {
                continue;
            }
            if let Some(ts) = &mut self.ts {
                ts.start(&mut enc, pass);
            }
            let program = if c == PREVIEW { preview_scene } else { tr.to };
            let t = if c == PREVIEW { TrState { active: false, progress: 1.0, from: None, to: preview_scene, transition: tr.transition } } else { tr };
            self.render_canvas(&mut enc, c, snap, &fi, program, &t, flash_scale);
            if let Some(ts) = &mut self.ts {
                ts.end(&mut enc, pass);
            }
        }
        if want_atlas {
            if let Some(ts) = &mut self.ts {
                ts.start(&mut enc, P_ATLAS);
            }
            self.render_atlas(&mut enc);
            self.canvases[ATLAS].last_render = now;
            if let Some(ts) = &mut self.ts {
                ts.end(&mut enc, P_ATLAS);
            }
        }
        if let Some(ts) = &mut self.ts {
            ts.start(&mut enc, P_OUTPUT);
        }
        self.presents.clear();
        self.barrier_images.clear();
        for c in [WIDE, TALL, PREVIEW, ATLAS] {
            if wanted[c] {
                self.export_canvas(&mut enc, c, now);
            }
        }
        if let Some(ts) = &mut self.ts {
            ts.end(&mut enc, P_OUTPUT);
            ts.resolve(&mut enc, self.frame);
        }
        {
            let _p = se_alloc::Pause::new();
            self.arena.upload(&self.gpu.queue);
        }
        self.stats.arena_overflow.store(self.arena.overflowed(), Ordering::Relaxed);
        self.submit(enc, now);
        self.after_submit();

        let ms = started.elapsed().as_secs_f32() * 1000.0;
        self.stats.set_frame(ms);
        self.stats.set_fx(self.fx_counts.passes, self.fx_counts.fused);
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        if now.saturating_sub(self.last_vram_query) > 1_000_000_000 {
            self.last_vram_query = now;
            let _p = se_alloc::Pause::new();
            if let Some((used, budget)) = self.gpu.vk.as_ref().and_then(|v| v.memory_budget()) {
                self.stats.vram_bytes.store(used, Ordering::Relaxed);
                self.stats.vram_budget.store(budget, Ordering::Relaxed);
            }
            self.stats.own_bytes.store(self.own_bytes(), Ordering::Relaxed);
        }
        let allocs = scope.allocs();
        if allocs > 0 && se_alloc::installed() {
            let n = self.stats.alloc_violations.fetch_add(allocs, Ordering::Relaxed);
            if n == 0 && cfg!(debug_assertions) {
                tracing::warn!(target: "render", "frame {} allocated {allocs} times outside third-party calls", self.frame);
            }
        }
    }

    fn own_bytes(&self) -> u64 {
        let c: u64 = self.canvases.iter().map(CanvasRt::bytes).sum();
        let s: u64 = self
            .sources
            .iter()
            .map(|s| {
                let fx: u64 = s.fx.iter().flatten().chain(s.layer_fx.iter().flat_map(|l| l.iter().flatten())).map(Tex::bytes).sum();
                fx + match &s.rt {
                    SourceRt::Video(v) => v.bytes,
                    SourceRt::Draw { layers, .. } | SourceRt::Patch { layers, .. } => layers.iter().flatten().map(Tex::bytes).sum(),
                    SourceRt::Solid(_) => 0,
                }
            })
            .sum();
        let p: u64 = self.patches.iter().map(PatchRt::frame_bytes).sum();
        c + s + p + (4 << 20) + self.freeze_photos.bytes()
    }

    fn flash_scale(&self) -> f32 {
        if !self.plan.settings.flash_limit {
            return 1.0;
        }
        let mut s: f32 = 1.0;
        for c in [WIDE, TALL] {
            if c == TALL && !self.vertical_enabled {
                continue;
            }
            if let Some(f) = &self.canvases[c].flash {
                s = s.min(f.detector.effect_scale());
            }
        }
        s
    }

    fn max_step(&self) -> f32 {
        if !self.plan.settings.flash_limit {
            return 1.0;
        }
        let mut k: f32 = 1.0;
        for c in [WIDE, TALL] {
            if c == TALL && !self.vertical_enabled {
                continue;
            }
            if let Some(f) = &self.canvases[c].flash {
                k = k.min(f.detector.max_step());
            }
        }
        k
    }

    fn read_flash(&mut self) {
        let _p = se_alloc::Pause::new();
        let mut limit: f32 = 0.0;
        let mut rate: f32 = 0.0;
        let mut limited = false;
        for c in [WIDE, TALL] {
            let Some(f) = &mut self.canvases[c].flash else { continue };
            while let Some(i) = f.readback.ready_read() {
                let mut cells = [0.0f32; CELLS];
                if let Ok(view) = f.readback.buffers[i].slice(..).get_mapped_range() {
                    cells.copy_from_slice(bytemuck::cast_slice(&view[..CELLS * 4]));
                }
                f.readback.release_read(i);
                f.detector.observe(&cells, f.times[i]);
            }
            if c == TALL && !self.vertical_enabled {
                continue;
            }
            let s = f.detector.state();
            limit = limit.max(s.limit);
            rate = rate.max(s.max_rate);
            limited |= s.violating || s.limit > 0.05;
        }
        self.stats.set_flash(limit, rate, limited && self.plan.settings.flash_limit);
    }

    fn update_sources(&mut self, enc: &mut wgpu::CommandEncoder, snap: &Snapshot, inputs: &mut Inputs, fi: &FrameInputs) {
        let plan = self.plan.clone();
        let Renderer {
            gpu,
            layouts,
            pipes,
            arena,
            binds,
            sources,
            patches,
            freeze_photos,
            luts,
            lut_views,
            assets,
            whens,
            res,
            fx,
            trigger_levels,
            vector,
            vector_error_logged,
            frame,
            fused,
            fx_counts,
            ..
        } = self;
        let device = &gpu.device;
        // particle simulation once per frame for every used particles patch
        for (si, s) in sources.iter_mut().enumerate() {
            if s.used == 0 {
                continue;
            }
            let sp = &plan.sources[si];
            match &mut s.rt {
                SourceRt::Video(v) => {
                    let SourceKind::Video { slot } = &sp.kind else { continue };
                    let fresh = inputs.video.get_mut(slot.as_str()).and_then(|r| r.fresh());
                    v.upload(gpu, enc, binds, &sp.name, fresh);
                    let lut_path = res.str(snap, sp.color.lut).unwrap_or("");
                    let mut lut = None;
                    if !lut_path.is_empty() {
                        let mut table = LutTable { map: luts, assets };
                        if let Some(id) = table.lut(lut_path) {
                            lut = Some((id, &lut_views[id as usize - 1]));
                        }
                    }
                    if s.last_lut != lut_path {
                        let _p = se_alloc::Pause::new();
                        s.last_lut.clear();
                        s.last_lut.push_str(lut_path);
                    }
                    if let Some(l) = v.layout {
                        let settings = color_settings(snap, res, &sp.color, l.format, l.width, lut.is_some());
                        v.convert(device, enc, layouts, &pipes.convert, arena, binds, settings, lut);
                    }
                    // source effects (all placements)
                    s.fx_out = None;
                    if !sp.fx.is_empty() && v.has_frame {
                        fx.clear();
                        let cx = FxContext { plan: &plan, snap, res, trigger_levels, flash_scale: 1.0 };
                        compose::eval_attaches(&cx, whens, &mut LutTable { map: luts, assets }, &sp.fx, fx);
                        if !fx.is_empty() {
                            let out = v.out.as_ref().expect("has_frame");
                            let size = out.size;
                            for t in s.fx.iter_mut() {
                                if t.as_ref().is_none_or(|t| t.size != size) {
                                    let _p = se_alloc::Pause::new();
                                    *t = Some(Tex::target(device, &format!("{} fx", sp.name), size));
                                }
                            }
                            let mut g = Gfx { device, layouts, pipes, arena, binds };
                            let [a, b] = &s.fx;
                            let (a, b) = (a.as_ref().expect("allocated"), b.as_ref().expect("allocated"));
                            let mut cur = out;
                            let mut which = 0;
                            let chain = Chain { evals: fx, fused, region: [0.0, 0.0, 1.0, 1.0], scissor: None, quarter: None, ctx: CANVASES };
                            for step in Steps::new(&plan, fx, fused) {
                                let dst = if which == 0 { a } else { b };
                                apply_step(enc, &mut g, &plan, patches, freeze_photos, &chain, step, cur, dst, snap, res, fi, lut_views, fx_counts);
                                cur = dst;
                                s.fx_out = Some(which);
                                which ^= 1;
                            }
                        }
                    }
                }
                SourceRt::Draw { layers, seq } => {
                    let SourceKind::Draw { slot } = &sp.kind else { continue };
                    let Some(reader) = inputs.draw.get_mut(slot.as_str()) else { continue };
                    let list = reader.latest();
                    for li in 0..LAYOUTS {
                        if s.used & (1 << li) == 0 || list.seq == 0 || list.seq == seq[li] {
                            continue;
                        }
                        let (Some(t), Some(v)) = (&layers[li], vector.as_mut()) else { continue };
                        let _p = se_alloc::Pause::new();
                        match v.render(device, &gpu.queue, list, &t.view, t.size) {
                            Ok(()) => seq[li] = list.seq,
                            Err(e) => {
                                if !*vector_error_logged {
                                    *vector_error_logged = true;
                                    tracing::error!(target: "render", "draw list {slot}: {e:#}");
                                }
                            }
                        }
                    }
                }
                SourceRt::Patch { patch: Some(pi), layers } => {
                    let (pi, pp) = (*pi, &plan.patches[*pi]);
                    let rt = &mut patches[pi];
                    let count = rt.particle_count(pp);
                    let mut simulated = false;
                    for (li, layer) in layers.iter().enumerate() {
                        if s.used & (1 << li) == 0 {
                            continue;
                        }
                        let Some(t) = layer else { continue };
                        let now_ns = (fi.time as f64 * 1e9) as u64;
                        if rt.min_interval_ns > 0 && now_ns.saturating_sub(rt.last_render_ns[li]) + 1_000_000 < rt.min_interval_ns && rt.last_render_ns[li] != 0
                        {
                            continue;
                        }
                        rt.last_render_ns[li] = now_ns.max(1);
                        let res_px = [t.size[0] as f32, t.size[1] as f32];
                        let off = rt.uniforms(pp, snap, res, fi, res_px, se_patch::wgsl::FULL, 0.0, None, None, None, arena);
                        if !simulated {
                            rt.simulate(device, enc, layouts, arena, binds, *frame, off, count);
                            simulated = true;
                        }
                        rt.draw(device, enc, layouts, arena, binds, &t.view, off, None, None, count, None);
                    }
                }
                SourceRt::Patch { patch: None, .. } | SourceRt::Solid(_) => {}
            }
            // Generated sources share their post-FX image across all placements in a layout.
            s.layer_fx_out = [None; LAYOUTS];
            if !matches!(s.rt, SourceRt::Video(_)) && !sp.fx.is_empty() {
                fx.clear();
                let cx = FxContext { plan: &plan, snap, res, trigger_levels, flash_scale: 1.0 };
                compose::eval_attaches(&cx, whens, &mut LutTable { map: luts, assets }, &sp.fx, fx);
                if !fx.is_empty() {
                    for li in 0..LAYOUTS {
                        if s.used & (1 << li) == 0 { continue; }
                        let (input, premult, color, size) = match &s.rt {
                            SourceRt::Draw { layers, seq } if seq[li] != 0 => match layers[li].as_ref() {
                                Some(t) => (Some(t), false, [0.0; 4], t.size),
                                None => continue,
                            },
                            SourceRt::Patch { layers, .. } => match layers[li].as_ref() {
                                Some(t) => (Some(t), true, [0.0; 4], t.size),
                                None => continue,
                            },
                            SourceRt::Solid(color) => (None, true, *color, sp.sizes[li]),
                            _ => continue,
                        };
                        if size[0] == 0 || size[1] == 0 { continue; }
                        for t in &mut s.layer_fx[li] {
                            if t.as_ref().is_none_or(|t| t.size != size) {
                                if let Some(old) = t.as_ref() { binds.forget(old.id); }
                                let _p = se_alloc::Pause::new();
                                *t = Some(Tex::target(device, &format!("{} source fx", sp.name), size));
                            }
                        }
                        let mut g = Gfx { device, layouts, pipes, arena, binds };
                        let [a, b] = &s.layer_fx[li];
                        let (a, b) = (a.as_ref().expect("allocated"), b.as_ref().expect("allocated"));
                        {
                            let _p = se_alloc::Pause::new();
                            let mut pass = compose::begin(enc, "source fx input", &a.view, Some([0.0; 4]));
                            let u = NodeUniform {
                                dst: [0.0, 0.0, size[0] as f32, size[1] as f32],
                                uv: [0.0, 0.0, 1.0, 1.0],
                                target_size: [size[0] as f32, size[1] as f32], color,
                                opacity: 1.0, premultiplied: f32::from(u8::from(premult)),
                                solid: f32::from(u8::from(input.is_none())), ..Default::default()
                            };
                            compose::draw_node(&mut pass, &mut g, &u, input, None, &pipes.composite_copy);
                        }
                        let mut current = 0;
                        let chain = Chain { evals: fx, fused, region: [0.0, 0.0, 1.0, 1.0], scissor: None, quarter: None, ctx: CANVASES + li };
                        for step in Steps::new(&plan, fx, fused) {
                            let (src, dst) = if current == 0 { (a, b) } else { (b, a) };
                            apply_step(enc, &mut g, &plan, patches, freeze_photos, &chain, step, src, dst, snap, res, fi, lut_views, fx_counts);
                            current ^= 1;
                        }
                        s.layer_fx_out[li] = Some(current);
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_canvas(
        &mut self,
        enc: &mut wgpu::CommandEncoder,
        c: usize,
        snap: &Snapshot,
        fi: &FrameInputs,
        program: Option<usize>,
        tr: &TrState,
        flash_scale: f32,
    ) {
        let plan = self.plan.clone();
        let final_textures = if c == PREVIEW { 1 } else { 2 };
        if self.canvases[c].ensure(&self.gpu.device, plan.canvases[c].name, final_textures) {
            self.binds.clear();
        }
        let layout = plan.canvases[c].layout;
        let max_step = self.max_step();
        // request masks used by visible items (loaded by the asset thread)
        for i in 0..self.canvases[c].items.len() + self.canvases[c].items_b.len() {
            let cv = &self.canvases[c];
            let it = if i < cv.items.len() { cv.items[i] } else { cv.items_b[i - cv.items.len()] };
            let mask = &it.node(&plan).mask;
            if mask.is_some() {
                self.mask(mask);
            }
        }
        let n = self.canvases[c].items.len();
        let nb = self.canvases[c].items_b.len();
        let Renderer {
            gpu,
            layouts,
            pipes,
            arena,
            binds,
            canvases,
            sources,
            patches,
            freeze_photos,
            luts,
            lut_views,
            masks,
            mask_views,
            assets,
            whens,
            res,
            fx,
            fx_node,
            trigger_levels,
            globals_canvas,
            globals_output,
            transitions,
            styles,
            fade_layout,
            fade_buf,
            glide_layout,
            glide_buf,
            fused,
            fx_counts,
            ..
        } = self;
        let device = &gpu.device;
        let cv = &mut canvases[c];
        let size = cv.size;
        let cx = FxContext { plan: &plan, snap, res, trigger_levels, flash_scale };
        let mut g = Gfx { device, layouts, pipes, arena, binds };

        // custom enter/exit styles that cannot run (not compiled yet, no texture) fade instead
        for it in cv.items.iter_mut().chain(cv.items_b.iter_mut()) {
            if let Some((st, p)) = it.style
                && (!style_ready(&plan, patches, styles, st) || sources[it.source as usize].tex(layout).is_none())
            {
                it.opacity *= p;
                it.style = None;
            }
        }

        // node effects and custom styles: pre-render each such node into a pool texture
        cv.slots.clear();
        cv.slots.resize(n, -1);
        cv.slots_b.clear();
        cv.slots_b.resize(nb, -1);
        let mut used_pool = 0usize;
        for pass_b in [false, true] {
            let count = if pass_b { nb } else { n };
            for i in 0..count {
                let it = if pass_b { cv.items_b[i] } else { cv.items[i] };
                let node = it.node(&plan);
                if node.fx.is_empty() && it.style.is_none() {
                    continue;
                }
                fx_node.clear();
                if !node.fx.is_empty() {
                    compose::eval_attaches(&cx, whens, &mut LutTable { map: luts, assets }, &node.fx, fx_node);
                }
                if fx_node.is_empty() && it.style.is_none() {
                    continue;
                }
                let source = &sources[it.source as usize];
                let texture = source.tex(layout);
                let (stex, premult) = texture.map_or((None, true), |(t, p)| (Some(t), p));
                let color = match source.rt { SourceRt::Solid(c) => c, _ => plan.settings.no_signal };
                while cv.pool.len() < used_pool + 2 {
                    let _p = se_alloc::Pause::new();
                    cv.pool.push(Tex::target(device, &format!("{} node fx", plan.canvases[c].name), size));
                }
                // unrotated copy of the node into the pool texture
                let region = rect_region(it.rect, size);
                {
                    let _p = se_alloc::Pause::new();
                    let mut pass = compose::begin(enc, "node fx source", &cv.pool[used_pool].view, Some([0.0; 4]));
                    let (content, uv) = source_placement(&plan, &it, stex, size);
                    let u = NodeUniform {
                        dst: it.rect,
                        uv,
                        content,
                        target_size: [size[0] as f32, size[1] as f32],
                        opacity: 1.0,
                        premultiplied: f32::from(u8::from(premult)),
                        color,
                        solid: f32::from(u8::from(stex.is_none())),
                        ..Default::default()
                    };
                    let pipes = g.pipes;
                    compose::draw_node(&mut pass, &mut g, &u, stex, None, &pipes.composite_copy);
                }
                let scissor = scissor_of(region, size);
                let mut cur = used_pool;
                let spare_idx = used_pool + 1;
                let mut spare = spare_idx;
                let chain = Chain { evals: fx_node, fused, region, scissor, quarter: None, ctx: c };
                for step in Steps::new(&plan, fx_node, fused) {
                    let (src_t, dst_t) = (&cv.pool[cur], &cv.pool[spare]);
                    apply_step(enc, &mut g, &plan, patches, freeze_photos, &chain, step, src_t, dst_t, snap, res, fi, lut_views, fx_counts);
                    std::mem::swap(&mut cur, &mut spare);
                }
                if let Some((st, presence)) = it.style {
                    style_pass(enc, &mut g, &plan, patches, styles, st, presence, region, &cv.pool[cur], &cv.pool[spare], snap, res, fi);
                    std::mem::swap(&mut cur, &mut spare);
                }
                if cur != used_pool {
                    cv.pool.swap(used_pool, cur);
                }
                if pass_b {
                    cv.slots_b[i] = used_pool as i32;
                } else {
                    cv.slots[i] = used_pool as i32;
                }
                used_pool += 1;
            }
        }

        // Compositor groups reuse the node arena, with a transparent isolated parent surface.
        // Even bypassed groups remain atomic layers: bypass only gates their FX chain.
        for pass_b in [false, true] {
            let (items, slots) = if pass_b { (&cv.items_b, &mut cv.slots_b) } else { (&cv.items, &mut cv.slots) };
            let mut i = 0;
            while i < items.len() {
                let Some((scene, gi)) = items[i].group else { i += 1; continue };
                let start = i;
                while i < items.len() && items[i].group == Some((scene, gi)) { i += 1; }
                let group = &plan.scenes[scene as usize].layouts[layout].groups[gi as usize];
                while cv.pool.len() < used_pool + 2 {
                    let _p = se_alloc::Pause::new();
                    cv.pool.push(Tex::target(device, &format!("{} group fx", plan.canvases[c].name), size));
                }
                {
                    let _p = se_alloc::Pause::new();
                    let mut pass = compose::begin(enc, "group", &cv.pool[used_pool].view, Some([0.0; 4]));
                    for k in start..i {
                        draw_item(&mut pass, &mut g, &plan, &items[k], slots[k], &cv.pool, sources, layout, size, masks, mask_views);
                    }
                }
                fx_node.clear();
                compose::eval_attaches(&cx, whens, &mut LutTable { map: luts, assets }, &group.fx, fx_node);
                let (mut gc, mut gs) = (used_pool, used_pool + 1);
                run_chain(enc, &mut g, &plan, patches, freeze_photos, fx_node, fused, &cv.pool, &cv.quarter, &mut gc, &mut gs, c, snap, res, fi, lut_views, fx_counts);
                if gc != used_pool { cv.pool.swap(used_pool, gc); }
                slots[start..i].fill(used_pool as i32);
                used_pool += 1;
            }
        }

        // scene pass(es)
        let bg_color = |scene: Option<usize>| scene.and_then(|s| plan.scenes[s].layouts[layout].background).unwrap_or([0.0, 0.0, 0.0, 1.0]);
        let shader_tr = tr.active && matches!(plan.transitions[tr.transition].kind, TrKind::Shader);
        let combined_tr = tr.active && matches!(plan.transitions[tr.transition].kind, TrKind::Combined);
        let glide_tr = tr.active && matches!(plan.transitions[tr.transition].kind, TrKind::Glide);
        // Scene effects may modify background RGB/alpha. Render backgrounds through those FX
        // and blend complete sides, rather than adding an unprocessed background afterwards.
        let full_scene_glide = glide_tr && [tr.from, program].into_iter().flatten().any(|s| !plan.scenes[s].layouts[layout].fx.is_empty());
        let draw_items = |g: &mut Gfx,
                          enc: &mut wgpu::CommandEncoder,
                          items: &[Item],
                          slots: &[i32],
                          pool: &[Tex],
                          target: &Tex,
                          clear: [f32; 4],
                          masks: &HashMap<String, Slot>,
                          mask_views: &[wgpu::TextureView]| {
            let _p = se_alloc::Pause::new();
            let mut pass = compose::begin(enc, "scene", &target.view, Some(clear));
            for (i, it) in items.iter().enumerate() {
                if let Some((scene, gi)) = it.group {
                    if i > 0 && items[i - 1].group == it.group { continue; }
                    let group = &plan.scenes[scene as usize].layouts[layout].groups[gi as usize];
                    let u = NodeUniform {
                        dst: [0.0, 0.0, size[0] as f32, size[1] as f32],
                        uv: [0.0, 0.0, 1.0, 1.0],
                        target_size: [size[0] as f32, size[1] as f32],
                        opacity: group.opacity.clamp(0.0, 1.0),
                        premultiplied: 1.0,
                        ..Default::default()
                    };
                    let pipeline = compose::blend_pipeline(g.pipes, group.blend);
                    compose::draw_node(&mut pass, g, &u, Some(&pool[slots[i] as usize]), None, pipeline);
                } else {
                    draw_item(&mut pass, g, &plan, it, slots[i], pool, sources, layout, size, masks, mask_views);
                }
            }
        };
        // program scene → t[0] (a glide's incoming side on transparent: `glide_pass` adds its
        // background)
        let program_clear = if glide_tr && !full_scene_glide { [0.0; 4] } else { bg_color(program) };
        draw_items(&mut g, enc, &cv.items, &cv.slots, &cv.pool, &cv.t[0], program_clear, masks, mask_views);
        let (mut cur, mut spare) = (0usize, 3usize);
        if let Some(s) = program {
            fx.clear();
            compose::eval_attaches(&cx, whens, &mut LutTable { map: luts, assets }, &plan.scenes[s].layouts[layout].fx, fx);
            run_chain(enc, &mut g, &plan, patches, freeze_photos, fx, fused, &cv.t, &cv.quarter, &mut cur, &mut spare, c, snap, res, fi, lut_views, fx_counts);
        }
        if shader_tr || combined_tr || glide_tr {
            let (a_idx, b_idx) = if shader_tr || glide_tr {
                // outgoing scene → t[1] (+ its scene effects, ping-pong with t[2])
                draw_items(&mut g, enc, &cv.items_b, &cv.slots_b, &cv.pool, &cv.t[1], bg_color(tr.from), masks, mask_views);
                let (mut fcur, mut fspare) = (1usize, 2usize);
                if let Some(f) = tr.from {
                    fx.clear();
                    compose::eval_attaches(&cx, whens, &mut LutTable { map: luts, assets }, &plan.scenes[f].layouts[layout].fx, fx);
                    run_chain(enc, &mut g, &plan, patches, freeze_photos, fx, fused, &cv.t, &cv.quarter, &mut fcur, &mut fspare, c, snap, res, fi, lut_views, fx_counts);
                }
                (fcur, cur)
            } else {
                (cur, cur)
            };
            let out = (0..4).find(|i| *i != a_idx && *i != b_idx).expect("4 targets");
            let tp = &plan.transitions[tr.transition];
            // shaders follow the transition's `ease` too (an overshooting ease stops at B)
            let t = tr.eased(&plan).clamp(0.0, 1.0);
            if glide_tr {
                glide_pass(enc, &mut g, glide_layout, glide_buf, &cv.t[a_idx], &cv.t[b_idx], &cv.t[out], tr.progress, scene::glide_fade(tp, tr.progress), full_scene_glide, bg_color(program), fi);
            } else {
                transition_pass(enc, &mut g, &plan, tp, transitions, patches, fade_layout, fade_buf, &cv.t[a_idx], &cv.t[b_idx], &cv.t[out], t, snap, res, fi);
            }
            cur = out;
            spare = (0..4).find(|i| *i != cur).expect("4 targets");
        }
        // overlays opting into effects, then canvas effects
        draw_overlays(enc, &mut g, &plan, c, sources, &cv.t[cur], true, &cx, whens);
        fx.clear();
        compose::eval_attaches(&cx, whens, &mut LutTable { map: luts, assets }, &plan.canvases[c].fx, fx);
        let (gate, default) = plan.canvases[c].fx_gate;
        if res.bool(snap, gate, default) {
            compose::eval_attaches(&cx, whens, &mut LutTable { map: luts, assets }, globals_canvas, fx);
        }
        run_chain(enc, &mut g, &plan, patches, freeze_photos, fx, fused, &cv.t, &cv.quarter, &mut cur, &mut spare, c, snap, res, fi, lut_views, fx_counts);
        draw_overlays(enc, &mut g, &plan, c, sources, &cv.t[cur], false, &cx, whens);
        fx.clear();
        compose::eval_attaches(&cx, whens, &mut LutTable { map: luts, assets }, &plan.canvases[c].output_fx, fx);
        let (gate, default) = plan.canvases[c].output_fx_gate;
        if res.bool(snap, gate, default) {
            compose::eval_attaches(&cx, whens, &mut LutTable { map: luts, assets }, globals_output, fx);
        }
        run_chain(enc, &mut g, &plan, patches, freeze_photos, fx, fused, &cv.t, &cv.quarter, &mut cur, &mut spare, c, snap, res, fi, lut_views, fx_counts);

        // final: flash limiter (program canvases) or plain copy (preview)
        if c == PREVIEW {
            compose::blit(enc, &mut g, BlitUniform { mode: BLIT_OPAQUE, ..Default::default() }, &cv.t[cur], None, &cv.fin[0].view);
            cv.cur_fin = 0;
        } else {
            let prev = cv.cur_fin;
            let next = prev ^ 1;
            // the very first frame has no history: pass through
            let k = if cv.rendered == 0 { 1.0 } else { max_step };
            compose::blit(enc, &mut g, BlitUniform { mode: BLIT_LIMIT, k, pad: [0.0; 2] }, &cv.t[cur], Some(&cv.fin[prev]), &cv.fin[next].view);
            cv.cur_fin = next;
        }
    }

    fn render_atlas(&mut self, enc: &mut wgpu::CommandEncoder) {
        let plan = self.plan.clone();
        if self.canvases[ATLAS].ensure(&self.gpu.device, "atlas", 1) {
            self.binds.clear();
        }
        let Renderer { gpu, layouts, pipes, arena, binds, canvases, sources, atlas_tiles, atlas_sources, .. } = self;
        let mut g = Gfx { device: &gpu.device, layouts, pipes, arena, binds };
        let cv = &mut canvases[ATLAS];
        let size = [cv.size[0] as f32, cv.size[1] as f32];
        let _p = se_alloc::Pause::new();
        let mut pass = compose::begin(enc, "atlas", &cv.fin[0].view, Some([0.0, 0.0, 0.0, 1.0]));
        for (tile, si) in atlas_tiles.iter().zip(atlas_sources.iter()) {
            let s = &sources[*si as usize];
            let cell = [tile[0] * size[0], tile[1] * size[1], tile[2] * size[0], tile[3] * size[1]];
            let (tex, premult) = match s.tex(WIDE) {
                Some((t, p)) => (Some(t), p),
                None => (None, true),
            };
            // letterbox the source's aspect into the 16:9 tile
            let dst = match tex {
                Some(t) => fit(cell, t.size[0] as f32 / t.size[1].max(1) as f32),
                None => cell,
            };
            let color = match &s.rt {
                SourceRt::Solid(c) => *c,
                _ => plan.settings.no_signal,
            };
            let u = NodeUniform {
                dst,
                uv: [0.0, 0.0, 1.0, 1.0],
                color,
                target_size: size,
                opacity: 1.0,
                premultiplied: f32::from(u8::from(premult)),
                solid: f32::from(u8::from(tex.is_none())),
                ..Default::default()
            };
            let pipes = g.pipes;
            compose::draw_node(&mut pass, &mut g, &u, tex, None, &pipes.composite[0]);
        }
        drop(pass);
        cv.cur_fin = 0;
    }

    /// Flash statistics, dmabuf and shm export of canvas `c`'s final texture.
    fn export_canvas(&mut self, enc: &mut wgpu::CommandEncoder, c: usize, now: u64) {
        let plan = self.plan.clone();
        let flash_on = plan.settings.flash_limit && (c == WIDE || c == TALL);
        let Renderer { gpu, layouts, pipes, arena, binds, canvases, frames, fence, barrier_images, presents, frame, stats, .. } = self;
        let device = &gpu.device;
        let cv = &mut canvases[c];
        cv.rendered += 1;
        cv.seq = stats.canvas_seq[c].fetch_add(1, Ordering::Relaxed) + 1;
        let fin = &cv.fin[cv.cur_fin];
        if flash_on {
            if cv.flash.is_none() {
                let _p = se_alloc::Pause::new();
                cv.flash = Some(Flash {
                    cells: device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("flash cells"),
                        size: (CELLS * 4) as u64,
                        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                        mapped_at_creation: false,
                    }),
                    readback: MappedRing::new(device, "flash readback", 4, (CELLS * 4) as u64),
                    detector: FlashDetector::new(plan.settings.max_flashes),
                    times: [0; 4],
                });
            }
            let f = cv.flash.as_mut().expect("created");
            f.detector.max_flashes = plan.settings.max_flashes;
            if let Some(i) = f.readback.acquire_read(*frame) {
                f.times[i] = now;
                let cells = &f.cells;
                let bg = binds.get_or((KIND_FLASH, fin.id, 0, 0), || {
                    let _p = se_alloc::Pause::new();
                    device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("flash"),
                        layout: &layouts.flash,
                        entries: &[
                            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&fin.view) },
                            wgpu::BindGroupEntry { binding: 1, resource: cells.as_entire_binding() },
                        ],
                    })
                });
                let _p = se_alloc::Pause::new();
                {
                    let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("flash"), timestamp_writes: None });
                    pass.set_pipeline(&pipes.flash);
                    pass.set_bind_group(0, bg, &[]);
                    pass.dispatch_workgroups(8, 8, 1);
                }
                enc.copy_buffer_to_buffer(&f.cells, 0, &f.readback.buffers[i], 0, (CELLS * 4) as u64);
            }
        }
        let Some(frames) = frames.as_ref() else { return };
        let demand = frames.demand(c as u32);
        let mut g = Gfx { device, layouts, pipes, arena, binds };
        if demand.dmabuf && fence.is_some() && !cv.dmabuf_failed {
            if cv.dmabuf.is_none() {
                let _p = se_alloc::Pause::new();
                match DmabufRing::new(gpu, plan.canvases[c].name, cv.size[0], cv.size[1], plan.settings.buffers, plan.settings.export_modifier) {
                    Ok(ring) => {
                        let desc = ring.dup_fds().map(|fds| RingDesc {
                            width: ring.width,
                            height: ring.height,
                            drm_fourcc: DRM_FORMAT_ABGR8888,
                            modifier: ring.modifier,
                            offsets: [ring.images[0].offset, 0, 0, 0],
                            strides: [ring.images[0].stride, 0, 0, 0],
                            planes: 1,
                            fds,
                        });
                        match desc.map_err(anyhow::Error::from).and_then(|d| frames.set_ring(c as u32, RingKind::Dmabuf, Some(d)).map_err(anyhow::Error::from))
                        {
                            Ok(()) => {
                                tracing::info!(target: "render", "canvas {}: dmabuf ring {}x{} modifier {:#x} stride {}", plan.canvases[c].name, ring.width, ring.height, ring.modifier, ring.images[0].stride);
                                cv.dmabuf = Some(ring);
                            }
                            Err(e) => {
                                tracing::error!(target: "render", "canvas {}: announcing dmabuf ring failed: {e:#}", plan.canvases[c].name);
                                cv.dmabuf_failed = true;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(target: "render", "canvas {}: dmabuf export unavailable ({e:#}); clients should use the shm fallback", plan.canvases[c].name);
                        stats.export_error.store(true, Ordering::Relaxed);
                        cv.dmabuf_failed = true;
                    }
                }
            }
            if let Some(ring) = &cv.dmabuf
                && barrier_images.len() < MAX_BARRIERS
                && let Some(b) = frames.acquire(c as u32, RingKind::Dmabuf)
            {
                {
                    let img = &ring.images[b as usize];
                    compose::blit(enc, &mut g, BlitUniform { mode: BLIT_OPAQUE, ..Default::default() }, fin, None, &img.view);
                    barrier_images.push(img.image);
                    presents.push((c, b, cv.seq));
                }
            }
        }
        if demand.shm {
            if cv.shm.is_none() {
                let _p = se_alloc::Pause::new();
                let row = (cv.size[0] * 4).div_ceil(256) * 256;
                let bufs: std::io::Result<Vec<ShmBuffer>> = (0..3).map(|_| ShmBuffer::new(cv.size[0] * 4, cv.size[1])).collect();
                match bufs.and_then(|b| b.iter().map(|x| x.try_clone_fd()).collect::<std::io::Result<Vec<OwnedFd>>>().map(|fds| (b, fds))) {
                    Ok((bufs, fds)) => match frames.set_ring(c as u32, RingKind::Shm, Some(RingDesc::shm(cv.size[0], cv.size[1], cv.size[0] * 4, fds))) {
                        Ok(()) => {
                            cv.shm = Some(Shm {
                                bufs,
                                readback: MappedRing::new(device, "shm readback", 3, row as u64 * cv.size[1] as u64),
                                row,
                                pending: Vec::with_capacity(SHM_PENDING),
                            });
                        }
                        Err(e) => tracing::error!(target: "render", "canvas {}: shm ring: {e}", plan.canvases[c].name),
                    },
                    Err(e) => tracing::error!(target: "render", "canvas {}: shm buffers: {e}", plan.canvases[c].name),
                }
            }
            if let Some(shm) = &mut cv.shm
                && shm.pending.len() < SHM_PENDING
                && let Some(i) = shm.readback.acquire_read(cv.seq)
            {
                let _p = se_alloc::Pause::new();
                enc.copy_texture_to_buffer(
                    fin.texture.as_image_copy(),
                    wgpu::TexelCopyBufferInfo {
                        buffer: &shm.readback.buffers[i],
                        layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(shm.row), rows_per_image: Some(cv.size[1]) },
                    },
                    wgpu::Extent3d { width: cv.size[0], height: cv.size[1], depth_or_array_layers: 1 },
                );
                shm.pending.push((i, cv.seq, now));
            }
        }
    }

    /// Copy mapped shm readbacks into the memfds and present them (debug fallback path).
    fn deliver_shm(&mut self, _now: u64) {
        let _p = se_alloc::Pause::new();
        let Some(frames) = &self.frames else { return };
        for (c, cv) in self.canvases.iter_mut().enumerate() {
            let Some(shm) = &mut cv.shm else { continue };
            let mut k = 0;
            while k < shm.pending.len() {
                let (i, seq, ts) = shm.pending[k];
                if !shm.readback.is_ready(i) {
                    k += 1;
                    continue;
                }
                // Drain already submitted readbacks without publishing a disabled canvas.
                // Keep its rings intact: consumers may still hold leases across the toggle.
                if (c != TALL || self.vertical_enabled) && let Some(b) = frames.acquire(c as u32, RingKind::Shm) {
                    let (w, h) = (cv.size[0] as usize * 4, cv.size[1] as usize);
                    let row = shm.row as usize;
                    if let Ok(view) = shm.readback.buffers[i].slice(..).get_mapped_range() {
                        let dst = shm.bufs[b as usize].as_mut_slice();
                        for y in 0..h {
                            dst[y * w..y * w + w].copy_from_slice(&view[y * row..y * row + w]);
                        }
                    }
                    frames.present(c as u32, RingKind::Shm, b, seq, ts, None);
                }
                shm.readback.release_read(i);
                shm.pending.swap_remove(k);
            }
        }
    }

    fn submit(&mut self, enc: wgpu::CommandEncoder, now: u64) {
        let _p = se_alloc::Pause::new();
        let main = enc.finish();
        let export = !self.barrier_images.is_empty();
        let acquire = if export { ownership_barrier(&self.gpu, &self.barrier_images, true) } else { None };
        let release = if export { ownership_barrier(&self.gpu, &self.barrier_images, false) } else { None };
        let armed = export && self.fence.as_ref().is_some_and(|f| f.arm(&self.gpu.queue));
        let cbs = acquire.into_iter().chain(std::iter::once(main)).chain(release);
        self.gpu.queue.submit(cbs);
        let Some(frames) = &self.frames else { return };
        if !export {
            return;
        }
        let fence = if armed {
            match self.fence.as_ref().map(FenceExport::export) {
                Some(Ok(f)) => f,
                Some(Err(e)) => {
                    tracing::error!(target: "render", "sync_file export failed: {e:#}");
                    self.stats.export_error.store(true, Ordering::Relaxed);
                    None
                }
                None => None,
            }
        } else {
            None
        };
        if !armed {
            // no explicit sync available: never hand out a buffer that may still be written
            for &(c, b, _) in &self.presents {
                frames.abandon(c as u32, RingKind::Dmabuf, b);
            }
            return;
        }
        for &(c, b, seq) in &self.presents {
            let f = fence.as_ref().and_then(|f| f.as_fd().try_clone_to_owned().ok());
            frames.present(c as u32, RingKind::Dmabuf, b, seq, now, f);
        }
        self.stats.exporting.store(true, Ordering::Relaxed);
    }

    fn after_submit(&mut self) {
        let _p = se_alloc::Pause::new();
        self.freeze_photos.after_submit();
        for s in &mut self.sources {
            if let SourceRt::Video(v) = &mut s.rt {
                v.after_submit(&self.gpu.queue);
            }
        }
        for cv in &mut self.canvases {
            if let Some(f) = &mut cv.flash {
                f.readback.after_submit();
            }
            if let Some(s) = &mut cv.shm {
                s.readback.after_submit();
            }
        }
        if let Some(ts) = &mut self.ts {
            ts.after_submit();
        }
    }

    /// Wait for the GPU (teardown, tests).
    pub fn wait_idle(&self) {
        let _ = self.gpu.device.poll(wgpu::PollType::wait_indefinitely());
    }

    /// Final texture of a canvas (tests / readback).
    pub fn final_texture(&self, c: usize) -> Option<&wgpu::Texture> {
        let cv = &self.canvases[c];
        cv.fin.get(cv.cur_fin).map(|t| &t.texture)
    }

    /// Master-clock time that patch/effect `time` counts from (deterministic tests).
    pub fn set_time_origin(&mut self, t0: u64) {
        self.t0 = t0;
        self.last_now = 0;
    }

    pub fn frame_count(&self) -> u64 {
        self.frame
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_item(
    pass: &mut wgpu::RenderPass, g: &mut Gfx, plan: &Plan, it: &Item,
    slot: i32, pool: &[Tex], sources: &[Src], layout: usize, size: [u32; 2],
    masks: &HashMap<String, Slot>, mask_views: &[wgpu::TextureView],
) {
    let node = it.node(plan);
    let mask = node.mask.as_deref().and_then(|m| match masks.get(m) {
        Some(Slot::Ready(id) | Slot::Reloading(id)) => Some((*id, &mask_views[(*id - 1_000_001) as usize])),
        _ => None,
    });
    let (tex, uv, content, premult) = if slot >= 0 {
        (Some(&pool[slot as usize]), rect_region(it.rect, size), [0.0; 4], true)
    } else {
        match sources[it.source as usize].tex(layout) {
            Some((t, p)) => {
                let (content, uv) = source_placement(plan, it, Some(t), size);
                (Some(t), uv, content, p)
            }
            None => (None, it.crop, [0.0; 4], true),
        }
    };
    let color = match &sources[it.source as usize].rt {
        SourceRt::Solid(c) => *c,
        _ => plan.settings.no_signal,
    };
    if tex.is_none() && color[3] <= 0.0 { return; }
    let u = NodeUniform {
        dst: it.rect, uv, color, content,
        target_size: [size[0] as f32, size[1] as f32],
        radius: it.radius, opacity: it.opacity, rotation: it.rotation,
        premultiplied: f32::from(u8::from(premult)),
        use_mask: f32::from(u8::from(mask.is_some())),
        solid: f32::from(u8::from(tex.is_none())),
    };
    let pipeline = compose::blend_pipeline(g.pipes, it.blend);
    compose::draw_node(pass, g, &u, tex, mask, pipeline);
}

/// Fit the sampled source before either the direct draw or the node-FX input copy.
/// Pool outputs already contain window-sized, fitted content and must not be fitted again.
fn source_placement(plan: &Plan, it: &Item, tex: Option<&Tex>, size: [u32; 2]) -> ([f32; 4], [f32; 4]) {
    let fit = it.node(plan).fit;
    if fit == NodeFit::Stretch {
        return ([0.0; 4], it.crop);
    }
    let Some(tex) = tex else { return ([0.0; 4], it.crop) };
    let canvas_scale = size[0] as f32 / plan.canvases[it.layout as usize].width.max(1) as f32;
    fit.placement(it.rect, it.crop, [tex.size[0] as f32, tex.size[1] as f32], canvas_scale)
}

/// Largest box of `aspect` centered in `cell` (px).
fn fit(cell: [f32; 4], aspect: f32) -> [f32; 4] {
    let (w, h) = if cell[2] / cell[3] > aspect { (cell[3] * aspect, cell[3]) } else { (cell[2], cell[2] / aspect) };
    [cell[0] + (cell[2] - w) * 0.5, cell[1] + (cell[3] - h) * 0.5, w, h]
}

/// Canvas px rect → uv region, clamped to the canvas.
fn rect_region(r: [f32; 4], size: [u32; 2]) -> [f32; 4] {
    let (w, h) = (size[0] as f32, size[1] as f32);
    [(r[0] / w).clamp(0.0, 1.0), (r[1] / h).clamp(0.0, 1.0), ((r[0] + r[2]) / w).clamp(0.0, 1.0), ((r[1] + r[3]) / h).clamp(0.0, 1.0)]
}

fn scissor_of(region: [f32; 4], size: [u32; 2]) -> Option<[u32; 4]> {
    let x0 = (region[0] * size[0] as f32).floor() as u32;
    let y0 = (region[1] * size[1] as f32).floor() as u32;
    let x1 = ((region[2] * size[0] as f32).ceil() as u32).min(size[0]);
    let y1 = ((region[3] * size[1] as f32).ceil() as u32).min(size[1]);
    (x1 > x0 && y1 > y0).then_some([x0, y0, x1 - x0, y1 - y0])
}

/// Static param values of a transition in header layout order.
fn transition_values(plan: &Plan, name: &str, layout: &se_patch::wgsl::Layout) -> Vec<[f32; 4]> {
    let Some(tp) = plan.transition_index.get(name).map(|i| &plan.transitions[*i]) else { return vec![[0.0; 4]; layout.params.len()] };
    layout.params.iter().map(|(n, _, _)| tp.params.iter().find(|(k, _)| k == n).map_or([0.0; 4], |(_, spec)| crate::plan::param_default4(spec))).collect()
}

/// Effect passes of the frame so far, and how many effects ran inside fused passes.
#[derive(Clone, Copy, Debug, Default)]
struct FxCounts {
    passes: u32,
    fused: u32,
}

/// An evaluated effect chain and where it runs.
struct Chain<'a> {
    evals: &'a [FxEval],
    fused: &'a [FusedPipe],
    /// uv region the effects apply to (node effects: the node rect).
    region: [f32; 4],
    scissor: Option<[u32; 4]>,
    /// Quarter-resolution targets for blur (canvas chains).
    quarter: Option<&'a [Tex]>,
    /// Render context of the chain's frame-state effects ([`crate::patches::CONTEXTS`]).
    ctx: usize,
}

/// Run an effect chain on canvas targets of canvas `ctx`, ping-ponging between `cur` and `spare`.
#[allow(clippy::too_many_arguments)]
fn run_chain(
    enc: &mut wgpu::CommandEncoder,
    g: &mut Gfx,
    plan: &Plan,
    patches: &mut [PatchRt],
    freeze_photos: &mut FreezePhotos,
    evals: &[FxEval],
    fused: &[FusedPipe],
    t: &[Tex],
    quarter: &[Tex],
    cur: &mut usize,
    spare: &mut usize,
    ctx: usize,
    snap: &Snapshot,
    res: &Resolved,
    fi: &FrameInputs,
    lut_views: &[wgpu::TextureView],
    counts: &mut FxCounts,
) {
    let chain = Chain { evals, fused, region: [0.0, 0.0, 1.0, 1.0], scissor: None, quarter: Some(quarter), ctx };
    for step in Steps::new(plan, evals, fused) {
        apply_step(enc, g, plan, patches, freeze_photos, &chain, step, &t[*cur], &t[*spare], snap, res, fi, lut_views, counts);
        std::mem::swap(cur, spare);
    }
}

/// One pass of a chain `input → out`: a single effect, or a run of pointwise effects through
/// their fused pipeline (stages of inactive effects stay at strength 0 and are skipped).
#[allow(clippy::too_many_arguments)]
fn apply_step(
    enc: &mut wgpu::CommandEncoder,
    g: &mut Gfx,
    plan: &Plan,
    patches: &mut [PatchRt],
    freeze_photos: &mut FreezePhotos,
    chain: &Chain,
    step: Step,
    input: &Tex,
    out: &Tex,
    snap: &Snapshot,
    res: &Resolved,
    fi: &FrameInputs,
    lut_views: &[wgpu::TextureView],
    counts: &mut FxCounts,
) {
    counts.passes += 1;
    match step {
        Step::Single(i) => {
            apply_effect(enc, g, plan, patches, freeze_photos, &chain.evals[i], input, out, chain.quarter, chain.region, chain.scissor, chain.ctx, snap, res, fi, lut_views);
        }
        Step::Fused { first, len, pipe, slots } => {
            counts.fused += len as u32;
            let mut u = FusedUniform {
                head: FxUniform {
                    resolution: [input.size[0] as f32, input.size[1] as f32],
                    time: fi.time,
                    region: chain.region,
                    beat_phase: res.signal(snap, plan.show.beat_phase),
                    bass: res.signal(snap, plan.show.bass),
                    ..Default::default()
                },
                ..Default::default()
            };
            for (e, slot) in chain.evals[first..first + len].iter().zip(slots) {
                u.stages[slot as usize] = FxStage { params: e.params, strength: e.strength, pad: [0.0; 3] };
            }
            compose::fx_pass(enc, g, &chain.fused[pipe].pipeline, &u, input, None, None, out, chain.scissor);
        }
    }
}

/// One effect `input → out` over `region` (uv), scissored for node effects; `ctx` is the render
/// context of frame-state effects.
#[allow(clippy::too_many_arguments)]
fn apply_effect(
    enc: &mut wgpu::CommandEncoder,
    g: &mut Gfx,
    plan: &Plan,
    patches: &mut [PatchRt],
    freeze_photos: &mut FreezePhotos,
    e: &FxEval,
    input: &Tex,
    out: &Tex,
    quarter: Option<&[Tex]>,
    region: [f32; 4],
    scissor: Option<[u32; 4]>,
    ctx: usize,
    snap: &Snapshot,
    res: &Resolved,
    fi: &FrameInputs,
    lut_views: &[wgpu::TextureView],
) {
    let size = [input.size[0] as f32, input.size[1] as f32];
    match &plan.effects[e.effect].kind {
        EffectKind::Builtin(i) => {
            let def = &LIBRARY[*i];
            let mut u = FxUniform {
                resolution: size,
                time: fi.time,
                strength: e.strength,
                region,
                params: e.params,
                beat_phase: res.signal(snap, plan.show.beat_phase),
                bass: res.signal(snap, plan.show.bass),
                seed: (e.effect as f32) * 7.31,
                pad: 0.0,
            };
            match def.exec {
                Exec::Simple | Exec::Pointwise => compose::fx_pass(enc, g, &g.pipes.effects[*i], &u, input, None, None, out, scissor),
                Exec::Lut => {
                    let lut = (e.lut > 0).then(|| (e.lut, &lut_views[e.lut as usize - 1]));
                    compose::fx_pass(enc, g, &g.pipes.effects[*i], &u, input, None, lut, out, scissor);
                }
                Exec::Blur => {
                    // quarter-resolution gaussian (canvas chains); node effects use their own
                    // full-size scratch when no quarter targets exist
                    let Some(q) = quarter.filter(|q| q.len() == 2) else {
                        compose::fx_pass(enc, g, &g.pipes.effects[*i], &u, input, None, None, out, scissor);
                        return;
                    };
                    compose::fx_pass(enc, g, &g.pipes.blur_down, &u, input, None, None, &q[0], None);
                    u.resolution = [q[0].size[0] as f32, q[0].size[1] as f32];
                    compose::fx_pass(enc, g, &g.pipes.blur_h, &u, &q[0], None, None, &q[1], None);
                    compose::fx_pass(enc, g, &g.pipes.blur_v, &u, &q[1], None, None, &q[0], None);
                    u.resolution = size;
                    compose::fx_pass(enc, g, &g.pipes.effects[*i], &u, input, Some(&q[0]), None, out, scissor);
                }
            }
        }
        EffectKind::Patch(id) => {
            let Some(pi) = plan.patch_index.get(id).copied() else { return };
            let rt = &mut patches[pi];
            let frames = rt.frame_state(g.device, enc, g.binds, e.frame_state, ctx, out.size, fi.frame);
            let slot = e.patch_slot.map(|index| &plan.patch_slots[index]);
            let off = rt.uniforms(&plan.patches[pi], snap, res, fi, size, region, 0.0, Some(e.strength), slot, frames, g.arena);
            if rt.draw(g.device, enc, g.layouts, g.arena, g.binds, &out.view, off, Some(input), None, 0, frames) {
                rt.record_frames(enc, frames, input, out, fi.frame);
                if id == "freeze_frame" {
                    if let Some(state) = rt.recorded_state(frames) {
                        freeze_photos.capture(g.device, enc, state, ctx);
                    }
                }
            } else {
                // not compiled (yet): pass the input through unchanged
                compose::blit(enc, g, BlitUniform { mode: BLIT_COPY, ..Default::default() }, input, None, &out.view);
            }
        }
    }
}

/// Fixed header fields of a transition-style pass (full target, env 1, no trigger).
fn transition_fixed(fi: &FrameInputs, size: [f32; 2], progress: f32) -> se_patch::wgsl::Fixed<'_> {
    se_patch::wgsl::Fixed {
        time: fi.time,
        dt: fi.dt,
        frame: fi.frame,
        env: 1.0,
        resolution: size,
        progress,
        trigger_count: 0,
        trigger_age: se_patch::wgsl::NEVER_TRIGGERED,
        region: se_patch::wgsl::FULL,
        trigger: &se_patch::wgsl::NO_TRIGGER,
        palette: &fi.palette,
    }
}

/// Shader transition `a` (outgoing) → `b` (incoming) at linear progress `progress`.
#[allow(clippy::too_many_arguments)]
fn transition_pass(
    enc: &mut wgpu::CommandEncoder,
    g: &mut Gfx,
    plan: &Plan,
    tp: &crate::plan::TransitionPlan,
    transitions: &mut HashMap<String, TransitionRt>,
    patches: &mut [PatchRt],
    fade_layout: &se_patch::wgsl::Layout,
    fade_buf: &mut [f32],
    a: &Tex,
    b: &Tex,
    out: &Tex,
    progress: f32,
    snap: &Snapshot,
    res: &Resolved,
    fi: &FrameInputs,
) {
    let size = [out.size[0] as f32, out.size[1] as f32];
    let sig = |i: usize, _: &str| plan.std_signals.get(i).map_or(0.0, |s| res.signal(snap, *s));
    if let Some(ShaderSource::Patch(id)) = &tp.shader
        && let Some(pi) = plan.patch_index.get(id).copied()
    {
        let rt = &mut patches[pi];
        let off = rt.uniforms(&plan.patches[pi], snap, res, fi, size, se_patch::wgsl::FULL, progress, Some(1.0), None, None, g.arena);
        if rt.draw(g.device, enc, g.layouts, g.arena, g.binds, &out.view, off, Some(a), Some(b), 0, None) {
            return;
        }
    }
    let compiled = match &tp.shader {
        Some(ShaderSource::File(_)) => true,
        Some(ShaderSource::Builtin(n)) => *n != "fade",
        _ => false,
    };
    let (pipeline, off) = match (compiled, transitions.get_mut(&tp.name)) {
        (true, Some(t)) => match t.pipes.as_ref() {
            UserPipes::Fullscreen(p) => {
                let values = &t.values;
                t.layout.write(&mut t.buf, &transition_fixed(fi, size, progress), |i, _| values.get(i).copied().unwrap_or([0.0; 4]), sig);
                (p, g.arena.push_f32(&t.buf))
            }
            UserPipes::Particles { .. } => return,
        },
        _ => {
            fade_layout.write(fade_buf, &transition_fixed(fi, size, progress), |_, _| [0.0; 4], sig);
            (&g.pipes.fade, g.arena.push_f32(fade_buf))
        }
    };
    header_pass(enc, g, "transition", pipeline, off, a, b, out);
}

/// Glide composite of `a` (outgoing scene on its background) and `b` (incoming scene on
/// transparent, premultiplied) at linear progress `u`, independent crossfade weight `fade`,
/// (`shaders/glide.wgsl`).
#[allow(clippy::too_many_arguments)]
fn glide_pass(enc: &mut wgpu::CommandEncoder, g: &mut Gfx, layout: &se_patch::wgsl::Layout, buf: &mut [f32], a: &Tex, b: &Tex, out: &Tex, u: f32, fade: f32, full_scene: bool, bg: [f32; 4], fi: &FrameInputs) {
    let size = [out.size[0] as f32, out.size[1] as f32];
    layout.write(buf, &transition_fixed(fi, size, u), |_, name| match name {
        "fade" => [fade, 0.0, 0.0, 0.0],
        "full_scene" => [u32::from(full_scene) as f32, 0.0, 0.0, 0.0],
        _ => bg,
    }, |_, _| 0.0);
    let off = g.arena.push_f32(buf);
    let pipes = g.pipes;
    header_pass(enc, g, "glide", &pipes.glide, off, a, b, out);
}

/// One full-screen pass of a shader compiled with the generated header (`a` → `se_input`,
/// `b` → `se_input_b`), uniform block at arena offset `off`.
#[allow(clippy::too_many_arguments)]
fn header_pass(enc: &mut wgpu::CommandEncoder, g: &mut Gfx, label: &'static str, pipeline: &wgpu::RenderPipeline, off: u32, a: &Tex, b: &Tex, out: &Tex) {
    let (device, layouts, arena) = (g.device, g.layouts, &*g.arena);
    let bg = g.binds.get_or((KIND_PATCH, a.id, b.id, 0), || {
        let _p = se_alloc::Pause::new();
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &layouts.patch,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: arena.binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&layouts.linear) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&a.view) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&b.view) },
                wgpu::BindGroupEntry { binding: 4, resource: layouts.dummy_storage.as_entire_binding() },
            ],
        })
    });
    let _p = se_alloc::Pause::new();
    let mut pass = compose::begin(enc, label, &out.view, Some([0.0; 4]));
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bg, &[off]);
    pass.draw(0..3, 0..1);
}

/// Whether custom style `style` can run this frame (its shader is compiled; `feedback = "state"`
/// effects need a frame-state instance, which styles do not have).
fn style_ready(plan: &Plan, patches: &[PatchRt], styles: &HashMap<String, TransitionRt>, style: u32) -> bool {
    match plan.styles.get(style as usize) {
        Some(ShaderSource::Patch(id)) => plan.patch_index.get(id).is_some_and(|pi| {
            matches!(patches[*pi].pipes.as_deref(), Some(UserPipes::Fullscreen(_))) && patches[*pi].layout.feedback != se_patch::Feedback::State
        }),
        Some(ShaderSource::File(p)) => p.to_str().is_some_and(|k| styles.contains_key(k)),
        _ => false,
    }
}

/// Draw a node (`input`, the node alone at its place) through its custom enter/exit style at
/// `presence` (0 = gone, 1 = in place) into `out`. `region`: the node's uv rect.
#[allow(clippy::too_many_arguments)]
fn style_pass(
    enc: &mut wgpu::CommandEncoder,
    g: &mut Gfx,
    plan: &Plan,
    patches: &mut [PatchRt],
    styles: &mut HashMap<String, TransitionRt>,
    style: u32,
    presence: f32,
    region: [f32; 4],
    input: &Tex,
    out: &Tex,
    snap: &Snapshot,
    res: &Resolved,
    fi: &FrameInputs,
) {
    let size = [out.size[0] as f32, out.size[1] as f32];
    match plan.styles.get(style as usize) {
        Some(ShaderSource::Patch(id)) => {
            let Some(pi) = plan.patch_index.get(id).copied() else { return };
            let rt = &mut patches[pi];
            let off = rt.uniforms(&plan.patches[pi], snap, res, fi, size, region, presence, Some(1.0), None, None, g.arena);
            rt.draw(g.device, enc, g.layouts, g.arena, g.binds, &out.view, off, Some(input), None, 0, None);
        }
        Some(ShaderSource::File(p)) => {
            let Some(s) = p.to_str().and_then(|k| styles.get_mut(k)) else { return };
            let UserPipes::Fullscreen(pipeline) = s.pipes.as_ref() else { return };
            let sig = |i: usize, _: &str| plan.std_signals.get(i).map_or(0.0, |s| res.signal(snap, *s));
            let fixed = se_patch::wgsl::Fixed { region, ..transition_fixed(fi, size, presence) };
            s.layout.write(&mut s.buf, &fixed, |_, _| [0.0; 4], sig);
            let off = g.arena.push_f32(&s.buf);
            header_pass(enc, g, "style", pipeline, off, input, input, out);
        }
        _ => {}
    }
}

/// Composite the overlay layers of canvas `c` (those with `fx == with_fx`) onto `target`.
#[allow(clippy::too_many_arguments)]
fn draw_overlays(
    enc: &mut wgpu::CommandEncoder,
    g: &mut Gfx,
    plan: &Plan,
    c: usize,
    sources: &[Src],
    target: &Tex,
    with_fx: bool,
    cx: &FxContext,
    whens: &mut Whens,
) {
    let layout = plan.canvases[c].layout;
    let size = [target.size[0] as f32, target.size[1] as f32];
    let mut visible = [0u16; 64];
    let mut n = 0;
    for (i, o) in plan.overlays.iter().enumerate().take(64) {
        if o.fx != with_fx || !o.canvases[c] || sources[o.source as usize].tex(layout).is_none() {
            continue;
        }
        if let Some(env) = o.env
            && cx.res.f32(cx.snap, env, 0.0) <= 0.0
        {
            continue;
        }
        if let Some(w) = o.when
            && !whens.eval(w as usize, &plan.whens[w as usize], cx.snap, cx.res)
        {
            continue;
        }
        visible[n] = i as u16;
        n += 1;
    }
    if n == 0 {
        return;
    }
    let pipes = g.pipes;
    let _p = se_alloc::Pause::new();
    let mut pass = compose::begin(enc, "overlays", &target.view, None);
    for &i in &visible[..n] {
        let o = &plan.overlays[i as usize];
        let Some((tex, premult)) = sources[o.source as usize].tex(layout) else { continue };
        let dst = match o.rect[c] {
            Some(r) => [r[0] * size[0], r[1] * size[1], r[2] * size[0], r[3] * size[1]],
            None => fit([0.0, 0.0, size[0], size[1]], tex.size[0] as f32 / tex.size[1].max(1) as f32),
        };
        let u =
            NodeUniform { dst, uv: [0.0, 0.0, 1.0, 1.0], target_size: size, opacity: 1.0, premultiplied: f32::from(u8::from(premult)), ..Default::default() };
        compose::draw_node(&mut pass, g, &u, Some(tex), None, &pipes.composite[0]);
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // resources referenced by in-flight work (and the exported semaphore) must outlive it
        if !self.gpu.is_lost() {
            self.wait_idle();
        }
        // the raw semaphore goes before any field could release the last device reference
        self.fence.take();
    }
}
