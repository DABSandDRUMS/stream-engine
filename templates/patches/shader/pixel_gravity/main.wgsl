// Pixel gravity: the picture cracks into square blocks; a share of them hop and fall away
// under gravity with staggered delays (fading as they drop), revealing a dark blurred copy of
// the live picture; on release they fly back into their slots with a small bounce.
// Blocks only move vertically, so each pixel inverse-maps by checking the few blocks of its
// own column that could cover it. Input and output are premultiplied alpha.

const PG_REACH: i32 = 7;          // farthest a block falls (in blocks) before it has faded out

fn pg_hash(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn pg_tap(uv: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(se_input, se_sampler, uv, 0.0);
}

struct PgBlock {
    d: f32,       // vertical offset in px (down positive)
    vis: f32,     // opacity
    loose: f32,   // 1 = detached block, 0 = stays in place
};

// State of the block at (col, row) for this trigger.
fn pg_block(cell: vec2<f32>, bs: f32, frac: f32, age: f32, ret: f32, g: f32, seed: f32) -> PgBlock {
    var b: PgBlock;
    b.d = 0.0;
    b.vis = 1.0;
    b.loose = 0.0;
    if (pg_hash(cell + vec2<f32>(seed * 13.7, 17.0)) >= frac) {
        return b;
    }
    b.loose = 1.0;
    let h2 = pg_hash(cell * 1.37 + vec2<f32>(3.1, seed));
    let h3 = pg_hash(cell * 0.71 + vec2<f32>(seed, 9.4));
    let reach = f32(PG_REACH) * bs;
    // Fall: wait for this block's delay (trembling), hop up a little, then drop.
    let tf = age - h2 * 0.6;
    var df = 0.0;
    if (tf < 0.0) {
        df = -abs(sin(age * 47.0 + h3 * 20.0)) * bs * 0.025 * smoothstep(-0.25, 0.0, tf);
    } else {
        let v0 = sqrt(2.0 * g * 0.18 * bs);
        df = min(-v0 * tf + 0.5 * g * tf * tf, reach);
    }
    let vis_fall = 1.0 - smoothstep(0.4 * reach, reach, df);
    // Return: ease back home, then a small bounce above the slot.
    let p = clamp((ret - h3 * 0.3) / 0.65, 0.0, 1.0);
    if (p <= 0.0) {
        b.d = df;
        b.vis = vis_fall;
        return b;
    }
    if (p < 0.75) {
        let k = 1.0 - p / 0.75;
        b.d = df * k * k * k;
    } else {
        b.d = -0.12 * bs * sin(3.14159 * (p - 0.75) / 0.25);
    }
    b.vis = mix(vis_fall, 1.0, smoothstep(0.0, 0.45, p));
    return b;
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let src = pg_tap(in.uv);
    let r0 = se.region.xy;
    let rs = max(se.region.zw - se.region.xy, vec2<f32>(1e-6));
    let q = (in.uv - r0) / rs;
    let env = clamp(se.env, 0.0, 1.0);
    let age = se.trigger_age;
    if (env <= 0.0 || age > 4.0 || p_amount() <= 0.0 || any(q < vec2<f32>(0.0)) || any(q > vec2<f32>(1.0))) {
        return src;
    }
    let px_size = rs * se.resolution;
    let lp = q * px_size;
    let bs = max(p_block() * px_size.y / 720.0, 4.0);
    let rows = ceil(px_size.y / bs);
    let speed = p_speed();
    let g = 2.4 * px_size.y * speed * speed;
    let sage = age * speed;
    let boost = clamp(log(1.0 + max(se.trigger.amount, 0.0) / 100.0) / 2.4, 0.0, 1.0);
    let frac = clamp(p_amount() * (0.35 + 0.35 * boost), 0.0, 0.97);
    // Return progress: the release (env falling) or, if held longer than the manifest, by time.
    let ret = max(select(0.0, 1.0 - env, age > 0.15), clamp((age - 1.3) / 0.8, 0.0, 1.0));
    let seed = f32(se.trigger_count % 64u);

    let col = floor(lp.x / bs);
    let row = floor(lp.y / bs);
    var best_d = -1.0e9;
    var best_vis = 0.0;
    var best_loose = 0.0;
    var best_row = row;
    var found = false;
    for (var k = -1; k <= PG_REACH + 1; k = k + 1) {
        let j = row - f32(k);
        if (j < 0.0 || j >= rows) {
            continue;
        }
        let b = pg_block(vec2<f32>(col, j), bs, frac, sage, ret, g, seed);
        let top = j * bs + b.d;
        if (lp.y >= top && lp.y < top + bs && b.vis > 0.003) {
            // moving blocks are in front of resting ones
            let pri = abs(b.d) + b.loose;
            if (!found || pri > abs(best_d) + best_loose) {
                best_d = b.d;
                best_vis = b.vis;
                best_loose = b.loose;
                best_row = j;
                found = true;
            }
        }
    }

    // What shows through the holes: the live picture, blurred, darkened and cooled, with a soft
    // inner shadow at the empty slot's edges.
    var under = vec4<f32>(0.0);
    if (!found || best_vis < 0.999) {
        let rad = (5.0 + 0.12 * bs) / px_size;
        var acc = src * 0.2;
        for (var i = 0; i < 8; i = i + 1) {
            let a = f32(i) * 0.7854 + 0.39;
            acc = acc + pg_tap(r0 + clamp(q + vec2<f32>(cos(a), sin(a)) * rad, vec2<f32>(0.0), vec2<f32>(1.0)) * rs) * 0.1;
        }
        let depth = p_depth();
        let lum = dot(acc.rgb, vec3<f32>(0.299, 0.587, 0.114));
        var c = mix(acc.rgb, vec3<f32>(lum) * vec3<f32>(0.75, 0.85, 1.1), 0.55 * depth);
        let fl = (lp - vec2<f32>(col, row) * bs) / bs;
        let edge = min(min(fl.x, 1.0 - fl.x), min(fl.y, 1.0 - fl.y));
        let socket = mix(0.55, 1.0, smoothstep(0.0, 0.22, edge));
        c = c * mix(1.0, 0.38, depth) * socket;
        under = vec4<f32>(c, acc.a);
    }
    if (!found) {
        return under;
    }

    // The covering block: shifted sample plus a chunky bevel on loose blocks and hairline
    // cracks on the resting ones.
    let sp = vec2<f32>(lp.x, lp.y - best_d);
    let blk = pg_tap(r0 + clamp(sp / px_size, vec2<f32>(0.0), vec2<f32>(1.0)) * rs);
    let f = (sp - vec2<f32>(col, best_row) * bs);
    let bev = max(1.5, bs * 0.045) * px_size.y / 720.0;
    let crack = env * (1.0 - ret);
    var bc = blk.rgb;
    if (best_loose > 0.5) {
        let light = select(1.0, 1.35, f.x < bev || f.y < bev);
        let shade = select(1.0, 0.55, f.x > bs - bev || f.y > bs - bev);
        let outline = select(1.0, 0.2, f.x < 1.0 || f.y < 1.0 || f.x > bs - 1.0 || f.y > bs - 1.0);
        bc = bc * mix(1.0, light * shade * outline, crack);
    } else {
        let line = select(1.0, 0.6, f.x < 1.0 || f.y < 1.0);
        bc = bc * mix(1.0, line, crack * 0.7);
    }
    let block = vec4<f32>(min(bc, vec3<f32>(blk.a)), blk.a);
    return mix(under, block, best_vis);
}
