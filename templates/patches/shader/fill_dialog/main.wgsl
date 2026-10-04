// Fill dialog: one Windows 3.1 message box (modal frame, navy caption, system menu box, icon,
// bold System-style text, default OK button). It zooms open with checkered "XOR" rectangles right
// after the trigger; on release the OK button clicks down and the rectangles collapse. Everything
// is drawn on an integer grid of "dialog pixels" (`s` screen pixels each), so edges stay hard.

// Bold-ready font, only the glyphs used below: up to 7 columns x 8 rows (row 7 = descender), bits row*7+col; x: rows 0-3, y: rows 4-7.
const GLYPHS = array<vec2<u32>, 32>(
    vec2<u32>(0x204081u, 0x4001u), // '!'
    vec2<u32>(0x0u, 0xc180u), // '.'
    vec2<u32>(0xc180u, 0x183u), // ':'
    vec2<u32>(0x1e0409fu, 0x7c081u), // 'E'
    vec2<u32>(0x3a0488eu, 0x78891u), // 'G'
    vec2<u32>(0x614491u, 0x44485u), // 'K'
    vec2<u32>(0x224488eu, 0x38891u), // 'O'
    vec2<u32>(0x1e4488fu, 0x4081u), // 'P'
    vec2<u32>(0x1c0488eu, 0x38890u), // 'S'
    vec2<u32>(0x81021fu, 0x10204u), // 'T'
    vec2<u32>(0x2a44891u, 0x44d95u), // 'W'
    vec2<u32>(0x2038000u, 0x7889eu), // 'a'
    vec2<u32>(0x2634081u, 0x3c891u), // 'b'
    vec2<u32>(0x238000u, 0x38881u), // 'c'
    vec2<u32>(0x3258810u, 0x78891u), // 'd'
    vec2<u32>(0x2238000u, 0x3809fu), // 'e'
    vec2<u32>(0xe0890cu, 0x8102u), // 'f'
    vec2<u32>(0x2278000u, 0x1c40f11u), // 'g'
    vec2<u32>(0x2634081u, 0x44891u), // 'h'
    vec2<u32>(0x40c002u, 0x1c102u), // 'i'
    vec2<u32>(0xa24081u, 0x24283u), // 'k'
    vec2<u32>(0x408103u, 0x1c102u), // 'l'
    vec2<u32>(0x92fc000u, 0x1264c9u), // 'm'
    vec2<u32>(0x2634000u, 0x44891u), // 'n'
    vec2<u32>(0x2238000u, 0x38891u), // 'o'
    vec2<u32>(0x2634000u, 0x4081u), // 'r'
    vec2<u32>(0x278000u, 0x3c80eu), // 's'
    vec2<u32>(0x41c102u, 0x30902u), // 't'
    vec2<u32>(0x2244000u, 0x58c91u), // 'u'
    vec2<u32>(0x2244000u, 0x10511u), // 'v'
    vec2<u32>(0x2244000u, 0x28a95u), // 'w'
    vec2<u32>(0x1444000u, 0x44504u), // 'x'
);
const GLYPH_W = array<u32, 32>(1u, 2u, 2u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 3u, 4u, 3u, 7u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 5u);
// Text runs: glyph index | x offset << 8 (font pixels; advance = glyph width + bold + gap).
const RUNS = array<u32, 87>(
    0x8u, 0x71bu, 0xe19u, 0x150fu, 0x1c0bu, 0x2316u, 0x3003u, 0x3717u, 0x3e11u, 0x4513u, 0x4a17u, 0x510fu, // Stream Engine
    0x6u, 0x705u, // OK
    0x8u, 0x709u, 0xe06u, 0x1507u, // STOP
    0x3u, 0x71fu, 0xe0du, 0x150fu, 0x1c15u, 0x2115u, 0x260fu, 0x2d17u, 0x341bu, 0x3f10u, 0x4613u, 0x4b15u, 0x5015u, 0x590eu, 0x600fu, 0x671bu, 0x6e0fu, 0x750du, 0x7c1bu, 0x830fu, 0x8a0eu, 0x9101u, // Excellent fill detected.
    0x4u, 0x719u, 0xe18u, 0x1518u, 0x1c1du, 0x230fu, 0x2e18u, 0x351du, 0x3c0fu, 0x4319u, 0x4a10u, 0x5115u, 0x5618u, 0x5d1eu, 0x6400u, // Groove overflow!
    0x9u, 0x718u, 0xe18u, 0x1916u, 0x221cu, 0x290du, 0x3012u, 0x3b0du, 0x4218u, 0x491eu, 0x500cu, 0x570fu, 0x5e15u, 0x6315u, 0x6801u, // Too much cowbell.
    0x8u, 0x713u, 0xc0du, 0x1314u, 0x1d0cu, 0x240fu, 0x2b0bu, 0x321bu, 0x3d0eu, 0x440fu, 0x4b1bu, 0x520fu, 0x590du, 0x601bu, 0x670fu, 0x6e0eu, 0x7501u, // Sick beat detected.
);
// Messages (enum order): run start, glyph count, pixel width, icon (0 info, 1 exclamation, 2 stop).
const MSG_START = array<u32, 4>(18u, 40u, 55u, 70u);
const MSG_COUNT = array<u32, 4>(22u, 15u, 15u, 17u);
const MSG_WIDTH = array<i32, 4>(148, 102, 107, 120);
const MSG_ICON = array<i32, 4>(0, 1, 2, 0);

const BLACK = vec3<f32>(0.0);
const WHITE = vec3<f32>(1.0);
const GRAY = vec3<f32>(0.753);
const DARK = vec3<f32>(0.502);
const NAVY = vec3<f32>(0.0, 0.0, 0.502);

const BORDER = 5;  // black, 3 navy (active modal frame), black
const TITLE = 19;  // caption height, then a 1-pixel black rule
const CLIENT_H = 94;
const BTN = vec2<i32>(64, 22);

fn glyph_on(g: u32, x: i32, y: i32) -> bool {
    let w = i32(GLYPH_W[g]);
    if (x < 0 || x >= w || y < 0 || y > 7) {
        return false;
    }
    let bit = u32(y * 7 + x);
    let word = GLYPHS[g];
    return select(((word.y >> (bit - 28u)) & 1u) == 1u, ((word.x >> bit) & 1u) == 1u, bit < 28u);
}

// Bold System-font look: every glyph pixel is doubled one pixel to the right.
fn run_on(start: u32, count: u32, p: vec2<i32>) -> bool {
    if (p.y < 0 || p.y > 7 || p.x < 0) {
        return false;
    }
    for (var i = 0u; i < count; i++) {
        let e = RUNS[start + i];
        let g = e & 0xffu;
        let lx = p.x - i32(e >> 8u);
        if (lx >= 0 && lx <= i32(GLYPH_W[g]) && (glyph_on(g, lx, p.y) || glyph_on(g, lx - 1, p.y))) {
            return true;
        }
    }
    return false;
}

fn in_box(p: vec2<i32>, lo: vec2<i32>, size: vec2<i32>) -> bool {
    return all(p >= lo) && all(p < lo + size);
}

// 32x32 message box icons on pixel centers: 0 information, 1 exclamation, 2 stop sign.
// Returns rgb + coverage (0 or 1).
fn icon(kind: i32, p: vec2<i32>) -> vec4<f32> {
    let c = vec2<f32>(p) + 0.5 - vec2<f32>(16.0);
    if (kind == 2) {
        let a = abs(c);
        let oct = max(max(a.x, a.y), (a.x + a.y) * 0.7071);
        if (oct > 15.0) {
            return vec4<f32>(0.0);
        }
        if (oct > 14.0 || (oct > 12.0 && oct <= 13.0)) {
            return vec4<f32>(select(BLACK, WHITE, oct <= 13.0), 1.0);
        }
        // "STOP" centered, plain (not bold) 1:1 glyphs.
        let tp = p - vec2<i32>(4, 12);
        var on = false;
        for (var i = 0u; i < 4u; i++) {
            let g = RUNS[14u + i] & 0xffu;
            on = on || glyph_on(g, tp.x - i32(i) * 6, tp.y);
        }
        return vec4<f32>(select(vec3<f32>(0.85, 0.0, 0.0), WHITE, on), 1.0);
    }
    let r = length(c);
    if (r > 15.0) {
        return vec4<f32>(0.0);
    }
    if (r > 13.8) {
        return vec4<f32>(BLACK, 1.0);
    }
    let fill = select(WHITE, vec3<f32>(1.0, 1.0, 0.0), kind == 1);
    let ink = select(vec3<f32>(0.0, 0.0, 1.0), BLACK, kind == 1);
    var mark = false;
    if (kind == 0) {
        // lowercase "i": dot, stem with a left serif, foot serif
        mark = in_box(p, vec2<i32>(14, 6), vec2<i32>(4, 4)) || in_box(p, vec2<i32>(14, 12), vec2<i32>(4, 13))
            || in_box(p, vec2<i32>(11, 12), vec2<i32>(3, 2)) || in_box(p, vec2<i32>(11, 23), vec2<i32>(10, 2));
    } else {
        // "!": tapered bar and a square dot
        let half = select(1, 2, p.y < 17);
        mark = (p.y >= 6 && p.y < 21 && p.x >= 16 - half && p.x < 16 + half) || in_box(p, vec2<i32>(14, 23), vec2<i32>(4, 4));
    }
    return vec4<f32>(select(fill, ink, mark), 1.0);
}

fn window(p: vec2<i32>, size: vec2<i32>, msg: i32, flash: bool, pressed: bool) -> vec3<f32> {
    // Modal frame: black / navy / black.
    if (p.x < 1 || p.y < 1 || p.x >= size.x - 1 || p.y >= size.y - 1) { return BLACK; }
    if (p.x < BORDER - 1 || p.y < BORDER - 1 || p.x >= size.x - BORDER + 1 || p.y >= size.y - BORDER + 1) { return NAVY; }
    if (p.x < BORDER || p.y < BORDER || p.x >= size.x - BORDER || p.y >= size.y - BORDER) { return BLACK; }
    let q = p - vec2<i32>(BORDER);
    let inner_w = size.x - 2 * BORDER;
    if (q.y < TITLE) {
        // System menu box (gray, black right edge, long bar with a black shadow).
        if (q.x < 19) {
            if (q.x == 18) { return BLACK; }
            if (q.x >= 5 && q.x < 14 && q.y == 8) { return WHITE; }
            if (q.x >= 4 && q.x < 15 && q.y >= 7 && q.y < 11) { return BLACK; }
            return GRAY;
        }
        let caption = select(NAVY, WHITE, flash);
        let tx = 19 + (inner_w - 19 - 87) / 2;
        if (run_on(0u, 12u, q - vec2<i32>(tx, 6))) {
            return select(WHITE, BLACK, flash);
        }
        return caption;
    }
    if (q.y == TITLE) { return BLACK; }
    let c = q - vec2<i32>(0, TITLE + 1);
    // Icon
    let ic = icon(MSG_ICON[msg], c - vec2<i32>(16, 14));
    if (in_box(c, vec2<i32>(16, 14), vec2<i32>(32)) && ic.a > 0.5) {
        return ic.rgb;
    }
    // Message text
    if (run_on(MSG_START[msg], MSG_COUNT[msg], c - vec2<i32>(62, 27))) {
        return BLACK;
    }
    // Default push button: 2-pixel black frame with clipped corners, 2-pixel bevel, dotted focus.
    let b0 = vec2<i32>((inner_w - BTN.x) / 2, CLIENT_H - 12 - BTN.y);
    let b = c - b0;
    if (all(b >= vec2<i32>(0)) && all(b < BTN)) {
        let e = min(b, BTN - 1 - b);
        if (e.x == 0 && e.y == 0) { return GRAY; }
        if (min(e.x, e.y) < 2) { return BLACK; }
        // Pressed: bevel replaced by a dark top/left edge, caption shifted down-right.
        let push = select(0, 1, pressed);
        let t = BTN - vec2<i32>(13, 7);
        let tp = b - t / 2 - vec2<i32>(push);
        if (run_on(12u, 2u, tp)) { return BLACK; }
        let f = tp + vec2<i32>(4, 3);
        let fsz = vec2<i32>(21, 13);
        if (all(f >= vec2<i32>(0)) && all(f < fsz) && (f.x == 0 || f.y == 0 || f.x == fsz.x - 1 || f.y == fsz.y - 1) && ((f.x + f.y) & 1) == 0) {
            return BLACK;
        }
        if (pressed) {
            return select(GRAY, DARK, min(b.x, b.y) < 3);
        }
        let light_d = min(b.x, b.y);
        let dark_d = min(BTN.x - 1 - b.x, BTN.y - 1 - b.y);
        if (min(light_d, dark_d) < 4) {
            return select(WHITE, DARK, dark_d < light_d);
        }
        return GRAY;
    }
    return GRAY;
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let env = clamp(se.env, 0.0, 1.0);
    if (env <= 0.0) {
        return vec4<f32>(0.0);
    }
    let msg = clamp(p_message(), 0, 3);
    let s = max(1.0, round(se.resolution.y / 360.0 * p_scale()));
    let size = vec2<i32>(max(MSG_WIDTH[msg] + 82, 210) + 2 * BORDER, CLIENT_H + TITLE + 1 + 2 * BORDER);
    let size_px = vec2<f32>(size) * s;
    // Whole-pixel origin, kept on screen.
    let center = p_position() * se.resolution;
    let origin = floor(clamp(center - size_px * 0.5, vec2<f32>(0.0), max(se.resolution - size_px, vec2<f32>(0.0))));
    let frag = in.uv * se.resolution;
    let p = vec2<i32>(floor((frag - origin) / s));

    // Open: zoom over a fixed 0.24 s from the trigger. Close (envelope falling): the OK button
    // goes down first, then the rectangles collapse with the envelope.
    let closing = env < 1.0 && se.trigger_age > 0.3;
    let pressed = closing && env > 0.8;
    let open = select(clamp(se.trigger_age / 0.24, 0.0, 1.0), select(smoothstep(0.05, 0.8, env), 1.0, pressed), closing);
    if (open >= 1.0) {
        if (any(p < vec2<i32>(0)) || any(p >= size)) {
            return vec4<f32>(0.0);
        }
        let flash = p_flash() && s_band_snare() > 0.6;
        return vec4<f32>(window(p, size, msg, flash, pressed), 1.0);
    }
    // Zoom rectangles: three checkered outlines growing from the window center, each lagging.
    let cell = vec2<i32>(floor(frag / s));
    let checker = ((cell.x + cell.y) & 1) == 0;
    let full_lo = vec2<f32>(0.0);
    let full_hi = vec2<f32>(size);
    let mid = floor(full_hi * 0.5);
    for (var k = 0; k < 3; k++) {
        let t = open - f32(k) * 0.16;
        if (t <= 0.0) {
            continue;
        }
        let e = t * t * (3.0 - 2.0 * t);
        let lo = vec2<i32>(floor(mix(mid - 6.0, full_lo, e)));
        let hi = vec2<i32>(floor(mix(mid + 6.0, full_hi, e)));
        let inside = all(p >= lo) && all(p < hi);
        let d = min(p - lo, hi - 1 - p);
        let edge = min(d.x, d.y) < 2;
        if (inside && edge) {
            return vec4<f32>(select(BLACK, WHITE, checker), 1.0);
        }
    }
    return vec4<f32>(0.0);
}
