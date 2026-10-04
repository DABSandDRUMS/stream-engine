// Drum explosion: the trigger blows the picture apart from `center` (the kit). The frame is cut
// into square shards on a grid; every shard flies straight out along its ray from the center,
// spinning, shrinking, pixelating and fading, over a dark backdrop with a flash and a
// shockwave ring. Kicks during the hold shove the debris a little further out. On release the
// motion runs backwards: the shards fly home, unspin and snap back into the picture.
//
// Inverse mapping (bounded search): a shard of speed level s at home radius r is drawn at radius
// r * (1 + k) + K0 * k along the same ray (k = K(t) * s), so for each of the 5 speed levels the
// home point of the pixel inverts exactly: r = (R - K0 k) / (1 + k). Shards never grow, so a
// covering shard of that level has its home in the 2x2 cells around that point; that is 20
// shard tests per pixel and one texture fetch for the winning shard.
// Input and output are premultiplied alpha.

const DE_LEVELS: i32 = 5;

fn de_hash(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn de_tap(uv: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(se_input, se_sampler, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
}

fn de_rot(v: vec2<f32>, a: f32) -> vec2<f32> {
    let c = cos(a);
    let s = sin(a);
    return vec2<f32>(c * v.x - s * v.y, s * v.x + c * v.y);
}

struct DeHit {
    found: bool,
    k: f32,          // this shard's flight amount (also the depth order: further out = in front)
    local: vec2<f32>, // pixel position in the shard's unrotated frame (px from its center)
    home: vec2<f32>,  // shard home center (px)
    vis: f32,
    seed: f32,
};

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let src = de_tap(in.uv);
    let env = clamp(se.env, 0.0, 1.0);
    let age = se.trigger_age;
    let amount = max(p_amount(), 0.0);
    if (env <= 0.0 || amount <= 0.0) {
        return src;
    }
    let res = se.resolution;
    let px = in.uv * res;
    let c = p_center() * res;
    let unit = res.y / 720.0;
    let bs = max(p_shard() * unit, 6.0);
    let seed = f32(se.trigger_count % 61u);

    // flight curve: a fast blast that keeps drifting; bigger payloads blow harder, kicks shove
    let boost = clamp(log2(1.0 + max(se.trigger.amount, 0.0) / 100.0) / 3.0, 0.0, 1.0);
    let releasing = env < 1.0 && age > 0.15;
    let ret = select(0.0, 1.0 - env, releasing);
    // rebuild: ease in, then land with a hard snap
    let back = 1.0 - ret * ret * (3.0 - 2.0 * ret);
    let blast = (1.0 - exp(-age * 3.2)) * (0.42 + 0.3 * boost) + 0.1 * age;
    let shove = 0.08 * s_band_kick() * (1.0 - ret);
    let kk = amount * (blast + shove) * back;
    let k0 = 0.12 * res.y;
    let spin = p_spin();

    let d = px - c;
    let rp = length(d);
    let dir = select(vec2<f32>(1.0, 0.0), d / max(rp, 1e-4), rp > 1e-3);

    var hit: DeHit;
    hit.found = false;
    hit.k = -1.0;
    for (var lv = 0; lv < DE_LEVELS; lv = lv + 1) {
        let s = 0.7 + 0.15 * f32(lv);
        let k = kk * s;
        let rh = (rp - k0 * k) / (1.0 + k);
        if (rh < -bs) {
            continue;
        }
        let hp = c + dir * rh;
        // 2x2 cells whose centers surround the candidate home point
        let base = floor(hp / bs - 0.5);
        for (var j = 0; j < 4; j = j + 1) {
            let cell = base + vec2<f32>(f32(j & 1), f32(j >> 1));
            let h = de_hash(cell + vec2<f32>(seed * 7.13, 1.7));
            if (i32(h * f32(DE_LEVELS)) != lv) {
                continue;
            }
            let home = (cell + 0.5) * bs;
            let hd = home - c;
            let hr = length(hd);
            let hdir = select(vec2<f32>(1.0, 0.0), hd / max(hr, 1e-4), hr > 1e-3);
            let pos = c + hdir * (hr * (1.0 + k) + k0 * k);
            let h2 = de_hash(cell * 1.31 + vec2<f32>(seed, 5.3));
            let ang = (h2 - 0.5) * 2.0 * spin * 3.0 * k;
            let scale = 1.0 - 0.3 * clamp(k, 0.0, 1.2);
            let local = de_rot(px - pos, -ang) / scale;
            if (all(abs(local) <= vec2<f32>(bs * 0.5)) && k > hit.k) {
                let h3 = de_hash(cell * 0.77 + vec2<f32>(3.9, seed));
                hit.found = true;
                hit.k = k;
                hit.local = local;
                hit.home = home;
                hit.vis = 1.0 - smoothstep(0.3 + 0.9 * h3, 0.55 + 1.1 * h3, k);
                hit.seed = h2;
            }
        }
    }

    // backdrop: a dim, cold ghost of the picture, a flash at the center and a shockwave ring
    let lum = dot(src.rgb, vec3<f32>(0.299, 0.587, 0.114));
    let vign = 1.0 - 0.6 * smoothstep(0.2 * res.y, 1.1 * res.y, rp);
    var under = (vec3<f32>(0.02, 0.025, 0.05) + vec3<f32>(lum) * vec3<f32>(0.08, 0.1, 0.16) * p_backdrop()) * vign;
    let uc = se.trigger.user_color.rgb;
    let hot = mix(vec3<f32>(1.0, 0.55, 0.15), uc, 0.35 * step(0.01, dot(uc, uc)));
    let wave_r = age * 1.4 * res.y;
    let ring = exp(-pow((rp - wave_r) / (0.03 * res.y + 0.05 * wave_r), 2.0)) * exp(-age * 2.2);
    let flash = exp(-age * 9.0) * exp(-rp / (0.25 * res.y));
    under = under + hot * (ring * 0.9 + flash * 1.6) * (1.0 - ret);

    if (!hit.found || hit.vis <= 0.0) {
        return vec4<f32>(min(under, vec3<f32>(1.0)) * src.a, src.a);
    }

    // shard content: the picture at its home, mosaicked into chunkier pixels as it flies
    let m = max(1.0, floor(bs / 8.0 * clamp(hit.k * 1.8, 0.0, 1.0) * p_pixelate()));
    var lq = hit.local;
    if (m > 1.0) {
        lq = (floor((hit.local + bs * 0.5) / m) + 0.5) * m - bs * 0.5;
    }
    let tex = de_tap((hit.home + lq) / res);
    var col = tex.rgb;
    // chunky bevel and a hot rim right after the blast (hottest near the center)
    let e = bs * 0.5 - abs(hit.local);
    let edge = min(e.x, e.y);
    let lit = select(1.0, 1.3, hit.local.x < -bs * 0.5 + 2.0 * unit || hit.local.y < -bs * 0.5 + 2.0 * unit);
    let dark = select(1.0, 0.55, hit.local.x > bs * 0.5 - 2.0 * unit || hit.local.y > bs * 0.5 - 2.0 * unit);
    let crack = clamp(hit.k * 6.0, 0.0, 1.0);
    col = col * mix(1.0, lit * dark, crack);
    let near = 0.25 + 0.75 * exp(-length(hit.home - c) / (0.35 * res.y));
    col = mix(col, hot, crack * (1.0 - smoothstep(0.0, 2.5 * unit, edge)) * exp(-age * 3.0) * near * (1.0 - ret));
    // fade out (in) with the flight, a little greyer as it goes
    let shard = vec4<f32>(min(col, vec3<f32>(tex.a)), tex.a);
    let back_c = vec4<f32>(min(under, vec3<f32>(1.0)) * src.a, src.a);
    return mix(back_c, shard, hit.vis);
}
