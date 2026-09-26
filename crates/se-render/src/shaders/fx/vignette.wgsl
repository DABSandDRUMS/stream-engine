// vignette: smooth darkening towards the corners.
@fragment
fn fs(in: FxVsOut) -> @location(0) vec4<f32> {
    let c = src(in.uv);
    let d = length(local_uv(in.uv) - 0.5) * 1.41421356;
    let r = param(2u);
    let v = 1.0 - smoothstep(r - param(3u), r, d) * fx.strength;
    return vec4<f32>(c.rgb * v, c.a);
}
