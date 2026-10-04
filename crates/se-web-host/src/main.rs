//! `stream-engine-web`: the CEF host process for web sources (§4.2, §6.1, §13.3).
//!
//! The engine never links libcef; it spawns this binary (and restarts it if it dies) and talks
//! to it over the protocol in `se_web::protocol`. The same binary is Chromium's subprocess helper
//! (`browser_subprocess_path` = itself), so `execute_process` runs first.
//!
//! Modes (the engine passes these; not meant to be run by hand except `--se-version`):
//! * `--se-ipc-fd=0 --se-profile=<dir> [--se-log-file=<path>] [--se-devtools-port=<n>] [--se-no-gpu]`
//!   — off-screen host driven by the engine.
//! * `--se-login=<url> --se-profile=<dir>` — windowed sign-in browser on the same profile.
//! * `--se-version` — print the CEF version (install check).

mod app;
mod ipc;
mod login;
mod osr;
mod task;
mod youtube;

use cef::*;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

pub enum Mode {
    Osr { ipc_fd: i32 },
    Login { url: String },
    Version,
}

pub struct Opts {
    pub mode: Option<Mode>,
    pub profile: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    pub devtools_port: Option<u16>,
    pub gpu: bool,
}

impl Opts {
    /// Our own `--se-*` switches; everything else (Chromium's subprocess switches) is ignored.
    fn parse(args: impl Iterator<Item = String>) -> Result<Opts, String> {
        let mut o = Opts { mode: None, profile: None, log_file: None, devtools_port: None, gpu: true };
        for a in args {
            if let Some(v) = a.strip_prefix("--se-ipc-fd=") {
                o.mode = Some(Mode::Osr { ipc_fd: v.parse().map_err(|_| format!("bad {a}"))? });
            } else if let Some(v) = a.strip_prefix("--se-login=") {
                o.mode = Some(Mode::Login { url: v.to_string() });
            } else if a == "--se-version" {
                o.mode = Some(Mode::Version);
            } else if let Some(v) = a.strip_prefix("--se-profile=") {
                o.profile = Some(PathBuf::from(v));
            } else if let Some(v) = a.strip_prefix("--se-log-file=") {
                o.log_file = Some(PathBuf::from(v));
            } else if let Some(v) = a.strip_prefix("--se-devtools-port=") {
                o.devtools_port = Some(v.parse().map_err(|_| format!("bad {a}"))?);
            } else if a == "--se-no-gpu" {
                o.gpu = false;
            }
        }
        Ok(o)
    }
}

fn fail(msg: String) -> ! {
    ipc::log("error", msg.clone());
    let _ = writeln!(std::io::stderr(), "stream-engine-web: {msg}");
    std::process::exit(1);
}

/// Private profile directory (it holds the signed-in Google session).
fn prepare_profile(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

fn main() {
    let opts = match Opts::parse(std::env::args().skip(1)) {
        Ok(o) => Arc::new(o),
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "stream-engine-web: {e}");
            std::process::exit(2);
        }
    };
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    if let Some(Mode::Version) = opts.mode {
        let v = std::ffi::CStr::from_bytes_until_nul(sys::CEF_VERSION).map(|c| c.to_string_lossy().into_owned()).unwrap_or_default();
        println!("stream-engine-web {} (CEF {v})", env!("CARGO_PKG_VERSION"));
        return;
    }

    let args = args::Args::new();
    let mut app = app::build(opts.clone());
    // Renderer, GPU, utility, and zygote processes run their main here and exit.
    let code = execute_process(Some(args.as_main_args()), Some(&mut app), std::ptr::null_mut());
    if code >= 0 {
        std::process::exit(code);
    }

    let Some(mode) = &opts.mode else {
        let _ =
            writeln!(std::io::stderr(), "stream-engine-web is started by the stream-engine daemon (see docs/web.md); `--se-version` prints the CEF version");
        std::process::exit(2);
    };
    if let Mode::Osr { ipc_fd } = mode
        && let Err(e) = ipc::adopt(*ipc_fd)
    {
        let _ = writeln!(std::io::stderr(), "stream-engine-web: {e}");
        std::process::exit(2);
    }
    let Some(profile) = opts.profile.clone() else { fail("--se-profile=<dir> is required".into()) };
    if let Err(e) = prepare_profile(&profile) {
        fail(format!("profile {}: {e}", profile.display()));
    }
    let exe = std::env::current_exe().unwrap_or_else(|e| fail(format!("current_exe: {e}")));
    let profile_s = profile.to_string_lossy().into_owned();
    let log_file = opts.log_file.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let settings = Settings {
        no_sandbox: 1,
        windowless_rendering_enabled: matches!(mode, Mode::Osr { .. }) as i32,
        browser_subprocess_path: exe.to_string_lossy().as_ref().into(),
        root_cache_path: profile_s.as_str().into(),
        cache_path: profile_s.as_str().into(),
        persist_session_cookies: 1,
        log_file: log_file.as_str().into(),
        log_severity: if log_file.is_empty() { LogSeverity::DEFAULT } else { LogSeverity::WARNING },
        remote_debugging_port: opts.devtools_port.map_or(0, i32::from),
        background_color: 0,
        ..Default::default()
    };
    if initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), std::ptr::null_mut()) != 1 {
        fail(format!("CEF initialization failed (is another stream-engine-web using the profile {}? see the CEF log)", profile.display()));
    }
    if matches!(mode, Mode::Osr { .. })
        && let Err(e) = ipc::spawn_reader()
    {
        fail(format!("control socket reader: {e}"));
    }
    run_message_loop();
    shutdown();
    std::process::exit(0);
}
