//! Client side of `frames.sock` (used by the UI, tools and `se-frames-client`).

use std::io;
use std::mem;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::proto::{self, CanvasMsg, FrameMsg, Goodbye, HELLO_FLAG_DMABUF, Hello, MAX_MSG_SIZE, MSG_CANVAS, MSG_FRAME, MSG_GOODBYE, Release};
use crate::sys::{self, RecvOutcome, SendOutcome};

/// How long `hello`/`release` wait for room in a full socket.
const SEND_TIMEOUT: Duration = Duration::from_secs(1);

/// A message from the engine with the fds that travelled with it.
#[derive(Debug)]
pub enum ClientMsg {
    /// `se_canvas`: `fds[i]` is buffer `i` (`msg.buffer_count` fds).
    Canvas {
        msg: CanvasMsg,
        fds: Vec<OwnedFd>,
    },
    /// `se_frame`: `fence` is the sync_file fd when `msg.has_fence`.
    Frame {
        msg: FrameMsg,
        fence: Option<OwnedFd>,
    },
    Goodbye(Goodbye),
}

/// A blocking `frames.sock` client.
pub struct FramesClient {
    fd: OwnedFd,
    buf: [u8; 2 * MAX_MSG_SIZE],
    fds: Vec<OwnedFd>,
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

impl FramesClient {
    pub fn connect(path: &Path) -> io::Result<FramesClient> {
        let fd = sys::connect_seqpacket(path).map_err(|e| io::Error::new(e.kind(), format!("connecting to {}: {e}", path.display())))?;
        Ok(FramesClient { fd, buf: [0; 2 * MAX_MSG_SIZE], fds: Vec::with_capacity(proto::MAX_BUFFERS) })
    }

    /// Sends `se_hello`. `client_kind` is [`proto::CLIENT_OBS`], [`proto::CLIENT_UI`] or
    /// [`proto::CLIENT_OTHER`]; `want_mask` has bit `1 << canvas` per wanted canvas.
    /// May be sent again to change `want` or the transport.
    pub fn hello(&self, client_kind: u32, want_mask: u32, dmabuf: bool) -> io::Result<()> {
        let mut buf = [0u8; Hello::SIZE];
        let n = Hello { client: client_kind, want: want_mask, flags: if dmabuf { HELLO_FLAG_DMABUF } else { 0 } }.encode(&mut buf);
        self.send(&buf[..n])
    }

    /// Sends `se_release`: the client no longer reads `buffer` of the frame `seq`.
    pub fn release(&self, canvas: u32, buffer: u32, seq: u64) -> io::Result<()> {
        let mut buf = [0u8; Release::SIZE];
        let n = Release { canvas, buffer, seq }.encode(&mut buf);
        self.send(&buf[..n])
    }

    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        let deadline = Instant::now() + SEND_TIMEOUT;
        loop {
            match sys::send_msg(self.fd.as_fd(), bytes, &[])? {
                SendOutcome::Sent => return Ok(()),
                SendOutcome::WouldBlock => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() || !sys::wait_fd(self.fd.as_fd(), libc::POLLOUT, left)? {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "frames server is not reading its socket"));
                    }
                }
            }
        }
    }

    /// Waits up to `timeout` for the next message; `Ok(None)` on timeout. A closed
    /// connection is `UnexpectedEof`; a malformed message is `InvalidData` (its fds are
    /// closed and the connection stays usable).
    pub fn recv(&mut self, timeout: Duration) -> io::Result<Option<ClientMsg>> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            self.fds.clear();
            match sys::recv_msg(self.fd.as_fd(), &mut self.buf, Some(&mut self.fds))? {
                RecvOutcome::Message { len, truncated } => {
                    return self.parse(len, truncated).map(Some);
                }
                RecvOutcome::Closed => {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "frames server closed the connection"));
                }
                RecvOutcome::WouldBlock => {
                    let left = match deadline {
                        Some(d) => d.saturating_duration_since(Instant::now()),
                        None => Duration::MAX,
                    };
                    if left.is_zero() || !sys::wait_readable(self.fd.as_fd(), left)? {
                        return Ok(None);
                    }
                }
            }
        }
    }

    fn parse(&mut self, len: usize, truncated: bool) -> io::Result<ClientMsg> {
        if truncated {
            self.fds.clear();
            return Err(invalid("truncated message or fds from the frames server"));
        }
        let bytes = &self.buf[..len];
        let result = match proto::peek_type(bytes) {
            Ok(MSG_CANVAS) => CanvasMsg::decode(bytes).map_err(invalid_proto).and_then(|msg| {
                if msg.buffer_count == 0 || msg.buffer_count as usize != self.fds.len() {
                    Err(invalid(format!("se_canvas announces {} buffers but carries {} fds", msg.buffer_count, self.fds.len())))
                } else {
                    Ok(ClientMsg::Canvas { msg, fds: mem::take(&mut self.fds) })
                }
            }),
            Ok(MSG_FRAME) => FrameMsg::decode(bytes).map_err(invalid_proto).and_then(|msg| {
                if self.fds.len() != usize::from(msg.has_fence) {
                    Err(invalid(format!("se_frame has_fence={} but carries {} fds", u8::from(msg.has_fence), self.fds.len())))
                } else {
                    Ok(ClientMsg::Frame { msg, fence: self.fds.pop() })
                }
            }),
            Ok(MSG_GOODBYE) => Goodbye::decode(bytes)
                .map_err(invalid_proto)
                .and_then(|msg| if self.fds.is_empty() { Ok(ClientMsg::Goodbye(msg)) } else { Err(invalid("se_goodbye carries fds")) }),
            Ok(ty) => Err(invalid(format!("unexpected message type {ty}"))),
            Err(e) => Err(invalid_proto(e)),
        };
        self.fds.clear();
        result
    }
}

fn invalid_proto(e: proto::ProtoError) -> io::Error {
    invalid(format!("malformed message from the frames server: {e}"))
}

impl AsFd for FramesClient {
    /// The socket, for integrating into an external poll loop (readable = message ready).
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Waits until a sync_file fence signals (`POLLIN`); `false` on timeout.
pub fn wait_fence<F: AsFd>(fence: &F, timeout: Duration) -> io::Result<bool> {
    sys::wait_readable(fence.as_fd(), timeout)
}
