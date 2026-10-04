// Window cascade: Window > Cascade, Windows 3.1 style. On the trigger the full picture shrinks
// into a window, then more windows (each a live copy of the whole frame, with sizing border,
// caption, system menu and min/max buttons) pop in one after another down a diagonal cascade,
// each new one active (title in `title_color`, older ones inactive white). A checkered zoom
// rectangle slides out from the previous window before each pop. On release the windows close
// from the front back, and the first one maximizes back to the full picture.
// Timed by se.trigger_age (pop-in) and the release (1 - env). Each pixel tests the windows
// front to back, one texture fetch per pixel. Input and output are premultiplied alpha.

const WC_MAX: i32 = 12;
const WC_PAD = vec2<f32>(10.0, 30.0);   // chrome around the client, in chrome pixels (5 + 5, 25 + 5)
const WC_ORIGIN = vec2<f32>(5.0, 25.0);

const WC_GRAY = vec3<f32>(0.753, 0.753, 0.753);
const WC_DARK = vec3<f32>(0.502, 0.502, 0.502);

// 5x7 caps font for the captions (row bits, bit 4 = leftmost): 0-9, D R U M C A, ':'
const WC_FONT = array<u32, 119>(
    14u, 17u, 19u, 21u, 25u, 17u, 14u,
    4u, 12u, 4u, 4u, 4u, 4u, 14u,
    14u, 17u, 1u, 2u, 4u, 8u, 31u,
    31u, 2u, 4u, 2u, 1u, 17u, 14u,
    2u, 6u, 10u, 18u, 31u, 2u, 2u,
    31u, 16u, 30u, 1u, 1u, 17u, 14u,
    6u, 8u, 16u, 30u, 17u, 17u, 14u,
    31u, 1u, 2u, 4u, 8u, 8u, 8u,
    14u, 17u, 17u, 14u, 17u, 17u, 14u,
    14u, 17u, 17u, 15u, 1u, 2u, 12u,
    30u, 17u, 17u, 17u, 17u, 17u, 30u,
    30u, 17u, 17u, 30u, 20u, 18u, 17u,
    17u, 17u, 17u, 17u, 17u, 17u, 14u,
    17u, 27u, 21u, 21u, 17u, 17u, 17u,
    14u, 17u, 16u, 16u, 16u, 17u, 14u,
    14u, 17u, 17u, 31u, 17u, 17u, 17u,
    0u, 12u, 12u, 0u, 12u, 12u, 0u,
);
// "DRUM CAM:" as glyph indices (99 = space); the window number follows
const WC_TITLE = array<u32, 9>(10u, 11u, 12u, 13u, 99u, 14u, 15u, 13u, 16u);

fn wc_tap(uv: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(se_input, se_sampler, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
}

fn wc_ease(t: f32) -> f32 {
    let x = clamp(t, 0.0, 1.0);
    return x * x * (3.0 - 2.0 * x);
}

// Glyph of caption character c (len = 9 + digits of n).
fn wc_caption_glyph(c: i32, n: i32) -> u32 {
    if (c < 9) {
        return WC_TITLE[c];
    }
    if (n >= 10 && c == 9) {
        return u32(n / 10);
    }
    return u32(n % 10);
}

// Bold 5x7 caption text lit at (gx, gy) relative to its top-left; advance 7 per character.
fn wc_caption(gx: f32, gy: f32, n: i32, len: i32) -> bool {
    if (gx < 0.0 || gy < 0.0 || gy >= 7.0 || gx >= f32(len) * 7.0) {
        return false;
    }
    let ci = i32(gx / 7.0);
    let col = i32(gx) - ci * 7;
    let glyph = wc_caption_glyph(ci, n);
    if (glyph == 99u || col > 5) {
        return false;
    }
    let bits = WC_FONT[glyph * 7u + u32(gy)];
    let a = col < 5 && ((bits >> u32(4 - col)) & 1u) == 1u;
    let b = col > 0 && ((bits >> u32(5 - col)) & 1u) == 1u;
    return a || b;
}

// Chrome colour at window pixel q (window size wsz, both in chrome pixels); alpha 0 = client.
fn wc_chrome(q: vec2<f32>, wsz: vec2<f32>, focused: bool, flash: bool, n: i32) -> vec4<f32> {
    let black = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    let white = vec4<f32>(1.0);
    let gray = vec4<f32>(WC_GRAY, 1.0);
    let dark = vec4<f32>(WC_DARK, 1.0);
    let x = floor(q.x);
    let y = floor(q.y);
    let rx = floor(wsz.x - q.x);
    let ry = floor(wsz.y - q.y);
    let d = min(min(x, rx), min(y, ry));
    if (d < 1.0) {
        return black;
    }
    if (d < 4.0) {
        // sizing border with the corner notches
        let on_tb = y < 4.0 || ry < 4.0;
        let on_lr = x < 4.0 || rx < 4.0;
        if ((on_tb && (x == 23.0 || rx == 23.0)) || (on_lr && (y == 23.0 || ry == 23.0))) {
            return black;
        }
        return gray;
    }
    if (d < 5.0 || y == 24.0) {
        return black;
    }
    if (y < 24.0) {
        let ty = y - 5.0;
        if (x < 23.0) {
            // system menu box
            if (x >= 9.0 && x < 20.0 && ty >= 7.0 && ty < 10.0) {
                if (x >= 10.0 && x < 19.0 && ty == 8.0) {
                    return white;
                }
                return select(dark, black, x >= 19.0 || ty >= 9.0);
            }
            return gray;
        }
        if (x == 23.0) {
            return black;
        }
        if (rx < 43.0) {
            // minimize / maximize buttons
            if (rx == 42.0 || rx == 23.0) {
                return black;
            }
            let maximize = rx < 23.0;
            let bx = select(x - (wsz.x - 43.0) - 1.0, x - (wsz.x - 23.0) - 1.0, maximize);
            if (bx < 1.0 || ty < 1.0) {
                return white;
            }
            if (bx >= 16.0 || ty >= 17.0) {
                return dark;
            }
            let cx = bx - 8.5;
            let row = select(ty - 7.0, 11.0 - ty, maximize);
            if (row >= 0.0 && row < 4.0 && abs(cx) <= 3.5 - row) {
                return black;
            }
            return gray;
        }
        // caption bar with the centred title
        let tc = p_title_color().rgb;
        var bar = select(vec3<f32>(1.0), tc, focused);
        let lum = dot(tc, vec3<f32>(0.299, 0.587, 0.114));
        var ink = select(vec3<f32>(0.0), select(vec3<f32>(0.0), vec3<f32>(1.0), lum < 0.5), focused);
        if (flash) {
            let t = bar;
            bar = ink;
            ink = t;
        }
        let len = select(10, 11, n >= 10);
        let left = floor(23.0 + (wsz.x - 66.0 - f32(len) * 7.0) * 0.5);
        if (wc_caption(x - left, ty - 6.0, n, len)) {
            return vec4<f32>(ink, 1.0);
        }
        return vec4<f32>(bar, 1.0);
    }
    return vec4<f32>(0.0);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let src = wc_tap(in.uv);
    let env = clamp(se.env, 0.0, 1.0);
    let age = se.trigger_age;
    if (env <= 0.0 || p_amount() <= 0.0) {
        return src;
    }
    let res = se.resolution;
    let px = in.uv * res;
    let cp = max(1.0, round(res.y / 720.0));      // screen pixels per chrome pixel

    // how many windows: the param, plus up to 3 for a big trigger payload
    let extra = i32(round(clamp(log2(1.0 + max(se.trigger.amount, 0.0) / 100.0), 0.0, 3.0)));
    let n = clamp(p_windows() + extra, 2, WC_MAX);
    let nf = f32(n);

    // cascade layout: equal windows stepped by one caption height, the whole stack centred
    let margin = round(res * 0.03 / cp) * cp;
    let step = cp * min(24.0, floor((res.y * 0.5 - margin.y) / (cp * max(nf - 1.0, 1.0))));
    let avail = res - 2.0 * margin - vec2<f32>((nf - 1.0) * step);
    let aspect = res.x / res.y;
    let ch = floor(min(avail.y / cp - WC_PAD.y, (avail.x / cp - WC_PAD.x) / aspect) * clamp(p_size(), 0.4, 1.0));
    let wsz = vec2<f32>(floor(ch * aspect), ch) + WC_PAD;   // window size in chrome pixels
    let stack = wsz * cp + vec2<f32>((nf - 1.0) * step);
    let origin = floor((res - stack) * 0.5 / cp) * cp;

    // timeline: window 0 shrinks out of the full frame, the rest pop in one by one
    let shrink_t = 0.22;
    let gap = min(0.09, 0.7 / max(nf - 1.0, 1.0));
    let zoom_t = 0.05;
    let releasing = env < 1.0 && age > 0.15;
    let ret = select(0.0, 1.0 - env, releasing);
    let full = (1.0 - wc_ease(age / shrink_t)) + wc_ease((ret - 0.6) / 0.35);
    let snare_flash = p_flash() && s_band_snare() > 0.6;

    // the topmost open window is the active one
    var top = 0;
    for (var i = 1; i < n; i = i + 1) {
        let t_i = shrink_t + f32(i - 1) * gap;
        let closed = ret > 0.5 * f32(n - 1 - i) / max(nf - 1.0, 1.0) + 0.02;
        if (age >= t_i + zoom_t && !(releasing && closed)) {
            top = i;
        }
    }

    for (var i = n - 1; i >= 0; i = i - 1) {
        var lo = origin + vec2<f32>(f32(i) * step);
        var size = wsz * cp;
        var shown = true;
        if (i == 0) {
            // client rect blends between the full frame and its window slot
            let f = clamp(full, 0.0, 1.0);
            let c_lo = mix(lo + WC_ORIGIN * cp, vec2<f32>(0.0), f);
            let c_size = mix(size - WC_PAD * cp, res, f);
            lo = c_lo - WC_ORIGIN * cp;
            size = c_size + WC_PAD * cp;
        } else {
            let t_i = shrink_t + f32(i - 1) * gap;
            let closed = releasing && ret > 0.5 * f32(n - 1 - i) / max(nf - 1.0, 1.0) + 0.02;
            if (closed || age < t_i) {
                continue;
            }
            if (age < t_i + zoom_t) {
                // checkered zoom rectangle sliding out of the previous window
                let z = wc_ease((age - t_i) / zoom_t);
                let zlo = lo - vec2<f32>(step * (1.0 - z));
                let rel = (px - zlo) / cp;
                let rsz = wsz;
                let dd = min(min(rel.x, rsz.x - rel.x), min(rel.y, rsz.y - rel.y));
                if (dd >= 0.0 && dd < 3.0) {
                    let ck = (i32(floor(px.x / (2.0 * cp))) + i32(floor(px.y / (2.0 * cp)))) & 1;
                    return vec4<f32>(vec3<f32>(f32(ck)) * src.a, src.a);
                }
                shown = false;
            }
        }
        if (!shown) {
            continue;
        }
        let rel = px - lo;
        if (any(rel < vec2<f32>(0.0)) || any(rel >= size)) {
            continue;
        }
        let q = rel / cp;
        let wq = size / cp;
        let chrome = wc_chrome(q, wq, i == top, i == top && snare_flash, i + 1);
        if (chrome.a > 0.0) {
            return vec4<f32>(chrome.rgb * src.a, src.a);
        }
        let cuv = (rel - WC_ORIGIN * cp) / (size - WC_PAD * cp);
        return wc_tap(cuv);
    }
    let desk = p_desktop().rgb;
    return vec4<f32>(desk * src.a, src.a);
}
