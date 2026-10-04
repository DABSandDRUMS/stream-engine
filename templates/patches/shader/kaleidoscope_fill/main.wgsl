// Kaleidoscope Fill: N-fold mirrored kaleidoscope around `center`, sampling a wedge whose apex
// sits on `source` in the picture.
// Polar angle around the center (aspect-correct) is folded into one wedge and mirrored at its
// middle, so neighbouring wedges meet with identical pixels (no seams); sample positions that
// leave the picture are mirror-repeated at its borders instead of clamped (no streaks or black).
// Rotation: one eased tick per beat (fast out, settles by 40 % of the beat), `spin` wedges per
// bar; beat index = round(se.time × bpm / 60 − beat.phase) (stable tempo assumed, 120 BPM on
// se.time without a tempo). While the envelope rises the pattern blooms open from the center
// with a soft edge and an extra twist that unwinds; on release it closes the same way.
// Input and output are premultiplied alpha.

const TAU: f32 = 6.2831853;

fn mirror01(x: f32) -> f32 {
    let t = fract(x * 0.5) * 2.0;
    return 1.0 - abs(1.0 - t);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let base = textureSampleLevel(se_input, se_sampler, in.uv, 0.0);
    let e = clamp(se.env, 0.0, 1.0) * clamp(p_amount(), 0.0, 1.0);
    if e <= 0.001 {
        return base;
    }
    let ee = e * e * (3.0 - 2.0 * e);

    // beat clock
    var bpm = s_beat_bpm();
    var phase = s_beat_phase();
    if bpm < 1.0 {
        bpm = 120.0;
        phase = fract(se.time * 2.0);
    }
    let beat_s = 60.0 / bpm;
    let bi = f32(round(se.time / beat_s - phase));
    let tick = 1.0 - pow(1.0 - clamp(phase / 0.4, 0.0, 1.0), 3.0);

    let n = f32(clamp(p_segments(), 2, 16));
    let seg = TAU / n;
    let rot = (bi + tick) * max(p_spin(), 0.0) * seg * 0.25 + (1.0 - ee) * seg * 0.75;

    let aspect = se.resolution.x / max(se.resolution.y, 1.0);
    let asp = vec2<f32>(aspect, 1.0);
    let p = (in.uv - p_center()) * asp;
    let r = length(p);
    let a = atan2(p.y, p.x) - rot;
    var m = a - seg * floor(a / seg);
    m = min(m, seg - m); // mirror inside the wedge: 0 … seg/2

    // the wedge opens downward (into the kit) from the source point; kick breathes the scale
    let kick = clamp(s_band_kick(), 0.0, 1.0);
    let scale = max(p_zoom(), 0.05) * (1.0 - 0.06 * clamp(p_pulse(), 0.0, 1.0) * kick);
    let ang = 0.25 * TAU - seg * 0.25 + m;
    let q = p_source() * asp + r * scale * vec2<f32>(cos(ang), sin(ang));
    let uv = q / asp;
    let kal = textureSampleLevel(se_input, se_sampler, vec2<f32>(mirror01(uv.x), mirror01(uv.y)), 0.0);

    // bloom open from the center with a soft edge
    let reach = length(max(p_center(), vec2<f32>(1.0) - p_center()) * asp) + 0.3;
    let rad = ee * reach;
    let mask = 1.0 - smoothstep(rad - 0.3, rad, r);
    return mix(base, kal, mask * ee);
}
