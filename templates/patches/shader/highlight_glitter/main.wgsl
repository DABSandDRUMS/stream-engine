// Highlight glitter: the frame is split into a coarse cell grid. Each cell lives through short
// random "lives"; in an active life it searches the cell for its brightest spot, and if that spot
// is a small isolated highlight (bright, and brighter than a ring around it) it grows a
// four-point star glint there. Rays are at most half a cell long, so every pixel only has to
// check the 2x2 cells nearest to it. Premultiplied alpha in/out.

const ROWS: f32 = 10.0;
const GRID: i32 = 4;
// Isolation ring radius (cells): highlights wider than about this never sparkle.
const RING: f32 = 0.25;
// Longest ray (cells); must stay below 0.5 for the 2x2 neighbourhood to be exact.
const MAX_RAY: f32 = 0.47;

fn hash3(p: vec3<f32>) -> f32 {
    var q = fract(p * vec3<f32>(0.1031, 0.1030, 0.0973));
    q = q + dot(q, q.yxz + 33.33);
    return fract((q.x + q.y) * q.z);
}

fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

// One star-filter ray pair along x (cell units): a hairline with a faint halo, tapering to the tip.
fn rays(d: vec2<f32>, len: f32, thin: f32) -> f32 {
    let along = max(1.0 - abs(d.x) / max(len, 1e-4), 0.0);
    let ay = abs(d.y);
    return along * along * (exp(-ay / thin) + 0.18 * exp(-ay / (4.0 * thin)));
}

// Trigger moment from se.trigger_age (seconds): rise, hold, fall; 0 before any trigger.
fn moment(age: f32, rise: f32, hold: f32, fall: f32) -> f32 {
    return smoothstep(0.0, rise, age) * (1.0 - smoothstep(rise + hold, rise + hold + fall, age));
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let c = textureSample(se_input, se_sampler, in.uv);
    let aspect = se.resolution.x / max(se.resolution.y, 1.0);
    let to_grid = vec2<f32>(aspect * ROWS, ROWS);
    let gp = in.uv * to_grid;
    let base = floor(gp - 0.5);
    let px_per_cell = se.resolution.y / ROWS;
    let thin = 0.9 / px_per_cell;

    let react = p_react();
    let hat = clamp(s_band_hat(), 0.0, 1.0);
    let high = clamp(s_band_high(), 0.0, 1.0);
    let big = clamp(log2(1.0 + max(se.trigger.amount, 0.0) / 100.0) / 3.0, 0.0, 1.0);
    // Always-on effect; the trigger's glitter shower is timed from se.trigger_age.
    let shower = moment(se.trigger_age, 0.08, 0.9 + 0.6 * big, 0.9);

    // Activity: a quiet baseline, more with hats/highs, a shower on the trigger; capped by density.
    let energy = clamp(react * (0.55 * hat + 0.6 * smoothstep(0.2, 0.9, high)), 0.0, 1.5);
    let prob = min(p_density(), 0.7) * clamp(0.45 + 0.7 * energy + shower, 0.0, 1.0);
    let thr = p_threshold() - 0.18 * shower;
    let size = p_size() * (1.0 + 0.2 * energy + 0.35 * shower * (0.6 + 0.4 * big));
    let rot = p_angle();
    let cr = cos(rot);
    let sr = sin(rot);
    let user = se.trigger.user_color;

    var glint = vec3<f32>(0.0);
    for (var j = 0; j <= 1; j = j + 1) {
        for (var i = 0; i <= 1; i = i + 1) {
            let id = base + vec2<f32>(f32(i), f32(j));
            let h0 = hash3(vec3<f32>(id, 1.7));
            let period = 0.45 + 0.5 * h0;
            let tt = se.time / period + h0 * 7.0;
            let life = floor(tt);
            let lt = fract(tt);
            let seed = hash3(vec3<f32>(id, life));
            if seed >= prob {
                continue;
            }
            // Search: a jittered 4x4 probe grid over the cell (the jitter changes every life, so
            // smaller highlights are found over time), then two hill-climb steps onto the peak.
            let jit = vec2<f32>(hash3(vec3<f32>(id, life + 0.3)), hash3(vec3<f32>(id, life + 0.9))) * 0.2;
            var best = -1.0;
            var pos = id + 0.5;
            for (var k = 0; k < GRID * GRID; k = k + 1) {
                let q = id + 0.06 + jit + vec2<f32>(f32(k % GRID), f32(k / GRID)) * (0.68 / f32(GRID - 1));
                let l = luma(textureSampleLevel(se_input, se_sampler, q / to_grid, 0.0).rgb);
                if l > best {
                    best = l;
                    pos = q;
                }
            }
            if best < thr - 0.12 {
                continue;
            }
            var step = 0.08;
            for (var r = 0; r < 2; r = r + 1) {
                let centre = pos;
                for (var k = 0; k < 4; k = k + 1) {
                    let a = f32(k) * 1.5708;
                    let q = centre + vec2<f32>(cos(a), sin(a)) * step;
                    let l = luma(textureSampleLevel(se_input, se_sampler, q / to_grid, 0.0).rgb);
                    if l > best {
                        best = l;
                        pos = q;
                    }
                }
                step = step * 0.5;
            }
            if best < thr {
                continue;
            }
            pos = clamp(pos, id + 0.03, id + 0.97);
            let col = textureSampleLevel(se_input, se_sampler, pos / to_grid, 0.0).rgb;
            // Isolation: the spot must outshine every point of an 8-point ring around it, so flat
            // bright areas (skin, white walls, drum heads), long edges and straight lines (window
            // borders, stands) do not sparkle.
            var ring = 0.0;
            for (var k = 0; k < 8; k = k + 1) {
                let a = f32(k) * 0.7854;
                let q = pos + vec2<f32>(cos(a), sin(a)) * RING;
                ring = max(ring, luma(textureSampleLevel(se_input, se_sampler, q / to_grid, 0.0).rgb));
            }
            let strength = smoothstep(thr, thr + 0.12, best) * smoothstep(0.04, 0.18, best - ring);
            if strength <= 0.0 {
                continue;
            }
            // Fast rise, slower fade, with a slight twist while it lives.
            let e = smoothstep(0.0, 0.18, lt) * (1.0 - smoothstep(0.3, 1.0, lt));
            let tw = (lt - 0.5) * 0.25 + (hash3(vec3<f32>(id, life + 5.5)) - 0.5) * 0.15;
            let ct = cos(tw);
            let st = sin(tw);
            let d0 = gp - pos;
            let dr = vec2<f32>(d0.x * cr + d0.y * sr, -d0.x * sr + d0.y * cr);
            let d = vec2<f32>(dr.x * ct + dr.y * st, -dr.x * st + dr.y * ct);
            let len = MAX_RAY * min(size * (0.7 + 0.3 * hash3(vec3<f32>(id, life + 2.2))), 1.0) * (0.4 + 0.6 * e);
            var star = rays(d, len, thin) + rays(d.yx, len * 0.9, thin);
            let dd = vec2<f32>(d.x + d.y, d.y - d.x) * 0.7071;
            star = star + 0.35 * (rays(dd, len * 0.35, thin) + rays(dd.yx, len * 0.35, thin));
            let r2 = dot(d, d) * px_per_cell * px_per_cell;
            star = star + exp(-r2 / (2.5 + 4.0 * e)) * 1.2 + exp(-r2 / (25.0 + 35.0 * e)) * 0.3;
            // Mostly white with a hint of the highlight's own colour (and the trigger user's).
            var tint = mix(vec3<f32>(1.0), col / max(max(col.r, max(col.g, col.b)), 1e-3), 0.3);
            tint = mix(tint, user.rgb / max(user.a, 1e-3), 0.3 * shower * user.a);
            glint = glint + tint * star * e * strength;
        }
    }

    // se.env is the slot strength when attached (1.0), so it scales the whole look.
    glint = glint * p_amount() * clamp(se.env, 0.0, 1.0);
    glint = vec3<f32>(1.0) - exp(-glint * 1.3);
    let rgb = c.rgb + glint * max(vec3<f32>(c.a) - c.rgb, vec3<f32>(0.0));
    return vec4<f32>(rgb, c.a);
}
