// Light leaks: soft elliptical leaks anchored just outside the frame edges drift around the
// border, warped by low-frequency noise and screened over the input. Snare/crash flare them;
// the trigger floods a large flare from a corner and sweeps a hot band across the frame
// (driven by se.trigger_age). Premultiplied alpha in/out.

const LEAKS: i32 = 4;
const SWEEP_SECONDS: f32 = 1.8;

fn hash1(n: f32) -> f32 {
    return fract(sin(n * 127.1 + 311.7) * 43758.5453);
}

fn hash2(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn noise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash2(i);
    let b = hash2(i + vec2<f32>(1.0, 0.0));
    let c = hash2(i + vec2<f32>(0.0, 1.0));
    let d = hash2(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn fbm(p: vec2<f32>) -> f32 {
    return 0.6 * noise(p) + 0.3 * noise(p * 2.13 + vec2<f32>(5.2, 1.3)) + 0.1 * noise(p * 4.7 + vec2<f32>(2.9, 8.1));
}

// Film-leak ramp: deep magenta -> red-orange -> amber -> pale gold as the leak gets hotter.
fn leak_ramp(x: f32, warmth: f32) -> vec3<f32> {
    let magenta = vec3<f32>(0.78, 0.10, 0.42);
    let red = vec3<f32>(1.0, 0.28, 0.12);
    let amber = vec3<f32>(1.0, 0.62, 0.22);
    let pale = vec3<f32>(1.0, 0.9, 0.7);
    let base = mix(magenta, red, warmth);
    let c = mix(base, amber, smoothstep(0.3, 0.8, x));
    return mix(c, pale, smoothstep(0.9, 1.4, x));
}

// Trigger moment from se.trigger_age (seconds): rise, hold, fall; 0 before any trigger.
fn moment(age: f32, rise: f32, hold: f32, fall: f32) -> f32 {
    return smoothstep(0.0, rise, age) * (1.0 - smoothstep(rise + hold, rise + hold + fall, age));
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let c = textureSample(se_input, se_sampler, in.uv);
    let aspect = se.resolution.x / max(se.resolution.y, 1.0);
    let p = vec2<f32>((in.uv.x - 0.5) * aspect, in.uv.y - 0.5);
    let t = se.time * p_speed();
    let warmth = p_warmth();

    let react = p_react();
    let crash = smoothstep(0.55, 0.95, clamp(s_band_high(), 0.0, 1.0));
    let accent = clamp(react * max(clamp(s_band_snare(), 0.0, 1.0) * 0.8, crash), 0.0, 1.6);
    let big = clamp(log2(1.0 + max(se.trigger.amount, 0.0) / 100.0) / 3.0, 0.0, 1.0);
    // Always-on effect; the trigger's flare is timed from se.trigger_age (bigger payload = longer).
    let flare = moment(se.trigger_age, 0.25, 1.0 + 0.6 * big, 1.2);
    let lfo = clamp(s_lfo_slow(), 0.0, 1.0);

    let reach = p_reach() * (1.0 + 0.22 * accent + 0.35 * flare);
    let warp = fbm(p * 1.6 + vec2<f32>(t * 0.035, -t * 0.02));

    // Everyday leaks: elliptical glows anchored just outside the frame border, drifting along it
    // and fading in and out on their own slow cycles.
    var tint = vec3<f32>(0.0);
    for (var k = 0; k < LEAKS; k = k + 1) {
        let fk = f32(k);
        let base_angle = fk * 1.71 + 0.5 + (hash1(fk) - 0.5) * 0.6;
        let a = base_angle + 0.5 * sin(t * (0.05 + 0.017 * fk) + fk * 2.1) + 0.3 * (lfo - 0.5);
        let out_dir = vec2<f32>(cos(a), sin(a));
        // Point where the ray hits the frame border, pushed a little outside.
        let hit = min((0.5 * aspect + 0.08) / max(abs(out_dir.x), 1e-3), (0.58) / max(abs(out_dir.y), 1e-3));
        let anchor = out_dir * hit;
        let d = p - anchor;
        let along = dot(d, -out_dir);
        let side = dot(d, vec2<f32>(-out_dir.y, out_dir.x));
        let len = (0.26 + 0.08 * hash1(fk + 7.0) + 0.04 * sin(t * 0.09 + fk)) * reach;
        let wid = 0.3 + 0.18 * hash1(fk + 3.0);
        var g = exp(-(along * along) / (len * len) - (side * side) / (wid * wid));
        g = g * (0.35 + 1.3 * noise(p * 2.4 + vec2<f32>(fk * 7.3, t * 0.06 + fk)) * warp);
        g = g * smoothstep(0.1, 0.85, 0.5 + 0.5 * sin(t * (0.06 + 0.021 * fk) + fk * 1.7));
        // Alternate leaks lean magenta or warm so the border is never one flat colour.
        let lean = select(warmth, warmth * 0.3, k % 2 == 1);
        tint = tint + leak_ramp(g * 1.5, lean) * g;
    }
    var light = tint * (0.75 + 0.25 * lfo) * (1.0 + 0.9 * accent);

    // Trigger: a big corner flare plus a hot band sweeping across the frame.
    if flare > 0.0 {
        let hc = hash1(f32(se.trigger_count) + 0.37);
        let corner_a = floor(hc * 4.0) * 1.5708 + 0.785;
        let cdir = vec2<f32>(cos(corner_a), sin(corner_a));
        let corner = cdir * vec2<f32>(0.5 * aspect, 0.5);
        let dc = length((p - corner) / vec2<f32>(aspect * 0.5, 0.5));
        let flood = exp(-dc * dc / (0.25 + 0.3 * big + 0.15 * flare)) * (0.5 + 0.8 * warp);
        light = light + leak_ramp(flood * 1.3, warmth) * flood * flare * (0.6 + 0.4 * big);

        let age = clamp(se.trigger_age / (SWEEP_SECONDS * (1.0 + 0.3 * big)), 0.0, 1.0);
        let sweep = -1.7 + 3.4 * age * age * (3.0 - 2.0 * age);
        let q = p / vec2<f32>(0.5 * aspect, 0.5);
        let s = dot(q, -cdir);
        let bw = 0.22 + 0.15 * big;
        let band = exp(-pow((s - sweep) / bw, 2.0)) * (0.6 + 0.7 * warp);
        let streak = 0.5 + 0.5 * sin(dot(q, vec2<f32>(-cdir.y, cdir.x)) * 6.0 + warp * 5.0 + t * 0.4);
        let rays = 0.45 + 0.55 * streak * streak;
        light = light + leak_ramp(0.4 + band * 0.75, warmth) * band * rays * flare * (0.85 + 0.5 * big);
    }

    // Keep the centre of the frame (the drummer) clean in everyday use.
    let r = length(p / vec2<f32>(aspect * 0.5, 0.5));
    light = light * mix(0.12 + 0.4 * flare, 1.0, smoothstep(0.35, 1.05, r));

    // se.env is the slot strength when attached (1.0), so it scales the whole look.
    light = light * p_amount() * clamp(se.env, 0.0, 1.0);
    light = vec3<f32>(1.0) - exp(-light);
    // Gentle grain inside the leak so it reads as film, not a gradient.
    light = light * (1.0 + (hash2(in.pos.xy + fract(se.time) * 97.0) - 0.5) * 0.06);
    let rgb = c.rgb + light * max(vec3<f32>(c.a) - c.rgb, vec3<f32>(0.0));
    return vec4<f32>(rgb, c.a);
}
