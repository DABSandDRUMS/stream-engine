// Pocket Bounce: punch-in zoom on the kick with a damped spring settle.
// No frame history exists, so the spring is driven analytically: band.kick is an onset envelope
// that jumps to the hit strength and decays exponentially (engine default: to 5 % in 150 ms,
// i.e. exp(-20 t)), so `-ln(kick) / 20` recovers the time since the hit (weaker hits start
// further along the curve = smaller punch) and the bounce curve is evaluated at that time. The
// bar phase marks downbeats (bigger punch) and picks an alternating rotation direction per
// eighth note. Input and output are premultiplied alpha.

const KICK_DECAY: f32 = 20.0;

// Non-negative bouncing settle: 1 at the hit, touches rest between bounces, never below rest,
// so the frame only ever zooms *in* (no edge exposure).
fn bounce(u: f32, spring: f32) -> f32 {
    let damp = mix(18.0, 8.0, spring);
    let freq = mix(20.0, 38.0, spring);
    let c = cos(0.5 * freq * u);
    return exp(-damp * u) * c * c;
}

// Rotation wobble: starts at 0 (no snap), swings out and back with the same spring character.
fn wobble(u: f32, spring: f32) -> f32 {
    let damp = mix(16.0, 8.0, spring);
    let freq = mix(22.0, 34.0, spring);
    return exp(-damp * u) * sin(freq * u) * 1.6;
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let amount = max(p_amount(), 0.0);
    let spring = clamp(p_spring(), 0.0, 1.0);

    // time since the last kick, reconstructed from the envelope
    let kick = clamp(s_band_kick(), 0.0, 1.0);
    let u = -log(max(kick, 1e-4)) / KICK_DECAY;
    let gate = smoothstep(0.002, 0.02, kick);

    // downbeat emphasis + rotation direction from the bar clock (only when a tempo is known)
    let bpm = s_beat_bpm();
    var downbeat = 0.0;
    var dir = 1.0;
    if bpm > 1.0 {
        let bar_s = 240.0 / bpm;
        let hit_bar = fract(s_lfo_bar() - u / bar_s + 1.0);
        let eighth = floor(hit_bar * 8.0 + 0.5);
        downbeat = select(0.0, 1.0, eighth < 0.5 || eighth > 7.5);
        dir = select(1.0, -1.0, (i32(eighth) & 1) == 1);
    }

    // a trigger (alert) makes the next couple of seconds of punches bigger; se.env is the slot
    // strength when attached, so the moment is timed from se.trigger_age instead
    let alert = exp(-max(se.trigger_age, 0.0) * 0.9);
    let strength = amount * gate * (1.0 + 0.5 * downbeat) * (1.0 + 0.6 * alert);
    let punch = min(p_zoom() * strength * bounce(u, spring), 0.15);
    let angle = clamp(p_rotation(), 0.0, 1.0) * 0.01745 * strength * dir * wobble(u, spring);

    // scale needed so the rotated frame still covers the output, times the punch
    let aspect = se.resolution.x / max(se.resolution.y, 1.0);
    let sa = abs(sin(angle));
    let ca = cos(angle);
    let cover = ca + max(aspect, 1.0 / aspect) * sa;
    let scale = (1.0 + punch) * cover;

    // zoom about the frame center in aspect-correct space
    var p = (in.uv - vec2<f32>(0.5)) * vec2<f32>(aspect, 1.0);
    p = vec2<f32>(ca * p.x + sin(angle) * p.y, -sin(angle) * p.x + ca * p.y) / scale;
    let uv = p / vec2<f32>(aspect, 1.0) + vec2<f32>(0.5);
    return textureSample(se_input, se_sampler, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)));
}
