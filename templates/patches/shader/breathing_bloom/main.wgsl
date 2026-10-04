// Breathing bloom: soft-knee highlight extraction, two-scale Vogel-disk blur of the bright parts,
// screen-blended back so highlights glow without washing out the frame. Premultiplied alpha in/out.

const GOLDEN: f32 = 2.39996323;
const INNER_TAPS: i32 = 8;
const OUTER_TAPS: i32 = 18;

// Bright part of `c` with a soft knee around `thr`. Brightness blends luma with the max channel
// so near-white highlights bloom but saturated surfaces (a red couch, a pink wall) do not.
fn bright(c: vec3<f32>, thr: f32, knee: f32) -> vec3<f32> {
    let br = 0.5 * (dot(c, vec3<f32>(0.2126, 0.7152, 0.0722)) + max(c.r, max(c.g, c.b)));
    var soft = clamp(br - thr + knee, 0.0, 2.0 * knee);
    soft = soft * soft / (4.0 * knee + 1e-4);
    return c * (max(soft, br - thr) / max(br, 1e-4));
}

// 2x2 interleaved pattern rotation: four rotated copies of the disk fill each other's gaps,
// while neighbouring pixels still read neighbouring texels (cache friendly, static, no shimmer).
fn cell_rot(p: vec2<f32>) -> f32 {
    let q = vec2<u32>(p) & vec2<u32>(1u);
    return f32(q.x + 2u * (q.y ^ q.x)) * (GOLDEN * 0.25);
}

// Gaussian-weighted Vogel disk of `n` taps out to `r` (uv units, x already aspect-corrected).
fn disk(uv: vec2<f32>, r: vec2<f32>, n: i32, rot: f32, thr: f32, knee: f32) -> vec3<f32> {
    var acc = vec3<f32>(0.0);
    var wsum = 0.0;
    let fn_ = f32(n);
    let step = vec2<f32>(cos(GOLDEN), sin(GOLDEN));
    var dir = vec2<f32>(cos(rot), sin(rot));
    for (var i = 0; i < n; i = i + 1) {
        let rr = sqrt((f32(i) + 0.5) / fn_);
        let w = exp(-2.2 * rr * rr);
        let s = textureSampleLevel(se_input, se_sampler, uv + dir * rr * r, 0.0).rgb;
        acc = acc + bright(s, thr, knee) * w;
        wsum = wsum + w;
        dir = vec2<f32>(dir.x * step.x - dir.y * step.y, dir.x * step.y + dir.y * step.x);
    }
    return acc / wsum;
}

// Trigger moment from se.trigger_age (seconds): rise, hold, fall; 0 before any trigger.
fn moment(age: f32, rise: f32, hold: f32, fall: f32) -> f32 {
    return smoothstep(0.0, rise, age) * (1.0 - smoothstep(rise + hold, rise + hold + fall, age));
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let c = textureSample(se_input, se_sampler, in.uv);

    let react = p_react();
    let level = smoothstep(0.1, 0.95, clamp(s_band_level(), 0.0, 1.0));
    let kick = clamp(s_band_kick(), 0.0, 1.0);
    let big = clamp(log2(1.0 + max(se.trigger.amount, 0.0) / 100.0) / 3.0, 0.0, 1.0);
    // Always-on effect: the trigger only adds a short swell (bigger payload = longer, stronger).
    let swell = moment(se.trigger_age, 0.15, 0.8 + 0.6 * big, 0.9) * (0.6 + 0.4 * big);

    // Louder band: lower threshold, wider and stronger glow. Kicks add a small bump on top.
    let open = react * (0.75 * level + 0.3 * kick) + swell;
    let thr = clamp(p_threshold() - 0.05 * react * level - 0.03 * react * kick - 0.08 * swell, 0.0, 1.0);
    let knee = 0.06 + 0.06 * thr;
    let aspect = se.resolution.y / max(se.resolution.x, 1.0);
    let rad = p_radius() * (1.0 + 0.25 * react * level + 0.08 * react * kick + 0.4 * swell);
    let r = vec2<f32>(rad * aspect, rad);

    let rot = cell_rot(in.pos.xy);
    let tight = disk(in.uv, r * 0.3, INNER_TAPS, rot, thr, knee);
    let wide = disk(in.uv, r, OUTER_TAPS, rot + 1.3, thr, knee);
    var glow = tight * 0.55 + wide * 0.75;

    let warm = vec3<f32>(1.0, 0.8, 0.58) * 1.12;
    glow = glow * mix(vec3<f32>(1.0), warm, p_warmth());
    // se.env is the slot strength when attached (1.0), so it scales the whole look.
    glow = glow * p_amount() * (0.85 + 0.6 * open) * clamp(se.env, 0.0, 1.0);
    // Soft ceiling so a hot frame never clips, then screen inside the input's coverage.
    glow = vec3<f32>(1.0) - exp(-glow * 1.2);
    let rgb = c.rgb + glow * max(vec3<f32>(c.a) - c.rgb, vec3<f32>(0.0));
    return vec4<f32>(rgb, c.a);
}
