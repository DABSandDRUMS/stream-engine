//! Built-in video effect library (§4.3). Every effect is a WGSL fragment pass over a premultiplied
//! RGBA texture with up to [`MAX_PARAMS`] float params, addressable as `fx.<name>.<param>` and
//! triggerable as `fx.<name>` (the core provides the envelope `fx.<name>.env`).
//!
//! Strength model (identical for every effect, so presets/bindings/triggers compose):
//! `strength = max(amount, level × env)` for the implicit global instance, where `amount` is the
//! latched intensity (presets `set`, bindings, manual) and `level × env` is the triggered
//! contribution (a trigger payload `amount`/`level` overrides `level` for that trigger).
//! Attached instances are on at their independent `amount` (default 1), or explicitly select
//! `triggered = true` to follow the shared effect-kind envelope. `enabled = false` bypasses
//! a slot regardless of its envelope; host `fx_enabled = false` bypasses the chain.

/// Max float params per effect (uniform block rows × 4).
pub const MAX_PARAMS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamDef {
    pub name: &'static str,
    pub default: f32,
    pub range: [f32; 2],
    pub unit: Option<&'static str>,
    pub description: &'static str,
}

const fn p(name: &'static str, default: f32, lo: f32, hi: f32, description: &'static str) -> ParamDef {
    ParamDef { name, default, range: [lo, hi], unit: None, description }
}

const fn pu(name: &'static str, default: f32, lo: f32, hi: f32, unit: &'static str, description: &'static str) -> ParamDef {
    ParamDef { name, default, range: [lo, hi], unit: Some(unit), description }
}

/// Where the implicit global instance of an effect runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    Source,
    Node,
    Scene,
    Canvas,
    Output,
}

impl Point {
    pub fn parse(s: &str) -> Option<Point> {
        Some(match s {
            "source" => Point::Source,
            "node" => Point::Node,
            "scene" => Point::Scene,
            "canvas" => Point::Canvas,
            "output" => Point::Output,
            _ => return None,
        })
    }
}

/// How the effect is executed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exec {
    /// One full-screen fragment pass.
    Simple,
    /// One full-screen pass whose output pixel depends only on the input pixel at the same
    /// position (and that position). The WGSL is a function `fn <name>(c: vec4<f32>, uv:
    /// vec2<f32>, st: FxStage) -> vec4<f32>`, so adjacent pointwise effects run fused in one pass
    /// ([`fused_source`]) with the same math as their single passes.
    Pointwise,
    /// Downsample ×4, separable gaussian at reduced resolution, composite (`fx_aux`).
    Blur,
    /// Samples a 3D LUT (`fx_lut`); the `.cube` file comes from the string param `file`.
    Lut,
}

#[derive(Clone, Copy, Debug)]
pub struct EffectDef {
    pub name: &'static str,
    pub description: &'static str,
    /// Params after the two standard ones (`amount`, `level`).
    pub params: &'static [ParamDef],
    /// Default of `amount` (latched strength). Effects that are identity at their param
    /// defaults (grade) default to 1 so setting any param shows immediately.
    pub amount_default: f32,
    /// Default of `level` (strength when triggered).
    pub level_default: f32,
    pub point: Point,
    pub exec: Exec,
    /// Scaled down by the video flash limiter.
    pub flashy: bool,
    /// Skip the pass when all params equal these identity values (only for `amount_default` 1).
    pub identity: Option<&'static [f32]>,
    pub wgsl: &'static str,
}

impl EffectDef {
    /// All params in uniform order: `amount`, `level`, then [`EffectDef::params`].
    pub fn all_params(&self) -> impl Iterator<Item = ParamDef> + '_ {
        [
            p("amount", self.amount_default, 0.0, 1.0, "Latched strength (presets, bindings, manual)"),
            p("level", self.level_default, 0.0, 1.0, "Strength while triggered (× envelope)"),
        ]
        .into_iter()
        .chain(self.params.iter().copied())
    }

    pub fn param_index(&self, name: &str) -> Option<usize> {
        self.all_params().position(|p| p.name == name)
    }
}

pub const COMMON_WGSL: &str = include_str!("shaders/fx_common.wgsl");

/// The library, in global chain order (canvas point first, then output).
pub static LIBRARY: &[EffectDef] = &[
    EffectDef {
        name: "zoom_pulse",
        description: "Punch-in zoom, optionally pulsing on the beat",
        params: &[
            p("zoom", 0.08, 0.0, 0.5, "Zoom at full strength (0.08 = 8%)"),
            p("beat", 1.0, 0.0, 1.0, "1 = pulse with beat.phase, 0 = steady"),
            p("center_x", 0.5, 0.0, 1.0, "Zoom center x"),
            p("center_y", 0.5, 0.0, 1.0, "Zoom center y"),
        ],
        amount_default: 0.0,
        level_default: 1.0,
        point: Point::Canvas,
        exec: Exec::Simple,
        flashy: true,
        identity: None,
        wgsl: include_str!("shaders/fx/zoom_pulse.wgsl"),
    },
    EffectDef {
        name: "pixelate",
        description: "Mosaic blocks",
        params: &[pu("size", 32.0, 1.0, 256.0, "px", "Block size at full strength")],
        amount_default: 0.0,
        level_default: 0.8,
        point: Point::Canvas,
        exec: Exec::Simple,
        flashy: true,
        identity: None,
        wgsl: include_str!("shaders/fx/pixelate.wgsl"),
    },
    EffectDef {
        name: "blur",
        description: "Gaussian blur (runs at quarter resolution)",
        params: &[pu("radius", 16.0, 0.0, 96.0, "px", "Blur radius at full strength")],
        amount_default: 0.0,
        level_default: 1.0,
        point: Point::Canvas,
        exec: Exec::Blur,
        flashy: false,
        identity: None,
        wgsl: include_str!("shaders/fx/blur.wgsl"),
    },
    EffectDef {
        name: "glitch",
        description: "Digital block glitch: displaced slices, channel tearing",
        params: &[
            p("blocks", 0.5, 0.0, 1.0, "Amount of displaced blocks"),
            p("shift", 0.5, 0.0, 1.0, "Horizontal displacement"),
            p("color", 0.5, 0.0, 1.0, "Channel separation inside blocks"),
            p("speed", 1.0, 0.0, 4.0, "Re-randomization speed"),
        ],
        amount_default: 0.0,
        level_default: 0.8,
        point: Point::Canvas,
        exec: Exec::Simple,
        flashy: true,
        identity: None,
        wgsl: include_str!("shaders/fx/glitch.wgsl"),
    },
    EffectDef {
        name: "rgb_split",
        description: "Chromatic aberration: red/blue channels pushed apart",
        params: &[pu("angle", 0.0, -180.0, 180.0, "deg", "Split direction"), p("spread", 0.012, 0.0, 0.1, "Offset at full strength (fraction of width)")],
        amount_default: 0.0,
        level_default: 0.8,
        point: Point::Canvas,
        exec: Exec::Simple,
        flashy: true,
        identity: None,
        wgsl: include_str!("shaders/fx/rgb_split.wgsl"),
    },
    EffectDef {
        name: "vhs",
        description: "VHS tape look: jitter, chroma bleed, scanlines, noise",
        params: &[
            p("noise", 0.5, 0.0, 1.0, "Grain and tracking noise"),
            p("jitter", 0.5, 0.0, 1.0, "Horizontal line jitter"),
            p("scanlines", 0.5, 0.0, 1.0, "Scanline darkness"),
            p("bleed", 0.5, 0.0, 1.0, "Chroma bleed"),
        ],
        amount_default: 0.0,
        level_default: 0.8,
        point: Point::Canvas,
        exec: Exec::Simple,
        flashy: true,
        identity: None,
        wgsl: include_str!("shaders/fx/vhs.wgsl"),
    },
    EffectDef {
        name: "grade",
        description: "Color grade: warmth, tint, contrast, saturation, lift, exposure",
        params: &[
            p("warmth", 0.0, -1.0, 1.0, "Cool (−) to warm (+)"),
            p("tint", 0.0, -1.0, 1.0, "Green (−) to magenta (+)"),
            p("contrast", 1.0, 0.0, 2.0, "Contrast around mid grey"),
            p("saturation", 1.0, 0.0, 2.0, "Saturation"),
            p("lift", 0.0, -0.5, 0.5, "Shadow lift"),
            pu("exposure", 0.0, -3.0, 3.0, "EV", "Exposure"),
        ],
        amount_default: 1.0,
        level_default: 1.0,
        point: Point::Canvas,
        exec: Exec::Pointwise,
        flashy: true,
        identity: Some(&[0.0, 0.0, 1.0, 1.0, 0.0, 0.0]),
        wgsl: include_str!("shaders/fx/grade.wgsl"),
    },
    EffectDef {
        name: "lut",
        description: "3D LUT (.cube from assets/), mixed by strength",
        params: &[],
        amount_default: 0.0,
        level_default: 1.0,
        point: Point::Canvas,
        exec: Exec::Lut,
        flashy: false,
        identity: None,
        wgsl: include_str!("shaders/fx/lut.wgsl"),
    },
    EffectDef {
        name: "chroma_key",
        description: "Chroma key (green screen) with spill suppression",
        params: &[
            p("key_r", 0.0, 0.0, 1.0, "Key color red"),
            p("key_g", 1.0, 0.0, 1.0, "Key color green"),
            p("key_b", 0.0, 0.0, 1.0, "Key color blue"),
            p("similarity", 0.4, 0.0, 1.0, "Chroma distance keyed out"),
            p("smoothness", 0.08, 0.0, 1.0, "Edge softness"),
            p("spill", 0.1, 0.0, 1.0, "Spill suppression"),
        ],
        amount_default: 0.0,
        level_default: 1.0,
        point: Point::Source,
        exec: Exec::Pointwise,
        flashy: false,
        identity: None,
        wgsl: include_str!("shaders/fx/chroma_key.wgsl"),
    },
    EffectDef {
        name: "shake",
        description: "Screen shake: the image jolts around smoothly, zoomed so no edge shows",
        params: &[
            p("strength", 0.03, 0.0, 0.15, "Movement at full strength (fraction of the frame's shorter side)"),
            pu("speed", 12.0, 1.0, 40.0, "Hz", "Shakes per second"),
        ],
        amount_default: 0.0,
        level_default: 1.0,
        point: Point::Canvas,
        exec: Exec::Simple,
        flashy: true,
        identity: None,
        wgsl: include_str!("shaders/fx/shake.wgsl"),
    },
    EffectDef {
        name: "vignette",
        description: "Darkened edges",
        params: &[p("radius", 0.75, 0.1, 1.5, "Distance where darkening starts"), p("softness", 0.45, 0.01, 1.0, "Falloff width")],
        amount_default: 0.0,
        level_default: 0.6,
        point: Point::Canvas,
        exec: Exec::Pointwise,
        flashy: false,
        identity: None,
        wgsl: include_str!("shaders/fx/vignette.wgsl"),
    },
    EffectDef {
        name: "fade_to_black",
        description: "Fade the whole output (incl. overlays) to a color",
        params: &[
            p("color_r", 0.0, 0.0, 1.0, "Fade color red"),
            p("color_g", 0.0, 0.0, 1.0, "Fade color green"),
            p("color_b", 0.0, 0.0, 1.0, "Fade color blue"),
        ],
        amount_default: 0.0,
        level_default: 1.0,
        point: Point::Output,
        exec: Exec::Pointwise,
        flashy: false,
        identity: None,
        wgsl: include_str!("shaders/fx/fade_to_black.wgsl"),
    },
];

pub fn find(name: &str) -> Option<usize> {
    LIBRARY.iter().position(|e| e.name == name)
}

/// Most stages one fused pass runs (`FxUniforms.stages` in `fx_common.wgsl`).
pub const MAX_FUSED: usize = 8;

/// Full WGSL module for a library effect (entry points `vs`, `fs`).
pub fn module_source(def: &EffectDef) -> String {
    match def.exec {
        Exec::Pointwise => format!(
            "{COMMON_WGSL}\n{}\n@fragment\nfn fs(in: FxVsOut) -> @location(0) vec4<f32> {{\n    return {}(src(in.uv), in.uv, fx_stage());\n}}\n",
            def.wgsl, def.name
        ),
        _ => format!("{COMMON_WGSL}\n{}", def.wgsl),
    }
}

/// WGSL module running the pointwise library effects `chain` (library indices, ≤ [`MAX_FUSED`])
/// in one pass: stage `k` reads `fx.stages[k]` and is skipped at strength 0, so any ordered
/// subset of the chain runs through the same pipeline.
pub fn fused_source(chain: &[u8]) -> String {
    debug_assert!(chain.len() <= MAX_FUSED);
    let mut s = format!("{COMMON_WGSL}\n");
    let mut seen = [false; 256];
    for &i in chain {
        let def = &LIBRARY[i as usize];
        debug_assert_eq!(def.exec, Exec::Pointwise, "{} is not fusable", def.name);
        if !std::mem::replace(&mut seen[i as usize], true) {
            s += def.wgsl;
            s += "\n";
        }
    }
    s += "@fragment\nfn fs(in: FxVsOut) -> @location(0) vec4<f32> {\n    var c = src(in.uv);\n";
    for (k, &i) in chain.iter().enumerate() {
        s += &format!("    if fx.stages[{k}].strength > 0.0 {{\n        c = {}(c, in.uv, fx.stages[{k}]);\n    }}\n", LIBRARY[i as usize].name);
    }
    s += "    return c;\n}\n";
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_is_consistent() {
        for (i, e) in LIBRARY.iter().enumerate() {
            let n = e.all_params().count();
            assert!(n <= MAX_PARAMS, "{} has {n} params", e.name);
            assert_eq!(find(e.name), Some(i));
            assert!(se_proto::address::is_valid(&format!("fx.{}", e.name), false));
            for p in e.all_params() {
                assert!(p.range[0] <= p.default && p.default <= p.range[1], "{}.{} default outside range", e.name, p.name);
            }
            if let Some(id) = e.identity {
                assert_eq!(id.len(), e.params.len(), "{} identity length", e.name);
                assert_eq!(e.amount_default, 1.0);
            }
        }
    }

    fn validate(label: &str, src: &str) -> naga::Module {
        let module = naga::front::wgsl::parse_str(src).unwrap_or_else(|err| panic!("{label}: {}", err.emit_to_string(src)));
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
            .validate(&module)
            .unwrap_or_else(|err| panic!("{label}: {}", err.emit_to_string(src)));
        module
    }

    #[test]
    fn every_library_shader_validates() {
        for e in LIBRARY {
            let module = validate(e.name, &module_source(e));
            assert!(module.entry_points.iter().any(|ep| ep.name == "fs"), "{} lacks fs", e.name);
        }
    }

    #[test]
    fn fused_chains_validate_with_repeats_and_full_length() {
        let pw: Vec<u8> = (0..LIBRARY.len() as u8).filter(|i| LIBRARY[*i as usize].exec == Exec::Pointwise).collect();
        assert!(pw.len() >= 2, "need at least two fusable effects");
        // every effect repeated (same function called by two stages) and a full MAX_FUSED chain
        let repeated: Vec<u8> = pw.iter().flat_map(|i| [*i, *i]).collect();
        let full: Vec<u8> = pw.iter().copied().cycle().take(MAX_FUSED).collect();
        for chain in [&repeated[..repeated.len().min(MAX_FUSED)], &full[..]] {
            let src = fused_source(chain);
            validate(&format!("{chain:?}"), &src);
        }
    }
}
