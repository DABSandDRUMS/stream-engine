// Groove Meter: a Windows 3.1 instrument window with an analog VU-style gauge. Everything is
// drawn on an integer grid of "gauge pixels" (`s` screen pixels each), so edges stay hard.
//
// Groove lock (an approximation; there is no hit history): hit envelopes jump to the hit strength
// and decay as exp(-20 t), so `-ln(env) / 20` is the time since the latest hit and
// `beat.phase - that / beat` is where on the beat that hit landed. Its distance to the nearest
// grid line (8ths for the kick, 16ths for the snare) is the timing error of that one hit:
// 0-10 ms = fully locked, 55 ms+ (25 ms for the snare's finer grid) = loose. The needle shows the
// latest kick's and snare's lock (each weighted by how recent it is), so it reads "how tight the
// last couple of hits were", not a long-term average. Soft hits read ~10-30 ms early (their
// envelope starts lower), so ghost-note playing reads a bit looser than it is; beat-tracker phase
// error shifts everything equally. Lock is mixed with band.level (playing harder = higher), and
// the needle falls to rest when the band stops.
// Fills: snare-band hits off the quarter-note grid with a grid-estimated strength over ~0.4
// (strength = env * exp(20 * time since the 16th it landed on)) count as fill notes; they push
// the needle into the red zone and blink the redline, lamp and caption on the 16ths.

// Bold-ready 5x7 font (from fill_dialog): G R O V E; bits row*7+col, x: rows 0-3, y: rows 4-7.
const GLYPHS = array<vec2<u32>, 5>(
    vec2<u32>(0x3a0488eu, 0x78891u), // 'G'
    vec2<u32>(0x1e4488fu, 0x44485u), // 'R'
    vec2<u32>(0x224488eu, 0x38891u), // 'O'
    vec2<u32>(0x2244891u, 0x10511u), // 'V'
    vec2<u32>(0x1e0409fu, 0x7c081u), // 'E'
);
// "GROOVE"
const TEXT = array<u32, 6>(0u, 1u, 2u, 2u, 3u, 4u);

const BLACK = vec3<f32>(0.0);
const WHITE = vec3<f32>(1.0);
const GRAY = vec3<f32>(0.753);
const DARK = vec3<f32>(0.502);
const FACE = vec3<f32>(0.99, 0.96, 0.84);
const RED = vec3<f32>(0.9, 0.06, 0.04);

const W = 118;
const H = 94;
const TITLE = 12;          // caption rows 1..12, black rule at 13
const PIVOT = vec2<f32>(59.0, 81.0);
const R_ARC = 48.0;
const SWEEP = 0.8;         // half the scale angle (radians)
const RED_FROM = 0.78;     // red zone starts here (0-1 of the scale)
const DECAY: f32 = 20.0;

fn glyph_on(g: u32, x: i32, y: i32) -> bool {
    if (x < 0 || x >= 5 || y < 0 || y > 7) {
        return false;
    }
    let bit = u32(y * 7 + x);
    let word = GLYPHS[g];
    return select(((word.y >> (bit - 28u)) & 1u) == 1u, ((word.x >> bit) & 1u) == 1u, bit < 28u);
}

// "GROOVE" in bold System style (each lit column doubled to the right), advance 7.
fn caption_on(p: vec2<i32>) -> bool {
    if (p.y < 0 || p.y > 7 || p.x < 0 || p.x >= 6 * 7) {
        return false;
    }
    let i = p.x / 7;
    let lx = p.x - i * 7;
    let g = TEXT[i];
    return glyph_on(g, lx, p.y) || glyph_on(g, lx - 1, p.y);
}

fn since_hit(env: f32) -> f32 {
    return -log(max(env, 1e-6)) / DECAY;
}

// Damped needle twitch from a hit envelope (0 at the hit, swings and settles), as in window_boing.
fn twitch(env: f32) -> f32 {
    if (env < 0.003) {
        return 0.0;
    }
    let u = -log(min(env, 1.0));
    return sin(1.8 * u) * exp(-0.45 * u) / 0.7;
}

struct Meter {
    needle: f32,   // 0-1 of the scale
    fill: f32,     // 0-1, fill notes happening
    blink: bool,   // redline blink phase
    level: f32,    // 0-1 LED bar
};

fn meter() -> Meter {
    var m: Meter;
    let kick = clamp(s_band_kick(), 0.0, 1.0);
    let snare = clamp(s_band_snare(), 0.0, 1.0);
    let level = clamp(s_band_level(), 0.0, 1.0);
    let bpm = s_beat_bpm();
    let tol = clamp(p_tolerance(), 0.2, 4.0);
    let presence = smoothstep(0.05, 0.2, level);

    var lock = 0.55;
    var fill = 0.0;
    if (bpm > 1.0) {
        let beat_s = 60.0 / bpm;
        let ph = s_beat_phase();
        // kick: error to the nearest 8th (in beats)
        let pk = ph - since_hit(kick) / beat_s;
        let ek = abs(pk * 2.0 - round(pk * 2.0)) * 0.5;
        let lk = 1.0 - smoothstep(0.02 * tol, 0.11 * tol, ek);
        let wk = smoothstep(1e-5, 2e-3, kick);
        // snare: error to the nearest 16th
        let ps = ph - since_hit(snare) / beat_s;
        let es = abs(ps * 4.0 - round(ps * 4.0)) * 0.25;
        let ls = 1.0 - smoothstep(0.012 * tol, 0.05 * tol, es);
        let ws = smoothstep(1e-5, 2e-3, snare);
        lock = (wk * lk + ws * ls + 0.3 * 0.55) / (wk + ws + 0.3);

        // fill notes: hard-ish snare-band hits off the quarter grid, or on beats 1/3 of the bar
        // (where a backbeat snare doesn't go)
        let g16 = round(ps * 4.0) * 0.25;
        let t16 = clamp((ph - g16) * beat_s, 0.0, since_hit(snare));
        let strength = snare * exp(DECAY * t16);
        let bar_hit = fract(s_lfo_bar() - t16 / (4.0 * beat_s) + 2.0);
        let quarter = i32(round(bar_hit * 4.0)) % 4;
        let off_q = max(smoothstep(0.08, 0.15, abs(g16 - round(g16))), select(0.0, 1.0, quarter == 0 || quarter == 2));
        fill = off_q * smoothstep(0.3, 0.5, strength) * smoothstep(0.002, 0.03, snare);
        m.blink = fract(ph * 4.0) < 0.5;
    } else {
        m.blink = fract(se.time * 8.0) < 0.5;
    }

    var needle = presence * (0.5 * lock + 0.24 * smoothstep(0.15, 0.8, level));
    needle += 0.035 * (twitch(kick) + 0.6 * twitch(snare)) * presence;
    needle = mix(needle, 0.9 + 0.05 * sin(se.time * 31.0), fill * 0.9);

    // trigger (alert): the needle pegs, rattles a little and drops back
    let age = se.trigger_age;
    let peg = smoothstep(0.0, 0.12, age) * exp(-1.6 * max(age - 0.5, 0.0));
    needle = mix(needle, 1.0 + 0.03 * sin(age * 40.0) * exp(-3.0 * age), peg);
    fill = max(fill, peg);

    m.needle = clamp(needle, -0.02, 1.04);
    m.fill = fill;
    m.level = level;
    return m;
}

fn in_box(p: vec2<i32>, lo: vec2<i32>, size: vec2<i32>) -> bool {
    return all(p >= lo) && all(p < lo + size);
}

// The gauge face (inside the sunken frame), at gauge pixel p.
fn face(p: vec2<i32>, m: Meter) -> vec3<f32> {
    let flash = m.fill > 0.3 && m.blink;
    let q = vec2<f32>(p) + vec2<f32>(0.5);
    let d = q - PIVOT;
    let r = length(d);
    let ang = atan2(d.x, -d.y);           // 0 = straight up, + = right
    let v = (ang + SWEEP) / (2.0 * SWEEP); // 0-1 along the scale

    // needle (with a 1-pixel drop shadow), from the housing to just past the arc
    let na = -SWEEP + 2.0 * SWEEP * m.needle;
    let nd = vec2<f32>(sin(na), -cos(na));
    let along = dot(d, nd);
    let across = abs(d.x * nd.y - d.y * nd.x);
    if (along > 10.0 && along < R_ARC + 4.0 && across < 0.6) {
        return select(BLACK, vec3<f32>(0.75, 0.0, 0.0), m.needle > RED_FROM + 0.02);
    }
    // housing at the pivot
    if (r < 11.5) {
        return select(DARK, BLACK, r > 10.5);
    }
    let ds = d - vec2<f32>(1.0, 1.0);
    let s_along = dot(ds, nd);
    let s_across = abs(ds.x * nd.y - ds.y * nd.x);
    let shadow = s_along > 10.0 && s_along < R_ARC + 3.0 && s_across < 0.6;

    var col = FACE;
    if (flash) {
        col = mix(FACE, vec3<f32>(1.0, 0.82, 0.78), 0.6);
    }
    if (v >= 0.0 && v <= 1.0) {
        // red zone band outside the arc
        if (v >= RED_FROM && r >= R_ARC + 1.0 && r < R_ARC + 5.0) {
            return select(vec3<f32>(0.86, 0.32, 0.28), vec3<f32>(1.0, 0.08, 0.05), m.fill > 0.3 && (flash || m.fill > 0.95));
        }
        // scale arc
        if (abs(r - R_ARC) < 0.5) {
            return select(BLACK, RED, v >= RED_FROM);
        }
        // ticks: 11, majors at 0, 0.5, 1
        let ti = round(v * 10.0);
        let major = ti == 0.0 || ti == 5.0 || ti == 10.0;
        let off = abs(v * 10.0 - ti) / 10.0 * 2.0 * SWEEP * r;
        let len = select(3.0, 6.0, major);
        if (off < select(0.5, 0.75, major) && r < R_ARC && r > R_ARC - len) {
            return select(BLACK, RED, ti >= RED_FROM * 10.0);
        }
    }
    // tiny "-" and "+" marks under the scale ends
    let mm = p - vec2<i32>(22, 52);
    if (mm.y == 0 && mm.x >= 0 && mm.x < 5) {
        return BLACK;
    }
    let pp = p - vec2<i32>(92, 50);
    if ((pp.y == 2 && pp.x >= 0 && pp.x < 5) || (pp.x == 2 && pp.y >= 0 && pp.y < 5)) {
        return BLACK;
    }
    if (shadow) {
        return col * 0.78;
    }
    return col;
}

fn window(p: vec2<i32>, m: Meter) -> vec3<f32> {
    // 1-pixel black frame
    if (p.x == 0 || p.y == 0 || p.x == W - 1 || p.y == H - 1) {
        return BLACK;
    }
    // caption bar
    if (p.y <= TITLE) {
        // system menu box
        if (p.x <= 12) {
            if (p.x == 12) { return BLACK; }
            if (p.x >= 3 && p.x < 10 && p.y == 6) { return WHITE; }
            if (p.x >= 2 && p.x < 11 && p.y >= 5 && p.y < 8) { return BLACK; }
            return GRAY;
        }
        let tc = p_title_color().rgb;
        let flash = m.fill > 0.3 && m.blink;
        let bar = select(tc, RED, flash);
        let ink = select(BLACK, WHITE, dot(bar, vec3<f32>(0.299, 0.587, 0.114)) < 0.5);
        let tx = 13 + (W - 14 - 41) / 2;
        if (caption_on(p - vec2<i32>(tx, 3))) {
            return ink;
        }
        return bar;
    }
    if (p.y == TITLE + 1) {
        return BLACK;
    }
    // sunken face frame
    let f0 = vec2<i32>(5, 18);
    let f1 = vec2<i32>(W - 6, 75);
    if (in_box(p, f0, f1 - f0 + 1)) {
        if (p.x == f0.x || p.y == f0.y) { return DARK; }
        if (p.x == f1.x || p.y == f1.y) { return WHITE; }
        return face(p, m);
    }
    // level LED bar: 12 segments, green -> yellow -> red
    let lb = p - vec2<i32>(6, 81);
    if (lb.y >= 0 && lb.y < 6 && lb.x >= 0 && lb.x < 12 * 6) {
        let seg = lb.x / 6;
        if (lb.x - seg * 6 == 5) { return GRAY; }
        if (lb.y == 0 || lb.x - seg * 6 == 0) { return DARK; }
        let lit = f32(seg) < m.level * 12.0;
        let c = select(select(vec3<f32>(0.0, 0.85, 0.1), vec3<f32>(1.0, 0.85, 0.0), seg >= 8), RED, seg >= 10);
        return select(c * 0.28 + GRAY * 0.2, c, lit);
    }
    // fill lamp: round red LED with a highlight
    let lc = vec2<f32>(p) + vec2<f32>(0.5) - vec2<f32>(f32(W) - 15.0, 84.0);
    let lr = length(lc);
    if (lr < 5.0) {
        if (lr > 4.0) { return BLACK; }
        let on = m.fill > 0.3 && (m.blink || m.fill > 0.95);
        let hl = lc.x < -0.5 && lc.y < -0.5 && lr < 2.5;
        return select(select(vec3<f32>(0.35, 0.05, 0.05), RED, on), select(vec3<f32>(0.6, 0.4, 0.4), WHITE, on), hl);
    }
    // panel bevel: white top/left, dark bottom/right
    if (p.x == 1 || p.y == TITLE + 2) { return WHITE; }
    if (p.x == W - 2 || p.y == H - 2) { return DARK; }
    return GRAY;
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let s = max(1.0, round(se.resolution.y / 360.0 * p_scale()));
    let size_px = vec2<f32>(f32(W), f32(H)) * s;
    let corner = clamp(p_corner(), 0, 3);
    let right = corner == 1 || corner == 3;
    let top = corner >= 2;
    let margin = p_margin() * se.resolution;
    let ox = select(margin.x, se.resolution.x - margin.x - size_px.x, right);
    let oy = select(se.resolution.y - margin.y - size_px.y, margin.y, top);
    let origin = floor(vec2<f32>(ox, oy));
    let p = vec2<i32>(floor((in.uv * se.resolution - origin) / s));
    if (any(p < vec2<i32>(0)) || any(p >= vec2<i32>(W, H))) {
        return vec4<f32>(0.0);
    }
    return vec4<f32>(window(p, meter()), 1.0);
}
