// Emoji percussion simulation.
//
// Slots 0 and 1 hold the onset detector, double-buffered by frame parity: every invocation reads
// last frame's slot, runs the same detector on the current band envelopes, and agrees on which
// hits fired; only the owner of this frame's slot stores the result. Each drum owns a ring of
// slots (the density cap): a hit claims the next slots of its ring from the ring's cursor.
//
// Detector slot: pos = armed (kick, snare), vel = (armed hat, last kick time),
// color = (last snare time, last hat time, kick cursor, snare cursor), life = hat cursor,
// size = hit counter (seeds the per-hit dice), seed = last trigger_count, age = 1 once initialised.
// A new patch trigger fires a fanfare: big burst, extra stars, sparkles from both cymbals.
// Sprite: color = (sprite kind, VGA tint, spin seed, pixel scale); kind 0 burst, 1 star, 2 sparkle.
const KICK0: u32 = 2u;
const KICK_N: u32 = 8u;
const SNARE0: u32 = 10u;
const SNARE_N: u32 = 24u;
const HAT0: u32 = 34u;
const HAT_N: u32 = 64u;
const REFRACTORY: f32 = 0.07;

struct Hit {
    fire: bool,
    armed: f32,
    last: f32,
};

// Rising edge through the sensitivity level; re-arms once the envelope falls below half of it.
fn onset(x: f32, armed: f32, last: f32) -> Hit {
    let hi = p_sensitivity();
    var h: Hit;
    h.fire = armed > 0.5 && x >= hi && se.time - last >= REFRACTORY;
    h.armed = select(armed, 1.0, x < hi * 0.5);
    h.last = last;
    if h.fire {
        h.armed = 0.0;
        h.last = se.time;
    }
    return h;
}

// slot k of a ring was claimed this frame if it lies in [cursor, cursor + n)
fn claimed(k: u32, cursor: u32, n: u32, size: u32) -> i32 {
    let rel = (k + size - cursor) % size;
    return select(-1, i32(rel), rel < n);
}

@compute @workgroup_size(64)
fn sim(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i >= SE_PARTICLE_COUNT {
        return;
    }
    let dt = min(se.dt, 0.05);
    let aspect = se.resolution.x / max(se.resolution.y, 1.0);
    let dens = p_density();

    // shared detector (identical in every invocation)
    let s = se_particles[(se.frame + 1u) & 1u];
    let kick = onset(s_band_kick(), s.pos.x, s.vel.y);
    let snare = onset(s_band_snare(), s.pos.y, s.color.x);
    let hat = onset(s_band_hat(), s.vel.x, s.color.y);
    let hits = u32(s.size);
    let crash = hat.fire && s_band_high() >= p_crash_level();
    // a patch trigger fires everything at once (bigger payloads spray more sparkles)
    let tc = f32(se.trigger_count);
    let fanfare = s.age > 0.5 && tc != s.seed;
    let n_kick = select(0u, 1u, kick.fire || fanfare);
    var n_snare = 0u;
    if snare.fire {
        n_snare = 1u + select(0u, 1u, se_hash(hits * 3u + 1u) < dens * 0.7);
    }
    if fanfare {
        n_snare = n_snare + 3u;
    }
    var n_hat = 0u;
    if crash {
        n_hat = 3u + u32(round(dens * 7.0));
    } else if hat.fire && se_hash(hits * 3u + 2u) < dens * 0.45 {
        n_hat = 1u;
    }
    if fanfare {
        n_hat = n_hat + 10u + u32(min(log2(1.0 + se.trigger.amount / 100.0), 3.0) * 4.0);
    }
    let ck = u32(s.color.z) % KICK_N;
    let cs = u32(s.color.w) % SNARE_N;
    let ch = u32(s.life) % HAT_N;

    if i < KICK0 {
        if i == (se.frame & 1u) {
            var w: Particle;
            w.pos = vec2<f32>(kick.armed, snare.armed);
            w.vel = vec2<f32>(hat.armed, kick.last);
            w.color = vec4<f32>(snare.last, hat.last, f32((ck + n_kick) % KICK_N), f32((cs + n_snare) % SNARE_N));
            w.life = f32((ch + n_hat) % HAT_N);
            w.size = f32((hits + select(0u, 1u, kick.fire || snare.fire || hat.fire)) % 65536u);
            w.seed = tc;
            w.age = 1.0;
            se_particles[i] = w;
        }
        return;
    }

    let h = i * 7919u + se.frame * 104729u;
    var p = se_particles[i];
    var spawned = false;
    if i < SNARE0 {
        if claimed(i - KICK0, ck, n_kick, KICK_N) >= 0 {
            // kick: one burst that slams in near the bass drum and drifts up
            let o = vec2<f32>(se_hash(h) - 0.5, se_hash(h + 1u) - 0.5) * vec2<f32>(0.08 / aspect, 0.06);
            p.pos = p_kick_pos() + o;
            p.vel = vec2<f32>((se_hash(h + 2u) - 0.5) * 0.06 / aspect, -0.1);
            p.color = vec4<f32>(0.0, 0.0, se_hash(h + 3u), select(1.5, 2.3, fanfare));
            p.life = 0.5;
            spawned = true;
        }
    } else if i < HAT0 {
        if claimed(i - SNARE0, cs, n_snare, SNARE_N) >= 0 {
            // snare: stars flung up in a little arc
            let side = se_hash(h) - 0.5;
            p.pos = p_snare_pos() + vec2<f32>(side * 0.04 / aspect, 0.0);
            let lift = select(1.0, 1.35, fanfare);
            p.vel = vec2<f32>(side * 0.9 / aspect, -(0.55 + 0.35 * se_hash(h + 1u)) * lift);
            p.color = vec4<f32>(1.0, 0.0, se_hash(h + 3u), 0.75 + 0.35 * se_hash(h + 2u));
            p.life = 0.95;
            spawned = true;
        }
    } else if i < HAT0 + HAT_N {
        let r = claimed(i - HAT0, ch, n_hat, HAT_N);
        if r >= 0 {
            let tint = select(0.0, 1.0, se_hash(h + 4u) < 0.35);
            if crash || fanfare {
                // crash: a ring of sparkles sprayed from the crash cymbal (a trigger sprays from both cymbals)
                let from_hat = fanfare && (r & 1) == 1;
                let a = 6.2832 * (f32(r) + 0.6 * se_hash(h)) / f32(n_hat);
                let v = 0.35 + 0.35 * se_hash(h + 1u);
                p.pos = select(p_crash_pos(), p_hat_pos(), from_hat);
                p.vel = vec2<f32>(cos(a) * v / aspect, sin(a) * v * 0.8);
                p.color = vec4<f32>(2.0, tint, se_hash(h + 3u), 0.8 + 0.5 * se_hash(h + 2u));
                p.life = 0.8 + 0.3 * se_hash(h + 5u);
            } else {
                // hat: a single twinkle near the hats
                let o = vec2<f32>(se_hash(h) - 0.5, se_hash(h + 1u) - 0.5) * vec2<f32>(0.1 / aspect, 0.08);
                p.pos = p_hat_pos() + o;
                p.vel = vec2<f32>(0.0, -0.04);
                p.color = vec4<f32>(2.0, tint, se_hash(h + 3u), 0.75 + 0.25 * se_hash(h + 2u));
                p.life = 0.45;
            }
            spawned = true;
        }
    } else {
        return;
    }
    if spawned {
        p.age = 0.0;
        p.size = p_pixel();
        p.seed = se_hash(h + 7u);
        se_particles[i] = p;
        return;
    }

    if p.life <= 0.0 {
        return;
    }
    let kind = u32(p.color.x);
    if kind == 1u {
        p.vel.y = p.vel.y + 1.5 * dt;
    } else if kind == 2u {
        p.vel = p.vel - p.vel * 2.5 * dt;
    }
    p.pos = p.pos + p.vel * dt;
    p.life = p.life - dt;
    p.age = p.age + dt;
    se_particles[i] = p;
}
