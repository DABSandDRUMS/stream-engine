// Sticker pops: up to four 28x28 4-bit pixel stickers, one per frame corner. Each springs in
// from off-screen with a damped overshoot (driven by se.trigger_age), wobbles, casts a hard pixel
// drop shadow, and peels off from its inner corner as the trigger envelope releases: the part past
// the fold line is gone and its mirror image is drawn as the curled paper back.
// Sticker art: nibble 0 clear, 1 white die-cut border, 2 outline, 3.. fill colours (see COLORS).
const GRID: f32 = 28.0;
const HALF: f32 = 14.0;
const WORDS: u32 = 112u;

// heart, star, WOW bubble, smiley; 4 u32 per row (8 pixels each, x = 0 in the low nibble)
const STK = array<u32, 448>(
    0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u,
    0x11110000u, 0x11111111u, 0x11111111u, 0x00000000u, 0x11111000u, 0x11111111u, 0x11111111u, 0x00000001u,
    0x22211100u, 0x21122222u, 0x12222222u, 0x00000011u, 0x33221110u, 0x22223333u, 0x22433333u, 0x00000111u,
    0x33322111u, 0x32233333u, 0x24333333u, 0x00001112u, 0x55332211u, 0x33333333u, 0x43333333u, 0x00001122u,
    0x55533211u, 0x33333335u, 0x33333333u, 0x00001124u, 0x15533211u, 0x33333333u, 0x33333333u, 0x00001124u,
    0x35533211u, 0x33333333u, 0x33333333u, 0x00001124u, 0x33333211u, 0x33333333u, 0x33333333u, 0x00001124u,
    0x33333211u, 0x33333333u, 0x43333333u, 0x00001124u, 0x33332211u, 0x33333333u, 0x44333333u, 0x00001122u,
    0x33322111u, 0x33333333u, 0x24433333u, 0x00001112u, 0x33221110u, 0x33333333u, 0x22443333u, 0x00000111u,
    0x32211100u, 0x33333333u, 0x12244333u, 0x00000011u, 0x22111000u, 0x33333333u, 0x11224433u, 0x00000001u,
    0x21110000u, 0x33333332u, 0x11122443u, 0x00000000u, 0x11100000u, 0x33333322u, 0x01112244u, 0x00000000u,
    0x11000000u, 0x43333221u, 0x00111224u, 0x00000000u, 0x10000000u, 0x44332211u, 0x00011122u, 0x00000000u,
    0x00000000u, 0x24322111u, 0x00001112u, 0x00000000u, 0x00000000u, 0x22221110u, 0x00000111u, 0x00000000u,
    0x00000000u, 0x11111100u, 0x00000011u, 0x00000000u, 0x00000000u, 0x11111000u, 0x00000001u, 0x00000000u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u,
    0x00000000u, 0x11111000u, 0x00000001u, 0x00000000u, 0x00000000u, 0x11111100u, 0x00000011u, 0x00000000u,
    0x00000000u, 0x22221110u, 0x00000111u, 0x00000000u, 0x00000000u, 0x27622110u, 0x00000112u, 0x00000000u,
    0x00000000u, 0x76682111u, 0x00001112u, 0x00000000u, 0x00000000u, 0x76682211u, 0x00001122u, 0x00000000u,
    0x11111110u, 0x66686211u, 0x11111127u, 0x00000111u, 0x11111111u, 0x66666221u, 0x11111227u, 0x00001111u,
    0x22222211u, 0x66666622u, 0x22222276u, 0x00001122u, 0x88866211u, 0x66666666u, 0x76666666u, 0x00001127u,
    0x68662211u, 0x66666666u, 0x77666666u, 0x00001122u, 0x66622111u, 0x26666266u, 0x27766666u, 0x00001112u,
    0x66221110u, 0x26666266u, 0x22776666u, 0x00000111u, 0x62211100u, 0x66226656u, 0x12277665u, 0x00000011u,
    0x22111000u, 0x66666666u, 0x11227666u, 0x00000001u, 0x22110000u, 0x66776666u, 0x11227666u, 0x00000000u,
    0x62111000u, 0x62277666u, 0x11276666u, 0x00000001u, 0x62211000u, 0x22227666u, 0x12276667u, 0x00000001u,
    0x66211100u, 0x21122266u, 0x12766622u, 0x00000011u, 0x66221100u, 0x11111226u, 0x22767221u, 0x00000011u,
    0x26621100u, 0x11111122u, 0x27722211u, 0x00000011u, 0x22221100u, 0x00001111u, 0x22221111u, 0x00000011u,
    0x11111100u, 0x00000111u, 0x11111110u, 0x00000011u, 0x11111000u, 0x00000001u, 0x11111000u, 0x00000001u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x11110000u, 0x11111111u, 0x11111111u, 0x00000000u,
    0x11111100u, 0x11111111u, 0x11111111u, 0x00000011u, 0x22211110u, 0x22222222u, 0x12222222u, 0x00000111u,
    0x99222111u, 0x99999999u, 0x22a99999u, 0x00001112u, 0x11192211u, 0x99999999u, 0xa9999999u, 0x00001122u,
    0x99919211u, 0x99999999u, 0x99999999u, 0x0000112au, 0x99999211u, 0x99999999u, 0x99999999u, 0x0000112au,
    0x99699211u, 0x96666969u, 0x96969996u, 0x0000112au, 0x99699211u, 0x96996969u, 0x96969996u, 0x0000112au,
    0x69699211u, 0x96996969u, 0x96969696u, 0x0000112au, 0x69699211u, 0x96996969u, 0x99969696u, 0x0000112au,
    0x96999211u, 0x96666996u, 0x96996969u, 0x0000112au, 0x99999211u, 0x99999999u, 0x99999999u, 0x0000112au,
    0x99999211u, 0x99999999u, 0xa9999999u, 0x0000112au, 0x99992211u, 0x99999999u, 0xaaa99999u, 0x00001122u,
    0x99222111u, 0xaaaaaa99u, 0x22aaaaaau, 0x00001112u, 0x92211110u, 0x22222299u, 0x12222222u, 0x00000111u,
    0x92111100u, 0x11111229u, 0x11111111u, 0x00000011u, 0x92110000u, 0x11111122u, 0x11111111u, 0x00000000u,
    0x22110000u, 0x00001112u, 0x00000000u, 0x00000000u, 0x11110000u, 0x00000111u, 0x00000000u, 0x00000000u,
    0x11100000u, 0x00000011u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u,
    0x00000000u, 0x11111111u, 0x00001111u, 0x00000000u, 0x11000000u, 0x11111111u, 0x00111111u, 0x00000000u,
    0x11100000u, 0x22222221u, 0x01111222u, 0x00000000u, 0x21110000u, 0x66666622u, 0x11122276u, 0x00000000u,
    0x22111000u, 0x66666666u, 0x11227666u, 0x00000001u, 0x62211100u, 0x66666666u, 0x12276666u, 0x00000011u,
    0x66221110u, 0x66666688u, 0x22766666u, 0x00000111u, 0x86622110u, 0x66666688u, 0x27666666u, 0x00000112u,
    0x86662111u, 0x66666668u, 0x76666666u, 0x00001112u, 0x66662211u, 0x66662266u, 0x76666226u, 0x00001122u,
    0x66666211u, 0x66662266u, 0x66666226u, 0x00001127u, 0x66666211u, 0x66662266u, 0x66666226u, 0x00001127u,
    0x66666211u, 0x66666666u, 0x66666666u, 0x00001127u, 0x56666211u, 0x66666665u, 0x66556666u, 0x00001127u,
    0x56666211u, 0x66666265u, 0x66556266u, 0x00001127u, 0x66666211u, 0x66662666u, 0x66666626u, 0x00001127u,
    0x66666211u, 0x22226666u, 0x66666662u, 0x00001127u, 0x66666211u, 0x66666666u, 0x76666666u, 0x00001127u,
    0x66662211u, 0x66666666u, 0x76666666u, 0x00001122u, 0x66662111u, 0x66666666u, 0x77666666u, 0x00001112u,
    0x66622110u, 0x66666666u, 0x27766666u, 0x00000112u, 0x66221110u, 0x66666666u, 0x22776666u, 0x00000111u,
    0x62211100u, 0x66666666u, 0x12277666u, 0x00000011u, 0x22111000u, 0x66666666u, 0x11227776u, 0x00000001u,
    0x21110000u, 0x77776622u, 0x11122277u, 0x00000000u, 0x11100000u, 0x22222221u, 0x01111222u, 0x00000000u,
    0x11000000u, 0x11111111u, 0x00111111u, 0x00000000u, 0x00000000u, 0x11111111u, 0x00001111u, 0x00000000u,
);

const COLORS = array<vec3<f32>, 11>(
    vec3<f32>(0.0, 0.0, 0.0),
    vec3<f32>(1.0, 1.0, 1.0),          // die-cut border
    vec3<f32>(0.118, 0.086, 0.188),    // outline
    vec3<f32>(0.910, 0.157, 0.235),    // red
    vec3<f32>(0.639, 0.071, 0.180),    // dark red
    vec3<f32>(1.0, 0.612, 0.706),      // pink
    vec3<f32>(1.0, 0.824, 0.235),      // yellow
    vec3<f32>(0.941, 0.549, 0.118),    // orange
    vec3<f32>(1.0, 0.957, 0.659),      // pale yellow
    vec3<f32>(0.878, 0.188, 0.604),    // magenta
    vec3<f32>(0.549, 0.078, 0.392),    // dark magenta
);

fn texel(kind: u32, q: vec2<f32>) -> u32 {
    let g = q + vec2<f32>(HALF);
    if g.x < 0.0 || g.y < 0.0 || g.x >= GRID || g.y >= GRID {
        return 0u;
    }
    let x = u32(g.x);
    let w = STK[kind * WORDS + u32(g.y) * 4u + (x >> 3u)];
    return (w >> ((x & 7u) * 4u)) & 15u;
}

struct Sticker {
    kind: u32,
    center: vec2<f32>,  // pixels
    unit: f32,          // screen pixels per sticker pixel (after pop scale)
    rot: f32,
    fold_dir: vec2<f32>,
    fold: f32,          // fold line offset along fold_dir (sticker pixels)
    flap_alpha: f32,
    lift: f32,          // 0 resting .. 1 in the air (shadow distance)
    visible: bool,
};

fn sticker(j: u32, n: u32) -> Sticker {
    var s: Sticker;
    let tc = se.trigger_count;
    let t = se.trigger_age - f32(j) * p_stagger();
    s.visible = t >= 0.0 && j < n;
    s.kind = (tc + j) % 4u;
    let corner = (tc * 3u + j * 2u + j / 2u) % 4u;
    // inward diagonal of this corner (y down)
    let inward = vec2<f32>(select(-1.0, 1.0, corner == 0u || corner == 3u), select(-1.0, 1.0, corner < 2u));
    let size = p_size() * se.resolution.y;
    let unit = max(1.0, round(size / GRID));
    let anchor = vec2<f32>(
        select(se.resolution.x, 0.0, inward.x > 0.0),
        select(se.resolution.y, 0.0, inward.y > 0.0),
    ) + inward * (p_margin() * se.resolution.y + unit * HALF);

    // damped springs: fly in along the diagonal, pop the scale, settle the tilt
    let b = p_bounce();
    let tt = max(t, 0.0);
    let fly = exp(-7.0 * tt) * cos(13.0 * tt);
    let pop = 1.0 - (0.75 * exp(-8.0 * tt) * cos(15.0 * tt)) * mix(0.6, 1.0, min(b, 1.0)) + 0.03 * s_band_kick();
    let seed = f32((tc * 7u + j * 3u) % 5u);
    let rest = radians(p_tilt()) * (0.7 + 0.15 * seed) * -inward.x * inward.y;
    let wobble = b * (0.45 * exp(-4.5 * tt) * sin(11.0 * tt + 0.6) + 0.025 * sin(se.time * 2.1 + f32(j) * 1.7));
    s.center = anchor - inward * fly * unit * GRID * 1.3 * b;
    s.unit = unit * max(pop, 0.05);
    s.rot = rest + wobble;
    s.lift = clamp(exp(-6.0 * tt), 0.0, 1.0);

    // peel from the corner that faces the frame centre once the envelope releases
    var peel = 0.0;
    if se.trigger_age > 0.25 {
        peel = clamp((1.0 - se.env) * 1.25 - f32(j) * 0.08, 0.0, 1.0);
    }
    s.fold_dir = normalize(inward);
    let reach = HALF * 1.42;
    s.fold = reach * (1.0 - 2.0 * peel * peel * (3.0 - 2.0 * peel));
    s.flap_alpha = 1.0 - smoothstep(0.65, 1.0, peel);
    s.visible = s.visible && peel < 1.0;
    return s;
}

// premultiplied colour of sticker + curled flap at local point q (sticker pixels)
fn shade(s: Sticker, q: vec2<f32>) -> vec4<f32> {
    let side = dot(q, s.fold_dir) - s.fold;
    var c = vec4<f32>(0.0);
    if side < 0.0 {
        let k = texel(s.kind, q);
        if k != 0u {
            // contact shadow of the curl just before the fold
            let dim = 1.0 - 0.35 * (1.0 - smoothstep(0.0, 3.0, -side)) * step(s.fold, HALF * 1.4);
            c = vec4<f32>(COLORS[k] * dim, 1.0);
        }
        // the lifted part, mirrored over the fold, shows its paper back: banded curl shading
        // (pixel-art style) and a slightly greyer die-cut ring so the flap keeps its silhouette
        let m = q - 2.0 * side * s.fold_dir;
        let km = texel(s.kind, m);
        if km != 0u {
            let curl = floor(smoothstep(0.0, 9.0, -side) * 3.0 + 0.5) / 3.0;
            var paper = mix(vec3<f32>(0.66, 0.66, 0.75), vec3<f32>(0.96, 0.96, 0.99), curl);
            if km == 1u {
                paper = paper * 0.86;
            }
            let a = s.flap_alpha;
            c = vec4<f32>(paper * a, a) + c * (1.0 - a);
        }
    }
    return c;
}

fn to_local(s: Sticker, p: vec2<f32>) -> vec2<f32> {
    let r = p - s.center;
    let cs = cos(s.rot);
    let sn = sin(s.rot);
    return vec2<f32>(cs * r.x + sn * r.y, -sn * r.x + cs * r.y) / s.unit;
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let px = in.uv * se.resolution;
    var n = 1u + u32(clamp(floor(log2(max(se.trigger.amount, se.trigger.count * 100.0) / 100.0 + 1e-3)), 0.0, 3.0));
    n = min(n, u32(clamp(p_max_stickers(), 1, 4)));
    var out = vec4<f32>(0.0);
    for (var j = 0u; j < 4u; j++) {
        let s = sticker(j, n);
        if !s.visible {
            continue;
        }
        let q = to_local(s, px);
        // the flap can swing out past the sticker; cull everything else
        if dot(q, q) > (HALF * 2.9) * (HALF * 2.9) {
            continue;
        }
        // hard pixel drop shadow, further away while the sticker is still in the air
        let off = vec2<f32>(0.9, 1.3) * (1.0 + 4.0 * s.lift) * max(1.0, s.unit / 4.0);
        let qs = to_local(s, px - off);
        let sh = shade(s, qs).a * 0.42;
        // 2x2 supersampled sticker so rotated pixel edges don't crawl
        var c = vec4<f32>(0.0);
        for (var k = 0u; k < 4u; k++) {
            let o = vec2<f32>(f32(k & 1u), f32(k >> 1u)) * 0.5 - vec2<f32>(0.25);
            c = c + shade(s, to_local(s, px + o));
        }
        c = c * 0.25;
        let layer = c + vec4<f32>(0.0, 0.0, 0.0, sh) * (1.0 - c.a);
        out = layer + out * (1.0 - layer.a);
    }
    return out;
}
