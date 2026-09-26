// {{label}}: soft diagonal wipe from A to B.
@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let a = textureSample(se_input, se_sampler, in.uv);
    let b = textureSample(se_input_b, se_sampler, in.uv);
    let dir = vec2<f32>(cos(p_angle()), sin(p_angle()));
    // project uv onto the wipe direction, normalized to 0..1 over the frame
    let lo = min(0.0, dir.x) + min(0.0, dir.y);
    let hi = max(0.0, dir.x) + max(0.0, dir.y);
    let x = (dot(in.uv, dir) - lo) / max(hi - lo, 1e-4);
    let soft = max(p_softness(), 0.001);
    let edge = se.progress * (1.0 + soft);
    let m = smoothstep(edge - soft, edge, x);
    return mix(b, a, m);
}
