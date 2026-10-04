//! One color vocabulary for render uniforms, video FX, and lighting.

/// Slot order matches the shared shader palette uniform.
pub const SLOTS: [&str; 8] = ["accent", "background", "foreground", "red", "yellow", "green", "cyan", "magenta"];

pub const ADDRESSES: [&str; 8] = [
    "palette.accent", "palette.background", "palette.foreground", "palette.red",
    "palette.yellow", "palette.green", "palette.cyan", "palette.magenta",
];

/// An authored live color reference, optionally selecting one RGBA component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reference {
    slot: usize,
    component: Option<usize>,
}

impl Reference {
    pub fn parse(value: &str) -> Option<Self> {
        let name = value.strip_prefix("stream:").or_else(|| value.strip_prefix("@palette."))?;
        let (slot, component) = match name.split_once('.') {
            Some((slot, component)) => (slot, Some(match component {
                "r" => 0, "g" => 1, "b" => 2, "a" => 3, _ => return None,
            })),
            None => (name, None),
        };
        Some(Self { slot: SLOTS.iter().position(|candidate| *candidate == slot)?, component })
    }

    pub fn slot(self) -> usize { self.slot }
    pub fn component(self) -> Option<usize> { self.component }
    pub fn address(self) -> &'static str { ADDRESSES[self.slot] }
}
