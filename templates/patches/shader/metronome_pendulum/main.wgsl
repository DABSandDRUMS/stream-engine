// Metronome pendulum: a classic pyramid metronome (mahogany or Windows 3.1 gray) whose pendulum
// swings exactly to the beat, one side per beat: it reaches an extreme on every beat
// (angle = A * cos(pi * (beat + beat.phase)), beat counted within the bar from lfo.bar) and a
// pixel "tick" spark flashes there; the downbeat tick is bigger and red. The sliding weight sits
// lower for faster tempos, like on the real thing, and the plate under the pivot shows the
// rounded BPM in a bitmap font ("---" and a resting pendulum without a tempo).
// Everything is drawn on an integer grid of "metronome pixels" (`s` screen pixels each); the rod
// and weight are rotated per pixel so their edges stay hard.
// Always drawn (no manifest trigger).

// 5x7 font, one u32 per row (bit 4 = leftmost column): 0 1 2 3 4 5 6 7 8 9 -
const FONT: array<u32, 77> = array<u32, 77>(
    14u, 17u, 19u, 21u, 25u, 17u, 14u, // 0
    4u, 12u, 4u, 4u, 4u, 4u, 14u, // 1
    14u, 17u, 1u, 2u, 4u, 8u, 31u, // 2
    31u, 2u, 4u, 2u, 1u, 17u, 14u, // 3
    2u, 6u, 10u, 18u, 31u, 2u, 2u, // 4
    31u, 16u, 30u, 1u, 1u, 17u, 14u, // 5
    6u, 8u, 16u, 30u, 17u, 17u, 14u, // 6
    31u, 1u, 2u, 4u, 8u, 8u, 8u, // 7
    14u, 17u, 17u, 14u, 17u, 17u, 14u, // 8
    14u, 17u, 17u, 15u, 1u, 2u, 12u, // 9
    0u, 0u, 0u, 31u, 0u, 0u, 0u, // -
);

const BLACK = vec3<f32>(0.0);
const WHITE = vec3<f32>(1.0);
const GRAY = vec3<f32>(0.753);
const DARK = vec3<f32>(0.502);
const NAVY = vec3<f32>(0.0, 0.0, 0.502);

// Layout (metronome pixels, y up from the bottom, x from the centre line).
const SIZE = vec2<i32>(96, 104);
const PLINTH_TOP = 5.0;
const BODY_TOP = 80.0;
const CAP_TOP = 86.0;
const WIN_LO = 28.0;   // scale window (and top of the lower front panel)
const WIN_HI = 76.0;
const PIVOT = vec2<f32>(0.0, 24.0);
const ROD = 68.0;
const SWING = 0.42;    // radians to each side

// Style colours: 0 wood, 1 win31.
struct Look {
    body: vec3<f32>,
    light: vec3<f32>,
    shade: vec3<f32>,
    line: vec3<f32>,
    face: vec3<f32>,
    tick: vec3<f32>,
    rod: vec3<f32>,
    weight: vec3<f32>,
    plate: vec3<f32>,
    digit: vec3<f32>,
};

fn look(style: i32) -> Look {
    if (style == 1) {
        return Look(GRAY, WHITE, DARK, BLACK, WHITE, BLACK, BLACK, GRAY, BLACK, vec3<f32>(0.25, 1.0, 0.3));
    }
    return Look(
        vec3<f32>(0.5, 0.21, 0.09), vec3<f32>(0.78, 0.43, 0.2), vec3<f32>(0.3, 0.11, 0.05), vec3<f32>(0.11, 0.04, 0.02),
        vec3<f32>(0.95, 0.88, 0.68), vec3<f32>(0.35, 0.16, 0.07), vec3<f32>(0.82, 0.82, 0.86),
        vec3<f32>(0.93, 0.72, 0.28), vec3<f32>(0.9, 0.72, 0.32), vec3<f32>(0.12, 0.06, 0.02),
    );
}

fn glyph_on(g: u32, p: vec2<i32>) -> bool {
    if (p.x < 0 || p.x > 4 || p.y < 0 || p.y > 6) {
        return false;
    }
    return ((FONT[g * 7u + u32(p.y)] >> u32(4 - p.x)) & 1u) == 1u;
}

// Half width of the case at height y (plinth, tapering body, little pyramid cap); -1 above it.
fn half_width(y: f32) -> f32 {
    if (y < PLINTH_TOP) { return 28.0; }
    if (y < BODY_TOP) { return mix(24.0, 9.0, (y - PLINTH_TOP) / (BODY_TOP - PLINTH_TOP)); }
    if (y < CAP_TOP) { return mix(9.0, 2.0, (y - BODY_TOP) / (CAP_TOP - BODY_TOP)); }
    return -1.0;
}

// Pendulum (rod, tip knob, sliding weight) at metronome pixel f; alpha 0 = not covered.
fn pendulum(f: vec2<f32>, theta: f32, weight_at: f32, lk: Look, style: i32) -> vec4<f32> {
    if (f.y < WIN_LO) {
        return vec4<f32>(0.0); // pivot and lower rod hidden behind the front panel
    }
    let dir = vec2<f32>(sin(theta), cos(theta));
    let r = f - PIVOT;
    let u = dot(r, dir);
    let v = dot(r, vec2<f32>(dir.y, -dir.x));
    // Weight: a trapezoid slider, wider at the bottom, outlined, lit from the left.
    let wu = u - (weight_at - 4.5);
    if (wu >= 0.0 && wu <= 9.0) {
        let hw = mix(6.5, 4.0, wu / 9.0);
        if (abs(v) < hw) {
            if (abs(v) > hw - 1.0 || wu < 1.0 || wu > 8.0) {
                return vec4<f32>(lk.line, 1.0);
            }
            if (v < -hw + 2.5) { return vec4<f32>(lk.light, 1.0); }
            if (v > hw - 2.5) { return vec4<f32>(select(lk.weight * 0.6, DARK, style == 1), 1.0); }
            return vec4<f32>(lk.weight, 1.0);
        }
    }
    // Tip knob and rod.
    if (u >= ROD - 3.0 && u <= ROD && abs(v) < 1.6) {
        return vec4<f32>(select(lk.weight, NAVY, style == 1), 1.0);
    }
    if (u >= 0.0 && u <= ROD && abs(v) < 1.0) {
        return vec4<f32>(select(lk.rod, lk.rod * 0.7, v > 0.0), 1.0);
    }
    return vec4<f32>(0.0);
}

// Case: plinth, body with bevelled edges (wood grain on the wood style), scale window, BPM plate.
fn case_color(f: vec2<f32>, i: vec2<i32>, lk: Look, style: i32, bpm: i32, flash: f32, down: bool) -> vec4<f32> {
    let hw = half_width(f.y);
    if (hw < 0.0 || abs(f.x) > hw) {
        return vec4<f32>(0.0);
    }
    let ax = abs(f.x);
    if (f.y < PLINTH_TOP) {
        if (ax > hw - 1.0 || f.y < 1.0) { return vec4<f32>(lk.line, 1.0); }
        if (f.y > PLINTH_TOP - 1.0) { return vec4<f32>(lk.light, 1.0); }
        return vec4<f32>(lk.shade, 1.0);
    }
    if (ax > hw - 1.0 || f.y > CAP_TOP - 1.0) {
        return vec4<f32>(lk.line, 1.0);
    }
    var col = lk.body;
    if (style == 0) {
        // Vertical mahogany grain, two tones with a 2x2 ordered dither.
        let g = 0.5 + 0.5 * sin(f.x * 0.85 + 2.2 * sin(f.y * 0.06 + f.x * 0.11) + 0.7 * sin(f.y * 0.21));
        var bayer = array<f32, 4>(0.125, 0.625, 0.875, 0.375);
        let th = bayer[(i.x & 1) + 2 * (i.y & 1)];
        col = select(lk.body, lk.body * 1.18, g > th);
    }
    if (f.x < 0.0 && ax > hw - 3.0) { col = lk.light; }
    if (f.x > 0.0 && ax > hw - 3.0) { col = lk.shade; }
    if (f.y >= BODY_TOP) {
        return vec4<f32>(select(col, lk.light, f.x < 0.0 && f.y < BODY_TOP + 1.0), 1.0);
    }
    // Scale window: sunken, light face with tempo ticks (longer every 4th) on the centre line.
    let wh = hw - 6.0;
    if (f.y >= WIN_LO && f.y < WIN_HI && ax < wh) {
        if (ax > wh - 1.0 || f.y > WIN_HI - 1.0) {
            return vec4<f32>(select(lk.shade, lk.line, f.x < 0.0 || f.y > WIN_HI - 1.0), 1.0);
        }
        let row = i32(f.y - WIN_LO);
        let tick_w = select(1.5, 3.5, row % 12 == 4);
        if (row % 4 == 0 && row > 0 && ax < tick_w) {
            return vec4<f32>(lk.tick, 1.0);
        }
        return vec4<f32>(lk.face, 1.0);
    }
    // BPM plate under the pivot: sunken, three bitmap digits; lights up with the tick.
    if (f.y >= 9.0 && f.y < 20.0 && ax < 12.0) {
        if (ax > 11.0 || f.y < 10.0 || f.y > 19.0) {
            return vec4<f32>(select(lk.line, lk.light, f.x > 0.0 || f.y < 10.0), 1.0);
        }
        let g = vec2<i32>(i.x + 9, 17 - i.y);
        let k = g.x / 6;
        // Win 3.1 readout flashes white-yellow; on wood the engraving only turns red on the downbeat.
        let digit_flash = select(select(lk.digit, vec3<f32>(0.8, 0.0, 0.0), down), vec3<f32>(1.0, 1.0, 0.8), style == 1);
        if (g.x >= 0 && k < 3) {
            var d = 10u;
            if (bpm > 0) {
                let div = select(select(1, 10, k == 1), 100, k == 0);
                d = u32((bpm / div) % 10);
            }
            if (glyph_on(d, vec2<i32>(g.x - k * 6, g.y))) {
                return vec4<f32>(mix(lk.digit, digit_flash, flash), 1.0);
            }
        }
        let plate = select(lk.plate * (1.0 + 0.25 * flash), lk.plate, style == 1);
        return vec4<f32>(plate, 1.0);
    }
    // Pivot cover screw.
    if (length(f - PIVOT) < 2.2) {
        return vec4<f32>(select(lk.weight, DARK, style == 1), 1.0);
    }
    return vec4<f32>(col, 1.0);
}

// Comic "tick" spark: three rays bursting outward from the extreme the rod just hit. They fly out
// and shrink over `dur` seconds (the downbeat burst is longer, bigger and red).
fn spark(f: vec2<f32>, side: f32, age: f32, down: bool) -> vec4<f32> {
    let dur = select(0.12, 0.2, down);
    if (age >= dur) {
        return vec4<f32>(0.0);
    }
    let t = age / dur;
    let tip = PIVOT + ROD * vec2<f32>(side * sin(SWING), cos(SWING));
    let r = (f - tip) * vec2<f32>(side, 1.0); // mirror so +x is always "outward"
    let start = 3.0 + select(6.0, 9.0, down) * t;
    let reach = select(4.0, 7.0, down) * (1.0 - t) + 1.0;
    let len = length(r);
    if (len < start || len > start + reach || r.x < 0.0) {
        return vec4<f32>(0.0);
    }
    let a = atan2(r.y, r.x);
    var hit = false;
    for (var k = -1; k <= 1; k++) {
        let ray = f32(k) * 0.75;
        let d = len * abs(sin(a - ray));
        hit = hit || (d < 0.75 && abs(a - ray) < 0.6);
    }
    if (!hit) {
        return vec4<f32>(0.0);
    }
    let col = select(vec3<f32>(1.0, 0.95, 0.6), vec3<f32>(1.0, 0.18, 0.1), down);
    return vec4<f32>(col, 1.0);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let s = max(1.0, round(se.resolution.y / 360.0 * p_scale()));
    let size_px = vec2<f32>(SIZE) * s;
    let center = p_position() * se.resolution;
    let origin = floor(clamp(center - size_px * 0.5, vec2<f32>(0.0), max(se.resolution - size_px, vec2<f32>(0.0))));
    let p = vec2<i32>(floor((in.uv * se.resolution - origin) / s));
    if (any(p < vec2<i32>(0)) || any(p >= SIZE)) {
        return vec4<f32>(0.0);
    }
    // Centre-line coordinates: i integer (x from the centre, y up), f = pixel centre.
    let i = vec2<i32>(p.x - SIZE.x / 2, SIZE.y - 1 - p.y);
    let f = vec2<f32>(i) + vec2<f32>(0.5);
    let style = clamp(p_style(), 0, 1);
    let lk = look(style);

    let bpm_f = s_beat_bpm();
    let running = bpm_f >= 1.0;
    let bpm = i32(round(bpm_f));
    let phase = fract(s_beat_phase());
    let beat = i32(round(fract(s_lfo_bar()) * 4.0 - phase)) & 3;
    // One side per beat: extreme on every beat, through the middle half way.
    let theta = select(0.0, SWING * cos(3.14159265 * (f32(beat) + phase)), running);
    let side = select(-1.0, 1.0, (beat & 1) == 0);
    let down = beat == 0;
    let age = phase * 60.0 / max(bpm_f, 40.0);
    let flash = select(0.0, exp(-age * select(14.0, 9.0, down)), running);
    let weight_at = mix(60.0, 36.0, clamp((bpm_f - 40.0) / 170.0, 0.0, 1.0));

    let sp = spark(f, side, select(1e3, age, running), down);
    if (sp.a > 0.0) {
        return sp;
    }
    let pen = pendulum(f, theta, weight_at, lk, style);
    if (pen.a > 0.0) {
        return pen;
    }
    return case_color(f, i, lk, style, select(-1, bpm, running), flash, down);
}
