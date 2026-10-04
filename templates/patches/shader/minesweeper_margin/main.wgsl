// Minesweeper margin: a ring of classic Minesweeper tiles framing the screen edges (the
// middle stays untouched). An invisible player clicks on the beat: each click opens a cluster of
// safe tiles (numbers 1-8 in the classic colours) and flags the mines in it. Every 48 s the
// board re-covers in a sweep and a new one is dealt. A trigger sets off a mine cascade: a chain
// reaction runs along the ring from a random tile, blowing up every mine it reaches (red tiles,
// flash) and laying the board open; bigger payloads reach further (amount 100+ = whole ring).
// Then the ring heals back.
// Always drawn (no manifest trigger); trigger timing comes from se.trigger_age.

const CYCLE: f32 = 48.0;
const RECOVER: f32 = 4.0;
const MAX_CLICKS: i32 = 40;
// trigger cascade: wave speed (tiles/s), hold after the wave, heal sweep duration
const WAVE: f32 = 40.0;
const HOLD: f32 = 3.5;
const HEAL: f32 = 1.5;

// ---- generated sprites: digits 8x10, mine 13x13 (+glint), flag 10x10 (red, black) ----
const DIGITS: array<u32, 80> = array<u32, 80>(
    24u, 56u, 120u, 24u, 24u, 24u, 24u, 24u, 126u, 126u,
    126u, 195u, 3u, 3u, 14u, 56u, 96u, 192u, 255u, 255u,
    126u, 195u, 3u, 3u, 62u, 62u, 3u, 3u, 195u, 126u,
    14u, 30u, 54u, 102u, 198u, 255u, 255u, 6u, 6u, 6u,
    255u, 192u, 192u, 254u, 3u, 3u, 3u, 3u, 195u, 126u,
    126u, 195u, 192u, 192u, 254u, 195u, 195u, 195u, 195u, 126u,
    255u, 255u, 3u, 6u, 12u, 24u, 24u, 48u, 48u, 48u,
    126u, 195u, 195u, 195u, 126u, 126u, 195u, 195u, 195u, 126u,
);
const MINE_INK: array<u32, 13> = array<u32, 13>(
    64u, 64u, 1524u, 1016u, 2044u, 2044u, 8191u, 2044u, 2044u, 1016u, 1524u, 64u, 64u,
);
const MINE_GLINT: array<u32, 13> = array<u32, 13>(
    0u, 0u, 0u, 0u, 384u, 384u, 0u, 0u, 0u, 0u, 0u, 0u, 0u,
);
const FLAG_RED: array<u32, 10> = array<u32, 10>(
    96u, 480u, 992u, 480u, 96u, 0u, 0u, 0u, 0u, 0u,
);
const FLAG_BLACK: array<u32, 10> = array<u32, 10>(
    16u, 16u, 16u, 16u, 16u, 16u, 16u, 124u, 511u, 511u,
);
// ---- end of generated sprites ----

const C_GRAY: vec3<f32> = vec3<f32>(0.753, 0.753, 0.753);
const C_DARK: vec3<f32> = vec3<f32>(0.502, 0.502, 0.502);

fn hash3(a: i32, b: i32, c: u32) -> f32 {
    var x = u32(a) * 73856093u ^ u32(b) * 19349663u ^ c * 83492791u;
    x = x ^ (x >> 16u);
    x = x * 0x7feb352du;
    x = x ^ (x >> 15u);
    x = x * 0x846ca68bu;
    x = x ^ (x >> 16u);
    return f32(x) / 4294967295.0;
}

fn in_margin(c: vec2<i32>, n: vec2<i32>, k: i32) -> bool {
    if c.x < 0 || c.y < 0 || c.x >= n.x || c.y >= n.y {
        return false;
    }
    return c.x < k || c.y < k || c.x >= n.x - k || c.y >= n.y - k;
}

fn is_mine(c: vec2<i32>, gen: u32) -> bool {
    return hash3(c.x, c.y, gen * 7919u + 1u) < p_mines();
}

// Clockwise position along the outer ring (inner rows map onto their nearest edge).
fn ring_pos(c: vec2<i32>, n: vec2<i32>) -> f32 {
    let w1 = f32(n.x - 1);
    let h1 = f32(n.y - 1);
    let dl = c.x;
    let dr = n.x - 1 - c.x;
    let dt = c.y;
    let db = n.y - 1 - c.y;
    let m = min(min(dl, dr), min(dt, db));
    if dt == m {
        return f32(c.x);
    }
    if dr == m {
        return w1 + f32(c.y);
    }
    if db == m {
        return w1 + h1 + (w1 - f32(c.x));
    }
    return 2.0 * w1 + h1 + (h1 - f32(c.y));
}

fn ring_dist(a: f32, b: f32, per: f32) -> f32 {
    let d = abs(a - b);
    return min(d, per - d);
}

fn bit(row: u32, x: i32, w: i32) -> bool {
    return x >= 0 && x < w && ((row >> u32(w - 1 - x)) & 1u) == 1u;
}

fn digit_color(d: i32) -> vec3<f32> {
    switch d {
        case 1: { return vec3<f32>(0.0, 0.0, 1.0); }
        case 2: { return vec3<f32>(0.0, 0.502, 0.0); }
        case 3: { return vec3<f32>(1.0, 0.0, 0.0); }
        case 4: { return vec3<f32>(0.0, 0.0, 0.502); }
        case 5: { return vec3<f32>(0.502, 0.0, 0.0); }
        case 6: { return vec3<f32>(0.0, 0.502, 0.502); }
        case 7: { return vec3<f32>(0.0, 0.0, 0.0); }
        default: { return C_DARK; }
    }
}

// Raised (covered) tile at unit (lx, ly) of 16x16, optionally with a flag.
fn covered_tile(lx: i32, ly: i32, flag: bool) -> vec3<f32> {
    let hi = lx < 2 || ly < 2;
    let lo = lx > 13 || ly > 13;
    if hi && lo {
        return select(C_DARK, vec3<f32>(1.0), lx + ly < 15);
    }
    if hi {
        return vec3<f32>(1.0);
    }
    if lo {
        return C_DARK;
    }
    if flag {
        let fx = lx - 3;
        let fy = ly - 3;
        if fy >= 0 && fy < 10 {
            if bit(FLAG_RED[fy], fx, 10) {
                return vec3<f32>(1.0, 0.0, 0.0);
            }
            if bit(FLAG_BLACK[fy], fx, 10) {
                return vec3<f32>(0.0);
            }
        }
    }
    return C_GRAY;
}

// Opened tile: grid lines top/left, then a number (1-8), a mine, or nothing.
fn open_tile(lx: i32, ly: i32, count: i32, mine: bool, blown: bool) -> vec3<f32> {
    if lx == 0 || ly == 0 {
        return C_DARK;
    }
    let bg = select(C_GRAY, vec3<f32>(1.0, 0.0, 0.0), blown);
    if mine {
        let mx = lx - 2;
        let my = ly - 2;
        if my >= 0 && my < 13 {
            if bit(MINE_GLINT[my], mx, 13) {
                return vec3<f32>(1.0);
            }
            if bit(MINE_INK[my], mx, 13) {
                return vec3<f32>(0.0);
            }
        }
        return bg;
    }
    if count > 0 {
        let dy = ly - 3;
        if dy >= 0 && dy < 10 && bit(DIGITS[(count - 1) * 10 + dy], lx - 4, 8) {
            return digit_color(count);
        }
    }
    return bg;
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let res = se.resolution;
    let want = max(p_tile(), 0.005) * res.y;
    let nx = max(8, i32(round(res.x / want)));
    let ny = max(6, i32(round(res.y / (res.x / f32(nx)))));
    let n = vec2<i32>(nx, ny);
    let tsz = res / vec2<f32>(n);
    let k = clamp(p_thickness(), 1, 4);
    let g = in.uv * res / tsz;
    let c = vec2<i32>(floor(g));
    if !in_margin(c, n, k) {
        return vec4<f32>(0.0);
    }
    let l = vec2<i32>(floor(fract(g) * 16.0));
    let per = f32(2 * (n.x - 1) + 2 * (n.y - 1));
    let rp = ring_pos(c, n);
    let depth = f32(min(min(c.x, n.x - 1 - c.x), min(c.y, n.y - 1 - c.y)));

    // beat-quantised clock: board changes land on the beat (half-second grid without a tempo)
    let bpm = s_beat_bpm();
    let beat = select(0.5, 60.0 / max(bpm, 1.0), bpm > 1.0);
    let phase = select(fract(se.time / 0.5), clamp(s_beat_phase(), 0.0, 1.0), bpm > 1.0);
    // (offset so a fresh start already shows a half-played board)
    let tq = se.time - phase * beat + 0.45 * CYCLE;
    let genf = floor(tq / CYCLE);
    let gen = u32(i32(genf) & 0xffff);
    let tau = tq - genf * CYCLE;

    // the invisible player: clicks every `pace`-scaled interval open clusters along the ring
    let spacing = 1.6 / max(p_pace(), 0.1);
    var opened = 1.0e9;
    for (var i = 0; i < MAX_CLICKS; i = i + 1) {
        let ti = 1.0 + (f32(i) + 0.6 * hash3(i, 3, gen)) * spacing;
        if ti > tau || ti > CYCLE - RECOVER {
            break;
        }
        let centre = hash3(i, 1, gen) * per;
        let h = hash3(i, 2, gen);
        let radius = 1.0 + 7.0 * h * h + depth * 0.5;
        if ring_dist(rp, centre, per) <= radius {
            opened = ti;
            break;
        }
    }
    let mine = is_mine(c, gen);
    // the board re-covers in a clockwise sweep before the next deal
    let cover_at = CYCLE - RECOVER + (RECOVER - 0.6) * (rp / per);
    var revealed = opened <= tau && tau < cover_at && !mine;
    var flagged = opened <= tau && tau < cover_at && mine;
    var blown = false;
    var since = se.time - (genf * CYCLE + opened);
    var flash = 0.0;

    // trigger: mine cascade along the ring
    let age = se.trigger_age;
    if age < 30.0 {
        let tc = se.trigger_count;
        let origin = hash3(i32(tc), 9, 77u) * per;
        let dist = ring_dist(rp, origin, per) + depth * 0.7;
        let reach = per * 0.5 * clamp(0.5 + 0.5 * log2(1.0 + max(se.trigger.amount, 0.0) / 100.0), 0.5, 1.0);
        let td = dist / WAVE + 0.06 * hash3(c.x, c.y, tc);
        let back = reach / WAVE + HOLD + HEAL * (dist / reach);
        if dist <= reach && age >= td && age < back {
            revealed = true;
            flagged = false;
            blown = mine || dist < 0.6;
            since = age - td;
            flash = select(0.35, 1.0, blown) * exp(-since * 7.0);
        }
    }

    let count_mine = mine || blown;
    var col: vec3<f32>;
    if revealed {
        var count = 0;
        for (var dy = -1; dy <= 1; dy = dy + 1) {
            for (var dx = -1; dx <= 1; dx = dx + 1) {
                let nc = c + vec2<i32>(dx, dy);
                if (dx != 0 || dy != 0) && in_margin(nc, n, k) && is_mine(nc, gen) {
                    count = count + 1;
                }
            }
        }
        col = open_tile(l.x, l.y, count, count_mine, blown);
        // freshly opened tiles glow briefly, a little more on a kick
        let fresh = clamp(1.0 - since / 0.3, 0.0, 1.0);
        col = mix(col, vec3<f32>(1.0), fresh * (0.12 + 0.12 * clamp(s_band_kick(), 0.0, 1.0)));
    } else {
        col = covered_tile(l.x, l.y, flagged);
    }
    col = mix(col, vec3<f32>(1.0, 0.95, 0.6), clamp(flash, 0.0, 1.0) * 0.85);
    let a = clamp(p_amount(), 0.0, 1.0);
    return vec4<f32>(col * a, a);
}
