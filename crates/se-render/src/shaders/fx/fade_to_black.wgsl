// fade_to_black: mix everything towards an opaque color.
@fragment
fn fs(in: FxVsOut) -> @location(0) vec4<f32> {
    let c = src(in.uv);
    let col = vec4<f32>(param(2u), param(3u), param(4u), 1.0);
    return mix(c, col, fx.strength);
}
