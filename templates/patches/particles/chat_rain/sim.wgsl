// Chat rain simulation.
//
// Slots 0 and 1 hold shared state, double-buffered by frame parity: every invocation reads the slot
// written last frame and derives the same spawn decision from it; only the owner of this frame's
// slot writes the new state. Two rings follow: message groups (one drop + six splash droplets each)
// and the ambient drizzle, so a heavy drizzle never overwrites a chatter's drop.
//
// State slot: pos = (last trigger_count, message group cursor), vel = (drizzle cursor, fractional
// drizzle drops owed), life = 1 once initialised.
// Particle: color.rgb = colour, color.a = kind + 8 * shape. Kinds: 0 message drop (falling),
// 1 message splash, 2 splash droplet (age < 0 = waiting for its drop to land), 3 drizzle streak,
// 4 drizzle splash. pos is the sprite's bottom-centre.
const RESERVED: u32 = 2u;
const GROUP: u32 = 7u;
const GROUPS: u32 = 40u;
const MSG0: u32 = RESERVED;
const AMB0: u32 = MSG0 + GROUP * GROUPS;
const AMB_N: u32 = SE_PARTICLE_COUNT - AMB0;
const MAX_MSG: u32 = 3u;
const MAX_AMB: u32 = 8u;
const START_Y: f32 = -0.005;

// stand-in colours for triggers without a user
const FALLBACK = array<vec3<f32>, 6>(
    vec3<f32>(1.0, 0.31, 0.64), vec3<f32>(0.31, 0.76, 1.0), vec3<f32>(1.0, 0.85, 0.31),
    vec3<f32>(0.49, 1.0, 0.42), vec3<f32>(0.72, 0.49, 1.0), vec3<f32>(1.0, 0.54, 0.24),
);

fn gravity() -> f32 {
    let sp = p_speed();
    return 1.5 * sp * sp;
}

@compute @workgroup_size(64)
fn sim(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i >= SE_PARTICLE_COUNT {
        return;
    }
    let dt = min(se.dt, 0.05);
    let aspect = se.resolution.x / max(se.resolution.y, 1.0);
    let floor_y = p_floor();

    // shared spawn decision (identical in every invocation)
    let s = se_particles[(se.frame + 1u) & 1u];
    let inited = s.life > 0.5;
    let tc = f32(se.trigger_count);
    var n_msg = 0u;
    if inited && tc > s.pos.x {
        n_msg = min(u32(tc - s.pos.x), MAX_MSG);
    }
    let rate = min(max(s_twitch_chat_rate(), 0.0) / 100.0 * p_density(), p_max_drops());
    var owed = select(0.0, s.vel.y + rate * dt, inited);
    let n_amb = min(u32(floor(owed)), MAX_AMB);
    owed = min(owed - f32(n_amb), 1.0);
    let mc = u32(s.pos.y) % GROUPS;
    let ac = u32(s.vel.x) % AMB_N;

    if i < RESERVED {
        if i == (se.frame & 1u) {
            var w: Particle;
            w.pos = vec2<f32>(tc, f32((mc + n_msg) % GROUPS));
            w.vel = vec2<f32>(f32((ac + n_amb) % AMB_N), owed);
            w.life = 1.0;
            se_particles[i] = w;
        }
        return;
    }

    let h = i * 7919u + se.frame * 104729u;
    if i < AMB0 {
        let k = i - MSG0;
        let m = (k / GROUP + GROUPS - mc) % GROUPS;
        if m < n_msg {
            // one chat message: the user picks the column and the shape, the message jitters it
            let msg = u32(tc) - n_msg + 1u + m;
            let has_user = se.trigger.user_color.a > 0.5;
            let uh = u32(se.trigger.user_hash * 65521.0);
            let user_seed = select(msg * 2654435761u, uh * 40503u + 17u, has_user);
            let lane = 0.05 + 0.9 * se_hash(user_seed);
            let x = clamp(lane + (se_hash(msg * 977u + 3u) - 0.5) * 0.07, 0.03, 0.97);
            let shape = min(u32(se_hash(user_seed + 5u) * 4.0), 3u);
            let rgb = select(FALLBACK[msg % 6u], se.trigger.user_color.rgb, has_user);
            let sp = p_speed();
            let g = gravity();
            let v0 = (0.22 + 0.1 * se_hash(msg * 31u + 7u)) * sp;
            let fall = max(floor_y - START_Y, 0.01);
            let t_hit = (sqrt(v0 * v0 + 2.0 * g * fall) - v0) / g;
            let j = k % GROUP;
            var p: Particle;
            p.size = p_pixel();
            p.seed = se_hash(h + 1u);
            if j == 0u {
                p.pos = vec2<f32>(x, START_Y);
                p.vel = vec2<f32>(0.0, v0);
                p.color = vec4<f32>(rgb, f32(shape * 8u));
                p.life = t_hit + 1.0;
                p.age = 0.0;
            } else if i32(j) <= p_droplets() {
                // droplets wait at the impact point until the drop gets there
                let side = select(-1.0, 1.0, (j & 1u) == 0u);
                let spread = f32((j + 1u) / 2u);
                p.pos = vec2<f32>(x, floor_y);
                p.vel = vec2<f32>(side * (0.06 + 0.07 * spread + 0.05 * se_hash(h + 2u)) / aspect, -(0.32 + 0.3 * se_hash(h + 3u)) / spread * 1.15);
                p.color = vec4<f32>(rgb, 2.0);
                p.life = 0.8;
                p.age = -t_hit;
            }
            se_particles[i] = p;
            return;
        }
    } else {
        let rel = (i - AMB0 + AMB_N - ac) % AMB_N;
        if rel < n_amb {
            var p: Particle;
            let wind = p_wind() * 0.35 / aspect;
            p.pos = vec2<f32>(mix(-0.05, 1.05, se_hash(h + 4u)) - wind * 0.4, -0.02 - 0.06 * se_hash(h + 5u));
            p.vel = vec2<f32>(wind, 1.15 + 0.45 * se_hash(h + 6u));
            p.color = vec4<f32>(p_rain_color().rgb, 3.0);
            p.size = p_pixel() * 0.55;
            p.seed = se_hash(h + 7u);
            p.life = 3.0;
            p.age = 0.0;
            se_particles[i] = p;
            return;
        }
    }

    var p = se_particles[i];
    if p.life <= 0.0 {
        return;
    }
    let code = u32(p.color.a);
    let kind = code & 7u;
    if kind == 0u {
        // exact constant-acceleration step, so the droplets' precomputed wait matches the landing
        let g = gravity();
        p.pos.y = p.pos.y + p.vel.y * dt + 0.5 * g * dt * dt;
        p.vel.y = p.vel.y + g * dt;
        if p.pos.y >= floor_y {
            p.pos.y = floor_y;
            p.color.a = f32((code & ~7u) | 1u);
            p.age = 0.0;
            p.life = 0.36;
        }
    } else if kind == 2u {
        if p.age < 0.0 {
            p.age = p.age + dt;
            se_particles[i] = p;
            return;
        }
        p.vel.y = p.vel.y + 3.2 * dt;
        p.pos = p.pos + p.vel * dt;
        if p.pos.y > floor_y && p.vel.y > 0.0 {
            p.life = 0.0;
        }
    } else if kind == 3u {
        p.pos = p.pos + p.vel * dt;
        if p.pos.y >= floor_y {
            p.pos.y = floor_y;
            p.color.a = 4.0;
            p.age = 0.0;
            p.life = 0.16;
        }
    }
    p.age = p.age + dt;
    p.life = p.life - dt;
    se_particles[i] = p;
}
