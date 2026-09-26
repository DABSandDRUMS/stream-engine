//! Build-mode dock (§15.5) on `egui_dock`: tabs can be rearranged, split, and popped out into
//! their own window (`stream-engine.<panel>`). The arrangement is saved in the layout as tab
//! groups laid out left → right.

use crate::app::App;
use crate::panels::Panel;
use egui_dock::{DockArea, DockState, NodeIndex, Style, TabViewer};

/// Default dock groups (left → right).
pub fn default_groups() -> Vec<Vec<Panel>> {
    let timeline = Panel::parse("timeline");
    let mut mid = vec![Panel::Scopes];
    if let Some(t) = timeline {
        mid.insert(0, t);
    }
    vec![vec![Panel::Rules, Panel::Modulate], mid, vec![Panel::Trace, Panel::Simulator, Panel::Console]]
}

/// Build a dock with `groups` side by side; `fractions[i]` is the share of group i (optional).
pub fn build(groups: &[Vec<Panel>], fractions: &[f32]) -> DockState<Panel> {
    let groups: Vec<Vec<Panel>> = groups.iter().filter(|g| !g.is_empty()).cloned().collect();
    let Some((first, rest)) = groups.split_first() else { return DockState::new(vec![Panel::Console]) };
    let mut state = DockState::new(first.clone());
    let n = groups.len() as f32;
    let share = |i: usize| fractions.get(i).copied().filter(|f| *f > 0.0 && *f < 1.0).unwrap_or(1.0 / n);
    let mut node = NodeIndex::root();
    let mut remaining = 1.0f32;
    for (i, g) in rest.iter().enumerate() {
        // split the remaining area: the left part keeps share(i) of the total
        let left = share(i) / remaining;
        let [_, right] = state.main_surface_mut().split_right(node, left.clamp(0.05, 0.95), g.clone());
        remaining -= share(i);
        node = right;
    }
    state
}

/// Tab groups of the main surface, left → right (for saving the layout).
pub fn groups(state: &DockState<Panel>) -> Vec<Vec<Panel>> {
    state.iter_leaves().map(|(_, leaf)| leaf.tabs().to_vec()).filter(|g: &Vec<Panel>| !g.is_empty()).collect()
}

struct Viewer<'a> {
    app: &'a mut App,
    pop: Vec<Panel>,
}

impl TabViewer for Viewer<'_> {
    type Tab = Panel;

    fn id(&mut self, tab: &mut Panel) -> egui::Id {
        egui::Id::new(("dock", tab.id()))
    }

    fn title(&mut self, tab: &mut Panel) -> egui::WidgetText {
        tab.title().into()
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Panel) {
        tab.ui(self.app, ui);
    }

    fn context_menu(&mut self, ui: &mut egui::Ui, tab: &mut Panel, _path: egui_dock::NodePath) {
        if ui.button(format!("pop out (stream-engine.{})", tab.id())).clicked() {
            self.pop.push(*tab);
            ui.close();
        }
    }

    fn is_closeable(&self, _tab: &Panel) -> bool {
        false
    }
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let mut state = std::mem::replace(&mut app.dock, DockState::new(Vec::new()));
    let mut style = Style::from_egui(ui.style());
    style.tab_bar.fill_tab_bar = true;
    style.tab.tab_body.bg_fill = app.t.bg;
    style.tab_bar.bg_fill = app.t.bg_dark;
    let mut viewer = Viewer { app, pop: Vec::new() };
    DockArea::new(&mut state)
        .id(egui::Id::new("build-dock"))
        .style(style)
        .show_close_buttons(false)
        .show_leaf_close_all_buttons(false)
        .show_leaf_collapse_buttons(false)
        .tab_context_menus(true)
        .draggable_tabs(true)
        .show_inside(ui, &mut viewer);
    let pop = std::mem::take(&mut viewer.pop);
    for p in pop {
        if let Some(path) = state.find_tab(&p) {
            state.remove_tab(path);
        }
        viewer.app.pop_out(&p.id());
    }
    viewer.app.dock = state;
}

/// Put a tab back into the dock (after its pop-out window closed).
pub fn dock_back(state: &mut DockState<Panel>, p: Panel) {
    if state.find_tab(&p).is_none() {
        state.push_to_focused_leaf(p);
    }
}

/// Make `p` the visible tab (adding it when missing). Returns false when it isn't a dock tab.
pub fn focus(state: &mut DockState<Panel>, p: Panel) -> bool {
    if !p.is_dock_tab() {
        return false;
    }
    match state.find_tab(&p) {
        Some(path) => {
            let _ = state.set_active_tab(path);
        }
        None => state.push_to_focused_leaf(p),
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_round_trip_through_the_dock_tree() {
        let g = vec![vec![Panel::Rules, Panel::Modulate], vec![Panel::Scopes], vec![Panel::Trace, Panel::Console]];
        let st = build(&g, &[0.4, 0.3, 0.3]);
        assert_eq!(groups(&st), g);
        let one = build(&[vec![Panel::Console]], &[]);
        assert_eq!(groups(&one), vec![vec![Panel::Console]]);
        let empty = build(&[], &[]);
        assert_eq!(groups(&empty), vec![vec![Panel::Console]]);
    }

    #[test]
    fn focus_adds_missing_dock_tabs_only() {
        let mut st = build(&[vec![Panel::Rules]], &[]);
        assert!(focus(&mut st, Panel::Trace));
        assert!(st.find_tab(&Panel::Trace).is_some());
        assert!(!focus(&mut st, Panel::Multiview));
        assert!(st.find_tab(&Panel::Multiview).is_none());
    }
}
