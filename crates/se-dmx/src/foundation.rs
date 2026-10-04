//! Shared look contract: authored content references and independent operator controls.
//! Layer execution deliberately uses the existing cue playback and core resolver.

use crate::cuelist::{self, Cue, CueList};
use crate::effects::{Kind, Unit};
use crate::palette;
use crate::show::Show;
use se_proto::{Ease, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Slot { Base, Rhythm, Accent }

impl Slot {
    pub const ALL: [Slot; 3] = [Slot::Base, Slot::Rhythm, Slot::Accent];
    pub fn parse(s: &str) -> Result<Self, String> {
        match s { "base" => Ok(Self::Base), "rhythm" => Ok(Self::Rhythm), "accent" => Ok(Self::Accent), _ => Err("layer must be base, rhythm, or accent".into()) }
    }
    pub fn name(self) -> &'static str { match self { Self::Base => "base", Self::Rhythm => "rhythm", Self::Accent => "accent" } }
    pub fn rank(self) -> u16 { match self { Self::Base => 0, Self::Rhythm => 1, Self::Accent => 2 } }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Controls {
    /// Operator/library metadata; not an automatic visual classifier.
    pub vibe: String,
    /// Depth/activity of owned effects, independent of fixture brightness.
    pub energy: f32,
    /// Multiplier of authored beats per cycle; Hz effects are unchanged.
    pub rhythm: f32,
    pub brightness: f32,
    /// Union of named fixture/cell/group targets; empty means no participating heads.
    pub coverage: Vec<String>,
}

impl Default for Controls {
    fn default() -> Self { Self { vibe: String::new(), energy: 1.0, rhythm: 1.0, brightness: 1.0, coverage: vec!["all".into()] } }
}

fn number(args: &Value, name: &str, default: f32, low: f64, high: f64) -> Result<f32, String> {
    let Some(v) = args.get_path(name) else { return Ok(default) };
    let n = v.as_f64().filter(|n| n.is_finite() && *n >= low && *n <= high).ok_or_else(|| format!("{name} must be a finite number in {low}..{high}"))?;
    Ok(n as f32)
}

pub(crate) fn names(v: &Value, field: &str) -> Result<Vec<String>, String> {
    let values: Vec<&Value> = match v { Value::Str(_) => vec![v], Value::List(v) => v.iter().collect(), _ => return Err(format!("{field} must be a name or list of names")) };
    let mut out = Vec::new();
    for v in values {
        let s = v.as_str().filter(|s| !s.is_empty()).ok_or_else(|| format!("{field} must contain non-empty names"))?;
        if !out.iter().any(|n| n == s) { out.push(s.to_string()); }
    }
    Ok(out)
}

impl Controls {
    pub fn patched(&self, args: &Value, show: &Show) -> Result<Self, String> {
        let mut next = self.clone();
        if let Some(v) = args.get_path("vibe") { next.vibe = v.as_str().ok_or("vibe must be a string")?.into(); }
        next.energy = number(args, "energy", next.energy, 0.0, 1.0)?;
        next.rhythm = number(args, "rhythm", next.rhythm, 0.125, 8.0)?;
        next.brightness = number(args, "brightness", next.brightness, 0.0, 1.0)?;
        if let Some(v) = args.get_path("coverage") { next.coverage = names(v, "coverage")?; }
        next.mask(show)?;
        Ok(next)
    }

    pub fn mask(&self, show: &Show) -> Result<Vec<bool>, String> {
        for t in &self.coverage {
            if t != "all" && show.rig.group(t).is_none() && show.rig.head(t).is_none() { return Err(format!("unknown coverage fixture or group `{t}`")); }
        }
        Ok((0..show.rig.heads.len()).map(|h| self.coverage.iter().any(|t| palette::covers(&show.rig, t, h))).collect())
    }

    pub fn value(&self) -> Value {
        Value::map().with("vibe", self.vibe.clone()).with("energy", self.energy as f64).with("rhythm", self.rhythm as f64)
            .with("brightness", self.brightness as f64).with("coverage", Value::List(self.coverage.iter().cloned().map(Value::Str).collect()))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Selection {
    pub palette: Option<String>,
    pub cuelist: Option<String>,
    pub cue: Option<String>,
    pub effects: Vec<String>,
}

impl Selection {
    pub fn parse(args: &Value) -> Result<Self, String> {
        let name = |field: &str| -> Result<Option<String>, String> {
            args.get_path(field).map(|v| v.as_str().filter(|s| !s.is_empty()).map(String::from).ok_or_else(|| format!("{field} must be a non-empty name"))).transpose()
        };
        let s = Self { palette: name("palette")?, cuelist: name("cuelist")?, cue: name("cue")?, effects: args.get_path("effects").map(|v| names(v, "effects")).transpose()?.unwrap_or_default() };
        if s.palette.is_none() && s.cuelist.is_none() && s.effects.is_empty() { return Err("select needs an authored palette, cuelist, or effects".into()); }
        if s.cue.is_some() && s.cuelist.is_none() { return Err("cue requires cuelist".into()); }
        Ok(s)
    }

    /// Compose references, without inventing fixture values or creating library content.
    pub fn compose(&self, show: &Show, controls: &Controls) -> Result<(CueList, usize), String> {
        let pal = self.palette.as_ref().map(|n| show.palettes.get(n).ok_or_else(|| format!("unknown palette `{n}`"))).transpose()?;
        if let Some(p) = pal { let errors = p.validate(&show.rig); if !errors.is_empty() { return Err(errors.join("; ")); } }
        let mut list = match &self.cuelist {
            Some(n) => show.lists.get(n).cloned().ok_or_else(|| format!("unknown cue list `{n}`"))?,
            None => match pal {
                Some(p) => CueList::look(p),
                None => CueList { name: String::new(), label: String::new(), priority: None, looped: false, autorelease: false, release_ms: 0, fader_start: false, tracking: true, back_fade_ms: None,
                    cues: vec![Cue { id: "effects".into(), label: String::new(), fade_ms: 0, fade_out_ms: None, delay_ms: 0, follow_ms: None, wait_ms: None, wait_beats: None, ease: Ease::Linear, block: false,
                        release: Vec::new(), effects: Vec::new(), stop_effects: Vec::new(), set: Vec::new(), addresses: Vec::new() }], knobs: Vec::new(), tags: Vec::new() },
            },
        };
        if list.cues.is_empty() { return Err("selected cue list has no cues".into()); }
        let k = self.cue.as_ref().map(|n| list.index_of(n).ok_or_else(|| format!("unknown cue `{n}`"))).transpose()?.unwrap_or(0);
        for cue in &mut list.cues {
            if self.cuelist.is_some() && let Some(p) = pal {
                // Authored cue attributes remain more specific than the added palette.
                for (target, _) in &p.set { cue.set.insert(0, (target.clone(), Vec::new(), vec![p.name.clone()])); }
            }
            // Fixture/group addresses use the same expansion as `set`, so coverage and
            // brightness cannot be bypassed by the authored address spelling.
            let mut addresses = Vec::new();
            for (address, entry) in std::mem::take(&mut cue.addresses) {
                if let Some(rest) = address.strip_prefix("lights.group.") {
                    let (target, attr) = rest.rsplit_once('.').ok_or_else(|| format!("bad layer group address `{address}`"))?;
                    let g = show.rig.group(target).ok_or_else(|| format!("unknown layer group `{target}`"))?;
                    if !show.rig.groups[g].attrs.iter().any(|a| a.name == attr) { return Err(format!("layer address `{address}` is not a fixture attribute; use global controls outside a look")); }
                    cue.set.push((target.into(), vec![(attr.into(), entry)], Vec::new()));
                } else if address.starts_with("lights.") {
                    let mut target = None;
                    for head in &show.rig.heads {
                        if let Some(attr) = head.attrs.iter().find(|a| head.addr(&a.name) == address) { target = Some((head.id.clone(), attr.name.clone())); break; }
                    }
                    let (target, attr) = target.ok_or_else(|| format!("layer address `{address}` is not a patched fixture attribute; use cue.effects or global controls outside a look"))?;
                    cue.set.push((target, vec![(attr, entry)], Vec::new()));
                } else { addresses.push((address, entry)); }
            }
            cue.addresses = addresses;
            for name in &self.effects {
                if !cue.effects.iter().any(|(n, _, _)| n == name) { cue.effects.push((name.clone(), None, None)); }
            }
        }
        let effect_names = show.effects.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        let errors = cuelist::validate(&list, &show.rig, &show.palettes, &effect_names);
        if !errors.is_empty() { return Err(errors.join("; ")); }
        let mask = controls.mask(show)?;
        for cue in &mut list.cues {
            for (name, _, rate) in &mut cue.effects {
                let e = show.effects.iter().find(|e| &e.name == name).ok_or_else(|| format!("unknown effect `{name}`"))?;
                let heads = e.layout(&show.rig)?;
                let capable = |h: usize| match e.kind {
                    Kind::Circle => show.rig.heads[h].attr("pan").is_some() && show.rig.heads[h].attr("tilt").is_some(),
                    Kind::ColorChase | Kind::ColorWave | Kind::Rainbow | Kind::FollowColor => show.rig.heads[h].attr("color").is_some(),
                    _ => show.rig.heads[h].attr("intensity").is_some(),
                };
                if !controls.coverage.is_empty() && !heads.iter().any(|(h, _)| mask[*h] && capable(*h)) { return Err(format!("effect `{name}` has no capable heads in coverage")); }
                // Omitted rates remain bindable at neutral rhythm; only a non-neutral
                // beat-period lever needs to take ownership of the authored default.
                if e.unit == Unit::Beats && rate.is_none() && controls.rhythm != 1.0 { *rate = Some(e.rate); }
                if e.unit == Unit::Beats && !(0.01..=64.0).contains(&(rate.unwrap_or(e.rate) * controls.rhythm)) { return Err(format!("effect `{name}` beat period after rhythm scaling must be 0.01..64")); }
            }
        }
        Ok((list, k))
    }

    pub fn value(&self) -> Value {
        Value::map().with("palette", self.palette.clone().map(Value::Str).unwrap_or(Value::Null))
            .with("cuelist", self.cuelist.clone().map(Value::Str).unwrap_or(Value::Null)).with("cue", self.cue.clone().map(Value::Str).unwrap_or(Value::Null))
            .with("effects", Value::List(self.effects.iter().cloned().map(Value::Str).collect()))
    }
}

/// The next grid boundary, always strictly after the current continuous beat position.
pub fn boundary(position: f64, division: f64) -> Result<f64, String> {
    if !position.is_finite() || position < 0.0 || !division.is_finite() || !(0.125..=64.0).contains(&division) { return Err("quantize needs a valid clock and a beat grid in 0.125..64".into()); }
    Ok((position / division).floor().mul_add(division, division))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::{Nav, Playback, Timer};
    use se_core::{Config, Core, Input};
    use se_proto::{Command, Origin};
    use std::time::Instant;

    fn show() -> Show {
        let mut s = Show::empty();
        s.rig = crate::engine::tests::rig("[fixtures.a]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 1\n[fixtures.b]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 4\n[groups]\nfront = [\"a\"]");
        s.palettes.insert("wash".into(), palette::Palette::parse("wash", &toml::from_str("[set]\nall = {intensity = 0.8, color = \"#ff0000\"}").unwrap()).unwrap());
        for (name, unit) in [("beat", "beats"), ("time", "hz")] {
            s.effects.push(crate::effects::EffectDef::parse(name, &toml::from_str(&format!("kind = \"dimmer_sine\"\norder = \"index\"\nunit = \"{unit}\"\nrate = 2.0\nsize = 0.6")).unwrap()).unwrap());
        }
        s
    }

    fn core(show: &Show) -> Core {
        let mut c = Core::new(Config::build(&[]), 0);
        for (address, meta) in show.declarations() { c.submit(Input::Declare { address, meta }); }
        c.step();
        c
    }

    fn apply(c: &mut Core, cmds: Vec<Command>) {
        for cmd in cmds { c.submit(Input::Command { cmd }); }
        c.step();
    }

    #[test]
    fn independent_controls_change_resolved_values_without_changing_tempo() {
        let s = show();
        let args = Value::map().with("palette", "wash").with("effects", Value::List(vec![Value::Str("beat".into()), Value::Str("time".into())]))
            .with("brightness", 0.5).with("energy", 0.25).with("rhythm", 2.0).with("coverage", "front").with("vibe", "operator-tag");
        let selection = Selection::parse(&args).unwrap();
        let controls = Controls::default().patched(&args, &s).unwrap();
        let (list, k) = selection.compose(&s, &controls).unwrap();
        let mut pb = Playback::layer("layer:base:1", 200, Origin::Rule, None, None, controls, &s);
        let mut tokens = 0;
        let out = pb.goto(&list, k, Nav::Goto, Some(0), &s, &|_| None, &mut tokens, Instant::now());
        let mut c = core(&s);
        apply(&mut c, out.cmds);
        assert!((c.get("lights.a.intensity").unwrap().as_f64().unwrap() - 0.4).abs() < 1e-6);
        assert_eq!(c.get("lights.b.intensity").unwrap().as_f64(), Some(0.0));
        assert!((c.get("lights.effect.beat.size").unwrap().as_f64().unwrap() - 0.15).abs() < 1e-6);
        assert_eq!(c.get("lights.effect.beat.rate").unwrap().as_f64(), Some(4.0));
        assert_eq!(c.get("lights.effect.time.rate").unwrap().as_f64(), Some(2.0));
        assert_eq!(c.get("lights.effect.beat.coverage.a"), Some(&Value::Bool(true)));
        assert_eq!(c.get("lights.effect.beat.coverage.b"), Some(&Value::Bool(false)));
        assert!(c.get("beat.bpm").is_none(), "operator rhythm must never write musical tempo");
    }

    #[test]
    fn releasing_a_generation_restores_advanced_owner_and_cannot_release_retrigger() {
        let s = show();
        let selection = Selection::parse(&Value::map().with("effects", "beat")).unwrap();
        let (list, k) = selection.compose(&s, &Controls::default()).unwrap();
        let now = Instant::now();
        let mut t = 0;
        let mut base = Playback::new("advanced", 200, Origin::Rule, None, None, 0.3);
        let mut accent = Playback::layer("layer:accent:2", 202, Origin::Rule, None, None, Controls { energy: 0.9, ..Controls::default() }, &s);
        let mut c = core(&s);
        apply(&mut c, base.goto(&list, k, Nav::Goto, Some(0), &s, &|_| None, &mut t, now).cmds);
        apply(&mut c, accent.goto(&list, k, Nav::Goto, Some(0), &s, &|_| None, &mut t, now).cmds);
        let release = accent.release(100, now);
        let mut replacement = Playback::layer("layer:accent:3", 202, Origin::Rule, None, None, Controls { energy: 0.5, ..Controls::default() }, &s);
        apply(&mut c, replacement.goto(&list, k, Nav::Goto, Some(0), &s, &|_| None, &mut t, now).cmds);
        for (_, timer) in release.timers {
            if let Timer::Release { cmds, .. } = timer { apply(&mut c, cmds.into_iter().map(|(_, _, c)| c).collect()); }
        }
        assert!((c.get("lights.effect.beat.size").unwrap().as_f64().unwrap() - 0.3).abs() < 1e-6);
        apply(&mut c, replacement.release(0, now).cmds);
        assert_eq!(c.get("lights.effect.beat.active"), Some(&Value::Bool(true)));
        assert!((c.get("lights.effect.beat.size").unwrap().as_f64().unwrap() - 0.18).abs() < 1e-6);
        apply(&mut c, base.release(0, now).cmds);
        assert_eq!(c.get("lights.effect.beat.active"), Some(&Value::Bool(false)));
    }

    #[test]
    fn reload_and_final_generation_release_clear_effects_and_retired_values_without_touching_manual_replacement() {
        let mut s = show();
        let mut c = core(&s);
        let args = Value::map().with("palette", "wash").with("effects", "beat");
        let old_selection = Selection::parse(&args).unwrap();
        let (list, k) = old_selection.compose(&s, &Controls::default()).unwrap();
        let now = Instant::now();
        let mut tokens = 0;
        let mut old = Playback::layer("layer:rhythm:27", 201, Origin::Rule, None, None, Controls::default(), &s);
        apply(&mut c, old.goto(&list, k, Nav::Goto, Some(0), &s, &|_| None, &mut tokens, now).cmds);
        assert_eq!(c.get("lights.effect.beat.active"), Some(&Value::Bool(true)));
        let new_list = Selection::parse(&Value::map().with("palette", "wash")).unwrap().compose(&s, &Controls::default()).unwrap().0;
        apply(&mut c, old.refresh(&new_list, &s, None, &|_| None, &mut tokens, now).cmds);
        assert_eq!(c.get("lights.effect.beat.active"), Some(&Value::Bool(false)));
        // A retired value may no longer appear in `applied`, but still belongs to the generation.
        apply(&mut c, vec![Command::new(Origin::Rule, se_proto::Op::Set { address: "lx.retired".into(), value: Value::Float(0.9) }).with_key(old.key.clone()).with_priority(Some(201))]);
        s.palettes.insert("manual".into(), palette::Palette::parse("manual", &toml::from_str("[set]\na={intensity=0.4,color='#00ff00'}").unwrap()).unwrap());
        let selected = Selection::parse(&Value::map().with("palette", "manual")).unwrap();
        let (manual_list, k) = selected.compose(&s, &Controls::default()).unwrap();
        let mut manual = Playback::layer("layer:rhythm:28", 298, Origin::Ui, None, None, Controls::default(), &s);
        apply(&mut c, manual.goto(&manual_list, k, Nav::Goto, Some(0), &s, &|_| None, &mut tokens, now).cmds);
        let released = old.release(100, now);
        apply(&mut c, released.cmds);
        for (_, timer) in released.timers {
            if let Timer::Release { cmds, .. } = timer { apply(&mut c, cmds.into_iter().map(|(_, _, cmd)| cmd).collect()); }
        }
        assert!((c.get("lights.a.intensity").unwrap().as_f64().unwrap() - 0.4).abs() < 1e-6);
        assert_eq!(c.get("lights.a.color").unwrap().as_color().unwrap(), [0.0, 1.0, 0.0, 1.0]);
        assert_eq!(c.get("lights.b.intensity").unwrap().as_f64(), Some(0.0));
        assert_eq!(c.get("lx.retired").unwrap().as_f64(), Some(0.0));
        assert!(c.state().params().iter().all(|p| p.overrides.iter().all(|o| o.key != "layer:rhythm:27")));
    }

    #[test]
    fn validation_rejects_unknown_content_capability_and_invalid_controls() {
        let s = show();
        assert!(Selection::parse(&Value::map()).is_err());
        assert!(Selection::parse(&Value::map().with("palette", "missing")).unwrap().compose(&s, &Controls::default()).is_err());
        assert!(Controls::default().patched(&Value::map().with("coverage", "missing"), &s).is_err());
        assert!(Controls::default().patched(&Value::map().with("energy", f64::NAN), &s).is_err());
        let mut s = s;
        s.effects.push(crate::effects::EffectDef::parse("motion", &toml::from_str("kind = \"circle\"\norder = \"index\"").unwrap()).unwrap());
        assert!(Selection::parse(&Value::map().with("effects", "motion")).unwrap().compose(&s, &Controls::default()).is_err());
        let empty = Controls::default().patched(&Value::map().with("coverage", Value::List(Vec::new())), &s).unwrap();
        assert!(empty.mask(&s).unwrap().iter().all(|v| !v));
        assert_eq!(boundary(4.0, 1.0).unwrap(), 5.0);
        assert_eq!(boundary(4.1, 0.5).unwrap(), 4.5);
        assert!(boundary(4.0, 0.0).is_err());
    }

    #[test]
    fn authored_fixture_and_group_addresses_cannot_escape_layer_coverage() {
        let mut s = show();
        s.lists.insert("addressed".into(), CueList::parse("addressed", &toml::from_str("[[cue]]\n[cue.addresses]\n\"lights.group.all.intensity\" = 0.8\n\"lights.b.color\" = \"#00ff00\"").unwrap()).unwrap());
        let selection = Selection::parse(&Value::map().with("cuelist", "addressed")).unwrap();
        let controls = Controls::default().patched(&Value::map().with("coverage", "front").with("brightness", 0.5), &s).unwrap();
        let (list, k) = selection.compose(&s, &controls).unwrap();
        let mut pb = Playback::layer("layer:base:1", 200, Origin::Rule, None, None, controls, &s);
        let out = pb.goto(&list, k, Nav::Goto, Some(0), &s, &|_| None, &mut 0, Instant::now());
        let mut c = core(&s);
        apply(&mut c, out.cmds);
        assert!((c.get("lights.a.intensity").unwrap().as_f64().unwrap() - 0.4).abs() < 1e-6);
        assert_eq!(c.get("lights.b.intensity").unwrap().as_f64(), Some(0.0));
        assert!(!pb.applied.contains_key("lights.b.color"));
        s.lists.insert("global".into(), CueList::parse("global", &toml::from_str("[[cue]]\n[cue.addresses]\n\"lights.master\" = 0.0").unwrap()).unwrap());
        assert!(Selection::parse(&Value::map().with("cuelist", "global")).unwrap().compose(&s, &Controls::default()).is_err());
    }
    #[test]
    fn beat_wait_loops_follow_variable_tempo_without_accumulating_poll_lateness() {
        let mut s = show();
        s.lists.insert("musical".into(), CueList::parse("musical", &toml::from_str(
            "loop=true\n[[cue]]\nwait_beats=16\n[cue.set]\na={intensity=0.2}\n[[cue]]\nwait_beats=16\n[cue.set]\na={intensity=0.4}"
        ).unwrap()).unwrap());
        let (list, _) = Selection::parse(&Value::map().with("cuelist", "musical")).unwrap().compose(&s, &Controls::default()).unwrap();
        let mut pb = Playback::layer("layer:rhythm:1", 201, Origin::Rule, None, None, Controls::default(), &s);
        let mut core = core(&s);
        let mut clock = crate::effects::BeatClock::default();
        let mut tokens = 0;
        let now = Instant::now();
        pb.beat_anchor = Some(0.0);
        let mut out = pb.goto(&list, 0, Nav::Goto, Some(0), &s, &|_| None, &mut tokens, now);
        for turn in 0..6 {
            let (epoch, deadline) = out.timers.iter().find_map(|(_, t)| match t {
                Timer::BeatAdvance { epoch, boundary: Some(boundary), .. } => Some((*epoch, *boundary)),
                _ => None,
            }).unwrap();
            apply(&mut core, out.cmds);
            let expected = if turn % 2 == 0 { 0.2 } else { 0.4 };
            assert!((core.get("lights.a.intensity").unwrap().as_f64().unwrap() - expected).abs() < 1e-6);
            assert_eq!(deadline, 16.0 * (turn + 1) as f64, "poll overshoot must never shift the next authored bar");
            let bpm = if turn < 2 { 90.0 } else { 150.0 };
            let before = (deadline - 0.5) as f32;
            let position = clock.update(15.5 * 60.0 / bpm as f64, Some(before.fract()), Some(bpm), Some(1.0), Some(before));
            assert_eq!(pb.beat_due(epoch, deadline, position), Some(false));
            let after = (deadline + 0.03125) as f32;
            let position = clock.update(0.53125 * 60.0 / bpm as f64, Some(after.fract()), Some(bpm), Some(1.0), Some(after));
            assert_eq!(pb.beat_due(epoch, deadline, position), Some(true));
            let next = (turn + 1) % 2;
            out = pb.goto(&list, next, Nav::Go, Some(0), &s, &|_| None, &mut tokens, now);
            assert_eq!(pb.beat_due(epoch, deadline, position), None, "a consumed timer cannot navigate the new cue twice");
        }
    }

}
