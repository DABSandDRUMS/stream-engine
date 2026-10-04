// Emoji percussion: one 16x16 4-bit sprite per particle. Nibbles index the 16-color VGA
// palette (0 = clear, 1 = black outline). Sprite pixels are whole screen pixels, so scale-ins step
// in integer pixel sizes, and fades are ordered dither rather than alpha blends.
const KICK0: u32 = 2u;
const CELLS: f32 = 16.0;

// burst (small), burst, star, sparkle, sparkle (small); 2 u32 per row, 4 bits per pixel
const SPR = array<u32, 160>(
    0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x11000000u, 0x00000011u, 0xc1111000u, 0x0001111cu,
    0xcc1c1000u, 0x0001c1ccu, 0xecc11000u, 0x00011cceu, 0xfecc1100u, 0x0011ccefu, 0xffecc100u, 0x001cceffu,
    0xffecc100u, 0x001cceffu, 0xfecc1100u, 0x0011ccefu, 0xecc11000u, 0x00011cceu, 0xcc1c1000u, 0x0001c1ccu,
    0xc1111000u, 0x0001111cu, 0x11000000u, 0x00000011u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u,
    0x11000000u, 0x00000011u, 0xc1001110u, 0x0111001cu, 0xc1111c10u, 0x01c1111cu, 0xcc11cc10u, 0x01cc11ccu,
    0xecccc110u, 0x011cccceu, 0xeeecc110u, 0x011cceeeu, 0xfeeecc11u, 0x11cceeefu, 0xffeeccc1u, 0x1ccceeffu,
    0xffeeccc1u, 0x1ccceeffu, 0xfeeecc11u, 0x11cceeefu, 0xeeecc110u, 0x011cceeeu, 0xecccc110u, 0x011cccceu,
    0xcc11cc10u, 0x01cc11ccu, 0xc1111c10u, 0x01c1111cu, 0xc1001110u, 0x0111001cu, 0x11000000u, 0x00000011u,
    0x11000000u, 0x00000001u, 0xe1100000u, 0x00000011u, 0xee100000u, 0x00000016u, 0xfe110000u, 0x00000116u,
    0xefe11111u, 0x0111116eu, 0xeefeeee1u, 0x016eeeeeu, 0xeeeffe11u, 0x0116eeeeu, 0xeeeee110u, 0x00116eeeu,
    0xeeee1100u, 0x000116eeu, 0x6eee1100u, 0x000116eeu, 0x16eee100u, 0x00016ee6u, 0x11eee110u, 0x00116e61u,
    0x011eee10u, 0x0016e611u, 0x0011ee10u, 0x00166110u, 0x00011110u, 0x00111100u, 0x00000000u, 0x00000000u,
    0x00000000u, 0x00000000u, 0x11000000u, 0x00000001u, 0xe1000000u, 0x00000001u, 0xe1100000u, 0x00000011u,
    0xee100000u, 0x0000001eu, 0xfe110000u, 0x0000011eu, 0xfee11111u, 0x011111eeu, 0xfffeeee1u, 0x01eeeeffu,
    0xfee11111u, 0x011111eeu, 0xfe110000u, 0x0000011eu, 0xee100000u, 0x0000001eu, 0xe1100000u, 0x00000011u,
    0xe1000000u, 0x00000001u, 0x11000000u, 0x00000001u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x11000000u, 0x00000001u,
    0xe1100000u, 0x00000011u, 0xee110000u, 0x0000011eu, 0xfee11000u, 0x000011eeu, 0xffee1000u, 0x00001eefu,
    0xfee11000u, 0x000011eeu, 0xee110000u, 0x0000011eu, 0xe1100000u, 0x00000011u, 0x11000000u, 0x00000001u,
    0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u, 0x00000000u,
);

const VGA = array<vec3<f32>, 16>(
    vec3<f32>(0.0, 0.0, 0.0), vec3<f32>(0.0, 0.0, 0.0), vec3<f32>(0.0, 0.667, 0.0), vec3<f32>(0.0, 0.667, 0.667),
    vec3<f32>(0.667, 0.0, 0.0), vec3<f32>(0.667, 0.0, 0.667), vec3<f32>(0.667, 0.333, 0.0), vec3<f32>(0.667, 0.667, 0.667),
    vec3<f32>(0.333, 0.333, 0.333), vec3<f32>(0.333, 0.333, 1.0), vec3<f32>(0.333, 1.0, 0.333), vec3<f32>(0.333, 1.0, 1.0),
    vec3<f32>(1.0, 0.333, 0.333), vec3<f32>(1.0, 0.333, 1.0), vec3<f32>(1.0, 1.0, 0.333), vec3<f32>(1.0, 1.0, 1.0),
);
const BAYER = array<f32, 16>(0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0);

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) sprite: u32,
    @location(2) @interpolate(flat) tint: u32,
    @location(3) @interpolate(flat) alpha: f32,
};

// pop-in overshoot, stepped like an old sprite engine
fn pop(age: f32) -> f32 {
    if age < 0.04 {
        return 0.45;
    }
    if age < 0.08 {
        return 1.3;
    }
    if age < 0.12 {
        return 1.12;
    }
    return 1.0;
}

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
    if i < KICK0 || p.life <= 0.0 {
        o.pos = vec4<f32>(2.0, 2.0, 0.0, 1.0);
        return o;
    }
    let kind = u32(p.color.x);
    var sprite = 2u;
    var s = pop(p.age);
    if kind == 0u {
        // the burst swaps from the compact frame to the full one as it pops
        sprite = select(1u, 0u, p.age < 0.07);
        s = select(1.0, 1.2, p.age >= 0.07 && p.age < 0.14);
    } else if kind == 2u {
        // sparkles twinkle between a big and a small frame
        let tw = u32((p.age + p.color.z * 0.2) / 0.09) & 1u;
        sprite = select(3u + tw, 4u, p.age < 0.05);
        s = 1.0;
    }
    o.sprite = sprite;
    o.tint = u32(p.color.y);
    let px = max(1.0, round(p.size * se.resolution.y * p.color.w * s));
    let origin = floor(p.pos * se.resolution / px) * px - vec2<f32>(8.0 * px);
    let xy = origin + c * CELLS * px;
    o.pos = vec4<f32>(xy.x / se.resolution.x * 2.0 - 1.0, 1.0 - xy.y / se.resolution.y * 2.0, 0.0, 1.0);
    o.alpha = clamp(p.life / 0.25, 0.0, 1.0);
    return o;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let cell = vec2<u32>(min(floor(in.local), vec2<f32>(15.0)));
    let d = (BAYER[(cell.x & 3u) + (cell.y & 3u) * 4u] + 0.5) / 16.0;
    if in.alpha <= d {
        discard;
    }
    let word = SPR[in.sprite * 32u + cell.y * 2u + (cell.x >> 3u)];
    var idx = (word >> ((cell.x & 7u) * 4u)) & 15u;
    if idx == 0u {
        discard;
    }
    // cyan sparkles: yellow body becomes light cyan
    if in.tint == 1u && idx == 14u {
        idx = 11u;
    }
    return vec4<f32>(VGA[idx], 1.0);
}
