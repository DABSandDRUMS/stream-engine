//! Read-only mappings of the engine's shm-fallback memfds.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::ptr::NonNull;

/// A `MAP_SHARED`, `PROT_READ` mapping of one memfd buffer; unmapped on drop.
pub(crate) struct ShmMap {
    ptr: NonNull<u8>,
    len: usize,
    offset: usize,
}

impl ShmMap {
    /// Maps `offset + stride * height` bytes of `fd` after checking the file is that large (so
    /// reads can never run past the end of the object).
    pub fn new(fd: BorrowedFd<'_>, offset: usize, stride: usize, height: usize) -> io::Result<Self> {
        let too_big = || io::Error::new(io::ErrorKind::InvalidInput, "shm buffer size overflows");
        let len = stride.checked_mul(height).and_then(|n| n.checked_add(offset)).ok_or_else(too_big)?;
        // SAFETY: stat is plain old data; fstat fills it for a valid fd.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &raw mut st) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if (st.st_size as u64) < len as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("shm buffer holds {} bytes, frame needs {len}", st.st_size)));
        }
        // SAFETY: maps a fresh read-only shared region; the result is checked below.
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(ptr.cast::<u8>()).ok_or_else(|| io::Error::other("mmap returned null"))?;
        Ok(Self { ptr, len, offset })
    }

    /// The frame rows (`stride * height` bytes starting at the plane offset).
    pub fn rows(&self) -> &[u8] {
        // SAFETY: the mapping is `len` bytes long, readable and lives as long as self.
        let all = unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) };
        &all[self.offset..]
    }
}

// SAFETY: the mapping is read-only and owned by this value; sharing or moving it across threads
// is no different from sharing a `&[u8]` / `Box<[u8]>`.
unsafe impl Send for ShmMap {}
unsafe impl Sync for ShmMap {}

impl Drop for ShmMap {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly the region mapped in `new`.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::testutil::memfd_with;
    use std::os::fd::AsFd;

    #[test]
    fn maps_rows_after_offset() {
        let data: Vec<u8> = (0..64u8).collect();
        let fd = memfd_with("se-shm-map", &data);
        let map = ShmMap::new(fd.as_fd(), 16, 8, 6).unwrap();
        assert_eq!(map.rows(), &data[16..64]);
    }

    #[test]
    fn rejects_buffers_smaller_than_the_frame() {
        let fd = memfd_with("se-shm-small", &[0u8; 63]);
        let err = ShmMap::new(fd.as_fd(), 16, 8, 6).err().expect("63 bytes cannot hold 16 + 8*6");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(ShmMap::new(fd.as_fd(), 0, usize::MAX, 2).is_err());
    }
}
