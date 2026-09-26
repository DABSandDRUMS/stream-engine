//! ALSA sequencer glue: event ↔ raw MIDI bytes, port enumeration, card identity
//! (USB port path and serial from sysfs/procfs).

use alsa::seq::{Addr, ClientIter, EvCtrl, EvNote, EvQueueControl, Event, EventType, PortCap, PortIter, Seq};
use std::collections::HashMap;

/// Append the MIDI bytes an incoming sequencer event represents (SysEx may arrive in chunks).
pub fn event_bytes(ev: &Event, out: &mut Vec<u8>) {
    let t = ev.get_type();
    let note = || ev.get_data::<EvNote>().unwrap_or_default();
    let ctrl = || ev.get_data::<EvCtrl>().unwrap_or_default();
    match t {
        EventType::Noteon | EventType::Note => {
            let n = note();
            out.extend_from_slice(&[0x90 | (n.channel & 15), n.note & 0x7F, n.velocity & 0x7F]);
        }
        EventType::Noteoff => {
            let n = note();
            out.extend_from_slice(&[0x80 | (n.channel & 15), n.note & 0x7F, n.velocity & 0x7F]);
        }
        EventType::Keypress => {
            let n = note();
            out.extend_from_slice(&[0xA0 | (n.channel & 15), n.note & 0x7F, n.velocity & 0x7F]);
        }
        EventType::Controller => {
            let c = ctrl();
            out.extend_from_slice(&[0xB0 | (c.channel & 15), (c.param & 0x7F) as u8, (c.value & 0x7F) as u8]);
        }
        EventType::Pgmchange => {
            let c = ctrl();
            out.extend_from_slice(&[0xC0 | (c.channel & 15), (c.value & 0x7F) as u8]);
        }
        EventType::Chanpress => {
            let c = ctrl();
            out.extend_from_slice(&[0xD0 | (c.channel & 15), (c.value & 0x7F) as u8]);
        }
        EventType::Pitchbend => {
            let c = ctrl();
            let v = (c.value + 8192).clamp(0, 16383) as u16;
            out.extend_from_slice(&[0xE0 | (c.channel & 15), (v & 0x7F) as u8, (v >> 7) as u8]);
        }
        EventType::Control14 => {
            let c = ctrl();
            let s = 0xB0 | (c.channel & 15);
            let v = c.value.clamp(0, 16383) as u16;
            if c.param < 32 {
                out.extend_from_slice(&[s, c.param as u8, (v >> 7) as u8, s, c.param as u8 + 32, (v & 0x7F) as u8]);
            } else {
                out.extend_from_slice(&[s, (c.param & 0x7F) as u8, (v & 0x7F) as u8]);
            }
        }
        EventType::Nonregparam | EventType::Regparam => {
            let c = ctrl();
            let s = 0xB0 | (c.channel & 15);
            let (m, l) = if t == EventType::Nonregparam { (99, 98) } else { (101, 100) };
            let v = c.value.clamp(0, 16383) as u16;
            out.extend_from_slice(&[s, m, ((c.param >> 7) & 0x7F) as u8, s, l, (c.param & 0x7F) as u8, s, 6, (v >> 7) as u8, s, 38, (v & 0x7F) as u8]);
        }
        EventType::Songpos => {
            let v = ctrl().value.clamp(0, 16383) as u16;
            out.extend_from_slice(&[0xF2, (v & 0x7F) as u8, (v >> 7) as u8]);
        }
        EventType::Songsel => out.extend_from_slice(&[0xF3, (ctrl().value & 0x7F) as u8]),
        EventType::Qframe => out.extend_from_slice(&[0xF1, (ctrl().value & 0x7F) as u8]),
        EventType::TuneRequest => out.push(0xF6),
        EventType::Clock => out.push(0xF8),
        EventType::Start => out.push(0xFA),
        EventType::Continue => out.push(0xFB),
        EventType::Stop => out.push(0xFC),
        EventType::Sensing => out.push(0xFE),
        EventType::Reset => out.push(0xFF),
        EventType::Sysex => {
            if let Some(b) = ev.get_ext() {
                out.extend_from_slice(b);
            }
        }
        _ => {}
    }
}

/// A sequencer event for one complete MIDI message.
pub fn msg_event(m: &super::parse::Msg) -> Option<Event<'static>> {
    use super::parse::Msg;
    let note = |t, ch: u8, n: u8, v: u8| Event::new(t, &EvNote { channel: ch, note: n, velocity: v, off_velocity: 0, duration: 0 });
    let ctrl = |t, ch: u8, p: u32, v: i32| Event::new(t, &EvCtrl { channel: ch, param: p, value: v });
    // transport realtime messages carry queue-control data (queue 0 = unused for direct sends)
    let qc = EvQueueControl { queue: 0, value: () };
    Some(match m {
        Msg::NoteOn { ch, note: n, vel } => note(EventType::Noteon, *ch, *n, *vel),
        Msg::NoteOff { ch, note: n, vel } => note(EventType::Noteoff, *ch, *n, *vel),
        Msg::PolyPressure { ch, note: n, value } => note(EventType::Keypress, *ch, *n, *value),
        Msg::Cc { ch, cc, value } => ctrl(EventType::Controller, *ch, *cc as u32, *value as i32),
        Msg::Program { ch, program } => ctrl(EventType::Pgmchange, *ch, 0, *program as i32),
        Msg::ChannelPressure { ch, value } => ctrl(EventType::Chanpress, *ch, 0, *value as i32),
        Msg::PitchBend { ch, value } => ctrl(EventType::Pitchbend, *ch, 0, *value as i32 - 8192),
        Msg::SysEx(b) => Event::new_ext(EventType::Sysex, b.clone()),
        Msg::QuarterFrame(v) => ctrl(EventType::Qframe, 0, 0, *v as i32),
        Msg::SongPosition(p) => ctrl(EventType::Songpos, 0, 0, *p as i32),
        Msg::SongSelect(s) => ctrl(EventType::Songsel, 0, 0, *s as i32),
        Msg::TuneRequest => Event::new(EventType::TuneRequest, &()),
        Msg::Realtime(0xF8) => Event::new(EventType::Clock, &qc),
        Msg::Realtime(0xFA) => Event::new(EventType::Start, &qc),
        Msg::Realtime(0xFB) => Event::new(EventType::Continue, &qc),
        Msg::Realtime(0xFC) => Event::new(EventType::Stop, &qc),
        Msg::Realtime(0xFE) => Event::new(EventType::Sensing, &()),
        Msg::Realtime(0xFF) => Event::new(EventType::Reset, &()),
        Msg::Realtime(_) => return None,
    })
}

/// One sequencer port of another client.
#[derive(Clone, Debug, PartialEq)]
pub struct SeqPort {
    pub addr: Addr,
    pub client_name: String,
    pub port_name: String,
    pub readable: bool,
    pub writable: bool,
    pub card: Option<i32>,
}

/// Enumerate ports we could talk to (skipping ourselves and the system client).
pub fn list_ports(seq: &Seq, own: &[i32]) -> Vec<SeqPort> {
    let mut v = Vec::new();
    for c in ClientIter::new(seq) {
        let id = c.get_client();
        if id == 0 || own.contains(&id) {
            continue;
        }
        let client_name = c.get_name().unwrap_or("").to_string();
        let card = c.get_card().ok().filter(|c| *c >= 0);
        for p in PortIter::new(seq, id) {
            let caps = p.get_capability();
            if caps.contains(PortCap::NO_EXPORT) {
                continue;
            }
            let readable = caps.contains(PortCap::READ | PortCap::SUBS_READ);
            let writable = caps.contains(PortCap::WRITE | PortCap::SUBS_WRITE);
            if !readable && !writable {
                continue;
            }
            v.push(SeqPort { addr: p.addr(), client_name: client_name.clone(), port_name: p.get_name().unwrap_or("").to_string(), readable, writable, card });
        }
    }
    v
}

/// USB identity of a sound card.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CardId {
    /// `usb-0000:0a:00.0-2.2` (as in `/proc/asound/cards`).
    pub usb: Option<String>,
    /// USB serial number (when the device reports a real one).
    pub serial: Option<String>,
}

/// Parse `/proc/asound/cards` → card number → USB port path.
pub fn parse_cards(src: &str) -> HashMap<i32, String> {
    let mut m = HashMap::new();
    let mut cur: Option<i32> = None;
    for line in src.lines() {
        let t = line.trim_start();
        if let Some((num, _)) = t.split_once(' ')
            && let Ok(n) = num.parse::<i32>()
            && t.contains('[')
        {
            cur = Some(n);
            continue;
        }
        if let Some(n) = cur
            && let Some(i) = t.find(" at usb-")
        {
            let rest = &t[i + 4..];
            let id = rest.split([',', ' ']).next().unwrap_or(rest);
            m.insert(n, id.to_string());
        }
    }
    m
}

pub fn card_ids() -> HashMap<i32, CardId> {
    let usb = std::fs::read_to_string("/proc/asound/cards").map(|s| parse_cards(&s)).unwrap_or_default();
    let mut out = HashMap::new();
    for (card, path) in usb {
        let serial = std::fs::read_to_string(format!("/sys/class/sound/card{card}/device/../serial"))
            .ok()
            .map(|s| s.trim().to_string())
            // version-like "serials" (X-TOUCH MINI reports 1.0.1) don't identify a unit
            .filter(|s| s.len() >= 6 && !s.chars().all(|c| c.is_ascii_digit() || c == '.'));
        out.insert(card, CardId { usb: Some(path), serial });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_cards_usb_paths() {
        let src = " 1 [MINI           ]: USB-Audio - X-TOUCH MINI\n                      Behringer X-TOUCH MINI at usb-0000:0a:00.0-2.2, full speed\n 3 [NVidia         ]: HDA-Intel - HDA NVidia\n                      HDA NVidia at 0xfc080000 irq 130\n 7 [II             ]: USB-Audio - FBV Express Mk II\n                      Line 6 FBV Express Mk II at usb-0000:0a:00.0-9.2, full speed\n";
        let m = parse_cards(src);
        assert_eq!(m.get(&1).map(String::as_str), Some("usb-0000:0a:00.0-2.2"));
        assert_eq!(m.get(&7).map(String::as_str), Some("usb-0000:0a:00.0-9.2"));
        assert!(!m.contains_key(&3));
    }

    #[test]
    fn seq_events_round_trip_through_bytes() {
        use crate::midi::parse::{Msg, Parser};
        let msgs = vec![
            Msg::NoteOn { ch: 9, note: 36, vel: 100 },
            Msg::Cc { ch: 0, cc: 16, value: 0x41 },
            Msg::PitchBend { ch: 8, value: 12345 },
            Msg::QuarterFrame(0x35),
            Msg::SysEx(vec![0xF0, 0x7F, 0x7F, 0x01, 0x01, 0x21, 0x02, 0x03, 0x04, 0xF7]),
            Msg::Realtime(0xF8),
        ];
        for m in msgs {
            let ev = msg_event(&m).unwrap();
            let mut b = Vec::new();
            event_bytes(&ev, &mut b);
            let mut p = Parser::default();
            let mut got = Vec::new();
            p.feed(&b, |x| got.push(x));
            assert_eq!(got, vec![m.clone()], "{m:?}");
        }
    }
}
