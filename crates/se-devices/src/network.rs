//! Network devices (§3.5): PreSonus StudioLive consoles (UCNET discovery) and Art-Net nodes
//! (ArtPoll → ArtPollReply), with stable identities; a device that goes quiet is removed.
//!
//! * **UCNET** (passive): consoles broadcast a `DA` announcement from UDP 53000 to
//!   255.255.255.255:47809 every 3 s; [`se_mixer::ucnet::discovery::parse_announcement`] reads
//!   it. The socket shares the port (`SO_REUSEADDR` + `SO_REUSEPORT`, like se-mixer's): Linux
//!   delivers a broadcast to every socket bound to the port, so se-mixer's own discovery and a
//!   second engine keep hearing the announcements.
//! * **Art-Net** (active): an ArtPoll every [`POLL`] from UDP 6454 to 255.255.255.255 and to each
//!   LAN interface's broadcast address; nodes answer with ArtPollReply to port 6454 (unicast or
//!   broadcast). This socket binds 6454 **exclusively**: on a shared port the kernel hands each
//!   unicast reply to just one of the sockets, so a dev engine would silently take the live
//!   engine's replies. The first program to bind keeps discovery; the others report the port as
//!   busy and retry every poll. se-dmx sends ArtDmx from an ephemeral port and is unaffected;
//!   `[devices.network] artnet = false` frees 6454 for other Art-Net software on this computer.
//! * **Identity**: `mac-<mac>` when the ArtPollReply carries the node's MAC (bytes 201..207);
//!   consoles by serial (`ucnet-<serial>`: the announcement has no MAC, and the kernel neighbour
//!   table only knows it sometimes, so it would not be stable; it is shown as `extra.mac` when
//!   known); else the node's IP (`artnet-<ip>`).
//! * A device not heard for [`TIMEOUT`] (three missed announcements/polls) is removed.
//! * Host firewalls with a default-deny input policy (ufw here) drop both protocols' inbound
//!   datagrams; [`FIREWALL_HINT`] lets them in.

use crate::identity::{DeviceInfo, Kind};
use se_mixer::ucnet::discovery as ucnet;
use std::collections::BTreeMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc, watch};

/// Art-Net's UDP port (source and destination of every Art-Net packet).
pub const ARTNET_PORT: u16 = 6454;
/// UCNET discovery announcements arrive on this port.
pub const UCNET_PORT: u16 = se_mixer::ucnet::packet::DISCOVERY_PORT;
/// How often ArtPoll goes out (Art-Net asks controllers for every 2.5–3 s).
pub const POLL: Duration = Duration::from_secs(3);
/// Not heard for this long → removed (three missed announcements/polls).
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Devices first heard this soon after start were already there: no `devices.added` event.
pub const STARTUP_QUIET: Duration = Duration::from_secs(5);
/// What the owner runs so a default-deny host firewall (ufw) lets discovery answers in.
pub const FIREWALL_HINT: &str = "sudo ufw allow in proto udp from any port 53000 to any port 47809 comment 'PreSonus UCNET discovery' && sudo ufw allow in proto udp from any port 6454 to any port 6454 comment 'Art-Net'";

const ARTNET_ID: [u8; 8] = *b"Art-Net\0";
const OP_POLL: u16 = 0x2000;
const OP_POLL_REPLY: u16 = 0x2100;
const PROTOCOL_VERSION: u16 = 14;
/// ArtPollReply up to and including LongName: the least we accept.
const REPLY_MIN: usize = 108;
/// MAC (6 bytes, high byte first); Art-Net I replies end before it.
const REPLY_MAC: usize = 201;
const REPLY_BIND_INDEX: usize = 211;

/// The fields of an ArtPollReply the registry uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtNode {
    /// The node's own address (0.0.0.0 when it doesn't say).
    pub ip: Ipv4Addr,
    pub short_name: String,
    pub long_name: String,
    /// None when the reply is too short to carry one, or it is all zeros / all ones.
    pub mac: Option<[u8; 6]>,
    /// VersInfo (high, low).
    pub firmware: u16,
    pub oem: u16,
    /// 0 = not given (Art-Net 3), 1 = root device, 2+ = further port groups of the same node.
    pub bind_index: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtError {
    NotArtNet,
    /// Another Art-Net packet (ArtPoll, ArtDmx, …).
    Opcode(u16),
    Truncated(usize),
}

/// The 14-byte ArtPoll (protocol 14, no unsolicited replies, no diagnostics).
pub fn art_poll() -> [u8; 14] {
    let mut p = [0u8; 14];
    p[..8].copy_from_slice(&ARTNET_ID);
    p[8..10].copy_from_slice(&OP_POLL.to_le_bytes());
    p[10..12].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    p
}

/// Parse an ArtPollReply (Art-Net 4 layout; shorter Art-Net I/II replies without the MAC too).
pub fn parse_poll_reply(p: &[u8]) -> Result<ArtNode, ArtError> {
    if p.len() < 10 || p[..8] != ARTNET_ID {
        return Err(ArtError::NotArtNet);
    }
    let op = u16::from_le_bytes([p[8], p[9]]);
    if op != OP_POLL_REPLY {
        return Err(ArtError::Opcode(op));
    }
    if p.len() < REPLY_MIN {
        return Err(ArtError::Truncated(p.len()));
    }
    let mac = p.get(REPLY_MAC..REPLY_MAC + 6).map(|m| [m[0], m[1], m[2], m[3], m[4], m[5]]).filter(|m| *m != [0; 6] && *m != [0xff; 6]);
    Ok(ArtNode {
        ip: Ipv4Addr::new(p[10], p[11], p[12], p[13]),
        firmware: u16::from_be_bytes([p[16], p[17]]),
        oem: u16::from_be_bytes([p[20], p[21]]),
        short_name: c_string(&p[26..44]),
        long_name: c_string(&p[44..108]),
        mac,
        bind_index: p.get(REPLY_BIND_INDEX).copied().unwrap_or(0),
    })
}

/// NUL-terminated ASCII field.
fn c_string(b: &[u8]) -> String {
    let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

/// `00:0a:92:03:3a:24`, `00-0A-92-03-3A-24` or `000a92033a24`.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let hex: Vec<u8> = s.trim().bytes().filter(|c| *c != b':' && *c != b'-').collect();
    if hex.len() != 12 || !hex.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let mut m = [0u8; 6];
    for (i, pair) in hex.chunks(2).enumerate() {
        m[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(m)
}

pub fn fmt_mac(m: [u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

fn network_device(kind: Kind, identity: String, name: String, path: String, extra: BTreeMap<String, String>) -> DeviceInfo {
    DeviceInfo {
        kind,
        syspath: format!("net:{identity}"),
        identity,
        name,
        path,
        bus: "network".into(),
        usb: None,
        serial: None,
        port: None,
        driver: kind.as_str().into(),
        card: String::new(),
        extra,
    }
}

/// Registry record for an Art-Net node; None for the extra replies a multi-port node sends
/// per port group (bind index 2+), which describe the same box.
pub fn artnet_device(n: &ArtNode, from: SocketAddr) -> Option<DeviceInfo> {
    if n.bind_index > 1 {
        return None;
    }
    let ip = match (n.ip.is_unspecified(), from.ip()) {
        (true, IpAddr::V4(src)) => src,
        _ => n.ip,
    };
    let identity = match n.mac {
        Some(m) => format!("mac-{}", fmt_mac(m)),
        None => format!("artnet-{ip}"),
    };
    let name = [&n.long_name, &n.short_name].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_else(|| format!("Art-Net node {ip}"));
    let mut extra = BTreeMap::from([
        ("ip".to_string(), ip.to_string()),
        ("firmware".to_string(), format!("{}.{}", n.firmware >> 8, n.firmware & 0xff)),
        ("oem".to_string(), format!("{:04x}", n.oem)),
    ]);
    if let Some(m) = n.mac {
        extra.insert("mac".into(), fmt_mac(m));
    }
    if !n.short_name.is_empty() && n.short_name != name {
        extra.insert("short_name".into(), n.short_name.clone());
    }
    Some(network_device(Kind::ArtNet, identity, name, format!("{ip}:{ARTNET_PORT}"), extra))
}

/// Registry record for a console announcement; `mac` from the neighbour table when known.
pub fn ucnet_device(d: &ucnet::Device, mac: Option<[u8; 6]>) -> DeviceInfo {
    let name = if d.name.is_empty() || d.name == d.model { d.model.clone() } else { format!("{} ({})", d.name, d.model) };
    let mut extra = BTreeMap::from([("ip".to_string(), d.addr.ip().to_string()), ("model".to_string(), d.model.clone())]);
    if let Some(m) = mac {
        extra.insert("mac".into(), fmt_mac(m));
    }
    let mut dev = network_device(Kind::Ucnet, format!("ucnet-{}", d.serial), name, d.addr.to_string(), extra);
    dev.serial = Some(d.serial.clone());
    dev
}

/// MAC of `ip` in the kernel neighbour table (`/proc/net/arp` text); None unless complete.
pub fn arp_mac(table: &str, ip: Ipv4Addr) -> Option<[u8; 6]> {
    table.lines().skip(1).find_map(|l| {
        let cols: Vec<&str> = l.split_whitespace().collect();
        if cols.first()?.parse::<Ipv4Addr>().ok()? != ip {
            return None;
        }
        // ATF_COM: the entry is resolved
        let flags = u32::from_str_radix(cols.get(2)?.trim_start_matches("0x"), 16).ok()?;
        parse_mac(cols.get(3)?).filter(|m| flags & 0x2 != 0 && *m != [0; 6])
    })
}

fn neighbour_mac(ip: IpAddr) -> Option<[u8; 6]> {
    let IpAddr::V4(ip) = ip else { return None };
    std::fs::read_to_string("/proc/net/arp").ok().and_then(|t| arp_mac(&t, ip))
}

/// Broadcast address of `ip`/`prefix` (`10.0.0.14/24` → `10.0.0.255`).
pub fn directed_broadcast(ip: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix.min(32))) };
    Ipv4Addr::from(u32::from(ip) | !mask)
}

/// Where ArtPoll goes by default: the limited broadcast (reaches nodes on the wire whatever
/// their subnet) and each LAN interface's broadcast address.
fn broadcast_targets() -> Vec<SocketAddr> {
    let mut out = vec![SocketAddr::from((Ipv4Addr::BROADCAST, ARTNET_PORT))];
    for (ip, prefix) in ucnet::local_ipv4_nets() {
        let a = SocketAddr::from((directed_broadcast(ip, prefix), ARTNET_PORT));
        if !out.contains(&a) {
            out.push(a);
        }
    }
    out
}

/// Network devices by registry key, with when each was last heard.
pub struct Tracker {
    timeout: Duration,
    seen: BTreeMap<String, (DeviceInfo, Instant)>,
}

impl Tracker {
    pub fn new(timeout: Duration) -> Tracker {
        Tracker { timeout, seen: BTreeMap::new() }
    }

    /// Record a sighting; returns the device when it is new or its details changed.
    pub fn observe(&mut self, d: DeviceInfo, now: Instant) -> Option<DeviceInfo> {
        match self.seen.get_mut(&d.syspath) {
            Some((old, at)) => {
                *at = now;
                if *old == d {
                    return None;
                }
                *old = d.clone();
                Some(d)
            }
            None => {
                self.seen.insert(d.syspath.clone(), (d.clone(), now));
                Some(d)
            }
        }
    }

    /// Remove and return devices not heard for the timeout.
    pub fn expire(&mut self, now: Instant) -> Vec<DeviceInfo> {
        let timeout = self.timeout;
        self.remove_where(|_, at| now.saturating_duration_since(at) >= timeout)
    }

    /// Remove and return every device of `kind` (its discovery was turned off).
    pub fn forget(&mut self, kind: Kind) -> Vec<DeviceInfo> {
        self.remove_where(|d, _| d.kind == kind)
    }

    fn remove_where(&mut self, f: impl Fn(&DeviceInfo, Instant) -> bool) -> Vec<DeviceInfo> {
        let keys: Vec<String> = self.seen.iter().filter(|(_, (d, at))| f(d, *at)).map(|(k, _)| k.clone()).collect();
        keys.into_iter().filter_map(|k| self.seen.remove(&k)).map(|(d, _)| d).collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NetConfig {
    /// Listen for UCNET console announcements.
    pub ucnet: bool,
    /// Poll for Art-Net nodes.
    pub artnet: bool,
    pub ucnet_bind: SocketAddr,
    pub artnet_bind: SocketAddr,
    /// ArtPoll destinations; empty = the default broadcasts.
    pub artnet_targets: Vec<SocketAddr>,
    pub poll: Duration,
    pub timeout: Duration,
}

impl Default for NetConfig {
    fn default() -> Self {
        NetConfig {
            ucnet: true,
            artnet: true,
            ucnet_bind: SocketAddr::from((Ipv4Addr::UNSPECIFIED, UCNET_PORT)),
            artnet_bind: SocketAddr::from((Ipv4Addr::UNSPECIFIED, ARTNET_PORT)),
            artnet_targets: Vec::new(),
            poll: POLL,
            timeout: TIMEOUT,
        }
    }
}

/// `project.toml [devices.network]`: `ucnet = false` / `artnet = false` turn that discovery off.
pub fn parse_config(section: Option<&toml::Value>) -> (NetConfig, Vec<String>) {
    let mut c = NetConfig::default();
    let mut errors = Vec::new();
    let Some(v) = section.and_then(|s| s.get("network")) else { return (c, errors) };
    let Some(t) = v.as_table() else {
        errors.push("devices.network: expected a table".into());
        return (c, errors);
    };
    for (k, v) in t {
        match (k.as_str(), v.as_bool()) {
            ("ucnet", Some(b)) => c.ucnet = b,
            ("artnet", Some(b)) => c.artnet = b,
            ("ucnet" | "artnet", None) => errors.push(format!("devices.network.{k}: expected true or false")),
            _ => errors.push(format!("devices.network.{k}: unknown setting (ucnet, artnet)")),
        }
    }
    (c, errors)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Proto {
    Ucnet,
    ArtNet,
}

impl Proto {
    pub fn as_str(self) -> &'static str {
        match self {
            Proto::Ucnet => "ucnet",
            Proto::ArtNet => "artnet",
        }
    }
    fn kind(self) -> Kind {
        match self {
            Proto::Ucnet => Kind::Ucnet,
            Proto::ArtNet => Kind::ArtNet,
        }
    }
}

#[derive(Debug)]
pub enum NetChange {
    /// New or changed device; `quiet` = first heard right after start (it was already there).
    Seen { device: Box<DeviceInfo>, quiet: bool },
    /// Not heard for the timeout, or its discovery was turned off (registry key).
    Gone(String),
    /// Whether one protocol's discovery works, and what it is doing.
    Status { proto: Proto, ok: bool, detail: String },
}

/// Start discovery. The returned handle sends an ArtPoll right away when notified
/// (`devices.rescan`). Ends when `tx` closes.
pub fn spawn(cfg: watch::Receiver<NetConfig>, tx: mpsc::UnboundedSender<NetChange>) -> Arc<Notify> {
    let poll_now = Arc::new(Notify::new());
    tokio::spawn(run(cfg, tx, poll_now.clone()));
    poll_now
}

/// Shares the port with other listeners (UCNET announcements are broadcasts).
fn shared_socket(addr: SocketAddr) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.set_reuse_port(true)?;
    s.set_broadcast(true)?;
    s.set_nonblocking(true)?;
    s.bind(&addr.into())?;
    UdpSocket::from_std(s.into())
}

/// Exclusive bind (no SO_REUSEADDR/SO_REUSEPORT), see the module docs.
fn exclusive_socket(addr: SocketAddr) -> io::Result<UdpSocket> {
    let s = std::net::UdpSocket::bind(addr)?;
    s.set_broadcast(true)?;
    s.set_nonblocking(true)?;
    UdpSocket::from_std(s)
}

async fn recv(s: Option<&UdpSocket>, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    match s {
        Some(s) => s.recv_from(buf).await,
        None => std::future::pending().await,
    }
}

struct Discovery {
    cfg: NetConfig,
    tx: mpsc::UnboundedSender<NetChange>,
    tracker: Tracker,
    started: Instant,
    ucnet: Option<UdpSocket>,
    artnet: Option<UdpSocket>,
    status: BTreeMap<Proto, (bool, String)>,
}

impl Discovery {
    fn set_status(&mut self, proto: Proto, ok: bool, detail: String) {
        if self.status.get(&proto).is_some_and(|(o, d)| *o == ok && *d == detail) {
            return;
        }
        self.status.insert(proto, (ok, detail.clone()));
        let _ = self.tx.send(NetChange::Status { proto, ok, detail });
    }

    fn open_ucnet(&mut self) {
        if !self.cfg.ucnet {
            self.ucnet = None;
            self.set_status(Proto::Ucnet, true, "off (project.toml [devices.network] ucnet = false)".into());
            return;
        }
        if self.ucnet.is_some() {
            return;
        }
        let port = self.cfg.ucnet_bind.port();
        match shared_socket(self.cfg.ucnet_bind) {
            Ok(s) => {
                self.ucnet = Some(s);
                self.set_status(Proto::Ucnet, true, format!("listening for mixer announcements on UDP {port}"));
            }
            Err(e) => self.set_status(Proto::Ucnet, false, format!("can't listen on UDP {port}: {e}")),
        }
    }

    fn open_artnet(&mut self) {
        if !self.cfg.artnet {
            self.artnet = None;
            self.set_status(Proto::ArtNet, true, "off (project.toml [devices.network] artnet = false)".into());
            return;
        }
        if self.artnet.is_some() {
            return;
        }
        let port = self.cfg.artnet_bind.port();
        match exclusive_socket(self.cfg.artnet_bind) {
            Ok(s) => self.artnet = Some(s),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => self.set_status(
                Proto::ArtNet,
                false,
                format!("UDP {port} is in use by another program (another Stream Engine or an Art-Net app); trying again every {} s", self.cfg.poll.as_secs()),
            ),
            Err(e) => self.set_status(Proto::ArtNet, false, format!("can't open UDP {port}: {e}")),
        }
    }

    async fn poll_artnet(&mut self) {
        self.open_artnet();
        let Some(s) = &self.artnet else { return };
        let targets = if self.cfg.artnet_targets.is_empty() { broadcast_targets() } else { self.cfg.artnet_targets.clone() };
        let pkt = art_poll();
        let mut last_err = None;
        let mut sent = 0;
        for t in &targets {
            match s.send_to(&pkt, *t).await {
                Ok(_) => sent += 1,
                Err(e) => last_err = Some(format!("{t}: {e}")),
            }
        }
        let (ok, detail) = match (sent, last_err) {
            (0, Some(e)) => (false, format!("can't send ArtPoll ({e})")),
            _ => (true, format!("asking for Art-Net nodes every {} s on UDP {}", self.cfg.poll.as_secs(), self.cfg.artnet_bind.port())),
        };
        self.set_status(Proto::ArtNet, ok, detail);
    }

    fn observe(&mut self, d: DeviceInfo, now: Instant) {
        if let Some(d) = self.tracker.observe(d, now) {
            let quiet = now.saturating_duration_since(self.started) < STARTUP_QUIET;
            let _ = self.tx.send(NetChange::Seen { device: Box::new(d), quiet });
        }
    }

    fn heard_ucnet(&mut self, data: &[u8], from: SocketAddr, now: Instant) {
        if let Some(d) = ucnet::parse_announcement(data, from) {
            let dev = ucnet_device(&d, neighbour_mac(from.ip()));
            self.observe(dev, now);
        }
    }

    fn heard_artnet(&mut self, data: &[u8], from: SocketAddr, now: Instant) {
        if let Ok(n) = parse_poll_reply(data)
            && let Some(d) = artnet_device(&n, from)
        {
            self.observe(d, now);
        }
    }

    fn gone(&mut self, list: Vec<DeviceInfo>) {
        for d in list {
            let _ = self.tx.send(NetChange::Gone(d.syspath));
        }
    }

    async fn apply_config(&mut self, cfg: NetConfig) {
        let old = std::mem::replace(&mut self.cfg, cfg);
        self.tracker.timeout = self.cfg.timeout;
        for (proto, was, now) in [(Proto::Ucnet, old.ucnet, self.cfg.ucnet), (Proto::ArtNet, old.artnet, self.cfg.artnet)] {
            if was && !now {
                let list = self.tracker.forget(proto.kind());
                self.gone(list);
            }
        }
        if old.ucnet_bind != self.cfg.ucnet_bind || !self.cfg.ucnet {
            self.ucnet = None;
        }
        if old.artnet_bind != self.cfg.artnet_bind || !self.cfg.artnet {
            self.artnet = None;
        }
        self.open_ucnet();
        self.poll_artnet().await;
    }
}

async fn run(mut cfg_rx: watch::Receiver<NetConfig>, tx: mpsc::UnboundedSender<NetChange>, poll_now: Arc<Notify>) {
    let cfg = cfg_rx.borrow_and_update().clone();
    let mut d = Discovery { tracker: Tracker::new(cfg.timeout), cfg, tx, started: Instant::now(), ucnet: None, artnet: None, status: BTreeMap::new() };
    d.open_ucnet();
    let new_interval = |p: Duration| {
        let mut i = tokio::time::interval(p.max(Duration::from_millis(100)));
        i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        i
    };
    let mut poll = new_interval(d.cfg.poll);
    let mut age = new_interval(Duration::from_secs(1));
    let mut ubuf = vec![0u8; 2048];
    let mut abuf = vec![0u8; 2048];
    while !d.tx.is_closed() {
        tokio::select! {
            r = recv(d.ucnet.as_ref(), &mut ubuf) => match r {
                Ok((n, from)) => d.heard_ucnet(&ubuf[..n], from, Instant::now()),
                Err(e) => {
                    // reopened on the next poll tick
                    d.ucnet = None;
                    d.set_status(Proto::Ucnet, false, format!("UDP {} receive failed: {e}", d.cfg.ucnet_bind.port()));
                }
            },
            r = recv(d.artnet.as_ref(), &mut abuf) => match r {
                Ok((n, from)) => d.heard_artnet(&abuf[..n], from, Instant::now()),
                Err(e) => {
                    d.artnet = None;
                    d.set_status(Proto::ArtNet, false, format!("UDP {} receive failed: {e}", d.cfg.artnet_bind.port()));
                }
            },
            _ = poll.tick() => {
                d.open_ucnet();
                d.poll_artnet().await;
            }
            _ = poll_now.notified() => d.poll_artnet().await,
            _ = age.tick() => {
                let list = d.tracker.expire(Instant::now());
                d.gone(list);
            }
            Ok(()) = cfg_rx.changed() => {
                let cfg = cfg_rx.borrow_and_update().clone();
                if cfg.poll != d.cfg.poll {
                    poll = new_interval(cfg.poll);
                }
                d.apply_config(cfg).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full Art-Net 4 ArtPollReply as a DMX gateway sends it (239 bytes).
    fn reply(ip: [u8; 4], short: &str, long: &str, mac: [u8; 6], bind_index: u8) -> Vec<u8> {
        let mut p = vec![0u8; 239];
        p[..8].copy_from_slice(b"Art-Net\0");
        p[8..10].copy_from_slice(&[0x00, 0x21]); // OpPollReply, little-endian
        p[10..14].copy_from_slice(&ip);
        p[14..16].copy_from_slice(&[0x36, 0x19]); // port 6454, little-endian
        p[16..18].copy_from_slice(&[0x01, 0x02]); // VersInfo 1.2
        p[20..22].copy_from_slice(&[0x08, 0x70]); // OEM
        p[26..26 + short.len()].copy_from_slice(short.as_bytes());
        p[44..44 + long.len()].copy_from_slice(long.as_bytes());
        p[108..108 + 10].copy_from_slice(b"#0001 [01]");
        p[173] = 4; // NumPortsLo
        p[201..207].copy_from_slice(&mac);
        p[207..211].copy_from_slice(&ip); // BindIp
        p[211] = bind_index;
        p
    }

    fn from(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn poll_reply_fields_and_mac() {
        let p = reply([10, 0, 0, 50], "DMX-4", "Stage left gateway", [0x00, 0x50, 0xc2, 0x12, 0x34, 0x56], 1);
        let n = parse_poll_reply(&p).unwrap();
        assert_eq!(n.ip, Ipv4Addr::new(10, 0, 0, 50));
        assert_eq!((n.short_name.as_str(), n.long_name.as_str()), ("DMX-4", "Stage left gateway"));
        assert_eq!(n.mac, Some([0x00, 0x50, 0xc2, 0x12, 0x34, 0x56]));
        assert_eq!((n.firmware, n.oem, n.bind_index), (0x0102, 0x0870, 1));

        let d = artnet_device(&n, from("10.0.0.50:6454")).unwrap();
        assert_eq!(d.identity, "mac-00:50:c2:12:34:56");
        assert_eq!(d.syspath, "net:mac-00:50:c2:12:34:56");
        assert_eq!(d.name, "Stage left gateway");
        assert_eq!(d.path, "10.0.0.50:6454");
        assert_eq!(d.kind, Kind::ArtNet);
        assert_eq!(d.extra["mac"], "00:50:c2:12:34:56");
        assert_eq!(d.extra["firmware"], "1.2");
        assert_eq!(d.extra["short_name"], "DMX-4");
    }

    #[test]
    fn truncated_foreign_and_other_opcodes_are_rejected() {
        let full = reply([10, 0, 0, 50], "n", "node", [0, 0x50, 0xc2, 1, 2, 3], 0);
        assert_eq!(parse_poll_reply(&full[..107]), Err(ArtError::Truncated(107)));
        assert_eq!(parse_poll_reply(&full[..9]), Err(ArtError::NotArtNet));
        assert_eq!(parse_poll_reply(&[]), Err(ArtError::NotArtNet));
        // a UCNET datagram on the wrong port
        assert_eq!(parse_poll_reply(b"UC\x00\x01\x08\xcf\x44\x41\x65\x00\x00\x00"), Err(ArtError::NotArtNet));
        // our own ArtPoll (broadcasts loop back) and se-dmx's ArtDmx
        assert_eq!(parse_poll_reply(&art_poll()), Err(ArtError::Opcode(0x2000)));
        let mut dmx = full.clone();
        dmx[8..10].copy_from_slice(&[0x00, 0x50]);
        assert_eq!(parse_poll_reply(&dmx), Err(ArtError::Opcode(0x5000)));
    }

    #[test]
    fn short_or_blank_mac_falls_back_to_ip_identity() {
        // Art-Net I reply: names but no MAC field
        let full = reply([10, 0, 0, 51], "old", "Old node", [0, 0x50, 0xc2, 1, 2, 3], 0);
        let n = parse_poll_reply(&full[..200]).unwrap();
        assert_eq!(n.mac, None);
        assert_eq!(artnet_device(&n, from("10.0.0.51:6454")).unwrap().identity, "artnet-10.0.0.51");
        // zeroed MAC, and a node that leaves its IP blank: the sender's address is used
        let n = parse_poll_reply(&reply([0, 0, 0, 0], "", "", [0; 6], 0)).unwrap();
        assert_eq!(n.mac, None);
        let d = artnet_device(&n, from("2.0.0.9:6454")).unwrap();
        assert_eq!((d.identity.as_str(), d.name.as_str(), d.path.as_str()), ("artnet-2.0.0.9", "Art-Net node 2.0.0.9", "2.0.0.9:6454"));
        assert!(!d.extra.contains_key("mac"));
        assert_eq!(parse_poll_reply(&reply([2, 0, 0, 9], "", "", [0xff; 6], 0)).unwrap().mac, None);
    }

    #[test]
    fn extra_port_groups_of_one_node_are_not_separate_devices() {
        let mac = [0, 0x50, 0xc2, 9, 9, 9];
        let root = parse_poll_reply(&reply([10, 0, 0, 60], "GW", "Gateway", mac, 1)).unwrap();
        let second = parse_poll_reply(&reply([10, 0, 0, 60], "GW", "Gateway ports 5-8", mac, 2)).unwrap();
        assert!(artnet_device(&root, from("10.0.0.60:6454")).is_some());
        assert!(artnet_device(&second, from("10.0.0.60:6454")).is_none());
    }

    #[test]
    fn art_poll_layout() {
        assert_eq!(art_poll(), [b'A', b'r', b't', b'-', b'N', b'e', b't', 0, 0x00, 0x20, 0x00, 0x0e, 0x00, 0x00]);
    }

    #[test]
    fn console_announcement_becomes_a_device_keyed_by_serial() {
        // the advertisement packet from featherbear/presonus-studiolive-console-advertisement
        let mut p = b"UC\x00\x01\x08\xcf\x44\x41\x65\x00\x00\x00\x00\x04\x00\x80\x48\x1c\x48\x67\x23\x60\x51\x4f\x92\x4e\x1e\x46\x91\x50\x51\xd1".to_vec();
        p.extend(b"StudioLive 16R\0AUD\0RA1E24110101\0Stage Rack\0");
        let a = ucnet::parse_announcement(&p, from("10.0.0.187:53000")).unwrap();
        let d = ucnet_device(&a, Some([0x00, 0x0a, 0x92, 0x03, 0x3a, 0x24]));
        assert_eq!(d.identity, "ucnet-RA1E24110101");
        assert_eq!(d.kind, Kind::Ucnet);
        assert_eq!(d.name, "Stage Rack (StudioLive 16R)");
        assert_eq!(d.path, "10.0.0.187:53000");
        assert_eq!(d.serial.as_deref(), Some("RA1E24110101"));
        assert_eq!(d.extra["mac"], "00:0a:92:03:3a:24");
        // the MAC is a detail: the identity stays the serial whether or not it is known
        assert_eq!(ucnet_device(&a, None).identity, d.identity);
    }

    #[test]
    fn neighbour_table_lookup_needs_a_complete_entry() {
        let t = "IP address       HW type     Flags       HW address            Mask     Device\n\
                 10.0.0.187       0x1         0x2         00:0a:92:03:3a:24     *        eno1\n\
                 10.0.0.50        0x1         0x0         00:00:00:00:00:00     *        eno1\n\
                 10.0.0.1         0x1         0x2         0c:fe:7b:7e:fe:e7     *        eno1\n";
        assert_eq!(arp_mac(t, Ipv4Addr::new(10, 0, 0, 187)), Some([0x00, 0x0a, 0x92, 0x03, 0x3a, 0x24]));
        assert_eq!(arp_mac(t, Ipv4Addr::new(10, 0, 0, 50)), None);
        assert_eq!(arp_mac(t, Ipv4Addr::new(10, 0, 0, 99)), None);
    }

    #[test]
    fn mac_text_forms() {
        let m = [0x00, 0x0a, 0x92, 0x03, 0x3a, 0x24];
        assert_eq!(parse_mac("00:0a:92:03:3a:24"), Some(m));
        assert_eq!(parse_mac("00-0A-92-03-3A-24"), Some(m));
        assert_eq!(parse_mac(" 000a92033a24 "), Some(m));
        assert_eq!(parse_mac("00:0a:92:03:3a"), None);
        assert_eq!(parse_mac("00:0a:92:03:3a:zz"), None);
        assert_eq!(parse_mac("+0:0a:92:03:3a:24"), None);
        assert_eq!(fmt_mac(m), "00:0a:92:03:3a:24");
    }

    #[test]
    fn directed_broadcasts() {
        assert_eq!(directed_broadcast(Ipv4Addr::new(10, 0, 0, 14), 24), Ipv4Addr::new(10, 0, 0, 255));
        assert_eq!(directed_broadcast(Ipv4Addr::new(2, 1, 2, 3), 8), Ipv4Addr::new(2, 255, 255, 255));
        assert_eq!(directed_broadcast(Ipv4Addr::new(192, 168, 1, 77), 26), Ipv4Addr::new(192, 168, 1, 127));
    }

    #[test]
    fn tracker_reports_new_and_changed_devices_and_ages_out_quiet_ones() {
        let n = parse_poll_reply(&reply([10, 0, 0, 50], "A", "Node A", [0, 0x50, 0xc2, 1, 1, 1], 1)).unwrap();
        let d = artnet_device(&n, from("10.0.0.50:6454")).unwrap();
        let mut t = Tracker::new(Duration::from_secs(10));
        let t0 = Instant::now();
        assert!(t.observe(d.clone(), t0).is_some(), "new");
        assert!(t.observe(d.clone(), t0 + Duration::from_secs(3)).is_none(), "same again");
        let mut renamed = d.clone();
        renamed.name = "Node A (moved)".into();
        assert_eq!(t.observe(renamed.clone(), t0 + Duration::from_secs(6)).map(|x| x.name), Some("Node A (moved)".into()));
        // last heard at 6 s: still there at 15.9 s, gone at 16 s
        assert!(t.expire(t0 + Duration::from_millis(15_900)).is_empty());
        let gone = t.expire(t0 + Duration::from_secs(16));
        assert_eq!(gone.iter().map(|x| x.syspath.as_str()).collect::<Vec<_>>(), ["net:mac-00:50:c2:01:01:01"]);
        assert!(t.expire(t0 + Duration::from_secs(30)).is_empty());
        // heard again later: new again
        assert!(t.observe(renamed, t0 + Duration::from_secs(40)).is_some());
        assert_eq!(t.forget(Kind::Ucnet).len(), 0);
        assert_eq!(t.forget(Kind::ArtNet).len(), 1);
    }

    #[test]
    fn network_section_toggles() {
        let cfg = |s: &str| toml::from_str::<toml::Table>(s).unwrap().get("devices").cloned();
        let (c, errs) = parse_config(cfg("[devices.network]\nartnet = false").as_ref());
        assert!(errs.is_empty());
        assert!(c.ucnet && !c.artnet);
        let (c, errs) = parse_config(cfg("[devices.network]\nucnet = \"no\"\nsacn = true").as_ref());
        assert!(c.ucnet, "a bad value keeps the default");
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert_eq!(parse_config(None).0, NetConfig::default());
    }
}
