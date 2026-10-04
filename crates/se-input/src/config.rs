//! Typed view of `controllers/*.toml` (decks, MIDI devices, mappings) and `project.toml [voice]`.
//!
//! A controllers file is one of:
//! * `kind = "deck"` (or has `[page.*]`): a Stream Deck with pages of keys;
//! * `kind = "midi"` (or has `match`): a MIDI device with a profile, controls, `[[map]]`
//!   mappings, optional `[bank]` (MCU/HUI strips) and `[drums]` (e-drum notes);
//! * only `[[binding]]` entries: core bindings (the core parses those in every controllers file).

use crate::midi::controls::{ControlDef, Kind, Out, RelEnc, RingMode, SwitchMode};
use crate::midi::mcu;
use se_proto::parse_duration_ms;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// What a key or button does.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Action {
    #[default]
    None,
    /// Fire a preset (the core handles `toggle` presets and conflicts).
    Preset(String),
    /// Scene to preview (`cut` = straight to program).
    Scene { name: String, cut: bool },
    /// Flip a boolean address.
    Toggle(String),
    /// Hold an address at its maximum (or `true`) while held.
    Momentary(String),
    /// Command lists on press and release.
    Commands { press: Vec<String>, release: Vec<String> },
    /// Deck page (`next`/`prev` or a name).
    Page(String),
    /// Voice push-to-talk.
    Ptt,
}

impl Action {
    /// Parse the action keys of a key/map table.
    pub fn from_table(t: &toml::Table) -> Result<Action, String> {
        let s = |k: &str| t.get(k).and_then(|v| v.as_str()).map(String::from);
        let list = |k: &str| -> Result<Vec<String>, String> {
            match t.get(k) {
                None => Ok(Vec::new()),
                Some(toml::Value::String(s)) => Ok(vec![s.clone()]),
                Some(toml::Value::Array(a)) => {
                    a.iter().map(|v| v.as_str().map(String::from).ok_or_else(|| format!("`{k}` must be a list of commands"))).collect()
                }
                Some(_) => Err(format!("`{k}` must be a command or a list of commands")),
            }
        };
        let mut found: Vec<Action> = Vec::new();
        if let Some(p) = s("preset") {
            found.push(Action::Preset(p));
        }
        if let Some(n) = s("scene") {
            found.push(Action::Scene { name: n, cut: t.get("cut").and_then(|v| v.as_bool()).unwrap_or(false) });
        }
        if let Some(a) = s("toggle") {
            found.push(Action::Toggle(a));
        }
        if let Some(a) = s("momentary") {
            found.push(Action::Momentary(a));
        }
        if t.contains_key("do") || t.contains_key("release") {
            let press = list("do")?;
            let release = list("release")?;
            for c in press.iter().chain(&release) {
                se_proto::Op::parse(c).map_err(|e| format!("command `{c}`: {e}"))?;
            }
            found.push(Action::Commands { press, release });
        }
        if let Some(p) = s("page") {
            found.push(Action::Page(p));
        }
        if t.get("ptt").and_then(|v| v.as_bool()).unwrap_or(false) {
            found.push(Action::Ptt);
        }
        match found.len() {
            0 => Ok(Action::None),
            1 => Ok(found.pop().unwrap_or_default()),
            _ => Err("more than one of preset/scene/toggle/momentary/do/page/ptt".into()),
        }
    }

    /// One-line command the action runs on press (for display and the UI pad grid).
    pub fn describe(&self) -> String {
        match self {
            Action::None => String::new(),
            Action::Preset(p) => format!("preset.fire {p}"),
            Action::Scene { name, cut: false } => format!("scene.go {name}"),
            Action::Scene { name, cut: true } => format!("scene.cut {name}"),
            Action::Toggle(a) => format!("toggle {a}"),
            Action::Momentary(a) => format!("hold {a}"),
            Action::Commands { press, .. } => press.join("; "),
            Action::Page(p) => format!("deck.page {p}"),
            Action::Ptt => "voice.ptt".into(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Action::None => "empty",
            Action::Preset(_) => "preset",
            Action::Scene { .. } => "scene",
            Action::Toggle(_) => "toggle",
            Action::Momentary(_) => "momentary",
            Action::Commands { .. } => "action",
            Action::Page(_) => "page",
            Action::Ptt => "ptt",
        }
    }

    /// Needs press *and* release (momentary controls).
    pub fn wants_release(&self) -> bool {
        matches!(self, Action::Momentary(_) | Action::Ptt) || matches!(self, Action::Commands { release, .. } if !release.is_empty())
    }
}

/// Presentation and safety options shared by deck keys and MIDI buttons.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Behavior {
    pub label: Option<String>,
    pub icon: Option<String>,
    pub color: Option<String>,
    /// Address whose truthiness lights the key/LED (overrides the action's own state).
    pub state: Option<String>,
    /// Ignore presses for this long after firing (shown as a sweep on the key).
    pub cooldown_ms: u64,
    /// Press twice within 3 s to run.
    pub confirm: Option<bool>,
    /// Hold this long to run (hold-to-confirm, e.g. panic).
    pub hold_ms: u64,
}

impl Behavior {
    fn from_table(t: &toml::Table) -> Result<Behavior, String> {
        let s = |k: &str| t.get(k).and_then(|v| v.as_str()).map(String::from);
        let dur = |k: &str| -> Result<u64, String> {
            match t.get(k) {
                None => Ok(0),
                Some(toml::Value::String(s)) => parse_duration_ms(s).ok_or_else(|| format!("bad duration `{s}` for `{k}`")),
                Some(toml::Value::Integer(i)) => Ok((*i).max(0) as u64),
                Some(toml::Value::Float(f)) => Ok((f * 1000.0).max(0.0) as u64),
                Some(_) => Err(format!("`{k}` must be a duration")),
            }
        };
        Ok(Behavior {
            label: s("label"),
            icon: s("icon"),
            color: s("color"),
            state: s("state"),
            cooldown_ms: dur("cooldown")?,
            confirm: t.get("confirm").and_then(|v| v.as_bool()),
            hold_ms: dur("hold")?,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct KeyDef {
    pub action: Action,
    pub b: Behavior,
    pub image: Option<Arc<crate::deck::render::Artwork>>,
    /// Display-only local time; never an action.
    pub clock: bool,
    /// Unavailable controls keep their presentation but cannot run actions.
    pub disabled: bool,
}

impl KeyDef {
    pub fn available(&self) -> bool {
        !self.disabled && !self.clock
    }

    pub fn from_table(t: &toml::Table, root: &Path) -> Result<Self, String> {
        Self::from_table_cached(t, root, &mut BTreeMap::new())
    }

    fn from_table_cached(t: &toml::Table, root: &Path, artwork: &mut BTreeMap<String, Arc<crate::deck::render::Artwork>>) -> Result<Self, String> {
        let action = Action::from_table(t)?;
        let b = Behavior::from_table(t)?;
        let boolean = |key: &str| match t.get(key) {
            None => Ok(false),
            Some(toml::Value::Boolean(value)) => Ok(*value),
            Some(_) => Err(format!("`{key}` must be a boolean")),
        };
        let clock = boolean("clock")?;
        let disabled = boolean("disabled")?;
        if clock && action != Action::None {
            return Err("`clock = true` is display-only; remove the key's action".into());
        }
        let image = match t.get("image") {
            None => None,
            Some(toml::Value::String(path)) => {
                if let Some(image) = artwork.get(path) {
                    Some(image.clone())
                } else {
                    let image = Arc::new(crate::deck::render::Artwork::load(root, path)?);
                    artwork.insert(path.clone(), image.clone());
                    Some(image)
                }
            }
            Some(_) => return Err("`image` must be a project-relative PNG path".into()),
        };
        Ok(Self { action, b, image, clock, disabled })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub name: String,
    pub label: String,
    pub keys: BTreeMap<u8, KeyDef>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeckCfg {
    pub id: String,
    pub file: String,
    /// Bind to this unit (serial number); `None` = first unclaimed deck.
    pub serial: Option<String>,
    pub brightness: u8,
    pub pages: Vec<Page>,
    pub start_page: String,
    /// Mirrors into `controllers.page` (the UI pad grid follows it).
    pub primary: bool,
}

impl DeckCfg {
    pub fn page(&self, name: &str) -> Option<&Page> {
        self.pages.iter().find(|p| p.name == name)
    }
}

/// Built-in control layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Profile {
    #[default]
    Generic,
    Mcu {
        motors: bool,
    },
    XTouchMiniMc,
    Hui,
}

impl Profile {
    pub fn name(self) -> &'static str {
        match self {
            Profile::Generic => "generic",
            Profile::Mcu { .. } => "mcu",
            Profile::XTouchMiniMc => "xtouch_mini_mc",
            Profile::Hui => "hui",
        }
    }
    pub fn is_mcu(self) -> bool {
        matches!(self, Profile::Mcu { .. } | Profile::XTouchMiniMc)
    }
}

/// One `[[map]]`: what a control does beyond its signal/event.
#[derive(Clone, Debug, PartialEq)]
pub struct MapDef {
    pub control: String,
    /// Button action (presets, scenes, toggles, commands, pages, push-to-talk).
    pub action: Action,
    pub b: Behavior,
    /// Relative encoder → address (`set` at manual priority).
    pub target: Option<String>,
    /// Per-detent step (default: 1/100 of the range).
    pub step: Option<f64>,
    /// Range override (default: the address metadata, else 0–1).
    pub range: Option<[f64; 2]>,
    pub ring: RingMode,
    /// Relative encoder → command lists per detent.
    pub inc: Vec<String>,
    pub dec: Vec<String>,
    /// Send state changes back (LED ring, button LED, motor fader).
    pub feedback: bool,
}

/// MCU/HUI strip banks: strips 1–8 show `offset + 1 ..= offset + 8` of `count` channels.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct BankCfg {
    pub size: u32,
    pub count: u32,
    /// Address templates with `{n}` (1-based channel number).
    pub fader: Option<String>,
    pub vpot: Option<String>,
    pub select: Option<String>,
    pub mute: Option<String>,
    pub solo: Option<String>,
    pub rec: Option<String>,
    /// Scribble-strip text source (address with a string value; falls back to `CH n`).
    pub name: Option<String>,
    /// Meter signal template.
    pub meter: Option<String>,
    pub ring: RingMode,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MidiDeviceCfg {
    pub id: String,
    pub file: String,
    /// Glob against the ALSA client name (and `client:port`).
    pub matcher: String,
    /// Glob against the port name when the client has several ports.
    pub port: Option<String>,
    /// USB path (`usb-0000:0a:00.0-2.2`) or serial to tell identical devices apart.
    pub usb: Option<String>,
    pub serial: Option<String>,
    pub profile: Profile,
    pub controls: Vec<ControlDef>,
    pub maps: Vec<MapDef>,
    pub bank: Option<BankCfg>,
    /// Note → pad name for `drums.<pad>` events.
    pub drums: BTreeMap<u8, String>,
    pub drum_channel: Option<u8>,
    /// Bytes sent on every connect.
    pub init: Vec<Vec<u8>>,
    /// Name undeclared messages automatically (`cc.7`, `note.36`, …).
    pub auto_controls: bool,
    /// Signal flush interval (CC coalescing).
    pub coalesce_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VoiceCfg {
    pub enabled: bool,
    /// ALSA capture PCM.
    pub device: String,
    /// Model name (`base.en`, `tiny.en`, `small.en`) or a path to a ggml file.
    pub model: String,
    pub max_seconds: f32,
    /// Intent kinds that need confirmation.
    pub confirm: Vec<String>,
    pub threads: i32,
    pub gpu: bool,
    pub confirm_timeout_ms: u64,
}

impl Default for VoiceCfg {
    fn default() -> Self {
        VoiceCfg {
            enabled: true,
            device: "default".into(),
            model: "base.en".into(),
            max_seconds: 12.0,
            confirm: vec!["panic".into(), "mode".into(), "clean".into()],
            threads: 4,
            gpu: false,
            confirm_timeout_ms: 10_000,
        }
    }
}

impl VoiceCfg {
    pub fn from_value(v: Option<&toml::Value>) -> Result<VoiceCfg, String> {
        let mut c = VoiceCfg::default();
        let Some(t) = v.and_then(|v| v.as_table()) else { return Ok(c) };
        if let Some(b) = t.get("enabled").and_then(|v| v.as_bool()) {
            c.enabled = b;
        }
        if let Some(s) = t.get("device").and_then(|v| v.as_str()) {
            c.device = s.into();
        }
        if let Some(s) = t.get("model").and_then(|v| v.as_str()) {
            c.model = s.into();
        }
        if let Some(n) = num(t.get("max_seconds")) {
            c.max_seconds = n.clamp(1.0, 30.0) as f32;
        }
        if let Some(a) = t.get("confirm").and_then(|v| v.as_array()) {
            c.confirm = a.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        if let Some(n) = t.get("threads").and_then(|v| v.as_integer()) {
            c.threads = n.clamp(1, 32) as i32;
        }
        if let Some(b) = t.get("gpu").and_then(|v| v.as_bool()) {
            c.gpu = b;
        }
        if let Some(s) = t.get("confirm_timeout").and_then(|v| v.as_str()) {
            c.confirm_timeout_ms = parse_duration_ms(s).ok_or_else(|| format!("[voice] bad confirm_timeout `{s}`"))?;
        }
        Ok(c)
    }
}

fn num(v: Option<&toml::Value>) -> Option<f64> {
    v.and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
}

/// All controllers files, parsed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Controllers {
    pub decks: Vec<DeckCfg>,
    pub midi: Vec<MidiDeviceCfg>,
}

/// Parse every controllers file. A file that fails keeps its entry from `last_good` (keyed by
/// file name); errors are returned for reporting. `last_good` is updated with fresh results.
pub fn parse_all(
    files: &BTreeMap<String, toml::Table>,
    paths: &BTreeMap<String, String>,
    root: &Path,
    last_good: &mut BTreeMap<String, Parsed>,
) -> (Controllers, Vec<(String, String)>) {
    let mut errors = Vec::new();
    let mut parsed: Vec<Parsed> = Vec::new();
    last_good.retain(|name, _| files.contains_key(name));
    for (name, t) in files {
        let file = paths.get(&format!("controllers/{name}")).cloned().unwrap_or_else(|| format!("controllers/{name}.toml"));
        match parse_file_at(name, &file, t, root) {
            Ok(p) => {
                last_good.insert(name.clone(), p.clone());
                parsed.push(p);
            }
            Err(e) => {
                errors.push((file, e));
                if let Some(p) = last_good.get(name) {
                    parsed.push(p.clone());
                }
            }
        }
    }
    (build(parsed), errors)
}

/// Assemble parsed files (the first deck is primary unless one says `primary = true`).
pub fn build(parsed: Vec<Parsed>) -> Controllers {
    let mut c = Controllers::default();
    for p in parsed {
        match p {
            Parsed::Deck(d) => c.decks.push(d),
            Parsed::Midi(m) => c.midi.push(*m),
            Parsed::BindingsOnly => {}
        }
    }
    if !c.decks.iter().any(|d| d.primary)
        && let Some(d) = c.decks.first_mut()
    {
        d.primary = true;
    }
    c
}

#[derive(Clone, Debug, PartialEq)]
pub enum Parsed {
    Deck(DeckCfg),
    Midi(Box<MidiDeviceCfg>),
    BindingsOnly,
}

/// State segments under `controllers.` that a deck id may not use.
const RESERVED: &[&str] = &["page", "midi", "learn", "voice"];

pub fn parse_file(name: &str, file: &str, t: &toml::Table) -> Result<Parsed, String> {
    let root = Path::new(file).parent().and_then(Path::parent).unwrap_or_else(|| Path::new("."));
    parse_file_at(name, file, t, root)
}

fn parse_file_at(name: &str, file: &str, t: &toml::Table, root: &Path) -> Result<Parsed, String> {
    let kind = t.get("kind").and_then(|v| v.as_str());
    match kind {
        Some("deck") => parse_deck(name, file, t, root).map(Parsed::Deck),
        Some("midi") => parse_midi(name, file, t).map(|m| Parsed::Midi(Box::new(m))),
        Some(k) => Err(format!("unknown controllers kind `{k}` (deck | midi)")),
        None if t.contains_key("page") => parse_deck(name, file, t, root).map(Parsed::Deck),
        None if t.contains_key("match") => parse_midi(name, file, t).map(|m| Parsed::Midi(Box::new(m))),
        None if t.contains_key("binding") => Ok(Parsed::BindingsOnly),
        None => Err("set `kind = \"deck\"` or `kind = \"midi\"`".into()),
    }
}

fn parse_deck(name: &str, file: &str, t: &toml::Table, root: &Path) -> Result<DeckCfg, String> {
    let id = t.get("id").and_then(|v| v.as_str()).unwrap_or(name).to_string();
    if RESERVED.contains(&id.as_str()) || !se_proto::address::is_valid(&id, false) || id.contains('.') {
        return Err(format!("deck id `{id}` is reserved or invalid (rename the file or set `id = \"…\"`)"));
    }
    let brightness = t.get("brightness").and_then(|v| v.as_integer()).unwrap_or(70).clamp(0, 100) as u8;
    let mut pages = Vec::new();
    let mut artwork = BTreeMap::<String, Arc<crate::deck::render::Artwork>>::new();
    if let Some(pt) = t.get("page").and_then(|v| v.as_table()) {
        for (pname, pv) in pt {
            let ptab = pv.as_table().ok_or_else(|| format!("page `{pname}` must be a table"))?;
            let mut keys = BTreeMap::new();
            if let Some(kt) = ptab.get("key").and_then(|v| v.as_table()) {
                for (k, kv) in kt {
                    let idx: u8 = k.parse().map_err(|_| format!("page `{pname}`: key `{k}` must be a number 0–14"))?;
                    if idx > 31 {
                        return Err(format!("page `{pname}`: key {idx} out of range"));
                    }
                    let ktab = kv.as_table().ok_or_else(|| format!("page `{pname}` key {idx} must be a table"))?;
                    let kd = KeyDef::from_table_cached(ktab, root, &mut artwork).map_err(|e| format!("page `{pname}` key {idx}: {e}"))?;
                    keys.insert(idx, kd);
                }
            }
            let label = ptab.get("label").and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| pname.to_uppercase());
            pages.push(Page { name: pname.clone(), label, keys });
        }
    }
    if let Some(order) = t.get("pages").and_then(|v| v.as_array()) {
        let order: Vec<&str> = order.iter().filter_map(|v| v.as_str()).collect();
        for o in &order {
            if !pages.iter().any(|p| p.name == *o) {
                return Err(format!("`pages` lists `{o}` but there is no [page.{o}]"));
            }
        }
        pages.sort_by_key(|p| order.iter().position(|o| *o == p.name).unwrap_or(usize::MAX));
    }
    if pages.is_empty() {
        pages.push(Page { name: "main".into(), label: "MAIN".into(), keys: BTreeMap::new() });
    }
    let start_page = t.get("start_page").and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| pages[0].name.clone());
    if !pages.iter().any(|p| p.name == start_page) {
        return Err(format!("start_page `{start_page}` does not exist"));
    }
    for p in &pages {
        for (k, kd) in &p.keys {
            if let Action::Page(target) = &kd.action
                && target != "next"
                && target != "prev"
                && !pages.iter().any(|q| &q.name == target)
            {
                return Err(format!("page `{}` key {k} switches to unknown page `{target}`", p.name));
            }
        }
    }
    Ok(DeckCfg {
        id,
        file: file.to_string(),
        serial: t.get("serial").and_then(|v| v.as_str()).map(String::from),
        brightness,
        pages,
        start_page,
        primary: t.get("primary").and_then(|v| v.as_bool()).unwrap_or(false),
    })
}

fn channel(t: &toml::Table) -> Result<Option<u8>, String> {
    match t.get("channel").and_then(|v| v.as_integer()) {
        None => Ok(None),
        Some(c @ 1..=16) => Ok(Some(c as u8 - 1)),
        Some(c) => Err(format!("channel {c} out of range 1–16")),
    }
}

fn int7(t: &toml::Table, k: &str) -> Result<Option<u8>, String> {
    match t.get(k).and_then(|v| v.as_integer()) {
        None => Ok(None),
        Some(v @ 0..=127) => Ok(Some(v as u8)),
        Some(v) => Err(format!("`{k}` = {v} out of range 0–127")),
    }
}

/// `[control.<name>]` → a control definition.
fn parse_control(name: &str, t: &toml::Table) -> Result<ControlDef, String> {
    let ch = channel(t)?;
    let kind = if let Some(cc) = int7(t, "cc")? {
        if let Some(enc) = t.get("relative").and_then(|v| v.as_str()) {
            Kind::Rel { cc, enc: RelEnc::parse(enc).ok_or_else(|| format!("unknown relative encoding `{enc}` (mcu, hui, twos, offset64)"))? }
        } else if let Some(lsb) = int7(t, "lsb")? {
            Kind::Cc14 { cc, lsb }
        } else if t.get("hires").and_then(|v| v.as_bool()).unwrap_or(false) {
            if cc >= 32 {
                return Err("14-bit CC pairs need an MSB controller below 32".into());
            }
            Kind::Cc14 { cc, lsb: cc + 32 }
        } else {
            Kind::Cc { cc }
        }
    } else if let Some(note) = int7(t, "note")? {
        Kind::Note { note }
    } else if t.get("pitchbend").and_then(|v| v.as_bool()).unwrap_or(false) {
        Kind::PitchBend
    } else if let Some(p) = t.get("nrpn").and_then(|v| v.as_integer()) {
        if !(0..16384).contains(&p) {
            return Err(format!("nrpn {p} out of range 0–16383"));
        }
        Kind::Nrpn { param: p as u16, hires: t.get("hires").and_then(|v| v.as_bool()).unwrap_or(true) }
    } else if let Some(p) = t.get("rpn").and_then(|v| v.as_integer()) {
        if !(0..16384).contains(&p) {
            return Err(format!("rpn {p} out of range 0–16383"));
        }
        Kind::Rpn { param: p as u16 }
    } else if t.get("pressure").and_then(|v| v.as_bool()).unwrap_or(false) {
        Kind::ChannelPressure
    } else {
        return Err(format!("control `{name}` needs one of cc, note, pitchbend, nrpn, rpn, pressure"));
    };
    let mut d = ControlDef::new(name, ch, kind);
    d.button = t.get("button").and_then(|v| v.as_bool()).unwrap_or(false);
    if let Some(s) = t.get("switch").and_then(|v| v.as_str()) {
        d.switch = SwitchMode::parse(s).ok_or_else(|| format!("unknown switch mode `{s}` (momentary, trigger)"))?;
        if matches!(kind, Kind::Cc { .. }) {
            d.button = true;
        }
    }
    d.motor = t.get("motor").and_then(|v| v.as_bool()).unwrap_or(false);
    d.touch_note = int7(t, "touch")?;
    let och = ch.unwrap_or(0);
    d.out = match t.get("out") {
        None => None,
        Some(toml::Value::String(s)) if s == "echo" => Some(match kind {
            Kind::Cc { cc } => Out::Cc { ch: och, cc },
            Kind::Cc14 { cc, lsb } => Out::Cc14 { ch: och, cc, lsb },
            Kind::Nrpn { param, .. } => Out::Nrpn { ch: och, param },
            Kind::PitchBend => Out::PitchBend { ch: och },
            Kind::Note { note } => Out::NoteLed { ch: och, note },
            _ => return Err(format!("control `{name}`: `out = \"echo\"` is not possible for {}", kind.describe())),
        }),
        Some(toml::Value::Table(o)) => {
            let och = channel(o)?.unwrap_or(och);
            if let Some(cc) = int7(o, "cc")? {
                match o.get("style").and_then(|v| v.as_str()) {
                    Some("behringer") => Some(Out::BehringerRing { ch: och, cc }),
                    None | Some("value") => Some(Out::Cc { ch: och, cc }),
                    Some(s) => return Err(format!("unknown out style `{s}` (value, behringer)")),
                }
            } else if let Some(note) = int7(o, "note")? {
                Some(Out::NoteLed { ch: och, note })
            } else if let Some(strip) = int7(o, "mcu_ring")? {
                Some(Out::McuRing { strip: strip.min(7) })
            } else if o.get("pitchbend").and_then(|v| v.as_bool()).unwrap_or(false) {
                Some(Out::PitchBend { ch: och })
            } else {
                return Err(format!("control `{name}`: `out` needs cc, note, mcu_ring, or pitchbend"));
            }
        }
        Some(_) => return Err(format!("control `{name}`: `out` must be \"echo\" or a table")),
    };
    Ok(d)
}

const KIND_KEYS: &[&str] = &["cc", "note", "pitchbend", "nrpn", "rpn", "pressure"];

/// Flatten `[control.enc.1]`-style nesting into dotted control names.
fn collect_controls(prefix: &str, t: &toml::Table, out: &mut Vec<ControlDef>) -> Result<(), String> {
    for (k, v) in t {
        let name = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        let Some(sub) = v.as_table() else {
            return Err(format!("control `{name}` must be a table"));
        };
        if KIND_KEYS.iter().any(|kk| sub.contains_key(*kk)) {
            out.push(parse_control(&name, sub)?);
        } else {
            collect_controls(&name, sub, out)?;
        }
    }
    Ok(())
}

/// General MIDI drum map (e-drum modules default to it).
pub fn gm_drums() -> BTreeMap<u8, String> {
    [
        (35, "kick"),
        (36, "kick"),
        (37, "side_stick"),
        (38, "snare"),
        (40, "snare_rim"),
        (41, "tom_floor_low"),
        (42, "hat_closed"),
        (43, "tom_floor"),
        (44, "hat_pedal"),
        (45, "tom_low"),
        (46, "hat_open"),
        (47, "tom_mid"),
        (48, "tom_high_mid"),
        (49, "crash"),
        (50, "tom_high"),
        (51, "ride"),
        (52, "china"),
        (53, "ride_bell"),
        (55, "splash"),
        (57, "crash2"),
        (59, "ride2"),
    ]
    .into_iter()
    .map(|(n, s)| (n, s.to_string()))
    .collect()
}

fn parse_midi(name: &str, file: &str, t: &toml::Table) -> Result<MidiDeviceCfg, String> {
    let id = t.get("id").and_then(|v| v.as_str()).unwrap_or(name).to_string();
    if !se_proto::address::is_valid(&id, false) || id.contains('.') {
        return Err(format!("device id `{id}` must be a simple name (letters, digits, _)"));
    }
    let matcher = t.get("match").and_then(|v| v.as_str()).ok_or("MIDI device needs `match` (ALSA client name glob)")?.to_string();
    let profile = match t.get("profile").and_then(|v| v.as_str()).unwrap_or("generic") {
        "generic" => Profile::Generic,
        "mcu" => Profile::Mcu { motors: t.get("motors").and_then(|v| v.as_bool()).unwrap_or(true) },
        "xtouch_mini_mc" | "xtouch_mini" => Profile::XTouchMiniMc,
        "hui" => Profile::Hui,
        p => return Err(format!("unknown profile `{p}` (generic, mcu, xtouch_mini_mc, hui)")),
    };
    let mut controls = match profile {
        Profile::Generic => Vec::new(),
        Profile::Mcu { motors } => mcu::mcu_controls(motors),
        Profile::XTouchMiniMc => mcu::xtouch_mini_mc(),
        Profile::Hui => mcu::hui_controls(),
    };
    if let Some(ct) = t.get("control").and_then(|v| v.as_table()) {
        let mut extra = Vec::new();
        collect_controls("", ct, &mut extra)?;
        for d in extra {
            match controls.iter_mut().find(|c| c.name == d.name) {
                Some(c) => *c = d,
                None => controls.push(d),
            }
        }
    }
    let mut maps = Vec::new();
    if let Some(arr) = t.get("map").and_then(|v| v.as_array()) {
        for (i, v) in arr.iter().enumerate() {
            let mt = v.as_table().ok_or_else(|| format!("map #{} must be a table", i + 1))?;
            let control = mt.get("control").and_then(|v| v.as_str()).ok_or_else(|| format!("map #{} needs `control`", i + 1))?.to_string();
            let ctl = controls.iter().find(|c| c.name == control);
            let action = Action::from_table(mt).map_err(|e| format!("map `{control}`: {e}"))?;
            let b = Behavior::from_table(mt).map_err(|e| format!("map `{control}`: {e}"))?;
            let target = mt.get("target").and_then(|v| v.as_str()).map(String::from);
            let list = |k: &str| -> Vec<String> {
                match mt.get(k) {
                    Some(toml::Value::String(s)) => vec![s.clone()],
                    Some(toml::Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
                    _ => Vec::new(),
                }
            };
            let (inc, dec) = (list("inc"), list("dec"));
            for c in inc.iter().chain(&dec) {
                se_proto::Op::parse(c).map_err(|e| format!("map `{control}` command `{c}`: {e}"))?;
            }
            if let Some(c) = ctl {
                if target.is_some() && c.kind.is_absolute() && !c.button {
                    return Err(format!(
                        "map `{control}`: absolute control ({}) with `target` — use a [[binding]] (signal `midi.{id}.{control}`) so takeover applies",
                        c.kind.describe()
                    ));
                }
                if action != Action::None && !c.is_button() {
                    return Err(format!("map `{control}`: {} is not a button (set `button = true` on the control)", c.kind.describe()));
                }
            }
            let range = match mt.get("range").and_then(|v| v.as_array()) {
                Some(a) if a.len() == 2 => Some([num(a.first()).ok_or("range needs numbers")?, num(a.get(1)).ok_or("range needs numbers")?]),
                Some(_) => return Err(format!("map `{control}`: range must be [min, max]")),
                None => None,
            };
            let ring = match mt.get("ring").and_then(|v| v.as_str()) {
                Some(s) => RingMode::parse(s).ok_or_else(|| format!("map `{control}`: unknown ring `{s}` (dot, boost, fill, spread)"))?,
                None => RingMode::Fill,
            };
            maps.push(MapDef {
                control,
                action,
                b,
                target,
                step: num(mt.get("step")),
                range,
                ring,
                inc,
                dec,
                feedback: mt.get("feedback").and_then(|v| v.as_bool()).unwrap_or(true),
            });
        }
    }
    let bank = match t.get("bank").and_then(|v| v.as_table()) {
        None => None,
        Some(bt) => {
            let s = |k: &str| bt.get(k).and_then(|v| v.as_str()).map(String::from);
            let ring = match bt.get("ring").and_then(|v| v.as_str()) {
                Some(r) => RingMode::parse(r).ok_or_else(|| format!("[bank] unknown ring `{r}`"))?,
                None => RingMode::Fill,
            };
            Some(BankCfg {
                size: bt.get("size").and_then(|v| v.as_integer()).unwrap_or(8).clamp(1, 8) as u32,
                count: bt.get("count").and_then(|v| v.as_integer()).unwrap_or(8).clamp(1, 1024) as u32,
                fader: s("fader"),
                vpot: s("vpot"),
                select: s("select"),
                mute: s("mute"),
                solo: s("solo"),
                rec: s("rec"),
                name: s("name"),
                meter: s("meter"),
                ring,
            })
        }
    };
    let mut drums = BTreeMap::new();
    let mut drum_channel = None;
    match t.get("drums") {
        Some(toml::Value::Boolean(true)) => drums = gm_drums(),
        Some(toml::Value::Table(dt)) => {
            if dt.get("gm").and_then(|v| v.as_bool()).unwrap_or(false) {
                drums = gm_drums();
            }
            drum_channel = channel(dt)?;
            for (k, v) in dt {
                if k == "channel" || k == "gm" {
                    continue;
                }
                let note: u8 = k.parse().ok().filter(|n| *n < 128).ok_or_else(|| format!("[drums] key `{k}` must be a note number 0–127"))?;
                let pad = v.as_str().ok_or_else(|| format!("[drums] {k} must be a pad name"))?;
                if !se_proto::address::is_valid(pad, false) {
                    return Err(format!("[drums] pad name `{pad}` is not a valid address segment"));
                }
                drums.insert(note, pad.to_string());
            }
        }
        _ => {}
    }
    let mut init = Vec::new();
    if profile == Profile::XTouchMiniMc && t.get("mode_select").and_then(|v| v.as_bool()).unwrap_or(true) {
        init.push(mcu::XTOUCH_MINI_MC_MODE.to_vec());
    }
    if let Some(a) = t.get("init").and_then(|v| v.as_array()) {
        for v in a {
            let s = v.as_str().ok_or("`init` must be a list of hex strings")?;
            init.push(crate::midi::parse::parse_hex(s).ok_or_else(|| format!("init: bad hex `{s}`"))?);
        }
    }
    Ok(MidiDeviceCfg {
        id,
        file: file.to_string(),
        matcher,
        port: t.get("port").and_then(|v| v.as_str()).map(String::from),
        usb: t.get("usb").and_then(|v| v.as_str()).map(String::from),
        serial: t.get("serial").and_then(|v| v.as_str()).map(String::from),
        profile,
        controls,
        maps,
        bank,
        drums,
        drum_channel,
        init,
        auto_controls: t.get("auto_controls").and_then(|v| v.as_bool()).unwrap_or(true),
        coalesce_ms: t.get("coalesce_ms").and_then(|v| v.as_integer()).unwrap_or(4).clamp(1, 100) as u64,
    })
}

/// Case-insensitive glob (`*`, `?`) over whole strings.
pub fn glob(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = s.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Identifier-safe slug of a device name (`Studio 24c` → `studio_24c`).
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert_str(0, "dev_");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(s: &str) -> toml::Table {
        s.parse().unwrap()
    }

    #[test]
    fn deck_pages_keys_and_order() {
        let t = table(
            r##"
kind = "deck"
brightness = 55
pages = ["show", "fx"]
[page.fx.key.0]
page = "show"
[page.show]
label = "Show"
[page.show.key.0]
preset = "hype"
[page.show.key.4]
do = ["scene.take"]
label = "TAKE"
color = "#e82424"
[page.show.key.14]
do = ["panic"]
hold = "1s"
[page.show.key.13]
ptt = true
"##,
        );
        let Parsed::Deck(d) = parse_file("deck", "controllers/deck.toml", &t).unwrap() else { panic!() };
        assert_eq!(d.pages.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec!["show", "fx"]);
        assert_eq!(d.start_page, "show");
        assert_eq!(d.brightness, 55);
        let show = d.page("show").unwrap();
        assert_eq!(show.keys[&0].action, Action::Preset("hype".into()));
        assert_eq!(show.keys[&14].b.hold_ms, 1000);
        assert!(show.keys[&13].action.wants_release());
        assert_eq!(show.keys[&4].action.describe(), "scene.take");
    }

    #[test]
    fn deck_errors_are_specific() {
        let bad = table("kind = \"deck\"\n[page.a.key.0]\npreset = \"x\"\nscene = \"y\"");
        assert!(parse_file("d", "f", &bad).err().unwrap().contains("more than one"));
        let bad = table("kind = \"deck\"\n[page.a.key.0]\npage = \"nope\"");
        assert!(parse_file("d", "f", &bad).err().unwrap().contains("unknown page"));
        let bad = table("kind = \"deck\"\n[page.a.key.0]\ndo = [\"$(x)\"]");
        assert!(parse_file("d", "f", &bad).is_err());
    }

    #[test]
    fn midi_device_profile_controls_maps_and_drums() {
        let t = table(
            r#"
kind = "midi"
match = "X-TOUCH MINI*"
profile = "xtouch_mini_mc"
[[map]]
control = "enc.1"
target = "fx.vignette.amount"
step = 0.02
[[map]]
control = "btn.1"
preset = "hype"
[control.fs.a]
cc = 81
switch = "trigger"
[drums]
channel = 10
36 = "kick"
"#,
        );
        let Parsed::Midi(m) = parse_file("xtouch", "controllers/xtouch.toml", &t).unwrap() else { panic!() };
        assert_eq!(m.id, "xtouch");
        assert_eq!(m.init, vec![vec![0xB0, 0x7F, 0x01]]);
        let fs = m.controls.iter().find(|c| c.name == "fs.a").unwrap();
        assert!(fs.button && fs.switch == SwitchMode::Trigger);
        assert_eq!(m.maps.len(), 2);
        assert_eq!(m.drums[&36], "kick");
        assert_eq!(m.drum_channel, Some(9));
    }

    #[test]
    fn absolute_control_with_target_points_to_bindings() {
        let t = table("kind = \"midi\"\nmatch = \"x\"\nprofile = \"xtouch_mini_mc\"\n[[map]]\ncontrol = \"fader\"\ntarget = \"fx.a.amount\"");
        let e = parse_file("x", "f", &t).err().unwrap();
        assert!(e.contains("[[binding]]"), "{e}");
    }

    #[test]
    fn globs_and_slugs() {
        assert!(glob("X-TOUCH MINI*", "x-touch mini:X-TOUCH MINI MIDI 1"));
        assert!(glob("*24c*", "Studio 24c"));
        assert!(!glob("FBV*", "Studio 24c"));
        assert!(glob("a?c", "abc"));
        assert_eq!(slug("FBV Express Mk II"), "fbv_express_mk_ii");
        assert_eq!(slug("24c"), "dev_24c");
    }
}
