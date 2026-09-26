//! Sources → Files: the project's pictures, videos, sounds, color looks and fonts (the files in
//! `assets/`). Add files by dropping them on the window or with the desktop's file chooser
//! (xdg-desktop-portal), browse them as a list grouped by kind with a picture per file, listen to
//! sounds, rename them (everything that uses a file follows), delete them (with a warning when
//! something still uses them) and make a source from a picture or video. Also home of
//! [`picker`], the compact "choose a file" control other editors use.
//!
//! Engine side: `project.assets`, `project.import`, `project.asset.rename`,
//! `project.asset.delete` (docs/api.md).

use crate::app::App;
use crate::views::live::nice;
use crossbeam_channel::{Receiver, Sender};
use egui::{Align, Align2, Color32, CornerRadius, Layout, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, TextureHandle, Vec2};
use se_proto::Value;
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_medium, font_mono, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, Tone, icon};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FONT_ICON: &str = "\u{f031}";
const UPLOAD: &str = "\u{f093}";

/// What kind of file a [`picker`] offers (or Files shows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Image,
    Video,
    Sound,
    /// Color looks (`.cube` files).
    Lut,
    Font,
    Any,
}

impl MediaKind {
    /// The five real kinds, in the order Files shows them.
    pub const ALL: [MediaKind; 5] = [MediaKind::Image, MediaKind::Video, MediaKind::Sound, MediaKind::Lut, MediaKind::Font];

    /// The engine's name for the kind (also its folder under `assets/`); empty for `Any`.
    pub fn key(self) -> &'static str {
        match self {
            MediaKind::Image => "images",
            MediaKind::Video => "video",
            MediaKind::Sound => "sounds",
            MediaKind::Lut => "luts",
            MediaKind::Font => "fonts",
            MediaKind::Any => "",
        }
    }

    pub fn from_key(k: &str) -> Option<MediaKind> {
        MediaKind::ALL.into_iter().find(|m| m.key() == k)
    }

    /// Plural heading ("Images", "Color looks").
    pub fn label(self) -> &'static str {
        match self {
            MediaKind::Image => "Images",
            MediaKind::Video => "Videos",
            MediaKind::Sound => "Sounds",
            MediaKind::Lut => "Color looks",
            MediaKind::Font => "Fonts",
            MediaKind::Any => "All",
        }
    }

    /// One of them, in a sentence ("a picture").
    fn one(self) -> &'static str {
        match self {
            MediaKind::Image => "picture",
            MediaKind::Video => "video",
            MediaKind::Sound => "sound",
            MediaKind::Lut => "color look",
            MediaKind::Font => "font",
            MediaKind::Any => "file",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            MediaKind::Image => icon::IMAGE,
            MediaKind::Video => icon::FILM,
            MediaKind::Sound => icon::VOLUME,
            MediaKind::Lut => icon::PALETTE,
            MediaKind::Font => FONT_ICON,
            MediaKind::Any => UPLOAD,
        }
    }

    /// File extensions the engine accepts for the kind (mirrors `project.import`).
    fn exts(self) -> &'static [&'static str] {
        match self {
            MediaKind::Image => &["png", "jpg", "jpeg", "webp", "gif", "svg"],
            MediaKind::Video => &["mp4", "mov", "webm", "mkv"],
            MediaKind::Sound => &["wav", "mp3", "ogg", "flac", "opus"],
            MediaKind::Lut => &["cube"],
            MediaKind::Font => &["ttf", "otf"],
            MediaKind::Any => &[],
        }
    }

    fn accepts(self, k: MediaKind) -> bool {
        self == MediaKind::Any || self == k
    }
}

/// One file of the library (`project.assets` row).
#[derive(Clone, Debug)]
struct Asset {
    /// Project-relative path (`assets/sounds/horn.wav`).
    path: String,
    kind: MediaKind,
    /// File stem (`horn`); shown through [`nice`].
    name: String,
    bytes: u64,
    /// Project files that mention it.
    used_by: Vec<String>,
    abs: String,
    modified: i64,
    /// Name to play it with (`audio.play`), when the sound player can use it.
    sound: Option<String>,
}

impl Asset {
    fn parse(v: &Value) -> Option<Asset> {
        let s = |k: &str| v.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
        let path = s("path");
        if path.is_empty() {
            return None;
        }
        Some(Asset {
            kind: MediaKind::from_key(&s("kind"))?,
            name: s("name"),
            bytes: v.get_path("bytes").and_then(Value::as_i64).unwrap_or(0).max(0) as u64,
            used_by: v.get_path("used_by").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|u| u.as_str().map(String::from)).collect(),
            abs: s("abs"),
            modified: v.get_path("modified_ms").and_then(Value::as_i64).unwrap_or(0),
            sound: v.get_path("sound").and_then(Value::as_str).map(String::from),
            path,
        })
    }

    fn title(&self) -> String {
        nice(&self.name)
    }

    fn ext(&self) -> String {
        Path::new(&self.path).extension().and_then(|e| e.to_str()).unwrap_or("").to_uppercase()
    }
}

/// Where an import started (Files, or a picker waiting for its file).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Library,
    Picker(egui::Id),
}

#[derive(Clone, Debug)]
enum ImportState {
    Copying,
    Done,
    Failed(String),
}

#[derive(Clone, Debug)]
struct Import {
    src: String,
    /// The file's own name (for the progress line).
    file: String,
    target: Target,
    state: ImportState,
    at: Instant,
}

/// What the worker made for a file, off the UI thread: a thumbnail (pictures, videos), the
/// length (sounds, videos) and a waveform (sounds).
#[derive(Clone, Default)]
struct Preview {
    tex: Option<TextureHandle>,
    seconds: Option<f32>,
    /// Loudest level per slice, 0..1 (empty = none).
    peaks: Arc<[f32]>,
}

enum Thumb {
    Loading,
    Ready(Preview),
}

struct Job {
    key: String,
    abs: PathBuf,
    kind: MediaKind,
}

struct Made {
    key: String,
    image: Option<egui::ColorImage>,
    seconds: Option<f32>,
    peaks: Vec<f32>,
}

/// Thumbnail cache plus its worker thread (started on first use).
#[derive(Default)]
struct Thumbs {
    tx: Option<Sender<Job>>,
    rx: Option<Receiver<Made>>,
    map: HashMap<String, Thumb>,
}

impl Thumbs {
    fn key(a: &Asset) -> String {
        format!("{}#{}", a.abs, a.modified)
    }

    /// The preview of `a` (None while it's being made), asking the worker the first time.
    fn get(&mut self, ctx: &egui::Context, a: &Asset) -> Option<Preview> {
        if a.abs.is_empty() {
            return Some(Preview::default());
        }
        let key = Thumbs::key(a);
        if !self.map.contains_key(&key) {
            if self.tx.is_none() {
                let (tx, jobs) = crossbeam_channel::unbounded::<Job>();
                let (done, rx) = crossbeam_channel::unbounded::<Made>();
                let ctx = ctx.clone();
                let spawned = std::thread::Builder::new().name("se-ui-thumbs".into()).spawn(move || {
                    while let Ok(j) = jobs.recv() {
                        if done.send(make_preview(j)).is_err() {
                            break;
                        }
                        ctx.request_repaint();
                    }
                });
                if spawned.is_err() {
                    self.map.insert(key, Thumb::Ready(Preview::default()));
                    return Some(Preview::default());
                }
                self.tx = Some(tx);
                self.rx = Some(rx);
            }
            if let Some(tx) = &self.tx {
                let _ = tx.send(Job { key: key.clone(), abs: PathBuf::from(&a.abs), kind: a.kind });
            }
            self.map.insert(key.clone(), Thumb::Loading);
        }
        match self.map.get(&key) {
            Some(Thumb::Ready(p)) => Some(p.clone()),
            _ => None,
        }
    }

    /// Turn finished thumbnails into textures.
    fn drain(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.rx else { return };
        while let Ok(m) = rx.try_recv() {
            let tex = m.image.map(|img| ctx.load_texture(format!("media:{}", m.key), img, egui::TextureOptions::LINEAR));
            self.map.insert(m.key, Thumb::Ready(Preview { tex, seconds: m.seconds, peaks: m.peaks.into() }));
        }
    }
}

/// Longest side of a thumbnail, in pixels.
const THUMB_PX: u32 = 480;
/// Bars in a sound's waveform.
const PEAKS: usize = 64;

fn make_preview(j: Job) -> Made {
    let abs = &j.abs;
    let (image, seconds, peaks) = match j.kind {
        MediaKind::Image => (decode_thumb(abs), None, Vec::new()),
        MediaKind::Video => (video_frame(abs), probe_seconds(abs), Vec::new()),
        MediaKind::Sound => (None, probe_seconds(abs), waveform(abs)),
        _ => (None, None, Vec::new()),
    };
    Made { key: j.key, image, seconds, peaks }
}

/// Peak level per slice of the first ten minutes, decoded by `ffmpeg` at a low rate.
fn waveform(abs: &Path) -> Vec<f32> {
    let Ok(out) = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-t", "600", "-i"])
        .arg(abs)
        .args(["-ac", "1", "-ar", "4000", "-f", "s16le", "-"])
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let n = out.stdout.len() / 2;
    if n < PEAKS {
        return Vec::new();
    }
    let per = n / PEAKS;
    let level = |i: usize| (i16::from_le_bytes([out.stdout[2 * i], out.stdout[2 * i + 1]]) as f32 / 32768.0).abs();
    let peaks: Vec<f32> = (0..PEAKS).map(|k| (k * per..(k + 1) * per).map(level).fold(0.0, f32::max)).collect();
    let top = peaks.iter().copied().fold(0.0, f32::max);
    if top <= 0.0 { Vec::new() } else { peaks.into_iter().map(|p| p / top).collect() }
}

fn to_color_image(img: image::DynamicImage) -> egui::ColorImage {
    let img = img.thumbnail(THUMB_PX, THUMB_PX).to_rgba8();
    egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw())
}

fn decode_thumb(abs: &Path) -> Option<egui::ColorImage> {
    // huge files would stall the worker for seconds; they just show their icon
    if std::fs::metadata(abs).ok()?.len() > 64 << 20 {
        return None;
    }
    let img = image::ImageReader::open(abs).ok()?.with_guessed_format().ok()?.decode().ok()?;
    Some(to_color_image(img))
}

/// A frame one second in (or the first frame of a shorter clip), via `ffmpeg`.
fn video_frame(abs: &Path) -> Option<egui::ColorImage> {
    for at in ["1", "0"] {
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-ss", at, "-i"])
            .arg(abs)
            .args(["-frames:v", "1", "-vf", &format!("scale={THUMB_PX}:-2"), "-f", "image2pipe", "-c:v", "png", "-"])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if out.status.success()
            && !out.stdout.is_empty()
            && let Ok(img) = image::load_from_memory_with_format(&out.stdout, image::ImageFormat::Png)
        {
            return Some(to_color_image(img));
        }
    }
    None
}

fn probe_seconds(abs: &Path) -> Option<f32> {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1"])
        .arg(abs)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse::<f32>().ok().filter(|s| s.is_finite() && *s > 0.0)
}

/// Everything Files and the pickers share (kept in egui's memory, one per window).
#[derive(Default)]
struct Media {
    /// `project.assets` reply the list was built from.
    seq: u64,
    assets: Vec<Asset>,
    asked: Option<Instant>,
    events: u64,
    thumbs: Thumbs,
    chooser: Option<(Receiver<Result<Vec<PathBuf>, String>>, Target, MediaKind)>,
    imports: Vec<Import>,
    /// Files a picker imported and should now select.
    picked: HashMap<egui::Id, String>,
    /// Result of the file chooser, a rename or a delete: (tone, title, body).
    notice: Option<(Tone, String, String)>,
    /// When the notice appeared (good news goes away by itself).
    notice_at: Option<Instant>,
    /// Files: kind filter (0 = all), search, selected file, rename box, delete confirm.
    filter: usize,
    search: String,
    selected: Option<String>,
    rename: String,
    rename_for: String,
    confirm_delete: bool,
    /// Scroll the delete confirmation into view once.
    scroll_confirm: bool,
    /// Sound being previewed: (path, started, seconds).
    playing: Option<(String, Instant, f32)>,
    /// On air: a sound waiting for "Play it on air".
    confirm_play: Option<Asset>,
    picker_search: String,
}

fn shared(ctx: &egui::Context) -> Arc<Mutex<Media>> {
    ctx.data_mut(|d| d.get_temp_mut_or_insert_with(egui::Id::new("media-shared"), || Arc::new(Mutex::new(Media::default()))).clone())
}

impl Media {
    /// Ask for the library when it is older than `every` (or when nothing arrived yet).
    fn poll(&mut self, app: &mut App, every: Duration) {
        if !app.m.connected {
            return;
        }
        if self.asked.is_none_or(|a| a.elapsed() >= every) {
            self.asked = Some(Instant::now());
            app.m.query("project.assets", Value::Null);
        }
    }

    /// Rebuild the list from a new reply, turn engine events into import/rename results, pick
    /// up the file chooser's answer and finished thumbnails.
    fn pump(&mut self, app: &mut App, ctx: &egui::Context) {
        let seq = app.m.q_seq("project.assets");
        if seq != self.seq {
            self.seq = seq;
            self.assets = app.m.q_list("project.assets").iter().filter_map(Asset::parse).collect();
            self.assets.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.path.cmp(&b.path)));
        }
        self.thumbs.drain(ctx);
        self.events(app);
        if let Some((rx, target, kind)) = &self.chooser {
            match rx.try_recv() {
                Ok(Ok(files)) => {
                    let (target, kind) = (*target, *kind);
                    self.chooser = None;
                    for f in files {
                        self.import(app, &f, target, kind);
                    }
                }
                Ok(Err(e)) => {
                    self.chooser = None;
                    self.notice = Some((Tone::Warn, "The file chooser didn't open".into(), format!("You can drag files onto this window instead. ({e})")));
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => self.chooser = None,
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }
        }
        self.imports.retain(|i| !matches!(i.state, ImportState::Done) || i.at.elapsed() < Duration::from_secs(4));
        match (&self.notice, self.notice_at) {
            (Some(_), None) => self.notice_at = Some(Instant::now()),
            (Some((Tone::Ok, _, _)), Some(at)) if at.elapsed() > Duration::from_secs(6) => (self.notice, self.notice_at) = (None, None),
            (Some((Tone::Ok, _, _)), Some(_)) => ctx.request_repaint_after(Duration::from_secs(1)),
            (None, Some(_)) => self.notice_at = None,
            _ => {}
        }
        if self.imports.iter().any(|i| matches!(i.state, ImportState::Copying)) {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if let Some((_, started, secs)) = &self.playing {
            if started.elapsed().as_secs_f32() > *secs {
                self.playing = None;
            } else {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }
    }

    fn events(&mut self, app: &mut App) {
        let new = app.m.events_seen.saturating_sub(self.events) as usize;
        self.events = app.m.events_seen;
        if new == 0 {
            return;
        }
        let n = app.m.events.len();
        let mut refresh = false;
        for e in app.m.events.iter().skip(n.saturating_sub(new)) {
            let s = |k: &str| e.payload.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
            match e.ty.as_str() {
                "project.imported" => {
                    refresh = true;
                    let (src, path) = (s("src"), s("path"));
                    // a single file added on Files opens it; a batch leaves the selection
                    let alone = self.imports.iter().filter(|i| i.target == Target::Library && !matches!(i.state, ImportState::Failed(_))).count() == 1;
                    if let Some(i) = self.imports.iter_mut().find(|i| i.src == src && matches!(i.state, ImportState::Copying)) {
                        i.state = ImportState::Done;
                        i.at = Instant::now();
                        match i.target {
                            Target::Picker(id) => {
                                self.picked.insert(id, path);
                            }
                            Target::Library if alone => self.selected = Some(path),
                            Target::Library => {}
                        }
                    }
                }
                "project.import.failed" => {
                    let src = s("src");
                    if let Some(i) = self.imports.iter_mut().find(|i| i.src == src && matches!(i.state, ImportState::Copying)) {
                        i.state = ImportState::Failed(s("error"));
                    }
                }
                "project.asset.renamed" => {
                    refresh = true;
                    if self.selected.as_deref() == Some(s("path").as_str()) {
                        self.selected = Some(s("to"));
                    }
                    self.rename_for.clear();
                    let updated: Vec<String> =
                        e.payload.get_path("updated").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|u| u.as_str().map(String::from)).collect();
                    let body = if updated.is_empty() {
                        "Nothing else needed to change.".to_string()
                    } else {
                        format!("{} now uses the new name.", upper_first(&used_words(&updated, 3)))
                    };
                    self.notice = Some((Tone::Ok, format!("Renamed to \u{201c}{}\u{201d}", nice(&file_stem(&s("to")))), body));
                }
                "project.asset.deleted" => {
                    refresh = true;
                    if self.selected.as_deref() == Some(s("path").as_str()) {
                        self.selected = None;
                    }
                    self.notice = Some((Tone::Ok, format!("Deleted \u{201c}{}\u{201d}", nice(&file_stem(&s("path")))), "It's gone from your files.".into()));
                }
                "project.asset.failed" => {
                    refresh = true;
                    let used: Vec<String> =
                        e.payload.get_path("used_by").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|u| u.as_str().map(String::from)).collect();
                    let body = if used.is_empty() { s("error") } else { format!("It's still used by {}. Take it out there first.", used_words(&used, 4)) };
                    self.notice = Some((Tone::Warn, format!("Couldn't change {}", nice(&file_stem(&s("path")))), body));
                }
                _ => {}
            }
        }
        if refresh {
            self.asked = None;
        }
    }

    /// Hand a local file to `project.import`.
    fn import(&mut self, app: &mut App, src: &Path, target: Target, kind: MediaKind) {
        let file = src.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
        let src_s = src.to_string_lossy().to_string();
        let mut imp = Import { src: src_s.clone(), file, target, state: ImportState::Copying, at: Instant::now() };
        if src.is_dir() {
            imp.state = ImportState::Failed("That's a folder. Open it and add the files inside instead.".into());
        } else if !app.m.connected {
            imp.state = ImportState::Failed("Stream Engine isn't running. Start it, then add the file again.".into());
        } else {
            let mut args = Value::map().with("src", src_s);
            if kind != MediaKind::Any {
                args = args.with("kind", kind.key());
            }
            app.m.action("project.import", args);
        }
        self.imports.push(imp);
    }

    /// Open the desktop's file chooser (answer arrives in [`Media::pump`]).
    fn choose(&mut self, target: Target, kind: MediaKind) {
        if self.chooser.is_some() {
            return;
        }
        self.notice = None;
        self.chooser = Some((open_chooser(kind), target, kind));
    }

    fn preview(&mut self, app: &mut App, a: &Asset, confirmed: bool) {
        let Some(name) = &a.sound else { return };
        if crate::views::status::on_air(app) && !confirmed {
            self.confirm_play = Some(a.clone());
            return;
        }
        app.m.action("audio.play", Value::map().with("sound", name.clone()));
        let secs = match self.thumbs.map.get(&Thumbs::key(a)) {
            Some(Thumb::Ready(p)) => p.seconds.unwrap_or(5.0),
            _ => 5.0,
        };
        self.playing = Some((a.path.clone(), Instant::now(), secs.min(120.0)));
    }

    fn stop(&mut self, app: &mut App) {
        app.m.action("audio.stop", Value::Null);
        self.playing = None;
    }

    fn is_playing(&self, path: &str) -> bool {
        self.playing.as_ref().is_some_and(|(p, _, _)| p == path)
    }

    /// How far the preview of `path` has played (0..1), while it plays.
    fn progress(&self, path: &str) -> Option<f32> {
        self.playing.as_ref().filter(|(p, _, _)| p == path).map(|(_, at, secs)| (at.elapsed().as_secs_f32() / secs.max(0.1)).min(1.0))
    }
}

/// The file chooser, through xdg-desktop-portal on its own thread (no GTK in this process).
fn open_chooser(kind: MediaKind) -> Receiver<Result<Vec<PathBuf>, String>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("se-ui-chooser".into()).spawn({
        let tx = tx.clone();
        move || {
            let res = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt.block_on(choose_files(kind)),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(res);
        }
    });
    if let Err(e) = spawned {
        let _ = tx.send(Err(e.to_string()));
    }
    rx
}

async fn choose_files(kind: MediaKind) -> Result<Vec<PathBuf>, String> {
    use ashpd::desktop::ResponseError;
    use ashpd::desktop::file_chooser::{FileFilter, SelectedFiles};
    let kinds: Vec<MediaKind> = if kind == MediaKind::Any { MediaKind::ALL.to_vec() } else { vec![kind] };
    let filter = |label: &str, kinds: &[MediaKind]| {
        kinds.iter().flat_map(|k| k.exts()).fold(FileFilter::new(label), |f, e| f.glob(&format!("*.{e}")).glob(&format!("*.{}", e.to_uppercase())))
    };
    let all = filter(if kind == MediaKind::Any { "Everything Stream Engine can use" } else { kind.label() }, &kinds);
    let mut req = SelectedFiles::open_file()
        .title(if kind == MediaKind::Any { "Add to your files".to_string() } else { format!("Add a {}", kind.one()) }.as_str())
        .accept_label("Add")
        .modal(true)
        .multiple(kind == MediaKind::Any)
        .filter(all.clone())
        .current_filter(all);
    if kinds.len() > 1 {
        for k in &kinds {
            req = req.filter(filter(k.label(), &[*k]));
        }
    }
    let answer = req.send().await.map_err(|e| e.to_string())?;
    match answer.response() {
        Ok(sel) => Ok(sel.uris().iter().filter_map(|u| uri_path(u.as_str())).collect()),
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => Ok(Vec::new()),
        Err(e) => Err(e.to_string()),
    }
}

/// `file:///home/me/My%20Horn.wav` → `/home/me/My Horn.wav`.
fn uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

fn file_stem(path: &str) -> String {
    Path::new(path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

/// "1.2 MB", "340 KB".
fn size_words(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} KB", b / 1e3)
    } else {
        format!("{bytes} bytes")
    }
}

/// "0:12", "3:05".
fn length_words(secs: f32) -> String {
    let s = secs.round().max(1.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// A project file that uses a media file, in words: `scenes/duo.toml` → scene "Duo".
fn user_words(file: &str) -> String {
    let mut parts = file.split('/');
    let dir = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    let id = rest.first().map(|f| file_stem(f)).unwrap_or_default();
    let q = |what: &str| format!("{what} \u{201c}{}\u{201d}", nice(&id));
    match dir {
        "scenes" => q("scene"),
        "presets" => q("saved action"),
        // a web page, generative visual or custom effect: its own name says which
        "patches" => format!("\u{201c}{}\u{201d}", nice(&id)),
        "transitions" => q("transition"),
        "alerts" => q("alert"),
        "timelines" => q("timeline"),
        "sources" => q("source"),
        "mixes" => q("mix"),
        "rewards" => q("channel reward"),
        "rules" => "your automation".into(),
        "lights" => "your lights".into(),
        "controllers" => "your buttons & pedals".into(),
        "commands" => "your chat commands".into(),
        "audio" => "your sound settings".into(),
        "layouts" => "a window layout".into(),
        _ => "your project settings".into(),
    }
}

/// "scene “Duo”, saved action “Hype” and 2 more".
fn used_words(files: &[String], max: usize) -> String {
    let mut words: Vec<String> = Vec::new();
    for f in files {
        let w = user_words(f);
        if !words.contains(&w) {
            words.push(w);
        }
    }
    match words.len() {
        0 => String::new(),
        1 => words.remove(0),
        n if n <= max => {
            let last = words.pop().unwrap_or_default();
            format!("{} and {last}", words.join(", "))
        }
        n => {
            // always name at least one place
            let shown = max.saturating_sub(1).max(1);
            format!("{} and {} more", words[..shown].join(", "), n - shown)
        }
    }
}

/// One line of text, cut with "…" to `max_w`.
fn one_line(ui: &egui::Ui, text: &str, fid: egui::FontId, color: Color32, max_w: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(text.to_string(), fid, color, max_w);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    ui.painter().layout_job(job)
}

/// Texture UVs that fill `target` (cover: crop the long side, centered).
fn cover_uv(tex: [usize; 2], target: Vec2) -> Rect {
    let ta = tex[0] as f32 / tex[1].max(1) as f32;
    let ra = target.x / target.y.max(1.0);
    if ta > ra {
        let w = ra / ta;
        Rect::from_min_max(Pos2::new((1.0 - w) / 2.0, 0.0), Pos2::new((1.0 + w) / 2.0, 1.0))
    } else {
        let h = ta / ra;
        Rect::from_min_max(Pos2::new(0.0, (1.0 - h) / 2.0), Pos2::new(1.0, (1.0 + h) / 2.0))
    }
}

/// The largest rect of the texture's shape inside `r` (contain), centered.
fn contain(tex: [usize; 2], r: Rect) -> Rect {
    let ta = tex[0] as f32 / tex[1].max(1) as f32;
    let size = if r.width() / r.height().max(1.0) > ta { Vec2::new(r.height() * ta, r.height()) } else { Vec2::new(r.width(), r.width() / ta) };
    Rect::from_center_size(r.center(), size)
}

/// Picture area of a tile or the details card: thumbnail, a sound's waveform (the played part
/// in the accent color), or the kind's icon ("Aa" for fonts). `pv` None = still being made.
#[allow(clippy::too_many_arguments)]
fn paint_preview(ui: &egui::Ui, t: &Theme, rect: Rect, a: &Asset, pv: Option<&Preview>, fill: bool, rounding: CornerRadius, progress: Option<f32>) {
    let p = ui.painter_at(rect);
    p.rect_filled(rect, rounding, mix(t.surface_hi, t.bg, 0.55));
    if let Some(tex) = pv.and_then(|pv| pv.tex.as_ref()) {
        let size = tex.size();
        let (r, uv) = if fill {
            (rect, cover_uv(size, rect.size()))
        } else {
            (contain(size, rect.shrink(spacing::S)), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)))
        };
        egui::Image::new(egui::load::SizedTexture::new(tex.id(), r.size()))
            .uv(uv)
            .corner_radius(if fill { rounding } else { CornerRadius::ZERO })
            .paint_at(ui, r);
        return;
    }
    let c = rect.center();
    match a.kind {
        MediaKind::Font => {
            p.text(c, Align2::CENTER_CENTER, "Aa", font_semibold((rect.height() * 0.32).clamp(22.0, 64.0)), t.text_dim);
        }
        MediaKind::Sound => {
            let Some(peaks) = pv.map(|pv| &pv.peaks).filter(|p| !p.is_empty()) else { return };
            let area = rect.shrink2(Vec2::new(rect.width() * 0.06, rect.height() * 0.18));
            let step = area.width() / peaks.len() as f32;
            let bar = (step * 0.6).max(1.0);
            let played = progress.map_or(0, |f| (f * peaks.len() as f32) as usize);
            for (i, v) in peaks.iter().enumerate() {
                let h = (v * area.height()).max(2.0);
                let x = area.left() + step * (i as f32 + 0.5);
                let col = if i < played { t.accent } else { mix(t.text_faint, t.text_dim, 0.4) };
                p.rect_filled(Rect::from_center_size(Pos2::new(x, area.center().y), Vec2::new(bar, h)), CornerRadius::same(1), col);
            }
        }
        k => {
            p.text(c, Align2::CENTER_CENTER, k.icon(), font((rect.height() * 0.22).clamp(18.0, 48.0)), t.text_faint);
            if pv.is_none() && rect.height() > 60.0 {
                p.text(c + Vec2::new(0.0, rect.height() * 0.22), Align2::CENTER_CENTER, "Loading…", font(type_scale::SMALL), t.text_faint);
            }
        }
    }
}

/// Round play/stop button painted over a sound's preview; returns true when clicked.
fn play_button(ui: &mut egui::Ui, t: &Theme, center: Pos2, r: f32, playing: bool, enabled: bool, id: egui::Id) -> egui::Response {
    let rect = Rect::from_center_size(center, Vec2::splat(r * 2.0));
    let resp = ui.interact(rect, id, if enabled { Sense::click() } else { Sense::hover() });
    let fill = if !enabled {
        t.surface_hi
    } else if playing {
        mix(t.surface, t.accent, if resp.hovered() { 0.4 } else { 0.28 })
    } else if resp.hovered() {
        mix(t.accent, Color32::WHITE, 0.08)
    } else {
        t.accent
    };
    let fg = if !enabled {
        t.text_faint
    } else if playing {
        t.fg
    } else {
        t.on_accent
    };
    ui.painter().circle(center, r, fill, Stroke::new(1.0, if playing { t.accent } else { Color32::TRANSPARENT }));
    let (glyph, nudge) = if playing { (icon::STOP, 0.0) } else { (icon::PLAY, r * 0.08) };
    ui.painter().text(center + Vec2::new(nudge, 0.0), Align2::CENTER_CENTER, glyph, font(r * 0.8), fg);
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// Why a sound can't be previewed right now (None = it can).
fn preview_blocker(app: &App, a: &Asset) -> Option<&'static str> {
    if a.sound.is_none() {
        Some("This kind of sound can't be previewed here. WAV, MP3, OGG and FLAC files can.")
    } else if !app.m.has("health.audio.pipewire") {
        Some("Sound isn't running in Stream Engine right now, so there's nothing to hear.")
    } else {
        None
    }
}

/// On air, "Play it on air?" before a sound preview (viewers would hear it).
fn confirm_play(app: &mut App, ctx: &egui::Context, m: &mut Media) {
    let Some(a) = m.confirm_play.clone() else { return };
    let t = app.t.clone();
    let mut answer = None;
    let resp = egui::Modal::new(egui::Id::new("media-confirm-play")).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.label(RichText::new(format!("Play \u{201c}{}\u{201d} on air?", a.title())).font(font_semibold(type_scale::LARGE)).color(t.fg));
        ui.add_space(spacing::S);
        ui.label(RichText::new("You're on air: your viewers will hear this sound too.").color(t.text_dim));
        ui.add_space(spacing::L);
        ui.horizontal(|ui| {
            if widgets::button_ex(ui, &t, Some(icon::PLAY), "Play it on air", Kind::Live, Size::Medium, 0.0, true).clicked() {
                answer = Some(true);
            }
            if widgets::button_ex(ui, &t, None, "Cancel", Kind::Secondary, Size::Medium, 0.0, true).clicked() {
                answer = Some(false);
            }
        });
    });
    if resp.should_close() && answer.is_none() {
        answer = Some(false);
    }
    if let Some(yes) = answer {
        m.confirm_play = None;
        if yes {
            m.preview(app, &a, true);
        }
    }
}

// ---- the shared picker ----------------------------------------------------------------------

/// A compact picker: current file name (+ thumbnail for images, ▶ for sounds), "Choose…" opens
/// a popup listing the library filtered by kind with search and "Import a file…".
/// Returns true when `current` changed (project-relative path, e.g. "assets/sounds/horn.wav";
/// empty = no file).
pub fn picker(app: &mut App, ui: &mut egui::Ui, id: impl std::hash::Hash, kind: MediaKind, current: &mut String) -> bool {
    let t = app.t.clone();
    let id = {
        use std::hash::Hasher;
        let mut h = std::hash::DefaultHasher::new();
        id.hash(&mut h);
        egui::Id::new("media-picker").with(h.finish())
    };
    let st = shared(ui.ctx());
    let Ok(mut m) = st.lock() else { return false };
    let pop_id = id.with("popup");
    let open = egui::Popup::is_id_open(ui.ctx(), pop_id);
    m.poll(app, if open { Duration::from_secs(2) } else { Duration::from_secs(10) });
    m.pump(app, ui.ctx());
    let mut changed = false;
    if let Some(p) = m.picked.remove(&id)
        && *current != p
    {
        *current = p;
        changed = true;
    }
    let cur = m.assets.iter().find(|a| a.path == *current).cloned();
    let h = 40.0;
    let choose = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = spacing::S;
            // small preview square
            let (sq, _) = ui.allocate_exact_size(Vec2::splat(h), Sense::hover());
            match &cur {
                Some(a) => {
                    let pv = m.thumbs.get(ui.ctx(), a);
                    paint_preview(ui, &t, sq, a, pv.as_ref(), true, CornerRadius::same(radius::CONTROL), None);
                    if a.kind == MediaKind::Sound {
                        let blocked = preview_blocker(app, a);
                        let playing = m.is_playing(&a.path);
                        let r = play_button(ui, &t, sq.center(), h * 0.36, playing, blocked.is_none(), id.with("play"));
                        let r = r.on_hover_text(blocked.unwrap_or(if playing { "Stop" } else { "Listen" }));
                        if r.clicked() {
                            if playing {
                                m.stop(app);
                            } else {
                                m.preview(app, a, false);
                            }
                        }
                    }
                }
                None => {
                    ui.painter().rect_filled(sq, CornerRadius::same(radius::CONTROL), mix(t.surface_hi, t.bg, 0.55));
                    ui.painter().text(sq.center(), Align2::CENTER_CENTER, kind.icon(), font(type_scale::LARGE), t.text_faint);
                }
            }
            let copying = m.imports.iter().any(|i| i.target == Target::Picker(id) && matches!(i.state, ImportState::Copying));
            let (title, sub, col) = match (&cur, current.is_empty()) {
                _ if copying => ("Adding your file…".to_string(), String::new(), t.text_dim),
                (Some(a), _) => (a.title(), size_words(a.bytes), t.fg),
                (None, true) => (format!("No {} yet", kind.one()), String::new(), t.text_dim),
                // not in the library (yet): the list may still be loading, or the file is gone
                (None, false) if m.seq == 0 => (nice(&file_stem(current)), String::new(), t.fg),
                (None, false) => (nice(&file_stem(current)), "This file is missing".to_string(), t.yellow),
            };
            let btn_w = 104.0;
            let text_w = (ui.available_width() - btn_w - spacing::S).max(60.0);
            ui.allocate_ui_with_layout(Vec2::new(text_w, h), Layout::top_down(Align::Min), |ui| {
                ui.set_min_size(Vec2::new(text_w, h));
                let g = one_line(ui, &title, font_medium(type_scale::BODY), col, text_w);
                let s = (!sub.is_empty()).then(|| one_line(ui, &sub, font(type_scale::SMALL), if col == t.yellow { t.yellow } else { t.text_dim }, text_w));
                let top = ui.max_rect().top();
                let total = g.size().y + s.as_ref().map_or(0.0, |s| s.size().y + 2.0);
                let y = top + (h - total) / 2.0;
                let x = ui.max_rect().left();
                let gy = g.size().y;
                ui.painter().galley(Pos2::new(x, y), g, col);
                if let Some(s) = s {
                    ui.painter().galley(Pos2::new(x, y + gy + 2.0), s, t.text_dim);
                }
            })
            .response
            .on_hover_text(&title);
            ui.allocate_ui_with_layout(Vec2::new(btn_w, h), Layout::right_to_left(Align::Center), |ui| {
                named(widgets::button_ex(ui, &t, None, "Choose…", Kind::Secondary, Size::Small, btn_w, true), "Choose…")
            })
            .inner
        })
        .inner;
    let mut pick: Option<String> = None;
    let mut import = false;
    egui::Popup::from_toggle_button_response(&choose).id(pop_id).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).width(380.0).show(|ui| {
        ui.set_width(380.0);
        let items: Vec<Asset> = m.assets.iter().filter(|a| kind.accepts(a.kind)).cloned().collect();
        if items.len() > 8 {
            ui.add(widgets::field(&mut m.picker_search).hint_text(format!("Search {}", kind.label().to_lowercase())).desired_width(f32::INFINITY));
            ui.add_space(spacing::S);
        }
        let q = m.picker_search.trim().to_lowercase();
        let shown: Vec<&Asset> = items.iter().filter(|a| q.is_empty() || a.title().to_lowercase().contains(&q) || a.name.contains(&q)).collect();
        if items.is_empty() {
            ui.add_space(spacing::S);
            ui.label(RichText::new(format!("You don't have any {} yet.", kind.label().to_lowercase())).color(t.text_dim));
            ui.add_space(spacing::S);
        } else if shown.is_empty() {
            ui.label(RichText::new("Nothing matches.").color(t.text_dim));
        }
        egui::ScrollArea::vertical().max_height(320.0).auto_shrink([false, true]).show(ui, |ui| {
            if !current.is_empty() && widgets::list_row(ui, &t, icon::CROSS, &format!("No {}", kind.one()), "", "", false).clicked() {
                pick = Some(String::new());
            }
            for a in shown {
                let sub = size_words(a.bytes);
                let ic = if kind == MediaKind::Any { a.kind.icon() } else { "" };
                let row = named(widgets::list_row(ui, &t, ic, &a.title(), &sub, "", a.path == *current), &a.title());
                if a.kind == MediaKind::Image
                    && let Some(tex) = m.thumbs.get(ui.ctx(), a).and_then(|pv| pv.tex)
                {
                    let sq = Rect::from_min_size(Pos2::new(row.rect.right() - 12.0 - 40.0, row.rect.center().y - 20.0), Vec2::splat(40.0));
                    let uv = cover_uv(tex.size(), sq.size());
                    egui::Image::new(egui::load::SizedTexture::new(tex.id(), sq.size())).uv(uv).corner_radius(CornerRadius::same(6)).paint_at(ui, sq);
                }
                if row.on_hover_text(a.title()).clicked() {
                    pick = Some(a.path.clone());
                }
            }
        });
        ui.add_space(spacing::S);
        ui.separator();
        ui.add_space(spacing::XS);
        let busy = m.chooser.is_some();
        let label = if busy { "Waiting for the file chooser…".to_string() } else { format!("Import a {}…", kind.one()) };
        if named(widgets::button_ex(ui, &t, Some(UPLOAD), &label, Kind::Ghost, Size::Small, 0.0, !busy), &label).clicked() {
            import = true;
        }
    });
    if import {
        m.choose(Target::Picker(id), kind);
    }
    // a failed import from this picker: say why, under the control
    if let Some(i) = m.imports.iter().position(|i| i.target == Target::Picker(id) && matches!(i.state, ImportState::Failed(_))) {
        let (file, why) = match &m.imports[i].state {
            ImportState::Failed(why) => (m.imports[i].file.clone(), why.clone()),
            _ => unreachable!(),
        };
        if widgets::callout(ui, &t, Tone::Warn, icon::WARN, &format!("Couldn't add {file}"), &why, Some("OK")) {
            m.imports.remove(i);
        }
    }
    if let Some(p) = pick {
        egui::Popup::close_id(ui.ctx(), pop_id);
        if *current != p {
            *current = p;
            changed = true;
        }
    }
    confirm_play(app, ui.ctx(), &mut m);
    changed
}

/// Paint the picture of the project file `path` (a picture's or video's thumbnail) into
/// `rect`, letterboxed. False when there's none (other kinds, missing file, still loading).
pub fn file_preview(app: &mut App, ui: &mut egui::Ui, rect: Rect, path: &str) -> bool {
    let t = app.t.clone();
    let st = shared(ui.ctx());
    let Ok(mut m) = st.lock() else { return false };
    m.poll(app, Duration::from_secs(10));
    m.pump(app, ui.ctx());
    let Some(a) = m.assets.iter().find(|a| a.path == path).cloned() else { return false };
    let pv = m.thumbs.get(ui.ctx(), &a);
    if pv.as_ref().is_none_or(|p| p.tex.is_none()) {
        return false;
    }
    paint_preview(ui, &t, rect, &a, pv.as_ref(), false, CornerRadius::same(radius::TILE), None);
    true
}

// ---- Sources → Files ------------------------------------------------------------------------

/// Width of the list pane.
const LIST_W: f32 = 296.0;

/// "Show" choices of the list: all files, then each kind.
fn filter_kind(filter: usize) -> Option<MediaKind> {
    filter.checked_sub(1).and_then(|i| MediaKind::ALL.get(i).copied())
}

/// Sources → Files: the files as a list grouped by kind (search, "Show" filter, one "+" to
/// import), the selected file on the right (preview, about, used by, name, delete, "Make a
/// source"). Files dropped anywhere on the page are imported.
pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let st = shared(ui.ctx());
    let area = ui.max_rect();
    let hovering = {
        let Ok(mut m) = st.lock() else { return };
        m.poll(app, Duration::from_secs(3));
        m.pump(app, ui.ctx());
        // drag & drop anywhere on this page
        let (dropped, hovering) = ui.ctx().input(|i| {
            let files: Vec<PathBuf> = i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect();
            (files, !i.raw.hovered_files.is_empty())
        });
        for p in dropped {
            m.import(app, &p, Target::Library, MediaKind::Any);
        }
        if m.selected.as_ref().is_some_and(|p| m.seq > 0 && !m.assets.iter().any(|a| a.path == *p)) {
            m.selected = None;
        }
        if m.selected.is_none() {
            let first = m.assets.iter().find(|a| filter_kind(m.filter).is_none_or(|k| a.kind == k)).map(|a| a.path.clone());
            m.selected = first;
        }
        hovering
    };
    widgets::split(
        ui,
        LIST_W,
        |ui| {
            if let Ok(mut m) = st.lock() {
                list_pane(ui, &t, &mut m);
            }
        },
        |ui| {
            if let Ok(mut m) = st.lock() {
                egui::ScrollArea::vertical().id_salt("files-detail").auto_shrink([false, false]).show(ui, |ui| {
                    status(ui, &t, &mut m);
                    match m.selected.clone().and_then(|p| m.assets.iter().find(|a| a.path == p).cloned()) {
                        Some(a) => details(app, ui, &t, &mut m, &a),
                        None => {
                            let body = if app.m.connected {
                                "Pictures, videos, sounds, color looks and fonts your sources, effects and alerts use. Drop files anywhere on this page to add them."
                            } else {
                                "Stream Engine isn't running. Start it to see and add your files."
                            };
                            if widgets::empty_state(ui, &t, UPLOAD, "Files", body, app.m.connected.then_some("Import files…")) {
                                m.choose(Target::Library, MediaKind::Any);
                            }
                        }
                    }
                });
                confirm_play(app, ui.ctx(), &mut m);
            }
        },
    );
    if hovering {
        let p = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("media-drop")));
        let r = area.shrink(spacing::S);
        p.rect(r, CornerRadius::same(radius::CARD), mix(t.bg, t.accent, 0.18).gamma_multiply(0.92), Stroke::new(2.0, t.accent), StrokeKind::Inside);
        p.text(r.center() - Vec2::new(0.0, 18.0), Align2::CENTER_CENTER, UPLOAD, font(36.0), t.accent);
        p.text(r.center() + Vec2::new(0.0, 22.0), Align2::CENTER_CENTER, "Drop to add to your files", font_semibold(type_scale::HEADING), t.fg);
    }
}

/// The list pane: heading with "+" (import), "Show" filter, search, files grouped by kind.
fn list_pane(ui: &mut egui::Ui, t: &Theme, m: &mut Media) {
    let busy = m.chooser.is_some();
    ui.add_enabled_ui(!busy, |ui| {
        if widgets::pane_header(ui, t, "Files", Some(m.assets.len()), Some(if busy { "Waiting for the file chooser…" } else { "Import files…" })) {
            m.choose(Target::Library, MediaKind::Any);
        }
    });
    if m.assets.is_empty() {
        return;
    }
    let count = |k: Option<MediaKind>| m.assets.iter().filter(|a| k.is_none_or(|k| a.kind == k)).count();
    let shown = |f: usize| match filter_kind(f) {
        None => format!("All files  {}", count(None)),
        Some(k) => format!("{}  {}", k.label(), count(Some(k))),
    };
    let mut filter = m.filter;
    egui::ComboBox::from_id_salt("files-filter").selected_text(shown(filter)).width(ui.available_width()).show_ui(ui, |ui| {
        for f in 0..=MediaKind::ALL.len() {
            ui.selectable_value(&mut filter, f, shown(f));
        }
    });
    m.filter = filter;
    if m.assets.len() > 8 {
        ui.add_space(spacing::S);
        ui.add(widgets::field(&mut m.search).hint_text("Search your files").desired_width(f32::INFINITY));
    }
    ui.add_space(spacing::XS);
    let q = m.search.trim().to_lowercase();
    let kinds: Vec<MediaKind> = filter_kind(m.filter).map_or_else(|| MediaKind::ALL.to_vec(), |k| vec![k]);
    let mut picked = None;
    egui::ScrollArea::vertical().id_salt("files-list").auto_shrink([false, false]).show(ui, |ui| {
        let mut any = false;
        for k in kinds {
            let items: Vec<Asset> =
                m.assets.iter().filter(|a| a.kind == k && (q.is_empty() || a.title().to_lowercase().contains(&q) || a.name.contains(&q))).cloned().collect();
            if items.is_empty() {
                continue;
            }
            any = true;
            widgets::group_label(ui, t, k.label());
            for a in &items {
                let used = if a.used_by.is_empty() { "not used yet".to_string() } else { format!("used by {}", used_words(&a.used_by, 1)) };
                let sub = format!("{} · {used}", size_words(a.bytes));
                let thumb = matches!(a.kind, MediaKind::Image | MediaKind::Video);
                let w = ui.available_width() - if thumb { 64.0 } else { 24.0 } - 28.0;
                let (title, sub) = (clip_to(ui, &a.title(), font_medium(type_scale::BODY), w), clip_to(ui, &sub, font(type_scale::SMALL), w));
                let selected = m.selected.as_deref() == Some(a.path.as_str());
                let row = named(widgets::list_row(ui, t, a.kind.icon(), &title, &sub, "", selected), &a.title());
                if thumb && let Some(tex) = m.thumbs.get(ui.ctx(), a).and_then(|pv| pv.tex) {
                    let sq = Rect::from_min_size(Pos2::new(row.rect.right() - 10.0 - 40.0, row.rect.center().y - 20.0), Vec2::splat(40.0));
                    let uv = cover_uv(tex.size(), sq.size());
                    egui::Image::new(egui::load::SizedTexture::new(tex.id(), sq.size()))
                        .uv(uv)
                        .corner_radius(CornerRadius::same(radius::CONTROL))
                        .paint_at(ui, sq);
                }
                if row.on_hover_text(a.title()).clicked() {
                    picked = Some(a.path.clone());
                }
            }
        }
        if !any {
            ui.add_space(spacing::S);
            widgets::hint(ui, t, if q.is_empty() { "None of these yet." } else { "Nothing matches." });
        }
    });
    if let Some(p) = picked {
        if m.selected.as_ref() != Some(&p) {
            m.confirm_delete = false;
        }
        m.selected = Some(p);
    }
}

/// `text` cut with "…" to fit `max_w` in `fid`.
fn clip_to(ui: &egui::Ui, text: &str, fid: egui::FontId, max_w: f32) -> String {
    let fits = |s: &str| ui.painter().layout_no_wrap(s.to_string(), fid.clone(), Color32::WHITE).size().x <= max_w;
    if fits(text) {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if fits(&format!("{}…", chars[..mid].iter().collect::<String>().trim_end())) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    format!("{}…", chars[..lo].iter().collect::<String>().trim_end())
}

/// Import progress, import problems and rename/delete results.
fn status(ui: &mut egui::Ui, t: &Theme, m: &mut Media) {
    let copying: Vec<&str> =
        m.imports.iter().filter(|i| i.target == Target::Library && matches!(i.state, ImportState::Copying)).map(|i| i.file.as_str()).collect();
    if !copying.is_empty() {
        let title = if copying.len() == 1 { "Adding 1 file…".to_string() } else { format!("Adding {} files…", copying.len()) };
        widgets::callout(ui, t, Tone::Info, UPLOAD, &title, &copying.join(", "), None);
        ui.add_space(spacing::M);
    }
    let done = m.imports.iter().filter(|i| i.target == Target::Library && matches!(i.state, ImportState::Done)).count();
    if done > 0 && copying.is_empty() {
        let title = if done == 1 { "Added 1 file".to_string() } else { format!("Added {done} files") };
        let body = if done == 1 { "It's in your files now and ready to use." } else { "They're in your files now and ready to use." };
        widgets::callout(ui, t, Tone::Ok, icon::CHECK, &title, body, None);
        ui.add_space(spacing::M);
    }
    let mut dismiss = None;
    for (n, i) in m.imports.iter().enumerate() {
        if let (Target::Library, ImportState::Failed(why)) = (&i.target, &i.state) {
            if widgets::callout(ui, t, Tone::Danger, icon::WARN, &format!("Couldn't add {}", i.file), why, Some("OK")) {
                dismiss = Some(n);
            }
            ui.add_space(spacing::M);
        }
    }
    if let Some(n) = dismiss {
        m.imports.remove(n);
    }
    if let Some((tone, title, body)) = m.notice.clone() {
        let ic = if tone == Tone::Ok { icon::CHECK } else { icon::WARN };
        if widgets::callout(ui, t, tone, ic, &title, &body, Some("OK")) {
            m.notice = None;
        }
        ui.add_space(spacing::M);
    }
}

/// The selected file: header (with "Make a source" for pictures and videos), its picture or
/// waveform, then About, Used by, Name and Delete.
fn details(app: &mut App, ui: &mut egui::Ui, t: &Theme, m: &mut Media, a: &Asset) {
    if m.rename_for != a.path {
        m.rename_for = a.path.clone();
        m.rename = a.title();
        m.confirm_delete = false;
    }
    let kind_word = match a.kind {
        MediaKind::Image => "Picture",
        MediaKind::Video => "Video",
        MediaKind::Sound => "Sound",
        MediaKind::Lut => "Color look",
        MediaKind::Font => "Font",
        MediaKind::Any => "File",
    };
    let sourceable = a.kind == MediaKind::Video || (a.kind == MediaKind::Image && crate::views::sources::is_picture_file(&a.path));
    let mut make_source = false;
    widgets::detail_header(ui, t, a.kind.icon(), &a.title(), &format!("{kind_word} · {} · {}", a.ext(), size_words(a.bytes)), |ui| {
        if sourceable {
            make_source =
                named(widgets::button_ex(ui, t, Some(icon::CAMERA), "Make a source", Kind::Secondary, Size::Small, 0.0, app.m.connected), "Make a source")
                    .on_hover_text("Show it in scenes: makes a source from this file")
                    .clicked();
        }
    });
    if make_source {
        crate::views::sources::new_from_file(app, ui.ctx(), &a.path);
    }

    let w = ui.available_width().min(560.0);
    let h = if a.kind == MediaKind::Sound { 120.0 } else { (w * 9.0 / 16.0).round() };
    let (pic, _) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
    let pv = m.thumbs.get(ui.ctx(), a);
    let seconds = pv.as_ref().and_then(|pv| pv.seconds);
    paint_preview(ui, t, pic, a, pv.as_ref(), false, CornerRadius::same(radius::TILE), m.progress(&a.path));
    if a.kind == MediaKind::Sound {
        let blocked = preview_blocker(app, a);
        let playing = m.is_playing(&a.path);
        let r = play_button(ui, t, pic.center(), 28.0, playing, blocked.is_none(), egui::Id::new(("media-play-big", &a.path)));
        let r = r.on_hover_text(blocked.unwrap_or(if playing { "Stop" } else { "Listen" }));
        if r.clicked() {
            if playing {
                m.stop(app);
            } else {
                m.preview(app, a, false);
            }
        }
        if let Some(why) = blocked {
            ui.add_space(spacing::S);
            widgets::hint(ui, t, why);
        }
    }
    ui.add_space(spacing::L);

    widgets::inspector_section(
        ui,
        t,
        ("files-about", &a.path),
        "About",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Type", |ui| widgets::hint(ui, t, &format!("{kind_word} ({})", a.ext())));
            widgets::prop_row(ui, t, "Size", |ui| widgets::hint(ui, t, &size_words(a.bytes)));
            if let Some(s) = seconds.filter(|_| matches!(a.kind, MediaKind::Video | MediaKind::Sound)) {
                widgets::prop_row(ui, t, "Length", |ui| widgets::hint(ui, t, &length_words(s)));
            }
            widgets::prop_row(ui, t, "In your project", |ui| {
                ui.add(egui::Label::new(RichText::new(&a.path).font(font_mono(type_scale::SMALL)).color(t.text_dim)).truncate()).on_hover_text(&a.path);
            });
        },
    );
    widgets::inspector_section(
        ui,
        t,
        ("files-used", &a.path),
        "Used by",
        true,
        |_| {},
        |ui| {
            if a.used_by.is_empty() {
                widgets::hint(ui, t, "Nothing uses it yet.");
            }
            let mut seen: Vec<String> = Vec::new();
            for f in &a.used_by {
                let w = user_words(f);
                if !seen.contains(&w) {
                    widgets::list_row(ui, t, icon::LINK, &upper_first(&w), "", "", false);
                    seen.push(w);
                }
            }
        },
    );
    let mut rename = false;
    widgets::inspector_section(
        ui,
        t,
        ("files-name", &a.path),
        "Name",
        true,
        |_| {},
        |ui| {
            let changed = m.rename.trim() != a.title() && !m.rename.trim().is_empty();
            ui.horizontal(|ui| {
                let r = ui
                    .add(widgets::field(&mut m.rename).hint_text("A name you'll recognise").desired_width((ui.available_width() - 110.0).clamp(120.0, 320.0)));
                rename = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && changed;
                if named(widgets::button_ex(ui, t, None, "Rename", Kind::Secondary, Size::Medium, 96.0, changed), "Rename").clicked() {
                    rename = true;
                }
            });
            widgets::hint(ui, t, "Everything that uses it keeps working.");
        },
    );
    if rename {
        app.m.action("project.asset.rename", Value::map().with("path", a.path.clone()).with("name", m.rename.trim().to_string()));
        m.notice = None;
    }
    let mut delete = None;
    widgets::inspector_section(
        ui,
        t,
        ("files-delete", &a.path),
        "Delete",
        m.confirm_delete,
        |_| {},
        |ui| {
            if !m.confirm_delete {
                if named(widgets::button_ex(ui, t, Some(icon::TRASH), "Delete…", Kind::Danger, Size::Medium, 0.0, true), "Delete…").clicked() {
                    m.confirm_delete = true;
                    m.scroll_confirm = true;
                }
                return;
            }
            let used = !a.used_by.is_empty();
            let (title, body) = if used {
                (
                    format!("\u{201c}{}\u{201d} is still used", a.title()),
                    format!(
                        "{} uses it. If you delete it, that spot will be empty until you pick another {}.",
                        upper_first(&used_words(&a.used_by, 3)),
                        a.kind.one()
                    ),
                )
            } else {
                (format!("Delete \u{201c}{}\u{201d}?", a.title()), format!("The {} is removed from your project.", a.kind.one()))
            };
            widgets::callout(ui, t, if used { Tone::Warn } else { Tone::Danger }, icon::TRASH, &title, &body, None);
            ui.add_space(spacing::M);
            let row = ui.horizontal(|ui| {
                let label = if used { "Delete anyway" } else { "Delete" };
                if named(widgets::button_ex(ui, t, Some(icon::TRASH), label, Kind::Danger, Size::Medium, 0.0, true), label).clicked() {
                    delete = Some(used);
                }
                if named(widgets::button_ex(ui, t, None, "Keep it", Kind::Secondary, Size::Medium, 0.0, true), "Keep it").clicked() {
                    m.confirm_delete = false;
                }
            });
            if std::mem::take(&mut m.scroll_confirm) {
                row.response.scroll_to_me(Some(Align::BOTTOM));
            }
        },
    );
    if let Some(force) = delete {
        app.m.action("project.asset.delete", Value::map().with("path", a.path.clone()).with("force", force));
        m.confirm_delete = false;
        m.notice = None;
    }
}

/// Give a painted button its words for screen readers (and UI tests).
fn named(r: egui::Response, label: &str) -> egui::Response {
    let enabled = r.enabled();
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    r
}

fn upper_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_uris_become_local_paths() {
        assert_eq!(uri_path("file:///home/me/My%20Horn%21.wav"), Some(PathBuf::from("/home/me/My Horn!.wav")));
        assert_eq!(uri_path("file://localhost/tmp/a.png"), Some(PathBuf::from("/tmp/a.png")));
        assert_eq!(uri_path("file:///x/%C3%A9t%C3%A9.mp4"), Some(PathBuf::from("/x/été.mp4")));
        assert_eq!(uri_path("https://example.com/a.png"), None);
    }

    #[test]
    fn users_read_as_plain_words() {
        let files =
            vec!["scenes/duo.toml".to_string(), "presets/big_hype.toml".to_string(), "patches/alertbox/index.html".to_string(), "rules/main.toml".to_string()];
        assert_eq!(used_words(&files[..1], 3), "scene \u{201c}Duo\u{201d}");
        assert_eq!(used_words(&files[..2], 3), "scene \u{201c}Duo\u{201d} and saved action \u{201c}Big hype\u{201d}");
        assert_eq!(used_words(&files, 3), "scene \u{201c}Duo\u{201d}, saved action \u{201c}Big hype\u{201d} and 2 more");
        // a one-line tile still names the first place
        assert_eq!(used_words(&files, 1), "scene \u{201c}Duo\u{201d} and 3 more");
        // the same place mentioned by two files counts once
        assert_eq!(used_words(&["rules/a.toml".into(), "rules/b.toml".into()], 3), "your automation");
        // a patch folder is named by itself; a source file is a source
        assert_eq!(user_words("patches/alertbox/index.html"), "\u{201c}Alertbox\u{201d}");
        assert_eq!(user_words("sources/brb.toml"), "source \u{201c}BRB\u{201d}");
    }
}
