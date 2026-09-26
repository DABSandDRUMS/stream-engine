// pixelate: square blocks, block size grows with strength.
@fragment
fn fs(in: FxVsOut) -> @location(0) vec4<f32> {
    let bs = max(1.0, mix(1.0, param(2u), fx.strength));
    let origin = fx.region.xy * fx.resolution;
    let px = in.uv * fx.resolution - origin;
    // sample the texel at the block center (never between two texels)
    let center = floor((floor(px / bs) + 0.5) * bs + origin) + 0.5;
    return src(center / fx.resolution);
}
