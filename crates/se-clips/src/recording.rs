//! Automatic OBS recording and per-session show folders. The OBS adapter asks
//! `recording.prepare` before *every* record start, including a manual one.

use crate::show::{RecordingConfig, show_dir_from_meta};
use parking_lot::Mutex;
use se_hub::EngineCtx;
use se_proto::{Command, Op, Origin, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TARGET: &str = "recording";

struct State {
    config: RecordingConfig,
    session: String,
    show_dir: Option<PathBuf>,
    requested: Option<Instant>,
    owned: bool,
    error: Option<String>,
}

fn config(ctx: &EngineCtx) -> Result<RecordingConfig, String> {
    let cfg = RecordingConfig::from_section(ctx.project_section("recording").as_ref())?;
    if !cfg.dir_path().is_absolute() {
        return Err(format!("[recording] dir must be absolute (or start with ~/): {}", cfg.dir));
    }
    for mode in &cfg.modes {
        if !ctx.config.borrow().project.modes.contains(mode) || mode == "offline" {
            return Err(format!("[recording] modes contains `{mode}`; choose an on-air show mode"));
        }
    }
    Ok(cfg)
}

fn wants_recording(config: &RecordingConfig, mode: &str, armed: bool) -> bool {
    config.auto && mode != "offline" && (armed || config.modes.iter().any(|start| start == mode))
}

fn show_name(id: &str) -> String {
    // Session creation may be hours earlier than the first recording.
    show_name_at(id, &se_store::session::new_session_id())
}

fn show_name_at(id: &str, stamp: &str) -> String {
    format!("{}-{}-{} {}-{} ({id})", &stamp[..4], &stamp[4..6], &stamp[6..8], &stamp[9..11], &stamp[11..13])
}

fn snapshot_project(root: &Path, show: &Path) -> Result<(), String> {
    let dest = show.join("data/project");
    if dest.exists() {
        return Ok(());
    }
    let partial = show.join("data/.project.partial");
    if partial.exists() {
        std::fs::remove_dir_all(&partial).map_err(|e| format!("cannot clear interrupted project snapshot: {e}"))?;
    }
    std::fs::create_dir_all(&partial).map_err(|e| format!("cannot create project snapshot: {e}"))?;
    fn copy_text(root: &Path, dest: &Path, show: &Path, depth: usize) -> std::io::Result<()> {
        if depth > 8 {
            return Ok(());
        }
        for item in std::fs::read_dir(root)? {
            let item = item?;
            if item.path().starts_with(show) {
                continue;
            }
            let name = item.file_name();
            let name = name.to_string_lossy();
            let lower = name.to_ascii_lowercase();
            if name.starts_with('.')
                || matches!(lower.as_str(), "assets" | "sessions" | "secrets" | "credentials" | "private" | "node_modules" | "target")
                || [".env", "secret", "credential", "password", "token", "private_key"].iter().any(|s| lower.contains(s))
            {
                continue;
            }
            let ty = item.file_type()?;
            if ty.is_symlink() {
                continue;
            }
            let out = dest.join(item.file_name());
            if ty.is_dir() {
                copy_text(&item.path(), &out, show, depth + 1)?;
            } else if ty.is_file()
                && matches!(item.path().extension().and_then(|s| s.to_str()), Some("toml" | "lua" | "js" | "ts" | "json" | "css" | "html" | "wgsl"))
                && item.metadata()?.len() <= 1_000_000
            {
                let text = std::fs::read_to_string(item.path())?;
                let ext = item.path().extension().and_then(|e| e.to_str()).unwrap_or_default().to_owned();
                let sensitive = |line: &str| {
                    let lower = line.to_ascii_lowercase();
                    (lower.contains('=') || lower.contains(':'))
                        && ["secret", "password", "token", "credential", "private_key", "api_key"].iter().any(|key| lower.contains(key))
                };
                // A JSON field cannot be removed line-wise without invalidating the
                // document; exclude the whole file rather than snapshot credentials.
                if ext == "json" && text.lines().any(sensitive) {
                    continue;
                }
                let sanitized = text
                    .lines()
                    .map(|line| {
                        if sensitive(line) {
                            match ext.as_str() {
                                "toml" => "# [redacted sensitive setting]",
                                "lua" => "-- [redacted sensitive setting]",
                                "css" => "/* [redacted sensitive setting] */",
                                "html" => "<!-- [redacted sensitive setting] -->",
                                _ => "// [redacted sensitive setting]",
                            }
                        } else {
                            line
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                std::fs::create_dir_all(dest)?;
                std::fs::write(out, sanitized)?;
            }
        }
        Ok(())
    }
    if let Err(e) = copy_text(root, &partial, show, 0) {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(format!("cannot snapshot project: {e}"));
    }
    std::fs::rename(&partial, &dest).map_err(|e| format!("cannot finish project snapshot: {e}"))?;
    Ok(())
}

fn prepare(ctx: &EngineCtx, state: &Mutex<State>) -> Result<PathBuf, String> {
    let session = ctx.hub.info.read().session.clone();
    if session.is_empty() {
        return Err("no active session".into());
    }
    let mut s = state.lock();
    if s.session != session {
        s.session = session.clone();
        s.show_dir = None;
        s.owned = false;
        s.requested = None;
    }
    if let Some(dir) = &s.show_dir {
        return Ok(dir.clone());
    }
    let session_dir = ctx.project_root.join("sessions").join(&session);
    if let Some(dir) = show_dir_from_meta(&session_dir) {
        s.show_dir = Some(dir.clone());
        return Ok(dir);
    }
    let root = s.config.dir_path();
    if !root.is_absolute() {
        return Err(format!("[recording] dir must be an absolute path: {}", root.display()));
    }
    std::fs::create_dir_all(&root).map_err(|e| format!("cannot create recordings directory {}: {e}", root.display()))?;
    let root = root.canonicalize().map_err(|e| format!("cannot resolve recordings directory {}: {e}", root.display()))?;
    let name = show_name(&session);
    let dir = root.join(&name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create show directory {}: {e}", dir.display()))?;
    if s.config.snapshot_project {
        snapshot_project(&ctx.project_root, &dir)?;
    }
    // The session writer serializes metadata writes with its own event stream.
    ctx.hub.command(Command::new(
        Origin::System,
        Op::Action {
            name: "session.meta".into(),
            args: Value::map().with("key", "show").with("value", Value::map().with("dir", dir.display().to_string()).with("name", name)),
        },
    ));
    s.show_dir = Some(dir.clone());
    Ok(dir)
}

fn free_gb(path: &Path) -> Option<f64> {
    use std::os::unix::ffi::OsStrExt;
    let path = path.ancestors().find(|p| p.exists())?;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: statvfs writes the struct on success, path is NUL-terminated.
    if unsafe { libc::statvfs(path.as_ptr(), st.as_mut_ptr()) } != 0 {
        return None;
    }
    let st = unsafe { st.assume_init() };
    Some(st.f_bavail as f64 * st.f_frsize as f64 / 1_000_000_000.0)
}

async fn status(ctx: &EngineCtx, state: &Mutex<State>) -> Value {
    let snap = ctx.hub.snapshot.load();
    let (dir, show_dir, error, expected, requested) = {
        let s = state.lock();
        let expected = snap.str("show.mode").is_some_and(|mode| wants_recording(&s.config, mode, s.owned));
        (s.config.dir_path(), s.show_dir.clone(), s.error.clone(), expected, s.requested.is_some())
    };
    let active = snap.bool("obs.record.active");
    let connected = snap.bool("obs.link");
    let path = snap.str("obs.record.path").unwrap_or("").to_string();
    let actual_dir = snap.str("obs.record.dir").unwrap_or("").to_string();
    let paused = snap.bool("obs.record.paused");
    drop(snap);
    let show = show_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default();
    let mismatch = active && !show.is_empty() && !path.is_empty() && !Path::new(&path).starts_with(&show);
    let dir_mismatch = active && !show.is_empty() && !actual_dir.is_empty() && Path::new(&actual_dir) != Path::new(&show);
    let (level, detail) = if let Some(e) = error {
        ("fail", e)
    } else if !connected {
        ("fail", "OBS is not connected".to_string())
    } else if mismatch {
        ("fail", format!("OBS is writing outside the show folder: {path}"))
    } else if dir_mismatch {
        ("fail", format!("OBS profile points outside the show folder: {actual_dir}"))
    } else if active && path.is_empty() {
        ("warn", "OBS is starting but has not reported a recording file".into())
    } else if active && !Path::new(&path).is_file() {
        ("warn", format!("OBS reports {path}, but the file has not appeared on disk"))
    } else if paused {
        ("warn", "OBS recording is paused".into())
    } else if active {
        ("pass", format!("Recording to {path}"))
    } else if expected && requested {
        ("warn", "Waiting for OBS to start recording".into())
    } else if expected {
        ("fail", "OBS is not recording in the current show mode".into())
    } else {
        ("pass", "Not recording".into())
    };
    let recordings = ctx.hub.query("obs", Value::Null).await.unwrap_or_default();
    let tracks = recordings
        .get_path("recordings")
        .and_then(Value::as_list)
        .and_then(|items| items.iter().rev().find(|r| r.get_path("path").and_then(Value::as_str) == Some(path.as_str())))
        .and_then(|r| r.get_path("tracks"))
        .cloned()
        .unwrap_or_else(|| Value::List(vec![]));
    Value::map()
        .with("dir", dir.display().to_string())
        .with("active", active)
        .with("path", if active { path } else { String::new() })
        .with("health", Value::map().with("status", level).with("detail", detail))
        .with("tracks", tracks)
        .with("free_gb", free_gb(show_dir.as_deref().unwrap_or(&dir)).map(Value::Float).unwrap_or_default())
        .with("session", ctx.hub.info.read().session.clone())
        .with("show_dir", show)
        .with("obs_dir", actual_dir)
}

/// Register the status and prepare queries, then follow show mode and configuration.
pub async fn start(ctx: EngineCtx) -> anyhow::Result<()> {
    let initial = config(&ctx).unwrap_or_else(|e| {
        ctx.hub.log("error", TARGET, format!("{e}; using defaults"));
        RecordingConfig::default()
    });
    let state = Arc::new(Mutex::new(State { config: initial, session: String::new(), show_dir: None, requested: None, owned: false, error: None }));
    {
        let (ctx, state) = (ctx.clone(), state.clone());
        let hub = ctx.hub.clone();
        hub.register_query(
            "recording",
            Arc::new(move |name, _args| {
                let (ctx, state) = (ctx.clone(), state.clone());
                Box::pin(async move {
                    match name.as_str() {
                        "recording.prepare" => {
                            let result = prepare(&ctx, &state);
                            state.lock().error = result.as_ref().err().cloned();
                            result.map(|dir| Value::Str(dir.display().to_string()))
                        }
                        "recording.status" => Ok(status(&ctx, &state).await),
                        _ => Err(format!("unknown recording query {name}")),
                    }
                })
            }),
        );
    }
    tokio::spawn(async move {
        let mut cfg_rx = ctx.config.clone();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                r = cfg_rx.changed() => {
                    if r.is_err() { break; }
                    match config(&ctx) {
                        Ok(cfg) => state.lock().config = cfg,
                        Err(e) => ctx.hub.log("error", TARGET, format!("{e}; keeping previous [recording] settings")),
                    }
                }
                _ = tick.tick() => {
                    let snap = ctx.hub.snapshot.load();
                    let mode = snap.str("show.mode").unwrap_or("offline").to_owned();
                    let connected = snap.bool("obs.link");
                    let active = snap.bool("obs.record.active");
                    drop(snap);
                    let current_session = ctx.hub.info.read().session.clone();
                    let mut s = state.lock();
                    if s.session != current_session {
                        s.session = current_session;
                        s.show_dir = show_dir_from_meta(&ctx.project_root.join("sessions").join(&s.session));
                        s.requested = None;
                        s.owned = false;
                        s.error = None;
                    }
                    // `modes` chooses where a recording begins; once armed it carries
                    // through BRB, ad breaks, and outro until offline.
                    let wants = wants_recording(&s.config, &mode, s.owned);
                    if active {
                        s.requested = None;
                        s.error = None;
                    } else if !connected {
                        s.requested = None;
                    } else if wants && s.requested.is_none() {
                        s.requested = Some(Instant::now());
                        s.owned = true;
                        ctx.hub.command(Command::new(Origin::System, Op::Action { name: "obs.record.start".into(), args: Value::map().with("auto", true) }));
                    } else if let Some(at) = s.requested && at.elapsed() > Duration::from_secs(15) {
                        s.error = Some("OBS did not confirm a recording file within 15 seconds".into());
                        s.requested = None;
                        // Keep the show armed: retry even if the mode is now BRB.
                    }
                    if !wants && s.owned {
                        if active || s.requested.is_some() {
                            ctx.hub.command(Command::new(Origin::System, Op::Action { name: "obs.record.stop".into(), args: Value::Null }));
                        }
                        s.owned = false;
                        s.requested = None;
                    }
                }
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_folder_name_uses_recording_time_not_old_session_time() {
        let name = show_name_at("20200101-000000", "20260926-123456");
        assert_eq!(name, "2026-09-26 12-34 (20200101-000000)");
    }

    #[test]
    fn on_air_recording_carries_through_breaks_until_offline() {
        let mut cfg = RecordingConfig::default();
        assert!(!wants_recording(&cfg, "brb", false));
        assert!(wants_recording(&cfg, "preshow", false));
        for mode in ["live", "brb", "ad_break", "outro"] {
            assert!(wants_recording(&cfg, mode, true), "recording stopped during {mode}");
        }
        assert!(!wants_recording(&cfg, "offline", true));
        cfg.auto = false;
        assert!(!wants_recording(&cfg, "live", true));
    }

    #[test]
    fn project_snapshot_is_at_start_and_excludes_secrets_and_recordings() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("project.toml"), "title = \"original\"\npassword = \"private\"\nauth = { credential = \"danger\" }\n").unwrap();
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("assets/a.toml"), "not copied").unwrap();
        std::fs::write(root.join("access_token.toml"), "not copied").unwrap();
        std::fs::write(root.join("settings.json"), "{\"token\":\"secret-value\"}").unwrap();
        std::fs::write(root.join("layout.json"), "{\"canvas\":\"wide\"}").unwrap();
        let show = root.join("shows/one");
        std::fs::create_dir_all(&show).unwrap();
        std::fs::create_dir_all(show.join("data/.project.partial")).unwrap();
        std::fs::write(show.join("data/.project.partial/stale.toml"), "old snapshot").unwrap();
        snapshot_project(root, &show).unwrap();
        std::fs::write(root.join("project.toml"), "title = \"new\"\n").unwrap();
        snapshot_project(root, &show).unwrap();
        let text = std::fs::read_to_string(show.join("data/project/project.toml")).unwrap();
        assert!(text.contains("title = \"original\""));
        assert!(!text.contains("private"));
        assert!(!text.contains("danger"));
        assert!(!show.join("data/project/settings.json").exists());
        assert!(show.join("data/project/layout.json").is_file());
        assert!(!show.join("data/project/assets/a.toml").exists());
        assert!(!show.join("data/project/access_token.toml").exists());
        assert!(!show.join("data/project/shows/one").exists());
        assert!(!show.join("data/project/stale.toml").exists());
        assert!(!show.join("data/.project.partial").exists());
    }

    #[test]
    fn preparing_session_creates_single_show_folder() {
        let d = tempfile::tempdir().unwrap();
        let cfg = RecordingConfig { dir: d.path().join("recordings").to_string_lossy().into_owned(), ..RecordingConfig::default() };
        let (hub, _rx) = se_hub::Hub::new(Arc::new(se_clock::Clock::new()));
        hub.info.write().session = "20260926-123456".into();
        let (_tx, config_rx) = tokio::sync::watch::channel(Arc::new(se_core::Config::default()));
        let ctx = EngineCtx {
            hub,
            db: se_store::Db::memory().unwrap(),
            project_root: d.path().to_path_buf(),
            data_dir: d.path().to_path_buf(),
            share_dir: d.path().to_path_buf(),
            config: config_rx,
            http: "127.0.0.1:0".parse().unwrap(),
            dev: true,
        };
        std::fs::write(d.path().join("project.toml"), "title = \"before\"\n").unwrap();
        let state = Mutex::new(State { config: cfg, session: String::new(), show_dir: None, requested: None, owned: false, error: None });
        let path = prepare(&ctx, &state).unwrap();
        assert!(path.is_dir());
        assert!(path.join("data/project/project.toml").is_file());
        assert!(path.to_string_lossy().ends_with("(20260926-123456)"));
        assert_eq!(prepare(&ctx, &state).unwrap(), path);
    }
}
