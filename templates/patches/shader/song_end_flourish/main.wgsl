// Song end flourish: the last hit of a song as a movie ending. With the trigger envelope the
// picture takes a small punch and then pushes in slowly toward `focus`, colour drains a little,
// film grain and a soft vignette come up, and black letterbox bars slide in from top and bottom.
// A Windows 3.1 "Song Complete" dialog pops in with a springy scale: a beating pixel heart,
// "Thank you!" in double-size bold System font, "See you next time." and a default OK button.
// It stays for the whole hold; on release the OK button clicks down, the dialog collapses with
// checkered zoom rectangles and everything else eases back out with the envelope.
// Input alpha is preserved (premultiplied); the dialog is drawn crisp on top.

// Bitmap font, only the glyphs used: 'SongCmpletThakyu!xi.OK'. 8 rows per glyph (row 7 =
// descender), bit 4 = leftmost column.
const FONT = array<u32, 176>(
    15u, 16u, 16u, 14u, 1u, 1u, 30u, 0u, // 'S'
    0u, 0u, 14u, 17u, 17u, 17u, 14u, 0u, // 'o'
    0u, 0u, 22u, 25u, 17u, 17u, 17u, 0u, // 'n'
    0u, 0u, 15u, 17u, 17u, 15u, 1u, 14u, // 'g'
    14u, 17u, 16u, 16u, 16u, 17u, 14u, 0u, // 'C'
    0u, 0u, 26u, 21u, 21u, 21u, 21u, 0u, // 'm'
    0u, 0u, 30u, 17u, 17u, 30u, 16u, 16u, // 'p'
    16u, 16u, 16u, 16u, 16u, 16u, 16u, 0u, // 'l'
    0u, 0u, 14u, 17u, 31u, 16u, 14u, 0u, // 'e'
    0u, 8u, 30u, 8u, 8u, 8u, 6u, 0u, // 't'
    31u, 4u, 4u, 4u, 4u, 4u, 4u, 0u, // 'T'
    16u, 16u, 22u, 25u, 17u, 17u, 17u, 0u, // 'h'
    0u, 0u, 14u, 1u, 15u, 17u, 15u, 0u, // 'a'
    16u, 16u, 18u, 20u, 24u, 20u, 18u, 0u, // 'k'
    0u, 0u, 17u, 17u, 17u, 15u, 1u, 14u, // 'y'
    0u, 0u, 17u, 17u, 17u, 19u, 13u, 0u, // 'u'
    16u, 16u, 16u, 16u, 16u, 0u, 16u, 0u, // '!'
    0u, 0u, 17u, 10u, 4u, 10u, 17u, 0u, // 'x'
    16u, 0u, 16u, 16u, 16u, 16u, 16u, 0u, // 'i'
    0u, 0u, 0u, 0u, 0u, 0u, 16u, 0u, // '.'
    14u, 17u, 17u, 17u, 17u, 17u, 14u, 0u, // 'O'
    17u, 18u, 20u, 24u, 20u, 18u, 17u, 0u, // 'K'
);
const GLYPH_W = array<u32, 22>(5u, 5u, 5u, 5u, 5u, 5u, 5u, 1u, 5u, 4u, 5u, 5u, 5u, 4u, 5u, 5u, 1u, 5u, 1u, 1u, 5u, 5u);
// Text runs: glyph index | x offset << 8 (bold advance = width + 2, plain = width + 1).
const RUNS = array<u32, 38>(
    0x0u, 0x701u, 0xe02u, 0x1503u, 0x2004u, 0x2701u, 0x2e05u, 0x3506u, 0x3c07u, 0x3f08u, 0x4609u, 0x4c08u, // Song Complete
    0xau, 0x70bu, 0xe0cu, 0x1502u, 0x1c0du, 0x260eu, 0x2d01u, 0x340fu, 0x3b10u, // Thank you!
    0x0u, 0x608u, 0xc08u, 0x150eu, 0x1b01u, 0x210fu, 0x2a02u, 0x3008u, 0x3611u, 0x3c09u, 0x4409u, 0x4912u, 0x4b05u, 0x5108u, 0x5713u, // See you next time.
    0x14u, 0x715u, // OK
);
// start, glyph count, pixel width
const T_CAPTION = vec3<u32>(0u, 12u, 82u);
const T_THANKS = vec3<u32>(12u, 9u, 61u);
const T_SEE = vec3<u32>(21u, 15u, 88u);
const T_OK = vec3<u32>(36u, 2u, 13u);

const BLACK = vec3<f32>(0.0);
const WHITE = vec3<f32>(1.0);
const GRAY = vec3<f32>(0.753);
const DARK = vec3<f32>(0.502);
const NAVY = vec3<f32>(0.0, 0.0, 0.502);

const BORDER = 5;  // black, 3 navy (active modal frame), black
const TITLE = 18;  // caption height, then a 1-pixel black rule
const CLIENT_H = 92;
const WIN = vec2<i32>(206, 116); // 2 * BORDER + TITLE + 1 + CLIENT_H
const BTN = vec2<i32>(64, 22);
const POP_AT = 0.3;

fn glyph_on(g: u32, x: i32, y: i32) -> bool {
    if (x < 0 || y < 0 || y > 7 || x >= i32(GLYPH_W[g])) {
        return false;
    }
    return ((FONT[g * 8u + u32(y)] >> u32(4 - x)) & 1u) == 1u;
}

// Text run; `bold` doubles every glyph pixel one to the right (System font look).
fn text_on(t: vec3<u32>, p: vec2<i32>, bold: bool) -> bool {
    if (p.y < 0 || p.y > 7 || p.x < 0 || p.x > i32(t.z)) {
        return false;
    }
    for (var i = 0u; i < t.y; i++) {
        let e = RUNS[t.x + i];
        let g = e & 0xffu;
        let lx = p.x - i32(e >> 8u);
        if (glyph_on(g, lx, p.y) || (bold && glyph_on(g, lx - 1, p.y))) {
            return true;
        }
    }
    return false;
}

fn in_box(p: vec2<i32>, lo: vec2<i32>, size: vec2<i32>) -> bool {
    return all(p >= lo) && all(p < lo + size);
}

// 32x32 pixel heart that thumps (lub-dub) about once a second; rgb + coverage.
fn heart(p: vec2<i32>, time: f32) -> vec4<f32> {
    let ph = fract(time * 1.1);
    let thump = exp(-ph * 14.0) + 0.6 * exp(-max(ph - 0.18, 0.0) * 14.0) * step(0.18, ph);
    let k = 13.0 * (1.0 + 0.1 * min(thump, 1.0));
    // classic implicit heart, y up
    let q = vec2<f32>(f32(p.x) + 0.5 - 16.0, 15.0 - (f32(p.y) + 0.5)) / k;
    let a = q.x * q.x + q.y * q.y - 1.0;
    let f = a * a * a - q.x * q.x * q.y * q.y * q.y;
    if (f > 0.0) {
        return vec4<f32>(0.0);
    }
    // outline: a pixel whose right/lower neighbour is outside
    let qn = vec2<f32>(f32(p.x) + 1.5 - 16.0, 15.0 - (f32(p.y) + 0.5)) / k;
    let qd = vec2<f32>(f32(p.x) + 0.5 - 16.0, 15.0 - (f32(p.y) + 1.5)) / k;
    let qw = vec2<f32>(f32(p.x) - 0.5 - 16.0, 15.0 - (f32(p.y) + 0.5)) / k;
    let qu = vec2<f32>(f32(p.x) + 0.5 - 16.0, 15.0 - (f32(p.y) - 0.5)) / k;
    var edge = false;
    for (var i = 0; i < 4; i++) {
        var n = qn;
        if (i == 1) { n = qd; }
        if (i == 2) { n = qw; }
        if (i == 3) { n = qu; }
        let an = n.x * n.x + n.y * n.y - 1.0;
        edge = edge || (an * an * an - n.x * n.x * n.y * n.y * n.y > 0.0);
    }
    if (edge) {
        return vec4<f32>(BLACK, 1.0);
    }
    // shine on the upper left lobe
    if (q.x < -0.35 && q.x > -0.75 && q.y > 0.35 && q.y < 0.75) {
        return vec4<f32>(vec3<f32>(1.0, 0.75, 0.78), 1.0);
    }
    return vec4<f32>(select(vec3<f32>(0.9, 0.05, 0.15), vec3<f32>(0.6, 0.0, 0.08), q.y < -0.45), 1.0);
}

fn dialog(p: vec2<i32>, pressed: bool) -> vec3<f32> {
    if (p.x < 1 || p.y < 1 || p.x >= WIN.x - 1 || p.y >= WIN.y - 1) { return BLACK; }
    if (p.x < BORDER - 1 || p.y < BORDER - 1 || p.x >= WIN.x - BORDER + 1 || p.y >= WIN.y - BORDER + 1) { return NAVY; }
    if (p.x < BORDER || p.y < BORDER || p.x >= WIN.x - BORDER || p.y >= WIN.y - BORDER) { return BLACK; }
    let q = p - vec2<i32>(BORDER);
    let inner_w = WIN.x - 2 * BORDER;
    if (q.y < TITLE) {
        if (q.x < 19) {
            if (q.x == 18) { return BLACK; }
            if (q.x >= 5 && q.x < 14 && q.y == 8) { return WHITE; }
            if (q.x >= 4 && q.x < 15 && q.y >= 7 && q.y < 11) { return BLACK; }
            return GRAY;
        }
        let tx = 19 + (inner_w - 19 - i32(T_CAPTION.z)) / 2;
        if (text_on(T_CAPTION, q - vec2<i32>(tx, 5), true)) {
            return WHITE;
        }
        return NAVY;
    }
    if (q.y == TITLE) { return BLACK; }
    let c = q - vec2<i32>(0, TITLE + 1);
    if (in_box(c, vec2<i32>(14, 12), vec2<i32>(32))) {
        let h = heart(c - vec2<i32>(14, 12), se.time);
        if (h.a > 0.5) {
            return h.rgb;
        }
    }
    // "Thank you!" at double size, then the plain second line
    let big = c - vec2<i32>(58, 12);
    if (big.x >= 0 && big.y >= 0 && text_on(T_THANKS, big / 2, true)) {
        return BLACK;
    }
    if (text_on(T_SEE, c - vec2<i32>(58, 34), false)) {
        return BLACK;
    }
    // Default push button: 2-pixel black frame with clipped corners, 2-pixel bevel, dotted focus.
    let b0 = vec2<i32>((inner_w - BTN.x) / 2, CLIENT_H - 10 - BTN.y);
    let b = c - b0;
    if (all(b >= vec2<i32>(0)) && all(b < BTN)) {
        let e = min(b, BTN - 1 - b);
        if (e.x == 0 && e.y == 0) { return GRAY; }
        if (min(e.x, e.y) < 2) { return BLACK; }
        let push = select(0, 1, pressed);
        let t = BTN - vec2<i32>(13, 7);
        let tp = b - t / 2 - vec2<i32>(push);
        if (text_on(T_OK, tp, true)) { return BLACK; }
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

fn hash(p: vec2<f32>, n: f32) -> f32 {
    return fract(sin(dot(p, vec2<f32>(12.9898, 78.233)) + n * 0.618) * 43758.5453);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let env = clamp(se.env, 0.0, 1.0);
    if (env <= 0.0) {
        return textureSampleLevel(se_input, se_sampler, in.uv, 0.0);
    }
    let age = max(se.trigger_age, 0.0);
    let e = env * env * (3.0 - 2.0 * env);
    let amt = clamp(p_amount(), 0.0, 1.0) * e;

    // node-local coordinates
    let r0 = se.region.xy;
    let rs = max(se.region.zw - se.region.xy, vec2<f32>(1e-4));
    let local = (in.uv - r0) / rs;
    let frag = in.uv * se.resolution;

    // letterbox bars slide in over ~0.7 s and leave with the envelope
    let bar_h = max(p_bars(), 0.0) * amt * smoothstep(0.0, 0.7, age);
    if (local.y < bar_h || local.y > 1.0 - bar_h) {
        let a = textureSampleLevel(se_input, se_sampler, in.uv, 0.0).a;
        var out = vec4<f32>(0.0, 0.0, 0.0, a);
        out = draw_dialog(out, frag, env, age);
        return out;
    }

    // a small punch on the hit, then the slow push-in toward `focus`
    let push = max(p_zoom(), 0.0) * (1.0 - exp(-age / 2.5)) + 0.025 * exp(-age * 7.0);
    let z = 1.0 + push * amt;
    // zooming about a point inside the node keeps every sample inside it (no edge exposure)
    let focus = clamp(p_focus(), vec2<f32>(0.0), vec2<f32>(1.0));
    let zl = focus + (local - focus) / z;
    let src = textureSampleLevel(se_input, se_sampler, r0 + zl * rs, 0.0);
    let base_a = textureSampleLevel(se_input, se_sampler, in.uv, 0.0).a;

    let a = max(src.a, 1e-4);
    var rgb = src.rgb / a;
    let luma = dot(rgb, vec3<f32>(0.299, 0.587, 0.114));
    rgb = mix(rgb, vec3<f32>(luma), clamp(p_desaturate(), 0.0, 1.0) * amt);
    // gentle filmic fade: lifted blacks, softened highlights, faint warm-cool split
    rgb = mix(rgb, rgb * 0.92 + vec3<f32>(0.035, 0.03, 0.04), amt);
    // vignette
    let vd = length((local - 0.5) * vec2<f32>(1.0, 0.8));
    rgb *= 1.0 - 0.35 * amt * smoothstep(0.3, 0.75, vd);
    // grain: ~2-pixel clumps, new every frame, strongest in the mid-tones
    let gs = max(1.0, se.resolution.y / 540.0);
    let gp = floor(frag / gs);
    let n = hash(gp, f32(se.frame % 977u)) + hash(gp + 17.0, f32(se.frame % 613u)) - 1.0;
    rgb += n * max(p_grain(), 0.0) * amt * (0.4 + 2.4 * luma * (1.0 - luma));
    // graded colour of the zoomed sample, alpha of the input at this pixel
    var out = vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)) * base_a, base_a);
    out = draw_dialog(out, frag, env, age);
    return out;
}

// Dialog on top: springy pop after POP_AT; on release the OK button goes down, then checkered
// zoom rectangles collapse it with the envelope.
fn draw_dialog(under: vec4<f32>, frag: vec2<f32>, env: f32, age: f32) -> vec4<f32> {
    if (!p_dialog() || age < POP_AT) {
        return under;
    }
    let s = max(1.0, round(se.resolution.y / 360.0 * p_scale()));
    let size_px = vec2<f32>(WIN) * s;
    let center = floor(clamp(p_position() * se.resolution, size_px * 0.5, max(se.resolution - size_px * 0.5, size_px * 0.5)));
    let closing = env < 1.0 && age > POP_AT + 0.3;
    let pressed = closing && env > 0.75;
    if (closing && !pressed) {
        let open = smoothstep(0.05, 0.75, env);
        let p = vec2<i32>(floor((frag - center) / s + vec2<f32>(WIN) * 0.5));
        let cell = vec2<i32>(floor(frag / s));
        let checker = ((cell.x + cell.y) & 1) == 0;
        let full_hi = vec2<f32>(WIN);
        let mid = floor(full_hi * 0.5);
        for (var k = 0; k < 3; k++) {
            let t = open - f32(k) * 0.16;
            if (t <= 0.0) {
                continue;
            }
            let ee = t * t * (3.0 - 2.0 * t);
            let lo = vec2<i32>(floor(mix(mid - 6.0, vec2<f32>(0.0), ee)));
            let hi = vec2<i32>(floor(mix(mid + 6.0, full_hi, ee)));
            let d = min(p - lo, hi - 1 - p);
            if (all(p >= lo) && all(p < hi) && min(d.x, d.y) < 2) {
                return vec4<f32>(select(BLACK, WHITE, checker), 1.0);
            }
        }
        return under;
    }
    // pop: springy scale from nothing, settling at exactly 1 (crisp pixels)
    let u = age - POP_AT;
    let k = select(1.0 - exp(-9.0 * u) * cos(14.0 * u), 1.0, u > 1.2);
    if (k < 0.05) {
        return under;
    }
    let p = vec2<i32>(floor((frag - center) / (s * k) + vec2<f32>(WIN) * 0.5));
    if (all(p >= vec2<i32>(0)) && all(p < WIN)) {
        return vec4<f32>(dialog(p, pressed), 1.0);
    }
    // hard drop shadow
    let sp = p - vec2<i32>(6, 6);
    if (all(sp >= vec2<i32>(0)) && all(sp < WIN)) {
        return under * 0.5;
    }
    return under;
}
