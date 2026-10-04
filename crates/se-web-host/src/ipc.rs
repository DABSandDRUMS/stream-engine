//! The control socket to the engine: a reader thread that dispatches commands to the UI
//! thread, and senders used from CEF callbacks.

use crate::{osr, task};
use se_web::protocol::{self, FromHost, ToHost};
use std::io::{self, Write};
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::OnceLock;
use std::time::Duration;

pub struct Ipc {
    sock: OwnedFd,
}

static IPC: OnceLock<Ipc> = OnceLock::new();

/// Adopt the inherited control socket (`--se-ipc-fd`).
pub fn adopt(fd: i32) -> io::Result<()> {
    // SAFETY: fstat only inspects the descriptor number handed to us by the engine.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 || st.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("fd {fd} is not the engine socket")));
    }
    // SAFETY: the engine passed this descriptor for our exclusive use; keep it out of the CEF
    // child processes.
    let sock = unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        OwnedFd::from_raw_fd(fd)
    };
    protocol::set_send_buffer(sock.as_fd(), 4 << 20);
    IPC.set(Ipc { sock }).map_err(|_| io::Error::other("control socket adopted twice"))
}

fn sock() -> Option<BorrowedFd<'static>> {
    IPC.get().map(|i| i.sock.as_fd())
}

/// Send a message that must arrive (status, surfaces). Blocks if the engine is slow.
pub fn send(msg: &FromHost, fd: Option<BorrowedFd<'_>>) -> bool {
    let Some(s) = sock() else { return false };
    match protocol::send(s, msg, fd, false) {
        Ok(()) => true,
        Err(e) => {
            let _ = writeln!(io::stderr(), "stream-engine-web: send failed: {e}");
            false
        }
    }
}

/// Send a high-rate notification (frames, audio); dropped instead of blocking the UI thread.
pub fn notify(msg: &FromHost) -> bool {
    sock().is_some_and(|s| protocol::send(s, msg, None, true).is_ok())
}

/// Log to the engine (or stderr in sign-in mode).
pub fn log(level: &str, msg: String) {
    if sock().is_none() || !send(&FromHost::Log { level: level.into(), msg: msg.clone() }, None) {
        let _ = writeln!(io::stderr(), "stream-engine-web [{level}] {msg}");
    }
}

/// Read engine commands until the socket closes, then shut the host down.
pub fn spawn_reader() -> io::Result<()> {
    let sock = sock().ok_or_else(|| io::Error::other("no control socket"))?;
    std::thread::Builder::new().name("se-web-ipc".into()).spawn(move || {
        let mut buf = Vec::new();
        loop {
            match protocol::recv::<ToHost>(sock, &mut buf) {
                Ok(Some((msg, _))) => dispatch(msg),
                Ok(None) => break,
                Err(e) if e.kind() == io::ErrorKind::InvalidData => log("error", format!("bad command from engine: {e}")),
                Err(e) => {
                    let _ = writeln!(io::stderr(), "stream-engine-web: control socket: {e}");
                    break;
                }
            }
        }
        // The engine is gone (exited, crashed, or asked us to stop): close everything.
        begin_shutdown();
    })?;
    Ok(())
}

fn dispatch(msg: ToHost) {
    match msg {
        // Frame acknowledgements free a slot without a trip through the UI thread.
        ToHost::FrameDone { id, surface, slot } => osr::frame_done(id, surface, slot),
        ToHost::Shutdown => begin_shutdown(),
        other => {
            if !task::on_ui(move || osr::command(other)) {
                let _ = writeln!(io::stderr(), "stream-engine-web: UI thread gone; dropping command");
            }
        }
    }
}

fn begin_shutdown() {
    task::on_ui(osr::shutdown);
    // CEF normally exits within a second; never outlive the engine by more than a few.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(8));
        let _ = writeln!(io::stderr(), "stream-engine-web: shutdown timed out; exiting");
        // SAFETY: immediate process exit without running destructors (CEF may be wedged).
        unsafe { libc::_exit(0) };
    });
}
