// grade: exposure, warmth/tint white balance, contrast, saturation, lift; mixed by strength.
fn grade(c: vec4<f32>, uv: vec2<f32>, st: FxStage) -> vec4<f32> {
    let orig = unpremul(c);
    var rgb = orig * exp2(stage_param(st, 7u));
    let w = stage_param(st, 2u);
    let t = stage_param(st, 3u);
    rgb = rgb * vec3<f32>(1.0 + 0.15 * w + 0.05 * t, 1.0 - 0.1 * t, 1.0 - 0.15 * w + 0.05 * t);
    rgb = (rgb - 0.5) * stage_param(st, 4u) + 0.5;
    rgb = mix(vec3<f32>(luma(rgb)), rgb, stage_param(st, 5u));
    rgb = rgb + stage_param(st, 6u) * (1.0 - rgb);
    return premul(mix(orig, rgb, st.strength), c.a);
}
