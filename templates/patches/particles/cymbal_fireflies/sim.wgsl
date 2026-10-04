// Cymbal fireflies simulation. The pool is split in two:
// - swirl slots [0, SWIRL_N): idle until `se.trigger_count` changes, then a payload-sized share
//   respawns at once and orbits the two cymbals on an analytic spiral (pos recomputed from age);
//   `vel.x` remembers the trigger count each slot has seen, `vel.y` marks the slot initialised.
// - ambient slots [SWIRL_N, count): respawn at a hat/crash-driven rate (capped), rise slowly and
//   wander on a smooth value-noise flow field.
// Motion is done in height units (x * aspect) so speeds and orbits are round on wide canvases.
const SWIRL_N: u32 = 320u;
const TAU: f32 = 6.2831853;

fn hash2(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn vnoise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash2(i);
    let b = hash2(i + vec2<f32>(1.0, 0.0));
    let c = hash2(i + vec2<f32>(0.0, 1.0));
    let d = hash2(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn aspect() -> f32 {
    return se.resolution.x / max(se.resolution.y, 1.0);
}

fn warm(h: f32) -> vec4<f32> {
    return mix(p_color_a(), p_color_b(), h * h);
}

fn cymbal_energy() -> f32 {
    // hats and cymbal wash; band.high has a floor in a busy mix, so only its excess counts
    return clamp(max(s_band_hat(), s_band_high() * 1.25 - 0.35), 0.0, 1.0);
}

fn swirl(i: u32, p_in: Particle) -> Particle {
    var p = p_in;
    let tc = se.trigger_count;
    if p.vel.y < 0.5 {
        // fresh buffer (load / hot reload): adopt the current count without bursting
        p.vel = vec2<f32>(f32(tc), 1.0);
        p.life = 0.0;
        return p;
    }
    let dt = min(se.dt, 0.05);
    if u32(p.vel.x) != tc {
        p.vel.x = f32(tc);
        let boost = 0.25 + 0.2 * min(log2(1.0 + max(se.trigger.amount, 0.0) / 100.0), 3.75);
        let n = u32(clamp(boost * p_burst(), 0.0, 1.0) * f32(SWIRL_N));
        if i < n {
            let h = i * 747796405u + tc * 2891336453u;
            p.seed = se_hash(h);
            p.life = 2.4 + 1.6 * se_hash(h + 1u);
            p.age = 0.0;
            p.size = p_size() * (0.7 + 0.6 * se_hash(h + 2u));
            // swirl radius for this burst lives in color.a (draw only uses rgb), so motes
            // still flying from an earlier burst keep their orbit when the payload changes
            let big = 0.13 + 0.03 * clamp(se.trigger.tier, 0.0, 3.0);
            p.color = vec4<f32>(warm(se_hash(h + 3u)).rgb, big);
            let user = se.trigger.user_color;
            if se_hash(h + 4u) < 0.3 * user.a {
                p.color = vec4<f32>(user.rgb, big);
            }
        }
    }
    if p.life <= 0.0 {
        return p;
    }
    p.age = p.age + dt;
    p.life = p.life - dt;
    let h = bitcast<u32>(p.seed) * 747796405u;
    let side = i & 1u;
    let arm = f32((i >> 1u) & 1u);
    let dir = select(1.0, -1.0, side == 1u);
    let big = p.color.a;
    let rn = sqrt(se_hash(h + 5u));
    let r_end = big * (0.2 + 0.8 * rn);
    let t = p.age;
    let theta0 = arm * 3.14159 + (se_hash(h + 6u) - 0.5) * 0.7 + rn * 5.0;
    let spin = (7.5 - 3.5 * rn) * (1.0 - exp(-t * 0.85)) / 0.85;
    let theta = theta0 + dir * spin;
    let r = r_end * (1.0 - exp(-t * 2.4)) * (1.0 + 0.12 * t);
    let e = select(p_emitter_a(), p_emitter_b(), side == 1u);
    let a = aspect();
    var c = vec2<f32>(e.x * a, e.y);
    c.y = c.y - 0.03 * t - 0.012 * t * t;
    let wob = (vnoise(vec2<f32>(p.seed * 31.0, t * 0.8)) - 0.5) * 0.02 * t;
    let q = c + vec2<f32>(cos(theta) * r + wob, sin(theta) * r * 0.42);
    p.pos = vec2<f32>(q.x / a, q.y);
    return p;
}

fn ambient(i: u32, p_in: Particle) -> Particle {
    var p = p_in;
    let dt = min(se.dt, 0.05);
    let a = aspect();
    if p.life <= 0.0 {
        let pool = f32(SE_PARTICLE_COUNT - SWIRL_N);
        let rate = min(p_rate() + p_hat_gain() * cymbal_energy(), p_max_rate());
        // fresh buffer (size 0): pre-warm with a steady-state population already in flight,
        // then mark the slot initialised (size < 0) so this happens once per slot
        let fresh = p.size == 0.0;
        var chance = rate * dt / pool;
        if fresh {
            chance = p_rate() * p_lifetime() / pool;
        }
        if se_hash(i ^ (se.frame * 2654435761u)) >= chance {
            if fresh {
                p.size = -1.0;
            }
            return p;
        }
        let h = i * 1664525u + se.frame * 1013904223u;
        let e = select(p_emitter_a(), p_emitter_b(), se_hash(h) < 0.5);
        let ang = se_hash(h + 1u) * TAU;
        let rr = sqrt(se_hash(h + 2u));
        let off = vec2<f32>(cos(ang) * p_spread(), sin(ang) * p_spread() * 0.3) * rr;
        p.pos = vec2<f32>(e.x + off.x / a, e.y + off.y);
        p.vel = vec2<f32>((se_hash(h + 3u) - 0.5) * 0.04, -p_rise() * 0.5);
        p.color = warm(se_hash(h + 4u));
        p.life = p_lifetime() * (0.6 + 0.8 * se_hash(h + 5u));
        p.size = p_size() * (0.55 + 0.9 * se_hash(h + 6u));
        p.seed = se_hash(h + 7u);
        p.age = 0.0;
        if fresh {
            let ff = se_hash(h + 8u) * p.life * 0.8;
            p.age = ff;
            p.life = p.life - ff;
            p.pos = p.pos + vec2<f32>((se_hash(h + 9u) - 0.5) * p_wander() * ff / a, -p_rise() * ff);
        }
        return p;
    }
    // smooth flow field: angle from slowly scrolling value noise, sampled in height units
    let q = vec2<f32>(p.pos.x * a, p.pos.y);
    let t = se.time;
    let n = vnoise(q * 2.6 + vec2<f32>(t * 0.13 + p.seed * 3.0, -t * 0.09));
    let ang = n * TAU * 1.5 + p.seed * 2.0;
    let target_v = vec2<f32>(cos(ang), sin(ang)) * p_wander() - vec2<f32>(0.0, p_rise());
    p.vel = mix(p.vel, target_v, 1.0 - exp(-dt * 1.3));
    p.pos = p.pos + vec2<f32>(p.vel.x / a, p.vel.y) * dt;
    p.life = p.life - dt;
    p.age = p.age + dt;
    if p.pos.y < -0.05 || p.pos.x < -0.05 || p.pos.x > 1.05 {
        p.life = 0.0;
    }
    return p;
}

@compute @workgroup_size(64)
fn sim(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i >= SE_PARTICLE_COUNT {
        return;
    }
    let p = se_particles[i];
    if i < SWIRL_N {
        se_particles[i] = swirl(i, p);
    } else {
        se_particles[i] = ambient(i, p);
    }
}
