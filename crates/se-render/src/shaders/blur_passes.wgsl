// Reduced-resolution gaussian blur passes (appended to fx_common.wgsl). Rendered into
// quarter-resolution targets: `fs_down` (4×4 box from the full-res input), then `fs_h` / `fs_v`
// separable gaussian (13 taps) reading fx_input at quarter resolution.
@fragment
fn fs_down(in: FxVsOut) -> @location(0) vec4<f32> {
    let t = 1.0 / fx.resolution; // full-res texel (resolution = full-res size here)
    var acc = vec4<f32>(0.0);
    for (var y = -1.5; y <= 1.5; y = y + 1.0) {
        for (var x = -1.5; x <= 1.5; x = x + 1.0) {
            acc = acc + src(in.uv + vec2<f32>(x, y) * t);
        }
    }
    return acc / 16.0;
}

fn gauss(uv: vec2<f32>, dir: vec2<f32>) -> vec4<f32> {
    // params: radius (index 2) is in full-resolution px; we run at quarter resolution.
    let r = max(param(2u) * fx.strength / 4.0, 0.5);
    let sigma = max(r / 2.0, 0.5);
    let step = max(r / 6.0, 1.0);
    let texel = 1.0 / fx.resolution; // quarter-res texel (resolution = quarter-res size here)
    var acc = vec4<f32>(0.0);
    var wsum = 0.0;
    for (var i = -6; i <= 6; i = i + 1) {
        let o = f32(i) * step;
        let w = exp(-0.5 * (o * o) / (sigma * sigma));
        acc = acc + src(uv + dir * o * texel) * w;
        wsum = wsum + w;
    }
    return acc / wsum;
}

@fragment
fn fs_h(in: FxVsOut) -> @location(0) vec4<f32> {
    return gauss(in.uv, vec2<f32>(1.0, 0.0));
}

@fragment
fn fs_v(in: FxVsOut) -> @location(0) vec4<f32> {
    return gauss(in.uv, vec2<f32>(0.0, 1.0));
}
