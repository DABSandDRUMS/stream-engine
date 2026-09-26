//! `se-ui`: the stream-engine window, a pure client of the engine API (§3.1, §15).

pub mod app;
pub mod model;
pub mod views;

use std::path::PathBuf;

pub struct UiOpts {
    pub socket: Option<PathBuf>,
    pub layout: Option<String>,
    pub program_only: bool,
}

pub fn run(opts: UiOpts) -> anyhow::Result<()> {
    let app_id = if opts.program_only { "stream-engine.program" } else { "stream-engine" };
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_app_id(app_id).with_title("stream-engine").with_inner_size([1600.0, 1000.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    let (socket, layout) = (opts.socket, opts.layout);
    eframe::run_native("stream-engine", native, Box::new(move |cc| Ok(Box::new(app::App::new(cc, socket, layout))))).map_err(|e| anyhow::anyhow!("ui: {e}"))
}
