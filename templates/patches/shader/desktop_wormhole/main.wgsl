// Desktop wormhole: the picture becomes a Windows 3.1 window that recedes into a tunnel of
// nested copies of itself (each a full window with chrome), twisting a little more per level
// and sliding toward `center` on a teal desktop; on release everything snaps back to full
// screen with a small overshoot. Driven by `se.trigger_age` (pull-in, flow) and `se.env`
// (release). Each pixel tests the windows innermost-first, so one texture fetch per pixel.
// Input and output are premultiplied alpha.

const WH_MAX: i32 = 10;
const WH_W: f32 = 480.0;          // window width in "design pixels"; the client is 470 wide

// 5x7 glyphs (row bits, MSB = left): D R U M S . E X
const WH_FONT = array<u32, 56>(
    30u, 17u, 17u, 17u, 17u, 17u, 30u,
    30u, 17u, 17u, 30u, 20u, 18u, 17u,
    17u, 17u, 17u, 17u, 17u, 17u, 14u,
    17u, 27u, 21u, 21u, 17u, 17u, 17u,
    15u, 16u, 16u, 14u, 1u, 1u, 30u,
    0u, 0u, 0u, 0u, 0u, 12u, 12u,
    31u, 16u, 16u, 30u, 16u, 16u, 31u,
    17u, 17u, 10u, 4u, 10u, 17u, 17u,
);
// "DRUMS.EXE"
const WH_TITLE = array<u32, 9>(0u, 1u, 2u, 3u, 4u, 5u, 6u, 7u, 6u);

fn wh_tap(uv: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(se_input, se_sampler, uv, 0.0);
}

fn wh_rot(v: vec2<f32>, a: f32) -> vec2<f32> {
    let c = cos(a);
    let s = sin(a);
    return vec2<f32>(c * v.x - s * v.y, s * v.x + c * v.y);
}

fn wh_title_text(p: vec2<f32>, w: f32) -> bool {
    let x0 = floor(w * 0.5 - 26.0);
    let lx = p.x - x0;
    let ly = p.y - 8.0;
    if (lx < 0.0 || ly < 0.0 || lx >= 54.0 || ly >= 7.0) {
        return false;
    }
    let cell = u32(lx / 6.0);
    let cx = u32(lx) - cell * 6u;
    if (cx >= 5u) {
        return false;
    }
    let row = WH_FONT[WH_TITLE[cell] * 7u + u32(ly)];
    return ((row >> (4u - cx)) & 1u) == 1u;
}

// Window chrome at design-pixel `p` (origin top-left of the window, size `sz`); `fw` = design
// pixels per screen pixel (thin lines widen and details drop out when the window is tiny).
// Returns rgb with a = 1, or a = 0 for the client area.
fn wh_chrome(p: vec2<f32>, sz: vec2<f32>, fw: f32) -> vec4<f32> {
    let lw = max(1.0, fw);
    let black = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    let gray = vec4<f32>(0.753, 0.753, 0.753, 1.0);
    let white = vec4<f32>(1.0);
    let tc = p_title_color();
    let title = vec4<f32>(tc.rgb, 1.0);
    if (p.x < lw || p.y < lw || p.x >= sz.x - lw || p.y >= sz.y - lw) { return black; }
    if (p.x < 4.0 || p.y < 4.0 || p.x >= sz.x - 4.0 || p.y >= sz.y - 4.0) { return gray; }
    if (p.x < 4.0 + lw || p.y < 4.0 + lw || p.x >= sz.x - 4.0 - lw || p.y >= sz.y - 4.0 - lw || (p.y >= 24.0 && p.y < 24.0 + lw)) { return black; }
    if (p.y < 24.0) {
        if (fw < 2.5) {
            let button = p.x < 24.0 || p.x >= sz.x - 43.0;
            if (button) {
                if ((p.x >= 23.0 && p.x < 23.0 + lw) || (p.x >= sz.x - 43.0 && p.x < sz.x - 43.0 + lw) || (p.x >= sz.x - 24.0 && p.x < sz.x - 24.0 + lw)) { return black; }
                if (p.x < 24.0) {
                    if (p.x >= 10.0 && p.x < 19.0 && p.y >= 13.0 && p.y < 14.0 + lw - 1.0) { return white; }
                    if (p.x >= 9.0 && p.x < 20.0 && p.y >= 12.0 && p.y < 16.0) { return black; }
                } else {
                    let maximize = p.x >= sz.x - 23.0;
                    let cx = select(sz.x - 33.0, sz.x - 14.0, maximize);
                    let y = select(p.y - 13.0, 16.0 - p.y, maximize);
                    if (y >= 0.0 && y < 4.0 && abs(p.x - cx) <= 3.0 - floor(y)) { return black; }
                }
                return gray;
            }
        }
        if (fw < 1.3 && wh_title_text(p, sz.x)) { return white; }
        return title;
    }
    return vec4<f32>(0.0);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let r0 = se.region.xy;
    let rs = max(se.region.zw - se.region.xy, vec2<f32>(1e-6));
    let q = (in.uv - r0) / rs;
    let env = clamp(se.env, 0.0, 1.0);
    let age = se.trigger_age;
    if (env <= 0.0 || age > 4.0 || p_amount() <= 0.0 || any(q < vec2<f32>(0.0)) || any(q > vec2<f32>(1.0))) {
        return wh_tap(in.uv);
    }
    let px_size = rs * se.resolution;
    let p = q * px_size;
    let boost = clamp(log(1.0 + max(se.trigger.amount, 0.0) / 100.0) / 2.4, 0.0, 1.0);

    // Pull-in: springy ease over the first ~0.6 s. Release: snap home fast, overshoot a bit.
    let pull = clamp(1.0 - exp(-age * 5.5) * (1.0 + age * 5.5), 0.0, 1.0);
    let ret = max(select(0.0, 1.0 - env, age > 0.15), clamp((age - 1.3) / 0.8, 0.0, 1.0));
    let snap = smoothstep(0.0, 0.5, ret);
    let over = 0.05 * sin(3.14159 * clamp((ret - 0.45) / 0.45, 0.0, 1.0));
    let amt = clamp(p_amount(), 0.0, 1.5);
    let kick = clamp(s_band_kick(), 0.0, 1.0);
    let depth = pull * (1.0 - snap) * amt;            // 0 = full screen, 1 = full tunnel
    let shrink = depth - over;                         // < 0 overshoots past full screen

    let n = clamp(p_copies(), 2, WH_MAX);
    let flow = age * p_flow();
    let z = fract(flow);
    let first_lap = flow < 1.0;
    let ratio = 0.74;
    let base_s = 0.8;
    let ctr = p_center() * px_size;
    let mid = 0.5 * px_size;
    let spin = p_spin() * (1.0 + 0.6 * boost);
    let aspect_h = 235.0 * px_size.y / px_size.x;     // client half height in design px
    let win = vec2<f32>(WH_W, 2.0 * aspect_h + 30.0);

    var hit = false;
    var out = vec4<f32>(0.0);
    var hit_uv = vec2<f32>(0.0);
    var hit_alpha = 1.0;
    var client = false;
    for (var k = WH_MAX - 1; k >= 0; k = k - 1) {
        if (k >= n) {
            continue;
        }
        let j = f32(k) + z;
        let sj = base_s * pow(ratio, j);
        let s = mix(1.0, sj, shrink) * (1.0 - 0.02 * kick * depth);
        let pullj = 1.0 - pow(ratio, j);
        let wob = vec2<f32>(cos(age * 1.3 + j * 0.9), sin(age * 1.1 + j * 0.7)) * 0.025 * px_size.y * pullj;
        let c = mix(mid, ctr + wob, shrink * pullj + shrink * 0.15);
        let th = (spin * j + 0.05 * sin(age * 2.2)) * shrink;
        let cw = s * px_size.x;
        let fw = 470.0 / max(cw, 1.0);
        let lp = wh_rot(p - c, -th) * fw;               // design px from the client center
        let wp = lp + vec2<f32>(240.0, aspect_h + 25.0);  // design px from the window corner
        if (wp.x < 0.0 || wp.y < 0.0 || wp.x >= win.x || wp.y >= win.y) {
            continue;
        }
        // windows pour in from the desktop: the outermost fades in once the flow has wrapped
        var a = 1.0;
        if (k == 0 && !first_lap) {
            a = smoothstep(0.0, 0.35, z);
        }
        let ch = wh_chrome(wp, win, fw);
        if (ch.a > 0.0) {
            // chrome fades in with the pull so full-screen frames never show a hard border
            let ca = smoothstep(0.0, 0.25, shrink) * a;
            out = vec4<f32>(ch.rgb, 1.0);
            hit_alpha = ca;
            client = false;
        } else {
            hit_uv = vec2<f32>(lp.x / 470.0 + 0.5, lp.y / (2.0 * aspect_h) + 0.5);
            hit_alpha = a;
            client = true;
        }
        hit = true;
        break;
    }

    // Desktop: teal with slow rings flowing into the tunnel.
    let dv = (p - ctr) / px_size.y;
    let dist = length(dv);
    let ring = 0.5 + 0.5 * sin(18.0 * log(max(dist, 0.004)) + atan2(dv.y, dv.x) * 2.0 + age * 6.0);
    let dc = p_desktop();
    let desk = vec4<f32>(dc.rgb * (0.82 + 0.18 * ring) * (0.55 + 0.45 * smoothstep(0.0, 0.35, dist)), 1.0);

    var res: vec4<f32>;
    if (!hit) {
        res = desk;
    } else {
        var front = out;
        if (client) {
            front = wh_tap(r0 + clamp(hit_uv, vec2<f32>(0.0), vec2<f32>(1.0)) * rs);
        }
        // behind a fading chrome/window: the full-screen picture early on, the desktop later
        let behind = mix(wh_tap(in.uv), desk, smoothstep(0.0, 0.3, shrink));
        res = mix(behind, front, hit_alpha);
    }
    return res;
}
