// rgb_split: red and blue sampled from opposite offsets along `angle`.
@fragment
fn fs(in: FxVsOut) -> @location(0) vec4<f32> {
    let a = radians(param(2u));
    let width_px = region_size().x * fx.resolution.x;
    let off = vec2<f32>(cos(a), sin(a)) * param(3u) * fx.strength * width_px / fx.resolution;
    let r = src(in.uv + off);
    let g = src(in.uv);
    let b = src(in.uv - off);
    return vec4<f32>(r.r, g.g, b.b, max(max(r.a, g.a), b.a));
}
