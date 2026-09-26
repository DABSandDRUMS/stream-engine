// Sparks simulation: one invocation per particle. Dead particles (life <= 0) respawn at the
// bottom edge with a probability that follows the trigger envelope; live ones fly up, slow down,
// and flicker out. The trigger payload shapes the burst: bigger events (bits, gift count, raid
// size) spawn more sparks, and a third of them take the colour of whoever fired it.
fn spawn(i: u32) -> Particle {
    let h = i * 1664525u + se.frame * 1013904223u;
    var p: Particle;
    p.pos = vec2<f32>(se_hash(h), 1.02);
    let spread = (se_hash(h + 1u) - 0.5) * 0.35;
    p.vel = vec2<f32>(spread, -(0.5 + se_hash(h + 2u)) * p_speed() * 1.6);
    p.color = mix(palette(PAL_ACCENT), palette(PAL_YELLOW), se_hash(h + 3u));
    let user = se.trigger.user_color;
    if se_hash(h + 7u) < 0.33 * user.a {
        p.color = user;
    }
    p.life = 1.0 + se_hash(h + 4u) * 1.5;
    p.size = p_size() * (0.5 + se_hash(h + 5u));
    p.seed = se_hash(h + 6u);
    p.age = 0.0;
    return p;
}

@compute @workgroup_size(64)
fn sim(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i >= SE_PARTICLE_COUNT {
        return;
    }
    var p = se_particles[i];
    let dt = min(se.dt, 0.05);
    if p.life <= 0.0 {
        let boost = 1.0 + min(log2(1.0 + se.trigger.amount / 100.0), 4.0);
        let chance = se.env * p_rate() * boost * dt;
        if se_hash(i ^ (se.frame * 2654435761u)) < chance {
            se_particles[i] = spawn(i);
        }
        return;
    }
    p.vel.y = p.vel.y + p_gravity() * dt;
    p.vel = p.vel * (1.0 - 0.6 * dt);
    p.vel.x = p.vel.x + sin(se.time * 5.0 + p.seed * 40.0) * 0.08 * dt;
    p.pos = p.pos + p.vel * dt;
    p.life = p.life - dt;
    p.age = p.age + dt;
    se_particles[i] = p;
}
