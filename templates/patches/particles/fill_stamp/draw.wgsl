// Fill stamp drawing: one rotated quad per live stamp. The quad's local coordinates are the
// stamp's own frame in "stamp heights" (the box is 2.68 x 0.94), so the ink texture sticks to the
// rubber. The stamp drops in large, slams down (slight overshoot and rotation settle) and a
// jagged comic burst pops behind it on impact, then shrinks away; the stamp's ink is eroded by
// its texture as it fades. Output is premultiplied alpha.
const STAMP0: u32 = 2u;
const STAMP_N: u32 = 3u;
const QUAD_R: f32 = 1.95;
const FADE: f32 = 0.6;

// "FILL!" in 5x7 (row bits, bit 4 = leftmost): F I L L !
const FONT = array<u32, 35>(
    31u, 16u, 16u, 30u, 16u, 16u, 16u,
    14u, 4u, 4u, 4u, 4u, 4u, 14u,
    16u, 16u, 16u, 16u, 16u, 16u, 31u,
    16u, 16u, 16u, 16u, 16u, 16u, 31u,
    4u, 4u, 4u, 4u, 4u, 0u, 4u,
);

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) age: f32,
    @location(2) @interpolate(flat) life: f32,
    @location(3) @interpolate(flat) seed: f32,
    @location(4) @interpolate(flat) ink: vec4<f32>,
};

fn hash2(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn vnoise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash2(i);
    let b = hash2(i + vec2<f32>(1.0, 0.0));
    let c = hash2(i + vec2<f32>(0.0, 1.0));
    let d = hash2(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn sd_box(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + r;
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

// Stamp scale while it drops and slams: big and falling, squash past 1, settle.
fn slam(a: f32) -> f32 {
    if a < 0.07 {
        let e = a / 0.07;
        return mix(2.3, 0.92, e * e);
    }
    if a < 0.13 {
        return mix(0.92, 1.04, (a - 0.07) / 0.06);
    }
    if a < 0.21 {
        return mix(1.04, 1.0, (a - 0.13) / 0.08);
    }
    return 1.0;
}

// Comic burst scale: pops on impact, holds, then swells away (see burst_alpha).
fn burst_scale(a: f32) -> f32 {
    if a < 0.07 {
        return 0.0;
    }
    if a < 0.11 {
        return mix(0.5, 1.15, (a - 0.07) / 0.04);
    }
    if a < 0.18 {
        return mix(1.15, 1.0, (a - 0.11) / 0.07);
    }
    if a < 0.6 {
        return 1.0 + 0.04 * sin((a - 0.18) * 30.0) * exp(-(a - 0.18) * 6.0);
    }
    return 1.0 + (a - 0.6) * 1.5;
}

// Burst opacity: gone 0.14 s after it starts to swell away.
fn burst_alpha(a: f32) -> f32 {
    return clamp(1.0 - (a - 0.6) / 0.14, 0.0, 1.0);
}

@vertex
fn vs(@builtin(vertex_index) v: u32, @builtin(instance_index) i: u32) -> VOut {
    let p = se_particles[i];
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    var o: VOut;
    let c = corners[v % 6u];
    o.local = c * QUAD_R;
    if i < STAMP0 || i >= STAMP0 + STAMP_N || p.life <= 0.0 {
        o.pos = vec4<f32>(2.0, 2.0, 0.0, 1.0);
        return o;
    }
    let a = p.age;
    let settle = clamp(a / 0.13, 0.0, 1.0);
    let ang = p.color.x * (1.0 + 0.6 * (1.0 - settle));
    let h = p.size * se.resolution.y * slam(a);
    let off = o.local * h;
    let rot = vec2<f32>(cos(ang) * off.x - sin(ang) * off.y, sin(ang) * off.x + cos(ang) * off.y);
    let xy = p.pos * se.resolution + rot;
    o.pos = vec4<f32>(xy.x / se.resolution.x * 2.0 - 1.0, 1.0 - xy.y / se.resolution.y * 2.0, 0.0, 1.0);
    o.age = a;
    o.life = p.life;
    o.seed = p.color.z;
    // ink: the param colour, or the viewer's colour for stamps fired by a trigger
    let uc = se.trigger.user_color;
    let use_user = p.color.y > 0.5 && dot(uc.rgb, uc.rgb) > 0.01;
    o.ink = select(p_ink(), vec4<f32>(uc.rgb, 1.0), use_user);
    return o;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let q = in.local;
    let aa = max(fwidth(q.x), 1e-4);
    let a = in.age;
    let fade = clamp(1.0 - in.life / FADE, 0.0, 1.0);
    let fall = smoothstep(0.0, 0.05, a);
    let sd = in.seed * 97.0;

    // ink texture in the stamp's frame: blotches plus fine grain
    let n1 = vnoise(q * 7.0 + sd);
    let n2 = vnoise(q * 31.0 - sd);
    let grain = 0.6 * n1 + 0.4 * n2;
    let rough = (n2 - 0.5) * 0.035;

    // stamp shape: rounded double border and the chunky letters
    let outer = sd_box(q, vec2<f32>(1.34, 0.47), 0.12);
    var d = max(outer, -(outer + 0.085));
    let inner = sd_box(q, vec2<f32>(1.2, 0.33), 0.05);
    d = min(d, max(inner, -(inner + 0.03)));
    // bold: every lit column also lights its right neighbour (6 wide, advance 7)
    let u = 0.064;
    let g = (q - vec2<f32>(-17.0 * u, -3.5 * u)) / u;
    let gx = i32(floor(g.x));
    let gy = i32(floor(g.y));
    if gx >= 0 && gx < 34 && gy >= 0 && gy < 7 {
        let ci = gx / 7;
        let col = gx - ci * 7;
        let bits = FONT[u32(ci * 7 + gy)];
        let lit = (col < 5 && ((bits >> u32(4 - col)) & 1u) == 1u) || (col > 0 && col < 6 && ((bits >> u32(5 - col)) & 1u) == 1u);
        if lit {
            let cell = (fract(g) - 0.5) * u;
            d = min(d, sd_box(cell, vec2<f32>(0.5 * u + 0.004), 0.012));
        }
    }
    d = d + rough;
    let shape = clamp(0.5 - d / aa, 0.0, 1.0);
    let th = 0.14 + 0.9 * fade;
    let texture = smoothstep(th - 0.06, th + 0.06, grain);
    let ink_a = shape * texture * (0.78 + 0.22 * n1) * fall;

    // comic burst behind the stamp: jagged star, black outline, halftone dots
    var back = vec4<f32>(0.0);
    let bsc = burst_scale(a);
    let balpha = burst_alpha(a) * (1.0 - fade);
    if bsc > 0.0 && balpha > 0.0 {
        let bq = q / bsc * vec2<f32>(0.78, 1.0);
        let r = length(bq);
        let ang = atan2(bq.y, bq.x) / 6.2832 + 0.5;
        let spikes = 13.0;
        let k = floor(ang * spikes);
        let f = fract(ang * spikes);
        let tip = 1.3 + 0.32 * hash2(vec2<f32>(k, in.seed * 13.0));
        let rad = mix(0.95, tip, 1.0 - abs(2.0 * f - 1.0));
        let bd = (r - rad) * bsc;
        let fill = clamp(0.5 - (bd + 0.06) / aa, 0.0, 1.0);
        let edge = clamp(0.5 - bd / aa, 0.0, 1.0);
        let dots = length(fract(q / 0.12) - 0.5) * 0.12 < 0.045 * clamp(r / 1.3, 0.0, 1.0);
        let body = select(vec3<f32>(1.0, 0.88, 0.3), vec3<f32>(1.0, 0.55, 0.12), dots);
        let col = mix(vec3<f32>(0.0), body, fill);
        back = vec4<f32>(col * edge, edge) * balpha;
    }
    let ink = in.ink.rgb;
    return vec4<f32>(ink * ink_a, ink_a) + back * (1.0 - ink_a);
}
