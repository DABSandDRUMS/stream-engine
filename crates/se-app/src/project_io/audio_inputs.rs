//! Explicit physical-input management, independent of the running audio subsystem.

use super::{WriteOutcome, apply};
use crate::daemon::Ctx;
use anyhow::{Context, Result, anyhow, bail};
use se_audio::config::{self, BUILTIN_BUSES, OPTIONAL_BUSES};
use se_proto::{Event, Op, Origin, Value};
use se_store::project::Project;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

struct Layer {
    path: String,
    text: String,
    audio: toml::Table,
}

impl Layer {
    fn key(&self, name: &str) -> String {
        format!("{}inputs.{name}", if self.path == "project.toml" { "audio." } else { "" })
    }

    fn input(&self, name: &str) -> Option<&toml::Value> {
        self.audio.get("inputs")?.get(name)
    }
}

struct Saved {
    layers: Vec<Layer>,
    merged: toml::Table,
    errors: Vec<String>,
}

fn audio_section(path: &str, text: &str) -> Result<toml::Table> {
    let mut table: toml::Table = text.parse().with_context(|| format!("parse {path}"))?;
    if path != "project.toml" {
        return Ok(table);
    }
    match table.remove("audio") {
        None => Ok(toml::Table::new()),
        Some(toml::Value::Table(t)) => Ok(t),
        Some(_) => bail!("project.toml: audio must be a table"),
    }
}

impl Saved {
    fn load(project: &Project) -> Self {
        let loaded = project.load();
        let relevant = |path: &str| path == "project.toml" || project.classify(&project.root().join(path)).is_some_and(|(kind, _)| kind == "audio");
        let mut errors: Vec<String> = loaded.errors.iter().filter(|e| relevant(&e.file)).map(|e| format!("{}: {}", e.file, e.msg)).collect();
        let mut files: Vec<_> = loaded.files.into_iter().filter(|f| f.kind == "project" || f.kind == "audio").collect();
        files.sort_by(|a, b| (a.kind != "project", &a.name).cmp(&(b.kind != "project", &b.name)));
        let mut layers = Vec::new();
        let mut merged = toml::Table::new();
        for file in files {
            let layer = (|| -> Result<Layer> {
                let text = std::fs::read_to_string(project.root().join(&file.path)).with_context(|| format!("read {}", file.path))?;
                let audio = audio_section(&file.path, &text)?;
                Ok(Layer { path: file.path, text, audio })
            })();
            match layer {
                Ok(layer) => {
                    config::deep_merge(&mut merged, &layer.audio);
                    layers.push(layer);
                }
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
        if !layers.iter().any(|layer| layer.path == "project.toml") && errors.is_empty() {
            errors.push("project.toml is missing".into());
        }
        Self { layers, merged, errors }
    }

    fn origins(&self, name: &str) -> Vec<Value> {
        self.layers.iter().filter(|l| l.input(name).is_some()).map(|l| Value::map().with("path", l.path.clone()).with("key", l.key(name))).collect()
    }

    fn field_owner(&self, name: &str, field: &str) -> Option<usize> {
        let mut owner = None;
        for (index, layer) in self.layers.iter().enumerate() {
            let Some(inputs) = layer.audio.get("inputs") else { continue };
            if !inputs.is_table() {
                owner = None;
            } else if let Some(input) = inputs.get(name) {
                if !input.is_table() {
                    owner = None;
                } else if input.get(field).is_some() {
                    owner = Some(index);
                }
            }
        }
        owner
    }

    fn references(&self, name: &str) -> Vec<String> {
        let mut keys = Vec::new();
        if at(&self.merged, &["analysis", "mic"]).and_then(toml::Value::as_str) == Some(name) {
            keys.push(vec!["analysis".to_string(), "mic".to_string()]);
        }
        if at(&self.merged, &["monitor", "inputs"]).and_then(toml::Value::as_array).is_some_and(|a| a.iter().any(|v| v.as_str() == Some(name))) {
            keys.push(vec!["monitor".to_string(), "inputs".to_string()]);
        }
        if let Some(pads) = at(&self.merged, &["drums", "pads"]).and_then(toml::Value::as_table) {
            for (pad, config) in pads {
                if config.get("input").and_then(toml::Value::as_str) == Some(name) {
                    keys.push(vec!["drums".into(), "pads".into(), pad.clone(), "input".into()]);
                }
            }
        }
        keys.into_iter()
            .map(|key| {
                let segments: Vec<&str> = key.iter().map(String::as_str).collect();
                let layer = self.layers.iter().rev().find(|l| at(&l.audio, &segments).is_some());
                match layer {
                    Some(l) => format!("{}: {}{}", l.path, if l.path == "project.toml" { "audio." } else { "" }, key.join(".")),
                    None => key.join("."),
                }
            })
            .collect()
    }

    fn query(&self) -> Value {
        let parsed = config::parse(&self.merged, &BTreeMap::new());
        let mut errors = self.errors.clone();
        if let Err(e) = &parsed {
            errors.push(format!("audio: {e}"));
        }
        let inputs = self
            .merged
            .get("inputs")
            .and_then(toml::Value::as_table)
            .into_iter()
            .flatten()
            .map(|(name, input)| {
                let field = |key: &str, default: Value| input.get(key).cloned().map(Value::from).unwrap_or(default);
                let delay =
                    parsed.as_ref().ok().and_then(|c| c.inputs.iter().find(|i| &i.name == name)).map(|i| Value::from(i.delay_ms as f64)).unwrap_or_else(|| {
                        match input.get("delay") {
                            Some(toml::Value::String(s)) => se_proto::parse_duration_ms(s).map(Value::from).unwrap_or(Value::Null),
                            Some(v) => Value::from(v.clone()),
                            None => Value::from(0.0),
                        }
                    });
                Value::map()
                    .with("name", name.clone())
                    .with("label", field("label", Value::Null))
                    .with("target", field("target", Value::Null))
                    .with("channels", field("channels", Value::List(vec![1.into(), 2.into()])))
                    .with("bus", field("bus", name.clone().into()))
                    .with("gain", field("gain", 0.0.into()))
                    .with("mute", field("mute", false.into()))
                    .with("delay_ms", delay)
                    .with("origins", Value::List(self.origins(name)))
                    .with("references", self.references(name))
            })
            .collect();
        Value::map()
            .with("inputs", Value::List(inputs))
            .with("buses", Value::List(bus_intent(&self.merged, parsed.as_ref().ok())))
            .with("errors", errors)
            .with("max_delay_ms", parsed.as_ref().map_or(Value::Null, |c| Value::from(c.max_delay_ms as f64)))
    }
}

fn at<'a>(table: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Value> {
    let (first, rest) = path.split_first()?;
    let mut value = table.get(*first)?;
    for key in rest {
        value = value.get(*key)?;
    }
    Some(value)
}

fn bus_intent(audio: &toml::Table, parsed: Option<&config::AudioConfig>) -> Vec<Value> {
    let mut configured = BTreeSet::<String>::new();
    let mut mark = |name: &str| {
        if segment_ok(name) {
            configured.insert(name.to_string());
        }
    };
    if let Some(buses) = audio.get("buses").and_then(toml::Value::as_table) {
        for name in buses.keys() {
            mark(name);
        }
    }
    for section in ["inputs", "sounds", "sources"] {
        if let Some(entries) = audio.get(section).and_then(toml::Value::as_table) {
            for (name, entry) in entries {
                if let Some(bus) = entry.get("bus").and_then(toml::Value::as_str) {
                    mark(bus);
                } else if section == "inputs" {
                    mark(name);
                } else if section == "sounds" {
                    mark("sfx");
                }
            }
        }
    }
    if let Some(slots) = audio.get("slots").and_then(toml::Value::as_table) {
        for entry in slots.values() {
            if let Some(bus) = entry.as_str().or_else(|| entry.get("bus").and_then(toml::Value::as_str)).filter(|b| *b != "none") {
                mark(bus);
            }
        }
    }
    for (section, key) in [("monitor", "buses"), ("playback", "buses"), ("analysis", "buses"), ("duck", "targets"), ("duck", "keys")] {
        if let Some(values) = at(audio, &[section, key]).and_then(toml::Value::as_array) {
            for value in values.iter().filter_map(toml::Value::as_str) {
                mark(value);
            }
        }
    }
    if let Some(name) = at(audio, &["analysis", "beat_source"]).and_then(toml::Value::as_str).filter(|b| *b != "auto") {
        mark(name);
    }
    if let Some(duck) = audio.get("duck").and_then(toml::Value::as_table) {
        if !duck.contains_key("targets") {
            mark("music");
        }
        if !duck.contains_key("keys") {
            mark("tts");
        }
    }
    fn fx_keys(value: &toml::Value, mark: &mut impl FnMut(&str)) {
        match value {
            toml::Value::Table(table) => {
                if let Some(fx) = table.get("fx").and_then(toml::Value::as_array) {
                    for entry in fx {
                        if let Some(key) = entry.get("key").and_then(toml::Value::as_str) {
                            mark(key);
                        }
                    }
                }
                for value in table.values() {
                    fx_keys(value, mark);
                }
            }
            toml::Value::Array(values) => {
                for value in values {
                    fx_keys(value, mark);
                }
            }
            _ => {}
        }
    }
    for value in audio.values() {
        fx_keys(value, &mut mark);
    }
    let mut names: BTreeSet<String> = BUILTIN_BUSES.iter().chain(OPTIONAL_BUSES).map(|s| s.to_string()).collect();
    names.extend(configured.iter().cloned());
    names
        .into_iter()
        .map(|name| {
            let label = parsed.and_then(|c| c.buses.iter().find(|bus| bus.name == name)).map_or_else(|| name.clone(), |bus| bus.label.clone());
            Value::map().with("configured", configured.contains(&name)).with("label", label).with("name", name)
        })
        .collect()
}

fn segment_ok(name: &str) -> bool {
    !name.is_empty() && !name.contains('.') && se_proto::address::is_valid(name, false)
}

fn requested_name(args: &Value) -> Result<&str> {
    let name = args.get_path("name").and_then(Value::as_str).ok_or_else(|| anyhow!("input needs `name`"))?;
    if !segment_ok(name) {
        bail!("bad input name `{name}`: use one address segment");
    }
    Ok(name)
}

fn save_fields(args: &Value) -> Result<BTreeMap<String, Value>> {
    let target = args
        .get_path("target")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && !s.contains('\0'))
        .ok_or_else(|| anyhow!("input needs `target` (source node name or description)"))?;
    let bus = args
        .get_path("bus")
        .and_then(Value::as_str)
        .filter(|b| segment_ok(b) && *b != "program")
        .ok_or_else(|| anyhow!("input needs a valid destination `bus` other than program"))?;
    let channels = args.get_path("channels").and_then(Value::as_list).ok_or_else(|| anyhow!("input needs `channels`"))?;
    if !(1..=2).contains(&channels.len())
        || channels.iter().any(|v| !matches!(v, Value::Int(n) if *n > 0 && *n <= u32::MAX as i64))
        || (channels.len() == 2 && channels[0] == channels[1])
    {
        bail!("channels must be one or two distinct, positive one-based device channel numbers");
    }
    let mut fields = BTreeMap::from([("target".into(), target.into()), ("bus".into(), bus.into()), ("channels".into(), Value::List(channels.to_vec()))]);
    for (arg, key) in [("gain", "gain"), ("delay_ms", "delay")] {
        if let Some(value) = args.get_path(arg) {
            let number = match value {
                Value::Int(n) => *n as f64,
                Value::Float(n) => *n,
                _ => bail!("{arg} must be a finite number"),
            };
            if !number.is_finite() || !(number as f32).is_finite() || (arg == "delay_ms" && number < 0.0) {
                bail!("{arg} must be finite{}", if arg == "delay_ms" { " and non-negative" } else { "" });
            }
            fields.insert(key.into(), value.clone());
        }
    }
    if let Some(value) = args.get_path("label") {
        let Value::Str(label) = value else { bail!("label must be text") };
        if label.trim().is_empty() || label.contains('\0') {
            bail!("give the input a name");
        }
        fields.insert("label".into(), label.trim().into());
    }
    if let Some(value) = args.get_path("mute") {
        if !matches!(value, Value::Bool(_)) {
            bail!("mute must be true or false");
        }
        fields.insert("mute".into(), value.clone());
    }
    Ok(fields)
}

struct Change {
    path: String,
    before: String,
    after: String,
}

fn plan(saved: &Saved, name: &str, args: Option<&Value>) -> Result<Vec<Change>> {
    if !saved.errors.is_empty() {
        bail!("fix audio configuration errors first: {}", saved.errors.join("; "));
    }
    let origins: Vec<usize> = saved.layers.iter().enumerate().filter_map(|(index, layer)| layer.input(name).is_some().then_some(index)).collect();
    let mut edits = BTreeMap::<usize, BTreeMap<String, Value>>::new();
    match args {
        Some(args) => {
            let create = match args.get_path("create") {
                None | Some(Value::Bool(false)) => false,
                Some(Value::Bool(true)) => true,
                Some(_) => bail!("create must be true or false"),
            };
            if create && !origins.is_empty() {
                bail!("input `{name}` already exists; choose another name or edit that input");
            }
            if !create && origins.is_empty() {
                bail!("input `{name}` no longer exists; reload or use Add input");
            }
            let fallback = origins
                .last()
                .copied()
                .or_else(|| saved.layers.iter().rposition(|layer| layer.audio.contains_key("inputs")))
                .or_else(|| saved.layers.iter().position(|layer| layer.path == "project.toml"))
                .ok_or_else(|| anyhow!("project.toml is missing"))?;
            for (field, value) in save_fields(args)? {
                // Deep merging is per field: update the layer that actually owns that value.
                let owner = saved.field_owner(name, &field).unwrap_or(fallback);
                edits.entry(owner).or_default().insert(format!("{}.{}", saved.layers[owner].key(name), field), value);
            }
        }
        None => {
            if origins.is_empty() {
                bail!("input `{name}` is not configured");
            }
            let references = saved.references(name);
            if !references.is_empty() {
                bail!("cannot remove input `{name}`; update these references first: {}", references.join("; "));
            }
            // Remove every definition, not only the winning layer, so restarting cannot resurrect it.
            for owner in origins {
                edits.entry(owner).or_default().insert(saved.layers[owner].key(name), Value::Null);
            }
        }
    }
    let mut changes = Vec::new();
    let mut merged = toml::Table::new();
    for (index, layer) in saved.layers.iter().enumerate() {
        if let Some(set) = edits.remove(&index) {
            match apply(Some(&layer.text), &Value::map().with("set", Value::Map(set)))? {
                WriteOutcome::Write(text) => {
                    config::deep_merge(&mut merged, &audio_section(&layer.path, &text)?);
                    changes.push(Change { path: layer.path.clone(), before: layer.text.clone(), after: text });
                    continue;
                }
                WriteOutcome::Unchanged => {}
                WriteOutcome::Delete => unreachable!("input edits never delete a file"),
            }
        }
        config::deep_merge(&mut merged, &layer.audio);
    }
    let parsed = config::parse(&merged, &BTreeMap::new()).map_err(|e| anyhow!("audio configuration: {e}"))?;
    if let Some(delay) = args.and_then(|a| a.get_path("delay_ms")).and_then(Value::as_f64)
        && delay > parsed.max_delay_ms as f64
    {
        bail!("delay_ms exceeds audio.max_delay ({} ms)", parsed.max_delay_ms);
    }
    Ok(changes)
}

fn commit(project: &Project, changes: &[Change]) -> Result<Vec<String>> {
    for change in changes {
        super::check_path(&change.path)?;
        let current = std::fs::read_to_string(project.root().join(&change.path)).with_context(|| format!("read {}", change.path))?;
        if current != change.before {
            bail!("{} changed while editing; reload and try again", change.path);
        }
    }
    for (index, change) in changes.iter().enumerate() {
        if let Err(e) = project.write_file(&change.path, &change.after) {
            let mut rollback_errors = Vec::new();
            for previous in changes[..index].iter().rev() {
                if let Err(e) = project.write_file(&previous.path, &previous.before) {
                    rollback_errors.push(format!("{}: {e:#}", previous.path));
                }
            }
            if !rollback_errors.is_empty() {
                bail!("write {}: {e:#}; could not restore {}", change.path, rollback_errors.join("; "));
            }
            return Err(e).with_context(|| format!("write {} (prior edits restored)", change.path));
        }
    }
    Ok(changes.iter().map(|c| c.path.clone()).collect())
}

fn mutate(project: &Project, name: &str, args: Option<&Value>) -> Result<Vec<String>> {
    let saved = Saved::load(project);
    commit(project, &plan(&saved, name, args)?)
}

pub fn start(ctx: &Ctx) {
    let project = ctx.project.clone();
    ctx.hub.register_query(
        "project.audio.inputs",
        Arc::new(move |_, _| {
            let value = {
                let _guard = super::CONFIG_WRITE.lock();
                Saved::load(&project).query()
            };
            Box::pin(async move { Ok(value) })
        }),
    );
    let mut actions = ctx.hub.route_actions("project.audio.input");
    let ctx = ctx.clone();
    tokio::spawn(async move {
        while let Some(command) = actions.recv().await {
            let Op::Action { name: action, args } = &command.op else { continue };
            let result = (|| -> Result<(&str, Vec<String>)> {
                let name = requested_name(args)?;
                let save = match action.as_str() {
                    "project.audio.input.save" => Some(args),
                    "project.audio.input.remove" => None,
                    _ => bail!("unknown action {action}"),
                };
                let _guard = super::CONFIG_WRITE.lock();
                Ok((name, mutate(&ctx.project, name, save)?))
            })();
            match result {
                Ok((name, paths)) => {
                    if !paths.is_empty() {
                        crate::daemon::reload(&ctx, &paths);
                    }
                    ctx.hub.emit(Event::new("project.audio.inputs.changed", Origin::System, Value::map().with("name", name)));
                }
                Err(e) => {
                    let error = format!("{e:#}");
                    ctx.hub.log("error", "project", format!("{action}: {error}"));
                    ctx.hub.emit(Event::new(
                        "project.audio.input.failed",
                        Origin::System,
                        Value::map().with("name", args.get_path("name").cloned().unwrap_or(Value::Null)).with("action", action.clone()).with("error", error),
                    ));
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
        for (path, text) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let project = Project::open(dir.path()).unwrap();
        (dir, project)
    }

    fn input(target: &str) -> Value {
        Value::map().with("name", "band").with("target", target).with("channels", vec![1i64, 2]).with("bus", "band")
    }

    #[test]
    fn adding_editing_and_removing_final_input_survives_reopen() {
        let (dir, p) = project(&[("project.toml", "# empty show\n")]);
        assert_eq!(Saved::load(&p).query().get_path("inputs"), Some(&Value::List(vec![])));
        mutate(&p, "band", Some(&input("My interface").with("create", true))).unwrap();
        let reopened = Project::open(dir.path()).unwrap();
        assert_eq!(Saved::load(&reopened).query().get_path("inputs.0.target").and_then(Value::as_str), Some("My interface"));
        mutate(&reopened, "band", Some(&input("Other interface"))).unwrap();
        assert_eq!(Saved::load(&p).query().get_path("inputs.0.target").and_then(Value::as_str), Some("Other interface"));
        mutate(&p, "band", None).unwrap();
        let reopened = Project::open(dir.path()).unwrap();
        assert!(config::parse(&Saved::load(&reopened).merged, &BTreeMap::new()).unwrap().inputs.is_empty());
    }

    #[test]
    fn stale_create_and_edit_requests_cannot_overwrite_or_resurrect_an_input() {
        let (_dir, p) = project(&[("project.toml", "")]);
        mutate(&p, "band", Some(&input("First interface").with("create", true))).unwrap();
        assert!(mutate(&p, "band", Some(&input("Stale add").with("create", true))).is_err());
        assert_eq!(Saved::load(&p).query().get_path("inputs.0.target").and_then(Value::as_str), Some("First interface"));
        mutate(&p, "band", None).unwrap();
        assert!(mutate(&p, "band", Some(&input("Stale edit"))).is_err());
        assert!(config::parse(&Saved::load(&p).merged, &BTreeMap::new()).unwrap().inputs.is_empty());
    }

    #[test]
    fn merged_field_edits_keep_effects_and_comments_and_remove_all_layers() {
        let (_dir, p) = project(&[
            (
                "project.toml",
                "# show\n[audio.inputs.band]\ntarget = 'Lower' # source\nchannels = [1, 2]\nfx = [{ kind = 'svf' }]\n[audio.buses.music]\ngain = -3\n",
            ),
            ("audio/a.toml", "# early fragment\n[inputs.band]\ntarget = 'Upper' # selected source\ngain = -4.0\n"),
            ("audio/z.toml", "[inputs.band]\nbus = 'band'\nmute = true\n[buses.game]\ngain = -6\n"),
        ]);
        let saved = Saved::load(&p);
        assert_eq!(saved.origins("band").len(), 3);
        mutate(&p, "band", Some(&input("Selected").with("channels", vec![3i64, 4]).with("bus", "mic"))).unwrap();
        let base = std::fs::read_to_string(p.root().join("project.toml")).unwrap();
        let middle = std::fs::read_to_string(p.root().join("audio/a.toml")).unwrap();
        assert!(base.contains("target = 'Lower' # source"));
        assert!(middle.contains("# selected source"));
        let parsed = config::parse(&Saved::load(&p).merged, &BTreeMap::new()).unwrap();
        assert_eq!(parsed.inputs[0].target, "Selected");
        assert_eq!(parsed.inputs[0].channels, [3, 4]);
        assert_eq!(parsed.inputs[0].bus, "mic");
        assert_eq!(parsed.inputs[0].gain_db, -4.0);
        assert!(parsed.inputs[0].mute);
        assert_eq!(parsed.inputs[0].fx[0].kind, config::FxKind::Builtin("svf".into()));
        mutate(&p, "band", None).unwrap();
        let saved = Saved::load(&Project::open(p.root()).unwrap());
        assert!(saved.origins("band").is_empty());
        assert!(config::parse(&saved.merged, &BTreeMap::new()).unwrap().inputs.is_empty());
        assert_eq!(at(&saved.merged, &["buses", "music", "gain"]).and_then(toml::Value::as_integer), Some(-3));
        assert_eq!(at(&saved.merged, &["buses", "game", "gain"]).and_then(toml::Value::as_integer), Some(-6));
    }

    #[test]
    fn replaced_tables_do_not_own_effective_input_fields() {
        let (_dir, p) = project(&[
            ("project.toml", "[audio.inputs.band]\ntarget = 'Lower'\ngain = -4.0\n"),
            ("audio/a.toml", "inputs = 'replaced by a later table'\n"),
            ("audio/z.toml", "[inputs.band]\ntarget = 'Effective'\n"),
        ]);
        mutate(&p, "band", Some(&input("Selected").with("gain", -8.0))).unwrap();
        let saved = Saved::load(&p);
        assert_eq!(saved.layers[0].input("band").unwrap().get("gain").and_then(toml::Value::as_float), Some(-4.0));
        assert_eq!(config::parse(&saved.merged, &BTreeMap::new()).unwrap().inputs[0].gain_db, -8.0);
    }

    #[test]
    fn referenced_inputs_cannot_be_removed_or_lose_required_channels() {
        let (_dir, p) = project(&[
            ("project.toml", "[audio.inputs.band]\ntarget = 'Interface'\n[audio.analysis]\nmic = 'band'\n"),
            ("audio/routes.toml", "[monitor]\ntarget = 'Headphones'\ninputs = ['band']\n[drums.pads.snare]\ninput = 'band'\nchannel = 1\n"),
        ]);
        let before = std::fs::read_to_string(p.root().join("project.toml")).unwrap();
        let refs = Saved::load(&p).references("band");
        assert_eq!(refs, ["project.toml: audio.analysis.mic", "audio/routes.toml: monitor.inputs", "audio/routes.toml: drums.pads.snare.input"]);
        assert!(mutate(&p, "band", None).is_err());
        assert!(mutate(&p, "band", Some(&input("Interface").with("channels", vec![1i64]))).is_err());
        assert_eq!(std::fs::read_to_string(p.root().join("project.toml")).unwrap(), before);
    }

    #[test]
    fn malformed_saved_input_is_visible_without_invented_hardware() {
        let (_dir, p) = project(&[("project.toml", "[audio.inputs.band]\nbus = 'band'\n")]);
        let value = Saved::load(&p).query();
        assert_eq!(value.get_path("inputs.0.name").and_then(Value::as_str), Some("band"));
        assert_eq!(value.get_path("inputs.0.target"), Some(&Value::Null));
        assert!(value.get_path("errors.0").and_then(Value::as_str).unwrap().contains("target"));
        mutate(&p, "band", Some(&input("My selected interface"))).unwrap();
        assert_eq!(Saved::load(&p).query().get_path("errors"), Some(&Value::List(vec![])));
    }

    #[test]
    fn invalid_channels_destinations_and_names_never_write() {
        let (_dir, p) = project(&[("project.toml", "# empty\n")]);
        for args in [
            input(""),
            input("Interface").with("bus", "program"),
            input("Interface").with("channels", vec![1i64, 1]),
            input("Interface").with("channels", vec![1.5]),
            input("Interface").with("channels", vec![0i64]),
            input("Interface").with("delay_ms", 10001.0),
            input("Interface").with("mute", "yes"),
        ] {
            assert!(mutate(&p, "band", Some(&args.with("create", true))).is_err());
        }
        assert!(requested_name(&input("Interface").with("name", "band.left")).is_err());
        assert_eq!(std::fs::read_to_string(p.root().join("project.toml")).unwrap(), "# empty\n");
    }

    #[test]
    fn only_explicit_bus_intent_is_marked_configured() {
        let audio: toml::Table = "[sounds.alert]\nfile = 'alert.wav'\n[sources.synth]\nbus = 'music'\n".parse().unwrap();
        let buses = bus_intent(&audio, None);
        let configured = |name: &str| buses.iter().find(|b| b.get_path("name").and_then(Value::as_str) == Some(name)).unwrap().get_path("configured").unwrap();
        assert_eq!(configured("music"), &Value::Bool(true));
        assert_eq!(configured("sfx"), &Value::Bool(true));
        assert_eq!(configured("band"), &Value::Bool(false));
        assert_eq!(configured("game"), &Value::Bool(false));
    }
}
