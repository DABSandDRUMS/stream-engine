// Sparks: one soft round quad per particle (6 vertices per instance), premultiplied output.
// Positions are 0–1 of the layer (y down); sizes are fractions of the layer height.
struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) v: u32, @builtin(instance_index) i: u32) -> VOut {
    let p = se_particles[i];
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    let c = corners[v % 6u];
    var o: VOut;
    o.uv = c;
    if p.life <= 0.0 {
        o.pos = vec4<f32>(2.0, 2.0, 0.0, 1.0);
        o.color = vec4<f32>(0.0);
        return o;
    }
    let aspect = se.resolution.x / max(se.resolution.y, 1.0);
    let clip = vec2<f32>(p.pos.x * 2.0 - 1.0, 1.0 - p.pos.y * 2.0);
    let half = vec2<f32>(p.size / aspect, p.size) * 2.0;
    o.pos = vec4<f32>(clip + c * half, 0.0, 1.0);
    let flicker = 0.75 + 0.25 * sin(se.time * 40.0 + p.seed * 100.0);
    let fade = clamp(p.life / 0.5, 0.0, 1.0) * clamp(p.age / 0.05, 0.0, 1.0) * flicker;
    o.color = vec4<f32>(p.color.rgb, p.color.a * fade);
    return o;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let r = length(in.uv);
    let core = 1.0 - smoothstep(0.0, 0.35, r);
    let glow = 1.0 - smoothstep(0.2, 1.0, r);
    let a = in.color.a * clamp(glow * 0.6 + core, 0.0, 1.0);
    let rgb = mix(in.color.rgb, vec3<f32>(1.0), core * 0.6);
    return vec4<f32>(rgb * a, a);
}
