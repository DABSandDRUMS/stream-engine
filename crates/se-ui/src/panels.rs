//! Panels: every dockable / pop-out-able piece of the UI, with a stable id used by layouts
//! (`layouts/*.toml`) and window app-ids (`stream-engine.<id>`, §15.3).

use crate::app::{App, ViewId};
use crate::views::{self, show::RailTab};
use se_ui_kit::widgets::icon;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Panel {
    /// Preview + program wide + program tall row.
    Monitors,
    Preview,
    Program,
    ProgramTall,
    /// Confidence view: exactly what goes out (wide, tall, or both per layout).
    Confidence,
    Multiview,
    Scenes,
    Presets,
    Active,
    Events,
    Chat,
    Queue,
    Mod,
    Mix,
    Library,
    Canvas,
    Inspector,
    Modulate,
    Rules,
    Scopes,
    Trace,
    Simulator,
    Console,
    /// Any full view (§15.6) as a panel.
    View(ViewId),
}

impl Panel {
    pub const FIXED: &'static [Panel] = &[
        Panel::Monitors,
        Panel::Preview,
        Panel::Program,
        Panel::ProgramTall,
        Panel::Confidence,
        Panel::Multiview,
        Panel::Scenes,
        Panel::Presets,
        Panel::Active,
        Panel::Events,
        Panel::Chat,
        Panel::Queue,
        Panel::Mod,
        Panel::Mix,
        Panel::Library,
        Panel::Canvas,
        Panel::Inspector,
        Panel::Modulate,
        Panel::Rules,
        Panel::Scopes,
        Panel::Trace,
        Panel::Simulator,
        Panel::Console,
    ];

    /// Every panel, including one per registered view.
    pub fn all() -> Vec<Panel> {
        let mut v = Panel::FIXED.to_vec();
        v.extend(ViewId::ALL.iter().map(|(id, _, _)| Panel::View(*id)));
        v
    }

    pub fn id(self) -> String {
        match self {
            Panel::Monitors => "monitors".into(),
            Panel::Preview => "preview".into(),
            Panel::Program => "program".into(),
            Panel::ProgramTall => "program-tall".into(),
            Panel::Confidence => "confidence".into(),
            Panel::Multiview => "multiview".into(),
            Panel::Scenes => "scenes".into(),
            Panel::Presets => "presets".into(),
            Panel::Active => "active".into(),
            Panel::Events => "events".into(),
            Panel::Chat => "chat".into(),
            Panel::Queue => "queue".into(),
            Panel::Mod => "mod".into(),
            Panel::Mix => "mix".into(),
            Panel::Library => "library".into(),
            Panel::Canvas => "canvas".into(),
            Panel::Inspector => "inspector".into(),
            Panel::Modulate => "modulate".into(),
            Panel::Rules => "rules".into(),
            Panel::Scopes => "scopes".into(),
            Panel::Trace => "trace".into(),
            Panel::Simulator => "simulator".into(),
            Panel::Console => "console".into(),
            Panel::View(v) => format!("view.{}", view_slug(v)),
        }
    }

    /// Parse a panel id; `timeline`/`perf`/`<view>` are accepted as view shorthands.
    pub fn parse(s: &str) -> Option<Panel> {
        let s = s.trim();
        if let Some(p) = Panel::FIXED.iter().copied().find(|p| p.id() == s) {
            return Some(p);
        }
        let v = s.strip_prefix("view.").unwrap_or(s);
        let v = match v {
            "perf" => "performance",
            other => other,
        };
        ViewId::ALL.iter().find(|(id, _, _)| view_slug(*id) == v).map(|(id, _, _)| Panel::View(*id))
    }

    pub fn title(self) -> String {
        let (ic, name) = match self {
            Panel::Monitors => (icon::SCENE, "Preview / Program"),
            Panel::Preview => (icon::SCENE, "Preview"),
            Panel::Program => (icon::LIVE, "Program"),
            Panel::ProgramTall => (icon::LIVE, "Program tall"),
            Panel::Confidence => (icon::LIVE, "Confidence"),
            Panel::Multiview => (icon::SCENE, "Multiview"),
            Panel::Scenes => (icon::SCENE, "Scenes"),
            Panel::Presets => (icon::PRESET, "Presets"),
            Panel::Active => (icon::EFFECT, "Active"),
            Panel::Events => (icon::ALERT, "Events"),
            Panel::Chat => (icon::CHAT, "Chat"),
            Panel::Queue => (icon::QUEUE, "Queue"),
            Panel::Mod => (icon::MOD, "Mod"),
            Panel::Mix => (icon::MIX, "Mix"),
            Panel::Library => (icon::SEARCH, "Library"),
            Panel::Canvas => (icon::SCENE, "Canvas editor"),
            Panel::Inspector => (icon::SEARCH, "Inspector"),
            Panel::Modulate => (icon::BINDING, "Modulate"),
            Panel::Rules => (icon::RULE, "Rules"),
            Panel::Scopes => (icon::SCOPE, "Signal scopes"),
            Panel::Trace => (icon::TRACE, "Trace"),
            Panel::Simulator => (icon::SIM, "Simulator"),
            Panel::Console => (icon::CONSOLE, "Console"),
            Panel::View(v) => {
                let (_, ic, label) = ViewId::ALL.iter().find(|(id, _, _)| *id == v).copied().unwrap_or((v, icon::SEARCH, "view"));
                return format!("{ic} {label}");
            }
        };
        format!("{ic} {name}")
    }

    /// Draw the panel's contents.
    pub fn ui(self, app: &mut App, ui: &mut egui::Ui) {
        match self {
            Panel::Monitors => views::monitor::preview_program_row(app, ui),
            Panel::Preview => views::monitor::single(app, ui, crate::frames::Canvas::Preview),
            Panel::Program => views::monitor::single(app, ui, crate::frames::Canvas::Wide),
            Panel::ProgramTall => views::monitor::single(app, ui, crate::frames::Canvas::Tall),
            Panel::Confidence => crate::windows::confidence_contents(app, ui),
            Panel::Multiview => {
                let h = (ui.available_height() - 8.0).clamp(60.0, 400.0);
                views::monitor::multiview(app, ui, h)
            }
            Panel::Scenes => {
                views::show::scenes(app, ui);
                views::show::transition_row(app, ui);
            }
            Panel::Presets => views::show::pads(app, ui),
            Panel::Active => views::show::active(app, ui),
            Panel::Events => views::rail::events(app, ui),
            Panel::Chat => views::rail::chat(app, ui),
            Panel::Queue => views::rail::queue(app, ui),
            Panel::Mod => views::rail::moderation(app, ui),
            Panel::Mix => views::mix::strip(app, ui),
            Panel::Library => views::build::library(app, ui),
            Panel::Canvas => views::canvas::ui(app, ui),
            Panel::Inspector => views::inspector::ui(app, ui),
            Panel::Modulate => views::modulate::ui(app, ui),
            Panel::Rules => views::rules::ui(app, ui),
            Panel::Scopes => views::tools::scopes(app, ui),
            Panel::Trace => views::tools::trace(app, ui),
            Panel::Simulator => views::tools::simulator(app, ui),
            Panel::Console => views::tools::console(app, ui),
            Panel::View(v) => app.view_ui(v, ui),
        }
    }

    /// The Show-mode rail tab this panel corresponds to.
    pub fn rail_tab(self) -> Option<RailTab> {
        match self {
            Panel::Events => Some(RailTab::Events),
            Panel::Chat => Some(RailTab::Chat),
            Panel::Queue => Some(RailTab::Queue),
            Panel::Mod => Some(RailTab::Mod),
            _ => None,
        }
    }

    /// Lives in the Build-mode dock (bottom area).
    pub fn is_dock_tab(self) -> bool {
        matches!(self, Panel::Rules | Panel::Modulate | Panel::Scopes | Panel::Trace | Panel::Simulator | Panel::Console | Panel::View(_))
    }
}

/// Stable lowercase id of a view (`ViewId::Performance` → `performance`).
pub fn view_slug(v: ViewId) -> String {
    let dbg = format!("{v:?}");
    let mut out = String::new();
    for (i, c) in dbg.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_ids_round_trip() {
        for p in Panel::all() {
            assert_eq!(Panel::parse(&p.id()), Some(p), "{}", p.id());
        }
        assert_eq!(Panel::parse("nope"), None);
        assert!(Panel::parse("view.sessions").is_some());
        assert!(Panel::parse("sessions").is_some());
    }

    #[test]
    fn view_slugs_are_snake_case() {
        assert_eq!(view_slug(ViewId::Sessions), "sessions");
    }
}
