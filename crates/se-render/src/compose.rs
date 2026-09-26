//! Encoding helpers shared by the compositor passes: node quads, effect passes, blits, and the
//! evaluation of effect attachments into per-frame [`FxEval`]s.

use crate::addr::{Resolved, Whens};
use crate::effects::{self, Exec, LIBRARY, MAX_PARAMS};
use crate::pipelines::Pipelines;
use crate::plan::{Attach, Blend, EffectKind, ParamSrc, Plan, StrSrc};
use crate::resources::{Arena, BindCache, Layouts, Tex};
use se_hub::Snapshot;

pub const KIND_NODE_U: u8 = 10;
pub const KIND_NODE_TEX: u8 = 11;
pub const KIND_FX: u8 = 12;
pub const KIND_BLIT: u8 = 13;
pub const KIND_FLASH: u8 = 14;
pub const KIND_NODE_TEX_NEAREST: u8 = 15;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct NodeUniform {
    pub dst: [f32; 4],
    pub uv: [f32; 4],
    pub color: [f32; 4],
    pub target_size: [f32; 2],
    pub radius: f32,
    pub opacity: f32,
    pub rotation: f32,
    pub premultiplied: f32,
    pub use_mask: f32,
    pub solid: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct FxUniform {
    pub resolution: [f32; 2],
    pub time: f32,
    pub strength: f32,
    pub region: [f32; 4],
    pub params: [f32; MAX_PARAMS],
    pub beat_phase: f32,
    pub bass: f32,
    pub seed: f32,
    pub pad: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct BlitUniform {
    pub mode: u32,
    pub k: f32,
    pub pad: [f32; 2],
}

pub const BLIT_COPY: u32 = 0;
pub const BLIT_OPAQUE: u32 = 1;
pub const BLIT_LIMIT: u32 = 2;

/// Borrowed encoding context.
pub struct Gfx<'a> {
    pub device: &'a wgpu::Device,
    pub layouts: &'a Layouts,
    pub pipes: &'a Pipelines,
    pub arena: &'a mut Arena,
    pub binds: &'a mut BindCache,
}

pub fn begin<'e>(enc: &'e mut wgpu::CommandEncoder, label: &'static str, view: &wgpu::TextureView, clear: Option<[f32; 4]>) -> wgpu::RenderPass<'e> {
    let load = match clear {
        Some(c) => wgpu::LoadOp::Clear(wgpu::Color { r: c[0] as f64, g: c[1] as f64, b: c[2] as f64, a: c[3] as f64 }),
        None => wgpu::LoadOp::Load,
    };
    let _p = se_alloc::Pause::new();
    enc.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    })
}

/// Draw one node quad. `tex = None` draws `u.color` (solid / no-signal).
pub fn draw_node(
    pass: &mut wgpu::RenderPass,
    g: &mut Gfx,
    u: &NodeUniform,
    tex: Option<&Tex>,
    mask: Option<(u32, &wgpu::TextureView)>,
    pipeline: &wgpu::RenderPipeline,
) {
    let off = g.arena.push_pod(u);
    let (device, layouts, arena) = (g.device, g.layouts, &*g.arena);
    let ubg = g.binds.get_or((KIND_NODE_U, 0, 0, 0), || {
        let _p = se_alloc::Pause::new();
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("node uniforms"),
            layout: &layouts.node,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: arena.binding() }],
        })
    });
    let _p = se_alloc::Pause::new();
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, ubg, &[off]);
    let tid = tex.map_or(0, |t| t.id);
    let mid = mask.map_or(0, |m| m.0);
    let tbg = g.binds.get_or((KIND_NODE_TEX, tid, mid, 0), || {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("node texture"),
            layout: &layouts.node_tex,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(tex.map_or(&layouts.dummy_2d, |t| &t.view)) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&layouts.linear) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(mask.map_or(&layouts.dummy_2d, |m| m.1)) },
            ],
        })
    });
    pass.set_bind_group(1, tbg, &[]);
    pass.draw(0..6, 0..1);
}

pub fn blend_pipeline(p: &Pipelines, b: Blend) -> &wgpu::RenderPipeline {
    &p.composite[b.index()]
}

fn fx_bind<'b>(g: &'b mut Gfx, input: &Tex, aux: Option<&Tex>, lut: Option<(u32, &wgpu::TextureView)>) -> &'b wgpu::BindGroup {
    let (device, layouts, arena) = (g.device, g.layouts, &*g.arena);
    g.binds.get_or((KIND_FX, input.id, aux.map_or(0, |t| t.id), lut.map_or(0, |l| l.0)), || {
        let _p = se_alloc::Pause::new();
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("fx"),
            layout: &layouts.fx,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: arena.binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&layouts.linear) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&input.view) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(aux.map_or(&layouts.dummy_2d, |t| &t.view)) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(lut.map_or(&layouts.dummy_3d, |l| l.1)) },
            ],
        })
    })
}

/// One full-screen effect-style pass `input → out` (scissored to `scissor` px when given).
#[allow(clippy::too_many_arguments)]
pub fn fx_pass(
    enc: &mut wgpu::CommandEncoder,
    g: &mut Gfx,
    pipeline: &wgpu::RenderPipeline,
    u: &FxUniform,
    input: &Tex,
    aux: Option<&Tex>,
    lut: Option<(u32, &wgpu::TextureView)>,
    out: &Tex,
    scissor: Option<[u32; 4]>,
) {
    let off = g.arena.push_pod(u);
    let bg = fx_bind(g, input, aux, lut);
    // declared first so it outlives the pass: ending a pass records commands inside wgpu
    let _p = se_alloc::Pause::new();
    let mut pass = begin(enc, "fx", &out.view, if scissor.is_some() { None } else { Some([0.0; 4]) });
    if let Some([x, y, w, h]) = scissor {
        pass.set_scissor_rect(x, y, w, h);
    }
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bg, &[off]);
    pass.draw(0..3, 0..1);
}

pub fn blit(enc: &mut wgpu::CommandEncoder, g: &mut Gfx, u: BlitUniform, a: &Tex, b: Option<&Tex>, out: &wgpu::TextureView) {
    let off = g.arena.push_pod(&u);
    let (device, layouts, arena) = (g.device, g.layouts, &*g.arena);
    let bb = b.unwrap_or(a);
    let bg = g.binds.get_or((KIND_BLIT, a.id, bb.id, 0), || {
        let _p = se_alloc::Pause::new();
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blit"),
            layout: &layouts.blit,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: arena.binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&a.view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&bb.view) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&layouts.linear) },
            ],
        })
    });
    let _p = se_alloc::Pause::new();
    let mut pass = begin(enc, "blit", out, Some([0.0; 4]));
    pass.set_pipeline(&g.pipes.blit);
    pass.set_bind_group(0, bg, &[off]);
    pass.draw(0..3, 0..1);
}

/// An effect attachment evaluated for this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FxEval {
    pub effect: usize,
    pub strength: f32,
    pub params: [f32; MAX_PARAMS],
    pub group: Option<u32>,
    /// Index into the renderer's LUT table (Exec::Lut).
    pub lut: u32,
}

/// Looks up (and requests) LUT textures by path.
pub trait LutLookup {
    /// `Some(id)` when loaded; `None` while loading or failed (the effect is skipped).
    fn lut(&mut self, path: &str) -> Option<u32>;
}

pub struct FxContext<'a> {
    pub plan: &'a Plan,
    pub snap: &'a Snapshot,
    pub res: &'a Resolved,
    /// Trigger payload level overrides per plan effect.
    pub trigger_levels: &'a [Option<f32>],
    /// Flash limiter scale for flashy effects.
    pub flash_scale: f32,
}

fn read_param(cx: &FxContext, src: ParamSrc, default: f32) -> f32 {
    match src {
        ParamSrc::Global(a) => cx.res.f32(cx.snap, a, default),
        ParamSrc::Local(a, v) => cx.res.f32(cx.snap, a, v),
        ParamSrc::Const(v) => v,
    }
}

/// Evaluate `attaches` into `out` (appends active ones; exclusive groups keep the strongest).
pub fn eval_attaches(cx: &FxContext, whens: &mut Whens, luts: &mut dyn LutLookup, attaches: &[Attach], out: &mut Vec<FxEval>) {
    let start = out.len();
    for a in attaches {
        let e = &cx.plan.effects[a.effect];
        if let Some(en) = a.enabled
            && !cx.res.bool(cx.snap, en, true)
        {
            continue;
        }
        if let Some(w) = a.when
            && !whens.eval(w as usize, &cx.plan.whens[w as usize], cx.snap, cx.res)
        {
            continue;
        }
        let mut params = [0.0f32; MAX_PARAMS];
        for (i, p) in params.iter_mut().enumerate().take(a.nparams) {
            *p = read_param(cx, a.params[i], e.defaults.get(i).copied().unwrap_or(0.0));
        }
        let env = cx.res.f32(cx.snap, e.env, 0.0).clamp(0.0, 1.0);
        let mut strength = match &e.kind {
            EffectKind::Builtin(_) => {
                let level = cx.trigger_levels.get(a.effect).copied().flatten().unwrap_or(params[1]);
                if a.global {
                    params[0].max(level * env)
                } else if a.triggered_only {
                    level * env
                } else {
                    params[0]
                }
            }
            EffectKind::Patch(_) => {
                if a.triggered_only {
                    env
                } else {
                    1.0
                }
            }
        };
        if let (Some(id), EffectKind::Builtin(_)) = (e.identity, &e.kind)
            && id.iter().zip(&params[2..]).all(|(i, p)| (i - p).abs() < 1e-4)
        {
            strength = 0.0;
        }
        if e.flashy {
            strength *= cx.flash_scale;
        }
        let strength = strength.clamp(0.0, 1.0);
        if strength <= 0.001 {
            continue;
        }
        let mut lut = 0;
        if let EffectKind::Builtin(i) = e.kind
            && LIBRARY[i].exec == Exec::Lut
        {
            let path = match &a.file {
                Some(StrSrc::Addr(addr)) => cx.res.str(cx.snap, *addr),
                Some(StrSrc::Const(s)) => Some(s.as_str()),
                None => None,
            };
            match path.filter(|p| !p.is_empty()).and_then(|p| luts.lut(p)) {
                Some(id) => lut = id,
                None => continue,
            }
        }
        out.push(FxEval { effect: a.effect, strength, params, group: a.group, lut });
    }
    // exclusive groups: keep the strongest (later wins ties)
    let mut i = start;
    while i < out.len() {
        let g = out[i].group;
        let beaten = g.is_some()
            && out[start..]
                .iter()
                .enumerate()
                .any(|(j, o)| j + start != i && o.group == g && (o.strength > out[i].strength || (o.strength == out[i].strength && j + start > i)));
        if beaten {
            out.remove(i);
        } else {
            i += 1;
        }
    }
}

/// Library effect for an evaluated attachment, if builtin.
pub fn builtin(plan: &Plan, e: &FxEval) -> Option<&'static effects::EffectDef> {
    match plan.effects[e.effect].kind {
        EffectKind::Builtin(i) => Some(&LIBRARY[i]),
        EffectKind::Patch(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::tests::snapshot;
    use crate::plan::tests::config;
    use se_proto::Value;
    use std::path::PathBuf;

    struct NoLuts;
    impl LutLookup for NoLuts {
        fn lut(&mut self, _: &str) -> Option<u32> {
            Some(7)
        }
    }

    #[test]
    fn uniform_layouts_match_wgsl() {
        assert_eq!(std::mem::size_of::<NodeUniform>(), 80);
        assert_eq!(std::mem::size_of::<FxUniform>(), 112);
        assert_eq!(std::mem::offset_of!(FxUniform, params), 32);
        assert_eq!(std::mem::offset_of!(FxUniform, beat_phase), 96);
        assert_eq!(std::mem::size_of::<BlitUniform>(), 16);
        assert_eq!(std::mem::size_of::<crate::sources::ConvertUniform>(), 48);
    }

    fn eval(p: &Plan, attaches: &[Attach], snap: &Snapshot, levels: &[Option<f32>], flash: f32) -> Vec<FxEval> {
        let mut res = Resolved::default();
        res.update(snap, &p.state, &p.signals);
        let mut whens = Whens::default();
        whens.reset(p.whens.len());
        let cx = FxContext { plan: p, snap, res: &res, trigger_levels: levels, flash_scale: flash };
        let mut out = Vec::new();
        eval_attaches(&cx, &mut whens, &mut NoLuts, attaches, &mut out);
        out
    }

    #[test]
    fn strength_model_and_groups() {
        let c = config(&[
            ("project", "project", "schema = 1"),
            (
                "scenes",
                "s",
                "fx = [{ name = \"vhs\", group = \"look\", amount = 0.3 }, { name = \"grade\", group = \"look\", amount = 0.8, warmth = 0.5 }, { name = \"rgb_split\", enabled = false }, { name = \"pixelate\", when = \"mode == 'chill'\" }]\n[canvas.wide]\nnodes = []",
            ),
        ]);
        let p = Plan::build(&c, &[], PathBuf::from("/tmp"));
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        let global = p.global_attaches(effects::Point::Canvas);
        let idx = |n: &str| p.effect_index[n];
        // nothing latched or triggered: no global effect except grade at identity → skipped
        let none = eval(&p, &global, &snapshot(&[], &[]), &[], 1.0);
        assert!(none.is_empty(), "{none:?}");
        // latched amount, triggered env × level, payload override
        let snap = snapshot(&[("fx.vhs.amount", Value::Float(0.4)), ("fx.rgb_split.env", Value::Float(0.5)), ("fx.grade.warmth", Value::Float(0.3))], &[]);
        let v = eval(&p, &global, &snap, &[], 1.0);
        let get = |v: &[FxEval], n: &str| v.iter().find(|e| e.effect == idx(n)).map(|e| e.strength);
        assert_eq!(get(&v, "vhs"), Some(0.4));
        assert_eq!(get(&v, "rgb_split"), Some(0.4), "level 0.8 × env 0.5");
        assert_eq!(get(&v, "grade"), Some(1.0), "grade departs from identity");
        let mut levels = vec![None; p.effects.len()];
        levels[idx("rgb_split")] = Some(1.0);
        assert_eq!(get(&eval(&p, &global, &snap, &levels, 1.0), "rgb_split"), Some(0.5));
        // flash limiter scales flashy effects
        assert_eq!(get(&eval(&p, &global, &snap, &[], 0.5), "vhs"), Some(0.2));
        // attached: group keeps the strongest, triggered-only follows env, when gates
        let scene = &p.scene("s").unwrap().layouts[crate::plan::WIDE].fx;
        let v = eval(&p, scene, &snap, &[], 1.0);
        assert_eq!(get(&v, "grade"), Some(0.8));
        assert_eq!(get(&v, "vhs"), None, "beaten in group `look`");
        assert_eq!(get(&v, "rgb_split"), Some(0.4));
        assert_eq!(get(&v, "pixelate"), None);
        let chill = snapshot(&[("show.mode", Value::Str("chill".into()))], &[]);
        assert_eq!(get(&eval(&p, scene, &chill, &[], 1.0), "pixelate"), Some(1.0));
    }
}
