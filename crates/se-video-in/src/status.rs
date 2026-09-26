//! Shared per-source status (worker thread → queries/health) and the readback publisher.

use parking_lot::Mutex;
use se_devices::v4l2::Control;
use se_hub::Hub;
use se_proto::{Meta, Value, ValueType};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const OWNER: &str = "video-in";

#[derive(Clone, Debug, Default)]
pub struct Info {
    /// Device node or media path currently open.
    pub path: String,
    /// Stable identity of the open camera.
    pub identity: String,
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub matrix: String,
    pub range: String,
    /// Configured frame rate the driver accepted.
    pub nominal_fps: f64,
    pub decoder: String,
    pub error: String,
    /// Device controls (current values), for the `sources` query.
    pub controls: Vec<Control>,
}

#[derive(Default)]
pub struct Status {
    pub capturing: AtomicBool,
    pub signal: AtomicBool,
    pub missing: AtomicBool,
    fps_milli: AtomicU64,
    cpu_milli: AtomicU64,
    pub dropped: AtomicU64,
    pub frames: AtomicU64,
    pub info: Mutex<Info>,
}

impl Status {
    pub fn fps(&self) -> f64 {
        self.fps_milli.load(Ordering::Relaxed) as f64 / 1000.0
    }
    pub fn set_fps(&self, f: f64) {
        self.fps_milli.store((f * 1000.0).round() as u64, Ordering::Relaxed);
    }
    pub fn cpu(&self) -> f64 {
        self.cpu_milli.load(Ordering::Relaxed) as f64 / 1000.0
    }
    pub fn set_cpu(&self, f: f64) {
        self.cpu_milli.store((f * 1000.0).round() as u64, Ordering::Relaxed);
    }
}

/// Readback addresses every source has.
pub fn declare_readbacks(hub: &Hub, name: &str, kind: &str) {
    let b = format!("source.{name}");
    let ro = |m: Meta, d: &str| m.readonly().owner(OWNER).describe(d);
    hub.declare(&format!("{b}.kind"), ro(Meta::string(kind), "camera | file"));
    hub.declare(&format!("{b}.label"), ro(Meta::string(""), "display name"));
    hub.declare(&format!("{b}.device"), ro(Meta::string(""), "configured device identity or media file"));
    hub.declare(&format!("{b}.path"), ro(Meta::string(""), "open device node or media path"));
    hub.declare(&format!("{b}.capturing"), ro(Meta::boolean(false), "frames are being captured/decoded into the video slot"));
    hub.declare(&format!("{b}.signal"), ro(Meta::boolean(false), "live picture (not no-signal, black, or timed out)"));
    hub.declare(&format!("{b}.fps"), ro(Meta::float(0.0, [0.0, 240.0]), "measured frames per second").unit("fps"));
    hub.declare(&format!("{b}.dropped"), ro(Meta::int(0, [0.0, 9.0e15]), "dropped frames since start"));
    hub.declare(&format!("{b}.width"), ro(Meta::int(0, [0.0, 16384.0]), "frame width").unit("px"));
    hub.declare(&format!("{b}.height"), ro(Meta::int(0, [0.0, 16384.0]), "frame height").unit("px"));
    hub.declare(&format!("{b}.format"), ro(Meta::string(""), "slot pixel format: yuyv | rgba | nv12"));
    hub.declare(&format!("{b}.matrix"), ro(Meta::string(""), "YCbCr matrix for YUV slots: bt601 | bt709"));
    hub.declare(&format!("{b}.range"), ro(Meta::string(""), "YCbCr range for YUV slots: full | limited"));
    hub.declare(&format!("{b}.cpu"), ro(Meta::float(0.0, [0.0, 2400.0]), "capture/decode thread CPU").unit("%"));
    hub.declare(&format!("{b}.error"), ro(Meta::string(""), "last error"));
    hub.declare(
        &format!("{b}.info"),
        Meta { ty: ValueType::Map, readonly: true, owner: Some(OWNER.into()), description: Some("source summary".into()), ..Default::default() },
    );
}

/// Publishes readbacks only when they change (called from worker threads; never blocks).
pub struct Publisher {
    hub: Arc<Hub>,
    base: String,
    last: Vec<(String, Value)>,
}

impl Publisher {
    pub fn new(hub: Arc<Hub>, name: &str) -> Publisher {
        Publisher { hub, base: format!("source.{name}"), last: Vec::with_capacity(24) }
    }

    pub fn set(&mut self, field: &str, v: Value) {
        if let Some((_, old)) = self.last.iter_mut().find(|(f, _)| f == field) {
            if *old == v {
                return;
            }
            *old = v.clone();
        } else {
            self.last.push((field.to_string(), v.clone()));
        }
        self.hub.publish(&format!("{}.{field}", self.base), v);
    }

    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }
}

/// CPU time of the calling thread in ns.
pub fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid timespec pointer; the clock always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Thread CPU usage in percent of one core over 1 s windows.
#[derive(Default)]
pub struct CpuMeter {
    start: Option<(u64, u64)>,
}

impl CpuMeter {
    pub fn sample(&mut self, now: u64) -> Option<f64> {
        let cpu = thread_cpu_ns();
        match self.start {
            Some((t0, c0)) if now > t0 && now - t0 >= 1_000_000_000 => {
                self.start = Some((now, cpu));
                Some((cpu - c0) as f64 * 100.0 / (now - t0) as f64)
            }
            Some(_) => None,
            None => {
                self.start = Some((now, cpu));
                None
            }
        }
    }
}
