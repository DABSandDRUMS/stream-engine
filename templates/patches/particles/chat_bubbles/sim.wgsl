// Chat bubbles simulation.
//
// Slots 0 and 1 hold shared state, double-buffered by frame parity: every invocation reads the slot
// written last frame and derives the same spawn decision; only the owner of this frame's slot
// writes it back. The bubbles live in a ring of `max_bubbles` slots: a message takes the slot at
// the cursor, so the cap is the ring size and the oldest bubble is the one replaced.
//
// State slot: pos = (last trigger_count, ring cursor), life = 1 once initialised.
// Bubble: pos = centre, vel.y = rise speed, color.rgb = chatter colour,
// color.a = content (0 typing dots, 1-6 emote) + 8 * side (0 left edge, 1 right edge).
const RESERVED: u32 = 2u;
const POOL: u32 = SE_PARTICLE_COUNT - RESERVED;
const MAX_MSG: u32 = 3u;

// stand-in colours for triggers without a user
const FALLBACK = array<vec3<f32>, 6>(
    vec3<f32>(1.0, 0.31, 0.64), vec3<f32>(0.31, 0.76, 1.0), vec3<f32>(1.0, 0.85, 0.31),
    vec3<f32>(0.49, 1.0, 0.42), vec3<f32>(0.72, 0.49, 1.0), vec3<f32>(1.0, 0.54, 0.24),
);

@compute @workgroup_size(64)
fn sim(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i >= SE_PARTICLE_COUNT {
        return;
    }
    let dt = min(se.dt, 0.05);

    // shared spawn decision (identical in every invocation)
    let s = se_particles[(se.frame + 1u) & 1u];
    let inited = s.life > 0.5;
    let tc = f32(se.trigger_count);
    let cap = u32(clamp(p_max_bubbles(), 1, i32(POOL)));
    var n = 0u;
    if inited && tc > s.pos.x {
        n = min(min(u32(tc - s.pos.x), MAX_MSG), cap);
    }
    let cursor = u32(s.pos.y) % cap;

    if i < RESERVED {
        if i == (se.frame & 1u) {
            var w: Particle;
            w.pos = vec2<f32>(tc, f32((cursor + n) % cap));
            w.life = 1.0;
            se_particles[i] = w;
        }
        return;
    }

    let k = i - RESERVED;
    if k < cap {
        let rel = (k + cap - cursor) % cap;
        if rel < n {
            let msg = u32(tc) - n + 1u + rel;
            let has_user = se.trigger.user_color.a > 0.5;
            let uh = u32(se.trigger.user_hash * 65521.0);
            let h = msg * 2654435761u + 977u;
            // the user picks the glyph; each message picks a side and a spot along it
            let content = select(msg % 7u, min(u32(se.trigger.user_hash * 7.0), 6u), has_user);
            let side = select(0u, 1u, se_hash(h + 1u) < 0.5);
            let edge = 0.035 + p_inset() * se_hash(h + 2u);
            let band = p_band();
            var p: Particle;
            // golden-ratio steps down the band keep consecutive bubbles apart
            let along = fract(f32(msg) * 0.618034 + 0.15 * se_hash(h + 3u));
            p.pos = vec2<f32>(select(edge, 1.0 - edge, side == 1u), mix(band.x, band.y, along));
            p.vel = vec2<f32>(0.0, -p_rise() * (0.8 + 0.4 * se_hash(h + 4u)));
            p.color = vec4<f32>(select(FALLBACK[msg % 6u], se.trigger.user_color.rgb, has_user), f32(content + 8u * side));
            p.life = p_life();
            p.size = p_pixel();
            p.seed = se_hash(h + 5u + uh);
            p.age = 0.0;
            se_particles[i] = p;
            return;
        }
    }

    var p = se_particles[i];
    if p.life <= 0.0 {
        return;
    }
    p.pos = p.pos + p.vel * dt;
    p.age = p.age + dt;
    p.life = p.life - dt;
    se_particles[i] = p;
}
