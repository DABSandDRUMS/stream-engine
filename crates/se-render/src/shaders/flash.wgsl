// Flash detection input (§22): mean relative luminance (linear light) of an 8×8 grid of cells
// of the output canvas, written to a storage buffer read back by the CPU limiter.
@group(0) @binding(0) var canvas: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> cells: array<f32, 64>;

var<workgroup> partial: array<f32, 256>;

fn to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@compute @workgroup_size(16, 16)
fn main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_id) lid: vec3<u32>) {
    let dims = vec2<f32>(textureDimensions(canvas));
    let cell = dims / 8.0;
    var sum = 0.0;
    // each thread averages a 2×2 set of samples spread over its 1/16 × 1/16 of the cell
    for (var j = 0u; j < 2u; j = j + 1u) {
        for (var i = 0u; i < 2u; i = i + 1u) {
            let f = (vec2<f32>(lid.xy) * 2.0 + vec2<f32>(f32(i), f32(j)) + 0.5) / 32.0;
            let p = vec2<f32>(wg.xy) * cell + f * cell;
            let c = textureLoad(canvas, vec2<i32>(min(p, dims - 1.0)), 0).rgb;
            sum = sum + dot(to_linear(c), vec3<f32>(0.2126, 0.7152, 0.0722));
        }
    }
    let li = lid.y * 16u + lid.x;
    partial[li] = sum * 0.25;
    workgroupBarrier();
    var stride = 128u;
    loop {
        if stride == 0u {
            break;
        }
        if li < stride {
            partial[li] = partial[li] + partial[li + stride];
        }
        workgroupBarrier();
        stride = stride / 2u;
    }
    if li == 0u {
        cells[wg.y * 8u + wg.x] = partial[0] / 256.0;
    }
}
