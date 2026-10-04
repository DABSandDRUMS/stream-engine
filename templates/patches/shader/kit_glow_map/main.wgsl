// Kit Glow Map: five soft light zones lit by their own drums. Premultiplied alpha in/out.
//
// Light model: the light colour multiplies the surface (out = c * (1 + gain * L)), so bright
// chrome, coated heads and cymbals catch it while black stays black, plus a smaller screen-blended
// haze term (light in the air), then a per-channel highlight shoulder so hot spots roll off to
// white like an overexposed stage lamp instead of clipping.
//
// Signals: hit envelopes jump to the hit strength and decay as exp(-20 t), so `-ln(env) / 20` is
// the time since the hit (soft hits read slightly older) and `env^p` (p < 1) is the same hit with
// a longer, natural light tail. There is no tom or crash signal; they are separated by timing:
// - snare-band hits that land on the quarter-note grid light the snare, hits off it (16th fills)
//   light the toms, and the fill's 16th position sweeps the tom light across its zone;
// - hat-band hits whose reconstructed hit time falls on the bar downbeat light the crash, plus a
//   short downbeat-locked ring that stays lit while band.high (cymbal wash) stays up.
// Without a tempo (bpm 0) everything snare-band lights the snare and every hat-band hit the hats.

const DECAY: f32 = 20.0;

fn since_hit(env: f32) -> f32 {
    return -log(max(env, 1e-6)) / DECAY;
}

// Soft elliptical light pool: a bright core plus a broad dim spill. z = (cx, cy, rx, ry) in uv.
fn pool(uv: vec2<f32>, z: vec4<f32>) -> f32 {
    let d = (uv - z.xy) / max(z.zw, vec2<f32>(1e-3));
    let r2 = dot(d, d);
    return exp(-2.2 * r2) + 0.12 * exp(-0.6 * r2);
}

// Stage-lamp shoulder on the added light only: the input below `lim` passes unchanged, light on
// top of it rolls off towards 1 instead of clipping.
fn shoulder(x: vec3<f32>, base: vec3<f32>) -> vec3<f32> {
    let lim = clamp(max(base, vec3<f32>(0.72)), vec3<f32>(0.0), vec3<f32>(0.999));
    let over = max(x - lim, vec3<f32>(0.0));
    return min(x, lim) + (vec3<f32>(1.0) - lim) * (vec3<f32>(1.0) - exp(-over / (vec3<f32>(1.0) - lim)));
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let c = textureSampleLevel(se_input, se_sampler, in.uv, 0.0);
    let amount = max(p_amount(), 0.0) * clamp(se.env, 0.0, 1.0);
    let tail = 1.0 / clamp(p_decay(), 0.2, 4.0);

    let kick = clamp(s_band_kick(), 0.0, 1.0);
    let snare = clamp(s_band_snare(), 0.0, 1.0);
    let hat = clamp(s_band_hat(), 0.0, 1.0);

    let bpm = s_beat_bpm();
    let tempo = select(0.0, 1.0, bpm > 1.0);
    let beat_s = 60.0 / max(bpm, 1.0);
    let bar_s = 4.0 * beat_s;

    // Snare vs toms: where on the beat grid did the latest snare-band hit land?
    let snare_ph = s_beat_phase() - since_hit(snare) / beat_s;
    let q_dist = abs(snare_ph - round(snare_ph));
    let on_quarter = mix(1.0, 1.0 - smoothstep(0.07, 0.13, q_dist), tempo);
    let snare_light = pow(snare, 0.45 * tail);
    let sixteenth = fract(snare_ph + 8.0) * 4.0;  // 1, 2, 3 for the e, &, a of a fill

    // Hats vs crash: did the latest hat-band hit land on the bar downbeat?
    let tb = s_lfo_bar() * bar_s;  // seconds since the downbeat (bar clock)
    let hat_bar = fract(s_lfo_bar() - since_hit(hat) / bar_s + 2.0);
    let down = tempo * (1.0 - smoothstep(0.025, 0.05, min(hat_bar, 1.0 - hat_bar)));
    let wash = tempo * exp(-3.0 * tb / tail) * smoothstep(0.35, 0.7, clamp(s_band_high(), 0.0, 1.0));

    var g = array<f32, 5>(
        pow(kick, 0.45 * tail),
        snare_light * on_quarter,
        snare_light * (1.0 - on_quarter),
        pow(hat, 0.5 * tail) * (1.0 - down),
        max(pow(hat, 0.3 * tail) * down, 0.7 * wash),
    );

    // Trigger (alert): the user's colour chases kick -> snare -> toms -> hats -> crash and back.
    let age = se.trigger_age;
    let chase_col = select(vec3<f32>(1.0, 0.85, 0.6), se.trigger.user_color.rgb, se.trigger.user_color.a > 0.01);
    let big = clamp(log2(1.0 + max(se.trigger.amount, 0.0) / 100.0) / 3.0, 0.0, 1.0);
    var zones = array<vec4<f32>, 5>(p_kick_zone(), p_snare_zone(), p_toms_zone(), p_hats_zone(), p_crash_zone());
    var cols = array<vec3<f32>, 5>(p_kick_color().rgb, p_snare_color().rgb, p_toms_color().rgb, p_hats_color().rgb, p_crash_color().rgb);

    // Fill sweep: the tom light walks across the tom zone with the fill's 16ths (high to floor).
    let sweep = clamp((sixteenth - 2.0) * 0.45, -0.6, 0.6) * (1.0 - on_quarter);
    zones[2] = vec4<f32>(zones[2].x + sweep * zones[2].z, zones[2].yzw);

    var light = vec3<f32>(0.0);
    for (var i = 0; i < 5; i++) {
        let w = pool(in.uv, zones[i]);
        let t = age - 0.11 * f32(i);
        let chase = select(0.0, exp(-6.0 * t) * (0.6 + 0.6 * big), t > 0.0 && age < 3.0);
        light += w * (cols[i] * g[i] + chase_col * chase);
    }
    light *= amount;

    // Light the picture: multiply (surface) + screen (haze), shoulder, keep the input alpha.
    let haze = clamp(p_haze(), 0.0, 1.0);
    let a = c.a;
    let lit = c.rgb * (vec3<f32>(1.0) + 2.2 * (1.0 - 0.5 * haze) * light);
    let air = vec3<f32>(1.0) - exp(-0.55 * haze * light);
    let rgb = lit + air * max(vec3<f32>(a) - lit, vec3<f32>(0.0));
    let inv = 1.0 / max(a, 1e-4);
    return vec4<f32>(shoulder(rgb * inv, c.rgb * inv) * a, a);
}
