//! Pipelines, all built at load or hot reload (§21): the fixed compositor pipelines, the effect
//! library, and user shaders (shader/particles patches, project transitions) compiled with the
//! generated `se_patch::wgsl` header. User compile errors map back to the patch source line.

use crate::effects::{self, LIBRARY};
use crate::gpu::{ShaderError, scoped, shader_module};
use crate::plan::Blend;
use crate::resources::{COLOR, Layouts};
use se_patch::wgsl::Layout;
use se_patch::{Kind, Manifest};

pub const COMPOSITE_WGSL: &str = include_str!("shaders/composite.wgsl");
pub const CONVERT_WGSL: &str = include_str!("shaders/convert.wgsl");
pub const BLIT_WGSL: &str = include_str!("shaders/blit.wgsl");
pub const FLASH_WGSL: &str = include_str!("shaders/flash.wgsl");
pub const BLUR_PASSES_WGSL: &str = include_str!("shaders/blur_passes.wgsl");
pub const FADE_WGSL: &str = include_str!("shaders/fade.wgsl");

/// Source of a built-in transition shader (`se_core::transitions::SHADERS`; `shader = "<name>"`
/// in a transition file). Compiled like a project transition, with the file's settings.
pub fn builtin_transition_wgsl(name: &str) -> Option<&'static str> {
    Some(match name {
        "fade" => FADE_WGSL,
        "glitch" => include_str!("shaders/transitions/glitch.wgsl"),
        "zoomblur" => include_str!("shaders/transitions/zoomblur.wgsl"),
        _ => return None,
    })
}

pub fn blend_state(b: Blend) -> wgpu::BlendState {
    use wgpu::{BlendComponent as C, BlendFactor as F, BlendOperation as O};
    let over_alpha = C { src_factor: F::One, dst_factor: F::OneMinusSrcAlpha, operation: O::Add };
    let color = match b {
        Blend::Normal => over_alpha,
        Blend::Add => C { src_factor: F::One, dst_factor: F::One, operation: O::Add },
        Blend::Screen => C { src_factor: F::One, dst_factor: F::OneMinusSrc, operation: O::Add },
        Blend::Multiply => C { src_factor: F::Dst, dst_factor: F::OneMinusSrcAlpha, operation: O::Add },
    };
    wgpu::BlendState { color, alpha: over_alpha }
}

fn fullscreen(
    device: &wgpu::Device,
    label: &str,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    vs: &str,
    fs: &str,
    blend: Option<wgpu::BlendState>,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState { module, entry_point: Some(vs), compilation_options: Default::default(), buffers: &[] },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fs),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState { format: COLOR, blend, write_mask: wgpu::ColorWrites::ALL })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Fixed pipelines of the compositor.
pub struct Pipelines {
    pub composite: [wgpu::RenderPipeline; 4],
    /// Composite without blending (node → effect scratch).
    pub composite_copy: wgpu::RenderPipeline,
    pub convert: wgpu::RenderPipeline,
    pub blit: wgpu::RenderPipeline,
    pub flash: wgpu::ComputePipeline,
    /// Library effects, `LIBRARY` order.
    pub effects: Vec<wgpu::RenderPipeline>,
    pub blur_down: wgpu::RenderPipeline,
    pub blur_h: wgpu::RenderPipeline,
    pub blur_v: wgpu::RenderPipeline,
    /// Built-in crossfade transition.
    pub fade: wgpu::RenderPipeline,
}

impl Pipelines {
    pub fn new(device: &wgpu::Device, l: &Layouts) -> Result<Pipelines, String> {
        let m = |label: &str, src: &str| shader_module(device, label, src).map_err(|e| format!("built-in shader `{label}`: {e}"));
        let composite_m = m("composite", COMPOSITE_WGSL)?;
        let node_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("composite"),
            bind_group_layouts: &[Some(&l.node), Some(&l.node_tex)],
            immediate_size: 0,
        });
        let composite = Blend::ALL.map(|b| fullscreen(device, "composite", &node_layout, &composite_m, "vs", "fs", Some(blend_state(b))));
        let composite_copy = fullscreen(device, "composite copy", &node_layout, &composite_m, "vs", "fs", None);
        let convert_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("convert"),
            bind_group_layouts: &[Some(&l.convert)],
            immediate_size: 0,
        });
        let convert = fullscreen(device, "convert", &convert_layout, &m("convert", CONVERT_WGSL)?, "vs", "fs", None);
        let blit_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("blit"), bind_group_layouts: &[Some(&l.blit)], immediate_size: 0 });
        let blit = fullscreen(device, "blit", &blit_layout, &m("blit", BLIT_WGSL)?, "vs", "fs", None);
        let flash_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("flash"), bind_group_layouts: &[Some(&l.flash)], immediate_size: 0 });
        let flash_m = m("flash", FLASH_WGSL)?;
        let flash = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("flash"),
            layout: Some(&flash_layout),
            module: &flash_m,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let mut fx = Vec::with_capacity(LIBRARY.len());
        for e in LIBRARY {
            let module = m(e.name, &effects::module_source(e))?;
            fx.push(fullscreen(device, e.name, &l.fx_pipeline, &module, "vs", "fs", None));
        }
        let blur_m = m("blur passes", &format!("{}\n{BLUR_PASSES_WGSL}", effects::COMMON_WGSL))?;
        let blur_down = fullscreen(device, "blur down", &l.fx_pipeline, &blur_m, "vs", "fs_down", None);
        let blur_h = fullscreen(device, "blur h", &l.fx_pipeline, &blur_m, "vs", "fs_h", None);
        let blur_v = fullscreen(device, "blur v", &l.fx_pipeline, &blur_m, "vs", "fs_v", None);
        let fade_layout = Layout::new(&transition_manifest("fade", &[]));
        let fade = patch_fullscreen(device, l, "fade", &format!("{}{FADE_WGSL}", fade_layout.header())).map_err(|e| format!("built-in fade: {e}"))?;
        Ok(Pipelines { composite, composite_copy, convert, blit, flash, effects: fx, blur_down, blur_h, blur_v, fade })
    }
}

/// Compile a fused pointwise chain ([`effects::fused_source`]); uses the effect bind group.
pub fn compile_fused(device: &wgpu::Device, l: &Layouts, chain: &[u8]) -> Result<wgpu::RenderPipeline, String> {
    let label = chain.iter().map(|i| LIBRARY[*i as usize].name).collect::<Vec<_>>().join("+");
    let module = shader_module(device, &label, &effects::fused_source(chain)).map_err(|e| format!("fused effects `{label}`: {e}"))?;
    scoped(device, || fullscreen(device, &label, &l.fx_pipeline, &module, "vs", "fs", None)).map_err(|e| format!("fused effects `{label}`: {e}"))
}

/// A manifest describing a project transition shader, so it gets the same generated header
/// (params → `p_<name>()`) as patches.
pub fn transition_manifest(name: &str, params: &[(String, se_patch::ParamSpec)]) -> Manifest {
    Manifest {
        id: name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect(),
        dir: std::path::PathBuf::new(),
        kind: Kind::Shader,
        entry: String::new(),
        layer: se_patch::Layer::Transition,
        params: params.iter().cloned().collect(),
        trigger: Default::default(),
        has_trigger: false,
        budget: Default::default(),
        signals: Vec::new(),
        particles: None,
        label: name.to_string(),
        description: String::new(),
        size: None,
        fps: None,
        grants: Vec::new(),
    }
}

fn patch_fullscreen(device: &wgpu::Device, l: &Layouts, label: &str, src: &str) -> Result<wgpu::RenderPipeline, ShaderError> {
    let module = shader_module(device, label, src)?;
    scoped(device, || fullscreen(device, label, &l.patch_pipeline, &module, "se_vs", "fs", None)).map_err(|message| ShaderError {
        line: None,
        column: None,
        message,
    })
}

/// Compiled user shader.
pub enum UserPipes {
    Fullscreen(wgpu::RenderPipeline),
    Particles { sim: wgpu::ComputePipeline, draw: wgpu::RenderPipeline },
}

/// Map an error in `header_lines + source` back to `file:line:col: message`.
pub fn map_error(file: &str, header_lines: usize, e: &ShaderError) -> String {
    match e.line {
        Some(l) if l > header_lines => match e.column {
            Some(c) => format!("{file}:{}:{c}: {}", l - header_lines, e.message),
            None => format!("{file}:{}: {}", l - header_lines, e.message),
        },
        Some(_) => format!("{file}: {} (in the generated header — check the names/types you use from it)", e.message),
        None => format!("{file}: {}", e.message),
    }
}

/// Compile a fullscreen user shader (patch kind `shader`, or a transition) from its source.
pub fn compile_fullscreen(device: &wgpu::Device, l: &Layouts, file: &str, layout: &Layout, src: &str) -> Result<UserPipes, String> {
    let header = layout.header();
    let full = format!("{header}{src}");
    patch_fullscreen(device, l, file, &full).map(UserPipes::Fullscreen).map_err(|e| map_error(file, header.lines().count(), &e))
}

/// Compile a particles patch (`sim.wgsl` compute + `draw.wgsl` instanced quads).
pub fn compile_particles(device: &wgpu::Device, l: &Layouts, m: &Manifest, layout: &Layout, sim_src: &str, draw_src: &str) -> Result<UserPipes, String> {
    let spec = m.particles.as_ref().ok_or("particles patch without [particles]")?;
    let header = layout.header();
    let sim_pre = format!("{header}{}", se_patch::wgsl::particles_prelude(true, spec.count));
    let draw_pre = format!("{header}{}", se_patch::wgsl::particles_prelude(false, spec.count));
    let sim_file = if spec.sim.is_empty() { "sim.wgsl" } else { spec.sim.as_str() };
    let draw_file = if spec.draw.is_empty() { "draw.wgsl" } else { spec.draw.as_str() };
    let sim_m = shader_module(device, sim_file, &format!("{sim_pre}{sim_src}")).map_err(|e| map_error(sim_file, sim_pre.lines().count(), &e))?;
    let draw_m = shader_module(device, draw_file, &format!("{draw_pre}{draw_src}")).map_err(|e| map_error(draw_file, draw_pre.lines().count(), &e))?;
    let sim = scoped(device, || {
        device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(sim_file),
            layout: Some(&l.sim_pipeline),
            module: &sim_m,
            entry_point: Some("sim"),
            compilation_options: Default::default(),
            cache: None,
        })
    })
    .map_err(|e| format!("{sim_file}: {e}"))?;
    let draw = scoped(device, || {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(draw_file),
            layout: Some(&l.patch_pipeline),
            vertex: wgpu::VertexState { module: &draw_m, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &draw_m,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format: COLOR, blend: Some(blend_state(Blend::Normal)), write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview_mask: None,
            cache: None,
        })
    })
    .map_err(|e| format!("{draw_file}: {e}"))?;
    Ok(UserPipes::Particles { sim, draw })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_lines_map_to_the_patch_source() {
        let e = ShaderError { line: Some(55), column: Some(7), message: "unknown identifier `foo`".into() };
        assert_eq!(map_error("main.wgsl", 50, &e), "main.wgsl:5:7: unknown identifier `foo`");
        let h = ShaderError { line: Some(3), column: None, message: "x".into() };
        assert!(map_error("main.wgsl", 50, &h).contains("generated header"));
    }

    #[test]
    fn builtin_shaders_validate() {
        for (name, src) in [
            ("composite", COMPOSITE_WGSL.to_string()),
            ("convert", CONVERT_WGSL.to_string()),
            ("blit", BLIT_WGSL.to_string()),
            ("flash", FLASH_WGSL.to_string()),
            ("blur", format!("{}\n{BLUR_PASSES_WGSL}", effects::COMMON_WGSL)),
            ("fade", format!("{}{FADE_WGSL}", Layout::new(&transition_manifest("fade", &[])).header())),
        ] {
            crate::gpu::validate_wgsl(&src).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    #[test]
    fn builtin_transitions_validate_with_their_settings() {
        for sh in se_core::transitions::SHADERS {
            let src = builtin_transition_wgsl(sh.name).unwrap_or_else(|| panic!("no source for built-in transition shader {}", sh.name));
            let params: Vec<(String, se_patch::ParamSpec)> = sh
                .params
                .iter()
                .map(|p| {
                    let spec = se_patch::ParamSpec {
                        ty: "float".into(),
                        default: Some(toml::Value::Float(p.default)),
                        range: None,
                        options: Vec::new(),
                        unit: None,
                        description: None,
                    };
                    (p.name.to_string(), spec)
                })
                .collect();
            let layout = Layout::new(&transition_manifest(sh.name, &params));
            let full = format!("{}{src}", layout.header());
            crate::gpu::validate_wgsl(&full).unwrap_or_else(|e| panic!("{}: {}", sh.name, map_error(sh.name, layout.header_lines(), &e)));
            assert!(sh.name == "fade" || src.contains("License"), "{} needs its license header", sh.name);
        }
    }
}
