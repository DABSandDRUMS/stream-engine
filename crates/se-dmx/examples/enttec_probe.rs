//! Hardware probe for the ENTTEC DMX USB PRO.
//!
//! Prints firmware, output timing and serial number, streams a slow RGB fade on channels 1-3 at
//! 44 Hz while timing every write, optionally runs RDM discovery + DEVICE_INFO, and always leaves
//! the widget outputting an all-zero universe.
//!
//! ```text
//! flock /tmp/se-lights-enttec.lock cargo run -p se-dmx --example enttec_probe -- [port|auto] [--rdm] [--frames N]
//! ```

use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use se_dmx::output::enttec::{DmxFrame, UsbPro};
use se_dmx::output::rdm::{self, Uid};

const USAGE: &str = "usage: enttec_probe [port|auto] [--rdm] [--frames N]";
const RATE_HZ: f64 = 44.0;
const DEFAULT_FRAMES: u32 = 220;
/// ENTTEC's ESTA manufacturer ID, used for the probe's controller UID.
const ENTTEC_ESTA_ID: u16 = 0x454E;
/// Seconds per full hue cycle of the fade.
const FADE_PERIOD_S: f64 = 8.0;

struct Args {
    port: Option<String>,
    rdm: bool,
    frames: u32,
}

fn parse_args() -> Result<Args> {
    let mut args = Args { port: None, rdm: false, frames: DEFAULT_FRAMES };
    let mut positional_seen = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--rdm" => args.rdm = true,
            "--frames" => {
                let n = it.next().context("--frames needs a count")?;
                args.frames = n.parse().with_context(|| format!("--frames: {n:?} is not a count"))?;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            p if !p.starts_with('-') && !positional_seen => {
                positional_seen = true;
                args.port = (p != "auto").then(|| p.to_string());
            }
            other => bail!("unexpected argument {other:?}\n{USAGE}"),
        }
    }
    Ok(args)
}

/// Fully saturated hue (0..1) as 8-bit RGB.
fn hue_rgb(h: f64) -> [u8; 3] {
    let h6 = h.rem_euclid(1.0) * 6.0;
    let x = 1.0 - (h6 % 2.0 - 1.0).abs();
    let (r, g, b) = match h6 as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    [(r * 255.0f64).round() as u8, (g * 255.0f64).round() as u8, (b * 255.0f64).round() as u8]
}

fn stream(dev: &mut UsbPro, frames: u32) -> Result<()> {
    if frames == 0 {
        return Ok(());
    }
    let mut frame = DmxFrame::new(512);
    let mut data = [0u8; 512];
    let period = Duration::from_secs_f64(1.0 / RATE_HZ);
    let (mut total, mut max) = (Duration::ZERO, Duration::ZERO);
    let mut max_late = Duration::ZERO;
    let start = Instant::now();
    let mut first_send = start;
    let mut last_send = start;
    for i in 0..frames {
        let due = start + period * i;
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        data[..3].copy_from_slice(&hue_rgb(f64::from(i) / RATE_HZ / FADE_PERIOD_S));
        frame.set(&data);
        let t0 = Instant::now();
        max_late = max_late.max(t0.saturating_duration_since(due));
        dev.send_dmx(&frame).with_context(|| format!("label 6 write {i}"))?;
        let dt = t0.elapsed();
        total += dt;
        max = max.max(dt);
        if i == 0 {
            first_send = t0;
        }
        last_send = t0;
    }
    let mean = total / frames;
    let span = last_send.duration_since(first_send).as_secs_f64();
    let rate = if frames > 1 && span > 0.0 { f64::from(frames - 1) / span } else { 0.0 };
    println!(
        "dmx       {frames} frames × {} B (label 6, 512 ch): write mean {:.1} µs, max {:.1} µs; achieved {rate:.2} Hz (target {RATE_HZ} Hz), max schedule lateness {:.2} ms",
        frame.bytes().len(),
        mean.as_secs_f64() * 1e6,
        max.as_secs_f64() * 1e6,
        max_late.as_secs_f64() * 1e3,
    );
    Ok(())
}

fn rdm_probe(dev: &mut UsbPro, src: Uid) {
    println!("rdm       controller UID {src}");
    let started = Instant::now();
    let uids = match rdm::discover(dev, src) {
        Ok(uids) => uids,
        Err(e) if e.kind() == io::ErrorKind::Unsupported => {
            println!("rdm       unsupported: {e}");
            return;
        }
        Err(e) => {
            println!("rdm       discovery failed: {e}");
            return;
        }
    };
    let ms = started.elapsed().as_millis();
    if uids.is_empty() {
        println!("rdm       discovery finished in {ms} ms: no responders");
        return;
    }
    println!("rdm       discovery finished in {ms} ms: {} device(s)", uids.len());
    for uid in uids {
        match rdm::device_info(dev, src, uid) {
            Ok(Some(i)) => println!(
                "rdm       {uid}: model 0x{:04X} category 0x{:04X} sw 0x{:08X} footprint {} personality {}/{} start {} sub-devices {} sensors {} | manufacturer {:?} model {:?} label {:?}",
                i.model_id,
                i.category,
                i.software_version,
                i.footprint,
                i.personality,
                i.personalities,
                i.start_address,
                i.sub_devices,
                i.sensors,
                i.manufacturer.as_deref().unwrap_or("-"),
                i.model.as_deref().unwrap_or("-"),
                i.label.as_deref().unwrap_or("-"),
            ),
            Ok(None) => println!("rdm       {uid}: no DEVICE_INFO response"),
            Err(e) => println!("rdm       {uid}: DEVICE_INFO failed: {e}"),
        }
    }
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let path = match args.port {
        Some(p) => p,
        None => UsbPro::find_port().context("no /dev/serial/by-id/*ENTTEC*DMX_USB_PRO* device found")?,
    };
    let mut dev = UsbPro::open(&path).with_context(|| format!("opening {path}"))?;
    println!("port      {path}");

    let params = dev.get_params().context("Get Widget Parameters (label 3)")?;
    println!("firmware  {} = {} (major {}, raw 0x{:04X})", params.version(), params.kind(), params.firmware_major, params.firmware);
    let rate = if params.rate == 0 { "max".to_string() } else { format!("{} packets/s", params.rate) };
    println!("timing    break {:.2} µs, MAB {:.2} µs, rate {rate}", params.break_us, params.mab_us);

    let serial = dev.get_serial().context("Get Widget Serial Number (label 10)")?;
    match serial {
        Some(s) => println!("serial    {s:08}"),
        None => println!("serial    unprogrammed"),
    }

    // Label 10 stopped periodic output; the stream's first label 6 resumes it.
    let streamed = stream(&mut dev, args.frames);
    if streamed.is_ok() && args.rdm {
        rdm_probe(&mut dev, Uid::new(ENTTEC_ESTA_ID, serial.unwrap_or(1)));
    }

    // Always leave the rig dark: RDM turned the port to input, and a label 6 resumes output.
    dev.send_dmx(&DmxFrame::new(512)).context("final all-zero label 6 frame")?;
    println!("final     all-zero 512-channel frame sent; widget keeps outputting zeros");
    streamed
}
