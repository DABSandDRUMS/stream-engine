// glitch: randomly displaced horizontal slices with channel tearing and rare inverted blocks.
@fragment
fn fs(in: FxVsOut) -> @location(0) vec4<f32> {
    let s = fx.strength;
    let l = local_uv(in.uv);
    let tt = floor(fx.time * 12.0 * max(param(5u), 0.01));
    let cell = floor(vec2<f32>(l.x * 8.0, l.y * 24.0));
    let rnd = hash12(vec2<f32>(cell.y * 1.37, tt + fx.seed));
    let on = step(1.0 - param(2u) * s * 0.6, rnd);
    let shift = (hash12(vec2<f32>(cell.y, tt + 3.1)) - 0.5) * 0.2 * param(3u) * s * on;
    let uv = region_uv(vec2<f32>(clamp(l.x + shift, 0.0, 1.0), l.y));
    let sep = vec2<f32>(0.012 * param(4u) * s * on * region_size().x, 0.0);
    let r = src(uv + sep);
    let g = src(uv);
    let b = src(uv - sep);
    var col = vec4<f32>(r.r, g.g, b.b, max(max(r.a, g.a), b.a));
    let blk = hash12(cell + vec2<f32>(tt * 1.3, 5.0 + fx.seed));
    if blk > 1.0 - 0.04 * param(2u) * s {
        col = vec4<f32>(vec3<f32>(col.a) - col.rgb, col.a);
    }
    return col;
}
