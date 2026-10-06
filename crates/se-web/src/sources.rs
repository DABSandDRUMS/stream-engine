//! Which web sources should exist, at what URL, size, and frame rate — derived from the patch
//! set, the scene configuration, and `[web]` in `project.toml`.

use crate::protocol::{MAX_FPS, MAX_SIDE};
use se_api::auth::Scope;
use se_core::Config;
use se_patch::{Kind, Layer, PatchSet};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Video/audio slot of the YouTube player page (§13.3).
pub const YOUTUBE_SLOT: &str = "youtube";
/// Player page shipped in `<share>/web/`.
pub const PLAYER_PAGE: &str = "player.html";
pub const DEFAULT_SIZE: (u32, u32) = (1920, 1080);
pub const DEFAULT_PATCH_FPS: u32 = 60;
pub const DEFAULT_YOUTUBE_FPS: u32 = 30;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    Patch(String),
    Youtube,
}

impl Owner {
    /// Token scope id (`patch.<id>.*` writes) and token name.
    pub fn scope_id(&self) -> &str {
        match self {
            Owner::Patch(id) => id,
            Owner::Youtube => "youtube",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    pub slot: String,
    pub owner: Owner,
    /// Page URL without the token.
    pub url: String,
    /// Append the source's API token (pages served by the engine itself).
    pub with_token: bool,
    pub fps: u32,
    pub size: (u32, u32),
    /// Patch generation; a change reloads the page.
    pub generation: u64,
    /// Manifest `grants`: what the page's token may write besides its own namespace.
    pub grants: Vec<String>,
    /// `Some` for the queue browser, including an unconfigured (closed) account policy.
    pub youtube_account: Option<crate::protocol::YoutubeAccount>,
}

impl Spec {
    /// The page token's scope (§19).
    pub fn scope(&self) -> Scope {
        Scope::Patch(self.owner.scope_id().to_string(), self.grants.clone())
    }
}

/// `[web]` settings that need a host restart when they change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSettings {
    /// ANGLE on Vulkan (hardware WebGL/compositing); `false` = software rendering.
    pub gpu: bool,
    /// Chromium remote debugging on 127.0.0.1 (DevTools for off-screen pages).
    pub devtools_port: Option<u16>,
    /// Explicit Vulkan ICD manifest for the off-screen GPU host, never the sign-in window.
    pub vulkan_driver: Option<PathBuf>,
}

impl Default for HostSettings {
    fn default() -> Self {
        HostSettings { gpu: true, devtools_port: None, vulkan_driver: None }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct YoutubeSettings {
    enabled: Option<bool>,
    url: Option<String>,
    fps: Option<u32>,
    size: Option<(u32, u32)>,
}

/// Parse `[web]`; problems are reported. An invalid explicit driver blocks the off-screen
/// host rather than discarding the selection and reverting to the default GPU.
fn web_settings(section: Option<&toml::Value>, errors: &mut Vec<String>) -> (HostSettings, YoutubeSettings) {
    let mut host = HostSettings::default();
    let mut yt = YoutubeSettings::default();
    let Some(section) = section else { return (host, yt) };
    let Some(t) = section.as_table() else {
        errors.push("[web] must be a table".into());
        return (host, yt);
    };
    for (k, v) in t {
        match (k.as_str(), v) {
            ("gpu", toml::Value::Boolean(b)) => host.gpu = *b,
            ("devtools_port", toml::Value::Integer(p)) if (1..=65535).contains(p) => host.devtools_port = Some(*p as u16),
            ("vulkan_driver", toml::Value::String(s)) => {
                let path = PathBuf::from(s);
                if let Err(e) = crate::host::validate_vulkan_driver(&path) {
                    errors.push(e.to_string());
                }
                host.vulkan_driver = Some(path);
            }
            ("vulkan_driver", _) => errors.push("[web] vulkan_driver must be a nonempty absolute ICD manifest path string".into()),
            ("youtube", toml::Value::Table(y)) => {
                for (k, v) in y {
                    match (k.as_str(), v) {
                        ("enabled", toml::Value::Boolean(b)) => yt.enabled = Some(*b),
                        ("url", toml::Value::String(s)) if !s.trim().is_empty() => yt.url = Some(s.trim().to_string()),
                        ("fps", toml::Value::Integer(f)) if (1..=MAX_FPS as i64).contains(f) => yt.fps = Some(*f as u32),
                        ("size", toml::Value::Array(a)) if a.len() == 2 => match (a[0].as_integer(), a[1].as_integer()) {
                            (Some(w), Some(h)) if (1..=MAX_SIDE as i64).contains(&w) && (1..=MAX_SIDE as i64).contains(&h) => {
                                yt.size = Some((even(w as f32), even(h as f32)))
                            }
                            _ => errors.push(format!("[web.youtube] size must be [width, height] with sides 1–{MAX_SIDE}")),
                        },
                        (k, _) => {
                            errors.push(format!("[web.youtube] {k}: unknown key or bad value (url = string, fps = 1–{MAX_FPS}, size = [w, h], enabled = bool)"))
                        }
                    }
                }
            }
            (k, _) => errors.push(format!("[web] {k}: unknown key or bad value (gpu = bool, devtools_port = 1–65535, vulkan_driver = absolute ICD manifest path, [web.youtube])")),
        }
    }
    (host, yt)
}

/// Round to an even pixel count within 2..=MAX_SIDE (video encoders and chroma subsampling
/// prefer even sizes).
pub fn even(v: f32) -> u32 {
    let v = if v.is_finite() { v.round().max(2.0) as u32 } else { 2 };
    (v.min(MAX_SIDE) + 1) & !1
}

fn canvas_size(config: &Config, name: &str) -> (u32, u32) {
    config.project.canvas.get(name).map_or(DEFAULT_SIZE, |c| (c.width, c.height))
}

/// Largest node (by area) showing `slot` in any scene, in canvas pixels.
pub fn node_size(config: &Config, slot: &str) -> Option<(u32, u32)> {
    let mut best: Option<(f32, f32)> = None;
    for scene in config.scenes.values() {
        for (canvas, sc) in &scene.canvas {
            let (cw, ch) = canvas_size(config, canvas);
            for n in sc.nodes.iter().filter(|n| n.src == slot) {
                let (w, h) = (n.rect[2].abs() * cw as f32, n.rect[3].abs() * ch as f32);
                if best.is_none_or(|(bw, bh)| w * h > bw * bh) {
                    best = Some((w, h));
                }
            }
        }
    }
    best.map(|(w, h)| (even(w), even(h)))
}

/// Largest project canvas (overlay layers cover a whole canvas).
pub fn largest_canvas(config: &Config) -> (u32, u32) {
    config
        .project
        .canvas
        .values()
        .map(|c| (c.width, c.height))
        .max_by_key(|(w, h)| *w as u64 * *h as u64)
        .map_or(DEFAULT_SIZE, |(w, h)| (even(w as f32), even(h as f32)))
}

/// Percent-encode a URL path (patch entry files may contain spaces etc.).
fn encode_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/!$&'()*+,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Origin pages are loaded from. Loopback and unspecified binds use `localhost`, not an IP
/// literal: YouTube's IFrame player refuses every video (error 150, "embedding disabled") when
/// the embedding page's origin is `http://127.0.0.1:…`. Chromium resolves `localhost` to
/// loopback itself (falling back to 127.0.0.1 when ::1 is refused), and cookies are keyed to
/// this one origin.
pub fn page_origin(http: std::net::SocketAddr) -> String {
    let ip = http.ip();
    let host = if ip.is_loopback() || ip.is_unspecified() {
        "localhost".to_string()
    } else {
        match ip {
            std::net::IpAddr::V6(v) => format!("[{v}]"),
            std::net::IpAddr::V4(v) => v.to_string(),
        }
    };
    format!("http://{host}:{}", http.port())
}

/// Every source that should be open. `base` is the engine's HTTP origin
/// ([`page_origin`], e.g. `http://localhost:<port>`); `errors` collects `[web]` configuration problems.
pub fn desired(config: &Config, patches: &PatchSet, base: &str, share_dir: &Path) -> (BTreeMap<String, Spec>, HostSettings, Vec<String>) {
    let mut errors = Vec::new();
    let (host, yt) = web_settings(config.project.extra.get("web"), &mut errors);
    let mut out = BTreeMap::new();
    for (id, p) in patches.iter() {
        let m = &p.manifest;
        if m.kind != Kind::Web || !p.enabled {
            continue;
        }
        let slot = format!("patch.{id}");
        let size = match m.size {
            Some([w, h]) => (even(w as f32), even(h as f32)),
            None if m.layer == Layer::Overlay => largest_canvas(config),
            None => node_size(config, &slot).unwrap_or(DEFAULT_SIZE),
        };
        let spec = Spec {
            url: format!("{base}/patches/{id}/{}", encode_path(&m.entry)),
            with_token: true,
            owner: Owner::Patch(id.clone()),
            fps: m.fps.unwrap_or(DEFAULT_PATCH_FPS).clamp(1, MAX_FPS),
            size,
            generation: p.generation,
            grants: m.grants.clone(),
            slot: slot.clone(),
            youtube_account: None,
        };
        out.insert(slot, spec);
    }
    let page_exists = share_dir.join("web").join(PLAYER_PAGE).is_file();
    if yt.enabled.unwrap_or(true) && (page_exists || yt.url.is_some()) {
        let (url, with_token) = match &yt.url {
            Some(u) if u.starts_with('/') => (format!("{base}{u}"), true),
            Some(u) if u == base || u.starts_with(&format!("{base}/")) => (u.clone(), true),
            Some(u) => (u.clone(), false),
            None => (format!("{base}/web/{PLAYER_PAGE}"), true),
        };
        let spec = Spec {
            slot: YOUTUBE_SLOT.into(),
            owner: Owner::Youtube,
            url,
            with_token,
            fps: yt.fps.unwrap_or(DEFAULT_YOUTUBE_FPS),
            size: yt.size.or_else(|| node_size(config, YOUTUBE_SLOT)).unwrap_or(DEFAULT_SIZE),
            generation: 0,
            grants: Vec::new(),
            youtube_account: Some(crate::protocol::YoutubeAccount {
                channel: config.project.extra.get("songs").and_then(|s| s.get("youtube_channel")).and_then(toml::Value::as_str).unwrap_or("").into(),
                delegate: config.project.extra.get("songs").and_then(|s| s.get("youtube_delegate")).and_then(toml::Value::as_str).unwrap_or("").into(),
            }),
        };
        out.insert(YOUTUBE_SLOT.into(), spec);
    }
    (out, host, errors)
}

/// `url` with `token=<token>` added to its query (before any fragment).
pub fn with_token(url: &str, token: &str) -> String {
    let (head, frag) = match url.split_once('#') {
        Some((h, f)) => (h, Some(f)),
        None => (url, None),
    };
    let sep = if head.contains('?') { '&' } else { '?' };
    let mut s = format!("{head}{sep}token={token}");
    if let Some(f) = frag {
        s.push('#');
        s.push_str(f);
    }
    s
}

/// Hide API tokens in URLs shown in status, logs, and queries.
pub fn redact(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let mut rest = url;
    while let Some(i) = rest.find("token=") {
        let preceded = i == 0 || matches!(rest.as_bytes()[i - 1], b'?' | b'&');
        out.push_str(&rest[..i + 6]);
        rest = &rest[i + 6..];
        if preceded {
            let end = rest.find(['&', '#']).unwrap_or(rest.len());
            if end > 0 {
                out.push('…');
            }
            rest = &rest[end..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_core::SourceFile;
    use se_patch::{Manifest, PatchInfo};
    use std::sync::Arc;

    fn config(project: &str, scenes: &[(&str, &str)]) -> Config {
        let mut files = vec![SourceFile { kind: "project".into(), name: "project".into(), path: "project.toml".into(), table: project.parse().unwrap() }];
        for (name, src) in scenes {
            files.push(SourceFile { kind: "scenes".into(), name: (*name).into(), path: format!("scenes/{name}.toml"), table: src.parse().unwrap() });
        }
        let c = Config::build(&files);
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        c
    }

    fn patch(id: &str, toml: &str, enabled: bool, generation: u64) -> (String, PatchInfo) {
        let m = Manifest::parse(&Path::new("/p/patches").join(id), toml).unwrap();
        (id.into(), PatchInfo { manifest: Arc::new(m), enabled, generation })
    }

    const PROJECT: &str = "schema = 1\n[canvas.wide]\nwidth = 1920\nheight = 1080\n[canvas.tall]\nwidth = 1080\nheight = 1920\n";

    #[test]
    fn size_comes_from_the_largest_node_across_scenes_and_canvases() {
        let c = config(
            PROJECT,
            &[
                ("duo", "[canvas.wide]\nnodes = [{ src = \"patch.clock\", rect = [0.0, 0.0, 0.25, 0.25] }, { src = \"cam\", rect = [0, 0, 1, 1] }]"),
                ("tall", "[canvas.tall]\nnodes = [{ src = \"patch.clock\", rect = [0.1, 0.1, 0.5, 0.3] }]"),
            ],
        );
        // wide: 480×270 = 129 600 px²; tall: 540×576 = 311 040 px² → tall wins
        assert_eq!(node_size(&c, "patch.clock"), Some((540, 576)));
        assert_eq!(node_size(&c, "patch.none"), None);
        assert_eq!(largest_canvas(&c), (1920, 1080));
    }

    #[test]
    fn odd_and_out_of_range_sizes_are_rounded_to_even() {
        assert_eq!(even(481.4), 482);
        assert_eq!(even(480.6), 482);
        assert_eq!(even(0.2), 2);
        assert_eq!(even(1e9), MAX_SIDE);
        assert_eq!(even(f32::NAN), 2);
    }

    #[test]
    fn web_patches_become_sources_with_manifest_overrides() {
        let c = config(PROJECT, &[("duo", "[canvas.wide]\nnodes = [{ src = \"patch.chat\", rect = [0.0, 0.0, 0.3, 0.5] }]")]);
        let set: PatchSet = [
            patch("chat", "kind = \"web\"\nfps = 30", true, 4),
            patch("alerts", "kind = \"web\"\nlayer = \"overlay\"", true, 1),
            patch("sized", "kind = \"web\"\nentry = \"my page.html\"\nsize = [641, 361]\nfps = 120", true, 1),
            patch("off", "kind = \"web\"", false, 1),
            patch("glow", "kind = \"shader\"", true, 1),
        ]
        .into_iter()
        .collect();
        let dir = tempfile::tempdir().unwrap();
        let (d, host, errs) = desired(&c, &set, "http://127.0.0.1:7870", dir.path());
        assert!(errs.is_empty());
        assert_eq!(host, HostSettings::default());
        assert_eq!(d.keys().collect::<Vec<_>>(), ["patch.alerts", "patch.chat", "patch.sized"], "disabled and non-web patches are skipped; no player page");
        let chat = &d["patch.chat"];
        assert_eq!((chat.size, chat.fps, chat.generation), ((576, 540), 30, 4));
        assert_eq!(chat.url, "http://127.0.0.1:7870/patches/chat/index.html");
        assert_eq!(chat.owner, Owner::Patch("chat".into()));
        assert_eq!(d["patch.alerts"].size, (1920, 1080), "overlay → largest canvas");
        assert_eq!(d["patch.alerts"].fps, DEFAULT_PATCH_FPS);
        let sized = &d["patch.sized"];
        assert_eq!((sized.size, sized.fps), ((642, 362), MAX_FPS), "manifest size wins, fps capped at CEF's 60");
        assert_eq!(sized.url, "http://127.0.0.1:7870/patches/sized/my%20page.html");
    }

    #[test]
    fn youtube_needs_the_page_or_an_override() {
        let base = "http://127.0.0.1:7870";
        let dir = tempfile::tempdir().unwrap();
        let scenes = [("duo", "[canvas.wide]\nnodes = [{ src = \"youtube\", rect = [0.70, 0.70, 0.28, 0.25] }]")];
        let (d, _, _) = desired(&config(PROJECT, &scenes), &PatchSet::new(), base, dir.path());
        assert!(d.is_empty(), "no player page installed");

        std::fs::create_dir_all(dir.path().join("web")).unwrap();
        std::fs::write(dir.path().join("web/player.html"), "<html>").unwrap();
        let (d, _, _) = desired(&config(PROJECT, &scenes), &PatchSet::new(), base, dir.path());
        let yt = &d[YOUTUBE_SLOT];
        assert_eq!((yt.url.as_str(), yt.with_token, yt.fps, yt.size), ("http://127.0.0.1:7870/web/player.html", true, 30, (538, 270)));
        assert_eq!(yt.owner.scope_id(), "youtube");

        let over = format!("{PROJECT}[web]\ngpu = false\ndevtools_port = 9333\n[web.youtube]\nurl = \"/web/player2.html\"\nfps = 60\nsize = [1280, 720]\n");
        let (d, host, errs) = desired(&config(&over, &scenes), &PatchSet::new(), base, dir.path());
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(host, HostSettings { gpu: false, devtools_port: Some(9333), ..HostSettings::default() });
        let yt = &d[YOUTUBE_SLOT];
        assert_eq!((yt.url.as_str(), yt.with_token, yt.fps, yt.size), ("http://127.0.0.1:7870/web/player2.html", true, 60, (1280, 720)));

        let ext = format!("{PROJECT}[web.youtube]\nurl = \"https://example.com/player\"\n");
        let (d, _, _) = desired(&config(&ext, &scenes), &PatchSet::new(), base, dir.path());
        assert!(!d[YOUTUBE_SLOT].with_token, "the engine token never goes to another origin");

        let off = format!("{PROJECT}[web.youtube]\nenabled = false\nfps = 500\n");
        let (d, _, errs) = desired(&config(&off, &scenes), &PatchSet::new(), base, dir.path());
        assert!(d.is_empty());
        assert_eq!(errs.len(), 1, "bad fps reported: {errs:?}");
    }

    #[test]
    fn explicit_vulkan_driver_is_validated_without_discarding_the_selection() {
        let dir = tempfile::tempdir().unwrap();
        let driver = dir.path().join("driver with spaces.json");
        std::fs::write(&driver, r#"{"file_format_version":"1.0.0","ICD":{"library_path":"libvulkan_radeon.so","api_version":"1.3.0"}}"#).unwrap();
        let settings = |value: toml::Value| {
            let mut table = toml::map::Map::new();
            table.insert("vulkan_driver".into(), value);
            let mut errors = Vec::new();
            let (host, _) = web_settings(Some(&toml::Value::Table(table)), &mut errors);
            (host, errors)
        };
        let (selected, errors) = settings(toml::Value::String(driver.display().to_string()));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(selected.vulkan_driver.as_deref(), Some(driver.as_path()));
        assert_ne!(selected, HostSettings::default(), "selection changes must trigger the existing host restart");

        for path in [
            PathBuf::new(),
            PathBuf::from("   "),
            PathBuf::from("relative.json"),
            dir.path().to_path_buf(),
            dir.path().join("missing.json"),
            dir.path().join("first.json:second.json"),
        ] {
            let (host, errors) = settings(toml::Value::String(path.display().to_string()));
            assert_eq!(host.vulkan_driver.as_deref(), Some(path.as_path()), "invalid selection must not revert to the default GPU");
            assert_eq!(errors.len(), 1, "{path:?}: {errors:?}");
            assert!(errors[0].starts_with("[web] vulkan_driver"), "{errors:?}");
        }
        for value in [toml::Value::Boolean(false), toml::Value::Integer(0), toml::Value::Array(Vec::new())] {
            let (_, errors) = settings(value);
            assert_eq!(errors.len(), 1);
            assert!(errors[0].starts_with("[web] vulkan_driver"), "{errors:?}");
        }
        for manifest in [
            "",
            "not JSON",
            "{}",
            r#"{"file_format_version":"1.0.0","ICD":{"library_path":"","api_version":"1.3.0"}}"#,
        ] {
            std::fs::write(&driver, manifest).unwrap();
            let (host, errors) = settings(toml::Value::String(driver.display().to_string()));
            assert_eq!(host, selected);
            assert_eq!(errors.len(), 1, "an existing non-manifest file must not be accepted: {manifest:?}");
            assert!(errors[0].starts_with("[web] vulkan_driver"), "{errors:?}");
        }
        std::fs::remove_file(&driver).unwrap();
        let (missing, errors) = settings(toml::Value::String(driver.display().to_string()));
        assert_eq!(missing, selected, "driver disappearance must not clear the selected GPU");
        assert_eq!(errors.len(), 1, "a disappeared manifest must become a visible config failure");
    }

    #[test]
    fn loopback_pages_load_from_localhost() {
        assert_eq!(page_origin("127.0.0.1:7870".parse().unwrap()), "http://localhost:7870");
        assert_eq!(page_origin("0.0.0.0:7870".parse().unwrap()), "http://localhost:7870");
        assert_eq!(page_origin("[::1]:7870".parse().unwrap()), "http://localhost:7870");
        assert_eq!(page_origin("192.168.1.5:7870".parse().unwrap()), "http://192.168.1.5:7870");
        assert_eq!(page_origin("[fd00::5]:7870".parse().unwrap()), "http://[fd00::5]:7870");
    }

    #[test]
    fn tokens_are_added_and_redacted() {
        assert_eq!(with_token("http://h/p/index.html", "ab"), "http://h/p/index.html?token=ab");
        assert_eq!(with_token("http://h/p?x=1#top", "ab"), "http://h/p?x=1&token=ab#top");
        assert_eq!(redact("http://h/p?x=1&token=abcdef#top"), "http://h/p?x=1&token=…#top");
        assert_eq!(redact("http://h/p?token=abc&y=2"), "http://h/p?token=…&y=2");
        assert_eq!(redact("http://h/mytoken=abc"), "http://h/mytoken=abc", "only query parameters named token");
    }
}
