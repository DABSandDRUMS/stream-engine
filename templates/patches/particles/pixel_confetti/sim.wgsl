// Pixel confetti simulation.
//
// Slots 0 and 1 hold shared state, double-buffered by frame parity: every invocation reads the
// slot written last frame and derives the same spawn decision from it, and only the invocation
// owning this frame's slot writes the new state. The rest of the buffer is a ring: a new trigger
// claims the next `n` slots from the cursor, so bursts never scan for free particles.
//
// State slot: pos = (last trigger_count, ring cursor), vel.x = kick re-armed flag, life = 1 once
// initialised.
// Piece: color = (VGA index, sprite, spin rate, bounces), age < 0 = waiting to leave the popper.
const RESERVED: u32 = 2u;
const POOL: u32 = SE_PARTICLE_COUNT - RESERVED;

const BRIGHT = array<u32, 7>(12u, 14u, 10u, 11u, 9u, 13u, 15u);
const VGA = array<vec3<f32>, 16>(
    vec3<f32>(0.0, 0.0, 0.0), vec3<f32>(0.0, 0.0, 0.667), vec3<f32>(0.0, 0.667, 0.0), vec3<f32>(0.0, 0.667, 0.667),
    vec3<f32>(0.667, 0.0, 0.0), vec3<f32>(0.667, 0.0, 0.667), vec3<f32>(0.667, 0.333, 0.0), vec3<f32>(0.667, 0.667, 0.667),
    vec3<f32>(0.333, 0.333, 0.333), vec3<f32>(0.333, 0.333, 1.0), vec3<f32>(0.333, 1.0, 0.333), vec3<f32>(0.333, 1.0, 1.0),
    vec3<f32>(1.0, 0.333, 0.333), vec3<f32>(1.0, 0.333, 1.0), vec3<f32>(1.0, 1.0, 0.333), vec3<f32>(1.0, 1.0, 1.0),
);

fn burst_size() -> u32 {
    let boost = 1.0 + 0.6 * min(log2(1.0 + max(se.trigger.amount, se.trigger.count * 100.0) / 100.0), 4.0);
    return min(u32(p_pieces() * boost), POOL / 2u);
}

// Nearest bright VGA colour to the user's chat colour.
fn user_vga() -> u32 {
    let c = se.trigger.user_color.rgb;
    var best = 15u;
    var d = 1e9;
    for (var k = 0u; k < 7u; k++) {
        let e = VGA[BRIGHT[k]] - c;
        let dd = dot(e, e);
        if dd < d {
            d = dd;
            best = BRIGHT[k];
        }
    }
    return best;
}

fn piece(h: u32, delay: f32) -> Particle {
    var p: Particle;
    p.seed = se_hash(h + 9u);
    var vga = BRIGHT[min(u32(se_hash(h + 1u) * 7.0), 6u)];
    if se_hash(h + 2u) < 0.3 * se.trigger.user_color.a {
        vga = user_vga();
    }
    // half the pieces are plain chips, the rest one of the five little icons
    var sprite = 0.0;
    if se_hash(h + 3u) > 0.5 {
        sprite = 1.0 + floor(se_hash(h + 4u) * 4.999);
    }
    let spin = (se_hash(h + 5u) - 0.5) * 18.0;
    p.color = vec4<f32>(f32(vga), sprite, spin, 0.0);
    p.size = p_pixel() * select(1.0, 1.34, se_hash(h + 6u) > 0.7);
    p.life = 2.6 + se_hash(h + 7u) * 1.2;
    p.age = -delay;
    return p;
}

@compute @workgroup_size(64)
fn sim(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i >= SE_PARTICLE_COUNT {
        return;
    }
    let dt = min(se.dt, 0.05);
    let aspect = se.resolution.x / max(se.resolution.y, 1.0);

    // shared spawn decision (identical in every invocation)
    let s = se_particles[(se.frame + 1u) & 1u];
    let inited = s.life > 0.5;
    let tc = f32(se.trigger_count);
    let fire = inited && tc != s.pos.x;
    let n_burst = select(0u, burst_size(), fire);
    let kick = s_band_kick();
    let kfire = inited && p_trickle() && se.env > 0.0 && s.vel.x > 0.5 && kick > 0.6;
    let n_trickle = select(0u, 5u, kfire);
    let cursor = u32(s.pos.y) % POOL;

    if i < RESERVED {
        if i == (se.frame & 1u) {
            var w: Particle;
            w.pos = vec2<f32>(tc, f32((cursor + n_burst + n_trickle) % POOL));
            var armed = s.vel.x;
            if kfire {
                armed = 0.0;
            } else if kick < 0.3 {
                armed = 1.0;
            }
            w.vel = vec2<f32>(armed, 0.0);
            w.life = 1.0;
            se_particles[i] = w;
        }
        return;
    }

    let rel = (i - RESERVED + POOL - cursor) % POOL;
    let h = i * 7919u + se.frame * 104729u;
    if rel < n_burst {
        // popper burst: alternate between the top-left and top-right corners
        let f = f32(rel) / f32(max(n_burst, 1u));
        var p = piece(h, f * 0.28 + se_hash(h + 10u) * 0.04);
        let side = select(1.0, -1.0, (rel & 1u) == 1u);
        let a = mix(-0.35, 0.6, se_hash(h + 11u));
        let v0 = mix(1.6, 4.2, se_hash(h + 12u)) * p_power();
        p.pos = vec2<f32>(0.5 - side * 0.51, 0.04 + se_hash(h + 13u) * 0.06);
        p.vel = vec2<f32>(side * v0 * cos(a) / aspect, v0 * sin(a));
        se_particles[i] = p;
        return;
    }
    if rel < n_burst + n_trickle {
        // trickle: a few pieces dropping in from the top edge
        var p = piece(h, se_hash(h + 10u) * 0.3);
        p.pos = vec2<f32>(0.08 + se_hash(h + 13u) * 0.84, -0.03);
        p.vel = vec2<f32>((se_hash(h + 14u) - 0.5) * 0.2 / aspect, 0.2);
        se_particles[i] = p;
        return;
    }

    var p = se_particles[i];
    if p.life <= 0.0 {
        return;
    }
    if p.age < 0.0 {
        p.age = p.age + dt;
        se_particles[i] = p;
        return;
    }
    let fl = p_flutter();
    let resting = p.color.w >= 2.0;
    if !resting {
        // drag gives confetti its low terminal speed; sway rides on a per-piece sine
        p.vel.y = p.vel.y + 1.6 * dt;
        p.vel = p.vel - p.vel * 2.8 * dt;
        let sway = sin(p.age * (3.0 + 2.0 * p.seed) + p.seed * 37.0) * 0.12 * fl;
        p.pos = p.pos + (p.vel + vec2<f32>(sway / aspect, 0.0)) * dt;
        // bounce once off the bottom edge, then settle and fade
        let floor_y = 1.0 - p.size * 5.0;
        if p.pos.y > floor_y && p.vel.y > 0.0 {
            p.pos.y = floor_y;
            if p.color.w < 0.5 {
                p.vel = vec2<f32>(p.vel.x * 0.5, -(0.3 + 0.2 * p.seed));
                p.color.w = 1.0;
                p.life = min(p.life, 0.9 + 0.3 * p.seed);
            } else {
                // vel.x keeps the flip/turn clock frozen for the draw pass
                p.vel = vec2<f32>(p.age, 0.0);
                p.color.w = 2.0;
            }
        }
    }
    p.life = p.life - dt;
    p.age = p.age + dt;
    if p.pos.x < -0.2 || p.pos.x > 1.2 {
        p.life = 0.0;
    }
    se_particles[i] = p;
}
