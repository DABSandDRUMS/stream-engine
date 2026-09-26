//! Stream Deck HID protocol (Elgato "expanded" family: Original V2 / MK.2 / 15-key module /
//! XL): input key reports, chunked JPEG key images, and feature reports.
//! Reference: docs.elgato.com/streamdeck/hid (general reference + Stream Deck Classic).

pub const VID: u16 = 0x0FD9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Model {
    pub pid: u16,
    pub name: &'static str,
    pub keys: u8,
    pub cols: u8,
    /// Square key image size in pixels.
    pub key_px: u32,
}

/// Models speaking this protocol (all need 180°-rotated JPEG key images).
pub const MODELS: &[Model] = &[
    Model { pid: 0x006D, name: "Stream Deck Original V2", keys: 15, cols: 5, key_px: 72 },
    Model { pid: 0x0080, name: "Stream Deck MK.2", keys: 15, cols: 5, key_px: 72 },
    Model { pid: 0x00A5, name: "Stream Deck MK.2 (Scissor Keys)", keys: 15, cols: 5, key_px: 72 },
    Model { pid: 0x00B9, name: "Stream Deck 15-Key Module", keys: 15, cols: 5, key_px: 72 },
    Model { pid: 0x006C, name: "Stream Deck XL", keys: 32, cols: 8, key_px: 96 },
    Model { pid: 0x008F, name: "Stream Deck XL V2", keys: 32, cols: 8, key_px: 96 },
];

pub fn model(pid: u16) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.pid == pid)
}

pub const IMAGE_REPORT_LEN: usize = 1024;
const IMAGE_HEADER: usize = 8;
pub const FEATURE_LEN: usize = 32;

/// Key states from an input report (`01 00 <len u16> <state per key>`).
pub fn parse_keys(report: &[u8], keys: usize) -> Option<Vec<bool>> {
    if report.len() < 4 || report[0] != 0x01 || report[1] != 0x00 {
        return None;
    }
    let n = u16::from_le_bytes([report[2], report[3]]) as usize;
    let n = n.min(keys);
    let states = report.get(4..4 + n)?;
    let mut v: Vec<bool> = states.iter().map(|b| *b != 0).collect();
    v.resize(keys, false);
    Some(v)
}

/// Split a JPEG into output reports for one key (each exactly 1024 bytes, zero padded).
pub fn image_reports(key: u8, jpeg: &[u8]) -> Vec<Vec<u8>> {
    let chunk = IMAGE_REPORT_LEN - IMAGE_HEADER;
    let mut out = Vec::with_capacity(jpeg.len().div_ceil(chunk).max(1));
    let mut page: u16 = 0;
    let mut rest = jpeg;
    loop {
        let n = rest.len().min(chunk);
        let last = n == rest.len();
        let mut r = vec![0u8; IMAGE_REPORT_LEN];
        r[0] = 0x02;
        r[1] = 0x07;
        r[2] = key;
        r[3] = last as u8;
        r[4..6].copy_from_slice(&(n as u16).to_le_bytes());
        r[6..8].copy_from_slice(&page.to_le_bytes());
        r[IMAGE_HEADER..IMAGE_HEADER + n].copy_from_slice(&rest[..n]);
        out.push(r);
        rest = &rest[n..];
        page += 1;
        if last {
            break;
        }
    }
    out
}

pub fn brightness_report(percent: u8) -> [u8; FEATURE_LEN] {
    let mut r = [0u8; FEATURE_LEN];
    r[0] = 0x03;
    r[1] = 0x08;
    r[2] = percent.min(100);
    r
}

/// Show the boot logo (clears all key images).
pub fn show_logo_report() -> [u8; FEATURE_LEN] {
    let mut r = [0u8; FEATURE_LEN];
    r[0] = 0x03;
    r[1] = 0x02;
    r
}

/// Disable the device's own sleep timer (the engine controls brightness).
pub fn sleep_disable_report() -> [u8; FEATURE_LEN] {
    let mut r = [0u8; FEATURE_LEN];
    r[0] = 0x03;
    r[1] = 0x0D;
    r
}

pub const REPORT_SERIAL: u8 = 0x06;
pub const REPORT_FIRMWARE: u8 = 0x05;

/// Serial number from a `06` getter feature report.
pub fn parse_serial(buf: &[u8]) -> Option<String> {
    if buf.len() < 3 || buf[0] != REPORT_SERIAL {
        return None;
    }
    let n = (buf[1] as usize).min(buf.len() - 2);
    let s: String = buf[2..2 + n].iter().take_while(|b| **b != 0).map(|b| *b as char).collect();
    (!s.is_empty()).then_some(s)
}

/// Firmware version from a `05` getter feature report (8 ASCII bytes at offset 6).
pub fn parse_firmware(buf: &[u8]) -> Option<String> {
    if buf.len() < 14 || buf[0] != REPORT_FIRMWARE {
        return None;
    }
    let s: String = buf[6..14].iter().take_while(|b| **b != 0).map(|b| *b as char).collect();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_report_parses_states() {
        let mut r = vec![0u8; 512];
        r[0] = 1;
        r[2] = 15;
        r[4 + 3] = 1;
        r[4 + 14] = 1;
        let k = parse_keys(&r, 15).unwrap();
        assert!(k[3] && k[14] && !k[0]);
        assert!(parse_keys(&[0x02, 0, 15, 0], 15).is_none());
    }

    #[test]
    fn image_chunks_have_headers_and_padding() {
        let jpeg: Vec<u8> = (0..2100u32).map(|i| (i % 251) as u8).collect();
        let r = image_reports(7, &jpeg);
        assert_eq!(r.len(), 3);
        assert!(r.iter().all(|p| p.len() == 1024 && p[0] == 2 && p[1] == 7 && p[2] == 7));
        assert_eq!((r[0][3], u16::from_le_bytes([r[0][4], r[0][5]]), u16::from_le_bytes([r[0][6], r[0][7]])), (0, 1016, 0));
        assert_eq!((r[2][3], u16::from_le_bytes([r[2][4], r[2][5]]), u16::from_le_bytes([r[2][6], r[2][7]])), (1, 68, 2));
        let joined: Vec<u8> = r.iter().flat_map(|p| p[8..8 + u16::from_le_bytes([p[4], p[5]]) as usize].to_vec()).collect();
        assert_eq!(joined, jpeg);
        // an image of exactly one chunk is a single final report
        assert_eq!(image_reports(0, &vec![1u8; 1016]).len(), 1);
    }

    #[test]
    fn feature_reports() {
        assert_eq!(&brightness_report(150)[..3], &[3, 8, 100]);
        let mut s = [0u8; 32];
        s[0] = 6;
        s[1] = 12;
        s[2..14].copy_from_slice(b"AL46J2C62768");
        assert_eq!(parse_serial(&s).as_deref(), Some("AL46J2C62768"));
        let mut f = [0u8; 32];
        f[0] = 5;
        f[1] = 12;
        f[6..14].copy_from_slice(b"1.01.000");
        assert_eq!(parse_firmware(&f).as_deref(), Some("1.01.000"));
    }
}
