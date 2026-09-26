//! Open a file at a line in the user's editor (console click-to-open, §15.5): `$VISUAL` /
//! `$EDITOR` (terminal editors run inside `xdg-terminal-exec`), else Omarchy's
//! `omarchy-launch-editor`.

use std::path::Path;
use std::process::{Command, Stdio};

/// Editors that need a terminal.
const TERMINAL_EDITORS: &[&str] = &["nvim", "vim", "vi", "nano", "micro", "hx", "helix", "kak", "emacs", "fresh"];

/// argv for `editor` opening `path` at `line`.
pub fn argv(editor: &str, path: &str, line: Option<u32>) -> Vec<String> {
    let mut words: Vec<String> = editor.split_whitespace().map(String::from).collect();
    let name = words.first().map(|w| w.rsplit('/').next().unwrap_or(w).to_string()).unwrap_or_default();
    match (name.as_str(), line) {
        ("code" | "codium" | "code-oss" | "cursor", Some(l)) => words.extend(["-g".into(), format!("{path}:{l}")]),
        ("zed" | "subl" | "hx" | "helix", Some(l)) => words.push(format!("{path}:{l}")),
        ("kate", Some(l)) => words.extend(["-l".into(), l.to_string(), path.into()]),
        ("gedit" | "gnome-text-editor", Some(l)) => words.extend([format!("+{l}"), path.into()]),
        (_, Some(l)) => words.extend([format!("+{l}"), path.into()]),
        (_, None) => words.push(path.into()),
    }
    words
}

/// Whether `editor` runs in a terminal.
pub fn needs_terminal(editor: &str) -> bool {
    let first = editor.split_whitespace().next().unwrap_or("");
    let name = first.rsplit('/').next().unwrap_or(first);
    TERMINAL_EDITORS.contains(&name) || (name == "emacs" && editor.contains("-nw"))
}

fn usable(v: &str) -> bool {
    let v = v.trim();
    !v.is_empty() && v != "true" && v != "false" && v != ":"
}

/// Launch detached; returns the command line used.
pub fn open(path: &Path, line: Option<u32>) -> std::io::Result<String> {
    let p = path.to_string_lossy().to_string();
    let editor = std::env::var("VISUAL").ok().filter(|v| usable(v)).or_else(|| std::env::var("EDITOR").ok().filter(|v| usable(v)));
    let argv: Vec<String> = match editor {
        Some(ed) if needs_terminal(&ed) => {
            let mut a = vec!["xdg-terminal-exec".to_string(), "--".into()];
            a.extend(argv(&ed, &p, line));
            a
        }
        Some(ed) => argv(&ed, &p, line),
        None => {
            // Omarchy's default editor launcher (handles TUI editors itself)
            let mut a = vec!["omarchy-launch-editor".to_string()];
            if let Some(l) = line {
                a.push(format!("+{l}"));
            }
            a.push(p.clone());
            a
        }
    };
    let mut child = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    // reap in the background so no zombie is left behind
    std::thread::Builder::new().name("se-editor-reap".into()).spawn(move || {
        let _ = child.wait();
    })?;
    Ok(argv.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_arguments_per_editor() {
        assert_eq!(argv("nvim", "/p/a.lua", Some(12)), vec!["nvim", "+12", "/p/a.lua"]);
        assert_eq!(argv("code --wait", "/p/a.lua", Some(3)), vec!["code", "--wait", "-g", "/p/a.lua:3"]);
        assert_eq!(argv("/usr/bin/hx", "/p/a.lua", Some(7)), vec!["/usr/bin/hx", "/p/a.lua:7"]);
        assert_eq!(argv("zed", "/p/x.toml", None), vec!["zed", "/p/x.toml"]);
        assert!(needs_terminal("nvim"));
        assert!(needs_terminal("/usr/bin/vim -u NONE"));
        assert!(!needs_terminal("code"));
        assert!(!usable("true") && !usable("") && usable("nvim"));
    }
}
