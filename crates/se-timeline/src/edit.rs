//! Timeline file edits with `toml_edit` (comments and formatting preserved): record-mode
//! commits and the editor's cue/key/region/track operations.
//!
//! Indices in edit requests refer to the time-sorted lists the core reports (query
//! `timelines`), so every operation maps them onto the file's own order first.

use anyhow::{Context, Result, anyhow, bail};
use se_clock::timecode::{FrameRate, Timecode};
use se_core::timeline::{SourceDef, TimelineDef, format_time, parse_time_str, parse_time_value};
use se_proto::Value;
use se_store::project::to_edit_value;
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, TableLike};

/// How a time is written back: timecode labels for MTC/LTC timelines, `m:ss.mmm` otherwise.
#[derive(Clone, Copy, Debug)]
pub struct TimeFmt {
    pub rate: FrameRate,
    pub timecode: bool,
}

impl TimeFmt {
    pub fn of(def: &TimelineDef) -> Self {
        TimeFmt { rate: def.rate, timecode: def.source.is_timecode() }
    }
    pub fn text(&self, secs: f64) -> String {
        if self.timecode { Timecode::from_frames((secs.max(0.0) * self.rate.fps()).round() as u64, self.rate).to_string() } else { format_time(secs) }
    }
    fn parse(&self, v: &toml_edit::Value) -> Option<f64> {
        match v {
            toml_edit::Value::String(s) => parse_time_str(s.value(), self.rate).ok(),
            toml_edit::Value::Integer(i) => Some(*i.value() as f64 / 1000.0),
            toml_edit::Value::Float(f) => Some(*f.value() / 1000.0),
            _ => None,
        }
    }
}

/// The items of one track (`cues`, `keys`, or `regions`), inline array or `[[…]]` tables.
enum Items<'a> {
    Inline(&'a mut Array),
    Tables(&'a mut ArrayOfTables),
}

impl Items<'_> {
    fn len(&self) -> usize {
        match self {
            Items::Inline(a) => a.len(),
            Items::Tables(t) => t.len(),
        }
    }
    fn get(&self, i: usize) -> Option<&dyn TableLike> {
        match self {
            Items::Inline(a) => a.get(i).and_then(|v| v.as_inline_table()).map(|t| t as &dyn TableLike),
            Items::Tables(t) => t.get(i).map(|t| t as &dyn TableLike),
        }
    }
    fn get_mut(&mut self, i: usize) -> Option<&mut dyn TableLike> {
        match self {
            Items::Inline(a) => a.get_mut(i).and_then(|v| v.as_inline_table_mut()).map(|t| t as &mut dyn TableLike),
            Items::Tables(t) => t.get_mut(i).map(|t| t as &mut dyn TableLike),
        }
    }
    fn remove(&mut self, i: usize) {
        match self {
            Items::Inline(a) => {
                a.remove(i);
            }
            Items::Tables(t) => {
                t.remove(i);
            }
        }
    }
    fn push(&mut self, fields: Vec<(&str, toml_edit::Value)>) {
        match self {
            Items::Inline(a) => {
                let multiline = a.is_empty() || a.get(0).is_some_and(|v| v.decor().prefix().and_then(|p| p.as_str()).is_some_and(|p| p.contains('\n')));
                let mut t = InlineTable::new();
                for (k, v) in fields {
                    t.insert(k, v);
                }
                let mut v = toml_edit::Value::InlineTable(t);
                v.decor_mut().set_prefix(if multiline { "\n  " } else { " " });
                a.push_formatted(v);
                if multiline {
                    a.set_trailing_comma(true);
                    a.set_trailing("\n");
                }
            }
            Items::Tables(t) => {
                let mut tab = Table::new();
                for (k, v) in fields {
                    tab.insert(k, Item::Value(v));
                }
                t.push(tab);
            }
        }
    }
    /// Stable sort by `field` (time); returns nothing, keeps each entry's comments/decor.
    fn sort_by_time(&mut self, field: &str, fmt: &TimeFmt) {
        let key = |t: &dyn TableLike| t.get(field).and_then(|i| i.as_value()).and_then(|v| fmt.parse(v)).unwrap_or(f64::MAX);
        match self {
            Items::Inline(a) => {
                let mut vals: Vec<toml_edit::Value> = a.iter().cloned().collect();
                vals.sort_by(|x, y| {
                    let kx = x.as_inline_table().map(|t| key(t)).unwrap_or(f64::MAX);
                    let ky = y.as_inline_table().map(|t| key(t)).unwrap_or(f64::MAX);
                    kx.total_cmp(&ky)
                });
                let (trail, comma) = (a.trailing().clone(), a.trailing_comma());
                a.clear();
                for v in vals {
                    a.push_formatted(v);
                }
                a.set_trailing(trail);
                a.set_trailing_comma(comma);
            }
            Items::Tables(t) => {
                let mut tabs: Vec<Table> = t.iter().cloned().collect();
                tabs.sort_by(|x, y| key(x).total_cmp(&key(y)));
                t.clear();
                for tab in tabs {
                    t.push(tab);
                }
            }
        }
    }
    /// File positions in time order (the core's indices).
    fn order(&self, field: &str, fmt: &TimeFmt) -> Vec<usize> {
        let mut idx: Vec<(f64, usize)> = (0..self.len())
            .map(|i| (self.get(i).and_then(|t| t.get(field)).and_then(|x| x.as_value()).and_then(|v| fmt.parse(v)).unwrap_or(f64::MAX), i))
            .collect();
        idx.sort_by(|a, b| a.0.total_cmp(&b.0));
        idx.into_iter().map(|(_, i)| i).collect()
    }
}

fn items_in<'a>(t: &'a mut dyn TableLike, key: &str) -> Result<Items<'a>> {
    if t.get(key).is_none() {
        let mut a = Array::new();
        a.set_trailing("\n");
        t.insert(key, Item::Value(toml_edit::Value::Array(a)));
    }
    match t.get_mut(key) {
        Some(Item::Value(toml_edit::Value::Array(a))) => Ok(Items::Inline(a)),
        Some(Item::ArrayOfTables(a)) => Ok(Items::Tables(a)),
        _ => bail!("`{key}` is not an array"),
    }
}

/// Which kind of items a track holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Cues,
    Keys,
    Regions,
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Kind::Cues => "cues",
            Kind::Keys => "keys",
            Kind::Regions => "regions",
        }
    }
    fn time_field(self) -> &'static str {
        match self {
            Kind::Regions => "start",
            _ => "at",
        }
    }
    fn type_name(self) -> &'static str {
        match self {
            Kind::Cues => "cues",
            Kind::Keys => "automation",
            Kind::Regions => "regions",
        }
    }
}

fn track_name(t: &dyn TableLike, i: usize) -> String {
    t.get("name").and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| format!("track{}", i + 1))
}

/// Find a track table by name (`cues` = the top-level `cues` list when present).
fn track<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut dyn TableLike> {
    if name == "cues" && doc.contains_key("cues") {
        return Ok(doc.as_table_mut() as &mut dyn TableLike);
    }
    let Some(item) = doc.get_mut("track") else { bail!("no track `{name}`") };
    match item {
        Item::ArrayOfTables(a) => {
            let i = a.iter().enumerate().position(|(i, t)| track_name(t, i) == name).ok_or_else(|| anyhow!("no track `{name}`"))?;
            Ok(a.get_mut(i).unwrap() as &mut dyn TableLike)
        }
        Item::Value(toml_edit::Value::Array(a)) => {
            let i = a
                .iter()
                .enumerate()
                .position(|(i, v)| v.as_inline_table().is_some_and(|t| track_name(t, i) == name))
                .ok_or_else(|| anyhow!("no track `{name}`"))?;
            Ok(a.get_mut(i).and_then(|v| v.as_inline_table_mut()).unwrap() as &mut dyn TableLike)
        }
        _ => bail!("`track` must be an array of tables"),
    }
}

fn track_count(doc: &DocumentMut) -> usize {
    match doc.get("track") {
        Some(Item::ArrayOfTables(a)) => a.len(),
        Some(Item::Value(toml_edit::Value::Array(a))) => a.len(),
        _ => 0,
    }
}

/// Append a new `[[track]]`.
pub fn add_track(doc: &mut DocumentMut, name: &str, kind: Kind, address: Option<&str>) -> Result<()> {
    if track(doc, name).is_ok() {
        bail!("track `{name}` already exists");
    }
    let mut t = Table::new();
    t.insert("name", toml_edit::value(name));
    t.insert("type", toml_edit::value(kind.type_name()));
    if kind == Kind::Keys {
        let a = address.ok_or_else(|| anyhow!("an automation track needs `address`"))?;
        if !se_proto::address::is_valid(a, true) {
            bail!("bad address `{a}`");
        }
        t.insert("address", toml_edit::value(a));
    }
    let mut arr = Array::new();
    arr.set_trailing("\n");
    t.insert(kind.key(), Item::Value(toml_edit::Value::Array(arr)));
    match doc.get_mut("track") {
        None => {
            let mut a = ArrayOfTables::new();
            a.push(t);
            doc.insert("track", Item::ArrayOfTables(a));
        }
        Some(Item::ArrayOfTables(a)) => a.push(t),
        Some(_) => bail!("`track` must be an array of tables"),
    }
    Ok(())
}

pub fn remove_track(doc: &mut DocumentMut, name: &str) -> Result<()> {
    if name == "cues" && doc.contains_key("cues") {
        doc.remove("cues");
        return Ok(());
    }
    match doc.get_mut("track") {
        Some(Item::ArrayOfTables(a)) => {
            let i = a.iter().enumerate().position(|(i, t)| track_name(t, i) == name).ok_or_else(|| anyhow!("no track `{name}`"))?;
            a.remove(i);
            if a.is_empty() {
                doc.remove("track");
            }
            Ok(())
        }
        _ => bail!("no track `{name}`"),
    }
}

pub fn set_mute(doc: &mut DocumentMut, name: &str, mute: bool) -> Result<()> {
    let t = track(doc, name)?;
    if mute {
        t.insert("mute", Item::Value(true.into()));
    } else {
        t.remove("mute");
    }
    Ok(())
}

fn val(v: &Value) -> Result<toml_edit::Value> {
    to_edit_value(v).ok_or_else(|| anyhow!("value must not be null"))
}

fn commands_value(v: &Value) -> Result<toml_edit::Value> {
    let list: Vec<String> = match v {
        Value::Str(s) => vec![s.clone()],
        Value::List(l) => l.iter().map(|x| x.as_str().map(String::from).ok_or_else(|| anyhow!("commands must be strings"))).collect::<Result<_>>()?,
        Value::Null => Vec::new(),
        _ => bail!("`do` must be a string or a list of strings"),
    };
    for c in &list {
        se_proto::Op::parse(c).map_err(|e| anyhow!("command `{c}`: {e}"))?;
    }
    let mut a = Array::new();
    for c in list {
        a.push(c);
    }
    Ok(toml_edit::Value::Array(a))
}

fn time_arg(args: &Value, k: &str, fmt: &TimeFmt) -> Result<Option<f64>> {
    args.get_path(k).filter(|v| !v.is_null()).map(|v| parse_time_value(v, fmt.rate).map_err(|e| anyhow!("`{k}`: {e}"))).transpose()
}

/// One editor operation. `op` = `cue.add|cue.set|cue.remove|key.add|key.set|key.remove|
/// region.add|region.set|region.remove`; `args` as documented in docs/timelines.md.
pub fn apply_item_op(doc: &mut DocumentMut, fmt: &TimeFmt, op: &str, args: &Value) -> Result<()> {
    let (kind, verb) = match op.split_once('.') {
        Some(("cue", v)) => (Kind::Cues, v),
        Some(("key", v)) => (Kind::Keys, v),
        Some(("region", v)) => (Kind::Regions, v),
        _ => bail!("unknown edit `{op}`"),
    };
    let name = args.get_path("track").and_then(Value::as_str).unwrap_or(if kind == Kind::Cues { "cues" } else { "" }).to_string();
    if name.is_empty() {
        bail!("`{op}` needs `track`");
    }
    if kind == Kind::Cues && name == "cues" && !doc.contains_key("cues") && track_count(doc) == 0 {
        // first cue of a fresh file: the PLAN §7 shorthand
        doc.insert("cues", Item::Value(toml_edit::Value::Array(Array::new())));
    }
    let t = track(doc, &name)?;
    let mut items = items_in(t, kind.key())?;
    let tf = kind.time_field();
    let index = |items: &Items| -> Result<usize> {
        let i = args.get_path("index").and_then(Value::as_i64).ok_or_else(|| anyhow!("`{op}` needs `index`"))?;
        items.order(tf, fmt).get(i as usize).copied().ok_or_else(|| anyhow!("index {i} out of range"))
    };
    match verb {
        "add" => {
            let mut fields: Vec<(&str, toml_edit::Value)> = Vec::new();
            match kind {
                Kind::Cues => {
                    let at = time_arg(args, "at", fmt)?.ok_or_else(|| anyhow!("needs `at`"))?;
                    fields.push(("at", fmt.text(at).into()));
                    fields.push(("do", commands_value(args.get_path("do").unwrap_or(&Value::Null))?));
                    if let Some(l) = args.get_path("label").and_then(Value::as_str).filter(|l| !l.is_empty()) {
                        fields.push(("label", l.into()));
                    }
                }
                Kind::Keys => {
                    let at = time_arg(args, "at", fmt)?.ok_or_else(|| anyhow!("needs `at`"))?;
                    fields.push(("at", fmt.text(at).into()));
                    fields.push(("value", val(args.get_path("value").ok_or_else(|| anyhow!("needs `value`"))?)?));
                    if let Some(c) = args.get_path("curve").and_then(Value::as_str) {
                        se_proto::Ease::parse(c).ok_or_else(|| anyhow!("unknown curve `{c}`"))?;
                        fields.push(("curve", c.into()));
                    }
                }
                Kind::Regions => {
                    let s = time_arg(args, "start", fmt)?.ok_or_else(|| anyhow!("needs `start`"))?;
                    let e = time_arg(args, "end", fmt)?.ok_or_else(|| anyhow!("needs `end`"))?;
                    if e <= s {
                        bail!("region end must be after start");
                    }
                    fields.push(("start", fmt.text(s).into()));
                    fields.push(("end", fmt.text(e).into()));
                    if let Some(l) = args.get_path("label").and_then(Value::as_str).filter(|l| !l.is_empty()) {
                        fields.push(("label", l.into()));
                    }
                    let mut any = false;
                    for k in ["preset", "cuelist", "cue", "fx"] {
                        if let Some(v) = args.get_path(k).and_then(Value::as_str).filter(|v| !v.is_empty()) {
                            fields.push((k, v.into()));
                            any |= k != "cue";
                        }
                    }
                    for k in ["do", "undo"] {
                        if let Some(v) = args.get_path(k) {
                            fields.push((k, commands_value(v)?));
                            any = true;
                        }
                    }
                    if !any {
                        bail!("a region needs `preset`, `cuelist`, `fx`, or `do`/`undo`");
                    }
                }
            }
            items.push(fields);
        }
        "set" => {
            let i = index(&items)?;
            let e = items.get_mut(i).ok_or_else(|| anyhow!("entry {i} is not a table"))?;
            for k in ["at", "start", "end"] {
                if let Some(t) = time_arg(args, k, fmt)? {
                    e.insert(k, Item::Value(fmt.text(t).into()));
                }
            }
            if let Some(v) = args.get_path("value") {
                e.insert("value", Item::Value(val(v)?));
            }
            if let Some(v) = args.get_path("do") {
                e.insert("do", Item::Value(commands_value(v)?));
            }
            if let Some(c) = args.get_path("curve").and_then(Value::as_str) {
                se_proto::Ease::parse(c).ok_or_else(|| anyhow!("unknown curve `{c}`"))?;
                e.insert("curve", Item::Value(c.into()));
            }
            match args.get_path("label") {
                Some(Value::Str(l)) if !l.is_empty() => {
                    e.insert("label", Item::Value(l.as_str().into()));
                }
                Some(Value::Str(_)) | Some(Value::Null) => {
                    e.remove("label");
                }
                _ => {}
            }
        }
        "remove" => {
            let i = index(&items)?;
            items.remove(i);
        }
        _ => bail!("unknown edit `{op}`"),
    }
    items.sort_by_time(tf, fmt);
    Ok(())
}

/// Result of writing a recording.
#[derive(Debug, Default, PartialEq)]
pub struct Committed {
    pub cues: usize,
    pub keys: usize,
}

/// Write a `timeline.record.commit` payload: cues appended to the record track (snapped by
/// `snap`), keyframes replacing each lane's keys inside the recorded span (punch-in).
pub fn commit_recording(doc: &mut DocumentMut, fmt: &TimeFmt, payload: &Value, snap: impl Fn(f64) -> f64) -> Result<Committed> {
    let from = payload.get_path("from").and_then(Value::as_f64).unwrap_or(0.0);
    let to = payload.get_path("to").and_then(Value::as_f64).unwrap_or(f64::MAX);
    let track_name = payload.get_path("track").and_then(Value::as_str).unwrap_or("recorded").to_string();
    let mut done = Committed::default();
    let cues = payload.get_path("cues").and_then(Value::as_list).unwrap_or_default();
    if !cues.is_empty() {
        if track(doc, &track_name).is_err() {
            add_track(doc, &track_name, Kind::Cues, None)?;
        }
        let t = track(doc, &track_name)?;
        let mut items = items_in(t, "cues")?;
        for c in cues {
            let at = snap(c.get_path("at").and_then(Value::as_f64).context("cue without `at`")?);
            let mut fields: Vec<(&str, toml_edit::Value)> =
                vec![("at", fmt.text(at).into()), ("do", commands_value(c.get_path("do").unwrap_or(&Value::Null))?)];
            if let Some(l) = c.get_path("label").and_then(Value::as_str).filter(|l| !l.is_empty()) {
                fields.push(("label", l.into()));
            }
            items.push(fields);
            done.cues += 1;
        }
        items.sort_by_time("at", fmt);
    }
    for lane in payload.get_path("lanes").and_then(Value::as_list).unwrap_or_default() {
        let addr = lane.get_path("address").and_then(Value::as_str).context("lane without address")?.to_string();
        let keys = lane.get_path("keys").and_then(Value::as_list).unwrap_or_default();
        if keys.is_empty() {
            continue;
        }
        let existing = lane_track(doc, &addr);
        let name = match existing {
            Some(n) => n,
            None => {
                let mut n = addr.clone();
                let mut k = 2;
                while track(doc, &n).is_ok() {
                    n = format!("{addr} {k}");
                    k += 1;
                }
                add_track(doc, &n, Kind::Keys, Some(&addr))?;
                n
            }
        };
        let t = track(doc, &name)?;
        let mut items = items_in(t, "keys")?;
        // punch in: drop old keys inside the recorded span
        let mut i = 0;
        while i < items.len() {
            let at = items.get(i).and_then(|e| e.get("at")).and_then(|x| x.as_value()).and_then(|v| fmt.parse(v));
            if at.is_some_and(|a| a >= from - 1e-3 && a <= to + 1e-3) {
                items.remove(i);
            } else {
                i += 1;
            }
        }
        for k in keys {
            let at = k.get_path("at").and_then(Value::as_f64).context("key without `at`")?;
            items.push(vec![("at", fmt.text(at).into()), ("value", val(k.get_path("value").unwrap_or(&Value::Null))?)]);
            done.keys += 1;
        }
        items.sort_by_time("at", fmt);
    }
    Ok(done)
}

/// Name of the automation track driving `address`, if any.
fn lane_track(doc: &DocumentMut, address: &str) -> Option<String> {
    match doc.get("track") {
        Some(Item::ArrayOfTables(a)) => {
            a.iter().enumerate().find(|(_, t)| t.get("address").and_then(|v| v.as_str()) == Some(address)).map(|(i, t)| track_name(t, i))
        }
        _ => None,
    }
}

/// Text of a new timeline file.
pub fn new_file(name: &str, source: &str, fps: Option<&str>, length: Option<f64>) -> Result<String> {
    let src = SourceDef::parse(source).map_err(|e| anyhow!(e))?;
    let rate = match fps {
        Some(f) => FrameRate::parse(f).ok_or_else(|| anyhow!("bad fps `{f}`"))?,
        None => FrameRate::Fps30,
    };
    let mut s = format!("# Timeline `{name}` — cues, automation lanes and regions on one clock (docs/timelines.md).\n");
    match &src {
        SourceDef::Media(id) => s.push_str(&format!("media = \"{id}\"\n")),
        other => s.push_str(&format!("source = \"{}\"\n", other.key())),
    }
    s.push_str(&format!("fps = \"{}\"\n", rate.as_str()));
    if let Some(l) = length {
        s.push_str(&format!("length = \"{}\"\n", format_time(l)));
    }
    s.push_str("\ncues = []\n");
    // validate what we wrote
    let table: toml::Table = s.parse().context("template")?;
    TimelineDef::parse(name, "new", &table).map_err(|e| anyhow!(e))?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_core::timeline::TrackKind;

    const FILE: &str = r#"# Nightly song — chorus hits
media = "yt:VIDEO_ID" # the official upload

cues = [
  { at = "1:43.000", do = ["lights.cue blackout"] }, # drop
  { at = "1:12.400", do = ["preset.fire chorus_blast"], label = "chorus" },
]

# front wash follows the song
[[track]]
name = "front"
type = "automation"
address = "lights.group.front.master"
keys = [
  { at = "0:00.000", value = 0.0 },
  { at = "0:10.000", value = 1.0 },
  { at = "0:30.000", value = 0.5 },
]
"#;

    fn parsed(doc: &DocumentMut) -> TimelineDef {
        TimelineDef::parse("n", "f", &doc.to_string().parse().unwrap()).unwrap()
    }

    fn fmt() -> TimeFmt {
        TimeFmt { rate: FrameRate::Fps30, timecode: false }
    }

    #[test]
    fn commit_appends_cues_and_punches_keys_preserving_comments() {
        let mut doc: DocumentMut = FILE.parse().unwrap();
        let payload = Value::map()
            .with("track", "recorded")
            .with("from", 5.0)
            .with("to", 20.0)
            .with("cues", vec![Value::map().with("at", 6.02).with("do", vec!["preset.fire hype"]).with("label", Value::Null)])
            .with(
                "lanes",
                vec![
                    Value::map()
                        .with("address", "lights.group.front.master")
                        .with("keys", vec![Value::map().with("at", 8.0).with("value", 0.2), Value::map().with("at", 12.0).with("value", 0.9)]),
                ],
            );
        let done = commit_recording(&mut doc, &fmt(), &payload, |t| (t * 2.0).round() / 2.0).unwrap();
        assert_eq!(done, Committed { cues: 1, keys: 2 });
        let text = doc.to_string();
        for c in ["# Nightly song — chorus hits", "# the official upload", "# drop", "# front wash follows the song"] {
            assert!(text.contains(c), "comment `{c}` kept:\n{text}");
        }
        let d = parsed(&doc);
        let rec = d.tracks.iter().find(|t| t.name == "recorded").unwrap();
        let TrackKind::Cues { cues } = &rec.kind else { panic!() };
        assert_eq!(cues[0].at, 6.0, "snapped");
        assert_eq!(cues[0].commands, vec!["preset.fire hype"]);
        let front = d.tracks.iter().find(|t| t.name == "front").unwrap();
        let TrackKind::Automation { lane } = &front.kind else { panic!() };
        let ats: Vec<f64> = lane.keys.iter().map(|k| k.at).collect();
        assert_eq!(ats, vec![0.0, 8.0, 12.0, 30.0], "10 s key punched out, recorded keys in");
    }

    #[test]
    fn commit_creates_lane_tracks() {
        let mut doc: DocumentMut = "source = \"mtc\"\nfps = \"25\"\n".parse().unwrap();
        let payload = Value::map()
            .with("from", 0.0)
            .with("to", 10.0)
            .with("lanes", vec![Value::map().with("address", "mixer.16r.ch.3.fader").with("keys", vec![Value::map().with("at", 3600.48).with("value", 0.75)])]);
        let f = TimeFmt { rate: FrameRate::Fps25, timecode: true };
        commit_recording(&mut doc, &f, &payload, |t| t).unwrap();
        assert!(doc.to_string().contains("at = \"01:00:00:12\""), "{doc}");
        let d = parsed(&doc);
        assert!(matches!(&d.tracks[0].kind, TrackKind::Automation { lane } if lane.address == "mixer.16r.ch.3.fader"));
    }

    #[test]
    fn editor_ops_use_time_sorted_indices() {
        let mut doc: DocumentMut = FILE.parse().unwrap();
        // index 0 in the core's (sorted) list is the chorus at 1:12.4 — second in the file
        apply_item_op(&mut doc, &fmt(), "cue.set", &Value::map().with("index", 0).with("at", 70.0).with("label", "early chorus")).unwrap();
        let d = parsed(&doc);
        let TrackKind::Cues { cues } = &d.tracks[0].kind else { panic!() };
        assert_eq!((cues[0].at, cues[0].label.as_deref()), (70.0, Some("early chorus")));
        assert_eq!(cues[1].at, 103.0);
        apply_item_op(&mut doc, &fmt(), "cue.add", &Value::map().with("at", "0:30.5").with("do", vec!["scene.cut wide"])).unwrap();
        apply_item_op(&mut doc, &fmt(), "cue.remove", &Value::map().with("index", 2)).unwrap();
        let d = parsed(&doc);
        let TrackKind::Cues { cues } = &d.tracks[0].kind else { panic!() };
        assert_eq!(cues.iter().map(|c| c.at).collect::<Vec<_>>(), vec![30.5, 70.0]);
        assert!(doc.to_string().contains("# the official upload"));
        apply_item_op(&mut doc, &fmt(), "key.set", &Value::map().with("track", "front").with("index", 1).with("value", 0.8).with("curve", "smoothstep"))
            .unwrap();
        apply_item_op(&mut doc, &fmt(), "key.add", &Value::map().with("track", "front").with("at", 20.0).with("value", 0.6)).unwrap();
        let d = parsed(&doc);
        let TrackKind::Automation { lane } = &d.tracks[1].kind else { panic!() };
        assert_eq!(lane.keys[1].value, Value::Float(0.8));
        assert_eq!(lane.keys[1].curve, se_proto::Ease::Smoothstep);
        assert_eq!(lane.keys[2].at, 20.0);
        assert!(apply_item_op(&mut doc, &fmt(), "cue.add", &Value::map().with("at", 1.0).with("do", vec![""])).is_err(), "commands are validated");
        assert!(apply_item_op(&mut doc, &fmt(), "cue.set", &Value::map().with("index", 99)).is_err());
        assert!(apply_item_op(&mut doc, &fmt(), "key.add", &Value::map().with("track", "nope").with("at", 1.0).with("value", 1)).is_err());
    }

    #[test]
    fn tracks_and_regions() {
        let mut doc: DocumentMut = "length = \"2:00\"\n".parse().unwrap();
        add_track(&mut doc, "wash", Kind::Regions, None).unwrap();
        apply_item_op(
            &mut doc,
            &fmt(),
            "region.add",
            &Value::map().with("track", "wash").with("start", 10.0).with("end", 20.0).with("preset", "wash").with("label", "verse"),
        )
        .unwrap();
        assert!(
            apply_item_op(&mut doc, &fmt(), "region.add", &Value::map().with("track", "wash").with("start", 30.0).with("end", 20.0).with("fx", "strobe"))
                .is_err()
        );
        apply_item_op(&mut doc, &fmt(), "region.set", &Value::map().with("track", "wash").with("index", 0).with("end", 25.0)).unwrap();
        set_mute(&mut doc, "wash", true).unwrap();
        let d = parsed(&doc);
        assert!(d.tracks[0].mute);
        let TrackKind::Regions { regions } = &d.tracks[0].kind else { panic!() };
        assert_eq!((regions[0].start, regions[0].end), (10.0, 25.0));
        remove_track(&mut doc, "wash").unwrap();
        assert!(parsed(&doc).tracks.is_empty());
        assert!(new_file("fresh", "mtc:studio24c", Some("25"), Some(300.0)).unwrap().contains("source = \"mtc:studio24c\""));
        assert!(new_file("bad", "spotify:x", None, None).is_err());
    }
}
