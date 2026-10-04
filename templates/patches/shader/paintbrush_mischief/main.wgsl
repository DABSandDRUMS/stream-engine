// Paintbrush mischief: every trigger starts one doodle (stars, arrow, circle or double underline,
// cycling with se.trigger_count) aimed at `target`. A pixel MS Paint brush cursor draws it stroke
// by stroke, it lingers, then a square eraser cursor retraces and wipes it. Optional auto mode runs
// the same doodles on a fixed clock between triggers. Everything is evaluated on a coarse pixel
// grid (cell centers), so ink and cursors have hard, chunky edges.

const PI = 3.14159265;
const TAU = 6.2831853;

// 16x16 brush cursor, 4 bits per pixel (two words per row), tip at column 0, row 15.
// 0 clear, 1 outline, 2 ferrule light, 3 ferrule shade, 4 handle, 5 handle shade, 6 paint.
const BRUSH = array<u32, 32>(
    0x00000000u, 0x01110000u, 0x00000000u, 0x15441000u, 0x00000000u, 0x01544100u, 0x00000000u, 0x00154410u,
    0x00000000u, 0x00015441u, 0x11000000u, 0x00001544u, 0x22100000u, 0x00000132u, 0x22210000u, 0x00000013u,
    0x33221000u, 0x00000001u, 0x16661000u, 0x00000000u, 0x01666100u, 0x00000000u, 0x00166100u, 0x00000000u,
    0x00016610u, 0x00000000u, 0x00001610u, 0x00000000u, 0x00000161u, 0x00000000u, 0x00000016u, 0x00000000u,
);
// Classic Paintbrush palette, the colors that read on camera footage.
const INK = array<vec3<f32>, 6>(
    vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(1.0, 1.0, 0.0), vec3<f32>(0.0, 1.0, 0.0),
    vec3<f32>(0.0, 1.0, 1.0), vec3<f32>(1.0, 0.0, 1.0), vec3<f32>(1.0, 1.0, 1.0));
// Trigger order of doodle kinds: 0 stars, 1 underline, 2 arrow, 3 circle.
const ORDER = array<i32, 4>(0, 2, 3, 1);

struct Doodle {
    kind: i32,
    a: vec2<f32>,     // stars/underline/circle: center; arrow: tail
    b: vec2<f32>,     // arrow: tip; circle: (start angle, wobble phase)
    size: f32,
    color: vec3<f32>,
    age: f32,         // seconds since this doodle started
    bc: vec2<f32>,    // bounding circle for culling
    br: f32,
}

fn hash(n: u32, salt: u32) -> f32 {
    var v = n * 747796405u + salt * 2891336453u + 1013904223u;
    v = ((v >> ((v >> 28u) + 4u)) ^ v) * 277803737u;
    v = (v >> 22u) ^ v;
    return f32(v & 0xffffu) / 65536.0;
}

fn t_draw() -> f32 { return 1.25 / max(p_speed(), 0.05); }
fn t_erase() -> f32 { return 0.8 / max(p_speed(), 0.05); }
fn t_total() -> f32 { return t_draw() + max(p_linger(), 0.0) + t_erase(); }

fn make_doodle(kind: i32, seed: u32, age: f32, scale: f32, view: vec2<f32>) -> Doodle {
    var d: Doodle;
    d.kind = kind;
    d.age = age;
    d.color = INK[min(u32(hash(seed, 1u) * 6.0), 5u)];
    let t = clamp(p_target(), vec2<f32>(0.0), vec2<f32>(1.0)) * view;
    let u = view.y / 240.0; // doodles are sized for a 240-row grid
    switch kind {
        case 0: {
            d.size = (22.0 + 6.0 * hash(seed, 2u)) * scale * u;
            let side = select(-1.0, 1.0, hash(seed, 3u) < 0.5);
            let off = vec2<f32>(side * (0.22 + 0.1 * hash(seed, 4u)) * view.x, -(0.08 + 0.12 * hash(seed, 5u)) * view.y);
            d.a = clamp(t + off, vec2<f32>(d.size * 1.6), view - vec2<f32>(d.size * 2.4, d.size * 1.6));
            d.bc = d.a + vec2<f32>(0.4, -0.2) * d.size;
            d.br = 2.0 * d.size;
        }
        case 1: {
            d.size = (44.0 + 10.0 * hash(seed, 2u)) * scale * u;
            d.a = clamp(t + vec2<f32>(0.0, 0.21 * view.y), vec2<f32>(d.size * 1.1), view - vec2<f32>(d.size * 1.1, 0.06 * view.y));
            d.bc = d.a;
            d.br = 1.15 * d.size;
        }
        case 2: {
            let ang = mix(-2.7, -0.45, hash(seed, 2u));
            var dir = vec2<f32>(cos(ang), sin(ang));
            let len = 78.0 * scale * u;
            var tail = t + dir * len;
            if (tail.x < 0.08 * view.x || tail.x > 0.92 * view.x) {
                dir.x = -dir.x;
                tail = t + dir * len;
            }
            d.a = clamp(tail, vec2<f32>(0.05) * view, vec2<f32>(0.95) * view);
            d.b = t + normalize(d.a - t) * 20.0 * scale * u;
            d.size = len;
            d.bc = (d.a + d.b) * 0.5;
            d.br = 0.5 * length(d.a - d.b) + 0.25 * len;
        }
        default: {
            d.size = (34.0 + 6.0 * hash(seed, 2u)) * scale * u;
            d.a = t;
            d.b = vec2<f32>(mix(-2.4, -1.2, hash(seed, 3u)), TAU * hash(seed, 4u));
            d.bc = d.a;
            d.br = 1.35 * d.size;
        }
    }
    return d;
}

fn path_count(kind: i32) -> i32 {
    switch kind {
        case 0: { return 12; }
        case 1: { return 21; }
        case 2: { return 14; }
        default: { return 23; }
    }
}

// Point j of the doodle path; z = 1 when the segment ending here is inked, 0 for a pen-up move.
fn path_pt(d: Doodle, j: i32) -> vec3<f32> {
    switch d.kind {
        case 0: {
            if (j < 6) {
                let a = -PI * 0.5 + f32(j) * 0.4 * TAU;
                return vec3<f32>(d.a + vec2<f32>(cos(a), sin(a)) * d.size, 1.0);
            }
            let a = -PI * 0.5 + f32(j - 6) * 0.4 * TAU;
            let c = d.a + vec2<f32>(1.25, -0.7) * d.size;
            return vec3<f32>(c + vec2<f32>(cos(a), sin(a)) * d.size * 0.42, select(1.0, 0.0, j == 6));
        }
        case 1: {
            let w = d.size;
            if (j < 15) {
                let t = f32(j) / 14.0;
                let y = w * (0.09 * sin(PI * t) - 0.05 * t - 0.32 * pow(t, 6.0));
                return vec3<f32>(d.a + vec2<f32>(mix(-w, w, t), y), 1.0);
            }
            let t = f32(j - 15) / 5.0;
            return vec3<f32>(d.a + vec2<f32>(mix(-0.75, 0.5, t) * w, w * (0.2 + 0.04 * sin(PI * t))), select(1.0, 0.0, j == 15));
        }
        case 2: {
            let axis = d.b - d.a;
            let perp = vec2<f32>(-axis.y, axis.x);
            if (j <= 10) {
                let t = f32(j) / 10.0;
                return vec3<f32>(d.a + axis * t + perp * 0.12 * sin(PI * t), 1.0);
            }
            if (j == 12) {
                return vec3<f32>(d.b, 1.0);
            }
            // Head barbs, measured from the end tangent of the bent shaft.
            let back = normalize(-axis + perp * 0.12 * PI);
            let s = select(-0.5, 0.5, j == 11);
            let r = vec2<f32>(back.x * cos(s) - back.y * sin(s), back.x * sin(s) + back.y * cos(s));
            return vec3<f32>(d.b + r * 0.3 * d.size, select(1.0, 0.0, j == 11));
        }
        default: {
            let t = f32(j) / 22.0;
            let a = d.b.x + t * TAU * 1.12;
            let r = d.size * (1.0 + 0.07 * sin(t * 7.0 + d.b.y));
            return vec3<f32>(d.a + vec2<f32>(cos(a) * 1.18, sin(a) * 0.92) * r, 1.0);
        }
    }
}

fn path_len(d: Doodle) -> f32 {
    let n = path_count(d.kind);
    var prev = path_pt(d, 0).xy;
    var total = 0.0;
    for (var j = 1; j < n; j++) {
        let p = path_pt(d, j).xy;
        total += length(p - prev);
        prev = p;
    }
    return total;
}

fn path_at(d: Doodle, cut: f32) -> vec2<f32> {
    let n = path_count(d.kind);
    var prev = path_pt(d, 0).xy;
    var acc = 0.0;
    for (var j = 1; j < n; j++) {
        let p = path_pt(d, j).xy;
        let l = length(p - prev);
        if (acc + l >= cut) {
            return mix(prev, p, clamp((cut - acc) / max(l, 1e-4), 0.0, 1.0));
        }
        acc += l;
        prev = p;
    }
    return prev;
}

// Signed offset from q to the nearest inked point between arc lengths c0..c1 (xy), and its length (z).
fn ink_dist(d: Doodle, q: vec2<f32>, c0: f32, c1: f32) -> vec3<f32> {
    let n = path_count(d.kind);
    var prev = path_pt(d, 0).xy;
    var acc = 0.0;
    var best = vec3<f32>(0.0, 0.0, 1e9);
    for (var j = 1; j < n; j++) {
        let pt = path_pt(d, j);
        let l = length(pt.xy - prev);
        let lo = max(acc, c0);
        let hi = min(acc + l, c1);
        if (pt.z > 0.5 && hi > lo && l > 1e-4) {
            let s0 = mix(prev, pt.xy, (lo - acc) / l);
            let s1 = mix(prev, pt.xy, (hi - acc) / l);
            let e = s1 - s0;
            let h = clamp(dot(q - s0, e) / max(dot(e, e), 1e-6), 0.0, 1.0);
            let o = q - (s0 + e * h);
            let dd = length(o);
            if (dd < best.z) {
                best = vec3<f32>(o, dd);
            }
        }
        acc += l;
        prev = pt.xy;
    }
    return best;
}

fn brush_px(p: vec2<i32>) -> u32 {
    if (any(p < vec2<i32>(0)) || any(p > vec2<i32>(15))) {
        return 0u;
    }
    let w = BRUSH[p.y * 2 + p.x / 8];
    return (w >> (u32(p.x % 8) * 4u)) & 15u;
}

// Doodle `d` at grid cell `cell` (center q): ink, shadow and cursors. rgb premultiplied, a coverage.
fn doodle_px(d: Doodle, cell: vec2<i32>) -> vec4<f32> {
    let q = vec2<f32>(cell) + 0.5;
    if (d.age < 0.0 || d.age > t_total() + 0.2 || length(q - d.bc) > d.br + 20.0) {
        return vec4<f32>(0.0);
    }
    let td = t_draw();
    let te = t_erase();
    let erase_start = td + max(p_linger(), 0.0);
    let total = path_len(d);
    let draw = clamp(d.age / td, 0.0, 1.0);
    let erase = clamp((d.age - erase_start) / te, 0.0, 1.0);
    // Ease each phase a little so strokes start and land like a hand.
    let c1 = total * (draw * draw * (3.0 - 2.0 * draw) * 0.35 + draw * 0.65);
    let c0 = total * erase;
    let radius = 2.1 * p_brush(); // in grid cells: a chunkier grid paints chunkier strokes

    // Brush cursor: while drawing and briefly after.
    if (d.age < td + 0.35) {
        let tip = vec2<i32>(floor(path_at(d, c1)));
        let v = brush_px(vec2<i32>(cell.x - tip.x, cell.y - tip.y + 15));
        if (v != 0u) {
            var c = vec3<f32>(0.0);
            switch v {
                case 2u: { c = vec3<f32>(1.0); }
                case 3u: { c = vec3<f32>(0.5); }
                case 4u: { c = vec3<f32>(0.85, 0.6, 0.25); }
                case 5u: { c = vec3<f32>(0.55, 0.33, 0.1); }
                case 6u: { c = d.color; }
                default: { c = vec3<f32>(0.0); }
            }
            return vec4<f32>(c, 2.0); // a > 1: cursor, ignores ink opacity
        }
        // Snare flecks flicked off the brush tip.
        let snare = s_band_snare();
        if (draw < 1.0 && snare > 0.35) {
            let beat = u32(floor(se.time * 4.0));
            for (var k = 0u; k < 3u; k++) {
                let o = vec2<i32>(i32(hash(beat, 10u + k) * 9.0) - 4, i32(hash(beat, 20u + k) * 9.0) - 4);
                if (all(cell == tip + o) && (abs(o.x) + abs(o.y)) > 2) {
                    return vec4<f32>(d.color, 1.0);
                }
            }
        }
    }
    // Eraser cursor: a hollow square (black outside, white inside) that wipes everything under it.
    let er = i32(ceil(radius)) + 5;
    var wiped = c0;
    if (d.age > erase_start - 0.15 && d.age < erase_start + te + 0.1) {
        let ec = vec2<i32>(floor(path_at(d, c0)));
        let o = abs(cell - ec);
        let m = max(o.x, o.y);
        if (m == er || m == er - 1) {
            return vec4<f32>(select(vec3<f32>(1.0), vec3<f32>(0.0), m == er), 2.0);
        }
        if (m < er - 1) {
            return vec4<f32>(0.0);
        }
        wiped = select(c0, c0 + f32(er), d.age > erase_start);
    }
    if (c1 <= wiped) {
        return vec4<f32>(0.0);
    }
    let hit = ink_dist(d, q, wiped, c1);
    if (hit.z <= radius) {
        return vec4<f32>(d.color, 1.0);
    }
    // One-pixel shadow on the lower right keeps light ink readable on bright footage.
    if (hit.z <= radius + 1.0 && hit.x + hit.y > 0.0) {
        return vec4<f32>(vec3<f32>(0.0), 0.55);
    }
    return vec4<f32>(0.0);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let src = textureSample(se_input, se_sampler, in.uv);
    let amount = clamp(p_amount(), 0.0, 1.0);
    let rsize = max(se.region.zw - se.region.xy, vec2<f32>(1e-6)) * se.resolution;
    let pix = (in.uv - se.region.xy) * se.resolution;
    let s = max(1.0, round(se.resolution.y / 240.0 * p_pixel()));
    let view = rsize / s;
    let cell = vec2<i32>(floor(pix / s));

    var col = vec4<f32>(0.0);
    // Triggered doodle.
    let total = t_total();
    let age = se.trigger_age;
    let tstart = se.time - age;
    if (se.trigger_count > 0u && age < total + 0.2) {
        let n = se.trigger_count;
        let kind = ORDER[(n - 1u) % 4u];
        let scale = 1.0 + 0.15 * (clamp(se.trigger.tier, 1.0, 3.0) - 1.0);
        col = doodle_px(make_doodle(kind, n, age, scale, view), cell);
    }
    // Auto doodles on a fixed clock, skipped when they would overlap a triggered one.
    if (col.a <= 0.0 && p_auto() && s_beat_bpm() > 1.0) {
        let period = total + 1.5;
        let k = floor(se.time / period);
        let start = k * period;
        let clash = se.trigger_count > 0u && start < tstart + total + 0.2 && start + total + 0.2 > tstart;
        if (!clash) {
            col = doodle_px(make_doodle(i32(u32(k) % 4u), 7919u + u32(k), se.time - start, 1.0, view), cell);
        }
    }
    let a = select(col.a * amount, 1.0, col.a > 1.0);
    let rgb = select(col.rgb * a, col.rgb, col.a > 1.0);
    return vec4<f32>(rgb + src.rgb * (1.0 - a), a + src.a * (1.0 - a));
}
