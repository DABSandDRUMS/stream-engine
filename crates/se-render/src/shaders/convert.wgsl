// Source conversion: packed/planar camera formats → premultiplied RGBA at the source size, with
// per-source color correction and an optional 3D LUT (§4.2 "Color").
struct Convert {
    mode: u32,          // 0 RGBA/BGRA straight, 1 RGBA/BGRA premultiplied, 2 YUYV, 3 NV12
    matrix: u32,        // 0 BT.601, 1 BT.709
    full_range: u32,
    lut_on: u32,
    brightness: f32,
    contrast: f32,
    saturation: f32,
    gamma: f32,
    temperature: f32,
    tint: f32,
    lut_amount: f32,
    width: f32,         // output width in px (YUYV pixel parity)
};

@group(0) @binding(0) var<uniform> cv: Convert;
@group(0) @binding(1) var plane0: texture_2d<f32>;
@group(0) @binding(2) var plane1: texture_2d<f32>;
@group(0) @binding(3) var lut: texture_3d<f32>;
@group(0) @binding(4) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VsOut;
    o.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    o.uv = vec2<f32>(p.x, 1.0 - p.y);
    return o;
}

fn yuv_to_rgb(y_in: f32, u_in: f32, v_in: f32) -> vec3<f32> {
    var y = y_in;
    var cb = u_in - 0.5;
    var cr = v_in - 0.5;
    if cv.full_range == 0u {
        y = (y_in - 16.0 / 255.0) * (255.0 / 219.0);
        cb = (u_in - 128.0 / 255.0) * (255.0 / 224.0);
        cr = (v_in - 128.0 / 255.0) * (255.0 / 224.0);
    }
    if cv.matrix == 1u {
        return vec3<f32>(y + 1.5748 * cr, y - 0.187324 * cb - 0.468124 * cr, y + 1.8556 * cb);
    }
    return vec3<f32>(y + 1.402 * cr, y - 0.344136 * cb - 0.714136 * cr, y + 1.772 * cb);
}

fn correct(rgb_in: vec3<f32>) -> vec3<f32> {
    var rgb = clamp(rgb_in, vec3<f32>(0.0), vec3<f32>(1.0));
    rgb = rgb * vec3<f32>(1.0 + 0.15 * cv.temperature + 0.05 * cv.tint, 1.0 - 0.1 * cv.tint, 1.0 - 0.15 * cv.temperature + 0.05 * cv.tint);
    rgb = rgb + vec3<f32>(cv.brightness);
    rgb = (rgb - 0.5) * cv.contrast + 0.5;
    let l = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    rgb = mix(vec3<f32>(l), rgb, cv.saturation);
    rgb = pow(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / max(cv.gamma, 0.01)));
    if cv.lut_on == 1u {
        let n = f32(textureDimensions(lut).x);
        let coord = rgb * ((n - 1.0) / n) + 0.5 / n;
        rgb = mix(rgb, textureSampleLevel(lut, samp, coord, 0.0).rgb, cv.lut_amount);
    }
    return clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let px = vec2<i32>(in.pos.xy);
    var rgb: vec3<f32>;
    var a = 1.0;
    switch cv.mode {
        case 2u: {
            // YUYV: one RGBA8 texel = Y0 U Y1 V (two pixels); chroma interpolated for odd pixels
            let dims = vec2<i32>(textureDimensions(plane0));
            let tx = clamp(px.x / 2, 0, dims.x - 1);
            let t = textureLoad(plane0, vec2<i32>(tx, px.y), 0);
            var y = t.x;
            var uv = vec2<f32>(t.y, t.w);
            if (px.x & 1) == 1 {
                y = t.z;
                let n = textureLoad(plane0, vec2<i32>(min(tx + 1, dims.x - 1), px.y), 0);
                uv = (uv + vec2<f32>(n.y, n.w)) * 0.5;
            }
            rgb = yuv_to_rgb(y, uv.x, uv.y);
        }
        case 3u: {
            let y = textureLoad(plane0, px, 0).x;
            let c = textureSampleLevel(plane1, samp, in.uv, 0.0).xy;
            rgb = yuv_to_rgb(y, c.x, c.y);
        }
        default: {
            let c = textureLoad(plane0, px, 0);
            a = c.a;
            if cv.mode == 1u {
                rgb = select(vec3<f32>(0.0), c.rgb / c.a, c.a > 1e-5);
            } else {
                rgb = c.rgb;
            }
        }
    }
    return vec4<f32>(correct(rgb) * a, a);
}
