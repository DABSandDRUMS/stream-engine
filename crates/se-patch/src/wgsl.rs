//! Generated WGSL header for shader/particles patches, effects, and transitions (§6.3).
//!
//! Every shader gets the same input contract as other patch kinds: `time`, `dt`, `frame`,
//! `env` (trigger envelope), `resolution`, `progress` (transitions), `region` (the node a pass
//! runs for), the last trigger's payload (`se.trigger`, [`se_core::triggers::TriggerPayload`]),
//! params by name, signals by name, and the stream palette. The renderer fills one uniform buffer per
//! patch with [`Layout::write`] (no allocation) and binds:
//!
//! | binding | resource |
//! |---|---|
//! | `@group(0) @binding(0)` | `se: SeInputs` uniforms |
//! | `@group(0) @binding(1)` | `se_sampler` (linear, clamp) |
//! | `@group(0) @binding(2)` | `se_input` — effect input / transition A |
//! | `@group(0) @binding(3)` | `se_input_b` — transition B |
//! | `@group(0) @binding(4)` | `se_particles` storage buffer (particles kind) |
//!
//! Accessors: `p_<param>()`, `s_<signal with dots as underscores>()`, `palette(i)` with
//! `PAL_*` indices, and a full-screen vertex entry `se_vs` producing `SeVsOut { pos, uv }`.

use crate::manifest::Manifest;
use se_core::triggers::{PAYLOAD_COLOR, PAYLOAD_FIELDS, PAYLOAD_FLOATS};
use se_proto::ValueType;

/// Signals every shader can read without declaring them.
pub const STANDARD_SIGNALS: &[&str] = &[
    "band.level",
    "band.bass",
    "band.mid",
    "band.high",
    "band.kick",
    "band.snare",
    "band.hat",
    "band.centroid",
    "music.level",
    "music.bass",
    "music.mid",
    "music.high",
    "beat.phase",
    "beat.bpm",
    "mic.level",
    "lfo.slow",
    "lfo.mid",
    "lfo.fast",
    "lfo.beat",
    "lfo.bar",
    "lfo.random",
];

/// Stream palette slots (§16.2).
pub const PALETTE: &[&str] = &["ACCENT", "BACKGROUND", "FOREGROUND", "RED", "YELLOW", "GREEN", "CYAN", "MAGENTA"];

/// Float offsets of the fixed fields in the uniform block.
pub mod off {
    pub const TIME: usize = 0;
    pub const DT: usize = 1;
    /// u32 bits
    pub const FRAME: usize = 2;
    pub const ENV: usize = 3;
    pub const RESOLUTION: usize = 4;
    pub const PROGRESS: usize = 6;
    /// u32 bits
    pub const TRIGGER_COUNT: usize = 7;
    pub const BEAT_PHASE: usize = 8;
    pub const BPM: usize = 9;
    /// uv rect x0, y0, x1, y1
    pub const REGION: usize = 12;
    /// `se_core::triggers::TriggerPayload::floats`
    pub const TRIGGER: usize = 16;
    pub const PALETTE: usize = TRIGGER + super::PAYLOAD_FLOATS;
    pub const PARAMS: usize = PALETTE + 8 * 4;
}

/// Values of the fixed fields of the uniform block (everything but params and signals).
#[derive(Clone, Copy, Debug)]
pub struct Fixed<'a> {
    pub time: f32,
    pub dt: f32,
    pub frame: u32,
    pub env: f32,
    pub resolution: [f32; 2],
    pub progress: f32,
    pub trigger_count: u32,
    /// uv rect (x0, y0, x1, y1) of the node this pass runs for; the whole target otherwise.
    pub region: [f32; 4],
    /// Last trigger's payload ([`se_core::triggers::TriggerPayload::floats`]).
    pub trigger: &'a [f32; PAYLOAD_FLOATS],
    pub palette: &'a [[f32; 4]; 8],
}

/// Full-target region.
pub const FULL: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

/// No trigger payload.
pub const NO_TRIGGER: [f32; PAYLOAD_FLOATS] = [0.0; PAYLOAD_FLOATS];

/// `struct SeTrigger` matching [`se_core::triggers::TriggerPayload::floats`].
fn trigger_struct() -> String {
    let mut s = String::from("struct SeTrigger {\n");
    for f in PAYLOAD_FIELDS {
        s += &format!("  {f}: f32,\n");
    }
    for i in PAYLOAD_FIELDS.len()..PAYLOAD_COLOR {
        s += &format!("  _pad{i}: f32,\n");
    }
    s += "  user_color: vec4<f32>,\n}\n";
    s
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// `(name, type, float offset from the start of the block)`
    pub params: Vec<(String, ValueType, usize)>,
    pub param_rows: usize,
    /// Signal names in slot order.
    pub signals: Vec<String>,
    pub signals_offset: usize,
    pub signal_rows: usize,
    /// Total size in f32s (multiple of 4).
    pub floats: usize,
}

fn mangle(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

impl Layout {
    pub fn new(m: &Manifest) -> Layout {
        let slots = m.param_slots();
        let used = slots.iter().map(|(_, p, o)| o + p.slots()).max().unwrap_or(0);
        let param_rows = used.div_ceil(4).max(1);
        let params = slots.iter().map(|(n, p, o)| (n.to_string(), p.value_type(), off::PARAMS + o)).collect();
        let mut signals: Vec<String> = STANDARD_SIGNALS.iter().map(|s| s.to_string()).collect();
        for s in &m.signals {
            if !signals.contains(s) {
                signals.push(s.clone());
            }
        }
        let signals_offset = off::PARAMS + param_rows * 4;
        let signal_rows = signals.len().div_ceil(4).max(1);
        Layout { params, param_rows, signals, signals_offset, signal_rows, floats: signals_offset + signal_rows * 4 }
    }

    pub fn byte_size(&self) -> usize {
        self.floats * 4
    }

    /// The WGSL header to prepend to the patch source.
    pub fn header(&self) -> String {
        let mut s = String::from("// ---- generated by stream-engine; do not edit ----\n");
        s += &trigger_struct();
        s += "struct SeInputs {\n  time: f32,\n  dt: f32,\n  frame: u32,\n  env: f32,\n  resolution: vec2<f32>,\n  progress: f32,\n  trigger_count: u32,\n  beat_phase: f32,\n  bpm: f32,\n  _pad0: f32,\n  _pad1: f32,\n  region: vec4<f32>,\n  trigger: SeTrigger,\n";
        s += &format!("  palette: array<vec4<f32>, {}>,\n", PALETTE.len());
        s += &format!("  params: array<vec4<f32>, {}>,\n", self.param_rows);
        s += &format!("  signals: array<vec4<f32>, {}>,\n}}\n", self.signal_rows);
        s += "@group(0) @binding(0) var<uniform> se: SeInputs;\n@group(0) @binding(1) var se_sampler: sampler;\n@group(0) @binding(2) var se_input: texture_2d<f32>;\n@group(0) @binding(3) var se_input_b: texture_2d<f32>;\n";
        for (i, p) in PALETTE.iter().enumerate() {
            s += &format!("const PAL_{p}: u32 = {i}u;\n");
        }
        s += "fn palette(i: u32) -> vec4<f32> { return se.palette[i]; }\n";
        for (name, ty, o) in &self.params {
            let rel = o - off::PARAMS;
            let (row, col) = (rel / 4, rel % 4);
            let comp = ["x", "y", "z", "w"];
            let body = match ty {
                ValueType::Color | ValueType::Vec4 => format!("vec4<f32> {{ return se.params[{row}]; }}"),
                ValueType::Vec2 => format!("vec2<f32> {{ return se.params[{row}].{}{}; }}", comp[col], comp[col + 1]),
                ValueType::Int | ValueType::Enum => format!("i32 {{ return i32(se.params[{row}].{}); }}", comp[col]),
                ValueType::Bool => format!("bool {{ return se.params[{row}].{} > 0.5; }}", comp[col]),
                _ => format!("f32 {{ return se.params[{row}].{}; }}", comp[col]),
            };
            s += &format!("fn p_{}() -> {body}\n", mangle(name));
        }
        for (i, name) in self.signals.iter().enumerate() {
            let comp = ["x", "y", "z", "w"][i % 4];
            s += &format!("fn s_{}() -> f32 {{ return se.signals[{}].{comp}; }}\n", mangle(name), i / 4);
        }
        s += "struct SeVsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };\n";
        s += "@vertex fn se_vs(@builtin(vertex_index) i: u32) -> SeVsOut {\n  let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));\n  var o: SeVsOut;\n  o.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);\n  o.uv = vec2<f32>(p.x, 1.0 - p.y);\n  return o;\n}\n";
        s += "// ---- end of generated header ----\n";
        s
    }

    /// Number of header lines (to map compiler error lines back to the patch source).
    pub fn header_lines(&self) -> usize {
        self.header().lines().count()
    }

    /// Fill a uniform block. `param` returns each param's current value; `signal` each signal.
    pub fn write(&self, buf: &mut [f32], fixed: &Fixed, mut param: impl FnMut(usize, &str) -> [f32; 4], mut signal: impl FnMut(usize, &str) -> f32) {
        debug_assert!(buf.len() >= self.floats);
        buf[off::TIME] = fixed.time;
        buf[off::DT] = fixed.dt;
        buf[off::FRAME] = f32::from_bits(fixed.frame);
        buf[off::ENV] = fixed.env;
        buf[off::RESOLUTION] = fixed.resolution[0];
        buf[off::RESOLUTION + 1] = fixed.resolution[1];
        buf[off::PROGRESS] = fixed.progress;
        buf[off::TRIGGER_COUNT] = f32::from_bits(fixed.trigger_count);
        buf[off::REGION..off::REGION + 4].copy_from_slice(&fixed.region);
        buf[off::TRIGGER..off::TRIGGER + PAYLOAD_FLOATS].copy_from_slice(fixed.trigger);
        for (i, c) in fixed.palette.iter().enumerate() {
            buf[off::PALETTE + i * 4..off::PALETTE + i * 4 + 4].copy_from_slice(c);
        }
        for (i, (name, ty, o)) in self.params.iter().enumerate() {
            let v = param(i, name);
            let n = match ty {
                ValueType::Color | ValueType::Vec4 => 4,
                ValueType::Vec2 => 2,
                _ => 1,
            };
            buf[*o..*o + n].copy_from_slice(&v[..n]);
        }
        for (i, name) in self.signals.iter().enumerate() {
            let v = signal(i, name);
            buf[self.signals_offset + i] = v;
            if name == "beat.phase" {
                buf[off::BEAT_PHASE] = v;
            } else if name == "beat.bpm" {
                buf[off::BPM] = v;
            }
        }
    }
}

/// Map a line number in the compiled (header + source) shader back to the patch source.
pub fn source_line(layout: &Layout, compiled_line: usize) -> Option<usize> {
    compiled_line.checked_sub(layout.header_lines())
}

/// Size of one `Particle` in the `se_particles` storage buffer (bytes).
pub const PARTICLE_BYTES: usize = 48;

/// Prelude for `particles` patches, prepended after [`Layout::header`]: the `Particle`
/// struct, the `se_particles` storage buffer at `@group(0) @binding(4)` (read-write for
/// `sim.wgsl`, read-only for `draw.wgsl`), `SE_PARTICLE_COUNT`, and `se_hash(n) -> 0..1`.
///
/// Entry points: `sim.wgsl` → `@compute @workgroup_size(64) fn sim(@builtin(global_invocation_id) id: vec3<u32>)`
/// (dispatched `ceil(count / 64)`); `draw.wgsl` → `@vertex fn vs(@builtin(vertex_index) v: u32,
/// @builtin(instance_index) i: u32) -> SeVsOut` (6 vertices per particle) and
/// `@fragment fn fs(in: SeVsOut) -> @location(0) vec4<f32>` (premultiplied alpha).
pub fn particles_prelude(read_write: bool, count: u32) -> String {
    let access = if read_write { "read_write" } else { "read" };
    format!(
        "// ---- particles prelude (generated) ----\n\
struct Particle {{ pos: vec2<f32>, vel: vec2<f32>, color: vec4<f32>, life: f32, size: f32, seed: f32, age: f32 }};\n\
@group(0) @binding(4) var<storage, {access}> se_particles: array<Particle>;\n\
const SE_PARTICLE_COUNT: u32 = {count}u;\n\
fn se_hash(n: u32) -> f32 {{ var x = n; x = x ^ (x >> 16u); x = x * 0x7feb352du; x = x ^ (x >> 15u); x = x * 0x846ca68bu; x = x ^ (x >> 16u); return f32(x) / 4294967295.0; }}\n\
// ---- end of particles prelude ----\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn manifest() -> Manifest {
        Manifest::parse(
            Path::new("/p/patches/aurora"),
            "kind = \"shader\"\nparams.speed = { type = \"float\", default = 1.0 }\nparams.tint = { type = \"color\" }\nparams.count = { type = \"int\" }\nsignals = [\"mixer.16r.meter.ch.1\"]",
        )
        .unwrap()
    }

    #[test]
    fn header_declares_accessors() {
        let l = Layout::new(&manifest());
        let h = l.header();
        assert!(h.contains("fn p_speed() -> f32"));
        assert!(h.contains("fn p_tint() -> vec4<f32>"));
        assert!(h.contains("fn p_count() -> i32"));
        assert!(h.contains("fn s_band_kick() -> f32"));
        assert!(h.contains("fn s_mixer_16r_meter_ch_1() -> f32"));
        assert!(h.contains("@vertex fn se_vs"));
        assert_eq!(l.floats % 4, 0);
    }

    /// Byte offset of `member` in struct `ty` of a parsed module.
    fn member_offset(m: &naga::Module, ty: &str, member: &str) -> u32 {
        let (_, t) = m.types.iter().find(|(_, t)| t.name.as_deref() == Some(ty)).unwrap_or_else(|| panic!("no struct {ty}"));
        let naga::TypeInner::Struct { members, .. } = &t.inner else { panic!("{ty} is not a struct") };
        members.iter().find(|x| x.name.as_deref() == Some(member)).unwrap_or_else(|| panic!("{ty}.{member} missing")).offset
    }

    #[test]
    fn header_layout_matches_the_writer_offsets() {
        let l = Layout::new(&manifest());
        let src =
            format!("{}@fragment fn fs(in: SeVsOut) -> @location(0) vec4<f32> {{ return se.trigger.user_color * se.trigger.bits + se.region; }}\n", l.header());
        let m = naga::front::wgsl::parse_str(&src).unwrap_or_else(|e| panic!("{}", e.emit_to_string(&src)));
        let at = |f: usize| (f * 4) as u32;
        assert_eq!(member_offset(&m, "SeInputs", "trigger_count"), at(off::TRIGGER_COUNT));
        assert_eq!(member_offset(&m, "SeInputs", "region"), at(off::REGION));
        assert_eq!(member_offset(&m, "SeInputs", "trigger"), at(off::TRIGGER));
        assert_eq!(member_offset(&m, "SeInputs", "palette"), at(off::PALETTE));
        assert_eq!(member_offset(&m, "SeInputs", "params"), at(off::PARAMS));
        assert_eq!(member_offset(&m, "SeInputs", "signals"), at(l.signals_offset));
        // every payload field sits where TriggerPayload::floats puts it
        for (i, f) in PAYLOAD_FIELDS.iter().enumerate() {
            assert_eq!(member_offset(&m, "SeTrigger", f), at(i), "{f}");
        }
        assert_eq!(member_offset(&m, "SeTrigger", "user_color"), at(PAYLOAD_COLOR));
    }

    #[test]
    fn write_fills_offsets() {
        let l = Layout::new(&manifest());
        let mut buf = vec![0.0f32; l.floats];
        let pal = [[0.5; 4]; 8];
        let payload = se_core::triggers::TriggerPayload { bits: 5000.0, amount: 5000.0, user_color: [1.0, 0.5, 0.0, 1.0], ..Default::default() };
        let fixed = Fixed {
            time: 1.5,
            dt: 0.016,
            frame: 7,
            env: 0.25,
            resolution: [1920.0, 1080.0],
            progress: 0.0,
            trigger_count: 2,
            region: [0.1, 0.2, 0.6, 0.9],
            trigger: &payload.floats(),
            palette: &pal,
        };
        l.write(
            &mut buf,
            &fixed,
            |_, n| if n == "tint" { [1.0, 0.0, 0.0, 1.0] } else { [3.0, 0.0, 0.0, 0.0] },
            |_, n| if n == "beat.phase" { 0.75 } else { 0.1 },
        );
        assert_eq!(buf[off::TIME], 1.5);
        assert_eq!(buf[off::FRAME].to_bits(), 7);
        assert_eq!(buf[off::TRIGGER_COUNT].to_bits(), 2);
        assert_eq!(buf[off::BEAT_PHASE], 0.75);
        assert_eq!(&buf[off::REGION..off::REGION + 4], &[0.1, 0.2, 0.6, 0.9]);
        assert_eq!(buf[off::TRIGGER + 1], 5000.0, "bits");
        assert_eq!(&buf[off::TRIGGER + PAYLOAD_COLOR..off::TRIGGER + PAYLOAD_COLOR + 4], &[1.0, 0.5, 0.0, 1.0]);
        let tint = l.params.iter().find(|p| p.0 == "tint").unwrap().2;
        assert_eq!(&buf[tint..tint + 4], &[1.0, 0.0, 0.0, 1.0]);
        // the palette must not overlap the payload
        assert_eq!(&buf[off::PALETTE..off::PALETTE + 4], &[0.5; 4]);
    }
}
