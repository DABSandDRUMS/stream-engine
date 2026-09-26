// fade_to_black: mix everything towards an opaque color.
fn fade_to_black(c: vec4<f32>, uv: vec2<f32>, st: FxStage) -> vec4<f32> {
    let col = vec4<f32>(stage_param(st, 2u), stage_param(st, 3u), stage_param(st, 4u), 1.0);
    return mix(c, col, st.strength);
}
