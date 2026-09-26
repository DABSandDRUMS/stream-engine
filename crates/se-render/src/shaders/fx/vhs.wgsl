// vhs: line jitter + rolling tracking band, chroma misregistration, tape desaturation,
// scanlines and grain.
@fragment
fn fs(in: FxVsOut) -> @location(0) vec4<f32> {
    let s = fx.strength;
    let l = local_uv(in.uv);
    let t = fx.time;
    let rows = region_size().y * fx.resolution.y;
    let line = floor(l.y * rows / 2.0);
    let jit = (hash12(vec2<f32>(line, floor(t * 30.0))) - 0.5) * 0.006 * param(3u) * s;
    let band_y = fract(t * 0.11);
    let track = (1.0 - smoothstep(0.0, 0.035, abs(band_y - l.y))) * 0.025 * param(3u) * s;
    let uv = region_uv(vec2<f32>(l.x + jit + track, l.y));
    let bleed = vec2<f32>(4.0 * param(5u) * s / fx.resolution.x, 0.0);
    let c = src(uv);
    var rgb = vec3<f32>(unpremul(src(uv - bleed)).r, unpremul(c).g, unpremul(src(uv + bleed)).b);
    rgb = mix(vec3<f32>(luma(rgb)), rgb, 1.0 - 0.3 * s);
    let sl = 0.5 + 0.5 * sin(l.y * rows * 3.14159265);
    rgb = rgb * (1.0 - param(4u) * s * 0.35 * sl);
    let n = hash12(in.uv * fx.resolution + vec2<f32>(fract(t * 7.13) * 1000.0, fract(t * 3.7) * 173.0)) - 0.5;
    rgb = rgb + vec3<f32>(n * 0.18 * param(2u) * s);
    return premul(rgb, c.a);
}
