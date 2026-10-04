// Chat rain: chunky 2-bit sprites snapped to their own pixel grid (each sprite pixel covers whole
// screen pixels). Levels: 1 dark outline, 2 the chatter's colour, 3 highlight. Message drops carry a
// dithered speed trail; splashes are short frame animations; the drizzle is thin translucent
// streaks one sprite pixel wide.
const RESERVED: u32 = 2u;
const TRAIL: u32 = 8u;

// drop, gem, diamond, heart: 7 x 11, bottom aligned; one row per u32, 2 bits per pixel (x = 0 low)
const SHAPES = array<u32, 44>(
    0x00000040u, 0x00000040u, 0x00000190u, 0x000001d0u, 0x000006e4u, 0x000006b4u, 0x00001aadu, 0x00001aadu,
    0x00001aa9u, 0x000006a4u, 0x00000150u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000554u,
    0x00001abdu, 0x00001aadu, 0x00001aa9u, 0x00001aa9u, 0x00001aa9u, 0x00000554u, 0x00000000u, 0x00000000u,
    0x00000000u, 0x00000040u, 0x000001d0u, 0x000006f4u, 0x00001abdu, 0x00001aadu, 0x000006a4u, 0x00000190u,
    0x00000040u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000514u, 0x00001a7du, 0x00001aadu,
    0x00001aa9u, 0x000006a4u, 0x00000190u, 0x00000040u,
);
// first non-empty row of each shape (the trail starts above it)
const TOPS = array<u32, 4>(0u, 4u, 3u, 4u);
// message splash: 11 x 5, 4 frames
const CROWN = array<u32, 20>(
    0x00000000u, 0x00000000u, 0x00000000u, 0x00006a40u, 0x0001aa90u, 0x00000000u, 0x00030030u, 0x00020c20u,
    0x00060024u, 0x0006aaa4u, 0x000c000cu, 0x00080008u, 0x00200002u, 0x00200002u, 0x0028000au, 0x00300003u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x00200002u,
);
// drizzle splash: 5 x 2, 2 frames
const MINI = array<u32, 4>(0x00000000u, 0x00000222u, 0x00000202u, 0x00000000u);
const BAYER = array<f32, 16>(0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0);

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) color: vec4<f32>,
    // kind, shape or frame, streak length, unused
    @location(2) @interpolate(flat) info: vec4<u32>,
    @location(3) @interpolate(flat) trail: f32,
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
    if i < RESERVED || p.life <= 0.0 || p.age < 0.0 {
        o.pos = vec4<f32>(2.0, 2.0, 0.0, 1.0);
        return o;
    }
    let code = u32(p.color.a);
    let kind = code & 7u;
    var cells = vec2<f32>(1.0);
    var centre = 0.0;
    var aux = code >> 3u;
    var len = 0u;
    if kind == 0u {
        cells = vec2<f32>(7.0, 11.0 + f32(TRAIL));
        centre = 3.0;
        // the trail grows as the drop speeds up
        o.trail = clamp(p.vel.y / (0.9 * p_speed()), 0.25, 1.0);
    } else if kind == 1u {
        cells = vec2<f32>(11.0, 5.0);
        centre = 5.0;
        aux = min(u32(p.age / 0.08), 3u);
    } else if kind == 3u {
        len = 4u + u32(p.seed * 3.99);
        cells = vec2<f32>(1.0, f32(len));
    } else if kind == 4u {
        cells = vec2<f32>(5.0, 2.0);
        centre = 2.0;
        aux = min(u32(p.age / 0.08), 1u);
    }
    // whole screen pixels per sprite pixel; the bottom-centre cell sits on the particle position
    let px = max(1.0, round(p.size * se.resolution.y));
    let anchor = floor(p.pos * se.resolution / px);
    let origin = (anchor - vec2<f32>(centre, cells.y - 1.0)) * px;
    let xy = origin + c * cells * px;
    o.pos = vec4<f32>(xy.x / se.resolution.x * 2.0 - 1.0, 1.0 - xy.y / se.resolution.y * 2.0, 0.0, 1.0);
    o.local = c * cells;
    o.color = vec4<f32>(p.color.rgb, 1.0);
    o.info = vec4<u32>(kind, aux, len, 0u);
    return o;
}

fn level(row: u32, x: u32) -> u32 {
    return (row >> (x * 2u)) & 3u;
}

fn shade(rgb: vec3<f32>, lv: u32) -> vec3<f32> {
    if lv == 1u {
        return rgb * 0.28;
    }
    if lv == 3u {
        return mix(rgb, vec3<f32>(1.0), 0.65);
    }
    return rgb;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let cell = vec2<u32>(max(floor(in.local), vec2<f32>(0.0)));
    let kind = in.info.x;
    let rgb = in.color.rgb;
    let d = (BAYER[(cell.x & 3u) + (cell.y & 3u) * 4u] + 0.5) / 16.0;
    if kind == 0u {
        let shape = min(in.info.y, 3u);
        if cell.y >= TRAIL {
            let lv = level(SHAPES[shape * 11u + min(cell.y - TRAIL, 10u)], min(cell.x, 6u));
            if lv == 0u {
                discard;
            }
            return vec4<f32>(shade(rgb, lv), 1.0);
        }
        // speed trail: dithered lines above the sprite (one behind a pointed top, two behind a flat one)
        let dist = f32(TRAIL + TOPS[shape]) - f32(cell.y);
        let flat_top = shape == 1u || shape == 3u;
        let on = select(cell.x == 3u, cell.x == 1u || cell.x == 5u, flat_top);
        if dist > f32(TRAIL) || !on {
            discard;
        }
        let a = (1.0 - dist / f32(TRAIL + 1u)) * in.trail;
        if a <= d {
            discard;
        }
        return vec4<f32>(mix(rgb, vec3<f32>(1.0), 0.3), 1.0);
    }
    if kind == 1u {
        let lv = level(CROWN[min(in.info.y, 3u) * 5u + min(cell.y, 4u)], min(cell.x, 10u));
        if lv == 0u {
            discard;
        }
        return vec4<f32>(shade(rgb, lv), 1.0);
    }
    if kind == 2u {
        return vec4<f32>(mix(rgb, vec3<f32>(1.0), 0.25), 1.0);
    }
    if kind == 3u {
        // translucent streak, brightest at the leading (bottom) pixel
        let k = f32(in.info.z - 1u - min(cell.y, in.info.z - 1u));
        let a = 0.75 * (1.0 - k / f32(in.info.z + 1u));
        return vec4<f32>(rgb * a, a);
    }
    let lv = level(MINI[min(in.info.y, 1u) * 2u + min(cell.y, 1u)], min(cell.x, 4u));
    if lv == 0u {
        discard;
    }
    return vec4<f32>(rgb * 0.6, 0.6);
}
