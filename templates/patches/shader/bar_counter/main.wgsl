// Bar counter: a compact Windows 3.1 window with a Minesweeper-style red LED bar number and four
// beat lamps (1-2-3-4, the downbeat lamp red, the others green) under which a 16-step progress bar
// fills across the bar. The caption flashes (FlashWindow style) and the LEDs flare on every
// downbeat; during a fill (snare hits off the backbeat) the lamps and progress blocks turn
// magenta and the caption reads "FILL!".
//
// Beat position: lamp = beat within the bar from lfo.bar, re-aligned to beat.phase so the lamp
// changes exactly when the beat wraps. Bar number: there is no bar counter signal, so it is
// derived from se.time * bpm / 240 (assumes a stable tempo; a tempo change makes it jump) and
// locked to the lfo.bar wrap so it ticks over together with lamp 1.
// Everything is drawn on an integer grid of "dialog pixels" (`s` screen pixels each).
// Always drawn (no manifest trigger).

// 5x7 font, one u32 per row (bit 4 = leftmost column): 1 2 3 4 B A R C O U N T E F I L !
const FONT: array<u32, 119> = array<u32, 119>(
    4u, 12u, 4u, 4u, 4u, 4u, 14u, // 1
    14u, 17u, 1u, 2u, 4u, 8u, 31u, // 2
    31u, 2u, 4u, 2u, 1u, 17u, 14u, // 3
    2u, 6u, 10u, 18u, 31u, 2u, 2u, // 4
    30u, 17u, 17u, 30u, 17u, 17u, 30u, // B
    14u, 17u, 17u, 31u, 17u, 17u, 17u, // A
    30u, 17u, 17u, 30u, 20u, 18u, 17u, // R
    14u, 17u, 16u, 16u, 16u, 17u, 14u, // C
    14u, 17u, 17u, 17u, 17u, 17u, 14u, // O
    17u, 17u, 17u, 17u, 17u, 17u, 14u, // U
    17u, 17u, 25u, 21u, 19u, 17u, 17u, // N
    31u, 4u, 4u, 4u, 4u, 4u, 4u, // T
    31u, 16u, 16u, 30u, 16u, 16u, 31u, // E
    31u, 16u, 16u, 30u, 16u, 16u, 16u, // F
    14u, 4u, 4u, 4u, 4u, 4u, 14u, // I
    16u, 16u, 16u, 16u, 16u, 16u, 31u, // L
    4u, 4u, 4u, 4u, 4u, 0u, 4u, // !
);
// Captions as glyph indices (255 = space): "BAR COUNTER", "FILL!"
const CAP_BAR: array<u32, 11> = array<u32, 11>(4u, 5u, 6u, 255u, 7u, 8u, 9u, 10u, 11u, 12u, 6u);
const CAP_FILL: array<u32, 5> = array<u32, 5>(13u, 14u, 15u, 15u, 16u);

// Seven-segment masks (bit 0 = a top, b upper right, c lower right, d bottom, e lower left,
// f upper left, g middle) for 0-9, then '-' (index 10).
const SEG: array<u32, 11> = array<u32, 11>(0x3fu, 0x06u, 0x5bu, 0x4fu, 0x66u, 0x6du, 0x7du, 0x07u, 0x7fu, 0x6fu, 0x40u);

const BLACK = vec3<f32>(0.0);
const WHITE = vec3<f32>(1.0);
const GRAY = vec3<f32>(0.753);
const DARK = vec3<f32>(0.502);
const NAVY = vec3<f32>(0.0, 0.0, 0.502);
const LED_ON = vec3<f32>(1.0, 0.08, 0.04);
const LED_OFF = vec3<f32>(0.26, 0.02, 0.02);
const LAMP_DOWN = vec3<f32>(1.0, 0.16, 0.1);
const LAMP_BEAT = vec3<f32>(0.2, 1.0, 0.25);
const LAMP_FILL = vec3<f32>(1.0, 0.2, 0.9);

// Layout (dialog pixels).
const SIZE = vec2<i32>(131, 54);
const BORDER = 4;   // black, 2 gray, black
const CAPTION = 11; // caption height, then a 1-pixel black rule
const PANEL = vec2<i32>(5, 5);        // LED panel (client coords), 41x24 with a 1-pixel bevel
const PANEL_SIZE = vec2<i32>(41, 24);
const LAMPS = vec2<i32>(52, 5);       // four 15x13 lamps, 17 apart
const PROG = vec2<i32>(52, 21);       // progress bar 66x8
const PROG_SIZE = vec2<i32>(66, 8);

fn in_box(p: vec2<i32>, lo: vec2<i32>, size: vec2<i32>) -> bool {
    return all(p >= lo) && all(p < lo + size);
}

fn glyph_on(g: u32, p: vec2<i32>) -> bool {
    if (g == 255u || p.x < 0 || p.x > 4 || p.y < 0 || p.y > 6) {
        return false;
    }
    return ((FONT[g * 7u + u32(p.y)] >> u32(4 - p.x)) & 1u) == 1u;
}

// Bold caption text (every glyph pixel doubled one to the right), 7-pixel advance.
fn caption_on(fill: bool, p: vec2<i32>) -> bool {
    if (p.y < 0 || p.y > 6 || p.x < 0) {
        return false;
    }
    let i = p.x / 7;
    let x = p.x - i * 7;
    var g = 255u;
    if (fill) {
        if (i < 5) { g = CAP_FILL[i]; }
    } else if (i < 11) {
        g = CAP_BAR[i];
    }
    return glyph_on(g, vec2<i32>(x, p.y)) || glyph_on(g, vec2<i32>(x - 1, p.y));
}

// Lit state of a 10x18 seven-segment digit cell: 1 = lit segment, 0 = unlit segment, -1 = gap.
fn seg7(mask: u32, p: vec2<i32>) -> i32 {
    var seg = -1;
    let mid_x = p.x >= 2 && p.x <= 7;
    if (mid_x && p.y <= 1) { seg = 0; }
    if (p.x >= 8 && p.y >= 2 && p.y <= 7) { seg = 1; }
    if (p.x >= 8 && p.y >= 10 && p.y <= 15) { seg = 2; }
    if (mid_x && p.y >= 16) { seg = 3; }
    if (p.x <= 1 && p.y >= 10 && p.y <= 15) { seg = 4; }
    if (p.x <= 1 && p.y >= 2 && p.y <= 7) { seg = 5; }
    if (mid_x && (p.y == 8 || p.y == 9)) { seg = 6; }
    if (seg < 0) {
        return -1;
    }
    return select(0, 1, ((mask >> u32(seg)) & 1u) == 1u);
}

// Sunken 1-pixel bevel (dark top/left, white bottom/right); returns alpha 0 inside.
fn sunken(q: vec2<i32>, size: vec2<i32>) -> vec4<f32> {
    if (q.x == 0 || q.y == 0) { return vec4<f32>(DARK, 1.0); }
    if (q.x == size.x - 1 || q.y == size.y - 1) { return vec4<f32>(WHITE, 1.0); }
    return vec4<f32>(0.0);
}

// Snare-fill amount 0-1 without history: map the snare envelope (≈ exp(-20 t)) back to the time
// of the last hit, place that hit in the bar and call it a fill hit when it is off the 2/4
// backbeat; fades out 0.25 s after the last such hit.
fn fill_amount(beat_pos: f32, bpm: f32) -> f32 {
    let sn = s_band_snare();
    if (sn < 0.01) {
        return 0.0;
    }
    let t = -log(min(sn, 1.0)) / 20.0;
    let hit = beat_pos - t * max(bpm, 40.0) / 60.0;
    let pos = hit - 4.0 * floor(hit / 4.0);
    let backbeat = abs(pos - 1.0) < 0.12 || abs(pos - 3.0) < 0.12;
    return select(1.0 - smoothstep(0.12, 0.25, t), 0.0, backbeat);
}

fn client(c: vec2<i32>, beat: i32, phase: f32, bar_pos: f32, number: i32, flash: f32, fill: f32) -> vec3<f32> {
    // LED panel: three red seven-segment digits, unlit segments dark red.
    if (in_box(c, PANEL, PANEL_SIZE)) {
        let q = c - PANEL;
        let bev = sunken(q, PANEL_SIZE);
        if (bev.a > 0.0) { return bev.rgb; }
        let d = q - vec2<i32>(4, 3);
        let k = d.x / 12;
        let x = d.x - k * 12;
        if (d.x >= 0 && d.y >= 0 && d.y < 18 && k < 3 && x < 10) {
            var mask = SEG[10];
            if (number >= 0) {
                let div = select(select(1, 10, k == 1), 100, k == 0);
                mask = SEG[(number / div) % 10];
            }
            let on = seg7(mask, vec2<i32>(x, d.y));
            if (on == 1) {
                return mix(LED_ON, vec3<f32>(1.0, 0.85, 0.6), flash);
            }
            if (on == 0) {
                return LED_OFF;
            }
        }
        return BLACK;
    }
    // Beat lamps: rounded 15x13 lenses in a black bezel with the beat number inside.
    let lq = c - LAMPS;
    let li = lq.x / 17;
    let lx = lq.x - li * 17;
    if (lq.x >= 0 && li < 4 && lx < 15 && lq.y >= 0 && lq.y < 13) {
        let e = min(vec2<i32>(lx, lq.y), vec2<i32>(14 - lx, 12 - lq.y));
        if (e.x + e.y < 2) {
            return GRAY; // rounded corner
        }
        if (min(e.x, e.y) == 0 || e.x + e.y == 2) {
            return BLACK; // bezel
        }
        let base = mix(select(LAMP_BEAT, LAMP_DOWN, li == 0), LAMP_FILL, fill);
        let lit = li == beat;
        // Lit: snaps on at the beat and settles to a steady glow; off: a dark lens.
        let glow = select(0.15, 0.6 + 0.4 * exp(-phase * 5.0), lit);
        var col = base * glow;
        if (lit && e.y >= 2 && e.x >= 2 && lx <= 4 && lq.y <= 4) {
            col = mix(col, WHITE, 0.7); // specular glint
        }
        if (glyph_on(u32(li), vec2<i32>(lx - 5, lq.y - 3))) {
            col = select(base * 0.55, base * 0.12, lit);
        }
        return col;
    }
    // Progress bar: 16 sixteenth-note blocks filling across the bar (Win 3.1 copy-progress look).
    if (in_box(c, PROG, PROG_SIZE)) {
        let q = c - PROG;
        let bev = sunken(q, PROG_SIZE);
        if (bev.a > 0.0) { return bev.rgb; }
        let b = q - vec2<i32>(1, 1);
        let k = b.x / 4;
        let filled = k <= i32(floor(bar_pos * 16.0));
        if (b.x - k * 4 < 3 && b.y >= 1 && b.y <= 4 && filled) {
            let blk = mix(NAVY, LAMP_FILL * 0.85, fill);
            return select(blk, mix(blk, WHITE, 0.35), k % 4 == 0);
        }
        return WHITE;
    }
    return GRAY;
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

    // Beat within the bar from lfo.bar, snapped to beat.phase so both wrap together.
    let phase = fract(s_beat_phase());
    let bar = fract(s_lfo_bar());
    let beat = i32(round(bar * 4.0 - phase)) & 3;
    let beat_pos = f32(beat) + phase;
    let bpm = s_beat_bpm();
    // Bar number (stable-tempo assumption): bars elapsed since the clock started, rounded so it
    // increments at the lfo.bar wrap; shown 001-999.
    var number = -1;
    if (bpm >= 1.0) {
        let bars = i32(round(se.time * bpm / 240.0 - bar));
        number = ((bars % 999) + 999) % 999 + 1;
    }
    let since_down = select(1e3, phase * 60.0 / max(bpm, 40.0), beat == 0);
    let flash = exp(-since_down * 9.0);
    let fill = fill_amount(beat_pos, bpm);

    // Window frame: black, 2 gray, black.
    let e = min(p, SIZE - 1 - p);
    let d = min(e.x, e.y);
    if (d == 0 || d == BORDER - 1) {
        return vec4<f32>(BLACK, 1.0);
    }
    if (d < BORDER) {
        return vec4<f32>(select(GRAY, WHITE, d == 1 && (p.x <= 1 || p.y <= 1)), 1.0);
    }
    let q = p - vec2<i32>(BORDER);
    let inner_w = SIZE.x - 2 * BORDER;
    if (q.y < CAPTION) {
        // System menu box: gray with the long bar.
        if (q.x < 12) {
            if (q.x == 11) { return vec4<f32>(BLACK, 1.0); }
            if (q.x >= 2 && q.x < 9 && q.y == 4) { return vec4<f32>(WHITE, 1.0); }
            if (q.x >= 2 && q.x < 10 && q.y >= 4 && q.y < 7) { return vec4<f32>(BLACK, 1.0); }
            return vec4<f32>(GRAY, 1.0);
        }
        // Caption flashes inverted on the downbeat; during a fill it reads "FILL!" in magenta.
        let is_fill = fill > 0.5;
        let blink = flash > 0.35;
        let cap = p_caption_color().rgb;
        let text_w = select(76, 34, is_fill);
        let tx = 12 + (inner_w - 12 - text_w) / 2;
        let on = caption_on(is_fill, q - vec2<i32>(tx, 2));
        var bg = select(cap, BLACK, blink);
        var fg = select(BLACK, cap, blink);
        if (is_fill) {
            bg = select(LAMP_FILL, BLACK, blink);
            fg = select(WHITE, LAMP_FILL, blink);
        }
        return vec4<f32>(select(bg, fg, on), 1.0);
    }
    if (q.y == CAPTION) {
        return vec4<f32>(BLACK, 1.0);
    }
    let c = q - vec2<i32>(0, CAPTION + 1);
    return vec4<f32>(client(c, beat, phase, beat_pos * 0.25, number, flash, fill), 1.0);
}
