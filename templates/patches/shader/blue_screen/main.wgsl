// Blue screen: a fake Windows crash. The trigger tears the picture apart for a moment (row
// tears, RGB split, corrupted blocks, a few flickers of blue), then the whole frame becomes a
// classic blue text-mode screen ("A fatal groove has occurred at 0028:C0FFEE ...", cursor
// blinking on the beat). When the envelope starts to fall the machine "reboots": the CRT
// collapses to a line and a dot, a DOS prompt types `win`, the Windows 3.1 logo is swept in and
// the live picture dithers back in by the end of the release. Glitch and crash are timed by
// se.trigger_age, the reboot by the release (1 - env) so it always lands back on the picture.
// Text mode is an 80-column grid of 8x16 cells of whole "dots" (2 px at 720p, 3 px at 1080p)
// drawn with the real VGA ROM font. Input and output are premultiplied alpha.

// VGA 8x16 glyphs (IBM code page 850 ROM font): 4 u32 per glyph, row r in byte r % 4 of
// word r / 4, MSB = leftmost pixel. Glyph 0 is blank, so the table starts at glyph 1.
// ()*+-.01235789:>ABCDEFLMPRSTUVWXY\_abcdefghiklmnoprstuvwy
const BS_FONT = array<u32, 228>(
    0x180c0000u, 0x30303030u, 0x0c183030u, 0x00000000u, // '('
    0x18300000u, 0x0c0c0c0cu, 0x30180c0cu, 0x00000000u, // ')'
    0x00000000u, 0xff3c6600u, 0x0000663cu, 0x00000000u, // '*'
    0x00000000u, 0x7e181800u, 0x00001818u, 0x00000000u, // '+'
    0x00000000u, 0xfe000000u, 0x00000000u, 0x00000000u, // '-'
    0x00000000u, 0x00000000u, 0x18180000u, 0x00000000u, // '.'
    0x6c380000u, 0xd6d6c6c6u, 0x386cc6c6u, 0x00000000u, // '0'
    0x38180000u, 0x18181878u, 0x7e181818u, 0x00000000u, // '1'
    0xc67c0000u, 0x30180c06u, 0xfec6c060u, 0x00000000u, // '2'
    0xc67c0000u, 0x063c0606u, 0x7cc60606u, 0x00000000u, // '3'
    0xc0fe0000u, 0x06fcc0c0u, 0x7cc60606u, 0x00000000u, // '5'
    0xc6fe0000u, 0x180c0606u, 0x30303030u, 0x00000000u, // '7'
    0xc67c0000u, 0xc67cc6c6u, 0x7cc6c6c6u, 0x00000000u, // '8'
    0xc67c0000u, 0x067ec6c6u, 0x780c0606u, 0x00000000u, // '9'
    0x00000000u, 0x00001818u, 0x00181800u, 0x00000000u, // ':'
    0x60000000u, 0x060c1830u, 0x6030180cu, 0x00000000u, // '>'
    0x38100000u, 0xfec6c66cu, 0xc6c6c6c6u, 0x00000000u, // 'A'
    0x66fc0000u, 0x667c6666u, 0xfc666666u, 0x00000000u, // 'B'
    0x663c0000u, 0xc0c0c0c2u, 0x3c66c2c0u, 0x00000000u, // 'C'
    0x6cf80000u, 0x66666666u, 0xf86c6666u, 0x00000000u, // 'D'
    0x66fe0000u, 0x68786862u, 0xfe666260u, 0x00000000u, // 'E'
    0x66fe0000u, 0x68786862u, 0xf0606060u, 0x00000000u, // 'F'
    0x60f00000u, 0x60606060u, 0xfe666260u, 0x00000000u, // 'L'
    0xeec60000u, 0xc6d6fefeu, 0xc6c6c6c6u, 0x00000000u, // 'M'
    0x66fc0000u, 0x607c6666u, 0xf0606060u, 0x00000000u, // 'P'
    0x66fc0000u, 0x6c7c6666u, 0xe6666666u, 0x00000000u, // 'R'
    0xc67c0000u, 0x0c3860c6u, 0x7cc6c606u, 0x00000000u, // 'S'
    0x7e7e0000u, 0x1818185au, 0x3c181818u, 0x00000000u, // 'T'
    0xc6c60000u, 0xc6c6c6c6u, 0x7cc6c6c6u, 0x00000000u, // 'U'
    0xc6c60000u, 0xc6c6c6c6u, 0x10386cc6u, 0x00000000u, // 'V'
    0xc6c60000u, 0xd6d6c6c6u, 0x6ceefed6u, 0x00000000u, // 'W'
    0xc6c60000u, 0x38387c6cu, 0xc6c66c7cu, 0x00000000u, // 'X'
    0x66660000u, 0x183c6666u, 0x3c181818u, 0x00000000u, // 'Y'
    0x80000000u, 0x3870e0c0u, 0x02060e1cu, 0x00000000u, // '\\'
    0x00000000u, 0x00000000u, 0x00000000u, 0x0000ff00u, // '_'
    0x00000000u, 0x7c0c7800u, 0x76ccccccu, 0x00000000u, // 'a'
    0x60e00000u, 0x666c7860u, 0x7c666666u, 0x00000000u, // 'b'
    0x00000000u, 0xc0c67c00u, 0x7cc6c0c0u, 0x00000000u, // 'c'
    0x0c1c0000u, 0xcc6c3c0cu, 0x76ccccccu, 0x00000000u, // 'd'
    0x00000000u, 0xfec67c00u, 0x7cc6c0c0u, 0x00000000u, // 'e'
    0x361c0000u, 0x30783032u, 0x78303030u, 0x00000000u, // 'f'
    0x00000000u, 0xcccc7600u, 0x7cccccccu, 0x0078cc0cu, // 'g'
    0x60e00000u, 0x66766c60u, 0xe6666666u, 0x00000000u, // 'h'
    0x18180000u, 0x18183800u, 0x3c181818u, 0x00000000u, // 'i'
    0x60e00000u, 0x786c6660u, 0xe6666c78u, 0x00000000u, // 'k'
    0x18380000u, 0x18181818u, 0x3c181818u, 0x00000000u, // 'l'
    0x00000000u, 0xd6feec00u, 0xc6d6d6d6u, 0x00000000u, // 'm'
    0x00000000u, 0x6666dc00u, 0x66666666u, 0x00000000u, // 'n'
    0x00000000u, 0xc6c67c00u, 0x7cc6c6c6u, 0x00000000u, // 'o'
    0x00000000u, 0x6666dc00u, 0x7c666666u, 0x00f06060u, // 'p'
    0x00000000u, 0x6676dc00u, 0xf0606060u, 0x00000000u, // 'r'
    0x00000000u, 0x60c67c00u, 0x7cc60c38u, 0x00000000u, // 's'
    0x30100000u, 0x3030fc30u, 0x1c363030u, 0x00000000u, // 't'
    0x00000000u, 0xcccccc00u, 0x76ccccccu, 0x00000000u, // 'u'
    0x00000000u, 0xc6c6c600u, 0x386cc6c6u, 0x00000000u, // 'v'
    0x00000000u, 0xd6c6c600u, 0x6cfed6d6u, 0x00000000u, // 'w'
    0x00000000u, 0xc6c6c600u, 0x7ec6c6c6u, 0x00f80c06u, // 'y'
);

// Text: glyph numbers (0 = space), 4 per u32, little-endian.
const BS_TEXT = array<u32, 94>(
    0x302c1f00u, 0x34383127u, 0x29001100u, 0x2e243524u, 0x31332a00u, 0x00283731u, 0x0034242bu, 0x36262631u,
    0x27283333u, 0x00352400u, 0x0d090707u, 0x1607130fu, 0x00151516u, 0x1e00302cu, 0x14001420u, 0x1b181d1au,
    0x02080701u, 0x07070400u, 0x0c111512u, 0x2b1c0006u, 0x36260028u, 0x30283333u, 0x28350035u, 0x0031322fu,
    0x2e2e2c38u, 0x00282500u, 0x2f332835u, 0x3524302cu, 0x03062728u, 0x33190000u, 0x00343428u, 0x00393024u,
    0x0039282du, 0x35003135u, 0x2c2f3328u, 0x28352430u, 0x282b3500u, 0x33362600u, 0x35302833u, 0x2e313400u,
    0x00030631u, 0x28331900u, 0x13003434u, 0x04171a1cu, 0x041c1711u, 0x00171514u, 0x2c242a24u, 0x31350030u,
    0x34283300u, 0x35332435u, 0x36313900u, 0x33270033u, 0x282f2f36u, 0x21000633u, 0x38003631u, 0x002e2e2cu,
    0x312e0000u, 0x24002834u, 0x36003930u, 0x37243430u, 0x29002728u, 0x342e2e2cu, 0x00302c00u, 0x002e2e24u,
    0x2e323224u, 0x3524262cu, 0x3430312cu, 0x28331906u, 0x24003434u, 0x2d003930u, 0x35003928u, 0x282d0031u,
    0x27003228u, 0x2f2f3633u, 0x002a302cu, 0x220f1323u, 0x302c3810u, 0x33262c18u, 0x29313431u, 0x302c1f35u,
    0x34383127u, 0x3433281eu, 0x0030312cu, 0x1308060au, 0x33393231u, 0x352b2a2cu, 0x02260100u, 0x0d0e0800u,
    0x0e08050bu, 0x1800090eu, 0x3133262cu, 0x35293134u, 0x33311300u, 0x00000632u,
);
// Lines: (first char, length, column, row)
const BS_LINES = array<vec4<i32>, 12>(
    vec4<i32>(0, 9, 35, 0), //  Windows 
    vec4<i32>(9, 61, 4, 2), // A fatal groove has occurred at 0028:C0FFEE in VXD DRUMS(01) +
    vec4<i32>(70, 45, 4, 3), // 00BEA7. The current tempo will be terminated.
    vec4<i32>(115, 47, 4, 5), // *  Press any key to terminate the current solo.
    vec4<i32>(162, 61, 4, 6), // *  Press CTRL+ALT+DEL again to restart your drummer. You will
    vec4<i32>(223, 46, 4, 7), //    lose any unsaved fills in all applications.
    vec4<i32>(269, 32, 24, 9), // Press any key to keep drumming _
    vec4<i32>(301, 7, 0, 0), // C:\>win
    vec4<i32>(308, 9, 0, 0), // Microsoft
    vec4<i32>(317, 7, 0, 0), // Windows
    vec4<i32>(324, 11, 0, 0), // Version 3.1
    vec4<i32>(335, 39, 0, 0), // Copyright (c) 1985-1992 Microsoft Corp.
);
// Text rows of the crash screen -> line index (-1 = empty)
const BS_ROW_LINE = array<i32, 10>(0, -1, 1, 2, -1, 3, 4, 5, -1, 6);

const BS_BLUE = vec3<f32>(0.0, 0.0, 0.667);
const BS_GRAY = vec3<f32>(0.667, 0.667, 0.667);
const BS_WHITE = vec3<f32>(1.0, 1.0, 1.0);
const BS_GLITCH = 0.32;     // seconds of glitch before the crash screen

fn bs_hash(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn bs_tap(uv: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(se_input, se_sampler, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
}

// Pixel (x, y) of glyph g (0 = blank) in its 8x16 cell.
fn bs_glyph(g: u32, x: i32, y: i32) -> bool {
    if (g == 0u || x < 0 || x > 7 || y < 0 || y > 15) {
        return false;
    }
    let w = BS_FONT[(g - 1u) * 4u + u32(y >> 2)];
    let row = (w >> (u32(y & 3) * 8u)) & 0xffu;
    return ((row >> u32(7 - x)) & 1u) == 1u;
}

// Glyph at character `i` of line `l` (0 outside the line).
fn bs_char(l: i32, i: i32) -> u32 {
    let ln = BS_LINES[l];
    if (i < 0 || i >= ln.y) {
        return 0u;
    }
    let k = u32(ln.x + i);
    return (BS_TEXT[k >> 2u] >> ((k & 3u) * 8u)) & 0xffu;
}

// Is the text of line `l`, scaled `z` dots per font pixel, lit at dot `p` (relative to its
// top-left corner)?
fn bs_text(l: i32, p: vec2<i32>, z: i32) -> bool {
    if (p.x < 0 || p.y < 0 || p.y >= 16 * z) {
        return false;
    }
    let q = p / z;
    return bs_glyph(bs_char(l, q.x >> 3), q.x & 7, q.y);
}

// Text-mode grid: dot size, columns/rows and the pixel origin of cell (0, 0).
struct BsGrid {
    d: f32,
    cols: i32,
    rows: i32,
    origin: vec2<f32>,
};

fn bs_grid() -> BsGrid {
    var g: BsGrid;
    g.d = max(1.0, round(se.resolution.y / 360.0));
    g.cols = i32(floor(se.resolution.x / (8.0 * g.d)));
    g.rows = i32(floor(se.resolution.y / (16.0 * g.d)));
    g.origin = floor((se.resolution - vec2<f32>(f32(g.cols) * 8.0, f32(g.rows) * 16.0) * g.d) * 0.5);
    return g;
}

// The crash screen at pixel `px`. `blink` = cursor visible.
fn bs_crash(px: vec2<f32>, g: BsGrid, blink: bool) -> vec3<f32> {
    let dot = vec2<i32>(floor((px - g.origin) / g.d));
    let cell = vec2<i32>(floor(vec2<f32>(dot) / vec2<f32>(8.0, 16.0)));
    let top = (g.rows - 10) / 2;
    let r = cell.y - top;
    if (r < 0 || r > 9 || dot.x < 0) {
        return BS_BLUE;
    }
    let l = BS_ROW_LINE[r];
    if (l < 0) {
        return BS_BLUE;
    }
    let ln = BS_LINES[l];
    let i = cell.x - (ln.z + (g.cols - 80) / 2);
    let local = dot - cell * vec2<i32>(8, 16);
    var on = bs_glyph(bs_char(l, i), local.x, local.y);
    // the trailing underscore of the last line is the blinking cursor
    if (l == 6 && i == ln.y - 1) {
        on = on && blink;
    }
    if (l == 0) {
        // heading in inverse video
        let inside = i >= 0 && i < ln.y;
        return select(BS_BLUE, select(BS_GRAY, BS_BLUE, on), inside);
    }
    return select(BS_BLUE, select(BS_GRAY, BS_WHITE, l == 6), on);
}

// Glitch frames before the crash: torn rows, RGB split, copied/crushed blocks. `k` ramps 0..1.
fn bs_glitch(uv: vec2<f32>, px: vec2<f32>, k: f32, step: f32, d: f32) -> vec3<f32> {
    let kick = s_band_kick();
    // torn rows: random-height bands slide sideways, more and further as k grows
    let bh = d * (4.0 + floor(bs_hash(vec2<f32>(step, 3.1)) * 24.0));
    let band = floor(px.y / bh);
    let hb = bs_hash(vec2<f32>(band, step));
    let torn = select(0.0, 1.0, hb < 0.25 + 0.5 * k);
    var u = uv;
    u.x = u.x + torn * (bs_hash(vec2<f32>(band, step + 7.0)) - 0.5) * (0.04 + 0.16 * k + 0.08 * kick);
    // corrupted blocks: copy a block from elsewhere and crush it to a few colours
    let bs = 16.0 * d;
    let cell = floor(px / bs);
    let hc = bs_hash(cell + vec2<f32>(step * 1.7, 5.0));
    var crush = false;
    if (hc < 0.14 * k) {
        let src_cell = vec2<f32>(bs_hash(cell + step), bs_hash(cell.yx + step + 2.0));
        u = src_cell + fract(px / bs) * bs / se.resolution * vec2<f32>(4.0, 1.0);
        crush = true;
    }
    // RGB split along x
    let o = vec2<f32>((0.004 + 0.012 * k) * (1.0 + 2.0 * kick), 0.0);
    var c = vec3<f32>(bs_tap(u + o).r, bs_tap(u).g, bs_tap(u - o).b);
    if (crush) {
        c = floor(c * 2.0 + 0.5) / 2.0;
        c = select(c, c.bgr, hc < 0.05 * k);
    }
    // a few rolling dark scanlines
    let roll = fract(px.y / (se.resolution.y * 0.37) - step * 0.11);
    c = c * (1.0 - 0.35 * k * smoothstep(0.9, 1.0, roll));
    return c;
}

// The Windows 3.1 flag: four waving panes with the trail of little squares on the left.
// `p` is in dots relative to the flag's top-left corner (flag is 96 x 80 dots).
fn bs_flag(p: vec2<f32>) -> vec4<f32> {
    // trailing squares, three columns shrinking toward the left
    if (p.x < 0.0) {
        let col = floor(-p.x / 12.0);
        let row = floor(p.y / 10.0);
        if (col > 2.0 || row < 0.0 || row > 7.0) {
            return vec4<f32>(0.0);
        }
        let wave = 5.0 * sin(-(col + 0.5) * 0.35 + 0.6);
        let cy = (row + 0.5) * 10.0 + wave;
        let side = 7.0 - 2.0 * col;
        let lx = -p.x - col * 12.0 - 6.0;
        if (abs(lx) * 2.0 < side && abs(p.y - cy) * 2.0 < side && bs_hash(vec2<f32>(col, row)) < 0.75 - 0.15 * col) {
            return vec4<f32>(select(vec3<f32>(0.08, 0.35, 0.95), vec3<f32>(0.95, 0.2, 0.12), row < 4.0), 1.0);
        }
        return vec4<f32>(0.0);
    }
    let fx = p.x / 96.0;
    // the whole flag waves: rows bend with x
    let bend = 7.0 * sin(fx * 3.4 + 0.6) - 7.0 * sin(0.6);
    let y = p.y - bend;
    if (fx > 1.0 || y < 0.0 || y > 80.0) {
        return vec4<f32>(0.0);
    }
    let gapx = 48.0 + 2.0 * sin(y * 0.08);
    if (abs(p.x - gapx) < 2.5 || abs(y - 40.0) < 2.5) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    let right = p.x > gapx;
    let low = y > 40.0;
    var c = select(select(vec3<f32>(0.95, 0.2, 0.12), vec3<f32>(0.2, 0.75, 0.2), right),
                   select(vec3<f32>(0.08, 0.35, 0.95), vec3<f32>(1.0, 0.8, 0.1), right), low);
    // a bit of shading along the wave
    c = c * (0.8 + 0.2 * cos(fx * 3.4 + 0.6));
    return vec4<f32>(c, 1.0);
}

// The boot logo, revealed left to right by `sweep` (0..1). `px` in pixels.
fn bs_logo(px: vec2<f32>, g: BsGrid, sweep: f32) -> vec3<f32> {
    // logo block: flag 96x80 dots, a 24-dot gap, then the text column (Windows at 3x = 168 dots)
    let size = vec2<f32>(36.0 + 96.0 + 24.0 + 168.0, 80.0);
    let lo = floor(se.resolution * 0.5 / g.d - size * 0.5 - vec2<f32>(0.0, 16.0));
    let dot = floor(px / g.d) - lo;
    let x_reveal = -40.0 + sweep * (size.x + 80.0);
    if (dot.x > x_reveal) {
        return vec3<f32>(0.0);
    }
    var c = vec3<f32>(0.0);
    let flag = bs_flag(dot - vec2<f32>(36.0, 0.0));
    if (flag.a > 0.5) {
        c = flag.rgb;
    }
    let t0 = vec2<i32>(dot - vec2<f32>(156.0, 0.0));
    if (bs_text(8, t0 - vec2<i32>(0, 6), 1) || bs_text(10, t0 - vec2<i32>(0, 72), 1)) {
        c = BS_WHITE;
    }
    if (bs_text(9, t0 - vec2<i32>(0, 22), 3)) {
        c = BS_WHITE;
    }
    if (bs_text(11, vec2<i32>(dot) - vec2<i32>(i32(size.x * 0.5) - 156, 112), 1)) {
        c = BS_GRAY;
    }
    // bright leading edge of the sweep
    let edge = x_reveal - dot.x;
    if (sweep < 1.0 && dot.y > -24.0 && dot.y < 136.0) {
        c = c + vec3<f32>(0.55, 0.75, 1.0) * (step(edge, 2.0) + 0.35 * exp(-edge * 0.15));
    }
    return c;
}

const BS_BAYER = array<f32, 16>(0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0);

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let src = bs_tap(in.uv);
    let env = clamp(se.env, 0.0, 1.0);
    let age = se.trigger_age;
    let amount = clamp(p_amount(), 0.0, 1.0);
    if (env <= 0.0 || amount <= 0.0) {
        return src;
    }
    let px = in.uv * se.resolution;
    let g = bs_grid();
    let frame_step = floor(age / 0.034);
    // cursor blinks on the beat (or at the classic ~3.7 Hz without a tempo)
    let blink = select(fract(age * 3.7) < 0.5, s_beat_phase() < 0.5, s_beat_bpm() > 1.0);

    let closing = env < 1.0 && age > 0.2;
    var c = vec3<f32>(0.0);
    var mixback = 0.0;     // share of the live picture showing through at the end
    if (!closing) {
        if (age < BS_GLITCH) {
            let k = smoothstep(0.0, BS_GLITCH, age);
            c = bs_glitch(in.uv, px, k, frame_step, g.d);
            // the crash screen flickers through in the last glitch frames
            if (age > BS_GLITCH - 0.12 && bs_hash(vec2<f32>(frame_step, 9.0)) < 0.55) {
                c = bs_crash(px, g, blink);
            }
        } else {
            c = bs_crash(px, g, blink);
        }
    } else {
        let r = 1.0 - env;
        if (r < 0.12) {
            // CRT power-off: squash to a bright line, then to a dot
            let sy = max(1.0 - r / 0.07, 0.004);
            let sx = select(1.0, max(1.0 - (r - 0.07) / 0.05, 0.003), r > 0.07);
            let ctr = se.resolution * 0.5;
            let q = (px - ctr) / vec2<f32>(sx, sy) + ctr;
            let inside = abs(px.y - ctr.y) <= max(se.resolution.y * 0.5 * sy, g.d) && abs(px.x - ctr.x) <= max(se.resolution.x * 0.5 * sx, g.d);
            if (inside) {
                let glow = 1.0 + 3.0 * smoothstep(0.02, 0.07, r);
                c = min(mix(bs_crash(q, g, blink), BS_WHITE, smoothstep(0.03, 0.08, r)) * glow, vec3<f32>(1.0));
            }
        } else if (r < 0.3) {
            // DOS prompt typing `win` with a fast-blinking underline cursor
            let dot = vec2<i32>(floor((px - g.origin) / g.d)) - vec2<i32>(16, 16);
            let typed = clamp(i32(floor((r - 0.13) / 0.12 * 3.0)) + 4, 4, 7);
            let cell = dot.x >> 3;
            if (cell < typed && bs_text(7, dot, 1)) {
                c = BS_GRAY;
            }
            let cur = dot - vec2<i32>(typed * 8, 0);
            if (cur.x >= 0 && cur.x < 8 && (cur.y == 13 || cur.y == 14) && fract(age * 4.0) < 0.5) {
                c = BS_GRAY;
            }
        } else {
            let sweep = clamp((r - 0.3) / 0.32, 0.0, 1.0);
            c = bs_logo(px, g, sweep);
            // dither the live picture back in over the last part of the release
            let t = clamp((r - 0.74) / 0.24, 0.0, 1.0);
            let cell = vec2<u32>(floor(px / (2.0 * g.d)));
            let th = (BS_BAYER[(cell.x & 3u) + (cell.y & 3u) * 4u] + 0.5) / 16.0;
            c = c * (1.0 - 0.6 * t);
            mixback = select(0.0, 1.0, t > th);
        }
    }
    let fx = vec4<f32>(c * src.a, src.a);
    return mix(src, fx, amount * (1.0 - mixback));
}
