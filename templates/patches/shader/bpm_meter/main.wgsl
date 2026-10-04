// BPM meter: a Windows 3.1 window with a big chunky dot-matrix BPM readout (rounded beat.bpm) and
// a small needle gauge showing the deviation from `target_bpm`. Within `tolerance` the readout
// glows gold, pulses on the beat and a dithered gold halo breathes around the window ("LOCKED");
// beyond it the window flinches: it shakes on a jittery pixel grid, tints red, the readout turns
// red and the needle swings into the red zone ("DRIFT!"). target_bpm = 0 follows the rounded
// current tempo, so it is always steady. Real drift needs the analysis tempo (beat.bpm) to move:
// a steady click or a fixed tempo never flinches. Kicks twitch the needle like a mechanical meter.
// Everything is drawn on an integer grid of "dialog pixels" (`s` screen pixels each).
// Always drawn (no manifest trigger).

// 5x7 font, one u32 per row (bit 4 = leftmost column): 0 1 2 3 4 5 6 7 8 9 - B P M L O C K E D R I F T ! +
const FONT: array<u32, 182> = array<u32, 182>(
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
    30u, 17u, 17u, 30u, 17u, 17u, 30u, // B
    30u, 17u, 17u, 30u, 16u, 16u, 16u, // P
    17u, 27u, 21u, 21u, 17u, 17u, 17u, // M
    16u, 16u, 16u, 16u, 16u, 16u, 31u, // L
    14u, 17u, 17u, 17u, 17u, 17u, 14u, // O
    14u, 17u, 16u, 16u, 16u, 17u, 14u, // C
    17u, 18u, 20u, 24u, 20u, 18u, 17u, // K
    31u, 16u, 16u, 30u, 16u, 16u, 31u, // E
    28u, 18u, 17u, 17u, 17u, 18u, 28u, // D
    30u, 17u, 17u, 30u, 20u, 18u, 17u, // R
    14u, 4u, 4u, 4u, 4u, 4u, 14u, // I
    31u, 16u, 16u, 30u, 16u, 16u, 16u, // F
    31u, 4u, 4u, 4u, 4u, 4u, 4u, // T
    4u, 4u, 4u, 4u, 4u, 0u, 4u, // !
    0u, 4u, 4u, 31u, 4u, 4u, 0u, // +
);
const G_MINUS = 10u;
const G_PLUS = 25u;
// Captions, 11 glyphs each (255 = space): "BPM", "BPM  LOCKED", "BPM  DRIFT!"
const CAPS: array<u32, 33> = array<u32, 33>(
    11u, 12u, 13u, 255u, 255u, 255u, 255u, 255u, 255u, 255u, 255u,
    11u, 12u, 13u, 255u, 255u, 14u, 15u, 16u, 17u, 18u, 19u,
    11u, 12u, 13u, 255u, 255u, 19u, 20u, 21u, 22u, 23u, 24u,
);
const CAP_LEN = array<i32, 3>(3, 11, 11);

const BLACK = vec3<f32>(0.0);
const WHITE = vec3<f32>(1.0);
const GRAY = vec3<f32>(0.753);
const DARK = vec3<f32>(0.502);
const GOLD = vec3<f32>(1.0, 0.78, 0.18);
const RED = vec3<f32>(1.0, 0.14, 0.08);
const GREEN = vec3<f32>(0.1, 0.75, 0.2);
const YELLOW = vec3<f32>(1.0, 0.85, 0.1);

// Layout (dialog pixels). The canvas has a MARGIN around the window for the halo and the shake.
const MARGIN = 6;
const WIN = vec2<i32>(150, 68);
const SIZE = vec2<i32>(162, 80);
const BORDER = 4;
const CAPTION = 11;
const PANEL = vec2<i32>(5, 5);        // readout panel (client coords), 78x38 with a 1-pixel bevel
const PANEL_SIZE = vec2<i32>(78, 38);
const GAUGE = vec2<i32>(89, 5);       // gauge face 48x38
const GAUGE_SIZE = vec2<i32>(48, 38);
const PIVOT = vec2<f32>(24.0, 33.0);  // needle pivot in gauge coords
const SWEEP = 1.22;                   // radians to full deflection either side

fn glyph_on(g: u32, p: vec2<i32>) -> bool {
    if (g == 255u || p.x < 0 || p.x > 4 || p.y < 0 || p.y > 6) {
        return false;
    }
    return ((FONT[g * 7u + u32(p.y)] >> u32(4 - p.x)) & 1u) == 1u;
}

fn in_box(p: vec2<i32>, lo: vec2<i32>, size: vec2<i32>) -> bool {
    return all(p >= lo) && all(p < lo + size);
}

fn hash(n: f32) -> f32 {
    return fract(sin(n * 127.1 + 311.7) * 43758.5453);
}

// Map a decaying hit envelope back to "time since the hit" and ring a small damped spring.
fn spring_from_env(k: f32) -> f32 {
    if (k < 0.003) {
        return 0.0;
    }
    let u = -log(min(k, 1.0));
    return sin(1.8 * u) * exp(-0.45 * u) / 0.7;
}

// Readout: three 5x7 digits as a dot matrix, each font pixel a 3x3 lit dot with a 1-pixel gap
// (unlit dots faintly visible); lit dots spill a little glow into their gaps.
fn readout(q: vec2<i32>, bpm: i32, col: vec3<f32>, glow: f32) -> vec3<f32> {
    let d = q - vec2<i32>(5, 5);
    let k = d.x / 24;
    let x = d.x - k * 24;
    if (d.x < 0 || d.y < 0 || k > 2 || x >= 20 || d.y >= 28) {
        return BLACK;
    }
    var g = G_MINUS;
    if (bpm > 0) {
        let div = select(select(1, 10, k == 1), 100, k == 0);
        let v = bpm / div;
        // no leading zero below 100 BPM
        g = select(u32(v % 10), 255u, k == 0 && v == 0);
    }
    let cell = vec2<i32>(x / 4, d.y / 4);
    let sub = vec2<i32>(x % 4, d.y % 4);
    let on = glyph_on(g, cell);
    if (sub.x < 3 && sub.y < 3) {
        if (on) {
            // a brighter top-left pixel per dot gives the chunky LEDs a little relief
            return select(col, mix(col, WHITE, 0.45), sub.x == 0 && sub.y == 0);
        }
        return col * 0.1;
    }
    return select(BLACK, col * glow, on);
}

fn gauge(q: vec2<i32>, needle: f32, zone: f32) -> vec3<f32> {
    // sunken white face
    if (q.x == 0 || q.y == 0) { return DARK; }
    if (q.x == GAUGE_SIZE.x - 1 || q.y == GAUGE_SIZE.y - 1) { return WHITE; }
    if (q.x == 1 || q.y == 1) { return BLACK; }
    let f = vec2<f32>(q) + 0.5 - PIVOT;
    let r = length(f);
    let a = atan2(f.x, -f.y); // 0 = straight up, + to the right
    // needle: black, 1.5 px wide, with a red hub
    let dir = vec2<f32>(sin(needle), -cos(needle));
    let along = dot(f, dir);
    let across = abs(f.x * dir.y - f.y * dir.x);
    if (r < 2.6) {
        return select(RED, BLACK, r > 1.8);
    }
    if (along > 0.0 && along < 19.0 && across < 0.75) {
        return BLACK;
    }
    // coloured arc band: green within tolerance, then yellow, then red
    let u = abs(a) / SWEEP;
    if (u <= 1.0 && r >= 17.0 && r < 21.0) {
        if (r >= 20.0) { return BLACK; }
        let c = select(select(RED, YELLOW, u <= zone * 2.0), GREEN, u <= zone);
        return c;
    }
    // ticks every 1/6 of the sweep, longer at the centre and ends
    let tick_u = round(a / SWEEP * 6.0);
    if (abs(tick_u) <= 6.0 && abs(a - tick_u / 6.0 * SWEEP) * r < 0.6) {
        let long = abs(tick_u) == 0.0 || abs(tick_u) == 6.0;
        if (r >= select(14.0, 11.0, long) && r < 17.0) {
            return BLACK;
        }
    }
    // "-" and "+" at the ends
    if (glyph_on(G_MINUS, q - vec2<i32>(3, 29)) || glyph_on(G_PLUS, q - vec2<i32>(40, 29))) {
        return BLACK;
    }
    return WHITE;
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let s = max(1.0, round(se.resolution.y / 360.0 * p_scale()));
    let bpm_f = s_beat_bpm();
    let running = bpm_f >= 1.0;
    let bpm = select(-1, i32(round(bpm_f)), running);
    let tol = max(p_tolerance(), 0.1);
    let tgt = select(round(bpm_f), p_target_bpm(), p_target_bpm() > 0.0);
    let dev = select(0.0, bpm_f - tgt, running);
    // 0 = steady, 1 = flinching (ramps over a quarter of the tolerance past the edge)
    let flinch = clamp((abs(dev) - tol) / max(tol * 0.25, 0.25), 0.0, 1.0);
    let steady = select(0.0, 1.0 - flinch, running);
    let phase = fract(s_beat_phase());
    let pulse = select(0.0, exp(-phase * 6.0), running);

    // Shake: integer offsets re-rolled 15x a second, bigger the further off and on snares.
    let over = max(abs(dev) - tol, 0.0) / tol;
    let amp = flinch * min(1.0 + over + 2.0 * s_band_snare(), 4.0);
    let step = floor(se.time * 15.0);
    let shake = vec2<i32>(round((vec2<f32>(hash(step), hash(step + 17.0)) - 0.5) * 2.0 * amp));

    let size_px = vec2<f32>(SIZE) * s;
    let center = p_position() * se.resolution;
    let origin = floor(clamp(center - size_px * 0.5, vec2<f32>(0.0), max(se.resolution - size_px, vec2<f32>(0.0))));
    let p = vec2<i32>(floor((in.uv * se.resolution - origin) / s)) - vec2<i32>(MARGIN) - shake;
    if (any(p < vec2<i32>(-MARGIN)) || any(p >= WIN + vec2<i32>(MARGIN))) {
        return vec4<f32>(0.0);
    }

    if (any(p < vec2<i32>(0)) || any(p >= WIN)) {
        // Gold halo: ordered-dither ring that breathes with the beat while steady.
        let out_d = max(max(-p.x, p.x - WIN.x + 1), max(-p.y, p.y - WIN.y + 1));
        let strength = steady * (0.55 + 0.45 * pulse) * (1.0 - f32(out_d - 1) / f32(MARGIN));
        var bayer = array<f32, 16>(0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0);
        let pq = (p + vec2<i32>(64)) & vec2<i32>(3);
        if (strength > (bayer[pq.x + pq.y * 4] + 0.5) / 16.0) {
            return vec4<f32>(GOLD * 0.9, 0.9);
        }
        return vec4<f32>(0.0);
    }

    let tint = vec3<f32>(1.0, 0.45, 0.4);
    let red_mix = 0.45 * flinch;
    // Window frame: black, 2 gray, black.
    let e = min(p, WIN - 1 - p);
    let d = min(e.x, e.y);
    if (d == 0 || d == BORDER - 1) {
        return vec4<f32>(BLACK, 1.0);
    }
    if (d < BORDER) {
        let fr = select(GRAY, WHITE, d == 1 && (p.x <= 1 || p.y <= 1));
        return vec4<f32>(mix(fr, fr * tint, red_mix), 1.0);
    }
    let q = p - vec2<i32>(BORDER);
    let inner_w = WIN.x - 2 * BORDER;
    if (q.y < CAPTION) {
        if (q.x < 12) {
            if (q.x == 11) { return vec4<f32>(BLACK, 1.0); }
            if (q.x >= 2 && q.x < 9 && q.y == 4) { return vec4<f32>(WHITE, 1.0); }
            if (q.x >= 2 && q.x < 10 && q.y >= 4 && q.y < 7) { return vec4<f32>(BLACK, 1.0); }
            return vec4<f32>(GRAY, 1.0);
        }
        let cap = select(select(0, 1, running), 2, flinch > 0.5);
        let len = CAP_LEN[cap];
        let tx = 12 + (inner_w - 12 - (len * 7 - 1)) / 2;
        let t = q - vec2<i32>(tx, 2);
        let i = t.x / 7;
        let x = t.x - i * 7;
        var on = false;
        if (t.x >= 0 && i < len) {
            let g = CAPS[cap * 11 + i];
            on = glyph_on(g, vec2<i32>(x, t.y)) || glyph_on(g, vec2<i32>(x - 1, t.y));
        }
        let bg = select(p_caption_color().rgb, RED, cap == 2);
        let fg = select(BLACK, WHITE, cap == 2);
        return vec4<f32>(select(bg, fg, on), 1.0);
    }
    if (q.y == CAPTION) {
        return vec4<f32>(BLACK, 1.0);
    }
    let c = q - vec2<i32>(0, CAPTION + 1);
    if (in_box(c, PANEL, PANEL_SIZE)) {
        let pq = c - PANEL;
        if (pq.x == 0 || pq.y == 0) { return vec4<f32>(DARK, 1.0); }
        if (pq.x == PANEL_SIZE.x - 1 || pq.y == PANEL_SIZE.y - 1) { return vec4<f32>(WHITE, 1.0); }
        // gold, brightening on each beat when steady; hard red when flinching; dim without tempo
        let gold = GOLD * (0.82 + 0.18 * pulse);
        let col = select(vec3<f32>(0.55, 0.45, 0.2), mix(gold, RED, flinch), running);
        return vec4<f32>(readout(pq, bpm, col, 0.18 + 0.22 * pulse * steady), 1.0);
    }
    if (in_box(c, GAUGE, GAUGE_SIZE)) {
        let full = 3.0 * tol;
        let wob = flinch * 0.08 * sin(se.time * 37.0);
        let needle = clamp(dev / full, -1.08, 1.08) * SWEEP + 0.05 * spring_from_env(s_band_kick()) + wob;
        return vec4<f32>(gauge(c - GAUGE, needle, 1.0 / 3.0), 1.0);
    }
    return vec4<f32>(mix(GRAY, GRAY * tint, red_mix), 1.0);
}
