// Cymbal fireflies: one soft glow quad per live mote. A tight hot core plus a wide warm halo;
// alpha is kept well below the colour so the glow mostly adds light (premultiplied output).
// Brightness = fade in/out x a slow per-mote firefly blink (never a fast flicker).
struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

const HALO: f32 = 5.0; // quad half-size in core radii

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
    if p.life <= 0.0 || p_amount() <= 0.0 {
        o.pos = vec4<f32>(2.0, 2.0, 0.0, 1.0);
        o.color = vec4<f32>(0.0);
        return o;
    }
    let aspect = se.resolution.x / max(se.resolution.y, 1.0);
    let clip = vec2<f32>(p.pos.x * 2.0 - 1.0, 1.0 - p.pos.y * 2.0);
    let half = vec2<f32>(p.size / aspect, p.size) * 2.0 * HALO;
    o.pos = vec4<f32>(clip + c * half, 0.0, 1.0);
    let fade = smoothstep(0.0, 0.6, p.age) * smoothstep(0.0, 1.2, p.life);
    // firefly blink: slow, per-mote rate and phase, soft peaks, never fully off
    let rate = 0.35 + 0.5 * fract(p.seed * 7.31);
    let b = 0.5 + 0.5 * sin(TAU_D * (p.age * rate) + p.seed * 40.0);
    let blink = 0.3 + 0.7 * smoothstep(0.25, 0.95, b);
    o.color = vec4<f32>(p.color.rgb, fade * blink * p_amount());
    return o;
}

const TAU_D: f32 = 6.2831853;

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let d = length(in.uv) * HALO;
    let edge = 1.0 - smoothstep(HALO * 0.6, HALO, d);
    let core = exp(-d * d * 1.6);
    let halo = (exp(-d * d * 0.18) * 0.35 + exp(-d * 0.7) * 0.25) * edge;
    let k = in.color.a;
    let rgb = (in.color.rgb * (core * 0.8 + halo) + vec3<f32>(1.0, 0.95, 0.85) * core * 0.55) * k;
    let a = clamp((core * 0.6 + halo * 0.2) * k, 0.0, 1.0);
    return vec4<f32>(rgb, a);
}
