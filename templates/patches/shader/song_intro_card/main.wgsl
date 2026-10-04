// Song intro card: a Windows 3.1 application window titled "NOW PLAYING". It zooms open with
// checkered XOR rectangles, a CD spins while a Setup-style gauge fills ("Loading song..." with
// animated dots, the percentage inverted where the bar has passed), and a green LCD shows the live
// tempo (beat.bpm) blinking on every beat. Once the bar is full it says "Ready!" and waits for the
// next downbeat (bar or beat, `close_on`), then the rectangles collapse exactly on that hit.
// The downbeat is found analytically: the most recent onset was `phase * period` ago, so the
// first onset after the fill ends sits on that grid in trigger-age time (assumes a stable tempo).
// Everything is drawn on an integer grid of window pixels (`s` screen pixels each).

// Bitmap font, only the glyphs used: 'NOWPLAYIGoadings.Rey!BM0123456789-%'. 8 rows per glyph
// (row 7 = descender), bit 4 = leftmost column.
const FONT = array<u32, 280>(
    17u, 25u, 21u, 19u, 17u, 17u, 17u, 0u, // 'N'
    14u, 17u, 17u, 17u, 17u, 17u, 14u, 0u, // 'O'
    17u, 17u, 17u, 21u, 21u, 27u, 17u, 0u, // 'W'
    30u, 17u, 17u, 30u, 16u, 16u, 16u, 0u, // 'P'
    16u, 16u, 16u, 16u, 16u, 16u, 31u, 0u, // 'L'
    14u, 17u, 17u, 31u, 17u, 17u, 17u, 0u, // 'A'
    17u, 17u, 10u, 4u, 4u, 4u, 4u, 0u, // 'Y'
    28u, 8u, 8u, 8u, 8u, 8u, 28u, 0u, // 'I'
    14u, 17u, 16u, 23u, 17u, 17u, 15u, 0u, // 'G'
    0u, 0u, 14u, 17u, 17u, 17u, 14u, 0u, // 'o'
    0u, 0u, 14u, 1u, 15u, 17u, 15u, 0u, // 'a'
    1u, 1u, 15u, 17u, 17u, 17u, 15u, 0u, // 'd'
    16u, 0u, 16u, 16u, 16u, 16u, 16u, 0u, // 'i'
    0u, 0u, 22u, 25u, 17u, 17u, 17u, 0u, // 'n'
    0u, 0u, 15u, 17u, 17u, 15u, 1u, 14u, // 'g'
    0u, 0u, 15u, 16u, 14u, 1u, 30u, 0u, // 's'
    0u, 0u, 0u, 0u, 0u, 0u, 16u, 0u, // '.'
    30u, 17u, 17u, 30u, 20u, 18u, 17u, 0u, // 'R'
    0u, 0u, 14u, 17u, 31u, 16u, 14u, 0u, // 'e'
    0u, 0u, 17u, 17u, 17u, 15u, 1u, 14u, // 'y'
    16u, 16u, 16u, 16u, 16u, 0u, 16u, 0u, // '!'
    30u, 17u, 17u, 30u, 17u, 17u, 30u, 0u, // 'B'
    17u, 27u, 21u, 21u, 17u, 17u, 17u, 0u, // 'M'
    14u, 17u, 19u, 21u, 25u, 17u, 14u, 0u, // '0'
    4u, 12u, 4u, 4u, 4u, 4u, 14u, 0u, // '1'
    14u, 17u, 1u, 2u, 4u, 8u, 31u, 0u, // '2'
    14u, 17u, 1u, 6u, 1u, 17u, 14u, 0u, // '3'
    2u, 6u, 10u, 18u, 31u, 2u, 2u, 0u, // '4'
    31u, 16u, 30u, 1u, 1u, 17u, 14u, 0u, // '5'
    6u, 8u, 16u, 30u, 17u, 17u, 14u, 0u, // '6'
    31u, 1u, 2u, 4u, 8u, 8u, 8u, 0u, // '7'
    14u, 17u, 17u, 14u, 17u, 17u, 14u, 0u, // '8'
    14u, 17u, 17u, 15u, 1u, 2u, 12u, 0u, // '9'
    0u, 0u, 0u, 28u, 0u, 0u, 0u, 0u, // '-'
    25u, 26u, 2u, 4u, 8u, 11u, 19u, 0u, // '%'
);
const GLYPH_W = array<u32, 35>(5u, 5u, 5u, 5u, 5u, 5u, 5u, 3u, 5u, 5u, 5u, 5u, 1u, 5u, 5u, 5u, 1u, 5u, 5u, 5u, 1u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 3u, 5u);
const DIGIT0 = 23u; // glyph index of '0' (digits are consecutive)
const G_DASH = 33u;
const G_PCT = 34u;
// Text runs: glyph index | x offset << 8 (bold advance = width + 2).
const RUNS = array<u32, 33>(
    0x0u, 0x701u, 0xe02u, 0x1903u, 0x2004u, 0x2705u, 0x2e06u, 0x3507u, 0x3a00u, 0x4108u, // NOW PLAYING
    0x4u, 0x709u, 0xe0au, 0x150bu, 0x1c0cu, 0x1f0du, 0x260eu, 0x310fu, 0x3809u, 0x3f0du, 0x460eu, 0x4d10u, 0x5010u, 0x5310u, // Loading song...
    0x11u, 0x712u, 0xe0au, 0x150bu, 0x1c13u, 0x2314u, // Ready!
    0x15u, 0x703u, 0xe16u, // BPM
);
// start, glyph count, pixel width
const T_CAPTION = vec3<u32>(0u, 10u, 71u);
const T_LOADING = vec3<u32>(10u, 14u, 85u);
const T_READY = vec3<u32>(24u, 6u, 37u);
const T_BPM = vec3<u32>(30u, 3u, 20u);

const BLACK = vec3<f32>(0.0);
const WHITE = vec3<f32>(1.0);
const GRAY = vec3<f32>(0.753);
const DARK = vec3<f32>(0.502);
const NAVY = vec3<f32>(0.0, 0.0, 0.502);
const LCD_ON = vec3<f32>(0.25, 1.0, 0.35);
const LCD_OFF = vec3<f32>(0.0, 0.16, 0.05);

// Window metrics (window pixels): sizing border, caption, client.
const WIN = vec2<i32>(232, 106);  // 5 frame + 18 caption + 1 rule + 82 client
const CLIENT_Y = 24;
const BAR_POS = vec2<i32>(12, 52);
const BAR_SIZE = vec2<i32>(198, 18);
const LCD_POS = vec2<i32>(54, 27);
const LCD_SIZE = vec2<i32>(40, 14);

const OPEN_T = 0.24;     // zoom-open / zoom-close duration (s)
const FILL_START = 0.25; // progress starts once the window is open

fn glyph_on(g: u32, x: i32, y: i32) -> bool {
    if (x < 0 || y < 0 || y > 7 || x >= i32(GLYPH_W[g])) {
        return false;
    }
    return ((FONT[g * 8u + u32(y)] >> u32(4 - x)) & 1u) == 1u;
}

// Bold System-font look: every glyph pixel is doubled one pixel to the right. `count` limits the
// glyphs drawn (for the typing dots).
fn text_on(t: vec3<u32>, count: u32, p: vec2<i32>) -> bool {
    if (p.y < 0 || p.y > 7 || p.x < 0 || p.x > i32(t.z)) {
        return false;
    }
    for (var i = 0u; i < min(count, t.y); i++) {
        let e = RUNS[t.x + i];
        let g = e & 0xffu;
        let lx = p.x - i32(e >> 8u);
        if (glyph_on(g, lx, p.y) || glyph_on(g, lx - 1, p.y)) {
            return true;
        }
    }
    return false;
}

// Right-aligned bold number (with optional trailing glyph), right edge at x = 0.
fn number_on(value: u32, digits: u32, suffix: u32, p: vec2<i32>) -> bool {
    var x = 0;
    var v = value;
    if (suffix != 0xffu) {
        x -= i32(GLYPH_W[suffix]) + 1;
        if (glyph_on(suffix, p.x - x, p.y) || glyph_on(suffix, p.x - x - 1, p.y)) {
            return true;
        }
        x -= 1;
    }
    for (var i = 0u; i < digits; i++) {
        let g = select(DIGIT0 + v % 10u, G_DASH, value == 0xffffu);
        let w = i32(GLYPH_W[g]);
        x -= w + 1;
        if (glyph_on(g, p.x - x, p.y) || glyph_on(g, p.x - x - 1, p.y)) {
            return true;
        }
        x -= 1;
        v /= 10u;
        if (v == 0u && value != 0xffffu) {
            break;
        }
    }
    return false;
}

fn in_box(p: vec2<i32>, lo: vec2<i32>, size: vec2<i32>) -> bool {
    return all(p >= lo) && all(p < lo + size);
}

// 32x32 CD icon: silver disc with a stepped rainbow sheen that spins, clear hub, black rim.
fn cd_icon(p: vec2<i32>, spin: f32) -> vec4<f32> {
    let c = vec2<f32>(p) + 0.5 - vec2<f32>(16.0);
    let r = length(c);
    if (r > 15.0 || r < 2.6) {
        return vec4<f32>(0.0);
    }
    if (r > 14.0 || r < 3.6) {
        return vec4<f32>(BLACK, 1.0);
    }
    if (r < 7.0) {
        // hub: clear plastic ring with a darker inner lip
        return vec4<f32>(select(vec3<f32>(0.86), DARK, r < 5.0), 1.0);
    }
    // two opposite sheen bands; hue stepped to 6 colours like a 16-colour icon
    let a = atan2(c.y, c.x) - spin;
    let band = pow(abs(cos(a)), 6.0);
    let hue = floor(fract(a / 3.14159 + r / 22.0) * 6.0) / 6.0;
    let rainbow = clamp(abs(fract(hue + vec3<f32>(0.0, 0.667, 0.333)) * 6.0 - 3.0) - 1.0, vec3<f32>(0.0), vec3<f32>(1.0));
    var col = mix(vec3<f32>(0.78, 0.8, 0.84), mix(WHITE, rainbow, 0.55), step(0.35, band));
    // a white glint dot that rides along the band
    if (band > 0.92 && r > 9.0 && r < 12.0) {
        col = WHITE;
    }
    return vec4<f32>(col, 1.0);
}

fn window(p: vec2<i32>, age: f32, prog: f32, ready: bool, beat: f32) -> vec3<f32> {
    let W = WIN.x;
    let H = WIN.y;
    let d = min(min(p.x, W - 1 - p.x), min(p.y, H - 1 - p.y));
    if (d < 1) {
        return BLACK;
    }
    if (d < 4) {
        // sizing border with corner notches 23 px in from each corner
        let on_tb = p.y < 4 || H - 1 - p.y < 4;
        let on_lr = p.x < 4 || W - 1 - p.x < 4;
        if ((on_tb && (p.x == 23 || W - 1 - p.x == 23)) || (on_lr && (p.y == 23 || H - 1 - p.y == 23))) {
            return BLACK;
        }
        return GRAY;
    }
    if (d < 5 || p.y == CLIENT_Y - 1) {
        return BLACK;
    }
    if (p.y < CLIENT_Y - 1) {
        let ty = p.y - 5;
        // system menu box
        if (p.x < 23) {
            if (p.x >= 9 && p.x < 20 && ty >= 7 && ty < 10) {
                if (p.x >= 10 && p.x < 19 && ty == 8) {
                    return WHITE;
                }
                return select(DARK, BLACK, p.x >= 19 || ty >= 9);
            }
            return GRAY;
        }
        if (p.x == 23) {
            return BLACK;
        }
        // minimize / maximize buttons
        let rx = W - 1 - p.x;
        if (rx < 43) {
            if (rx == 42 || rx == 23) {
                return BLACK;
            }
            let maximize = rx < 23;
            let bx = select(p.x - (W - 43), p.x - (W - 23), maximize);
            if (bx < 1 || ty < 1) {
                return WHITE;
            }
            if (bx >= 16 || ty >= 16) {
                return DARK;
            }
            let row = select(ty - 7, 10 - ty, maximize);
            if (row >= 0 && row < 4 && abs(f32(bx) - 8.0) <= 3.5 - f32(row)) {
                return BLACK;
            }
            return GRAY;
        }
        let tx = 24 + (W - 24 - 43 - i32(T_CAPTION.z)) / 2;
        if (text_on(T_CAPTION, 99u, vec2<i32>(p.x - tx, ty - 5))) {
            return WHITE;
        }
        return NAVY;
    }
    // client area
    let c = p - vec2<i32>(5, CLIENT_Y);
    if (in_box(c, vec2<i32>(12, 10), vec2<i32>(32))) {
        let ic = cd_icon(c - vec2<i32>(12, 10), age * select(9.0, 2.0, ready));
        if (ic.a > 0.5) {
            return ic.rgb;
        }
    }
    // status line: "Loading song" + typing dots, then "Ready!"
    if (ready) {
        if (text_on(T_READY, 99u, c - vec2<i32>(54, 11))) {
            return BLACK;
        }
    } else {
        let dots = u32(age * 5.0) % 4u;
        if (text_on(T_LOADING, T_LOADING.y - 3u + dots, c - vec2<i32>(54, 11))) {
            return BLACK;
        }
    }
    // tempo LCD: sunken bevel, black glass, green digits that flash on each beat
    let l = c - LCD_POS;
    if (in_box(l, vec2<i32>(0), LCD_SIZE)) {
        if (l.x == 0 || l.y == 0) {
            return DARK;
        }
        if (l.x == LCD_SIZE.x - 1 || l.y == LCD_SIZE.y - 1) {
            return WHITE;
        }
        let bpm = s_beat_bpm();
        let shown = select(0xffffu, u32(round(clamp(bpm, 0.0, 999.0))), bpm > 1.0);
        let glow = mix(0.7, 1.0, exp(-beat * 6.0));
        // beat dot on the left
        if (in_box(l, vec2<i32>(4, 5), vec2<i32>(3, 3))) {
            return select(LCD_OFF, LCD_ON, beat < 0.25 && bpm > 1.0);
        }
        if (number_on(shown, 3u, 0xffu, l - vec2<i32>(LCD_SIZE.x - 4, 3))) {
            return LCD_ON * glow;
        }
        // unlit segments hint: faint "888"
        if (number_on(888u, 3u, 0xffu, l - vec2<i32>(LCD_SIZE.x - 4, 3))) {
            return LCD_OFF;
        }
        return vec3<f32>(0.0, 0.05, 0.02);
    }
    if (text_on(T_BPM, 99u, c - vec2<i32>(LCD_POS.x + LCD_SIZE.x + 6, LCD_POS.y + 3))) {
        return BLACK;
    }
    // Setup-style gauge: sunken bevel, black frame, white trough, navy fill, centred inverted %
    let b = c - BAR_POS;
    if (in_box(b, vec2<i32>(0), BAR_SIZE)) {
        if (b.x == 0 || b.y == 0) {
            return DARK;
        }
        if (b.x == BAR_SIZE.x - 1 || b.y == BAR_SIZE.y - 1) {
            return WHITE;
        }
        if (b.x == 1 || b.y == 1 || b.x == BAR_SIZE.x - 2 || b.y == BAR_SIZE.y - 2) {
            return BLACK;
        }
        let inner = BAR_SIZE.x - 4;
        let filled = (b.x - 2) < i32(round(prog * f32(inner)));
        let pct = u32(round(prog * 100.0));
        let digits_w = select(select(26, 19, pct < 100u), 12, pct < 10u);
        let on = number_on(pct, 3u, G_PCT, b - vec2<i32>(BAR_SIZE.x / 2 + digits_w / 2, 5));
        if (on) {
            return select(NAVY, WHITE, filled);
        }
        return select(WHITE, NAVY, filled);
    }
    return GRAY;
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let env = clamp(se.env, 0.0, 1.0);
    if (env <= 0.0) {
        return vec4<f32>(0.0);
    }
    let age = max(se.trigger_age, 0.0);
    let s = max(1.0, round(se.resolution.y / 360.0 * p_scale()));
    let size_px = vec2<f32>(WIN) * s;
    let center = p_position() * se.resolution;
    let origin = floor(clamp(center - size_px * 0.5, vec2<f32>(0.0), max(se.resolution - size_px, vec2<f32>(0.0))));
    let frag = in.uv * se.resolution;
    let p = vec2<i32>(floor((frag - origin) / s));

    // progress: a little stall in the middle, stepped in 4 % chunks like an installer
    let fill = max(p_fill_time(), 0.05);
    let u = clamp((age - FILL_START) / fill, 0.0, 1.0);
    let prog = select(floor((u - 0.08 * sin(6.2832 * u)) * 25.0) / 25.0, 1.0, u >= 1.0);
    let fill_end = FILL_START + fill;
    let ready = age >= fill_end;

    // close on the first downbeat at or after the fill ends (on the same onset grid every frame)
    let bpm = s_beat_bpm();
    let on_beat = p_close_on() == 1;
    var close_start = fill_end + 0.3;
    let beat = fract(s_beat_phase());
    if (bpm > 1.0) {
        let period = select(240.0, 60.0, on_beat) / bpm;
        let ph = fract(select(s_lfo_bar(), s_beat_phase(), on_beat));
        let last = age - ph * period;
        close_start = last + ceil((fill_end - last) / period) * period;
    }
    var open = clamp(age / OPEN_T, 0.0, 1.0);
    // the collapse is already under way on the downbeat frame itself
    if (age >= close_start) {
        open = 0.85 * (1.0 - clamp((age - close_start) / OPEN_T, 0.0, 1.0));
    }
    // released before the downbeat arrived: collapse with the envelope instead
    if (env < 1.0 && age > OPEN_T + 0.05) {
        open = min(open, smoothstep(0.05, 0.8, env));
    }
    if (open <= 0.0) {
        return vec4<f32>(0.0);
    }
    if (open >= 1.0) {
        if (any(p < vec2<i32>(0)) || any(p >= WIN)) {
            return vec4<f32>(0.0);
        }
        return vec4<f32>(window(p, age, prog, ready, beat), 1.0);
    }
    // zoom rectangles: three checkered outlines between the window center and its frame
    let cell = vec2<i32>(floor(frag / s));
    let checker = ((cell.x + cell.y) & 1) == 0;
    let full_hi = vec2<f32>(WIN);
    let mid = floor(full_hi * 0.5);
    for (var k = 0; k < 3; k++) {
        let t = open - f32(k) * 0.16;
        if (t <= 0.0) {
            continue;
        }
        let e = t * t * (3.0 - 2.0 * t);
        let lo = vec2<i32>(floor(mix(mid - 6.0, vec2<f32>(0.0), e)));
        let hi = vec2<i32>(floor(mix(mid + 6.0, full_hi, e)));
        let d = min(p - lo, hi - 1 - p);
        if (all(p >= lo) && all(p < hi) && min(d.x, d.y) < 2) {
            return vec4<f32>(select(BLACK, WHITE, checker), 1.0);
        }
    }
    return vec4<f32>(0.0);
}
