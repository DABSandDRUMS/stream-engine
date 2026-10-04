// Freeze frame. State (feedback = "state"): rgb = the clean frozen frame; alpha (the still itself
// needs none) carries bookkeeping: texels (0..3, 0) hold the four bytes of the f32 se.time at the
// capture, every other texel the marker 0xA5. The first frame of a freeze (state cleared because
// the global instance did not run the frame before, so no marker) captures se_input; every later
// frame copies the state texel for texel (sampled at pixel centres, so it never softens). A new
// trigger while frozen only extends the hold: same still, the animation keeps running from the
// capture (so chat-rate retriggers never strobe). The
// display samples the still through a slow push-in towards `focus`, drains some colour, adds a
// vignette, animated film grain, a drifting VCR tracking band and a short shutter blink at the
// capture, and draws a Windows 3.1 "Media Player" window with a raised pause button and a
// blinking PAUSED. None of that is written back, so the still stays pristine for the whole hold.
// Release (envelope falling after the hold): the window collapses with checkered zoom rectangles
// and the live picture crossfades back in.
// The window is drawn on an integer grid of window pixels (`s` screen pixels each).

// 5x8 font (bit 4 = leftmost column, row 7 = descender), only the glyphs used:
// 0 M, 1 e, 2 d, 3 i, 4 a, 5 P, 6 l, 7 y, 8 r, 9 A, 10 U, 11 S, 12 E, 13 D
const FONT = array<u32, 112>(
    17u, 27u, 21u, 21u, 17u, 17u, 17u, 0u, // M
    0u, 0u, 14u, 17u, 31u, 16u, 14u, 0u, // e
    1u, 1u, 15u, 17u, 17u, 19u, 13u, 0u, // d
    8u, 0u, 24u, 8u, 8u, 8u, 28u, 0u, // i (3 wide)
    0u, 0u, 14u, 1u, 15u, 17u, 15u, 0u, // a
    30u, 17u, 17u, 30u, 16u, 16u, 16u, 0u, // P
    24u, 8u, 8u, 8u, 8u, 8u, 28u, 0u, // l (3 wide)
    0u, 0u, 17u, 17u, 19u, 13u, 1u, 14u, // y
    0u, 0u, 22u, 25u, 16u, 16u, 16u, 0u, // r
    14u, 17u, 17u, 31u, 17u, 17u, 17u, 0u, // A
    17u, 17u, 17u, 17u, 17u, 17u, 14u, 0u, // U
    15u, 16u, 16u, 14u, 1u, 1u, 30u, 0u, // S
    31u, 16u, 16u, 30u, 16u, 16u, 31u, 0u, // E
    30u, 17u, 17u, 17u, 17u, 17u, 30u, 0u, // D
);
const GLYPH_W = array<i32, 14>(5, 5, 5, 3, 5, 5, 3, 5, 5, 5, 5, 5, 5, 5);
// "Media Player" (255 = space), then "PAUSED"
const TEXT = array<u32, 18>(0u, 1u, 2u, 3u, 4u, 255u, 5u, 6u, 4u, 7u, 1u, 8u, 5u, 9u, 10u, 11u, 12u, 13u);
const TITLE_W = 76; // bold advance = glyph width + 2, space = 4, minus the last gap
const PAUSED_W = 41;

const BLACK = vec3<f32>(0.0);
const WHITE = vec3<f32>(1.0);
const GRAY = vec3<f32>(0.753);
const DARK = vec3<f32>(0.502);

const BORDER = 4; // black, 2 gray, black
const CAPTION = 13;
const INNER_W = 138;
const CLIENT_H = 34;
const SIZE = vec2<i32>(INNER_W + 2 * BORDER, CAPTION + 1 + CLIENT_H + 2 * BORDER);
const BTN_POS = vec2<i32>(10, 7);
const BTN = vec2<i32>(24, 20);

fn glyph_on(g: u32, x: i32, y: i32) -> bool {
    if (x < 0 || x >= GLYPH_W[g] || y < 0 || y > 7) {
        return false;
    }
    return ((FONT[g * 8u + u32(y)] >> u32(4 - x)) & 1u) == 1u;
}

// Bold System-font look: every glyph pixel doubled one pixel to the right.
fn text_on(start: u32, count: u32, p: vec2<i32>) -> bool {
    if (p.y < 0 || p.y > 7 || p.x < 0) {
        return false;
    }
    var x0 = 0;
    for (var i = 0u; i < count; i++) {
        let g = TEXT[start + i];
        if (g == 255u) {
            x0 += 4;
            continue;
        }
        let lx = p.x - x0;
        if (glyph_on(g, lx, p.y) || glyph_on(g, lx - 1, p.y)) {
            return true;
        }
        x0 += GLYPH_W[g] + 2;
    }
    return false;
}

fn in_box(p: vec2<i32>, lo: vec2<i32>, size: vec2<i32>) -> bool {
    return all(p >= lo) && all(p < lo + size);
}

fn window(p: vec2<i32>, caption: vec3<f32>, blink_on: bool) -> vec3<f32> {
    let e = min(p, SIZE - 1 - p);
    let edge = min(e.x, e.y);
    if (edge == 0 || edge == BORDER - 1) { return BLACK; }
    if (edge < BORDER) { return GRAY; }
    let q = p - vec2<i32>(BORDER);
    if (q.y < CAPTION) {
        // system menu box: gray, black right edge, white bar with a black outline
        if (q.x < CAPTION) {
            if (q.x == CAPTION - 1) { return BLACK; }
            if (q.x >= 3 && q.x < 10 && q.y == 6) { return WHITE; }
            if (q.x >= 2 && q.x < 11 && q.y >= 5 && q.y < 8) { return BLACK; }
            return GRAY;
        }
        let ink = select(BLACK, WHITE, dot(caption, vec3<f32>(0.299, 0.587, 0.114)) < 0.5);
        let tx = CAPTION + (INNER_W - CAPTION - TITLE_W) / 2;
        if (text_on(0u, 12u, q - vec2<i32>(tx, 3))) { return ink; }
        return caption;
    }
    if (q.y == CAPTION) { return BLACK; }
    let c = q - vec2<i32>(0, CAPTION + 1);
    // raised push button (black frame, clipped corners, white/dark bevel) with a pause icon
    let b = c - BTN_POS;
    if (in_box(b, vec2<i32>(0), BTN)) {
        let be = min(b, BTN - 1 - b);
        if (be.x == 0 && be.y == 0) { return GRAY; }
        if (min(be.x, be.y) == 0) { return BLACK; }
        let ic = b - BTN / 2;
        if (ic.y >= -5 && ic.y < 5 && ((ic.x >= -5 && ic.x < -1) || (ic.x >= 2 && ic.x < 6))) { return BLACK; }
        let light_d = min(b.x, b.y);
        let dark_d = min(BTN.x - 1 - b.x, BTN.y - 1 - b.y);
        if (min(light_d, dark_d) < 3) { return select(WHITE, DARK, dark_d < light_d); }
        return GRAY;
    }
    // PAUSED at double size, blinking like a VCR display
    let t = (c - vec2<i32>(BTN_POS.x + BTN.x + 12, 9)) / 2;
    if (blink_on && c.x >= BTN_POS.x + BTN.x + 12 && c.y >= 9 && text_on(12u, 6u, t)) { return BLACK; }
    // sunken groove under the text
    let gy = 31;
    let gx0 = BTN_POS.x + BTN.x + 12;
    if (c.x >= gx0 && c.x < gx0 + PAUSED_W * 2 && (c.y == gy || c.y == gy + 1)) { return select(WHITE, DARK, c.y == gy); }
    return GRAY;
}

fn hash3(p: vec3<f32>) -> f32 {
    var q = fract(p * vec3<f32>(0.1031, 0.1030, 0.0973));
    q += dot(q, q.yxz + 33.33);
    return fract((q.x + q.y) * q.z);
}

fn still(uv: vec2<f32>, have: bool) -> vec4<f32> {
    if (have) {
        return textureSampleLevel(se_prev, se_sampler, uv, 0.0);
    }
    return textureSampleLevel(se_input, se_sampler, uv, 0.0);
}

// One byte of bookkeeping from the state alpha of texel (x, 0).
fn byte_at(x: i32) -> u32 {
    return u32(round(textureLoad(se_prev, vec2<i32>(x, 0), 0).a * 255.0));
}

@fragment
fn fs(in: SeVsOut) -> SeOut {
    let uv = in.uv;
    let live = textureSampleLevel(se_input, se_sampler, uv, 0.0);
    let marker = 0xA5u;
    let have = byte_at(4) == marker;
    var start = se.time;
    if (have) {
        start = bitcast<f32>(byte_at(0) | (byte_at(1) << 8u) | (byte_at(2) << 16u) | (byte_at(3) << 24u));
    }
    let age = clamp(se.time - start, 0.0, 600.0);
    let env = clamp(se.env, 0.0, 1.0);
    let closing = env < 0.999 && age > 0.2;
    let k = select(1.0, smoothstep(0.2, 0.7, env), closing);

    // the still: exact copy for the state, pushed-in sample for display
    let keep = still(uv, have);
    let nudge = 1.0 - exp(-age * 9.0);
    let z = 1.0 + p_zoom() * (0.3 * nudge + 0.7 * min(age / 3.0, 1.0));
    let f = p_focus();
    var q = f + (uv - f) / z;
    let res = se.resolution;
    let frag = uv * res;
    let kp = res.y / 720.0;

    // VCR tracking band: drifts up slowly, jitters rows sideways, a few bright streaks
    var band = 0.0;
    if (p_tracking()) {
        let by = fract(0.8 - age * 0.05);
        let dy = abs(uv.y - by);
        band = 1.0 - smoothstep(0.004, 0.022, min(dy, 1.0 - dy));
        let row = floor(frag.y / max(2.0 * kp, 1.0));
        q.x += (hash3(vec3<f32>(row, f32(se.frame % 512u), 3.0)) - 0.5) * 0.008 * band;
    }
    let sv = still(q, have);
    var rgb = sv.rgb;

    // look: drained colour, a touch of contrast, vignette
    let y = dot(rgb, vec3<f32>(0.299, 0.587, 0.114));
    rgb = mix(rgb, vec3<f32>(y), p_fade());
    rgb = clamp((rgb - 0.5) * 1.06 + 0.5, vec3<f32>(0.0), vec3<f32>(1.0));
    let v = (uv - 0.5) * vec2<f32>(res.x / res.y, 1.0);
    rgb *= 1.0 - 0.28 * smoothstep(0.35, 1.0, length(v));
    // animated grain in ~2 px clumps, stronger in the shadows
    let cell = floor(frag / max(round(1.6 * kp), 1.0));
    let g = hash3(vec3<f32>(cell, f32(se.frame % 1024u))) - 0.5;
    rgb += vec3<f32>(g * 0.16 * p_grain() * (0.55 + 0.45 * (1.0 - y)));
    // tracking band: lifted, noisy, with sparse white streaks
    let streak = step(0.985, hash3(vec3<f32>(floor(frag.x / (6.0 * kp)), floor(frag.y / max(2.0 * kp, 1.0)), f32(se.frame % 512u))));
    rgb = mix(rgb, rgb * 1.12 + 0.05 + 0.6 * streak, band * 0.7);
    // shutter blink at the capture
    rgb = mix(rgb, WHITE, 0.55 * exp(-age * 16.0));
    rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)) * live.a;

    // PAUSED window
    let s = max(1.0, round(res.y / 360.0 * p_scale()));
    let size_px = vec2<f32>(SIZE) * s;
    let center = p_position() * res;
    let origin = floor(clamp(center - size_px * 0.5, vec2<f32>(0.0), max(res - size_px, vec2<f32>(0.0))));
    let p = vec2<i32>(floor((frag - origin) / s));
    let open = select(clamp(age / 0.22, 0.0, 1.0), smoothstep(0.15, 0.85, env), closing);
    if (p_scale() > 0.0) {
        if (open >= 1.0) {
            if (all(p >= vec2<i32>(0)) && all(p < SIZE)) {
                let beat_on = fract(age * 1.25) < 0.7;
                rgb = window(p, p_caption_color().rgb, beat_on || age < 0.4) * live.a;
            }
        } else {
            // checkered zoom rectangles growing from (or collapsing into) the window centre
            let checker = ((i32(floor(frag.x / s)) + i32(floor(frag.y / s))) & 1) == 0;
            let mid = vec2<f32>(SIZE / 2);
            for (var r = 0; r < 3; r++) {
                let t = open - f32(r) * 0.16;
                if (t <= 0.0) {
                    continue;
                }
                let ez = t * t * (3.0 - 2.0 * t);
                let lo = vec2<i32>(floor(mix(mid - 5.0, vec2<f32>(0.0), ez)));
                let hi = vec2<i32>(floor(mix(mid + 5.0, vec2<f32>(SIZE), ez)));
                let d = min(p - lo, hi - 1 - p);
                if (all(p >= lo) && all(p < hi) && min(d.x, d.y) < 2) {
                    rgb = select(BLACK, WHITE, checker) * live.a;
                }
            }
        }
    }

    // bookkeeping alpha: capture time bytes in texels (0..3, 0), the marker everywhere else
    let tx = vec2<i32>(floor(in.pos.xy));
    var book = marker;
    if (tx.y == 0 && tx.x < 4) {
        book = (bitcast<u32>(start) >> (8u * u32(tx.x))) & 255u;
    }
    var o: SeOut;
    o.color = vec4<f32>(mix(live.rgb, rgb, k), live.a);
    o.state = vec4<f32>(keep.rgb, f32(book) / 255.0);
    return o;
}
