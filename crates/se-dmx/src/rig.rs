//! The rig (`lights/rig.toml`): patched fixtures, groups, stage layout, outputs, safety
//! settings — compiled into heads (addressable light units) and DMX channel encoders.

use crate::profile::{Channel, Mode, Profile, Role, WhiteMix};
use se_proto::{Meta, Value};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

/// Number of fixed attribute slots per head.
pub const SLOTS: usize = 19;

/// Slot indices of the fixed attributes in a head's value array.
pub mod slot {
    pub const INTENSITY: usize = 0;
    pub const RED: usize = 1;
    pub const GREEN: usize = 2;
    pub const BLUE: usize = 3;
    pub const WHITE: usize = 4;
    pub const AMBER: usize = 5;
    pub const UV: usize = 6;
    pub const LIME: usize = 7;
    pub const CTO: usize = 8;
    pub const PAN: usize = 9;
    pub const TILT: usize = 10;
    pub const ZOOM: usize = 11;
    pub const FOCUS: usize = 12;
    pub const IRIS: usize = 13;
    pub const FROST: usize = 14;
    pub const PRISM: usize = 15;
    pub const GOBO_ROTATE: usize = 16;
    pub const STROBE: usize = 17;
    pub const GOBO: usize = 18;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttrKind {
    /// 0..1, HTP.
    Intensity,
    /// RGB 0..1 (addresses carry `[r, g, b, a]`).
    Color,
    /// 0..1 float.
    Unit,
    /// Wheel slot index.
    Index,
    /// Raw DMX value 0..255.
    Raw,
}

/// A fixed attribute: name, kind, first slot, default.
#[derive(Clone, Copy, Debug)]
pub struct Known {
    pub name: &'static str,
    pub kind: AttrKind,
    pub slot: usize,
    pub default: f32,
}

pub const KNOWN: &[Known] = &[
    Known { name: "intensity", kind: AttrKind::Intensity, slot: slot::INTENSITY, default: 0.0 },
    Known { name: "color", kind: AttrKind::Color, slot: slot::RED, default: 1.0 },
    Known { name: "white", kind: AttrKind::Unit, slot: slot::WHITE, default: 0.0 },
    Known { name: "amber", kind: AttrKind::Unit, slot: slot::AMBER, default: 0.0 },
    Known { name: "uv", kind: AttrKind::Unit, slot: slot::UV, default: 0.0 },
    Known { name: "lime", kind: AttrKind::Unit, slot: slot::LIME, default: 0.0 },
    Known { name: "cto", kind: AttrKind::Unit, slot: slot::CTO, default: 0.0 },
    Known { name: "pan", kind: AttrKind::Unit, slot: slot::PAN, default: 0.5 },
    Known { name: "tilt", kind: AttrKind::Unit, slot: slot::TILT, default: 0.5 },
    Known { name: "zoom", kind: AttrKind::Unit, slot: slot::ZOOM, default: 0.5 },
    Known { name: "focus", kind: AttrKind::Unit, slot: slot::FOCUS, default: 0.5 },
    Known { name: "iris", kind: AttrKind::Unit, slot: slot::IRIS, default: 0.0 },
    Known { name: "frost", kind: AttrKind::Unit, slot: slot::FROST, default: 0.0 },
    Known { name: "prism", kind: AttrKind::Unit, slot: slot::PRISM, default: 0.0 },
    Known { name: "gobo_rotate", kind: AttrKind::Unit, slot: slot::GOBO_ROTATE, default: 0.0 },
    Known { name: "strobe", kind: AttrKind::Unit, slot: slot::STROBE, default: 0.0 },
    Known { name: "gobo", kind: AttrKind::Index, slot: slot::GOBO, default: 0.0 },
];

pub fn known(name: &str) -> Option<&'static Known> {
    KNOWN.iter().find(|k| k.name == name)
}

/// One attribute of a head (or group).
#[derive(Clone, Debug, PartialEq)]
pub struct Attr {
    pub name: String,
    pub kind: AttrKind,
    /// Fixed slot, or `None` for raw attributes.
    pub slot: Option<usize>,
    /// Raw attributes: index into the head's raw values.
    pub raw: Option<usize>,
    /// Default resolved value when nothing controls it; `Null` = pass-through (group-like).
    pub default: Value,
    /// Gobo wheel size (Index attributes).
    pub slots: usize,
}

impl Attr {
    pub fn meta(&self, owner_desc: &str) -> Meta {
        let m = match self.kind {
            AttrKind::Intensity => Meta::float(self.default.as_f64().unwrap_or(0.0), [0.0, 1.0]).htp(),
            AttrKind::Color => Meta { ty: se_proto::ValueType::Color, default: self.default.clone(), ..Default::default() },
            AttrKind::Unit => Meta { ty: se_proto::ValueType::Float, range: Some([0.0, 1.0]), default: self.default.clone(), ..Default::default() },
            AttrKind::Index => {
                Meta { ty: se_proto::ValueType::Int, range: Some([0.0, (self.slots.max(1) - 1) as f64]), default: self.default.clone(), ..Default::default() }
            }
            AttrKind::Raw => Meta { ty: se_proto::ValueType::Int, range: Some([0.0, 255.0]), default: self.default.clone(), ..Default::default() },
        };
        m.owner("lights").describe(&format!("{} {}", owner_desc, self.name))
    }
}

/// How one DMX channel is filled from its head's output.
#[derive(Clone, Debug, PartialEq)]
pub enum Enc {
    /// Head intensity (coarse byte, or fine byte of a 16-bit pair).
    Dimmer {
        fine: bool,
    },
    /// Master dimmer of a multi-cell fixture: open while any cell is lit.
    MasterDimmer,
    /// Additive emitter slot, scaled by the virtual dimmer when the head has no dimmer.
    Emitter {
        slot: usize,
    },
    /// Subtractive colour (1 − component).
    Subtractive {
        slot: usize,
    },
    /// Colour wheel: nearest slot to the head colour.
    Wheel {
        values: Vec<u8>,
        colors: Vec<[f32; 3]>,
    },
    /// 0..1 slot mapped onto `range` (8-bit, or one byte of a 16-bit pair).
    Scalar {
        slot: usize,
        fine: bool,
        lo: u8,
        hi: u8,
    },
    /// Gobo index → slot value.
    Gobo {
        values: Vec<u8>,
    },
    /// Strobe 0..1 → rate within `range` (Hz mapping for the limiter), 0 → `open`.
    Strobe {
        lo: u8,
        hi: u8,
        hz: Option<(f32, f32)>,
        open: u8,
        closed: Option<u8>,
    },
    Raw {
        index: usize,
    },
    Fixed(u8),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChanEnc {
    pub head: usize,
    pub universe: usize,
    /// 0-based channel within the universe.
    pub channel: usize,
    pub invert: bool,
    pub enc: Enc,
}

/// An addressable light unit: a whole fixture, or one cell of a multi-cell fixture.
#[derive(Clone, Debug)]
pub struct Head {
    pub id: String,
    pub fixture: usize,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub attrs: Vec<Attr>,
    pub position: [f32; 2],
    pub rotation: f32,
    pub beam: f32,
    pub kind: String,
    /// Emits light itself (has emitters or a dimmer and no cells).
    pub leaf: bool,
    /// No physical dimmer: intensity scales the emitters.
    pub virtual_dimmer: bool,
    pub moving: bool,
    pub white_mix: WhiteMix,
    pub max_intensity: f32,
    pub raw_names: Vec<String>,
    pub raw_defaults: Vec<u8>,
    /// Pan/tilt travel in degrees (visualizer).
    pub pan_deg: f32,
    pub tilt_deg: f32,
    pub zoom_deg: (f32, f32),
}

impl Head {
    pub fn attr(&self, name: &str) -> Option<&Attr> {
        self.attrs.iter().find(|a| a.name == name)
    }
    pub fn addr(&self, attr: &str) -> String {
        format!("lights.{}.{attr}", self.id)
    }
}

#[derive(Clone, Debug)]
pub struct Fixture {
    pub id: String,
    pub label: String,
    pub profile: String,
    pub profile_name: String,
    pub mode: String,
    pub kind: String,
    pub beam: f32,
    pub universe: u16,
    pub address: u16,
    pub footprint: usize,
    pub position: [f32; 2],
    pub rotation: f32,
    pub notes: Option<String>,
    /// Installed coordinates/orientation are calibrated; otherwise only index-ordered effects.
    pub layout_verified: bool,
    /// Root head (same id as the fixture).
    pub root: usize,
    /// Leaf heads (the root itself, or its cells).
    pub leaves: Vec<usize>,
}

/// Effective channel ownership, not a promise of unrestricted raw-byte access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelControl {
    Operator,
    Fixed,
    Managed,
    Blocked,
}

impl ChannelControl {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Fixed => "fixed",
            Self::Managed => "managed",
            Self::Blocked => "blocked",
        }
    }
}

/// One physical slot, including cells and channels deliberately not exposed as attributes.
#[derive(Clone, Copy, Debug)]
pub struct ChannelCapability<'a> {
    /// 1-based slot within the fixture.
    pub local_channel: usize,
    /// 1-based address in the fixture's universe.
    pub address: usize,
    /// Compiled head owning this slot (root or individual pixel).
    pub head: usize,
    pub channel: &'a Channel,
    pub control: ChannelControl,
    /// Hardware rate control is available only with calibration and an allowing policy.
    /// A shutter may still have managed steady-open/blackout behavior when this is false.
    pub strobe_available: bool,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub name: String,
    /// Fixture indices.
    pub fixtures: Vec<usize>,
    /// Root heads of those fixtures.
    pub roots: Vec<usize>,
    /// All leaf heads.
    pub leaves: Vec<usize>,
    pub attrs: Vec<Attr>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OutputKind {
    /// `widget_rate`: set the widget's stored output rate on connect (0 = fastest).
    EnttecPro { port: String, universe: u16, channels: usize, widget_rate: Option<u8> },
    /// `port`: UDP port (E1.31: 5568).
    Sacn { universes: Vec<u16>, priority: u8, destination: Option<IpAddr>, source_name: String, port: u16 },
    /// `port`: UDP port (Art-Net: 6454).
    ArtNet { destination: IpAddr, universes: Vec<u16>, net: u8, subnet: u8, port: u16 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutputDef {
    pub id: String,
    pub enabled: bool,
    /// Keeps sending the live show in `rehearsal` mode (a test node or visualizer); other
    /// outputs hold the look they had when rehearsal started (§17.2).
    pub rehearsal: bool,
    pub kind: OutputKind,
}

impl OutputDef {
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            OutputKind::EnttecPro { .. } => "enttec_pro",
            OutputKind::Sacn { .. } => "sacn",
            OutputKind::ArtNet { .. } => "artnet",
        }
    }
    pub fn universes(&self) -> Vec<u16> {
        match &self.kind {
            OutputKind::EnttecPro { universe, .. } => vec![*universe],
            OutputKind::Sacn { universes, .. } | OutputKind::ArtNet { universes, .. } => universes.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrobePolicy {
    /// Hardware strobe rates are capped at `max_flash_hz` (profiles without a Hz mapping stay open).
    Limit,
    /// Hardware strobe channels always stay open.
    Block,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Safety {
    pub max_flash_hz: f32,
    pub flash_threshold: f32,
    pub max_intensity: f32,
    pub strobe: StrobePolicy,
    pub safe_intensity: f32,
    pub safe_color: [f32; 3],
}

impl Default for Safety {
    fn default() -> Self {
        Safety { max_flash_hz: 3.0, flash_threshold: 0.2, max_intensity: 1.0, strobe: StrobePolicy::Limit, safe_intensity: 0.6, safe_color: [1.0, 0.9, 0.8] }
    }
}

/// Static ambient fallback, compiled once; never an authored playback or room takeover.
#[derive(Clone, Debug)]
pub struct Idle {
    pub heads: Vec<usize>,
    pub color: [f32; 3],
    pub intensity: f32,
}

/// The compiled rig.
#[derive(Clone, Debug)]
pub struct Rig {
    pub fixtures: Vec<Fixture>,
    pub heads: Vec<Head>,
    pub groups: Vec<Group>,
    pub chans: Vec<ChanEnc>,
    /// Universe numbers in use (fixtures and outputs), sorted; `ChanEnc::universe` indexes this.
    pub universes: Vec<u16>,
    pub outputs: Vec<OutputDef>,
    /// Hard transport interlock; disarmed rigs still render/query offline but never open outputs.
    pub output_armed: bool,
    pub main_light: Option<crate::main_light::Config>,
    pub idle: Option<Idle>,
    pub safety: Safety,
    pub rate_hz: f32,
    pub rt_priority: i32,
    pub rdm_on_start: bool,
    pub profiles: BTreeMap<String, Profile>,
    pub errors: Vec<String>,
}

impl Rig {
    pub fn head(&self, id: &str) -> Option<usize> {
        self.heads.iter().position(|h| h.id == id)
    }
    pub fn fixture(&self, id: &str) -> Option<usize> {
        self.fixtures.iter().position(|f| f.id == id)
    }
    pub fn group(&self, name: &str) -> Option<usize> {
        self.groups.iter().position(|g| g.name == name)
    }
    pub fn universe_index(&self, u: u16) -> Option<usize> {
        self.universes.iter().position(|x| *x == u)
    }

    /// Complete physical footprint in address order, borrowing profile channel metadata.
    /// This query-time iterator does not add work or allocation to per-frame encoding.
    pub fn fixture_channels(&self, fixture: usize) -> impl Iterator<Item = ChannelCapability<'_>> {
        let f = self.fixtures.get(fixture);
        let mode = f.and_then(|f| self.profiles.get(&f.profile)?.mode(Some(&f.mode)));
        let master_off = mode.map(|m| if m.cells_first { m.cells as usize * m.cell_channels.len() } else { 0 }).unwrap_or(0);
        (0..f.map_or(0, |f| f.footprint)).map(move |offset| {
            let f = f.expect("nonempty fixture footprint");
            let m = mode.expect("compiled fixture mode");
            let master = offset >= master_off && offset < master_off + m.channels.len();
            let (head, channel) = if master {
                (f.root, &m.channels[offset - master_off])
            } else {
                let cell_offset = if m.cells_first { offset } else { offset - m.channels.len() };
                let width = m.cell_channels.len();
                (f.leaves[cell_offset / width], &m.cell_channels[cell_offset % width])
            };
            let strobe_available = matches!(channel.role, Role::Strobe | Role::Shutter)
                && channel.hz.is_some() && self.safety.strobe == StrobePolicy::Limit;
            let control = match channel.role {
                Role::Fixed => ChannelControl::Fixed,
                Role::Shutter => ChannelControl::Managed,
                Role::Strobe if !strobe_available => ChannelControl::Blocked,
                Role::Dimmer if !self.heads[head].leaf => ChannelControl::Managed,
                _ => ChannelControl::Operator,
            };
            ChannelCapability { local_channel: offset + 1, address: f.address as usize + offset, head, channel, control, strobe_available }
        })
    }

    /// Heads a cue value for `attr` on `target` (fixture, cell, group, or `all`) lands on:
    /// fixture roots when they carry the attribute, otherwise their cells.
    pub fn heads_for(&self, target: &str, attr: &str) -> Result<Vec<usize>, String> {
        let roots: Vec<usize> = if let Some(g) = self.group(target) {
            self.groups[g].roots.clone()
        } else if let Some(h) = self.head(target) {
            vec![h]
        } else {
            return Err(format!("unknown fixture or group `{target}`"));
        };
        let mut out = Vec::new();
        for r in roots {
            if self.heads[r].attr(attr).is_some() {
                out.push(r);
            } else {
                out.extend(self.heads[r].children.iter().copied().filter(|c| self.heads[*c].attr(attr).is_some()));
            }
        }
        Ok(out)
    }

    /// Leaf heads of a target (effects, highlight).
    pub fn leaves_for(&self, target: &str) -> Result<Vec<usize>, String> {
        if let Some(g) = self.group(target) {
            return Ok(self.groups[g].leaves.clone());
        }
        let h = self.head(target).ok_or_else(|| format!("unknown fixture or group `{target}`"))?;
        Ok(if self.heads[h].leaf { vec![h] } else { self.heads[h].children.clone() })
    }

    /// Specificity of a target for palette precedence (smaller = more specific).
    pub fn specificity(&self, target: &str) -> usize {
        if target == "all" {
            usize::MAX
        } else if let Some(g) = self.group(target) {
            self.groups[g].leaves.len() + 2
        } else if let Some(h) = self.head(target) {
            if self.heads[h].leaf { 0 } else { 1 }
        } else {
            usize::MAX - 1
        }
    }

    /// Compile `lights/rig.toml` against the profile library. Errors in single fixtures are
    /// collected (that fixture is skipped); a structurally broken file is an `Err`.
    pub fn compile(table: &toml::Table, profiles: BTreeMap<String, Profile>, mut errors: Vec<String>) -> Result<Rig, String> {
        let mut raw: RawRig = toml::Value::Table(table.clone()).try_into().map_err(|e: toml::de::Error| e.message().to_string())?;
        let mut rig = Rig {
            fixtures: Vec::new(),
            heads: Vec::new(),
            groups: Vec::new(),
            chans: Vec::new(),
            universes: Vec::new(),
            outputs: Vec::new(),
            safety: Safety::default(),
            rate_hz: raw.output.rate_hz.unwrap_or(44.0).clamp(1.0, 44.0),
            rt_priority: raw.output.rt_priority.unwrap_or(40).clamp(0, 98),
            rdm_on_start: raw.rdm.discover_on_start.unwrap_or(true),
            profiles,
            output_armed: raw.output.armed.unwrap_or(false),
            main_light: raw.main_light.take(),
            idle: None,
            errors: Vec::new(),
        };
        rig.safety = raw.safety.build()?;
        if let Some(main_light) = &rig.main_light { main_light.validate()?; }
        // universes → occupied channels (overlap detection)
        let mut occupied: BTreeMap<(u16, usize), String> = BTreeMap::new();
        let mut universes = BTreeSet::new();
        for (id, f) in raw.fixtures_iter() {
            let res = f.and_then(|f| rig.add_fixture(&id, &f, &mut occupied).map(|_| f.universe.unwrap_or(1)));
            match res {
                Ok(u) => {
                    universes.insert(u);
                }
                Err(e) => errors.push(format!("fixture `{id}`: {e}")),
            }
        }
        for (id, o) in &raw.outputs {
            match o.build(id) {
                Ok(o) => {
                    if o.enabled {
                        universes.extend(o.universes());
                    }
                    rig.outputs.push(o);
                }
                Err(e) => errors.push(format!("output `{id}`: {e}")),
            }
        }
        rig.universes = universes.into_iter().collect();
        for c in rig.chans.iter_mut() {
            // `add_fixture` stored the universe number; map it to its index now.
            c.universe = rig.universes.iter().position(|u| *u as usize == c.universe).unwrap_or(0);
        }
        // groups
        let mut defs = raw.groups.clone();
        if !defs.contains_key("all") {
            defs.insert("all".into(), rig.fixtures.iter().map(|f| f.id.clone()).collect());
        }
        for name in defs.keys() {
            if !se_proto::address::is_valid(name, false) || name.contains('.') {
                errors.push(format!("group `{name}`: bad name"));
                continue;
            }
            if rig.fixture(name).is_some() || rig.head(name).is_some() {
                errors.push(format!("group `{name}`: name collides with a fixture"));
                continue;
            }
            match expand_group(name, &defs, &rig, &mut Vec::new()) {
                Ok(members) => rig.add_group(name, members),
                Err(e) => errors.push(format!("group `{name}`: {e}")),
            }
        }
        if let Some(idle) = raw.idle {
            if !idle.intensity.is_finite() || !(0.0..=1.0).contains(&idle.intensity) {
                return Err("idle intensity must be between 0 and 1".into());
            }
            let color = Value::from(idle.color).as_color().ok_or("idle color must be an RGB color")?;
            let heads = rig.leaves_for(&idle.target)?;
            if heads.is_empty() || heads.iter().any(|h| rig.heads[*h].attr("color").is_none()) {
                return Err("idle target must contain RGB heads".into());
            }
            rig.idle = Some(Idle { heads, color: [color[0], color[1], color[2]], intensity: idle.intensity });
        }
        rig.errors = errors;
        Ok(rig)
    }

    /// `members`: fixture root heads or single cells.
    fn add_group(&mut self, name: &str, members: Vec<usize>) {
        let roots = members;
        let mut fixtures: Vec<usize> = Vec::new();
        for r in &roots {
            if !fixtures.contains(&self.heads[*r].fixture) {
                fixtures.push(self.heads[*r].fixture);
            }
        }
        let leaves: Vec<usize> = roots.iter().flat_map(|r| if self.heads[*r].leaf { vec![*r] } else { self.heads[*r].children.clone() }).collect();
        let mut attrs: Vec<Attr> = Vec::new();
        for r in &roots {
            let h = &self.heads[*r];
            for a in h.attrs.iter().chain(h.children.iter().flat_map(|c| self.heads[*c].attrs.iter())) {
                if a.kind == AttrKind::Raw || attrs.iter().any(|x| x.name == a.name) {
                    continue;
                }
                let mut g = a.clone();
                g.default = if a.kind == AttrKind::Intensity { Value::Float(0.0) } else { Value::Null };
                attrs.push(g);
            }
        }
        attrs.sort_by_key(|a| a.slot.unwrap_or(usize::MAX));
        self.groups.push(Group { name: name.into(), fixtures, roots, leaves, attrs });
    }

    fn add_fixture(&mut self, id: &str, f: &RawFixture, occupied: &mut BTreeMap<(u16, usize), String>) -> Result<(), String> {
        if !se_proto::address::is_valid(id, false)
            || id.contains('.')
            || id == "group"
            || id == "cuelist"
            || id == "effect"
            || id == "programmer"
            || id == "output"
        {
            return Err("bad or reserved fixture id".into());
        }
        let profile = self.profiles.get(&f.profile).cloned().ok_or_else(|| format!("unknown profile `{}`", f.profile))?;
        let mode =
            profile.mode(f.mode.as_deref()).cloned().ok_or_else(|| format!("profile `{}` has no mode `{}`", f.profile, f.mode.clone().unwrap_or_default()))?;
        let universe = f.universe.unwrap_or(1);
        if universe == 0 || universe > 63999 {
            return Err(format!("universe {universe} out of range 1..63999"));
        }
        let address = f.address.ok_or("missing `address`")?;
        let footprint = mode.footprint();
        if address == 0 || address as usize + footprint - 1 > 512 {
            return Err(format!("address {address} with {footprint} channels does not fit in 1..512"));
        }
        for ch in 0..footprint {
            let c = address as usize - 1 + ch;
            if let Some(other) = occupied.insert((universe, c), id.to_string()) {
                return Err(format!("channel {universe}.{} overlaps fixture `{other}`", c + 1));
            }
        }
        let fi = self.fixtures.len();
        let position = f.position.unwrap_or([0.5, 0.5]);
        let rotation = f.rotation.unwrap_or(0.0);
        let max_intensity = f.max_intensity.unwrap_or(1.0).clamp(0.0, 1.0);
        let base = address as usize - 1;
        let (master_off, cell_offs) = mode.layout();
        let root = self.heads.len();
        let root_head = build_head(id, fi, &profile, &mode, &mode.channels, position, rotation, max_intensity, mode.cells == 0);
        self.heads.push(root_head);
        self.encoders(root, &mode.channels, universe, base + master_off, f.invert_pan, f.invert_tilt);
        let mut leaves = Vec::new();
        if mode.cells == 0 {
            leaves.push(root);
        } else {
            let len = f.length.unwrap_or(0.3).max(0.0);
            let n = mode.cells as usize;
            for (k, off) in cell_offs.iter().enumerate() {
                let x = if n > 1 { position[0] - len / 2.0 + len * k as f32 / (n - 1) as f32 } else { position[0] };
                let hid = format!("{id}_{}", k + 1);
                let mut h = build_head(&hid, fi, &profile, &mode, &mode.cell_channels, [x, position[1]], rotation, max_intensity, true);
                h.parent = Some(root);
                let hi = self.heads.len();
                self.heads.push(h);
                self.encoders(hi, &mode.cell_channels, universe, base + off, f.invert_pan, f.invert_tilt);
                self.heads[root].children.push(hi);
                leaves.push(hi);
            }
            // the root carries the union of its cells' attributes as pass-through (group-like) addresses
            let mut extra: Vec<Attr> = Vec::new();
            for c in &self.heads[root].children {
                for a in &self.heads[*c].attrs {
                    if a.kind != AttrKind::Raw && !self.heads[root].attrs.iter().any(|x| x.name == a.name) && !extra.iter().any(|x| x.name == a.name) {
                        let mut p = a.clone();
                        p.default = if a.kind == AttrKind::Intensity { Value::Float(0.0) } else { Value::Null };
                        extra.push(p);
                    }
                }
            }
            self.heads[root].attrs.extend(extra);
            self.heads[root].attrs.sort_by_key(|a| a.slot.unwrap_or(usize::MAX));
        }
        for h in leaves.iter().copied().chain(std::iter::once(root)) {
            if self.heads.iter().filter(|x| x.id == self.heads[h].id).count() > 1 {
                return Err(format!("head id `{}` is used twice", self.heads[h].id));
            }
        }
        self.fixtures.push(Fixture {
            id: id.into(),
            label: f.label.clone().unwrap_or_else(|| id.to_string()),
            profile: f.profile.clone(),
            profile_name: profile.name.clone(),
            mode: mode.id.clone(),
            kind: profile.kind.clone(),
            beam: profile.beam,
            universe,
            address,
            footprint,
            position,
            rotation,
            notes: f.notes.clone(),
            layout_verified: f.layout_verified.unwrap_or(false),
            root,
            leaves,
        });
        Ok(())
    }

    fn encoders(&mut self, head: usize, chans: &[Channel], universe: u16, base: usize, invert_pan: bool, invert_tilt: bool) {
        // A dimmer on a multi-cell fixture's master channels is the master dimmer.
        let is_root_with_cells = !self.heads[head].leaf;
        for (i, c) in chans.iter().enumerate() {
            let (lo, hi) = c.range.unwrap_or((0, 255));
            let enc = match c.role {
                Role::Dimmer if is_root_with_cells => Enc::MasterDimmer,
                Role::Dimmer => Enc::Dimmer { fine: false },
                Role::DimmerFine => Enc::Dimmer { fine: true },
                Role::Red => Enc::Emitter { slot: slot::RED },
                Role::Green => Enc::Emitter { slot: slot::GREEN },
                Role::Blue => Enc::Emitter { slot: slot::BLUE },
                Role::White => Enc::Emitter { slot: slot::WHITE },
                Role::Amber => Enc::Emitter { slot: slot::AMBER },
                Role::Uv => Enc::Emitter { slot: slot::UV },
                Role::Lime => Enc::Emitter { slot: slot::LIME },
                Role::Cyan => Enc::Subtractive { slot: slot::RED },
                Role::Magenta => Enc::Subtractive { slot: slot::GREEN },
                Role::Yellow => Enc::Subtractive { slot: slot::BLUE },
                Role::ColorWheel => {
                    Enc::Wheel { values: c.slots.iter().map(|s| s.value).collect(), colors: c.slots.iter().map(|s| s.color.unwrap_or([1.0; 3])).collect() }
                }
                Role::Cto => Enc::Scalar { slot: slot::CTO, fine: false, lo, hi },
                Role::Pan => Enc::Scalar { slot: slot::PAN, fine: false, lo, hi },
                Role::PanFine => Enc::Scalar { slot: slot::PAN, fine: true, lo, hi },
                Role::Tilt => Enc::Scalar { slot: slot::TILT, fine: false, lo, hi },
                Role::TiltFine => Enc::Scalar { slot: slot::TILT, fine: true, lo, hi },
                Role::Zoom => Enc::Scalar { slot: slot::ZOOM, fine: false, lo, hi },
                Role::Focus => Enc::Scalar { slot: slot::FOCUS, fine: false, lo, hi },
                Role::Iris => Enc::Scalar { slot: slot::IRIS, fine: false, lo, hi },
                Role::Frost => Enc::Scalar { slot: slot::FROST, fine: false, lo, hi },
                Role::Prism => Enc::Scalar { slot: slot::PRISM, fine: false, lo, hi },
                Role::GoboRotate => Enc::Scalar { slot: slot::GOBO_ROTATE, fine: false, lo, hi },
                Role::Gobo => Enc::Gobo { values: c.slots.iter().map(|s| s.value).collect() },
                Role::Strobe | Role::Shutter => Enc::Strobe { lo, hi, hz: c.hz, open: c.open, closed: c.closed },
                Role::Raw => Enc::Raw { index: self.heads[head].raw_names.iter().position(|n| *n == c.name).unwrap_or(0) },
                Role::Fixed => Enc::Fixed(c.default),
            };
            let invert =
                c.invert ^ (matches!(c.role, Role::Pan | Role::PanFine) && invert_pan) ^ (matches!(c.role, Role::Tilt | Role::TiltFine) && invert_tilt);
            self.chans.push(ChanEnc { head, universe: universe as usize, channel: base + i, invert, enc });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_head(
    id: &str,
    fixture: usize,
    profile: &Profile,
    mode: &Mode,
    chans: &[Channel],
    position: [f32; 2],
    rotation: f32,
    max_intensity: f32,
    leaf: bool,
) -> Head {
    let mut attrs: Vec<Attr> = Vec::new();
    let mut raw_names = Vec::new();
    let mut raw_defaults = Vec::new();
    let mut push = |a: Attr| {
        if !attrs.iter().any(|x| x.name == a.name) {
            attrs.push(a);
        }
    };
    let k = |n: &str| known(n).expect("known attribute");
    // Every head has an intensity (virtual when there is no dimmer channel).
    let ki = k("intensity");
    push(Attr { name: ki.name.into(), kind: ki.kind, slot: Some(ki.slot), raw: None, default: Value::Float(0.0), slots: 0 });
    let mut pan_deg = 540.0;
    let mut tilt_deg = 270.0;
    let mut zoom_deg = (profile.beam, profile.beam);
    for c in chans {
        match c.role {
            Role::Raw => {
                raw_names.push(c.name.clone());
                raw_defaults.push(c.default);
                push(Attr {
                    name: c.name.clone(),
                    kind: AttrKind::Raw,
                    slot: None,
                    raw: Some(raw_names.len() - 1),
                    default: Value::Int(c.default as i64),
                    slots: 0,
                });
            }
            Role::Fixed => {}
            role => {
                let name = role.attr().expect("attribute role");
                let kn = k(name);
                let default = match kn.kind {
                    AttrKind::Color => Value::from([1.0f32, 1.0, 1.0, 1.0]),
                    AttrKind::Index => Value::Int(0),
                    _ => Value::Float(kn.default as f64),
                };
                push(Attr { name: name.into(), kind: kn.kind, slot: Some(kn.slot), raw: None, default, slots: c.slots.len() });
                if let Some((a, b)) = c.deg {
                    match role {
                        Role::Pan => pan_deg = b - a,
                        Role::Tilt => tilt_deg = b - a,
                        Role::Zoom => zoom_deg = (a, b),
                        _ => {}
                    }
                }
            }
        }
    }
    attrs.sort_by_key(|a| (a.slot.unwrap_or(usize::MAX), a.raw.unwrap_or(0)));
    let has_dimmer = chans.iter().any(|c| c.role == Role::Dimmer);
    let moving = chans.iter().any(|c| matches!(c.role, Role::Pan | Role::Tilt));
    let _ = fixture;
    Head {
        id: id.into(),
        fixture,
        parent: None,
        children: Vec::new(),
        attrs,
        position,
        rotation,
        beam: profile.beam,
        kind: profile.kind.clone(),
        leaf,
        virtual_dimmer: !has_dimmer,
        moving,
        white_mix: mode.white_mix,
        max_intensity,
        raw_names,
        raw_defaults,
        pan_deg,
        tilt_deg,
        zoom_deg,
    }
}

fn expand_group(name: &str, defs: &BTreeMap<String, Vec<String>>, rig: &Rig, stack: &mut Vec<String>) -> Result<Vec<usize>, String> {
    if stack.iter().any(|s| s == name) {
        return Err(format!("group cycle through `{name}`"));
    }
    stack.push(name.to_string());
    let mut out = Vec::new();
    for m in &defs[name] {
        let direct = rig.fixture(m).map(|f| rig.fixtures[f].root).or_else(|| rig.head(m));
        if let Some(h) = direct {
            if !out.contains(&h) {
                out.push(h);
            }
        } else if defs.contains_key(m) && m != name {
            for h in expand_group(m, defs, rig, stack)? {
                if !out.contains(&h) {
                    out.push(h);
                }
            }
        } else {
            return Err(format!("unknown member `{m}`"));
        }
    }
    stack.pop();
    Ok(out)
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct RawRig {
    #[serde(default)]
    fixtures: toml::map::Map<String, toml::Value>,
    #[serde(default)]
    groups: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    output: RawOutputCfg,
    #[serde(default)]
    outputs: BTreeMap<String, RawOutput>,
    #[serde(default)]
    safety: RawSafety,
    #[serde(default)]
    rdm: RawRdm,
    main_light: Option<crate::main_light::Config>,
    idle: Option<RawIdle>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct RawIdle {
    target: String,
    color: String,
    intensity: f32,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct RawFixture {
    profile: String,
    mode: Option<String>,
    universe: Option<u16>,
    address: Option<u16>,
    label: Option<String>,
    position: Option<[f32; 2]>,
    rotation: Option<f32>,
    length: Option<f32>,
    max_intensity: Option<f32>,
    #[serde(default)]
    invert_pan: bool,
    #[serde(default)]
    invert_tilt: bool,
    #[serde(default)]
    notes: Option<String>,
    layout_verified: Option<bool>,
}

impl RawRig {
    /// Fixtures in document order (`fixtures` is an order-preserving map).
    fn fixtures_iter(&self) -> impl Iterator<Item = (String, Result<RawFixture, String>)> + '_ {
        self.fixtures.iter().map(|(k, v)| (k.clone(), v.clone().try_into::<RawFixture>().map_err(|e| e.message().to_string())))
    }
}

#[derive(Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
struct RawOutputCfg {
    rate_hz: Option<f32>,
    rt_priority: Option<i32>,
    armed: Option<bool>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct RawOutput {
    kind: String,
    enabled: Option<bool>,
    /// Serial device (USB PRO) or UDP port (sACN / Art-Net).
    port: Option<toml::Value>,
    universe: Option<u16>,
    channels: Option<usize>,
    universes: Option<Vec<u16>>,
    priority: Option<u8>,
    destination: Option<String>,
    source_name: Option<String>,
    net: Option<u8>,
    subnet: Option<u8>,
    widget_rate: Option<u8>,
    rehearsal: Option<bool>,
}

impl RawOutput {
    fn build(&self, id: &str) -> Result<OutputDef, String> {
        let enabled = self.enabled.unwrap_or(true);
        let port_str = || -> Result<String, String> {
            match &self.port {
                None => Ok("auto".into()),
                Some(toml::Value::String(s)) => Ok(s.clone()),
                Some(o) => Err(format!("`port` must be \"auto\" or a device path, not {o}")),
            }
        };
        let port_num = |default: u16| -> Result<u16, String> {
            match &self.port {
                None => Ok(default),
                Some(toml::Value::Integer(p)) if (1..=65535).contains(p) => Ok(*p as u16),
                Some(o) => Err(format!("`port` must be a UDP port number, not {o}")),
            }
        };
        let check_u = |u: u16| if (1..=63999).contains(&u) { Ok(u) } else { Err(format!("universe {u} out of range 1..63999")) };
        let kind = match self.kind.as_str() {
            "enttec_pro" | "enttec" | "usb_pro" => {
                let channels = self.channels.unwrap_or(512);
                if !(24..=512).contains(&channels) {
                    return Err(format!("channels {channels} out of range 24..512"));
                }
                if self.widget_rate.is_some_and(|r| r > 40) {
                    return Err("widget_rate must be 0 (fastest) or 1..40".into());
                }
                OutputKind::EnttecPro { port: port_str()?, universe: check_u(self.universe.unwrap_or(1))?, channels, widget_rate: self.widget_rate }
            }
            "sacn" | "e131" => {
                let universes = self.universes.clone().unwrap_or_else(|| vec![self.universe.unwrap_or(1)]);
                for u in &universes {
                    check_u(*u)?;
                }
                let destination = match self.destination.as_deref() {
                    None | Some("multicast") => None,
                    Some(ip) => Some(ip.parse::<IpAddr>().map_err(|_| format!("bad destination `{ip}`"))?),
                };
                let priority = self.priority.unwrap_or(100);
                if priority > 200 {
                    return Err("sACN priority must be 0..200".into());
                }
                OutputKind::Sacn {
                    universes,
                    priority,
                    destination,
                    source_name: self.source_name.clone().unwrap_or_else(|| "stream-engine".into()),
                    port: port_num(crate::output::sacn::PORT)?,
                }
            }
            "artnet" | "art-net" => {
                let universes = self.universes.clone().unwrap_or_else(|| vec![self.universe.unwrap_or(1)]);
                for u in &universes {
                    check_u(*u)?;
                    if *u > 16 {
                        return Err(format!("Art-Net universe {u}: use net/subnet for more than 16 universes per output"));
                    }
                }
                let d = self.destination.clone().unwrap_or_else(|| "2.255.255.255".into());
                let destination = d.parse::<IpAddr>().map_err(|_| format!("bad destination `{d}`"))?;
                let net = self.net.unwrap_or(0);
                let subnet = self.subnet.unwrap_or(0);
                if net > 127 || subnet > 15 {
                    return Err("Art-Net net must be 0..127 and subnet 0..15".into());
                }
                OutputKind::ArtNet { destination, universes, net, subnet, port: port_num(crate::output::artnet::PORT)? }
            }
            o => return Err(format!("unknown output kind `{o}`")),
        };
        Ok(OutputDef { id: id.into(), enabled, rehearsal: self.rehearsal.unwrap_or(false), kind })
    }
}

#[derive(Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
struct RawSafety {
    max_flash_hz: Option<f32>,
    flash_threshold: Option<f32>,
    max_intensity: Option<f32>,
    strobe: Option<String>,
    safe_look: Option<RawSafeLook>,
}

#[derive(Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
struct RawSafeLook {
    intensity: Option<f32>,
    color: Option<String>,
}

impl RawSafety {
    fn build(&self) -> Result<Safety, String> {
        let d = Safety::default();
        let max_flash_hz = self.max_flash_hz.unwrap_or(d.max_flash_hz);
        if !(0.0..=25.0).contains(&max_flash_hz) {
            return Err("safety.max_flash_hz must be 0..25".into());
        }
        let flash_threshold = self.flash_threshold.unwrap_or(d.flash_threshold);
        if !(0.01..=1.0).contains(&flash_threshold) {
            return Err("safety.flash_threshold must be 0.01..1".into());
        }
        let strobe = match self.strobe.as_deref() {
            None | Some("limit") => StrobePolicy::Limit,
            Some("block") => StrobePolicy::Block,
            Some(o) => return Err(format!("safety.strobe: unknown policy `{o}`")),
        };
        let (safe_intensity, safe_color) = match &self.safe_look {
            Some(l) => {
                let c = match &l.color {
                    Some(c) => {
                        let [r, g, b, _] = se_proto::value::parse_hex_color(c).ok_or_else(|| format!("safety.safe_look.color: bad colour `{c}`"))?;
                        [r, g, b]
                    }
                    None => d.safe_color,
                };
                (l.intensity.unwrap_or(d.safe_intensity).clamp(0.0, 1.0), c)
            }
            None => (d.safe_intensity, d.safe_color),
        };
        Ok(Safety { max_flash_hz, flash_threshold, max_intensity: self.max_intensity.unwrap_or(1.0).clamp(0.0, 1.0), strobe, safe_intensity, safe_color })
    }
}

#[derive(Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
struct RawRdm {
    discover_on_start: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn lib() -> BTreeMap<String, Profile> {
        let mut e = Vec::new();
        let l = crate::profile::library(&BTreeMap::new(), &mut e);
        assert!(e.is_empty(), "{e:?}");
        l
    }

    fn rig(src: &str) -> Rig {
        Rig::compile(&toml::from_str(src).unwrap(), lib(), Vec::new()).unwrap()
    }

    #[test]
    fn placeholder_rgb_par_at_address_one() {
        let r = rig("[fixtures.par1]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 1\n[outputs.usb]\nkind = \"enttec_pro\"");
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.universes, vec![1]);
        assert_eq!(r.heads.len(), 1);
        let h = &r.heads[0];
        assert!(h.virtual_dimmer && h.leaf);
        assert_eq!(h.attrs.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), vec!["intensity", "color"]);
        assert_eq!(r.chans.iter().map(|c| c.channel).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(r.groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), vec!["all"]);
    }

    #[test]
    fn overlaps_bad_modes_and_groups_are_reported() {
        let r = rig(r#"
[fixtures.a]
profile = "generic_rgb"
mode = "3ch"
address = 1
[fixtures.b]
profile = "generic_rgb"
mode = "3ch"
address = 3
[fixtures.c]
profile = "generic_rgb"
mode = "nope"
address = 20
[fixtures.d]
profile = "generic_rgb"
mode = "3ch"
address = 511
[groups]
front = ["a", "ghost"]
loop1 = ["loop2"]
loop2 = ["loop1"]
"#);
        let e = r.errors.join("\n");
        assert!(e.contains("fixture `b`: channel 1.3 overlaps fixture `a`"), "{e}");
        assert!(e.contains("fixture `c`: profile `generic_rgb` has no mode `nope`"), "{e}");
        assert!(e.contains("fixture `d`: address 511"), "{e}");
        assert!(e.contains("group `front`: unknown member `ghost`"), "{e}");
        assert!(e.contains("cycle"), "{e}");
        assert_eq!(r.fixtures.len(), 1);
    }

    #[test]
    fn led_bar_cells_and_root_passthrough() {
        let r = rig(
            "[fixtures.bar]\nprofile = \"generic_led_bar\"\nmode = \"26ch\"\naddress = 10\nposition = [0.5, 0.5]\nlength = 0.7\n[groups]\nback = [\"bar\"]",
        );
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let root = r.head("bar").unwrap();
        assert_eq!(r.heads[root].children.len(), 8);
        assert!(!r.heads[root].leaf);
        let c1 = r.head("bar_1").unwrap();
        let c8 = r.head("bar_8").unwrap();
        assert!((r.heads[c1].position[0] - 0.15).abs() < 1e-5 && (r.heads[c8].position[0] - 0.85).abs() < 1e-5);
        // root: master channels + pass-through colour of its cells
        let color = r.heads[root].attr("color").unwrap();
        assert_eq!(color.default, Value::Null);
        assert!(r.chans.iter().any(|c| c.head == root && c.enc == Enc::MasterDimmer));
        assert_eq!(r.heads_for("back", "color").unwrap(), vec![root]);
        assert_eq!(r.leaves_for("back").unwrap().len(), 8);
        assert_eq!(r.heads_for("bar_3", "color").unwrap(), vec![r.head("bar_3").unwrap()]);
    }

    #[test]
    fn original_stick_pixels_and_master_at_universe_boundary() {
        let r = rig("[fixtures.stick]\nprofile = \"chauvet_freedom_stick\"\nmode = \"50ch\"\naddress = 463");
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let f = &r.fixtures[0];
        let map: Vec<_> = r.fixture_channels(0).collect();
        assert_eq!(map.len(), 50);
        for pixel in 0..16 {
            let head = r.head(&format!("stick_{}", pixel + 1)).unwrap();
            assert_eq!(r.heads_for(&format!("stick_{}", pixel + 1), "color").unwrap(), vec![head]);
            for (component, role) in [Role::Red, Role::Green, Role::Blue].into_iter().enumerate() {
                let offset = pixel * 3 + component;
                let c = map[offset];
                assert_eq!((c.local_channel, c.address, c.head, c.channel.role, c.control),
                    (offset + 1, 463 + offset, head, role, ChannelControl::Operator));
                assert!(r.chans.iter().any(|enc| enc.head == head && enc.channel == c.address - 1));
            }
        }
        assert_eq!((map[48].address, map[48].head, map[48].control), (511, f.root, ChannelControl::Blocked));
        assert!(!map[48].strobe_available);
        assert_eq!((map[49].address, map[49].head, map[49].control), (512, f.root, ChannelControl::Managed));
        assert!(r.chans.iter().any(|c| c.channel == 511 && c.enc == Enc::MasterDimmer));
        let overflow = rig("[fixtures.stick]\nprofile = \"chauvet_freedom_stick\"\nmode = \"50ch\"\naddress = 464");
        assert!(overflow.fixtures.is_empty());
        assert!(overflow.errors.iter().any(|e| e.contains("does not fit")));
        assert!(r.profiles["chauvet_freedom_stick"].mode(Some("55ch")).is_none());
    }

    #[test]
    fn direct_rig_channels_distinguish_operator_fixed_and_managed_control() {
        let r = rig("[fixtures.par]\nprofile = \"chauvet_freedom_par_tri6\"\nmode = \"9ch\"\naddress = 200\n\
            [fixtures.scan]\nprofile = \"adj_inno_pocket_scan\"\nmode = \"6ch\"\naddress = 15");
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let par: Vec<_> = r.fixture_channels(r.fixture("par").unwrap()).collect();
        for c in &par[..4] {
            assert_eq!(c.control, ChannelControl::Operator);
        }
        for (offset, value) in [(4, 0), (6, 0), (7, 0), (8, 52)] {
            assert_eq!((par[offset].control, par[offset].channel.default), (ChannelControl::Fixed, value));
        }
        assert_eq!(par[5].control, ChannelControl::Blocked);
        let scan: Vec<_> = r.fixture_channels(r.fixture("scan").unwrap()).collect();
        assert_eq!((scan[2].control, scan[2].channel.open, scan[2].channel.closed), (ChannelControl::Managed, 8, Some(0)));
        assert!(!scan[2].strobe_available);
        assert_eq!(scan[3].channel.role, Role::Gobo);
        assert_eq!(scan[3].channel.slots.iter().map(|s| s.value).collect::<Vec<_>>(), vec![0, 8, 15, 22, 29, 36, 43, 50, 57]);
        assert!(r.heads[scan[3].head].attr("color").is_none());
        assert_eq!((scan[5].control, scan[5].channel.default), (ChannelControl::Fixed, 0));
        assert_eq!(r.fixture_channels(usize::MAX).count(), 0);
    }

    #[test]
    fn outputs_validate() {
        let r = rig(r#"
[outputs.a]
kind = "sacn"
universes = [1, 2]
destination = "multicast"
[outputs.b]
kind = "artnet"
universes = [3]
destination = "10.0.0.255"
enabled = false
[outputs.c]
kind = "laser"
[outputs.d]
kind = "sacn"
universes = [0]
"#);
        assert_eq!(r.outputs.len(), 2);
        assert_eq!(r.universes, vec![1, 2], "disabled outputs don't allocate universes");
        let e = r.errors.join("\n");
        assert!(e.contains("unknown output kind `laser`") && e.contains("universe 0 out of range"), "{e}");
    }
}
