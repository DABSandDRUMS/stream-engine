// Color Atmosphere: luminance-preserving split toning with a tinted black floor, a gentle film
// contrast curve, skin-tone protection, and a saturation lift on strong hits. Each mood has two
// shades (A/B); the grade drifts between them with lfo.slow (phrases) plus a very slow clock, so
// it keeps breathing even without audio. Input and output are premultiplied alpha.

const TAU: f32 = 6.2831853;
const LUMA: vec3<f32> = vec3<f32>(0.2126, 0.7152, 0.0722);

// Tint for mood `m`, shade `b` (0/1), shadows (`hi` = false) or highlights (`hi` = true).
fn tint(m: i32, b: i32, hi: bool) -> vec3<f32> {
    let k = m * 4 + b * 2 + select(0, 1, hi);
    switch k {
        // warm_club: plum shadows / amber highlights  ->  wine shadows / soft gold
        case 0: { return vec3<f32>(0.45, 0.20, 0.65); }
        case 1: { return vec3<f32>(1.00, 0.72, 0.42); }
        case 2: { return vec3<f32>(0.60, 0.15, 0.35); }
        case 3: { return vec3<f32>(1.00, 0.80, 0.55); }
        // cool_night: navy / moonlight  ->  teal / lavender
        case 4: { return vec3<f32>(0.12, 0.30, 0.70); }
        case 5: { return vec3<f32>(0.80, 0.92, 1.00); }
        case 6: { return vec3<f32>(0.10, 0.50, 0.55); }
        case 7: { return vec3<f32>(0.90, 0.85, 1.00); }
        // sunset: violet / orange  ->  dusk blue / coral
        case 8: { return vec3<f32>(0.35, 0.20, 0.70); }
        case 9: { return vec3<f32>(1.00, 0.58, 0.30); }
        case 10: { return vec3<f32>(0.20, 0.25, 0.65); }
        case 11: { return vec3<f32>(1.00, 0.60, 0.55); }
        // neon: electric blue / hot pink  ->  violet / mint
        case 12: { return vec3<f32>(0.10, 0.45, 0.95); }
        case 13: { return vec3<f32>(1.00, 0.45, 0.85); }
        case 14: { return vec3<f32>(0.55, 0.15, 0.95); }
        case 15: { return vec3<f32>(0.50, 1.00, 0.85); }
        default: { return vec3<f32>(1.0); }
    }
}

// Luma-normalised multiplier (1 = no change) from a tint colour.
fn gain_of(c: vec3<f32>) -> vec3<f32> {
    return c / max(dot(c, LUMA), 1e-3);
}

// 0..1 "looks like skin": red-dominant, orange-ish hue, moderate saturation, not too dark.
fn skin_mask(c: vec3<f32>, l: f32) -> f32 {
    let mx = max(c.r, max(c.g, c.b));
    let mn = min(c.r, min(c.g, c.b));
    let ch = mx - mn;
    let red_top = step(c.g, c.r) * step(c.b, c.g);
    let hue = (c.g - c.b) / max(ch, 1e-4); // 0 = red, 1 = yellow
    let sat = ch / max(mx, 1e-4);
    let h = smoothstep(0.1, 0.3, hue) * (1.0 - smoothstep(0.7, 0.95, hue));
    let s = smoothstep(0.1, 0.2, sat) * (1.0 - smoothstep(0.5, 0.68, sat));
    return red_top * h * s * smoothstep(0.06, 0.18, l);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let src = textureSample(se_input, se_sampler, in.uv);
    if src.a <= 1e-4 {
        return src;
    }
    var c = clamp(src.rgb / src.a, vec3<f32>(0.0), vec3<f32>(1.0));
    let l = dot(c, LUMA);
    let str = max(p_amount(), 0.0);
    let m = clamp(p_mood(), 0, 3);

    // drift between the mood's two shades: phrase LFO + a ~45 s clock (alive with no audio)
    let clock = 0.5 + 0.5 * sin(se.time * TAU / 45.0);
    let d = smoothstep(0.0, 1.0, clamp(p_drift(), 0.0, 1.0) * (0.65 * clamp(s_lfo_slow(), 0.0, 1.0) + 0.35 * clock));
    let sh = mix(tint(m, 0, false), tint(m, 1, false), d);
    let hl = mix(tint(m, 0, true), tint(m, 1, true), d);

    let skin = skin_mask(c, l) * clamp(p_skin(), 0.0, 1.0);
    let ws = (1.0 - smoothstep(0.0, 0.65, l)) * (1.0 - 0.75 * skin);
    let wh = smoothstep(0.3, 1.0, l) * (1.0 - 0.4 * skin);

    // split tone (multiplicative, then restore the original luminance)
    c = c * mix(vec3<f32>(1.0), gain_of(sh), ws * 0.5 * str);
    c = c * mix(vec3<f32>(1.0), gain_of(hl), wh * 0.42 * str);
    c = c * mix(1.0, l / max(dot(c, LUMA), 1e-4), 0.85);
    // gentle film contrast, then a tinted black floor so deep shadows carry the mood
    c = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    c = mix(c, c * c * (3.0 - 2.0 * c), 0.12 * min(str, 1.0));
    let floor_w = pow(1.0 - l, 3.0) * (1.0 - 0.6 * skin);
    c = c + sh * (0.05 * str * floor_w);

    // saturation: slight base richness, a lift on strong hits and a fading bloom after a trigger
    // (se.env is the slot strength when attached, so the alert is timed from se.trigger_age)
    let hit = smoothstep(0.45, 1.0, max(s_band_kick(), 0.9 * s_band_snare()));
    let alert = smoothstep(0.0, 0.15, se.trigger_age) * exp(-max(se.trigger_age, 0.0) * 0.6);
    let lift = clamp(p_punch(), 0.0, 1.0) * (0.22 * hit + 0.2 * alert);
    let sat = 1.0 + (0.06 * str + lift) * (1.0 - 0.5 * skin);
    let l2 = dot(c, LUMA);
    c = clamp(mix(vec3<f32>(l2), c, sat), vec3<f32>(0.0), vec3<f32>(1.0));

    return vec4<f32>(c * src.a, src.a);
}
