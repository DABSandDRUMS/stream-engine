//! Raw Linux hidraw access: discovery through sysfs, reads/writes, and feature reports via
//! `HIDIOCSFEATURE` / `HIDIOCGFEATURE`.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// A hidraw node as seen in sysfs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HidInfo {
    pub devnode: PathBuf,
    pub vid: u16,
    pub pid: u16,
    pub name: String,
    /// `HID_UNIQ` (USB serial) when the kernel knows it.
    pub uniq: String,
    /// `HID_PHYS` (`usb-0000:0a:00.0-9.4/input0`): the USB port path.
    pub phys: String,
}

fn parse_uevent(s: &str) -> Option<(u16, u16, String, String, String)> {
    let mut id = None;
    let (mut name, mut uniq, mut phys) = (String::new(), String::new(), String::new());
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("HID_ID=") {
            // bus:vendor:product, 4/8/8 hex digits
            let parts: Vec<&str> = v.split(':').collect();
            if parts.len() == 3 {
                let vid = u32::from_str_radix(parts[1], 16).ok()? as u16;
                let pid = u32::from_str_radix(parts[2], 16).ok()? as u16;
                id = Some((vid, pid));
            }
        } else if let Some(v) = line.strip_prefix("HID_NAME=") {
            name = v.to_string();
        } else if let Some(v) = line.strip_prefix("HID_UNIQ=") {
            uniq = v.to_string();
        } else if let Some(v) = line.strip_prefix("HID_PHYS=") {
            phys = v.to_string();
        }
    }
    let (vid, pid) = id?;
    Some((vid, pid, name, uniq, phys))
}

/// All hidraw devices (from `/sys/class/hidraw`).
pub fn scan() -> Vec<HidInfo> {
    scan_in(Path::new("/sys/class/hidraw"), Path::new("/dev"))
}

pub fn scan_in(sys: &Path, dev: &Path) -> Vec<HidInfo> {
    let Ok(rd) = std::fs::read_dir(sys) else { return Vec::new() };
    let mut v: Vec<HidInfo> = rd
        .flatten()
        .filter_map(|e| {
            let node = e.file_name().to_string_lossy().to_string();
            let ue = std::fs::read_to_string(e.path().join("device/uevent")).ok()?;
            let (vid, pid, name, uniq, phys) = parse_uevent(&ue)?;
            Some(HidInfo { devnode: dev.join(&node), vid, pid, name, uniq, phys })
        })
        .collect();
    v.sort_by(|a, b| a.devnode.cmp(&b.devnode));
    v
}

/// USB port part of `HID_PHYS` (`usb-0000:0a:00.0-9.4/input0` → `usb-0000:0a:00.0-9.4`).
pub fn usb_port(phys: &str) -> &str {
    phys.split('/').next().unwrap_or(phys)
}

pub struct HidDev {
    file: File,
    pub path: PathBuf,
}

const fn ioc(dir: u64, ty: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | (ty << 8) | nr
}
const IOC_RW: u64 = 3;
fn hidiocsfeature(len: usize) -> u64 {
    ioc(IOC_RW, b'H' as u64, 0x06, len as u64)
}
fn hidiocgfeature(len: usize) -> u64 {
    ioc(IOC_RW, b'H' as u64, 0x07, len as u64)
}

impl HidDev {
    pub fn open(path: &Path) -> io::Result<HidDev> {
        let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)?;
        Ok(HidDev { file, path: path.to_path_buf() })
    }

    /// Wait up to `timeout_ms` for input. Errors when the device went away.
    pub fn wait_readable(&self, timeout_ms: i32) -> io::Result<bool> {
        let mut pfd = libc::pollfd { fd: self.file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted { Ok(false) } else { Err(e) };
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "device removed"));
        }
        Ok(r > 0 && pfd.revents & libc::POLLIN != 0)
    }

    /// Non-blocking read of one input report; `Ok(None)` when none is pending.
    pub fn read_report(&mut self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        match self.file.read(buf) {
            Ok(0) => Err(io::Error::new(io::ErrorKind::NotConnected, "device removed")),
            Ok(n) => Ok(Some(n)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Write one output report (first byte = report id).
    pub fn write_report(&mut self, data: &[u8]) -> io::Result<()> {
        let mut tries = 0;
        loop {
            match self.file.write(data) {
                Ok(n) if n == data.len() => return Ok(()),
                Ok(n) => return Err(io::Error::other(format!("short HID write ({n} of {})", data.len()))),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && tries < 50 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn set_feature(&self, data: &[u8]) -> io::Result<()> {
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), hidiocsfeature(data.len()) as _, data.as_ptr()) };
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    pub fn get_feature(&self, report_id: u8, len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        buf[0] = report_id;
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), hidiocgfeature(len) as _, buf.as_mut_ptr()) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(r as usize);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uevent_and_sysfs_scan() {
        let dir = tempfile::tempdir().unwrap();
        let sys = dir.path().join("sys");
        let d = sys.join("hidraw8/device");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("uevent"),
            "DRIVER=hid-generic\nHID_ID=0003:00000FD9:0000006D\nHID_NAME=Elgato Stream Deck\nHID_PHYS=usb-0000:0a:00.0-9.4/input0\nHID_UNIQ=AL46J2C62768\n",
        )
        .unwrap();
        let v = scan_in(&sys, Path::new("/dev"));
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].vid, v[0].pid), (0x0FD9, 0x006D));
        assert_eq!(v[0].devnode, PathBuf::from("/dev/hidraw8"));
        assert_eq!(v[0].uniq, "AL46J2C62768");
        assert_eq!(usb_port(&v[0].phys), "usb-0000:0a:00.0-9.4");
    }

    #[test]
    fn ioctl_numbers_match_linux_headers() {
        // from <linux/hidraw.h>: HIDIOCSFEATURE(32) = 0xC0204806, HIDIOCGFEATURE(32) = 0xC0204807
        assert_eq!(hidiocsfeature(32), 0xC020_4806);
        assert_eq!(hidiocgfeature(32), 0xC020_4807);
    }
}
