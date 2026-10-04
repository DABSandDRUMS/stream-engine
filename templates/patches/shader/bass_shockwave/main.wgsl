// Bass Shockwave: hard-kick radial shockwave. Premultiplied alpha in/out.
//
// No frame history, so everything is reconstructed from the kick envelope (jumps to the hit's
// strength 0-1, decays as exp(-20 t)):
// - age: tau = -ln(kick) / 20 is the time since a full-strength hit; a softer hit reads a little
//   older and so enters the wave part-way through (smaller, already expanding).
// - hardness: kicks land on the 16th grid, so snapping the reconstructed hit time to the nearest
//   16th (beat.phase) gives the real elapsed time t; strength = kick * exp(20 t). Drummer timing
//   off the grid skews it (10 ms late reads ~20 % harder), hence the soft knee on `threshold`.
//   Without a tempo the strength is approximated as sqrt(kick).
// - ceiling / refractory: each wave is over within ~0.9 beat (max 0.45 s) so waves never stack;
//   quarter-note kicks get the full wave, off-beat 8ths about half, 16ths a third, and the bar
//   downbeat a little more. A new kick simply restarts the wave (one wave on screen at a time).

const DECAY: f32 = 20.0;

// Ring displacement profile: positive just inside the crest (pushes outward), negative outside.
fn ring_profile(x: f32) -> f32 {
    return -x * exp(-x * x) * 1.6487;  // peak magnitude 1 at |x| = 0.707
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let amount = max(p_amount(), 0.0) * clamp(se.env, 0.0, 1.0);
    let kick = clamp(s_band_kick(), 0.0, 1.0);
    let tau = -log(max(kick, 1e-6)) / DECAY;

    let bpm = s_beat_bpm();
    let tempo = bpm > 1.0;
    let beat_s = 60.0 / max(bpm, 1.0);

    // strength + grid weight of the latest kick
    var strength = sqrt(kick);
    var grid_w = 1.0;
    var dur = 0.4;
    if (tempo) {
        let hit_ph = s_beat_phase() - tau / beat_s;          // beats, relative to this beat
        let g = round(hit_ph * 4.0) * 0.25;                    // nearest 16th
        let t_real = clamp((s_beat_phase() - g) * beat_s, 0.0, tau);
        strength = kick * exp(DECAY * t_real);
        let sub = fract(g + 8.0);                              // 0, .25, .5, .75 within the beat
        grid_w = select(select(0.33, 0.5, abs(sub - 0.5) < 0.01), 1.0, sub < 0.01);
        let bar_hit = fract(s_lfo_bar() - tau / (4.0 * beat_s) + 2.0);
        grid_w *= select(1.0, 1.25, min(bar_hit, 1.0 - bar_hit) < 0.04);
        dur = min(0.45, 0.9 * beat_s);
    }
    let thr = clamp(p_threshold(), 0.0, 1.0);
    let hard = smoothstep(thr - 0.12, thr + 0.05, strength);

    // A trigger: one big wave now, and harder kick waves for the next ~3 s.
    let age = se.trigger_age;
    let big = clamp(log2(1.0 + max(se.trigger.amount, 0.0) / 100.0) / 3.0, 0.0, 1.0);
    let boost = 1.0 + 0.6 * exp(-0.5 * max(age, 0.0));
    let tint = select(vec3<f32>(1.0, 0.92, 0.8), se.trigger.user_color.rgb, se.trigger.user_color.a > 0.01);

    let speed = clamp(p_speed(), 0.25, 4.0);
    let life = clamp(tau / dur, 0.0, 1.0);
    let a_kick = min(amount * hard * grid_w * boost * (1.0 - life) * (1.0 - life * 0.5), 2.0);
    let r_kick = 0.8 * speed * (1.0 - exp(-tau * 4.0 / max(dur, 0.1)));

    let t_life = clamp(age / 1.0, 0.0, 1.0);
    let a_trig = amount * (1.3 + 0.7 * big) * (1.0 - t_life) * (1.0 - t_life);
    let r_trig = 1.25 * speed * (1.0 - exp(-age * 3.2));

    // aspect-correct polar coordinates around the centre
    let aspect = se.resolution.x / max(se.resolution.y, 1.0);
    let ctr = p_center();
    let d = (in.uv - ctr) * vec2<f32>(aspect, 1.0);
    let dist = length(d);
    let dir = d / max(dist, 1e-4);

    let w_kick = 0.06 + 0.07 * life;
    let w_trig = 0.08 + 0.06 * t_life;
    let pk = ring_profile((dist - r_kick) / w_kick) * a_kick;
    let pt = ring_profile((dist - r_trig) / w_trig) * a_trig;
    let crest = exp(-pow((dist - r_kick) / w_kick, 2.0)) * a_kick + exp(-pow((dist - r_trig) / w_trig, 2.0)) * a_trig;

    // Scale punch about the centre: sharp in, eases back within the wave's life.
    let punch = p_punch() * (a_kick * exp(-tau * 9.0) + 1.4 * a_trig * exp(-age * 5.0));
    let scale = 1.0 + min(punch, 0.08);

    // Radial chromatic push: red leads, blue lags; a little lens-style split near the hit.
    let push = clamp(p_push(), 0.0, 4.0) * 0.022 * (pk + pt);
    let core = clamp(p_push(), 0.0, 4.0) * 0.004 * dist * (a_kick * exp(-tau * 12.0) + a_trig * exp(-age * 4.0));
    let base = d / scale;
    let to_uv = vec2<f32>(1.0 / aspect, 1.0);
    let uv_r = ctr + (base - dir * (push * 1.0 + core)) * to_uv;
    let uv_g = ctr + (base - dir * (push * 0.6)) * to_uv;
    let uv_b = ctr + (base - dir * (push * 0.25 - core)) * to_uv;
    let lo = vec2<f32>(0.0);
    let hi = vec2<f32>(1.0);
    let cr = textureSampleLevel(se_input, se_sampler, clamp(uv_r, lo, hi), 0.0);
    let cg = textureSampleLevel(se_input, se_sampler, clamp(uv_g, lo, hi), 0.0);
    let cb = textureSampleLevel(se_input, se_sampler, clamp(uv_b, lo, hi), 0.0);
    // keep the input's coverage at this pixel (premultiplied: channels travel with their alpha)
    let a = cg.a;
    var rgb = vec3<f32>(cr.r, cg.g, cb.b);

    // faint light crest (screen), tinted by the trigger user's colour while the alert wave runs
    let ring_col = mix(vec3<f32>(1.0, 0.95, 0.88), tint, clamp(a_trig, 0.0, 1.0));
    let glow = vec3<f32>(1.0) - exp(-clamp(p_ring(), 0.0, 1.0) * 0.6 * crest * ring_col);
    rgb = rgb + glow * max(vec3<f32>(a) - rgb, vec3<f32>(0.0));
    return vec4<f32>(rgb, a);
}
