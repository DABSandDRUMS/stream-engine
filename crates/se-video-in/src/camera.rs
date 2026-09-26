//! Camera capture worker: one thread per captured source. Resolves the device by stable
//! identity, negotiates format/size/fps, streams mmap buffers into the source's video slot,
//! detects signal, measures fps/drops/CPU, applies camera controls, and reopens after
//! unplug/replug or an HDMI timing change.

use crate::config::{CameraDef, SourceDef};
use crate::controls;
use crate::frame::{DropCounter, FpsMeter, MjpegDecoder, SignalDetector, copy_rows, rgba_stats, yuyv_stats};
use crate::preview;
use crate::status::{CpuMeter, OWNER, Publisher, Status};
use crossbeam_channel::{Receiver, RecvTimeoutError, TryRecvError};
use se_devices::v4l2::{self, Control, Device, Stream};
use se_hub::Hub;
use se_hub::media::{PixelFormat, VideoWriter};
use se_proto::Value;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub enum Cmd {
    Stop,
    Config(Arc<SourceDef>),
    /// Devices changed (hotplug): re-resolve the identity.
    Rescan,
    /// Close and reopen the device.
    Reopen,
}

enum Exit {
    Stop,
    Reopen,
    Lost(String),
}

const CONTROL_PERIOD_NS: u64 = 50_000_000;
const STATS_PERIOD_NS: u64 = 1_000_000_000;

/// A declared camera control and what we last wrote to the device.
struct Bound {
    ctrl: Control,
    address: String,
    /// Last resolved engine value seen (skip conversion when unchanged).
    seen: Value,
    /// Device value last written or read.
    applied: Option<i64>,
    /// Value the device rejected (don't retry until it changes or flags change).
    failed: Option<i64>,
}

/// Start the worker; the thread hands the slot writer back when it stops.
pub fn spawn(
    hub: Arc<Hub>,
    def: Arc<SourceDef>,
    status: Arc<Status>,
    mut writer: VideoWriter,
    rx: Receiver<Cmd>,
) -> std::io::Result<std::thread::JoinHandle<VideoWriter>> {
    std::thread::Builder::new().name(format!("se-cam-{}", def.name)).spawn(move || {
        run(hub, def, status, &mut writer, rx);
        writer
    })
}

fn run(hub: Arc<Hub>, mut def: Arc<SourceDef>, st: Arc<Status>, writer: &mut VideoWriter, rx: Receiver<Cmd>) {
    let mut pubs = Publisher::new(hub.clone(), &def.name);
    let mut last_err = String::new();
    loop {
        let Some(cam) = def.camera().cloned() else { return };
        let Some(info) = se_devices::find_camera(&cam.device) else {
            st.missing.store(true, Ordering::Relaxed);
            set_idle(&st, &mut pubs, &format!("device `{}` not found", cam.device), &mut last_err);
            // wait for hotplug/config (also poll: udev events can be missed across suspend)
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(Cmd::Config(d)) => def = d,
                Ok(Cmd::Rescan) | Ok(Cmd::Reopen) | Err(RecvTimeoutError::Timeout) => {}
            }
            continue;
        };
        st.missing.store(false, Ordering::Relaxed);
        match capture(&hub, &mut def, &cam, &info, &st, writer, &rx, &mut pubs) {
            Exit::Stop => {
                set_idle(&st, &mut pubs, "", &mut last_err);
                return;
            }
            Exit::Reopen => continue,
            Exit::Lost(e) => {
                set_idle(&st, &mut pubs, &e, &mut last_err);
                match rx.recv_timeout(Duration::from_millis(1000)) {
                    Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                    Ok(Cmd::Config(d)) => def = d,
                    _ => {}
                }
            }
        }
    }
}

fn set_idle(st: &Status, pubs: &mut Publisher, err: &str, last_err: &mut String) {
    st.capturing.store(false, Ordering::Relaxed);
    st.signal.store(false, Ordering::Relaxed);
    st.set_fps(0.0);
    st.set_cpu(0.0);
    st.info.lock().error = err.to_string();
    pubs.set("capturing", Value::Bool(false));
    pubs.set("signal", Value::Bool(false));
    pubs.set("fps", Value::Float(0.0));
    pubs.set("cpu", Value::Float(0.0));
    pubs.set("error", Value::Str(err.to_string()));
    if !err.is_empty() && err != last_err {
        pubs.hub().log("warn", OWNER, err.to_string());
    }
    *last_err = err.to_string();
}

#[allow(clippy::too_many_arguments)]
fn capture(
    hub: &Arc<Hub>,
    def: &mut Arc<SourceDef>,
    cam: &CameraDef,
    info: &se_devices::DeviceInfo,
    st: &Status,
    writer: &mut VideoWriter,
    rx: &Receiver<Cmd>,
    pubs: &mut Publisher,
) -> Exit {
    let name = def.name.clone();
    let dev = match Device::open(&info.path, true) {
        Ok(d) => d,
        Err(e) => return Exit::Lost(format!("{name}: open {}: {e}", info.path)),
    };
    match dev.caps() {
        Ok(c) if c.is_capture() => {}
        Ok(_) => return Exit::Lost(format!("{name}: {} is not a streaming capture device", info.path)),
        Err(e) => return Exit::Lost(format!("{name}: {}: {e}", info.path)),
    }
    // HDMI inputs: lock onto the detected timing before choosing a format.
    if let Ok(t) = dev.query_dv_timing()
        && dev.current_dv_timing().ok() != Some(t)
    {
        let _ = dev.set_dv_timing(&t);
    }
    let fmt = match dev.set_format(cam.fourcc, cam.width, cam.height) {
        Ok(f) => f,
        Err(e) => return Exit::Lost(format!("{name}: set format: {e}")),
    };
    if fmt.fourcc != cam.fourcc {
        return Exit::Lost(format!("{name}: {} does not offer {} (got {})", info.path, v4l2::format_name(cam.fourcc), v4l2::format_name(fmt.fourcc)));
    }
    if (fmt.width, fmt.height) != (cam.width, cam.height) {
        hub.log("warn", OWNER, format!("{name}: asked {}x{}, device gives {}x{}", cam.width, cam.height, fmt.width, fmt.height));
    }
    let nominal = dev.set_fps(cam.fps).ok().flatten().unwrap_or(cam.fps);
    if (nominal - cam.fps).abs() > 0.5 {
        hub.log("warn", OWNER, format!("{name}: asked {} fps, device runs {nominal:.2}", cam.fps));
    }
    let _ = dev.subscribe_source_change();

    let mut bound = bind_controls(hub, def, &dev);
    let (w, h) = (fmt.width, fmt.height);
    let is_mjpeg = cam.fourcc == v4l2::PIX_MJPEG;
    let (slot_fmt, slot_stride) = if is_mjpeg { (PixelFormat::Rgba8, w * 4) } else { (PixelFormat::Yuyv, fmt.bytesperline.max(w * 2)) };
    {
        let mut i = st.info.lock();
        i.path = info.path.clone();
        i.identity = info.identity.clone();
        i.format = if is_mjpeg { "rgba".into() } else { "yuyv".into() };
        i.width = w;
        i.height = h;
        i.matrix = fmt.matrix().into();
        i.range = fmt.range().into();
        i.nominal_fps = nominal;
        i.decoder = if is_mjpeg { "turbojpeg".into() } else { "passthrough".into() };
        i.error.clear();
        i.controls = bound.iter().map(|b| b.ctrl.clone()).collect();
    }
    pubs.set("path", Value::Str(info.path.clone()));
    pubs.set("width", Value::Int(w as i64));
    pubs.set("height", Value::Int(h as i64));
    pubs.set("format", Value::Str(if is_mjpeg { "rgba" } else { "yuyv" }.into()));
    pubs.set("matrix", Value::Str(if is_mjpeg { "" } else { fmt.matrix() }.into()));
    pubs.set("range", Value::Str(if is_mjpeg { "" } else { fmt.range() }.into()));
    pubs.set("decoder", Value::Str(if is_mjpeg { "turbojpeg" } else { "passthrough" }.into()));
    pubs.set("error", Value::Str(String::new()));

    let mut decoder = if is_mjpeg {
        match MjpegDecoder::new() {
            Ok(d) => Some(d),
            Err(e) => return Exit::Lost(format!("{name}: turbojpeg: {e}")),
        }
    } else {
        None
    };
    // Apply configured/overridden control values before the first frame.
    apply_controls(hub, &name, &dev, &mut bound, st);

    let mut stream = match Stream::start(&dev, cam.buffers) {
        Ok(s) => s,
        Err(e) => return Exit::Lost(format!("{name}: start streaming: {e}")),
    };
    hub.log(
        "info",
        OWNER,
        format!(
            "{name}: capturing {} {}x{} @ {nominal:.2} fps ({} buffers) from {} [{}]",
            v4l2::format_name(cam.fourcc),
            w,
            h,
            stream.buffer_count(),
            info.path,
            info.identity
        ),
    );
    st.capturing.store(true, Ordering::Relaxed);
    pubs.set("capturing", Value::Bool(true));

    let mut det = SignalDetector::new(cam.signal.timeout_ms, cam.signal.black_level, cam.signal.hold_ms);
    let mut fps = FpsMeter::default();
    let mut drops = DropCounter::default();
    let mut cpu = CpuMeter::default();
    let mut corrupt: u64 = 0;
    let mut last_ctrl = 0u64;
    let mut last_stats = 0u64;
    let base_dropped = st.dropped.load(Ordering::Relaxed);
    let mut signal = false;
    let frame_bytes = (slot_stride * h) as usize;
    // Settings → Devices thumbnails come from this thread while the camera is captured.
    let yuv = preview::Yuv::new(fmt.matrix(), fmt.range());

    loop {
        let ready = match stream.wait(100) {
            Ok(r) => r,
            Err(e) => return Exit::Lost(format!("{name}: poll: {e}")),
        };
        if ready.event {
            loop {
                match dev.dequeue_event() {
                    Ok(Some(ev)) if ev == v4l2::sys::V4L2_EVENT_SOURCE_CHANGE => {
                        hub.log("info", OWNER, format!("{name}: input timing changed; restarting capture"));
                        return Exit::Reopen;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        }
        if ready.frame {
            loop {
                let f = match stream.dequeue() {
                    Ok(Some(f)) => f,
                    Ok(None) => break,
                    Err(e) => return Exit::Lost(format!("{name}: dequeue: {e}")),
                };
                drops.frame(f.sequence);
                if f.is_error() {
                    corrupt += 1;
                } else {
                    let now = se_clock::now();
                    let ts = f.monotonic_ns.filter(|t| *t <= now && now - *t < 1_000_000_000).unwrap_or(now);
                    let data = stream.data(&f);
                    let thumb = st.preview.due(now);
                    let ok = match &mut decoder {
                        None => {
                            if data.len() >= frame_bytes || data.len() >= (slot_stride * (h - 1) + w * 2) as usize {
                                let stats = yuyv_stats(data, w, h, slot_stride);
                                writer.write_with(w, h, slot_stride, slot_fmt, ts, |dst| {
                                    copy_rows(data, slot_stride as usize, dst, slot_stride as usize, (w * 2) as usize, h as usize);
                                });
                                if thumb {
                                    st.preview.offer(now, |out| preview::yuyv_thumb(data, w, h, slot_stride, yuv, out));
                                }
                                det.frame(now, stats);
                                true
                            } else {
                                false
                            }
                        }
                        Some(dec) => match dec.header(data) {
                            Some((jw, jh)) if (jw, jh) == (w, h) => {
                                let mut stats = Default::default();
                                let mut res = Ok(());
                                writer.write_with(w, h, slot_stride, slot_fmt, ts, |dst| {
                                    res = dec.decode_rgba(data, dst, w, h, slot_stride);
                                    stats = rgba_stats(dst, w, h, slot_stride);
                                    if thumb && res.is_ok() {
                                        st.preview.offer(now, |out| preview::rgba_thumb(dst, w, h, slot_stride, out));
                                    }
                                });
                                det.frame(now, stats);
                                res.is_ok()
                            }
                            _ => false,
                        },
                    };
                    if ok {
                        fps.frame(now);
                        st.frames.fetch_add(1, Ordering::Relaxed);
                    } else {
                        corrupt += 1;
                    }
                }
                if let Err(e) = stream.requeue(f.index) {
                    return Exit::Lost(format!("{name}: requeue: {e}"));
                }
            }
        } else if ready.error {
            return Exit::Lost(format!("{name}: device {} went away", info.path));
        }

        let now = se_clock::now();
        let s = det.tick(now);
        if s != signal {
            signal = s;
            st.signal.store(s, Ordering::Relaxed);
            st.preview.no_signal.store(!s, Ordering::Relaxed);
            pubs.set("signal", Value::Bool(s));
            if !s {
                let ls = det.last_stats;
                hub.log("warn", OWNER, format!("{name}: no signal (luma max {:.2}, sd {:.3})", ls.max, ls.stddev));
            }
        }
        if now.saturating_sub(last_ctrl) >= CONTROL_PERIOD_NS {
            last_ctrl = now;
            apply_controls(hub, &name, &dev, &mut bound, st);
        }
        if now.saturating_sub(last_stats) >= STATS_PERIOD_NS {
            last_stats = now;
            // controls gated by an auto mode someone else changed (v4l2-ctl, camera button)
            if bound.iter().any(|b| b.ctrl.inactive()) {
                refresh_flags(&dev, &mut bound);
            }
            if let Ok(inp) = dev.input_status() {
                det.input(!v4l2::input_has_no_signal(inp));
            }
            if let Some(f) = fps.tick(now) {
                st.set_fps(f);
                pubs.set("fps", Value::Float((f * 100.0).round() / 100.0));
            }
            if let Some(c) = cpu.sample(now) {
                st.set_cpu(c);
                pubs.set("cpu", Value::Float((c * 10.0).round() / 10.0));
            }
            let total = base_dropped + drops.dropped + corrupt;
            st.dropped.store(total, Ordering::Relaxed);
            pubs.set("dropped", Value::Int(total as i64));
        }

        match rx.try_recv() {
            Ok(Cmd::Stop) | Err(TryRecvError::Disconnected) => return Exit::Stop,
            Ok(Cmd::Config(d)) => {
                let restream = d.camera().is_none_or(|c| !c.same_stream(cam));
                *def = d;
                if restream {
                    return Exit::Reopen;
                }
                // new [controls] defaults: re-declare with the device ranges
                bound = bind_controls(hub, def, &dev);
            }
            Ok(Cmd::Reopen) => return Exit::Reopen,
            Ok(Cmd::Rescan) => {
                // the node may now belong to another device (replug order changed)
                match se_devices::find_camera(&cam.device) {
                    Some(i) if i.path == info.path => {}
                    _ => return Exit::Reopen,
                }
            }
            Err(TryRecvError::Empty) => {}
        }
    }
}

/// Enumerate device controls and declare `source.<n>.ctrl.<c>` with the device's ranges.
fn bind_controls(hub: &Hub, def: &SourceDef, dev: &Device) -> Vec<Bound> {
    let Some(cam) = def.camera() else { return Vec::new() };
    let list = match dev.controls() {
        Ok(l) => l,
        Err(e) => {
            hub.log("warn", OWNER, format!("{}: controls: {e}", def.name));
            return Vec::new();
        }
    };
    for k in cam.controls.keys() {
        if !list.iter().any(|c| &c.name == k && controls::exposed(c)) {
            let names: Vec<&str> = list.iter().filter(|c| controls::exposed(c)).map(|c| c.name.as_str()).collect();
            hub.log("warn", OWNER, format!("{}: [controls] {k}: the device has no such control (has: {})", def.name, names.join(", ")));
        }
    }
    let mut out = Vec::new();
    for c in list.into_iter().filter(controls::exposed) {
        let address = format!("source.{}.ctrl.{}", def.name, c.name);
        let initial = cam.controls.get(&c.name).map(|v| Value::from(v.clone()));
        if let Some(v) = &initial
            && controls::to_device(&c, v).is_none()
        {
            hub.log("warn", OWNER, format!("{}: [controls] {} = {v} does not fit {} (range {}..{})", def.name, c.name, c.label, c.min, c.max));
        }
        hub.declare(&address, controls::meta(&c, initial.as_ref(), OWNER));
        if c.read_only()
            && let Some(v) = c.value
        {
            hub.publish(&address, controls::from_device(&c, v));
        }
        out.push(Bound { applied: c.value, ctrl: c, address, seen: Value::Null, failed: None });
    }
    out
}

/// Write resolved control values that differ from what the device has. Auto modes go first,
/// then control flags are refreshed so newly activated manual controls get their values.
fn apply_controls(hub: &Hub, name: &str, dev: &Device, bound: &mut [Bound], st: &Status) {
    let snap = hub.snapshot.load();
    let mut auto_changed = false;
    let mut any = false;
    for auto_pass in [true, false] {
        if !auto_pass && auto_changed {
            refresh_flags(dev, bound);
        }
        for b in bound.iter_mut().filter(|b| b.ctrl.is_auto_mode() == auto_pass) {
            if b.ctrl.read_only() {
                continue;
            }
            let Some(v) = snap.get(&b.address) else { continue };
            if *v == b.seen && b.failed.is_none() {
                continue;
            }
            if *v != b.seen {
                b.seen = v.clone();
                b.failed = None;
            }
            let Some(raw) = controls::to_device(&b.ctrl, v) else { continue };
            if b.ctrl.inactive() {
                // gated by an auto mode: retry once the mode changes (refresh_flags)
                b.seen = Value::Null;
                continue;
            }
            if b.applied == Some(raw) || b.failed == Some(raw) {
                continue;
            }
            match dev.set_control(b.ctrl.id, raw, b.ctrl.kind == v4l2::ControlKind::Integer64) {
                Ok(()) => {
                    let actual = dev.get_control(b.ctrl.id, b.ctrl.kind == v4l2::ControlKind::Integer64).unwrap_or(raw);
                    b.applied = Some(actual);
                    b.ctrl.value = Some(actual);
                    any = true;
                    auto_changed |= auto_pass;
                    tracing::debug!("{name}: {} = {raw}", b.ctrl.name);
                }
                Err(e) => {
                    b.failed = Some(raw);
                    hub.log("warn", OWNER, format!("{name}: set {} = {raw}: {e}", b.ctrl.name));
                }
            }
        }
    }
    if auto_changed {
        refresh_flags(dev, bound);
    }
    if any || auto_changed {
        st.info.lock().controls = bound.iter().map(|b| b.ctrl.clone()).collect();
    }
}

fn refresh_flags(dev: &Device, bound: &mut [Bound]) {
    let Ok(list) = dev.controls() else { return };
    for b in bound.iter_mut() {
        if let Some(c) = list.iter().find(|c| c.id == b.ctrl.id) {
            if c.flags != b.ctrl.flags {
                b.failed = None;
            }
            b.ctrl.flags = c.flags;
            b.ctrl.value = c.value;
            b.applied = c.value;
        }
    }
}
