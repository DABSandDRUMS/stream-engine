// Polyrhythm dots: A dots against B dots (default 3:2) spread evenly over one bar and locked to
// lfo.bar (re-aligned to beat.phase so the bar wraps exactly on a beat). Each dot pops and lights
// as the bar passes it and stays "selected" until the next one; a playhead (rows) or clock hand
// (rings) sweeps the bar. Where both rhythms land together (always the downbeat, plus any shared
// subdivisions such as the half bar in 4:2) a gold connector joins the rows; it flashes when hit,
// brightest on the downbeat. Rows layout has a 16th-note ruler between the rows.
// Styles: a Windows 3.1 window with radio-button dots, or a neon panel with glowing dots.
// Everything is drawn on an integer grid of "dialog pixels" (`s` screen pixels each).
// Always drawn (no manifest trigger).

// 5x7 font, one u32 per row (bit 4 = leftmost column): 0 1 2 3 4 5 6 7 8 9 : P O L Y
const FONT: array<u32, 105> = array<u32, 105>(
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
    0u, 4u, 4u, 0u, 4u, 4u, 0u, // :
    30u, 17u, 17u, 30u, 16u, 16u, 16u, // P
    14u, 17u, 17u, 17u, 17u, 17u, 14u, // O
    16u, 16u, 16u, 16u, 16u, 16u, 31u, // L
    17u, 17u, 10u, 4u, 4u, 4u, 4u, // Y
);
const G_COLON = 10u;

const BLACK = vec3<f32>(0.0);
const WHITE = vec3<f32>(1.0);
const GRAY = vec3<f32>(0.753);
const DARK = vec3<f32>(0.502);
const GOLD = vec3<f32>(1.0, 0.8, 0.1);

const BORDER = 4;
const CAPTION = 11;
// Rows layout: client 150x44; labels in the first 24 columns, dots from x 29 over TRACK.
const ROWS_CLIENT = vec2<i32>(150, 44);
const TRACK_X = 29.0;
const TRACK = 112.0;
const ROW_A = 11.0;
const ROW_B = 33.0;
// Rings layout: client 96x92, outer ring A, inner ring B.
const RINGS_CLIENT = vec2<i32>(96, 92);
const RING_C = vec2<f32>(48.0, 46.0);
const RING_A = 38.0;
const RING_B = 22.0;
const DOT_R = 4.5;
const TAU = 6.2831853;

struct Look {
    style: i32,
    a: vec3<f32>,
    b: vec3<f32>,
    bg: vec3<f32>,
    bg_alpha: f32,
    line: vec3<f32>,
};

fn glyph_on(g: u32, p: vec2<i32>) -> bool {
    if (g == 255u || p.x < 0 || p.x > 4 || p.y < 0 || p.y > 6) {
        return false;
    }
    return ((FONT[g * 7u + u32(p.y)] >> u32(4 - p.x)) & 1u) == 1u;
}

fn digits(n: i32) -> i32 {
    return select(1, 2, n >= 10);
}

// Glyph k of the caption "POLY a:b".
fn caption_glyph(k: i32, a: i32, b: i32) -> u32 {
    if (k < 4) { return 11u + u32(k); }
    if (k == 4) { return 255u; }
    let da = digits(a);
    let j = k - 5;
    if (j < da) { return u32(select(a, select(a % 10, a / 10, j == 0), da == 2)); }
    if (j == da) { return G_COLON; }
    let m = j - da - 1;
    return u32(select(b, select(b % 10, b / 10, m == 0), digits(b) == 2));
}

// Number n (1-2 digits) at double size, top-left at p.
fn big_number(n: i32, p: vec2<i32>) -> bool {
    let q = p / 2;
    if (p.x < 0 || p.y < 0) {
        return false;
    }
    if (n >= 10) {
        return glyph_on(u32(n / 10), q) || glyph_on(u32(n % 10), q - vec2<i32>(6, 0));
    }
    return glyph_on(u32(n), q);
}

// One dot at distance d from its centre: pops on its hit (`pulse`), stays selected while
// `current`. Returns straight colour + coverage.
fn dot_px(d: f32, pulse: f32, current: bool, col: vec3<f32>, lk: Look) -> vec4<f32> {
    let r = DOT_R + 1.5 * pulse;
    let held = select(0.0, 0.35, current);
    if (lk.style == 1) {
        if (d < r) {
            if (d > r - 1.0) { return vec4<f32>(col, 1.0); }
            let lit = max(pulse, held);
            return vec4<f32>(mix(col * 0.12, mix(col, WHITE, 0.5 * pulse), lit), 1.0);
        }
        let halo = max(pulse, held * 0.5) * (1.0 - (d - r) / 4.0);
        if (halo > 0.0) {
            return vec4<f32>(col, 0.65 * halo);
        }
        return vec4<f32>(0.0);
    }
    // Win 3.1 radio button: black ring, white fill, black centre dot when selected.
    if (d >= r) {
        return vec4<f32>(0.0);
    }
    if (d > r - 1.0) {
        return vec4<f32>(BLACK, 1.0);
    }
    if (current && d < 1.8) {
        return vec4<f32>(BLACK, 1.0);
    }
    return vec4<f32>(mix(WHITE, col, max(pulse, held)), 1.0);
}

// Seconds since dot i of n was hit, at bar position `bar` (0-1), with `bar_s` seconds per bar.
fn age_s(i: i32, n: i32, bar: f32, bar_s: f32) -> f32 {
    return fract(bar - f32(i) / f32(n) + 1e-5) * bar_s;
}

fn pulse_of(age: f32) -> f32 {
    return exp(-age * 9.0);
}

// Connector strength where A dot ia coincides with a B dot (0 if they do not coincide).
fn shared_flash(ia: i32, a: i32, b: i32, bar: f32, bar_s: f32) -> f32 {
    if ((ia * b) % a != 0) {
        return -1.0;
    }
    let p = pulse_of(age_s(ia, a, bar, bar_s));
    return select(p * 0.8, p, ia == 0);
}

fn over(top: vec4<f32>, under: vec3<f32>) -> vec3<f32> {
    return mix(under, top.rgb, top.a);
}

fn rows(c: vec2<i32>, a: i32, b: i32, bar: f32, bar_s: f32, lk: Look) -> vec4<f32> {
    let f = vec2<f32>(c) + 0.5;
    let bg = vec4<f32>(lk.bg, lk.bg_alpha);
    // labels
    if (big_number(a, c - vec2<i32>(4, 4)) ) { return vec4<f32>(lk.a, 1.0); }
    if (big_number(b, c - vec2<i32>(4, 26))) { return vec4<f32>(lk.b, 1.0); }
    let x = f.x - TRACK_X;
    // dots, nearest on each row
    let ia = (i32(round(x / TRACK * f32(a))) % a + a) % a;
    let ib = (i32(round(x / TRACK * f32(b))) % b + b) % b;
    let xa = f32(ia) / f32(a) * TRACK;
    let xb = f32(ib) / f32(b) * TRACK;
    let cur_a = i32(floor(bar * f32(a)));
    let cur_b = i32(floor(bar * f32(b)));
    let da = dot_px(length(vec2<f32>(x - xa, f.y - ROW_A)), pulse_of(age_s(ia, a, bar, bar_s)), ia == cur_a, lk.a, lk);
    let db = dot_px(length(vec2<f32>(x - xb, f.y - ROW_B)), pulse_of(age_s(ib, b, bar, bar_s)), ib == cur_b, lk.b, lk);
    let d = select(db, da, f.y < 22.0);
    if (d.a >= 1.0) {
        return d;
    }
    var col = bg.rgb;
    var alpha = bg.a;
    if (x > -6.0 && x < TRACK + 2.0) {
        // track grooves
        if (f.y == ROW_A + 0.5 || f.y == ROW_B + 0.5) {
            col = select(DARK, lk.line * 0.4, lk.style == 1);
        }
        // 16th-note ruler between the rows: quarter ticks taller
        let sx = x / TRACK * 16.0;
        let tick = abs(sx - round(sx)) * TRACK / 16.0 < 0.5 && x >= 0.0 && x < TRACK;
        let quarter = i32(round(sx)) % 4 == 0;
        if (tick && abs(f.y - 22.0) < select(1.0, 3.0, quarter)) {
            col = select(BLACK, lk.line * select(0.5, 0.9, quarter), lk.style == 1);
            alpha = 1.0;
        }
        // connectors where both rhythms land together
        let sf = shared_flash(ia, a, b, bar, bar_s);
        if (sf >= 0.0 && abs(x - xa) < 1.0 && f.y > ROW_A && f.y < ROW_B) {
            let base = select(0.35, 0.3, lk.style == 1);
            col = mix(select(DARK, GOLD * 0.35, lk.style == 1), mix(GOLD, WHITE, 0.4 * sf), max(sf, base));
            alpha = 1.0;
        }
        // playhead
        let ph = bar * TRACK;
        if (abs(x - ph) < 0.5 && f.y > 2.0 && f.y < 42.0) {
            col = select(BLACK, WHITE, lk.style == 1);
            alpha = 1.0;
        }
    }
    if (d.a > 0.0) {
        col = mix(col, d.rgb, d.a);
        alpha = max(alpha, d.a);
    }
    return vec4<f32>(col, alpha);
}

fn rings(c: vec2<i32>, a: i32, b: i32, bar: f32, bar_s: f32, lk: Look) -> vec4<f32> {
    let f = vec2<f32>(c) + 0.5 - RING_C;
    let r = length(f);
    let u = fract(atan2(f.x, -f.y) / TAU + 1.0); // 0 at the top, clockwise
    let ia = i32(round(u * f32(a))) % a;
    let ib = i32(round(u * f32(b))) % b;
    let pa = RING_A * vec2<f32>(sin(f32(ia) / f32(a) * TAU), -cos(f32(ia) / f32(a) * TAU));
    let pb = RING_B * vec2<f32>(sin(f32(ib) / f32(b) * TAU), -cos(f32(ib) / f32(b) * TAU));
    let cur_a = i32(floor(bar * f32(a)));
    let cur_b = i32(floor(bar * f32(b)));
    let da = dot_px(length(f - pa), pulse_of(age_s(ia, a, bar, bar_s)), ia == cur_a, lk.a, lk);
    let db = dot_px(length(f - pb), pulse_of(age_s(ib, b, bar, bar_s)), ib == cur_b, lk.b, lk);
    let d = select(db, da, r > (RING_A + RING_B) * 0.5);
    if (d.a >= 1.0) {
        return d;
    }
    var col = lk.bg;
    var alpha = lk.bg_alpha;
    // ring tracks
    if (abs(r - RING_A) < 0.5 || abs(r - RING_B) < 0.5) {
        col = select(DARK, lk.line * 0.4, lk.style == 1);
    }
    // quarter-note ticks just outside the outer ring
    let q = u * 4.0;
    if (abs(q - round(q)) * TAU / 4.0 * r < 0.6 && r > RING_A + 6.5 && r < RING_A + 9.0) {
        col = select(BLACK, lk.line * 0.9, lk.style == 1);
        alpha = 1.0;
    }
    // connectors (radial) where both rings land together
    let sf = shared_flash(ia, a, b, bar, bar_s);
    let ua = f32(ia) / f32(a);
    let ang = abs(fract(u - ua + 0.5) - 0.5) * TAU * r;
    if (sf >= 0.0 && ang < 1.0 && r > RING_B && r < RING_A) {
        let base = select(0.35, 0.3, lk.style == 1);
        col = mix(select(DARK, GOLD * 0.35, lk.style == 1), mix(GOLD, WHITE, 0.4 * sf), max(sf, base));
        alpha = 1.0;
    }
    // clock hand from the hub to past the outer ring
    let hd = vec2<f32>(sin(bar * TAU), -cos(bar * TAU));
    let along = dot(f, hd);
    let across = abs(f.x * hd.y - f.y * hd.x);
    if (along > 0.0 && along < RING_A + 5.0 && across < 0.6) {
        col = select(BLACK, WHITE, lk.style == 1);
        alpha = 1.0;
    }
    if (r < 3.0) {
        col = select(BLACK, WHITE, lk.style == 1);
        alpha = 1.0;
    }
    if (d.a > 0.0) {
        col = mix(col, d.rgb, d.a);
        alpha = max(alpha, d.a);
    }
    return vec4<f32>(col, alpha);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let style = clamp(p_style(), 0, 1);
    let ring_layout = p_layout() == 1;
    let a = clamp(p_ratio_a(), 1, 12);
    let b = clamp(p_ratio_b(), 1, 12);
    let client = select(ROWS_CLIENT, RINGS_CLIENT, ring_layout);
    let size = client + vec2<i32>(2 * BORDER, 2 * BORDER + CAPTION + 1);

    let s = max(1.0, round(se.resolution.y / 360.0 * p_scale()));
    let size_px = vec2<f32>(size) * s;
    let center = p_position() * se.resolution;
    let origin = floor(clamp(center - size_px * 0.5, vec2<f32>(0.0), max(se.resolution - size_px, vec2<f32>(0.0))));
    let p = vec2<i32>(floor((in.uv * se.resolution - origin) / s));
    if (any(p < vec2<i32>(0)) || any(p >= size)) {
        return vec4<f32>(0.0);
    }

    var lk: Look;
    if (style == 1) {
        lk = Look(1, vec3<f32>(0.1, 1.0, 0.95), vec3<f32>(1.0, 0.2, 0.85), vec3<f32>(0.02, 0.0, 0.06), 0.72, vec3<f32>(0.6, 0.5, 1.0));
    } else {
        lk = Look(0, vec3<f32>(0.9, 0.05, 0.05), vec3<f32>(0.0, 0.2, 1.0), GRAY, 1.0, BLACK);
    }

    // Bar position from lfo.bar, snapped to beat.phase so the downbeat lands on a beat wrap.
    let phase = fract(s_beat_phase());
    let beat = i32(round(fract(s_lfo_bar()) * 4.0 - phase)) & 3;
    let bar = (f32(beat) + phase) * 0.25;
    let bar_s = 240.0 / max(s_beat_bpm(), 40.0);
    let down = pulse_of(bar * bar_s);

    // Frame and caption.
    let e = min(p, size - 1 - p);
    let d = min(e.x, e.y);
    if (style == 1) {
        if (d == 0) {
            let glow = mix(lk.line, GOLD, down);
            return vec4<f32>(glow, 1.0);
        }
    } else {
        if (d == 0 || d == BORDER - 1) { return vec4<f32>(BLACK, 1.0); }
        if (d < BORDER) { return vec4<f32>(select(GRAY, WHITE, d == 1 && (p.x <= 1 || p.y <= 1)), 1.0); }
    }
    let q = p - vec2<i32>(BORDER);
    let inner_w = size.x - 2 * BORDER;
    if (q.y < CAPTION && d >= BORDER) {
        let len = 5 + digits(a) + 1 + digits(b);
        var left = 0;
        if (style == 0) {
            if (q.x < 12) {
                if (q.x == 11) { return vec4<f32>(BLACK, 1.0); }
                if (q.x >= 2 && q.x < 9 && q.y == 4) { return vec4<f32>(WHITE, 1.0); }
                if (q.x >= 2 && q.x < 10 && q.y >= 4 && q.y < 7) { return vec4<f32>(BLACK, 1.0); }
                return vec4<f32>(GRAY, 1.0);
            }
            left = 12;
        }
        let tx = left + (inner_w - left - (len * 7 - 1)) / 2;
        let t = q - vec2<i32>(tx, 2);
        let k = t.x / 7;
        let x = t.x - k * 7;
        var on = false;
        if (t.x >= 0 && k < len) {
            let g = caption_glyph(k, a, b);
            on = glyph_on(g, vec2<i32>(x, t.y)) || glyph_on(g, vec2<i32>(x - 1, t.y));
        }
        if (style == 1) {
            let txt = mix(lk.b, GOLD, down);
            return select(vec4<f32>(lk.bg * lk.bg_alpha, lk.bg_alpha), vec4<f32>(txt, 1.0), on);
        }
        // caption blinks inverted on the downbeat
        let blink = down > 0.4;
        let cap = p_caption_color().rgb;
        return vec4<f32>(select(select(cap, BLACK, blink), select(BLACK, cap, blink), on), 1.0);
    }
    if (style == 0 && q.y == CAPTION) {
        return vec4<f32>(BLACK, 1.0);
    }
    if (style == 1 && (d < BORDER || q.y <= CAPTION)) {
        return vec4<f32>(lk.bg * lk.bg_alpha, lk.bg_alpha);
    }
    let c = q - vec2<i32>(0, CAPTION + 1);
    var out = vec4<f32>(0.0);
    if (ring_layout) {
        out = rings(c, a, b, bar, bar_s, lk);
    } else {
        out = rows(c, a, b, bar, bar_s, lk);
    }
    return vec4<f32>(out.rgb * out.a, out.a);
}
