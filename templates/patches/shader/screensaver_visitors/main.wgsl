// Screensaver visitors: retro screensaver guests crossing the picture, composited over
// `se_input`. Flying toasters (3-frame flapping wings, flaps locked to the beat) drift from the
// upper right to the lower left; pixel fish swim across with a tail wiggle; an optional
// bouncing disc logo changes colour on every wall hit. Ambient visitors are scheduled on three
// slow lanes (sparse: most lane cycles stay empty); firing the patch sends a whole flock,
// sized by the payload tier, timed from `se.trigger_age`. Sprites are 24-px-wide 4-bit bitmaps
// (3 u32 per row) drawn at an integer art-pixel scale with a soft drop shadow.

const TAU: f32 = 6.2831853;
const SPR_W: i32 = 24;
const TOASTER_H: i32 = 20;
const FISH_H: i32 = 12;
const LOGO_W: i32 = 32;
const LOGO_H: i32 = 15;
const LANES: i32 = 3;
const MAX_FLOCK: i32 = 14;

// toaster: 3 frames (wings up, level, down) x 20 rows. 1 outline, 2 chrome, 3 chrome light,
// 4 chrome dark, 5 slot, 6 wing, 7 wing shade, 8 crust, 9 lever, 10 toast.
const TOASTER = array<u32, 180>(
    0x00000000u, 0x10000000u, 0x00101010u, 0x00000000u, 0x61000000u, 0x11616161u,
    0x00000000u, 0x66100000u, 0x01666666u, 0x00000000u, 0x76610000u, 0x01667676u,
    0x00000000u, 0x77610000u, 0x00167777u, 0x11100000u, 0x77761111u, 0x00001677u,
    0xaa810000u, 0x777618aau, 0x00000167u, 0xaaa10000u, 0x677761aau, 0x00000001u,
    0xaa810000u, 0x167761aau, 0x00000000u, 0x55511100u, 0x11666155u, 0x00000011u,
    0x33333310u, 0x33333333u, 0x00001333u, 0x22222310u, 0x22222222u, 0x00001422u,
    0x22666310u, 0x22222222u, 0x00001422u, 0x66622319u, 0x22222222u, 0x00001422u,
    0x62222319u, 0x22222226u, 0x00001422u, 0x22222310u, 0x22222222u, 0x00001422u,
    0x22222310u, 0x22222222u, 0x00001442u, 0x22222310u, 0x22222222u, 0x00001444u,
    0x44444410u, 0x44444444u, 0x00001444u, 0x11111100u, 0x11111111u, 0x00000111u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x11100000u, 0x10000111u, 0x00001111u,
    0xaa810000u, 0x611018aau, 0x01116666u, 0xaaa10000u, 0x77611aaau, 0x01667777u,
    0xaa810000u, 0x777661aau, 0x16777777u, 0x55511100u, 0x76666155u, 0x16767676u,
    0x33333310u, 0x61611333u, 0x11616161u, 0x22222310u, 0x21212222u, 0x00011121u,
    0x22666310u, 0x22222222u, 0x00001422u, 0x66622319u, 0x22222222u, 0x00001422u,
    0x62222319u, 0x22222226u, 0x00001422u, 0x22222310u, 0x22222222u, 0x00001422u,
    0x22222310u, 0x22222222u, 0x00001442u, 0x22222310u, 0x22222222u, 0x00001444u,
    0x44444410u, 0x44444444u, 0x00001444u, 0x11111100u, 0x11111111u, 0x00000111u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x11100000u, 0x00000111u, 0x00000000u,
    0xaa810000u, 0x000018aau, 0x00000000u, 0xaaa10000u, 0x00001aaau, 0x00000000u,
    0xaa810000u, 0x000018aau, 0x00000000u, 0x55511100u, 0x11111155u, 0x00000011u,
    0x33333310u, 0x31666133u, 0x00001333u, 0x22222310u, 0x67761222u, 0x00001421u,
    0x22666310u, 0x77761222u, 0x00001166u, 0x66622319u, 0x77612222u, 0x00116777u,
    0x62222319u, 0x77612226u, 0x01667777u, 0x22222310u, 0x76122222u, 0x01667676u,
    0x22222310u, 0x16122222u, 0x01161616u, 0x22222310u, 0x21222222u, 0x00011141u,
    0x44444410u, 0x44444444u, 0x00001444u, 0x11111100u, 0x11111111u, 0x00000111u,
);

// fish: 2 frames (tail spread, tail swish) x 12 rows, facing left. 1 outline, 2 body,
// 3 belly, 4 fin, 5 eye white, 6 pupil, 7 stripe.
const FISH = array<u32, 72>(
    0x00000000u, 0x00000000u, 0x00000000u, 0x10000000u, 0x00001111u, 0x00000000u,
    0x41100000u, 0x00114444u, 0x01100000u, 0x22210000u, 0x01222272u, 0x01410000u,
    0x22551000u, 0x12222272u, 0x01441000u, 0x22256100u, 0x22222272u, 0x01444101u,
    0x22222100u, 0x22222272u, 0x01144412u, 0x22222100u, 0x22222272u, 0x01441012u,
    0x33331000u, 0x33333373u, 0x01410001u, 0x33110000u, 0x11333333u, 0x01100000u,
    0x11000000u, 0x00011444u, 0x00000000u, 0x00000000u, 0x00000111u, 0x00000000u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x10000000u, 0x00001111u, 0x00000000u,
    0x41100000u, 0x00114444u, 0x00000000u, 0x22210000u, 0x01222272u, 0x00000000u,
    0x22551000u, 0x12222272u, 0x01110000u, 0x22256100u, 0x22222272u, 0x01144111u,
    0x22222100u, 0x22222272u, 0x00144441u, 0x22222100u, 0x22222272u, 0x01144111u,
    0x33331000u, 0x33333373u, 0x01110011u, 0x33110000u, 0x11333333u, 0x00000000u,
    0x11000000u, 0x00011444u, 0x00000000u, 0x00000000u, 0x00000111u, 0x00000000u,
);

// disc logo, 1 bit per pixel, 32 x 15.
const LOGO = array<u32, 15>(
    0x0fcc30fcu, 0x1fcc31fcu, 0x38c6338cu, 0x30633306u, 0x3061b306u,
    0x3860f386u, 0x1e3071e3u, 0x07f0307fu, 0x03f0103fu, 0x00000000u,
    0x00ffff00u, 0x1ffffff8u, 0xfff87ffeu, 0x1ffffff8u, 0x00ffff00u,
);

fn hash11(n: f32) -> f32 {
    return fract(sin(n * 91.345 + 47.853) * 43758.5453);
}

fn toaster_idx(frame: i32, x: i32, y: i32) -> u32 {
    let w = TOASTER[u32((frame * TOASTER_H + y) * 3 + x / 8)];
    return (w >> u32(4 * (x % 8))) & 15u;
}

fn fish_idx(frame: i32, x: i32, y: i32) -> u32 {
    let w = FISH[u32((frame * FISH_H + y) * 3 + x / 8)];
    return (w >> u32(4 * (x % 8))) & 15u;
}

fn toaster_rgb(i: u32) -> vec3<f32> {
    switch i {
        case 1u: { return vec3<f32>(0.10, 0.10, 0.14); }
        case 2u: { return vec3<f32>(0.68, 0.71, 0.76); }
        case 3u: { return vec3<f32>(0.93, 0.95, 0.97); }
        case 4u: { return vec3<f32>(0.42, 0.45, 0.51); }
        case 5u: { return vec3<f32>(0.16, 0.15, 0.17); }
        case 6u: { return vec3<f32>(1.0, 1.0, 1.0); }
        case 7u: { return vec3<f32>(0.80, 0.84, 0.95); }
        case 8u: { return vec3<f32>(0.67, 0.41, 0.18); }
        case 9u: { return vec3<f32>(0.27, 0.29, 0.33); }
        default: { return vec3<f32>(0.94, 0.78, 0.51); }
    }
}

fn fish_body(h: f32) -> vec3<f32> {
    if h < 0.3 { return vec3<f32>(1.0, 0.55, 0.16); }
    if h < 0.55 { return vec3<f32>(0.25, 0.55, 1.0); }
    if h < 0.8 { return vec3<f32>(1.0, 0.85, 0.20); }
    return vec3<f32>(1.0, 0.45, 0.65);
}

fn fish_rgb(i: u32, body: vec3<f32>) -> vec3<f32> {
    switch i {
        case 1u: { return vec3<f32>(0.08, 0.09, 0.16); }
        case 2u: { return body; }
        case 3u: { return mix(body, vec3<f32>(1.0, 0.97, 0.88), 0.5); }
        case 4u: { return body * vec3<f32>(0.78, 0.55, 0.5); }
        case 5u: { return vec3<f32>(1.0); }
        case 6u: { return vec3<f32>(0.04, 0.04, 0.08); }
        default: { return vec3<f32>(0.98, 0.98, 0.98); }
    }
}

// Wing frame for a flap cycle position f (0..1): up, level, down, level.
fn wing_frame(f: f32) -> i32 {
    let q = i32(floor(fract(f) * 4.0));
    return select(1, q, q == 0 || q == 2) ;
}

// Flap/swim clock in cycles: one flap per beat with a tempo, else 2 per second.
fn flap_clock() -> f32 {
    let bpm = s_beat_bpm();
    if bpm < 30.0 {
        return se.time * 2.0;
    }
    return round(se.time * bpm / 60.0 - s_beat_phase()) + s_beat_phase();
}

// Visitor layer colour accumulated so far for one pixel (premultiplied).
struct Px {
    rgb: vec3<f32>,
    a: f32,
};

fn over(dst: Px, rgb: vec3<f32>, a: f32) -> Px {
    var o: Px;
    o.rgb = rgb * a + dst.rgb * (1.0 - a);
    o.a = a + dst.a * (1.0 - a);
    return o;
}

// Draw one visitor whose sprite top-left is at `pos` (screen px). kind 0 toaster, 1 fish.
fn visitor(dst: Px, p: vec2<f32>, pos: vec2<f32>, s: f32, kind: i32, frame: i32, flip: bool, tint: vec3<f32>, alpha: f32) -> Px {
    let rows = select(TOASTER_H, FISH_H, kind == 1);
    let size = vec2<f32>(f32(SPR_W), f32(rows)) * s;
    let d = p - pos;
    // bounding box incl. the 1-px shadow offset
    if d.x < 0.0 || d.y < 0.0 || d.x >= size.x + s || d.y >= size.y + s || alpha <= 0.0 {
        return dst;
    }
    var o = dst;
    // soft drop shadow: the sprite shape one art pixel down-right
    let sh = vec2<i32>(floor((d - vec2<f32>(s)) / s));
    if sh.x >= 0 && sh.y >= 0 && sh.x < SPR_W && sh.y < rows {
        let sx = select(sh.x, SPR_W - 1 - sh.x, flip);
        let si = select(toaster_idx(frame, sx, sh.y), fish_idx(frame, sx, sh.y), kind == 1);
        if si != 0u {
            o = over(o, vec3<f32>(0.0), 0.28 * alpha);
        }
    }
    let a = vec2<i32>(floor(d / s));
    if a.x < SPR_W && a.y < rows {
        let ax = select(a.x, SPR_W - 1 - a.x, flip);
        if kind == 1 {
            let i = fish_idx(frame, ax, a.y);
            if i != 0u {
                o = over(o, fish_rgb(i, tint), alpha);
            }
        } else {
            let i = toaster_idx(frame, ax, a.y);
            if i != 0u {
                o = over(o, toaster_rgb(i), alpha);
            }
        }
    }
    return o;
}

fn logo_color(n: f32) -> vec3<f32> {
    let k = i32(n - 6.0 * floor(n / 6.0));
    switch k {
        case 0: { return vec3<f32>(1.0, 0.25, 0.35); }
        case 1: { return vec3<f32>(0.25, 0.75, 1.0); }
        case 2: { return vec3<f32>(1.0, 0.85, 0.20); }
        case 3: { return vec3<f32>(0.45, 1.0, 0.45); }
        case 4: { return vec3<f32>(0.80, 0.45, 1.0); }
        default: { return vec3<f32>(1.0, 0.55, 0.15); }
    }
}

// Triangle-wave bounce of travel `a` within [0, len]; .y counts wall hits so far.
fn bounce(a: f32, len: f32) -> vec2<f32> {
    let l = max(len, 1.0);
    let m = a - 2.0 * l * floor(a / (2.0 * l));
    return vec2<f32>(l - abs(l - m), floor(a / l));
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let src = textureSample(se_input, se_sampler, in.uv);
    let res = se.resolution;
    let amount = clamp(p_amount(), 0.0, 1.0);
    if amount <= 0.0 {
        return src;
    }
    let p = in.uv * res;
    let hu = res.y; // height unit in px
    let aspect = res.x / max(res.y, 1.0);
    let s = max(round(p_size() * res.y / 720.0), 1.0);
    let sf = max(round(p_size() * 1.25 * res.y / 720.0), 1.0); // fish are drawn a bit larger
    let alpha = select(1.0, 0.38, p_ghost());
    let mode = p_visitor(); // 0 toasters, 1 fish, 2 mixed
    let clock = flap_clock();
    let kick = clamp(s_band_kick(), 0.0, 1.0);
    let speed = p_speed();
    var o: Px;
    o.rgb = vec3<f32>(0.0);
    o.a = 0.0;

    // bouncing logo sits furthest back
    if p_logo() {
        let lsize = vec2<f32>(f32(LOGO_W), f32(LOGO_H)) * s;
        let t = se.time * speed;
        let bx = bounce(t * 0.13 * hu + 0.31 * res.x, res.x - lsize.x);
        let by = bounce(t * 0.10 * hu + 0.17 * res.y, res.y - lsize.y);
        let a = vec2<i32>(floor((p - vec2<f32>(bx.x, by.x)) / s));
        if a.x >= 0 && a.y >= 0 && a.x < LOGO_W && a.y < LOGO_H {
            if ((LOGO[u32(a.y)] >> u32(a.x)) & 1u) != 0u {
                o = over(o, logo_color(bx.y + by.y), 0.85 * alpha);
            }
        }
    }

    // ambient lanes: each lane cycle may carry a small group (1-3) across, most stay empty
    let tsp = 0.15 * speed; // toaster speed along x, heights per second
    let fsp = 0.12 * speed;
    for (var k = 0; k < LANES; k = k + 1) {
        let fk = f32(k);
        let travel = (aspect + 1.0) / min(tsp, fsp);
        let period = max(p_interval() * (1.0 + 0.37 * fk), travel + 1.0);
        let local = se.time + period * (0.61 * fk + 0.92);
        let cyc = floor(local / period);
        let u = local - cyc * period;
        let hseed = hash11(cyc * 7.13 + fk * 3.71);
        if hseed > p_chance() {
            continue;
        }
        var kind = select(0, 1, mode == 1);
        if mode == 2 {
            kind = select(0, 1, hash11(hseed * 13.1) > 0.5);
        }
        let n = 1 + i32(hash11(hseed * 5.3) * 2.99);
        let y0 = hash11(hseed * 9.7);
        let flip = hash11(hseed * 2.9) > 0.5;
        for (var j = 0; j < 3; j = j + 1) {
            if j >= n {
                break;
            }
            let fj = f32(j);
            let hj = hash11(hseed * 17.0 + fj * 1.37);
            var pos: vec2<f32>;
            var frame: i32;
            if kind == 0 {
                // drift from upper right to lower left
                let x = aspect + 0.08 + fj * 0.22 - u * tsp;
                let y = -0.55 + 0.75 * y0 + fj * 0.12 + u * tsp * 0.5;
                pos = vec2<f32>(x, y - kick * 0.006) * hu;
                frame = wing_frame(clock + hj);
            } else {
                let dirx = select(-1.0, 1.0, flip);
                let x0 = select(aspect + 0.05, -0.05 - f32(SPR_W) * s / hu, flip);
                let x = x0 + dirx * (u * fsp - fj * 0.16);
                let y = 0.18 + 0.5 * y0 + fj * 0.07 + 0.018 * sin(u * 1.3 + hj * TAU);
                pos = vec2<f32>(x, y) * hu;
                frame = i32(floor(fract(clock * 0.5 + hj) * 2.0));
            }
            o = visitor(o, p, pos, select(s, sf, kind == 1), kind, frame, kind == 1 && flip, fish_body(hj), alpha);
        }
    }

    // trigger flock: enters right away and clears the frame in a few seconds
    let ta = se.trigger_age;
    if ta < 20.0 {
        let tier = clamp(se.trigger.tier, 1.0, 3.0);
        let n = min(4 + i32(tier) * 3, MAX_FLOCK);
        let tc = f32(se.trigger_count);
        var kind = select(0, 1, mode == 1);
        if mode == 2 {
            kind = i32(se.trigger_count % 2u);
        }
        let fl = 2.4 * speed;
        let flip = hash11(tc * 3.3) > 0.5;
        for (var j = 0; j < MAX_FLOCK; j = j + 1) {
            if j >= n {
                break;
            }
            let fj = f32(j);
            let hj = hash11(tc * 11.0 + fj * 2.17);
            let col = f32(j / 3);
            let row = f32(j % 3);
            var pos: vec2<f32>;
            var frame: i32;
            if kind == 0 {
                // loose echelon from the top right
                let x = aspect + 0.02 + col * 0.24 + row * 0.1 + hj * 0.06 - ta * tsp * fl;
                let y = -0.38 + row * 0.27 - col * 0.07 + hash11(hj * 7.0) * 0.05 + ta * tsp * fl * 0.5;
                pos = vec2<f32>(x, y - kick * 0.006) * hu;
                frame = wing_frame(clock * 1.5 + hj);
            } else {
                // a school sweeping across the middle
                let dirx = select(-1.0, 1.0, flip);
                let x0 = select(aspect + 0.02, -0.02 - f32(SPR_W) * s / hu, flip);
                let x = x0 + dirx * (ta * fsp * fl - col * 0.2 - hj * 0.08);
                let y = 0.2 + row * 0.2 + hash11(hj * 7.0) * 0.08 + 0.02 * sin(ta * 2.0 + hj * TAU);
                pos = vec2<f32>(x, y) * hu;
                frame = i32(floor(fract(clock + hj) * 2.0));
            }
            o = visitor(o, p, pos, select(s, sf, kind == 1), kind, frame, kind == 1 && flip, fish_body(hj), alpha);
        }
    }

    let a = o.a * amount;
    let rgb = o.rgb * amount;
    return vec4<f32>(rgb + src.rgb * (1.0 - a), a + src.a * (1.0 - a));
}
