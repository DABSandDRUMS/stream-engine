// fade_to_black: tint the covered image without filling transparent layer gaps.
fn fade_to_black(c: vec4<f32>, uv: vec2<f32>, st: FxStage) -> vec4<f32> {
    let col = premul(vec3<f32>(stage_param(st, 2u), stage_param(st, 3u), stage_param(st, 4u)), c.a);
    return mix(c, col, st.strength);
}
