//! Built-in transition looks (§4.4): the shaders the renderer ships for `transitions/*.toml`
//! (`shader = "<name>"`) with their settings, and the starting points offered when making a new
//! transition. The renderer compiles these shaders by name; editors read the plain labels and
//! slider ranges from here.

/// One setting of a built-in shader: its key in `transitions/<name>.toml`, a plain label, the
/// slider range, and words for the two ends of the slider.
#[derive(Debug)]
pub struct Param {
    pub name: &'static str,
    pub label: &'static str,
    pub default: f64,
    pub min: f64,
    pub max: f64,
    pub low: &'static str,
    pub high: &'static str,
}

/// A transition shader built into the renderer.
#[derive(Debug)]
pub struct Shader {
    pub name: &'static str,
    pub label: &'static str,
    pub params: &'static [Param],
}

/// Every built-in transition shader. `fade` is also the fallback when a shader is missing.
pub const SHADERS: &[Shader] = &[
    Shader { name: "fade", label: "Crossfade", params: &[] },
    Shader {
        name: "glitch",
        label: "Glitch",
        params: &[
            Param { name: "strength", label: "Glitchiness", default: 1.0, min: 0.0, max: 3.0, low: "Subtle", high: "Wild" },
            Param { name: "block", label: "Block size", default: 16.0, min: 4.0, max: 64.0, low: "Fine", high: "Chunky" },
        ],
    },
    Shader {
        name: "zoomblur",
        label: "Zoom blur",
        params: &[Param { name: "strength", label: "Blur amount", default: 0.4, min: 0.05, max: 1.0, low: "Light", high: "Heavy" }],
    },
];

/// The built-in shader called `name`.
pub fn shader(name: &str) -> Option<&'static Shader> {
    SHADERS.iter().find(|s| s.name == name)
}

/// A starting point for a new transition: the file it becomes.
#[derive(Debug)]
pub struct Style {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// `morph | shader | combined`
    pub kind: &'static str,
    pub shader: Option<&'static str>,
    pub ms: u32,
    /// Engine ease name (`se_proto::Ease`, snake_case).
    pub ease: &'static str,
    /// Starting values for some of the shader's settings (the rest start at the shader default).
    pub values: &'static [(&'static str, f64)],
}

/// "New transition → Start from", in menu order.
pub const STYLES: &[Style] = &[
    Style {
        id: "fade",
        label: "Crossfade",
        description: "The new scene fades in over the old one.",
        kind: "shader",
        shader: Some("fade"),
        ms: 600,
        ease: "in_out_cubic",
        values: &[],
    },
    Style {
        id: "morph",
        label: "Glide",
        description: "Cameras that are in both scenes glide and resize into their new spots; the rest come and go.",
        kind: "morph",
        shader: None,
        ms: 700,
        ease: "in_out_cubic",
        values: &[],
    },
    Style {
        id: "zoomblur",
        label: "Zoom blur",
        description: "A fast zoom with motion blur that whooshes into the new scene.",
        kind: "shader",
        shader: Some("zoomblur"),
        ms: 800,
        ease: "in_out_cubic",
        values: &[],
    },
    Style {
        id: "glitch",
        label: "Glitch",
        description: "Blocky, colorful digital glitches break up the picture while it changes.",
        kind: "shader",
        shader: Some("glitch"),
        ms: 600,
        ease: "in_out_cubic",
        values: &[],
    },
    Style {
        id: "morph_glitch",
        label: "Glide + glitch",
        description: "A glide with a burst of glitches while things move.",
        kind: "combined",
        shader: Some("glitch"),
        ms: 700,
        ease: "in_out_cubic",
        values: &[("strength", 1.5), ("block", 24.0)],
    },
];

/// The starting point called `id`.
pub fn style(id: &str) -> Option<&'static Style> {
    STYLES.iter().find(|s| s.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_use_known_shaders_eases_and_settings() {
        for s in STYLES {
            assert!(matches!(s.kind, "morph" | "shader" | "combined"), "{}", s.id);
            assert_eq!(s.shader.is_some(), s.kind != "morph", "{}", s.id);
            assert!(se_proto::Ease::parse(s.ease).is_some(), "{}: ease {}", s.id, s.ease);
            let sh = s.shader.map(|n| shader(n).unwrap_or_else(|| panic!("{}: unknown shader {n}", s.id)));
            for (k, v) in s.values {
                let p = sh.and_then(|sh| sh.params.iter().find(|p| p.name == *k)).unwrap_or_else(|| panic!("{}: no setting {k}", s.id));
                assert!((p.min..=p.max).contains(v), "{}: {k} = {v} outside its slider", s.id);
            }
        }
        for sh in SHADERS {
            for p in sh.params {
                assert!(p.min < p.max && (p.min..=p.max).contains(&p.default), "{}.{}", sh.name, p.name);
            }
        }
    }
}
