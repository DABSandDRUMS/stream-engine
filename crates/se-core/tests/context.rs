//! Operator effects/lights switches and the context layer (docs/context.md).

use se_core::config::SourceFile;
use se_core::{Config, Core, Input, Output};
use se_proto::{Actor, Command, Event, Op, Origin, Role, Value};

const MS: u64 = 1_000_000;

fn file(kind: &str, name: &str, src: &str) -> SourceFile {
    SourceFile { kind: kind.into(), name: name.into(), path: format!("{kind}/{name}.toml"), table: toml::from_str(src).unwrap() }
}

fn core_with(project: &str, extra: &[SourceFile]) -> Core {
    let mut files = vec![file("project", "project", project)];
    files.extend_from_slice(extra);
    let cfg = Config::build(&files);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    Core::new(cfg, 1_000 * MS)
}

fn run(c: &mut Core, ms: u64) -> Vec<Output> {
    let mut out = Vec::new();
    for _ in 0..(ms * MS).div_ceil(c.period()) {
        c.step();
        out.extend(c.drain_outputs());
    }
    out
}

fn cmd(c: &mut Core, origin: Origin, op: Op) {
    c.submit(Input::Command { cmd: Command::new(origin, op) });
}

fn act(c: &mut Core, origin: Origin, name: &str) {
    cmd(c, origin, Op::Action { name: name.into(), args: Value::Null });
}

fn fire(c: &mut Core, origin: Origin, name: &str) {
    cmd(c, origin, Op::PresetFire { name: name.into(), payload: Value::Null });
}

fn trigger(c: &mut Core, origin: Origin, address: &str) {
    cmd(c, origin, Op::Trigger { address: address.into(), payload: Value::map().with("hold", "latch") });
}

fn on(c: &Core, a: &str) -> bool {
    c.get(a).is_some_and(Value::truthy)
}

fn errors(out: &[Output]) -> Vec<String> {
    out.iter().filter_map(|o| if let Output::Ack { ok: false, error: Some(e), .. } = o { Some(e.clone()) } else { None }).collect()
}

fn events(out: &[Output], ty: &str) -> Vec<Event> {
    out.iter().filter_map(|o| if let Output::Event(e) = o { (e.ty == ty).then(|| e.clone()) } else { None }).collect()
}

fn sig(c: &Core, n: &str) -> f32 {
    c.signals().get(n).unwrap_or(f32::NAN)
}

fn viewer_event(ty: &str) -> Input {
    let mut e = Event::new(ty, Origin::Twitch, Value::map().with("user", "drumfan"));
    e.actor = Some(Actor { platform: "twitch".into(), id: "u1".into(), name: "drumfan".into(), roles: vec![Role::Everyone] });
    Input::Event { event: e }
}

fn fx_core() -> Core {
    core_with(
        "schema = 1\nstart_mode = \"live\"\n[fx]\nexempt = [\"win31_alerts\"]",
        &[
            file("presets", "moment", "lane = \"moment\"\nhold = \"20s\"\nfx = [{ name = \"patch.blue_screen\", hold = \"20s\" }]"),
            file("presets", "moment2", "lane = \"moment\"\nhold = \"2s\"\nfx = [{ name = \"patch.crt\" }]"),
            file("presets", "alert", "fx = [{ name = \"patch.win31_alerts\", hold = \"5s\" }]"),
            file("presets", "warm", "hold = \"20s\"\nset = { \"warm_level\" = 1.0 }"),
            file("rules", "chat", "[[rule]]\nwhen = \"twitch.redeem\"\ndo = [\"patch.confetti.trigger\"]"),
        ],
    )
}

#[test]
fn effects_off_refuses_automatic_and_viewer_effects_and_releases_running_ones() {
    let mut c = fx_core();
    assert!(on(&c, "fx.enabled"), "effects start on");
    fire(&mut c, Origin::Ui, "moment");
    fire(&mut c, Origin::Ui, "moment2");
    trigger(&mut c, Origin::Ui, "patch.confetti");
    trigger(&mut c, Origin::Ui, "patch.win31_alerts");
    run(&mut c, 50);
    assert!(on(&c, "preset.moment.active") && on(&c, "patch.blue_screen.active") && on(&c, "patch.confetti.active"));
    assert!(!on(&c, "preset.moment2.active"), "waits in the lane");

    // viewers, patches and rules may not switch effects
    for o in [Origin::Twitch, Origin::Chat, Origin::Patch, Origin::Rule, Origin::Timeline] {
        act(&mut c, o, "fx.off");
        let refused = errors(&run(&mut c, 20));
        assert_eq!(refused.len(), 1, "{o:?}: {refused:?}");
    }
    assert!(on(&c, "fx.enabled"));

    act(&mut c, Origin::Ui, "fx.off");
    let out = run(&mut c, 20);
    assert!(errors(&out).is_empty(), "{out:?}");
    assert!(!on(&c, "fx.enabled"));
    let changed = events(&out, "fx.changed");
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].payload.get_path("enabled"), Some(&Value::Bool(false)));
    assert!(!on(&c, "preset.moment.active"), "running effect presets are released");
    run(&mut c, 2_000);
    assert!(!on(&c, "patch.blue_screen.active") && !on(&c, "patch.confetti.active"), "effect envelopes let go");
    assert!(on(&c, "patch.win31_alerts.active"), "exempt patches keep running");
    assert!(!on(&c, "preset.moment2.active"), "the lane queue was dropped");

    // viewer and automatic firings are refused with the refund error
    fire(&mut c, Origin::Twitch, "moment");
    trigger(&mut c, Origin::Chat, "patch.confetti");
    fire(&mut c, Origin::Rule, "moment");
    trigger(&mut c, Origin::Patch, "fx.glitch");
    let refused = errors(&run(&mut c, 20));
    assert_eq!(refused, vec!["effects are off"; 4], "{refused:?}");
    c.submit(viewer_event("twitch.redeem"));
    run(&mut c, 20);
    assert!(!on(&c, "patch.confetti.active"), "a rule on a viewer event can't trigger a patch");
    assert!(c.trace().recent(200).iter().any(|r| r.label == "effects are off"), "the rule's refusal is traced");

    // exempt patches and non-effect presets still fire for everyone; the operator fires anything
    trigger(&mut c, Origin::Twitch, "patch.win31_alerts");
    fire(&mut c, Origin::Twitch, "alert");
    fire(&mut c, Origin::Twitch, "warm");
    assert!(errors(&run(&mut c, 20)).is_empty());
    assert!(on(&c, "preset.alert.active") && on(&c, "preset.warm.active"));
    fire(&mut c, Origin::Ui, "moment");
    trigger(&mut c, Origin::Deck, "patch.confetti");
    assert!(errors(&run(&mut c, 20)).is_empty());
    assert!(on(&c, "preset.moment.active") && on(&c, "patch.confetti.active"));

    // kept across a restart
    let rs = c.runtime_state();
    let mut restored = fx_core();
    restored.restore(&rs);
    run(&mut restored, 20);
    assert!(!on(&restored, "fx.enabled"));

    act(&mut c, Origin::Ui, "fx.toggle");
    let out = run(&mut c, 20);
    assert!(on(&c, "fx.enabled"));
    assert_eq!(events(&out, "fx.changed")[0].payload.get_path("enabled"), Some(&Value::Bool(true)));
    fire(&mut c, Origin::Twitch, "moment2");
    assert!(errors(&run(&mut c, 20)).is_empty(), "viewers may fire effects again");
}

#[test]
fn scene_on_enter_runs_as_operator_content_while_effects_are_off() {
    let mut c = core_with(
        "schema = 1",
        &[
            file("scenes", "a", "key = 1"),
            file("scenes", "b", "key = 2\non_enter = [\"preset.fire moment\", \"patch.title.trigger\"]"),
            file("presets", "moment", "lane = \"moment\"\nhold = \"5s\"\nset = { \"moment_level\" = 1.0 }"),
        ],
    );
    act(&mut c, Origin::Ui, "fx.off");
    cmd(&mut c, Origin::Rule, Op::SceneCut { scene: "b".into(), transition: Some("cut".into()) });
    run(&mut c, 20);
    assert_eq!(c.get("show.scene.program").and_then(Value::as_str), Some("b"));
    assert!(on(&c, "preset.moment.active") && on(&c, "patch.title.active"));
}

#[test]
fn lights_auto_off_is_operator_only_and_persists() {
    let chat_rule = file("rules", "chat", "[[rule]]\nwhen = \"twitch.chat\"\ndo = [\"lights.auto.on\"]");
    let mut c = core_with("schema = 1\nstart_mode = \"live\"", &[chat_rule]);
    assert!(on(&c, "lights.auto"));
    for o in [Origin::Twitch, Origin::Relay, Origin::Rule, Origin::System] {
        act(&mut c, o, "lights.auto.off");
    }
    c.submit(Input::Command {
        cmd: Command {
            actor: Some(Actor { platform: "twitch".into(), id: "u1".into(), name: "v".into(), roles: vec![Role::Vip] }),
            ..Command::new(Origin::Cli, Op::Action { name: "lights.auto.off".into(), args: Value::Null })
        },
    });
    assert_eq!(errors(&run(&mut c, 20)).len(), 5);
    assert!(on(&c, "lights.auto"));

    act(&mut c, Origin::Ui, "lights.auto.off");
    let out = run(&mut c, 20);
    assert!(!on(&c, "lights.auto"));
    let changed = events(&out, "lights.auto.changed");
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].payload.get_path("enabled"), Some(&Value::Bool(false)));

    // a chat command from a viewer can't switch it back; the channel owner's can
    c.submit(viewer_event("twitch.chat"));
    run(&mut c, 20);
    assert!(!on(&c, "lights.auto"));
    let mut owner = Event::new("twitch.chat", Origin::Twitch, Value::Null);
    owner.actor = Some(Actor { platform: "twitch".into(), id: "me".into(), name: "me".into(), roles: vec![Role::Owner] });
    c.submit(Input::Event { event: owner });
    run(&mut c, 20);
    assert!(on(&c, "lights.auto"));
    act(&mut c, Origin::Midi, "lights.auto.toggle");
    run(&mut c, 20);
    let rs = c.runtime_state();
    let mut restored = core_with("schema = 1", &[]);
    restored.restore(&rs);
    run(&mut restored, 20);
    assert!(!on(&restored, "lights.auto"), "kept across a restart");
}

fn context_event(c: &mut Core, ty: &str) {
    c.submit(Input::Event { event: Event::new(ty, Origin::System, Value::Null) });
}

#[test]
fn automatic_fx_switch_is_operator_only_durable_and_generic_set_has_no_override() {
    let mut c = core_with("schema = 1\nstart_mode = \"live\"", &[]);
    assert!(!on(&c, "fx.auto"));
    assert!(on(&c, "fx.enabled") && on(&c, "lights.auto"));
    for origin in [Origin::Twitch, Origin::Chat, Origin::Rule, Origin::System, Origin::Patch, Origin::Timeline] {
        act(&mut c, origin, "fx.auto.on");
        cmd(&mut c, origin, Op::Set { address: "fx.auto".into(), value: Value::Bool(true) });
    }
    c.submit(Input::Command {
        cmd: Command {
            actor: Some(Actor { platform: "twitch".into(), id: "v".into(), name: "v".into(), roles: vec![Role::Mod] }),
            ..Command::new(Origin::Cli, Op::Action { name: "fx.auto.toggle".into(), args: Value::Null })
        },
    });
    assert_eq!(errors(&run(&mut c, 20)), vec!["only the operator can switch automatic effects"; 13]);
    assert!(!on(&c, "fx.auto"));

    cmd(&mut c, Origin::Deck, Op::Set { address: "fx.auto".into(), value: Value::Bool(true) });
    let out = run(&mut c, 20);
    assert!(errors(&out).is_empty());
    assert!(on(&c, "fx.auto"));
    assert_eq!(events(&out, "fx.auto.changed")[0].payload.get_path("enabled"), Some(&Value::Bool(true)));
    let rs = c.runtime_state();
    assert_eq!(rs.bases.iter().find(|(a, _)| a == "fx.auto").map(|(_, v)| v), Some(&Value::Bool(true)));
    assert!(!rs.overrides.iter().any(|(a, _)| a == "fx.auto"));
    let mut restored = core_with("schema = 1", &[]);
    restored.restore(&rs);
    run(&mut restored, 20);
    assert!(on(&restored, "fx.auto"));
    act(&mut restored, Origin::Deck, "fx.auto.toggle");
    run(&mut restored, 20);
    assert!(!on(&restored, "fx.auto"));
    act(&mut restored, Origin::Deck, "fx.auto.on");
    act(&mut restored, Origin::Deck, "fx.auto.off");
    act(&mut restored, Origin::Deck, "fx.auto.off");
    run(&mut restored, 20);
    let mut restarted = core_with("schema = 1", &[]);
    restarted.restore(&restored.runtime_state());
    run(&mut restarted, 20);
    assert!(!on(&restarted, "fx.auto"));
    assert!(on(&restarted, "fx.enabled") && on(&restarted, "lights.auto"));

    // Older runtime snapshots lack the new opt-in base.
    let mut older = rs;
    older.bases.retain(|(a, _)| a != "fx.auto");
    let mut upgraded = core_with("schema = 1", &[]);
    upgraded.restore(&older);
    run(&mut upgraded, 20);
    assert!(!on(&upgraded, "fx.auto"));
}

#[test]
fn automatic_fx_gate_uses_context_identity_not_rule_origin_and_leaves_lights_and_spend_alone() {
    let mut c = core_with(
        "schema = 1\nstart_mode = \"live\"\n[fx]\nexempt = [\"automatic\"]",
        &[
            file("presets", "automatic", "hold = \"2s\"\nfx = [{ name = \"patch.automatic\", hold = \"2s\" }]"),
            file("presets", "light", "lane = \"lights\"\nhold = \"2s\"\nset = { light_marker = 1.0 }"),
            file("presets", "light_roll", "pick = [{ name = \"light\" }]"),
            file("presets", "video_roll", "pick = [{ name = \"automatic\" }]"),
            file("rules", "automatic", "[[rule]]\nwhen = \"context.musical_fx\"\ndo = [\"context.spend\", \"preset.fire video_roll\", \"patch.direct.trigger hold=latch\", \"preset.fire light_roll\", \"lights.layer.release layer=accent owner=context\"]"),
            file("rules", "viewers", r#"
[[rule]]
when = "twitch.redeem"
do = ["patch.paid.trigger hold=latch"]
[[rule]]
when = "twitch.sub"
do = ["patch.sub.trigger hold=latch"]
[[rule]]
when = "twitch.cheer"
do = ["patch.bits.trigger hold=latch"]
[[rule]]
when = "queue.song_started"
do = ["patch.song_ritual.trigger hold=latch"]
"#),
        ],
    );
    run(&mut c, 20);
    let budget = sig(&c, "context.budget");
    context_event(&mut c, "context.musical_fx");
    let out = run(&mut c, 20);
    assert!(!on(&c, "preset.automatic.active") && !on(&c, "patch.direct.active"));
    assert_eq!(sig(&c, "context.budget"), budget - 1.0);
    assert!(on(&c, "preset.light.active"));
    assert!(out.iter().any(|o| matches!(o, Output::Action(command) if matches!(&command.op, Op::Action { name, .. } if name == "lights.layer.release"))));
    assert!(c.trace().recent(200).iter().any(|r| r.label == "automatic effects are off"));

    for ty in ["twitch.redeem", "twitch.sub", "twitch.cheer"] {
        c.submit(viewer_event(ty));
    }
    context_event(&mut c, "queue.song_started");
    trigger(&mut c, Origin::Deck, "patch.manual");
    trigger(&mut c, Origin::Rule, "patch.noncontext");
    run(&mut c, 20);
    for address in ["paid", "sub", "bits", "song_ritual", "manual", "noncontext"] {
        assert!(on(&c, &format!("patch.{address}.active")), "{address}");
    }

    act(&mut c, Origin::Deck, "fx.auto.on");
    run(&mut c, 20);
    context_event(&mut c, "context.musical_fx");
    run(&mut c, 20);
    assert!(on(&c, "preset.automatic.active") && on(&c, "patch.direct.active"));
    act(&mut c, Origin::Deck, "fx.auto.off");
    run(&mut c, 20);
    assert!(on(&c, "preset.automatic.active") && on(&c, "patch.automatic.active"), "running automatic effects keep their authored hold");
    assert!(on(&c, "patch.paid.active"), "viewer effect was not released");
    run(&mut c, 3_000);
    assert!(!on(&c, "preset.automatic.active"), "automatic preset releases naturally");
    assert!(on(&c, "patch.paid.active"));
}

#[test]
fn automatic_fx_on_does_not_bypass_overall_effects_kill_switch() {
    let mut c = core_with(
        "schema = 1\nstart_mode = \"live\"",
        &[file("rules", "automatic", "[[rule]]\nwhen = \"context.musical_fx\"\ndo = [\"patch.automatic.trigger\"]")],
    );
    act(&mut c, Origin::Deck, "fx.auto.on");
    act(&mut c, Origin::Deck, "fx.off");
    run(&mut c, 20);
    context_event(&mut c, "context.musical_fx");
    run(&mut c, 20);
    assert!(!on(&c, "patch.automatic.active"));
    assert!(c.trace().recent(200).iter().any(|r| r.label == "effects are off"));
    act(&mut c, Origin::Deck, "fx.on");
    run(&mut c, 20);
    context_event(&mut c, "context.musical_fx");
    run(&mut c, 20);
    assert!(on(&c, "patch.automatic.active"));
}

#[test]
fn automatic_pending_starts_are_canceled_by_off_even_after_reenable() {
    for kind in ["delay", "lane", "quantize", "conflict"] {
        let preset = match kind {
            "lane" => "lane = \"video\"\nhold = \"100ms\"\nfx = [{ name = \"patch.automatic\" }]",
            "quantize" => "quantize = \"beat\"\nhold = \"100ms\"\nfx = [{ name = \"patch.automatic\" }]",
            "conflict" => "conflict = \"queue\"\nhold = \"200ms\"\nfx = [{ name = \"patch.automatic\" }]",
            _ => "hold = \"100ms\"\nfx = [{ name = \"patch.automatic\" }]",
        };
        let automatic_rule = if kind == "delay" {
            "[[rule]]\nwhen = \"context.musical_fx\"\ndo = [\"wait 200ms\", \"preset.fire automatic\", \"patch.delayed.trigger hold=latch\", \"preset.fire light\"]"
        } else {
            "[[rule]]\nwhen = \"context.musical_fx\"\ndo = [\"preset.fire automatic\", \"wait 200ms\", \"preset.fire light\"]"
        };
        let mut c = core_with(
            "schema = 1\nstart_mode = \"live\"",
            &[
                file("presets", "automatic", preset),
                file("presets", "holder", "lane = \"video\"\nhold = \"200ms\"\nset = { holder_marker = 1.0 }"),
                file("presets", "light", "hold = \"100ms\"\nset = { light_marker = 1.0 }"),
                file("rules", "automatic", automatic_rule),
                file("rules", "viewer", "[[rule]]\nwhen = \"twitch.redeem\"\ndo = [\"wait 200ms\", \"patch.viewer.trigger hold=latch\"]"),
            ],
        );
        act(&mut c, Origin::Deck, "fx.auto.on");
        if kind == "lane" { fire(&mut c, Origin::Deck, "holder"); }
        if kind == "conflict" { fire(&mut c, Origin::Deck, "automatic"); }
        run(&mut c, 20);
        c.submit(Input::Signals { values: vec![("beat.bpm".into(), 120.0), ("beat.phase".into(), 0.0)] });
        context_event(&mut c, "context.musical_fx");
        c.submit(viewer_event("twitch.redeem"));
        let out = run(&mut c, 20);
        assert!(events(&out, "preset.automatic.fired").is_empty(), "{kind}: starts pending");
        act(&mut c, Origin::Deck, "fx.auto.off");
        act(&mut c, Origin::Deck, "fx.auto.on");
        let out = run(&mut c, 1_000);
        assert!(events(&out, "preset.automatic.fired").is_empty(), "{kind}: canceled start must not resurrect");
        assert!(!on(&c, "patch.delayed.active"), "{kind}");
        assert!(on(&c, "patch.viewer.active"), "{kind}: viewer delay must survive");
        assert_eq!(events(&out, "preset.light.fired").len(), 1, "{kind}: lighting delay must survive");
    }
}

#[test]
fn conflict_queue_retains_context_identity_for_automatic_release_commands() {
    let mut c = core_with(
        "schema = 1\nstart_mode = \"live\"",
        &[
            file("presets", "automatic", "conflict = \"queue\"\nhold = \"200ms\"\nfx = [{ name = \"patch.automatic\" }]\non_release = [\"patch.release_new.trigger\"]"),
            file("rules", "automatic", "[[rule]]\nwhen = \"context.musical_fx\"\ndo = [\"preset.fire automatic\"]"),
        ],
    );
    act(&mut c, Origin::Deck, "fx.auto.on");
    fire(&mut c, Origin::Deck, "automatic");
    run(&mut c, 20);
    context_event(&mut c, "context.musical_fx");
    run(&mut c, 20);
    // Let the conflict queue begin normally, retaining its context event for release commands.
    let out = run(&mut c, 300);
    assert_eq!(events(&out, "preset.automatic.fired").len(), 1);
    assert!(on(&c, "preset.automatic.active"));
    act(&mut c, Origin::Deck, "fx.auto.off");
    let out = run(&mut c, 300);
    assert!(events(&out, "patch.release_new.trigger").is_empty(), "automatic release may fade existing effects, not start new ones after off");
    assert!(!on(&c, "preset.automatic.active"));
    assert!(c.trace().recent(200).iter().any(|r| r.label == "automatic effects are off"));
}

// ---- context layer --------------------------------------------------------------------------

/// Song audio: short-term loudness, flux and the onset envelopes' level.
fn song(c: &mut Core, lufs: f32, flux: f32, env: f32) {
    let level = if lufs <= -70.0 { 0.0 } else { 10f32.powf(lufs / 20.0) };
    c.submit(Input::Signals {
        values: vec![
            ("music.level".into(), level),
            ("music.lufs".into(), lufs),
            ("music.flux".into(), flux),
            ("music.kick".into(), env),
            ("music.snare".into(), env),
            ("music.hat".into(), env),
            ("music.bass".into(), 0.1),
            ("music.mid".into(), 0.1),
            ("music.high".into(), 0.05),
            ("music.centroid".into(), 0.5),
        ],
    });
}

fn baseline(c: &mut Core) {
    song(c, -20.0, 0.5, 0.2);
}

fn big(c: &mut Core) {
    song(c, -10.0, 1.5, 0.5);
}

fn drop(c: &mut Core) {
    c.submit(Input::Event { event: Event::new("music.drop", Origin::System, Value::map().with("strength", 0.8)) });
}

#[test]
fn song_peak_fires_once_for_a_drop_with_sustained_energy_and_respects_cooldown() {
    let mut c = core_with("schema = 1", &[]);
    baseline(&mut c);
    let out = run(&mut c, 40_000);
    let s = sig(&c, "context.song");
    assert!((0.4..0.6).contains(&s), "steady song sits mid-scale: {s}");

    // a big section without a drop/section change is not a song peak
    big(&mut c);
    assert!(events(&run(&mut c, 8_000), "context.song_peak").is_empty());
    assert!(events(&out, "context.song_peak").is_empty());
    baseline(&mut c);
    run(&mut c, 30_000);

    // drop, then sustained high energy: exactly one
    drop(&mut c);
    big(&mut c);
    let peaks = events(&run(&mut c, 25_000), "context.song_peak");
    assert_eq!(peaks.len(), 1, "{peaks:?}");
    let strength = peaks[0].payload.get_path("strength").and_then(Value::as_f64).unwrap();
    assert!(strength > 0.5 && strength <= 1.0, "{strength}");
    assert!(peaks[0].payload.get_path("mood").and_then(Value::as_str).is_some());

    // another drop within 45 s of it: cooldown
    baseline(&mut c);
    run(&mut c, 5_000);
    drop(&mut c);
    big(&mut c);
    assert!(events(&run(&mut c, 10_000), "context.song_peak").is_empty(), "cooling down");

    // a drop that dies straight away is no peak either
    baseline(&mut c);
    run(&mut c, 15_000);
    drop(&mut c);
    assert!(events(&run(&mut c, 15_000), "context.song_peak").is_empty(), "needs sustained energy");

    // cooled down: the next one fires
    drop(&mut c);
    big(&mut c);
    assert_eq!(events(&run(&mut c, 10_000), "context.song_peak").len(), 1);
}

#[test]
fn song_peak_never_fires_off_a_drop_while_the_host_talks() {
    for talking in [false, true] {
        let mut c = core_with("schema = 1", &[]);
        c.submit(Input::Signal { name: "mic.talking".into(), value: 0.0 });
        baseline(&mut c);
        run(&mut c, 40_000);
        // the song stops; the host talks (or not) over the gap
        song(&mut c, -70.0, 0.0, 0.0);
        c.submit(Input::Signal { name: "mic.talking".into(), value: if talking { 1.0 } else { 0.0 } });
        run(&mut c, 3_000);
        assert_eq!(sig(&c, "context.talking"), if talking { 1.0 } else { 0.0 });
        // the drop hits as the song comes back in
        c.submit(Input::Signal { name: "mic.talking".into(), value: 0.0 });
        drop(&mut c);
        big(&mut c);
        let peaks = events(&run(&mut c, 12_000), "context.song_peak");
        assert_eq!(peaks.len(), usize::from(!talking), "talking={talking}: {peaks:?}");
        assert_eq!(sig(&c, "context.talking"), 0.0, "talking ends once the song is back");
    }
}

fn band(c: &mut Core, ty: &str, velocity: f64) {
    c.submit(Input::Event { event: Event::new(ty, Origin::System, Value::map().with("velocity", velocity)) });
}

fn grid(c: &mut Core, position: f32) {
    c.submit(Input::Signals { values: vec![("beat.bpm".into(), 120.0), ("beat.confidence".into(), 1.0), ("beat.position".into(), position)] });
}

/// Six snare hits 150 ms apart, then a kick `land_ms` later at bar position `position`.
fn fill(c: &mut Core, position: f32, land_ms: u64) -> Vec<Output> {
    let mut out = Vec::new();
    for _ in 0..6 {
        band(c, "band.snare", 0.7);
        out.extend(run(c, 150));
    }
    out.extend(run(c, land_ms.saturating_sub(150)));
    grid(c, position);
    band(c, "band.kick", 0.9);
    out.extend(run(c, 50));
    out
}

#[test]
fn fill_landed_on_a_snare_burst_and_a_kick_near_the_downbeat() {
    let mut c = core_with("schema = 1", &[]);
    grid(&mut c, 0.0);
    // a plain kick, or a groove's sparse snares, is no fill
    band(&mut c, "band.kick", 1.0);
    let mut out = run(&mut c, 500);
    for _ in 0..4 {
        band(&mut c, "band.snare", 0.7);
        out.extend(run(&mut c, 500));
    }
    band(&mut c, "band.kick", 1.0);
    out.extend(run(&mut c, 50));
    assert!(events(&out, "context.fill_landed").is_empty());

    let landed = events(&fill(&mut c, 8.1, 200), "context.fill_landed");
    assert_eq!(landed.len(), 1);
    let strength = landed[0].payload.get_path("strength").and_then(Value::as_f64).unwrap();
    assert!(strength > 0.5 && strength <= 1.0, "{strength}");
    assert!(events(&fill(&mut c, 12.0, 200), "context.fill_landed").is_empty(), "20 s cooldown");
    run(&mut c, 20_000);
    assert!(events(&fill(&mut c, 17.5, 200), "context.fill_landed").is_empty(), "mid-bar is no landing");
    assert!(events(&fill(&mut c, 20.0, 1_000), "context.fill_landed").is_empty(), "landed too late");
    assert_eq!(events(&fill(&mut c, 23.9, 200), "context.fill_landed").len(), 1, "just before the one counts");
}

#[test]
fn mood_follows_the_genres_and_changes_at_most_every_20_seconds() {
    let mut c = core_with("schema = 1", &[]);
    assert_eq!(c.get("context.mood").and_then(Value::as_str), Some("none"));
    let genres = |c: &mut Core, g: &[&str]| {
        c.submit(Input::Publish { address: "song.current.genres".into(), value: Value::List(g.iter().map(|s| Value::Str(s.to_string())).collect()) })
    };
    genres(&mut c, &["metalcore", "pop"]);
    baseline(&mut c);
    let out = run(&mut c, 10_000);
    assert_eq!(c.get("context.mood").and_then(Value::as_str), Some("heavy"));
    let moods = events(&out, "context.mood");
    assert_eq!(moods.len(), 1);
    assert_eq!(moods[0].payload.get_path("previous").and_then(Value::as_str), Some("none"));

    genres(&mut c, &["funk"]);
    run(&mut c, 15_000);
    assert_eq!(c.get("context.mood").and_then(Value::as_str), Some("heavy"), "held until the next evaluation");
    let out = run(&mut c, 6_000);
    assert_eq!(c.get("context.mood").and_then(Value::as_str), Some("groove"));
    assert_eq!(events(&out, "context.mood")[0].payload.get_path("previous").and_then(Value::as_str), Some("heavy"));

    // no song: held
    genres(&mut c, &["lo-fi"]);
    song(&mut c, -70.0, 0.0, 0.0);
    run(&mut c, 45_000);
    assert_eq!(c.get("context.mood").and_then(Value::as_str), Some("groove"));
}

#[test]
fn budget_counts_down_per_spend_and_rolls_over_after_an_hour() {
    let mut c = core_with(
        "schema = 1\n[context]\nauto_per_hour = 2",
        &[file("rules", "auto", "[[rule]]\nwhen = \"test.moment\"\nif = \"context.budget >= 1\"\ndo = [\"context.spend\", \"emit test.spent\"]")],
    );
    run(&mut c, 20);
    assert_eq!(sig(&c, "context.budget"), 2.0);
    let mut spent = 0;
    for _ in 0..3 {
        c.submit(Input::Event { event: Event::new("test.moment", Origin::System, Value::Null) });
        spent += events(&run(&mut c, 20), "test.spent").len();
    }
    assert_eq!(spent, 2, "the rule's budget gate stops the third");
    assert_eq!(sig(&c, "context.budget"), 0.0);
    act(&mut c, Origin::Ui, "context.spend");
    assert_eq!(errors(&run(&mut c, 20)), ["no automatic moments left this hour"]);
    let later = c.now() + 3_600_000 * MS;
    c.advance_to(later);
    run(&mut c, 20);
    assert_eq!(sig(&c, "context.budget"), 2.0, "an hour later the spends have rolled off");
}

#[test]
fn chat_heat_drums_and_energy_rise_with_the_room() {
    let mut c = core_with("schema = 1", &[]);
    c.submit(Input::Signal { name: "twitch.chat_rate".into(), value: 5.0 });
    c.submit(Input::Signals { values: vec![("band.level".into(), 0.1), ("band.kick".into(), 0.2), ("band.snare".into(), 0.2), ("band.hat".into(), 0.2)] });
    baseline(&mut c);
    run(&mut c, 60_000);
    let (chat0, drums0, energy0) = (sig(&c, "context.chat"), sig(&c, "context.drums"), sig(&c, "context.energy"));
    assert!((0.2..0.5).contains(&chat0), "chat at its baseline: {chat0}");
    assert!((0.4..0.6).contains(&drums0), "drummer at the session norm: {drums0}");
    c.submit(Input::Signal { name: "twitch.chat_rate".into(), value: 20.0 });
    c.submit(Input::Signals { values: vec![("band.level".into(), 0.3), ("band.kick".into(), 0.5), ("band.snare".into(), 0.5), ("band.hat".into(), 0.5)] });
    c.submit(Input::Event { event: Event::new("twitch.raid", Origin::Twitch, Value::map().with("viewers", 50)) });
    big(&mut c);
    run(&mut c, 3_000);
    assert!(sig(&c, "context.chat") > 0.9, "{}", sig(&c, "context.chat"));
    assert!(sig(&c, "context.drums") > 0.85, "{}", sig(&c, "context.drums"));
    assert!(sig(&c, "context.energy") > energy0 + 0.25, "{energy0} → {}", sig(&c, "context.energy"));
}

fn room(c: &mut Core, hot: bool) {
    let (rate, lvl, env) = if hot { (20.0, 0.3, 0.5) } else { (0.0, 0.1, 0.2) };
    c.submit(Input::Signals {
        values: vec![
            ("twitch.chat_rate".into(), rate),
            ("band.level".into(), lvl),
            ("band.kick".into(), env),
            ("band.snare".into(), env),
            ("band.hat".into(), env),
        ],
    });
    if hot { big(c) } else { baseline(c) }
}

#[test]
fn peak_needs_sustained_energy_settles_after_and_waits_two_minutes() {
    let mut c = core_with("schema = 1", &[]);
    room(&mut c, false);
    run(&mut c, 60_000);
    // a short burst is no peak
    room(&mut c, true);
    let mut out = run(&mut c, 4_000);
    room(&mut c, false);
    out.extend(run(&mut c, 20_000));
    assert!(events(&out, "context.peak").is_empty() && events(&out, "context.settle").is_empty());

    room(&mut c, true);
    let out = run(&mut c, 15_000);
    let peaks = events(&out, "context.peak");
    assert_eq!(peaks.len(), 1, "{peaks:?}");
    assert!(peaks[0].payload.get_path("energy").and_then(Value::as_f64).unwrap() >= 0.72);
    room(&mut c, false);
    let out = run(&mut c, 20_000);
    assert_eq!(events(&out, "context.settle").len(), 1);

    // high again within two minutes: no peak, and no settle without one
    room(&mut c, true);
    let mut out = run(&mut c, 15_000);
    room(&mut c, false);
    out.extend(run(&mut c, 20_000));
    assert!(events(&out, "context.peak").is_empty() && events(&out, "context.settle").is_empty(), "cooling down");
    run(&mut c, 60_000);
    room(&mut c, true);
    assert_eq!(events(&run(&mut c, 15_000), "context.peak").len(), 1);
}

#[test]
fn musical_master_does_not_gate_unrelated_context_notifications_or_rituals() {
    let mut c = core_with("schema = 1", &[
        file("rules", "notification", "[[rule]]\nwhen = \"context.peak\"\ndo = [\"patch.notification.trigger hold=latch\"]\n[[rule]]\nwhen = \"context.test\"\ndo = [\"patch.ritual.trigger hold=latch\"]"),
    ]);
    context_event(&mut c, "context.peak");
    context_event(&mut c, "context.test");
    run(&mut c, 20);
    assert!(on(&c, "patch.notification.active") && on(&c, "patch.ritual.active"));
    act(&mut c, Origin::Deck, "fx.auto.off");
    run(&mut c, 20);
    assert!(on(&c, "patch.notification.active") && on(&c, "patch.ritual.active"));
}

#[test]
fn auto_effect_controls_persist_and_only_operator_can_edit_them() {
    let mut c = core_with("schema = 1", &[]);
    for origin in [Origin::Rule, Origin::Patch, Origin::Twitch, Origin::Chat, Origin::Timeline] {
        cmd(&mut c, origin, Op::SetBase { address: "fx.glitch.auto.enabled".into(), value: Value::Bool(false) });
        assert_eq!(errors(&run(&mut c, 40)).len(), 1, "{origin:?} must not edit auto policy");
    }
    let mut viewer = Command::new(Origin::Ui, Op::SetBase { address: "fx.glitch.auto.enabled".into(), value: Value::Bool(false) });
    viewer.actor = Some(Actor { platform: "twitch".into(), id: "mod".into(), name: "mod".into(), roles: vec![Role::Mod] });
    c.submit(Input::Command { cmd: viewer });
    assert_eq!(errors(&run(&mut c, 40)).len(), 1, "an unprivileged actor cannot borrow a UI origin");
    cmd(&mut c, Origin::Ui, Op::SetBase { address: "fx.glitch.auto.enabled".into(), value: Value::Bool(false) });
    cmd(&mut c, Origin::Ui, Op::SetBase { address: "fx.glitch.auto.interval".into(), value: Value::Float(300.0) });
    cmd(&mut c, Origin::Deck, Op::SetBase { address: "patch.custom.auto.interval".into(), value: Value::Float(90.0) });
    let out = run(&mut c, 40);
    assert!(errors(&out).is_empty());
    assert!(out.iter().any(|o| matches!(o, Output::RuntimeDirty)));
    assert!(!out.iter().any(|o| matches!(o, Output::PersistBase { address, .. } if address.contains(".auto."))),
        "runtime preferences are not unsupported project-file edits");
    let saved = c.runtime_state();
    let mut restored = core_with("schema = 1", &[]);
    restored.restore(&saved);
    run(&mut restored, 40);
    assert!(!on(&restored, "fx.glitch.auto.enabled"));
    assert_eq!(restored.get("fx.glitch.auto.interval").and_then(Value::as_f64), Some(300.0));
    assert_eq!(restored.get("patch.custom.auto.interval").and_then(Value::as_f64), Some(90.0));
}

#[test]
fn auto_effect_policy_gates_context_direct_presets_and_roulette_not_manual_or_twitch() {
    let mut c = core_with("schema = 1\nstart_mode = \"rehearsal\"", &[
        file("presets", "blocked", "fx = [{ name = \"patch.blocked\" }]"),
        file("presets", "allowed", "fx = [{ name = \"patch.allowed\" }]"),
        file("presets", "roll", "pick = [{ name = \"blocked\", w = 1000000 }, { name = \"allowed\" }]"),
        file("rules", "auto", "[[rule]]\nwhen = \"context.peak\"\ndo = [\"patch.blocked.trigger\", \"preset.fire blocked\", \"preset.fire roll\"]"),
        file("rules", "viewer", "[[rule]]\nwhen = \"twitch.redeem\"\ndo = [\"patch.blocked.trigger\", \"preset.fire blocked\"]"),
        file("rules", "ritual", "[[rule]]\nwhen = \"context.song_ended\"\ndo = [\"patch.blocked.trigger\"]"),
    ]);
    cmd(&mut c, Origin::Ui, Op::SetBase { address: "patch.blocked.auto.enabled".into(), value: Value::Bool(false) });
    run(&mut c, 40);
    context_event(&mut c, "context.peak");
    let out = run(&mut c, 40);
    assert!(events(&out, "patch.blocked.trigger").is_empty());
    assert!(events(&out, "preset.blocked.fired").is_empty());
    assert_eq!(events(&out, "preset.allowed.fired").len(), 1, "roulette filters before the weighted pick");
    trigger(&mut c, Origin::Deck, "patch.blocked");
    fire(&mut c, Origin::Ui, "blocked");
    let out = run(&mut c, 40);
    assert_eq!(events(&out, "patch.blocked.trigger").len(), 2);
    assert!(errors(&out).is_empty());
    c.submit(viewer_event("twitch.redeem"));
    let out = run(&mut c, 40);
    assert_eq!(events(&out, "patch.blocked.trigger").len(), 2, "paid viewer trigger and preset retain access");
    context_event(&mut c, "context.song_ended");
    assert_eq!(events(&run(&mut c, 40), "patch.blocked.trigger").len(), 1, "notifications/rituals are not automatic musical opportunities");
}

#[test]
fn auto_effect_cooldown_starts_only_after_success_and_ignores_manual_starts() {
    let mut c = core_with("schema = 1\nstart_mode = \"rehearsal\"", &[
        file("rules", "auto", "[[rule]]\nwhen = \"context.fill_landed\"\ndo = [\"fx.glitch.trigger hold=10ms\"]"),
    ]);
    c.submit(Input::DeclareTrigger { address: "fx.glitch".into(), spec: se_core::triggers::TriggerSpec {
        retrigger: se_core::config::Conflict::Reject, attack_ms: 0, release_ms: 0, ..Default::default()
    } });
    cmd(&mut c, Origin::Ui, Op::SetBase { address: "fx.glitch.auto.interval".into(), value: Value::Float(600.0) });
    cmd(&mut c, Origin::Ui, Op::Trigger { address: "fx.glitch".into(), payload: Value::map().with("hold", 500i64) });
    run(&mut c, 40);
    context_event(&mut c, "context.fill_landed");
    assert!(events(&run(&mut c, 40), "fx.glitch.trigger").is_empty(), "retrigger reject is a failed automatic start");
    run(&mut c, 600);
    context_event(&mut c, "context.fill_landed");
    assert_eq!(events(&run(&mut c, 40), "fx.glitch.trigger").len(), 1,
        "neither the manual start nor the failed automatic attempt started cooldown");
    run(&mut c, 40);
    context_event(&mut c, "context.fill_landed");
    assert!(events(&run(&mut c, 40), "fx.glitch.trigger").is_empty(), "successful auto start starts cooldown");
    cmd(&mut c, Origin::Twitch, Op::Trigger { address: "fx.glitch".into(), payload: Value::map().with("hold", 10i64) });
    assert_eq!(events(&run(&mut c, 40), "fx.glitch.trigger").len(), 1, "cooldown never denies Twitch");
    run(&mut c, 600_000);
    context_event(&mut c, "context.fill_landed");
    assert_eq!(events(&run(&mut c, 40), "fx.glitch.trigger").len(), 1);
}

#[test]
fn auto_effect_pending_preset_does_not_consume_interval_until_it_starts() {
    let mut c = core_with("schema = 1", &[
        file("presets", "holder", "lane = \"moment\"\nhold = \"300ms\"\nset = { \"holder\" = 1.0 }"),
        file("presets", "automatic", "lane = \"moment\"\nhold = \"100ms\"\nfx = [{ name = \"glitch\" }]"),
        file("rules", "auto", "[[rule]]\nwhen = \"context.fill_landed\"\ndo = [\"preset.fire automatic\"]"),
    ]);
    cmd(&mut c, Origin::Ui, Op::SetBase { address: "fx.glitch.auto.interval".into(), value: Value::Float(600.0) });
    fire(&mut c, Origin::Ui, "holder");
    run(&mut c, 40);
    context_event(&mut c, "context.fill_landed");
    assert!(events(&run(&mut c, 40), "fx.glitch.trigger").is_empty(), "lane queues the preset without starting an effect");
    let out = run(&mut c, 400);
    assert_eq!(events(&out, "fx.glitch.trigger").len(), 1, "queued request did not consume its own future start");
    run(&mut c, 600);
    context_event(&mut c, "context.fill_landed");
    assert!(events(&run(&mut c, 40), "fx.glitch.trigger").is_empty(), "the actual start consumed the interval");
}
