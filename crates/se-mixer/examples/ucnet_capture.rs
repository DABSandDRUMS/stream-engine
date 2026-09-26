//! Record UCNET traffic from a real console into test fixtures.
//!
//! ```text
//! cargo run -p se-mixer --example ucnet_capture -- --host 10.0.0.187 --out crates/se-mixer/tests/fixtures
//! cargo run -p se-mixer --example ucnet_capture -- --host 10.0.0.187 --out … --exercise 16
//! ```
//!
//! Files (TCP captures are the raw byte stream; UDP captures are u32-LE-length-prefixed
//! datagrams):
//! * `handshake.tcp`  – everything from subscribe until the state + subscription reply
//! * `keepalive.tcp`  – replies to two keep-alive/liveness requests
//! * `meters.udp`     – two seconds of meter datagrams
//! * `discovery.udp`  – announcements heard on UDP 47809 (only if the host firewall lets
//!   broadcasts in; see `se_mixer::ucnet::discovery::FIREWALL_HINT`)
//! * with `--exercise N`: `mute_on.tcp`, `fader_set.tcp`, `fader_restore.tcp`,
//!   `mute_restore.tcp` – the console's echoes while line channel N's mute and fader are
//!   changed and then restored to their original values. N must be silent (its input meter
//!   is checked first) so the exercise is inaudible.

use se_mixer::ucnet::meters::{LevelFrame, MeterKind, group, to_db};
use se_mixer::ucnet::msg::{self, Decoder, Incoming};
use se_mixer::ucnet::packet::{self, CONTROL_PORT, DISCOVERY_PORT, Reassembler};
use se_mixer::ucnet::tree::ConsoleState;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

struct Cap {
    tcp: TcpStream,
    udp: UdpSocket,
    re: Reassembler,
    dec: Decoder,
}

impl Cap {
    /// Read TCP for `dur`, returning raw packets and decoded messages.
    async fn tcp_for(&mut self, dur: Duration) -> anyhow::Result<(Vec<u8>, Vec<Incoming>)> {
        let mut raw = Vec::new();
        let mut decoded = Vec::new();
        let mut buf = vec![0u8; 65536];
        let deadline = tokio::time::Instant::now() + dur;
        while let Ok(r) = tokio::time::timeout_at(deadline, self.tcp.read(&mut buf)).await {
            let n = r?;
            anyhow::ensure!(n > 0, "console closed the connection");
            self.re.push(&buf[..n]);
            while let Some(p) = self.re.next_packet()? {
                raw.extend_from_slice(&p);
                decoded.push(self.dec.decode(packet::parse(&p)?)?);
            }
        }
        Ok((raw, decoded))
    }

    async fn udp_for(&mut self, dur: Duration) -> anyhow::Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 4096];
        let deadline = tokio::time::Instant::now() + dur;
        while let Ok(r) = tokio::time::timeout_at(deadline, self.udp.recv_from(&mut buf)).await {
            let (n, _) = r?;
            out.push(buf[..n].to_vec());
        }
        Ok(out)
    }
}

fn write_udp(path: &Path, datagrams: &[Vec<u8>]) -> anyhow::Result<()> {
    let mut out = Vec::new();
    for d in datagrams {
        out.extend_from_slice(&(d.len() as u32).to_le_bytes());
        out.extend_from_slice(d);
    }
    std::fs::write(path, out)?;
    println!("wrote {} ({} datagrams)", path.display(), datagrams.len());
    Ok(())
}

fn write_tcp(path: &Path, raw: &[u8]) -> anyhow::Result<()> {
    std::fs::write(path, raw)?;
    println!("wrote {} ({} bytes)", path.display(), raw.len());
    Ok(())
}

fn summarize(label: &str, msgs: &[Incoming]) {
    for m in msgs {
        match m {
            Incoming::Param { path, value } => println!("  {label}: PV {path} = {value}"),
            Incoming::Faders(g) => {
                let line: Vec<String> = g.iter().filter(|g| g.group == 0).flat_map(|g| g.values.iter().map(|v| format!("{v:.4}"))).collect();
                println!("  {label}: MS fdrs line = [{}]", line.join(", "));
            }
            Incoming::Json(j) => println!("  {label}: JM {j}"),
            Incoming::Text { path, value } => println!("  {label}: PS {path} = {value:?}"),
            Incoming::State(s) => println!("  {label}: state with {} values", s.values.len()),
            _ => {}
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut host: Option<String> = None;
    let mut out = PathBuf::from("crates/se-mixer/tests/fixtures");
    let mut exercise: Option<u16> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--host" => host = args.next(),
            "--out" => out = args.next().map(PathBuf::from).unwrap_or(out),
            "--exercise" => exercise = args.next().and_then(|s| s.parse().ok()),
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    let host = host.ok_or_else(|| anyhow::anyhow!("--host <console ip> is required"))?;
    std::fs::create_dir_all(&out)?;
    let addr: SocketAddr = format!("{host}:{CONTROL_PORT}").parse()?;

    // discovery (best effort: needs broadcasts to pass the host firewall)
    match UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT))).await {
        Ok(sock) => {
            let mut heard = Vec::new();
            let mut buf = vec![0u8; 1500];
            let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
            while let Ok(Ok((n, _))) = tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await {
                heard.push(buf[..n].to_vec());
            }
            if heard.is_empty() {
                println!("no discovery broadcasts heard in 4 s (firewall?): {}", se_mixer::ucnet::discovery::FIREWALL_HINT);
            } else {
                write_udp(&out.join("discovery.udp"), &heard)?;
            }
        }
        Err(e) => println!("cannot listen on UDP {DISCOVERY_PORT}: {e}"),
    }

    let tcp = TcpStream::connect(addr).await?;
    tcp.set_nodelay(true)?;
    let udp = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))).await?;
    let mut cap = Cap { tcp, udp, re: Reassembler::default(), dec: Decoder::default() };
    cap.tcp.write_all(&msg::subscribe("stream-engine capture", "5e0e5e0e5e0ec0de")).await?;
    cap.udp.send_to(&[], SocketAddr::new(addr.ip(), CONTROL_PORT)).await?;
    let port = cap.udp.local_addr()?.port();
    cap.tcp.write_all(&msg::meter_hello(port)).await?;

    let (raw, msgs) = cap.tcp_for(Duration::from_secs(3)).await?;
    let state: ConsoleState = msgs
        .iter()
        .find_map(|m| match m {
            Incoming::State(s) => Some((**s).clone()),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("no state payload in the handshake"))?;
    write_tcp(&out.join("handshake.tcp"), &raw)?;
    let info = state.info();
    println!("console: {} \"{}\" fw {} serial {}", info.model, info.name, info.firmware, info.serial);

    let meters = cap.udp_for(Duration::from_secs(2)).await?;
    write_udp(&out.join("meters.udp"), &meters)?;

    cap.tcp.write_all(&[msg::keep_alive(), msg::liveness_request(0x5e01), msg::keep_alive(), msg::liveness_request(0x5e02)].concat()).await?;
    let (raw, _) = cap.tcp_for(Duration::from_millis(800)).await?;
    write_tcp(&out.join("keepalive.tcp"), &raw)?;

    if let Some(ch) = exercise {
        let fader_path = format!("line/ch{ch}/volume");
        let mute_path = format!("line/ch{ch}/mute");
        let orig_fader = state.num(&fader_path).ok_or_else(|| anyhow::anyhow!("no {fader_path}"))?;
        let orig_mute = state.num(&mute_path).ok_or_else(|| anyhow::anyhow!("no {mute_path}"))?;
        // safety: the channel must carry no signal
        let mut peak = 0f32;
        let mut f = LevelFrame::default();
        for d in &meters {
            if let Ok(MeterKind::Level) = f.parse(d) {
                peak = peak.max(f.level(group::INPUT, ch as usize - 1));
            }
        }
        anyhow::ensure!(to_db(peak) < -70.0, "channel {ch} has input signal ({:.1} dBFS); refusing to touch it", to_db(peak));
        println!("exercising line ch{ch} (\"{}\"): original fader {orig_fader}, mute {orig_mute}", state.text(&format!("line/ch{ch}/username")).unwrap_or(""));

        let steps: [(&str, &str, f32); 4] = [
            ("mute_on", &mute_path, 1.0),
            ("fader_set", &fader_path, 0.5),
            ("fader_restore", &fader_path, orig_fader as f32),
            ("mute_restore", &mute_path, orig_mute as f32),
        ];
        let mut failed = None;
        for (name, path, v) in steps {
            if let Err(e) = cap.tcp.write_all(&msg::set_param(path, v)).await {
                failed = Some(e.to_string());
                break;
            }
            match cap.tcp_for(Duration::from_millis(700)).await {
                Ok((raw, msgs)) => {
                    summarize(name, &msgs);
                    write_tcp(&out.join(format!("{name}.tcp")), &raw)?;
                }
                Err(e) => {
                    failed = Some(e.to_string());
                    break;
                }
            }
        }
        if let Some(e) = failed {
            // restore on a fresh connection whatever happened
            let mut t = TcpStream::connect(addr).await?;
            t.write_all(&msg::subscribe("stream-engine capture", "5e0e5e0e5e0ec0de")).await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            t.write_all(&[msg::set_param(&fader_path, orig_fader as f32), msg::set_param(&mute_path, orig_mute as f32)].concat()).await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            anyhow::bail!("exercise failed ({e}); originals re-sent");
        }
    }
    cap.tcp.write_all(&msg::unsubscribe()).await?;
    Ok(())
}
