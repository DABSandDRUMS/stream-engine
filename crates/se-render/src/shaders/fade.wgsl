// Crossfade (built-in "fade"; same contract as project transitions: generated header + `fs`).
@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let a = textureSampleLevel(se_input, se_sampler, in.uv, 0.0);
    let b = textureSampleLevel(se_input_b, se_sampler, in.uv, 0.0);
    return mix(a, b, se.progress);
}
