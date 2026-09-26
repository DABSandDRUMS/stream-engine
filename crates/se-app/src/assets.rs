//! Media files (`assets/`) for the UI's Scenes → Media tab: the `project.import`,
//! `project.asset.rename` and `project.asset.delete` actions and the `project.assets` query.
//!
//! Imports copy a local file to `assets/<kind>/<slug>.<ext>` (never overwriting: `-2`, `-3`…).
//! "Used by" is found by scanning the project's text files for the file's path (and, for sounds
//! the sampler picks up by file name, for `sound = "<name>"` / `audio.play <name>`); a rename
//! rewrites exactly those references.

use crate::daemon::Ctx;
use anyhow::{Context, Result, anyhow, bail};
use se_proto::{Event, Origin, Value};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Kinds (= folders under `assets/`) and the file types each accepts.
pub const KINDS: &[(&str, &[&str])] = &[
    ("images", &["png", "jpg", "jpeg", "webp", "gif", "svg"]),
    ("video", &["mp4", "mov", "webm", "mkv"]),
    ("sounds", &["wav", "mp3", "ogg", "flac", "opus"]),
    ("luts", &["cube"]),
    ("fonts", &["ttf", "otf"]),
];

/// Largest file `project.import` copies.
pub const MAX_BYTES: u64 = 2_000_000_000;

/// Project text files scanned for references (bigger files are skipped).
const TEXT_EXTS: &[&str] = &["toml", "html", "htm", "js", "mjs", "css", "json", "wgsl", "rhai", "lua", "yaml", "yml"];
const MAX_TEXT_BYTES: u64 = 2 << 20;
/// Top-level folders that never hold references.
const SKIP_DIRS: &[&str] = &["assets", "sessions", "target", "node_modules"];

/// Kind of a file from its extension (case-insensitive).
pub fn kind_of(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    KINDS.iter().find(|(_, exts)| exts.contains(&ext.as_str())).map(|(k, _)| *k)
}

fn one(kind: &str) -> &'static str {
    match kind {
        "images" => "a picture",
        "video" => "a video",
        "sounds" => "a sound",
        "luts" => "a color look",
        "fonts" => "a font",
        _ => "a file",
    }
}

/// Validate a project-relative media path (`assets/…`, a known file type); returns its kind.
pub fn check_asset_path(rel: &str) -> Result<&'static str> {
    crate::project_io::check_relative(rel)?;
    if !rel.starts_with("assets/") {
        bail!("`{rel}` is not in your media");
    }
    kind_of(Path::new(rel)).ok_or_else(|| anyhow!("`{rel}` is not a media file"))
}

/// A file-name-safe name (`My Horn!` → `my_horn`), like scene ids.
pub fn slug(name: &str) -> String {
    let mut s: String = name.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    let s = s.trim_matches('_').to_string();
    if s.is_empty() { "media".into() } else { s }
}

/// What an import did.
#[derive(Debug, PartialEq)]
pub struct Imported {
    pub path: String,
    pub kind: &'static str,
    pub name: String,
}

/// Copy `src` (an absolute local path) into `root/assets/<kind>/`; errors are plain sentences.
pub fn import(root: &Path, src: &Path, want: Option<&str>, name: Option<&str>) -> Result<Imported> {
    if !src.is_absolute() {
        bail!("Stream Engine needs the full location of the file.");
    }
    let meta = match std::fs::metadata(src) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => bail!("That file isn't there any more. Pick it again."),
        Err(e) => bail!("Stream Engine can't read that file ({e})."),
    };
    if meta.is_dir() {
        bail!("That's a folder. Open it and add the files inside instead.");
    }
    let Some(kind) = kind_of(src) else {
        let ext = src.extension().and_then(|e| e.to_str()).map(|e| format!(".{} files", e.to_lowercase())).unwrap_or_else(|| "files without a type".into());
        bail!(
            "Stream Engine can't use {ext}. Add a picture (PNG, JPG, WebP, GIF, SVG), a video (MP4, MOV, WebM, MKV), a sound (WAV, MP3, OGG, FLAC, Opus), a color look (.cube) or a font (TTF, OTF)."
        );
    };
    if let Some(w) = want.filter(|w| !w.is_empty())
        && w != kind
    {
        if !KINDS.iter().any(|(k, _)| *k == w) {
            bail!("`{w}` is not a kind of media");
        }
        bail!("That's {}, but this needs {}.", one(kind), one(w));
    }
    if meta.len() > MAX_BYTES {
        bail!("This file is bigger than 2 GB. Make it smaller (shorter, or lower quality) and add it again.");
    }
    let assets = root.join("assets");
    if let (Ok(a), Ok(s)) = (assets.canonicalize(), src.canonicalize())
        && s.starts_with(&a)
    {
        bail!("That file is already in your media.");
    }
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let base = slug(
        name.filter(|n| !n.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| src.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default())
            .as_str(),
    );
    let dir = assets.join(kind);
    std::fs::create_dir_all(&dir).with_context(|| format!("Stream Engine couldn't make the {kind} folder"))?;
    // copy under a hidden name (the watcher ignores it), then give it the first free name:
    // hard_link never replaces an existing file, so two imports can't pick the same one
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let part = dir.join(format!(".{base}.{}-{nanos}.part", std::process::id()));
    if let Err(e) = std::fs::copy(src, &part) {
        let _ = std::fs::remove_file(&part);
        bail!("Stream Engine couldn't copy the file ({e}). Check there's enough free space and try again.");
    }
    let placed = (1..10_000).find_map(|n| {
        let stem = if n == 1 { base.clone() } else { format!("{base}-{n}") };
        match std::fs::hard_link(&part, dir.join(format!("{stem}.{ext}"))) {
            Ok(()) => Some(Ok(stem)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => None,
            Err(e) => Some(Err(e)),
        }
    });
    let _ = std::fs::remove_file(&part);
    let stem = match placed {
        Some(Ok(stem)) => stem,
        Some(Err(e)) => bail!("Stream Engine couldn't save the file ({e})."),
        None => bail!("Stream Engine couldn't find a free name for this file. Give it another name."),
    };
    Ok(Imported { path: format!("assets/{kind}/{stem}.{ext}"), kind, name: stem })
}

/// Project text files that may mention media: (relative path, text).
pub fn text_files(root: &Path) -> Vec<(String, String)> {
    fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(String, String)>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() {
                if depth == 0 && SKIP_DIRS.contains(&name.as_str()) || depth > 6 || name == "node_modules" {
                    continue;
                }
                walk(root, &p, depth + 1, out);
            } else if ft.is_file()
                && p.extension().and_then(|x| x.to_str()).is_some_and(|x| TEXT_EXTS.contains(&x.to_ascii_lowercase().as_str()))
                && e.metadata().is_ok_and(|m| m.len() <= MAX_TEXT_BYTES)
                && let Ok(text) = std::fs::read_to_string(&p)
                && let Ok(rel) = p.strip_prefix(root)
            {
                out.push((rel.to_string_lossy().to_string(), text));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, 0, &mut out);
    out
}

fn word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// The name the sampler plays a sound by (`audio.play horn`, `sound = "horn"`): files directly
/// in `assets/sounds/` of a type it loads are known by their file name.
fn sound_name(rel: &str) -> Option<String> {
    let rest = rel.strip_prefix("assets/sounds/")?;
    if rest.contains('/') {
        return None;
    }
    let p = Path::new(rest);
    let ext = p.extension()?.to_str()?.to_ascii_lowercase();
    if !se_audio::sounds::AUDIO_EXTS.contains(&ext.as_str()) {
        return None;
    }
    let stem = p.file_stem()?.to_str()?;
    (!stem.is_empty() && !stem.contains('.') && stem.chars().all(word)).then(|| stem.to_string())
}

/// A `[sounds.<name>]` entry in the audio settings defines the name itself (and points at its
/// file by path), so renaming the file must not rename the sound.
fn claimed(name: &str, texts: &[(String, String)]) -> bool {
    texts.iter().any(|(f, t)| f.ends_with(".toml") && (t.contains(&format!("[sounds.{name}]")) || t.contains(&format!("[audio.sounds.{name}]"))))
}

/// Byte ranges in `text` (a project file at `file`) that refer to the media file `rel`: its
/// path after `assets/` (as `assets/…` anywhere; bare, e.g. alert images, inside TOML strings)
/// and, for a sound the sampler knows by name, that name after `sound =` / `audio.play`.
fn refs(file: &str, text: &str, rel: &str, sound: Option<&str>) -> Vec<(std::ops::Range<usize>, Part)> {
    let mut out = Vec::new();
    let toml = file.ends_with(".toml");
    let Some(inner) = rel.strip_prefix("assets/") else { return out };
    for (i, _) in text.match_indices(inner) {
        let end = i + inner.len();
        if text[end..].chars().next().is_some_and(|c| word(c) || c == '.') {
            continue;
        }
        let before = &text[..i];
        let full = before.strip_suffix("assets/").is_some_and(|b| !b.chars().next_back().is_some_and(word));
        let bare = toml && before.ends_with(['"', '\'']);
        if full || bare {
            out.push((i..end, Part::Path));
        }
    }
    if let Some(name) = sound.filter(|_| toml) {
        for (i, _) in text.match_indices(name) {
            let end = i + name.len();
            let prev = text[..i].chars().next_back();
            let next = text[end..].chars().next();
            if prev.is_some_and(word) || next.is_some_and(|c| word(c) || c == '.') {
                continue;
            }
            let line = &text[text[..i].rfind('\n').map_or(0, |n| n + 1)..i];
            let unquoted = line.strip_suffix(['"', '\'']).unwrap_or(line);
            let quoted = unquoted.len() != line.len() && next.is_some_and(|c| c == '"' || c == '\'');
            let play = unquoted.trim_end().ends_with("audio.play") && unquoted.len() > unquoted.trim_end().len();
            let key = quoted && {
                let k = unquoted.trim_end();
                k.strip_suffix('=').or_else(|| k.strip_suffix(':')).is_some_and(|k| {
                    let k = k.trim_end().trim_end_matches(['"', '\'']);
                    k.rsplit(|c: char| !word(c)).next().is_some_and(|k| k == "sound" || k.ends_with("_sound"))
                })
            };
            if play || key {
                out.push((i..end, Part::Name));
            }
        }
    }
    out.sort_by_key(|(r, _)| r.start);
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Part {
    Path,
    Name,
}

/// Project files that refer to `rel`.
pub fn used_by(rel: &str, texts: &[(String, String)]) -> Vec<String> {
    let sound = sound_name(rel);
    texts.iter().filter(|(f, t)| !refs(f, t, rel, sound.as_deref()).is_empty()).map(|(f, _)| f.clone()).collect()
}

/// The library for `project.assets`.
pub fn list(root: &Path) -> Vec<Value> {
    let texts = text_files(root);
    let mut files = Vec::new();
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() && depth < 6 {
                walk(&e.path(), depth + 1, out);
            } else if ft.is_file() && kind_of(&e.path()).is_some() {
                out.push(e.path());
            }
        }
    }
    walk(&root.join("assets"), 0, &mut files);
    files.sort();
    files
        .into_iter()
        .filter_map(|p| {
            let rel = p.strip_prefix(root).ok()?.to_string_lossy().to_string();
            let kind = kind_of(&p)?;
            let meta = std::fs::metadata(&p).ok()?;
            let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64);
            let name = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let sound = sound_name(&rel);
            let used = texts.iter().filter(|(f, t)| !refs(f, t, &rel, sound.as_deref()).is_empty()).map(|(f, _)| f.clone()).collect::<Vec<_>>();
            let mut v = Value::map()
                .with("path", rel)
                .with("kind", kind)
                .with("name", name)
                .with("bytes", meta.len())
                .with("used_by", used)
                .with("abs", p.to_string_lossy().to_string())
                .with("modified_ms", modified);
            if let Some(s) = sound {
                v = v.with("sound", s);
            }
            Some(v)
        })
        .collect()
}

/// The new text of every project file that refers to `rel`, pointing at `to` instead (sound
/// names follow the file only when no `[sounds.<name>]` entry defines the name).
pub fn rewrite(texts: &[(String, String)], rel: &str, to: &str) -> Vec<(String, String)> {
    let sound = sound_name(rel).filter(|s| !claimed(s, texts));
    let (new_inner, new_name) =
        (to.strip_prefix("assets/").unwrap_or(to), Path::new(to).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
    texts
        .iter()
        .filter_map(|(f, t)| {
            let found = refs(f, t, rel, sound.as_deref());
            if found.is_empty() {
                return None;
            }
            let mut out = String::with_capacity(t.len() + 32);
            let mut at = 0;
            for (r, part) in found {
                if r.start < at {
                    continue;
                }
                out.push_str(&t[at..r.start]);
                out.push_str(if part == Part::Path { new_inner } else { &new_name });
                at = r.end;
            }
            out.push_str(&t[at..]);
            Some((f.clone(), out))
        })
        .collect()
}

/// Where a rename moves `rel` (same folder and type, the slug of `name`).
pub fn renamed_path(rel: &str, name: &str) -> String {
    let p = Path::new(rel);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    let dir = p.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
    format!("{dir}/{}.{ext}", slug(name))
}

fn arg<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get_path(k).and_then(Value::as_str)
}

/// Rename a media file and update everything that refers to it; returns (new path, rewritten
/// project files).
pub fn rename(ctx: &Ctx, args: &Value) -> Result<(String, Vec<String>)> {
    let rel = arg(args, "path").ok_or_else(|| anyhow!("project.asset.rename needs `path`"))?;
    let name = arg(args, "name").filter(|n| !n.trim().is_empty()).ok_or_else(|| anyhow!("Type a name first."))?;
    let kind = check_asset_path(rel)?;
    let root = ctx.project.root();
    if !root.join(rel).is_file() {
        bail!("That file isn't in your media any more.");
    }
    let to = renamed_path(rel, name);
    if to == rel {
        return Ok((to, Vec::new()));
    }
    if root.join(&to).exists() {
        bail!("You already have {} called “{name}”. Pick another name.", one(kind));
    }
    let texts = text_files(root);
    let changes = rewrite(&texts, rel, &to);
    std::fs::rename(root.join(rel), root.join(&to)).with_context(|| format!("Stream Engine couldn't rename {rel}"))?;
    let mut files = Vec::new();
    for (f, text) in changes {
        match ctx.project.write_file(&f, &text) {
            Ok(()) => files.push(f),
            Err(e) => ctx.hub.log("error", "project", format!("rename {rel}: could not update {f}: {e:#}")),
        }
    }
    Ok((to, files))
}

/// Delete a media file; refuses while something refers to it unless `force`.
pub fn delete(ctx: &Ctx, args: &Value) -> Result<String, (String, Vec<String>)> {
    let rel = arg(args, "path").ok_or_else(|| ("project.asset.delete needs `path`".to_string(), Vec::new()))?;
    check_asset_path(rel).map_err(|e| (e.to_string(), Vec::new()))?;
    let force = args.get_path("force").is_some_and(Value::truthy);
    let root = ctx.project.root();
    let users = used_by(rel, &text_files(root));
    if !users.is_empty() && !force {
        return Err((format!("{rel} is still used by {}", users.join(", ")), users));
    }
    match std::fs::remove_file(root.join(rel)) {
        Ok(()) => Ok(rel.to_string()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(rel.to_string()),
        Err(e) => Err((format!("Stream Engine couldn't delete it ({e})."), Vec::new())),
    }
}

fn emit(ctx: &Ctx, ty: &str, payload: Value) {
    ctx.hub.emit(Event::new(ty, Origin::System, payload));
}

/// The sampler picks sounds up from `assets/sounds/` only when its graph is rebuilt.
fn sounds_changed(ctx: &Ctx, paths: &[&str]) {
    if paths.iter().any(|p| p.starts_with("assets/sounds/")) {
        ctx.hub.op(Origin::System, se_proto::Op::Action { name: "audio.reload".into(), args: Value::Null });
    }
}

/// Handle `project.import` / `project.asset.rename` / `project.asset.delete`; returns project
/// files rewritten (for a reload).
pub fn action(ctx: &Ctx, name: &str, args: &Value) -> Vec<String> {
    match name {
        "project.import" => {
            let src = arg(args, "src").unwrap_or("").to_string();
            let (kind, want) = (arg(args, "kind").map(String::from), arg(args, "name").map(String::from));
            let (ctx, root) = (ctx.clone(), ctx.project.root().to_path_buf());
            // big videos take a while: copy off the action loop
            tokio::task::spawn_blocking(move || match import(&root, Path::new(&src), kind.as_deref(), want.as_deref()) {
                Ok(i) => {
                    ctx.hub.log("info", "project", format!("imported {src} → {}", i.path));
                    sounds_changed(&ctx, &[&i.path]);
                    emit(&ctx, "project.imported", Value::map().with("src", src).with("path", i.path).with("kind", i.kind).with("name", i.name));
                }
                Err(e) => {
                    ctx.hub.log("warn", "project", format!("project.import {src}: {e:#}"));
                    emit(&ctx, "project.import.failed", Value::map().with("src", src).with("error", format!("{e:#}")));
                }
            });
            Vec::new()
        }
        "project.asset.rename" => {
            let path = arg(args, "path").unwrap_or("").to_string();
            match rename(ctx, args) {
                Ok((to, files)) => {
                    if to != path {
                        sounds_changed(ctx, &[&path, &to]);
                        emit(ctx, "project.asset.renamed", Value::map().with("path", path).with("to", to).with("updated", files.clone()));
                    }
                    files
                }
                Err(e) => {
                    ctx.hub.log("warn", "project", format!("project.asset.rename {path}: {e:#}"));
                    emit(ctx, "project.asset.failed", Value::map().with("path", path).with("error", format!("{e:#}")));
                    Vec::new()
                }
            }
        }
        "project.asset.delete" => {
            let path = arg(args, "path").unwrap_or("").to_string();
            match delete(ctx, args) {
                Ok(p) => {
                    sounds_changed(ctx, &[&p]);
                    emit(ctx, "project.asset.deleted", Value::map().with("path", p));
                }
                Err((e, users)) => {
                    ctx.hub.log("warn", "project", format!("project.asset.delete {path}: {e}"));
                    emit(ctx, "project.asset.failed", Value::map().with("path", path).with("error", e).with("used_by", users));
                }
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

pub fn register_queries(ctx: &Ctx) {
    let root = ctx.project.root().to_path_buf();
    ctx.hub.register_query(
        "project.assets",
        Arc::new(move |_, _| {
            let root = root.clone();
            Box::pin(async move { tokio::task::spawn_blocking(move || Value::List(list(&root))).await.map_err(|e| e.to_string()) })
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("se-assets-").tempdir().unwrap()
    }

    fn put(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn kinds_come_from_the_extension() {
        assert_eq!(kind_of(Path::new("/x/Logo.PNG")), Some("images"));
        assert_eq!(kind_of(Path::new("a.jpeg")), Some("images"));
        assert_eq!(kind_of(Path::new("clip.MOV")), Some("video"));
        assert_eq!(kind_of(Path::new("horn.opus")), Some("sounds"));
        assert_eq!(kind_of(Path::new("warm.cube")), Some("luts"));
        assert_eq!(kind_of(Path::new("Inter.otf")), Some("fonts"));
        assert_eq!(kind_of(Path::new("notes.txt")), None);
        assert_eq!(kind_of(Path::new("README")), None);
    }

    #[test]
    fn imports_never_overwrite_and_refuse_unknown_types() {
        let (src, proj) = (project(), project());
        let horn = put(src.path(), "My Horn!.WAV", b"one");
        let a = import(proj.path(), &horn, None, None).unwrap();
        assert_eq!(a, Imported { path: "assets/sounds/my_horn.wav".into(), kind: "sounds", name: "my_horn".into() });
        std::fs::write(&horn, b"two").unwrap();
        let b = import(proj.path(), &horn, None, None).unwrap();
        assert_eq!(b.path, "assets/sounds/my_horn-2.wav");
        let c = import(proj.path(), &horn, Some("sounds"), Some("my horn")).unwrap();
        assert_eq!(c.path, "assets/sounds/my_horn-3.wav");
        assert_eq!(std::fs::read(proj.path().join(&a.path)).unwrap(), b"one", "the first copy is untouched");
        assert_eq!(std::fs::read(proj.path().join(&b.path)).unwrap(), b"two");
        // no temporary files left behind
        let left: Vec<_> = std::fs::read_dir(proj.path().join("assets/sounds")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 3, "{left:?}");

        let notes = put(src.path(), "notes.txt", b"hi");
        assert!(import(proj.path(), &notes, None, None).unwrap_err().to_string().contains("can't use .txt files"));
        let err = import(proj.path(), &horn, Some("images"), None).unwrap_err().to_string();
        assert_eq!(err, "That's a sound, but this needs a picture.");
        assert!(import(proj.path(), &src.path().join("gone.png"), None, None).unwrap_err().to_string().contains("isn't there"));
        assert!(import(proj.path(), src.path(), None, None).unwrap_err().to_string().contains("folder"));
        assert!(import(proj.path(), &proj.path().join(&a.path), None, None).unwrap_err().to_string().contains("already in your media"));
        assert!(!proj.path().join("assets/images").exists(), "a refused import leaves nothing behind");
    }

    #[test]
    fn asset_paths_stay_in_assets() {
        assert_eq!(check_asset_path("assets/images/logo.png").unwrap(), "images");
        for bad in ["assets/../scenes/x.png", "scenes/logo.png", "/etc/x.png", "assets/.hidden/x.png", "assets/images/notes.txt", ""] {
            assert!(check_asset_path(bad).is_err(), "{bad}");
        }
    }

    fn texts(files: &[(&str, &str)]) -> Vec<(String, String)> {
        files.iter().map(|(f, t)| (f.to_string(), t.to_string())).collect()
    }

    #[test]
    fn references_are_found_by_path_and_by_sound_name() {
        let t = texts(&[
            ("scenes/duo.toml", "[canvas.wide]\nnodes = [{ src = \"cam\", mask = \"assets/images/logo.png\" }]\n"),
            ("alerts/cheers.toml", "sound = \"horn\"\nimage = \"images/logo.png\"\n"),
            ("controllers/deck.toml", "do = [\"audio.play horn\", \"audio.play horn_big\"]\n"),
            ("patches/box/index.html", "<img src=\"/assets/images/logo.png\"><img src=\"images/logo.png\">"),
            ("lights/cues.toml", "cue = \"horn\"\nfile = \"assets/images/logo.png.bak\"\n"),
            ("sources/clip.toml", "file = \"myassets/images/logo.png\"\n"),
        ]);
        assert_eq!(used_by("assets/images/logo.png", &t), ["scenes/duo.toml", "alerts/cheers.toml", "patches/box/index.html"]);
        assert_eq!(used_by("assets/sounds/horn.wav", &t), ["alerts/cheers.toml", "controllers/deck.toml"]);
        // a sound defined in the audio settings: its users still count, but a rename only moves
        // the entry's file path (the name belongs to the entry)
        let mut claimed = t.clone();
        claimed.push(("audio/sounds.toml".into(), "[sounds.horn]\nfile = \"assets/sounds/horn.wav\"\n".into()));
        assert_eq!(used_by("assets/sounds/horn.wav", &claimed), ["alerts/cheers.toml", "controllers/deck.toml", "audio/sounds.toml"]);
        assert_eq!(
            rewrite(&claimed, "assets/sounds/horn.wav", "assets/sounds/boom.wav"),
            texts(&[("audio/sounds.toml", "[sounds.horn]\nfile = \"assets/sounds/boom.wav\"\n")])
        );
    }

    #[test]
    fn rename_rewrites_every_reference_and_nothing_else() {
        let t = texts(&[
            ("scenes/duo.toml", "# keep this comment: assets/images/logo.png.bak\nnodes = [{ src = \"cam\", mask = \"assets/images/logo.png\" }]\n"),
            ("presets/hype.toml", "sound = \"horn\" # the horn\nlights = { cue = \"horn\" }\n"),
            ("controllers/deck.toml", "do = [\"audio.play horn\"]\n"),
            ("rules/main.toml", "[[rule]]\ndo = \"audio.play\"\nargs = { sound = 'horn' }\n"),
        ]);
        let out = rewrite(&t, "assets/images/logo.png", "assets/images/big_logo.png");
        assert_eq!(
            out,
            texts(&[(
                "scenes/duo.toml",
                "# keep this comment: assets/images/logo.png.bak\nnodes = [{ src = \"cam\", mask = \"assets/images/big_logo.png\" }]\n"
            )])
        );
        let out = rewrite(&t, "assets/sounds/horn.wav", "assets/sounds/air_horn.wav");
        assert_eq!(
            out,
            texts(&[
                ("presets/hype.toml", "sound = \"air_horn\" # the horn\nlights = { cue = \"horn\" }\n"),
                ("controllers/deck.toml", "do = [\"audio.play air_horn\"]\n"),
                ("rules/main.toml", "[[rule]]\ndo = \"audio.play\"\nargs = { sound = 'air_horn' }\n"),
            ])
        );
        assert_eq!(renamed_path("assets/sounds/horn.wav", "Air Horn!"), "assets/sounds/air_horn.wav");
    }

    #[test]
    fn listing_reports_kind_size_and_users() {
        let proj = project();
        put(proj.path(), "assets/images/logo.png", b"12345");
        put(proj.path(), "assets/sounds/horn.wav", b"123");
        put(proj.path(), "assets/sounds/.horn.wav.part", b"x");
        put(proj.path(), "assets/readme.txt", b"x");
        put(proj.path(), "scenes/duo.toml", "nodes = [{ src = \"cam\", mask = \"assets/images/logo.png\" }]\n".as_bytes());
        put(proj.path(), "sessions/x/events.json", b"assets/images/logo.png");
        let l = list(proj.path());
        let row = |p: &str| l.iter().find(|v| v.get_path("path").and_then(Value::as_str) == Some(p)).cloned();
        assert_eq!(l.len(), 2, "{l:?}");
        let logo = row("assets/images/logo.png").unwrap();
        assert_eq!(logo.get_path("kind").and_then(Value::as_str), Some("images"));
        assert_eq!(logo.get_path("bytes").and_then(Value::as_i64), Some(5));
        assert_eq!(logo.get_path("used_by"), Some(&Value::from(vec!["scenes/duo.toml".to_string()])));
        let horn = row("assets/sounds/horn.wav").unwrap();
        assert_eq!(horn.get_path("sound").and_then(Value::as_str), Some("horn"));
        assert_eq!(horn.get_path("used_by"), Some(&Value::List(Vec::new())));
    }
}
