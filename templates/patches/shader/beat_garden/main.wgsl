// Beat garden: pixel-art flowers along the bottom edge, composited over `se_input`.
// The strip is a grid of art pixels (`pixel` screen px at 720p) split into 6-column plant slots.
// Each slot hashes a plant (kind, colour, height, sprout time). Growth uses time snapped to the
// last beat, so stems gain pixels on the beat. Sway is a half-time sine locked to beat.phase
// with a kick push; stems bend as a staircase (row shift grows with height). Blooms: grown heads
// sit half open and open fully when snare/fill energy or the trigger moment passes a per-plant
// threshold. A dark 1-px outline keeps the sprites readable over busy footage.
// Per pixel only the 3 slots that can reach it are built; lookups return a material code and
// colours are resolved once at the end (keeps registers low: the pass is full screen).

const SLOT_W: i32 = 6;
const TAU: f32 = 6.2831853;
const NONE: i32 = -1;
const GRASS: i32 = 5;

// materials
const M_PETAL: u32 = 1u;
const M_SHADE: u32 = 2u;
const M_CENTER: u32 = 3u;
const M_SEPAL: u32 = 4u;
const M_HIGHLIGHT: u32 = 5u;
const M_STEM: u32 = 6u;
const M_LEAF: u32 = 7u;
const M_LEAF_DARK: u32 = 8u;
const M_SOIL: u32 = 9u;
const M_SOIL_LIGHT: u32 = 10u;

// 5 flower kinds x 3 stages (bud, half, open) x 7 rows (bottom row first, row 0 touches the
// stem top). 4 bits per pixel, column c at bits 4c: 0 empty, 1 petal, 2 petal shade, 3 centre,
// 4 sepal green, 5 highlight (= the material codes above).
const SPRITES = array<u32, 105>(
    // tulip
    0x0004000u, 0x0021100u, 0x0021100u, 0x0001000u, 0x0000000u, 0x0000000u, 0x0000000u,
    0x0004000u, 0x0021100u, 0x0211510u, 0x0211110u, 0x0101010u, 0x0000000u, 0x0000000u,
    0x0004000u, 0x0021100u, 0x0211510u, 0x0213110u, 0x2201011u, 0x1001001u, 0x0000000u,
    // daisy
    0x0004000u, 0x0043400u, 0x0011100u, 0x0000000u, 0x0000000u, 0x0000000u, 0x0000000u,
    0x0004000u, 0x0012100u, 0x0113110u, 0x0011100u, 0x0000000u, 0x0000000u, 0x0000000u,
    0x0004000u, 0x0101010u, 0x0023200u, 0x1135311u, 0x0023200u, 0x0101010u, 0x0001000u,
    // poppy
    0x0004000u, 0x0041400u, 0x0021200u, 0x0002000u, 0x0000000u, 0x0000000u, 0x0000000u,
    0x0004000u, 0x0022200u, 0x0213110u, 0x0211110u, 0x0011100u, 0x0000000u, 0x0000000u,
    0x0004000u, 0x0222220u, 0x2233111u, 0x2133511u, 0x2111111u, 0x0110110u, 0x0000000u,
    // sunflower
    0x0004000u, 0x0004000u, 0x0042400u, 0x0044400u, 0x0000000u, 0x0000000u, 0x0000000u,
    0x0004000u, 0x0001000u, 0x0013100u, 0x0133310u, 0x0013100u, 0x0001000u, 0x0000000u,
    0x0014100u, 0x0133310u, 0x1333331u, 0x1333331u, 0x1335331u, 0x0133310u, 0x0011100u,
    // lavender
    0x0004000u, 0x0004000u, 0x0020200u, 0x0004000u, 0x0002000u, 0x0000000u, 0x0000000u,
    0x0004000u, 0x0004000u, 0x0012100u, 0x0004000u, 0x0012100u, 0x0001000u, 0x0000000u,
    0x0004000u, 0x0114110u, 0x0012100u, 0x0114110u, 0x0012100u, 0x0011100u, 0x0001000u,
);

struct Plant {
    kind: i32,
    cx: i32,
    maxh: i32,
    h: i32,
    stage: i32,
    amp: f32,
    pollen: f32,
    seed: f32,
};

fn hash1(n: f32) -> f32 {
    return fract(sin(n * 127.1 + 311.7) * 43758.5453);
}

fn hash2(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn sprite(kind: i32, stage: i32, row: i32, col: i32) -> u32 {
    if row < 0 || row > 6 || col < 0 || col > 6 {
        return 0u;
    }
    let idx = u32((kind * 3 + stage) * 7 + row);
    return (SPRITES[idx] >> u32(4 * col)) & 15u;
}

fn petal_color(kind: i32, h: f32) -> vec3<f32> {
    switch kind {
        case 0: {
            if h < 0.2 { return vec3<f32>(1.0, 0.29, 0.36); }
            if h < 0.4 { return vec3<f32>(1.0, 0.56, 0.78); }
            if h < 0.6 { return vec3<f32>(1.0, 0.70, 0.25); }
            if h < 0.8 { return vec3<f32>(1.0, 0.88, 0.40); }
            return vec3<f32>(0.70, 0.48, 1.0);
        }
        case 1: {
            return select(vec3<f32>(0.98, 0.97, 0.93), vec3<f32>(1.0, 0.84, 0.93), h > 0.6);
        }
        case 2: {
            return select(vec3<f32>(1.0, 0.23, 0.19), vec3<f32>(1.0, 0.48, 0.18), h > 0.55);
        }
        case 3: {
            return vec3<f32>(1.0, 0.76, 0.10);
        }
        default: {
            return select(vec3<f32>(0.65, 0.48, 1.0), vec3<f32>(0.50, 0.61, 1.0), h > 0.6);
        }
    }
}

fn center_color(kind: i32) -> vec3<f32> {
    switch kind {
        case 1: { return vec3<f32>(1.0, 0.81, 0.20); }
        case 2: { return vec3<f32>(0.36, 0.08, 0.12); }
        case 3: { return vec3<f32>(0.48, 0.29, 0.12); }
        default: { return vec3<f32>(1.0, 0.88, 0.40); }
    }
}

// Time of the most recent beat (growth steps land on beats); free-running without a tempo.
fn beat_time() -> f32 {
    let bpm = s_beat_bpm();
    if bpm < 30.0 {
        return floor(se.time * 2.0) * 0.5;
    }
    return se.time - clamp(s_beat_phase(), 0.0, 1.0) * 60.0 / bpm;
}

// Half-time sway phase (0..1 over two beats), continuous across beats.
fn sway_phase() -> f32 {
    let bpm = s_beat_bpm();
    if bpm < 30.0 {
        return se.time * 0.4;
    }
    let ph = clamp(s_beat_phase(), 0.0, 1.0);
    let beats = round(se.time * bpm / 60.0 - ph);
    let parity = beats - 2.0 * floor(beats * 0.5);
    return (parity + ph) * 0.5;
}

// Bloom/pollen moment after a trigger, from seconds since it fired (1e6 before any trigger).
// Bigger tiers hold the full bloom longer.
fn trigger_env() -> f32 {
    let t = se.trigger_age;
    let hold = 2.0 + 0.6 * clamp(se.trigger.tier, 0.0, 3.0);
    return smoothstep(0.0, 0.12, t) * (1.0 - smoothstep(hold, hold + 1.2, t));
}

fn make_plant(slot: i32, rows: i32, cols: i32, tq: f32, sway_ph: f32, bloom: f32, trig: f32) -> Plant {
    var p: Plant;
    p.kind = NONE;
    let fs = f32(slot);
    let h0 = hash1(fs);
    let h1 = hash1(fs + 17.3);
    p.cx = slot * SLOT_W + 2 + i32(h1 * 2.99);
    let xf = (f32(p.cx) + 0.5) / f32(max(cols, 1));
    if h0 > p_density() || abs(xf - 0.5) < p_center_gap() * 0.5 || slot < 0 {
        return p;
    }
    // sprout time: a head-start share is grown already, the rest sprout over fill_time
    let u = hash1(fs + 41.9);
    var ts = -p_grow_time();
    let hs = clamp(p_head_start(), 0.0, 1.0);
    if u >= hs {
        ts = pow((u - hs) / max(1.0 - hs, 0.001), 1.3) * p_fill_time();
    }
    if tq < ts {
        return p;
    }
    let g = clamp((tq - ts) / p_grow_time(), 0.0, 1.0);
    let h4 = hash1(fs + 93.7);
    p.seed = h4;
    var kind = i32(hash1(fs + 77.1) * 5.0);
    if h0 / max(p_density(), 0.001) < 0.18 {
        kind = GRASS;
    }
    p.kind = kind;
    let room = max(rows - 9, 3);
    if kind == GRASS {
        p.maxh = 2 + i32(h1 * 3.99);
    } else {
        let frac = select(0.35 + 0.6 * h1, 0.75 + 0.25 * h1, kind == 3);
        p.maxh = max(i32(round(f32(room) * frac)), 3);
    }
    // stems grow first, heads appear in the last stretch
    let gs = select(clamp(g / 0.7, 0.0, 1.0), g, kind == GRASS);
    p.h = max(i32(ceil(gs * f32(p.maxh))), 1);
    p.stage = -1;
    if kind != GRASS && g >= 0.72 {
        p.stage = 0;
        if g >= 1.0 {
            p.stage = select(1, 2, bloom > 0.3 + 0.55 * h4);
        }
    }
    p.pollen = select(0.0, trig, p.stage == 2);
    // half-time sway, travelling as a slow wave along the strip; kick leans a little further
    let kick = clamp(s_band_kick(), 0.0, 1.0);
    let lean = p_sway() * (0.85 + 0.5 * kick) * sin(TAU * (sway_ph - fs * 0.035 + h4 * 0.08));
    p.amp = clamp(lean, -3.0, 3.0);
    return p;
}

fn shift(p: Plant, ly: i32) -> i32 {
    let k = clamp(f32(ly) / f32(max(p.maxh, 1)), 0.0, 1.0);
    return i32(round(p.amp * k * (0.4 + 0.6 * k)));
}

// Material of plant `p` at art pixel (ax, ly) (ly = rows above the soil); 0 when empty.
fn plant_mat(p: Plant, ax: i32, ly: i32) -> u32 {
    // cheap bounding-box reject: heads are 7 wide, sway stays within +-3 art pixels
    if p.kind == NONE || ly < 0 || ly > p.h + 7 || abs(ax - p.cx) > 6 {
        return 0u;
    }
    if p.kind == GRASS {
        // a tuft of blades at -2, 0, +2 (and +1 for taller tufts)
        let lx = ax - p.cx - shift(p, ly);
        var blade = 0;
        if lx == 0 { blade = p.h; }
        if abs(lx) == 2 { blade = p.h - 1; }
        if lx == 1 && p.h > 3 { blade = p.h - 2; }
        if ly < blade {
            return select(M_STEM, M_LEAF, ly == blade - 1);
        }
        return 0u;
    }
    if ly < p.h {
        let lx = ax - p.cx - shift(p, ly);
        if lx == 0 {
            return M_STEM;
        }
        // sprout leaves at the tip while young
        if p.h <= 3 && ly == p.h - 1 && abs(lx) == 1 {
            return M_LEAF;
        }
        // two leaves on alternating sides once the stem has passed them
        let l1 = max(p.maxh / 3, 1);
        if p.h > l1 + 1 && ((ly == l1 && lx == -1) || (ly == l1 + 1 && (lx == -1 || lx == -2))) {
            return select(M_LEAF, M_LEAF_DARK, ly == l1);
        }
        let l2 = max((p.maxh * 2) / 3, 2);
        if p.maxh > 6 && p.h > l2 + 1 && ((ly == l2 && lx == 1) || (ly == l2 + 1 && (lx == 1 || lx == 2))) {
            return select(M_LEAF, M_LEAF_DARK, ly == l2);
        }
        return 0u;
    }
    if p.stage < 0 {
        return 0u;
    }
    // head sprite sits on the stem top and moves rigidly with it
    return sprite(p.kind, p.stage, ly - p.h, ax - p.cx - shift(p, p.h) + 3);
}

// Material at (ax, ay) with the index of the plant that owns it in bits 8+.
fn garden_mat(ax: i32, ay: i32, soil: i32, p0: Plant, p1: Plant, p2: Plant) -> u32 {
    if ay < 0 {
        return 0u;
    }
    let ly = ay - soil;
    var m = plant_mat(p2, ax, ly);
    if m != 0u {
        return m | (2u << 8u);
    }
    m = plant_mat(p1, ax, ly);
    if m != 0u {
        return m | (1u << 8u);
    }
    m = plant_mat(p0, ax, ly);
    if m != 0u {
        return m;
    }
    if soil > 0 {
        if ay < soil {
            return select(M_SOIL, M_SOIL_LIGHT, hash2(vec2<f32>(f32(ax), f32(ay))) > 0.8);
        }
        // short grass fringe on the soil
        let gh = i32(hash2(vec2<f32>(f32(ax), 7.0)) * 2.99);
        if ly < gh {
            return select(M_LEAF_DARK, M_STEM, ly == gh - 1);
        }
    }
    return 0u;
}

// Rising pollen sparkles above fully open heads while the trigger moment is up.
fn pollen(p: Plant, ax: i32, ly: i32) -> f32 {
    if p.pollen <= 0.01 {
        return 0.0;
    }
    let base = p.h + 7;
    var a = 0.0;
    for (var k = 0; k < 3; k = k + 1) {
        let fk = f32(k);
        let t = fract(se.time * 0.55 + fk / 3.0 + p.seed);
        let y = base + i32(t * 9.0);
        let x = p.cx + shift(p, p.h) + i32(round(sin(se.time * 2.0 + fk * 2.1 + p.seed * 9.0) * 2.5));
        if ax == x && ly == y {
            a = max(a, p.pollen * (1.0 - t));
        }
    }
    return a;
}

fn material_rgb(m: u32, p: Plant, slot: i32) -> vec3<f32> {
    switch m {
        case 1u, 2u, 5u: {
            let c = petal_color(p.kind, hash1(f32(slot) + 5.5));
            if m == M_SHADE { return c * vec3<f32>(0.78, 0.66, 0.72); }
            if m == M_HIGHLIGHT { return mix(c, vec3<f32>(1.0), 0.65); }
            return c;
        }
        case 3u: { return center_color(p.kind); }
        case 4u: { return vec3<f32>(0.24, 0.56, 0.21); }
        case 6u: { return vec3<f32>(0.29, 0.65, 0.24); }
        case 7u: { return vec3<f32>(0.44, 0.80, 0.31); }
        case 8u: { return vec3<f32>(0.24, 0.52, 0.21); }
        case 9u: { return vec3<f32>(0.17, 0.12, 0.09); }
        default: { return vec3<f32>(0.25, 0.18, 0.12); }
    }
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let src = textureSample(se_input, se_sampler, in.uv);
    let res = se.resolution;
    let px = max(p_pixel() * res.y / 720.0, 1.0);
    let rows = i32(p_height() * res.y / px);
    let ay = i32(floor((1.0 - in.uv.y) * res.y / px));
    // above the strip (plus pollen headroom): untouched input
    if ay > rows + 8 || p_amount() <= 0.0 {
        return src;
    }
    let ax = i32(floor(in.uv.x * res.x / px));
    let cols = i32(res.x / px);
    let s0 = ax / SLOT_W;
    let tq = beat_time();
    let sw = sway_phase();
    let trig = trigger_env();
    let bloom = max(trig * 1.2, pow(clamp(s_band_snare(), 0.0, 1.0), 1.3) * p_bloom_gain());
    let p0 = make_plant(s0 - 1, rows, cols, tq, sw, bloom, trig);
    let p1 = make_plant(s0, rows, cols, tq, sw, bloom, trig);
    let p2 = make_plant(s0 + 1, rows, cols, tq, sw, bloom, trig);
    let soil = select(0, 2, p_ground());
    // above every nearby plant (incl. outline and pollen headroom): untouched input
    var top = 2;
    if p0.kind != NONE { top = max(top, p0.h + select(8, 17, p0.pollen > 0.01)); }
    if p1.kind != NONE { top = max(top, p1.h + select(8, 17, p1.pollen > 0.01)); }
    if p2.kind != NONE { top = max(top, p2.h + select(8, 17, p2.pollen > 0.01)); }
    if ay - soil > top {
        return src;
    }
    let m = garden_mat(ax, ay, soil, p0, p1, p2);
    var c = vec4<f32>(0.0);
    if m != 0u {
        let k = i32(m >> 8u);
        var p = p0;
        if k == 1 { p = p1; }
        if k == 2 { p = p2; }
        c = vec4<f32>(material_rgb(m & 255u, p, s0 - 1 + k), 1.0);
    } else {
        // outline: an empty pixel touching the garden gets a soft dark edge
        let n = garden_mat(ax - 1, ay, soil, p0, p1, p2) | garden_mat(ax + 1, ay, soil, p0, p1, p2)
            | garden_mat(ax, ay + 1, soil, p0, p1, p2) | garden_mat(ax, ay - 1, soil, p0, p1, p2);
        if n != 0u {
            c = vec4<f32>(0.04, 0.07, 0.04, 0.55);
        } else if trig > 0.01 {
            let ly = ay - soil;
            let a = max(max(pollen(p0, ax, ly), pollen(p1, ax, ly)), pollen(p2, ax, ly));
            c = vec4<f32>(1.0, 0.95, 0.70, a);
        }
    }
    let a = c.a * clamp(p_amount(), 0.0, 1.0);
    return vec4<f32>(c.rgb * a + src.rgb * (1.0 - a), a + src.a * (1.0 - a));
}
