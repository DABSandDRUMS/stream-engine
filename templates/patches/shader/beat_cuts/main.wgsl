// Beat Cuts: an automatic editor that hard-cuts between crop framings of the input on beats.
//
// There is no frame state, so everything is a pure function of the clock:
// - beat index = round(se.time × bpm / 60 − beat.phase) (stable tempo assumed), beat in bar from
//   lfo.bar, bar index = (beat index − beat in bar) / 4, which only steps on a downbeat even when
//   the two clocks drift apart;
// - the framing of a segment (bar / half bar / beat) comes from a hashed sequence that never
//   shows the same framing twice in a row;
// - a trigger makes every beat a cut (cut-away on even beats, back on odd ones) for
//   `trigger_beats` beats, counted in whole beats from the trigger to the current beat's start
//   (trigger_age − time into the beat is constant within a beat);
// - fills: band.snare decays as exp(−20 t), so −ln(snare)/20 is the time since the last snare hit.
//   Beat 3 cuts away while the last hit is inside this beat or within 0.3 beat before it (that
//   only switches on at a snare hit and never off mid-beat), beat 4 cuts back. Backbeats on 2 and
//   4 never count, so a normal groove keeps one cut per bar.
// Crops scale uv uniformly (aspect kept) and their center is clamped so the window always lies
// inside the picture. Input and output are premultiplied alpha.

const SNARE_DECAY: f32 = 20.0;
const FILL_WINDOW: f32 = 0.3;

fn hash(x: u32) -> u32 {
    var h = x * 747796405u + 2891336453u;
    h = ((h >> ((h >> 28u) + 4u)) ^ h) * 277803737u;
    return (h >> 22u) ^ h;
}

fn floor_div(a: i32, b: i32) -> i32 {
    return i32(floor(f32(a) / f32(b)));
}

fn shot(i: i32) -> vec4<f32> {
    switch i {
        case 1: { return p_shot_snare(); }
        case 2: { return p_shot_kick(); }
        case 3: { return p_shot_cymbals(); }
        default: { return p_shot_wide(); }
    }
}

// Order of the n framings inside block `b`: a rotation (forward or backward) from a hashed start.
fn perm_at(b: i32, pos: i32, n: i32, seed: u32) -> i32 {
    let h = hash(u32(b + 1000000) ^ seed);
    let a = i32(h % u32(n));
    let step = select(1, n - 1, (h & 0x100u) != 0u);
    return (a + step * pos) % n;
}

// Framing for segment k: one shuffled block of n per n segments, never the same twice in a row
// (if a block would start with the previous block's last framing, its first two are swapped;
// that keeps the block's own last element, so the rule never chains).
fn seq(k: i32, n: i32, seed: u32) -> i32 {
    if n <= 2 {
        return (k + i32(seed & 1u)) & 1;
    }
    let b = floor_div(k, n);
    var pos = k - b * n;
    let prev_last = perm_at(b - 1, n - 1, n, seed);
    if perm_at(b, 0, n, seed) == prev_last {
        if pos == 0 {
            pos = 1;
        } else if pos == 1 {
            pos = 0;
        }
    }
    return perm_at(b, pos, n, seed);
}

struct Clock {
    bi: i32,  // beat index
    idx: i32, // beat in bar, 0-3
    bar: i32, // bar index
    n: i32,
    mode: i32,
}

// Normal framing of the beat `o` beats away from the current one (segment = bar, beat or half bar).
fn base_at(k: Clock, o: i32) -> i32 {
    let i = k.idx + o;
    let b = k.bar + floor_div(i, 4);
    let ib = i - 4 * floor_div(i, 4);
    if k.mode == 1 {
        return seq(k.bi + o, k.n, 0x51c3u);
    }
    if k.mode == 2 {
        return seq(b * 2 + ib / 2, k.n, 0x2d97u);
    }
    return seq(b, k.n, 0x9e37u);
}

// Cut-away framing for the beat `o` beats away: differs from the normal framing of that beat and
// of both its neighbours, so cutting to it and back are both real cuts.
fn alt_at(k: Clock, o: i32) -> i32 {
    let b0 = base_at(k, o - 1);
    let b1 = base_at(k, o);
    let b2 = base_at(k, o + 1);
    let h = i32(hash(u32(k.bi + o + 1000000) ^ 0x3c6eu) % u32(k.n));
    for (var j = 0; j < k.n; j++) {
        let c = (h + j) % k.n;
        if c != b0 && c != b1 && c != b2 {
            return c;
        }
    }
    return b1;
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let n = clamp(p_shots(), 2, 4);
    let mode = p_mode();

    // clock (falls back to 120 BPM on se.time with no tempo)
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
    let into = phase * beat_s;

    // trigger window, in whole beats from the last trigger to this beat's start (d, constant
    // within a beat; a trigger right on a beat counts for that beat). d = −1: the trigger landed
    // inside this beat; counted as active so a stream of triggers (chat) keeps a steady cut every
    // beat, at the cost that a lone trigger mid-beat may cut right at the trigger.
    let d = floor((se.trigger_age - into) / beat_s + 0.02);
    let nb = f32(max(p_trigger_beats(), 0));
    let trig = nb > 0.0 && d >= -1.0 && d < nb;
    let trig_prev = nb > 0.0 && d >= 0.0 && d - 1.0 < nb;

    // inside the window even beats cut away and odd beats cut back to the normal framing, so
    // every beat is a cut
    let k = Clock(bi, idx, bar, n, mode);
    var alt = trig && (bi & 1) == 0;
    let prev_alt = trig_prev && (bi & 1) == 1;

    // fill cut-away on beat 3 (beat 4 is a normal beat, so it cuts back)
    let snare = s_band_snare();
    if p_fill_cuts() && idx == 2 && mode != 1 && !trig && snare > 1e-5 {
        let since = -log(snare) / SNARE_DECAY;
        let q = (since - into) / beat_s; // beats before this beat's start (< 0: inside it)
        alt = q < FILL_WINDOW;
    }
    let cur = select(base_at(k, 0), alt_at(k, 0), alt);
    let prev = select(base_at(k, -1), alt_at(k, -1), prev_alt);

    // window: uniform scale (aspect kept) that contains the framing rect, eased toward the full
    // frame by amount, plus a small push that settles after a cut
    let amount = clamp(p_amount(), 0.0, 1.0);
    let r = shot(cur);
    let size = clamp(max(r.z, r.w), 0.1, 1.0);
    var s = mix(1.0, size, amount);
    var c = mix(vec2<f32>(0.5), r.xy + 0.5 * r.zw, amount);
    if cur != prev {
        s = s / (1.0 + max(p_settle(), 0.0) * exp(-into * 9.0));
    }
    c = clamp(c, vec2<f32>(0.5 * s), vec2<f32>(1.0 - 0.5 * s));
    let uv = c + (in.uv - vec2<f32>(0.5)) * s;

    let col = textureSampleLevel(se_input, se_sampler, uv, 0.0);

    // unsharp mask, stronger the more the crop is magnified
    let sharp = clamp(p_sharpen(), 0.0, 1.0) * clamp(1.0 / s - 1.0, 0.0, 1.5) * 0.6;
    if sharp <= 0.0 {
        return col;
    }
    let texel = 1.0 / vec2<f32>(textureDimensions(se_input));
    let blur = 0.25 * (textureSampleLevel(se_input, se_sampler, uv + vec2<f32>(texel.x, 0.0), 0.0)
        + textureSampleLevel(se_input, se_sampler, uv - vec2<f32>(texel.x, 0.0), 0.0)
        + textureSampleLevel(se_input, se_sampler, uv + vec2<f32>(0.0, texel.y), 0.0)
        + textureSampleLevel(se_input, se_sampler, uv - vec2<f32>(0.0, texel.y), 0.0));
    let rgb = clamp(col.rgb + sharp * (col.rgb - blur.rgb), vec3<f32>(0.0), vec3<f32>(col.a));
    return vec4<f32>(rgb, col.a);
}
