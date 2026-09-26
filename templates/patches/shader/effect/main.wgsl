// {{label}}: chromatic split along `angle`. Input and output are premultiplied alpha.
@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let kick = max(se.env, s_band_kick());
    let d = vec2<f32>(cos(p_angle()), sin(p_angle())) * p_amount() * (0.2 + 0.8 * kick) * 0.015;
    let center = textureSample(se_input, se_sampler, in.uv);
    let r = textureSample(se_input, se_sampler, in.uv + d).r;
    let b = textureSample(se_input, se_sampler, in.uv - d).b;
    return vec4<f32>(r, center.g, b, center.a);
}
