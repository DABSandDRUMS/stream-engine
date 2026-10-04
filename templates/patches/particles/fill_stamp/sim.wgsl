// Fill stamp simulation: a snare-fill detector plus a ring of three stamps.
//
// Fill detection (a shader has no memory, so the detector lives in the particle buffer):
// band.snare is a hit envelope (jumps on a hit, decays ~exp(-20 t)). A snare onset is a rising
// edge through `sensitivity`, re-armed once the envelope falls below half of it, with a 50 ms
// refractory time. The last six onset times are kept in a shift register. A fill is "the
// `hits`-th most recent onset is less than `window` beats ago" (beat length from
// beat.bpm, 120 BPM if unknown): a plain backbeat has one snare per two beats, an eighth-note
// fill four hits in 1.5 beats, a sixteenth fill four in 0.75 beats. A fill stamps at most once
// per `cooldown` seconds (and never < 0.4 s after another stamp); a patch trigger stamps too,
// at most once per TRIGGER_GAP seconds, so a flood of chat triggers can't spam it.
//
// Slots 0 and 1 hold the detector, double-buffered by frame parity: every invocation reads last
// frame's slot and runs the same detector; only the owner of this frame's slot stores it.
// Detector slot: pos, vel, color.xy = last six onset times (newest first), color.z = armed,
// color.w = last fill stamp time, life = last stamp time, size = stamp cursor, seed = last
// trigger_count, age = 1 once initialised.
// Stamp slot: pos (0-1), color = (rotation rad, from trigger, ink seed, 0), size = height
// (fraction of the layer), life = remaining seconds, seed = total life, age.
const STAMP0: u32 = 2u;
const STAMP_N: u32 = 3u;
const REFRACTORY: f32 = 0.05;
const TRIGGER_GAP: f32 = 1.2;

fn onset_time(s: Particle, k: i32) -> f32 {
    switch k {
        case 0: { return s.pos.x; }
        case 1: { return s.pos.y; }
        case 2: { return s.vel.x; }
        case 3: { return s.vel.y; }
        case 4: { return s.color.x; }
        default: { return s.color.y; }
    }
}

@compute @workgroup_size(64)
fn sim(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i >= SE_PARTICLE_COUNT {
        return;
    }
    let dt = min(se.dt, 0.05);
    let now = se.time;

    // shared detector (identical in every invocation)
    var s = se_particles[(se.frame + 1u) & 1u];
    let fresh = s.age < 0.5;
    if fresh {
        s.pos = vec2<f32>(-100.0);
        s.vel = vec2<f32>(-100.0);
        s.color = vec4<f32>(-100.0, -100.0, 1.0, -100.0);
        s.life = -100.0;
        s.size = 0.0;
        s.seed = f32(se.trigger_count);
    }
    let x = s_band_snare();
    let hi = p_sensitivity();
    let hit = s.color.z > 0.5 && x >= hi && now - s.pos.x >= REFRACTORY;
    var armed = select(s.color.z, 1.0, x < hi * 0.5);
    var t = s;
    if hit {
        armed = 0.0;
        t.color.y = s.color.x;
        t.color.x = s.vel.y;
        t.vel.y = s.vel.x;
        t.vel.x = s.pos.y;
        t.pos.y = s.pos.x;
        t.pos.x = now;
    }
    let bpm = select(120.0, s_beat_bpm(), s_beat_bpm() > 1.0);
    let window = p_window() * 60.0 / bpm;
    let need = clamp(p_hits(), 2, 6);
    let fill = hit && now - onset_time(t, need - 1) <= window;
    let fill_stamp = fill && now - s.color.w >= p_cooldown() && now - s.life >= 0.4;
    let tc = f32(se.trigger_count);
    let trig_stamp = !fresh && tc != s.seed && now - s.life >= TRIGGER_GAP;
    let stamp = fill_stamp || trig_stamp;
    let cursor = u32(s.size) % STAMP_N;

    if i < STAMP0 {
        if i == (se.frame & 1u) {
            var w = t;
            w.color.z = armed;
            w.color.w = select(s.color.w, now, fill_stamp);
            w.life = select(s.life, now, stamp);
            w.size = f32((cursor + select(0u, 1u, stamp)) % STAMP_N);
            w.seed = tc;
            w.age = 1.0;
            se_particles[i] = w;
        }
        return;
    }
    if i >= STAMP0 + STAMP_N {
        return;
    }

    var p = se_particles[i];
    if stamp && i - STAMP0 == cursor {
        let h = i * 7919u + se.frame * 104729u;
        let jitter = vec2<f32>(se_hash(h) - 0.5, se_hash(h + 1u) - 0.5) * vec2<f32>(0.14, 0.08) * p_scatter();
        p.pos = p_position() + jitter;
        let side = select(-1.0, 1.0, se_hash(h + 2u) < 0.5);
        p.color = vec4<f32>(side * (0.07 + 0.17 * se_hash(h + 3u)), select(0.0, 1.0, trig_stamp && !fill_stamp), se_hash(h + 4u), 0.0);
        p.size = p_size();
        p.life = p_hold() + 0.6;
        p.seed = p.life;
        p.age = 0.0;
        p.vel = vec2<f32>(0.0);
        se_particles[i] = p;
        return;
    }
    if p.life <= 0.0 {
        return;
    }
    p.life = p.life - dt;
    p.age = p.age + dt;
    se_particles[i] = p;
}
