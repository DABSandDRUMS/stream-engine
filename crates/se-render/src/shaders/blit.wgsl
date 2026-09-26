// Full-screen copies: canvas output (opaque), flash-limiter temporal smoothing, plain copy.
struct Blit {
    mode: u32,      // 0 copy, 1 opaque (composite over black, alpha = 1), 2 limiter
    k: f32,         // limiter: max per-frame luminance step (1 = unlimited)
    _pad0: f32,
    _pad1: f32,
};

@group(0) @binding(0) var<uniform> bl: Blit;
@group(0) @binding(1) var src_a: texture_2d<f32>;
@group(0) @binding(2) var src_b: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VsOut;
    o.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    o.uv = vec2<f32>(p.x, 1.0 - p.y);
    return o;
}

fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let a = textureSampleLevel(src_a, samp, in.uv, 0.0);
    switch bl.mode {
        case 1u: {
            return vec4<f32>(a.rgb, 1.0);
        }
        case 2u: {
            // Limit the luminance change against the previous output frame (src_b) to ±k,
            // preserving hue: scale the step towards the previous color.
            let prev = textureSampleLevel(src_b, samp, in.uv, 0.0);
            let cur = vec4<f32>(a.rgb, 1.0);
            let d = luma(cur.rgb) - luma(prev.rgb);
            if abs(d) <= bl.k {
                return cur;
            }
            let t = bl.k / abs(d);
            return vec4<f32>(mix(prev.rgb, cur.rgb, t), 1.0);
        }
        default: {
            return a;
        }
    }
}
