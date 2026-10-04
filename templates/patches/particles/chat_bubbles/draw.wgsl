// Chat bubbles: one 16x14 2-bit speech bubble per particle on its own pixel grid (whole screen
// pixels per bubble pixel). Levels: 1 dark outline, 2 the chatter's colour (shaded on the inner
// right/bottom edge), 3 highlight. The tail is mirrored toward the bubble's screen edge; the
// content is a 7x7 emote (1 ink, 2 black, 3 accent, auto-outlined) or three typing dots. Wobble is
// a per-row horizontal jelly shear; pops step through integer pixel sizes; fades are ordered dither.
const RESERVED: u32 = 2u;
const W: f32 = 16.0;
const H: f32 = 14.0;
const PAD: f32 = 2.0;

const BUBBLE = array<u32, 14>(
    0x05555550u, 0x1ffffff4u, 0x6aaaaabdu, 0x6aaaaaadu, 0x6aaaaaadu, 0x6aaaaaa9u, 0x6aaaaaa9u, 0x6aaaaaa9u,
    0x6aaaaaa9u, 0x1aaaaaa4u, 0x05555690u, 0x00000690u, 0x00000190u, 0x00000050u,
);
// heart, smiley, star, note, exclamation, laughing face: 7 rows each, 2 bits per pixel
const GLYPHS = array<u32, 42>(
    0x00000f3cu, 0x00003ff7u, 0x00003fffu, 0x00003fffu, 0x00000ffcu, 0x000003f0u, 0x000000c0u, 0x000003f0u,
    0x00000ffcu, 0x00003eefu, 0x00003fffu, 0x00003bfbu, 0x00000eacu, 0x000003f0u, 0x000000c0u, 0x000003f0u,
    0x00003fffu, 0x00000ffcu, 0x000003f0u, 0x00000f3cu, 0x00000c0cu, 0x00000140u, 0x00000540u, 0x00001440u,
    0x00001040u, 0x00000054u, 0x00000055u, 0x00000014u, 0x00000150u, 0x00000150u, 0x00000150u, 0x00000040u,
    0x00000040u, 0x00000000u, 0x00000150u, 0x000003f0u, 0x00000ffcu, 0x00003bfbu, 0x00003fffu, 0x00003aabu,
    0x00000e6cu, 0x000003f0u,
);
const ACCENT = array<vec3<f32>, 6>(
    vec3<f32>(1.0, 0.18, 0.3), vec3<f32>(1.0, 0.86, 0.2), vec3<f32>(1.0, 0.86, 0.2),
    vec3<f32>(1.0, 1.0, 1.0), vec3<f32>(1.0, 1.0, 1.0), vec3<f32>(1.0, 0.86, 0.2),
);
const BAYER = array<f32, 16>(0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0);

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) color: vec4<f32>,
    // content, side, dot phase, unused
    @location(2) @interpolate(flat) info: vec4<u32>,
    // wobble amplitude, wobble phase, alpha, unused
    @location(3) @interpolate(flat) anim: vec4<f32>,
};

// pop-in overshoot, stepped like an old sprite engine
fn pop(age: f32) -> f32 {
    if age < 0.05 {
        return 0.4;
    }
    if age < 0.1 {
        return 1.3;
    }
    if age < 0.15 {
        return 0.9;
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
    if i < RESERVED || p.life <= 0.0 {
        o.pos = vec4<f32>(2.0, 2.0, 0.0, 1.0);
        return o;
    }
    let wob = p_wobble();
    let kick = s_band_kick();
    let px = max(1.0, round(p.size * se.resolution.y * pop(p.age)));
    // gentle sideways drift plus a hop on every kick
    let drift = sin(p.age * (1.6 + p.seed) + p.seed * 40.0) * 0.006;
    let hop = round(kick * kick * 2.0 * wob);
    let centre = p.pos + vec2<f32>(drift * se.resolution.y / se.resolution.x, 0.0);
    let anchor = floor(centre * se.resolution / px) - vec2<f32>(0.0, hop);
    let cells = vec2<f32>(W + 2.0 * PAD, H);
    let origin = (anchor - vec2<f32>(W * 0.5 + PAD, H * 0.5)) * px;
    let xy = origin + c * cells * px;
    o.pos = vec4<f32>(xy.x / se.resolution.x * 2.0 - 1.0, 1.0 - xy.y / se.resolution.y * 2.0, 0.0, 1.0);
    o.local = c * cells - vec2<f32>(PAD, 0.0);
    let code = u32(p.color.a);
    o.color = vec4<f32>(p.color.rgb, 1.0);
    o.info = vec4<u32>(code & 7u, code >> 3u, u32(p.age * 3.5 + p.seed * 4.0) & 3u, 0u);
    // jelly: about a pixel of sway right after the pop, re-kicked by the kick drum, still at rest
    let amp = (1.3 * exp(-p.age * 3.0) + 0.75 * kick * kick) * wob;
    o.anim = vec4<f32>(amp, p.age * 10.0 + p.seed * 6.0, clamp(p.life / 0.6, 0.0, 1.0), 0.0);
    return o;
}

fn glyph_level(g: u32, x: i32, y: i32) -> u32 {
    if x < 0 || x > 6 || y < 0 || y > 6 {
        return 0u;
    }
    return (GLYPHS[g * 7u + u32(y)] >> (u32(x) * 2u)) & 3u;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let row = i32(floor(in.local.y));
    let shear = i32(round(in.anim.x * sin(in.anim.y + f32(row) * 0.45)));
    let x = i32(floor(in.local.x)) - shear;
    if x < 0 || x > 15 || row < 0 || row > 13 {
        discard;
    }
    let d = (BAYER[(u32(x) & 3u) + (u32(row) & 3u) * 4u] + 0.5) / 16.0;
    if in.anim.z <= d {
        discard;
    }
    // the body is lit from the top left on both sides; only the tail flips toward the edge
    let bx = select(x, 15 - x, in.info.y == 1u && row >= 10);
    let lv = (BUBBLE[u32(row)] >> (u32(bx) * 2u)) & 3u;
    if lv == 0u {
        discard;
    }
    let rgb = in.color.rgb;
    let dark = rgb * 0.22;
    if lv == 1u {
        return vec4<f32>(dark, 1.0);
    }
    if lv == 3u {
        return vec4<f32>(mix(rgb, vec3<f32>(1.0), 0.55), 1.0);
    }
    var fill = rgb;
    if x == 14 || row == 9 {
        fill = rgb * 0.72;
    }
    // ink contrasts with the bubble colour
    let light = dot(rgb, vec3<f32>(0.299, 0.587, 0.114)) > 0.62;
    let ink = select(vec3<f32>(1.0), dark, light);
    let content = in.info.x;
    if content == 0u {
        // typing dots: 2x2 each, the active one jumps a pixel
        let k = (x - 3) / 4;
        let dx = (x - 3) % 4;
        if x >= 3 && k < 3 && dx < 2 {
            let lift = select(0, 1, u32(k) == in.info.z);
            let dy = row - (5 - lift);
            if dy >= 0 && dy < 2 {
                return vec4<f32>(ink, 1.0);
            }
        }
        return vec4<f32>(fill, 1.0);
    }
    let g = content - 1u;
    let gx = x - 4;
    let gy = row - 2;
    let gl = glyph_level(g, gx, gy);
    // note and "!" are drawn in ink; the others are coloured stickers with a dark outline
    let sticker = g != 3u && g != 4u;
    if gl == 1u {
        return vec4<f32>(select(ink, vec3<f32>(1.0), sticker), 1.0);
    }
    if gl == 2u {
        return vec4<f32>(vec3<f32>(0.06), 1.0);
    }
    if gl == 3u {
        // an accent too close to the bubble colour turns white
        let acc = ACCENT[g];
        let e = acc - rgb;
        return vec4<f32>(select(acc, vec3<f32>(1.0), dot(e, e) < 0.12), 1.0);
    }
    if sticker && glyph_level(g, gx - 1, gy) + glyph_level(g, gx + 1, gy) + glyph_level(g, gx, gy - 1) + glyph_level(g, gx, gy + 1) > 0u {
        return vec4<f32>(dark, 1.0);
    }
    return vec4<f32>(fill, 1.0);
}
