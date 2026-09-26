//! Engine addresses ↔ console parameters, and the coverage measured from a console's state.
//!
//! | Address (`mixer.<name>.…`)          | Console path              |
//! |-------------------------------------|---------------------------|
//! | `ch.N.fader\|mute\|pan\|name\|link` | `line/chN/volume\|mute\|pan\|username\|link` |
//! | `ret.N.*`, `fxret.N.*`, `tb.*`      | `return/chN`, `fxreturn/chN`, `talkback/ch1` |
//! | `aux.A.fader\|mute\|name`           | `aux/chA/volume\|mute\|username` |
//! | `aux.A.<strip>.send`                | `<strip path>/auxA`       |
//! | `fx.F.fader\|mute\|name`            | `fxbus/chF/…`             |
//! | `fx.F.<strip>.send`                 | `<strip path>/FX<letter>` |
//! | `fx.F.type`, `fx.F.param.P`         | `fx/chF/type`, `fx/chF/plugin/P` (readback) |
//! | `main.fader\|mute\|name`            | `main/ch1/…`              |

use crate::ucnet::tree::{ConsoleState, Leaf};
use se_proto::{Meta, Value, ValueType};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Strip {
    Line(u16),
    Return(u16),
    FxReturn(u16),
    Talkback,
    Aux(u16),
    FxBus(u16),
    Main,
}

impl Strip {
    pub fn path(self) -> String {
        match self {
            Strip::Line(n) => format!("line/ch{n}"),
            Strip::Return(n) => format!("return/ch{n}"),
            Strip::FxReturn(n) => format!("fxreturn/ch{n}"),
            Strip::Talkback => "talkback/ch1".into(),
            Strip::Aux(n) => format!("aux/ch{n}"),
            Strip::FxBus(n) => format!("fxbus/ch{n}"),
            Strip::Main => "main/ch1".into(),
        }
    }

    /// Address segment(s) below `mixer.<name>`.
    pub fn addr(self) -> String {
        match self {
            Strip::Line(n) => format!("ch.{n}"),
            Strip::Return(n) => format!("ret.{n}"),
            Strip::FxReturn(n) => format!("fxret.{n}"),
            Strip::Talkback => "tb".into(),
            Strip::Aux(n) => format!("aux.{n}"),
            Strip::FxBus(n) => format!("fx.{n}"),
            Strip::Main => "main".into(),
        }
    }

    /// Input strips feed the aux and FX buses.
    pub fn is_input(self) -> bool {
        matches!(self, Strip::Line(_) | Strip::Return(_) | Strip::FxReturn(_) | Strip::Talkback)
    }

    /// Output buses (main and monitor/aux, FX bus masters): level-safety relevant.
    pub fn is_output(self) -> bool {
        matches!(self, Strip::Aux(_) | Strip::FxBus(_) | Strip::Main)
    }

    fn from_path(group: &str, ch: &str) -> Option<Strip> {
        let n: u16 = ch.strip_prefix("ch")?.parse().ok()?;
        Some(match group {
            "line" => Strip::Line(n),
            "return" => Strip::Return(n),
            "fxreturn" => Strip::FxReturn(n),
            "talkback" if n == 1 => Strip::Talkback,
            "aux" => Strip::Aux(n),
            "fxbus" => Strip::FxBus(n),
            "main" if n == 1 => Strip::Main,
            _ => return None,
        })
    }

    fn from_addr(segs: &[&str]) -> Option<(Strip, usize)> {
        let num = |i: usize| segs.get(i).and_then(|s| s.parse::<u16>().ok()).filter(|n| *n > 0);
        Some(match *segs.first()? {
            "ch" => (Strip::Line(num(1)?), 2),
            "ret" => (Strip::Return(num(1)?), 2),
            "fxret" => (Strip::FxReturn(num(1)?), 2),
            "tb" => (Strip::Talkback, 1),
            "aux" => (Strip::Aux(num(1)?), 2),
            "fx" => (Strip::FxBus(num(1)?), 2),
            "main" => (Strip::Main, 1),
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Param {
    Fader,
    Mute,
    Pan,
    Name,
    Link,
    /// Send level from an input strip to aux bus N.
    SendAux(u16),
    /// Send level from an input strip to FX bus N (A = 1).
    SendFx(u16),
    /// FX processor type of FX bus N (readback).
    FxType,
    /// FX processor parameter (readback).
    FxParam(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 0–1 float (fader positions, sends, pans).
    Level,
    Bool,
    Text,
}

/// One console parameter the adapter knows.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Control {
    pub strip: Strip,
    pub param: Param,
}

impl Control {
    pub fn new(strip: Strip, param: Param) -> Control {
        Control { strip, param }
    }

    pub fn kind(&self) -> Kind {
        match self.param {
            Param::Mute | Param::Link => Kind::Bool,
            Param::Name | Param::FxType => Kind::Text,
            _ => Kind::Level,
        }
    }

    /// Whether the protocol lets us set it (names, links, and FX processor settings are
    /// readback only).
    pub fn settable(&self) -> bool {
        matches!(self.param, Param::Fader | Param::Mute | Param::Pan | Param::SendAux(_) | Param::SendFx(_))
    }

    pub fn path(&self) -> String {
        let s = self.strip.path();
        match &self.param {
            Param::Fader => format!("{s}/volume"),
            Param::Mute => format!("{s}/mute"),
            Param::Pan => format!("{s}/pan"),
            Param::Name => format!("{s}/username"),
            Param::Link => format!("{s}/link"),
            Param::SendAux(a) => format!("{s}/aux{a}"),
            Param::SendFx(f) => format!("{s}/FX{}", fx_letter(*f)),
            Param::FxType => match self.strip {
                Strip::FxBus(n) => format!("fx/ch{n}/type"),
                _ => format!("{s}/type"),
            },
            Param::FxParam(p) => match self.strip {
                Strip::FxBus(n) => format!("fx/ch{n}/plugin/{p}"),
                _ => format!("{s}/{p}"),
            },
        }
    }

    /// Address below the mixer prefix (`ch.3.fader`, `aux.2.ch.3.send`, `fx.1.param.predelay`).
    pub fn rel_addr(&self) -> String {
        let s = self.strip.addr();
        match &self.param {
            Param::Fader => format!("{s}.fader"),
            Param::Mute => format!("{s}.mute"),
            Param::Pan => format!("{s}.pan"),
            Param::Name => format!("{s}.name"),
            Param::Link => format!("{s}.link"),
            Param::SendAux(a) => format!("aux.{a}.{s}.send"),
            Param::SendFx(f) => format!("fx.{f}.{s}.send"),
            Param::FxType => format!("{s}.type"),
            Param::FxParam(p) => format!("{s}.param.{p}"),
        }
    }

    /// Parse a console path (`line/ch3/aux2`) into a control.
    pub fn from_path(path: &str) -> Option<Control> {
        let segs: Vec<&str> = path.split('/').collect();
        if segs.first() == Some(&"fx") {
            let n: u16 = segs.get(1)?.strip_prefix("ch")?.parse().ok()?;
            return match segs.as_slice() {
                [_, _, "type"] => Some(Control::new(Strip::FxBus(n), Param::FxType)),
                [_, _, "plugin", p] if valid_seg(p) => Some(Control::new(Strip::FxBus(n), Param::FxParam(p.to_string()))),
                _ => None,
            };
        }
        let [group, ch, param] = segs.as_slice() else { return None };
        let strip = Strip::from_path(group, ch)?;
        let p = match *param {
            "volume" => Param::Fader,
            "mute" => Param::Mute,
            "pan" => Param::Pan,
            "username" => Param::Name,
            "link" => Param::Link,
            other if strip.is_input() => {
                if let Some(a) = other.strip_prefix("aux").and_then(|a| a.parse::<u16>().ok()) {
                    Param::SendAux(a)
                } else {
                    let l = other.strip_prefix("FX").filter(|l| l.len() == 1)?;
                    Param::SendFx(fx_index(l.as_bytes()[0])?)
                }
            }
            _ => return None,
        };
        Some(Control::new(strip, p))
    }

    /// Parse an address below the mixer prefix.
    pub fn from_rel_addr(rel: &str) -> Option<Control> {
        let segs: Vec<&str> = rel.split('.').collect();
        let (strip, used) = Strip::from_addr(&segs)?;
        let rest = &segs[used..];
        let param = match rest {
            ["fader"] => Param::Fader,
            ["mute"] => Param::Mute,
            ["pan"] => Param::Pan,
            ["name"] => Param::Name,
            ["link"] => Param::Link,
            ["type"] if matches!(strip, Strip::FxBus(_)) => Param::FxType,
            ["param", p] if matches!(strip, Strip::FxBus(_)) => Param::FxParam(p.to_string()),
            _ => {
                // bus-relative send: aux.A.<strip>.send / fx.F.<strip>.send
                let (input, n) = Strip::from_addr(rest)?;
                if rest.get(n..) != Some(&["send"][..]) || !input.is_input() {
                    return None;
                }
                return match strip {
                    Strip::Aux(a) => Some(Control::new(input, Param::SendAux(a))),
                    Strip::FxBus(f) => Some(Control::new(input, Param::SendFx(f))),
                    _ => None,
                };
            }
        };
        let c = Control::new(strip, param);
        // parameters that do not exist on that strip kind
        match (&c.param, c.strip) {
            (Param::Pan, s) if s.is_output() && s != Strip::Main => None,
            (Param::Link, s) if s.is_output() => None,
            _ => Some(c),
        }
    }

    pub fn meta(&self, writable: bool) -> Meta {
        let m = match &self.param {
            Param::Fader => Meta::float(0.0, [0.0, 1.0]).describe("Fader position (console law: ≈0.72 = 0 dB, 1.0 = +10 dB, 0 = −∞)"),
            Param::Pan => Meta::float(0.5, [0.0, 1.0]).describe("Pan (0 = left, 0.5 = center, 1 = right)"),
            Param::SendAux(_) | Param::SendFx(_) => Meta::float(0.0, [0.0, 1.0]).describe("Send level (console law, like a fader)"),
            Param::FxParam(_) => Meta::float(0.0, [0.0, 1.0]).describe("FX processor parameter (normalized, readback)"),
            Param::Mute => Meta::boolean(false),
            Param::Link => Meta::boolean(false).describe("Stereo-linked with its neighbour"),
            Param::Name => Meta::string("").describe("Label set on the console"),
            Param::FxType => Meta { ty: ValueType::String, default: Value::Str(String::new()), ..Default::default() }.describe("FX processor type"),
        };
        let m = m.owner("mixer");
        if writable && self.settable() { m } else { m.readonly() }
    }

    /// Engine value from a console number.
    pub fn value_from(&self, n: f64) -> Value {
        match self.kind() {
            Kind::Bool => Value::Bool(n >= 0.5),
            _ => Value::Float(n),
        }
    }

    /// Console number from an engine value.
    pub fn number_of(&self, v: &Value) -> Option<f64> {
        match self.kind() {
            Kind::Bool => Some(if v.truthy() { 1.0 } else { 0.0 }),
            Kind::Level => v.as_f64().map(|f| f.clamp(0.0, 1.0)),
            Kind::Text => None,
        }
    }

    /// Two console values count as equal (fader positions come back quantized to 1/65535).
    pub fn same(&self, a: f64, b: f64) -> bool {
        match self.kind() {
            Kind::Bool => (a >= 0.5) == (b >= 0.5),
            _ => (a - b).abs() <= 2.0 / 65535.0,
        }
    }
}

fn valid_seg(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn fx_letter(n: u16) -> char {
    char::from(b'A' + (n.clamp(1, 26) - 1) as u8)
}

fn fx_index(letter: u8) -> Option<u16> {
    letter.is_ascii_uppercase().then(|| (letter - b'A') as u16 + 1)
}

/// What a connected console supports.
#[derive(Debug, Clone, Default)]
pub struct Coverage {
    pub lines: Vec<u16>,
    pub returns: Vec<u16>,
    pub fxreturns: Vec<u16>,
    pub talkback: bool,
    pub auxes: Vec<u16>,
    pub fxbuses: Vec<u16>,
    pub main: bool,
    /// Every control present on this console, in a stable order.
    pub controls: Vec<Control>,
}

impl Coverage {
    /// Measure from the state payload: a control is supported when its path exists and the
    /// bus it targets exists (consoles report aux1..aux32/FXA..FXH on every strip).
    pub fn measure(st: &ConsoleState) -> Coverage {
        let mut c = Coverage {
            lines: st.strips("line"),
            returns: st.strips("return"),
            fxreturns: st.strips("fxreturn"),
            talkback: st.has("talkback/ch1/volume"),
            auxes: st.strips("aux"),
            fxbuses: st.strips("fxbus"),
            main: st.has("main/ch1/volume"),
            controls: Vec::new(),
        };
        let mut inputs: Vec<Strip> = c.lines.iter().map(|n| Strip::Line(*n)).collect();
        inputs.extend(c.returns.iter().map(|n| Strip::Return(*n)));
        inputs.extend(c.fxreturns.iter().map(|n| Strip::FxReturn(*n)));
        if c.talkback {
            inputs.push(Strip::Talkback);
        }
        let mut outputs: Vec<Strip> = c.auxes.iter().map(|n| Strip::Aux(*n)).collect();
        outputs.extend(c.fxbuses.iter().map(|n| Strip::FxBus(*n)));
        if c.main {
            outputs.push(Strip::Main);
        }
        let mut controls: Vec<Control> = Vec::new();
        let mut push = |ctl: Control| {
            if st.has(&ctl.path()) {
                controls.push(ctl);
            }
        };
        for s in &inputs {
            for p in [Param::Name, Param::Fader, Param::Mute, Param::Pan, Param::Link] {
                push(Control::new(*s, p));
            }
        }
        for s in &outputs {
            for p in [Param::Name, Param::Fader, Param::Mute] {
                push(Control::new(*s, p));
            }
        }
        for a in c.auxes.clone() {
            for s in &inputs {
                push(Control::new(*s, Param::SendAux(a)));
            }
        }
        for f in c.fxbuses.clone() {
            for s in &inputs {
                push(Control::new(*s, Param::SendFx(f)));
            }
            push(Control::new(Strip::FxBus(f), Param::FxType));
            let prefix = format!("fx/ch{f}/plugin/");
            for k in st.values.keys().filter(|k| k.starts_with(&prefix)) {
                if let Some(ctl) = Control::from_path(k)
                    && matches!(st.values.get(k), Some(Leaf::Num(_)))
                {
                    push(ctl);
                }
            }
        }
        c.controls = controls;
        c
    }
}

/// Console fader law (reference `logVolumeToLinear`): dB → position 0–1.
pub fn db_to_fader(db: f64) -> f64 {
    if db <= -84.0 {
        return 0.0;
    }
    if db >= 10.0 {
        return 1.0;
    }
    let x = db;
    ((72.520_417_778_2 + 2.473_473_992 * x + 0.026_567_557 * x * x + 0.000_088_086_6 * x * x * x) / 100.0).clamp(0.0, 1.0)
}

/// Inverse of [`db_to_fader`]: position → dB (−∞ as −120 at the bottom).
pub fn fader_to_db(pos: f64) -> f64 {
    if pos <= 0.0 {
        return -120.0;
    }
    if pos >= 1.0 {
        return 10.0;
    }
    // the law is monotonic above its minimum at ≈ −73.1 dB
    let (mut lo, mut hi) = (-73.1f64, 10.0f64);
    for _ in 0..60 {
        let mid = 0.5 * (lo + hi);
        if db_to_fader(mid) < pos { lo = mid } else { hi = mid }
    }
    0.5 * (lo + hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_and_address_round_trip() {
        let cases = [
            ("line/ch3/volume", "ch.3.fader"),
            ("line/ch16/mute", "ch.16.mute"),
            ("line/ch1/pan", "ch.1.pan"),
            ("line/ch1/username", "ch.1.name"),
            ("line/ch10/aux1", "aux.1.ch.10.send"),
            ("line/ch2/FXB", "fx.2.ch.2.send"),
            ("return/ch1/aux3", "aux.3.ret.1.send"),
            ("fxreturn/ch2/aux6", "aux.6.fxret.2.send"),
            ("talkback/ch1/aux2", "aux.2.tb.send"),
            ("talkback/ch1/mute", "tb.mute"),
            ("aux/ch4/volume", "aux.4.fader"),
            ("fxbus/ch1/mute", "fx.1.mute"),
            ("main/ch1/volume", "main.fader"),
            ("fx/ch1/type", "fx.1.type"),
            ("fx/ch2/plugin/predelay", "fx.2.param.predelay"),
        ];
        for (path, addr) in cases {
            let c = Control::from_path(path).unwrap_or_else(|| panic!("{path}"));
            assert_eq!(c.path(), path);
            assert_eq!(c.rel_addr(), addr);
            assert_eq!(Control::from_rel_addr(addr), Some(c), "{addr}");
        }
    }

    #[test]
    fn unknown_or_invalid_paths_are_rejected() {
        for p in ["line/ch1/48v", "aux/ch1/aux2", "main/ch2/volume", "line/chx/volume", "line/ch1", "fx/ch1/plugin/a/b", "geq/ch1/gain"] {
            assert!(Control::from_path(p).is_none(), "{p}");
        }
        for a in ["ch.0.fader", "ch.1", "aux.1.aux.2.send", "aux.1.ch.3", "ch.1.param.x", "aux.1.pan", "main.link", "fx.1.ch.2.fader"] {
            assert!(Control::from_rel_addr(a).is_none(), "{a}");
        }
    }

    #[test]
    fn fader_law_round_trips_and_hits_unity() {
        assert!((db_to_fader(0.0) - 0.7252).abs() < 1e-3);
        assert_eq!(db_to_fader(10.0), 1.0);
        assert_eq!(db_to_fader(-90.0), 0.0);
        for db in [-60.0, -30.0, -10.0, -3.0, 0.0, 4.5, 9.9] {
            assert!((fader_to_db(db_to_fader(db)) - db).abs() < 1e-6, "{db}");
        }
        assert_eq!(fader_to_db(0.0), -120.0);
        // Kick on this 16R sat at 0.753 → a little above unity
        let kick = fader_to_db(0.7529);
        assert!(kick > 0.5 && kick < 2.0, "{kick}");
    }

    #[test]
    fn values_and_equality_follow_kind() {
        let mute = Control::from_path("line/ch1/mute").unwrap();
        assert_eq!(mute.value_from(1.0), Value::Bool(true));
        assert_eq!(mute.number_of(&Value::Bool(false)), Some(0.0));
        assert!(mute.same(1.0, 0.9));
        let fader = Control::from_path("line/ch1/volume").unwrap();
        assert!(fader.same(0.5, 32768.0 / 65535.0));
        assert!(!fader.same(0.5, 0.51));
        assert_eq!(fader.number_of(&Value::Float(1.4)), Some(1.0));
        assert!(!fader.meta(true).readonly);
        assert!(fader.meta(false).readonly);
        assert!(Control::from_path("line/ch1/username").unwrap().meta(true).readonly);
    }
}
