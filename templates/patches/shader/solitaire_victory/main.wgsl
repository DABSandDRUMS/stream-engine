// Solitaire victory: the Windows Solitaire win cascade. On a trigger the four foundation piles
// appear top right and launch their cards one after another (kings first); each card falls under
// gravity, bounces along the bottom edge losing energy, and leaves the iconic trail of stamped
// copies. No frame history: every pixel re-traces each card's deterministic trajectory and finds
// the newest stamp covering it (stamps every 1/trail s, newest first, bounded per card).
// Cards are 33x45 pixel-art sprites drawn with integer-scaled cells.

const GW: i32 = 33;
const GH: i32 = 45;
// seconds between launches
const LAUNCH_EVERY: f32 = 0.18;
// stamps tested per card and pixel (newest first)
const MAX_STAMPS: i32 = 16;

// ---- generated bitmaps (rank font 5x7, pips 7x7 / 5x5, court sprites 17x16 @ 4 bpp) ----
const RANK_FONT: array<u32, 91> = array<u32, 91>(
    14u, 17u, 17u, 31u, 17u, 17u, 17u, 14u,
    17u, 1u, 2u, 4u, 8u, 31u, 30u, 1u,
    1u, 14u, 1u, 1u, 30u, 2u, 6u, 10u,
    18u, 31u, 2u, 2u, 31u, 16u, 30u, 1u,
    1u, 17u, 14u, 14u, 16u, 16u, 30u, 17u,
    17u, 14u, 31u, 1u, 2u, 4u, 8u, 8u,
    8u, 14u, 17u, 17u, 14u, 17u, 17u, 14u,
    14u, 17u, 17u, 15u, 1u, 1u, 14u, 23u,
    21u, 21u, 21u, 21u, 21u, 23u, 7u, 2u,
    2u, 2u, 2u, 18u, 12u, 14u, 17u, 17u,
    17u, 21u, 18u, 13u, 17u, 18u, 20u, 24u,
    20u, 18u, 17u,
);
const PIP_BIG: array<u32, 28> = array<u32, 28>(
    8u, 28u, 62u, 127u, 127u, 42u, 28u,
    54u, 127u, 127u, 127u, 62u, 28u, 8u,
    28u, 28u, 107u, 127u, 107u, 8u, 28u,
    8u, 28u, 62u, 127u, 62u, 28u, 8u,
);
const PIP_SMALL: array<u32, 20> = array<u32, 20>(
    4u, 14u, 31u, 21u, 14u,
    27u, 31u, 31u, 14u, 4u,
    14u, 14u, 31u, 21u, 14u,
    4u, 14u, 31u, 14u, 4u,
);
const COURT: array<u32, 144> = array<u32, 144>(
    0x11000000u, 0x00000111u, 0x00000000u, 0x44100000u, 0x00301444u, 0x00000000u,
    0x44410000u, 0x03314444u, 0x00000000u, 0x11111000u, 0x03111111u, 0x00000000u,
    0x22210000u, 0x03012222u, 0x00000000u, 0x66210000u, 0x00012666u, 0x00000000u,
    0x61610000u, 0x00016166u, 0x00000000u, 0x66610000u, 0x00116661u, 0x00000000u,
    0x66610000u, 0x00016611u, 0x00000000u, 0x66100000u, 0x00011666u, 0x00000000u,
    0x11211000u, 0x00112111u, 0x00000000u, 0x52255100u, 0x01552255u, 0x00000000u,
    0x32235510u, 0x15532234u, 0x00000000u, 0x55225510u, 0x15522554u, 0x00000000u,
    0x55255510u, 0x15552553u, 0x00000000u, 0x52555510u, 0x15555255u, 0x00000000u,
    0x02000000u, 0x00000202u, 0x00000000u, 0x23200000u, 0x00002322u, 0x00000000u,
    0x24200000u, 0x00002423u, 0x00000000u, 0x11110000u, 0x00011111u, 0x00000000u,
    0x66122000u, 0x00221666u, 0x00000000u, 0x61612000u, 0x00216166u, 0x00000000u,
    0x66612000u, 0x03216661u, 0x00000000u, 0x66612000u, 0x33326666u, 0x00000000u,
    0x66122000u, 0x03021661u, 0x00000000u, 0x66122000u, 0x00221666u, 0x00000000u,
    0x11122200u, 0x02221111u, 0x00000000u, 0x33255100u, 0x01552332u, 0x00000000u,
    0x22555510u, 0x15555222u, 0x00000000u, 0x55335510u, 0x15335555u, 0x00000000u,
    0x55455510u, 0x15554552u, 0x00000000u, 0x55555510u, 0x15555552u, 0x00000000u,
    0x02020000u, 0x00020202u, 0x00000000u, 0x22220000u, 0x00022222u, 0x00000000u,
    0x22320000u, 0x00023224u, 0x00000000u, 0x22220000u, 0x00022222u, 0x00000000u,
    0x11110000u, 0x00011111u, 0x00000000u, 0x61610000u, 0x00116166u, 0x00000000u,
    0x66610000u, 0x02016666u, 0x00000000u, 0x66610000u, 0x02016661u, 0x00000000u,
    0x11110000u, 0x02011111u, 0x00000000u, 0x11111000u, 0x02111111u, 0x00000000u,
    0x11155100u, 0x12551111u, 0x00000000u, 0x15525510u, 0x12525511u, 0x00000000u,
    0x55222510u, 0x12222553u, 0x00000000u, 0x35525510u, 0x15525532u, 0x00000000u,
    0x55555510u, 0x15555553u, 0x00000000u, 0x55555510u, 0x15555555u, 0x00000000u,
);
const PIP_POS: array<vec2<i32>, 54> = array<vec2<i32>, 54>(
    vec2<i32>(16, 6), vec2<i32>(16, 38), vec2<i32>(16, 6), vec2<i32>(16, 22), vec2<i32>(16, 38),
    vec2<i32>(11, 6), vec2<i32>(21, 6), vec2<i32>(11, 38), vec2<i32>(21, 38), vec2<i32>(11, 6),
    vec2<i32>(21, 6), vec2<i32>(16, 22), vec2<i32>(11, 38), vec2<i32>(21, 38), vec2<i32>(11, 6),
    vec2<i32>(21, 6), vec2<i32>(11, 22), vec2<i32>(21, 22), vec2<i32>(11, 38), vec2<i32>(21, 38),
    vec2<i32>(11, 6), vec2<i32>(21, 6), vec2<i32>(16, 14), vec2<i32>(11, 22), vec2<i32>(21, 22),
    vec2<i32>(11, 38), vec2<i32>(21, 38), vec2<i32>(11, 6), vec2<i32>(21, 6), vec2<i32>(16, 14),
    vec2<i32>(11, 22), vec2<i32>(21, 22), vec2<i32>(16, 30), vec2<i32>(11, 38), vec2<i32>(21, 38),
    vec2<i32>(11, 6), vec2<i32>(21, 6), vec2<i32>(11, 17), vec2<i32>(21, 17), vec2<i32>(16, 22),
    vec2<i32>(11, 27), vec2<i32>(21, 27), vec2<i32>(11, 38), vec2<i32>(21, 38), vec2<i32>(11, 6),
    vec2<i32>(21, 6), vec2<i32>(16, 11), vec2<i32>(11, 17), vec2<i32>(21, 17), vec2<i32>(11, 27),
    vec2<i32>(21, 27), vec2<i32>(16, 33), vec2<i32>(11, 38), vec2<i32>(21, 38),
);
// ---- end of generated bitmaps ----

fn hash11(n: u32) -> f32 {
    var x = n;
    x = x ^ (x >> 16u);
    x = x * 0x7feb352du;
    x = x ^ (x >> 15u);
    x = x * 0x846ca68bu;
    x = x ^ (x >> 16u);
    return f32(x) / 4294967295.0;
}

struct Card {
    x0: f32,
    y0: f32,
    vx: f32,
    vy0: f32,
    // first floor contact time and downward speed there
    t1: f32,
    v1: f32,
};

// Seconds since the cascade started.
fn cascade_age() -> f32 {
    return se.trigger_age;
}

// Size of one sprite cell in screen pixels (integer for crisp pixel art).
fn cell_px() -> f32 {
    return max(1.0, round(p_size() * se.resolution.y / f32(GH)));
}

// Cards in this cascade: `cards`, more for bigger trigger payloads.
fn card_count() -> u32 {
    let boost = 1.0 + 0.6 * log2(1.0 + max(se.trigger.amount, 0.0) / 100.0);
    return u32(clamp(round(p_cards() * boost), 1.0, 52.0));
}

// Foundation (0 = leftmost of the four) a card launches from; the rightmost pile goes first.
fn foundation(c: u32) -> u32 {
    return 3u - (c % 4u);
}

fn pile_pos(f: u32, cw: f32) -> vec2<f32> {
    let gap = round(cw * 0.22);
    let margin = round(cw * 0.45);
    let x = se.resolution.x - margin - f32(4u - f) * cw - f32(3u - f) * gap;
    return vec2<f32>(x, round(cw * 0.35));
}

fn card_at(c: u32, cw: f32, ch: f32, g: f32) -> Card {
    let h = se.resolution.y;
    let seed = c * 747796405u + se.trigger_count * 2891336453u + 17u;
    var k: Card;
    let p = pile_pos(foundation(c), cw);
    k.x0 = p.x;
    k.y0 = p.y;
    // mostly to the left (the piles sit top right), now and then off to the right
    let dir = select(1.0, -1.0, hash11(seed) < 0.8);
    k.vx = dir * (0.45 + 0.65 * hash11(seed + 1u)) * h;
    k.vy0 = -(0.1 + 0.9 * hash11(seed + 2u)) * 0.85 * h;
    let floor_y = h - ch;
    k.t1 = (-k.vy0 + sqrt(k.vy0 * k.vy0 + 2.0 * g * max(floor_y - k.y0, 0.0))) / g;
    k.v1 = k.vy0 + g * k.t1;
    return k;
}

// Top edge of the card at time t: a parabola until the floor, then rebounds that keep a
// `bounce` share of the speed each time (closed form), finally sliding along the floor.
fn y_at(k: Card, t: f32, g: f32, e: f32, floor_y: f32) -> f32 {
    if t <= k.t1 {
        return k.y0 + k.vy0 * t + 0.5 * g * t * t;
    }
    let s = t - k.t1;
    let unit = 2.0 * k.v1 / g;
    let z = s * (1.0 - e) / (unit * e);
    if z >= 1.0 {
        return floor_y;
    }
    let n = max(ceil(log(1.0 - z) / log(e)), 1.0);
    let before = unit * e * (1.0 - pow(e, n - 1.0)) / (1.0 - e);
    let v = pow(e, n) * k.v1;
    let sl = s - before;
    return min(floor_y, floor_y - v * sl + 0.5 * g * sl * sl);
}

fn rank_of(c: u32) -> i32 {
    return 13 - i32((c % 52u) / 4u);
}

fn glyph_bit(row: u32, x: i32, w: i32) -> bool {
    return ((row >> u32(w - 1 - x)) & 1u) == 1u;
}

// Colour of sprite cell (gx, gy) of a card; alpha 0 for the cut-off corners.
fn card_color(rank: i32, suit: u32, gx: i32, gy: i32) -> vec4<f32> {
    let black = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    let white = vec4<f32>(1.0);
    let ax = min(gx, GW - 1 - gx);
    let ay = min(gy, GH - 1 - gy);
    if ax + ay < 2 {
        return vec4<f32>(0.0);
    }
    if ax == 0 || ay == 0 || (ax == 1 && ay == 1) {
        return black;
    }
    let red = suit == 1u || suit == 3u;
    let ink = select(black, vec4<f32>(0.9, 0.0, 0.0, 1.0), red);
    // corner indices, the second one turned 180 degrees
    for (var r = 0; r < 2; r = r + 1) {
        let x = select(gx, GW - 1 - gx, r == 1);
        let y = select(gy, GH - 1 - gy, r == 1);
        if x >= 2 && x <= 6 && y >= 2 && y <= 8 {
            if glyph_bit(RANK_FONT[(rank - 1) * 7 + (y - 2)], x - 2, 5) {
                return ink;
            }
            return white;
        }
        if x >= 2 && x <= 6 && y >= 10 && y <= 14 {
            if glyph_bit(PIP_SMALL[i32(suit) * 5 + (y - 10)], x - 2, 5) {
                return ink;
            }
            return white;
        }
    }
    if rank == 1 {
        // ace: one big pip, 3x scaled
        let lx = gx - 6;
        let ly = gy - 12;
        if lx >= 0 && lx < 21 && ly >= 0 && ly < 21 && glyph_bit(PIP_BIG[i32(suit) * 7 + ly / 3], lx / 3, 7) {
            return ink;
        }
        return white;
    }
    if rank <= 10 {
        let start = rank * (rank - 1) / 2 - 1;
        for (var i = 0; i < 10; i = i + 1) {
            if i >= rank {
                break;
            }
            let pc = PIP_POS[start + i];
            var lx = gx - pc.x + 3;
            var ly = gy - pc.y + 3;
            if lx >= 0 && lx < 7 && ly >= 0 && ly < 7 {
                if pc.y > 22 {
                    lx = 6 - lx;
                    ly = 6 - ly;
                }
                if glyph_bit(PIP_BIG[i32(suit) * 7 + ly], lx, 7) {
                    return ink;
                }
                return white;
            }
        }
        return white;
    }
    // court card: framed double-ended portrait
    if gx >= 7 && gx <= 25 && gy >= 5 && gy <= 39 {
        if gx == 7 || gx == 25 || gy == 5 || gy == 39 {
            return ink;
        }
        let ly = gy - 6;
        if ly == 16 {
            return ink;
        }
        var row = ly;
        var col = gx - 8;
        if ly > 16 {
            row = 32 - ly;
            col = 16 - col;
        }
        let word = COURT[((rank - 11) * 16 + row) * 3 + col / 8];
        let idx = (word >> (4u * u32(col % 8))) & 15u;
        switch idx {
            case 1u: { return black; }
            case 2u: { return vec4<f32>(1.0, 0.82, 0.0, 1.0); }
            case 3u: { return vec4<f32>(0.85, 0.0, 0.0, 1.0); }
            case 4u: { return vec4<f32>(0.0, 0.15, 0.75, 1.0); }
            case 5u: { return ink; }
            case 6u: { return vec4<f32>(1.0, 0.88, 0.72, 1.0); }
            default: { return white; }
        }
    }
    return white;
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let age = cascade_age();
    if age > 1.0e5 || se.env <= 0.0 {
        return vec4<f32>(0.0);
    }
    let p = floor(in.uv * se.resolution) + 0.5;
    let cell = cell_px();
    let cw = f32(GW) * cell;
    let ch = f32(GH) * cell;
    let g = max(p_gravity(), 0.05) * se.resolution.y;
    let e = clamp(p_bounce(), 0.05, 0.95);
    let floor_y = se.resolution.y - ch;
    let rate = max(p_trail(), 1.0);
    let n = card_count();
    let launched = min(n, u32(floor(age / LAUNCH_EVERY)) + 1u);

    var col = vec4<f32>(0.0);
    var found = false;
    // newest card first: later cards were stamped over earlier trails
    for (var i = 0u; i < 52u; i = i + 1u) {
        if i >= launched || found {
            break;
        }
        let c = launched - 1u - i;
        let k = card_at(c, cw, ch, g);
        let u = age - f32(c) * LAUNCH_EVERY;
        // times when the card's columns cover this pixel (1 px slack for snapping)
        let ta = (p.x - cw - 1.0 - k.x0) / k.vx;
        let tb = (p.x + 1.0 - k.x0) / k.vx;
        let lo = max(min(ta, tb), 0.0);
        let hi = max(ta, tb);
        if lo > min(hi, u) {
            continue;
        }
        let rank = rank_of(c);
        let suit = foundation(c);
        // the moving card itself, then its stamps from newest to oldest
        let newest = i32(floor(min(hi, u) * rate));
        for (var j = -1; j < MAX_STAMPS; j = j + 1) {
            var t = u;
            if j < 0 {
                if u > hi {
                    continue;
                }
            } else {
                t = f32(newest - j) / rate;
                if t < lo {
                    break;
                }
            }
            let pos = floor(vec2<f32>(k.x0 + k.vx * t, y_at(k, t, g, e, floor_y)));
            let l = p - pos;
            if l.x < 0.0 || l.y < 0.0 || l.x >= cw || l.y >= ch {
                continue;
            }
            let cc = card_color(rank, suit, i32(l.x / cell), i32(l.y / cell));
            if cc.a > 0.0 {
                col = cc;
                found = true;
                break;
            }
        }
    }
    // the piles still waiting to launch
    if !found && launched < n {
        for (var f = 0u; f < 4u; f = f + 1u) {
            let c = launched + ((3u - f) + 4u - launched % 4u) % 4u;
            if c >= n {
                continue;
            }
            let l = p - pile_pos(f, cw);
            if l.x < 0.0 || l.y < 0.0 || l.x >= cw || l.y >= ch {
                continue;
            }
            col = card_color(rank_of(c), f, i32(l.x / cell), i32(l.y / cell));
            break;
        }
    }
    let fade = smoothstep(0.0, 1.0, se.env);
    return col * fade;
}
