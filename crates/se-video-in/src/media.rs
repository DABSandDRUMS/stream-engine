//! Media-file sources: FFmpeg demux + decode (NVDEC through the `*_cuvid` decoders when
//! available, CPU otherwise), converted into NV12 (opaque video) or RGBA (straight alpha, and
//! RGB/palette pictures at full color) video slots and paced against the master clock (loop,
//! rate, pause, seek). A file with a single picture (PNG, JPEG, WebP, a one-frame GIF) is shown
//! once and held; animated GIFs loop like videos. Each file also publishes its timeline
//! identity (`file:<hash>`, ISRC from its tags) and feeds timelines following it the exact
//! position of every shown frame (§2.7).

use crate::config::{FileDef, HwAccel, SourceDef};
use crate::status::{CpuMeter, OWNER, Publisher, Status};
use crossbeam_channel::{Receiver, RecvTimeoutError, TryRecvError};
use ff::codec::packet::Mut as _;
use ff::ffi;
use ffmpeg_next as ff;
use parking_lot::Mutex;
use se_clock::timecode::{ObsKind, ObsThrottle, TcObs};
use se_core::Input;
use se_core::timeline::{local_media_key, normalize_isrc};
use se_hub::Hub;
use se_hub::media::{PixelFormat, VideoWriter};
use se_proto::{Event, Origin, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

pub enum Cmd {
    Stop,
    Config(Arc<SourceDef>),
    Restart,
    Seek(f64),
}

enum Exit {
    Stop,
    Reopen,
    Lost(String),
}

static INIT: std::sync::Once = std::sync::Once::new();

pub fn init() {
    INIT.call_once(|| {
        let _ = ff::init();
        ff::util::log::set_level(ff::util::log::Level::Error);
    });
}

/// Start the worker; the thread hands the slot writer back when it stops.
pub fn spawn(
    hub: Arc<Hub>,
    def: Arc<SourceDef>,
    status: Arc<Status>,
    mut writer: VideoWriter,
    rx: Receiver<Cmd>,
) -> std::io::Result<std::thread::JoinHandle<VideoWriter>> {
    std::thread::Builder::new().name(format!("se-media-{}", def.name)).spawn(move || {
        init();
        run(hub, def, status, &mut writer, rx);
        writer
    })
}

fn run(hub: Arc<Hub>, mut def: Arc<SourceDef>, st: Arc<Status>, writer: &mut VideoWriter, rx: Receiver<Cmd>) {
    let mut pubs = Publisher::new(hub.clone(), &def.name);
    loop {
        match play(&hub, &mut def, &st, writer, &rx, &mut pubs) {
            Exit::Stop => {
                idle(&st, &mut pubs, "");
                return;
            }
            Exit::Reopen => continue,
            Exit::Lost(e) => {
                if st.info.lock().error != e {
                    hub.log("error", OWNER, e.clone());
                }
                idle(&st, &mut pubs, &e);
                match rx.recv_timeout(Duration::from_secs(3)) {
                    Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                    Ok(Cmd::Config(d)) => def = d,
                    _ => {}
                }
            }
        }
    }
}

fn idle(st: &Status, pubs: &mut Publisher, err: &str) {
    st.capturing.store(false, Ordering::Relaxed);
    st.signal.store(false, Ordering::Relaxed);
    st.set_fps(0.0);
    st.info.lock().error = err.to_string();
    pubs.set("capturing", Value::Bool(false));
    pubs.set("signal", Value::Bool(false));
    pubs.set("playing", Value::Bool(false));
    pubs.set("fps", Value::Float(0.0));
    pubs.set("error", Value::Str(err.to_string()));
}

/// `*_cuvid` (NVDEC) decoder name for a codec, if FFmpeg has one.
fn cuvid_name(id: ff::codec::Id) -> Option<&'static str> {
    use ff::codec::Id;
    Some(match id {
        Id::H264 => "h264_cuvid",
        Id::HEVC => "hevc_cuvid",
        Id::AV1 => "av1_cuvid",
        Id::VP9 => "vp9_cuvid",
        Id::VP8 => "vp8_cuvid",
        Id::MPEG2VIDEO => "mpeg2_cuvid",
        Id::MPEG4 => "mpeg4_cuvid",
        Id::MJPEG => "mjpeg_cuvid",
        Id::VC1 => "vc1_cuvid",
        _ => return None,
    })
}

fn pix_has_alpha(p: ff::format::Pixel) -> bool {
    pix_flags(p) & ffi::AV_PIX_FMT_FLAG_ALPHA as u64 != 0
}

/// RGB or palette pixels (pictures, GIFs): kept as RGBA, since NV12 would blur colored edges and
/// re-encode them with a guessed matrix.
fn pix_is_rgb(p: ff::format::Pixel) -> bool {
    pix_flags(p) & (ffi::AV_PIX_FMT_FLAG_RGB | ffi::AV_PIX_FMT_FLAG_PAL) as u64 != 0
}

fn pix_flags(p: ff::format::Pixel) -> u64 {
    // SAFETY: descriptors are static tables owned by libavutil.
    p.descriptor().map_or(0, |d| unsafe { (*d.as_ptr()).flags })
}

fn open_decoder(stream: &ff::format::stream::Stream, hw: HwAccel, alpha_side: bool) -> Result<(ff::decoder::Video, String), String> {
    let params = stream.parameters();
    let id = params.id();
    let mut tried = Vec::new();
    // Alpha in WebM lives in side data only libvpx decodes; NVDEC drops alpha.
    if alpha_side {
        let name = if id == ff::codec::Id::VP8 { "libvpx" } else { "libvpx-vp9" };
        if let Some(codec) = ff::decoder::find_by_name(name) {
            let ctx = ff::codec::context::Context::from_parameters(params.clone()).map_err(|e| e.to_string())?;
            match ctx.decoder().open_as(codec).and_then(|o| o.video()) {
                Ok(d) => return Ok((d, name.into())),
                Err(e) => tried.push(format!("{name}: {e}")),
            }
        }
    }
    if hw != HwAccel::None && !alpha_side {
        if let Some(name) = cuvid_name(id)
            && let Some(codec) = ff::decoder::find_by_name(name)
        {
            let ctx = ff::codec::context::Context::from_parameters(params.clone()).map_err(|e| e.to_string())?;
            match ctx.decoder().open_as(codec).and_then(|o| o.video()) {
                Ok(d) => return Ok((d, name.into())),
                Err(e) => tried.push(format!("{name}: {e}")),
            }
        }
        if hw == HwAccel::Cuda {
            return Err(format!("NVDEC decode unavailable for {id:?} ({})", tried.join("; ")));
        }
    }
    let mut ctx = ff::codec::context::Context::from_parameters(params).map_err(|e| e.to_string())?;
    ctx.set_threading(ff::codec::threading::Config { kind: ff::codec::threading::Type::Frame, count: 0 });
    let codec = ff::decoder::find(id).ok_or_else(|| format!("no decoder for {id:?}"))?;
    let d = ctx.decoder().open_as(codec).and_then(|o| o.video()).map_err(|e| format!("open {id:?} decoder: {e}"))?;
    Ok((d, codec.name().to_string()))
}

/// Reused swscale context (conversion straight into the slot buffer).
struct Scaler {
    ctx: *mut ffi::SwsContext,
    key: (i32, u32, u32, i32, i32),
}

impl Drop for Scaler {
    fn drop(&mut self) {
        // SAFETY: allocated by sws_getContext.
        unsafe { ffi::sws_freeContext(self.ctx) };
    }
}

impl Scaler {
    fn get(slot: &mut Option<Scaler>, src: ffi::AVPixelFormat, w: u32, h: u32, dst: ffi::AVPixelFormat, bt709: bool, full: bool) -> Option<&mut Scaler> {
        let key = (src as i32, w, h, dst as i32, (bt709 as i32) | ((full as i32) << 1));
        if slot.as_ref().is_none_or(|s| s.key != key) {
            *slot = None;
            // SAFETY: plain constructor call; NULL on failure.
            let ctx = unsafe {
                ffi::sws_getContext(
                    w as i32,
                    h as i32,
                    src,
                    w as i32,
                    h as i32,
                    dst,
                    ffi::SwsFlags::SWS_BILINEAR as i32,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            if ctx.is_null() {
                return None;
            }
            if dst == ffi::AVPixelFormat::AV_PIX_FMT_RGBA {
                let cs = if bt709 { ffi::SWS_CS_ITU709 } else { ffi::SWS_CS_DEFAULT };
                // SAFETY: valid context and static coefficient tables.
                unsafe {
                    let t = ffi::sws_getCoefficients(cs);
                    ffi::sws_setColorspaceDetails(ctx, t, full as i32, t, 1, 0, 1 << 16, 1 << 16);
                }
            }
            *slot = Some(Scaler { ctx, key });
        }
        slot.as_mut()
    }

    /// Convert `frame` into `dst` (RGBA or NV12 with `stride`).
    fn run(&mut self, frame: &ff::frame::Video, dst: &mut [u8], stride: u32, h: u32, nv12: bool) {
        // SAFETY: frame planes/linesizes come from FFmpeg; `dst` is sized by the slot writer
        // for this format (Y plane + UV plane for NV12).
        unsafe {
            let f = &*frame.as_ptr();
            let base = dst.as_mut_ptr();
            let planes: [*mut u8; 4] =
                [base, if nv12 { base.add((stride * h) as usize) } else { std::ptr::null_mut() }, std::ptr::null_mut(), std::ptr::null_mut()];
            let strides: [i32; 4] = [stride as i32, if nv12 { stride as i32 } else { 0 }, 0, 0];
            ffi::sws_scale(self.ctx, f.data.as_ptr() as *const *const u8, f.linesize.as_ptr(), 0, h as i32, planes.as_ptr(), strides.as_ptr());
        }
    }
}

/// Copy an NV12 frame (as produced by NVDEC) into the slot layout.
fn copy_nv12(frame: &ff::frame::Video, dst: &mut [u8], stride: usize, w: usize, h: usize) {
    let (ys, uvs) = (frame.stride(0), frame.stride(1));
    let (y, uv) = (frame.data(0), frame.data(1));
    for r in 0..h {
        dst[r * stride..r * stride + w].copy_from_slice(&y[r * ys..r * ys + w]);
    }
    let off = stride * h;
    for r in 0..h.div_ceil(2) {
        dst[off + r * stride..off + r * stride + w].copy_from_slice(&uv[r * uvs..r * uvs + w]);
    }
}

struct Pacing {
    /// Master-clock time of `base_pts` at the current rate.
    base_clock: u64,
    base_pts: f64,
    /// Rebase on the next frame (start, loop, seek, resume): its time becomes this clock.
    pending: Option<u64>,
    rate: f64,
}

impl Pacing {
    fn due(&mut self, pts: f64) -> u64 {
        if let Some(c) = self.pending.take() {
            self.base_clock = c;
            self.base_pts = pts;
        }
        let d = (pts - self.base_pts) / self.rate;
        (self.base_clock as i64 + (d * 1e9) as i64).max(0) as u64
    }
}

fn play(hub: &Arc<Hub>, def: &mut Arc<SourceDef>, st: &Status, writer: &mut VideoWriter, rx: &Receiver<Cmd>, pubs: &mut Publisher) -> Exit {
    let Some(file): Option<FileDef> = def.file().cloned() else { return Exit::Stop };
    let name = def.name.clone();
    if !file.path.exists() {
        return Exit::Lost(format!("{name}: media file {} not found", file.path.display()));
    }
    let mut ictx = match ff::format::input(&file.path) {
        Ok(i) => i,
        Err(e) => return Exit::Lost(format!("{name}: open {}: {e}", file.path.display())),
    };
    let (idx, tb, fps_nominal, alpha_side) = match ictx.streams().best(ff::media::Type::Video) {
        Some(s) => {
            let r = s.avg_frame_rate();
            let fps = if r.denominator() > 0 && r.numerator() > 0 { r.numerator() as f64 / r.denominator() as f64 } else { 30.0 };
            let alpha = s.metadata().get("alpha_mode").is_some_and(|v| v == "1");
            (s.index(), s.time_base(), fps, alpha)
        }
        None => return Exit::Lost(format!("{name}: {} has no video stream", file.path.display())),
    };
    let duration = if ictx.duration() > 0 { ictx.duration() as f64 / ffi::AV_TIME_BASE as f64 } else { 0.0 };
    let (mut dec, decoder_name) = {
        let stream = ictx.stream(idx).expect("best stream index");
        match open_decoder(&stream, file.hwaccel, alpha_side) {
            Ok(d) => d,
            Err(e) => return Exit::Lost(format!("{name}: {e}")),
        }
    };
    let (w, h) = (dec.width(), dec.height());
    if w == 0 || h == 0 {
        return Exit::Lost(format!("{name}: video has no size"));
    }
    let rgba = alpha_side || pix_has_alpha(dec.format()) || pix_is_rgb(dec.format());
    let (slot_fmt, stride) = if rgba { (PixelFormat::Rgba8, w * 4) } else { (PixelFormat::Nv12, (w + 1) & !1) };
    let bt709 = match dec.color_space() {
        ff::color::Space::BT709 => true,
        ff::color::Space::Unspecified => h >= 720,
        _ => false,
    };
    let full = dec.color_range() == ff::color::Range::JPEG;
    {
        let mut i = st.info.lock();
        i.path = file.path.display().to_string();
        i.identity = file.rel.clone();
        i.format = if rgba { "rgba".into() } else { "nv12".into() };
        i.width = w;
        i.height = h;
        i.matrix = if bt709 { "bt709" } else { "bt601" }.into();
        i.range = if full { "full" } else { "limited" }.into();
        i.nominal_fps = fps_nominal;
        i.decoder = decoder_name.clone();
        i.error.clear();
    }
    hub.log("info", OWNER, format!("{name}: playing {} ({w}x{h} @ {fps_nominal:.2} fps, {decoder_name}, {})", file.rel, if rgba { "rgba" } else { "nv12" }));
    pubs.set("path", Value::Str(file.path.display().to_string()));
    pubs.set("width", Value::Int(w as i64));
    pubs.set("height", Value::Int(h as i64));
    pubs.set("format", Value::Str(if rgba { "rgba" } else { "nv12" }.into()));
    pubs.set(
        "matrix",
        Value::Str(
            if rgba {
                ""
            } else if bt709 {
                "bt709"
            } else {
                "bt601"
            }
            .into(),
        ),
    );
    pubs.set(
        "range",
        Value::Str(
            if rgba {
                ""
            } else if full {
                "full"
            } else {
                "limited"
            }
            .into(),
        ),
    );
    pubs.set("decoder", Value::Str(decoder_name.clone()));
    pubs.set("duration", Value::Float((duration * 1000.0).round() / 1000.0));
    pubs.set("error", Value::Str(String::new()));
    let mut tags: Vec<(String, String)> = ictx.metadata().iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    for s in ictx.streams() {
        tags.extend(s.metadata().iter().map(|(k, v)| (k.to_string(), v.to_string())));
    }
    let isrc = isrc_from_tags(tags.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    pubs.set("media", Value::Str(String::new()));
    let mut feed = Feed::new(hub.clone(), &name, &file.path, isrc);

    let a_paused = format!("source.{name}.paused");
    let a_rate = format!("source.{name}.rate");
    let a_loop = format!("source.{name}.loop");
    let tb_s = tb.numerator() as f64 / tb.denominator().max(1) as f64;
    let frame_ns = (1e9 / fps_nominal.max(1.0)) as u64;
    let mut frame = ff::frame::Video::empty();
    let mut pkt = ff::Packet::empty();
    let mut scaler: Option<Scaler> = None;
    let mut pace = Pacing { base_clock: 0, base_pts: 0.0, pending: Some(se_clock::now()), rate: file.rate };
    let mut eof_sent = false;
    let mut ended = false;
    let mut seek_target: Option<f64> = None;
    let mut last_pts = 0.0f64;
    let mut last_due = se_clock::now();
    let mut fpsm = crate::frame::FpsMeter::default();
    let mut cpu = CpuMeter::default();
    let mut dropped: u64 = 0;
    let mut last_pub = 0u64;
    let mut paused_since: Option<u64> = None;
    // frames decoded since the start of the file (None after a seek into the middle): a whole
    // pass with one frame is a still picture, held instead of looped
    let mut pass: Option<u64> = Some(0);
    let mut any_frame = false;
    let mut still = false;
    st.capturing.store(true, Ordering::Relaxed);
    pubs.set("capturing", Value::Bool(true));
    pubs.set("playing", Value::Bool(true));

    loop {
        feed.poll(pubs);
        // commands
        match rx.try_recv() {
            Ok(Cmd::Stop) | Err(TryRecvError::Disconnected) => return Exit::Stop,
            Ok(Cmd::Config(d)) => {
                let reopen = d.file().is_none_or(|f| f.path != file.path || f.hwaccel != file.hwaccel);
                *def = d;
                if reopen {
                    return Exit::Reopen;
                }
            }
            Ok(Cmd::Restart) => seek_target = Some(0.0),
            Ok(Cmd::Seek(s)) => seek_target = Some(s.max(0.0)),
            Err(TryRecvError::Empty) => {}
        }
        let (paused, rate, looping) = {
            let snap = hub.snapshot.load();
            (
                snap.bool(&a_paused),
                snap.f32(&a_rate).map(|r| (r as f64).clamp(0.05, 8.0)).unwrap_or(file.rate),
                snap.get(&a_loop).map(Value::truthy).unwrap_or(file.looping),
            )
        };
        if (rate - pace.rate).abs() > 1e-6 {
            pace.rate = rate;
            pace.pending = Some(se_clock::now());
            pace.base_pts = last_pts;
        }
        if let Some(target) = seek_target.take().filter(|_| !still) {
            let ts = (target * ffi::AV_TIME_BASE as f64) as i64;
            if let Err(e) = ictx.seek(ts, ..ts) {
                hub.log("warn", OWNER, format!("{name}: seek {target:.2}s: {e}"));
            }
            dec.flush();
            eof_sent = false;
            ended = false;
            pace.pending = Some(se_clock::now());
            pass = (target == 0.0).then_some(0);
            // decode forward to the exact target
            let mut skip_until = Some(target);
            while let Some(t) = skip_until {
                match next_frame(&mut ictx, &mut dec, &mut pkt, &mut frame, idx, &mut eof_sent) {
                    Ok(true) => {
                        any_frame = true;
                        if let Some(n) = &mut pass {
                            *n += 1;
                        }
                        let pts = frame.timestamp().unwrap_or(0) as f64 * tb_s;
                        if pts + 0.5 / fps_nominal >= t {
                            skip_until = None;
                            last_pts = pts;
                            pace.pending = Some(se_clock::now());
                            let due = pace.due(pts);
                            publish(writer, &frame, slot_fmt, stride, w, h, &mut scaler, bt709, full, due);
                            if paused {
                                feed.locate(pts);
                            } else {
                                feed.frame(pts, due);
                            }
                        }
                    }
                    _ => skip_until = None,
                }
            }
            pubs.set("playing", Value::Bool(true));
            continue;
        }
        if paused || ended {
            if paused && paused_since.is_none() {
                paused_since = Some(se_clock::now());
                pubs.set("playing", Value::Bool(false));
                feed.stop(last_pts, false);
            }
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => return Exit::Stop,
                Ok(Cmd::Config(d)) => {
                    let reopen = d.file().is_none_or(|f| f.path != file.path || f.hwaccel != file.hwaccel);
                    *def = d;
                    if reopen {
                        return Exit::Reopen;
                    }
                }
                Ok(Cmd::Restart) => seek_target = Some(0.0),
                Ok(Cmd::Seek(s)) => seek_target = Some(s.max(0.0)),
                Err(RecvTimeoutError::Timeout) => {}
            }
            if !paused && paused_since.take().is_some() && !ended {
                pace.pending = Some(se_clock::now());
                pace.base_pts = last_pts;
                pubs.set("playing", Value::Bool(true));
            }
            continue;
        }

        match next_frame(&mut ictx, &mut dec, &mut pkt, &mut frame, idx, &mut eof_sent) {
            Ok(true) => {
                any_frame = true;
                if let Some(n) = &mut pass {
                    *n += 1;
                }
            }
            Ok(false) => {
                // end of file
                match pass {
                    // a single picture: keep showing it; there is nothing to loop and it never ends
                    Some(1) => {
                        still = true;
                        ended = true;
                        st.set_fps(0.0);
                        pubs.set("fps", Value::Float(0.0));
                        pubs.set("playing", Value::Bool(false));
                        feed.stop(last_pts, true);
                        continue;
                    }
                    Some(0) if !any_frame => return Exit::Lost(format!("{name}: {} has no pictures", file.rel)),
                    // the rewind didn't take (a demuxer that can't seek): start the file over
                    Some(0) => return Exit::Reopen,
                    _ => {}
                }
                if looping {
                    if ictx.seek(0, ..0).is_err() {
                        return Exit::Reopen;
                    }
                    dec.flush();
                    eof_sent = false;
                    pass = Some(0);
                    pace.pending = Some(last_due + (frame_ns as f64 / pace.rate) as u64);
                } else {
                    ended = true;
                    pubs.set("playing", Value::Bool(false));
                    feed.stop(last_pts, true);
                    hub.emit(Event::new("source.ended", Origin::System, Value::map().with("source", name.as_str())));
                }
                continue;
            }
            Err(e) => return Exit::Lost(format!("{name}: decode: {e}")),
        }
        let pts = frame.timestamp().unwrap_or(0) as f64 * tb_s;
        last_pts = pts;
        let due = pace.due(pts);
        let now = se_clock::now();
        let frame_at_rate = (frame_ns as f64 / pace.rate) as u64;
        if now > due + 1_000_000_000 {
            // stalled for > 1 s (suspend, debugger): restart the clock here instead of dropping
            pace.pending = Some(now);
            pace.base_pts = pts;
        } else if now > due + 2 * frame_at_rate {
            dropped += 1;
            continue;
        }
        // wait until due, staying responsive
        loop {
            let now = se_clock::now();
            if now + 500_000 >= due {
                break;
            }
            let wait = ((due - now) / 1_000_000).clamp(1, 10);
            match rx.recv_timeout(Duration::from_millis(wait)) {
                Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => return Exit::Stop,
                Ok(Cmd::Config(d)) => {
                    let reopen = d.file().is_none_or(|f| f.path != file.path || f.hwaccel != file.hwaccel);
                    *def = d;
                    if reopen {
                        return Exit::Reopen;
                    }
                }
                Ok(Cmd::Restart) => {
                    seek_target = Some(0.0);
                    break;
                }
                Ok(Cmd::Seek(s)) => {
                    seek_target = Some(s.max(0.0));
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        if seek_target.is_some() {
            continue;
        }
        publish(writer, &frame, slot_fmt, stride, w, h, &mut scaler, bt709, full, due);
        feed.frame(pts, due);
        last_due = due;
        let now = se_clock::now();
        fpsm.frame(now);
        st.frames.fetch_add(1, Ordering::Relaxed);
        if !st.signal.swap(true, Ordering::Relaxed) {
            pubs.set("signal", Value::Bool(true));
        }
        if now.saturating_sub(last_pub) >= 100_000_000 {
            last_pub = now;
            pubs.set("position", Value::Float((pts * 1000.0).round() / 1000.0));
        }
        if let Some(f) = fpsm.tick(now) {
            st.set_fps(f);
            pubs.set("fps", Value::Float((f * 100.0).round() / 100.0));
            st.dropped.store(dropped, Ordering::Relaxed);
            pubs.set("dropped", Value::Int(dropped as i64));
        }
        if let Some(c) = cpu.sample(now) {
            st.set_cpu(c);
            pubs.set("cpu", Value::Float((c * 10.0).round() / 10.0));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn publish(
    writer: &mut VideoWriter,
    frame: &ff::frame::Video,
    slot_fmt: PixelFormat,
    stride: u32,
    w: u32,
    h: u32,
    scaler: &mut Option<Scaler>,
    bt709: bool,
    full: bool,
    ts: u64,
) {
    let src = frame.format();
    let nv12 = slot_fmt == PixelFormat::Nv12;
    if nv12 && src == ff::format::Pixel::NV12 && frame.width() == w && frame.height() == h {
        writer.write_with(w, h, stride, slot_fmt, ts, |dst| copy_nv12(frame, dst, stride as usize, w as usize, h as usize));
        return;
    }
    let dst_fmt = if nv12 { ffi::AVPixelFormat::AV_PIX_FMT_NV12 } else { ffi::AVPixelFormat::AV_PIX_FMT_RGBA };
    let Some(s) = Scaler::get(scaler, src.into(), frame.width(), frame.height(), dst_fmt, bt709, full) else { return };
    if frame.width() != w || frame.height() != h {
        return;
    }
    writer.write_with(w, h, stride, slot_fmt, ts, |dst| s.run(frame, dst, stride, h, nv12));
}

/// Decode the next video frame; Ok(false) at end of stream.
fn next_frame(
    ictx: &mut ff::format::context::Input,
    dec: &mut ff::decoder::Video,
    pkt: &mut ff::Packet,
    frame: &mut ff::frame::Video,
    idx: usize,
    eof_sent: &mut bool,
) -> Result<bool, ff::Error> {
    loop {
        match dec.receive_frame(frame) {
            Ok(()) => return Ok(true),
            Err(ff::Error::Eof) => return Ok(false),
            Err(ff::Error::Other { errno }) if errno == libc::EAGAIN => {}
            Err(e) => return Err(e),
        }
        if *eof_sent {
            return Ok(false);
        }
        // SAFETY: the packet is ours; unref before reuse so av_read_frame gets a blank packet.
        unsafe { ffi::av_packet_unref(pkt.as_mut_ptr()) };
        match pkt.read(ictx) {
            Ok(()) => {
                if pkt.stream() == idx {
                    match dec.send_packet(pkt) {
                        Ok(()) | Err(ff::Error::InvalidData) => {}
                        Err(ff::Error::Other { errno }) if errno == libc::EAGAIN => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            Err(ff::Error::Eof) => {
                dec.send_eof()?;
                *eof_sent = true;
            }
            Err(ff::Error::InvalidData) => {}
            Err(e) => return Err(e),
        }
    }
}

// ---- timeline identity and position (§2.7) ------------------------------------------------

/// `file:` + the first 16 hex characters of the BLAKE3 hash of the file's bytes: exactly
/// `se_analysis::offline::media_id`, so a timeline's `media = "file:<hash>"` and its offline
/// beat grid share the key. Hashed streaming (videos are large).
pub fn file_media_id(path: &Path) -> std::io::Result<String> {
    let mut h = blake3::Hasher::new();
    h.update_reader(std::fs::File::open(path)?)?;
    Ok(format!("file:{}", &h.finalize().to_hex().as_str()[..16]))
}

/// ISRC from container/stream tags (`ISRC` in Vorbis comments, MP4 and Matroska; `TSRC` in
/// ID3), normalized; the first valid one wins.
pub fn isrc_from_tags<'a>(tags: impl IntoIterator<Item = (&'a str, &'a str)>) -> Option<String> {
    tags.into_iter().filter(|(k, _)| k.eq_ignore_ascii_case("isrc") || k.eq_ignore_ascii_case("tsrc")).find_map(|(_, v)| normalize_isrc(v))
}

/// Media ids by path, valid while size and mtime are unchanged (reopen/loop don't re-hash).
static IDS: Mutex<Vec<(PathBuf, u64, Option<SystemTime>, String)>> = Mutex::new(Vec::new());

/// Timeline keys and the playback (source name) driving each: the first playback of a file
/// holds its key until it stops, so two sources showing the same file don't fight.
static CLAIMS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Hash `path` off the playback thread (cached); the receiver yields the id or `None`.
fn media_id_async(path: PathBuf) -> Receiver<Option<String>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let stamp = std::fs::metadata(&path).ok().map(|m| (m.len(), m.modified().ok()));
    if let Some((len, mtime)) = stamp
        && let Some(id) = IDS.lock().iter().find(|(p, l, m, _)| *p == path && *l == len && *m == mtime).map(|e| e.3.clone())
    {
        let _ = tx.send(Some(id));
        return rx;
    }
    let spawned = std::thread::Builder::new().name("se-media-hash".into()).spawn(move || {
        let id = file_media_id(&path).ok();
        if let (Some(id), Some((len, mtime))) = (&id, stamp) {
            let mut ids = IDS.lock();
            ids.retain(|(p, ..)| *p != path);
            ids.push((path, len, mtime, id.clone()));
        }
        let _ = tx.send(id);
    });
    if spawned.is_err() {
        tracing::warn!(target: "video-in", "media identity: could not start the hashing thread");
    }
    rx
}

fn claim(key: &str, source: &str) -> bool {
    let mut c = CLAIMS.lock();
    match c.iter().find(|(k, _)| k == key) {
        Some((_, owner)) => owner == source,
        None => {
            c.push((key.to_string(), source.to_string()));
            true
        }
    }
}

fn release_claims(source: &str) {
    CLAIMS.lock().retain(|(_, owner)| owner != source);
}

/// Publishes a media file's identity (`source.<n>.media`, `.isrc`) and feeds the timelines
/// following it (`media = "file:<hash>"` / `"isrc:<code>"`) the exact position of every shown
/// frame at its master-clock time, as replayable `Input::Timecode` observations.
struct Feed {
    hub: Arc<Hub>,
    source: String,
    pending: Option<Receiver<Option<String>>>,
    isrc: Option<String>,
    keys: Vec<String>,
    thr: ObsThrottle,
    last: Option<TcObs>,
}

impl Feed {
    fn new(hub: Arc<Hub>, source: &str, path: &Path, isrc: Option<String>) -> Feed {
        Feed {
            hub,
            source: source.to_string(),
            pending: Some(media_id_async(path.to_path_buf())),
            isrc,
            keys: Vec::new(),
            // ≈15 observations/s while running; discontinuities always pass
            thr: ObsThrottle::new(66_000_000, 0.1),
            last: None,
        }
    }

    /// Pick up the identity once hashed.
    fn poll(&mut self, pubs: &mut Publisher) {
        let Some(rx) = &self.pending else { return };
        let id = match rx.try_recv() {
            Ok(id) => id,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => None,
        };
        self.pending = None;
        pubs.set("media", Value::Str(id.clone().unwrap_or_default()));
        pubs.set("isrc", Value::Str(self.isrc.clone().unwrap_or_default()));
        self.keys = id.into_iter().chain(self.isrc.as_ref().map(|c| format!("isrc:{c}"))).map(|m| local_media_key(&m)).collect();
        // a stop/locate that happened while hashing still counts
        if let Some(o) = self.last.take().filter(|o| o.kind != ObsKind::Run) {
            self.send(o);
        }
    }

    fn send(&mut self, o: TcObs) {
        self.last = Some(o);
        if self.keys.is_empty() || !self.thr.pass(&o) {
            return;
        }
        for k in &self.keys {
            if claim(k, &self.source) {
                self.hub.submit(Input::Timecode { source: k.clone(), obs: o });
            }
        }
    }

    /// A frame with presentation time `pts` (s) goes on screen at master-clock `due`.
    fn frame(&mut self, pts: f64, due: u64) {
        self.send(TcObs::run(pts, due));
    }

    /// Parked at `pts` after a seek while paused.
    fn locate(&mut self, pts: f64) {
        self.send(TcObs { seconds: pts, ts: se_clock::now(), kind: ObsKind::Locate, rate: None });
    }

    /// Paused or finished at `pts`; `done` hands the timeline keys back.
    fn stop(&mut self, pts: f64, done: bool) {
        if self.last.is_some_and(|o| o.kind == ObsKind::Stop && o.seconds == pts) {
            return;
        }
        self.send(TcObs { seconds: pts, ts: se_clock::now(), kind: ObsKind::Stop, rate: None });
        if done {
            release_claims(&self.source);
        }
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        if let Some(o) = self.last {
            self.stop(o.seconds, true);
        }
        release_claims(&self.source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_id_matches_the_offline_analysis_key() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("clip.bin");
        // larger than one BLAKE3 chunk and one reader buffer
        let bytes: Vec<u8> = (0..200_003u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        std::fs::write(&p, &bytes).unwrap();
        assert_eq!(file_media_id(&p).unwrap(), se_analysis::offline::media_id(&bytes));
        assert!(file_media_id(&dir.path().join("missing.mp4")).is_err());
    }

    #[test]
    fn isrc_comes_from_either_tag_spelling_and_is_normalized() {
        assert_eq!(isrc_from_tags([("title", "x"), ("TSRC", "us-rc1-76-07839")]), Some("USRC17607839".into()));
        assert_eq!(isrc_from_tags([("isrc", "not an isrc"), ("ISRC", "GBAYE0601498")]), Some("GBAYE0601498".into()), "first valid one");
        assert_eq!(isrc_from_tags([("comment", "USRC17607839")]), None, "only ISRC/TSRC tags");
    }

    #[test]
    fn the_first_playback_of_a_file_holds_its_timeline_key() {
        let key = "media:file:test-claims";
        assert!(claim(key, "a"));
        assert!(claim(key, "a"), "the holder keeps it");
        assert!(!claim(key, "b"), "a second playback of the same file doesn't drive it");
        release_claims("a");
        assert!(claim(key, "b"), "free again once the first stops");
        release_claims("b");
    }
}
