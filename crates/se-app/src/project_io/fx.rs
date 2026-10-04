use super::CONFIG_WRITE;
use crate::daemon::Ctx;
use anyhow::{Result, anyhow, bail};
use se_core::config::{Config, SourceFile, valid_fx_id};
use se_proto::{Op, Value};
use se_store::fx::{Target, visit, read_chain, edit_chain, set};
use std::collections::BTreeMap;
use toml_edit::{DocumentMut, Item, Table};

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str> { args.get_path(key).and_then(Value::as_str).ok_or_else(|| anyhow!("missing `{key}`")) }
fn target_path(ctx: &Ctx, t: &Target) -> Result<String> {
    let c = ctx.config.lock();
    let key = match t.kind.as_str() { "source" => format!("sources/{}",t.name), "canvas"|"output" => {
        if !c.project.canvas.contains_key(&t.canvas) { bail!("unknown canvas"); }
        return Ok("project.toml".into());
    }, _ => format!("scenes/{}",t.scene) };
    c.files.get(&key).cloned().ok_or_else(|| anyhow!("unknown FX target"))
}
fn validate(ctx: &Ctx, path: &str, text: &str) -> Result<()> {
    let (kind,name) = ctx.project.classify(&ctx.project.root().join(path)).ok_or_else(|| anyhow!("invalid config path"))?;
    let c = Config::build(&[SourceFile { kind, name, path: path.into(), table: text.parse()? }]);
    if let Some(e) = c.errors.iter().find(|e| !e.msg.starts_with("unknown transition") && !e.msg.contains("unknown scene")) { bail!("{}",e.msg); }
    Ok(())
}

pub fn plan(ctx: &Ctx, action: &str, args: &Value) -> Result<BTreeMap<String,String>> {
    let mut docs: BTreeMap<String,DocumentMut> = BTreeMap::new();
    let targets: Vec<Target> = if action == "fx.chain.apply" {
        args.get_path("targets").and_then(Value::as_list).ok_or_else(|| anyhow!("missing targets"))?.iter().map(Target::parse).collect::<Result<_>>()?
    } else if action == "fx.group.set" {
        vec![Target::parse(&Value::map().with("kind","layout").with("scene",required(args,"scene")?).with("canvas",required(args,"canvas")?))?]
    } else { vec![Target::parse(args.get_path("target").ok_or_else(|| anyhow!("missing target"))?)?] };
    if targets.is_empty() { bail!("targets must not be empty"); }
    let incoming = if action == "fx.chain.apply" { Some(ctx.config.lock().fx_chains.get(required(args,"name")?).ok_or_else(|| anyhow!("unknown chain"))?.fx.clone()) } else { None };
    for target in targets {
        let path = target_path(ctx,&target)?;
        if !docs.contains_key(&path) { docs.insert(path.clone(),std::fs::read_to_string(ctx.project.root().join(&path))?.parse()?); }
        let doc = docs.get_mut(&path).unwrap();
        if action == "fx.chain.save" {
            let name = required(args,"name")?;
            if !valid_fx_id(name) || name.contains('.') { bail!("invalid chain name"); }
            let mut chain = None;
            visit(doc,&target,|t,k| { let fx = read_chain(t,k)?; if chain.as_ref().is_some_and(|previous| previous != &fx) { bail!("node chains differ across canvases"); } chain = Some(fx); Ok(()) })?;
            let mut saved: DocumentMut = match std::fs::read_to_string(ctx.project.root().join(format!("fx_chains/{name}.toml"))) { Ok(s) => s.parse()?, Err(e) if e.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(), Err(e) => return Err(e.into()) };
            if let Some(label) = args.get_path("label") { set(saved.as_table_mut(),"label",label)?; }
            set(saved.as_table_mut(),"fx",&Value::from(serde_json::to_value(chain.unwrap_or_default())?))?;
            docs.insert(format!("fx_chains/{name}.toml"),saved);
        } else if action == "fx.group.set" {
            let id = required(args,"id")?;
            if !valid_fx_id(id) || id.contains('.') { bail!("invalid group id"); }
            visit(doc,&target,|layout,_| {
                let old = layout.get("groups").cloned();
                let aot = matches!(old,Some(Item::ArrayOfTables(_)));
                let mut rows: Vec<Item> = match old {
                    Some(Item::ArrayOfTables(a)) => a.iter().cloned().map(Item::Table).collect(),
                    Some(Item::Value(toml_edit::Value::Array(a))) => a.iter().cloned().map(Item::Value).collect(),
                    None => Vec::new(), _ => bail!("invalid groups array"),
                };
                let i = rows.iter().position(|r| r.as_table_like().and_then(|t|t.get("id")).and_then(Item::as_str) == Some(id));
                if args.get_path("delete").is_some_and(Value::truthy) { let i = i.ok_or_else(|| anyhow!("unknown group"))?; rows.remove(i); }
                else {
                    let nodes = args.get_path("nodes").ok_or_else(|| anyhow!("missing nodes"))?;
                    if nodes.as_list().is_none_or(|l| l.iter().any(|v| v.as_str().is_none())) { bail!("nodes must be string list"); }
                    let i = match i { Some(i) => i, None => { rows.push(if aot { Item::Table(Table::new()) } else { Item::Value(toml_edit::Value::InlineTable(toml_edit::InlineTable::new())) }); rows.len()-1 } };
                    let row = rows[i].as_table_like_mut().ok_or_else(||anyhow!("invalid group table"))?;
                    set(row,"id",&Value::Str(id.into()))?; set(row,"nodes",nodes)?;
                    if let Some(z) = args.get_path("z") { set(row,"z",z)?; }
                    if let Some(enabled) = args.get_path("enabled") { if !matches!(enabled, Value::Bool(_)) { bail!("enabled must be boolean"); } set(row,"fx_enabled",enabled)?; }
                }
                let mut arr = toml_edit::Array::new(); let mut tables = toml_edit::ArrayOfTables::new();
                for row in rows { match row { Item::Table(t) => tables.push(t), Item::Value(v) => arr.push(v), _ => bail!("invalid group entry") } }
                layout.insert("groups",if aot { Item::ArrayOfTables(tables) } else { Item::Value(toml_edit::Value::Array(arr)) }); Ok(())
            })?;
        } else if action == "fx.chain.set" {
            let enabled = args.get_path("enabled").filter(|v|matches!(v,Value::Bool(_))).ok_or_else(||anyhow!("enabled must be boolean"))?;
            if target.kind == "canvas" || target.kind == "output" {
                let key = format!("{}_fx_enabled",target.kind);
                if doc.get("render").is_none() { doc.insert("render",Item::Table(Table::new())); }
                let render = doc.get_mut("render").unwrap().as_table_like_mut().ok_or_else(||anyhow!("invalid render table"))?;
                if render.get(&key).is_none() { render.insert(&key,Item::Table(Table::new())); }
                set(render.get_mut(&key).unwrap().as_table_like_mut().ok_or_else(||anyhow!("invalid gate map"))?,&target.canvas,enabled)?;
            } else { visit(doc,&target,|t,_|set(t,"fx_enabled",enabled))?; }
        } else { visit(doc,&target,|t,k|edit_chain(t,k,action,args,incoming.as_deref()))?; }
    }
    let mut changes = BTreeMap::new();
    for (path,doc) in docs { let text = doc.to_string(); validate(ctx,&path,&text)?; if std::fs::read_to_string(ctx.project.root().join(&path)).ok().as_deref() != Some(&text) { changes.insert(path,text); } }
    Ok(changes)
}

pub fn start(ctx: &Ctx) {
    for prefix in ["fx.chain", "fx.slot", "fx.group"] {
        let mut rx = ctx.hub.route_actions(prefix); let ctx = ctx.clone();
        tokio::spawn(async move { while let Some(command) = rx.recv().await {
            let Op::Action { name,args } = command.op else {continue};
            let result = (|| -> Result<Vec<String>> {
                let _guard = CONFIG_WRITE.lock(); let changes = plan(&ctx,&name,&args)?;
                let before: BTreeMap<_,_> = changes.keys().map(|p| (p.clone(),std::fs::read_to_string(ctx.project.root().join(p)).ok())).collect();
                let mut written: Vec<String> = Vec::new();
                for (path,text) in changes {
                    if let Err(e) = ctx.project.write_file(&path,&text) {
                        for p in &written { if let Some(Some(old)) = before.get(p) { let _ = ctx.project.write_file(p,old); } else { let _ = std::fs::remove_file(ctx.project.root().join(p)); } }
                        return Err(e);
                    }
                    written.push(path);
                }
                Ok(written)
            })();
            match result { Ok(paths) if !paths.is_empty() => crate::daemon::reload(&ctx,&paths), Ok(_) => {}, Err(e) => ctx.hub.log("error","project",format!("{name}: {e:#}")) }
        }});
    }
}
