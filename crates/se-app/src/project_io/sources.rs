//! Explicit, project-backed scene sources, independent of the video subsystem.

use super::{CONFIG_WRITE, WriteOutcome};
use crate::daemon::Ctx;
use anyhow::{Context, Result, anyhow, bail};
use se_proto::{Event, Op, Origin, Value};
use se_store::project::{Loaded, Project};
use se_video_in::config::{self, Kind};
use std::path::Path;
use std::sync::Arc;

fn name(args: &Value) -> Result<&str> {
    let name = args.get_path("name").and_then(Value::as_str).ok_or_else(|| anyhow!("source needs `name`"))?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        bail!("source name must contain only letters, numbers, _ or -");
    }
    Ok(name)
}

fn references(loaded: &Loaded, name: &str) -> Vec<String> {
    fn visit_table(table: &toml::Table, key: &str, name: &str, out: &mut Vec<String>) {
        for (k, v) in table {
            let path = if key.is_empty() { k.clone() } else { format!("{key}.{k}") };
            if k == "src" && v.as_str() == Some(name) {
                out.push(path);
            } else {
                visit(v, &path, name, out);
            }
        }
    }
    fn visit(value: &toml::Value, key: &str, name: &str, out: &mut Vec<String>) {
        match value {
            toml::Value::Table(table) => visit_table(table, key, name, out),
            toml::Value::Array(values) => {
                for (i, value) in values.iter().enumerate() {
                    visit(value, &format!("{key}[{i}]"), name, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for scene in loaded.files.iter().filter(|f| f.kind == "scenes") {
        let mut keys = Vec::new();
        visit_table(&scene.table, "", name, &mut keys);
        out.extend(keys.into_iter().map(|key| format!("{}: {key}", scene.path)));
    }
    out
}

fn inventory(project: &Project) -> Value {
    let _guard = CONFIG_WRITE.lock();
    let loaded = project.load();
    let mut errors: Vec<String> =
        loaded.errors.iter().filter(|e| e.file.starts_with("sources/") || e.file.starts_with("scenes/")).map(|e| format!("{}: {}", e.file, e.msg)).collect();
    let mut sources = Vec::new();
    for file in loaded.files.iter().filter(|f| f.kind == "sources") {
        let mut row = Value::from(toml::Value::Table(file.table.clone()));
        row = row
            .with("name", file.name.as_str())
            .with("origins", vec![Value::map().with("path", file.path.as_str()).with("key", "")])
            .with("references", references(&loaded, &file.name));
        match config::parse(&file.name, &file.table, project.root()) {
            Ok(def) => {
                row = row.with("label", def.label.as_str()).with("kind", def.kind_str());
                match def.kind {
                    Kind::Camera(c) => {
                        row = row
                            .with("device", c.device)
                            .with("format", if c.fourcc == se_devices::v4l2::PIX_YUYV { "yuyv" } else { "mjpeg" })
                            .with("size", vec![c.width as i64, c.height as i64])
                            .with("fps", c.fps);
                    }
                    Kind::File(f) => row = row.with("file", f.rel).with("loop", f.looping).with("rate", f.rate),
                }
            }
            Err(error) => {
                errors.push(format!("{}: {error}", file.path));
                row = row.with("kind", if file.table.contains_key("device") { "camera" } else { "file" }).with("error", error);
            }
        }
        sources.push(row);
    }
    for error in loaded.errors.iter().filter(|e| Path::new(&e.file).parent() == Some(Path::new("sources"))) {
        let name = Path::new(&error.file).file_stem().and_then(|s| s.to_str()).unwrap_or("");
        sources.push(
            Value::map()
                .with("name", name)
                .with("label", name)
                .with("error", error.msg.as_str())
                .with("origins", vec![Value::map().with("path", error.file.as_str()).with("key", "")])
                .with("references", references(&loaded, name)),
        );
    }
    sources.sort_by(|a, b| a.get_path("name").and_then(Value::as_str).cmp(&b.get_path("name").and_then(Value::as_str)));
    Value::map().with("sources", sources).with("errors", errors)
}

fn source_path(project: &Project, name: &str) -> Result<String> {
    let rel = format!("sources/{name}.toml");
    super::check_path(&rel)?;
    let dir = project.root().join("sources");
    if dir.exists() && !dir.canonicalize()?.starts_with(project.root()) {
        bail!("sources/ must remain inside the project");
    }
    let path = project.root().join(&rel);
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("{rel} is a symbolic link; manage the original file directly");
    }
    Ok(rel)
}

fn save(project: &Project, args: &Value) -> Result<String> {
    let _guard = CONFIG_WRITE.lock();
    let name = name(args)?;
    let rel = source_path(project, name)?;
    let old = match std::fs::read_to_string(project.root().join(&rel)) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {rel}")),
    };
    let create = matches!(args.get_path("create"), Some(Value::Bool(true)));
    if create && old.is_some() {
        bail!("source `{name}` already exists; choose another name or edit the existing source");
    }
    if !create && old.is_none() {
        bail!("source `{name}` no longer exists; use Add source to create it");
    }
    let previous: Option<toml::Table> =
        old.as_deref().map(str::parse).transpose().context("existing source is not valid TOML; fix its syntax before editing")?;
    if previous
        .as_ref()
        .is_some_and(|t| (t.contains_key("device") && args.get_path("file").is_some()) || (t.contains_key("file") && args.get_path("device").is_some()))
    {
        bail!("a source's type cannot change; add a separate camera or media source instead");
    }
    let mut set = Value::map();
    for key in ["label", "device", "format", "size", "fps", "file", "loop", "rate"] {
        if let Some(value) = args.get_path(key) {
            match (key, value) {
                ("label" | "device" | "format" | "file", Value::Str(_)) | ("loop", Value::Bool(_)) => {}
                ("fps" | "rate", Value::Int(_)) => {}
                ("fps" | "rate", Value::Float(v)) if v.is_finite() => {}
                ("size", Value::List(v)) if v.len() == 2 && v.iter().all(|v| matches!(v, Value::Int(_))) => {}
                _ => bail!("invalid value for `{key}`"),
            }
            set = set.with(key, value.clone());
        }
    }
    let changed = match super::apply(old.as_deref(), &Value::map().with("set", set))? {
        WriteOutcome::Write(text) => Some(text),
        WriteOutcome::Unchanged => None,
        WriteOutcome::Delete => unreachable!(),
    };
    let text = changed.as_deref().or(old.as_deref()).unwrap_or("");
    let table: toml::Table = text.parse()?;
    let def = config::parse(name, &table, project.root()).map_err(|e| anyhow!("{rel}: {e}"))?;
    if let Kind::File(file) = &def.kind {
        let unchanged = previous.as_ref().and_then(|t| t.get("file")).and_then(toml::Value::as_str) == Some(file.rel.as_str());
        if !unchanged {
            // videos, and pictures FFmpeg decodes (not SVG)
            let playable = match crate::assets::check_asset_path(&file.rel)? {
                "video" => true,
                "images" => !file.rel.to_ascii_lowercase().ends_with(".svg"),
                _ => false,
            };
            if !playable {
                bail!("choose a video, picture or GIF from project media");
            }
            let path = file.path.canonicalize().with_context(|| format!("media file `{}` is missing", file.rel))?;
            if !path.starts_with(project.root()) || !path.is_file() {
                bail!("media must be a file inside the project");
            }
        }
    }
    if changed.is_some() {
        project.write_file(&rel, text)?;
    }
    Ok(rel)
}

fn remove(project: &Project, args: &Value) -> Result<String> {
    let _guard = CONFIG_WRITE.lock();
    let name = name(args)?;
    let rel = source_path(project, name)?;
    let loaded = project.load();
    let unknown: Vec<&str> = loaded.errors.iter().filter(|e| e.file.starts_with("scenes/")).map(|e| e.file.as_str()).collect();
    if !unknown.is_empty() {
        bail!("cannot check scene usage; fix parse errors in {} first", unknown.join(", "));
    }
    let references = references(&loaded, name);
    if !references.is_empty() {
        bail!("source `{name}` is still used by {}. Remove these source nodes in Scenes before removing the source", references.join(", "));
    }
    std::fs::remove_file(project.root().join(&rel)).with_context(|| format!("remove {rel}"))?;
    Ok(rel)
}

pub fn start(ctx: &Ctx) {
    let project = ctx.project.clone();
    ctx.hub.register_query(
        "project.sources",
        Arc::new(move |_, _| {
            let project = project.clone();
            Box::pin(async move { Ok(inventory(&project)) })
        }),
    );
    let mut actions = ctx.hub.route_actions("project.source");
    let ctx = ctx.clone();
    tokio::spawn(async move {
        while let Some(command) = actions.recv().await {
            let Op::Action { name, args } = command.op else { continue };
            let result = match name.as_str() {
                "project.source.save" => save(&ctx.project, &args),
                "project.source.remove" => remove(&ctx.project, &args),
                _ => Err(anyhow!("unknown source action `{name}`")),
            };
            let source = args.get_path("name").and_then(Value::as_str).unwrap_or("");
            match result {
                Ok(path) => {
                    crate::daemon::reload(&ctx, &[path]);
                    ctx.hub.emit(Event::new("project.sources.changed", Origin::System, Value::map().with("name", source).with("action", name)));
                }
                Err(error) => {
                    ctx.hub.log("error", "project", format!("{name}: {error:#}"));
                    ctx.hub.emit(Event::new("project.source.failed", Origin::System, Value::map().with("name", source).with("error", format!("{error:#}"))));
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("project.toml"), "schema = 1\n").unwrap();
        for (rel, text) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let project = Project::open(dir.path()).unwrap();
        (dir, project)
    }

    #[test]
    fn duplicate_create_cannot_overwrite_and_edit_preserves_source_settings() {
        let original = "# camera notes\ndevice = \"disconnected\"\nformat = \"yuyv\"\nsize = [640, 480]\nfps = 30\nfx = [{ name = \"grade\", amount = 0.3 }]\n[controls]\nbrightness = 127\n[color]\ncontrast = 1.2\n[signal]\ntimeout = \"900ms\"\n";
        let (_dir, project) = project(&[("sources/cam.toml", original)]);
        let path = project.root().join("sources/cam.toml");
        let args = Value::map()
            .with("name", "cam")
            .with("label", "New label")
            .with("device", "another-disconnected-target")
            .with("format", "mjpeg")
            .with("size", vec![1280_i64, 720])
            .with("fps", 60.0);
        assert!(save(&project, &args.clone().with("create", true)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        save(&project, &args).unwrap();
        let saved = std::fs::read_to_string(path).unwrap();
        let before: toml::Table = original.parse().unwrap();
        let after: toml::Table = saved.parse().unwrap();
        for field in ["controls", "color", "signal", "fx"] {
            assert_eq!(after[field], before[field], "{field}");
        }
        assert_eq!(after["device"].as_str(), Some("another-disconnected-target"));
        assert_eq!(after["label"].as_str(), Some("New label"));
        assert!(saved.starts_with("# camera notes\n"));
        assert_eq!(inventory(&project).get_path("sources.0.device").and_then(Value::as_str), Some("another-disconnected-target"));
    }

    #[test]
    fn referenced_removal_is_refused_until_scene_use_is_removed() {
        let scene = "[canvas.wide]\nnodes = [{ src = \"cam\", id = \"camera\" }, { src = \"other\" }]\n[canvas.tall]\nnodes = [{ src = \"cam\" }]\n";
        let (_dir, project) = project(&[("sources/cam.toml", "device = \"disconnected\"\n"), ("scenes/show.toml", scene)]);
        let args = Value::map().with("name", "cam");
        let refs: std::collections::BTreeSet<_> = references(&project.load(), "cam").into_iter().collect();
        assert_eq!(
            refs,
            std::collections::BTreeSet::from([
                "scenes/show.toml: canvas.tall.nodes[0].src".to_string(),
                "scenes/show.toml: canvas.wide.nodes[0].src".to_string(),
            ])
        );
        assert!(remove(&project, &args).is_err());
        assert!(project.root().join("sources/cam.toml").exists());
        assert_eq!(std::fs::read_to_string(project.root().join("scenes/show.toml")).unwrap(), scene);
        std::fs::write(project.root().join("scenes/show.toml"), "[canvas.wide]\nnodes = [{ src = \"other\" }]\n").unwrap();
        remove(&project, &args).unwrap();
        assert!(!project.root().join("sources/cam.toml").exists());
    }

    #[test]
    fn parse_errors_are_listed_and_unknown_scene_use_blocks_removal() {
        let (_dir, project) =
            project(&[("sources/cam.toml", "device = \"disconnected\"\n"), ("sources/broken.toml", "device = [\n"), ("scenes/broken.toml", "canvas = [\n")]);
        let reply = inventory(&project);
        let sources = reply.get_path("sources").and_then(Value::as_list).unwrap();
        let broken = sources.iter().find(|s| s.get_path("name").and_then(Value::as_str) == Some("broken")).unwrap();
        assert_eq!(broken.get_path("origins.0.path").and_then(Value::as_str), Some("sources/broken.toml"));
        assert!(broken.get_path("error").and_then(Value::as_str).is_some());
        assert!(remove(&project, &Value::map().with("name", "cam")).is_err());
        assert!(project.root().join("sources/cam.toml").exists());
    }

    #[test]
    fn media_removal_keeps_the_asset_and_missing_assets_reject_new_sources() {
        let (_dir, project) = project(&[("assets/video/clip.mp4", "fixture")]);
        let args = Value::map().with("name", "clip").with("create", true).with("file", "assets/video/clip.mp4").with("loop", false).with("rate", 0.5);
        save(&project, &args).unwrap();
        let reply = inventory(&project);
        assert_eq!(reply.get_path("sources.0.loop"), Some(&Value::Bool(false)));
        assert_eq!(reply.get_path("sources.0.rate"), Some(&Value::Float(0.5)));
        remove(&project, &Value::map().with("name", "clip")).unwrap();
        assert_eq!(std::fs::read_to_string(project.root().join("assets/video/clip.mp4")).unwrap(), "fixture");
        assert!(save(&project, &args.with("file", "assets/video/missing.mp4")).is_err());
        assert!(!project.root().join("sources/clip.toml").exists());
    }
}
