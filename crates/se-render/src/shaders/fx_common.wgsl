// Shared prelude of every built-in effect. Textures hold premultiplied, sRGB-encoded RGBA.

// One stage of a fused pass: a pointwise effect's params and strength. Strength 0 = skipped.
struct FxStage {
    params: array<vec4<f32>, 4>,
    strength: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

struct FxUniforms {
    resolution: vec2<f32>,   // target size in px
    time: f32,               // seconds (master clock)
    strength: f32,           // 0..1 effective strength (never 0: zero-strength passes are skipped)
    region: vec4<f32>,       // uv rect (x0, y0, x1, y1) the effect applies to and samples from
    params: array<vec4<f32>, 4>, // amount, level, then the effect's own params in declaration order
    beat_phase: f32,
    bass: f32,
    seed: f32,
    _pad: f32,
    stages: array<FxStage, 8>, // fused passes only (MAX_FUSED stages, in chain order)
};

@group(0) @binding(0) var<uniform> fx: FxUniforms;
@group(0) @binding(1) var fx_sampler: sampler;
@group(0) @binding(2) var fx_input: texture_2d<f32>;
@group(0) @binding(3) var fx_aux: texture_2d<f32>;
@group(0) @binding(4) var fx_lut: texture_3d<f32>;

struct FxVsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> FxVsOut {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: FxVsOut;
    o.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    o.uv = vec2<f32>(p.x, 1.0 - p.y);
    return o;
}

// Effect param by index (0 = amount, 1 = level, 2.. = own params).
fn param(i: u32) -> f32 {
    return fx.params[i / 4u][i % 4u];
}

// The single-pass effect as a stage (pointwise effects run through the same function whether
// they run alone or fused with their neighbours).
fn fx_stage() -> FxStage {
    return FxStage(fx.params, fx.strength, 0.0, 0.0, 0.0);
}

// Stage param by index (same numbering as `param`).
fn stage_param(st: FxStage, i: u32) -> f32 {
    return st.params[i / 4u][i % 4u];
}

fn region_size() -> vec2<f32> {
    return fx.region.zw - fx.region.xy;
}

// uv relative to the region (0..1 inside it).
fn local_uv(uv: vec2<f32>) -> vec2<f32> {
    return (uv - fx.region.xy) / max(region_size(), vec2<f32>(1e-6));
}

fn region_uv(local: vec2<f32>) -> vec2<f32> {
    return fx.region.xy + local * region_size();
}

// Sample the input, clamped to the region so nothing outside it bleeds in.
fn src(uv: vec2<f32>) -> vec4<f32> {
    let half_texel = 0.5 / fx.resolution;
    let c = clamp(uv, fx.region.xy + half_texel, fx.region.zw - half_texel);
    return textureSampleLevel(fx_input, fx_sampler, c, 0.0);
}

fn unpremul(c: vec4<f32>) -> vec3<f32> {
    if c.a <= 1e-5 {
        return vec3<f32>(0.0);
    }
    return c.rgb / c.a;
}

fn premul(rgb: vec3<f32>, a: f32) -> vec4<f32> {
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)) * a, a);
}

fn luma(rgb: vec3<f32>) -> f32 {
    return dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
}

fn hash12(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn hash11(x: f32) -> f32 {
    return hash12(vec2<f32>(x, x * 1.618 + 0.5));
}
