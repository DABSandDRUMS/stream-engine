//! Thin libc wrappers: `SOCK_SEQPACKET` sockets with `SCM_RIGHTS`, eventfd, memfd, poll.
//!
//! Every fd created here is `O_CLOEXEC`. Send/receive calls never block
//! (`MSG_DONTWAIT`); callers wait with [`poll`] / [`wait_readable`].

use std::ffi::CStr;
use std::fs;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::path::Path;
use std::ptr;
use std::time::{Duration, Instant};

/// Most fds carried by one message (the protocol needs at most 4).
pub const MAX_FDS: usize = 8;

/// Control buffer for [`MAX_FDS`] fds: `CMSG_SPACE(8 * 4)` = 48 on 64-bit Linux.
const CMSG_BUF_LEN: usize = 64;

#[repr(C, align(8))]
struct CmsgBuf([u8; CMSG_BUF_LEN]);

/// Result of a non-blocking [`send_msg`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The whole record (and its fds) was queued.
    Sent,
    /// The socket buffer is full; nothing was sent.
    WouldBlock,
}

/// Result of a non-blocking [`recv_msg`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvOutcome {
    /// One record of `len` bytes; `truncated` when the record or its fds did not fit.
    Message { len: usize, truncated: bool },
    /// No record queued.
    WouldBlock,
    /// The peer closed the connection.
    Closed,
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

/// # Safety
/// `fd` must be a freshly created descriptor owned by nobody else.
unsafe fn owned(fd: libc::c_int) -> OwnedFd {
    // SAFETY: guaranteed by the caller.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn sockaddr(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: sockaddr_un is plain old data; all-zero is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unix socket path {} is empty, contains NUL or exceeds {} bytes", path.display(), addr.sun_path.len() - 1),
        ));
    }
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    let len = mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    Ok((addr, len as libc::socklen_t))
}

fn seqpacket_socket(nonblocking: bool) -> io::Result<OwnedFd> {
    let mut ty = libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC;
    if nonblocking {
        ty |= libc::SOCK_NONBLOCK;
    }
    // SAFETY: plain syscall; the result is a new fd we own.
    let fd = cvt(unsafe { libc::socket(libc::AF_UNIX, ty, 0) })?;
    // SAFETY: fresh fd.
    Ok(unsafe { owned(fd) })
}

/// Connects a blocking `SOCK_SEQPACKET` socket to `path`.
pub fn connect_seqpacket(path: &Path) -> io::Result<OwnedFd> {
    let fd = seqpacket_socket(false)?;
    let (addr, len) = sockaddr(path)?;
    // SAFETY: addr/len describe a valid sockaddr_un.
    cvt(unsafe { libc::connect(fd.as_raw_fd(), (&raw const addr).cast::<libc::sockaddr>(), len) })?;
    Ok(fd)
}

/// Creates the socket's directory (missing components get mode 0700) and binds a
/// non-blocking listening `SOCK_SEQPACKET` socket at `path` (mode 0600).
///
/// A stale socket file (nobody listening) is replaced; a live server at `path` or a
/// non-socket file yields an error.
pub fn bind_seqpacket(path: &Path) -> io::Result<OwnedFd> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        match fs::metadata(dir) {
            Ok(meta) if meta.is_dir() => {
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    tracing::warn!(
                        dir = %dir.display(),
                        mode = format_args!("{mode:o}"),
                        "frames socket directory is accessible by other users"
                    );
                }
            }
            Ok(_) => {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} exists and is not a directory", dir.display())));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            }
            Err(e) => return Err(e),
        }
    }

    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => match connect_seqpacket(path) {
            Ok(_) => {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, format!("another frames server is listening on {}", path.display())));
            }
            Err(e) if e.raw_os_error() == Some(libc::ECONNREFUSED) || e.kind() == io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "removing stale frames socket");
                match fs::remove_file(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            Err(e) => {
                return Err(io::Error::new(e.kind(), format!("probing existing socket {}: {e}", path.display())));
            }
        },
        Ok(_) => {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} exists and is not a socket", path.display())));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    let fd = seqpacket_socket(true)?;
    let (addr, len) = sockaddr(path)?;
    // SAFETY: addr/len describe a valid sockaddr_un.
    cvt(unsafe { libc::bind(fd.as_raw_fd(), (&raw const addr).cast::<libc::sockaddr>(), len) })?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    // SAFETY: plain syscall on our socket.
    cvt(unsafe { libc::listen(fd.as_raw_fd(), 64) })?;
    Ok(fd)
}

/// Accepts one pending connection as a non-blocking CLOEXEC socket; `None` when no
/// connection is pending.
pub fn accept(listener: BorrowedFd<'_>) -> io::Result<Option<OwnedFd>> {
    loop {
        // SAFETY: null address pointers are allowed; the result is a new fd we own.
        let fd = unsafe { libc::accept4(listener.as_raw_fd(), ptr::null_mut(), ptr::null_mut(), libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) };
        if fd >= 0 {
            // SAFETY: fresh fd.
            return Ok(Some(unsafe { owned(fd) }));
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) | Some(libc::ECONNABORTED) => continue,
            Some(libc::EAGAIN) => return Ok(None),
            _ => return Err(err),
        }
    }
}

/// Peer credentials (`SO_PEERCRED`) of a connected unix socket.
pub fn peer_cred(fd: BorrowedFd<'_>) -> io::Result<libc::ucred> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: cred/len describe a writable ucred.
    cvt(unsafe { libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len) })?;
    Ok(cred)
}

/// Sets `SO_SNDBUF` (the kernel doubles the value).
pub fn set_send_buffer(fd: BorrowedFd<'_>, bytes: u32) -> io::Result<()> {
    let v = bytes as libc::c_int;
    // SAFETY: v is a readable c_int.
    cvt(unsafe {
        libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, (&raw const v).cast(), mem::size_of::<libc::c_int>() as libc::socklen_t)
    })?;
    Ok(())
}

/// Bytes this socket sent that the peer has not consumed yet (`SIOCOUTQ`).
pub fn unread_bytes(fd: BorrowedFd<'_>) -> io::Result<usize> {
    let mut v: libc::c_int = 0;
    // SAFETY: SIOCOUTQ (== TIOCOUTQ) writes one c_int.
    cvt(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCOUTQ, &mut v) })?;
    Ok(v.max(0) as usize)
}

/// Sends one record with `fds` as `SCM_RIGHTS`, never blocking and never raising SIGPIPE.
pub fn send_msg(fd: BorrowedFd<'_>, bytes: &[u8], fds: &[RawFd]) -> io::Result<SendOutcome> {
    if fds.len() > MAX_FDS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} fds exceed the per-message limit {MAX_FDS}", fds.len())));
    }
    let mut iov = libc::iovec { iov_base: bytes.as_ptr().cast_mut().cast(), iov_len: bytes.len() };
    let mut cbuf = CmsgBuf([0; CMSG_BUF_LEN]);
    // SAFETY: msghdr is plain old data; all-zero is valid.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        let payload = mem::size_of_val(fds) as libc::c_uint;
        // SAFETY: pure arithmetic.
        let space = unsafe { libc::CMSG_SPACE(payload) } as usize;
        debug_assert!(space <= CMSG_BUF_LEN);
        msg.msg_control = cbuf.0.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: the control buffer is aligned, zeroed and large enough for one cmsghdr
        // plus `payload` bytes, so CMSG_FIRSTHDR is non-null and CMSG_DATA in bounds.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(payload) as _;
            ptr::copy_nonoverlapping(fds.as_ptr().cast::<u8>(), libc::CMSG_DATA(cmsg), payload as usize);
        }
    }
    loop {
        // SAFETY: msg points to live iov/control buffers.
        let n = unsafe { libc::sendmsg(fd.as_raw_fd(), &msg, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT) };
        if n >= 0 {
            if n as usize != bytes.len() {
                return Err(io::Error::new(io::ErrorKind::WriteZero, format!("short seqpacket send: {n} of {} bytes", bytes.len())));
            }
            return Ok(SendOutcome::Sent);
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return Ok(SendOutcome::WouldBlock),
            _ => return Err(err),
        }
    }
}

/// Receives one record without blocking (`MSG_CMSG_CLOEXEC`). Received fds are appended
/// to `fds_out`; with `None` every received fd is closed immediately.
pub fn recv_msg(fd: BorrowedFd<'_>, buf: &mut [u8], mut fds_out: Option<&mut Vec<OwnedFd>>) -> io::Result<RecvOutcome> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut cbuf = CmsgBuf([0; CMSG_BUF_LEN]);
    // SAFETY: msghdr is plain old data; all-zero is valid.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.0.as_mut_ptr().cast();
    msg.msg_controllen = CMSG_BUF_LEN as _;
    let n = loop {
        // SAFETY: msg points to live iov/control buffers.
        let n = unsafe { libc::recvmsg(fd.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT) };
        if n >= 0 {
            break n as usize;
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return Ok(RecvOutcome::WouldBlock),
            _ => return Err(err),
        }
    };

    // Take ownership of every received fd first so none can leak.
    // SAFETY: the kernel filled msg_control/msg_controllen; the CMSG_* macros walk it
    // within bounds. SCM_RIGHTS payloads are arrays of fds now installed in our table.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg);
                let bytes = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for i in 0..bytes / mem::size_of::<RawFd>() {
                    let raw = ptr::read_unaligned(data.cast::<RawFd>().add(i));
                    let fd = owned(raw);
                    if let Some(out) = fds_out.as_deref_mut() {
                        out.push(fd);
                    }
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    if n == 0 {
        return Ok(RecvOutcome::Closed);
    }
    let truncated = msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0;
    Ok(RecvOutcome::Message { len: n, truncated })
}

fn timeout_ms(timeout: Option<Duration>) -> libc::c_int {
    match timeout {
        None => -1,
        Some(t) => {
            // Round up so a 0.5 ms timeout does not become a busy 0.
            let ms = t.as_nanos().div_ceil(1_000_000);
            ms.min(libc::c_int::MAX as u128) as libc::c_int
        }
    }
}

/// `poll(2)`; `None` waits forever. Returns the number of ready fds; `EINTR` returns 0.
pub fn poll(fds: &mut [libc::pollfd], timeout: Option<Duration>) -> io::Result<usize> {
    // SAFETY: fds is a valid mutable slice of pollfd.
    let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms(timeout)) };
    if r < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            return Ok(0);
        }
        return Err(err);
    }
    Ok(r as usize)
}

/// Waits until `fd` reports any of `events`; `false` on timeout. POLLERR/POLLHUP count
/// as ready so callers notice dead peers.
pub fn wait_fd(fd: BorrowedFd<'_>, events: libc::c_short, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now().checked_add(timeout);
    loop {
        let mut pfd = [libc::pollfd { fd: fd.as_raw_fd(), events, revents: 0 }];
        let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if poll(&mut pfd, left)? > 0 {
            if pfd[0].revents & libc::POLLNVAL != 0 {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
            return Ok(true);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(false);
        }
    }
}

/// Waits until `fd` is readable; `false` on timeout.
pub fn wait_readable(fd: BorrowedFd<'_>, timeout: Duration) -> io::Result<bool> {
    wait_fd(fd, libc::POLLIN, timeout)
}

/// A non-blocking CLOEXEC eventfd with counter 0.
pub fn eventfd() -> io::Result<OwnedFd> {
    // SAFETY: plain syscall; the result is a new fd we own.
    let fd = cvt(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) })?;
    // SAFETY: fresh fd.
    Ok(unsafe { owned(fd) })
}

/// Adds 1 to an eventfd counter. Never blocks (a saturated counter is already readable).
pub fn eventfd_signal(fd: BorrowedFd<'_>) -> io::Result<()> {
    let one: u64 = 1;
    loop {
        // SAFETY: writes 8 bytes from a live u64.
        let n = unsafe { libc::write(fd.as_raw_fd(), (&raw const one).cast(), 8) };
        if n == 8 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return Ok(()),
            _ => return Err(err),
        }
    }
}

/// Reads and resets an eventfd counter; 0 when it was not signalled.
pub fn eventfd_drain(fd: BorrowedFd<'_>) -> io::Result<u64> {
    let mut v: u64 = 0;
    loop {
        // SAFETY: reads 8 bytes into a live u64.
        let n = unsafe { libc::read(fd.as_raw_fd(), (&raw mut v).cast(), 8) };
        if n == 8 {
            return Ok(v);
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return Ok(0),
            _ => return Err(err),
        }
    }
}

/// `memfd_create(name, flags)`.
pub fn memfd_create(name: &CStr, flags: libc::c_uint) -> io::Result<OwnedFd> {
    // SAFETY: name is NUL-terminated; the result is a new fd we own.
    let fd = cvt(unsafe { libc::memfd_create(name.as_ptr(), flags) })?;
    // SAFETY: fresh fd.
    Ok(unsafe { owned(fd) })
}

/// Size of a memfd/dmabuf via `lseek(fd, 0, SEEK_END)` (dmabufs report 0 in `fstat`).
pub fn fd_size(fd: BorrowedFd<'_>) -> io::Result<u64> {
    // SAFETY: plain syscall on a borrowed fd.
    let r = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(r as u64)
}

/// `fstat(2)`.
pub fn fstat(fd: BorrowedFd<'_>) -> io::Result<libc::stat> {
    // SAFETY: stat is plain old data; fstat fills it.
    let mut st: libc::stat = unsafe { mem::zeroed() };
    // SAFETY: st is writable.
    cvt(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
    Ok(st)
}

/// Seals of a memfd (`F_GET_SEALS`).
pub fn seals(fd: BorrowedFd<'_>) -> io::Result<libc::c_int> {
    // SAFETY: plain fcntl on a borrowed fd.
    cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: fds is writable; results are fresh fds.
        let r = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) };
        assert_eq!(r, 0);
        // SAFETY: fresh fds.
        unsafe { (owned(fds[0]), owned(fds[1])) }
    }

    #[test]
    fn fds_travel_and_extra_fds_close() {
        let (a, b) = pair();
        let memfd = memfd_create(c"t", libc::MFD_CLOEXEC).unwrap();
        let ev = eventfd().unwrap();
        let sent = send_msg(a.as_fd(), b"hello", &[memfd.as_raw_fd(), ev.as_raw_fd()]).unwrap();
        assert_eq!(sent, SendOutcome::Sent);

        let mut buf = [0u8; 16];
        let mut fds = Vec::new();
        let r = recv_msg(b.as_fd(), &mut buf, Some(&mut fds)).unwrap();
        assert_eq!(r, RecvOutcome::Message { len: 5, truncated: false });
        assert_eq!(&buf[..5], b"hello");
        assert_eq!(fds.len(), 2);
        // The received eventfd is the same object: a signal through it is visible here.
        eventfd_signal(fds[1].as_fd()).unwrap();
        assert_eq!(eventfd_drain(ev.as_fd()).unwrap(), 1);
        // CLOEXEC was requested.
        // SAFETY: plain fcntl.
        let flags = unsafe { libc::fcntl(fds[0].as_raw_fd(), libc::F_GETFD) };
        assert!(flags & libc::FD_CLOEXEC != 0);

        // With no fd sink the fds are closed: the pipe's write end is then gone.
        let mut p = [0; 2];
        // SAFETY: p is writable.
        assert_eq!(unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: fresh fds.
        let (rd, wr) = unsafe { (owned(p[0]), owned(p[1])) };
        send_msg(a.as_fd(), b"x", &[wr.as_raw_fd()]).unwrap();
        drop(wr);
        let r = recv_msg(b.as_fd(), &mut buf, None).unwrap();
        assert!(matches!(r, RecvOutcome::Message { len: 1, .. }));
        assert!(wait_readable(rd.as_fd(), Duration::ZERO).unwrap(), "EOF on pipe");
        let mut byte = 0u8;
        // SAFETY: reads one byte.
        let n = unsafe { libc::read(rd.as_raw_fd(), (&raw mut byte).cast(), 1) };
        assert_eq!(n, 0, "all write ends closed");
    }

    #[test]
    fn would_block_truncation_and_close() {
        let (a, b) = pair();
        let mut buf = [0u8; 4];
        assert_eq!(recv_msg(b.as_fd(), &mut buf, None).unwrap(), RecvOutcome::WouldBlock);
        send_msg(a.as_fd(), b"too long", &[]).unwrap();
        assert_eq!(recv_msg(b.as_fd(), &mut buf, None).unwrap(), RecvOutcome::Message { len: 4, truncated: true });
        set_send_buffer(a.as_fd(), 4096).unwrap();
        let mut blocked = false;
        for _ in 0..100_000 {
            match send_msg(a.as_fd(), &[0u8; 64], &[]).unwrap() {
                SendOutcome::Sent => {}
                SendOutcome::WouldBlock => {
                    blocked = true;
                    break;
                }
            }
        }
        assert!(blocked, "a full socket reports WouldBlock instead of blocking");
        assert!(unread_bytes(a.as_fd()).unwrap() > 0);
        drop(a);
        let mut big = [0u8; 64];
        loop {
            match recv_msg(b.as_fd(), &mut big, None).unwrap() {
                RecvOutcome::Message { .. } => {}
                RecvOutcome::Closed => break,
                RecvOutcome::WouldBlock => panic!("closed peer must read as Closed"),
            }
        }
    }

    #[test]
    fn bind_refuses_live_server_and_replaces_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/frames.sock");
        let listener = bind_seqpacket(&path).unwrap();
        let mode = fs::metadata(path.parent().unwrap()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        let err = bind_seqpacket(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        let client = connect_seqpacket(&path).unwrap();
        // The live-server probe of the failed bind is queued first, then `client`.
        let probe = accept(listener.as_fd()).unwrap().expect("probe connection");
        let conn = accept(listener.as_fd()).unwrap().expect("pending connection");
        let cred = peer_cred(conn.as_fd()).unwrap();
        // SAFETY: plain syscalls.
        assert_eq!(cred.uid, unsafe { libc::geteuid() });
        assert_eq!(cred.pid, std::process::id() as i32);
        assert!(accept(listener.as_fd()).unwrap().is_none());
        drop((client, conn, probe));

        drop(listener); // leaves a stale socket file behind
        assert!(path.exists());
        let _again = bind_seqpacket(&path).unwrap();

        let file = dir.path().join("plain");
        fs::write(&file, b"x").unwrap();
        assert_eq!(bind_seqpacket(&file).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
    }
}
