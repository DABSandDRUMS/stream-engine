// Afterglow trails. Per pixel, a detail "emits" glow when it is
//   - bright (luma above `threshold`) and a thin highlight: brighter than both neighbours ~6 px
//     away along at least one of four axes (sticks, rims, glints). Edges of big bright areas
//     (walls, a face against a dark shirt) fail that test because one side is as bright,
//   - moving (luma changed since the last frame: state alpha keeps last frame's input luma),
//   - not skin coloured (YCbCr skin key, scaled by `face_guard`; light stick wood sits just
//     below the skin Cr range and still trails).
// The glow layer lives in the state buffer (feedback = "state"): each frame it is softly blurred
// (5 taps), decays with a half-life of `length` beats (frame-rate independent via se.dt) and takes
// the max with the new emission. The visible output screens glow plus a wider halo over the input,
// so the picture underneath is never smeared and static scenes look exactly like the input.

fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.299, 0.587, 0.114));
}

// 0-1 skin likelihood from chroma (BT.601 Cb/Cr box with soft edges).
fn skin(c: vec3<f32>) -> f32 {
    let y = luma(c);
    let cb = (c.b - y) * 0.564 + 0.5;
    let cr = (c.r - y) * 0.713 + 0.5;
    let in_cb = smoothstep(0.27, 0.31, cb) * (1.0 - smoothstep(0.49, 0.53, cb));
    let in_cr = smoothstep(0.56, 0.59, cr) * (1.0 - smoothstep(0.68, 0.72, cr));
    return in_cb * in_cr * smoothstep(0.12, 0.22, y);
}

fn tap(uv: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(se_prev, se_sampler, uv, 0.0);
}

@fragment
fn fs(in: SeVsOut) -> SeOut {
    let uv = in.uv;
    let px = 1.0 / se.resolution;
    let k = se.resolution.y / 720.0;
    let c = textureSampleLevel(se_input, se_sampler, uv, 0.0);
    let y = luma(c.rgb);

    // previous frame's state: glow (rgb), input luma encoded as 0.02 + 0.98 * y (0 = no history)
    let s0 = tap(uv);
    let d = 2.0 * k * px;
    let blurred = s0.rgb * 0.4 + (tap(uv + vec2<f32>(d.x, 0.0)).rgb + tap(uv - vec2<f32>(d.x, 0.0)).rgb + tap(uv + vec2<f32>(0.0, d.y)).rgb + tap(uv - vec2<f32>(0.0, d.y)).rgb) * 0.15;

    // thin highlight: brighter than both sides along the best of four axes
    let r = 6.0 * k * px;
    var ridge = -1.0;
    for (var i = 0; i < 4; i++) {
        let a = f32(i) * 0.7853982;
        let o = vec2<f32>(cos(a), sin(a)) * r;
        let ya = luma(textureSampleLevel(se_input, se_sampler, uv + o, 0.0).rgb);
        let yb = luma(textureSampleLevel(se_input, se_sampler, uv - o, 0.0).rgb);
        ridge = max(ridge, min(y - ya, y - yb));
    }
    let th = p_threshold();
    let highlight = smoothstep(th, th + 0.15, y) * smoothstep(0.04, 0.14, ridge);

    // motion against last frame's luma (no emission on the first frame after a restart)
    let valid = step(0.01, s0.a);
    let motion = valid * smoothstep(0.04, 0.16, abs(y - (s0.a - 0.02) / 0.98));

    let hits = max(s_band_kick(), s_band_snare());
    let gain = 1.0 + p_hit_boost() * hits;
    let guard = 1.0 - p_face_guard() * skin(c.rgb / max(c.a, 1e-3));
    let emit = clamp(highlight * motion * guard * gain, 0.0, 1.0);
    let own = c.rgb / max(y, 0.2) * 0.75;
    let col = mix(own, p_color().rgb, p_tint()) * 1.4;

    // decay: half-life `length` beats; small floor so 8-bit residue dies out
    let bpm = select(120.0, s_beat_bpm(), s_beat_bpm() > 30.0);
    let half_life = max(p_length(), 0.05) * 60.0 / bpm;
    let keep = exp2(-max(se.dt, 0.0) / half_life);
    let glow = clamp(max(blurred * keep - vec3<f32>(1.5 / 255.0), col * emit), vec3<f32>(0.0), vec3<f32>(1.0));

    // halo: the previous glow a little further out, for a soft bloom around the trail
    let h = 5.0 * k * px;
    let halo = (tap(uv + vec2<f32>(h.x, h.y)).rgb + tap(uv + vec2<f32>(-h.x, h.y)).rgb + tap(uv + vec2<f32>(h.x, -h.y)).rgb + tap(uv - h).rgb) * 0.25 * keep;
    let light = clamp((glow + halo * 0.9) * p_amount() * c.a, vec3<f32>(0.0), vec3<f32>(1.0));
    let rgb = c.rgb + (vec3<f32>(c.a) - c.rgb) * light; // screen, premultiplied

    var o: SeOut;
    o.color = vec4<f32>(rgb, c.a);
    o.state = vec4<f32>(glow, 0.02 + 0.98 * clamp(y, 0.0, 1.0));
    return o;
}
