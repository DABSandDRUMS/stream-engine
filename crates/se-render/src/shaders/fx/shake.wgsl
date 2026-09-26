// shake: screen shake. The image moves along smoothed noise of time (`speed` new targets per
// second, up to `strength` of the region's shorter side) and is zoomed by just enough that its
// edges never come into view.

// Smoothed value noise in -1..1 (hash keys wrap every 4096 steps to keep f32 hashing precise).
fn shake_noise(t: f32, salt: f32) -> f32 {
    let i = floor(t);
    let f = t - i;
    let a = i - 4096.0 * floor(i / 4096.0);
    let b = a + 1.0 - 4096.0 * step(4095.0, a);
    let u = f * f * (3.0 - 2.0 * f);
    return mix(hash12(vec2<f32>(a, salt)), hash12(vec2<f32>(b, salt)), u) * 2.0 - 1.0;
}

@fragment
fn fs(in: FxVsOut) -> @location(0) vec4<f32> {
    let amp = clamp(param(2u) * fx.strength, 0.0, 0.45);
    let t = fx.time * max(param(3u), 0.0);
    let px = max(region_size() * fx.resolution, vec2<f32>(1.0));
    let axis = amp * min(px.x, px.y) / px;
    let off = vec2<f32>(shake_noise(t, 1.7 + fx.seed), shake_noise(t, 5.3 + fx.seed)) * axis;
    // |off| ≤ amp per axis, so a zoom of 1 / (1 − 2·amp) keeps every sample inside the region
    let s = (local_uv(in.uv) - 0.5) * (1.0 - 2.0 * amp) + 0.5 + off;
    return src(region_uv(clamp(s, vec2<f32>(0.0), vec2<f32>(1.0))));
}
