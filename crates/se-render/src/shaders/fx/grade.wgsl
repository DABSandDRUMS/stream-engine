// grade: exposure, warmth/tint white balance, contrast, saturation, lift; mixed by strength.
@fragment
fn fs(in: FxVsOut) -> @location(0) vec4<f32> {
    let c = src(in.uv);
    let orig = unpremul(c);
    var rgb = orig * exp2(param(7u));
    let w = param(2u);
    let t = param(3u);
    rgb = rgb * vec3<f32>(1.0 + 0.15 * w + 0.05 * t, 1.0 - 0.1 * t, 1.0 - 0.15 * w + 0.05 * t);
    rgb = (rgb - 0.5) * param(4u) + 0.5;
    rgb = mix(vec3<f32>(luma(rgb)), rgb, param(5u));
    rgb = rgb + param(6u) * (1.0 - rgb);
    return premul(mix(orig, rgb, fx.strength), c.a);
}
