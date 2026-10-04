// Glide composite (built-in, transition kind `glide`): `se_input` = the outgoing scene on its
// background, `se_input_b` = the incoming scene on transparent (premultiplied), `p_bg()` = the
// incoming scene's background. Where B has content A crossfades into it; where B is empty A
// stays (no brightness dip) and eases into B's background over the last quarter, so the last
// frame is B on its background.
@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let a = textureSampleLevel(se_input, se_sampler, in.uv, 0.0);
    let b = textureSampleLevel(se_input_b, se_sampler, in.uv, 0.0);
    let u = clamp(se.progress, 0.0, 1.0);
    let fade = p_fade();
    if p_full_scene() > 0.5 {
        return mix(a, b, fade);
    }
    // Cleanup is tied to LINEAR time, not the content crossfade which may finish early.
    let cleanup = smoothstep(0.75, 1.0, u);
    return fade * b + (1.0 - b.a * fade) * mix(a, p_bg(), cleanup);
}
