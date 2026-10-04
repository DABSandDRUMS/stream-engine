// Window boing: the layer becomes an elastic Windows 3.1 window. Kicks squash it down about its
// bottom centre and it springs back with a little overshoot; a trigger gives one big wobbly boing
// (bigger payloads boing harder). The classic chrome (black-outlined gray sizing border with
// corner notches, navy title bar with caption, system-menu box, beveled minimize/maximize
// buttons) is drawn inside se.region, and the whole input is fitted into the client area, so the
// content is never cropped even at full stretch.

// 5x7 caption font: D r u m C a (bit 4 = leftmost column)
const CAPTION_FONT: array<u32, 42> = array<u32, 42>(
    30u, 17u, 17u, 17u, 17u, 17u, 30u,
    0u, 0u, 22u, 25u, 16u, 16u, 16u,
    0u, 0u, 17u, 17u, 17u, 19u, 13u,
    0u, 0u, 26u, 21u, 21u, 21u, 21u,
    14u, 17u, 16u, 16u, 16u, 17u, 14u,
    0u, 0u, 14u, 1u, 15u, 17u, 15u,
);
// "Drum Cam": glyph indices, 6 = space
const CAPTION: array<u32, 8> = array<u32, 8>(0u, 1u, 2u, 3u, 6u, 4u, 5u, 3u);
const CAPTION_LEN: i32 = 8;

const GRAY: vec3<f32> = vec3<f32>(0.753, 0.753, 0.753);
const DARK: vec3<f32> = vec3<f32>(0.502, 0.502, 0.502);

// Squash amount from an exponentially decaying hit envelope: map it back to "time since the
// hit" and run a damped spring there (0 at the hit, squash peak, smaller stretch rebound).
fn spring_from_env(k: f32) -> f32 {
    if k < 0.003 {
        return 0.0;
    }
    let u = -log(min(k, 1.0));
    return sin(1.8 * u) * exp(-0.45 * u) / 0.7;
}

// Big trigger boing: squash first, then a few decaying stretch/squash swings.
fn spring_from_age(t: f32) -> f32 {
    if t > 4.0 {
        return 0.0;
    }
    return sin(6.2832 * 2.1 * t) * exp(-2.4 * t) * smoothstep(0.0, 0.03, t);
}

// Chrome colour at window pixel q (window size wsz, both in window pixels); alpha 0 = client.
fn chrome(q: vec2<f32>, wsz: vec2<f32>) -> vec4<f32> {
    let black = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    let white = vec4<f32>(1.0);
    let gray = vec4<f32>(GRAY, 1.0);
    let dark = vec4<f32>(DARK, 1.0);
    let x = floor(q.x);
    let y = floor(q.y);
    let rx = floor(wsz.x - q.x);
    let ry = floor(wsz.y - q.y);
    let d = min(min(x, rx), min(y, ry));
    if d < 1.0 {
        return black;
    }
    if d < 4.0 {
        // sizing border with the corner notches 24 px in from each corner
        let on_tb = y < 4.0 || ry < 4.0;
        let on_lr = x < 4.0 || rx < 4.0;
        if (on_tb && (x == 23.0 || rx == 23.0)) || (on_lr && (y == 23.0 || ry == 23.0)) {
            return black;
        }
        return gray;
    }
    if d < 5.0 || y == 24.0 {
        return black;
    }
    if y < 24.0 {
        let ty = y - 5.0;
        // system menu box
        if x < 23.0 {
            if x >= 9.0 && x < 20.0 && ty >= 7.0 && ty < 10.0 {
                if x >= 10.0 && x < 19.0 && ty == 8.0 {
                    return white;
                }
                return select(dark, black, x >= 19.0 || ty >= 9.0);
            }
            return gray;
        }
        if x == 23.0 {
            return black;
        }
        // minimize / maximize: raised buttons with triangles
        if rx < 43.0 {
            if rx == 42.0 || rx == 23.0 {
                return black;
            }
            let maximize = rx < 23.0;
            let bx = select(x - (wsz.x - 43.0) - 1.0, x - (wsz.x - 23.0) - 1.0, maximize);
            let bw = 18.0;
            if bx < 1.0 || ty < 1.0 {
                return white;
            }
            if bx >= bw - 2.0 || ty >= 17.0 {
                return dark;
            }
            let cx = bx - 8.5;
            let row = select(ty - 7.0, 11.0 - ty, maximize);
            if row >= 0.0 && row < 4.0 && abs(cx) <= 3.5 - row {
                return black;
            }
            return gray;
        }
        // title bar and its centred caption (bold: every lit column also lights its right neighbour)
        let tc = p_title_color();
        let title = vec4<f32>(tc.rgb, 1.0);
        let cw = 7.0;
        let left = floor((wsz.x - f32(CAPTION_LEN) * cw) * 0.5);
        let gx = x - left;
        let gy = ty - 5.0;
        if gx >= 0.0 && gx < f32(CAPTION_LEN) * cw && gy >= 0.0 && gy < 7.0 {
            let ci = i32(gx / cw);
            let col = i32(gx) - ci * 7;
            let glyph = CAPTION[ci];
            if glyph < 6u && col < 6 {
                let bits = CAPTION_FONT[glyph * 7u + u32(gy)];
                let a = col < 5 && ((bits >> u32(4 - col)) & 1u) == 1u;
                let b = col > 0 && ((bits >> u32(5 - col)) & 1u) == 1u;
                if a || b {
                    return white;
                }
            }
        }
        return title;
    }
    return vec4<f32>(0.0);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let res = se.resolution;
    let r0 = se.region.xy * res;
    let rs = max((se.region.zw - se.region.xy) * res, vec2<f32>(1.0));
    let p = in.uv * res;
    let rel = p - r0;
    if any(rel < vec2<f32>(0.0)) || any(rel >= rs) {
        return textureSampleLevel(se_input, se_sampler, in.uv, 0.0);
    }

    // spring: kicks give a small squash, the trigger a big boing
    let base = max(p_amount(), 0.0) * max(p_squash(), 0.0);
    let boost = 1.0 + min(log2(1.0 + max(se.trigger.amount, 0.0) / 100.0), 1.5) / 1.5;
    let reach = base * 2.4;
    let s = clamp(base * (spring_from_env(s_band_kick()) + 1.1 * boost * spring_from_age(se.trigger_age)), -reach, reach);
    let sc = vec2<f32>(1.0 + s, 1.0 - s);

    // window layout in screen pixels: chrome pixels are `px` screen pixels; the client keeps the
    // region's aspect and the window leaves room for the widest squash and tallest stretch
    let chrome_on = p_chrome();
    let px = select(0.0, max(1.0, round(rs.x / 640.0)), chrome_on);
    let pad = vec2<f32>(10.0, 30.0) * px;
    let room = rs / (1.0 + reach);
    let aspect = rs.x / rs.y;
    let client_h = min((room.x - pad.x) / aspect, room.y - pad.y);
    let client = vec2<f32>(client_h * aspect, client_h);
    let win = client + pad;

    // undo the squash about the window's bottom centre (which rests on the region's bottom)
    let w = vec2<f32>((rel.x - rs.x * 0.5) / sc.x + win.x * 0.5, win.y - (rs.y - rel.y) / sc.y);
    if any(w < vec2<f32>(0.0)) || any(w >= win) {
        return vec4<f32>(0.0);
    }
    if chrome_on {
        let c = chrome(w / px, win / px);
        if c.a > 0.0 {
            return c;
        }
    }
    let cuv = (w - vec2<f32>(5.0, 25.0) * px) / client;
    let uv = se.region.xy + clamp(cuv, vec2<f32>(0.0), vec2<f32>(1.0)) * (se.region.zw - se.region.xy);
    return textureSampleLevel(se_input, se_sampler, uv, 0.0);
}
