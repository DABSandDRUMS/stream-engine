//! Settings → Troubleshooting: the power tools, kept out of the way (§15.5). Test events
//! (simulator), "why did that happen?" (trace), messages and errors (console), and live signal
//! graphs (scopes). Signal → setting links live in Automation → Modulation.

use crate::app::App;
use crate::panels::Panel;
use crate::views::tools;
use se_ui_kit::widgets;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tool {
    #[default]
    Test,
    Why,
    Messages,
    Signals,
}

impl Tool {
    const ALL: [(Tool, &'static str); 4] =
        [(Tool::Test, "Test events"), (Tool::Why, "Why did that happen?"), (Tool::Messages, "Messages & errors"), (Tool::Signals, "Live signals")];

    /// The tool that replaces an old dock panel.
    pub fn for_panel(p: Panel) -> Tool {
        match p {
            Panel::Trace => Tool::Why,
            Panel::Console => Tool::Messages,
            Panel::Scopes => Tool::Signals,
            _ => Tool::Test,
        }
    }
}

#[derive(Default)]
pub struct TroubleshootState {
    pub tool: Tool,
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let labels: Vec<&str> = Tool::ALL.iter().map(|(_, l)| *l).collect();
    let mut i = Tool::ALL.iter().position(|(tool, _)| *tool == app.troubleshoot.tool).unwrap_or(0);
    if widgets::segmented(ui, &t, &mut i, &labels) {
        app.troubleshoot.tool = Tool::ALL[i].0;
    }
    ui.add_space(se_ui_kit::theme::spacing::M);
    let blurb = match app.troubleshoot.tool {
        Tool::Test => "Pretend something happened (a raid, a big cheer, a gift bomb) to test your triggers and alerts. Nothing is sent to Twitch.",
        Tool::Why => "Pick something that happened and see, step by step, what it set off.",
        Tool::Messages => "What Stream Engine is saying, including mistakes in your settings files. “Open file” takes you straight to the line.",
        Tool::Signals => "Graphs of live signals like drum hits, music level and controller moves.",
    };
    widgets::hint(ui, &t, blurb);
    ui.add_space(se_ui_kit::theme::spacing::M);
    widgets::panel(ui, &t, |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height(ui.available_height() - 40.0);
        match app.troubleshoot.tool {
            Tool::Test => tools::simulator(app, ui),
            Tool::Why => tools::trace(app, ui),
            Tool::Messages => tools::console(app, ui),
            Tool::Signals => tools::scopes(app, ui),
        }
    });
}
