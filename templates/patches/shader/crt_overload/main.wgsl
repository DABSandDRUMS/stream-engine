// CRT overload: barrel-curved tube, beam scanlines, RGB slot mask, bloom and chroma
// misconvergence. `amount` sets the everyday look, kicks/snares pump it, and the trigger
// envelope drives a full overload (vertical roll with sync bar, line tearing, snow).
// Input and output are premultiplied alpha; outside the curved tube is transparent.

fn crt_hash(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn crt_tap(uv: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(se_input, se_sampler, uv, 0.0);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let r0 = se.region.xy;
    let rs = max(se.region.zw - se.region.xy, vec2<f32>(1e-6));
    let q = (in.uv - r0) / rs;
    if (any(q < vec2<f32>(0.0)) || any(q > vec2<f32>(1.0))) {
        return crt_tap(in.uv);
    }
    let px_size = rs * se.resolution;
    let aspect = px_size.x / max(px_size.y, 1.0);
    let t = se.time;

    // Drive levels: baseline from `amount`, accents from the band, overload from the trigger.
    let kick = clamp(s_band_kick(), 0.0, 1.0);
    let snare = clamp(s_band_snare(), 0.0, 1.0);
    let boost = clamp(log(1.0 + max(se.trigger.amount, 0.0) / 100.0) / 2.4, 0.0, 1.0);
    // Overload timeline from the trigger age: slam in, hold, then the picture catches and settles.
    let age = se.trigger_age;
    let e = smoothstep(0.0, 0.08, age) * (1.0 - smoothstep(1.3, 2.4, age));
    let over = e * (0.8 + 0.2 * boost);
    let base = clamp(p_amount(), 0.0, 2.0);
    let hit = (0.7 * kick + 0.45 * snare) * p_pulse();
    let lvl = clamp(0.45 * base + 0.25 * base * hit + over, 0.0, 1.8);

    // Barrel curvature (plus a small high-voltage "breath" on kicks).
    var c = q - vec2<f32>(0.5);
    let ca = vec2<f32>(c.x * aspect, c.y) / aspect;
    let r2 = dot(ca, ca);
    let k = p_curve() * (0.10 + 0.10 * min(lvl, 1.0) + 0.08 * over);
    c = c * (1.0 + k * r2 * 2.2) * (1.0 - 0.006 * kick * p_pulse() * min(base, 1.0) - 0.03 * over * (0.5 + 0.5 * sin(t * 9.0)));
    let qc = c + vec2<f32>(0.5);
    // Rounded tube edge (soft, a couple of pixels wide).
    let edge_d = min(min(qc.x, 1.0 - qc.x) * px_size.x, min(qc.y, 1.0 - qc.y) * px_size.y);
    let tube = smoothstep(0.0, 2.0, edge_d);
    if (tube <= 0.0) {
        return vec4<f32>(0.0);
    }

    // Vertical roll: picture slips by up to a full frame during overload, sync bar at the seam.
    let roll_w = smoothstep(0.35, 1.0, over);
    let roll = roll_w * fract(age * 0.9);
    var y = fract(qc.y + roll);
    let seam = fract(1.0 - roll);
    var dy = abs(qc.y - seam);
    dy = min(dy, 1.0 - dy);
    let bar = roll_w * (1.0 - smoothstep(0.0, 0.045, dy));

    // Horizontal wobble and sync tear.
    let o2 = over * over;
    var x = qc.x;
    x = x + 0.0012 * min(lvl, 1.0) * sin(qc.y * 70.0 + t * 11.0);
    x = x + over * 0.006 * sin(qc.y * 9.0 + t * 23.0);
    x = x + over * 0.05 * exp(-qc.y * 9.0) * sin(t * 6.3);
    let band = floor(qc.y * 28.0 + t * 4.0);
    let tick = floor(t * 14.0);
    let tear_h = crt_hash(vec2<f32>(band, tick));
    let tear = select(0.0, (crt_hash(vec2<f32>(tick, band + 7.0)) - 0.5) * 0.14, tear_h > 0.78) * o2;
    x = x + tear;
    let suv = vec2<f32>(x, y);

    // Chroma misconvergence grows toward the edges and with the drive.
    let conv = (0.0007 + 0.0022 * min(lvl, 1.0) + 0.0015 * hit * min(base, 1.0) + 0.009 * over) * (0.4 + 2.2 * length(ca));
    let cdir = vec2<f32>(1.0, 0.25 * sin(t * 1.7 + qc.y * 3.0));
    let s_g = crt_tap(r0 + clamp(suv, vec2<f32>(0.0), vec2<f32>(1.0)) * rs);
    let s_r = crt_tap(r0 + clamp(suv + cdir * conv, vec2<f32>(0.0), vec2<f32>(1.0)) * rs);
    let s_b = crt_tap(r0 + clamp(suv - cdir * conv, vec2<f32>(0.0), vec2<f32>(1.0)) * rs);
    var col = vec3<f32>(s_r.r, s_g.g, s_b.b);
    let a = s_g.a;

    // Bloom: two rotated 4-tap rings of thresholded highlights.
    let bloom_amt = 0.3 * min(base, 1.2) + 0.35 * hit * min(base, 1.0) + 0.9 * over;
    var glow = vec3<f32>(0.0);
    if (bloom_amt > 0.01) {
        let rad = (3.0 + 4.0 * min(lvl, 1.0) + 8.0 * over) / px_size;
        for (var i = 0; i < 8; i = i + 1) {
            let fi = f32(i);
            let ang = fi * 0.7854 + select(0.0, 0.3927, i >= 4);
            let rr = select(1.0, 2.2, i >= 4);
            let o = vec2<f32>(cos(ang), sin(ang)) * rad * rr;
            let s = crt_tap(r0 + clamp(suv + o, vec2<f32>(0.0), vec2<f32>(1.0)) * rs).rgb;
            glow = glow + max(s - vec3<f32>(0.5), vec3<f32>(0.0));
        }
        glow = glow / 8.0 * bloom_amt * 1.8;
    }

    // Beam scanlines: bright lines get fatter, dark ones thinner.
    let lines = max(px_size.y / 3.0, 60.0);
    let lum = dot(col, vec3<f32>(0.299, 0.587, 0.114));
    let fy = fract(qc.y * lines) - 0.5;
    let beam_w = mix(0.22, 0.48, clamp(lum, 0.0, 1.0));
    let beam = exp(-(fy * fy) / (beam_w * beam_w));
    let scan_s = p_scanlines() * clamp(0.35 + 0.55 * min(lvl, 1.0), 0.0, 1.0);
    col = col * mix(1.0, beam * 1.35, scan_s);

    // Slot mask in screen pixels.
    let px = floor(in.pos.xy);
    let triad = i32(px.x) % 3;
    var m = vec3<f32>(0.72);
    if (triad == 0) { m.r = 1.25; } else if (triad == 1) { m.g = 1.25; } else { m.b = 1.25; }
    let slot_row = (i32(px.y) + select(0, 2, (i32(px.x) / 3) % 2 == 1)) % 4;
    m = m * select(1.0, 0.7, slot_row == 0);
    let mask_s = p_mask() * clamp(0.3 + 0.6 * min(lvl, 1.0), 0.0, 1.0);
    col = col * mix(vec3<f32>(1.0), m * 1.12, mask_s);

    col = col + glow;

    // Overload: brightness surge, snow, sync bar, green-ish phosphor smear.
    let snow = crt_hash(px * 0.73 + vec2<f32>(fract(t * 37.0) * 311.0, fract(t * 19.0) * 173.0));
    col = col * (1.0 + 0.35 * over + 0.12 * hit * min(base, 1.0) * 0.5);
    col = mix(col, vec3<f32>(snow * 0.9), 0.18 * o2 + 0.025 * min(lvl, 1.0) * snow);
    col = mix(col, col * vec3<f32>(0.85, 1.05, 0.95), 0.4 * over);
    col = col * (1.0 - 0.85 * bar) + vec3<f32>(0.9, 0.95, 1.0) * bar * (1.0 - bar) * 0.8;

    // Tube vignette.
    let vig = 1.0 - (0.25 + 0.35 * p_curve()) * min(lvl, 1.0) * smoothstep(0.15, 0.55, r2 * 1.6);
    col = col * vig * tube;

    let out_a = clamp(max(a * tube, max(col.r, max(col.g, col.b))), 0.0, 1.0);
    return vec4<f32>(clamp(col, vec3<f32>(0.0), vec3<f32>(out_a)), out_a);
}
