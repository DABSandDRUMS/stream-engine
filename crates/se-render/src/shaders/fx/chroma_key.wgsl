// chroma_key: chroma-distance key in BT.709 CbCr with smoothness and spill suppression
// (same model as OBS's chroma key filter); mixed by strength.
fn cbcr(rgb: vec3<f32>) -> vec2<f32> {
    let y = luma(rgb);
    return vec2<f32>((rgb.b - y) / 1.8556, (rgb.r - y) / 1.5748);
}

fn chroma_key(c: vec4<f32>, uv: vec2<f32>, st: FxStage) -> vec4<f32> {
    var rgb = unpremul(c);
    let key = vec3<f32>(stage_param(st, 2u), stage_param(st, 3u), stage_param(st, 4u));
    let d = distance(cbcr(rgb), cbcr(key));
    let base = d - stage_param(st, 5u);
    let mask = pow(clamp(base / max(stage_param(st, 6u), 1e-4), 0.0, 1.0), 1.5);
    let spill = pow(clamp(base / max(stage_param(st, 7u), 1e-4), 0.0, 1.0), 1.5);
    rgb = mix(vec3<f32>(luma(rgb)), rgb, mix(1.0, spill, st.strength));
    let a = c.a * mix(1.0, mask, st.strength);
    return premul(rgb, a);
}
