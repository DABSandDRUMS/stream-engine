//! Fixture profiles (§9.1): our TOML format describing a fixture's DMX channels per mode and
//! how they map to normalized attributes (`intensity`, `color`, `pan`, …).

use serde::Deserialize;
use std::collections::BTreeMap;

/// What a DMX channel does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Dimmer,
    DimmerFine,
    Red,
    Green,
    Blue,
    White,
    Amber,
    Uv,
    Lime,
    Cyan,
    Magenta,
    Yellow,
    ColorWheel,
    Cto,
    Pan,
    PanFine,
    Tilt,
    TiltFine,
    Zoom,
    Focus,
    Iris,
    Frost,
    Prism,
    Gobo,
    GoboRotate,
    Strobe,
    Shutter,
    Raw,
    Fixed,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "dimmer" | "intensity" => Role::Dimmer,
            "dimmer_fine" | "intensity_fine" => Role::DimmerFine,
            "red" => Role::Red,
            "green" => Role::Green,
            "blue" => Role::Blue,
            "white" => Role::White,
            "amber" => Role::Amber,
            "uv" => Role::Uv,
            "lime" => Role::Lime,
            "cyan" => Role::Cyan,
            "magenta" => Role::Magenta,
            "yellow" => Role::Yellow,
            "color_wheel" => Role::ColorWheel,
            "cto" => Role::Cto,
            "pan" => Role::Pan,
            "pan_fine" => Role::PanFine,
            "tilt" => Role::Tilt,
            "tilt_fine" => Role::TiltFine,
            "zoom" => Role::Zoom,
            "focus" => Role::Focus,
            "iris" => Role::Iris,
            "frost" => Role::Frost,
            "prism" => Role::Prism,
            "gobo" => Role::Gobo,
            "gobo_rotate" => Role::GoboRotate,
            "strobe" => Role::Strobe,
            "shutter" => Role::Shutter,
            "raw" => Role::Raw,
            "fixed" => Role::Fixed,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Dimmer => "dimmer",
            Role::DimmerFine => "dimmer_fine",
            Role::Red => "red",
            Role::Green => "green",
            Role::Blue => "blue",
            Role::White => "white",
            Role::Amber => "amber",
            Role::Uv => "uv",
            Role::Lime => "lime",
            Role::Cyan => "cyan",
            Role::Magenta => "magenta",
            Role::Yellow => "yellow",
            Role::ColorWheel => "color_wheel",
            Role::Cto => "cto",
            Role::Pan => "pan",
            Role::PanFine => "pan_fine",
            Role::Tilt => "tilt",
            Role::TiltFine => "tilt_fine",
            Role::Zoom => "zoom",
            Role::Focus => "focus",
            Role::Iris => "iris",
            Role::Frost => "frost",
            Role::Prism => "prism",
            Role::Gobo => "gobo",
            Role::GoboRotate => "gobo_rotate",
            Role::Strobe => "strobe",
            Role::Shutter => "shutter",
            Role::Raw => "raw",
            Role::Fixed => "fixed",
        }
    }

    /// The attribute this channel renders, if any (`None` for fixed channels).
    pub fn attr(self) -> Option<&'static str> {
        Some(match self {
            Role::Dimmer | Role::DimmerFine => "intensity",
            Role::Red | Role::Green | Role::Blue | Role::Cyan | Role::Magenta | Role::Yellow | Role::ColorWheel => "color",
            Role::White => "white",
            Role::Amber => "amber",
            Role::Uv => "uv",
            Role::Lime => "lime",
            Role::Cto => "cto",
            Role::Pan | Role::PanFine => "pan",
            Role::Tilt | Role::TiltFine => "tilt",
            Role::Zoom => "zoom",
            Role::Focus => "focus",
            Role::Iris => "iris",
            Role::Frost => "frost",
            Role::Prism => "prism",
            Role::Gobo => "gobo",
            Role::GoboRotate => "gobo_rotate",
            Role::Strobe | Role::Shutter => "strobe",
            Role::Raw | Role::Fixed => return None,
        })
    }

    /// Light-emitting channels scaled by a virtual dimmer.
    pub fn is_emitter(self) -> bool {
        matches!(self, Role::Red | Role::Green | Role::Blue | Role::White | Role::Amber | Role::Uv | Role::Lime)
    }
}

/// A wheel slot (color or gobo wheel).
#[derive(Clone, Debug, PartialEq)]
pub struct Slot {
    pub value: u8,
    pub name: String,
    pub color: Option<[f32; 3]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Channel {
    pub role: Role,
    /// Attribute name for `raw` channels; role name otherwise.
    pub name: String,
    /// Human-facing physical channel function, including fixed/managed functions.
    pub label: Option<String>,
    /// Model-specific control restrictions and unresolved calibration/source caveats.
    pub restriction: Option<String>,
    /// Value sent when the attribute is unset (raw/fixed) or the channel's resting value.
    pub default: u8,
    pub invert: bool,
    /// DMX sub-range used by the attribute (strobe: slow→fast; scalars: 0→1).
    pub range: Option<(u8, u8)>,
    /// Strobe rates (Hz) at the ends of `range`.
    pub hz: Option<(f32, f32)>,
    /// Strobe/shutter value meaning "open, not strobing".
    pub open: u8,
    /// Optional shutter byte for zero intensity / blackout. Never inferred from `open`.
    pub closed: Option<u8>,
    pub slots: Vec<Slot>,
    /// Pan/tilt: full travel in degrees; zoom: beam angle at 0 and 1.
    pub deg: Option<(f32, f32)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WhiteMix {
    #[default]
    None,
    /// Derive white from min(r, g, b) and subtract it from the colour channels.
    Extract,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Mode {
    pub id: String,
    /// Channels of the whole fixture (for multi-cell fixtures: the master channels).
    pub channels: Vec<Channel>,
    pub cells: u32,
    pub cell_channels: Vec<Channel>,
    pub cells_first: bool,
    pub white_mix: WhiteMix,
}

impl Mode {
    /// Total DMX footprint.
    pub fn footprint(&self) -> usize {
        self.channels.len() + self.cells as usize * self.cell_channels.len()
    }

    /// Offsets (0-based within the fixture) of the master channels and of each cell's first channel.
    pub fn layout(&self) -> (usize, Vec<usize>) {
        let cw = self.cell_channels.len();
        if self.cells_first {
            let cells = (0..self.cells as usize).map(|i| i * cw).collect();
            (self.cells as usize * cw, cells)
        } else {
            let base = self.channels.len();
            (0, (0..self.cells as usize).map(|i| base + i * cw).collect())
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub manufacturer: String,
    pub kind: String,
    pub beam: f32,
    pub power_w: Option<f32>,
    pub notes: Option<String>,
    pub modes: Vec<Mode>,
}

impl Profile {
    pub fn mode(&self, id: Option<&str>) -> Option<&Mode> {
        match id {
            Some(m) => self.modes.iter().find(|x| x.id == m),
            None => self.modes.first(),
        }
    }

    /// Parse `lights/fixtures/<id>.toml`.
    pub fn parse(id: &str, table: &toml::Table) -> Result<Profile, String> {
        let raw: RawProfile = toml::Value::Table(table.clone()).try_into().map_err(|e: toml::de::Error| e.message().to_string())?;
        let mut modes = Vec::new();
        // `toml::Table` keeps document order (preserve_order), so the first mode is the default.
        for (mid, m) in raw.modes {
            let m: RawMode = m.try_into().map_err(|e: toml::de::Error| format!("mode `{mid}`: {}", e.message()))?;
            let channels = m
                .channels
                .iter()
                .enumerate()
                .map(|(i, c)| c.build().map_err(|e| format!("mode `{mid}` channel {}: {e}", i + 1)))
                .collect::<Result<Vec<_>, _>>()?;
            let cell_channels = m
                .cell_channels
                .iter()
                .enumerate()
                .map(|(i, c)| c.build().map_err(|e| format!("mode `{mid}` cell channel {}: {e}", i + 1)))
                .collect::<Result<Vec<_>, _>>()?;
            if m.cells > 0 && cell_channels.is_empty() {
                return Err(format!("mode `{mid}`: `cells` needs `cell_channels`"));
            }
            if m.cells == 0 && !cell_channels.is_empty() {
                return Err(format!("mode `{mid}`: `cell_channels` needs `cells`"));
            }
            let white_mix = match m.white_mix.as_deref() {
                None | Some("none") => WhiteMix::None,
                Some("extract") => WhiteMix::Extract,
                Some(o) => return Err(format!("mode `{mid}`: unknown white_mix `{o}`")),
            };
            let mode = Mode { id: mid.clone(), channels, cells: m.cells, cell_channels, cells_first: m.cells_first, white_mix };
            if mode.footprint() == 0 {
                return Err(format!("mode `{mid}` has no channels"));
            }
            if mode.footprint() > 512 {
                return Err(format!("mode `{mid}` has {} channels (max 512)", mode.footprint()));
            }
            validate_fine(&mode.channels).map_err(|e| format!("mode `{mid}`: {e}"))?;
            validate_fine(&mode.cell_channels).map_err(|e| format!("mode `{mid}` cells: {e}"))?;
            modes.push(mode);
        }
        if modes.is_empty() {
            return Err("profile has no [modes.<id>]".into());
        }
        Ok(Profile {
            id: id.to_string(),
            name: raw.name.unwrap_or_else(|| id.to_string()),
            manufacturer: raw.manufacturer.unwrap_or_default(),
            kind: raw.kind.unwrap_or_else(|| "par".into()),
            beam: raw.beam.unwrap_or(25.0),
            power_w: raw.power_w,
            notes: raw.notes,
            modes,
        })
    }
}

/// A fine channel needs its coarse partner in the same channel list.
fn validate_fine(chs: &[Channel]) -> Result<(), String> {
    for (fine, coarse) in [(Role::DimmerFine, Role::Dimmer), (Role::PanFine, Role::Pan), (Role::TiltFine, Role::Tilt)] {
        let nf = chs.iter().filter(|c| c.role == fine).count();
        let nc = chs.iter().filter(|c| c.role == coarse).count();
        if nf > 1 || nc > 1 {
            return Err(format!("`{}` appears more than once", coarse.as_str()));
        }
        if nf == 1 && nc == 0 {
            return Err(format!("`{}` without `{}`", fine.as_str(), coarse.as_str()));
        }
    }
    let mut names = std::collections::HashSet::new();
    for c in chs.iter().filter(|c| c.role == Role::Raw) {
        if !names.insert(c.name.as_str()) {
            return Err(format!("raw channel `{}` appears twice", c.name));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    name: Option<String>,
    manufacturer: Option<String>,
    kind: Option<String>,
    beam: Option<f32>,
    #[serde(default)]
    power_w: Option<f32>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    modes: toml::map::Map<String, toml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMode {
    #[serde(default)]
    channels: Vec<RawChannel>,
    #[serde(default)]
    cells: u32,
    #[serde(default)]
    cell_channels: Vec<RawChannel>,
    #[serde(default)]
    cells_first: bool,
    white_mix: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawChannel {
    Role(String),
    Full(RawChannelTable),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawChannelTable {
    role: String,
    name: Option<String>,
    default: Option<u8>,
    #[serde(default)]
    invert: bool,
    range: Option<[u8; 2]>,
    hz: Option<[f32; 2]>,
    open: Option<u8>,
    closed: Option<u8>,
    value: Option<u8>,
    #[serde(default)]
    slots: Vec<RawSlot>,
    deg: Option<toml::Value>,
    #[serde(default)]
    label: Option<String>,
    restriction: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSlot {
    value: u8,
    name: Option<String>,
    color: Option<String>,
}

impl RawChannel {
    fn build(&self) -> Result<Channel, String> {
        let t = match self {
            RawChannel::Role(r) => RawChannelTable {
                role: r.clone(),
                name: None,
                default: None,
                invert: false,
                range: None,
                hz: None,
                open: None,
                value: None,
                closed: None,
                slots: Vec::new(),
                deg: None,
                label: None,
                restriction: None,
            },
            RawChannel::Full(t) => RawChannelTable {
                role: t.role.clone(),
                name: t.name.clone(),
                default: t.default,
                invert: t.invert,
                range: t.range,
                hz: t.hz,
                open: t.open,
                value: t.value,
                closed: t.closed,
                slots: t.slots.iter().map(|s| RawSlot { value: s.value, name: s.name.clone(), color: s.color.clone() }).collect(),
                deg: t.deg.clone(),
                label: t.label.clone(),
                restriction: t.restriction.clone(),
            },
        };
        let role = Role::parse(&t.role).ok_or_else(|| format!("unknown role `{}`", t.role))?;
        let name = match role {
            Role::Raw => {
                let n = t.name.clone().ok_or("`raw` channel needs `name`")?;
                if !se_proto::address::is_valid(&n, false) || n.contains('.') {
                    return Err(format!("bad raw channel name `{n}`"));
                }
                if RESERVED.contains(&n.as_str()) {
                    return Err(format!("raw channel name `{n}` collides with an attribute"));
                }
                n
            }
            _ => role.as_str().to_string(),
        };
        let mut slots = Vec::new();
        for s in &t.slots {
            let color = match &s.color {
                Some(c) => {
                    let [r, g, b, _] = se_proto::value::parse_hex_color(c).ok_or_else(|| format!("bad slot color `{c}`"))?;
                    Some([r, g, b])
                }
                None => None,
            };
            slots.push(Slot { value: s.value, name: s.name.clone().unwrap_or_else(|| format!("{}", s.value)), color });
        }
        if role == Role::ColorWheel && (slots.is_empty() || slots.iter().any(|s| s.color.is_none())) {
            return Err("`color_wheel` needs `slots` with a `color` each".into());
        }
        if role == Role::Gobo && slots.is_empty() {
            return Err("`gobo` needs `slots`".into());
        }
        let deg = match &t.deg {
            None => None,
            Some(toml::Value::Integer(i)) => Some((0.0, *i as f32)),
            Some(toml::Value::Float(f)) => Some((0.0, *f as f32)),
            Some(toml::Value::Array(a)) if a.len() == 2 => {
                let f = |v: &toml::Value| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)).map(|x| x as f32);
                Some((f(&a[0]).ok_or("bad `deg`")?, f(&a[1]).ok_or("bad `deg`")?))
            }
            Some(_) => return Err("`deg` must be a number or [min, max]".into()),
        };
        if let Some([lo, hi]) = t.range
            && lo > hi
        {
            return Err(format!("range [{lo}, {hi}] is reversed"));
        }
        if matches!(role, Role::Strobe | Role::Shutter)
            && let Some([a, b]) = t.hz
            && (a <= 0.0 || b <= 0.0)
        {
            return Err("strobe `hz` must be positive".into());
        }
        if role == Role::Fixed && t.value.is_none() && t.default.is_none() {
            return Err("`fixed` channel needs `value`".into());
        }
        let default = t.value.or(t.default).unwrap_or(0);
        Ok(Channel {
            role,
            name,
            label: t.label,
            restriction: t.restriction,
            default,
            invert: t.invert,
            range: t.range.map(|[a, b]| (a, b)),
            hz: t.hz.map(|[a, b]| (a, b)),
            open: t.open.unwrap_or(default),
            slots,
            closed: t.closed,
            deg,
        })
    }
}

/// Attribute names a raw channel may not use.
pub const RESERVED: &[&str] = &[
    "intensity",
    "color",
    "white",
    "amber",
    "uv",
    "lime",
    "cto",
    "pan",
    "tilt",
    "zoom",
    "focus",
    "iris",
    "frost",
    "prism",
    "gobo",
    "gobo_rotate",
    "strobe",
    "master",
    "position",
    "palette",
    "palettes",
];

/// Profiles by id: the built-in library (embedded from `project-example/lights/fixtures`)
/// overlaid with the project's `lights/fixtures/*.toml`.
pub fn library(project: &BTreeMap<String, toml::Table>, errors: &mut Vec<String>) -> BTreeMap<String, Profile> {
    let mut out = BTreeMap::new();
    for (id, src) in BUILTIN {
        match toml::from_str::<toml::Table>(src).map_err(|e| e.to_string()).and_then(|t| Profile::parse(id, &t)) {
            Ok(p) => {
                out.insert(id.to_string(), p);
            }
            Err(e) => errors.push(format!("built-in fixture `{id}`: {e}")),
        }
    }
    for (id, t) in project {
        match Profile::parse(id, t) {
            Ok(p) => {
                out.insert(id.clone(), p);
            }
            Err(e) => errors.push(format!("lights/fixtures/{id}.toml: {e}")),
        }
    }
    out
}

/// Fixture library shipped with the engine (same files as the starter project).
pub const BUILTIN: &[(&str, &str)] = &[
    ("generic_dimmer", include_str!("../../../project-example/lights/fixtures/generic_dimmer.toml")),
    ("generic_rgb", include_str!("../../../project-example/lights/fixtures/generic_rgb.toml")),
    ("generic_rgbw", include_str!("../../../project-example/lights/fixtures/generic_rgbw.toml")),
    ("generic_rgbawuv", include_str!("../../../project-example/lights/fixtures/generic_rgbawuv.toml")),
    ("generic_moving_head", include_str!("../../../project-example/lights/fixtures/generic_moving_head.toml")),
    ("generic_led_bar", include_str!("../../../project-example/lights/fixtures/generic_led_bar.toml")),
    ("generic_strobe", include_str!("../../../project-example/lights/fixtures/generic_strobe.toml")),
    ("generic_cmy_wash", include_str!("../../../project-example/lights/fixtures/generic_cmy_wash.toml")),
    ("chauvet_colorstrip", include_str!("../../../project-example/lights/fixtures/chauvet_colorstrip.toml")),
    ("chauvet_freedom_par_tri6", include_str!("../../../project-example/lights/fixtures/chauvet_freedom_par_tri6.toml")),
    ("chauvet_freedom_stick", include_str!("../../../project-example/lights/fixtures/chauvet_freedom_stick.toml")),
    ("adj_inno_pocket_scan", include_str!("../../../project-example/lights/fixtures/adj_inno_pocket_scan.toml")),
    ("chauvet_circus_20_irc", include_str!("../../../project-example/lights/fixtures/chauvet_circus_20_irc.toml")),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Result<Profile, String> {
        Profile::parse("t", &toml::from_str(src).unwrap())
    }

    #[test]
    fn modes_keep_file_order_and_footprints() {
        let p = parse(
            r##"name = "X"
kind = "bar"
[modes.26ch]
channels = ["dimmer", { role = "strobe", range = [10, 255], hz = [1, 20], open = 0 }]
cells = 8
cell_channels = ["red", "green", "blue"]
[modes.3ch]
channels = ["red", "green", "blue"]
"##,
        )
        .unwrap();
        assert_eq!(p.modes[0].id, "26ch", "first mode in the file is the default");
        assert_eq!(p.mode(None).unwrap().footprint(), 26);
        let (master, cells) = p.modes[0].layout();
        assert_eq!(master, 0);
        assert_eq!(cells, vec![2, 5, 8, 11, 14, 17, 20, 23]);
        assert_eq!(p.modes[0].channels[1].hz, Some((1.0, 20.0)));
    }

    #[test]
    fn rejects_bad_profiles() {
        assert!(parse("[modes.a]\nchannels = [\"sparkles\"]").unwrap_err().contains("unknown role"));
        assert!(parse("[modes.a]\nchannels = [\"pan_fine\"]").unwrap_err().contains("without"));
        assert!(parse("[modes.a]\nchannels = [{ role = \"raw\" }]").unwrap_err().contains("name"));
        assert!(parse("[modes.a]\nchannels = [{ role = \"raw\", name = \"color\" }]").unwrap_err().contains("collides"));
        assert!(parse("[modes.a]\nchannels = [{ role = \"color_wheel\" }]").unwrap_err().contains("slots"));
        assert!(parse("[modes.a]\ncells = 2\nchannels = [\"dimmer\"]").unwrap_err().contains("cell_channels"));
        assert!(parse("name = \"x\"").unwrap_err().contains("no [modes"));
        assert!(parse("[modes.a]\nchannels = [\"red\"]\nbogus = 1").is_err(), "unknown keys are errors");
    }

    #[test]
    fn builtin_library_parses() {
        let mut errors = Vec::new();
        let lib = library(&BTreeMap::new(), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(lib.len(), BUILTIN.len());
        assert_eq!(lib["generic_rgb"].mode(Some("3ch")).unwrap().footprint(), 3);
        let mh = &lib["generic_moving_head"];
        assert!(mh.modes.iter().any(|m| m.channels.iter().any(|c| c.role == Role::PanFine)));
        let bar = &lib["generic_led_bar"];
        assert!(bar.modes.iter().any(|m| m.cells == 8));
    }
}
