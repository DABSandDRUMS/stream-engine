//! "New patch" (§6.1, §6.5): create `project/patches/<id>/` from a template in
//! `<share>/templates/patches/<kind>/<template>/` and open it in the user's editor.
//!
//! Text files in the template may use `{{id}}` and `{{label}}` placeholders.

use crate::manifest::Manifest;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const KINDS: &[&str] = &["shader", "particles", "script", "web", "dsp"];
const TEXT_EXT: &[&str] = &["toml", "lua", "wgsl", "html", "htm", "js", "css", "rs", "md", "txt", "json", "sh", "zig", "c", "h"];

#[derive(Clone, Debug, PartialEq)]
pub struct TemplateInfo {
    pub kind: String,
    pub name: String,
    pub description: String,
    pub layer: String,
}

pub fn root(share: &Path) -> PathBuf {
    share.join("templates").join("patches")
}

/// Every template, by kind then name.
pub fn list(share: &Path) -> Vec<TemplateInfo> {
    let mut out = Vec::new();
    for kind in KINDS {
        let Ok(rd) = std::fs::read_dir(root(share).join(kind)) else { continue };
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.join("patch.toml").is_file()).collect();
        dirs.sort();
        for d in dirs {
            let name = d.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let table: toml::Table = std::fs::read_to_string(d.join("patch.toml")).ok().and_then(|s| s.parse().ok()).unwrap_or_default();
            let text = |k: &str| table.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
            out.push(TemplateInfo { kind: kind.to_string(), name, description: text("description"), layer: text("layer") });
        }
    }
    out
}

/// Human label from an id: `sub_meteors` → `Sub meteors`.
pub fn label_for(id: &str) -> String {
    let s = id.replace(['_', '-'], " ");
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn copy_dir(src: &Path, dst: &Path, id: &str, label: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let p = e.path();
        let name = e.file_name();
        if name.to_string_lossy().starts_with('.') || name == "target" {
            continue;
        }
        let to = dst.join(&name);
        if e.file_type()?.is_dir() {
            copy_dir(&p, &to, id, label)?;
        } else {
            let text = p.extension().and_then(|x| x.to_str()).is_some_and(|x| TEXT_EXT.contains(&x));
            match (text, std::fs::read_to_string(&p)) {
                (true, Ok(s)) => {
                    std::fs::write(&to, s.replace("{{id}}", id).replace("{{label}}", label))?;
                    // keep modes (build scripts stay executable)
                    std::fs::set_permissions(&to, e.metadata()?.permissions())?;
                }
                _ => {
                    std::fs::copy(&p, &to)?;
                }
            }
        }
    }
    Ok(())
}

/// Create `project/patches/<id>/` from `kind`/`template`. The folder appears atomically
/// (built under a hidden name, validated, then renamed).
pub fn create(share: &Path, project_root: &Path, id: &str, kind: &str, template: &str) -> Result<Manifest, String> {
    if !se_proto::address::is_valid(id, false) || id.contains('.') || id.starts_with('_') {
        return Err(format!("`{id}` is not a valid patch id (one address segment: letters, digits, `_`, `-`)"));
    }
    if !KINDS.contains(&kind) {
        return Err(format!("unknown patch kind `{kind}` (one of {})", KINDS.join(", ")));
    }
    if template.is_empty() || template.contains(['/', '\\']) || template.starts_with('.') {
        return Err(format!("bad template name `{template}`"));
    }
    let src = root(share).join(kind).join(template);
    if !src.join("patch.toml").is_file() {
        let names: Vec<String> = list(share).into_iter().filter(|t| t.kind == kind).map(|t| t.name).collect();
        return Err(format!("no template `{template}` for kind `{kind}` in {} (available: {})", root(share).join(kind).display(), names.join(", ")));
    }
    let patches = project_root.join("patches");
    let dst = patches.join(id);
    if dst.exists() {
        return Err(format!("patch `{id}` already exists ({})", dst.display()));
    }
    std::fs::create_dir_all(&patches).map_err(|e| format!("{}: {e}", patches.display()))?;
    let tmp = patches.join(format!(".{id}.new-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let label = label_for(id);
    let built = copy_dir(&src, &tmp, id, &label).map_err(|e| format!("copy template: {e}")).and_then(|_| {
        // validate under the final name so the id check applies
        let raw = std::fs::read_to_string(tmp.join("patch.toml")).map_err(|e| format!("template patch.toml: {e}"))?;
        let m = Manifest::parse(&dst, &raw).map_err(|e| format!("template `{kind}/{template}` is broken: {e}"))?;
        if m.kind.as_str() != kind {
            return Err(format!("template `{kind}/{template}` declares kind `{}`", m.kind.as_str()));
        }
        Ok(m)
    });
    match built {
        Ok(m) => {
            std::fs::rename(&tmp, &dst).map_err(|e| {
                let _ = std::fs::remove_dir_all(&tmp);
                format!("create {}: {e}", dst.display())
            })?;
            Ok(m)
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Err(e)
        }
    }
}

/// The file to open for editing: the entry, or for compiled `dsp` patches their source.
pub fn edit_target(m: &Manifest) -> PathBuf {
    let entry = m.entry_path();
    if entry.extension().is_some_and(|x| x == "wasm") {
        for cand in ["src/lib.rs", "main.zig", "main.c"] {
            let p = m.dir.join(cand);
            if p.is_file() {
                return p;
            }
        }
        return m.dir.clone();
    }
    entry
}

fn in_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// Open `path` in the user's editor without blocking: `editor` (a shell command) if given,
/// else `$STREAM_ENGINE_EDITOR`, `omarchy-launch-editor`, `$VISUAL`, then `xdg-open`.
/// Returns the launcher used.
pub fn open_in_editor(path: &Path, editor: Option<&str>) -> Result<String, String> {
    let env_cmd = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty() && s.trim() != "true");
    let shell_cmd = editor.map(str::to_string).or_else(|| env_cmd("STREAM_ENGINE_EDITOR"));
    let (label, mut cmd) = if let Some(c) = shell_cmd {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("{c} \"$1\"")).arg("sh").arg(path);
        (c, cmd)
    } else if in_path("omarchy-launch-editor") {
        let mut cmd = Command::new("omarchy-launch-editor");
        cmd.arg(path);
        ("omarchy-launch-editor".to_string(), cmd)
    } else if let Some(v) = env_cmd("VISUAL") {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("{v} \"$1\"")).arg("sh").arg(path);
        (v, cmd)
    } else if in_path("xdg-open") {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(path);
        ("xdg-open".to_string(), cmd)
    } else {
        return Err("no editor found: set STREAM_ENGINE_EDITOR or VISUAL, or install xdg-utils".into());
    };
    use std::os::unix::process::CommandExt;
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
    let mut child = cmd.spawn().map_err(|e| format!("{label}: {e}"))?;
    std::thread::Builder::new()
        .name("se-editor-reap".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|e| e.to_string())?;
    Ok(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share_with_template() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let t = root(d.path()).join("script").join("default");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("patch.toml"), "kind = \"script\"\nlabel = \"{{label}}\"\ndescription = \"blank script\"\nlayer = \"overlay\"\n").unwrap();
        std::fs::write(t.join("main.lua"), "-- {{id}}\nfunction frame(dt, s) end\n").unwrap();
        d
    }

    #[test]
    fn creates_from_template_with_placeholders() {
        let share = share_with_template();
        let proj = tempfile::tempdir().unwrap();
        let m = create(share.path(), proj.path(), "my_fx", "script", "default").unwrap();
        assert_eq!(m.id, "my_fx");
        assert_eq!(m.label, "My fx");
        let lua = std::fs::read_to_string(proj.path().join("patches/my_fx/main.lua")).unwrap();
        assert!(lua.starts_with("-- my_fx"));
        // no temp leftovers
        let names: Vec<_> = std::fs::read_dir(proj.path().join("patches")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("my_fx")]);
        assert_eq!(
            list(share.path()),
            vec![TemplateInfo { kind: "script".into(), name: "default".into(), description: "blank script".into(), layer: "overlay".into() }]
        );
    }

    #[test]
    fn refuses_bad_ids_existing_and_unknown() {
        let share = share_with_template();
        let proj = tempfile::tempdir().unwrap();
        assert!(create(share.path(), proj.path(), "a.b", "script", "default").is_err());
        assert!(create(share.path(), proj.path(), "../x", "script", "default").is_err());
        assert!(create(share.path(), proj.path(), "x", "nope", "default").is_err());
        assert!(create(share.path(), proj.path(), "x", "script", "missing").unwrap_err().contains("available: default"));
        create(share.path(), proj.path(), "x", "script", "default").unwrap();
        assert!(create(share.path(), proj.path(), "x", "script", "default").unwrap_err().contains("already exists"));
    }
}
