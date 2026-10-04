// Viewer claim: for `bars` bars after a trigger, snare hits light the frame's rim in the claimer's
// chat colour (se.trigger.user_color; the palette accent without a user). Layers, in order: a rim
// light that tints bright picture detail near the edges, a screen-blended edge glow, a crisp
// border, and a corner badge (Win 3.1 push button with a snare icon, one pip per remaining bar)
// drawn on an integer grid of "badge pixels". The claim opens with a colour ring sweeping in from
// the edges; it closes with the rim fading and the badge sliding off its edge.
// Works in unpremultiplied colour and returns the input alpha untouched.

// 16x16 snare with crossed sticks, 2 bits per pixel (x = 0 low): 1 black, 2 white, 3 chrome
const ICON = array<u32, 16>(
    0x50000005u, 0x7400001du, 0x1d000074u, 0x074001d0u, 0x01d00740u, 0x00741d00u, 0x155d7554u, 0x6aab6aa9u,
    0x6aaaaaa9u, 0x55555555u, 0x7efefefdu, 0x7dfdfdfdu, 0x7dfdfdfdu, 0x7dfdfdfdu, 0x7efefefdu, 0x55555555u,
);
const BLACK = vec3<f32>(0.0);
const WHITE = vec3<f32>(1.0);
const PIP_DARK = vec3<f32>(0.35);
const BOX: i32 = 24;
const PIP: i32 = 5;      // pip pitch (4 px pip + 1 px gap)
const PER_ROW: i32 = 5;
const MARGIN: f32 = 10.0;
const HALO: i32 = 2;

fn claim_color() -> vec3<f32> {
    if se.trigger.user_color.a > 0.5 {
        return se.trigger.user_color.rgb;
    }
    return palette(PAL_ACCENT).rgb;
}

fn icon_level(p: vec2<i32>) -> u32 {
    if any(p < vec2<i32>(0)) || any(p > vec2<i32>(15)) {
        return 0u;
    }
    return (ICON[u32(p.y)] >> (u32(p.x) * 2u)) & 3u;
}

// Badge pixel at p (badge pixels, origin top-left of the button). rgb + coverage.
fn badge(p: vec2<i32>, col: vec3<f32>, pressed: bool, remaining: i32, bars: i32, blink: bool, halo: f32) -> vec4<f32> {
    let rows = (bars + PER_ROW - 1) / PER_ROW;
    let size = vec2<i32>(BOX, BOX + 2 + rows * PIP - 1);
    // pulsing halo around the button
    if all(p >= vec2<i32>(-HALO)) && all(p < vec2<i32>(BOX + HALO)) && (any(p < vec2<i32>(0)) || any(p >= vec2<i32>(BOX))) {
        return vec4<f32>(mix(col, WHITE, 0.3), halo);
    }
    if all(p >= vec2<i32>(0)) && all(p < vec2<i32>(BOX)) {
        let e = min(p, vec2<i32>(BOX - 1) - p);
        // black frame with clipped corners
        if e.x == 0 && e.y == 0 {
            return vec4<f32>(0.0);
        }
        if min(e.x, e.y) == 0 {
            return vec4<f32>(BLACK, 1.0);
        }
        let q = p - vec2<i32>(1);
        let inner = BOX - 2;
        let push = select(0, 1, pressed);
        let lv = icon_level(q - vec2<i32>(3 + push, 2 + push));
        if lv == 1u {
            return vec4<f32>(vec3<f32>(0.04), 1.0);
        }
        if lv == 2u {
            return vec4<f32>(WHITE, 1.0);
        }
        if lv == 3u {
            return vec4<f32>(vec3<f32>(0.78), 1.0);
        }
        let light = col * 0.45 + vec3<f32>(0.55);
        let shade = col * 0.45;
        if pressed {
            // pressed: a dark top/left edge, face lit up
            if min(q.x, q.y) < 1 {
                return vec4<f32>(shade, 1.0);
            }
            return vec4<f32>(mix(col, WHITE, 0.3), 1.0);
        }
        let light_d = min(q.x, q.y);
        let dark_d = min(inner - 1 - q.x, inner - 1 - q.y);
        if min(light_d, dark_d) < 2 {
            return vec4<f32>(select(light, shade, dark_d < light_d), 1.0);
        }
        return vec4<f32>(col, 1.0);
    }
    // bar pips: rows of five under the button, the current bar's pip blinks on the beat
    let r = p - vec2<i32>(0, BOX + 2);
    if r.y < 0 || r.y >= rows * PIP || p.y >= size.y {
        return vec4<f32>(0.0);
    }
    let row = r.y / PIP;
    let n_row = min(bars - row * PER_ROW, PER_ROW);
    let x0 = (BOX - (n_row * PIP - 1)) / 2;
    let rx = r.x - x0;
    if rx < 0 || rx >= n_row * PIP {
        return vec4<f32>(0.0);
    }
    let cell = vec2<i32>(rx % PIP, r.y % PIP);
    if cell.x == 4 || cell.y == 4 {
        return vec4<f32>(0.0);
    }
    let k = row * PER_ROW + rx / PIP;
    if cell.x == 0 || cell.y == 0 || cell.x == 3 || cell.y == 3 {
        return vec4<f32>(BLACK, 1.0);
    }
    if k < remaining - 1 || (k == remaining - 1 && !blink) {
        return vec4<f32>(col, 1.0);
    }
    if k == remaining - 1 {
        return vec4<f32>(WHITE, 1.0);
    }
    return vec4<f32>(PIP_DARK, 1.0);
}

// stepped pop-in: small, overshoot, settle
fn pop(t: f32) -> f32 {
    if t < 0.05 {
        return 0.45;
    }
    if t < 0.1 {
        return 1.25;
    }
    if t < 0.15 {
        return 0.92;
    }
    return 1.0;
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let src = textureSampleLevel(se_input, se_sampler, in.uv, 0.0);
    let amt = clamp(p_amount(), 0.0, 2.0);
    let bpm = select(120.0, s_beat_bpm(), s_beat_bpm() > 30.0);
    let bar = 240.0 / bpm;
    let bars = clamp(p_bars(), 1, 16);
    let dur = f32(bars) * bar;
    let t = se.trigger_age;
    if t >= dur || amt <= 0.0 {
        return src;
    }
    let a = src.a;
    var c = select(src.rgb / max(a, 1e-4), vec3<f32>(0.0), a <= 1e-4);
    let claim = smoothstep(0.0, 0.12, t) * (1.0 - smoothstep(dur - 0.45, dur, t));
    let col = claim_color();
    let snare = clamp(s_band_snare(), 0.0, 1.0);
    let hit = pow(snare, 1.4);

    // distance to the nearest frame edge, in frame heights
    let aspect = se.resolution.x / max(se.resolution.y, 1.0);
    let uv = in.uv;
    let e = min(min(uv.x, 1.0 - uv.x) * aspect, min(uv.y, 1.0 - uv.y));
    let w = p_glow();
    // opening: a flash on the rim and a colour ring sweeping in from the edges
    let st = clamp(t / 0.55, 0.0, 1.0);
    let stamp = 1.0 - st;
    let ring_r = 0.32 * (1.0 - (1.0 - st) * (1.0 - st));
    let rd = (e - ring_r) / 0.01;
    let ring = exp(-rd * rd) * stamp * stamp;
    let g = amt * (claim * (p_idle() * 0.3 + hit) * exp(-e / w) + stamp * stamp * exp(-e / (w * 1.5)) + ring * 0.8);

    // rim light: bright detail near the edges catches the colour, then a screen-blended glow
    let luma = dot(c, vec3<f32>(0.299, 0.587, 0.114));
    c = c + col * luma * g * 1.4;
    c = vec3<f32>(1.0) - (vec3<f32>(1.0) - min(c, vec3<f32>(1.0))) * (vec3<f32>(1.0) - clamp(col * g, vec3<f32>(0.0), vec3<f32>(1.0)));

    // crisp border, brighter on hits
    let s = max(1.0, round(se.resolution.y / 360.0 * p_scale()));
    if e * se.resolution.y < 2.0 * s {
        c = mix(c, mix(col, WHITE, 0.4 * hit), clamp(claim * amt * (0.5 + 0.5 * hit), 0.0, 1.0));
    }

    // corner badge
    let rows = (bars + PER_ROW - 1) / PER_ROW;
    let size = vec2<f32>(f32(BOX), f32(BOX + 2 + rows * PIP - 1));
    let corner = clamp(p_corner(), 0, 3);
    let right = corner == 1 || corner == 3;
    let bottom = corner <= 1;
    let size_px = size * s;
    var origin = vec2<f32>(
        select(MARGIN * s, se.resolution.x - MARGIN * s - size_px.x, right),
        select(MARGIN * s, se.resolution.y - MARGIN * s - size_px.y, bottom),
    );
    // closing: slide off through the nearest horizontal edge
    let u = clamp((t - (dur - 0.4)) / 0.4, 0.0, 1.0);
    origin.y = origin.y + select(-1.0, 1.0, bottom) * u * u * (size_px.y + (MARGIN + 4.0) * s);
    origin = floor(origin);
    let k = pop(t);
    let centre = origin + size_px * 0.5;
    let frag = in.uv * se.resolution;
    let bp = vec2<i32>(floor((frag - centre) / (s * k) + size * 0.5));
    let elapsed = i32(floor(t / bar));
    let remaining = max(bars - elapsed, 1);
    let blink = fract(s_beat_phase()) < 0.5;
    let halo = clamp(max(hit, 0.25 * (1.0 - fract(s_beat_phase()))) * min(amt, 1.0), 0.0, 1.0);
    let b = badge(bp, col, snare > 0.18, remaining, bars, blink, halo);
    c = mix(c, b.rgb, b.a * min(amt, 1.0));

    return vec4<f32>(c * a, a);
}
