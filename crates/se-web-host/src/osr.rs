//! Off-screen browsers, one per web source: CPU paint → shared-memory frame slots, audio →
//! shared-memory ring, load/crash reporting, and the engine's commands.
//!
//! Threading: commands and most CEF callbacks run on the browser UI thread; the audio
//! callbacks may run elsewhere and frame acknowledgements arrive on the IPC thread, so the
//! per-browser state they touch ([`Shared`]) is behind mutexes. `Browser` handles live in a
//! UI-thread-local map.

use crate::{ipc, task};
use cef::*;
use parking_lot::Mutex;
use se_web::protocol::{self, AUDIO_CHANNELS, AUDIO_RATE, AUDIO_RING_SAMPLES, AudioRing, FRAME_SLOTS, FromHost, MAX_FPS, MAX_SIDE, Shm, ToHost};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::os::raw::c_int;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

/// A slot the engine never acknowledged is reclaimed after this long (lost notification).
const STUCK_SLOT_NS: u64 = 1_000_000_000;
/// CEF audio packets of 10 ms at 48 kHz.
const AUDIO_FRAMES_PER_BUFFER: c_int = 480;
/// Page console errors forwarded per browser per window.
const CONSOLE_BUDGET: u32 = 20;
const CONSOLE_WINDOW: Duration = Duration::from_secs(10);

struct Surface {
    generation: u32,
    width: u32,
    height: u32,
    stride: u32,
    shm: Shm,
    /// Paint time of the frame in each slot until the engine acknowledges it (0 = free).
    busy: [u64; FRAME_SLOTS as usize],
}

struct AudioOut {
    ring: AudioRing,
    channels: usize,
}

/// Per-browser state shared between CEF callbacks and the IPC thread.
pub struct Shared {
    id: u32,
    /// `width << 32 | height` of the view.
    size: AtomicU64,
    surface: Mutex<Option<Surface>>,
    surfaces: AtomicU32,
    audio: Mutex<Option<AudioOut>>,
    url: Mutex<String>,
    crashes: Mutex<VecDeque<Instant>>,
    console: Mutex<(Instant, u32)>,
}

impl Shared {
    fn size(&self) -> (u32, u32) {
        let v = self.size.load(Ordering::Relaxed);
        ((v >> 32) as u32, v as u32)
    }

    fn set_size(&self, w: u32, h: u32) {
        let (w, h) = (w.clamp(1, MAX_SIDE), h.clamp(1, MAX_SIDE));
        self.size.store((w as u64) << 32 | h as u64, Ordering::Relaxed);
    }

    fn paint(&self, px: &[u8], width: u32, height: u32) {
        let now = protocol::monotonic_ns();
        let mut g = self.surface.lock();
        if g.as_ref().is_none_or(|s| s.width != width || s.height != height) {
            *g = None;
            let generation = self.surfaces.fetch_add(1, Ordering::Relaxed) + 1;
            let stride = width * 4;
            let bytes = stride as usize * height as usize * FRAME_SLOTS as usize;
            let shm = match Shm::create(&format!("se-web-{}-{generation}", self.id), bytes) {
                Ok(s) => s,
                Err(e) => {
                    ipc::log("error", format!("browser {}: frame surface {width}x{height}: {e}", self.id));
                    return;
                }
            };
            let msg = FromHost::Surface { id: self.id, surface: generation, width, height, stride, slots: FRAME_SLOTS };
            if !ipc::send(&msg, Some(shm.fd())) {
                return;
            }
            *g = Some(Surface { generation, width, height, stride, shm, busy: [0; FRAME_SLOTS as usize] });
        }
        let Some(s) = g.as_mut() else { return };
        let Some(slot) = (0..FRAME_SLOTS as usize).find(|&i| s.busy[i] == 0 || now.saturating_sub(s.busy[i]) > STUCK_SLOT_NS) else {
            // the engine still holds every slot: skip this paint, the next one supersedes it
            return;
        };
        let range = protocol::slot_range(s.stride, s.height, slot as u32);
        if px.len() != range.len() || !s.shm.write_at(range.start, px) {
            return;
        }
        s.busy[slot] = now;
        if !ipc::notify(&FromHost::Frame { id: self.id, surface: s.generation, slot: slot as u32, paint_ns: now }) {
            s.busy[slot] = 0;
        }
    }

    fn frame_done(&self, surface: u32, slot: u32) {
        let mut g = self.surface.lock();
        if let Some(s) = g.as_mut()
            && s.generation == surface
            && let Some(b) = s.busy.get_mut(slot as usize)
        {
            *b = 0;
        }
    }

    fn audio_started(&self, channels: usize) {
        let mut g = self.audio.lock();
        match g.as_mut() {
            Some(a) => a.channels = channels,
            None => match AudioRing::create(&format!("se-web-audio-{}", self.id), AUDIO_RING_SAMPLES, AUDIO_CHANNELS) {
                Ok(ring) => {
                    let msg = FromHost::AudioRing { id: self.id, channels: AUDIO_CHANNELS, rate: AUDIO_RATE, capacity: AUDIO_RING_SAMPLES };
                    if ipc::send(&msg, Some(ring.fd())) {
                        *g = Some(AudioOut { ring, channels });
                    }
                }
                Err(e) => ipc::log("error", format!("browser {}: audio ring: {e}", self.id)),
            },
        }
    }

    fn audio_packet(&self, data: *mut *const f32, frames: c_int) {
        if data.is_null() || frames <= 0 {
            return;
        }
        let g = self.audio.lock();
        let Some(a) = g.as_ref() else { return };
        // SAFETY: CEF passes `channels` planar channel pointers, each with `frames` samples.
        let (left, right) = unsafe { (*data, *data.add(if a.channels > 1 { 1 } else { 0 })) };
        if left.is_null() || right.is_null() {
            return;
        }
        let planes = [left, right];
        // SAFETY: `i >> 1 < frames` for every `i < frames * 2`.
        a.ring.push_with(frames as usize * 2, |i| unsafe { *planes[i & 1].add(i >> 1) });
        drop(g);
        ipc::notify(&FromHost::Audio { id: self.id });
    }

    fn renderer_gone(&self, status: TerminationStatus, code: c_int, text: String) {
        let what = match status {
            TerminationStatus::PROCESS_WAS_KILLED => "killed",
            TerminationStatus::PROCESS_CRASHED => "crashed",
            TerminationStatus::PROCESS_OOM => "out of memory",
            TerminationStatus::LAUNCH_FAILED => "launch failed",
            TerminationStatus::INTEGRITY_FAILURE => "integrity failure",
            _ => "abnormal termination",
        };
        let reason = if text.is_empty() { format!("{what}, code {code}") } else { format!("{what}: {text}") };
        // Back off when a page keeps crashing its renderer: 0.25 s, 0.5 s, 1 s … 30 s.
        let recent = {
            let mut c = self.crashes.lock();
            let now = Instant::now();
            c.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
            c.push_back(now);
            c.len() as u32
        };
        let retry_ms = (250u64 << (recent - 1).min(7)).min(30_000);
        ipc::send(&FromHost::RendererGone { id: self.id, reason, retry_ms }, None);
        let id = self.id;
        task::on_ui_after(retry_ms, move || {
            let url = shared(id).map(|s| s.url.lock().clone());
            if let Some(url) = url {
                with_browser(id, |b| {
                    if let Some(f) = b.main_frame() {
                        f.load_url(Some(&url.as_str().into()));
                    }
                });
            }
        });
    }

    fn console(&self, msg: String) {
        let mut g = self.console.lock();
        let now = Instant::now();
        if now.duration_since(g.0) > CONSOLE_WINDOW {
            *g = (now, 0);
        }
        g.1 += 1;
        if g.1 <= CONSOLE_BUDGET {
            ipc::send(&FromHost::Console { id: self.id, msg }, None);
        }
    }
}

static SHARED: LazyLock<Mutex<HashMap<u32, Arc<Shared>>>> = LazyLock::new(Default::default);

thread_local! {
    static BROWSERS: RefCell<HashMap<u32, Browser>> = RefCell::new(HashMap::new());
    static SHUTTING_DOWN: Cell<bool> = const { Cell::new(false) };
}

fn shared(id: u32) -> Option<Arc<Shared>> {
    SHARED.lock().get(&id).cloned()
}

fn with_browser(id: u32, f: impl FnOnce(&Browser)) {
    BROWSERS.with(|b| {
        if let Some(br) = b.borrow().get(&id) {
            f(br)
        }
    });
}

fn is_main(frame: Option<&mut Frame>) -> bool {
    frame.is_some_and(|f| f.is_main() == 1)
}

fn text(s: Option<&CefString>) -> String {
    s.map(|s| s.to_string()).unwrap_or_default()
}

wrap_render_handler! {
    struct OsrRender {
        s: Arc<Shared>,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(r) = rect {
                let (w, h) = self.s.size();
                *r = Rect { x: 0, y: 0, width: w as c_int, height: h as c_int };
            }
        }

        fn screen_info(&self, _browser: Option<&mut Browser>, screen_info: Option<&mut ScreenInfo>) -> c_int {
            let Some(i) = screen_info else { return 0 };
            let (w, h) = self.s.size();
            i.device_scale_factor = 1.0;
            i.depth = 24;
            i.depth_per_component = 8;
            i.rect = Rect { x: 0, y: 0, width: w as c_int, height: h as c_int };
            i.available_rect = i.rect.clone();
            1
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: c_int,
            height: c_int,
        ) {
            if type_ != PaintElementType::VIEW || buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            // SAFETY: CEF provides a `width * height * 4` byte BGRA buffer valid for this call.
            let px = unsafe { std::slice::from_raw_parts(buffer, width as usize * height as usize * 4) };
            self.s.paint(px, width as u32, height as u32);
        }
    }
}

wrap_audio_handler! {
    struct OsrAudio {
        s: Arc<Shared>,
    }

    impl AudioHandler {
        fn audio_parameters(&self, _browser: Option<&mut Browser>, params: Option<&mut AudioParameters>) -> c_int {
            let Some(p) = params else { return 0 };
            p.sample_rate = AUDIO_RATE as c_int;
            p.channel_layout = ChannelLayout::LAYOUT_STEREO;
            p.frames_per_buffer = AUDIO_FRAMES_PER_BUFFER;
            1
        }

        fn on_audio_stream_started(&self, _browser: Option<&mut Browser>, _params: Option<&AudioParameters>, channels: c_int) {
            self.s.audio_started(channels.max(1) as usize);
        }

        fn on_audio_stream_packet(&self, _browser: Option<&mut Browser>, data: *mut *const f32, frames: c_int, _pts: i64) {
            self.s.audio_packet(data, frames);
        }

        fn on_audio_stream_error(&self, _browser: Option<&mut Browser>, message: Option<&CefString>) {
            ipc::log("warn", format!("browser {}: audio stream error: {}", self.s.id, text(message)));
        }
    }
}

wrap_load_handler! {
    struct OsrLoad {
        s: Arc<Shared>,
    }

    impl LoadHandler {
        fn on_load_start(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, _transition_type: TransitionType) {
            if is_main(frame) {
                ipc::send(&FromHost::Loading { id: self.s.id }, None);
            }
        }

        fn on_load_end(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, http_status_code: c_int) {
            if is_main(frame) {
                ipc::send(&FromHost::Loaded { id: self.s.id, status: http_status_code }, None);
            }
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            // ERR_ABORTED: a newer navigation (reload, crash recovery) replaced this one.
            if is_main(frame) && error_code != Errorcode::ABORTED {
                let msg = FromHost::LoadFailed { id: self.s.id, url: text(failed_url), code: error_code.get_raw(), text: text(error_text) };
                ipc::send(&msg, None);
            }
        }
    }
}

wrap_request_handler! {
    struct OsrRequests {
        s: Arc<Shared>,
    }

    impl RequestHandler {
        fn on_render_process_terminated(
            &self,
            _browser: Option<&mut Browser>,
            status: TerminationStatus,
            error_code: c_int,
            error_string: Option<&CefString>,
        ) {
            self.s.renderer_gone(status, error_code, text(error_string));
        }
    }
}

wrap_life_span_handler! {
    struct OsrLifeSpan {
        s: Arc<Shared>,
    }

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: c_int,
            _target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut c_int>,
        ) -> c_int {
            // Off-screen sources never open windows (e.g. the YouTube logo link).
            1
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            closed(self.s.id);
        }
    }
}

wrap_jsdialog_handler! {
    struct OsrDialogs {}

    impl JsdialogHandler {
        fn on_jsdialog(
            &self,
            _browser: Option<&mut Browser>,
            _origin_url: Option<&CefString>,
            _dialog_type: JsdialogType,
            _message_text: Option<&CefString>,
            _default_prompt_text: Option<&CefString>,
            _callback: Option<&mut JsdialogCallback>,
            suppress_message: Option<&mut c_int>,
        ) -> c_int {
            // Nobody can click an off-screen alert(); never let one block a page.
            if let Some(s) = suppress_message {
                *s = 1;
            }
            0
        }

        fn on_before_unload_dialog(
            &self,
            _browser: Option<&mut Browser>,
            _message_text: Option<&CefString>,
            _is_reload: c_int,
            callback: Option<&mut JsdialogCallback>,
        ) -> c_int {
            if let Some(cb) = callback {
                cb.cont(1, None);
            }
            1
        }
    }
}

wrap_display_handler! {
    struct OsrDisplay {
        s: Arc<Shared>,
    }

    impl DisplayHandler {
        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            level: LogSeverity,
            message: Option<&CefString>,
            source: Option<&CefString>,
            line: c_int,
        ) -> c_int {
            if level.get_raw() >= LogSeverity::ERROR.get_raw() {
                self.s.console(format!("{} ({}:{line})", text(message), text(source)));
            }
            0
        }
    }
}

wrap_client! {
    struct OsrClient {
        render: RenderHandler,
        audio: AudioHandler,
        load: LoadHandler,
        requests: RequestHandler,
        life_span: LifeSpanHandler,
        dialogs: JsdialogHandler,
        display: DisplayHandler,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render.clone())
        }

        fn audio_handler(&self) -> Option<AudioHandler> {
            Some(self.audio.clone())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load.clone())
        }

        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.requests.clone())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span.clone())
        }

        fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
            Some(self.dialogs.clone())
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display.clone())
        }
    }
}

/// CEF is initialized: tell the engine (UI thread, from `on_context_initialized`).
pub fn ready(gpu: bool) {
    let full = std::ffi::CStr::from_bytes_until_nul(cef::sys::CEF_VERSION).map(|c| c.to_string_lossy().into_owned()).unwrap_or_default();
    let (cef_version, chromium) = match full.split_once("+chromium-") {
        Some((c, ch)) => (c.to_string(), ch.to_string()),
        None => (full.clone(), String::new()),
    };
    ipc::send(&FromHost::Hello { protocol: protocol::PROTOCOL_VERSION, cef: cef_version, chromium, pid: std::process::id(), gpu }, None);
}

/// Frame acknowledgement from the engine (IPC thread).
pub fn frame_done(id: u32, surface: u32, slot: u32) {
    if let Some(s) = shared(id) {
        s.frame_done(surface, slot);
    }
}

/// Execute an engine command (UI thread).
pub fn command(msg: ToHost) {
    match msg {
        ToHost::Open { id, url, width, height, fps } => open(id, url, width, height, fps),
        ToHost::Close { id } => with_browser(id, |b| {
            if let Some(h) = b.host() {
                h.close_browser(1);
            }
        }),
        ToHost::Resize { id, width, height } => {
            if let Some(s) = shared(id) {
                s.set_size(width, height);
            }
            with_browser(id, |b| {
                if let Some(h) = b.host() {
                    h.was_resized();
                }
            });
        }
        ToHost::SetFps { id, fps } => with_browser(id, |b| {
            if let Some(h) = b.host() {
                h.set_windowless_frame_rate(fps.clamp(1, MAX_FPS) as c_int);
            }
        }),
        ToHost::Navigate { id, url } => {
            if let Some(s) = shared(id) {
                *s.url.lock() = url.clone();
            }
            with_browser(id, |b| {
                if let Some(f) = b.main_frame() {
                    f.load_url(Some(&url.as_str().into()));
                }
            });
        }
        ToHost::Reload { id } => with_browser(id, |b| b.reload_ignore_cache()),
        ToHost::Ping { seq } => {
            ipc::send(&FromHost::Pong { seq }, None);
        }
        ToHost::FrameDone { id, surface, slot } => frame_done(id, surface, slot),
        ToHost::Shutdown => shutdown(),
    }
}

fn open(id: u32, url: String, width: u32, height: u32, fps: u32) {
    if shared(id).is_some() || SHUTTING_DOWN.get() {
        return;
    }
    let s = Arc::new(Shared {
        id,
        size: AtomicU64::new(0),
        surface: Mutex::new(None),
        surfaces: AtomicU32::new(0),
        audio: Mutex::new(None),
        url: Mutex::new(url.clone()),
        crashes: Mutex::new(VecDeque::new()),
        console: Mutex::new((Instant::now(), 0)),
    });
    s.set_size(width, height);
    SHARED.lock().insert(id, s.clone());
    let mut client = OsrClient::new(
        OsrRender::new(s.clone()),
        OsrAudio::new(s.clone()),
        OsrLoad::new(s.clone()),
        OsrRequests::new(s.clone()),
        OsrLifeSpan::new(s.clone()),
        OsrDialogs::new(),
        OsrDisplay::new(s.clone()),
    );
    let window_info = WindowInfo { windowless_rendering_enabled: 1, runtime_style: RuntimeStyle::ALLOY, ..Default::default() };
    // Fully transparent background: pages without their own background composite as overlays.
    let settings = BrowserSettings { windowless_frame_rate: fps.clamp(1, MAX_FPS) as c_int, background_color: 0, ..Default::default() };
    match browser_host_create_browser_sync(Some(&window_info), Some(&mut client), Some(&url.as_str().into()), Some(&settings), None, None) {
        Some(b) => {
            BROWSERS.with(|m| m.borrow_mut().insert(id, b));
        }
        None => {
            SHARED.lock().remove(&id);
            ipc::send(&FromHost::LoadFailed { id, url, code: -1, text: "CEF could not create the browser".into() }, None);
            ipc::send(&FromHost::Closed { id }, None);
        }
    }
}

fn closed(id: u32) {
    SHARED.lock().remove(&id);
    let empty = BROWSERS.with(|m| {
        let mut m = m.borrow_mut();
        m.remove(&id);
        m.is_empty()
    });
    ipc::send(&FromHost::Closed { id }, None);
    if empty && SHUTTING_DOWN.get() {
        quit_message_loop();
    }
}

/// Close every browser, then leave the message loop (UI thread).
pub fn shutdown() {
    if SHUTTING_DOWN.replace(true) {
        return;
    }
    let browsers: Vec<Browser> = BROWSERS.with(|m| m.borrow().values().cloned().collect());
    if browsers.is_empty() {
        quit_message_loop();
        return;
    }
    for b in browsers {
        if let Some(h) = b.host() {
            h.close_browser(1);
        }
    }
}
