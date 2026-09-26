//! The render plan: everything the render thread needs from the project configuration and the
//! patch manifests, pre-digested on the IO side (strings interned, expressions parsed, sources
//! resolved) so the frame loop never parses, formats, or allocates.

use crate::addr::{AddrId, AddrTable, WhenPlan};
use crate::effects::{self, LIBRARY, MAX_PARAMS, Point};
use se_core::config::{Config, FxRef, NodeDef, TransitionDef};
use se_patch::{Kind, Layer, Manifest};
use se_proto::{Ease, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Canvas slots (= frames.sock canvas ids).
pub const WIDE: usize = 0;
pub const TALL: usize = 1;
pub const PREVIEW: usize = 2;
pub const ATLAS: usize = 3;
pub const CANVASES: usize = 4;
pub const CANVAS_NAMES: [&str; CANVASES] = ["wide", "tall", "preview", "atlas"];
/// Scene layouts (scene `[canvas.<name>]` tables) the renderer composes.
pub const LAYOUTS: usize = 2;
pub const LAYOUT_NAMES: [&str; LAYOUTS] = ["wide", "tall"];

/// Palette slot names, in `se_patch::wgsl::PALETTE` order.
pub const PALETTE_SLOTS: [&str; 8] = ["accent", "background", "foreground", "red", "yellow", "green", "cyan", "magenta"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModifierPref {
    Auto,
    Linear,
    Explicit(u64),
}

/// `[render]` and `[safety]` settings of project.toml.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub adapter: Option<String>,
    pub frames_socket: Option<PathBuf>,
    pub export_modifier: ModifierPref,
    pub buffers: u32,
    pub preview_scale: f32,
    pub preview_layout: usize,
    pub atlas_size: [u32; 2],
    pub atlas_fps: u32,
    pub no_signal: [f32; 4],
    pub font: Option<String>,
    pub flash_limit: bool,
    pub max_flashes: f32,
    pub palette_mode_follow_theme: bool,
    pub palette: [[f32; 4]; 8],
}

/// Default stream palette (Kanagawa-like; replaced by `[palette]` or the Omarchy theme).
pub const DEFAULT_PALETTE: [[f32; 4]; 8] = [
    [0.494, 0.612, 0.847, 1.0],
    [0.086, 0.086, 0.114, 1.0],
    [0.863, 0.843, 0.729, 1.0],
    [0.910, 0.141, 0.141, 1.0],
    [0.902, 0.765, 0.518, 1.0],
    [0.596, 0.733, 0.424, 1.0],
    [0.498, 0.706, 0.792, 1.0],
    [0.584, 0.498, 0.722, 1.0],
];

impl Default for Settings {
    fn default() -> Self {
        Settings {
            adapter: None,
            frames_socket: None,
            export_modifier: ModifierPref::Auto,
            buffers: 4,
            preview_scale: 0.5,
            preview_layout: WIDE,
            atlas_size: [1920, 1080],
            atlas_fps: 30,
            no_signal: [0.063, 0.063, 0.078, 1.0],
            font: None,
            flash_limit: true,
            max_flashes: 3.0,
            palette_mode_follow_theme: false,
            palette: DEFAULT_PALETTE,
        }
    }
}

fn color_of(v: &toml::Value) -> Option<[f32; 4]> {
    Value::from(v.clone()).as_color()
}

impl Settings {
    pub fn from_config(c: &Config, errors: &mut Vec<String>) -> Settings {
        let mut s = Settings::default();
        if let Some(toml::Value::Table(r)) = c.project.extra.get("render") {
            for (k, v) in r {
                let bad = |errors: &mut Vec<String>| errors.push(format!("[render] {k}: unexpected value `{v}`"));
                match k.as_str() {
                    "adapter" => match v.as_str() {
                        Some(a) => s.adapter = Some(a.to_string()),
                        None => bad(errors),
                    },
                    "frames_socket" => match v.as_str() {
                        Some(a) => s.frames_socket = Some(PathBuf::from(a)),
                        None => bad(errors),
                    },
                    "export_modifier" => match v.as_str() {
                        Some("auto") => s.export_modifier = ModifierPref::Auto,
                        Some("linear") => s.export_modifier = ModifierPref::Linear,
                        Some(h) if h.starts_with("0x") => match u64::from_str_radix(&h[2..], 16) {
                            Ok(m) => s.export_modifier = ModifierPref::Explicit(m),
                            Err(_) => bad(errors),
                        },
                        _ => bad(errors),
                    },
                    "buffers" => match v.as_integer() {
                        Some(n @ 3..=4) => s.buffers = n as u32,
                        _ => bad(errors),
                    },
                    "preview" => {
                        if let Some(t) = v.as_table() {
                            if let Some(sc) = t.get("scale").and_then(|x| x.as_float().or(x.as_integer().map(|i| i as f64))) {
                                s.preview_scale = (sc as f32).clamp(0.1, 1.0);
                            }
                            if let Some(l) = t.get("layout").and_then(|x| x.as_str()) {
                                match LAYOUT_NAMES.iter().position(|n| *n == l) {
                                    Some(i) => s.preview_layout = i,
                                    None => bad(errors),
                                }
                            }
                        } else {
                            bad(errors)
                        }
                    }
                    "atlas" => {
                        if let Some(t) = v.as_table() {
                            let int = |k: &str| t.get(k).and_then(|x| x.as_integer());
                            if let Some(w) = int("width") {
                                s.atlas_size[0] = (w as u32).clamp(64, 3840);
                            }
                            if let Some(h) = int("height") {
                                s.atlas_size[1] = (h as u32).clamp(64, 2160);
                            }
                            if let Some(f) = int("fps") {
                                s.atlas_fps = (f as u32).clamp(1, 60);
                            }
                        } else {
                            bad(errors)
                        }
                    }
                    "no_signal" => match v.as_str() {
                        Some("transparent") => s.no_signal = [0.0; 4],
                        _ => match color_of(v) {
                            Some(c) => s.no_signal = c,
                            None => bad(errors),
                        },
                    },
                    "font" => match v.as_str() {
                        Some(f) => s.font = Some(f.to_string()),
                        None => bad(errors),
                    },
                    // attachment lists, parsed with the canvases
                    "canvas_fx" | "output_fx" => {}
                    _ => errors.push(format!("[render] unknown key `{k}`")),
                }
            }
        }
        if let Some(toml::Value::Table(t)) = c.project.extra.get("safety") {
            if let Some(b) = t.get("video_flash_limit").and_then(|v| v.as_bool()) {
                s.flash_limit = b;
            }
            if let Some(n) = t.get("video_max_flashes").and_then(|v| v.as_float().or(v.as_integer().map(|i| i as f64))) {
                s.max_flashes = (n as f32).clamp(0.5, 30.0);
            }
        }
        if let Some(toml::Value::Table(t)) = c.project.extra.get("palette") {
            if let Some(m) = t.get("mode").and_then(|v| v.as_str()) {
                match m {
                    "follow_theme" => s.palette_mode_follow_theme = true,
                    "fixed" => s.palette_mode_follow_theme = false,
                    _ => errors.push(format!("[palette] mode `{m}` (expected fixed | follow_theme)")),
                }
            }
            for (i, slot) in PALETTE_SLOTS.iter().enumerate() {
                if let Some(v) = t.get(*slot) {
                    match color_of(v) {
                        Some(c) => s.palette[i] = c,
                        None => errors.push(format!("[palette] {slot}: not a color")),
                    }
                }
            }
        }
        s
    }
}

#[derive(Clone, Debug)]
pub struct CanvasPlan {
    pub name: &'static str,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Scene layout this canvas renders.
    pub layout: usize,
    pub fx: Vec<Attach>,
    pub output_fx: Vec<Attach>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceKind {
    /// `hub.video` slot (cameras, media, web pages, the YouTube player).
    Video {
        slot: String,
    },
    /// `hub.draw` slot (script patches), rendered with vello.
    Draw {
        slot: String,
    },
    /// Shader or particles patch rendered by us.
    Patch {
        id: String,
    },
    Solid([f32; 4]),
}

/// Per-source color addresses (published/declared by the video input slice).
#[derive(Clone, Copy, Debug)]
pub struct ColorAddrs {
    pub brightness: AddrId,
    pub contrast: AddrId,
    pub saturation: AddrId,
    pub gamma: AddrId,
    pub temperature: AddrId,
    pub tint: AddrId,
    pub lut: AddrId,
    pub lut_amount: AddrId,
    pub matrix: AddrId,
    pub range: AddrId,
}

#[derive(Clone, Debug)]
pub struct SourcePlan {
    pub name: String,
    pub kind: SourceKind,
    pub fx: Vec<Attach>,
    /// Render size of generated sources per layout (max node size), `[0, 0]` when unused there.
    pub sizes: [[u32; 2]; LAYOUTS],
    pub color: ColorAddrs,
    /// Shown in the multiview atlas.
    pub in_atlas: bool,
    /// Used as a global overlay layer (generated per canvas at canvas size).
    pub overlay: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamSrc {
    Global(AddrId),
    /// Attachment-local address with its configured value as fallback.
    Local(AddrId, f32),
    Const(f32),
}

#[derive(Clone, Debug, PartialEq)]
pub enum StrSrc {
    Addr(AddrId),
    Const(String),
}

/// One effect attachment (or the implicit global instance).
#[derive(Clone, Debug)]
pub struct Attach {
    pub effect: usize,
    pub when: Option<u32>,
    pub group: Option<u32>,
    pub enabled: Option<AddrId>,
    /// `enabled = false`: only while triggered.
    pub triggered_only: bool,
    /// The implicit global instance: `max(amount, level × env)`.
    pub global: bool,
    pub params: [ParamSrc; MAX_PARAMS],
    pub nparams: usize,
    pub file: Option<StrSrc>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EffectKind {
    Builtin(usize),
    /// Effect-layer shader patch.
    Patch(String),
}

#[derive(Clone, Debug)]
pub struct EffectPlan {
    pub name: String,
    pub kind: EffectKind,
    pub point: Point,
    pub flashy: bool,
    /// Global param addresses in uniform order (builtins: amount, level, own params).
    pub params: Vec<AddrId>,
    pub defaults: Vec<f32>,
    pub env: AddrId,
    pub has_trigger: bool,
    pub file: Option<AddrId>,
    pub identity: Option<&'static [f32]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blend {
    Normal,
    Add,
    Screen,
    Multiply,
}

impl Blend {
    pub const ALL: [Blend; 4] = [Blend::Normal, Blend::Add, Blend::Screen, Blend::Multiply];
    pub fn parse(s: &str) -> Option<Blend> {
        Some(match s {
            "normal" | "" => Blend::Normal,
            "add" | "additive" => Blend::Add,
            "screen" => Blend::Screen,
            "multiply" => Blend::Multiply,
            _ => return None,
        })
    }
    pub fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    None,
    Fade,
    Scale,
    SlideLeft,
    SlideRight,
    SlideUp,
    SlideDown,
    /// Shader style `Plan::styles[i]` (a shader patch or a `.wgsl` file): the node is drawn
    /// through it with `se.progress` = presence (0 = gone, 1 = in place).
    Custom(u32),
}

impl Style {
    /// Built-in styles (custom ones are resolved by the plan builder).
    pub fn parse(s: &str) -> Option<Style> {
        Some(match s {
            "none" | "cut" => Style::None,
            "fade" => Style::Fade,
            "scale" => Style::Scale,
            "slide_left" => Style::SlideLeft,
            "slide_right" => Style::SlideRight,
            "slide_up" => Style::SlideUp,
            "slide_down" => Style::SlideDown,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NodeDefaults {
    pub rect: [f32; 4],
    pub crop: [f32; 4],
    pub radius: f32,
    pub z: i64,
    pub opacity: f32,
    pub offset: [f32; 2],
    pub scale: f32,
    pub rotation: f32,
    pub visible: bool,
}

#[derive(Clone, Debug)]
pub struct NodePlan {
    pub id: String,
    pub source: u32,
    pub rect: AddrId,
    pub crop: AddrId,
    pub radius: AddrId,
    pub z: AddrId,
    pub opacity: AddrId,
    pub offset_x: AddrId,
    pub offset_y: AddrId,
    pub scale: AddrId,
    pub rotation: AddrId,
    pub visible: AddrId,
    pub def: NodeDefaults,
    pub blend: Blend,
    pub mask: Option<String>,
    pub when: Option<u32>,
    pub fx: Vec<Attach>,
    pub enter: Option<Style>,
    pub exit: Option<Style>,
}

#[derive(Clone, Debug, Default)]
pub struct LayoutPlan {
    pub nodes: Vec<NodePlan>,
    pub fx: Vec<Attach>,
    pub background: Option<[f32; 4]>,
}

#[derive(Clone, Debug)]
pub struct ScenePlan {
    pub name: String,
    pub layouts: [LayoutPlan; LAYOUTS],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrKind {
    Cut,
    Morph,
    Shader,
    Combined,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ShaderSource {
    Builtin(&'static str),
    File(PathBuf),
    Patch(String),
}

#[derive(Clone, Debug)]
pub struct TransitionPlan {
    pub name: String,
    pub kind: TrKind,
    pub shader: Option<ShaderSource>,
    pub ease: Ease,
    pub enter: Style,
    pub exit: Style,
    /// Float/vec params exposed to the shader header (`p_<name>()`).
    pub params: Vec<(String, se_patch::ParamSpec)>,
}

#[derive(Clone, Debug)]
pub struct OverlayPlan {
    pub id: String,
    pub source: u32,
    pub canvases: [bool; CANVASES],
    pub rect: [Option<[f32; 4]>; CANVASES],
    pub when: Option<u32>,
    pub z: i32,
    pub fx: bool,
    /// Visible only while `env > 0` (patches with a trigger).
    pub env: Option<AddrId>,
}

/// Shader/particles patch runtime inputs.
#[derive(Clone, Debug)]
pub struct PatchPlan {
    pub manifest: Arc<Manifest>,
    pub layout: se_patch::wgsl::Layout,
    pub params: Vec<AddrId>,
    pub defaults: Vec<[f32; 4]>,
    pub signals: Vec<AddrId>,
    pub env: AddrId,
}

/// State addresses of the show (core).
#[derive(Clone, Copy, Debug)]
pub struct ShowAddrs {
    pub program: AddrId,
    pub preview: AddrId,
    pub tr_name: AddrId,
    pub tr_ms: AddrId,
    pub tr_start: AddrId,
    pub tr_active: AddrId,
    pub tr_from: AddrId,
    pub beat_phase: AddrId,
    pub bass: AddrId,
    pub level: AddrId,
}

pub struct Plan {
    pub settings: Settings,
    pub canvases: [CanvasPlan; CANVASES],
    pub sources: Vec<SourcePlan>,
    pub source_index: HashMap<String, u32>,
    pub effects: Vec<EffectPlan>,
    pub effect_index: HashMap<String, usize>,
    pub scenes: Vec<ScenePlan>,
    pub scene_index: HashMap<String, usize>,
    pub transitions: Vec<TransitionPlan>,
    pub transition_index: HashMap<String, usize>,
    pub overlays: Vec<OverlayPlan>,
    pub patches: Vec<PatchPlan>,
    pub patch_index: HashMap<String, usize>,
    pub whens: Vec<WhenPlan>,
    pub groups: Vec<String>,
    pub palette: [AddrId; 8],
    /// `se_patch::wgsl::STANDARD_SIGNALS` in the signal table (transition headers).
    pub std_signals: Vec<AddrId>,
    pub show: ShowAddrs,
    pub state: AddrTable,
    pub signals: AddrTable,
    pub project_root: PathBuf,
    /// Shaders of custom enter/exit styles ([`Style::Custom`] indexes this).
    pub styles: Vec<ShaderSource>,
    /// Non-fatal problems (logged and shown on `health.render`).
    pub errors: Vec<String>,
}

fn f32_of(v: &Value) -> Option<f32> {
    v.as_f32().filter(|f| f.is_finite())
}

struct Builder<'a> {
    cfg: &'a Config,
    manifests: HashMap<String, Arc<Manifest>>,
    state: AddrTable,
    signals: AddrTable,
    whens: Vec<WhenPlan>,
    when_src: HashMap<String, u32>,
    groups: Vec<String>,
    effects: Vec<EffectPlan>,
    effect_index: HashMap<String, usize>,
    sources: Vec<SourcePlan>,
    source_index: HashMap<String, u32>,
    styles: Vec<ShaderSource>,
    errors: Vec<String>,
}

impl Builder<'_> {
    /// An enter/exit style: a built-in name, `patch.<id>` (a shader patch), or a project `.wgsl`
    /// file. Unknown values are reported and yield `None`.
    fn style(&mut self, s: &str, ctx: &str) -> Option<Style> {
        if let Some(st) = Style::parse(s) {
            return Some(st);
        }
        let src = if let Some(id) = s.strip_prefix("patch.") {
            match self.manifests.get(id) {
                Some(m) if m.kind == Kind::Shader => ShaderSource::Patch(id.to_string()),
                Some(_) => {
                    self.errors.push(format!("{ctx}: style `{s}` must be a shader patch"));
                    return None;
                }
                None => {
                    self.errors.push(format!("{ctx}: style `{s}`: no such patch"));
                    return None;
                }
            }
        } else if s.ends_with(".wgsl") {
            ShaderSource::File(PathBuf::from(s))
        } else {
            self.errors
                .push(format!("{ctx}: unknown style `{s}` (fade, scale, slide_left, slide_right, slide_up, slide_down, none, patch.<id>, or a .wgsl file)"));
            return None;
        };
        let i = match self.styles.iter().position(|x| *x == src) {
            Some(i) => i,
            None => {
                self.styles.push(src);
                self.styles.len() - 1
            }
        };
        Some(Style::Custom(i as u32))
    }

    fn when(&mut self, src: &Option<String>, ctx: &str) -> Option<u32> {
        let s = src.as_deref()?.trim();
        if s.is_empty() {
            return None;
        }
        if let Some(i) = self.when_src.get(s) {
            return Some(*i);
        }
        match WhenPlan::new(s, &mut self.state, &mut self.signals) {
            Ok(w) => {
                let i = self.whens.len() as u32;
                self.whens.push(w);
                self.when_src.insert(s.to_string(), i);
                Some(i)
            }
            Err(e) => {
                // A broken condition hides the element (fail closed) rather than showing it.
                self.errors.push(format!("{ctx}: when {e}"));
                let i = self.whens.len() as u32;
                self.whens.push(WhenPlan::new("false", &mut self.state, &mut self.signals).expect("literal parses"));
                self.when_src.insert(s.to_string(), i);
                Some(i)
            }
        }
    }

    fn group(&mut self, g: &Option<String>) -> Option<u32> {
        let g = g.as_deref()?;
        Some(match self.groups.iter().position(|x| x == g) {
            Some(i) => i as u32,
            None => {
                self.groups.push(g.to_string());
                (self.groups.len() - 1) as u32
            }
        })
    }

    fn effect(&mut self, name: &str) -> Option<usize> {
        if let Some(i) = self.effect_index.get(name) {
            return Some(*i);
        }
        let id = name.strip_prefix("patch.")?;
        let m = self.manifests.get(id)?.clone();
        if m.kind != Kind::Shader || m.layer != Layer::Effect {
            return None;
        }
        let base = format!("patch.{id}");
        let mut params = Vec::new();
        let mut defaults = Vec::new();
        for (n, p, _) in m.param_slots() {
            params.push(self.state.intern(&format!("{base}.{n}")));
            defaults.push(p.default_value().as_f32().unwrap_or(0.0));
        }
        let e = EffectPlan {
            name: name.to_string(),
            kind: EffectKind::Patch(id.to_string()),
            point: Point::Canvas,
            flashy: true,
            params,
            defaults,
            env: self.state.intern(&format!("{base}.env")),
            has_trigger: m.has_trigger,
            file: None,
            identity: None,
        };
        self.effects.push(e);
        let i = self.effects.len() - 1;
        self.effect_index.insert(name.to_string(), i);
        Some(i)
    }

    /// `local`: address prefix of attachment-local params declared by the core (node fx).
    fn attach(&mut self, r: &FxRef, local: Option<&str>, ctx: &str) -> Option<Attach> {
        let Some(effect) = self.effect(&r.name) else {
            self.errors.push(format!("{ctx}: unknown effect `{}`", r.name));
            return None;
        };
        let e = self.effects[effect].clone();
        let mut params = [ParamSrc::Const(0.0); MAX_PARAMS];
        let names: Vec<String> = match &e.kind {
            EffectKind::Builtin(i) => LIBRARY[*i].all_params().map(|p| p.name.to_string()).collect(),
            EffectKind::Patch(id) => self.manifests[id].param_slots().iter().map(|(n, _, _)| n.to_string()).collect(),
        };
        for (i, n) in names.iter().enumerate().take(MAX_PARAMS) {
            params[i] = match r.params.get(n) {
                Some(v) => {
                    let val = f32_of(v).unwrap_or(e.defaults[i]);
                    match local {
                        Some(pre) => ParamSrc::Local(self.state.intern(&format!("{pre}.{n}")), val),
                        None => ParamSrc::Const(val),
                    }
                }
                // Attached without an explicit amount: on at full strength.
                None if n == "amount" && matches!(e.kind, EffectKind::Builtin(_)) => ParamSrc::Const(1.0),
                None => ParamSrc::Global(e.params[i]),
            };
        }
        let file = match r.params.get("file") {
            Some(Value::Str(s)) => Some(StrSrc::Const(s.clone())),
            _ => e.file.map(StrSrc::Addr),
        };
        for k in r.params.keys() {
            if k != "file" && !names.iter().any(|n| n == k) {
                self.errors.push(format!("{ctx}: effect `{}` has no param `{k}`", r.name));
            }
        }
        Some(Attach {
            effect,
            when: self.when(&r.when, ctx),
            group: self.group(&r.group),
            enabled: local.map(|pre| self.state.intern(&format!("{pre}.enabled"))),
            triggered_only: r.enabled == Some(false),
            global: false,
            params,
            nparams: names.len().min(MAX_PARAMS),
            file,
        })
    }

    fn attaches(&mut self, list: &[FxRef], local_prefix: Option<&str>, ctx: &str) -> Vec<Attach> {
        list.iter()
            .filter_map(|r| {
                let local = local_prefix.map(|p| format!("{p}.fx.{}", r.name));
                self.attach(r, local.as_deref(), ctx)
            })
            .collect()
    }

    fn source(&mut self, name: &str) -> u32 {
        if let Some(i) = self.source_index.get(name) {
            return *i;
        }
        let kind = if let Some(id) = name.strip_prefix("patch.") {
            match self.manifests.get(id).map(|m| m.kind) {
                Some(Kind::Shader | Kind::Particles) => SourceKind::Patch { id: id.to_string() },
                Some(Kind::Script) => SourceKind::Draw { slot: name.to_string() },
                _ => SourceKind::Video { slot: name.to_string() },
            }
        } else if let Some(c) = name.strip_prefix("color:").or_else(|| name.starts_with('#').then_some(name)) {
            match se_proto::value::parse_hex_color(c.trim_start_matches('#')).or_else(|| se_proto::value::parse_hex_color(c)) {
                Some(col) => SourceKind::Solid(col),
                None => {
                    self.errors.push(format!("source `{name}`: bad color"));
                    SourceKind::Solid([1.0, 0.0, 1.0, 1.0])
                }
            }
        } else {
            SourceKind::Video { slot: name.to_string() }
        };
        let a = |b: &mut Builder, k: &str| b.state.intern(&format!("source.{name}.{k}"));
        let color = ColorAddrs {
            brightness: a(self, "color.brightness"),
            contrast: a(self, "color.contrast"),
            saturation: a(self, "color.saturation"),
            gamma: a(self, "color.gamma"),
            temperature: a(self, "color.temperature"),
            tint: a(self, "color.tint"),
            lut: a(self, "lut"),
            lut_amount: a(self, "lut_amount"),
            matrix: a(self, "matrix"),
            range: a(self, "range"),
        };
        let fx = match self.cfg.other.get("sources").and_then(|m| m.get(name)).and_then(|t| t.get("fx")) {
            Some(v) => match toml::Value::try_into::<Vec<FxRef>>(v.clone()) {
                Ok(list) => self.attaches(&list, None, &format!("sources/{name}.toml")),
                Err(e) => {
                    self.errors.push(format!("sources/{name}.toml fx: {}", e.message()));
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        let in_atlas = matches!(kind, SourceKind::Video { .. } | SourceKind::Patch { .. } | SourceKind::Draw { .. });
        self.sources.push(SourcePlan { name: name.to_string(), kind, fx, sizes: [[0, 0]; LAYOUTS], color, in_atlas, overlay: false });
        let i = (self.sources.len() - 1) as u32;
        self.source_index.insert(name.to_string(), i);
        i
    }
}

fn node_plan(b: &mut Builder, scene: &str, layout: &str, n: &NodeDef) -> NodePlan {
    let p = format!("scene.{scene}.node.{}", n.id);
    let ctx = format!("scenes/{scene}.toml node `{}`", n.id);
    let source = b.source(&n.src);
    let blend = Blend::parse(&n.blend).unwrap_or_else(|| {
        b.errors.push(format!("{ctx}: unknown blend `{}`", n.blend));
        Blend::Normal
    });
    let enter = n.enter.as_deref().and_then(|s| b.style(s, &format!("{ctx} enter")));
    let exit = n.exit.as_deref().and_then(|s| b.style(s, &format!("{ctx} exit")));
    let fx = b.attaches(&n.fx, Some(&p), &ctx);
    NodePlan {
        id: n.id.clone(),
        source,
        rect: b.state.intern(&format!("{p}.rect.{layout}")),
        crop: b.state.intern(&format!("{p}.crop.{layout}")),
        radius: b.state.intern(&format!("{p}.radius.{layout}")),
        z: b.state.intern(&format!("{p}.z.{layout}")),
        opacity: b.state.intern(&format!("{p}.opacity")),
        offset_x: b.state.intern(&format!("{p}.offset_x")),
        offset_y: b.state.intern(&format!("{p}.offset_y")),
        scale: b.state.intern(&format!("{p}.scale")),
        rotation: b.state.intern(&format!("{p}.rotation")),
        visible: b.state.intern(&format!("{p}.visible")),
        def: NodeDefaults {
            rect: n.rect,
            crop: n.crop,
            radius: n.radius,
            z: n.z as i64,
            opacity: n.opacity,
            offset: [n.offset_x, n.offset_y],
            scale: n.scale,
            rotation: n.rotation,
            visible: n.visible,
        },
        blend,
        mask: n.mask.clone(),
        when: b.when(&n.when, &ctx),
        fx,
        enter,
        exit,
    }
}

fn transition_plan(b: &mut Builder, t: &TransitionDef) -> TransitionPlan {
    let errors = &mut b.errors;
    let kind = match t.kind.as_str() {
        "cut" => TrKind::Cut,
        "morph" => TrKind::Morph,
        "shader" => TrKind::Shader,
        "combined" => TrKind::Combined,
        k => {
            errors.push(format!("transitions/{}.toml: unknown kind `{k}`", t.name));
            TrKind::Shader
        }
    };
    let shader = match (&t.shader, kind) {
        (Some(s), _) if s.starts_with("patch.") => Some(ShaderSource::Patch(s["patch.".len()..].to_string())),
        (Some(s), _) if s.ends_with(".wgsl") => Some(ShaderSource::File(PathBuf::from(s))),
        (Some(s), _) => match se_core::transitions::shader(s) {
            Some(sh) => Some(ShaderSource::Builtin(sh.name)),
            None => {
                let known: Vec<&str> = se_core::transitions::SHADERS.iter().map(|s| s.name).collect();
                errors.push(format!(
                    "transitions/{}.toml: unknown shader `{s}` (built in: {}; or patch.<id>, or a .wgsl file); using a crossfade",
                    t.name,
                    known.join(", ")
                ));
                Some(ShaderSource::Builtin("fade"))
            }
        },
        (None, TrKind::Shader | TrKind::Combined) => {
            if t.name != "fade" {
                errors.push(format!("transitions/{}.toml: kind `{}` needs `shader`; using a crossfade", t.name, t.kind));
            }
            Some(ShaderSource::Builtin("fade"))
        }
        (None, _) => None,
    };
    // a built-in shader's settings are floats; missing ones take the shader's defaults
    let builtin = match &shader {
        Some(ShaderSource::Builtin(n)) => se_core::transitions::shader(n).map(|s| s.params).unwrap_or(&[]),
        _ => &[],
    };
    let mut params = Vec::new();
    for (k, v) in &t.params {
        let v = match v {
            Value::Int(i) if builtin.iter().any(|p| p.name == k.as_str()) => &Value::Float(*i as f64),
            v => v,
        };
        let ty = match v {
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::List(l) if l.len() == 2 => "vec2",
            Value::List(l) if l.len() == 3 || l.len() == 4 => "color",
            Value::Str(s) if se_proto::value::parse_hex_color(s).is_some() => "color",
            _ => {
                errors.push(format!("transitions/{}.toml: param `{k}` must be a number, bool, vec or color", t.name));
                continue;
            }
        };
        let default = match v {
            Value::Str(s) => se_proto::value::parse_hex_color(s).map(|c| toml::Value::Array(c.iter().map(|x| toml::Value::Float(*x as f64)).collect())),
            v => se_proto::value::to_toml(v),
        };
        let valid = se_proto::address::is_valid(k, false) && !k.contains('.');
        if !valid {
            errors.push(format!("transitions/{}.toml: bad param name `{k}`", t.name));
            continue;
        }
        params.push((k.clone(), se_patch::ParamSpec { ty: ty.into(), default, range: None, options: Vec::new(), unit: None, description: None }));
    }
    for p in builtin.iter().filter(|p| !t.params.contains_key(p.name)) {
        let default = Some(toml::Value::Float(p.default));
        params.push((p.name.to_string(), se_patch::ParamSpec { ty: "float".into(), default, range: None, options: Vec::new(), unit: None, description: None }));
    }
    let ctx = format!("transitions/{}.toml", t.name);
    let enter = b.style(&t.enter, &format!("{ctx} enter")).unwrap_or(Style::Fade);
    let exit = b.style(&t.exit, &format!("{ctx} exit")).unwrap_or(Style::Fade);
    TransitionPlan { name: t.name.clone(), kind, shader, ease: t.ease, enter, exit, params }
}

/// Built-in transitions when the project has no file for them (§4.4; core `BUILTIN_TRANSITIONS`).
fn builtin_transition(name: &str) -> Option<TransitionPlan> {
    let (kind, shader) = match name {
        "cut" => (TrKind::Cut, None),
        "morph" => (TrKind::Morph, None),
        "fade" => (TrKind::Shader, Some(ShaderSource::Builtin("fade"))),
        _ => return None,
    };
    Some(TransitionPlan { name: name.into(), kind, shader, ease: Ease::InOutCubic, enter: Style::Fade, exit: Style::Fade, params: Vec::new() })
}

fn overlay_settings(cfg: &Config, id: &str) -> Option<toml::Table> {
    match cfg.project.extra.get("overlays") {
        Some(toml::Value::Table(t)) => t.get(id).and_then(|v| v.as_table()).cloned(),
        _ => None,
    }
}

impl Plan {
    /// Build from the applied configuration and the scanned patch manifests.
    pub fn build(cfg: &Config, manifests: &[Arc<Manifest>], project_root: PathBuf) -> Plan {
        let mut b = Builder {
            cfg,
            manifests: manifests.iter().map(|m| (m.id.clone(), m.clone())).collect(),
            state: AddrTable::default(),
            signals: AddrTable::default(),
            whens: Vec::new(),
            when_src: HashMap::new(),
            groups: Vec::new(),
            effects: Vec::new(),
            effect_index: HashMap::new(),
            sources: Vec::new(),
            source_index: HashMap::new(),
            styles: Vec::new(),
            errors: Vec::new(),
        };
        let settings = Settings::from_config(cfg, &mut b.errors);

        // effect library (global addresses)
        for (i, e) in LIBRARY.iter().enumerate() {
            let base = format!("fx.{}", e.name);
            let params: Vec<AddrId> = e.all_params().map(|p| b.state.intern(&format!("{base}.{}", p.name))).collect();
            let defaults = e.all_params().map(|p| p.default).collect();
            let file = matches!(e.exec, effects::Exec::Lut).then(|| b.state.intern(&format!("{base}.file")));
            b.effects.push(EffectPlan {
                name: e.name.into(),
                kind: EffectKind::Builtin(i),
                point: e.point,
                flashy: e.flashy,
                params,
                defaults,
                env: b.state.intern(&format!("{base}.env")),
                has_trigger: true,
                file,
                identity: e.identity,
            });
            b.effect_index.insert(e.name.into(), i);
        }

        let show = ShowAddrs {
            program: b.state.intern("show.scene.program"),
            preview: b.state.intern("show.scene.preview"),
            tr_name: b.state.intern("show.transition.name"),
            tr_ms: b.state.intern("show.transition.ms"),
            tr_start: b.state.intern("show.transition.start"),
            tr_active: b.state.intern("show.transition.active"),
            tr_from: b.state.intern("show.transition.from"),
            beat_phase: b.signals.intern("beat.phase"),
            bass: b.signals.intern("band.bass"),
            level: b.signals.intern("band.level"),
        };
        let palette = PALETTE_SLOTS.map(|s| b.state.intern(&format!("palette.{s}")));
        let std_signals = se_patch::wgsl::STANDARD_SIGNALS.iter().map(|s| b.signals.intern(s)).collect();

        // canvases
        let canvas_def = |n: &str| cfg.project.canvas.get(n).cloned();
        let wide = canvas_def("wide").unwrap_or(se_core::config::CanvasDef { width: 1920, height: 1080, fps: 60 });
        let tall = canvas_def("tall").unwrap_or(se_core::config::CanvasDef { width: 1080, height: 1920, fps: 60 });
        for (n, _) in cfg.project.canvas.iter().filter(|(n, _)| !["wide", "tall", "preview", "atlas"].contains(&n.as_str())) {
            b.errors.push(format!("[canvas.{n}]: only wide and tall canvases are exported (frames.sock)"));
        }
        let even = |v: u32| (v.clamp(16, 7680) / 2) * 2;
        let canvas_fx = |b: &mut Builder, name: &str, key: &str| -> Vec<Attach> {
            match cfg.project.extra.get("render").and_then(|r| r.get(key)).and_then(|t| t.get(name)) {
                Some(v) => match toml::Value::try_into::<Vec<FxRef>>(v.clone()) {
                    Ok(l) => b.attaches(&l, None, &format!("[render.{key}.{name}]")),
                    Err(e) => {
                        b.errors.push(format!("[render.{key}.{name}]: {}", e.message()));
                        Vec::new()
                    }
                },
                None => Vec::new(),
            }
        };
        let mk = |b: &mut Builder, name: &'static str, w: u32, h: u32, fps: u32, layout: usize| CanvasPlan {
            name,
            width: even(w),
            height: even(h),
            fps: fps.clamp(1, 240),
            layout,
            fx: canvas_fx(b, name, "canvas_fx"),
            output_fx: canvas_fx(b, name, "output_fx"),
        };
        let pl = settings.preview_layout;
        let (pw, ph) = if pl == WIDE { (wide.width, wide.height) } else { (tall.width, tall.height) };
        let canvases = [
            mk(&mut b, "wide", wide.width, wide.height, wide.fps, WIDE),
            mk(&mut b, "tall", tall.width, tall.height, tall.fps, TALL),
            mk(&mut b, "preview", (pw as f32 * settings.preview_scale) as u32, (ph as f32 * settings.preview_scale) as u32, wide.fps, pl),
            mk(&mut b, "atlas", settings.atlas_size[0], settings.atlas_size[1], settings.atlas_fps, WIDE),
        ];

        // scenes
        let mut scenes = Vec::new();
        let mut scene_index = HashMap::new();
        for s in cfg.scenes.values() {
            let mut layouts: [LayoutPlan; LAYOUTS] = Default::default();
            for (li, lname) in LAYOUT_NAMES.iter().enumerate() {
                let Some(c) = s.canvas.get(*lname) else { continue };
                let nodes: Vec<NodePlan> = c.nodes.iter().map(|n| node_plan(&mut b, &s.name, lname, n)).collect();
                let ctx = format!("scenes/{}.toml", s.name);
                let mut fx = b.attaches(&s.fx, None, &ctx);
                fx.extend(b.attaches(&c.fx, None, &ctx));
                let background = c.background.as_ref().and_then(|v| {
                    v.as_color().or_else(|| {
                        b.errors.push(format!("{ctx}: [canvas.{lname}] background is not a color"));
                        None
                    })
                });
                // generated-source render sizes: largest placement per layout
                let cw = if li == WIDE { canvases[WIDE].width } else { canvases[TALL].width } as f32;
                let ch = if li == WIDE { canvases[WIDE].height } else { canvases[TALL].height } as f32;
                for n in &nodes {
                    let sz = &mut b.sources[n.source as usize].sizes[li];
                    let w = (n.def.rect[2] * cw * n.def.scale.max(1.0)).ceil().clamp(16.0, 3840.0) as u32;
                    let h = (n.def.rect[3] * ch * n.def.scale.max(1.0)).ceil().clamp(16.0, 3840.0) as u32;
                    sz[0] = sz[0].max(w);
                    sz[1] = sz[1].max(h);
                }
                layouts[li] = LayoutPlan { nodes, fx, background };
            }
            scene_index.insert(s.name.clone(), scenes.len());
            scenes.push(ScenePlan { name: s.name.clone(), layouts });
        }

        // transitions
        let mut transitions = Vec::new();
        let mut transition_index = HashMap::new();
        for t in cfg.transitions.values() {
            transition_index.insert(t.name.clone(), transitions.len());
            transitions.push(transition_plan(&mut b, t));
        }
        for n in se_core::config::BUILTIN_TRANSITIONS {
            if !transition_index.contains_key(*n)
                && let Some(t) = builtin_transition(n)
            {
                transition_index.insert(n.to_string(), transitions.len());
                transitions.push(t);
            }
        }

        // patches rendered by us + overlays
        let mut patches = Vec::new();
        let mut patch_index = HashMap::new();
        let mut overlays = Vec::new();
        for m in manifests {
            if matches!(m.kind, Kind::Shader | Kind::Particles) {
                let layout = se_patch::wgsl::Layout::new(m);
                let base = m.address();
                let mut params = Vec::new();
                let mut defaults = Vec::new();
                for (n, p, _) in m.param_slots() {
                    params.push(b.state.intern(&format!("{base}.{n}")));
                    defaults.push(param_default4(p));
                }
                let signals = layout.signals.iter().map(|s| b.signals.intern(s)).collect();
                patch_index.insert(m.id.clone(), patches.len());
                patches.push(PatchPlan { manifest: m.clone(), layout, params, defaults, signals, env: b.state.intern(&format!("{base}.env")) });
            }
            if m.kind == Kind::Shader && m.layer == Layer::Transition {
                let name = m.address();
                if !transition_index.contains_key(&name) {
                    transition_index.insert(name.clone(), transitions.len());
                    transitions.push(TransitionPlan {
                        name,
                        kind: TrKind::Shader,
                        shader: Some(ShaderSource::Patch(m.id.clone())),
                        ease: Ease::InOutCubic,
                        enter: Style::Fade,
                        exit: Style::Fade,
                        params: Vec::new(),
                    });
                }
            }
            if m.layer == Layer::Overlay && matches!(m.kind, Kind::Shader | Kind::Particles | Kind::Script | Kind::Web) {
                let name = m.address();
                let source = b.source(&name);
                b.sources[source as usize].overlay = true;
                b.sources[source as usize].in_atlas = false;
                let t = overlay_settings(cfg, &m.id).unwrap_or_default();
                let ctx = format!("[overlays.{}]", m.id);
                let mut canvases_on = [true, true, true, false];
                if let Some(list) = t.get("canvases").and_then(|v| v.as_array()) {
                    canvases_on = [false; CANVASES];
                    for c in list.iter().filter_map(|v| v.as_str()) {
                        match CANVAS_NAMES.iter().position(|n| *n == c) {
                            Some(i) if i != ATLAS => canvases_on[i] = true,
                            _ => b.errors.push(format!("{ctx}: unknown canvas `{c}`")),
                        }
                    }
                    canvases_on[PREVIEW] = canvases_on[pl];
                }
                let mut rect = [None; CANVASES];
                if let Some(r) = t.get("rect").and_then(|v| v.as_table()) {
                    for (c, v) in r {
                        match (CANVAS_NAMES.iter().position(|n| n == c), color_of(v)) {
                            (Some(i), Some(xywh)) if i < ATLAS => rect[i] = Some(xywh),
                            _ => b.errors.push(format!("{ctx}: bad rect.{c}")),
                        }
                    }
                    if rect[PREVIEW].is_none() {
                        rect[PREVIEW] = rect[pl];
                    }
                }
                let when_src = t.get("when").and_then(|v| v.as_str()).map(String::from);
                let when = b.when(&when_src, &ctx);
                overlays.push(OverlayPlan {
                    id: m.id.clone(),
                    source,
                    canvases: canvases_on,
                    rect,
                    when,
                    z: t.get("z").and_then(|v| v.as_integer()).unwrap_or(0) as i32,
                    fx: t.get("fx").and_then(|v| v.as_bool()).unwrap_or(false),
                    env: m.has_trigger.then(|| b.state.intern(&format!("{name}.env"))),
                });
            }
        }
        overlays.sort_by_key(|o| o.z);
        if let Some(toml::Value::Table(t)) = cfg.project.extra.get("overlays") {
            for k in t.keys() {
                if !overlays.iter().any(|o| &o.id == k) {
                    b.errors.push(format!("[overlays.{k}]: no overlay-layer patch `{k}`"));
                }
            }
        }
        // overlay sources render at canvas size
        for o in &overlays {
            let s = &mut b.sources[o.source as usize];
            s.sizes = [[canvases[WIDE].width, canvases[WIDE].height], [canvases[TALL].width, canvases[TALL].height]];
        }
        // generated sources that were never placed: render at canvas size (atlas, previews)
        for s in &mut b.sources {
            if matches!(s.kind, SourceKind::Patch { .. } | SourceKind::Draw { .. }) {
                if let Some(m) = s.kind_patch_id().and_then(|id| b.manifests.get(id)).and_then(|m| m.size) {
                    s.sizes = [m, m];
                }
                for (li, sz) in s.sizes.iter_mut().enumerate() {
                    if sz[0] == 0 {
                        let c = &canvases[li];
                        *sz = [c.width, c.height];
                    }
                }
            }
        }

        let errors = std::mem::take(&mut b.errors);
        let styles = std::mem::take(&mut b.styles);
        Plan {
            settings,
            canvases,
            sources: b.sources,
            source_index: b.source_index,
            effects: b.effects,
            effect_index: b.effect_index,
            scenes,
            scene_index,
            transitions,
            transition_index,
            overlays,
            patches,
            patch_index,
            whens: b.whens,
            groups: b.groups,
            palette,
            std_signals,
            show,
            state: b.state,
            signals: b.signals,
            project_root,
            styles,
            errors,
        }
    }

    pub fn scene(&self, name: &str) -> Option<&ScenePlan> {
        self.scene_index.get(name).map(|i| &self.scenes[*i])
    }

    /// Transition by name; unknown names fall back to a crossfade.
    pub fn transition(&self, name: &str) -> &TransitionPlan {
        let i = self.transition_index.get(name).or_else(|| self.transition_index.get("fade")).copied().unwrap_or(0);
        &self.transitions[i]
    }

    /// Canvas-point / output-point implicit global instances, in library order.
    pub fn global_attaches(&self, point: Point) -> Vec<Attach> {
        self.effects
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e.point == point && matches!(e.kind, EffectKind::Builtin(_))
                    || (point == Point::Canvas && matches!(e.kind, EffectKind::Patch(_)) && e.has_trigger)
            })
            .map(|(i, e)| {
                let mut params = [ParamSrc::Const(0.0); MAX_PARAMS];
                for (j, a) in e.params.iter().enumerate().take(MAX_PARAMS) {
                    params[j] = ParamSrc::Global(*a);
                }
                Attach {
                    effect: i,
                    when: None,
                    group: None,
                    enabled: None,
                    triggered_only: matches!(e.kind, EffectKind::Patch(_)),
                    global: matches!(e.kind, EffectKind::Builtin(_)),
                    params,
                    nparams: e.params.len().min(MAX_PARAMS),
                    file: e.file.map(StrSrc::Addr),
                }
            })
            .collect()
    }

    /// Video sources referenced by any scene, in first-seen order (the multiview set).
    pub fn atlas_sources(&self) -> Vec<u32> {
        (0..self.sources.len() as u32).filter(|i| self.sources[*i as usize].in_atlas).collect()
    }
}

impl SourcePlan {
    pub fn kind_patch_id(&self) -> Option<&str> {
        match &self.kind {
            SourceKind::Patch { id } => Some(id),
            SourceKind::Draw { slot } => slot.strip_prefix("patch."),
            _ => None,
        }
    }
}

/// Multiview tiles (normalized x, y, w, h in the atlas) for `n` sources: a near-square grid;
/// each tile is the largest 16:9 box centered in its cell.
pub fn atlas_tiles(n: usize, size: [u32; 2]) -> Vec<[f32; 4]> {
    if n == 0 {
        return Vec::new();
    }
    let cols = (n as f32).sqrt().ceil() as usize;
    let rows = n.div_ceil(cols);
    let (cw, ch) = (size[0] as f32 / cols as f32, size[1] as f32 / rows as f32);
    let gap = 4.0;
    let (bw, bh) = ((cw - gap).max(1.0), (ch - gap).max(1.0));
    let (tw, th) = if bw / bh > 16.0 / 9.0 { (bh * 16.0 / 9.0, bh) } else { (bw, bw * 9.0 / 16.0) };
    (0..n)
        .map(|i| {
            let (c, r) = ((i % cols) as f32, (i / cols) as f32);
            let x = c * cw + (cw - tw) * 0.5;
            let y = r * ch + (ch - th) * 0.5;
            [x / size[0] as f32, y / size[1] as f32, tw / size[0] as f32, th / size[1] as f32]
        })
        .collect()
}

/// A param's default as 4 floats (colors/vectors fill all components).
pub fn param_default4(p: &se_patch::ParamSpec) -> [f32; 4] {
    value4(&p.default_value())
}

pub fn value4(v: &Value) -> [f32; 4] {
    match v {
        Value::List(l) => {
            let mut o = [0.0, 0.0, 0.0, 1.0];
            for (i, x) in l.iter().take(4).enumerate() {
                o[i] = x.as_f32().unwrap_or(0.0);
            }
            o
        }
        Value::Str(s) => se_proto::value::parse_hex_color(s).unwrap_or([0.0; 4]),
        v => [v.as_f32().unwrap_or(0.0), 0.0, 0.0, 0.0],
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use se_core::config::SourceFile;

    pub fn config(files: &[(&str, &str, &str)]) -> Config {
        let files: Vec<SourceFile> = files
            .iter()
            .map(|(k, n, src)| SourceFile { kind: k.to_string(), name: n.to_string(), path: format!("{k}/{n}.toml"), table: toml::from_str(src).unwrap() })
            .collect();
        Config::build(&files)
    }

    pub fn example_config() -> Config {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../project-example");
        let mut files = Vec::new();
        files.push(("project".to_string(), "project".to_string(), std::fs::read_to_string(root.join("project.toml")).unwrap()));
        for kind in ["scenes", "transitions", "presets"] {
            for e in std::fs::read_dir(root.join(kind)).unwrap().flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "toml") {
                    files.push((kind.to_string(), p.file_stem().unwrap().to_string_lossy().to_string(), std::fs::read_to_string(&p).unwrap()));
                }
            }
        }
        let files: Vec<SourceFile> = files
            .iter()
            .map(|(k, n, s)| SourceFile { kind: k.clone(), name: n.clone(), path: format!("{k}/{n}.toml"), table: toml::from_str(s).unwrap() })
            .collect();
        Config::build(&files)
    }

    #[test]
    fn example_project_plans_cleanly() {
        let c = example_config();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../project-example");
        let manifests: Vec<Arc<Manifest>> = se_patch::scan(&root).into_iter().filter_map(Result::ok).map(Arc::new).collect();
        let p = Plan::build(&c, &manifests, root);
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        let duo = p.scene("duo").unwrap();
        assert_eq!(duo.layouts[TALL].nodes.len(), 2);
        let wide_cam = &duo.layouts[WIDE].nodes[1];
        assert_eq!(p.state.name(wide_cam.rect), "scene.duo.node.cam_wide.rect.wide");
        assert_eq!(p.state.name(wide_cam.opacity), "scene.duo.node.cam_wide.opacity");
        assert_eq!(wide_cam.fx.len(), 1);
        assert!(wide_cam.fx[0].when.is_some());
        assert_eq!(p.effects[wide_cam.fx[0].effect].name, "vhs");
        // node-attached effect params use the core's node-local addresses when configured
        assert_eq!(wide_cam.fx[0].params[0], ParamSrc::Const(1.0));
        assert_eq!(p.state.name(wide_cam.fx[0].enabled.unwrap()), "scene.duo.node.cam_wide.fx.vhs.enabled");
        // one source entry per name, shared by every placement
        assert_eq!(p.sources.iter().filter(|s| s.name == "cam_kit").count(), 1);
        assert_eq!(p.canvases[WIDE].width, 1920);
        assert_eq!(p.canvases[TALL].height, 1920);
        assert_eq!(p.canvases[PREVIEW].width, 960);
        assert!(p.transition_index.contains_key("morph"));
        assert!(p.transition_index.contains_key("cut"));
        assert_eq!(p.transition("fade").kind, TrKind::Shader);
        assert_eq!(p.transition("zoomblur").params.len(), 1);
        assert_eq!(p.transition("no-such").name, "fade");
        let atlas: Vec<&str> = p.atlas_sources().iter().map(|i| p.sources[*i as usize].name.as_str()).collect();
        assert!(atlas.contains(&"cam_kit") && atlas.contains(&"youtube") && atlas.contains(&"patch.aurora"));
    }

    #[test]
    fn atlas_grid() {
        assert!(atlas_tiles(0, [1920, 1080]).is_empty());
        let one = atlas_tiles(1, [1920, 1080]);
        assert!((one[0][2] - (1916.0 / 1920.0)).abs() < 1e-3 || (one[0][3] - (1076.0 / 1080.0)).abs() < 1e-3);
        let six = atlas_tiles(6, [1920, 1080]);
        assert_eq!(six.len(), 6);
        for t in &six {
            assert!(t[0] >= 0.0 && t[1] >= 0.0 && t[0] + t[2] <= 1.0 + 1e-6 && t[1] + t[3] <= 1.0 + 1e-6);
            let aspect = t[2] * 1920.0 / (t[3] * 1080.0);
            assert!((aspect - 16.0 / 9.0).abs() < 1e-3);
        }
        assert!(six[1][0] > six[0][0] && six[3][1] > six[0][1], "row-major");
    }

    #[test]
    fn errors_are_reported_not_fatal() {
        let c = config(&[
            ("project", "project", "schema = 1\n[render]\nbuffers = 9\nbogus = 1\n[overlays.nope]\nz = 1"),
            (
                "scenes",
                "a",
                "[canvas.wide]\nnodes = [{ src = \"cam\", blend = \"weird\", when = \"mode ==\", fx = [{ name = \"nope\" }, { name = \"grade\", warmthh = 1 }] }]",
            ),
        ]);
        let p = Plan::build(&c, &[], PathBuf::from("/tmp"));
        let all = p.errors.join("\n");
        for needle in ["buffers", "bogus", "weird", "when", "unknown effect `nope`", "no param `warmthh`", "[overlays.nope]"] {
            assert!(all.contains(needle), "missing `{needle}` in:\n{all}");
        }
        assert_eq!(p.settings.buffers, 4);
        let n = &p.scene("a").unwrap().layouts[WIDE].nodes[0];
        assert_eq!(n.blend, Blend::Normal);
        // broken `when` fails closed (hidden)
        assert_eq!(p.whens[n.when.unwrap() as usize].expr.source(), "false");
    }

    #[test]
    fn custom_enter_exit_styles_resolve_to_shared_shaders() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |id: &str, toml: &str| {
            let d = dir.path().join("patches").join(id);
            std::fs::create_dir_all(&d).unwrap();
            Arc::new(Manifest::parse(&d, toml).unwrap())
        };
        let ms = vec![mk("dissolve", "kind = \"shader\"\nlayer = \"effect\""), mk("meteors", "kind = \"script\"\nlayer = \"overlay\"")];
        let c = config(&[
            ("project", "project", "schema = 1"),
            ("transitions", "wipe", "kind = \"morph\"\nenter = \"transitions/wipe.wgsl\"\nexit = \"patch.dissolve\""),
            ("transitions", "bad", "kind = \"morph\"\nenter = \"patch.meteors\"\nexit = \"wobble\""),
            (
                "scenes",
                "s",
                "[canvas.wide]\nnodes = [{ src = \"cam\", enter = \"patch.dissolve\", exit = \"transitions/wipe.wgsl\" }, { src = \"cam2\", enter = \"patch.nope\" }]",
            ),
        ]);
        let p = Plan::build(&c, &ms, dir.path().to_path_buf());
        // one shader per distinct source, shared by nodes and transitions
        assert_eq!(p.styles.len(), 2, "{:?}", p.styles);
        let custom = |src: ShaderSource| Style::Custom(p.styles.iter().position(|s| *s == src).unwrap() as u32);
        let (file, patch) = (custom(ShaderSource::File("transitions/wipe.wgsl".into())), custom(ShaderSource::Patch("dissolve".into())));
        let wipe = p.transition("wipe");
        assert_eq!((wipe.enter, wipe.exit), (file, patch));
        let nodes = &p.scene("s").unwrap().layouts[WIDE].nodes;
        assert_eq!((nodes[0].enter, nodes[0].exit), (Some(patch), Some(file)));
        // non-shader patch, unknown patch, unknown name: reported; transitions fall back to fade,
        // nodes to the transition's style
        let bad = p.transition("bad");
        assert_eq!((bad.enter, bad.exit), (Style::Fade, Style::Fade));
        assert_eq!(nodes[1].enter, None);
        let all = p.errors.join("\n");
        for needle in ["`patch.meteors` must be a shader patch", "unknown style `wobble`", "`patch.nope`: no such patch"] {
            assert!(all.contains(needle), "missing `{needle}` in:\n{all}");
        }
    }

    #[test]
    fn sources_patches_overlays_and_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |id: &str, toml: &str| {
            let d = dir.path().join("patches").join(id);
            std::fs::create_dir_all(&d).unwrap();
            Arc::new(Manifest::parse(&d, toml).unwrap())
        };
        let ms = vec![
            mk("aurora", "kind = \"shader\"\nparams.speed = { default = 1.0 }"),
            mk("alerts", "kind = \"web\"\nlayer = \"overlay\""),
            mk("meteors", "kind = \"script\"\nlayer = \"overlay\"\ntrigger = { hold = \"2s\" }"),
            mk("warp", "kind = \"shader\"\nlayer = \"effect\"\ntrigger = { hold = \"1s\" }\nparams.depth = { default = 0.5 }"),
        ];
        let c = config(&[
            (
                "project",
                "project",
                "schema = 1\n[render]\npreview = { scale = 0.25 }\nno_signal = \"transparent\"\nexport_modifier = \"0x0300000000606014\"\n[render.output_fx]\nwide = [{ name = \"vignette\", amount = 0.3 }]\n[overlays.alerts]\ncanvases = [\"wide\"]\nrect.wide = [0.5, 0.0, 0.5, 0.5]\nz = 5\n[palette]\naccent = \"#ff0000\"",
            ),
            (
                "scenes",
                "s",
                "[canvas.wide]\nnodes = [{ src = \"patch.aurora\", rect = [0, 0, 0.5, 0.5] }, { src = \"color:#00ff00\" }, { src = \"cam\", fx = [{ name = \"patch.warp\" }] }]",
            ),
        ]);
        let p = Plan::build(&c, &ms, dir.path().to_path_buf());
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert_eq!(p.settings.preview_scale, 0.25);
        assert_eq!(p.settings.no_signal, [0.0; 4]);
        assert_eq!(p.settings.export_modifier, ModifierPref::Explicit(0x0300000000606014));
        assert_eq!(p.settings.palette[0], [1.0, 0.0, 0.0, 1.0]);
        let src = |n: &str| &p.sources[p.source_index[n] as usize];
        assert_eq!(src("patch.aurora").kind, SourceKind::Patch { id: "aurora".into() });
        assert_eq!(src("patch.aurora").sizes[WIDE], [960, 540]);
        assert_eq!(src("color:#00ff00").kind, SourceKind::Solid([0.0, 1.0, 0.0, 1.0]));
        assert_eq!(src("patch.alerts").kind, SourceKind::Video { slot: "patch.alerts".into() });
        assert_eq!(src("patch.meteors").kind, SourceKind::Draw { slot: "patch.meteors".into() });
        assert!(src("patch.alerts").overlay && !src("patch.alerts").in_atlas);
        assert_eq!(p.overlays.len(), 2);
        let alerts = p.overlays.iter().find(|o| o.id == "alerts").unwrap();
        assert_eq!(alerts.canvases, [true, false, true, false]);
        assert_eq!(alerts.rect[WIDE], Some([0.5, 0.0, 0.5, 0.5]));
        assert_eq!(alerts.rect[PREVIEW], Some([0.5, 0.0, 0.5, 0.5]));
        assert!(alerts.env.is_none());
        assert!(p.overlays.iter().find(|o| o.id == "meteors").unwrap().env.is_some());
        assert_eq!(p.overlays.last().unwrap().id, "alerts", "sorted by z");
        assert_eq!(p.canvases[WIDE].output_fx.len(), 1);
        assert!(p.patch_index.contains_key("aurora") && p.patch_index.contains_key("warp"));
        let warp = p.effect_index["patch.warp"];
        assert_eq!(p.effects[warp].kind, EffectKind::Patch("warp".into()));
        let g = p.global_attaches(Point::Canvas);
        assert!(g.iter().any(|a| a.effect == warp && a.triggered_only));
        assert!(p.global_attaches(Point::Output).iter().all(|a| p.effects[a.effect].name == "fade_to_black"));
    }
}
