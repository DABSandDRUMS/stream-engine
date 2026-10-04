// Drum portal: a ring opens around `center` on the trigger (spring open, hold, implode) and
// flicks partly open on big hits (kick together with a crash). Inside: swirl refraction with a
// little chromatic split; rim: palette-colored glow with sparks; on the trigger, pixel debris
// and four-point stars spill out on ballistic paths. Exactly the input at rest.
// Input and output are premultiplied alpha.

fn dp_hash(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn dp_hash4(n: u32) -> vec4<f32> {
    var x = n * 747796405u + 2891336453u;
    var v = vec4<u32>(x, x ^ 0x9e3779b9u, x * 1664525u + 1013904223u, x ^ 0x7f4a7c15u);
    v = v * 1664525u + 1013904223u;
    v = v ^ (v >> vec4<u32>(16u));
    v = v * 2246822519u;
    v = v ^ (v >> vec4<u32>(13u));
    return vec4<f32>(v >> vec4<u32>(8u)) / 16777216.0;
}

fn dp_tap(uv: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(se_input, se_sampler, uv, 0.0);
}

fn dp_rot(v: vec2<f32>, a: f32) -> vec2<f32> {
    let c = cos(a);
    let s = sin(a);
    return vec2<f32>(c * v.x - s * v.y, s * v.x + c * v.y);
}

// Trigger opening over the trigger age: spring open with overshoot, hold, ease shut by 2 s.
fn dp_open(age: f32) -> f32 {
    if (age >= 2.0) {
        return 0.0;
    }
    let spring = max(1.0 - exp(-age * 7.0) * cos(age * 11.0), 0.0);
    let c = smoothstep(1.3, 2.0, age);
    return spring * (1.0 - c * c * (3.0 - 2.0 * c));
}

fn dp_pal(i: u32) -> vec3<f32> {
    switch (i % 5u) {
        case 0u: { return palette(PAL_ACCENT).rgb; }
        case 1u: { return palette(PAL_CYAN).rgb; }
        case 2u: { return palette(PAL_MAGENTA).rgb; }
        case 3u: { return palette(PAL_YELLOW).rgb; }
        default: { return vec3<f32>(1.0); }
    }
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let src = dp_tap(in.uv);
    let r0 = se.region.xy;
    let rs = max(se.region.zw - se.region.xy, vec2<f32>(1e-6));
    let q = (in.uv - r0) / rs;
    let px_size = rs * se.resolution;
    let aspect = px_size.x / max(px_size.y, 1.0);
    let t = se.time;
    let strength = clamp(p_amount(), 0.0, 2.0);
    let age = se.trigger_age;
    let boost = clamp(log(1.0 + max(se.trigger.amount, 0.0) / 100.0) / 2.4, 0.0, 1.0);

    let o_trig = min(dp_open(age), clamp(se.env, 0.0, 1.0));
    let big = clamp(s_band_kick(), 0.0, 1.0) * smoothstep(0.7, 0.95, s_band_high());
    let o_hit = 0.5 * big * p_hits();
    let open = max(o_trig, o_hit);
    let debris_on = age < 1.9 && se.env > 0.0;
    if ((open < 0.002 && !debris_on) || strength <= 0.0 || any(q < vec2<f32>(0.0)) || any(q > vec2<f32>(1.0))) {
        return src;
    }

    let ctr = p_center();
    let pv = (q - ctr) * vec2<f32>(aspect, 1.0);
    let d = length(pv);
    let th = atan2(pv.y, pv.x);
    let big_r = p_radius() * (0.85 + 0.35 * boost);
    let r = big_r * open * (1.0 + 0.014 * sin(5.0 * th + t * 2.3) + 0.008 * sin(9.0 * th - t * 3.1));
    let aa = 1.5 / px_size.y;

    var col = src.rgb;
    var alpha = src.a;

    // Inside: swirl refraction (lens pull toward the center plus a twist), chroma on the twist.
    let inside = 1.0 - smoothstep(r - aa, r + aa, d);
    if (inside > 0.0 && r > 1e-4) {
        let s = clamp(1.0 - d / r, 0.0, 1.0);
        let tw = p_swirl() * min(strength, 1.5) * (s * s * 2.6 + s * 0.4) * open + t * 0.5 * s * min(open, 1.0);
        let lens = mix(1.0, 0.72, s * min(open, 1.0));
        let base_v = pv * lens;
        let to_uv = vec2<f32>(1.0 / aspect, 1.0);
        let ca = 0.06 * s * open;
        let sg = dp_tap(r0 + clamp(ctr + dp_rot(base_v, tw) * to_uv, vec2<f32>(0.0), vec2<f32>(1.0)) * rs);
        let sr = dp_tap(r0 + clamp(ctr + dp_rot(base_v, tw + ca) * to_uv, vec2<f32>(0.0), vec2<f32>(1.0)) * rs);
        let sb = dp_tap(r0 + clamp(ctr + dp_rot(base_v, tw - ca) * to_uv, vec2<f32>(0.0), vec2<f32>(1.0)) * rs);
        var inner = vec3<f32>(sr.r, sg.g, sb.b);
        // Depth: darker toward the rim, faint palette vortex streaks.
        let streak = pow(0.5 + 0.5 * sin(10.0 * log(max(d / r, 0.02)) + th * 3.0 - t * 5.0), 6.0) * (1.0 - s);
        let vortex_col = mix(palette(PAL_MAGENTA).rgb, palette(PAL_CYAN).rgb, 0.5 + 0.5 * sin(th + t));
        inner = inner * (0.82 + 0.25 * s) + vortex_col * streak * 0.22 * min(strength, 1.0) * sg.a;
        col = mix(col, inner, inside);
        alpha = mix(alpha, sg.a, inside);
    }

    // Rim glow: sharp hot core, soft halo both sides, sparks crawling around it.
    let vis = smoothstep(0.0, 0.12, open) * min(strength, 1.5);
    if (vis > 0.0) {
        let w = 0.008 + 0.006 * open;
        let x = (d - r) / w;
        let band = exp(-x * x);
        let xh = x * 3.2;
        let hot = exp(-xh * xh);
        let outside = d - r;
        let outer = select(0.0, exp(-outside / (0.03 + 0.03 * open)) * 0.55, outside > 0.0);
        let inner_h = select(0.0, exp(outside / 0.02) * 0.4, outside <= 0.0);
        let k = 0.5 + 0.5 * sin(th * 2.0 + t * 1.6);
        var rim = mix(palette(PAL_ACCENT).rgb, palette(PAL_MAGENTA).rgb, k);
        rim = mix(rim, palette(PAL_CYAN).rgb, 0.3 + 0.3 * sin(th * 3.0 - t * 2.1));
        // push toward saturation so the rim reads as colored light, not white
        let rl = max(rim.r, max(rim.g, rim.b));
        rim = pow(rim / max(rl, 1e-3), vec3<f32>(1.8));
        let seg = floor((th + 3.1416) * 14.0);
        let spark = step(0.86, dp_hash(vec2<f32>(seg, floor(t * 15.0)))) * exp(-abs(outside - 0.012) / 0.006);
        let glow = rim * (outer + inner_h + band * 1.3) + vec3<f32>(1.0, 0.97, 0.92) * hot * 0.9 + mix(rim, vec3<f32>(1.0), 0.4) * spark * 0.8;
        col = col + glow * vis;
    }

    // Debris and stars spilling from the rim after a trigger (only pixels inside the annulus the
    // particles can have reached by now run the loop).
    let vmax = 0.9 * (0.8 + 0.5 * boost);
    let reach_hi = big_r + vmax * age + 0.45 * age * age + 0.04;
    let reach_lo = big_r - 0.45 * age * age - 0.04;
    if (debris_on && d < reach_hi && d > reach_lo) {
        let n = 20u + u32(28.0 * boost);
        let p_px = q * px_size;
        let fade_all = clamp(se.env, 0.0, 1.0) * min(strength, 1.5);
        var acc = vec3<f32>(0.0);
        var solid = vec4<f32>(0.0);
        for (var i = 0u; i < 48u; i = i + 1u) {
            if (i >= n) {
                break;
            }
            let h = dp_hash4(i * 31u + se.trigger_count * 977u);
            let delay = h.y * h.y * 0.4;
            let tt = age - delay;
            let life = 0.8 + 0.6 * h.z;
            if (tt < 0.0 || tt > life) {
                continue;
            }
            let a = h.x * 6.2832;
            let dir = vec2<f32>(cos(a), sin(a));
            let v = (0.3 + 0.6 * h.z) * (0.8 + 0.5 * boost);
            let pos_h = dir * (big_r + v * tt) + vec2<f32>(0.0, 0.45 * tt * tt);
            let pos_px = (ctr + pos_h * vec2<f32>(1.0 / aspect, 1.0)) * px_size;
            let dd = p_px - pos_px;
            let life_f = 1.0 - tt / life;
            let scale = px_size.y / 720.0;
            let c = dp_pal(u32(h.w * 97.0));
            if (h.w < 0.55) {
                // chunky pixel debris: hard-edged square with a bright 1-px top-left bevel
                let half = floor((3.0 + 4.0 * h.z) * scale * (0.6 + 0.4 * life_f)) + 0.5;
                if (abs(dd.x) < half && abs(dd.y) < half) {
                    let bevel = select(1.0, 1.6, dd.x < -half + 1.5 * scale || dd.y < -half + 1.5 * scale);
                    let cover = smoothstep(0.0, 0.25, life_f);
                    solid = mix(solid, vec4<f32>(c * bevel, 1.0), cover);
                }
            } else {
                // four-point star that twinkles
                let len = (12.0 + 12.0 * h.z) * scale * (0.3 + 0.7 * life_f);
                let thin = 1.5 * scale;
                if (abs(dd.x) < len + 2.0 && abs(dd.y) < len + 2.0) {
                    let arm = max(clamp(1.0 - abs(dd.x) / len, 0.0, 1.0) * clamp(1.0 - abs(dd.y) / thin, 0.0, 1.0),
                                  clamp(1.0 - abs(dd.y) / len, 0.0, 1.0) * clamp(1.0 - abs(dd.x) / thin, 0.0, 1.0));
                    let dot_c = exp(-dot(dd, dd) / (4.0 * scale * scale));
                    let tw = 0.7 + 0.3 * sin(t * 25.0 + h.x * 40.0);
                    acc = acc + mix(c, vec3<f32>(1.0), 0.5) * (arm * arm + dot_c) * life_f * tw * 1.4;
                }
            }
        }
        col = mix(col, solid.rgb, solid.a * fade_all) + acc * fade_all;
        alpha = max(alpha, solid.a * fade_all);
    }

    let out_a = clamp(max(alpha, max(col.r, max(col.g, col.b))), 0.0, 1.0);
    return vec4<f32>(clamp(col, vec3<f32>(0.0), vec3<f32>(out_a)), out_a);
}
