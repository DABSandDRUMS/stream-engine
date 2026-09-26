//! `stream-engine`: one binary, two processes (§3.1).
//!
//! * `stream-engine daemon` — the headless engine (systemd user service)
//! * `stream-engine ui` — the window (a client of the engine)
//! * `stream-engine replay` — re-run a recorded session
//! * `stream-engine new` — create a project from the starter template

mod api_admin;
mod daemon;
mod http_extra;
mod logging;
mod omarchy;
mod project_io;
mod queries;
mod replay;
mod session_tasks;
mod subsystems;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Debug builds count allocations on real-time threads (render, audio, DMX) to enforce the
/// no-per-frame-allocation rule (§21).
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOC: se_alloc::Counting = se_alloc::Counting;

#[derive(Parser)]
#[command(name = "stream-engine", version, about = "All-in-one live-stream production engine")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the engine (normally via `systemctl --user start stream-engine`).
    Daemon {
        #[arg(long)]
        project: Option<PathBuf>,
        /// Verbose tracing, validation layers, simulator enabled.
        #[arg(long)]
        dev: bool,
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        http: Option<SocketAddr>,
        #[arg(long)]
        osc: Option<SocketAddr>,
        /// Start fresh instead of restoring the last runtime state.
        #[arg(long)]
        no_restore: bool,
        /// Runtime DB location (default ~/.local/share/stream-engine).
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Open the UI window.
    Ui {
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Named layout (`show-3disp`, `show-2disp`, `build`).
        #[arg(long)]
        layout: Option<String>,
        /// Open only the confidence (program) window.
        #[arg(long)]
        program: bool,
    },
    /// Replay a session segment through a fresh core and print the state.
    Replay {
        /// Session directory (`<project>/sessions/<id>`) or id.
        session: String,
        #[arg(long)]
        project: Option<PathBuf>,
        /// Segment (engine run) within the session; negative counts from the end.
        #[arg(long, default_value_t = -1, allow_hyphen_values = true)]
        segment: isize,
        /// Keep ticking this long after the last input.
        #[arg(long, default_value_t = 0)]
        settle_ms: u64,
        /// Address patterns to print.
        #[arg(long = "print")]
        print: Vec<String>,
    },
    /// Create a new project from the starter template.
    New { dir: PathBuf },
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct EngineToml {
    project: Option<PathBuf>,
    http: Option<SocketAddr>,
    osc: Option<SocketAddr>,
}

fn engine_toml() -> EngineToml {
    let p = config_dir().join("engine.toml");
    std::fs::read_to_string(&p).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("stream-engine")
}

/// Installed data (`/usr/share/stream-engine`) or the source tree in development.
pub fn share_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("STREAM_ENGINE_SHARE") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(prefix) = exe.parent().and_then(Path::parent)
    {
        let p = prefix.join("share/stream-engine");
        if p.join("web").exists() {
            return p;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn resolve_project(arg: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = arg.or_else(|| engine_toml().project) {
        return Ok(p);
    }
    bail!(
        "no project configured. Create one with `stream-engine new ~/stream-project` and set `project = \"...\"` in {}",
        config_dir().join("engine.toml").display()
    )
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let (s, d) = (e.path(), dst.join(e.file_name()));
        if e.file_name() == "sessions" || e.file_name() == ".backups" {
            continue;
        }
        if s.is_dir() {
            copy_dir(&s, &d)?;
        } else {
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Daemon { project, dev, socket, http, osc, no_restore, data_dir } => {
            let et = engine_toml();
            daemon::run(daemon::DaemonOpts {
                project: resolve_project(project)?,
                dev,
                socket,
                http: http.or(et.http).unwrap_or_else(|| "127.0.0.1:7870".parse().unwrap()),
                osc: osc.or(et.osc).unwrap_or_else(|| "127.0.0.1:7871".parse().unwrap()),
                restore: !no_restore,
                web_root: share_dir().join("web"),
                data_dir: data_dir.unwrap_or_else(se_store::data_dir),
            })
        }
        Cmd::Ui { socket, layout, program } => se_ui::run(se_ui::UiOpts { socket, layout, program_only: program }),
        Cmd::Replay { session, project, segment, settle_ms, print } => {
            let project = resolve_project(project)?;
            let dir = if Path::new(&session).is_dir() { PathBuf::from(&session) } else { project.join("sessions").join(&session) };
            let out = replay::replay(&project, &dir, segment, settle_ms)?;
            replay::print(&out, &print);
            Ok(())
        }
        Cmd::New { dir } => {
            if dir.join("project.toml").exists() {
                bail!("{} already contains a project", dir.display());
            }
            let tpl = share_dir().join("project-example");
            copy_dir(&tpl, &dir).with_context(|| format!("copy template {}", tpl.display()))?;
            println!("created {}", dir.display());
            println!("set `project = \"{}\"` in {}", dir.canonicalize()?.display(), config_dir().join("engine.toml").display());
            Ok(())
        }
    }
}
