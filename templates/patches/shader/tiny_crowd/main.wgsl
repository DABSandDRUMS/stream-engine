// Tiny crowd: fans seen from behind, standing along the bottom edge of the attached node
// (se.region), on a whole-pixel grid. Each slot holds at most one fan; a pixel only looks at its
// own slot and the two neighbours, so the cost is flat. Poses: bob/nod on the beat, one arm up on
// snare hits for a random share, and after a trigger a cheer wave (jumping, both arms up, hearts)
// spreading out from the middle.

const SLOT = 9;          // slot width in crowd pixels
const ROWS = 16.0;       // crowd height in crowd pixels (sets the pixel size)
const OUTLINE = vec3<f32>(0.05, 0.04, 0.07);

const SKIN = array<vec3<f32>, 6>(
    vec3<f32>(1.0, 0.86, 0.67), vec3<f32>(0.95, 0.76, 0.49), vec3<f32>(0.88, 0.67, 0.41),
    vec3<f32>(0.78, 0.53, 0.26), vec3<f32>(0.55, 0.33, 0.14), vec3<f32>(0.36, 0.23, 0.13));
const HAIR = array<vec3<f32>, 8>(
    vec3<f32>(0.08, 0.06, 0.05), vec3<f32>(0.18, 0.11, 0.06), vec3<f32>(0.33, 0.19, 0.08),
    vec3<f32>(0.63, 0.32, 0.18), vec3<f32>(0.9, 0.76, 0.36), vec3<f32>(0.7, 0.7, 0.72),
    vec3<f32>(0.8, 0.2, 0.15), vec3<f32>(0.45, 0.2, 0.7));
const SHIRT = array<vec3<f32>, 10>(
    vec3<f32>(0.91, 0.3, 0.24), vec3<f32>(0.2, 0.6, 0.86), vec3<f32>(0.18, 0.8, 0.44),
    vec3<f32>(0.95, 0.77, 0.06), vec3<f32>(0.61, 0.35, 0.71), vec3<f32>(0.9, 0.49, 0.13),
    vec3<f32>(0.1, 0.74, 0.61), vec3<f32>(0.93, 0.94, 0.95), vec3<f32>(0.2, 0.29, 0.37),
    vec3<f32>(1.0, 0.44, 0.71));
// Heart, 7x6, row 0 on top; bit x = column x.
const HEART = array<u32, 6>(0x36u, 0x7fu, 0x7fu, 0x3eu, 0x1cu, 0x08u);

fn hash(i: i32, salt: u32) -> f32 {
    var v = bitcast<u32>(i) * 747796405u + salt * 2891336453u + 12345u;
    v = ((v >> ((v >> 28u) + 4u)) ^ v) * 277803737u;
    v = (v >> 22u) ^ v;
    return f32(v & 0xffffu) / 65536.0;
}

fn pick(i: i32, salt: u32, n: i32) -> i32 {
    return min(i32(hash(i, salt) * f32(n)), n - 1);
}

struct Fan {
    present: bool,
    x: i32,          // center column
    lift: i32,       // whole-body offset (jump, bob)
    nod: i32,        // head offset
    shoulder: i32,   // torso height
    half: i32,       // torso half width
    hair_style: i32,
    arms: i32,       // 0 down, 1 right arm up, 2 both arms up (V)
    skin: vec3<f32>,
    hair: vec3<f32>,
    shirt: vec3<f32>,
    cheer: f32,
    age: f32,        // seconds into this fan's cheer
}

struct Ctx {
    beat: f32,       // beat phase 0-1
    beat_n: i32,     // beat counter for random choices
    snare: f32,
    age: f32,        // seconds since the trigger
    center: f32,     // middle slot
    tier: f32,
}

fn cheer_env(age: f32) -> f32 {
    let len = p_cheer();
    return smoothstep(0.0, 0.12, age) * (1.0 - smoothstep(len - 0.7, len, age));
}

fn fan(i: i32, c: Ctx) -> Fan {
    var f: Fan;
    f.present = hash(i, 1u) < p_density();
    f.x = i * SLOT + 4 + pick(i, 2u, 3) - 1;
    f.shoulder = 6 + pick(i, 3u, 4);
    f.half = select(2, 3, hash(i, 4u) < 0.55);
    f.hair_style = pick(i, 5u, 5);
    f.skin = SKIN[pick(i, 6u, 6)];
    f.hair = HAIR[pick(i, 7u, 8)];
    f.shirt = SHIRT[pick(i, 8u, 10)];
    // Cheer wave from the middle outward.
    f.age = c.age - 0.018 * abs(f32(i) - c.center) - 0.08 * hash(i, 9u);
    f.cheer = select(0.0, cheer_env(f.age), f.age >= 0.0);
    // Groove: bob on the beat, nod on the beat, bob on the off-beat, or stand still.
    let style = pick(i, 10u, 4);
    let ph = fract(c.beat + select(0.0, 0.5, style == 2) - 0.04 * hash(i, 11u));
    let down = ph < 0.3;
    f.lift = select(0, -1, down && (style == 0 || style == 2));
    f.nod = select(0, -1, down && style == 1);
    f.arms = select(0, 1, c.snare > 0.12 && hash(i * 31 + c.beat_n, 12u) < p_arms());
    if (f.cheer > 0.05) {
        let height = 2.0 + c.tier;
        let hop = abs(sin(3.14159 * (f.age * (1.9 + 0.5 * hash(i, 13u)) + hash(i, 14u))));
        f.lift = i32(round(height * f.cheer * hop));
        f.nod = 0;
        f.arms = select(1, 2, hash(i, 15u) < 0.8 || f.cheer > 0.6);
    }
    return f;
}

// Color + coverage of fan `f` at crowd pixel q (x right, y up from the bottom edge).
fn fan_px(f: Fan, q: vec2<i32>) -> vec4<f32> {
    if (!f.present) {
        return vec4<f32>(0.0);
    }
    let x = q.x - f.x;
    let ax = abs(x);
    let y = q.y - f.lift;
    let sh = f.shoulder;
    if (ax > f.half + 3 || y < -8 || y > sh + 9) {
        return vec4<f32>(0.0);
    }
    // Head (5 rows, rounded), seen from behind.
    let r = y - sh - f.nod;
    if (r >= 0 && r <= 4 && ax <= select(2, 1, r == 0 || r == 4)) {
        switch f.hair_style {
            case 0: { // short hair: neck and ears show
                return vec4<f32>(select(f.hair, f.skin, r == 0 || (r == 1 && ax == 2)), 1.0);
            }
            case 3: { // cap in a contrasting shirt color
                let cap = SHIRT[(u32(f.x) / 9u + 3u) % 10u];
                return vec4<f32>(select(select(f.hair, cap, r >= 3), f.skin, r == 0), 1.0);
            }
            case 4: { // buzz cut
                return vec4<f32>(select(f.skin, f.hair * 0.8 + f.skin * 0.2, r >= 2), 1.0);
            }
            default: { // long hair / bun
                return vec4<f32>(select(f.hair, f.skin, r == 0 && f.hair_style == 2), 1.0);
            }
        }
    }
    if (f.hair_style == 2 && ((r == 5 && ax <= 1) || (r == 6 && x == 0))) {
        return vec4<f32>(f.hair, 1.0);
    }
    // Long hair falls over the shoulders.
    if (f.hair_style == 1 && ax <= 2 && y >= sh - 2 && y < sh) {
        return vec4<f32>(f.hair, 1.0);
    }
    // Torso with clipped shoulder corners; some shirts have a stripe.
    if (y >= 0 && y < sh && ax <= f.half && !(y == sh - 1 && ax == f.half)) {
        let stripe = hash(f.x, 16u) < 0.3 && y == sh - 3;
        return vec4<f32>(select(f.shirt, f.shirt * 0.55, stripe), 1.0);
    }
    // Legs only show while jumping clear of the bottom edge.
    if (y < 0 && ax >= 1 && ax <= f.half - 1) {
        return vec4<f32>(select(vec3<f32>(0.16, 0.2, 0.36), vec3<f32>(0.12, 0.11, 0.12), hash(f.x, 17u) < 0.4), 1.0);
    }
    // Arms.
    let side_up = (f.arms == 2) || (f.arms == 1 && x > 0);
    let ay = y - (sh - 2);
    if (side_up && ay >= 0 && ay <= 8) {
        let reach = f.half + 1 + select(0, ay / 3, f.arms == 2);
        let fist = ay >= 7;
        if (ax == reach || (fist && ax == reach + select(-1, 1, f.arms == 2))) {
            return vec4<f32>(select(f.skin * 0.92, f.shirt * 0.85, ay < 2), 1.0);
        }
    } else if (ax == f.half + 1 && y >= 1 && y < sh - 1) {
        return vec4<f32>(select(f.skin * 0.85, f.shirt * 0.75, y >= sh - 3), 1.0);
    }
    return vec4<f32>(0.0);
}

fn crowd_px(q: vec2<i32>, slot: i32, c: Ctx) -> vec4<f32> {
    let a = fan_px(fan(slot, c), q);
    if (a.a > 0.0) { return a; }
    let b = fan_px(fan(slot - 1, c), q);
    if (b.a > 0.0) { return b; }
    return fan_px(fan(slot + 1, c), q);
}

fn bayer4(p: vec2<i32>) -> f32 {
    let m = array<f32, 16>(0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0);
    return (m[(p.y & 3) * 4 + (p.x & 3)] + 0.5) / 16.0;
}

fn heart_on(h: vec2<i32>) -> bool {
    return h.x >= 0 && h.x < 7 && h.y >= 0 && h.y < 6 && ((HEART[h.y] >> u32(h.x)) & 1u) == 1u;
}

// Hearts rising from cheering fans in this slot. Returns color + coverage.
fn heart_px(q: vec2<i32>, slot: i32, c: Ctx) -> vec4<f32> {
    for (var k = -1; k <= 1; k++) {
        let i = slot + k;
        let f = fan(i, c);
        if (!f.present || f.cheer <= 0.0 || hash(i, 20u) > 0.3 + 0.2 * c.tier) {
            continue;
        }
        let u = f.age * 0.85 - 0.6 * hash(i, 21u);
        if (u < 0.0) {
            continue; // this fan's first heart hasn't left yet
        }
        let t = fract(u);
        let base = vec2<f32>(f32(f.x) - 3.0 + 2.0 * sin(t * 9.0 + 6.0 * hash(i, 22u)), f32(f.shoulder + 7) + t * 22.0);
        let h = vec2<i32>(q.x - i32(round(base.x)), i32(round(base.y)) + 5 - q.y);
        // Fade out by dithering near the top of the rise and with the cheer.
        if (bayer4(q) > min(f.cheer * 1.5, 1.0) * (1.0 - smoothstep(0.65, 1.0, t))) {
            continue;
        }
        if (heart_on(h)) {
            let shine = h.x == 1 && h.y == 1;
            return vec4<f32>(select(vec3<f32>(1.0, 0.16, 0.35), vec3<f32>(1.0, 0.8, 0.88), shine), 1.0);
        }
        if (heart_on(h + vec2<i32>(1, 0)) || heart_on(h - vec2<i32>(1, 0)) || heart_on(h + vec2<i32>(0, 1)) || heart_on(h - vec2<i32>(0, 1))) {
            return vec4<f32>(vec3<f32>(0.3, 0.02, 0.08), 1.0);
        }
    }
    return vec4<f32>(0.0);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let src = textureSample(se_input, se_sampler, in.uv);
    let amount = clamp(p_amount(), 0.0, 1.0);
    let rsize = max(se.region.zw - se.region.xy, vec2<f32>(1e-6)) * se.resolution;
    let pix = (in.uv - se.region.xy) * se.resolution;
    let s = max(1.0, round(p_height() * rsize.y / ROWS));
    let cols = i32(floor(rsize.x / s));
    let margin = (cols % SLOT) / 2;
    let q = vec2<i32>(i32(floor(pix.x / s)) - margin, i32(floor((rsize.y - pix.y) / s)));
    // Hearts can rise about two crowd heights above the band.
    if (amount <= 0.0 || q.y < 0 || q.y > i32(ROWS) * 3) {
        return src;
    }
    var c: Ctx;
    let bpm = s_beat_bpm();
    c.beat = select(fract(se.time), s_beat_phase(), bpm > 1.0);
    c.beat_n = select(i32(floor(se.time)), i32(floor(se.time * bpm / 60.0 - c.beat + 0.5)), bpm > 1.0);
    c.snare = s_band_snare();
    c.age = se.trigger_age;
    c.center = f32(cols / SLOT) * 0.5;
    c.tier = clamp(se.trigger.tier, 1.0, 3.0);
    let slot = i32(floor(f32(q.x) / f32(SLOT)));

    var col = crowd_px(q, slot, c);
    if (col.a <= 0.0) {
        // 1-pixel dark outline around the silhouettes.
        let n = crowd_px(q + vec2<i32>(1, 0), slot, c).a + crowd_px(q - vec2<i32>(1, 0), slot, c).a
            + crowd_px(q + vec2<i32>(0, 1), slot, c).a + crowd_px(q - vec2<i32>(0, 1), slot, c).a;
        if (n > 0.0) {
            col = vec4<f32>(OUTLINE, 1.0);
        } else {
            col = heart_px(q, slot, c);
        }
    } else if (crowd_px(q + vec2<i32>(0, 1), slot, c).a <= 0.0) {
        // Stage light catches the top edges.
        col = vec4<f32>(min(col.rgb * 1.3 + 0.06, vec3<f32>(1.0)), 1.0);
    }
    let a = col.a * amount;
    return vec4<f32>(col.rgb * a + src.rgb * (1.0 - a), a + src.a * (1.0 - a));
}
