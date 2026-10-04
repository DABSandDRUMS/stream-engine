// Node compositing: one rotated, cropped, rounded-corner quad per placement (SDF coverage for
// anti-aliased edges and corners), optional texture mask, opacity. Output is premultiplied.
struct Node {
    dst: vec4<f32>,        // x, y, w, h in target px (before rotation about the center)
    uv: vec4<f32>,         // x0, y0, x1, y1 in the sampled texture
    color: vec4<f32>,      // solid color (straight alpha) when `solid` = 1
    target_size: vec2<f32>,
    radius: f32,
    opacity: f32,
    rotation: f32,         // radians, clockwise
    premultiplied: f32,    // 1: texture is premultiplied; 0: straight alpha
    use_mask: f32,
    solid: f32,
    content: vec4<f32>,    // visible native content rect; zero uses dst (stretch)
};

@group(0) @binding(0) var<uniform> node: Node;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;
@group(1) @binding(2) var mask_tex: texture_2d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,   // px relative to the node center (unrotated)
    @location(1) uv: vec2<f32>,
    @location(2) muv: vec2<f32>,     // 0..1 across the node (mask)
};

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
    // 6 vertices, two triangles; expanded by 1px so edge coverage can fade out
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let c = corners[vi];
    let size = node.dst.zw;
    let half = size * 0.5;
    let local = (c - 0.5) * (size + vec2<f32>(2.0)) ;
    let s = sin(node.rotation);
    let co = cos(node.rotation);
    let rotated = vec2<f32>(local.x * co - local.y * s, local.x * s + local.y * co);
    let p = node.dst.xy + half + rotated;
    var o: VsOut;
    o.pos = vec4<f32>(p.x / node.target_size.x * 2.0 - 1.0, 1.0 - p.y / node.target_size.y * 2.0, 0.0, 1.0);
    o.local = local;
    let t = local / max(size, vec2<f32>(1e-3)) + 0.5;
    o.muv = t;
    // Content placement is unrotated; the window remains the rotation/mask/corner frame.
    var t_source = t;
    if node.content.z > 0.0 && node.content.w > 0.0 {
        let p_source = node.dst.xy + half + local;
        t_source = (p_source - node.content.xy) / node.content.zw;
    }
    // extrapolated past the rect edge (the quad is 1 px larger); clamped per fragment
    o.uv = mix(node.uv.xy, node.uv.zw, t_source);
    return o;
}

fn sd_round_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - r;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let half = node.dst.zw * 0.5;
    let r = clamp(node.radius, 0.0, min(half.x, half.y));
    let d = sd_round_box(in.local, half, r);
    var coverage = clamp(0.5 - d, 0.0, 1.0);
    if node.content.z > 0.0 && node.content.w > 0.0 {
        let content_center = node.content.xy + node.content.zw * 0.5;
        let p_source = node.dst.xy + half + in.local - content_center;
        coverage *= clamp(0.5 - sd_round_box(p_source, node.content.zw * 0.5, 0.0), 0.0, 1.0);
    }
    var c: vec4<f32>;
    if node.solid > 0.5 {
        c = vec4<f32>(node.color.rgb * node.color.a, node.color.a);
    } else {
        c = textureSampleLevel(tex, samp, clamp(in.uv, min(node.uv.xy, node.uv.zw), max(node.uv.xy, node.uv.zw)), 0.0);
        if node.premultiplied < 0.5 {
            c = vec4<f32>(c.rgb * c.a, c.a);
        }
    }
    var k = coverage * node.opacity;
    if node.use_mask > 0.5 {
        let m = textureSampleLevel(mask_tex, samp, clamp(in.muv, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
        k = k * m.r * m.a;
    }
    return c * k;
}
