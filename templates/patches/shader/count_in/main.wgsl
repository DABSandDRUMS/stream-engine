// Count-in: giant 5x7 pixel numbers built from chunky beveled blocks (black outline, hard drop
// shadow). After the trigger, "1" pops on the next beat (a trigger up to a fifth of a beat late
// still counts that beat), then 2, 3, 4 on the following beats and "GO!" on the beat after. Each
// pops in stretched tall, lands squashed wide and settles with a little overshoot; the previous
// count swells and fades out underneath. Four beat pips below fill up as it counts.
// The beat grid is reconstructed from beat.phase (or lfo.bar for `align = bar`): the most recent
// onset was `phase * period` ago, so onsets sit on that grid in trigger-age time (assumes a
// stable tempo; 120 BPM without one).

// Bitmap font, only the glyphs used: 'GO!1234'. 8 rows per glyph, bit 4 = leftmost column.
const FONT = array<u32, 56>(
    14u, 17u, 16u, 23u, 17u, 17u, 15u, 0u, // 'G'
    14u, 17u, 17u, 17u, 17u, 17u, 14u, 0u, // 'O'
    16u, 16u, 16u, 16u, 16u, 0u, 16u, 0u, // '!'
    4u, 12u, 4u, 4u, 4u, 4u, 14u, 0u, // '1'
    14u, 17u, 1u, 2u, 4u, 8u, 31u, 0u, // '2'
    14u, 17u, 1u, 6u, 1u, 17u, 14u, 0u, // '3'
    2u, 6u, 10u, 18u, 31u, 2u, 2u, 0u, // '4'
);
const GLYPH_W = array<u32, 7>(5u, 5u, 1u, 5u, 5u, 5u, 5u);
const DIGIT1 = 3u;
// "GO!" glyph x offsets in blocks (1-block gaps): G at 0, O at 6, ! at 12; 13 blocks wide.
const GO_W = 13.0;

// VGA bright colours: 1 cyan, 2 magenta, 3 yellow, 4 red, GO! green.
const COLORS = array<vec3<f32>, 5>(
    vec3<f32>(0.333, 1.0, 1.0),
    vec3<f32>(1.0, 0.333, 1.0),
    vec3<f32>(1.0, 1.0, 0.333),
    vec3<f32>(1.0, 0.333, 0.333),
    vec3<f32>(0.333, 1.0, 0.333),
);

const FADE = 0.14;   // previous count fade-out (s)
const OUTLINE = 0.2; // outline thickness (blocks)

fn glyph_on(g: u32, x: i32, y: i32) -> bool {
    if (x < 0 || y < 0 || y > 6 || x >= i32(GLYPH_W[g])) {
        return false;
    }
    return ((FONT[g * 8u + u32(y)] >> u32(4 - x)) & 1u) == 1u;
}

// Block at integer block coords of element `e` (0-3 digits, 4 = "GO!").
fn block_on(e: i32, b: vec2<i32>) -> bool {
    if (e < 4) {
        return glyph_on(DIGIT1 + u32(e), b.x, b.y);
    }
    if (b.x < 6) {
        return glyph_on(0u, b.x, b.y);
    }
    if (b.x < 12) {
        return glyph_on(1u, b.x - 6, b.y);
    }
    return glyph_on(2u, b.x - 12, b.y);
}

fn on_at(e: i32, q: vec2<f32>) -> bool {
    return block_on(e, vec2<i32>(floor(q)));
}

// One count element at glyph coords q (blocks, origin top-left); premultiplied rgba.
fn element(e: i32, q: vec2<f32>, color: vec3<f32>) -> vec4<f32> {
    let width = select(5.0, GO_W, e == 4);
    if (q.x < -1.0 || q.y < -1.0 || q.x > width + 1.0 || q.y > 8.0) {
        return vec4<f32>(0.0);
    }
    let b = vec2<i32>(floor(q));
    if (block_on(e, b)) {
        // bevel only where the neighbour block is empty: light top/left, dark bottom/right
        let f = fract(q);
        let bev = 0.17;
        var c = color;
        if (f.y < bev && !block_on(e, b - vec2<i32>(0, 1))) || (f.x < bev && !block_on(e, b - vec2<i32>(1, 0))) {
            c = mix(color, vec3<f32>(1.0), 0.6);
        } else if (f.y > 1.0 - bev && !block_on(e, b + vec2<i32>(0, 1))) || (f.x > 1.0 - bev && !block_on(e, b + vec2<i32>(1, 0))) {
            c = color * 0.55;
        } else if (b.y < 3) {
            c = mix(color, vec3<f32>(1.0), 0.18);
        }
        return vec4<f32>(c, 1.0);
    }
    // black outline around the shape
    let o = OUTLINE;
    if (on_at(e, q + vec2<f32>(o, 0.0)) || on_at(e, q - vec2<f32>(o, 0.0)) || on_at(e, q + vec2<f32>(0.0, o)) || on_at(e, q - vec2<f32>(0.0, o))
        || on_at(e, q + vec2<f32>(o, o)) || on_at(e, q - vec2<f32>(o, o)) || on_at(e, q + vec2<f32>(o, -o)) || on_at(e, q + vec2<f32>(-o, o))) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    // hard drop shadow down-right
    if (on_at(e, q - vec2<f32>(0.55, 0.55))) {
        return vec4<f32>(0.0, 0.0, 0.0, 0.5);
    }
    return vec4<f32>(0.0);
}

fn over(top: vec4<f32>, under: vec4<f32>) -> vec4<f32> {
    return top + under * (1.0 - top.a);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    if (se.env <= 0.0) {
        return vec4<f32>(0.0);
    }
    let age = max(se.trigger_age, 0.0);
    let bpm = s_beat_bpm();
    let beat_t = 60.0 / select(120.0, clamp(bpm, 30.0, 300.0), bpm > 1.0);
    let bar = p_align() == 1;
    let period = select(beat_t, beat_t * 4.0, bar);
    let ph = select(fract(s_beat_phase()), fract(s_lfo_bar()), bar && bpm > 1.0);
    let last = age - select(fract(se.time / beat_t), ph, bpm > 1.0) * period;
    let tol = 0.2 * beat_t;
    let o0 = last - floor((last + tol) / period) * period;  // age of the "1"

    let go = p_go();
    let last_e = select(3, 4, go);
    let n = i32(floor((age - o0) / beat_t));
    if (n < 0) {
        return vec4<f32>(0.0);
    }
    let hi = min(n, last_e);
    let lo = max(hi - 1, 0);

    let B = max(2.0, floor(p_size() * se.resolution.y / 7.0));
    let anchor = p_position() * se.resolution + vec2<f32>(0.0, 3.5 * B); // bottom center
    let frag = in.uv * se.resolution;
    var out = vec4<f32>(0.0);
    for (var e = lo; e <= hi; e++) {
        let t = age - o0 - f32(e) * beat_t;
        let life = select(beat_t, beat_t * 1.5, e == last_e);
        if (t < 0.0 || t > life + FADE) {
            continue;
        }
        let fade = clamp((t - life) / FADE, 0.0, 1.0);
        // pop: from half size with an overshoot, stretched tall first then squashed wide
        let grow = 1.0 - 0.5 * exp(-8.0 * t) * cos(16.0 * t);
        let squash = -0.3 * exp(-8.0 * t) * sin(17.0 * t);
        let scale = grow * (1.0 + 0.35 * fade);
        let sc = vec2<f32>(scale * (1.0 - squash), scale * (1.0 + squash)) * B;
        let width = select(5.0, GO_W, e == 4);
        let rise = 0.8 * B * fade;
        let q = vec2<f32>((frag.x - anchor.x) / sc.x + width * 0.5, (frag.y - anchor.y + rise) / sc.y + 7.0);
        let color = select(p_color().rgb, COLORS[e], p_rainbow());
        // the outgoing count drops to a ghost the moment the next one lands
        let alpha = select(1.0, 0.55 * (1.0 - fade), t > life);
        let c = element(e, q, color) * alpha;
        out = over(c, out);
    }

    // beat pips: four squares under the numbers, lit up to the current count
    let pip = max(2.0, floor(B * 0.42));
    let gap = pip * 0.8;
    let row_w = 4.0 * pip + 3.0 * gap;
    let pp = frag - vec2<f32>(anchor.x - row_w * 0.5, anchor.y + B * 0.9);
    let k = i32(floor(pp.x / (pip + gap)));
    let lx = pp.x - f32(k) * (pip + gap);
    let end = beat_t * (f32(last_e) + 1.5) + FADE;
    if (pp.y >= -2.0 && pp.y < pip + 2.0 && k >= 0 && k < 4 && lx >= -2.0 && lx < pip + 2.0 && age - o0 < end) {
        let fade = clamp((age - o0 - end + FADE) / FADE, 0.0, 1.0);
        var c = vec4<f32>(0.0, 0.0, 0.0, 1.0);
        if (pp.y >= 0.0 && pp.y < pip && lx >= 0.0 && lx < pip) {
            let lit = k <= n;
            let pc = select(COLORS[min(k, 3)], COLORS[4], n >= 4);
            c = vec4<f32>(select(vec3<f32>(0.2), select(p_color().rgb, pc, p_rainbow()), lit), 1.0);
        }
        out = over(out, c * (1.0 - fade));
    }
    return out;
}
