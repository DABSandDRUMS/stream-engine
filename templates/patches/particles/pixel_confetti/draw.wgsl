// Pixel confetti: one 10x10 2-bit sprite per particle, snapped to its own pixel grid so every
// sprite pixel lands on whole screen pixels. Levels: 0 clear, 1 dark VGA shade, 2 bright VGA
// colour, 3 white. Pieces flip like paper (horizontal squash in quarter steps, dark back side),
// chips/stars/bolts also turn in 90-degree steps, and fades are ordered 4x4 dither, not blends.
const RESERVED: u32 = 2u;
const CELLS: f32 = 10.0;

// chip, star, note, smiley, heart, bolt; one row per u32, 2 bits per pixel (x = 0 in the low bits)
const SPR = array<u32, 60>(
    0x00000000u, 0x00000000u, 0x00005550u, 0x0001aaf4u, 0x0001aab4u, 0x0001aaa4u, 0x00016aa4u, 0x00005550u, 0x00000000u, 0x00000000u,
    0x00000100u, 0x00000640u, 0x00000640u, 0x00005b94u, 0x0001aaa9u, 0x00006ba4u, 0x00001a90u, 0x000069a4u, 0x00006464u, 0x00001010u,
    0x00005540u, 0x0001aa90u, 0x0001ab90u, 0x00019590u, 0x00019190u, 0x00019190u, 0x0001a5a4u, 0x0001a9a9u, 0x00006464u, 0x00001010u,
    0x00001540u, 0x00006a90u, 0x0001abe4u, 0x00069a69u, 0x0006aab9u, 0x00066a99u, 0x00069569u, 0x0001aaa4u, 0x00006a90u, 0x00001540u,
    0x00001450u, 0x000069a4u, 0x0001aab9u, 0x0001aab9u, 0x0001aaa9u, 0x00006aa4u, 0x00001a90u, 0x00000640u, 0x00000100u, 0x00000000u,
    0x00005400u, 0x0001a900u, 0x00006a40u, 0x00005a90u, 0x0001aaa4u, 0x00006a50u, 0x00001a90u, 0x00000690u, 0x00000190u, 0x00000040u,
);

const VGA = array<vec3<f32>, 16>(
    vec3<f32>(0.0, 0.0, 0.0), vec3<f32>(0.0, 0.0, 0.667), vec3<f32>(0.0, 0.667, 0.0), vec3<f32>(0.0, 0.667, 0.667),
    vec3<f32>(0.667, 0.0, 0.0), vec3<f32>(0.667, 0.0, 0.667), vec3<f32>(0.667, 0.333, 0.0), vec3<f32>(0.667, 0.667, 0.667),
    vec3<f32>(0.333, 0.333, 0.333), vec3<f32>(0.333, 0.333, 1.0), vec3<f32>(0.333, 1.0, 0.333), vec3<f32>(0.333, 1.0, 1.0),
    vec3<f32>(1.0, 0.333, 0.333), vec3<f32>(1.0, 0.333, 1.0), vec3<f32>(1.0, 1.0, 0.333), vec3<f32>(1.0, 1.0, 1.0),
);
const BAYER = array<f32, 16>(0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0);

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) sprite: u32,
    @location(2) @interpolate(flat) rot: u32,
    @location(3) @interpolate(flat) squash: f32,
    @location(4) @interpolate(flat) back: u32,
    @location(5) @interpolate(flat) vga: u32,
    @location(6) @interpolate(flat) alpha: f32,
};

@vertex
fn vs(@builtin(vertex_index) v: u32, @builtin(instance_index) i: u32) -> VOut {
    let p = se_particles[i];
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let c = corners[v % 6u];
    var o: VOut;
    o.local = c * CELLS;
    if i < RESERVED || p.life <= 0.0 || p.age < 0.0 {
        o.pos = vec4<f32>(2.0, 2.0, 0.0, 1.0);
        return o;
    }
    // whole screen pixels per sprite pixel; snap the sprite origin to that grid
    let px = max(1.0, round(p.size * se.resolution.y));
    let origin = floor(p.pos * se.resolution / px) * px - vec2<f32>(5.0 * px);
    let xy = origin + c * CELLS * px;
    o.pos = vec4<f32>(xy.x / se.resolution.x * 2.0 - 1.0, 1.0 - xy.y / se.resolution.y * 2.0, 0.0, 1.0);

    // resting pieces freeze their flip/turn at the moment they landed (stored in vel.x)
    let t = select(p.age, p.vel.x, p.color.w >= 2.0);
    let fp = t * (3.0 + 4.0 * p.seed) * p_flutter() + p.seed * 20.0;
    let cf = cos(fp);
    o.squash = max(0.25, round(abs(cf) * 4.0) / 4.0);
    o.back = select(0u, 1u, cf < 0.0);
    let sprite = u32(p.color.y);
    o.sprite = sprite;
    // chips, stars and bolts tumble in quarter turns; the faces only flip
    let turns = u32(floor(abs(p.color.z * t + p.seed * 6.0) / 1.5708)) & 3u;
    o.rot = select(0u, turns, sprite == 0u || sprite == 1u || sprite == 5u);
    o.vga = u32(p.color.x);
    let env_fade = clamp(se.env * 2.5, 0.0, 1.0);
    o.alpha = clamp(p.life / 0.6, 0.0, 1.0) * env_fade;
    return o;
}

fn dark_of(b: u32) -> u32 {
    if b == 15u {
        return 7u;
    }
    return b - 8u;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let cell = floor(in.local);
    // ordered dither on the sprite-pixel grid: pixels drop out whole
    let d = (BAYER[(u32(cell.x) & 3u) + (u32(cell.y) & 3u) * 4u] + 0.5) / 16.0;
    if in.alpha <= d {
        discard;
    }
    // paper flip: squash around the centre column
    let lx = (in.local.x - 5.0) / in.squash + 5.0;
    if lx < 0.0 || lx >= CELLS {
        discard;
    }
    var x = u32(lx);
    var y = u32(cell.y);
    if in.back == 1u {
        x = 9u - x;
    }
    var q = vec2<u32>(x, y);
    if in.rot == 1u {
        q = vec2<u32>(y, 9u - x);
    } else if in.rot == 2u {
        q = vec2<u32>(9u - x, 9u - y);
    } else if in.rot == 3u {
        q = vec2<u32>(9u - y, x);
    }
    let level = (SPR[in.sprite * 10u + q.y] >> (q.x * 2u)) & 3u;
    if level == 0u {
        discard;
    }
    let bright = in.vga;
    let dark = dark_of(bright);
    var idx = bright;
    if level == 1u {
        idx = select(dark, 8u, bright == 15u);
    } else if level == 3u {
        idx = 15u;
    }
    if in.back == 1u {
        idx = select(dark, 0u, level == 1u);
    }
    return vec4<f32>(VGA[idx], 1.0);
}
