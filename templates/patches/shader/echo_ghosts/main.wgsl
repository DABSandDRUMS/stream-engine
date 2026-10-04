// Echo ghosts. The state buffer (feedback = "state") holds a stepped video-feedback echo. On every
// 16th note (beat.phase × 4 wraps; 8 per second without a tempo) it takes one step
//   echo = mix(input, T(echo), echoes)
// where T scales the old echo up by `zoom` about the centre and shifts it by `drift` in a
// direction that turns once per bar; between 16ths it is carried over unchanged (texel copy).
// The visible ghosts are T(echo): the frame of the last 16th one step out, the one before two steps
// out (`echoes` times fainter) and so on — distinct copies that jump outward on the 16ths. The echo
// runs all the time; only the visible mix follows the echo level, so nothing changes at rest.
//
// Echo level: fills = snare hits off the 2/4 backbeat (bar_counter's fill_amount: band.snare
// decays as exp(-20 t), so -ln(snare)/20 is the time since the last hit, placed in the bar via
// lfo.bar + beat.phase); triggers = exp(-0.9 * trigger_age). State alpha (same in every pixel,
// read at texel 0,0) packs the level (7 bits) and the parity of the current 16th (1 bit): the
// level rises instantly and falls with a half-life of `release` seconds, the parity tells when the
// next 16th starts.

fn fill_amount(beat_pos: f32, bpm: f32) -> f32 {
    let sn = s_band_snare();
    if (sn < 1e-4) {
        return 0.0;
    }
    let t = -log(min(sn, 1.0)) / 20.0;
    let hit = beat_pos - t * max(bpm, 40.0) / 60.0;
    let pos = hit - 4.0 * floor(hit / 4.0);
    let backbeat = abs(pos - 1.0) < 0.12 || abs(pos - 3.0) < 0.12;
    return select(1.0 - smoothstep(0.1, 0.3, t), 0.0, backbeat);
}

@fragment
fn fs(in: SeVsOut) -> SeOut {
    let uv = in.uv;
    let c = textureSampleLevel(se_input, se_sampler, uv, 0.0);

    // beat within the bar (lfo.bar re-aligned to beat.phase so both wrap together)
    let has_tempo = s_beat_bpm() > 30.0;
    let phase = fract(s_beat_phase());
    let bar = fract(s_lfo_bar());
    let beat = i32(round(bar * 4.0 - phase)) & 3;
    let bpm = select(120.0, s_beat_bpm(), has_tempo);
    var want = fill_amount(f32(beat) + phase, bpm);
    if (p_triggers() && se.trigger_count > 0u) {
        want = max(want, exp(-0.9 * se.trigger_age) * (1.0 - smoothstep(2.0, 3.0, se.trigger_age)));
    }

    // unpack last frame's level and 16th parity
    let packed = u32(round(textureLoad(se_prev, vec2<i32>(0, 0), 0).a * 255.0));
    let before = f32(packed >> 1u) / 127.0;
    let sixteenth = select(u32(floor(se.time * 8.0)), u32(beat * 4 + i32(floor(phase * 4.0))), has_tempo);
    let parity = sixteenth & 1u;
    let step_now = parity != (packed & 1u);
    let level = max(want, max(before * exp2(-max(se.dt, 0.0) / max(p_release(), 0.01)) - 1.0 / 127.0, 0.0));

    // T: scale up about the centre (a little more on kicks), drift turning once per bar
    let aspect = se.resolution.y / se.resolution.x;
    let theta = 6.2831853 * (bar + 0.125);
    let drift = p_drift() * vec2<f32>(cos(theta) * aspect, sin(theta));
    let s = 1.0 + p_zoom() * (1.0 + 0.5 * s_band_kick());
    let q = vec2<f32>(0.5) + (uv - vec2<f32>(0.5)) / s - drift;
    let ghosts = textureSampleLevel(se_prev, se_sampler, q, 0.0).rgb;
    let held = textureSampleLevel(se_prev, se_sampler, uv, 0.0).rgb;
    let echo = select(held, mix(c.rgb, ghosts, p_echoes()), step_now);

    // visible: tinted copies over the input, only as much as the echo level
    let tinted = ghosts * mix(vec3<f32>(1.0), p_color().rgb * 1.15, 0.5) * c.a;
    let w = clamp(level * p_amount(), 0.0, 1.0);

    var o: SeOut;
    o.color = vec4<f32>(mix(c.rgb, min(tinted, vec3<f32>(c.a)), w), c.a);
    o.state = vec4<f32>(echo, f32((u32(round(level * 127.0)) << 1u) | parity) / 255.0);
    return o;
}
