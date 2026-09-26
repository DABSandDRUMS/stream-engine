// Crossfade.
// License: MIT OR Apache-2.0 (stream-engine). Contract: the engine prepends the generated header
// (`se.progress`, `se_input` = outgoing scene A, `se_input_b` = incoming scene B, `se_sampler`,
// params as `p_<name>()`); entry point `fs`.
@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let a = textureSampleLevel(se_input, se_sampler, in.uv, 0.0);
    let b = textureSampleLevel(se_input_b, se_sampler, in.uv, 0.0);
    return mix(a, b, se.progress);
}
