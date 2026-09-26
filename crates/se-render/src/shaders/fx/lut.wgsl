// lut: 3D LUT lookup (trilinear), mixed by strength.
@fragment
fn fs(in: FxVsOut) -> @location(0) vec4<f32> {
    let c = src(in.uv);
    let rgb = clamp(unpremul(c), vec3<f32>(0.0), vec3<f32>(1.0));
    let n = f32(textureDimensions(fx_lut).x);
    let coord = rgb * ((n - 1.0) / n) + 0.5 / n;
    let graded = textureSampleLevel(fx_lut, fx_sampler, coord, 0.0).rgb;
    return premul(mix(rgb, graded, fx.strength), c.a);
}
