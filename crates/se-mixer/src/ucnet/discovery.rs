//! Finding consoles on the LAN.
//!
//! 1. **Broadcast** (the protocol's way, port of `Discovery.ts`): consoles send an announcement
//!    from UDP 53000 to 255.255.255.255:47809 every 3 s. Host firewalls with a default-deny
//!    input policy (ufw on this machine) drop these; see [`FIREWALL_HINT`].
//! 2. **Probe** (ours, works through such firewalls because we initiate): PreSonus neighbours
//!    from the ARP cache first (OUI 00:0A:92), then a TCP connect sweep of port 53000 across the
//!    local /24 of every running LAN interface.

use super::packet::{self, CONTROL_PORT, Code, DISCOVERY_PORT};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::{TcpStream, UdpSocket};

/// What the owner runs so broadcast discovery works through ufw.
pub const FIREWALL_HINT: &str = "sudo ufw allow in proto udp from any port 53000 to any port 47809 comment 'PreSonus UCNET discovery'";

/// IEEE OUI of PreSonus Audio Electronics.
pub const PRESONUS_OUI: [u8; 3] = [0x00, 0x0a, 0x92];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    Broadcast,
    Probe,
    Configured,
    Cached,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Device {
    /// Model string used for console identification ("StudioLive 16R"); empty when probed.
    pub model: String,
    pub serial: String,
    /// Friendly name set on the console.
    pub name: String,
    pub addr: SocketAddr,
    pub via: Via,
}

/// Parse an announcement datagram (strings after 32 bytes: model, "AUD", serial, name).
pub fn parse_announcement(datagram: &[u8], from: SocketAddr) -> Option<Device> {
    let f = packet::parse_lenient(datagram).ok()?;
    if f.code != Code::DISCOVERY {
        return None;
    }
    let port = u16::from_le_bytes([datagram[4], datagram[5]]);
    let strings: Vec<String> = f.body.get(20..)?.split(|b| *b == 0).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    let model = strings.first()?.clone();
    let serial = strings.get(2).cloned().unwrap_or_default();
    if model.is_empty() || serial.is_empty() {
        return None;
    }
    Some(Device {
        model,
        serial,
        name: strings.get(3).cloned().unwrap_or_default(),
        addr: SocketAddr::new(from.ip(), if port == 0 { CONTROL_PORT } else { port }),
        via: Via::Broadcast,
    })
}

fn discovery_socket() -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    // share the port with UC Surface / Universal Control running on the same machine
    s.set_reuse_port(true)?;
    s.set_broadcast(true)?;
    s.set_nonblocking(true)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT)).into())?;
    UdpSocket::from_std(s.into())
}

/// Listen for announcements for `dur`; one entry per serial.
pub async fn listen(dur: Duration) -> std::io::Result<Vec<Device>> {
    let sock = discovery_socket()?;
    let mut found: BTreeMap<String, Device> = BTreeMap::new();
    let mut buf = [0u8; 1500];
    let deadline = tokio::time::Instant::now() + dur;
    while let Ok(r) = tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await {
        let (n, from) = r?;
        if let Some(d) = parse_announcement(&buf[..n], from) {
            found.insert(d.serial.clone(), d);
        }
    }
    Ok(found.into_values().collect())
}

fn virtual_iface(name: &str) -> bool {
    ["docker", "br-", "veth", "virbr", "tailscale", "tun", "tap", "wg", "zt", "lxc", "podman"].iter().any(|p| name.starts_with(p))
}

/// Running, non-loopback, non-virtual IPv4 interfaces: (address, prefix length).
pub fn local_ipv4_nets() -> Vec<(Ipv4Addr, u8)> {
    let mut out = Vec::new();
    // SAFETY: getifaddrs returns a linked list we only read and free once; pointers are
    // checked for null and sockaddr casts are guarded by the address family.
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_netmask.is_null() || (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let flags = ifa.ifa_flags;
            if flags & libc::IFF_UP as u32 == 0 || flags & libc::IFF_RUNNING as u32 == 0 || flags & libc::IFF_LOOPBACK as u32 != 0 {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy();
            if virtual_iface(&name) {
                continue;
            }
            let a = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let m = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
            let ip = Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr));
            let prefix = u32::from_be(m.sin_addr.s_addr).count_ones() as u8;
            if prefix >= 31 || ip.is_link_local() {
                continue;
            }
            out.push((ip, prefix));
        }
        libc::freeifaddrs(ifap);
    }
    out
}

/// Hosts of the /24 (or smaller subnet) around `ip`, excluding `ip` itself, network and
/// broadcast addresses. Larger subnets are narrowed to the /24 to bound the sweep.
pub fn sweep_hosts(ip: Ipv4Addr, prefix: u8) -> Vec<Ipv4Addr> {
    let prefix = prefix.max(24);
    let mask = u32::MAX << (32 - prefix as u32);
    let net = u32::from(ip) & mask;
    let size = 1u32 << (32 - prefix as u32);
    (1..size.saturating_sub(1)).map(|i| Ipv4Addr::from(net + i)).filter(|h| *h != ip).collect()
}

/// PreSonus hosts in the kernel neighbour table (`/proc/net/arp`).
pub fn presonus_neighbors() -> Vec<Ipv4Addr> {
    std::fs::read_to_string("/proc/net/arp").map(|s| parse_arp(&s)).unwrap_or_default()
}

fn parse_arp(table: &str) -> Vec<Ipv4Addr> {
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            let ip: Ipv4Addr = cols.first()?.parse().ok()?;
            let mac: Vec<u8> = cols.get(3)?.split(':').filter_map(|h| u8::from_str_radix(h, 16).ok()).collect();
            (mac.len() == 6 && mac[..3] == PRESONUS_OUI).then_some(ip)
        })
        .collect()
}

/// TCP-connect probe of `port` on `hosts`; returns responsive hosts in input order.
pub async fn probe(hosts: &[Ipv4Addr], port: u16, timeout: Duration, concurrency: usize) -> Vec<Ipv4Addr> {
    let mut found = Vec::new();
    for batch in hosts.chunks(concurrency.max(1)) {
        let mut set = tokio::task::JoinSet::new();
        for (i, h) in batch.iter().enumerate() {
            let addr = SocketAddr::new(IpAddr::V4(*h), port);
            set.spawn(async move { (i, tokio::time::timeout(timeout, TcpStream::connect(addr)).await.is_ok_and(|r| r.is_ok())) });
        }
        let mut ok: Vec<usize> = Vec::new();
        while let Some(r) = set.join_next().await {
            if let Ok((i, true)) = r {
                ok.push(i);
            }
        }
        ok.sort_unstable();
        found.extend(ok.into_iter().map(|i| batch[i]));
    }
    found
}

/// Probe for consoles: ARP-known PreSonus neighbours first; the subnet sweep only when none
/// of them answers.
pub async fn probe_lan(port: u16) -> Vec<Device> {
    let as_devices = |hosts: Vec<Ipv4Addr>| -> Vec<Device> {
        hosts
            .into_iter()
            .map(|h| Device { model: String::new(), serial: String::new(), name: String::new(), addr: SocketAddr::new(IpAddr::V4(h), port), via: Via::Probe })
            .collect()
    };
    let neighbours = presonus_neighbors();
    let hit = probe(&neighbours, port, Duration::from_millis(1500), 16).await;
    if !hit.is_empty() {
        return as_devices(hit);
    }
    let mut hosts: Vec<Ipv4Addr> = Vec::new();
    for (ip, prefix) in local_ipv4_nets() {
        for h in sweep_hosts(ip, prefix) {
            if !hosts.contains(&h) {
                hosts.push(h);
            }
        }
    }
    as_devices(probe(&hosts, port, Duration::from_millis(3200), 96).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announcement_parses_reference_layout() {
        // the advertisement packet from featherbear/presonus-studiolive-console-advertisement
        let mut p = b"UC\x00\x01\x08\xcf\x44\x41\x65\x00\x00\x00\x00\x04\x00\x80\x48\x1c\x48\x67\x23\x60\x51\x4f\x92\x4e\x1e\x46\x91\x50\x51\xd1".to_vec();
        p.extend(b"StudioLive 16R\0AUD\0RA1E24110101\0Stage Rack\0");
        let from: SocketAddr = "10.0.0.187:53000".parse().unwrap();
        let d = parse_announcement(&p, from).unwrap();
        assert_eq!(d.model, "StudioLive 16R");
        assert_eq!(d.serial, "RA1E24110101");
        assert_eq!(d.name, "Stage Rack");
        assert_eq!(d.addr, "10.0.0.187:53000".parse().unwrap());
        // other message codes on the port are ignored
        let ka = packet::encode(Code::KEEP_ALIVE, &[]).unwrap();
        assert!(parse_announcement(&ka, from).is_none());
    }

    #[test]
    fn sweep_covers_the_slash_24_only() {
        let h = sweep_hosts(Ipv4Addr::new(10, 0, 0, 14), 24);
        assert_eq!(h.len(), 253);
        assert!(!h.contains(&Ipv4Addr::new(10, 0, 0, 14)));
        assert!(h.contains(&Ipv4Addr::new(10, 0, 0, 187)));
        assert!(!h.contains(&Ipv4Addr::new(10, 0, 0, 255)));
        // a /16 is narrowed to our /24
        assert_eq!(sweep_hosts(Ipv4Addr::new(172, 16, 5, 9), 16).len(), 253);
        assert_eq!(sweep_hosts(Ipv4Addr::new(192, 168, 1, 1), 30), vec![Ipv4Addr::new(192, 168, 1, 2)]);
    }

    #[test]
    fn arp_table_filters_presonus_oui() {
        let t = "IP address       HW type     Flags       HW address            Mask     Device\n\
                 10.0.0.187       0x1         0x2         00:0a:92:03:3a:24     *        eno1\n\
                 10.0.0.1         0x1         0x2         0c:fe:7b:7e:fe:e7     *        eno1\n";
        assert_eq!(parse_arp(t), vec![Ipv4Addr::new(10, 0, 0, 187)]);
    }
}
