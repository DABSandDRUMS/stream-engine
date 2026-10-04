// Time slice. Strip i of `bands` shows the input from age_i frames ago, read from the history ring
// (history = 8, se_history_at): age_i = round(rank_i * SE_HISTORY * level), where rank_i runs 0-1
// down the frame (`down`), up it (`up`) or from the centre outwards (`center_out`). Older strips
// shear sideways a little (alternating direction) and drift towards `tint`; a Win 3.1 style bevel
// seam (light line over dark line) separates the strips while the effect is up. With level 0
// every strip has age 0 and there is no seam: the output is the input.
//
// Level: fills = snare hits off the 2/4 backbeat (bar_counter's fill_amount: band.snare decays
// as exp(-20 t), so -ln(snare)/20 is the time since the last hit, placed in the bar via lfo.bar +
// beat.phase); triggers = exp(-1.5 * trigger_age). The level is kept in the state alpha
// (feedback = "state", same value in every pixel, read at texel 0,0): it rises instantly and
// falls with a 0.18 s half-life, which bridges the fill note that lands on the beat-4 backbeat
// (the strips only half catch up there) and lets them catch up smoothly after the fill.
// While the ring is still filling (first frames) ages clamp to what has been recorded.

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
    let phase = fract(s_beat_phase());
    let bar = fract(s_lfo_bar());
    let beat = i32(round(bar * 4.0 - phase)) & 3;
    let bpm = select(120.0, s_beat_bpm(), s_beat_bpm() > 30.0);
    var want = fill_amount(f32(beat) + phase, bpm);
    if (p_triggers() && se.trigger_count > 0u) {
        want = max(want, exp(-1.5 * se.trigger_age) * (1.0 - smoothstep(1.2, 2.0, se.trigger_age)));
    }
    let before = textureLoad(se_prev, vec2<i32>(0, 0), 0).a;
    let level = max(want, max(before * exp2(-max(se.dt, 0.0) / 0.18) - 1.0 / 255.0, 0.0));
    var o: SeOut;
    o.state = vec4<f32>(0.0, 0.0, 0.0, level);
    if (level < 0.01) {
        o.color = textureSampleLevel(se_input, se_sampler, uv, 0.0);
        return o;
    }

    let bands = f32(clamp(p_bands(), 2, 24));
    let fb = uv.y * bands;
    let bi = floor(fb);
    var rank = bi / (bands - 1.0);
    let order = p_order();
    if (order == 1) {
        rank = 1.0 - rank;
    } else if (order == 2) {
        rank = abs(bi - (bands - 1.0) * 0.5) / ((bands - 1.0) * 0.5);
    }
    let age = round(rank * f32(SE_HISTORY) * level);
    let old = age / f32(SE_HISTORY);
    let side = select(-1.0, 1.0, (i32(bi) & 1) == 0);
    let q = vec2<f32>(uv.x + side * p_shear() * old, uv.y);
    var c = se_history_at(q, u32(age));
    c = vec4<f32>(mix(c.rgb, c.rgb * p_tint().rgb * 1.25, 0.45 * old), c.a);

    // bevel seam at the top edge of every strip but the first: 1 light row over 1 dark row
    let px_y = se.resolution.y / bands;
    let rows = (fb - bi) * px_y;
    let w = max(1.0, round(se.resolution.y / 720.0));
    if (bi > 0.0) {
        let s = p_seams() * smoothstep(0.0, 0.4, level);
        if (rows < w) {
            c = vec4<f32>(mix(c.rgb, vec3<f32>(c.a), 0.55 * s), c.a);
        } else if (rows < 2.0 * w) {
            c = vec4<f32>(c.rgb * (1.0 - 0.6 * s), c.a);
        }
    }
    o.color = c;
    return o;
}
