use anyhow::{Result, anyhow, bail};
use se_core::config::{FxRef, normalize_fx, valid_fx_id};
use se_proto::Value;
use toml_edit::{DocumentMut, Item, Table, TableLike};

#[derive(Clone, Debug)]
pub struct Target { pub kind: String, pub scene: String, pub node: String, pub canvas: String, pub name: String, pub group: String }
impl Target {
    pub fn parse(v: &Value) -> Result<Self> {
        let get = |key: &str| v.get_path(key).and_then(Value::as_str).unwrap_or("").to_string();
        let t = Self { kind: get("kind"), scene: get("scene"), node: get("node"), canvas: get("canvas"), name: get("name"), group: get("group") };
        let required: &[&str] = match t.kind.as_str() { "node" => &[&t.scene,&t.node], "scene" => &[&t.scene], "layout" => &[&t.scene,&t.canvas], "group" => &[&t.scene,&t.canvas,&t.group], "source" => &[&t.name], "canvas"|"output" => &[&t.canvas], _ => bail!("unknown FX target kind") };
        if required.iter().any(|s| !valid_fx_id(s)) { bail!("invalid FX target identifier"); }
        if [&t.scene, &t.canvas, &t.group].iter().any(|s| s.contains('.')) { bail!("invalid FX target identifier"); }
        Ok(t)
    }
    pub fn path(&self) -> String { match self.kind.as_str() { "source" => format!("sources/{}.toml",self.name), "canvas"|"output" => "project.toml".into(), _ => format!("scenes/{}.toml",self.scene) } }
    pub fn chain_key(&self) -> String { if self.kind == "canvas" || self.kind == "output" { format!("{}_fx",self.kind) } else { "fx".into() } }
}

pub fn set(t: &mut dyn TableLike, key: &str, v: &Value) -> Result<()> {
    match crate::project::to_edit_value(v) {
        Some(mut value) => {
            if let Some(Item::Value(old)) = t.get_mut(key) {
                *value.decor_mut() = old.decor().clone();
                *old = value;
            } else {
                t.insert(key, Item::Value(value));
            }
        }
        None => {
            t.remove(key);
        }
    }
    Ok(())
}
fn child<'a>(t: &'a mut dyn TableLike, key: &str) -> Result<&'a mut dyn TableLike> {
    t.get_mut(key).and_then(Item::as_table_like_mut).ok_or_else(|| anyhow!("missing table `{key}`"))
}
pub fn visit_entries(item: &mut Item, mut f: impl FnMut(&mut dyn TableLike) -> Result<()>) -> Result<()> {
    match item {
        Item::ArrayOfTables(a) => for t in a.iter_mut() { f(t)?; },
        Item::Value(toml_edit::Value::Array(a)) => for v in a.iter_mut() { f(v.as_inline_table_mut().ok_or_else(|| anyhow!("expected table entry"))?)?; },
        _ => bail!("expected table array"),
    }
    Ok(())
}
pub fn visit(doc: &mut DocumentMut, target: &Target, mut f: impl FnMut(&mut dyn TableLike, &str) -> Result<()>) -> Result<()> {
    if target.kind == "source" || target.kind == "scene" { return f(doc.as_table_mut(), "fx"); }
    if target.kind == "canvas" || target.kind == "output" {
        let key = target.chain_key();
        if doc.get("render").is_none() { doc.insert("render",Item::Table(Table::new())); }
        let render = child(doc.as_table_mut(),"render")?;
        if render.get(&key).is_none() { render.insert(&key, Item::Table(Table::new())); }
        return f(child(render, &key)?, &target.canvas);
    }
    let canvases = child(doc.as_table_mut(), "canvas")?;
    let mut found = 0;
    for (name, item) in canvases.iter_mut() {
        if target.kind != "node" && name.get() != target.canvas { continue; }
        let Some(layout) = item.as_table_like_mut() else { continue };
        if target.kind == "layout" { f(layout,"fx")?; found += 1; continue; }
        let key = if target.kind == "node" { "nodes" } else { "groups" };
        if let Some(entries) = layout.get_mut(key) {
            visit_entries(entries, |t| {
                let id = t.get("id").and_then(Item::as_str).or_else(|| t.get("src").and_then(Item::as_str));
                let want = if target.kind == "node" { &target.node } else { &target.group };
                if id == Some(want.as_str()) { f(t,"fx")?; found += 1; }
                Ok(())
            })?;
        }
    }
    if found == 0 { bail!("FX target does not exist"); }
    Ok(())
}

pub fn read_chain(t: &dyn TableLike, key: &str) -> Result<Vec<FxRef>> {
    let Some(item) = t.get(key) else { return Ok(Vec::new()) };
    let mut doc = DocumentMut::new();
    doc.insert("fx", item.clone());
    let table: toml::Table = doc.to_string().parse()?;
    let mut fx: Vec<FxRef> = table.get("fx").ok_or_else(|| anyhow!("invalid FX array"))?.clone().try_into()?;
    normalize_fx(&mut fx).map_err(|e| anyhow!(e))?;
    Ok(fx)
}


pub fn edit_chain(t: &mut dyn TableLike, key: &str, action: &str, args: &Value, incoming: Option<&[FxRef]>) -> Result<()> {
    let mut fx = read_chain(t,key)?;
    let id = args.get_path("id").and_then(Value::as_str).unwrap_or("");
    let index = || fx.iter().position(|f| f.slot_id() == id).ok_or_else(|| anyhow!("unknown FX slot `{id}`"));
    match action {
        "fx.chain.apply" => {
            if args.get_path("replace").is_some_and(Value::truthy) { fx.clear(); }
            for entry in incoming.ok_or_else(|| anyhow!("missing chain"))? {
                let mut copy = entry.clone();
                let base = copy.id.clone(); let mut n = 2;
                while fx.iter().any(|f| f.id == copy.id) { copy.id = format!("{base}_{n}"); n += 1; }
                fx.push(copy);
            }
        }
        "fx.slot.add" => { let name = args.get_path("name").and_then(Value::as_str).ok_or_else(|| anyhow!("missing effect name"))?; fx.push(FxRef { name: name.into(), ..Default::default() }); normalize_fx(&mut fx).map_err(|e| anyhow!(e))?; }
        "fx.slot.remove" => { let i = index()?; fx.remove(i); }
        "fx.slot.move" => { let i = index()?; let forward = args.get_path("forward").and_then(|v| if let Value::Bool(b) = v { Some(*b) } else { None }).ok_or_else(|| anyhow!("missing forward"))?; let j = if forward { (i+1).min(fx.len()-1) } else { i.saturating_sub(1) }; fx.swap(i,j); }
        "fx.slot.set" => {
            let i = index()?; let key = args.get_path("key").and_then(Value::as_str).ok_or_else(|| anyhow!("missing key"))?;
            if key == "id" || key == "name" || key.contains('.') || key.is_empty() { bail!("cannot change FX identity"); }
            let value = args.get_path("value").ok_or_else(|| anyhow!("missing value"))?;
            let mut row: serde_json::Value = serde_json::to_value(&fx[i])?;
            if value.is_null() { row.as_object_mut().unwrap().remove(key); } else { row[key] = serde_json::to_value(value)?; }
            fx[i] = serde_json::from_value(row)?;
        }
        _ => bail!("unknown FX chain action"),
    }
    normalize_fx(&mut fx).map_err(|e| anyhow!(e))?;
    // Retain existing table decorations and unrelated authored keys while reordering entries.
    let old = t.get(key).cloned();
    let old_fx = read_chain(t,key)?;
    let aot = matches!(old, Some(Item::ArrayOfTables(_)));
    let mut rows = std::collections::BTreeMap::new();
    let mut arr = match &old { Some(Item::Value(toml_edit::Value::Array(a))) => { let mut a = a.clone(); a.clear(); a }, _ => toml_edit::Array::new() };
    match old {
        Some(Item::ArrayOfTables(a)) => for (entry,row) in old_fx.iter().zip(a.iter()) { rows.insert(entry.id.clone(),Item::Table(row.clone())); },
        Some(Item::Value(toml_edit::Value::Array(a))) => for (entry,row) in old_fx.iter().zip(a.iter()) { rows.insert(entry.id.clone(),Item::Value(row.clone())); },
        _ => {}
    }
    if action == "fx.chain.apply" && args.get_path("replace").is_some_and(Value::truthy) { rows.clear(); }
    let mut tables = toml_edit::ArrayOfTables::new();
    for entry in &fx {
        let mut item = rows.remove(&entry.id).unwrap_or_else(|| if aot { Item::Table(Table::new()) } else { Item::Value(toml_edit::Value::InlineTable(toml_edit::InlineTable::new())) });
        let row = item.as_table_like_mut().ok_or_else(||anyhow!("invalid FX table"))?;
        let Value::Map(values) = Value::from(serde_json::to_value(entry)?) else { unreachable!() };
        for (k,v) in values { set(row,&k,&v)?; }
        if action == "fx.slot.set" && entry.id == id && args.get_path("value").is_some_and(Value::is_null) {
            if let Some(key) = args.get_path("key").and_then(Value::as_str) { row.remove(key); }
        }
        match item {
            Item::Table(mut table) => {
                table.set_position(None);
                tables.push(table);
            }
            Item::Value(value) => arr.push(value),
            _ => unreachable!(),
        }
    }
    t.insert(key,if aot { Item::ArrayOfTables(tables) } else { Item::Value(toml_edit::Value::Array(arr)) });
    Ok(())
}

pub fn address_target(address: &str) -> Result<Option<(Target, String)>> {
    let (host, property) = if let Some(host) = address.strip_suffix(".fx_enabled") {
        (host, "fx_enabled".to_string())
    } else if let Some((host, property)) = address.split_once(".fx.") {
        (host, format!("fx.{property}"))
    } else {
        return Ok(None);
    };
    let args = if let Some(name) = host.strip_prefix("source.") {
        Value::map().with("kind", "source").with("name", name)
    } else if let Some(rest) = host.strip_prefix("scene.") {
        if let Some((scene, node)) = rest.split_once(".node.") {
            Value::map().with("kind", "node").with("scene", scene).with("node", node)
        } else if let Some((scene, rest)) = rest.split_once(".canvas.") {
            if let Some((canvas, group)) = rest.split_once(".group.") {
                Value::map().with("kind", "group").with("scene", scene).with("canvas", canvas).with("group", group)
            } else {
                Value::map().with("kind", "layout").with("scene", scene).with("canvas", rest)
            }
        } else {
            Value::map().with("kind", "scene").with("scene", rest)
        }
    } else if let Some(rest) = host.strip_prefix("render.") {
        let Some((kind @ ("canvas" | "output"), canvas)) = rest.split_once('.') else { return Ok(None) };
        Value::map().with("kind", kind).with("canvas", canvas)
    } else {
        return Ok(None);
    };
    Ok(Some((Target::parse(&args)?, property)))
}

pub fn write_property(doc: &mut DocumentMut, target: &Target, property: &str, value: &Value) -> Result<()> {
    if property == "fx_enabled" {
        if !matches!(value, Value::Bool(_)) { bail!("FX gate must be boolean"); }
        if target.kind == "canvas" || target.kind == "output" {
            let key = format!("{}_fx_enabled",target.kind);
            if doc.get("render").is_none() { doc.insert("render",Item::Table(Table::new())); }
            let render = child(doc.as_table_mut(),"render")?;
            if render.get(&key).is_none() { render.insert(&key,Item::Table(Table::new())); }
            return set(child(render,&key)?,&target.canvas,value);
        }
        return visit(doc,target,|t,_|set(t,"fx_enabled",value));
    }
    let (slot,key) = property.strip_prefix("fx.").and_then(|p|p.rsplit_once('.')).ok_or_else(||anyhow!("invalid FX property"))?;
    visit(doc,target,|t,k|edit_chain(t,k,"fx.slot.set",&Value::map().with("id",slot).with("key",key).with("value",value.clone()),None))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejected_edits_leave_authored_text_intact() {
        let mut doc: DocumentMut = "# keep\nfx=[{name='vhs',amount=0.3}]\n".parse().unwrap();
        let before = doc.to_string();
        for args in [
            Value::map().with("id","missing").with("key","amount").with("value",0.5),
            Value::map().with("id","vhs").with("key","enabled").with("value","not a bool"),
            Value::map().with("id","vhs").with("key","id").with("value","different"),
        ] {
            assert!(edit_chain(doc.as_table_mut(),"fx","fx.slot.set",&args,None).is_err());
            assert_eq!(doc.to_string(),before);
        }
    }
    #[test]
    fn repeated_slots_are_independent_and_move_without_losing_comments() {
        let mut doc: DocumentMut = "# scene\n[[fx]]\n# first\nname='vhs'\namount=0.2 # authored\n[[fx]]\nname='vhs'\namount=0.8\n".parse().unwrap();
        edit_chain(doc.as_table_mut(),"fx","fx.slot.set",&Value::map().with("id","vhs_2").with("key","enabled").with("value",false),None).unwrap();
        edit_chain(doc.as_table_mut(),"fx","fx.slot.move",&Value::map().with("id","vhs").with("forward",true),None).unwrap();
        let chain = read_chain(doc.as_table(),"fx").unwrap();
        assert_eq!(chain[0].id,"vhs_2");
        assert_eq!(chain[0].enabled,Some(false));
        assert_eq!(chain[1].params["amount"],Value::Float(0.2));
        assert!(doc.to_string().contains("# authored"));
        assert!(doc.to_string().contains("# first"));
    }
    #[test]
    fn copied_chains_do_not_link_targets_or_collide() {
        let mut doc: DocumentMut = "[canvas.wide]\nnodes=[{id='a',src='camera',fx=[{name='vhs'}]},{id='b',src='camera'}]\n".parse().unwrap();
        let mut incoming = vec![FxRef { name:"vhs".into(), ..Default::default() }];
        normalize_fx(&mut incoming).unwrap();
        for node in ["a","b"] {
            let target = Target::parse(&Value::map().with("kind","node").with("scene","show").with("node",node)).unwrap();
            visit(&mut doc,&target,|t,k|edit_chain(t,k,"fx.chain.apply",&Value::Null,Some(&incoming))).unwrap();
        }
        let target = Target::parse(&Value::map().with("kind","node").with("scene","show").with("node","a")).unwrap();
        visit(&mut doc,&target,|t,k|edit_chain(t,k,"fx.slot.set",&Value::map().with("id","vhs_2").with("key","amount").with("value",0.4),None)).unwrap();
        let scene: se_core::config::SceneDef = doc.to_string().parse::<toml::Table>().unwrap().try_into().unwrap();
        assert_eq!(scene.canvas["wide"].nodes[0].fx[1].params["amount"],Value::Float(0.4));
        assert!(!scene.canvas["wide"].nodes[1].fx[0].params.contains_key("amount"));
    }
    #[test]
    fn dotted_first_slot_addresses_persist_without_aliases() {
        let mut doc: DocumentMut = "fx=[{name='patch.win31_video'}]\n".parse().unwrap();
        let (target,property) = address_target("scene.live.fx.patch.win31_video.enabled").unwrap().unwrap();
        write_property(&mut doc,&target,&property,&Value::Bool(false)).unwrap();
        let chain = read_chain(doc.as_table(),"fx").unwrap();
        assert_eq!(chain[0].id,"patch.win31_video");
        assert_eq!(chain[0].enabled,Some(false));
    }
    #[test]
    fn dotted_source_and_implicit_patch_layer_hosts_keep_their_slot_settings() {
        let mut scene: DocumentMut = "[canvas.wide]\nnodes=[{src='patch.tint',fx=[{name='blur'}]}]\n".parse().unwrap();
        let (target, property) = address_target("scene.live.node.patch.tint.fx.blur.radius").unwrap().unwrap();
        write_property(&mut scene, &target, &property, &Value::Float(24.0)).unwrap();
        let parsed: se_core::config::SceneDef = scene.to_string().parse::<toml::Table>().unwrap().try_into().unwrap();
        assert_eq!(parsed.canvas["wide"].nodes[0].fx[0].params["radius"], Value::Float(24.0));

        let mut source: DocumentMut = "fx=[{name='vhs'}]\n".parse().unwrap();
        let (target, property) = address_target("source.camera.front.fx.vhs.enabled").unwrap().unwrap();
        assert_eq!(target.name, "camera.front");
        write_property(&mut source, &target, &property, &Value::Bool(false)).unwrap();
        assert_eq!(read_chain(source.as_table(), "fx").unwrap()[0].enabled, Some(false));
    }
}
