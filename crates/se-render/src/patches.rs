//! Runtime of shader and particles patches (§6.1, §6.3): uniform blocks filled through the
//! generated-header layout (time, env, trigger payload, params, signals, palette, resolution,
//! region), the particle
//! storage buffer, and the passes that render a patch layer. Pipelines arrive compiled from the
//! loader thread; a failed recompile keeps the previous pipeline (last good version).

use crate::addr::Resolved;
use crate::pipelines::UserPipes;
use crate::plan::PatchPlan;
use crate::resources::{Arena, BindCache, Layouts, Tex};
use se_core::triggers::PAYLOAD_FLOATS;
use se_hub::Snapshot;
use std::sync::Arc;

pub const KIND_PATCH: u8 = 2;

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
    last_sim_frame: u64,
    pub last_render_ns: [u64; 2],
    pub min_interval_ns: u64,
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
            last_sim_frame: u64::MAX,
            last_render_ns: [0; 2],
            min_interval_ns: m.fps.map_or(0, |f| 1_000_000_000 / f.max(1) as u64),
        }
    }

    /// Adopt a (re)compiled pipeline whose layout matches this plan.
    pub fn set_pipes(&mut self, layout: se_patch::wgsl::Layout, pipes: Arc<UserPipes>) {
        if layout != self.layout {
            self.buf = vec![0.0; layout.floats];
            self.layout = layout;
        }
        self.pipes = Some(pipes);
    }

    pub fn particle_count(&self, plan: &PatchPlan) -> u32 {
        plan.manifest.particles.as_ref().map_or(0, |p| p.count)
    }

    /// Fill the uniform block and push it into the arena. `region`: uv rect of the node the pass
    /// runs for (full target otherwise).
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
        arena: &mut Arena,
    ) -> u32 {
        let env = env_override.unwrap_or_else(|| if plan.manifest.has_trigger { res.f32(snap, plan.env, 0.0) } else { 1.0 });
        let params = &plan.params;
        let defaults = &plan.defaults;
        let types = &self.layout.params;
        let fixed = se_patch::wgsl::Fixed {
            time: fi.time,
            dt: fi.dt,
            frame: fi.frame,
            env,
            resolution,
            progress,
            trigger_count: self.trigger_count,
            region,
            trigger: &self.trigger,
            palette: &fi.palette,
        };
        self.layout.write(
            &mut self.buf,
            &fixed,
            |i, _| {
                let d = defaults.get(i).copied().unwrap_or([0.0; 4]);
                match (params.get(i), types.get(i).map(|t| t.1)) {
                    (Some(a), Some(se_proto::ValueType::Color | se_proto::ValueType::Vec4 | se_proto::ValueType::Vec2)) => res.vec4(snap, *a, d),
                    (Some(a), _) => [res.f32(snap, *a, d[0]), 0.0, 0.0, 0.0],
                    _ => d,
                }
            },
            |i, _| plan.signals.get(i).map_or(0.0, |s| res.signal(snap, *s)),
        );
        arena.push_f32(&self.buf)
    }

    fn bind_group<'b>(
        &self,
        device: &wgpu::Device,
        layouts: &Layouts,
        arena: &Arena,
        binds: &'b mut BindCache,
        a: Option<&Tex>,
        b: Option<&Tex>,
        sim: bool,
    ) -> &'b wgpu::BindGroup {
        let key = (KIND_PATCH + u8::from(sim) * 16, a.map_or(0, |t| t.id), b.map_or(0, |t| t.id), self.storage_id);
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
        let bg = self.bind_group(device, layouts, arena, binds, None, None, true);
        let _p = se_alloc::Pause::new();
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("particles sim"), timestamp_writes: None });
        pass.set_pipeline(sim);
        pass.set_bind_group(0, bg, &[uniform]);
        pass.dispatch_workgroups(count.div_ceil(64).max(1), 1, 1);
    }

    /// Render the patch into `target` (cleared first). `a`/`b` are the effect input or the
    /// transition's outgoing/incoming scenes.
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
    ) -> bool {
        let Some(pipes) = self.pipes.as_deref() else { return false };
        let bg = self.bind_group(device, layouts, arena, binds, a, b, false);
        let _p = se_alloc::Pause::new();
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("patch"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
            })],
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
