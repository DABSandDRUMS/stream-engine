// Clip flash: the "someone clipped that" moment. Six dark iris blades spiral shut over the frame
// (70 ms), hold closed for a beat of a breath and spiral open while a soft white flash fades out
// (one flash per trigger, capped at 60 % white). Then a small Windows 3.1 "Clipboard" window
// springs in from the side into a corner: a camera icon painted in the clipper's chat colour
// (se.trigger.user_color, else the palette accent), bold "Clipped!" and a colour chip. It sits there
// for the hold and slides back out with the release. Window graphics are drawn on an integer grid
// of window pixels (`s` screen pixels each).

// Bitmap font, only the glyphs used: 'Clipboarde!'. 8 rows per glyph, bit 4 = leftmost column.
const FONT = array<u32, 88>(
    14u, 17u, 16u, 16u, 16u, 17u, 14u, 0u, // 'C'
    16u, 16u, 16u, 16u, 16u, 16u, 16u, 0u, // 'l'
    16u, 0u, 16u, 16u, 16u, 16u, 16u, 0u, // 'i'
    0u, 0u, 30u, 17u, 17u, 30u, 16u, 16u, // 'p'
    16u, 16u, 30u, 17u, 17u, 17u, 30u, 0u, // 'b'
    0u, 0u, 14u, 17u, 17u, 17u, 14u, 0u, // 'o'
    0u, 0u, 14u, 1u, 15u, 17u, 15u, 0u, // 'a'
    0u, 0u, 22u, 25u, 16u, 16u, 16u, 0u, // 'r'
    1u, 1u, 15u, 17u, 17u, 17u, 15u, 0u, // 'd'
    0u, 0u, 14u, 17u, 31u, 16u, 14u, 0u, // 'e'
    16u, 16u, 16u, 16u, 16u, 0u, 16u, 0u, // '!'
);
const GLYPH_W = array<u32, 11>(5u, 1u, 1u, 5u, 5u, 5u, 5u, 5u, 5u, 5u, 1u);
// Text runs: glyph index | x offset << 8 (bold advance = width + 2).
const RUNS = array<u32, 17>(
    0x0u, 0x701u, 0xa02u, 0xd03u, 0x1404u, 0x1b05u, 0x2206u, 0x2907u, 0x3008u, // Clipboard
    0x0u, 0x701u, 0xa02u, 0xd03u, 0x1403u, 0x1b09u, 0x2208u, 0x290au, // Clipped!
);
// start, glyph count, pixel width
const T_CAPTION = vec3<u32>(0u, 9u, 54u);
const T_CLIPPED = vec3<u32>(9u, 8u, 43u);

const BLACK = vec3<f32>(0.0);
const WHITE = vec3<f32>(1.0);
const GRAY = vec3<f32>(0.753);
const DARK = vec3<f32>(0.502);
const NAVY = vec3<f32>(0.0, 0.0, 0.502);

const WIN = vec2<i32>(132, 72); // 5 frame + 18 caption + 1 rule + 43 client + 5 frame
const BORDER = 5;
const TITLE = 18;

// shutter timing (s): close, hold shut, open
const T_CLOSE = 0.07;
const T_SHUT = 0.035;
const T_OPEN = 0.2;
const SLIDE_AT = 0.3;

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

// 32x24 camera icon; body in `body`, flash bulb lit by `bulb`. Returns rgb + coverage.
fn camera(p: vec2<i32>, body: vec3<f32>, bulb: f32) -> vec4<f32> {
    // viewfinder hump
    if (in_box(p, vec2<i32>(9, 2), vec2<i32>(9, 5))) {
        let edge = p.x == 9 || p.x == 17 || p.y == 2;
        return vec4<f32>(select(vec3<f32>(0.25), BLACK, edge), 1.0);
    }
    // flash window
    if (in_box(p, vec2<i32>(21, 3), vec2<i32>(8, 4))) {
        let edge = p.x == 21 || p.x == 28 || p.y == 3;
        return vec4<f32>(select(mix(vec3<f32>(0.75, 0.75, 0.6), vec3<f32>(1.0, 1.0, 0.75), bulb), BLACK, edge), 1.0);
    }
    // shutter button
    if (in_box(p, vec2<i32>(3, 4), vec2<i32>(4, 3))) {
        return vec4<f32>(select(vec3<f32>(0.9, 0.1, 0.1), BLACK, p.y == 4 && (p.x == 3 || p.x == 6)), 1.0);
    }
    // body: rounded rectangle with a black outline, lit top band, shaded bottom
    if (in_box(p, vec2<i32>(0, 7), vec2<i32>(32, 17))) {
        let q = p - vec2<i32>(0, 7);
        let corner = (q.x == 0 || q.x == 31) && (q.y == 0 || q.y == 16);
        if (corner) {
            return vec4<f32>(0.0);
        }
        // lens: black ring, gray barrel, dark glass with a highlight
        let c = vec2<f32>(p) + 0.5 - vec2<f32>(16.0, 15.5);
        let r = length(c);
        if (r < 7.5) {
            if (r > 6.3) {
                return vec4<f32>(BLACK, 1.0);
            }
            if (r > 4.6) {
                return vec4<f32>(select(GRAY, DARK, c.x + c.y > 0.0), 1.0);
            }
            if (c.x < -0.6 && c.x > -2.8 && c.y < -0.6 && c.y > -2.8) {
                return vec4<f32>(WHITE, 1.0);
            }
            return vec4<f32>(vec3<f32>(0.05, 0.08, 0.25), 1.0);
        }
        if (q.x == 0 || q.x == 31 || q.y == 0 || q.y == 16 || ((q.x == 1 || q.x == 30) && (q.y == 1 || q.y == 15))) {
            return vec4<f32>(BLACK, 1.0);
        }
        if (q.y < 3) {
            return vec4<f32>(mix(body, WHITE, 0.45), 1.0);
        }
        if (q.y > 13) {
            return vec4<f32>(body * 0.6, 1.0);
        }
        return vec4<f32>(body, 1.0);
    }
    return vec4<f32>(0.0);
}

fn window(p: vec2<i32>, ucol: vec3<f32>, bulb: f32) -> vec3<f32> {
    // modal frame: black / navy / black
    if (p.x < 1 || p.y < 1 || p.x >= WIN.x - 1 || p.y >= WIN.y - 1) {
        return BLACK;
    }
    if (p.x < BORDER - 1 || p.y < BORDER - 1 || p.x >= WIN.x - BORDER + 1 || p.y >= WIN.y - BORDER + 1) {
        return NAVY;
    }
    if (p.x < BORDER || p.y < BORDER || p.x >= WIN.x - BORDER || p.y >= WIN.y - BORDER) {
        return BLACK;
    }
    let q = p - vec2<i32>(BORDER);
    let inner_w = WIN.x - 2 * BORDER;
    if (q.y < TITLE) {
        // system menu box
        if (q.x < 19) {
            if (q.x == 18) {
                return BLACK;
            }
            if (q.x >= 5 && q.x < 14 && q.y == 8) {
                return WHITE;
            }
            if (q.x >= 4 && q.x < 15 && q.y >= 7 && q.y < 11) {
                return BLACK;
            }
            return GRAY;
        }
        let tx = 19 + (inner_w - 19 - i32(T_CAPTION.z)) / 2;
        if (text_on(T_CAPTION, q - vec2<i32>(tx, 5))) {
            return WHITE;
        }
        return NAVY;
    }
    if (q.y == TITLE) {
        return BLACK;
    }
    let c = q - vec2<i32>(0, TITLE + 1);
    let cam = camera(c - vec2<i32>(10, 9), ucol, bulb);
    if (cam.a > 0.5) {
        return cam.rgb;
    }
    if (text_on(T_CLIPPED, c - vec2<i32>(54, 11))) {
        return BLACK;
    }
    // the clipper's colour chip: sunken swatch under the text
    let w = c - vec2<i32>(54, 24);
    if (in_box(w, vec2<i32>(0), vec2<i32>(56, 8))) {
        if (w.x == 0 || w.y == 0) {
            return DARK;
        }
        if (w.x == 55 || w.y == 7) {
            return WHITE;
        }
        if (w.x == 1 || w.y == 1 || w.x == 54 || w.y == 6) {
            return BLACK;
        }
        return ucol;
    }
    return GRAY;
}

// Iris: 0 = open, 1 = shut. Six blades whose edges spiral out from a turning hexagonal aperture.
fn iris(frag: vec2<f32>, shut: f32) -> vec4<f32> {
    let half_h = se.resolution.y * 0.5;
    let p = (frag - se.resolution * 0.5) / half_h;
    let reach = length(se.resolution * 0.5) / half_h + 0.05;
    let radius = reach * (1.0 - shut);
    let rot = 0.9 * shut;
    let r = length(p);
    let th = atan2(p.y, p.x) + rot;
    // hexagon distance with the edge normals turned by `rot`
    let seg = 1.0471976;
    let local = th - seg * (floor(th / seg) + 0.5);
    let hex = r * cos(local);
    if (hex < radius) {
        // a thin dark lip on the aperture edge
        if (hex > radius - 0.012 && shut > 0.0) {
            return vec4<f32>(vec3<f32>(0.02), 0.85);
        }
        return vec4<f32>(0.0);
    }
    // blade id along a spiral so blade edges curve away from the hexagon corners
    let sp = th - 1.1 * log(max(r, 1e-3) / max(radius, 0.02));
    let fb = sp / seg;
    let blade = floor(fb);
    let f = fract(fb);
    // brushed dark metal, a soft sheen across each blade, darker toward the aperture
    let sheen = pow(sin(3.14159 * f), 3.0);
    let shade = 0.1 + 0.11 * sheen + 0.04 * fract(blade * 0.37) + 0.05 * clamp((hex - radius) * 2.0, 0.0, 1.0);
    var col = vec3<f32>(shade, shade, shade * 1.08);
    // overlap line where one blade tucks under the next
    if (f < 0.035) {
        col = vec3<f32>(0.01);
    } else if (f < 0.06) {
        col = vec3<f32>(0.32, 0.32, 0.34);
    }
    return vec4<f32>(col, 1.0);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let env = clamp(se.env, 0.0, 1.0);
    if (env <= 0.0) {
        return vec4<f32>(0.0);
    }
    let age = max(se.trigger_age, 0.0);
    let frag = in.uv * se.resolution;
    var out = vec4<f32>(0.0);

    // --- sticker window (drawn first, under the shutter) ---
    if (age > SLIDE_AT) {
        let uc = se.trigger.user_color;
        let accent = palette(PAL_ACCENT);
        let ucol = select(select(vec3<f32>(1.0, 0.31, 0.64), accent.rgb, accent.a > 0.5), uc.rgb / max(uc.a, 1e-3), uc.a > 0.5);
        let s = max(1.0, round(se.resolution.y / 360.0 * p_scale()));
        let size_px = vec2<f32>(WIN) * s;
        let corner = p_corner();
        let right = corner == 1 || corner == 3;
        let top = corner >= 2;
        let m = floor(p_margin() * se.resolution.y);
        let rest_x = select(m, se.resolution.x - m - size_px.x, right);
        let rest_y = select(se.resolution.y - m - size_px.y, m, top);
        // spring in from the side, slide back out with the release
        let t = age - SLIDE_AT;
        let spring = 1.0 - exp(-7.0 * t) * cos(11.0 * t);
        let leave = select(0.0, smoothstep(1.0, 0.0, env), env < 1.0 && age > 0.4);
        let travel = (size_px.x + m + 8.0 * s) * (1.0 - spring + leave);
        let x0 = rest_x + select(-travel, travel, right);
        let origin = floor(vec2<f32>(x0, rest_y));
        let p = vec2<i32>(floor((frag - origin) / s));
        // camera bulb pops when the window lands, then glows down
        let bulb = select(0.0, exp(-6.0 * max(t - 0.12, 0.0)), t > 0.12);
        if (all(p >= vec2<i32>(0)) && all(p < WIN)) {
            out = vec4<f32>(window(p, ucol, bulb), 1.0);
        } else {
            // sticker: 2-pixel white outline and a hard drop shadow
            let d = max(max(-p, p - (WIN - 1)), vec2<i32>(0));
            if (max(d.x, d.y) <= 2) {
                out = vec4<f32>(WHITE, 1.0);
            } else {
                let sh = p - vec2<i32>(5, 5);
                if (all(sh >= vec2<i32>(-2)) && all(sh < WIN + 2)) {
                    out = vec4<f32>(0.0, 0.0, 0.0, 0.45);
                }
            }
        }
    }

    // --- shutter iris ---
    var shut = 0.0;
    if (age < T_CLOSE) {
        let u = age / T_CLOSE;
        shut = u * u;
    } else if (age < T_CLOSE + T_SHUT) {
        shut = 1.0;
    } else if (age < T_CLOSE + T_SHUT + T_OPEN) {
        let u = (age - T_CLOSE - T_SHUT) / T_OPEN;
        shut = 1.0 - u * u * (3.0 - 2.0 * u);
    }
    if (p_iris() && shut > 0.0) {
        let b = iris(frag, shut);
        out = b + out * (1.0 - b.a);
    }

    // --- flash: one soft white pulse as the shutter opens, never above 60 % white ---
    let ft = age - T_CLOSE - T_SHUT * 0.5;
    if (ft > 0.0) {
        let peak = clamp(p_flash(), 0.0, 0.6);
        let f = peak * min(ft / 0.03, 1.0) * exp(-max(ft - 0.03, 0.0) * 9.0);
        out = vec4<f32>(f) + out * (1.0 - f);
    }
    return out;
}
