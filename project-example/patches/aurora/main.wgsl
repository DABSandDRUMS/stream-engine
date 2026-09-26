// Aurora: layered value noise warped into vertical curtains, tinted with the stream palette.
// The engine prepends the generated header (se.*, p_*(), s_*(), palette(), se_vs).

fn hash(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn noise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash(i);
    let b = hash(i + vec2<f32>(1.0, 0.0));
    let c = hash(i + vec2<f32>(0.0, 1.0));
    let d = hash(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn fbm(p_in: vec2<f32>) -> f32 {
    var p = p_in;
    var v = 0.0;
    var a = 0.5;
    for (var i = 0; i < 5; i = i + 1) {
        v = v + a * noise(p);
        p = p * 2.03 + vec2<f32>(17.1, 9.2);
        a = a * 0.5;
    }
    return v;
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let aspect = se.resolution.x / max(se.resolution.y, 1.0);
    let uv = vec2<f32>(in.uv.x * aspect, in.uv.y);
    let t = se.time * p_speed();
    var col = palette(PAL_BACKGROUND).rgb * 0.6;
    let bass = clamp(s_band_bass(), 0.0, 1.0) * p_bass_gain();
    let n = i32(p_bands());
    for (var k = 0; k < 8; k = k + 1) {
        if k >= n {
            break;
        }
        let fk = f32(k);
        let warp = fbm(vec2<f32>(uv.x * 1.5 + fk * 3.7, t * 0.6 + fk));
        let center = 0.25 + 0.5 * fract(fk * 0.618 + 0.13) + (warp - 0.5) * 0.35;
        let x = uv.x / aspect;
        let curtain = exp(-pow((x - center) * 9.0, 2.0));
        let rays = fbm(vec2<f32>(x * 40.0 + fk * 11.0, t * 1.7)) * 0.7 + 0.3;
        let height = smoothstep(1.0, 0.15 + 0.2 * warp, in.uv.y) * smoothstep(0.0, 0.35, in.uv.y);
        let tint = mix(palette(PAL_GREEN).rgb, select(palette(PAL_CYAN).rgb, palette(PAL_MAGENTA).rgb, k % 2 == 1), 0.5 + 0.5 * sin(t + fk));
        col = col + tint * curtain * rays * height * (1.1 + 1.5 * bass);
    }
    col = col * p_intensity();
    // gentle vignette and dither against banding
    let v = 1.0 - 0.35 * length(in.uv - 0.5);
    col = col * v + (hash(in.pos.xy + vec2<f32>(se.time)) - 0.5) / 255.0;
    return vec4<f32>(clamp(col, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
