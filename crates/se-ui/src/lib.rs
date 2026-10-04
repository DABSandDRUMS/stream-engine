//! `se-ui`: the stream-engine window, a pure client of the engine API (§3.1, §15).

pub mod app;
pub mod diagnose;
pub mod display;
pub mod editor;
pub mod frames;
pub mod health;
pub mod layout;
pub mod model;
pub mod monitors;
pub mod palette;
pub mod panels;
pub mod preferences;
pub mod recovery;
pub mod shortcuts;
pub mod shortcuts_ui;
pub mod views;
pub mod windows;

use std::path::PathBuf;

#[derive(Clone, Debug, Default)]
pub struct UiOpts {
    pub socket: Option<PathBuf>,
    pub layout: Option<String>,
    pub program_only: bool,
}

pub fn run(opts: UiOpts) -> anyhow::Result<()> {
    let app_id = if opts.program_only { layout::CONFIDENCE_APP_ID } else { layout::MAIN_APP_ID };
    let mut viewport = egui::ViewportBuilder::default()
        .with_app_id(app_id)
        .with_title(if opts.program_only { "stream-engine · program" } else { "stream-engine" })
        .with_inner_size([1600.0, 1000.0]);
    if opts.program_only {
        viewport = viewport.with_decorations(false);
    }
    let native = eframe::NativeOptions { viewport, renderer: eframe::Renderer::Wgpu, wgpu_options: frames::wgpu_options(), ..Default::default() };
    eframe::run_native("stream-engine", native, Box::new(move |cc| Ok(Box::new(app::App::new(cc, opts))))).map_err(|e| anyhow::anyhow!("ui: {e}"))
}
