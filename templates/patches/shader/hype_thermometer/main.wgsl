// Hype thermometer: a narrow Windows 3.1 window (mint caption "HYPE" like the stream's camera
// windows) holding a mercury thermometer on a paper scale: ticks every 10 % on the left, a red
// "MAX" label, and a segment meter on the right. Everything is drawn on an integer grid of
// "window pixels" (`s` screen pixels each).
//
// Level (shaders have no memory, so it is rebuilt every frame from what is known):
//   pressure = chat_weight * min(chat_rate / chat_full, 1)
//            + trigger_weight * size(amount) * exp(-trigger_age / decay)
// i.e. chat rate plus the most recent trigger, amount-weighted and decaying (only the latest
// trigger is known, so a small trigger right after a big one lowers the bump).
//
// Bursts are bar-locked: bars are counted as round(time * bpm / 240 - lfo.bar) (assumes a steady
// tempo) and every `cycle` bars, on the downbeat, a cycle whose pressure was >= 1 bursts: the bulb
// strobes, mercury geysers out of the tube top with a fountain of pixel sparks, a shock ring leaves
// the bulb, the window shakes and the caption flashes; then the mercury drains and refills. While
// maxed between bursts it sits pinned: boiling bubbles, a blinking MAX, kick shudders.

const BIG = array<u32, 28>(
    0x00000011u, 0x00000011u, 0x00000011u, 0x0000001fu, 0x00000011u, 0x00000011u, 0x00000011u,
    0x00000011u, 0x00000011u, 0x0000000au, 0x00000004u, 0x00000004u, 0x00000004u, 0x00000004u,
    0x0000000fu, 0x00000011u, 0x00000011u, 0x0000000fu, 0x00000001u, 0x00000001u, 0x00000001u,
    0x0000001fu, 0x00000001u, 0x00000001u, 0x0000000fu, 0x00000001u, 0x00000001u, 0x0000001fu,
);
// M, A, X: 3x5, bit y*3+x
const SMALL = array<u32, 3>(0x00005b7du, 0x00005beau, 0x00005aadu);

const BLACK = vec3<f32>(0.0);
const WHITE = vec3<f32>(1.0);
const GRAY = vec3<f32>(0.753);
const DARK = vec3<f32>(0.502);
const FRAME = vec3<f32>(0.80, 0.86, 0.84);
const PAPER = vec3<f32>(0.97, 0.97, 0.94);
const GLASS = vec3<f32>(0.80, 0.91, 0.97);
const MERCURY = vec3<f32>(0.86, 0.04, 0.06);
const MERCURY_LIGHT = vec3<f32>(1.0, 0.45, 0.42);
const YELLOW = vec3<f32>(1.0, 0.92, 0.2);

const W: i32 = 56;
const H: i32 = 150;
const TX: i32 = 28;          // tube centre column
const TUBE_TOP: i32 = 30;    // black cap row
const MERC_TOP: i32 = 31;    // highest mercury row
const MERC_ZERO: i32 = 116;  // mercury top at 0 %
const SCALE_TOP: i32 = 33;   // 100 % tick
const BULB = vec2<i32>(28, 127);
const SPARKS: u32 = 48u;

// burst timeline (seconds from the cycle's downbeat)
const BURST: f32 = 0.8;    // bulb strobe, shake, caption flash, shock ring
const GEYSER: f32 = 0.7;   // mercury column up and back
const FX_END: f32 = 1.4;   // last sparks land
const DRAIN: f32 = 0.18;
const REFILL: f32 = 1.0;

fn hash(n: u32) -> f32 {
    var x = n * 747796405u + 2891336453u;
    x = ((x >> ((x >> 28u) + 4u)) ^ x) * 277803737u;
    return f32((x >> 22u) ^ x) / 4294967295.0;
}

fn big_on(g: u32, x: i32, y: i32) -> bool {
    if x < 0 || x > 4 || y < 0 || y > 6 {
        return false;
    }
    return ((BIG[g * 7u + u32(y)] >> u32(x)) & 1u) == 1u;
}

// "HYPE" in bold (each glyph pixel doubled to the right), 7 px advance
fn caption_text(p: vec2<i32>) -> bool {
    if p.x < 0 || p.y < 0 || p.y > 6 || p.x >= 28 {
        return false;
    }
    let g = u32(p.x / 7);
    let x = p.x % 7;
    return big_on(g, x, p.y) || big_on(g, x - 1, p.y);
}

fn max_label(p: vec2<i32>) -> bool {
    if p.x < 0 || p.y < 0 || p.y > 4 || p.x >= 12 {
        return false;
    }
    let g = u32(p.x / 4);
    let x = p.x % 4;
    if x > 2 {
        return false;
    }
    return ((SMALL[g] >> u32(p.y * 3 + x)) & 1u) == 1u;
}

fn tick_y(k: i32) -> i32 {
    return i32(round(mix(f32(MERC_ZERO), f32(SCALE_TOP), f32(k) / 10.0)));
}

fn segment_color(k: i32) -> vec3<f32> {
    if k >= 9 {
        return vec3<f32>(1.0, 0.1, 0.1);
    }
    if k >= 7 {
        return vec3<f32>(1.0, 0.55, 0.1);
    }
    if k >= 4 {
        return vec3<f32>(1.0, 0.9, 0.15);
    }
    return vec3<f32>(0.2, 0.85, 0.25);
}

struct Gauge {
    level: f32,      // displayed mercury level 0-1
    burst: f32,      // seconds into the burst, < 0 when not bursting
    pinned: bool,
    flash: bool,     // strobe state (bulb/caption)
    blink: bool,     // MAX label on
};

// Window pixel colour (rgb + coverage) at p.
fn window(p: vec2<i32>, g: Gauge) -> vec4<f32> {
    if p.x < 0 || p.y < 0 || p.x >= W || p.y >= H {
        return vec4<f32>(0.0);
    }
    // frame: black, 2 px light frame, black
    let e = min(min(p.x, p.y), min(W - 1 - p.x, H - 1 - p.y));
    if e == 0 || e == 3 {
        return vec4<f32>(BLACK, 1.0);
    }
    if e < 3 {
        return vec4<f32>(FRAME, 1.0);
    }
    // caption
    if p.y < 18 {
        let q = p - vec2<i32>(4, 4);
        if q.x < 13 {
            // system menu box
            if q.x == 12 {
                return vec4<f32>(BLACK, 1.0);
            }
            if q.y == 6 && q.x >= 4 && q.x < 9 {
                return vec4<f32>(WHITE, 1.0);
            }
            if q.y >= 6 && q.y < 9 && q.x >= 3 && q.x < 10 {
                return vec4<f32>(BLACK, 1.0);
            }
            return vec4<f32>(GRAY, 1.0);
        }
        let on = caption_text(p - vec2<i32>(21, 7));
        let cap = p_caption().rgb;
        if g.flash && g.burst >= 0.0 {
            return vec4<f32>(select(BLACK, WHITE, on), 1.0);
        }
        return vec4<f32>(select(cap, BLACK, on), 1.0);
    }
    if p.y == 18 {
        return vec4<f32>(BLACK, 1.0);
    }
    // sunken paper well
    if p.x < 9 || p.x > 46 || p.y < 23 || p.y > 141 {
        return vec4<f32>(GRAY, 1.0);
    }
    if p.x == 9 || p.y == 23 {
        return vec4<f32>(DARK, 1.0);
    }
    if p.x == 46 || p.y == 141 {
        return vec4<f32>(WHITE, 1.0);
    }
    let top = i32(round(mix(f32(MERC_ZERO), f32(MERC_TOP), clamp(g.level, 0.0, 1.0))));
    // bulb
    let bd = length(vec2<f32>(p - BULB));
    if bd < 9.0 {
        if bd >= 7.6 {
            return vec4<f32>(BLACK, 1.0);
        }
        var c = MERCURY;
        if g.burst >= 0.0 {
            c = select(YELLOW, WHITE, g.flash);
        } else if g.pinned && g.flash {
            c = MERCURY_LIGHT;
        }
        let hl = p - BULB;
        if (hl.x == -3 || hl.x == -2) && (hl.y == -3 || hl.y == -4) {
            c = mix(c, WHITE, 0.75);
        }
        return vec4<f32>(c, 1.0);
    }
    // tube
    if p.y >= TUBE_TOP && p.y <= 121 && p.x >= TX - 4 && p.x <= TX + 4 {
        if p.y == TUBE_TOP {
            return vec4<f32>(select(BLACK, PAPER, p.x == TX - 4 || p.x == TX + 4), 1.0);
        }
        if p.x == TX - 4 || p.x == TX + 4 {
            return vec4<f32>(BLACK, 1.0);
        }
        let inner = p.x >= TX - 2 && p.x <= TX + 2;
        if inner && (p.y >= top || p.y > MERC_ZERO) {
            // boiling bubbles rise through the column when it runs hot
            if g.level > 0.8 {
                for (var i = 0; i < 3; i++) {
                    let speed = 0.9 + 0.35 * f32(i);
                    let by = i32(round(mix(f32(MERC_ZERO + 3), f32(top), fract(se.time * speed + f32(i) * 0.37))));
                    if p.x == TX - 1 + i && p.y == by {
                        return vec4<f32>(MERCURY_LIGHT, 1.0);
                    }
                }
            }
            if p.y == top && top <= MERC_ZERO {
                return vec4<f32>(MERCURY_LIGHT, 1.0);
            }
            return vec4<f32>(select(MERCURY, MERCURY_LIGHT, p.x == TX - 2), 1.0);
        }
        return vec4<f32>(select(GLASS, WHITE, p.x == TX - 3), 1.0);
    }
    // ticks on the left: majors at 0/50/100 %
    for (var k = 0; k <= 10; k++) {
        if p.y == tick_y(k) {
            let len = select(3, 6, k % 5 == 0);
            if p.x <= TX - 6 && p.x > TX - 6 - len {
                return vec4<f32>(BLACK, 1.0);
            }
        }
    }
    // MAX label above the 100 % tick
    if g.blink && max_label(p - vec2<i32>(11, 25)) {
        return vec4<f32>(vec3<f32>(0.8, 0.0, 0.0), 1.0);
    }
    // segment meter on the right: ten LEDs between the ticks
    if p.x >= TX + 6 && p.x <= TX + 9 && p.y <= tick_y(0) && p.y > tick_y(10) {
        var k = 0;
        for (var j = 1; j <= 10; j++) {
            if p.y <= tick_y(j) {
                k = j;
            }
        }
        if p.y == tick_y(k) {
            return vec4<f32>(PAPER, 1.0);
        }
        let lit = g.level * 10.0 > f32(k) + 0.5;
        let c = segment_color(k);
        return vec4<f32>(select(mix(c, PAPER, 0.75), c, lit), 1.0);
    }
    return vec4<f32>(PAPER, 1.0);
}

// height of the mercury geyser above the tube cap, u seconds into the burst
fn geyser(u: f32) -> f32 {
    return 46.0 * sin(3.14159 * clamp(u / GEYSER, 0.0, 1.0));
}

// Burst extras: geyser, spark fountain, shock ring. rgb + coverage. Runs until FX_END so the
// sparks can fall while the tube drains.
fn burst_fx(p: vec2<i32>, u: f32, screen: vec2<i32>) -> vec4<f32> {
    let pf = vec2<f32>(p) + 0.5;
    // spark fountain thrown off the geyser head: mercury droplets, yellow and white sparks
    for (var i = 0u; i < SPARKS; i++) {
        let t0 = f32(i) / f32(SPARKS) * 0.55;
        let dt = u - t0;
        let life = 0.5 + 0.3 * hash(i * 5u + 1u);
        if dt < 0.0 || dt > life {
            continue;
        }
        let a = -1.5708 + (hash(i * 5u + 2u) - 0.5) * 2.6;
        let v = 70.0 + 140.0 * hash(i * 5u + 3u);
        let head = vec2<f32>(f32(TX) + 0.5, f32(TUBE_TOP) - geyser(t0));
        let sp = head + vec2<f32>(cos(a), sin(a)) * v * dt + vec2<f32>(0.0, 300.0) * dt * dt;
        let d = abs(pf - sp);
        let r = select(1.0, 1.6, i % 4u == 0u);
        if d.x < r && d.y < r {
            // late sparks drop out on a checkerboard
            if dt > life * 0.65 && ((screen.x + screen.y) & 1) == 0 {
                continue;
            }
            let kind = i % 5u;
            let c = select(select(WHITE, YELLOW, kind == 2u || kind == 3u), MERCURY, kind < 2u);
            return vec4<f32>(c, 1.0);
        }
    }
    // geyser: a wobbling mercury column out of the tube cap with a fat head
    let gh = geyser(u);
    let top = f32(TUBE_TOP) - gh;
    let y = f32(p.y);
    if gh > 0.5 && y < f32(TUBE_TOP) && y >= top - 2.0 {
        let wob = i32(round(sin(y * 0.5 + u * 34.0) * 1.2));
        let half = select(2, 3, y < top + 3.0);
        let x = p.x - wob - TX;
        if x >= -half && x <= half {
            return vec4<f32>(select(MERCURY, MERCURY_LIGHT, x == -half || y < top), 1.0);
        }
    }
    // shock ring from the bulb, checker-dithered
    if u < BURST {
        let r = 11.0 + 64.0 * u / BURST;
        let bd = length(pf - vec2<f32>(BULB) - 0.5);
        if abs(bd - r) < 1.5 && ((screen.x + screen.y) & 1) == 0 {
            return vec4<f32>(YELLOW, 1.0 - u / BURST);
        }
    }
    return vec4<f32>(0.0);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    // pressure from chat rate and the latest trigger
    let rate = max(s_twitch_chat_rate(), 0.0);
    let chat = p_chat_weight() * min(rate / max(p_chat_full(), 1.0), 1.0);
    let age = se.trigger_age;
    let size = 0.35 + 0.65 * clamp(log2(1.0 + max(se.trigger.amount, 0.0) / 100.0) / log2(11.0), 0.0, 1.0);
    let w = select(0.0, p_trigger_weight() * size, age < 1.0e5);
    let decay = max(p_decay(), 0.1);
    let pressure = chat + w * exp(-age / decay);

    // bar-locked cycle (steady tempo assumed)
    let bpm = select(120.0, s_beat_bpm(), s_beat_bpm() > 30.0);
    let bar = 240.0 / bpm;
    let cycle = clamp(p_cycle(), 1, 4);
    let lb = fract(s_lfo_bar());
    let n = i32(round(se.time / bar - lb));
    let k = ((n % cycle) + cycle) % cycle;
    let u = (f32(k) + lb) * bar;
    // pressure at the cycle's downbeat: exact if the latest trigger came before it
    let start = chat + select(w * exp(-age / decay), w * exp(-(age - u) / decay), age >= u);
    let burst_cycle = start >= 1.0;
    let hot = pressure >= 1.0;

    var g: Gauge;
    g.burst = -1.0;
    var level = select(min(pressure, 0.985), 1.0, hot);
    var refilling = false;
    if burst_cycle {
        if u < BURST {
            level = 1.0;
            g.burst = u;
        } else if u < BURST + DRAIN {
            let x = (u - BURST) / DRAIN;
            level = 1.0 - x * x;
            refilling = true;
        } else if u < BURST + DRAIN + REFILL {
            let x = (u - BURST - DRAIN) / REFILL;
            level = level * (1.0 - (1.0 - x) * (1.0 - x) * (1.0 - x));
            refilling = true;
        }
    }
    g.pinned = hot && g.burst < 0.0 && !refilling;
    let snare = clamp(s_band_snare(), 0.0, 1.0);
    g.level = min(level + 0.025 * snare * (1.0 - level), 1.0);
    g.flash = fract(se.time / 0.12) < 0.5;
    let beat = fract(s_beat_phase());
    g.blink = select(level > 0.9, beat < 0.5, g.pinned);

    // placement on whole screen pixels; shakes while bursting, shudders on kicks while pinned
    let s = max(1.0, round(se.resolution.y / 360.0 * p_scale()));
    let size_px = vec2<f32>(f32(W), f32(H)) * s;
    let rest = floor(clamp(p_position() * se.resolution - size_px * 0.5, vec2<f32>(0.0), max(se.resolution - size_px, vec2<f32>(0.0))) / s) * s;
    var origin = rest;
    if g.burst >= 0.0 {
        let f = u32(se.time * 30.0);
        origin = origin + vec2<f32>(round((hash(f) - 0.5) * 4.0), round((hash(f + 7u) - 0.5) * 4.0)) * s;
    } else if g.pinned && s_band_kick() > 0.6 {
        origin.y = origin.y - s;
    }
    let frag = in.uv * se.resolution;
    let p = vec2<i32>(floor((frag - origin) / s));
    let screen = vec2<i32>(floor(frag / s));
    if p.x < -110 || p.x > W + 110 || p.y < -150 || p.y > H + 90 {
        return vec4<f32>(0.0);
    }
    // burst extras draw over the window (the geyser erupts through the caption) and keep going
    // while the tube drains; they don't shake with the window
    var fx = vec4<f32>(0.0);
    if burst_cycle && u < FX_END {
        fx = burst_fx(vec2<i32>(floor((frag - rest) / s)), u, screen);
    }
    let win = window(p, g);
    let base = vec4<f32>(win.rgb * win.a, win.a);
    return vec4<f32>(fx.rgb * fx.a, fx.a) + base * (1.0 - fx.a);
}
