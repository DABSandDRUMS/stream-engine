//! Sources → Sources: everything that makes picture or sound (docs/ui-model.md). A source is
//! made once and placed in any number of scenes. The list groups them by kind (cameras &
//! capture, media, web, generative); the detail pane shows the selected one: its live picture,
//! its settings by kind, color, where it's used, "Add to scene…", rename and remove. A new
//! source starts from a kind chooser and a draft that is saved only on Create.
//!
//! Engine side: `project.sources` and `project.source.save|remove` (cameras and media files),
//! `sources` (running cameras and players: controls, playback), `video_in.preview` (camera
//! pictures), `patches` and `patch.*` (web pages and generative visuals), `scenes`,
//! `project.read|write`.

use crate::app::{App, ViewId};
use crate::frames::Canvas;
use crate::views::live::nice;
use crate::views::media::{self, MediaKind};
use crate::views::{composition, links, monitor, patches, scene_edit};
use egui::{Align2, CornerRadius, Pos2, Rect, Sense, TextureHandle, Ui, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, Tone, icon};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Width of the list pane.
const LIST_W: f32 = 296.0;
const GLOBE: &str = "\u{f0ac}";
/// A draft or change the engine hasn't confirmed after this long is reported.
const CONFIRM_WAIT: Duration = Duration::from_secs(20);
/// Camera preview leases last this long and are renewed every `LEASE_RENEW` while shown.
const LEASE_SECS: f64 = 6.0;
const LEASE_RENEW: Duration = Duration::from_secs(2);
const PREVIEW_EVERY: Duration = Duration::from_millis(250);
const PROJECT_KEY: &str = "sources.project_toml";
const PREVIEW_KEY: &str = "sources.preview";
/// ctx memory slot for "Make a source" from Files.
const FROM_FILE: &str = "sources-new-from-file";
/// One-shot source selection when another page opens this editor.
const OPEN_SOURCE: &str = "sources-open-source";

// ---- kinds ------------------------------------------------------------------------------------

/// What a source is (docs/ui-model.md). `Color` exists only as a layer (`color:#rrggbb`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SourceKind {
    Camera,
    Video,
    Image,
    Web,
    Generative,
    Color,
}

impl SourceKind {
    /// The kinds a user can make here, in chooser order.
    const NEW: [SourceKind; 5] = [SourceKind::Camera, SourceKind::Video, SourceKind::Image, SourceKind::Web, SourceKind::Generative];

    pub fn icon(self) -> &'static str {
        match self {
            SourceKind::Camera => icon::CAMERA,
            SourceKind::Video => icon::FILM,
            SourceKind::Image => icon::IMAGE,
            SourceKind::Web => GLOBE,
            SourceKind::Generative => icon::SPARKLE,
            SourceKind::Color => icon::PALETTE,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SourceKind::Camera => "Camera",
            SourceKind::Video => "Video",
            SourceKind::Image => "Image",
            SourceKind::Web => "Web page",
            SourceKind::Generative => "Generative",
            SourceKind::Color => "Color",
        }
    }

    /// Chooser tile: (title, one line).
    fn tile(self) -> (&'static str, &'static str) {
        match self {
            SourceKind::Camera => ("Camera or capture card", "A webcam, camera or capture card on this computer."),
            SourceKind::Video => ("Video", "A video from your files. Loops or plays once."),
            SourceKind::Image => ("Image or GIF", "A picture, or an animated GIF that loops."),
            SourceKind::Web => ("Web page", "Chat, alerts, goals, labels: pages made of HTML."),
            SourceKind::Generative => ("Generative", "Animated backgrounds, particles and scripted graphics."),
            SourceKind::Color => ("Color", "A solid color."),
        }
    }
}

fn ext(path: &str) -> String {
    std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// Pictures a source can show (FFmpeg decodes them; SVG it doesn't).
pub fn is_picture_file(path: &str) -> bool {
    matches!(ext(path).as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif")
}

fn is_gif(path: &str) -> bool {
    ext(path) == "gif"
}

/// A media file source is an image when its file is a picture.
fn media_kind(file: &str) -> SourceKind {
    if matches!(ext(file).as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif" | "svg") { SourceKind::Image } else { SourceKind::Video }
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn list<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get_path(k).and_then(Value::as_list).unwrap_or(&[])
}

fn num(v: &Value, k: &str) -> f64 {
    v.get_path(k).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Saved camera and media sources (`project.sources`).
fn project_sources(app: &App) -> &[Value] {
    app.m.q("project.sources").map(|v| list(v, "sources")).unwrap_or(&[])
}

fn project_source(app: &App, name: &str) -> Option<Value> {
    project_sources(app).iter().find(|s| text(s, "name") == name).cloned()
}

/// The running source (`sources`: controls, identity, signal), when video is running.
fn runtime_source(app: &App, name: &str) -> Option<Value> {
    app.m.q_list("sources").iter().find(|s| text(s, "name") == name).cloned()
}

/// What kind of source a layer's `src` is.
pub fn source_kind(app: &App, src: &str) -> SourceKind {
    if src.starts_with("color:") || src.starts_with('#') {
        return SourceKind::Color;
    }
    if let Some(id) = src.strip_prefix("patch.") {
        let web = app.m.q_list("patches").iter().any(|p| text(p, "id") == id && text(p, "kind") == "web");
        return if web { SourceKind::Web } else { SourceKind::Generative };
    }
    if src == "youtube" {
        return SourceKind::Web;
    }
    let row = project_sources(app).iter().chain(app.m.q_list("sources")).find(|s| text(s, "name") == src);
    match row {
        Some(r) if text(r, "kind") == "file" => media_kind(text(r, "file")),
        _ => SourceKind::Camera,
    }
}

/// Web pages and generative visuals shown as sources: visual patches drawn as a source or above
/// every scene (effects, transitions and sound processors live on their own pages). A patch
/// whose manifest doesn't load shows too, so it can be fixed.
fn is_source_patch(p: &Value) -> bool {
    let (kind, layer) = (text(p, "kind"), text(p, "layer"));
    kind.is_empty() || (matches!(kind, "web" | "shader" | "particles" | "script") && matches!(layer, "source" | "overlay"))
}

fn patch_kind_words(p: &Value) -> &'static str {
    match text(p, "kind") {
        "web" => "Web page",
        "shader" => "Shader",
        "particles" => "Particles",
        "script" => "Script",
        _ => "Can't load",
    }
}

/// A camera's modes the capture path supports: (format, width, height, fps).
fn modes(device: &Value) -> Vec<(String, i64, i64, f64)> {
    let mut out = Vec::new();
    for mode in list(device, "modes") {
        let format = text(mode, "format");
        if !matches!(format, "yuyv" | "mjpeg") {
            continue;
        }
        for fps in list(mode, "fps").iter().filter_map(Value::as_f64).filter(|f| (1.0..=240.0).contains(f)) {
            out.push((format.into(), num(mode, "width") as i64, num(mode, "height") as i64, fps));
        }
    }
    out
}

fn mode_words(m: &(String, i64, i64, f64)) -> String {
    format!("{} × {} · {} fps · {}", m.1, m.2, (m.3 * 100.0).round() / 100.0, m.0.to_uppercase())
}

/// Discovered cameras (`devices`).
fn cameras(app: &App) -> Vec<Value> {
    app.m.q("devices").map(|v| list(v, "devices")).unwrap_or(&[]).iter().filter(|d| text(d, "kind") == "camera").cloned().collect()
}

/// The discovered camera a source's `device` names (identity or device path).
fn camera_for(cams: &[Value], device: &str) -> Option<Value> {
    if device.is_empty() {
        return None;
    }
    cams.iter().find(|d| text(d, "identity") == device || text(d, "path") == device).cloned()
}

/// `s` cut to `max` characters with "…".
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>().trim_end()) }
}

/// Scenes that place `src`: (scene name, label, layer id when it can be told).
fn scenes_using(app: &App, src: &str) -> Vec<(String, String, Option<String>)> {
    app.m
        .q_list("scenes")
        .iter()
        .filter(|sc| list(sc, "sources").iter().any(|v| v.as_str() == Some(src)))
        .map(|sc| {
            let layer = list(sc, "nodes").iter().filter_map(Value::as_str).find(|n| *n == src).map(String::from);
            (text(sc, "name").to_string(), text(sc, "label").to_string(), layer)
        })
        .collect()
}

/// Scene names from `project.sources` references (`scenes/show.toml: canvas.wide.nodes[0].src`),
/// which also cover scene files that don't load.
fn referenced_scenes(row: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in list(row, "references").iter().filter_map(Value::as_str) {
        let file = r.split(':').next().unwrap_or("");
        let name = std::path::Path::new(file).file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

fn scene_label(app: &App, name: &str) -> String {
    app.m
        .q_list("scenes")
        .iter()
        .find(|s| text(s, "name") == name)
        .map(|s| text(s, "label").to_string())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| nice(name))
}

/// "A, B and C".
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn quoted(s: &str) -> String {
    format!("\u{201c}{s}\u{201d}")
}

// ---- list -------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    /// A camera or media file (`sources/<name>.toml`).
    Source(String),
    /// A web page or generative visual (`patches/<id>/`).
    Patch(String),
}

impl Key {
    /// Its `src` in a scene.
    fn src(&self) -> String {
        match self {
            Key::Source(n) => n.clone(),
            Key::Patch(id) => format!("patch.{id}"),
        }
    }
}

struct Row {
    key: Key,
    kind: SourceKind,
    title: String,
    subtitle: String,
    trailing: String,
}

const GROUPS: [(&str, &[SourceKind]); 4] = [
    ("Cameras & capture", &[SourceKind::Camera]),
    ("Media", &[SourceKind::Video, SourceKind::Image]),
    ("Web", &[SourceKind::Web]),
    ("Generative", &[SourceKind::Generative]),
];

fn usage_words(scenes: usize, every: bool) -> String {
    match (every, scenes) {
        (true, _) => "on every scene".into(),
        (false, 0) => "not in a scene".into(),
        (false, 1) => "in 1 scene".into(),
        (false, n) => format!("in {n} scenes"),
    }
}

fn rows(app: &App, project_toml: &str) -> Vec<Row> {
    let cams = cameras(app);
    let mut out = Vec::new();
    for s in project_sources(app) {
        let name = text(s, "name");
        let camera = text(s, "kind") == "camera";
        let kind = if camera { SourceKind::Camera } else { media_kind(text(s, "file")) };
        let subtitle = if camera {
            camera_for(&cams, text(s, "device"))
                .map(|d| text(&d, "label").to_string())
                .unwrap_or_else(|| text(s, "device").rsplit('/').next().unwrap_or("").to_string())
        } else {
            text(s, "file").rsplit('/').next().unwrap_or("").to_string()
        };
        let problem = !text(s, "error").is_empty() || runtime_source(app, name).is_some_and(|r| r.get_path("missing").is_some_and(Value::truthy));
        let used = scenes_using(app, name).len().max(referenced_scenes(s).len());
        out.push(Row {
            key: Key::Source(name.to_string()),
            kind,
            title: clip(&if text(s, "label").is_empty() { nice(name) } else { text(s, "label").to_string() }, 26),
            subtitle: clip(&subtitle, 30),
            trailing: if problem { "needs a look".into() } else { usage_words(used, false) },
        });
    }
    for p in app.m.q_list("patches").iter().filter(|p| is_source_patch(p)) {
        let id = text(p, "id");
        let every = text(p, "layer") == "overlay" && patches::on_every_scene(project_toml, id);
        let kind = if text(p, "kind") == "web" { SourceKind::Web } else { SourceKind::Generative };
        let problem = patches::status(app, p).1;
        out.push(Row {
            key: Key::Patch(id.to_string()),
            kind,
            title: clip(&patches::label(p), 26),
            subtitle: patch_kind_words(p).to_string(),
            trailing: if problem { "needs a look".into() } else { usage_words(scenes_using(app, &format!("patch.{id}")).len(), every) },
        });
    }
    out.sort_by_key(|a| a.title.to_lowercase());
    out
}

/// The list pane; returns (clicked row, "+" clicked).
fn list_pane(ui: &mut Ui, t: &Theme, rows: &[Row], selected: Option<&Key>) -> (Option<Key>, bool) {
    let create = widgets::pane_header(ui, t, "Sources", Some(rows.len()), Some("New source"));
    let mut picked = None;
    egui::ScrollArea::vertical().id_salt("sources-list").auto_shrink([false, false]).show(ui, |ui| {
        for (title, kinds) in GROUPS {
            let group: Vec<&Row> = rows.iter().filter(|r| kinds.contains(&r.kind)).collect();
            if group.is_empty() {
                continue;
            }
            widgets::group_label(ui, t, title);
            for r in group {
                if widgets::list_row(ui, t, r.kind.icon(), &r.title, &r.subtitle, &r.trailing, selected == Some(&r.key)).clicked() {
                    picked = Some(r.key.clone());
                }
            }
        }
    });
    (picked, create)
}

// ---- state ------------------------------------------------------------------------------------

/// A new source being set up (nothing exists until Create).
#[derive(Clone)]
struct Draft {
    kind: Option<SourceKind>,
    name: String,
    device: String,
    format: String,
    width: i64,
    height: i64,
    fps: f64,
    file: String,
    looping: bool,
    /// Template (`patch.templates` name) and its kind (`web`, `shader`, `particles`, `script`).
    template: String,
    template_kind: String,
    every_scene: bool,
}

impl Default for Draft {
    fn default() -> Self {
        Draft {
            kind: None,
            name: String::new(),
            device: String::new(),
            format: "yuyv".into(),
            width: 0,
            height: 0,
            fps: 0.0,
            file: String::new(),
            looping: true,
            template: String::new(),
            template_kind: String::new(),
            every_scene: true,
        }
    }
}

impl Draft {
    fn choose_mode(&mut self, m: &(String, i64, i64, f64)) {
        self.format.clone_from(&m.0);
        self.width = m.1;
        self.height = m.2;
        self.fps = m.3;
    }

    /// Source name / patch id from the typed name.
    fn id(&self) -> String {
        scene_edit::slug(&self.name)
    }

    /// Why Create isn't possible yet.
    fn issue(&self, app: &App) -> Option<&'static str> {
        let id = self.id();
        if id.is_empty() {
            return Some("Give it a name.");
        }
        if project_sources(app).iter().any(|s| text(s, "name") == id) || app.m.q_list("patches").iter().any(|p| text(p, "id") == id) {
            return Some("That name is taken.");
        }
        match self.kind? {
            SourceKind::Camera => {
                if self.device.trim().is_empty() {
                    return Some("Choose a camera.");
                }
                if !matches!(self.format.as_str(), "yuyv" | "mjpeg")
                    || !(16..=8192).contains(&self.width)
                    || !(16..=8192).contains(&self.height)
                    || !(1.0..=240.0).contains(&self.fps)
                {
                    return Some("Choose a mode, or enter one by hand.");
                }
                if self.format == "yuyv" && self.width % 2 != 0 {
                    return Some("YUYV needs an even width.");
                }
            }
            SourceKind::Video if self.file.is_empty() => return Some("Choose a video."),
            SourceKind::Image if self.file.is_empty() => return Some("Choose a picture or GIF."),
            SourceKind::Image if !is_picture_file(&self.file) => return Some("SVG pictures can't be shown as a source. Use PNG, JPEG, WebP or GIF."),
            SourceKind::Web | SourceKind::Generative if self.template.is_empty() => return Some("Choose what to start from."),
            _ => {}
        }
        None
    }
}

/// A camera's temporary preview picture (`video_in.preview` lease).
#[derive(Clone, Default)]
struct CamPreview {
    identity: String,
    seq: i64,
    tex: Option<TextureHandle>,
    state: String,
    leased: Option<Instant>,
    asked: Option<Instant>,
}

impl CamPreview {
    /// Keep the lease for `identity` and take new pictures; call while it's on screen.
    fn pump(&mut self, app: &mut App, ctx: &egui::Context, identity: &str, now: Instant) {
        use base64::Engine;
        if self.identity != identity {
            *self = CamPreview { identity: identity.to_string(), ..Default::default() };
        }
        if let Some(p) = app.m.q(PREVIEW_KEY).map(|v| list(v, "previews")).unwrap_or(&[]).iter().find(|p| text(p, "identity") == identity) {
            self.state = text(p, "state").to_string();
            let seq = p.get_path("seq").and_then(Value::as_i64).unwrap_or(0);
            if seq == 0 {
                self.tex = None;
            } else if seq != self.seq
                && let Some(bytes) = p.get_path("jpeg").and_then(Value::as_str).and_then(|x| base64::engine::general_purpose::STANDARD.decode(x).ok())
                && let Ok(img) = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
            {
                let rgba = img.to_rgba8();
                let ci = egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
                match &mut self.tex {
                    Some(tex) => tex.set(ci, egui::TextureOptions::LINEAR),
                    None => self.tex = Some(ctx.load_texture(format!("sources.preview.{identity}"), ci, egui::TextureOptions::LINEAR)),
                }
                self.seq = seq;
            }
        }
        if !app.m.connected {
            return;
        }
        if self.leased.is_none_or(|t| now.duration_since(t) >= LEASE_RENEW) {
            self.leased = Some(now);
            app.m.action("video_in.preview", Value::map().with("identity", identity).with("on", true).with("lease", LEASE_SECS));
        }
        if self.asked.is_none_or(|t| now.duration_since(t) >= PREVIEW_EVERY) {
            self.asked = Some(now);
            let have = if self.seq > 0 { vec![Value::map().with("identity", identity).with("seq", self.seq)] } else { Vec::new() };
            app.m.query_as(PREVIEW_KEY, "video_in.preview", Value::map().with("have", Value::List(have)));
        }
        ctx.request_repaint_after(PREVIEW_EVERY);
    }
}

/// "Add to scene…": the scene file being read before the layer is added.
#[derive(Clone)]
struct Adding {
    scene: String,
    src: String,
    seq: u64,
    since: Instant,
}

/// A web page or generative visual just asked for: finished once the loader has it.
#[derive(Clone)]
struct AfterCreate {
    id: String,
    label: String,
    every_scene: bool,
    since: Instant,
}

#[derive(Clone, Default)]
struct State {
    selected: Option<Key>,
    creating: bool,
    draft: Draft,
    /// `project.source.save|remove` waiting for the engine: (source name, since).
    pending: Option<(String, Instant)>,
    error: String,
    events: Option<u64>,
    refreshed: Option<Instant>,
    project_asked: Option<Instant>,
    adding: Option<Adding>,
    after_create: Option<AfterCreate>,
    preview: CamPreview,
    /// Camera control values being changed here: address → (value, when).
    ctrl_edits: HashMap<String, (Value, Instant)>,
    /// Color slider being dragged: address → value.
    name_edit: Option<(Key, String)>,
    /// "On every scene" just switched: (patch id, on, when); shown until project.toml catches up.
    every_local: Option<(String, bool, Instant)>,
    /// On air: Play once waits for a confirmation.
    confirm_play: bool,
    /// Just added to a scene: (scene, layer), offered as "Open scene".
    added: Option<(String, String)>,
}

impl State {
    fn refresh(&mut self, app: &mut App, now: Instant) {
        if !app.m.connected {
            return;
        }
        if self.refreshed.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(2)) {
            self.refreshed = Some(now);
            for q in ["project.sources", "sources", "scenes", "devices"] {
                app.m.query(q, Value::Null);
            }
        }
        if self.project_asked.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(5)) {
            self.project_asked = Some(now);
            app.m.query_as(PROJECT_KEY, "project.read", Value::map().with("path", "project.toml"));
        }
        self.ctrl_edits.retain(|_, (_, at)| now.duration_since(*at) < Duration::from_millis(1200));
        if self.every_local.as_ref().is_some_and(|(_, _, at)| now.duration_since(*at) > Duration::from_secs(4)) {
            self.every_local = None;
        }
    }

    /// Read project.toml again shortly (after writing it).
    fn reread_project(&mut self, now: Instant) {
        self.project_asked = now.checked_sub(Duration::from_millis(4500));
    }

    /// Engine answers to `project.source.save|remove`.
    fn follow_events(&mut self, app: &mut App, now: Instant) {
        let seen = app.m.events_seen;
        let new = seen.saturating_sub(self.events.unwrap_or(seen)) as usize;
        self.events = Some(seen);
        let mut refresh = false;
        let n = app.m.events.len();
        let events: Vec<(String, Value)> = app.m.events.iter().skip(n.saturating_sub(new)).map(|e| (e.ty.clone(), e.payload.clone())).collect();
        for (ty, payload) in events {
            let name = text(&payload, "name").to_string();
            let ours = self.pending.as_ref().is_some_and(|(p, _)| *p == name);
            match ty.as_str() {
                "project.sources.changed" => {
                    refresh = true;
                    if !ours {
                        continue;
                    }
                    self.pending = None;
                    self.error.clear();
                    if text(&payload, "action") == "project.source.remove" {
                        if self.selected == Some(Key::Source(name.clone())) {
                            self.selected = None;
                        }
                        app.m.toast("Source removed.", false);
                    } else if self.creating {
                        self.creating = false;
                        self.selected = Some(Key::Source(name));
                        self.draft = Draft::default();
                    }
                }
                "project.source.failed" if ours => {
                    self.pending = None;
                    self.error = text(&payload, "error").to_string();
                }
                _ => {}
            }
        }
        if self.pending.as_ref().is_some_and(|(_, at)| !app.m.connected || now.duration_since(*at) > CONFIRM_WAIT) {
            self.pending = None;
            self.error = "Stream Engine didn't confirm this change. Check the list before trying again.".into();
        }
        if refresh {
            self.refreshed = None;
        }
    }

    fn save(&mut self, app: &mut App, name: &str, args: Value) {
        self.pending = Some((name.to_string(), Instant::now()));
        self.error.clear();
        app.m.action("project.source.save", args.with("name", name));
    }

    /// A web page or generative visual appeared after Create: name it as typed, keep it off
    /// the other scenes when asked, and select it.
    fn after_create_step(&mut self, app: &mut App, now: Instant) {
        let Some(a) = self.after_create.clone() else { return };
        if let Some(p) = patches::find(app, &a.id) {
            if !a.label.trim().is_empty() && patches::label(&p) != a.label.trim() {
                patches::rename(app, &a.id, &a.label);
            }
            if !a.every_scene && text(&p, "layer") == "overlay" {
                set_every_scene(app, self, &a.id, false, now);
            }
            self.after_create = None;
            self.creating = false;
            self.draft = Draft::default();
            self.selected = Some(Key::Patch(a.id.clone()));
            app.m.toast(format!("Made {}.", quoted(a.label.trim())), false);
        } else if now.duration_since(a.since) > CONFIRM_WAIT {
            self.after_create = None;
            self.error = "It wasn't made. Stream Engine's messages (Settings → Troubleshooting) say why.".into();
        }
    }

    /// "Add to scene…": once the scene file arrives, add the layer and save the scene.
    fn adding_step(&mut self, app: &mut App, now: Instant) {
        let Some(a) = self.adding.clone() else { return };
        let key = format!("sources.scene:{}", a.scene);
        if app.m.q_seq(&key) > a.seq {
            self.adding = None;
            let label = scene_label(app, &a.scene);
            let text = app.m.q(&key).and_then(|v| v.get_path("text")).and_then(Value::as_str).unwrap_or("").to_string();
            match scene_edit::add_layer(&text, &a.src) {
                Ok((new, layer)) => {
                    let path = format!("scenes/{}.toml", a.scene);
                    app.m.action("project.write", Value::map().with("path", path).with("text", new));
                    composition::scene_file_changed(app, &a.scene);
                    self.added = Some((a.scene.clone(), layer));
                    app.m.refresh_soon();
                    self.refreshed = None;
                }
                Err(e) => app.m.toast(format!("Couldn't add it to {}: {e:#}", quoted(&label)), true),
            }
        } else if now.duration_since(a.since) > Duration::from_secs(10) {
            self.adding = None;
            app.m.toast("Couldn't read the scene to add it. Try again.", true);
        }
    }

    fn start_add(&mut self, app: &mut App, scene: &str, src: &str) {
        let key = format!("sources.scene:{scene}");
        let seq = app.m.q_seq(&key);
        app.m.query_as(&key, "project.read", Value::map().with("path", format!("scenes/{scene}.toml")));
        self.adding = Some(Adding { scene: scene.to_string(), src: src.to_string(), seq, since: Instant::now() });
    }

    fn start_new(&mut self) {
        self.creating = true;
        self.draft = Draft::default();
        self.error.clear();
        self.added = None;
    }
}

/// Switch "On every scene" for an overlay-layer patch (`[overlays.<id>] canvases`).
fn set_every_scene(app: &mut App, st: &mut State, id: &str, on: bool, now: Instant) {
    let v = if on { Value::Null } else { Value::List(Vec::new()) };
    app.m.action("project.write", Value::map().with("path", "project.toml").with("set", Value::map().with(format!("overlays.{id}.canvases"), v)));
    st.every_local = Some((id.to_string(), on, now));
    st.reread_project(now);
}

/// Open the Sources page with a new-source draft for a file from Files (a video, or a picture
/// or GIF).
pub fn new_from_file(app: &mut App, ctx: &egui::Context, path: &str) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(FROM_FILE), path.to_string()));
    app.open_view(ViewId::Sources);
}

/// Open this page on the source used by a scene layer.
pub fn open(app: &mut App, ctx: &egui::Context, src: &str) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(OPEN_SOURCE), src.to_string()));
    app.open_view(ViewId::Sources);
}

// ---- page -------------------------------------------------------------------------------------

pub fn ui(app: &mut App, ui: &mut Ui) {
    let id = egui::Id::new("sources-view");
    let now = Instant::now();
    let mut st: State = ui.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<State>(id)));
    st.refresh(app, now);
    patches::refresh(app, ui);
    if let Some(path) = ui.ctx().data_mut(|d| d.remove_temp::<String>(egui::Id::new(FROM_FILE))) {
        st.start_new();
        st.draft.kind = Some(media_kind(&path));
        st.draft.name = nice(std::path::Path::new(&path).file_stem().and_then(|s| s.to_str()).unwrap_or(""));
        st.draft.file = path;
    }
    if let Some(src) = ui.ctx().data_mut(|d| d.remove_temp::<String>(egui::Id::new(OPEN_SOURCE))) {
        st.selected = Some(match src.strip_prefix("patch.") {
            Some(id) => Key::Patch(id.to_string()),
            None => Key::Source(src),
        });
        st.creating = false;
    }
    st.follow_events(app, now);
    st.after_create_step(app, now);
    st.adding_step(app, now);

    let project_toml = app.m.q(PROJECT_KEY).and_then(|v| v.get_path("text")).and_then(Value::as_str).unwrap_or("").to_string();
    let rows = rows(app, &project_toml);
    // a selection that's gone (removed elsewhere) falls back to the first source
    let loaded = app.m.q("project.sources").is_some() && app.m.q("patches").is_some();
    if loaded && st.selected.as_ref().is_some_and(|k| !rows.iter().any(|r| r.key == *k)) && st.after_create.is_none() {
        st.selected = None;
    }
    if st.selected.is_none() && !st.creating {
        st.selected = rows.first().map(|r| r.key.clone());
    }
    let t = app.t.clone();
    let shown = if st.creating { None } else { st.selected.clone() };
    let ((picked, create), ()) =
        widgets::split(ui, LIST_W, |ui| list_pane(ui, &t, &rows, shown.as_ref()), |ui| detail(app, ui, &mut st, &rows, &project_toml, now));
    if let Some(k) = picked {
        if st.selected.as_ref() != Some(&k) {
            st.confirm_play = false;
            st.added = None;
            st.error.clear();
        }
        st.selected = Some(k);
        st.creating = false;
    }
    if create {
        st.start_new();
    }
    ui.data_mut(|d| d.insert_temp(id, st));
}

fn detail(app: &mut App, ui: &mut Ui, st: &mut State, rows: &[Row], project_toml: &str, now: Instant) {
    let t = app.t.clone();
    egui::ScrollArea::vertical().id_salt("sources-detail").auto_shrink([false, false]).show(ui, |ui| {
        if st.creating {
            new_source(app, ui, st);
            return;
        }
        if let Some((scene, layer)) = st.added.clone() {
            let body = "It's a new layer in the middle of the scene. Move and size it there.";
            if widgets::callout(ui, &t, Tone::Ok, icon::CHECK, &format!("Added to {}", quoted(&scene_label(app, &scene))), body, Some("Open scene")) {
                st.added = None;
                composition::open(app, &scene, Some(&layer));
            }
            ui.add_space(spacing::M);
        }
        match st.selected.clone() {
            Some(Key::Source(name)) => match project_source(app, &name) {
                Some(row) => source_detail(app, ui, st, &row, now),
                None => {
                    widgets::empty_state(ui, &t, icon::CAMERA, "Loading…", "", None);
                }
            },
            Some(Key::Patch(id)) => match patches::find(app, &id) {
                Some(p) => patch_detail(app, ui, st, &p, project_toml, now),
                None => {
                    widgets::empty_state(ui, &t, icon::SPARKLE, "Loading…", "", None);
                }
            },
            None => {
                let body = if !app.m.connected {
                    "Stream Engine isn't running. Your sources show here once it is."
                } else if rows.is_empty() {
                    "A source is anything that makes picture or sound: a camera, a video, a picture, a web page or a generative visual. Make it once, then place it in any scene."
                } else {
                    "Choose a source on the left."
                };
                if widgets::empty_state(ui, &t, icon::CAMERA, "Sources", body, app.m.connected.then_some("New source")) {
                    st.start_new();
                }
            }
        }
    });
}

// ---- shared detail parts ------------------------------------------------------------------------

/// "Add to scene…" with a menu of scenes.
fn add_to_scene_button(app: &mut App, ui: &mut Ui, st: &mut State, src: &str) {
    let t = app.t.clone();
    let busy = st.adding.is_some();
    let r = widgets::button_ex(
        ui,
        &t,
        Some(icon::PLUS),
        if busy { "Adding…" } else { "Add to scene…" },
        Kind::Secondary,
        Size::Small,
        0.0,
        !busy && app.m.connected,
    );
    let scenes: Vec<(String, String, bool)> = app
        .m
        .q_list("scenes")
        .iter()
        .map(|s| (text(s, "name").to_string(), text(s, "label").to_string(), list(s, "sources").iter().any(|v| v.as_str() == Some(src))))
        .collect();
    let pop = egui::Id::new(("sources-add-to-scene", src));
    let mut pick = None;
    egui::Popup::from_toggle_button_response(&r).id(pop).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).width(280.0).show(|ui| {
        ui.set_width(280.0);
        if scenes.is_empty() {
            widgets::hint(ui, &t, "You don't have any scenes yet. Make one in Scenes.");
        }
        for (name, label, has) in &scenes {
            let label = if label.is_empty() { nice(name) } else { label.clone() };
            if widgets::list_row(ui, &t, icon::LAYERS, &clip(&label, 24), "", if *has { "in it already" } else { "" }, false).clicked() {
                pick = Some(name.clone());
            }
        }
    });
    if let Some(scene) = pick {
        egui::Popup::close_id(ui.ctx(), pop);
        st.start_add(app, &scene, src);
    }
}

/// The picture: a camera's preview, the source's tile while a scene shows it, a media file's
/// own picture, else the kind's illustration with what it's waiting for.
#[allow(clippy::too_many_arguments)]
fn preview(app: &mut App, ui: &mut Ui, st: &mut State, kind: SourceKind, src: &str, camera: Option<&str>, file: Option<&str>, waiting: &str) {
    let t = app.t.clone();
    let w = ui.available_width().min(560.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, (w * 9.0 / 16.0).round()), Sense::hover());
    let full = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    if let Some(identity) = camera.filter(|i| !i.is_empty()) {
        st.preview.pump(app, ui.ctx(), identity, Instant::now());
        if let Some(tex) = &st.preview.tex {
            widgets::video_frame(ui, &t, rect, Some((tex.id(), full)), "", None, None);
            ui.add_space(spacing::L);
            return;
        }
    }
    let hz = app.atlas_hz();
    if let Some((_, r)) = monitor::atlas_tiles(app).into_iter().find(|(s, _)| s == src)
        && let Some(tex) = monitor::texture(app, Canvas::Atlas, hz)
    {
        let uv = Rect::from_min_size(Pos2::new(r[0], r[1]), Vec2::new(r[2], r[3]));
        widgets::video_frame(ui, &t, rect, Some((tex.id, uv)), "", None, None);
        ui.add_space(spacing::L);
        return;
    }
    if let Some(f) = file.filter(|f| !f.is_empty())
        && media::file_preview(app, ui, rect, f)
    {
        ui.add_space(spacing::L);
        return;
    }
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(se_ui_kit::theme::radius::TILE), t.inset);
    p.text(rect.center() - Vec2::new(0.0, 14.0), Align2::CENTER_CENTER, kind.icon(), font(34.0), t.text_faint);
    p.text(rect.center() + Vec2::new(0.0, 26.0), Align2::CENTER_CENTER, waiting, font(type_scale::SMALL + 0.5), t.text_dim);
    ui.add_space(spacing::L);
}

/// Name row: returns the new name when it's committed (Enter or leaving the field).
fn name_row(ui: &mut Ui, t: &Theme, st: &mut State, key: &Key, current: &str) -> Option<String> {
    if st.name_edit.as_ref().is_none_or(|(k, _)| k != key) {
        st.name_edit = Some((key.clone(), current.to_string()));
    }
    let (_, buf) = st.name_edit.as_mut()?;
    widgets::prop_row(ui, t, "Name", |ui| {
        let r = ui.add(widgets::field(buf).desired_width(ui.available_width().min(320.0)));
        if !r.has_focus() && !r.lost_focus() && *buf != current {
            // someone else renamed it, or a rename didn't take: show what it's called
            buf.clear();
            buf.push_str(current);
        }
        let name = buf.trim().to_string();
        (r.lost_focus() && !name.is_empty() && name != current).then_some(name)
    })
}

/// "Used in": the scenes that place it (click to open the scene), and "On every scene".
fn used_in(app: &mut App, ui: &mut Ui, src: &str, extra: &[String], every: bool) {
    let t = app.t.clone();
    let mut scenes = scenes_using(app, src);
    for name in extra {
        if !scenes.iter().any(|(n, _, _)| n == name) {
            scenes.push((name.clone(), scene_label(app, name), None));
        }
    }
    let mut open = None;
    widgets::inspector_section(
        ui,
        &t,
        ("sources-used", src),
        "Used in",
        true,
        |_| {},
        |ui| {
            if every {
                widgets::list_row(ui, &t, icon::LAYERS, "On every scene", "Drawn above all your scenes", "", false);
            }
            if scenes.is_empty() && !every {
                widgets::hint(ui, &t, "Not in any scene yet. Use Add to scene… to place it.");
            }
            for (name, label, layer) in &scenes {
                let label = if label.is_empty() { nice(name) } else { label.clone() };
                if widgets::list_row(ui, &t, icon::LAYERS, &clip(&label, 30), "", "Open", false).on_hover_text("Open this scene").clicked() {
                    open = Some((name.clone(), layer.clone()));
                }
            }
        },
    );
    if let Some((scene, layer)) = open {
        composition::open(app, &scene, layer.as_deref());
    }
}

/// "Remove": hold to remove; not while a scene still uses it (it says which).
fn remove_section(ui: &mut Ui, t: &Theme, salt: &str, used: &[String], note: &str, busy: bool) -> bool {
    let mut go = false;
    widgets::inspector_section(
        ui,
        t,
        ("sources-remove", salt),
        "Remove",
        false,
        |_| {},
        |ui| {
            if used.is_empty() {
                widgets::hint(ui, t, note);
            } else {
                widgets::hint(
                    ui,
                    t,
                    &format!("It's in {}. Take it out of {} first.", and_list(used), if used.len() == 1 { "that scene" } else { "those scenes" }),
                );
            }
            ui.add_space(spacing::S);
            ui.add_enabled_ui(used.is_empty() && !busy, |ui| {
                go = widgets::hold_button(ui, t, "Hold to remove", t.bright_red, 1.0);
            });
        },
    );
    go
}

/// Color correction (`source.<n>.color.*`) with ∿ modulation beside each setting.
fn color_section(app: &mut App, ui: &mut Ui, name: &str) {
    const FIELDS: [(&str, &str, f64, f64, f64); 6] = [
        ("brightness", "Brightness", 0.0, -1.0, 1.0),
        ("contrast", "Contrast", 1.0, 0.0, 3.0),
        ("saturation", "Saturation", 1.0, 0.0, 3.0),
        ("gamma", "Gamma", 1.0, 0.2, 5.0),
        ("temperature", "Warmth", 0.0, -1.0, 1.0),
        ("tint", "Tint", 0.0, -1.0, 1.0),
    ];
    let t = app.t.clone();
    let addr = |f: &str| format!("source.{name}.color.{f}");
    let declared = app.m.has(&addr("brightness"));
    let mut reset = false;
    widgets::inspector_section(
        ui,
        &t,
        ("sources-color", name),
        "Color",
        true,
        |ui| {
            if declared {
                reset = widgets::button_ex(ui, &t, Some(icon::UNDO), "Reset", Kind::Ghost, Size::Small, 0.0, true).clicked();
            }
        },
        |ui| {
            if !declared {
                widgets::hint(ui, &t, "Color settings show while video is running.");
                return;
            }
            for (f, label, default, lo, hi) in FIELDS {
                let a = addr(f);
                let cur = app.m.get(&a).and_then(Value::as_f64).unwrap_or(default);
                let drag = egui::Id::new(("sources-color-drag", &a));
                widgets::prop_row(ui, &t, label, |ui| {
                    let mut x = ui.data(|d| d.get_temp::<f64>(drag)).unwrap_or(cur);
                    ui.spacing_mut().slider_width = (ui.available_width() - 110.0).max(80.0);
                    let r = ui.add(egui::Slider::new(&mut x, lo..=hi).max_decimals(2));
                    if r.dragged() {
                        ui.data_mut(|d| d.insert_temp(drag, x));
                    } else {
                        ui.data_mut(|d| d.remove::<f64>(drag));
                    }
                    if r.drag_stopped() || (r.changed() && !r.dragged()) {
                        app.m.command(Op::SetBase { address: a.clone(), value: Value::Float((x * 1000.0).round() / 1000.0) });
                    }
                    links::modulate_button(app, ui, &a, label, (lo, hi));
                });
            }
        },
    );
    if reset {
        for (f, _, default, _, _) in FIELDS {
            app.m.command(Op::SetBase { address: addr(f), value: Value::Float(default) });
        }
    }
}

// ---- cameras and media files ------------------------------------------------------------------

/// Running state of a camera or media source in words, and a "waiting" line for the preview.
fn source_state(app: &App, row: &Value, rt: Option<&Value>) -> (&'static str, &'static str) {
    let name = text(row, "name");
    let Some(rt) = rt else { return ("Video isn't running", "Shows here once video is running.") };
    let a = |k: &str| format!("source.{name}.{k}");
    let signal = app.m.get(&a("signal")).map(Value::truthy).unwrap_or(rt.get_path("signal").is_some_and(Value::truthy));
    let capturing = app.m.get(&a("capturing")).map(Value::truthy).unwrap_or(rt.get_path("capturing").is_some_and(Value::truthy));
    if rt.get_path("missing").is_some_and(Value::truthy) {
        ("Not connected", "Its camera isn't plugged in.")
    } else if !text(rt, "error").is_empty() && !capturing {
        ("Has a problem", "It couldn't start.")
    } else if !rt.get_path("used").is_some_and(Value::truthy) {
        ("Off until a scene shows it", "Shows here while a scene on preview or program uses it.")
    } else if signal {
        ("Working", "")
    } else {
        ("No picture", "Nothing is coming in. Is it switched on?")
    }
}

fn source_detail(app: &mut App, ui: &mut Ui, st: &mut State, row: &Value, now: Instant) {
    let t = app.t.clone();
    let name = text(row, "name").to_string();
    let key = Key::Source(name.clone());
    let camera = text(row, "kind") == "camera";
    let file = text(row, "file").to_string();
    let kind = if camera { SourceKind::Camera } else { media_kind(&file) };
    let title = if text(row, "label").is_empty() { nice(&name) } else { text(row, "label").to_string() };
    let rt = runtime_source(app, &name);
    let (state, waiting) = source_state(app, row, rt.as_ref());
    let cams = cameras(app);
    let device = camera_for(&cams, text(row, "device"));
    widgets::detail_header(ui, &t, kind.icon(), &title, &format!("{} · {state}", kind.label()), |ui| add_to_scene_button(app, ui, st, &name));
    let identity =
        if camera { device.as_ref().map(|d| text(d, "identity").to_string()).or_else(|| rt.as_ref().map(|r| text(r, "identity").to_string())) } else { None };
    preview(app, ui, st, kind, &name, identity.as_deref(), (!camera).then_some(file.as_str()), waiting);
    for e in [text(row, "error"), rt.as_ref().map_or("", |r| text(r, "config_error"))].into_iter().filter(|e| !e.is_empty()) {
        widgets::callout(ui, &t, Tone::Danger, icon::WARN, "Its settings have a problem", e, None);
        ui.add_space(spacing::M);
    }
    if !st.error.is_empty() && widgets::callout(ui, &t, Tone::Warn, icon::WARN, "That didn't work", &st.error.clone(), Some("OK")) {
        st.error.clear();
    }
    let busy = st.pending.is_some() || !app.m.connected;
    if camera {
        camera_section(app, ui, st, &key, row, &title, device.as_ref(), &cams, busy);
        camera_controls(app, ui, st, &name, rt.as_ref(), now);
    } else {
        media_section(app, ui, st, &key, row, &title, kind, busy);
    }
    color_section(app, ui, &name);
    let used: Vec<String> = {
        let mut u = referenced_scenes(row);
        for (n, _, _) in scenes_using(app, &name) {
            if !u.contains(&n) {
                u.push(n);
            }
        }
        u
    };
    used_in(app, ui, &name, &referenced_scenes(row), false);
    let labels: Vec<String> = used.iter().map(|n| quoted(&scene_label(app, n))).collect();
    let note =
        if camera { "Removes this source. The camera itself stays set up in Settings → Devices." } else { "Removes this source. Its file stays in Files." };
    if remove_section(ui, &t, &name, &labels, note, busy) {
        st.pending = Some((name.clone(), Instant::now()));
        st.error.clear();
        app.m.action("project.source.remove", Value::map().with("name", name.as_str()));
    }
}

#[allow(clippy::too_many_arguments)]
fn camera_section(app: &mut App, ui: &mut Ui, st: &mut State, key: &Key, row: &Value, title: &str, device: Option<&Value>, cams: &[Value], busy: bool) {
    let t = app.t.clone();
    let name = text(row, "name").to_string();
    let saved = (text(row, "format").to_string(), num(row, "size.0") as i64, num(row, "size.1") as i64, num(row, "fps"));
    let mut save: Option<Value> = None;
    widgets::inspector_section(
        ui,
        &t,
        ("sources-camera", &name),
        "Camera",
        true,
        |_| {},
        |ui| {
            if let Some(label) = name_row(ui, &t, st, key, title) {
                save = Some(Value::map().with("label", label));
            }
            ui.add_enabled_ui(!busy, |ui| {
                widgets::prop_row(ui, &t, "Device", |ui| {
                    let shown = device.map(|d| text(d, "label").to_string()).unwrap_or_else(|| clip(text(row, "device"), 40));
                    egui::ComboBox::from_id_salt(("sources-device", &name)).selected_text(shown).width(ui.available_width().min(320.0)).show_ui(ui, |ui| {
                        for d in cams {
                            let chosen = device.is_some_and(|c| text(c, "identity") == text(d, "identity"));
                            if ui.selectable_label(chosen, text(d, "label")).on_hover_text(text(d, "identity")).clicked() && !chosen {
                                let mut args = Value::map().with("device", text(d, "identity"));
                                if let Some(m) = modes(d).first() {
                                    args = args.with("format", m.0.as_str()).with("size", vec![m.1, m.2]).with("fps", m.3);
                                }
                                save = Some(args);
                            }
                        }
                        if cams.is_empty() {
                            ui.label("No cameras found");
                        }
                    });
                });
                let supported = device.map(modes).unwrap_or_default();
                widgets::prop_row(ui, &t, "Mode", |ui| {
                    let current = mode_words(&saved);
                    egui::ComboBox::from_id_salt(("sources-mode", &name)).selected_text(current).width(ui.available_width().min(320.0)).show_ui(ui, |ui| {
                        for m in &supported {
                            let chosen = (m.0.as_str(), m.1, m.2, m.3) == (saved.0.as_str(), saved.1, saved.2, saved.3);
                            if ui.selectable_label(chosen, mode_words(m)).clicked() && !chosen {
                                save = Some(Value::map().with("format", m.0.as_str()).with("size", vec![m.1, m.2]).with("fps", m.3));
                            }
                        }
                        if supported.is_empty() {
                            ui.label("This camera reports no modes. Enter one by hand below.");
                        }
                    });
                });
                widgets::details(ui, &t, ("sources-manual", &name), "Enter by hand", |ui| {
                    let id = egui::Id::new(("sources-manual-draft", &name));
                    let mut d: Draft = ui.data_mut(|m| m.get_temp(id)).unwrap_or_else(|| Draft {
                        device: text(row, "device").to_string(),
                        format: saved.0.clone(),
                        width: saved.1,
                        height: saved.2,
                        fps: saved.3,
                        ..Draft::default()
                    });
                    manual_rows(ui, &t, &mut d, &name);
                    if widgets::button_ex(ui, &t, Some(icon::CHECK), "Use these", Kind::Secondary, Size::Small, 0.0, !d.device.trim().is_empty()).clicked() {
                        save = Some(
                            Value::map()
                                .with("device", d.device.trim())
                                .with("format", d.format.as_str())
                                .with("size", vec![d.width, d.height])
                                .with("fps", d.fps),
                        );
                    }
                    ui.data_mut(|m| m.insert_temp(id, d));
                });
            });
            ui.add_space(spacing::S);
            if widgets::button_ex(ui, &t, Some(icon::UNDO), "Restart camera", Kind::Ghost, Size::Small, 0.0, app.m.connected)
                .on_hover_text("Close and open the camera again")
                .clicked()
            {
                app.m.action("source.reopen", Value::map().with("source", name.as_str()));
            }
        },
    );
    if let Some(args) = save {
        st.save(app, &name, args);
    }
}

/// Device path, format and mode fields (camera draft and "Enter by hand").
fn manual_rows(ui: &mut Ui, t: &Theme, d: &mut Draft, salt: &str) {
    widgets::prop_row(ui, t, "Device path", |ui| {
        ui.add(widgets::field(&mut d.device).hint_text("/dev/v4l/by-id/… or its identity").desired_width(ui.available_width().min(320.0)));
    });
    widgets::prop_row(ui, t, "Format", |ui| {
        let mut i = usize::from(d.format == "mjpeg");
        if widgets::segmented(ui, t, &mut i, &["YUYV", "MJPEG"]) {
            d.format = if i == 1 { "mjpeg" } else { "yuyv" }.into();
        }
    });
    widgets::prop_row(ui, t, "Size", |ui| {
        ui.push_id(("size", salt), |ui| {
            ui.add(egui::DragValue::new(&mut d.width).range(0..=8192).suffix(" w"));
            ui.add(egui::DragValue::new(&mut d.height).range(0..=8192).suffix(" h"));
            ui.add(egui::DragValue::new(&mut d.fps).range(0.0..=240.0).speed(0.1).suffix(" fps"));
        });
    });
}

/// Pan, tilt & zoom and the other controls a running camera reports (`source.<n>.ctrl.*`).
fn camera_controls(app: &mut App, ui: &mut Ui, st: &mut State, name: &str, rt: Option<&Value>, now: Instant) {
    let t = app.t.clone();
    let controls: Vec<Value> = rt.map(|r| list(r, "controls").to_vec()).unwrap_or_default();
    let ptz = |c: &Value| ["pan_", "tilt_", "zoom_"].iter().any(|p| text(c, "name").starts_with(p));
    let (moves, picture): (Vec<Value>, Vec<Value>) = controls.into_iter().partition(ptz);
    if !moves.is_empty() {
        widgets::inspector_section(
            ui,
            &t,
            ("sources-ptz", name),
            "Pan, tilt & zoom",
            true,
            |_| {},
            |ui| {
                for c in &moves {
                    control_row(app, ui, st, name, c, now);
                }
            },
        );
    }
    let mut keep = false;
    let have = !picture.is_empty() || !moves.is_empty();
    let connected = app.m.connected;
    widgets::inspector_section(
        ui,
        &t,
        ("sources-controls", name),
        "Camera controls",
        !picture.is_empty(),
        |ui| {
            if have {
                keep = widgets::button_ex(ui, &t, Some(icon::SAVE), "Keep these", Kind::Ghost, Size::Small, 0.0, connected)
                    .on_hover_text("Use these settings every time the camera starts")
                    .clicked();
            }
        },
        |ui| {
            if picture.is_empty() {
                widgets::hint(
                    ui,
                    &t,
                    if rt.is_some() {
                        "Controls show while the camera is on (a scene on preview or program uses it)."
                    } else {
                        "Controls show while video is running."
                    },
                );
            }
            for c in &picture {
                control_row(app, ui, st, name, c, now);
            }
        },
    );
    if keep {
        app.m.action("source.save_controls", Value::map().with("source", name));
        app.m.toast("The camera starts with these settings from now on.", false);
    }
}

fn control_row(app: &mut App, ui: &mut Ui, st: &mut State, name: &str, c: &Value, now: Instant) {
    let t = app.t.clone();
    let cname = text(c, "name");
    let addr = format!("source.{name}.ctrl.{cname}");
    let auto = c.get_path("inactive").is_some_and(Value::truthy);
    let ro = c.get_path("read_only").is_some_and(Value::truthy);
    let live =
        st.ctrl_edits.get(&addr).map(|(v, _)| v.clone()).or_else(|| app.m.get(&addr).cloned()).or_else(|| c.get_path("value").cloned()).unwrap_or_default();
    let label = if text(c, "label").is_empty() { nice(cname) } else { text(c, "label").to_string() };
    let mut new: Option<Value> = None;
    ui.add_enabled_ui(!ro, |ui| {
        widgets::prop_row(ui, &t, &label, |ui| match text(c, "type") {
            "bool" => {
                let mut v = live.truthy();
                if widgets::toggle(ui, &t, &mut v).changed() {
                    new = Some(Value::Bool(v));
                }
            }
            "enum" => {
                let menu = list(c, "menu");
                let cur = live.as_str().unwrap_or("").to_string();
                let shown = menu.iter().find(|x| text(x, "name") == cur).map(|x| text(x, "label").to_string()).unwrap_or(cur.clone());
                egui::ComboBox::from_id_salt(("sources-ctrl", &addr)).selected_text(shown).width(ui.available_width().min(260.0)).show_ui(ui, |ui| {
                    for it in menu {
                        if ui.selectable_label(text(it, "name") == cur, text(it, "label")).clicked() {
                            new = Some(Value::Str(text(it, "name").to_string()));
                        }
                    }
                });
            }
            _ => {
                let (lo, hi) = (num(c, "min") as i64, num(c, "max") as i64);
                let mut v = live.as_i64().unwrap_or(lo);
                ui.spacing_mut().slider_width = (ui.available_width() - 110.0).max(80.0);
                if ui.add(egui::Slider::new(&mut v, lo..=hi).step_by(num(c, "step").max(1.0)).clamping(egui::SliderClamping::Always)).changed() {
                    new = Some(Value::Int(v));
                }
                links::modulate_button(app, ui, &addr, &label, (lo as f64, hi as f64));
            }
        });
    });
    if auto {
        ui.horizontal(|ui| {
            ui.add_space(widgets::PROP_LABEL_W);
            widgets::hint(ui, &t, "Set automatically by the camera right now.");
        });
    }
    if let Some(v) = new {
        st.ctrl_edits.insert(addr.clone(), (v.clone(), now));
        app.m.command(Op::Set { address: addr, value: v });
    }
}

#[allow(clippy::too_many_arguments)]
fn media_section(app: &mut App, ui: &mut Ui, st: &mut State, key: &Key, row: &Value, title: &str, kind: SourceKind, busy: bool) {
    let t = app.t.clone();
    let name = text(row, "name").to_string();
    let mut file = text(row, "file").to_string();
    let looping = row.get_path("loop").is_none_or(Value::truthy);
    let rate = row.get_path("rate").and_then(Value::as_f64).unwrap_or(1.0);
    let moving = kind == SourceKind::Video || is_gif(&file);
    let mut save: Option<Value> = None;
    let mut bad_file = false;
    widgets::inspector_section(
        ui,
        &t,
        ("sources-media", &name),
        if kind == SourceKind::Video { "Video" } else { "Image" },
        true,
        |_| {},
        |ui| {
            if let Some(label) = name_row(ui, &t, st, key, title) {
                save = Some(Value::map().with("label", label));
            }
            ui.add_enabled_ui(!busy, |ui| {
                widgets::prop_row(ui, &t, "File", |ui| {
                    let mk = if kind == SourceKind::Video { MediaKind::Video } else { MediaKind::Image };
                    if media::picker(app, ui, ("sources-file", &name), mk, &mut file) && !file.is_empty() {
                        if media_kind(&file) == SourceKind::Image && !is_picture_file(&file) {
                            bad_file = true;
                        } else {
                            save = Some(Value::map().with("file", file.as_str()));
                        }
                    }
                });
                if moving {
                    widgets::prop_row(ui, &t, "Loop", |ui| {
                        let mut on = looping;
                        if widgets::toggle(ui, &t, &mut on).changed() {
                            save = Some(Value::map().with("loop", on));
                        }
                        widgets::hint(ui, &t, if on { "Starts over at the end." } else { "Plays once, then holds the last frame." });
                    });
                    widgets::prop_row(ui, &t, "Speed", |ui| {
                        let drag = egui::Id::new(("sources-rate", &name));
                        let mut x = ui.data(|d| d.get_temp::<f64>(drag)).unwrap_or(rate);
                        let r = ui.add(egui::Slider::new(&mut x, 0.25..=4.0).logarithmic(true).max_decimals(2).suffix("×"));
                        if r.dragged() {
                            ui.data_mut(|d| d.insert_temp(drag, x));
                        } else {
                            ui.data_mut(|d| d.remove::<f64>(drag));
                        }
                        if r.drag_stopped() || (r.changed() && !r.dragged()) {
                            save = Some(Value::map().with("rate", (x * 100.0).round() / 100.0));
                        }
                    });
                }
            });
            if moving {
                playback_row(app, ui, &name);
            }
        },
    );
    if bad_file {
        st.error = "SVG pictures can't be shown as a source. Use PNG, JPEG, WebP or GIF.".into();
    }
    if let Some(args) = save {
        st.save(app, &name, args);
    }
}

/// Pause/play and start over while the player runs.
fn playback_row(app: &mut App, ui: &mut Ui, name: &str) {
    let t = app.t.clone();
    let a = |k: &str| format!("source.{name}.{k}");
    if !app.m.has(&a("playing")) {
        return;
    }
    let paused = app.m.b(&a("paused"));
    let (pos, dur) = (app.m.f(&a("position")), app.m.f(&a("duration")));
    let clock = |s: f64| format!("{}:{:02}", s.max(0.0) as i64 / 60, s.max(0.0) as i64 % 60);
    let mut out: Vec<Op> = Vec::new();
    widgets::prop_row(ui, &t, "Playback", |ui| {
        if widgets::button_ex(
            ui,
            &t,
            Some(if paused { icon::PLAY } else { icon::PAUSE }),
            if paused { "Play" } else { "Pause" },
            Kind::Secondary,
            Size::Small,
            0.0,
            true,
        )
        .clicked()
        {
            out.push(Op::Set { address: a("paused"), value: Value::Bool(!paused) });
        }
        if widgets::button_ex(ui, &t, Some(icon::UNDO), "From the start", Kind::Ghost, Size::Small, 0.0, true).clicked() {
            out.push(Op::Action { name: "source.restart".into(), args: Value::map().with("source", name) });
        }
        if dur > 0.0 {
            widgets::hint(ui, &t, &format!("{} / {}", clock(pos), clock(dur)));
        }
    });
    for op in out {
        app.m.command(op);
    }
}

// ---- web pages and generative visuals ------------------------------------------------------------

fn patch_detail(app: &mut App, ui: &mut Ui, st: &mut State, p: &Value, project_toml: &str, now: Instant) {
    let t = app.t.clone();
    let id = text(p, "id").to_string();
    let key = Key::Patch(id.clone());
    let src = key.src();
    let kind = if text(p, "kind") == "web" { SourceKind::Web } else { SourceKind::Generative };
    let title = patches::label(p);
    let (state, _) = patches::status(app, p);
    let overlay = text(p, "layer") == "overlay";
    let every = overlay
        && match &st.every_local {
            Some((i, on, _)) if *i == id => *on,
            _ => patches::on_every_scene(project_toml, &id),
        };
    let trigger = p.get_path("trigger").is_some_and(Value::truthy);
    let on_air = crate::views::status::on_air(app);
    let mut play = false;
    widgets::detail_header(ui, &t, kind.icon(), &title, &format!("{} · {state}", patch_kind_words(p)), |ui| {
        if trigger {
            let (label, k, tip) = if on_air {
                ("Play on air", Kind::Live, "You're on air: your viewers will see it.")
            } else {
                ("Play once", Kind::Secondary, "Plays it once now. You're off air: only you see it.")
            };
            play = widgets::button_ex(ui, &t, Some(icon::PLAY), label, k, Size::Small, 0.0, patches::enabled(app, p)).on_hover_text(tip).clicked();
        }
        add_to_scene_button(app, ui, st, &src);
    });
    if play {
        if on_air {
            st.confirm_play = !st.confirm_play;
        } else {
            patches::play_once(app, &id);
        }
    }
    if st.confirm_play && on_air {
        if widgets::callout(
            ui,
            &t,
            Tone::Danger,
            icon::LIVE,
            "Your viewers will see this",
            "You're on air, so it plays on stream right now.",
            Some("Play on air"),
        ) {
            patches::play_once(app, &id);
            st.confirm_play = false;
        }
        ui.add_space(spacing::M);
    }
    let desc = patches::description(p);
    if !desc.is_empty() {
        widgets::hint(ui, &t, &desc);
        ui.add_space(spacing::M);
    }
    let waiting =
        if every { "Drawn above every scene. Shows here while a scene uses it as a layer." } else { "Shows here while a scene on preview or program uses it." };
    preview(app, ui, st, kind, &src, None, None, waiting);
    if let Some(pr) = patches::problem(app, p) {
        let title = if pr.still_showing { "Its last change has a mistake" } else { "It has a mistake and can't show" };
        let body = if pr.location.is_empty() { pr.message.clone() } else { format!("{}: {}", pr.location, pr.message) };
        if widgets::callout(ui, &t, Tone::Danger, icon::WARN, title, &body, Some("Open files")) {
            patches::open_files(app, &id);
        }
        ui.add_space(spacing::M);
    }
    if !st.error.is_empty() && widgets::callout(ui, &t, Tone::Warn, icon::WARN, "That didn't work", &st.error.clone(), Some("OK")) {
        st.error.clear();
    }

    let mut rename = None;
    let mut enable = None;
    let mut every_set = None;
    widgets::inspector_section(
        ui,
        &t,
        ("sources-patch", &id),
        patch_kind_words(p),
        true,
        |_| {},
        |ui| {
            rename = name_row(ui, &t, st, &key, &title);
            widgets::prop_row(ui, &t, "On", |ui| {
                let mut on = patches::enabled(app, p);
                if widgets::toggle(ui, &t, &mut on).changed() {
                    enable = Some(on);
                }
                widgets::hint(ui, &t, if on { "Running." } else { "Off everywhere until you turn it on." });
            });
            if overlay {
                widgets::prop_row(ui, &t, "On every scene", |ui| {
                    let mut on = every;
                    if widgets::toggle(ui, &t, &mut on).changed() {
                        every_set = Some(on);
                    }
                    widgets::hint(ui, &t, if on { "Drawn above all your scenes." } else { "Only in the scenes you add it to." });
                });
            }
        },
    );
    if let Some(label) = rename {
        patches::rename(app, &id, &label);
    }
    if let Some(on) = enable {
        patches::set_enabled(app, &id, on);
    }
    if let Some(on) = every_set {
        set_every_scene(app, st, &id, on, now);
    }
    let grants = patches::grants_words(app, p);
    if !grants.is_empty() {
        widgets::callout(
            ui,
            &t,
            Tone::Info,
            icon::MOD,
            &format!("Can also control {}", and_list(&grants)),
            "Its files give it this. To take it away, edit its files.",
            None,
        );
        ui.add_space(spacing::M);
    }
    if p.get_path("params").and_then(Value::as_list).is_some_and(|l| !l.is_empty()) {
        widgets::inspector_section(ui, &t, ("sources-settings", &id), "Settings", true, |_| {}, |ui| patches::params_ui(app, ui, p));
    }
    let mut open = false;
    let mut reload = false;
    widgets::inspector_section(
        ui,
        &t,
        ("sources-files", &id),
        "Files",
        false,
        |_| {},
        |ui| {
            widgets::prop_row(ui, &t, "Folder", |ui| {
                widgets::hint(ui, &t, &format!("patches/{id}/"));
            });
            if let Some(sc) = p.get_path("script") {
                let n = |k: &str| num(sc, k);
                let budget = p.get_path("budget.cpu_ms").and_then(Value::as_f64).unwrap_or(2.0);
                widgets::prop_row(ui, &t, "Script time", |ui| {
                    widgets::hint(ui, &t, &format!("{:.2} ms a frame (peak {:.2}, allowed {budget})", n("cpu_ms_avg"), n("cpu_ms_peak")));
                });
                widgets::prop_row(ui, &t, "Memory", |ui| {
                    widgets::hint(ui, &t, &format!("{} KB", n("memory_kb") as u64));
                });
            }
            ui.horizontal(|ui| {
                open = widgets::button_ex(ui, &t, Some(icon::EDIT), "Open files", Kind::Secondary, Size::Small, 0.0, true).clicked();
                reload = widgets::button_ex(ui, &t, Some(icon::UNDO), "Reload", Kind::Ghost, Size::Small, 0.0, true).clicked();
            });
        },
    );
    if open {
        patches::open_files(app, &id);
    }
    if reload {
        patches::reload(app, &id);
    }
    used_in(app, ui, &src, &[], every);
    let used: Vec<String> = scenes_using(app, &src).into_iter().map(|(n, l, _)| quoted(if l.is_empty() { &n } else { &l })).collect();
    let note = "Removes it from this project. Its folder is set aside in a hidden \u{201c}.removed\u{201d} folder next to it, in case you want it back.";
    if remove_section(ui, &t, &id, &used, note, !app.m.connected) {
        if overlay {
            // its [overlays.<id>] settings would point at nothing (an engine error): gone first
            app.m.action("project.write", Value::map().with("path", "project.toml").with("set", Value::map().with(format!("overlays.{id}"), Value::Null)));
        }
        patches::remove(app, &id);
        st.selected = None;
        app.m.toast(format!("Removed {}.", quoted(&title)), false);
    }
}

// ---- new source ---------------------------------------------------------------------------------

fn new_source(app: &mut App, ui: &mut Ui, st: &mut State) {
    let t = app.t.clone();
    let mut cancel = false;
    widgets::detail_header(ui, &t, icon::PLUS, "New source", "Choose what kind, set it up, then Create.", |ui| {
        cancel = widgets::button_ex(ui, &t, None, "Cancel", Kind::Ghost, Size::Small, 0.0, true).clicked();
    });
    if cancel {
        st.creating = false;
        st.draft = Draft::default();
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(spacing::M);
        for k in SourceKind::NEW {
            let (title, body) = k.tile();
            if widgets::kind_tile(ui, &t, k.icon(), title, body, st.draft.kind == Some(k)).clicked() && st.draft.kind != Some(k) {
                st.draft.kind = Some(k);
                st.draft.file.clear();
                st.draft.template.clear();
                st.draft.template_kind.clear();
            }
        }
    });
    let Some(kind) = st.draft.kind else { return };
    ui.add_space(spacing::L);
    let busy = st.pending.is_some() || st.after_create.is_some();
    widgets::inspector_section(
        ui,
        &t,
        "sources-new-draft",
        kind.tile().0,
        true,
        |_| {},
        |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                widgets::prop_row(ui, &t, "Name", |ui| {
                    let hint = match kind {
                        SourceKind::Camera => "e.g. Face cam",
                        SourceKind::Video => "e.g. Starting soon loop",
                        SourceKind::Image => "e.g. Logo",
                        SourceKind::Web => "e.g. Chat",
                        _ => "e.g. Background",
                    };
                    ui.add(widgets::field(&mut st.draft.name).hint_text(hint).desired_width(ui.available_width().min(320.0)));
                });
                match kind {
                    SourceKind::Camera => draft_camera(app, ui, &mut st.draft),
                    SourceKind::Video | SourceKind::Image => draft_media(app, ui, &mut st.draft, kind),
                    _ => draft_patch(app, ui, &mut st.draft, kind),
                }
            });
        },
    );
    ui.add_space(spacing::M);
    if !st.error.is_empty() {
        if widgets::callout(ui, &t, Tone::Warn, icon::WARN, "It wasn't made", &st.error.clone(), Some("OK")) {
            st.error.clear();
        }
        ui.add_space(spacing::M);
    }
    let issue = st.draft.issue(app);
    let mut create = false;
    ui.horizontal(|ui| {
        let label = if busy { "Creating…" } else { "Create" };
        create = widgets::button_ex(ui, &t, Some(icon::CHECK), label, Kind::Primary, Size::Medium, 0.0, issue.is_none() && !busy && app.m.connected).clicked();
        if let Some(why) = issue {
            widgets::hint(ui, &t, why);
        }
    });
    if create {
        create_draft(app, st);
    }
}

fn create_draft(app: &mut App, st: &mut State) {
    let d = st.draft.clone();
    let (id, label) = (d.id(), d.name.trim().to_string());
    match d.kind {
        Some(SourceKind::Camera) => st.save(
            app,
            &id,
            Value::map()
                .with("create", true)
                .with("label", label)
                .with("device", d.device.trim())
                .with("format", d.format.as_str())
                .with("size", vec![d.width, d.height])
                .with("fps", d.fps),
        ),
        Some(SourceKind::Video | SourceKind::Image) => {
            st.save(app, &id, Value::map().with("create", true).with("label", label).with("file", d.file.as_str()).with("loop", d.looping).with("rate", 1.0))
        }
        Some(SourceKind::Web | SourceKind::Generative) => {
            patches::create(app, &id, &d.template_kind, &d.template);
            st.error.clear();
            st.after_create = Some(AfterCreate { id, label, every_scene: d.every_scene, since: Instant::now() });
        }
        _ => {}
    }
}

fn draft_camera(app: &mut App, ui: &mut Ui, d: &mut Draft) {
    let t = app.t.clone();
    let cams = cameras(app);
    if cams.is_empty() {
        if widgets::callout(
            ui,
            &t,
            Tone::Info,
            icon::CAMERA,
            "No cameras found",
            "Plug one in and it shows up here. Settings → Devices lists everything connected.",
            Some("Open Devices"),
        ) {
            app.open_view(ViewId::Devices);
        }
        ui.add_space(spacing::S);
    } else {
        let current = camera_for(&cams, &d.device);
        widgets::prop_row(ui, &t, "Camera", |ui| {
            let shown = current.as_ref().map(|c| text(c, "label").to_string()).unwrap_or_else(|| "Choose a camera…".into());
            egui::ComboBox::from_id_salt("sources-new-camera").selected_text(shown).width(ui.available_width().min(320.0)).show_ui(ui, |ui| {
                for c in &cams {
                    let used_by = text(c, "source");
                    let label = if used_by.is_empty() { text(c, "label").to_string() } else { format!("{} (used by {})", text(c, "label"), nice(used_by)) };
                    if ui.selectable_label(text(c, "identity") == d.device, label).on_hover_text(text(c, "identity")).clicked() {
                        d.device = text(c, "identity").to_string();
                        if d.name.trim().is_empty() {
                            d.name = text(c, "label").to_string();
                        }
                        (d.width, d.height, d.fps) = (0, 0, 0.0);
                        if let Some(m) = modes(c).first() {
                            d.choose_mode(m);
                        }
                    }
                }
            });
        });
        let supported = current.as_ref().map(modes).unwrap_or_default();
        if !supported.is_empty() {
            widgets::prop_row(ui, &t, "Mode", |ui| {
                let now = (d.format.clone(), d.width, d.height, d.fps);
                egui::ComboBox::from_id_salt("sources-new-mode").selected_text(mode_words(&now)).width(ui.available_width().min(320.0)).show_ui(ui, |ui| {
                    for m in &supported {
                        if ui.selectable_label(*m == now, mode_words(m)).clicked() {
                            d.choose_mode(m);
                        }
                    }
                });
            });
        }
    }
    widgets::details(ui, &t, "sources-new-manual", "Enter by hand", |ui| manual_rows(ui, &t, d, "new"));
}

fn draft_media(app: &mut App, ui: &mut Ui, d: &mut Draft, kind: SourceKind) {
    let t = app.t.clone();
    widgets::prop_row(ui, &t, "File", |ui| {
        let mk = if kind == SourceKind::Video { MediaKind::Video } else { MediaKind::Image };
        if media::picker(app, ui, ("sources-new-file", kind.label()), mk, &mut d.file) && d.name.trim().is_empty() {
            d.name = nice(std::path::Path::new(&d.file).file_stem().and_then(|s| s.to_str()).unwrap_or(""));
        }
    });
    if kind == SourceKind::Video || is_gif(&d.file) {
        widgets::prop_row(ui, &t, "Loop", |ui| {
            widgets::toggle(ui, &t, &mut d.looping);
            widgets::hint(ui, &t, if d.looping { "Starts over at the end." } else { "Plays once, then holds the last frame." });
        });
    }
}

/// Starting points for a web page or generative visual, as a list to pick from.
fn draft_patch(app: &mut App, ui: &mut Ui, d: &mut Draft, kind: SourceKind) {
    let t = app.t.clone();
    let groups: Vec<(&str, Vec<patches::Template>)> = if kind == SourceKind::Web {
        vec![("", patches::templates(app, "web"))]
    } else {
        vec![
            // shaders made for effects and transitions live on their own pages
            ("Shader", patches::templates(app, "shader").into_iter().filter(|t| t.layer == "source").collect()),
            ("Particles", patches::templates(app, "particles")),
            ("Script", patches::templates(app, "script")),
        ]
    };
    if groups.iter().all(|(_, g)| g.is_empty()) {
        widgets::hint(ui, &t, if app.m.connected { "Loading starting points…" } else { "Starting points show once Stream Engine is running." });
        return;
    }
    let chars = ((ui.available_width() - 60.0) / 7.0).max(20.0) as usize;
    let mut chosen: Option<patches::Template> = None;
    widgets::prop_row(ui, &t, "Start from", |ui| {
        ui.vertical(|ui| {
            for (group, items) in &groups {
                if items.is_empty() {
                    continue;
                }
                if !group.is_empty() {
                    widgets::group_label(ui, &t, group);
                }
                for tp in items {
                    let title = if tp.name == "default" { "Basic".to_string() } else { nice(&tp.name) };
                    let selected = d.template == tp.name && d.template_kind == tp.kind;
                    let trailing = if tp.layer == "overlay" { "every scene" } else { "" };
                    if widgets::list_row(ui, &t, "", &title, &clip(&tp.description, chars), trailing, selected).clicked() {
                        chosen = Some(tp.clone());
                    }
                }
            }
        });
    });
    if let Some(tp) = chosen {
        if d.name.trim().is_empty() || groups.iter().flat_map(|(_, g)| g).any(|g| nice(&g.name) == d.name || (g.name == "default" && d.name == "Basic")) {
            d.name = if tp.name == "default" { String::new() } else { nice(&tp.name) };
        }
        d.template = tp.name;
        d.template_kind = tp.kind;
        d.every_scene = tp.layer == "overlay";
    }
    let overlay = groups.iter().flat_map(|(_, g)| g).any(|g| g.name == d.template && g.kind == d.template_kind && g.layer == "overlay");
    if overlay {
        widgets::prop_row(ui, &t, "On every scene", |ui| {
            widgets::toggle(ui, &t, &mut d.every_scene);
            widgets::hint(ui, &t, if d.every_scene { "Drawn above all your scenes." } else { "Only in the scenes you add it to." });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_and_videos_are_told_apart_by_file() {
        assert_eq!(media_kind("assets/images/logo.PNG"), SourceKind::Image);
        assert_eq!(media_kind("assets/images/party.gif"), SourceKind::Image);
        assert_eq!(media_kind("assets/video/brb.mp4"), SourceKind::Video);
        // SVG is a picture, but not one a source can show
        assert_eq!(media_kind("assets/images/icon.svg"), SourceKind::Image);
        assert!(!is_picture_file("assets/images/icon.svg"));
        assert!(is_picture_file("assets/images/a.webp") && is_picture_file("assets/images/b.jpeg"));
    }

    #[test]
    fn scene_references_name_each_scene_once() {
        let row = Value::map().with(
            "references",
            vec![
                Value::Str("scenes/show.toml: canvas.wide.nodes[0].src".into()),
                Value::Str("scenes/show.toml: canvas.tall.nodes[0].src".into()),
                Value::Str("scenes/brb.toml: canvas.wide.nodes[2].src".into()),
            ],
        );
        assert_eq!(referenced_scenes(&row), vec!["show".to_string(), "brb".to_string()]);
    }

    #[test]
    fn only_visual_source_and_overlay_patches_are_sources() {
        let p = |kind: &str, layer: &str| Value::map().with("kind", kind).with("layer", layer);
        assert!(is_source_patch(&p("web", "overlay")));
        assert!(is_source_patch(&p("shader", "source")));
        assert!(is_source_patch(&p("script", "overlay")));
        assert!(!is_source_patch(&p("shader", "effect")), "effects live in Scenes → Effects");
        assert!(!is_source_patch(&p("shader", "transition")));
        assert!(!is_source_patch(&p("dsp", "audio-effect")));
        assert!(is_source_patch(&Value::map().with("id", "broken")), "a patch that can't load is still listed");
    }

    #[test]
    fn words_for_usage_and_lists() {
        assert_eq!(usage_words(0, false), "not in a scene");
        assert_eq!(usage_words(1, false), "in 1 scene");
        assert_eq!(usage_words(3, false), "in 3 scenes");
        assert_eq!(usage_words(0, true), "on every scene");
        assert_eq!(and_list(&["A".into(), "B".into(), "C".into()]), "A, B and C");
        assert_eq!(clip("Starting soon background loop", 12), "Starting so…");
    }
}
