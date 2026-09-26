// {{label}}: generative plasma. Output is premultiplied alpha.
@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let aspect = se.resolution.x / max(se.resolution.y, 1.0);
    let p = (in.uv - vec2<f32>(0.5)) * vec2<f32>(aspect, 1.0) * p_scale();
    let t = se.time * p_speed();
    let bass = s_band_bass();
    var v = sin(p.x * 1.7 + t) + sin(p.y * 2.3 - t * 1.3) + sin((p.x + p.y) * 1.3 + t * 0.7) + sin(length(p) * 3.0 - t * 2.0 - bass * 4.0);
    v = v * 0.125 + 0.5;
    let a = palette(PAL_ACCENT).rgb;
    let b = palette(PAL_MAGENTA).rgb;
    var col = mix(a, b, v) * (0.55 + 0.45 * sin(v * 6.2831 + t));
    // flash on trigger, in the colour of whoever fired it (white when nobody did)
    let who = mix(vec3<f32>(1.0), se.trigger.user_color.rgb, se.trigger.user_color.a);
    col = col * p_tint().rgb + who * (se.env * 0.25);
    return vec4<f32>(col, 1.0);
}
