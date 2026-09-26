//! ENTTEC DMX USB PRO (Widget API v1.44): message framing, reply parsing and the serial device.
//!
//! Every message is `SOM (0x7E), label, data length LSB, data length MSB, data, EOM (0xE7)`.
//! Widget behaviour worth knowing:
//! - Label 6 (Output Only Send DMX) makes the widget transmit that universe continuously until
//!   the next label 6. Any request other than labels 6 and 3 stops periodic output and turns the
//!   port to input; the next label 6 resumes output.
//! - The widget answers label 3 (Get Widget Parameters) with firmware version, break and
//!   mark-after-break times (units of 10.67 µs) and the output rate. Firmware major version 1 is
//!   the DMX firmware (no RDM), 2 the RDM firmware, 3 the RDM sniffer.
//! - With RDM firmware, label 7 sends an RDM request and label 11 a DISC_UNIQUE_BRANCH; replies
//!   arrive as label 5 (Received DMX: status byte, then the data from the start code). Newer RDM
//!   firmware also reports "no response" with label 12. The widget has no reply timeout of its
//!   own for DUB, so the host times out.

use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPort, StopBits};

use super::rdm::{self, Dub, RdmTransport, Response};

/// Start of message delimiter.
pub const SOM: u8 = 0x7E;
/// End of message delimiter.
pub const EOM: u8 = 0xE7;

/// Message labels (API v1.44 plus the RDM firmware extensions).
pub mod label {
    pub const GET_PARAMS: u8 = 3;
    pub const SET_PARAMS: u8 = 4;
    pub const RECV_DMX: u8 = 5;
    pub const SEND_DMX: u8 = 6;
    pub const SEND_RDM: u8 = 7;
    pub const RECV_ON_CHANGE: u8 = 8;
    pub const GET_SERIAL: u8 = 10;
    pub const SEND_RDM_DISCOVERY: u8 = 11;
    /// Sent by RDM firmware when an RDM request or DUB got no response.
    pub const RDM_TIMEOUT: u8 = 12;
}

/// Fewest channels a label-6 frame may carry.
pub const MIN_CHANNELS: usize = 24;
/// Most channels a label-6 frame may carry.
pub const MAX_CHANNELS: usize = 512;
/// Largest data length the widget accepts or sends.
pub const MAX_PAYLOAD: usize = 600;
/// Firmware major version of the RDM firmware.
pub const FIRMWARE_RDM: u8 = 2;

/// Framing bytes around the data: SOM, label, length (2), EOM.
const OVERHEAD: usize = 5;
/// Break / mark-after-break time unit reported by label 3.
const TIME_UNIT_US: f32 = 10.67;
/// Serial read/write timeout.
const PORT_TIMEOUT: Duration = Duration::from_millis(50);
/// Reply timeout for label 3 / label 10.
const INFO_TIMEOUT: Duration = Duration::from_millis(500);
/// Host-side wait for a DUB reply (the widget has no timeout of its own).
const DUB_TIMEOUT: Duration = Duration::from_millis(60);
/// Host-side wait for an RDM response (2.8 ms on the wire plus USB latency-timer delays).
const RDM_TIMEOUT: Duration = Duration::from_millis(100);
/// After a broadcast request: swallow a label-12 "no response" the firmware may still send, so
/// it cannot be mistaken for the reply to the next request.
const BROADCAST_SETTLE: Duration = Duration::from_millis(30);

/// Appends one framed message (SOM, label, length LSB, length MSB, payload, EOM). Non-RT helper.
///
/// # Panics
/// If `payload` exceeds [`MAX_PAYLOAD`] bytes.
pub fn frame(out: &mut Vec<u8>, label: u8, payload: &[u8]) {
    assert!(payload.len() <= MAX_PAYLOAD, "USB PRO payload is {} bytes, the maximum is {MAX_PAYLOAD}", payload.len());
    let [lsb, msb] = (payload.len() as u16).to_le_bytes();
    out.reserve(payload.len() + OVERHEAD);
    out.extend_from_slice(&[SOM, label, lsb, msb]);
    out.extend_from_slice(payload);
    out.push(EOM);
}

/// A pre-built label-6 (Output Only Send DMX) message: start code 0 followed by `channels`
/// values. [`DmxFrame::set`] rewrites the values in place.
#[derive(Clone, Debug)]
pub struct DmxFrame {
    buf: Box<[u8]>,
}

impl DmxFrame {
    /// `channels` is clamped to [`MIN_CHANNELS`]..=[`MAX_CHANNELS`]; all values start at 0.
    pub fn new(channels: usize) -> DmxFrame {
        let channels = channels.clamp(MIN_CHANNELS, MAX_CHANNELS);
        let mut buf = vec![0u8; channels + 1 + OVERHEAD].into_boxed_slice();
        let [lsb, msb] = ((channels + 1) as u16).to_le_bytes();
        buf[..4].copy_from_slice(&[SOM, label::SEND_DMX, lsb, msb]);
        // buf[4] = DMX start code 0
        buf[channels + 5] = EOM;
        DmxFrame { buf }
    }

    /// Number of channel values carried.
    pub fn channels(&self) -> usize {
        self.buf.len() - 1 - OVERHEAD
    }

    /// Copies channel values (channel 1 first); longer input is truncated, shorter input
    /// zero-fills the remaining channels. No allocation.
    pub fn set(&mut self, data: &[u8]) {
        let n = self.channels();
        let values = &mut self.buf[5..5 + n];
        let k = data.len().min(n);
        values[..k].copy_from_slice(&data[..k]);
        values[k..].fill(0);
    }

    /// The complete framed message.
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }
}

/// Incremental parser for widget messages. Feed raw serial bytes with [`Decoder::push`] and
/// take complete messages as `(label, payload)` from the iterator. Garbage before a SOM, a
/// length beyond [`MAX_PAYLOAD`] or a missing EOM discards one byte and resynchronises on the
/// next SOM. Iteration yields `None` while a message is incomplete and resumes after more bytes
/// are pushed.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Drops everything buffered.
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

impl Iterator for Decoder {
    type Item = (u8, Vec<u8>);

    fn next(&mut self) -> Option<(u8, Vec<u8>)> {
        loop {
            let Some(start) = self.buf.iter().position(|&b| b == SOM) else {
                self.buf.clear();
                return None;
            };
            self.buf.drain(..start);
            if self.buf.len() < 4 {
                return None;
            }
            let len = usize::from(u16::from_le_bytes([self.buf[2], self.buf[3]]));
            if len > MAX_PAYLOAD {
                self.buf.drain(..1);
                continue;
            }
            if self.buf.len() < len + OVERHEAD {
                return None;
            }
            if self.buf[len + 4] != EOM {
                self.buf.drain(..1);
                continue;
            }
            let label = self.buf[1];
            let payload = self.buf[4..4 + len].to_vec();
            self.buf.drain(..len + OVERHEAD);
            return Some((label, payload));
        }
    }
}

/// Label-3 reply: firmware version and output timing.
#[derive(Clone, Debug, serde::Serialize)]
pub struct WidgetParams {
    /// `major << 8 | minor`.
    pub firmware: u16,
    /// 1 = DMX firmware, 2 = RDM firmware, 3 = RDM sniffer.
    pub firmware_major: u8,
    pub break_us: f32,
    pub mab_us: f32,
    /// Output rate in packets per second (0 = as fast as possible).
    pub rate: u8,
}

impl WidgetParams {
    /// "major.minor", e.g. "1.44".
    pub fn version(&self) -> String {
        format!("{}.{}", self.firmware_major, self.firmware & 0xFF)
    }

    /// Firmware family from the major version.
    pub fn kind(&self) -> &'static str {
        match self.firmware_major {
            1 => "DMX firmware",
            2 => "RDM firmware",
            3 => "RDM sniffer firmware",
            _ => "unknown firmware",
        }
    }
}

/// Parses a label-3 reply payload (firmware LSB, firmware MSB, break, MAB, rate, user config…).
pub fn parse_params(payload: &[u8]) -> Option<WidgetParams> {
    let &[fw_lsb, fw_msb, brk, mab, rate, ..] = payload else {
        return None;
    };
    Some(WidgetParams {
        firmware: u16::from_le_bytes([fw_lsb, fw_msb]),
        firmware_major: fw_msb,
        break_us: f32::from(brk) * TIME_UNIT_US,
        mab_us: f32::from(mab) * TIME_UNIT_US,
        rate,
    })
}

/// Parses a label-10 reply: 4 BCD bytes, least significant first. `None` for an unprogrammed
/// serial (`FF FF FF FF`), invalid BCD or a short payload.
pub fn parse_serial(payload: &[u8]) -> Option<u32> {
    let bytes = payload.get(..4)?;
    if bytes == [0xFF; 4] {
        return None;
    }
    bytes.iter().rev().try_fold(0u32, |acc, &b| {
        let (hi, lo) = (u32::from(b >> 4), u32::from(b & 0x0F));
        (hi <= 9 && lo <= 9).then_some(acc * 100 + hi * 10 + lo)
    })
}

/// An open ENTTEC DMX USB PRO.
pub struct UsbPro {
    port: Box<dyn SerialPort>,
    path: String,
    decoder: Decoder,
    params: Option<WidgetParams>,
    /// Scratch buffer for framing requests (control path only).
    tx: Vec<u8>,
}

impl UsbPro {
    /// Opens the widget's serial device: raw 8N2, no flow control, 50 ms timeout, exclusive,
    /// with pending input and output flushed. The baud rate is irrelevant to the FTDI VCP.
    pub fn open(path: &str) -> io::Result<UsbPro> {
        let context = |e: serialport::Error| {
            let description = format!("{path}: {}", e.description);
            io::Error::new(io::Error::from(e).kind(), description)
        };
        let port = serialport::new(path, 115_200)
            .data_bits(DataBits::Eight)
            .parity(Parity::None)
            .stop_bits(StopBits::Two)
            .flow_control(FlowControl::None)
            .timeout(PORT_TIMEOUT)
            .open()
            .map_err(context)?;
        port.clear(ClearBuffer::All).map_err(context)?;
        Ok(UsbPro { port, path: path.to_string(), decoder: Decoder::new(), params: None, tx: Vec::with_capacity(64) })
    }

    /// The first `/dev/serial/by-id/*ENTTEC*DMX_USB_PRO*` entry (sorted by name), canonicalised
    /// to its device node.
    pub fn find_port() -> Option<String> {
        let mut candidates: Vec<_> = std::fs::read_dir(Path::new("/dev/serial/by-id"))
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains("ENTTEC") && n.contains("DMX_USB_PRO")))
            .collect();
        candidates.sort();
        candidates.into_iter().find_map(|p| std::fs::canonicalize(p).ok()).map(|p| p.to_string_lossy().into_owned())
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Parameters from the last successful [`UsbPro::get_params`].
    pub fn params(&self) -> Option<&WidgetParams> {
        self.params.as_ref()
    }

    /// Label 3 with user configuration size 0; waits up to 500 ms for the reply.
    pub fn get_params(&mut self) -> io::Result<WidgetParams> {
        self.drain();
        self.send(label::GET_PARAMS, &[0, 0])?;
        let params = self.wait(INFO_TIMEOUT, |l, p| if l == label::GET_PARAMS { parse_params(&p) } else { None })?;
        let params = params.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{}: no reply to Get Widget Parameters within {} ms (not a DMX USB PRO?)", self.path, INFO_TIMEOUT.as_millis()),
            )
        })?;
        self.params = Some(params.clone());
        Ok(params)
    }

    /// Label 10: the widget's serial number, `None` if unprogrammed. Stops DMX output until the
    /// next [`UsbPro::send_dmx`].
    pub fn get_serial(&mut self) -> io::Result<Option<u32>> {
        self.drain();
        self.send(label::GET_SERIAL, &[])?;
        let serial = self.wait(INFO_TIMEOUT, |l, p| (l == label::GET_SERIAL && p.len() >= 4).then(|| parse_serial(&p)))?;
        serial.ok_or_else(|| {
            io::Error::new(io::ErrorKind::TimedOut, format!("{}: no reply to Get Widget Serial Number within {} ms", self.path, INFO_TIMEOUT.as_millis()))
        })
    }

    /// Label 4: set the widget's output rate (packets/s, 0 = as fast as the universe size
    /// allows), keeping its current break and MAB times. The widget stores this across power
    /// cycles; re-reads the parameters to confirm.
    pub fn set_rate(&mut self, rate: u8) -> io::Result<WidgetParams> {
        let cur = match self.params.clone() {
            Some(p) => p,
            None => self.get_params()?,
        };
        if rate > 40 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "widget rate must be 0 (fastest) or 1..40 packets/s"));
        }
        let brk = (cur.break_us / TIME_UNIT_US).round().clamp(9.0, 127.0) as u8;
        let mab = (cur.mab_us / TIME_UNIT_US).round().clamp(1.0, 127.0) as u8;
        self.send(label::SET_PARAMS, &[0, 0, brk, mab, rate])?;
        self.get_params()
    }

    /// Writes a label-6 frame with a single `write_all`; no allocation.
    pub fn send_dmx(&mut self, f: &DmxFrame) -> io::Result<()> {
        self.port.write_all(f.bytes())
    }

    /// Discards unsolicited input (e.g. label-5 frames while the port is in input mode) without
    /// blocking, and resets the reply parser.
    pub fn drain(&mut self) {
        let mut buf = [0u8; 1024];
        // Bounded: a widget streaming label 5 frames must not keep us here.
        for _ in 0..16 {
            let pending = match self.port.bytes_to_read() {
                Ok(n) if n > 0 => (n as usize).min(buf.len()),
                _ => break,
            };
            if self.port.read(&mut buf[..pending]).is_err() {
                break;
            }
        }
        self.decoder.clear();
    }

    fn send(&mut self, label: u8, payload: &[u8]) -> io::Result<()> {
        self.tx.clear();
        frame(&mut self.tx, label, payload);
        self.port.write_all(&self.tx)
    }

    /// Reads until `accept` returns a value for a received message or `timeout` passes.
    fn wait<T>(&mut self, timeout: Duration, mut accept: impl FnMut(u8, Vec<u8>) -> Option<T>) -> io::Result<Option<T>> {
        let result = self.wait_inner(timeout, &mut accept);
        self.port.set_timeout(PORT_TIMEOUT)?;
        result
    }

    fn wait_inner<T>(&mut self, timeout: Duration, accept: &mut impl FnMut(u8, Vec<u8>) -> Option<T>) -> io::Result<Option<T>> {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 1024];
        loop {
            for (label, payload) in self.decoder.by_ref() {
                if let Some(v) = accept(label, payload) {
                    return Ok(Some(v));
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            self.port.set_timeout((deadline - now).min(PORT_TIMEOUT))?;
            match self.port.read(&mut buf) {
                Ok(n) => self.decoder.push(&buf[..n]),
                Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Fetches parameters if needed and refuses RDM unless the widget runs RDM firmware.
    fn ensure_rdm(&mut self) -> io::Result<()> {
        let params = match &self.params {
            Some(p) => p.clone(),
            None => self.get_params()?,
        };
        if params.firmware_major == FIRMWARE_RDM {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{}: DMX USB PRO runs firmware {} ({}); RDM needs the ENTTEC RDM firmware (major version {FIRMWARE_RDM})",
                self.path,
                params.version(),
                params.kind()
            ),
        ))
    }
}

/// RDM over the widget. Every call stops DMX output (the port turns to input); the caller
/// resumes it with the next [`UsbPro::send_dmx`]. Fails with [`io::ErrorKind::Unsupported`]
/// unless the widget runs RDM firmware (major version 2).
impl RdmTransport for UsbPro {
    fn dub(&mut self, packet: &[u8]) -> io::Result<Dub> {
        self.ensure_rdm()?;
        self.drain();
        self.send(label::SEND_RDM_DISCOVERY, packet)?;
        let reply = self.wait(DUB_TIMEOUT, |l, p| match l {
            label::RECV_DMX => Some(p.get(1..).map_or(Dub::None, rdm::decode_dub)),
            label::RDM_TIMEOUT => Some(Dub::None),
            _ => None,
        })?;
        Ok(reply.unwrap_or(Dub::None))
    }

    fn request(&mut self, packet: &[u8]) -> io::Result<Option<Response>> {
        let Some(dest) = packet.get(3..9) else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "RDM packet shorter than its header"));
        };
        let broadcast = rdm::Uid::from_bytes(dest).is_broadcast();
        self.ensure_rdm()?;
        self.drain();
        self.send(label::SEND_RDM, packet)?;
        if broadcast {
            self.wait(BROADCAST_SETTLE, |l, _| (l == label::RDM_TIMEOUT).then_some(()))?;
            return Ok(None);
        }
        let reply = self.wait(RDM_TIMEOUT, |l, mut p| match l {
            label::RECV_DMX if p.get(1) == Some(&rdm::START_CODE) => {
                p.remove(0);
                Some(Some(p))
            }
            label::RDM_TIMEOUT => Some(None),
            _ => None,
        })?;
        match reply.flatten() {
            None => Ok(None),
            Some(data) => rdm::decode(&data).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::rdm::{Uid, cc, pid, response_type};
    use std::fs::File;
    use std::os::fd::{FromRawFd, OwnedFd};

    #[test]
    fn frame_appends_label_length_and_delimiters() {
        let mut out = vec![0xAB];
        frame(&mut out, label::GET_PARAMS, &[0x00, 0x00]);
        // (pre-existing byte), SOM, label 3, length LSB 2, MSB 0, user config size 0 0, EOM
        assert_eq!(out, [0xAB, 0x7E, 0x03, 0x02, 0x00, 0x00, 0x00, 0xE7]);
        let mut empty = Vec::new();
        frame(&mut empty, label::GET_SERIAL, &[]);
        assert_eq!(empty, [0x7E, 0x0A, 0x00, 0x00, 0xE7]);
    }

    #[test]
    fn label6_frame_24_channels() {
        let mut f = DmxFrame::new(24);
        f.set(&(1..=24).collect::<Vec<u8>>());
        let expected: [u8; 30] = [
            0x7E, // 0: SOM
            0x06, // 1: label 6 Output Only Send DMX
            0x19, 0x00, // 2-3: data length 25 (start code + 24 channels), LSB first
            0x00, // 4: DMX start code
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, // 5-16: ch 1-12
            0x0D, 0x0E, 0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, // 17-28: ch 13-24
            0xE7, // 29: EOM
        ];
        assert_eq!(f.bytes(), expected);
        assert_eq!(f.channels(), 24);
    }

    #[test]
    fn label6_frame_512_channels() {
        let data: Vec<u8> = (0..512).map(|i| (i % 256) as u8 ^ 0x5A).collect();
        let mut f = DmxFrame::new(512);
        f.set(&data);
        let bytes = f.bytes();
        assert_eq!(bytes.len(), 518);
        // SOM, label 6, length 513 = 0x0201 LSB first, start code 0
        assert_eq!(&bytes[..5], &[0x7E, 0x06, 0x01, 0x02, 0x00]);
        assert_eq!(&bytes[5..517], data.as_slice());
        assert_eq!(bytes[517], 0xE7);
    }

    #[test]
    fn dmx_frame_clamps_truncates_and_zero_pads() {
        assert_eq!(DmxFrame::new(3).channels(), 24);
        assert_eq!(DmxFrame::new(1000).channels(), 512);
        let mut f = DmxFrame::new(24);
        f.set(&[9; 100]);
        assert!(f.bytes()[5..29].iter().all(|&v| v == 9));
        assert_eq!(f.bytes()[29], EOM, "long input never overwrites EOM");
        f.set(&[7, 7]);
        assert_eq!(&f.bytes()[5..8], &[7, 7, 0]);
        assert!(f.bytes()[7..29].iter().all(|&v| v == 0), "short input zero-fills");
        assert_eq!(f.bytes()[4], 0, "start code stays 0");
    }

    #[test]
    fn dmx_frame_set_does_not_allocate() {
        assert!(se_alloc::installed(), "se-dmx tests run with the counting allocator");
        let mut f = DmxFrame::new(512);
        let full = [0x42u8; 512];
        let scope = se_alloc::Scope::begin();
        for i in 0..100 {
            f.set(if i % 2 == 0 { &full } else { &full[..10] });
            std::hint::black_box(f.bytes());
        }
        assert_eq!(scope.allocs(), 0);
    }

    #[test]
    fn parses_widget_params() {
        // fw 1.44 (LSB 44 = 0x2C, MSB 1), break 9 × 10.67 µs, MAB 1 × 10.67 µs, 40 packets/s
        let p = parse_params(&[0x2C, 0x01, 0x09, 0x01, 0x28]).unwrap();
        assert_eq!(p.firmware, 0x012C);
        assert_eq!(p.firmware_major, 1);
        assert!((p.break_us - 96.03).abs() < 1e-3, "{}", p.break_us);
        assert!((p.mab_us - 10.67).abs() < 1e-4, "{}", p.mab_us);
        assert_eq!(p.rate, 40);
        assert_eq!(p.version(), "1.44");
        assert_eq!(p.kind(), "DMX firmware");
        // user configuration bytes after the fixed fields are ignored
        assert_eq!(parse_params(&[0x04, 0x02, 0x09, 0x01, 0x00, 0xAA, 0xBB]).unwrap().version(), "2.4");
        assert!(parse_params(&[0x2C, 0x01, 0x09, 0x01]).is_none());
    }

    #[test]
    fn parses_bcd_serial() {
        // 405589 → BCD LSB first: 89 55 40 00
        assert_eq!(parse_serial(&[0x89, 0x55, 0x40, 0x00]), Some(405_589));
        assert_eq!(parse_serial(&[0x99, 0x99, 0x99, 0x99]), Some(99_999_999));
        assert_eq!(parse_serial(&[0x00, 0x00, 0x00, 0x00]), Some(0));
        assert_eq!(parse_serial(&[0xFF, 0xFF, 0xFF, 0xFF]), None, "unprogrammed");
        assert_eq!(parse_serial(&[0x8A, 0x55, 0x40, 0x00]), None, "invalid BCD digit");
        assert_eq!(parse_serial(&[0x89, 0x55, 0x40]), None, "short");
    }

    #[test]
    fn decoder_handles_partial_reads() {
        let reply = [0x7E, 0x03, 0x05, 0x00, 0x2C, 0x01, 0x09, 0x01, 0x28, 0xE7];
        let mut d = Decoder::new();
        for &b in &reply[..reply.len() - 1] {
            d.push(&[b]);
            assert_eq!(d.next(), None);
        }
        d.push(&reply[reply.len() - 1..]);
        assert_eq!(d.next(), Some((3, vec![0x2C, 0x01, 0x09, 0x01, 0x28])));
        assert_eq!(d.next(), None);
    }

    #[test]
    fn decoder_skips_garbage_and_resyncs() {
        let mut d = Decoder::new();
        // garbage, then a message, then two back-to-back messages in one read
        d.push(&[0x00, 0x13, 0xE7, 0x7E, 0x0A, 0x04, 0x00, 0x89, 0x55, 0x40, 0x00, 0xE7]);
        d.push(&[0x7E, 0x05, 0x01, 0x00, 0x00, 0xE7, 0x7E, 0x0C, 0x00, 0x00, 0xE7]);
        assert_eq!(d.next(), Some((10, vec![0x89, 0x55, 0x40, 0x00])));
        assert_eq!(d.next(), Some((5, vec![0x00])));
        assert_eq!(d.next(), Some((12, vec![])));
        assert_eq!(d.next(), None);

        // bad EOM: the false start is dropped and the real message behind it is found
        d.push(&[0x7E, 0x03, 0x01, 0x00, 0x55, 0x00, 0x7E, 0x0A, 0x00, 0x00, 0xE7]);
        assert_eq!(d.next(), Some((10, vec![])));

        // a length beyond 600 (0xFFFF) is not a header: resync on the next SOM
        d.push(&[0x7E, 0x0A, 0xFF, 0xFF, 0x7E, 0x06, 0x02, 0x00, 0x00, 0x01, 0xE7]);
        assert_eq!(d.next(), Some((6, vec![0x00, 0x01])));

        // a SOM-less stream is discarded entirely
        d.push(&[0x01; 2000]);
        assert_eq!(d.next(), None);
        d.push(&[0x7E, 0x03, 0x00, 0x00, 0xE7]);
        assert_eq!(d.next(), Some((3, vec![])));

        // payload bytes equal to SOM/EOM are data, not delimiters
        d.push(&[0x7E, 0x05, 0x03, 0x00, 0x7E, 0xE7, 0x7E, 0xE7]);
        assert_eq!(d.next(), Some((5, vec![0x7E, 0xE7, 0x7E])));
    }

    // ---- UsbPro against an emulated widget on a pseudo-terminal ----

    const RESPONDER: Uid = Uid(0x7A70_0000_0042);
    const CONTROLLER: Uid = Uid(0x454E_0040_5589);

    struct Pty {
        master: File,
        /// Keeps the slave side alive so reads on the master block instead of failing with EIO
        /// before the device under test opens it.
        _slave: OwnedFd,
        path: String,
    }

    fn pty() -> Pty {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty writes two fds into the provided ints; name/termios/winsize are unused.
        let rc = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), std::ptr::null()) };
        assert_eq!(rc, 0, "openpty: {}", io::Error::last_os_error());
        // SAFETY: both fds were just returned by openpty and are owned by nobody else.
        let (master, slave) = unsafe { (File::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let path = std::fs::read_link(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&slave))).unwrap();
        Pty { master, _slave: slave, path: path.to_string_lossy().into_owned() }
    }

    /// Emulated widget: answers labels 3/10 and, with RDM firmware, DUB / RDM requests for one
    /// responder. Returns every message it received.
    fn spawn_widget(master: File, fw_major: u8) -> std::thread::JoinHandle<Vec<(u8, Vec<u8>)>> {
        std::thread::spawn(move || {
            let mut tx = master.try_clone().unwrap();
            let mut rx = master;
            let mut decoder = Decoder::new();
            let mut seen = Vec::new();
            let mut muted = false;
            let mut dubs = 0;
            let mut buf = [0u8; 1024];
            loop {
                let n = match rx.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                decoder.push(&buf[..n]);
                for (l, p) in decoder.by_ref() {
                    let mut out = Vec::new();
                    // Received-DMX traffic (start code 0) ahead of replies to non-DUB requests:
                    // it must be skipped while waiting for the real reply.
                    if l != label::SEND_RDM_DISCOVERY {
                        frame(&mut out, label::RECV_DMX, &[0x00, 0x00, 0x10, 0x20]);
                    }
                    match l {
                        label::GET_PARAMS => frame(&mut out, label::GET_PARAMS, &[4, fw_major, 9, 1, 40]),
                        label::GET_SERIAL => frame(&mut out, label::GET_SERIAL, &[0x89, 0x55, 0x40, 0x00]),
                        label::SEND_RDM_DISCOVERY => {
                            let req = rdm::decode(&p).unwrap();
                            let (lo, hi) = (Uid::from_bytes(&req.data[..6]), Uid::from_bytes(&req.data[6..]));
                            dubs += 1;
                            if !muted && (lo..=hi).contains(&RESPONDER) {
                                let mut data = vec![0x00];
                                data.extend(rdm::encode_dub_response(RESPONDER, 7));
                                frame(&mut out, label::RECV_DMX, &data);
                            } else if dubs % 2 == 0 {
                                // alternate between firmware timeout reports and host timeouts
                                frame(&mut out, label::RDM_TIMEOUT, &[]);
                            }
                        }
                        label::SEND_RDM => {
                            let req = rdm::decode(&p).unwrap();
                            if req.pid == pid::DISC_UN_MUTE {
                                muted = false;
                            } else if req.dest == RESPONDER {
                                let (rcc, data) = match req.pid {
                                    pid::DISC_MUTE => {
                                        muted = true;
                                        (cc::DISCOVERY_RESPONSE, vec![0, 0])
                                    }
                                    pid::DEVICE_INFO => (cc::GET_RESPONSE, vec![1, 0, 0, 7, 1, 1, 0, 0, 0, 3, 0, 3, 1, 1, 0, 1, 0, 0, 0]),
                                    pid::MANUFACTURER_LABEL => (cc::GET_RESPONSE, b"Test Co".to_vec()),
                                    _ => (cc::GET_RESPONSE, Vec::new()),
                                };
                                let resp = rdm::encode(req.src, RESPONDER, req.tn, response_type::ACK, 0, rcc, req.pid, &data);
                                let mut payload = vec![0x00];
                                payload.extend(resp);
                                frame(&mut out, label::RECV_DMX, &payload);
                            } else {
                                frame(&mut out, label::RDM_TIMEOUT, &[]);
                            }
                        }
                        _ => {}
                    }
                    if tx.write_all(&out).is_err() {
                        return seen;
                    }
                    seen.push((l, p));
                }
            }
            seen
        })
    }

    #[test]
    fn usbpro_params_serial_and_dmx_over_pty() {
        let pty = pty();
        let widget = spawn_widget(pty.master.try_clone().unwrap(), 1);
        let mut dev = UsbPro::open(&pty.path).unwrap();
        assert_eq!(dev.path(), pty.path);
        let params = dev.get_params().unwrap();
        assert_eq!((params.version().as_str(), params.rate), ("1.4", 40));
        assert_eq!(dev.get_serial().unwrap(), Some(405_589));
        let mut f = DmxFrame::new(24);
        f.set(&[1, 2, 3]);
        dev.send_dmx(&f).unwrap();

        // DMX firmware: RDM is refused without touching the wire
        let err = rdm::discover(&mut dev, CONTROLLER).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(err.to_string().contains("firmware 1.4 (DMX firmware)"), "{err}");

        drop(dev);
        drop(pty);
        let seen = widget.join().unwrap();
        let labels: Vec<u8> = seen.iter().map(|(l, _)| *l).collect();
        assert_eq!(labels, [label::GET_PARAMS, label::GET_SERIAL, label::SEND_DMX]);
        assert_eq!(seen[0].1, [0, 0], "label 3 carries user config size 0");
        assert_eq!(seen[2].1, &f.bytes()[4..f.bytes().len() - 1]);
    }

    #[test]
    fn usbpro_rdm_discovery_and_device_info_over_pty() {
        let pty = pty();
        let widget = spawn_widget(pty.master.try_clone().unwrap(), FIRMWARE_RDM);
        let mut dev = UsbPro::open(&pty.path).unwrap();
        let found = rdm::discover(&mut dev, CONTROLLER).unwrap();
        assert_eq!(found, [RESPONDER]);
        let info = rdm::device_info(&mut dev, CONTROLLER, RESPONDER).unwrap().unwrap();
        assert_eq!((info.model_id, info.footprint, info.start_address), (7, 3, 1));
        assert_eq!(info.manufacturer.as_deref(), Some("Test Co"));
        assert_eq!(info.model, None);
        assert!(rdm::device_info(&mut dev, CONTROLLER, Uid(0x7A70_0000_0043)).unwrap().is_none(), "label 12 = no response");

        drop(dev);
        drop(pty);
        let seen = widget.join().unwrap();
        let labels: Vec<u8> = seen.iter().map(|(l, _)| *l).collect();
        // params once (cached), un-mute, DUB → mute, DUB ×3 (empty), then GETs
        assert_eq!(&labels[..7], &[3, 7, 11, 7, 11, 11, 11]);
        assert!(labels[7..].iter().all(|&l| l == label::SEND_RDM));
    }
}
