// Loudness Heat: energy-following film grade. Premultiplied alpha in/out (graded in straight rgb).
//
// No frame history, so "slow" comes from slow signals instead of filtering:
// - loudness: band.level with the hit transients taken out (level * (1 - 0.35 kick - 0.2 snare):
//   a kick adds roughly a third to the level, a snare a fifth), mapped quiet..loud;
// - phrase: bar number = se.time * bpm / 240 (assumes a stable tempo), aligned to lfo.bar, gives
//   the position in an N-bar phrase; heat ramps up through the phrase and relaxes for about a bar
//   after it turns over (continuous at the boundary);
// - lfo.slow adds a slow breathing drift; mic.level with the band down = talking -> cool.
// Changes in the band itself (stopping, starting) are therefore immediate, kept small enough to
// read as a lighting cue.

const GOLDEN: f32 = 2.39996323;
const TAPS: i32 = 10;
const LUMA = vec3<f32>(0.2126, 0.7152, 0.0722);

fn bright(c: vec3<f32>, thr: f32) -> vec3<f32> {
    let br = max(dot(c, LUMA), max(c.r, max(c.g, c.b)) * 0.8);
    let k = smoothstep(thr, thr + 0.25, br);
    return c * k;
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let src = textureSampleLevel(se_input, se_sampler, in.uv, 0.0);
    let a = src.a;
    if (a <= 0.0) {
        return src;
    }
    let amount = max(p_amount(), 0.0) * clamp(se.env, 0.0, 1.0);

    // --- heat / cool ------------------------------------------------------------------------
    let level = clamp(s_band_level(), 0.0, 1.0);
    let hit = 0.35 * clamp(s_band_kick(), 0.0, 1.0) + 0.2 * clamp(s_band_snare(), 0.0, 1.0);
    let body = level * (1.0 - min(hit, 0.5));
    let loud = smoothstep(p_quiet(), max(p_loud(), p_quiet() + 0.01), body);
    let band_on = smoothstep(0.05, 0.18, level);

    // Phrase shape: heat ramps up through the phrase; at the turn-over it carries on from where a
    // build usually ends (about 1.6x the new phrase's opening loudness) and relaxes over ~1 bar,
    // so the grade does not snap back when the level drops after a fill.
    let bpm = s_beat_bpm();
    var phrase_term = 0.8;
    var carry = 0.0;
    if (bpm > 1.0) {
        let n = f32(max(p_phrase(), 1));
        let bar_f = s_lfo_bar();
        let bar_i = round(se.time * bpm / 240.0 - bar_f);
        let pos = (bar_i - n * floor(bar_i / n) + bar_f) / n;   // 0..1 through the phrase
        phrase_term = mix(0.5, 1.0, pow(pos, 1.4));
        carry = exp(-pos * n * 1.5) * min(loud * 1.6, 0.95);
    }
    let drift = 0.12 * (s_lfo_slow() - 0.5) * band_on;
    let swell = smoothstep(0.0, 0.3, se.trigger_age) * exp(-0.8 * max(se.trigger_age - 0.3, 0.0)) * 0.3;
    let heat = clamp(max(loud * phrase_term, carry) + drift + swell, 0.0, 1.2) * amount;

    let talk = smoothstep(0.1, 0.3, clamp(s_mic_level(), 0.0, 1.0)) * (1.0 - 0.7 * loud);
    let cool = clamp(max(talk, 0.55 * (1.0 - band_on)), 0.0, 1.0) * max(p_cool(), 0.0) * amount;

    // --- grade (straight rgb) -----------------------------------------------------------------
    var c = src.rgb / a;
    let warm = clamp(p_warmth(), 0.0, 2.0) * heat;
    // white balance: amber with heat, slightly blue/steel when cool
    c *= mix(vec3<f32>(1.0), vec3<f32>(1.06, 1.0, 0.9), warm);
    c *= mix(vec3<f32>(1.0), vec3<f32>(0.95, 0.99, 1.06), cool);
    // saturation
    let y = dot(c, LUMA);
    c = vec3<f32>(y) + (c - vec3<f32>(y)) * (1.0 + 0.18 * heat - 0.25 * cool);
    // gentle warm lift in the shadows, soft film S-curve as it heats
    c += vec3<f32>(0.04, 0.026, 0.01) * heat * (vec3<f32>(1.0) - clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)));
    let cc = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    c = mix(c, cc * cc * (3.0 - 2.0 * cc), 0.18 * min(heat, 1.0));

    // --- warm highlight glow ------------------------------------------------------------------
    let g_amt = max(p_glow(), 0.0) * heat;
    if (g_amt > 0.01) {
        let aspect = se.resolution.y / max(se.resolution.x, 1.0);
        let r = vec2<f32>(0.045 * aspect, 0.045) * (0.7 + 0.5 * min(heat, 1.0));
        let q = vec2<u32>(in.pos.xy) & vec2<u32>(1u);
        let rot = f32(q.x + 2u * (q.y ^ q.x)) * (GOLDEN * 0.25);
        var dir = vec2<f32>(cos(rot), sin(rot));
        let step = vec2<f32>(cos(GOLDEN), sin(GOLDEN));
        var acc = vec3<f32>(0.0);
        var wsum = 0.0;
        for (var i = 0; i < TAPS; i++) {
            let rr = sqrt((f32(i) + 0.5) / f32(TAPS));
            let w = exp(-2.0 * rr * rr);
            let s = textureSampleLevel(se_input, se_sampler, in.uv + dir * rr * r, 0.0);
            acc += bright(s.rgb / max(s.a, 1e-4), 0.62) * s.a * w;
            wsum += w;
            dir = vec2<f32>(dir.x * step.x - dir.y * step.y, dir.x * step.y + dir.y * step.x);
        }
        let glow = (vec3<f32>(1.0) - exp(-acc / wsum * g_amt * 0.9)) * vec3<f32>(1.0, 0.82, 0.6);
        c = c + glow * max(vec3<f32>(1.0) - c, vec3<f32>(0.0));
    }

    return vec4<f32>(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)) * a, a);
}
