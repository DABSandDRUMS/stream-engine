// Groove Ripple: radial wave packets from `center`, driven by the kick envelope (the latest hit),
// a slow bass swell, and a one-shot "stone drop" burst timed from se.trigger_age.
// No frame history: band.kick jumps to the hit strength and decays exponentially (engine
// default: to 5 % in 150 ms, i.e. exp(-20 t)), so `-ln(kick) / 20` is the time since the hit and
// the packet radius is evaluated analytically. Input/output are premultiplied alpha.

const KICK_DECAY: f32 = 20.0;
const TAU: f32 = 6.2831853;
const WAVELENGTH: f32 = 0.075;

// One ring packet at distance `r` (height units): x = radial slope (refraction), y = crest glint.
fn packet(r: f32, front: f32, width: f32, amp: f32) -> vec2<f32> {
    let x = r - front;
    let g = amp * exp(-(x * x) / (width * width));
    let ph = x * TAU / WAVELENGTH;
    let glint = pow(max(-sin(ph), 0.0), 6.0);
    return vec2<f32>(g * cos(ph), g * glint);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let amount = max(p_amount(), 0.0);
    let aspect = se.resolution.x / max(se.resolution.y, 1.0);
    let ar = vec2<f32>(aspect, 1.0);
    let speed = 2.8 * p_speed();

    let q = (in.uv - p_center()) * ar;
    let r = length(q);
    // rings grow in as they leave the kit, so the source itself stays steady
    let birth = smoothstep(0.04, 0.45, r);
    let dir = q / max(r, 1e-4);

    var w = vec2<f32>(0.0);

    // latest kick: one packet of a few crests, widening and fading as it travels
    let kick = clamp(s_band_kick(), 0.0, 1.0);
    let u = -log(max(kick, 1e-4)) / KICK_DECAY;
    let kick_amp = exp(-2.0 * u) * smoothstep(0.0012, 0.012, kick);
    w = w + packet(r, speed * u, 0.06 + 0.2 * u, kick_amp);

    // continuous bass swell: a long, slow outgoing undulation (zero when the bass is quiet)
    let bass = smoothstep(0.35, 0.95, clamp(s_band_bass(), 0.0, 1.0));
    let swell_ph = (r - se.time * 0.12 * p_speed()) * TAU / 0.4;
    w.x = w.x + 0.18 * bass * cos(swell_ph);

    // trigger: a one-shot "stone drop" from the center — a big ring and two echoes that roll out
    // to the edges over ~3 s; bigger payloads make bigger waves. Timed from se.trigger_age
    // (se.env is the slot strength when attached).
    let age = se.trigger_age;
    let big = clamp(1.3 + 0.35 * log2(1.0 + se.trigger.amount / 100.0), 1.3, 2.4);
    var drop_fade = 0.0;
    if age < 4.0 {
        drop_fade = exp(-age * 0.7);
        for (var i = 0; i < 3; i = i + 1) {
            let tau = age - f32(i) * 0.22;
            if tau > 0.0 {
                let a = big * (1.0 - 0.3 * f32(i)) * smoothstep(0.0, 0.08, tau) * exp(-tau * 0.9);
                w = w + packet(r, speed * 0.3 * tau, 0.07 + 0.08 * tau, a);
            }
        }
    }

    // keep faces clean: calm ellipse in the upper middle; ripple grows toward the edges
    let face = length((in.uv - vec2<f32>(0.5, 0.4)) * vec2<f32>(aspect * 0.75, 1.0));
    let calm = max(p_calm(), 1e-3);
    let mask = smoothstep(calm * 0.55, calm * 1.45, face);
    let edge = smoothstep(0.15, 0.85, length((in.uv - vec2<f32>(0.5)) * ar) / (0.5 * length(ar)));
    let weight = amount * birth * mask * (0.6 + 0.4 * edge);

    // refraction with a whisper of dispersion along the ring normal
    let off = dir * (w.x * weight * p_refraction() * 0.008) / ar;
    let base = textureSample(se_input, se_sampler, in.uv + off);
    let rch = textureSample(se_input, se_sampler, in.uv + off * 1.25).r;
    let bch = textureSample(se_input, se_sampler, in.uv + off * 0.75).b;
    var col = vec3<f32>(rch, base.g, bch);

    // faint crest glint, screen-blended so it never clips; tinted by the trigger user's colour
    let tint = mix(vec3<f32>(0.85, 0.92, 1.0), se.trigger.user_color.rgb, 0.7 * drop_fade * se.trigger.user_color.a);
    let glint = clamp(w.y * weight * p_highlight() * 0.22 * (1.0 + 1.5 * drop_fade), 0.0, 0.5);
    col = col + (vec3<f32>(base.a) - col) * tint * glint;
    return vec4<f32>(min(col, vec3<f32>(base.a)), base.a);
}
