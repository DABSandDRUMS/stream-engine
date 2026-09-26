// blur (final pass): mix the input with the quarter-resolution gaussian result in fx_aux.
@fragment
fn fs(in: FxVsOut) -> @location(0) vec4<f32> {
    let c = src(in.uv);
    let half_texel = 0.5 / vec2<f32>(textureDimensions(fx_aux));
    let uv = clamp(in.uv, fx.region.xy + half_texel, fx.region.zw - half_texel);
    let b = textureSampleLevel(fx_aux, fx_sampler, uv, 0.0);
    return mix(c, b, clamp(fx.strength * 2.0, 0.0, 1.0));
}
