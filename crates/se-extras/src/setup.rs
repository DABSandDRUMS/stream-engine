//! First-run setup state (§15.9). The UI opens its setup wizard while `setup.done` is false,
//! i.e. while `project.toml` lacks `[setup] done = true`. The wizard's steps use the owning
//! subsystems' own actions (device naming, Twitch device code, YouTube key, OBS link); this
//! module only records completion:
//!
//! * `setup.complete` writes `[setup] done = true` (comments/formatting preserved),
//! * `setup.reopen` writes `done = false` so the wizard shows again.

use crate::util::Section;
use se_hub::EngineCtx;
use se_proto::{Event, Meta, Op, Origin, Value};
use serde::Deserialize;
use std::path::Path;

const TARGET: &str = "setup";

#[derive(Clone, Debug, Deserialize, Default, PartialEq)]
#[serde(default)]
struct SetupSection {
    done: bool,
}

/// Set `[setup] done` in `project.toml`, creating the table when missing.
pub fn write_done(project_root: &Path, done: bool) -> anyhow::Result<()> {
    let project = se_store::Project::open(project_root)?;
    project.edit("project.toml", |doc| {
        let t = doc.entry("setup").or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        let t = t.as_table_like_mut().ok_or_else(|| anyhow::anyhow!("[setup] in project.toml is not a table"))?;
        t.insert("done", toml_edit::value(done));
        Ok(())
    })
}

pub fn start(ctx: EngineCtx) {
    ctx.hub.declare("setup.done", Meta::boolean(false).readonly().owner(TARGET).describe("First-run setup finished ([setup] done in project.toml)"));
    let mut actions = ctx.hub.route_actions("setup");
    tokio::spawn(async move {
        let mut ctx = ctx;
        let mut section: Section<SetupSection> = Section::new("setup", TARGET);
        section.reload(&ctx);
        ctx.hub.publish("setup.done", Value::Bool(section.value.done));
        loop {
            tokio::select! {
                r = ctx.config.changed() => {
                    if r.is_err() { break; }
                    if section.reload(&ctx) {
                        ctx.hub.publish("setup.done", Value::Bool(section.value.done));
                    }
                }
                a = actions.recv() => {
                    let Some(cmd) = a else { break };
                    let Op::Action { name, .. } = &cmd.op else { continue };
                    let done = match name.as_str() {
                        "setup.complete" => true,
                        "setup.reopen" => false,
                        other => {
                            ctx.hub.log("warn", TARGET, format!("unknown action {other}"));
                            continue;
                        }
                    };
                    let root = ctx.project_root.clone();
                    match tokio::task::spawn_blocking(move || write_done(&root, done)).await {
                        Ok(Ok(())) => {
                            // the project watcher reloads the file; publish now so the UI reacts at once
                            ctx.hub.publish("setup.done", Value::Bool(done));
                            ctx.hub.emit(Event::new(if done { "setup.completed" } else { "setup.reopened" }, Origin::System, Value::Null).with_causal(Some(cmd.id)));
                        }
                        Ok(Err(e)) => ctx.hub.log("error", TARGET, format!("could not update project.toml: {e:#}")),
                        Err(e) => ctx.hub.log("error", TARGET, format!("setup write task: {e}")),
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_done_preserving_comments() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("project.toml"), "# my show\nschema = 1\nname = \"x\"\n\n[canvas.wide]\nwidth = 1920 # full hd\n").unwrap();
        write_done(d.path(), true).unwrap();
        let s = std::fs::read_to_string(d.path().join("project.toml")).unwrap();
        assert!(s.starts_with("# my show\n"), "{s}");
        assert!(s.contains("width = 1920 # full hd"), "{s}");
        let t: toml::Table = s.parse().unwrap();
        assert_eq!(t["setup"]["done"].as_bool(), Some(true));
        write_done(d.path(), false).unwrap();
        let t: toml::Table = std::fs::read_to_string(d.path().join("project.toml")).unwrap().parse().unwrap();
        assert_eq!(t["setup"]["done"].as_bool(), Some(false));
    }

    #[test]
    fn inline_setup_table_is_updated() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("project.toml"), "schema = 1\nsetup = { done = false, note = \"hi\" }\n").unwrap();
        write_done(d.path(), true).unwrap();
        let t: toml::Table = std::fs::read_to_string(d.path().join("project.toml")).unwrap().parse().unwrap();
        assert_eq!(t["setup"]["done"].as_bool(), Some(true));
        assert_eq!(t["setup"]["note"].as_str(), Some("hi"));
    }

    #[test]
    fn rejects_non_table_setup() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("project.toml"), "schema = 1\nsetup = 3\n").unwrap();
        assert!(write_done(d.path(), true).is_err());
    }
}
