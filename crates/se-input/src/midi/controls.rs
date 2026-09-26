//! Control layer: MIDI messages → named controls (7-bit CC, 14-bit CC pairs, NRPN/RPN,
//! pitch-bend, notes, relative encoders, MCU/HUI specifics), and the reverse direction
//! (normalized value → feedback bytes for LED rings, button LEDs, motor faders).

use super::parse::Msg;

/// How a relative encoder encodes its delta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelEnc {
    /// Mackie Control V-Pot: bit 6 set = counter-clockwise, bits 0–5 = magnitude.
    SignMag,
    /// HUI V-Pot: bit 6 set = clockwise, bits 0–5 = magnitude.
    SignMagCw,
    /// 1..63 = +n, 127..64 = -(128-n).
    TwosComplement,
    /// 64 = no change, 65.. = +, ..63 = - ("binary offset").
    Offset64,
}

impl RelEnc {
    pub fn parse(s: &str) -> Option<RelEnc> {
        Some(match s {
            "mcu" | "signmag" | "sign_magnitude" => RelEnc::SignMag,
            "hui" | "signmag_cw" => RelEnc::SignMagCw,
            "twos" | "twos_complement" | "relative1" => RelEnc::TwosComplement,
            "offset" | "offset64" | "binary_offset" | "relative2" => RelEnc::Offset64,
            _ => return None,
        })
    }

    pub fn delta(self, v: u8) -> i32 {
        let v = v & 0x7F;
        match self {
            RelEnc::SignMag => {
                let m = (v & 0x3F) as i32;
                if v & 0x40 != 0 { -m } else { m }
            }
            RelEnc::SignMagCw => {
                let m = (v & 0x3F) as i32;
                if v & 0x40 != 0 { m } else { -m }
            }
            RelEnc::TwosComplement => {
                if v < 64 {
                    v as i32
                } else {
                    v as i32 - 128
                }
            }
            RelEnc::Offset64 => v as i32 - 64,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            RelEnc::SignMag => "mcu",
            RelEnc::SignMagCw => "hui",
            RelEnc::TwosComplement => "twos",
            RelEnc::Offset64 => "offset64",
        }
    }
}

/// How a button-like control reports presses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SwitchMode {
    /// value ≥ 64 (or note on with velocity > 0) = down, below = up.
    #[default]
    Momentary,
    /// Every message is a complete press (down + up): footswitches in "single"/"toggle" mode.
    Trigger,
}

impl SwitchMode {
    pub fn parse(s: &str) -> Option<SwitchMode> {
        Some(match s {
            "momentary" => SwitchMode::Momentary,
            "trigger" | "single" | "toggle" => SwitchMode::Trigger,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            SwitchMode::Momentary => "momentary",
            SwitchMode::Trigger => "trigger",
        }
    }
}

/// Where a control's messages come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// 7-bit absolute CC (a knob, pedal, or — with `button` — a switch).
    Cc {
        cc: u8,
    },
    /// 14-bit CC pair: MSB on `cc`, LSB on `lsb` (conventionally `cc + 32`).
    Cc14 {
        cc: u8,
        lsb: u8,
    },
    /// Relative (endless) encoder on a CC.
    Rel {
        cc: u8,
        enc: RelEnc,
    },
    /// NRPN parameter (`hires`: 14-bit data entry, else MSB only).
    Nrpn {
        param: u16,
        hires: bool,
    },
    Rpn {
        param: u16,
    },
    /// 14-bit pitch-bend (MCU faders).
    PitchBend,
    Note {
        note: u8,
    },
    ChannelPressure,
    /// HUI switch: zone select (CC 0x0F) then port (CC 0x2F).
    HuiSwitch {
        zone: u8,
        port: u8,
    },
}

impl Kind {
    pub fn is_absolute(&self) -> bool {
        matches!(self, Kind::Cc { .. } | Kind::Cc14 { .. } | Kind::Nrpn { .. } | Kind::Rpn { .. } | Kind::PitchBend | Kind::ChannelPressure)
    }
    pub fn is_relative(&self) -> bool {
        matches!(self, Kind::Rel { .. })
    }
    pub fn describe(&self) -> String {
        match self {
            Kind::Cc { cc } => format!("cc {cc}"),
            Kind::Cc14 { cc, lsb } => format!("cc14 {cc}/{lsb}"),
            Kind::Rel { cc, enc } => format!("relative cc {cc} ({})", enc.name()),
            Kind::Nrpn { param, hires } => format!("nrpn {param}{}", if *hires { " (14-bit)" } else { "" }),
            Kind::Rpn { param } => format!("rpn {param}"),
            Kind::PitchBend => "pitch-bend".into(),
            Kind::Note { note } => format!("note {note}"),
            Kind::ChannelPressure => "channel pressure".into(),
            Kind::HuiSwitch { zone, port } => format!("hui zone {zone} port {port}"),
        }
    }
}

/// LED ring display style (MCU/HUI and generic ring CCs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum RingMode {
    /// One LED at the position.
    Dot,
    /// Center-out (boost/cut).
    BoostCut,
    /// Filled from the left.
    #[default]
    Fill,
    /// Spread symmetric from the center.
    Spread,
}

impl RingMode {
    pub fn parse(s: &str) -> Option<RingMode> {
        Some(match s {
            "dot" | "single" => RingMode::Dot,
            "boost" | "boost_cut" | "pan" => RingMode::BoostCut,
            "fill" | "wrap" => RingMode::Fill,
            "spread" | "width" => RingMode::Spread,
            _ => return None,
        })
    }
    fn mcu_code(self) -> u8 {
        match self {
            RingMode::Dot => 0,
            RingMode::BoostCut => 1,
            RingMode::Fill => 2,
            RingMode::Spread => 3,
        }
    }
}

/// How feedback is sent back to a control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Out {
    Cc {
        ch: u8,
        cc: u8,
    },
    Cc14 {
        ch: u8,
        cc: u8,
        lsb: u8,
    },
    Nrpn {
        ch: u8,
        param: u16,
    },
    PitchBend {
        ch: u8,
    },
    /// Button LED: note on with velocity 127 (on), 1 (blink), 0 (off).
    NoteLed {
        ch: u8,
        note: u8,
    },
    /// MCU V-Pot ring: CC `0x30 + strip`, value = mode << 4 | position (0–11).
    McuRing {
        strip: u8,
    },
    /// HUI V-Pot ring: CC `0x10 + strip`, value 0–11 (+0x40 center LED).
    HuiRing {
        strip: u8,
    },
    /// HUI LED: CC 0x0C zone, CC 0x2C port (| 0x40 = on).
    HuiLed {
        zone: u8,
        port: u8,
    },
    /// HUI fader: CC `strip` (hi) + CC `0x20 + strip` (lo), 14-bit.
    HuiFader {
        strip: u8,
    },
    /// Behringer standard-mode ring: CC value 0 = off, 1–13 = LED position, 27 = all on.
    BehringerRing {
        ch: u8,
        cc: u8,
    },
}

/// LED state for button feedback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Led {
    Off,
    On,
    Blink,
}

impl Out {
    /// Encode a normalized value (0–1) for this output; `led` is used by LED outputs.
    pub fn encode(&self, norm: f32, led: Led, ring: RingMode, buf: &mut Vec<u8>) {
        let n = norm.clamp(0.0, 1.0);
        let v7 = (n * 127.0).round() as u8;
        let v14 = (n * 16383.0).round() as u16;
        match *self {
            Out::Cc { ch, cc } => buf.extend_from_slice(&[0xB0 | ch, cc, v7]),
            Out::Cc14 { ch, cc, lsb } => buf.extend_from_slice(&[0xB0 | ch, cc, (v14 >> 7) as u8, 0xB0 | ch, lsb, (v14 & 0x7F) as u8]),
            Out::Nrpn { ch, param } => buf.extend_from_slice(&[
                0xB0 | ch,
                99,
                (param >> 7) as u8 & 0x7F,
                0xB0 | ch,
                98,
                (param & 0x7F) as u8,
                0xB0 | ch,
                6,
                (v14 >> 7) as u8,
                0xB0 | ch,
                38,
                (v14 & 0x7F) as u8,
            ]),
            Out::PitchBend { ch } => buf.extend_from_slice(&[0xE0 | ch, (v14 & 0x7F) as u8, (v14 >> 7) as u8]),
            Out::NoteLed { ch, note } => {
                let vel = match led {
                    Led::Off => 0,
                    Led::On => 127,
                    Led::Blink => 1,
                };
                buf.extend_from_slice(&[0x90 | ch, note, vel]);
            }
            Out::McuRing { strip } => {
                let pos = ring_position(n, ring);
                buf.extend_from_slice(&[0xB0, 0x30 + strip, (ring.mcu_code() << 4) | pos]);
            }
            Out::HuiRing { strip } => {
                let pos = ring_position(n, ring);
                buf.extend_from_slice(&[0xB0, 0x10 + strip, pos]);
            }
            Out::HuiLed { zone, port } => {
                let on = led != Led::Off;
                buf.extend_from_slice(&[0xB0, 0x0C, zone, 0xB0, 0x2C, port | if on { 0x40 } else { 0 }]);
            }
            Out::HuiFader { strip } => buf.extend_from_slice(&[0xB0, strip, (v14 >> 7) as u8, 0xB0, 0x20 + strip, (v14 & 0x7F) as u8]),
            Out::BehringerRing { ch, cc } => {
                let v = match ring {
                    RingMode::Fill | RingMode::Spread | RingMode::BoostCut | RingMode::Dot => {
                        if n <= 0.0 {
                            0
                        } else {
                            1 + (n * 12.0).round() as u8
                        }
                    }
                };
                buf.extend_from_slice(&[0xB0 | ch, cc, v]);
            }
        }
    }
}

/// MCU ring position 0–11: 0 lights nothing (except in dot mode, where 1 is the leftmost LED).
fn ring_position(n: f32, ring: RingMode) -> u8 {
    match ring {
        RingMode::Dot | RingMode::BoostCut | RingMode::Spread => 1 + (n * 10.0).round() as u8,
        RingMode::Fill => (n * 11.0).round() as u8,
    }
}

/// One control of a device.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlDef {
    pub name: String,
    /// `None` = any channel.
    pub ch: Option<u8>,
    pub kind: Kind,
    /// Treat an absolute CC as a switch (press/release) instead of a continuous value.
    pub button: bool,
    pub switch: SwitchMode,
    /// MCU touch-sensitive fader: note that reports touch on/off.
    pub touch_note: Option<u8>,
    /// Motorized (feedback moves the physical control).
    pub motor: bool,
    /// Feedback output (LED ring, button LED, motor fader, echo).
    pub out: Option<Out>,
    /// Created on the fly for a message nobody declared.
    pub auto: bool,
}

impl ControlDef {
    pub fn new(name: impl Into<String>, ch: Option<u8>, kind: Kind) -> ControlDef {
        ControlDef { name: name.into(), ch, kind, button: false, switch: SwitchMode::Momentary, touch_note: None, motor: false, out: None, auto: false }
    }
    pub fn out(mut self, o: Out) -> Self {
        self.out = Some(o);
        self
    }
    pub fn motor(mut self, touch: Option<u8>) -> Self {
        self.motor = true;
        self.touch_note = touch;
        self
    }
    /// Controls that produce press/release (notes, switch CCs, HUI switches).
    pub fn is_button(&self) -> bool {
        self.button || matches!(self.kind, Kind::Note { .. } | Kind::HuiSwitch { .. })
    }
    fn matches_ch(&self, ch: u8) -> bool {
        self.ch.is_none_or(|c| c == ch)
    }
}

/// Decoded control activity.
#[derive(Clone, Debug, PartialEq)]
pub enum Ev {
    /// Absolute value 0–1 (with the raw integer for learn/monitoring).
    Value {
        idx: usize,
        value: f32,
        raw: u16,
    },
    /// Relative encoder movement in detents.
    Delta {
        idx: usize,
        steps: i32,
    },
    Press {
        idx: usize,
        velocity: f32,
    },
    Release {
        idx: usize,
    },
    /// Touch-sensitive fader touched/released.
    Touch {
        idx: usize,
        on: bool,
    },
    /// Program change (value = program number).
    Program {
        ch: u8,
        program: u8,
    },
}

#[derive(Clone, Copy, Default)]
struct ParamSel {
    msb: Option<u8>,
    lsb: Option<u8>,
    nrpn: bool,
    data_msb: u8,
}

impl ParamSel {
    fn param(&self) -> Option<u16> {
        let (m, l) = (self.msb?, self.lsb?);
        if !self.nrpn && m == 127 && l == 127 {
            return None; // RPN null
        }
        Some(((m as u16) << 7) | l as u16)
    }
}

/// Stateful decoder for one device.
pub struct Decoder {
    pub defs: Vec<ControlDef>,
    params: [ParamSel; 16],
    cc14_msb: Vec<(u8, u8, u8)>, // (ch, cc, msb)
    hui_zone: Option<u8>,
    /// Swallow HUI ping replies (`90 00 7F`).
    pub hui: bool,
    /// Auto-create controls for undeclared messages.
    pub auto: bool,
}

impl Decoder {
    pub fn new(defs: Vec<ControlDef>) -> Decoder {
        Decoder { defs, params: [ParamSel::default(); 16], cc14_msb: Vec::new(), hui_zone: None, hui: false, auto: true }
    }

    pub fn find(&self, name: &str) -> Option<usize> {
        self.defs.iter().position(|d| d.name == name)
    }

    fn auto_def(&mut self, ch: u8, kind: Kind, base: String) -> Option<usize> {
        if !self.auto {
            return None;
        }
        let name = if ch == 0 { base } else { format!("ch{}.{base}", ch + 1) };
        if let Some(i) = self.find(&name) {
            return Some(i);
        }
        let mut d = ControlDef::new(name, Some(ch), kind);
        d.auto = true;
        self.defs.push(d);
        Some(self.defs.len() - 1)
    }

    fn switch(&self, idx: usize, down: bool, velocity: f32, out: &mut Vec<Ev>) {
        match self.defs[idx].switch {
            SwitchMode::Momentary => {
                if down {
                    out.push(Ev::Press { idx, velocity });
                } else {
                    out.push(Ev::Release { idx });
                }
            }
            SwitchMode::Trigger => {
                out.push(Ev::Press { idx, velocity: 1.0 });
                out.push(Ev::Release { idx });
            }
        }
    }

    pub fn decode(&mut self, m: &Msg, out: &mut Vec<Ev>) {
        match *m {
            Msg::NoteOn { ch, note, vel } | Msg::NoteOff { ch, note, vel } => {
                let on = matches!(m, Msg::NoteOn { .. }) && vel > 0;
                if self.hui && ch == 0 && note == 0 {
                    return; // HUI ping reply
                }
                // MCU touch notes
                if let Some(idx) = self.defs.iter().position(|d| d.touch_note == Some(note) && d.matches_ch(0) && ch == 0) {
                    out.push(Ev::Touch { idx, on });
                    return;
                }
                let idx = match self.defs.iter().position(|d| d.kind == Kind::Note { note } && d.matches_ch(ch)) {
                    Some(i) => i,
                    None => match self.auto_def(ch, Kind::Note { note }, format!("note.{note}")) {
                        Some(i) => i,
                        None => return,
                    },
                };
                self.switch(idx, on, vel as f32 / 127.0, out);
            }
            Msg::Cc { ch, cc, value } => self.cc(ch, cc, value, out),
            Msg::PitchBend { ch, value } => {
                let idx = match self.defs.iter().position(|d| d.kind == Kind::PitchBend && d.matches_ch(ch)) {
                    Some(i) => i,
                    None => match self.auto_def(ch, Kind::PitchBend, "pb".into()) {
                        Some(i) => i,
                        None => return,
                    },
                };
                out.push(Ev::Value { idx, value: value as f32 / 16383.0, raw: value });
            }
            Msg::ChannelPressure { ch, value } => {
                let idx = match self.defs.iter().position(|d| d.kind == Kind::ChannelPressure && d.matches_ch(ch)) {
                    Some(i) => i,
                    None => match self.auto_def(ch, Kind::ChannelPressure, "pressure".into()) {
                        Some(i) => i,
                        None => return,
                    },
                };
                out.push(Ev::Value { idx, value: value as f32 / 127.0, raw: value as u16 });
            }
            Msg::Program { ch, program } => out.push(Ev::Program { ch, program }),
            _ => {}
        }
    }

    fn cc(&mut self, ch: u8, cc: u8, value: u8, out: &mut Vec<Ev>) {
        let chi = ch as usize & 15;
        // HUI switch protocol
        if self.hui && cc == 0x0F {
            self.hui_zone = Some(value);
            return;
        }
        if self.hui && cc == 0x2F {
            let Some(zone) = self.hui_zone else { return };
            let port = value & 0x0F;
            let down = value & 0x40 != 0;
            // strip touch: zone 0-7 port 0
            if let Some(idx) = self.defs.iter().position(|d| d.kind == Kind::HuiSwitch { zone, port }) {
                if self.defs[idx].name.ends_with(".touch") || self.defs[idx].name.starts_with("touch.") {
                    out.push(Ev::Touch { idx, on: down });
                } else {
                    self.switch(idx, down, 1.0, out);
                }
            } else if let Some(idx) = self.auto_def(0, Kind::HuiSwitch { zone, port }, format!("hui.{zone}.{port}")) {
                self.switch(idx, down, 1.0, out);
            }
            return;
        }
        // declared controls take precedence over NRPN/14-bit conventions
        for (idx, d) in self.defs.iter().enumerate() {
            if !d.matches_ch(ch) {
                continue;
            }
            match d.kind {
                Kind::Rel { cc: c, enc } if c == cc => {
                    let steps = enc.delta(value);
                    if steps != 0 {
                        out.push(Ev::Delta { idx, steps });
                    }
                    return;
                }
                Kind::Cc { cc: c } if c == cc => {
                    if d.button {
                        let down = value >= 64;
                        self.switch(idx, down, value as f32 / 127.0, out);
                    } else {
                        out.push(Ev::Value { idx, value: value as f32 / 127.0, raw: value as u16 });
                    }
                    return;
                }
                Kind::Cc14 { cc: c, lsb } if c == cc => {
                    match self.cc14_msb.iter_mut().find(|(h, k, _)| *h == ch && *k == cc) {
                        Some(e) => e.2 = value,
                        None => self.cc14_msb.push((ch, cc, value)),
                    }
                    let raw = (value as u16) << 7;
                    let _ = lsb;
                    out.push(Ev::Value { idx, value: raw as f32 / 16383.0, raw });
                    return;
                }
                Kind::Cc14 { cc: c, lsb } if lsb == cc => {
                    let msb = self.cc14_msb.iter().find(|(h, k, _)| *h == ch && *k == c).map(|e| e.2).unwrap_or(0);
                    let raw = ((msb as u16) << 7) | value as u16;
                    out.push(Ev::Value { idx, value: raw as f32 / 16383.0, raw });
                    return;
                }
                _ => {}
            }
        }
        // NRPN / RPN
        let sel = &mut self.params[chi];
        match cc {
            99 => {
                sel.msb = Some(value);
                sel.nrpn = true;
                return;
            }
            98 => {
                sel.lsb = Some(value);
                sel.nrpn = true;
                return;
            }
            101 => {
                sel.msb = Some(value);
                sel.nrpn = false;
                return;
            }
            100 => {
                sel.lsb = Some(value);
                sel.nrpn = false;
                return;
            }
            6 | 38 | 96 | 97 if sel.param().is_some() => {
                let s = *sel;
                let param = s.param().unwrap_or(0);
                let kind = if s.nrpn { Kind::Nrpn { param, hires: true } } else { Kind::Rpn { param } };
                let found = self.defs.iter().position(|d| {
                    d.matches_ch(ch)
                        && match (d.kind, kind) {
                            (Kind::Nrpn { param: a, .. }, Kind::Nrpn { param: b, .. }) => a == b,
                            (Kind::Rpn { param: a }, Kind::Rpn { param: b }) => a == b,
                            _ => false,
                        }
                });
                let idx = match found {
                    Some(i) => i,
                    None => match self.auto_def(ch, kind, if s.nrpn { format!("nrpn.{param}") } else { format!("rpn.{param}") }) {
                        Some(i) => i,
                        None => return,
                    },
                };
                let hires = !matches!(self.defs[idx].kind, Kind::Nrpn { hires: false, .. });
                match cc {
                    6 => {
                        self.params[chi].data_msb = value;
                        if hires {
                            let raw = (value as u16) << 7;
                            out.push(Ev::Value { idx, value: raw as f32 / 16383.0, raw });
                        } else {
                            out.push(Ev::Value { idx, value: value as f32 / 127.0, raw: value as u16 });
                        }
                    }
                    38 => {
                        if hires {
                            let raw = ((self.params[chi].data_msb as u16) << 7) | value as u16;
                            out.push(Ev::Value { idx, value: raw as f32 / 16383.0, raw });
                        }
                    }
                    96 => out.push(Ev::Delta { idx, steps: value.max(1) as i32 }),
                    _ => out.push(Ev::Delta { idx, steps: -(value.max(1) as i32) }),
                }
                return;
            }
            _ => {}
        }
        if let Some(idx) = self.auto_def(ch, Kind::Cc { cc }, format!("cc.{cc}")) {
            out.push(Ev::Value { idx, value: value as f32 / 127.0, raw: value as u16 });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(d: &mut Decoder, bytes: &[u8]) -> Vec<Ev> {
        let mut p = super::super::parse::Parser::default();
        let mut out = Vec::new();
        p.feed(bytes, |m| d.decode(&m, &mut out));
        out
    }

    #[test]
    fn relative_encodings() {
        assert_eq!(RelEnc::SignMag.delta(0x01), 1);
        assert_eq!(RelEnc::SignMag.delta(0x41), -1);
        assert_eq!(RelEnc::SignMag.delta(0x45), -5);
        assert_eq!(RelEnc::SignMagCw.delta(0x41), 1);
        assert_eq!(RelEnc::TwosComplement.delta(0x7F), -1);
        assert_eq!(RelEnc::TwosComplement.delta(0x03), 3);
        assert_eq!(RelEnc::Offset64.delta(62), -2);
    }

    #[test]
    fn cc14_pair_msb_then_lsb() {
        let mut d = Decoder::new(vec![ControlDef::new("fader", Some(0), Kind::Cc14 { cc: 7, lsb: 39 })]);
        let ev = run(&mut d, &[0xB0, 7, 0x40, 0xB0, 39, 0x7F]);
        assert_eq!(ev[0], Ev::Value { idx: 0, value: (0x40 << 7) as f32 / 16383.0, raw: 0x2000 });
        assert_eq!(ev[1], Ev::Value { idx: 0, value: 0x207F as f32 / 16383.0, raw: 0x207F });
        assert_eq!(ev.len(), 2);
    }

    #[test]
    fn nrpn_data_entry_and_increment() {
        let mut d = Decoder::new(vec![]);
        // select NRPN 1/2 (param 130), data MSB 64, LSB 1, increment
        let ev = run(&mut d, &[0xB0, 99, 1, 98, 2, 6, 64, 38, 1, 96, 0]);
        let idx = d.find("nrpn.130").unwrap();
        assert_eq!(
            ev,
            vec![Ev::Value { idx, value: 8192.0 / 16383.0, raw: 8192 }, Ev::Value { idx, value: 8193.0 / 16383.0, raw: 8193 }, Ev::Delta { idx, steps: 1 }]
        );
        // RPN null deselects: CC 6 falls through as a plain CC
        let ev = run(&mut d, &[0xB0, 101, 127, 100, 127, 6, 10]);
        assert_eq!(ev, vec![Ev::Value { idx: d.find("cc.6").unwrap(), value: 10.0 / 127.0, raw: 10 }]);
    }

    #[test]
    fn switch_modes() {
        let mut fs = ControlDef::new("fs_a", None, Kind::Cc { cc: 1 });
        fs.button = true;
        fs.switch = SwitchMode::Trigger;
        let mut mom = ControlDef::new("fs_b", None, Kind::Cc { cc: 2 });
        mom.button = true;
        let mut d = Decoder::new(vec![fs, mom]);
        // footswitch in toggle mode alternates 127/0: each message is one press
        assert_eq!(run(&mut d, &[0xB0, 1, 127, 0xB0, 1, 0]).iter().filter(|e| matches!(e, Ev::Press { .. })).count(), 2);
        assert_eq!(run(&mut d, &[0xB0, 2, 127, 2, 0]), vec![Ev::Press { idx: 1, velocity: 1.0 }, Ev::Release { idx: 1 }]);
    }

    #[test]
    fn note_on_velocity_zero_is_release_and_channels_are_named() {
        let mut d = Decoder::new(vec![]);
        let ev = run(&mut d, &[0x99, 36, 100, 0x99, 36, 0]);
        let idx = d.find("ch10.note.36").unwrap();
        assert_eq!(ev, vec![Ev::Press { idx, velocity: 100.0 / 127.0 }, Ev::Release { idx }]);
    }

    #[test]
    fn ring_and_led_encoding() {
        let mut b = Vec::new();
        Out::McuRing { strip: 2 }.encode(1.0, Led::Off, RingMode::Fill, &mut b);
        assert_eq!(b, vec![0xB0, 0x32, 0x2B]);
        b.clear();
        Out::McuRing { strip: 0 }.encode(0.5, Led::Off, RingMode::Dot, &mut b);
        assert_eq!(b, vec![0xB0, 0x30, 0x06]);
        b.clear();
        Out::NoteLed { ch: 0, note: 89 }.encode(0.0, Led::Blink, RingMode::Fill, &mut b);
        assert_eq!(b, vec![0x90, 89, 1]);
        b.clear();
        Out::PitchBend { ch: 8 }.encode(1.0, Led::Off, RingMode::Fill, &mut b);
        assert_eq!(b, vec![0xE8, 0x7F, 0x7F]);
        b.clear();
        Out::HuiLed { zone: 3, port: 2 }.encode(0.0, Led::On, RingMode::Fill, &mut b);
        assert_eq!(b, vec![0xB0, 0x0C, 3, 0xB0, 0x2C, 0x42]);
    }

    #[test]
    fn hui_switch_zone_port_and_ping_reply() {
        let mut d = Decoder::new(vec![ControlDef::new("touch.1", None, Kind::HuiSwitch { zone: 0, port: 0 })]);
        d.hui = true;
        let ev = run(&mut d, &[0x90, 0x00, 0x7F, 0xB0, 0x0F, 0x00, 0xB0, 0x2F, 0x40, 0xB0, 0x2F, 0x00]);
        assert_eq!(ev, vec![Ev::Touch { idx: 0, on: true }, Ev::Touch { idx: 0, on: false }]);
    }
}
