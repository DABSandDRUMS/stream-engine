// Talk mode: a calm "just chatting" look for the moments between songs. The talk amount is
// (mic up) x (band quiet), both through wide soft knees so it eases in and out with the analysis'
// own smoothing; shaders keep no state, so how gently it fades follows how smoothly band.level and
// mic.level move. Short gaps between words don't drop it: the mic knee starts well below speech
// level. With the amount up: the edges of the picture go soft (a radius that grows toward the
// corners, 12-tap disc), a gentle warm grade with lifted shadows and a little less saturation, a
// light vignette, and a small Windows 3.1 "Chat" badge whose talk light blinks and glows brighter
// with the voice while a typing bubble animates. Input alpha is preserved (premultiplied).

// Bitmap font, only the glyphs used: 'ChatJuscing'. 8 rows per glyph, bit 4 = leftmost column.
const FONT = array<u32, 88>(
    14u, 17u, 16u, 16u, 16u, 17u, 14u, 0u, // 'C'
    16u, 16u, 22u, 25u, 17u, 17u, 17u, 0u, // 'h'
    0u, 0u, 14u, 1u, 15u, 17u, 15u, 0u, // 'a'
    0u, 8u, 30u, 8u, 8u, 8u, 6u, 0u, // 't'
    7u, 2u, 2u, 2u, 2u, 18u, 12u, 0u, // 'J'
    0u, 0u, 17u, 17u, 17u, 19u, 13u, 0u, // 'u'
    0u, 0u, 15u, 16u, 14u, 1u, 30u, 0u, // 's'
    0u, 0u, 14u, 17u, 16u, 17u, 14u, 0u, // 'c'
    16u, 0u, 16u, 16u, 16u, 16u, 16u, 0u, // 'i'
    0u, 0u, 22u, 25u, 17u, 17u, 17u, 0u, // 'n'
    0u, 0u, 15u, 17u, 17u, 15u, 1u, 14u, // 'g'
);
const GLYPH_W = array<u32, 11>(5u, 5u, 5u, 4u, 5u, 5u, 5u, 5u, 1u, 5u, 5u);
// Text runs: glyph index | x offset << 8 (bold advance = width + 2).
const RUNS = array<u32, 16>(
    0x0u, 0x701u, 0xe02u, 0x1503u, // Chat
    0x4u, 0x705u, 0xe06u, 0x1503u, 0x1f07u, 0x2601u, 0x2d02u, 0x3403u, 0x3a03u, 0x4008u, 0x4309u, 0x4a0au, // Just chatting
);
// start, glyph count, pixel width
const T_CAPTION = vec3<u32>(0u, 4u, 26u);
const T_TEXT = vec3<u32>(4u, 12u, 80u);

const BLACK = vec3<f32>(0.0);
const WHITE = vec3<f32>(1.0);
const GRAY = vec3<f32>(0.753);
const DARK = vec3<f32>(0.502);
const NAVY = vec3<f32>(0.0, 0.0, 0.502);

// badge: 1 black + 13 caption + 1 rule + 20 client + 1 black
const BADGE = vec2<i32>(124, 36);
const CAP_H = 13;

fn glyph_on(g: u32, x: i32, y: i32) -> bool {
    if (x < 0 || y < 0 || y > 7 || x >= i32(GLYPH_W[g])) {
        return false;
    }
    return ((FONT[g * 8u + u32(y)] >> u32(4 - x)) & 1u) == 1u;
}

// Bold System-font look: every glyph pixel is doubled one pixel to the right.
fn text_on(t: vec3<u32>, p: vec2<i32>) -> bool {
    if (p.y < 0 || p.y > 7 || p.x < 0 || p.x > i32(t.z)) {
        return false;
    }
    for (var i = 0u; i < t.y; i++) {
        let e = RUNS[t.x + i];
        let g = e & 0xffu;
        let lx = p.x - i32(e >> 8u);
        if (glyph_on(g, lx, p.y) || glyph_on(g, lx - 1, p.y)) {
            return true;
        }
    }
    return false;
}

fn in_box(p: vec2<i32>, lo: vec2<i32>, size: vec2<i32>) -> bool {
    return all(p >= lo) && all(p < lo + size);
}

// 16x13 speech bubble with three typing dots lighting in turn; rgb + coverage.
fn bubble(p: vec2<i32>, time: f32) -> vec4<f32> {
    // tail
    if (p.y >= 9 && p.y < 13 && p.x >= 3 && p.x <= 3 + (12 - p.y)) {
        let edge = p.x == 3 || p.x == 3 + (12 - p.y);
        if (p.y == 9 && !edge) {
            return vec4<f32>(WHITE, 1.0);
        }
        return vec4<f32>(select(WHITE, BLACK, edge || p.y == 12), 1.0);
    }
    if (!in_box(p, vec2<i32>(0), vec2<i32>(16, 10))) {
        return vec4<f32>(0.0);
    }
    let corner = (p.x == 0 || p.x == 15) && (p.y == 0 || p.y == 9);
    if (corner) {
        return vec4<f32>(0.0);
    }
    if (p.x == 0 || p.x == 15 || p.y == 0 || p.y == 9) {
        return vec4<f32>(BLACK, 1.0);
    }
    let lit_n = i32(time * 3.0) % 4;
    for (var k = 0; k < 3; k++) {
        if (in_box(p, vec2<i32>(3 + 4 * k, 4), vec2<i32>(2, 2))) {
            return vec4<f32>(select(DARK, NAVY, k < lit_n), 1.0);
        }
    }
    return vec4<f32>(WHITE, 1.0);
}

fn badge(p: vec2<i32>, mic: f32) -> vec3<f32> {
    if (p.x == 0 || p.y == 0 || p.x == BADGE.x - 1 || p.y == BADGE.y - 1 || p.y == CAP_H + 1) {
        return BLACK;
    }
    if (p.y <= CAP_H) {
        // system menu box, then caption
        if (p.x < 14) {
            if (p.x == 13) {
                return BLACK;
            }
            if (p.x >= 3 && p.x < 11 && p.y >= 6 && p.y < 9) {
                return select(BLACK, WHITE, p.y == 6 && p.x < 10);
            }
            return GRAY;
        }
        let tx = 14 + (BADGE.x - 15 - i32(T_CAPTION.z)) / 2;
        if (text_on(T_CAPTION, p - vec2<i32>(tx, 3))) {
            return WHITE;
        }
        return NAVY;
    }
    let c = p - vec2<i32>(1, CAP_H + 2);
    let b = bubble(c - vec2<i32>(4, 3), se.time);
    if (b.a > 0.5) {
        return b.rgb;
    }
    if (text_on(T_TEXT, c - vec2<i32>(25, 6))) {
        return BLACK;
    }
    // talk light: blinks like an on-air lamp, brighter while the voice is loud
    let l = vec2<f32>(c) + 0.5 - vec2<f32>(113.0, 9.5);
    let r = length(l);
    if (r < 4.6) {
        if (r > 3.6) {
            return BLACK;
        }
        let blink = select(0.0, 1.0, fract(se.time * 1.6) < 0.55);
        let on = blink * (0.65 + 0.35 * clamp(mic * 3.0, 0.0, 1.0));
        var col = mix(vec3<f32>(0.35, 0.0, 0.0), vec3<f32>(1.0, 0.12, 0.1), on);
        if (l.x < -0.5 && l.y < -0.5 && r < 2.6 && on > 0.0) {
            col = vec3<f32>(1.0, 0.75, 0.7);
        }
        return col;
    }
    return GRAY;
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let src = textureSampleLevel(se_input, se_sampler, in.uv, 0.0);
    let mic = s_mic_level();
    let gate_m = max(p_mic_gate(), 0.005);
    let gate_b = max(p_band_gate(), 0.01);
    let mic_on = smoothstep(gate_m * 0.4, gate_m * 1.6, mic);
    let quiet = 1.0 - smoothstep(gate_b * 0.5, gate_b * 1.5, s_band_level());
    let talk = clamp(p_amount(), 0.0, 1.0) * smoothstep(0.0, 1.0, mic_on * quiet);
    if (talk <= 0.001) {
        return src;
    }

    // edge weight: 0 in the middle of the node, 1 toward its corners
    let r0 = se.region.xy;
    let rs = max(se.region.zw - se.region.xy, vec2<f32>(1e-4));
    let rc = (in.uv - r0) / rs - 0.5;
    let aspect = rs.x * se.resolution.x / max(rs.y * se.resolution.y, 1.0);
    let ell = length(rc * vec2<f32>(aspect / max(aspect, 1.0), 1.0) * 2.0);
    let edge = smoothstep(0.55, 1.25, ell);

    // soft focus: 12-tap golden-angle disc, radius growing toward the edges
    var col = src;
    let radius = talk * edge * max(p_blur(), 0.0) * 9.0 * se.resolution.y / 720.0;
    if (radius > 0.6) {
        let px = radius / se.resolution;
        var acc = src;
        for (var i = 0; i < 12; i++) {
            let fi = f32(i) + 0.5;
            let a = fi * 2.39996;
            let d = sqrt(fi / 12.0);
            let uv = clamp(in.uv + vec2<f32>(cos(a), sin(a)) * d * px, r0, se.region.zw);
            acc += textureSampleLevel(se_input, se_sampler, uv, 0.0);
        }
        col = acc / 13.0;
    }

    // warm grade on straight colour: warmer whites, lifted warm shadows, a little less saturation
    let a = max(col.a, 1e-4);
    var rgb = col.rgb / a;
    let w = talk * max(p_warmth(), 0.0);
    let luma = dot(rgb, vec3<f32>(0.299, 0.587, 0.114));
    rgb = mix(rgb, vec3<f32>(luma), 0.15 * w);
    rgb *= mix(vec3<f32>(1.0), vec3<f32>(1.07, 1.0, 0.86), w);
    rgb += vec3<f32>(0.045, 0.022, 0.0) * w * (1.0 - luma);
    rgb = mix(rgb, vec3<f32>(0.5), 0.05 * w);
    rgb *= 1.0 - 0.2 * talk * edge;
    var out = vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)) * col.a, col.a);

    // Chat badge (eases up a few pixels as it fades in)
    if (p_badge()) {
        let s = max(1.0, round(se.resolution.y / 360.0 * p_scale()));
        let size_px = vec2<f32>(BADGE) * s;
        let lo = r0 * se.resolution;
        let hi = se.region.zw * se.resolution - size_px;
        let show = smoothstep(0.25, 0.85, talk);
        let origin = floor(clamp(lo + p_position() * rs * se.resolution, lo, max(hi, lo)) + vec2<f32>(0.0, (1.0 - show) * 6.0 * s));
        let p = vec2<i32>(floor((in.uv * se.resolution - origin) / s));
        if (show > 0.0 && all(p >= vec2<i32>(0)) && all(p < BADGE)) {
            let bc = badge(p, mic);
            out = vec4<f32>(bc, 1.0) * show + out * (1.0 - show);
        } else if (show > 0.0) {
            // soft drop shadow
            let sp = p - vec2<i32>(3, 3);
            if (all(sp >= vec2<i32>(0)) && all(sp < BADGE)) {
                out = out * (1.0 - 0.35 * show);
            }
        }
    }
    return out;
}
