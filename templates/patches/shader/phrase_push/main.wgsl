// Phrase Push: a slow push-in locked to the bar clock that builds over `bars` bars and resets on
// the phrase downbeat.
// Clock: beat index = round(se.time × bpm / 60 − beat.phase) (stable tempo assumed), beat in bar
// from lfo.bar, bar index = (beat index − beat in bar) / 4 (only steps on a downbeat), and the
// position in the phrase = (bar mod bars + lfo.bar) / bars. With no tempo it runs at 120 BPM on
// se.time. The push accelerates gently into the next downbeat; "smooth" eases the accumulated
// zoom back out over 0.6 beat while the next push already starts, "snap" cuts straight back.
// Zoom ≥ 1 and a drift bounded by the zoom margin keep the window inside the picture.
// Input and output are premultiplied alpha.

const RESET_BEATS: f32 = 0.6;

fn floor_div(a: i32, b: i32) -> i32 {
    return i32(floor(f32(a) / f32(b)));
}

fn push_curve(p: f32) -> f32 {
    return p * (0.6 + 0.4 * p);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    var bpm = s_beat_bpm();
    var phase = s_beat_phase();
    var barpos = s_lfo_bar();
    if bpm < 1.0 {
        bpm = 120.0;
        phase = fract(se.time * 2.0);
        barpos = fract(se.time * 0.5);
    }
    let beat_s = 60.0 / bpm;
    let bi = i32(round(se.time / beat_s - phase));
    let idx = ((i32(round(barpos * 4.0 - phase)) % 4) + 4) % 4;
    let bar = floor_div(bi - idx, 4);

    let nbars = max(p_bars(), 1);
    let in_phrase = bar - nbars * floor_div(bar, nbars);
    let beats = f32(in_phrase * 4 + idx) + phase; // beats since the phrase downbeat
    let p = clamp(beats / f32(nbars * 4), 0.0, 1.0);

    var f = push_curve(p);
    if p_reset() == 0 {
        // carry the finished push and ease it out (cubic) right after the downbeat
        let r = clamp(beats / RESET_BEATS, 0.0, 1.0);
        let eased = 1.0 - (1.0 - r) * (1.0 - r) * (1.0 - r);
        f += 1.0 - eased;
    }
    let z = max(p_zoom() * max(p_amount(), 0.0) * f, 0.0);
    let s = 1.0 / (1.0 + z);

    // drift toward the focus, bounded by the margin the zoom leaves
    let dir = clamp((p_focus() - vec2<f32>(0.5)) * 2.0 * clamp(p_drift(), 0.0, 1.0), vec2<f32>(-1.0), vec2<f32>(1.0));
    let c = vec2<f32>(0.5) + dir * 0.5 * (1.0 - s);
    let uv = c + (in.uv - vec2<f32>(0.5)) * s;
    return textureSampleLevel(se_input, se_sampler, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
}
