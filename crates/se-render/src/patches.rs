//! Runtime of shader and particles patches (§6.1, §6.3): uniform blocks filled through the
//! generated-header layout (time, env, trigger payload, params, signals, palette, resolution,
//! region), the particle
//! storage buffer, per-instance frame state of `feedback`/`history` effects, and the passes
//! that render a patch layer. Pipelines arrive compiled from the loader thread; a failed
//! recompile keeps the previous pipeline (last good version).

use crate::addr::Resolved;
use crate::pipelines::UserPipes;
use crate::plan::PatchPlan;
use crate::resources::{Arena, BindCache, Layouts, Tex};
use se_core::triggers::PAYLOAD_FLOATS;
use se_patch::Feedback;
use se_hub::Snapshot;
use std::sync::Arc;

pub const KIND_PATCH: u8 = 2;
/// Bind groups of `feedback`/`history` effects with their frame-state textures.
pub const KIND_PATCH_FRAMES: u8 = 3;

/// Render contexts one frame-state instance can run in: the canvases (`0..CANVASES`), then
/// source chains (`CANVASES + layout`; video sources use `CANVASES`). The same attachment runs
/// once per context per frame, and each context keeps its own textures.
pub const CONTEXTS: usize = crate::plan::CANVASES + crate::plan::LAYOUTS;

/// Textures of one frame-state instance in one render context, sized like its pass target.
pub struct FrameState {
    /// `se_prev` buffers: `feedback = true` keeps one (a copy of the last output);
    /// `feedback = "state"` ping-pongs two (read `prev[read]`, write `@location(1)` to the other).
    prev: [Option<Tex>; 2],
    read: usize,
    feedback: Feedback,
    /// `se_history`: ring of the last `history` input frames (2D-array view).
    history: Option<Tex>,
    size: [u32; 2],
    /// Ring layers (`history`); with `size` and `feedback`, must match the current layout.
    layers: u32,
    /// Recorded input frames (≤ `history`) and the ring layer the next one goes to.
    len: u32,
    head: u32,
    /// `FrameInputs::frame` of the last pass; a gap means the instance was paused.
    last_frame: u32,
}

impl FrameState {
    fn new(device: &wgpu::Device, id: &str, size: [u32; 2], feedback: Feedback, history_layers: u32) -> FrameState {
        use wgpu::TextureUsages as U;
        let buffer = |i: usize| Tex::new(device, &format!("{id} se_prev {i}"), size, crate::resources::COLOR, U::TEXTURE_BINDING | U::COPY_DST | U::COPY_SRC | U::RENDER_ATTACHMENT);
        let prev = match feedback {
            Feedback::Off => [None, None],
            Feedback::Output => [Some(buffer(0)), None],
            Feedback::State => [Some(buffer(0)), Some(buffer(1))],
        };
        let history = (history_layers > 0).then(|| {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(&format!("{id} se_history")),
                size: wgpu::Extent3d { width: size[0].max(1), height: size[1].max(1), depth_or_array_layers: history_layers },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: crate::resources::COLOR,
                usage: U::TEXTURE_BINDING | U::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor { dimension: Some(wgpu::TextureViewDimension::D2Array), ..Default::default() });
            Tex::wrap(texture, view, size)
        });
        FrameState { prev, read: 0, feedback, history, size, layers: history_layers, len: 0, head: 0, last_frame: 0 }
    }

    fn fits(&self, size: [u32; 2], feedback: Feedback, history: u32) -> bool {
        self.size == size && self.feedback == feedback && self.layers == history
    }

    /// Bind group cache key: the texture bound as `se_prev` (alternates in state mode), else
    /// the history ring.
    fn key(&self) -> u32 {
        self.prev[self.read].as_ref().or(self.history.as_ref()).map_or(0, |t| t.id)
    }

    fn forget(&self, binds: &mut BindCache) {
        for t in self.prev.iter().flatten().chain(&self.history) {
            binds.forget(t.id);
        }
    }

    /// `feedback = "state"`: the buffer this pass writes (`@location(1)`).
    fn state_target(&self) -> Option<&Tex> {
        if self.feedback == Feedback::State { self.prev[self.read ^ 1].as_ref() } else { None }
    }

    fn bytes(&self) -> u64 {
        self.prev.iter().flatten().map(Tex::bytes).sum::<u64>() + self.history.as_ref().map_or(0, |t| t.bytes() * self.layers as u64)
    }
}

/// Per-frame inputs common to every patch.
#[derive(Clone, Copy, Debug)]
pub struct FrameInputs {
    pub time: f32,
    pub dt: f32,
    pub frame: u32,
    pub palette: [[f32; 4]; 8],
}

pub struct PatchRt {
    pub id: String,
    pub pipes: Option<Arc<UserPipes>>,
    pub layout: se_patch::wgsl::Layout,
    buf: Vec<f32>,
    pub particles: Option<wgpu::Buffer>,
    storage_id: u32,
    pub trigger_count: u32,
    /// Payload of the last trigger (`se.trigger`).
    pub trigger: [f32; PAYLOAD_FLOATS],
    /// Frame time (`FrameInputs::time`) of the first pass after the last trigger; `None` until a
    /// pass stamps it (then `se.trigger_age` counts from there).
    pub trigger_at: Option<f32>,
    last_sim_frame: u64,
    pub last_render_ns: [u64; 2],
    pub min_interval_ns: u64,
    /// Frame state of `feedback`/`history` effects, indexed `instance * CONTEXTS + context`
    /// (allocated on an instance's first pass in a context and on resize; dropped with the plan).
    frames: Vec<Option<FrameState>>,
}

static NEXT_STORAGE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1_000_000);

impl PatchRt {
    pub fn new(device: &wgpu::Device, plan: &PatchPlan) -> PatchRt {
        let m = &plan.manifest;
        let particles = m.particles.as_ref().filter(|_| m.kind == se_patch::Kind::Particles).map(|p| {
            let _p = se_alloc::Pause::new();
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(&format!("particles {}", m.id)),
                size: (p.count.max(1) as u64) * se_patch::wgsl::PARTICLE_BYTES as u64,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            })
        });
        PatchRt {
            id: m.id.clone(),
            pipes: None,
            layout: plan.layout.clone(),
            buf: vec![0.0; plan.layout.floats],
            particles,
            storage_id: NEXT_STORAGE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            trigger_count: 0,
            trigger: [0.0; PAYLOAD_FLOATS],
            trigger_at: None,
            last_sim_frame: u64::MAX,
            last_render_ns: [0; 2],
            min_interval_ns: m.fps.map_or(0, |f| 1_000_000_000 / f.max(1) as u64),
            frames: if plan.layout.frame_state() { (0..plan.instances as usize * CONTEXTS).map(|_| None).collect() } else { Vec::new() },
        }
    }

    /// Adopt a (re)compiled pipeline whose layout matches this plan.
    pub fn set_pipes(&mut self, layout: se_patch::wgsl::Layout, pipes: Arc<UserPipes>) {
        if layout != self.layout {
            if layout.frame_state() != self.layout.frame_state() {
                // the bind group layout changes: never reuse groups cached for the old one
                self.storage_id = NEXT_STORAGE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            self.buf = vec![0.0; layout.floats];
            self.layout = layout;
        }
        self.pipes = Some(pipes);
    }

    /// Bytes of the frame-state textures currently allocated.
    pub fn frame_bytes(&self) -> u64 {
        self.frames.iter().flatten().map(FrameState::bytes).sum()
    }

    /// Prepare frame-state instance `instance` for a pass in render context `ctx` whose target
    /// is `size`: allocated on first use and on resize (new textures are zeroed = transparent),
    /// restarted (cleared `se_prev`, empty history) when the instance did not run on the
    /// previous frame. Returns the slot for [`Self::uniforms`], [`Self::draw`] and
    /// [`Self::record_frames`]; `None` for patches without frame state.
    #[allow(clippy::too_many_arguments)]
    pub fn frame_state(
        &mut self,
        device: &wgpu::Device,
        enc: &mut wgpu::CommandEncoder,
        binds: &mut BindCache,
        instance: Option<u32>,
        ctx: usize,
        size: [u32; 2],
        frame: u32,
    ) -> Option<usize> {
        let slot = instance? as usize * CONTEXTS + ctx;
        let (feedback, history) = (self.layout.feedback, self.layout.history);
        let entry = self.frames.get_mut(slot)?;
        match entry {
            Some(s) if s.fits(size, feedback, history) => {
                if s.last_frame.wrapping_add(1) != frame {
                    s.len = 0;
                    s.head = 0;
                    if let Some(p) = &s.prev[s.read] {
                        let _p = se_alloc::Pause::new();
                        drop(crate::compose::begin(enc, "se_prev clear", &p.view, Some([0.0; 4])));
                    }
                }
            }
            _ => {
                let _p = se_alloc::Pause::new();
                if let Some(old) = entry.take() {
                    old.forget(binds);
                }
                *entry = Some(FrameState::new(device, &self.id, size, feedback, history));
            }
        }
        Some(slot)
    }

    /// After a pass `input → out` of frame-state slot `slot`: the output (`feedback = true`) or
    /// the state just written (`"state"`) becomes the next `se_prev`, and `input` goes into the
    /// `se_history` ring.
    pub fn record_frames(&mut self, enc: &mut wgpu::CommandEncoder, slot: Option<usize>, input: &Tex, out: &Tex, frame: u32) {
        let Some(s) = slot.and_then(|i| self.frames.get_mut(i)).and_then(Option::as_mut) else { return };
        let extent = wgpu::Extent3d { width: s.size[0].max(1), height: s.size[1].max(1), depth_or_array_layers: 1 };
        let _p = se_alloc::Pause::new();
        match s.feedback {
            Feedback::Output => {
                if let Some(p) = &s.prev[0] {
                    enc.copy_texture_to_texture(out.texture.as_image_copy(), p.texture.as_image_copy(), extent);
                }
            }
            Feedback::State => s.read ^= 1,
            Feedback::Off => {}
        }
        if let Some(h) = &s.history {
            let n = s.layers;
            let dst = wgpu::TexelCopyTextureInfo { texture: &h.texture, mip_level: 0, origin: wgpu::Origin3d { x: 0, y: 0, z: s.head }, aspect: wgpu::TextureAspect::All };
            enc.copy_texture_to_texture(input.texture.as_image_copy(), dst, extent);
            s.head = (s.head + 1) % n;
            s.len = (s.len + 1).min(n);
        }
        s.last_frame = frame;
    }

    /// Clean state just written by a state-feedback shader, after [`Self::record_frames`].
    /// Freeze-frame display decoration is deliberately not part of this texture.
    pub fn recorded_state(&self, slot: Option<usize>) -> Option<&Tex> {
        let state = slot.and_then(|i| self.frames.get(i)).and_then(Option::as_ref)?;
        (state.feedback == Feedback::State).then(|| state.prev[state.read].as_ref()).flatten()
    }

    pub fn particle_count(&self, plan: &PatchPlan) -> u32 {
        plan.manifest.particles.as_ref().map_or(0, |p| p.count)
    }

    /// Fill the uniform block and push it into the arena. `region`: uv rect of the node the pass
    /// runs for (full target otherwise). `frames`: frame-state slot ([`Self::frame_state`]).
    #[allow(clippy::too_many_arguments)]
    pub fn uniforms(
        &mut self,
        plan: &PatchPlan,
        snap: &Snapshot,
        res: &Resolved,
        fi: &FrameInputs,
        resolution: [f32; 2],
        region: [f32; 4],
        progress: f32,
        env_override: Option<f32>,
        slot: Option<&crate::plan::PatchSlotPlan>,
        frames: Option<usize>,
        arena: &mut Arena,
    ) -> u32 {
        let env = env_override.unwrap_or_else(|| if plan.manifest.has_trigger { res.f32(snap, plan.env, 0.0) } else { 1.0 });
        let params = &plan.params;
        let defaults = &plan.defaults;
        let types = &self.layout.params;
        let aliased;
        let palette = if plan.manifest.palette.is_empty() {
            &fi.palette
        } else {
            aliased = std::array::from_fn(|i| {
                plan.palette[i]
                    .and_then(|address| res.get(snap, address))
                    .and_then(se_proto::Value::as_color)
                    .filter(|color| color[3] > 0.0 && color.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)))
                    .unwrap_or(fi.palette[i])
            });
            &aliased
        };
        let fixed = se_patch::wgsl::Fixed {
            time: fi.time,
            dt: fi.dt,
            frame: fi.frame,
            env,
            resolution,
            progress,
            trigger_count: self.trigger_count,
            trigger_age: if self.trigger_count == 0 { se_patch::wgsl::NEVER_TRIGGERED } else { (fi.time - *self.trigger_at.get_or_insert(fi.time)).max(0.0) },
            region,
            trigger: &self.trigger,
            palette,
        };
        self.layout.write(
            &mut self.buf,
            &fixed,
            |i, name| {
                if let Some(local) = slot.and_then(|s| s.params.get(i)) {
                    let value = res.get(snap, local.local)
                        .or_else(|| local.global.and_then(|global| res.get(snap, global)))
                        .unwrap_or(&local.default);
                    if local.spec.value_type() == se_proto::ValueType::Enum {
                        let index = value.as_str().and_then(|name| local.spec.options.iter().position(|option| option == name)).unwrap_or(0);
                        return [index as f32, 0.0, 0.0, 0.0];
                    }
                    return crate::plan::value4(value);
                }
                let d = defaults.get(i).copied().unwrap_or([0.0; 4]);
                match (params.get(i), types.get(i).map(|t| t.1)) {
                    (Some(a), Some(se_proto::ValueType::Color | se_proto::ValueType::Vec4 | se_proto::ValueType::Vec2)) => res.vec4(snap, *a, d),
                    (Some(a), Some(se_proto::ValueType::Enum)) => {
                        let options = plan.manifest.params.get(name).map_or(&[][..], |p| p.options.as_slice());
                        let index = res.get(snap, *a).and_then(|v| v.as_str()).and_then(|v| options.iter().position(|o| o == v));
                        [index.map_or(d[0], |i| i as f32), 0.0, 0.0, 0.0]
                    }
                    (Some(a), _) => [res.f32(snap, *a, d[0]), 0.0, 0.0, 0.0],
                    _ => d,
                }
            },
            |i, _| plan.signals.get(i).map_or(0.0, |s| res.signal(snap, *s)),
        );
        let (len, head) = frames.and_then(|i| self.frames.get(i)).and_then(Option::as_ref).map_or((0, 0), |s| (s.len, s.head));
        self.layout.write_history(&mut self.buf, len, head);
        arena.push_f32(&self.buf)
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_group<'b>(
        &self,
        device: &wgpu::Device,
        layouts: &Layouts,
        arena: &Arena,
        binds: &'b mut BindCache,
        a: Option<&Tex>,
        b: Option<&Tex>,
        sim: bool,
        frames: Option<usize>,
    ) -> &'b wgpu::BindGroup {
        let (a_id, b_id) = (a.map_or(0, |t| t.id), b.map_or(0, |t| t.id));
        if !sim && self.layout.frame_state() {
            // extended layout: se_prev / se_history of this instance, or placeholders (an
            // enter/exit style use, or no instance planned yet)
            let state = frames.and_then(|i| self.frames.get(i)).and_then(Option::as_ref);
            let key = match state {
                Some(s) => (KIND_PATCH_FRAMES, a_id, b_id, s.key()),
                None => (KIND_PATCH, a_id, b_id, self.storage_id),
            };
            return binds.get_or(key, || {
                let _p = se_alloc::Pause::new();
                let prev = state.and_then(|s| s.prev[s.read].as_ref()).map_or(&layouts.dummy_2d, |t| &t.view);
                let history = state.and_then(|s| s.history.as_ref()).map_or(&layouts.dummy_2d_array, |t| &t.view);
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("patch frames"),
                    layout: &layouts.patch_frames,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: arena.binding() },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&layouts.linear) },
                        wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(a.map_or(&layouts.dummy_2d, |t| &t.view)) },
                        wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(b.map_or(&layouts.dummy_2d, |t| &t.view)) },
                        wgpu::BindGroupEntry { binding: 4, resource: layouts.dummy_storage.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(prev) },
                        wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(history) },
                    ],
                })
            });
        }
        let key = (KIND_PATCH + u8::from(sim) * 16, a_id, b_id, self.storage_id);
        binds.get_or(key, || {
            let _p = se_alloc::Pause::new();
            let storage = self.particles.as_ref().unwrap_or(&layouts.dummy_storage);
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("patch"),
                layout: if sim { &layouts.sim } else { &layouts.patch },
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: arena.binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&layouts.linear) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(a.map_or(&layouts.dummy_2d, |t| &t.view)) },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(b.map_or(&layouts.dummy_2d, |t| &t.view)) },
                    wgpu::BindGroupEntry { binding: 4, resource: storage.as_entire_binding() },
                ],
            })
        })
    }

    /// Advance the particle simulation once per frame (no-op for shader patches).
    #[allow(clippy::too_many_arguments)]
    pub fn simulate(
        &mut self,
        device: &wgpu::Device,
        enc: &mut wgpu::CommandEncoder,
        layouts: &Layouts,
        arena: &mut Arena,
        binds: &mut BindCache,
        frame: u64,
        uniform: u32,
        count: u32,
    ) {
        let Some(UserPipes::Particles { sim, .. }) = self.pipes.as_deref() else { return };
        if self.last_sim_frame == frame {
            return;
        }
        self.last_sim_frame = frame;
        let bg = self.bind_group(device, layouts, arena, binds, None, None, true, None);
        let _p = se_alloc::Pause::new();
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("particles sim"), timestamp_writes: None });
        pass.set_pipeline(sim);
        pass.set_bind_group(0, bg, &[uniform]);
        pass.dispatch_workgroups(count.div_ceil(64).max(1), 1, 1);
    }

    /// Render the patch into `target` (cleared first). `a`/`b` are the effect input or the
    /// transition's outgoing/incoming scenes; `frames` the frame-state slot of an effect pass.
    /// A `feedback = "state"` shader also writes its state buffer and needs that slot; without
    /// one it does not draw (returns `false`, like an uncompiled patch).
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &self,
        device: &wgpu::Device,
        enc: &mut wgpu::CommandEncoder,
        layouts: &Layouts,
        arena: &Arena,
        binds: &mut BindCache,
        target: &wgpu::TextureView,
        uniform: u32,
        a: Option<&Tex>,
        b: Option<&Tex>,
        count: u32,
        frames: Option<usize>,
    ) -> bool {
        let Some(pipes) = self.pipes.as_deref() else { return false };
        let state_target = if self.layout.feedback == Feedback::State {
            match frames.and_then(|i| self.frames.get(i)).and_then(Option::as_ref).and_then(FrameState::state_target) {
                Some(t) => Some(&t.view),
                None => return false,
            }
        } else {
            None
        };
        let bg = self.bind_group(device, layouts, arena, binds, a, b, false, frames);
        let _p = se_alloc::Pause::new();
        let attachment = |view| {
            Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
            })
        };
        let attachments = [attachment(target), state_target.and_then(attachment)];
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("patch"),
            color_attachments: &attachments[..1 + usize::from(state_target.is_some())],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        match pipes {
            UserPipes::Fullscreen(p) => {
                pass.set_pipeline(p);
                pass.set_bind_group(0, bg, &[uniform]);
                pass.draw(0..3, 0..1);
            }
            UserPipes::Particles { draw, .. } => {
                pass.set_pipeline(draw);
                pass.set_bind_group(0, bg, &[uniform]);
                pass.draw(0..6, 0..count);
            }
        }
        true
    }
}
