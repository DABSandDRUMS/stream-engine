//! UI queries (`lights.rig`, `lights.cuelists`, `lights.palettes`, `lights.effects`,
//! `lights.programmer`, `lights.output`, `lights.rdm`) — shapes documented in `docs/lights.md`.

use crate::control::View;
use crate::output::Shared;
use crate::palette::Spec;
use crate::show::Show;
use parking_lot::RwLock;
use se_hub::Hub;
use se_proto::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

fn spec_value(s: &Spec) -> Value {
    match s {
        Spec::Literal(v) => v.clone(),
        Spec::Palette(p) => Value::Str(format!("palette:{p}")),
        Spec::Address(a) => match a.strip_prefix("palette.") {
            Some(slot) => Value::Str(format!("stream:{slot}")),
            None => Value::Str(format!("@{a}")),
        },
    }
}

fn strs<I: IntoIterator<Item = S>, S: Into<String>>(i: I) -> Value {
    Value::List(i.into_iter().map(|s| Value::Str(s.into())).collect())
}

fn pos(p: [f32; 2]) -> Value {
    Value::List(vec![Value::Float(p[0] as f64), Value::Float(p[1] as f64)])
}

pub fn rig(show: &Show, plan_errors: &[String], shared: &Shared) -> Value {
    let r = &show.rig;
    let fixtures: Vec<Value> = r
        .fixtures
        .iter()
        .map(|f| {
            let root = &r.heads[f.root];
            Value::map()
                .with("id", f.id.clone())
                .with("label", f.label.clone())
                .with("profile", f.profile.clone())
                .with("profile_name", f.profile_name.clone())
                .with("mode", f.mode.clone())
                .with("kind", f.kind.clone())
                .with("universe", f.universe as i64)
                .with("address", f.address as i64)
                .with("footprint", f.footprint as i64)
                .with("position", pos(f.position))
                .with("rotation", f.rotation as f64)
                .with("beam", f.beam as f64)
                .with("heads", strs(f.leaves.iter().map(|h| r.heads[*h].id.clone())))
                .with("attrs", strs(root.attrs.iter().map(|a| a.name.clone())))
        })
        .collect();
    let heads: Vec<Value> = r
        .heads
        .iter()
        .filter(|h| h.leaf)
        .map(|h| {
            Value::map()
                .with("id", h.id.clone())
                .with("fixture", r.fixtures[h.fixture].id.clone())
                .with("position", pos(h.position))
                .with("rotation", h.rotation as f64)
                .with("kind", h.kind.clone())
                .with("beam", h.beam as f64)
                .with("pan_deg", h.pan_deg as f64)
                .with("tilt_deg", h.tilt_deg as f64)
                .with("attrs", strs(h.attrs.iter().map(|a| a.name.clone())))
                .with("moving", h.moving)
        })
        .collect();
    let groups: BTreeMap<String, Value> = r.groups.iter().map(|g| (g.name.clone(), strs(g.leaves.iter().map(|h| r.heads[*h].id.clone())))).collect();
    let profiles: Vec<Value> = r
        .profiles
        .values()
        .map(|p| {
            Value::map().with("id", p.id.clone()).with("name", p.name.clone()).with("manufacturer", p.manufacturer.clone()).with("kind", p.kind.clone()).with(
                "modes",
                Value::List(
                    p.modes
                        .iter()
                        .map(|m| {
                            let mut chans: Vec<String> = m.channels.iter().map(|c| c.name.clone()).collect();
                            if m.cells > 0 {
                                chans.push(format!("{} × [{}]", m.cells, m.cell_channels.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")));
                            }
                            Value::map().with("id", m.id.clone()).with("footprint", m.footprint() as i64).with("channels", strs(chans))
                        })
                        .collect(),
                ),
            )
        })
        .collect();
    let status = shared.status.lock().clone();
    let outputs: Vec<Value> = r
        .outputs
        .iter()
        .map(|o| {
            let st = status.iter().find(|s| s.id == o.id);
            Value::map()
                .with("id", o.id.clone())
                .with("kind", o.kind_name())
                .with("enabled", o.enabled)
                .with("universes", Value::List(o.universes().into_iter().map(|u| Value::Int(u as i64)).collect()))
                .with("status", st.map(|s| s.state.clone()).unwrap_or_else(|| if o.enabled { "fail".into() } else { "off".into() }))
                .with("detail", st.map(|s| s.detail.clone()).unwrap_or_default())
                .with("frames", st.map(|s| s.frames as i64).unwrap_or(0))
                .with("errors", st.map(|s| s.errors as i64).unwrap_or(0))
        })
        .collect();
    let mut errors: Vec<String> = show.errors.clone();
    errors.extend(plan_errors.iter().cloned());
    Value::map()
        .with("fixtures", Value::List(fixtures))
        .with("heads", Value::List(heads))
        .with("groups", Value::Map(groups))
        .with("profiles", Value::List(profiles))
        .with("outputs", Value::List(outputs))
        .with("universes", Value::List(r.universes.iter().map(|u| Value::Int(*u as i64)).collect()))
        .with("errors", strs(errors))
        .with("has_rig", show.has_rig)
}

pub fn cuelists(v: &View) -> Value {
    let now = Instant::now();
    Value::List(
        v.show
            .lists
            .iter()
            .map(|(name, l)| {
                let pb = v.playbacks.get(name).filter(|p| p.current.is_some());
                let cur = pb.and_then(|p| p.current);
                let next = match cur {
                    Some(c) if c + 1 < l.cues.len() => Some(l.cues[c + 1].id.clone()),
                    Some(_) if l.looped => l.cues.first().map(|c| c.id.clone()),
                    Some(_) => None,
                    None => l.cues.first().map(|c| c.id.clone()),
                };
                let (progress, elapsed, total) = match pb {
                    Some(p) => {
                        let el = now.saturating_duration_since(p.go_at).as_millis() as u64;
                        (if p.total_ms == 0 { 1.0 } else { (el as f64 / p.total_ms as f64).min(1.0) }, el, p.total_ms)
                    }
                    None => (0.0, 0, 0),
                };
                let cues: Vec<Value> = l
                    .cues
                    .iter()
                    .map(|c| {
                        Value::map()
                            .with("id", c.id.clone())
                            .with("label", c.label.clone())
                            .with("fade_ms", c.fade_ms as i64)
                            .with("fade_out_ms", c.fade_out_ms.unwrap_or(c.fade_ms) as i64)
                            .with("delay_ms", c.delay_ms as i64)
                            .with("follow_ms", c.follow_ms.map(|x| Value::Int(x as i64)).unwrap_or(Value::Null))
                            .with("wait_ms", c.wait_ms.map(|x| Value::Int(x as i64)).unwrap_or(Value::Null))
                            .with("block", c.block)
                            .with("duration_ms", c.duration() as i64)
                            .with("targets", strs(c.set.iter().map(|(t, _, _)| t.clone())))
                            .with("effects", strs(c.effects.iter().map(|(e, _, _)| e.clone())))
                            .with("palettes", strs(c.palettes()))
                    })
                    .collect();
                Value::map()
                    .with("name", name.clone())
                    .with("label", l.label.clone())
                    .with("priority", pb.map(|p| p.priority as i64).unwrap_or(l.priority.unwrap_or(se_proto::PRIORITY_PRESET) as i64))
                    .with("master", pb.map(|p| p.master as f64).unwrap_or(1.0))
                    .with("playing", pb.is_some())
                    .with("looped", l.looped)
                    .with("current", cur.map(|c| Value::Str(l.cues[c].id.clone())).unwrap_or(Value::Null))
                    .with("current_index", cur.map(|c| c as i64).unwrap_or(-1))
                    .with("next", next.map(Value::Str).unwrap_or(Value::Null))
                    .with("progress", progress)
                    .with("elapsed_ms", elapsed as i64)
                    .with("total_ms", total as i64)
                    .with("cues", Value::List(cues))
            })
            .collect(),
    )
}

pub fn palettes(show: &Show) -> Value {
    Value::List(
        show.palettes
            .values()
            .map(|p| {
                let set: BTreeMap<String, Value> =
                    p.set.iter().map(|(t, attrs)| (t.clone(), Value::Map(attrs.iter().map(|(a, s)| (a.clone(), spec_value(s))).collect()))).collect();
                Value::map()
                    .with("name", p.name.clone())
                    .with("label", p.label.clone())
                    .with("kind", p.kind.clone())
                    .with("set", Value::Map(set))
                    .with("used_by", strs(show.palette_users(&p.name)))
            })
            .collect(),
    )
}

pub fn effects(show: &Show, hub: &Hub) -> Value {
    let snap = hub.snapshot.load();
    Value::List(
        show.effects
            .iter()
            .map(|e| {
                let p = |k: &str| format!("lights.effect.{}.{k}", e.name);
                Value::map()
                    .with("name", e.name.clone())
                    .with("label", e.label.clone())
                    .with("kind", e.kind.as_str())
                    .with("targets", strs(e.targets.clone()))
                    .with("unit", if e.unit == crate::effects::Unit::Beats { "beats" } else { "hz" })
                    .with("rate", snap.f32(&p("rate")).unwrap_or(e.rate) as f64)
                    .with("size", snap.f32(&p("size")).unwrap_or(e.size) as f64)
                    .with("spread", e.spread as f64)
                    .with("active", snap.bool(&p("active")))
            })
            .collect(),
    )
}

pub fn programmer(v: &View) -> Value {
    let heads = v.prog.heads(&v.show.rig);
    Value::map()
        .with("selection", strs(v.prog.selection.clone()))
        .with("heads", strs(heads.iter().map(|h| v.show.rig.heads[*h].id.clone())))
        .with("values", Value::Map(v.prog.values.iter().map(|(a, x)| (a.clone(), x.clone())).collect()))
        .with("highlight", !v.prog.highlight.is_empty())
}

pub fn output(v: &View, shared: &Shared) -> Value {
    let rig = v.show.rig.clone();
    let mut mon = shared.monitor.lock();
    let m = mon.read();
    let universes: BTreeMap<String, Value> = rig
        .universes
        .iter()
        .enumerate()
        .filter_map(|(i, u)| m.frame.universes.get(i).map(|d| (u.to_string(), Value::List(d.iter().map(|b| Value::Int(*b as i64)).collect()))))
        .collect();
    let heads: Vec<Value> = rig
        .heads
        .iter()
        .enumerate()
        .filter(|(_, h)| h.leaf)
        .filter_map(|(i, h)| {
            let o = m.frame.heads.get(i)?;
            Some(
                Value::map()
                    .with("id", h.id.clone())
                    .with("x", h.position[0] as f64)
                    .with("y", h.position[1] as f64)
                    .with("rotation", h.rotation as f64)
                    .with("intensity", o.intensity as f64)
                    .with("color", Value::List(o.color.iter().map(|c| Value::Float(*c as f64)).collect()))
                    .with("pan", o.pan as f64)
                    .with("tilt", o.tilt as f64)
                    .with("zoom", o.zoom as f64)
                    .with("strobe", o.strobe as f64)
                    .with("beam", h.beam as f64)
                    .with("moving", h.moving)
                    .with("limited", o.limited),
            )
        })
        .collect();
    let s = m.stats;
    Value::map()
        .with("fps", s.fps as f64)
        .with("frames", s.frames as i64)
        .with("seq", m.seq as i64)
        .with("beat", m.beat)
        .with(
            "jitter",
            Value::map()
                .with("mean_ms", s.late_mean_us as f64 / 1000.0)
                .with("p50_ms", s.jitter_p50_us as f64 / 1000.0)
                .with("p99_ms", s.jitter_p99_us as f64 / 1000.0)
                .with("p999_ms", s.jitter_p999_us as f64 / 1000.0)
                .with("max_ms", s.jitter_worst_us as f64 / 1000.0)
                .with("late_p99_ms", s.late_p99_us as f64 / 1000.0)
                .with("late_max_ms", s.late_worst_us as f64 / 1000.0)
                .with("write_mean_ms", s.write_mean_us as f64 / 1000.0)
                .with("write_max_ms", s.write_max_us as f64 / 1000.0)
                .with("render_worst_ms", s.render_worst_us as f64 / 1000.0)
                .with("overruns", s.overruns as i64)
                .with("samples", s.samples as i64),
        )
        .with("scheduling", shared.scheduling.lock().clone())
        .with("universes", Value::Map(universes))
        .with("heads", Value::List(heads))
        .with(
            "limiter",
            Value::map()
                .with("active", m.frame.limited > 0)
                .with("limited", m.frame.limited as i64)
                .with("suppressed", m.frame.suppressed as i64)
                .with("strobe_capped", m.frame.strobe_capped as i64),
        )
}

pub fn rdm(shared: &Shared) -> Value {
    let r = shared.rdm.lock().clone();
    let devices: Vec<Value> = r
        .devices
        .iter()
        .map(|d| {
            Value::map()
                .with("uid", d.uid.0 as i64)
                .with("uid_str", d.uid.to_string())
                .with("manufacturer_id", d.uid.manufacturer() as i64)
                .with("model_id", d.model_id as i64)
                .with("category", d.category as i64)
                .with("footprint", d.footprint as i64)
                .with("personality", d.personality as i64)
                .with("personalities", d.personalities as i64)
                .with("start_address", d.start_address as i64)
                .with("software_version", d.software_version as i64)
                .with("manufacturer", d.manufacturer.clone().map(Value::Str).unwrap_or(Value::Null))
                .with("model", d.model.clone().map(Value::Str).unwrap_or(Value::Null))
                .with("label", d.label.clone().map(Value::Str).unwrap_or(Value::Null))
        })
        .collect();
    Value::map()
        .with(
            "status",
            if r.running && r.at == 0 {
                "discovering…".to_string()
            } else if r.status.is_empty() {
                "not run yet".to_string()
            } else {
                r.status
            },
        )
        .with("firmware", r.firmware.map(Value::Str).unwrap_or(Value::Null))
        .with("serial", r.serial.map(Value::Str).unwrap_or(Value::Null))
        .with("devices", Value::List(devices))
        .with("at", r.at)
}

pub fn register(hub: &Arc<Hub>, view: Arc<RwLock<View>>, shared: Arc<Shared>) {
    let h = hub.clone();
    hub.register_query(
        "lights",
        Arc::new(move |name: String, _args: Value| {
            let view = view.clone();
            let shared = shared.clone();
            let hub = h.clone();
            Box::pin(async move {
                let v = view.read();
                Ok(match name.as_str() {
                    "lights.rig" => rig(&v.show, &v.plan_errors, &shared),
                    "lights.cuelists" => cuelists(&v),
                    "lights.palettes" => palettes(&v.show),
                    "lights.effects" => effects(&v.show, &hub),
                    "lights.programmer" => programmer(&v),
                    "lights.output" => output(&v, &shared),
                    "lights.rdm" => rdm(&shared),
                    other => return Err(format!("unknown query `{other}`")),
                })
            })
        }),
    );
}
