//! Shared-memory canvas buffers for the shm fallback path (`drm_fourcc == 0`).
//!
//! The engine writes tightly packed RGBA8 rows into a sealed memfd ([`ShmBuffer`]);
//! clients map the received fd read-only ([`ShmView`]).

use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::ptr::{self, NonNull};
use std::slice;

use crate::sys;

fn map(fd: BorrowedFd<'_>, len: usize, prot: libc::c_int) -> io::Result<NonNull<u8>> {
    // SAFETY: mapping a shared file range; failure is reported via MAP_FAILED.
    let p = unsafe { libc::mmap(ptr::null_mut(), len, prot, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    NonNull::new(p.cast()).ok_or_else(|| io::Error::other("mmap returned null"))
}

/// A writable memfd-backed canvas buffer: `memfd_create("se-canvas")`, sized to
/// `stride * height`, mapped shared, sealed against shrinking and growing.
pub struct ShmBuffer {
    fd: OwnedFd,
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is owned by this value and only written through `&mut self`.
unsafe impl Send for ShmBuffer {}
// SAFETY: `&self` only gives read access to the mapping.
unsafe impl Sync for ShmBuffer {}

impl ShmBuffer {
    pub fn new(stride: u32, height: u32) -> io::Result<ShmBuffer> {
        let len = (stride as usize)
            .checked_mul(height as usize)
            .filter(|&l| l > 0 && l <= isize::MAX as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid shm buffer size {stride} x {height}")))?;
        let fd = sys::memfd_create(c"se-canvas", libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)?;
        // SAFETY: plain syscall on our memfd.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let ptr = map(fd.as_fd(), len, libc::PROT_READ | libc::PROT_WRITE)?;
        let buf = ShmBuffer { fd, ptr, len };
        // SAFETY: plain fcntl on our memfd.
        if unsafe { libc::fcntl(buf.fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK | libc::F_SEAL_GROW) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(buf)
    }

    /// The memfd (pass a dup — [`ShmBuffer::try_clone_fd`] — to `RingDesc::fds`).
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// A new CLOEXEC descriptor for the same memfd.
    pub fn try_clone_fd(&self) -> io::Result<OwnedFd> {
        self.fd.try_clone()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Always `false`: buffers are at least one byte.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr/len describe our live mapping.
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr/len describe our live writable mapping; `&mut self` is exclusive.
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl fmt::Debug for ShmBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShmBuffer").field("fd", &self.fd).field("len", &self.len).finish()
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        // SAFETY: unmapping our own mapping exactly once.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

/// A read-only mapping of a received shm canvas buffer.
///
/// Mapping requires `F_SEAL_SHRINK` on the memfd and at least `len` bytes, so reads can
/// never fault with SIGBUS. The producer may write other buffers of the ring at any time
/// but never one the client holds (protocol rule).
pub struct ShmView {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: read-only mapping owned by this value.
unsafe impl Send for ShmView {}
// SAFETY: read-only mapping owned by this value.
unsafe impl Sync for ShmView {}

impl ShmView {
    pub fn map(fd: BorrowedFd<'_>, len: usize) -> io::Result<ShmView> {
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "cannot map an empty shm buffer"));
        }
        let seals = sys::seals(fd)?;
        if seals & libc::F_SEAL_SHRINK == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "shm buffer memfd is not sealed against shrinking"));
        }
        let size = sys::fd_size(fd)?;
        if size < len as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("shm buffer is {size} bytes, need {len}")));
        }
        let ptr = map(fd, len, libc::PROT_READ)?;
        Ok(ShmView { ptr, len })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Always `false`: views are at least one byte.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr/len describe our live mapping; the file cannot shrink (sealed).
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl fmt::Debug for ShmView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShmView").field("len", &self.len).finish()
    }
}

impl Drop for ShmView {
    fn drop(&mut self) {
        // SAFETY: unmapping our own mapping exactly once.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_visible_through_a_view_and_size_is_sealed() {
        let mut buf = ShmBuffer::new(64, 3).unwrap();
        assert_eq!(buf.len(), 192);
        buf.as_mut_slice()[100] = 0x5a;
        let dup = buf.try_clone_fd().unwrap();
        let view = ShmView::map(dup.as_fd(), 192).unwrap();
        assert_eq!(view.as_slice()[100], 0x5a);
        buf.as_mut_slice()[191] = 7;
        assert_eq!(view.as_slice()[191], 7);

        // SAFETY: plain syscalls on our memfd.
        unsafe {
            assert!(libc::ftruncate(dup.as_raw_fd(), 10) < 0, "shrink is sealed");
            assert!(libc::ftruncate(dup.as_raw_fd(), 4096) < 0, "grow is sealed");
        }
        assert!(ShmView::map(dup.as_fd(), 193).is_err(), "view larger than file");
        assert!(ShmBuffer::new(0, 10).is_err());
    }

    #[test]
    fn unsealed_memfd_is_rejected() {
        let fd = sys::memfd_create(c"plain", libc::MFD_CLOEXEC).unwrap();
        // SAFETY: plain syscall.
        assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), 4096) }, 0);
        let err = ShmView::map(fd.as_fd(), 4096).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
