// zoom_pulse: punch-in zoom around (center_x, center_y); with beat = 1 the zoom decays over each beat.
@fragment
fn fs(in: FxVsOut) -> @location(0) vec4<f32> {
    let decay = pow(1.0 - fract(fx.beat_phase), 3.0);
    let pulse = mix(1.0, decay, clamp(param(3u), 0.0, 1.0));
    let z = 1.0 + param(2u) * fx.strength * pulse;
    let c = vec2<f32>(param(4u), param(5u));
    let s = (local_uv(in.uv) - c) / z + c;
    return src(region_uv(s));
}
