//! Finding, spawning, and talking to the `stream-engine-web` host process.

use crate::media::Media;
use crate::protocol::{self, FromHost, ToHost};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, oneshot};

pub const HOST_BIN: &str = "stream-engine-web";
pub const INSTALL_HINT: &str = "CEF runtime not installed: run scripts/install-cef.sh";

/// An installed host: the binary and the directory holding libcef.so and its resources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPaths {
    pub exe: PathBuf,
    pub runtime_dir: PathBuf,
}

fn candidate(exe: PathBuf) -> Option<HostPaths> {
    let dir = exe.parent()?.to_path_buf();
    (exe.is_file() && dir.join("libcef.so").is_file()).then_some(HostPaths { exe, runtime_dir: dir })
}

/// Host lookup: `$STREAM_ENGINE_WEB_HOST`, next to the running executable (dev builds),
/// `<share>/../../lib/stream-engine/` (packages), `~/.local/share/stream-engine/cef/bin/`
/// (`scripts/install-cef.sh`). A candidate counts only with libcef.so beside it.
pub fn find_host(share_dir: &Path) -> Option<HostPaths> {
    let mut c: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("STREAM_ENGINE_WEB_HOST") {
        c.push(PathBuf::from(p));
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        c.push(dir.join(HOST_BIN));
    }
    c.push(share_dir.join("../../lib/stream-engine").join(HOST_BIN));
    if let Some(home) = std::env::var_os("HOME") {
        c.push(PathBuf::from(home).join(".local/share/stream-engine/cef/bin").join(HOST_BIN));
    }
    c.into_iter().find_map(candidate)
}

/// Per-run options passed on the host's command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostArgs {
    pub profile: PathBuf,
    pub log_file: PathBuf,
    pub gpu: bool,
    pub devtools_port: Option<u16>,
}

/// Messages from host processes to the supervisor.
#[derive(Debug)]
pub enum Event {
    Msg { session: u64, msg: FromHost },
    Exited { session: u64, status: String },
    LoginExited { status: String },
}

/// Browser id → sink, for the frames and audio of one host run.
pub type Routes = Arc<Mutex<HashMap<u32, Arc<Mutex<Media>>>>>;

/// One running host process.
pub struct Session {
    pub id: u64,
    pub pid: u32,
    sock: Arc<OwnedFd>,
    kill: Option<oneshot::Sender<()>>,
    pub routes: Routes,
}

impl Session {
    /// Send a command without blocking the async runtime (a full queue means the host is
    /// wedged; the caller restarts it).
    pub fn send(&self, msg: &ToHost) -> io::Result<()> {
        protocol::send(self.sock.as_fd(), msg, None, true)
    }

    pub fn kill(&mut self) {
        if let Some(k) = self.kill.take() {
            let _ = k.send(());
        }
    }
}

impl Drop for Session {
    /// Hang up: the host sees EOF and exits cleanly, and the IPC thread (which shares the
    /// socket) wakes up and ends.
    fn drop(&mut self) {
        // SAFETY: shutdown(2) on a socket we still own (the fd stays open until the last Arc).
        unsafe { libc::shutdown(self.sock.as_raw_fd(), libc::SHUT_RDWR) };
    }
}
fn describe(status: io::Result<std::process::ExitStatus>) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status {
        Ok(s) => match (s.code(), s.signal()) {
            (Some(c), _) => format!("exit code {c}"),
            (None, Some(sig)) => format!("killed by signal {sig}"),
            _ => "exited".into(),
        },
        Err(e) => format!("wait failed: {e}"),
    }
}

fn command(paths: &HostPaths) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(&paths.exe);
    let mut lib = paths.runtime_dir.clone().into_os_string();
    if let Some(old) = std::env::var_os("LD_LIBRARY_PATH").filter(|o| !o.is_empty()) {
        lib.push(":");
        lib.push(old);
    }
    cmd.env("LD_LIBRARY_PATH", lib).current_dir(&paths.runtime_dir).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    cmd
}

/// Forward the host's stderr to tracing (Chromium's own logging goes to the log file).
fn forward_stderr(stderr: Option<tokio::process::ChildStderr>) {
    let Some(stderr) = stderr else { return };
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if l.starts_with("stream-engine-web") {
                tracing::info!(target: "se_web::host", "{l}");
            } else {
                tracing::debug!(target: "se_web::host", "{l}");
            }
        }
    });
}

/// Keep the log from growing without bound across many runs.
fn rotate_log(path: &Path) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 16 << 20) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
}

/// Spawn the off-screen host with its control socket mapped to standard input.
pub fn spawn(paths: &HostPaths, args: &HostArgs, session: u64, events: mpsc::UnboundedSender<Event>) -> io::Result<Session> {
    let (ours, theirs) = protocol::socketpair()?;
    protocol::set_send_buffer(ours.as_fd(), 1 << 20);
    if let Some(dir) = args.log_file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    rotate_log(&args.log_file);
    let mut cmd = command(paths);
    cmd.stdin(Stdio::from(theirs))
        .arg(format!("--se-ipc-fd={}", protocol::HOST_IPC_FD))
        .arg(format!("--se-profile={}", args.profile.display()))
        .arg(format!("--se-log-file={}", args.log_file.display()));
    if !args.gpu {
        cmd.arg("--se-no-gpu");
    }
    if let Some(p) = args.devtools_port {
        cmd.arg(format!("--se-devtools-port={p}"));
    }
    let mut child = cmd.spawn()?;
    let pid = child.id().unwrap_or(0);
    forward_stderr(child.stderr.take());
    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    {
        let events = events.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                s = child.wait() => s,
                _ = kill_rx => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let _ = events.send(Event::Exited { session, status: describe(status) });
        });
    }
    let sock = Arc::new(ours);
    let routes: Routes = Arc::default();
    {
        let (sock, routes) = (sock.clone(), routes.clone());
        std::thread::Builder::new().name("se-web-ipc".into()).spawn(move || reader(session, sock, routes, events))?;
    }
    Ok(Session { id: session, pid, sock, kill: Some(kill_tx), routes })
}

/// IPC thread for one host run: media messages are handled here (frames straight into the hub
/// slots), everything else goes to the supervisor.
fn reader(session: u64, sock: Arc<OwnedFd>, routes: Routes, events: mpsc::UnboundedSender<Event>) {
    let mut buf = Vec::new();
    let media = |id: u32| routes.lock().get(&id).cloned();
    loop {
        let (msg, fd) = match protocol::recv::<FromHost>(sock.as_fd(), &mut buf) {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let _ = events.send(Event::Msg { session, msg: FromHost::Log { level: "error".into(), msg: format!("bad message from CEF host: {e}") } });
                continue;
            }
            Err(e) => {
                tracing::warn!("CEF host socket: {e}");
                break;
            }
        };
        let problem = match msg {
            FromHost::Frame { id, surface, slot, paint_ns } => {
                if let Some(m) = media(id) {
                    m.lock().frame(surface, slot, paint_ns);
                }
                // Always hand the slot back, even for a stale surface or a closed source.
                let _ = protocol::send(sock.as_fd(), &ToHost::FrameDone { id, surface, slot }, None, true);
                None
            }
            FromHost::Audio { id } => {
                if let Some(m) = media(id) {
                    m.lock().drain_audio();
                }
                None
            }
            FromHost::Surface { id, surface, width, height, stride, slots } => {
                media(id).and_then(|m| m.lock().surface(surface, width, height, stride, slots, fd).err().map(|e| format!("browser {id}: {e}")))
            }
            FromHost::AudioRing { id, channels, rate, capacity } => {
                media(id).and_then(|m| m.lock().audio_ring(channels, rate, capacity, fd).err().map(|e| format!("browser {id}: {e}")))
            }
            other => {
                if events.send(Event::Msg { session, msg: other }).is_err() {
                    break;
                }
                None
            }
        };
        if let Some(p) = problem {
            let _ = events.send(Event::Msg { session, msg: FromHost::Log { level: "error".into(), msg: p } });
        }
    }
}

/// Spawn the windowed sign-in browser on the shared profile.
pub fn spawn_login(paths: &HostPaths, profile: &Path, url: &str, events: mpsc::UnboundedSender<Event>) -> io::Result<u32> {
    let mut cmd = command(paths);
    cmd.arg(format!("--se-login={url}")).arg(format!("--se-profile={}", profile.display()));
    let mut child = cmd.spawn()?;
    let pid = child.id().unwrap_or(0);
    forward_stderr(child.stderr.take());
    tokio::spawn(async move {
        let status = child.wait().await;
        let _ = events.send(Event::LoginExited { status: describe(status) });
    });
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_counts_only_with_libcef_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(HOST_BIN);
        std::fs::write(&exe, b"").unwrap();
        assert_eq!(candidate(exe.clone()), None, "binary without the CEF runtime");
        std::fs::write(dir.path().join("libcef.so"), b"").unwrap();
        assert_eq!(candidate(exe.clone()), Some(HostPaths { exe, runtime_dir: dir.path().to_path_buf() }));
        assert_eq!(candidate(dir.path().join("missing")), None);
    }
}
