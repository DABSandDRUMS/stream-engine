//! `patch.toml` manifests (§6.2).
//!
//! ```toml
//! kind    = "script"            # shader | particles | script | web | dsp
//! entry   = "main.lua"
//! layer   = "overlay"           # source | overlay | effect | transition | audio-effect | audio-source
//! params.count = { type = "int",   default = 40,  range = [1, 400] }
//! params.speed = { type = "float", default = 1.0, range = [0.1, 4.0] }
//! trigger = { attack = "100ms", hold = "4s", release = "1s", retrigger = "stack" }
//! budget  = { cpu_ms = 1.0 }
//! signals = ["band.bass", "beat.phase"]   # extra signals exposed to shaders
//! ```

use se_core::triggers::TriggerSpec;
use se_proto::{Meta, Value, ValueType};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Shader,
    Particles,
    Script,
    Web,
    Dsp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Layer {
    #[default]
    Source,
    Overlay,
    Effect,
    Transition,
    AudioEffect,
    AudioSource,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Shader => "shader",
            Kind::Particles => "particles",
            Kind::Script => "script",
            Kind::Web => "web",
            Kind::Dsp => "dsp",
        }
    }
}

impl Layer {
    pub fn as_str(self) -> &'static str {
        match self {
            Layer::Source => "source",
            Layer::Overlay => "overlay",
            Layer::Effect => "effect",
            Layer::Transition => "transition",
            Layer::AudioEffect => "audio-effect",
            Layer::AudioSource => "audio-source",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ParamSpec {
    #[serde(rename = "type", default = "float_ty")]
    pub ty: String,
    #[serde(default)]
    pub default: Option<toml::Value>,
    #[serde(default)]
    pub range: Option<[f64; 2]>,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

fn float_ty() -> String {
    "float".into()
}

impl ParamSpec {
    pub fn value_type(&self) -> ValueType {
        match self.ty.as_str() {
            "int" => ValueType::Int,
            "bool" => ValueType::Bool,
            "color" => ValueType::Color,
            "vec2" => ValueType::Vec2,
            "vec4" => ValueType::Vec4,
            "enum" => ValueType::Enum,
            "string" => ValueType::String,
            "texture" => ValueType::TextureRef,
            _ => ValueType::Float,
        }
    }

    pub fn default_value(&self) -> Value {
        match &self.default {
            Some(v) => Value::from(v.clone()),
            None => match self.value_type() {
                ValueType::Int => Value::Int(0),
                ValueType::Bool => Value::Bool(false),
                ValueType::Color => Value::from([1.0f32, 1.0, 1.0, 1.0]),
                ValueType::Vec2 => Value::from([0.0f32, 0.0]),
                ValueType::Vec4 => Value::from([0.0f32; 4]),
                ValueType::Enum => Value::Str(self.options.first().cloned().unwrap_or_default()),
                ValueType::String | ValueType::TextureRef => Value::Str(String::new()),
                _ => Value::Float(0.0),
            },
        }
    }

    pub fn meta(&self, patch: &str) -> Meta {
        Meta {
            ty: self.value_type(),
            range: self.range,
            default: self.default_value(),
            unit: self.unit.clone(),
            description: self.description.clone(),
            options: self.options.clone(),
            readonly: false,
            merge: Default::default(),
            owner: Some(format!("patch.{patch}")),
        }
    }

    /// Number of f32 slots in the shader uniform block.
    pub fn slots(&self) -> usize {
        match self.value_type() {
            ValueType::Color | ValueType::Vec4 => 4,
            ValueType::Vec2 => 2,
            ValueType::String | ValueType::TextureRef => 0,
            _ => 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Default)]
#[serde(default)]
pub struct Budget {
    /// Per-tick CPU budget for scripts (ms).
    pub cpu_ms: Option<f64>,
    /// Per-frame GPU budget for shaders (ms); exceeding it repeatedly disables the patch.
    pub gpu_ms: Option<f64>,
    /// Scripts: VM instructions allowed in one callback.
    pub instructions: Option<u64>,
    /// Scripts: Lua heap cap (MB).
    pub memory_mb: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Default)]
#[serde(default)]
pub struct ParticlesSpec {
    /// Particle count (storage buffer capacity).
    pub count: u32,
    /// Compute shader (simulation), relative to the patch.
    pub sim: String,
    /// Render shader (vertex+fragment), relative to the patch.
    pub draw: String,
}

#[derive(Deserialize)]
struct Raw {
    kind: Kind,
    entry: Option<String>,
    #[serde(default)]
    layer: Layer,
    #[serde(default)]
    params: BTreeMap<String, ParamSpec>,
    #[serde(default)]
    trigger: Option<toml::Value>,
    #[serde(default)]
    budget: Budget,
    #[serde(default)]
    signals: Vec<String>,
    #[serde(default)]
    particles: Option<ParticlesSpec>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    description: Option<String>,
    /// Render size for source/overlay layers (defaults to the canvas size).
    #[serde(default)]
    size: Option<[u32; 2]>,
    /// Frame rate for `web` pages (CEF windowless frame rate, 1–120; default 60).
    #[serde(default)]
    fps: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub dir: PathBuf,
    pub kind: Kind,
    pub entry: String,
    pub layer: Layer,
    pub params: BTreeMap<String, ParamSpec>,
    pub trigger: TriggerSpec,
    pub has_trigger: bool,
    pub budget: Budget,
    pub signals: Vec<String>,
    pub particles: Option<ParticlesSpec>,
    pub label: String,
    pub description: String,
    pub size: Option<[u32; 2]>,
    pub fps: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("{0}: {1}")]
    Parse(PathBuf, String),
}

impl ManifestError {
    /// `patch.toml:<line>: message` (line 0 when unknown), as shown in `patch.<id>.error`.
    pub fn located(&self) -> String {
        let name = |p: &Path| p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "patch.toml".into());
        match self {
            ManifestError::Io(p, e) => format!("{}:0: {e}", name(p)),
            ManifestError::Parse(p, m) => match m.strip_prefix("line ").and_then(|r| r.split_once(": ")) {
                Some((n, rest)) if n.parse::<u32>().is_ok() => format!("{}:{n}: {rest}", name(p)),
                _ => format!("{}:0: {m}", name(p)),
            },
        }
    }
}

fn default_entry(kind: Kind) -> &'static str {
    match kind {
        Kind::Shader => "main.wgsl",
        Kind::Particles => "sim.wgsl",
        Kind::Script => "main.lua",
        Kind::Web => "index.html",
        Kind::Dsp => "main.wasm",
    }
}

impl Manifest {
    pub fn load(dir: &Path) -> Result<Manifest, ManifestError> {
        let path = dir.join("patch.toml");
        let src = std::fs::read_to_string(&path).map_err(|e| ManifestError::Io(path.clone(), e))?;
        Self::parse(dir, &src)
    }

    pub fn parse(dir: &Path, src: &str) -> Result<Manifest, ManifestError> {
        let path = dir.join("patch.toml");
        let raw: Raw = toml::from_str(src).map_err(|e| {
            let msg = e.message().trim().to_string();
            match e.span() {
                Some(s) => ManifestError::Parse(path.clone(), format!("line {}: {msg}", src[..s.start.min(src.len())].matches('\n').count() + 1)),
                None => ManifestError::Parse(path.clone(), msg),
            }
        })?;
        let id = dir.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
        if !se_proto::address::is_valid(&id, false) || id.contains('.') {
            return Err(ManifestError::Parse(path, format!("patch folder name `{id}` must be a single address segment")));
        }
        for name in raw.params.keys() {
            if !se_proto::address::is_valid(name, false) || name.contains('.') || ["env", "active", "error", "trigger"].contains(&name.as_str()) {
                return Err(ManifestError::Parse(path, format!("bad or reserved param name `{name}`")));
            }
        }
        let has_trigger = raw.trigger.is_some();
        let trigger = match &raw.trigger {
            Some(v) => TriggerSpec::from_value(&Value::from(v.clone())).map_err(|e| ManifestError::Parse(path.clone(), e))?,
            None => TriggerSpec::default(),
        };
        if raw.kind == Kind::Particles && raw.particles.is_none() {
            return Err(ManifestError::Parse(path, "particles patches need a [particles] table (count, sim, draw)".into()));
        }
        let entry = raw.entry.unwrap_or_else(|| default_entry(raw.kind).to_string());
        if entry.contains("..") || entry.starts_with('/') {
            return Err(ManifestError::Parse(path, "entry must be inside the patch folder".into()));
        }
        if let Some(f) = raw.fps
            && !(1..=120).contains(&f)
        {
            return Err(ManifestError::Parse(path, format!("fps = {f} is outside 1–120")));
        }
        if let Some([w, h]) = raw.size
            && (w == 0 || h == 0 || w > 8192 || h > 8192)
        {
            return Err(ManifestError::Parse(path, format!("size = [{w}, {h}] must be 1–8192 per side")));
        }
        Ok(Manifest {
            label: raw.label.unwrap_or_else(|| id.clone()),
            description: raw.description.unwrap_or_default(),
            id,
            dir: dir.to_path_buf(),
            kind: raw.kind,
            entry,
            layer: raw.layer,
            params: raw.params,
            trigger,
            has_trigger,
            budget: raw.budget,
            signals: raw.signals,
            particles: raw.particles,
            size: raw.size,
            fps: raw.fps,
        })
    }

    pub fn address(&self) -> String {
        format!("patch.{}", self.id)
    }

    pub fn entry_path(&self) -> PathBuf {
        self.dir.join(&self.entry)
    }

    /// Params in declaration order with their uniform slot offsets (for shaders).
    pub fn param_slots(&self) -> Vec<(&str, &ParamSpec, usize)> {
        let mut off = 0;
        let mut out = Vec::new();
        for (n, p) in &self.params {
            let s = p.slots();
            if s == 0 {
                continue;
            }
            // vec2/vec4 start on their own alignment within vec4 rows
            if s == 4 && off % 4 != 0 {
                off += 4 - off % 4;
            }
            if s == 2 && off % 2 != 0 {
                off += 1;
            }
            out.push((n.as_str(), p, off));
            off += s;
        }
        out
    }
}

/// Scan `project/patches/*/patch.toml`. Errors are returned per patch.
pub fn scan(project_root: &Path) -> Vec<Result<Manifest, ManifestError>> {
    let dir = project_root.join("patches");
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.join("patch.toml").exists()).collect();
    dirs.sort();
    dirs.into_iter().map(|d| Manifest::load(&d)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_example() {
        let m = Manifest::parse(
            Path::new("/p/patches/sub_meteors"),
            r#"
kind    = "script"
entry   = "main.lua"
layer   = "overlay"
params.count = { type = "int",   default = 40,  range = [1, 400] }
params.speed = { type = "float", default = 1.0, range = [0.1, 4.0] }
trigger = { attack = "100ms", hold = "4s", release = "1s", retrigger = "stack" }
budget  = { cpu_ms = 1.0 }
"#,
        )
        .unwrap();
        assert_eq!(m.id, "sub_meteors");
        assert_eq!(m.kind, Kind::Script);
        assert_eq!(m.layer, Layer::Overlay);
        assert_eq!(m.params["count"].meta("sub_meteors").default, Value::Int(40));
        assert_eq!((m.trigger.attack_ms, m.trigger.hold_ms, m.trigger.release_ms), (100, Some(4000), 1000));
        assert!(m.has_trigger);
        assert_eq!(m.budget.cpu_ms, Some(1.0));
    }

    #[test]
    fn rejects_bad_manifests() {
        assert!(Manifest::parse(Path::new("/p/patches/x"), "kind = \"nope\"").is_err());
        assert!(Manifest::parse(Path::new("/p/patches/x"), "kind = \"shader\"\nentry = \"../../etc/passwd\"").is_err());
        assert!(Manifest::parse(Path::new("/p/patches/x"), "kind = \"shader\"\nparams.env = { type = \"float\" }").is_err());
        assert!(Manifest::parse(Path::new("/p/patches/x"), "kind = \"particles\"").is_err());
        assert!(Manifest::parse(Path::new("/p/patches/a.b"), "kind = \"shader\"").is_err());
    }

    #[test]
    fn slots_align_vectors() {
        let m = Manifest::parse(
            Path::new("/p/patches/x"),
            "kind = \"shader\"\nparams.a = { type = \"float\" }\nparams.b = { type = \"color\" }\nparams.c = { type = \"vec2\" }\nparams.d = { type = \"float\" }",
        )
        .unwrap();
        let s: Vec<(&str, usize)> = m.param_slots().into_iter().map(|(n, _, o)| (n, o)).collect();
        assert_eq!(s, vec![("a", 0), ("b", 4), ("c", 8), ("d", 10)]);
    }
}
