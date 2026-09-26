//! Mackie Control (MCU) and HUI protocol layer: control tables for the built-in profiles
//! and encoders for the displays (scribble strips, meters, timecode/assignment digits).

use super::controls::{ControlDef, Kind, Out, RelEnc};

/// MCU note names 40–101 (global buttons), indexed by `note - 40`.
const MCU_BUTTONS: &[&str] = &[
    "assign.track",
    "assign.send",
    "assign.pan",
    "assign.plugin",
    "assign.eq",
    "assign.instrument",
    "bank.left",
    "bank.right",
    "channel.left",
    "channel.right",
    "flip",
    "global",
    "name_value",
    "smpte_beats",
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "view.midi",
    "view.inputs",
    "view.audio",
    "view.instrument",
    "view.aux",
    "view.busses",
    "view.outputs",
    "view.user",
    "shift",
    "option",
    "control",
    "alt",
    "auto.read",
    "auto.write",
    "auto.trim",
    "auto.touch",
    "auto.latch",
    "group",
    "save",
    "undo",
    "cancel",
    "enter",
    "marker",
    "nudge",
    "cycle",
    "drop",
    "replace",
    "click",
    "solo_mode",
    "rewind",
    "forward",
    "stop",
    "play",
    "record",
    "up",
    "down",
    "left",
    "right",
    "zoom",
    "scrub",
];

/// MCU bank/channel navigation notes.
pub const NOTE_BANK_LEFT: u8 = 46;
pub const NOTE_BANK_RIGHT: u8 = 47;
pub const NOTE_CHANNEL_LEFT: u8 = 48;
pub const NOTE_CHANNEL_RIGHT: u8 = 49;

fn led(name: String, note: u8) -> ControlDef {
    ControlDef::new(name, Some(0), Kind::Note { note }).out(Out::NoteLed { ch: 0, note })
}

/// A full Mackie Control surface: 8 strips (fader, touch, V-Pot + ring, rec/solo/mute/select
/// LEDs, V-Pot push), the master fader, global buttons, and the jog wheel.
pub fn mcu_controls(motors: bool) -> Vec<ControlDef> {
    let mut v = Vec::new();
    for i in 0..8u8 {
        let n = i + 1;
        let mut f = ControlDef::new(format!("fader.{n}"), Some(i), Kind::PitchBend).out(Out::PitchBend { ch: i });
        if motors {
            f = f.motor(Some(104 + i));
        }
        v.push(f);
        v.push(ControlDef::new(format!("vpot.{n}"), Some(0), Kind::Rel { cc: 16 + i, enc: RelEnc::SignMag }).out(Out::McuRing { strip: i }));
        v.push(led(format!("rec.{n}"), i));
        v.push(led(format!("solo.{n}"), 8 + i));
        v.push(led(format!("mute.{n}"), 16 + i));
        v.push(led(format!("select.{n}"), 24 + i));
        v.push(ControlDef::new(format!("vsel.{n}"), Some(0), Kind::Note { note: 32 + i }));
    }
    let mut master = ControlDef::new("fader.master", Some(8), Kind::PitchBend).out(Out::PitchBend { ch: 8 });
    if motors {
        master = master.motor(Some(112));
    }
    v.push(master);
    for (k, name) in MCU_BUTTONS.iter().enumerate() {
        v.push(led(name.to_string(), 40 + k as u8));
    }
    v.push(ControlDef::new("jog", Some(0), Kind::Rel { cc: 0x3C, enc: RelEnc::SignMag }));
    v
}

/// Behringer X-TOUCH MINI in MC mode: 8 encoders (relative V-Pots with LED rings),
/// encoder pushes, two rows of 8 LED buttons, layer A/B, and a non-motorized master fader
/// (pitch-bend on MIDI channel 9).
pub fn xtouch_mini_mc() -> Vec<ControlDef> {
    const TOP: [u8; 8] = [89, 90, 40, 41, 42, 43, 44, 45];
    const BOTTOM: [u8; 8] = [87, 88, 91, 92, 86, 93, 94, 95];
    let mut v = Vec::new();
    for i in 0..8u8 {
        v.push(ControlDef::new(format!("enc.{}", i + 1), Some(0), Kind::Rel { cc: 16 + i, enc: RelEnc::SignMag }).out(Out::McuRing { strip: i }));
        v.push(ControlDef::new(format!("push.{}", i + 1), Some(0), Kind::Note { note: 32 + i }));
    }
    for (i, n) in TOP.iter().chain(BOTTOM.iter()).enumerate() {
        v.push(led(format!("btn.{}", i + 1), *n));
    }
    v.push(led("layer.a".into(), 84));
    v.push(led("layer.b".into(), 85));
    v.push(ControlDef::new("fader", Some(8), Kind::PitchBend));
    v
}

/// Put the X-TOUCH MINI into MC mode (operation-mode select, CC 127 on the global channel).
pub const XTOUCH_MINI_MC_MODE: [u8; 3] = [0xB0, 0x7F, 0x01];

/// HUI: 8 strips of fader (14-bit CC pair), touch, V-Pot (+ ring), and strip switches with LEDs.
pub fn hui_controls() -> Vec<ControlDef> {
    const STRIP_PORTS: [&str; 8] = ["touch", "select", "mute", "solo", "auto", "vsel", "insert", "rec"];
    let mut v = Vec::new();
    for i in 0..8u8 {
        let n = i + 1;
        v.push(ControlDef::new(format!("fader.{n}"), Some(0), Kind::Cc14 { cc: i, lsb: 0x20 + i }).out(Out::HuiFader { strip: i }).motor(None));
        v.push(ControlDef::new(format!("vpot.{n}"), Some(0), Kind::Rel { cc: 0x40 + i, enc: RelEnc::SignMagCw }).out(Out::HuiRing { strip: i }));
        for (port, name) in STRIP_PORTS.iter().enumerate() {
            let mut d = ControlDef::new(format!("{name}.{n}"), Some(0), Kind::HuiSwitch { zone: i, port: port as u8 });
            if port > 0 {
                d = d.out(Out::HuiLed { zone: i, port: port as u8 });
            }
            v.push(d);
        }
    }
    v
}

/// HUI host ping (the surface replies `90 00 7F` and goes offline without it).
pub const HUI_PING: [u8; 3] = [0x90, 0x00, 0x00];

/// MCU device ids for SysEx: main unit and extender.
pub const MCU_MAIN: u8 = 0x14;
pub const MCU_EXTENDER: u8 = 0x15;

/// Scribble strip text for one strip (7 characters, row 0 = top, row 1 = bottom).
pub fn mcu_scribble(device: u8, strip: u8, row: u8, text: &str, out: &mut Vec<u8>) {
    let offset = row.min(1) * 56 + strip.min(7) * 7;
    out.extend_from_slice(&[0xF0, 0x00, 0x00, 0x66, device, 0x12, offset]);
    let mut n = 0;
    for c in text.chars().take(7) {
        out.push(ascii7(c));
        n += 1;
    }
    for _ in n..7 {
        out.push(b' ');
    }
    out.push(0xF7);
}

/// Level meter for a strip: level 0–1 → 0–12 (0x0C = clip LED zone).
pub fn mcu_meter(strip: u8, level: f32, out: &mut Vec<u8>) {
    let l = (level.clamp(0.0, 1.0) * 12.0).round() as u8;
    out.extend_from_slice(&[0xD0, ((strip & 7) << 4) | l]);
}

/// Timecode/beats display: 10 digits, rightmost first on CC 0x40.. (a `.` after a character
/// lights that digit's dot).
pub fn mcu_timecode(text: &str, out: &mut Vec<u8>) {
    let mut cells: Vec<(u8, bool)> = Vec::new();
    for c in text.chars() {
        if c == '.' || c == ':' {
            if let Some(last) = cells.last_mut() {
                last.1 = true;
            }
            continue;
        }
        cells.push((seven_seg(c), false));
    }
    let n = cells.len().min(10);
    for (i, (code, dot)) in cells[cells.len() - n..].iter().rev().enumerate() {
        out.extend_from_slice(&[0xB0, 0x40 + i as u8, code | if *dot { 0x40 } else { 0 }]);
    }
}

/// Two-character assignment display (CC 0x4B left, 0x4A right).
pub fn mcu_assignment(text: &str, out: &mut Vec<u8>) {
    let mut c = text.chars();
    let l = c.next().unwrap_or(' ');
    let r = c.next().unwrap_or(' ');
    out.extend_from_slice(&[0xB0, 0x4B, seven_seg(l), 0xB0, 0x4A, seven_seg(r)]);
}

/// HUI 4-character scribble strip.
pub fn hui_scribble(strip: u8, text: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(&[0xF0, 0x00, 0x00, 0x66, 0x05, 0x00, 0x10, strip & 7]);
    let mut n = 0;
    for c in text.chars().take(4) {
        out.push(ascii7(c));
        n += 1;
    }
    for _ in n..4 {
        out.push(b' ');
    }
    out.push(0xF7);
}

/// HUI meter: side 0 = left, 1 = right; level 0–1 → 0–12.
pub fn hui_meter(strip: u8, side: u8, level: f32, out: &mut Vec<u8>) {
    let l = (level.clamp(0.0, 1.0) * 12.0).round() as u8;
    out.extend_from_slice(&[0xA0, strip & 7, ((side & 1) << 4) | l]);
}

fn ascii7(c: char) -> u8 {
    if c.is_ascii() && !c.is_ascii_control() { c as u8 } else { b'?' }
}

/// MCU 7-segment character code: ASCII 0x40–0x5F → 0x00–0x1F, 0x20–0x3F unchanged.
fn seven_seg(c: char) -> u8 {
    let u = c.to_ascii_uppercase();
    match u as u32 {
        0x40..=0x5F => (u as u8) - 0x40,
        0x20..=0x3F => u as u8,
        _ => 0x20,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scribble_strip_offsets_and_padding() {
        let mut b = Vec::new();
        mcu_scribble(MCU_MAIN, 1, 1, "Kick", &mut b);
        assert_eq!(b, vec![0xF0, 0, 0, 0x66, 0x14, 0x12, 63, b'K', b'i', b'c', b'k', b' ', b' ', b' ', 0xF7]);
    }

    #[test]
    fn meter_and_displays() {
        let mut b = Vec::new();
        mcu_meter(3, 1.0, &mut b);
        assert_eq!(b, vec![0xD0, 0x3C]);
        b.clear();
        mcu_timecode("01:02", &mut b);
        // rightmost first: '2', '0', '1' (with dot), '0'
        assert_eq!(b, vec![0xB0, 0x40, b'2', 0xB0, 0x41, b'0', 0xB0, 0x42, b'1' | 0x40, 0xB0, 0x43, b'0']);
        b.clear();
        mcu_assignment("A1", &mut b);
        assert_eq!(b, vec![0xB0, 0x4B, 0x01, 0xB0, 0x4A, b'1']);
    }

    #[test]
    fn profiles_have_unique_names_and_inputs() {
        for defs in [mcu_controls(true), xtouch_mini_mc(), hui_controls()] {
            let mut names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
            names.sort();
            let n = names.len();
            names.dedup();
            assert_eq!(n, names.len());
        }
        assert_eq!(xtouch_mini_mc().iter().filter(|d| d.name.starts_with("btn.")).count(), 16);
        assert!(mcu_controls(true).iter().any(|d| d.name == "fader.1" && d.touch_note == Some(104)));
    }
}
