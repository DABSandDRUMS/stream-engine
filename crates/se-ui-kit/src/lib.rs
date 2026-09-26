//! `se-ui-kit`: the stream-engine design system. Theme tokens follow the active Omarchy
//! theme and font live; every widget draws with the tokens (egui's default look is not used).

pub mod theme;
pub mod widgets;

pub use theme::{Theme, ThemeChange, ThemeWatcher, install_font, spacing, type_scale};
pub use widgets::{LedState, icon};
