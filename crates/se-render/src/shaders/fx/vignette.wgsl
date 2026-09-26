// vignette: smooth darkening towards the corners.
fn vignette(c: vec4<f32>, uv: vec2<f32>, st: FxStage) -> vec4<f32> {
    let d = length(local_uv(uv) - 0.5) * 1.41421356;
    let r = stage_param(st, 2u);
    let v = 1.0 - smoothstep(r - stage_param(st, 3u), r, d) * st.strength;
    return vec4<f32>(c.rgb * v, c.a);
}
