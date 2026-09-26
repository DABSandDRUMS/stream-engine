use se_core::config::SourceFile;
use se_core::{Config, Core, Input, Output};
use se_proto::{Actor, Command, Event, Meta, Op, Origin, Role, Value};

const MS: u64 = 1_000_000;

fn file(kind: &str, name: &str, src: &str) -> SourceFile {
    SourceFile { kind: kind.into(), name: name.into(), path: format!("{kind}/{name}.toml"), table: toml::from_str(src).unwrap() }
}

#[test]
fn deleting_show_content_retires_runtime_without_touching_survivors() {
    let files = [
        file("scenes", "gone", "key = 1\nset = { \"scene_level\" = 0.7 }\n[canvas.wide]\nnodes = [{ src = \"camera\" }]"),
        file("scenes", "keep", "key = 2"),
        file(
            "presets",
            "gone",
            "toggle = true\nfx = [{ name = \"shake\", amount = 0.8 }]\nset = { \"removed_level\" = 0.9 }\ndo = [\"wait 1s\", \"set late_level 1\"]",
        ),
        file("presets", "keep", "toggle = true\nset = { \"kept_level\" = 0.4 }"),
        file("rules", "gone", "when = \"test.deleted\"\ndo = [\"wait 1s\", \"set late_rule 1\"]"),
    ];
    let mut c = Core::new(Config::build(&files), 0);
    for name in ["gone", "keep"] {
        c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetFire { name: name.into(), payload: Value::Null }) });
    }
    c.submit(Input::Event { event: Event::new("test.deleted", Origin::System, Value::Null) });
    run(&mut c, 100);
    assert_eq!(c.get("show.scene.program").and_then(Value::as_str), Some("gone"));
    assert_eq!(c.get("fx.shake.active"), Some(&Value::Bool(true)));
    let before_delete = c.runtime_state();

    let remaining = Config::build(&[files[1].clone(), files[3].clone()]);
    c.apply_config(remaining.clone());
    run(&mut c, 1100);
    assert_eq!(c.get("show.scene.program").and_then(Value::as_str), Some("keep"));
    assert_eq!(c.get("show.scene.preview").and_then(Value::as_str), Some("keep"));
    assert_eq!(c.active_presets(), [("keep".into(), None)]);
    assert!(c.get("preset.gone.active").is_none());
    assert_eq!(c.get("fx.shake.active"), Some(&Value::Bool(false)));
    for address in ["scene_level", "removed_level", "fx.shake.amount", "late_level", "late_rule"] {
        assert!(c.get(address).is_none_or(|v| v.as_f64().is_none_or(|v| v == 0.0)), "{address}: {:?}", c.get(address));
    }
    assert_eq!(f(&c, "kept_level"), 0.4);

    let mut restored = Core::new(remaining, 0);
    restored.restore(&before_delete);
    run(&mut restored, 10);
    assert_eq!(restored.get("show.scene.program").and_then(Value::as_str), Some("keep"));
    assert_eq!(restored.active_presets(), [("keep".into(), None)]);
    assert_eq!(f(&restored, "kept_level"), 0.4);
    assert!(restored.get("removed_level").is_none());
    assert!(restored.get("fx.shake.amount").is_none());

    c.apply_config(Config::default());
    run(&mut c, 10);
    assert_eq!(c.get("show.scene.program").and_then(Value::as_str), Some(""));
    assert_eq!(c.get("show.scene.preview").and_then(Value::as_str), Some(""));
    assert!(c.active_presets().is_empty());
    let mut blank = Core::new(Config::default(), 0);
    blank.restore(&before_delete);
    run(&mut blank, 10);
    assert_eq!(blank.get("show.scene.program").and_then(Value::as_str), Some(""));
    assert_eq!(blank.get("show.scene.preview").and_then(Value::as_str), Some(""));
    assert!(blank.active_presets().is_empty());
    assert!(blank.get("kept_level").is_none());
}

fn project() -> Vec<SourceFile> {
    vec![
        file(
            "project",
            "project",
            "schema = 1\nname = \"test\"\n[safety]\nchat_ttl = \"10s\"\nchat_caps = { \"fx.*.amount\" = [0.0, 0.5], \"lights.blackout\" = [0, 0] }",
        ),
        file(
            "scenes",
            "duo",
            "key = 1\nset = { \"fx.grade.amount\" = 0.3 }\n[canvas.wide]\nnodes = [{ src = \"cam_face\", rect = [0.0, 0.0, 0.5, 1.0] }, { src = \"cam_desk\", rect = [0.5, 0.0, 0.5, 1.0] }]\n[transitions]\npool = [{ name = \"morph\", w = 3 }, { name = \"fade\", w = 1 }]\navoid_repeat = 1\nms = [500, 900]",
        ),
        file(
            "scenes",
            "wide",
            "key = 2\n[canvas.wide]\nnodes = [{ src = \"cam_wide\" }]\n[transitions]\npool = [{ name = \"morph\" }, { name = \"fade\" }]\navoid_repeat = 1",
        ),
        file(
            "presets",
            "hype",
            "hold = \"8s\"\nfx = [{ name = \"rgb_split\" }]\nset = { \"fx.rgb_split.amount\" = 0.8 }\nsound = \"airhorn\"\nlights = { cue = \"chase_fast\" }\nconflict = \"replace\"",
        ),
        file("presets", "chill", "set = { \"fx.vhs.amount\" = 0.6 }\ntoggle = true"),
        file("presets", "safe", "lights = { cue = \"safe\" }"),
        file(
            "rules",
            "cheers",
            "[[rule]]\nname = \"big cheer\"\nwhen = \"twitch.cheer\"\nif = \"event.bits >= 1000\"\ndo = [\"preset.fire hype\", \"bot.say 'thanks {user} for {bits} bits'\"]\ncooldown = { global = \"10s\" }",
        ),
        file(
            "rules",
            "chain",
            "[[rule]]\nwhen = \"test.a\"\ndo = [\"emit test.b\", \"wait 1s\", \"set fx.delayed 1\"]\n[[rule]]\nwhen = \"test.loop\"\ndo = [\"emit test.loop\"]",
        ),
        file("rules", "wild", "[[rule]]\nwhen = \"test.wild\"\ndo = [\"set scene.*.node.cam_face.offset_y 5\"]"),
        file("bindings", "shake", "target = \"scene.*.node.cam_face.offset_y\"\nsignal = \"music.kick\"\nrange = [0, 12]\nscope = \"preset.hype\""),
    ]
}

fn core() -> Core {
    let c = Config::build(&project());
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    Core::new(c, 1_000 * MS)
}

fn run(c: &mut Core, ms: u64) -> Vec<Output> {
    let mut out = Vec::new();
    let ticks = (ms * MS).div_ceil(c.period());
    for _ in 0..ticks {
        c.step();
        out.extend(c.drain_outputs());
    }
    out
}

fn f(c: &Core, a: &str) -> f64 {
    c.get(a).and_then(Value::as_f64).unwrap_or(f64::NAN)
}

fn cheer(bits: i64) -> Input {
    let mut e = Event::new("twitch.cheer", Origin::Sim, Value::map().with("bits", bits).with("user", "drumfan"));
    e.actor = Some(Actor { platform: "twitch".into(), id: "u1".into(), name: "drumfan".into(), roles: vec![Role::Everyone] });
    Input::Event { event: e }
}

#[test]
fn cheer_fires_preset_with_trace_and_actions() {
    let mut c = core();
    c.submit(cheer(1000));
    let out = run(&mut c, 20);
    // chat-originated → preset overrides clamp to chat priority but the amount is set
    assert!(c.get("preset.hype.active").unwrap().truthy());
    assert!(f(&c, "fx.rgb_split.amount") > 0.0);
    assert!(f(&c, "fx.rgb_split.env") > 0.0);
    let actions: Vec<String> = out
        .iter()
        .filter_map(|o| match o {
            Output::Action(c) => Some(c.op.describe()),
            _ => None,
        })
        .collect();
    assert!(actions.iter().any(|a| a.starts_with("audio.play")), "{actions:?}");
    assert!(actions.iter().any(|a| a.starts_with("lights.cue")), "{actions:?}");
    let say = out.iter().find_map(|o| match o {
        Output::Action(c) if matches!(&c.op, Op::Action { name, .. } if name == "bot.say") => Some(c.op.clone()),
        _ => None,
    });
    match say {
        Some(Op::Action { args, .. }) => assert_eq!(args.get_path("args.0").unwrap().as_str(), Some("thanks drumfan for 1000 bits")),
        other => panic!("no bot.say: {other:?}"),
    }
    // trace: event → rule → command → preset → change
    let ev_id = out.iter().find_map(|o| match o {
        Output::Event(e) if e.ty == "twitch.cheer" => Some(e.id),
        _ => None,
    });
    let chain = c.trace_chain(ev_id.unwrap());
    let kinds: Vec<&str> = chain.iter().map(|r| r.kind.as_str()).collect();
    for k in ["event", "rule", "command", "preset", "change"] {
        assert!(kinds.contains(&k), "missing {k} in {kinds:?}");
    }
    // under 1000 bits: no fire; cooldown blocks a second 1000 within 10s
    let mut c2 = core();
    c2.submit(cheer(500));
    run(&mut c2, 20);
    assert!(!c2.get("preset.hype.active").unwrap().truthy());
}

#[test]
fn preset_holds_then_releases_and_binding_scoped() {
    let mut c = core();
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetFire { name: "hype".into(), payload: Value::Null }) });
    c.submit(Input::Signal { name: "music.kick".into(), value: 0.5 });
    run(&mut c, 50);
    assert_eq!(f(&c, "fx.rgb_split.amount"), 0.8, "ui-fired preset is not chat-clamped");
    assert_eq!(f(&c, "scene.duo.node.cam_face.offset_y"), 6.0, "binding active in preset scope");
    run(&mut c, 8100);
    assert!(!c.get("preset.hype.active").unwrap().truthy());
    assert_eq!(f(&c, "scene.duo.node.cam_face.offset_y"), 0.0, "binding out of scope");
}

#[test]
fn priorities_chat_caps_mixer_and_clean() {
    let mut c = core();
    c.submit(Input::Declare { address: "fx.vhs.amount".into(), meta: Meta::float(0.0, [0.0, 1.0]) });
    run(&mut c, 5);
    let chat = |op| {
        let mut cmd = Command::new(Origin::Chat, op);
        cmd.actor = Some(Actor { platform: "twitch".into(), id: "v".into(), name: "viewer".into(), roles: vec![] });
        Input::Command { cmd }
    };
    // chat effects only run in live (§12.1)
    c.submit(chat(Op::Set { address: "fx.vhs.amount".into(), value: Value::Float(0.9) }));
    let out = run(&mut c, 5);
    assert!(out.iter().any(|o| matches!(o, Output::Ack { ok: false, error: Some(e), .. } if e.contains("paused"))));
    assert_eq!(f(&c, "fx.vhs.amount"), 0.0);
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::ModeSet { mode: "live".into() }) });
    c.submit(chat(Op::Set { address: "fx.vhs.amount".into(), value: Value::Float(0.9) }));
    run(&mut c, 5);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.5, "chat cap");
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetFire { name: "chill".into(), payload: Value::Null }) });
    run(&mut c, 5);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.6, "preset (200) beats chat (100)");
    c.submit(chat(Op::Set { address: "mixer.16r.ch.1.fader".into(), value: Value::Float(1.0) }));
    let out = run(&mut c, 5);
    assert!(out.iter().any(|o| matches!(o, Output::Ack { ok: false, error: Some(e), .. } if e.contains("mixer"))));
    // toggle preset releases on second press
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetFire { name: "chill".into(), payload: Value::Null }) });
    run(&mut c, 5);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.5, "chat override visible again");
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Clean) });
    run(&mut c, 5);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.0, "clean removed chat override");
    // chat overrides auto-expire (chat_ttl = 10s)
    c.submit(chat(Op::Set { address: "fx.vhs.amount".into(), value: Value::Float(0.4) }));
    run(&mut c, 5);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.4);
    run(&mut c, 10_100);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.0);
}

#[test]
fn scenes_take_transitions_and_scene_layer() {
    let mut c = core();
    run(&mut c, 5);
    assert_eq!(c.get("show.scene.program").unwrap().as_str(), Some("duo"));
    assert_eq!(f(&c, "fx.grade.amount"), 0.3, "scene layer");
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::SceneGo { scene: "wide".into() }) });
    run(&mut c, 5);
    assert_eq!(c.get("show.scene.program").unwrap().as_str(), Some("duo"), "go only previews");
    let mut names = Vec::new();
    for i in 0..6 {
        let s = if i % 2 == 0 { "wide" } else { "duo" };
        c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::SceneCut { scene: s.into(), transition: None }) });
        run(&mut c, 1000);
        assert_eq!(c.get("show.scene.program").unwrap().as_str(), Some(s));
        assert!(!c.get("show.transition.active").unwrap().truthy());
        names.push(c.get("show.transition.name").unwrap().as_str().unwrap().to_string());
    }
    for w in names.windows(2) {
        assert_ne!(w[0], w[1], "avoid_repeat = 1: {names:?}");
    }
    // last cut was to duo (scene layer back on)
    assert_eq!(f(&c, "fx.grade.amount"), 0.3);
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::SceneCut { scene: "wide".into(), transition: Some("cut".into()) }) });
    run(&mut c, 5);
    assert_eq!(f(&c, "fx.grade.amount"), 0.0, "scene layer removed off-program");
}

#[test]
fn delays_chains_and_loop_guard() {
    let mut c = core();
    c.submit(Input::Event { event: Event::new("test.a", Origin::Sim, Value::Null) });
    let out = run(&mut c, 10);
    assert!(out.iter().any(|o| matches!(o, Output::Event(e) if e.ty == "test.b")));
    assert!(c.get("fx.delayed").is_none());
    run(&mut c, 1000);
    assert_eq!(c.get("fx.delayed"), Some(&Value::Int(1)));
    c.submit(Input::Event { event: Event::new("test.loop", Origin::Sim, Value::Null) });
    let out = run(&mut c, 10);
    let loops = out.iter().filter(|o| matches!(o, Output::Event(e) if e.ty == "test.loop")).count();
    assert!(loops <= 20, "loop guard: {loops}");
}

#[test]
fn restore_after_restart() {
    let mut c = core();
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::ModeSet { mode: "live".into() }) });
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::SceneCut { scene: "wide".into(), transition: Some("cut".into()) }) });
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetFire { name: "chill".into(), payload: Value::Null }) });
    c.submit(Input::Command { cmd: Command::new(Origin::Midi, Op::Set { address: "lights.par.intensity".into(), value: Value::Float(0.7) }) });
    run(&mut c, 50);
    let rs = c.runtime_state();
    let json = serde_json::to_string(&rs).unwrap();
    let mut c2 = core();
    c2.restore(&serde_json::from_str(&json).unwrap());
    run(&mut c2, 5);
    assert_eq!(c2.mode_str(), "live");
    assert_eq!(c2.get("show.scene.program").unwrap().as_str(), Some("wide"));
    assert_eq!(f(&c2, "fx.vhs.amount"), 0.6);
    assert_eq!(f(&c2, "lights.par.intensity"), 0.7);
    assert!(c2.get("preset.chill.active").unwrap().truthy());
    // released after restore like any other active preset
    c2.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetRelease { name: "chill".into() }) });
    run(&mut c2, 5);
    assert!(c2.get("fx.vhs.amount").is_none_or(|v| v.as_f64() == Some(0.0) || v.is_null()));
}

#[test]
fn replay_reproduces_state() {
    let mut live = core();
    let mut log = Vec::new();
    live.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::ModeSet { mode: "live".into() }) });
    run(&mut live, 30);
    log.extend(live.drain_applied());
    live.submit(cheer(1000));
    run(&mut live, 3000);
    log.extend(live.drain_applied());
    live.submit(Input::Command { cmd: Command::new(Origin::Cli, Op::Action { name: "sim.gift_bomb".into(), args: Value::map().with("count", 5) }) });
    live.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::SceneCut { scene: "wide".into(), transition: None }) });
    run(&mut live, 2000);
    log.extend(live.drain_applied());
    let target_tick = live.tick_index();

    let mut rep = core();
    let mut i = 0;
    while rep.tick_index() < target_tick {
        while i < log.len() && log[i].0 == rep.tick_index() + 1 {
            rep.submit(log[i].1.clone());
            i += 1;
        }
        rep.step();
        rep.drain_outputs();
    }
    for a in ["show.mode", "show.scene.program", "show.transition.name", "fx.rgb_split.amount", "preset.hype.active", "fx.rgb_split.env"] {
        assert_eq!(live.get(a), rep.get(a), "{a}");
    }
    assert_eq!(live.runtime_state(), rep.runtime_state());
}

#[test]
fn set_base_undo_redo_persist() {
    let mut c = core();
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::SetBase { address: "scene.duo.node.cam_face.opacity".into(), value: Value::Float(0.5) }) });
    let out = run(&mut c, 5);
    assert!(out.iter().any(|o| matches!(o, Output::PersistBase { address, .. } if address == "scene.duo.node.cam_face.opacity")));
    assert_eq!(f(&c, "scene.duo.node.cam_face.opacity"), 0.5);
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Undo) });
    run(&mut c, 5);
    assert_eq!(f(&c, "scene.duo.node.cam_face.opacity"), 1.0);
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Redo) });
    run(&mut c, 5);
    assert_eq!(f(&c, "scene.duo.node.cam_face.opacity"), 0.5);
}

#[test]
fn panic_clears_automation() {
    let mut c = core();
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetFire { name: "hype".into(), payload: Value::Null }) });
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Set { address: "manual.x".into(), value: Value::Int(3) }) });
    run(&mut c, 20);
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Panic) });
    let out = run(&mut c, 600);
    assert!(!c.get("preset.hype.active").unwrap().truthy());
    assert_eq!(c.get("manual.x"), Some(&Value::Int(3)), "manual survives panic");
    assert!(f(&c, "fx.rgb_split.env") < 0.01);
    assert!(out.iter().any(|o| matches!(o, Output::Action(c) if c.op.describe() == "lights.panic")));
}

#[test]
fn command_keys_layer_overrides_per_owner() {
    let mut c = core();
    let a = |v: f64, key: &str| Input::Command {
        cmd: Command::new(Origin::System, Op::Set { address: "lights.par.intensity".into(), value: Value::Float(v) }).with_key(key),
    };
    c.submit(Input::Declare { address: "lights.par.intensity".into(), meta: Meta::float(0.0, [0.0, 1.0]).htp() });
    c.submit(a(0.4, "cuelist:main"));
    c.submit(a(0.7, "cuelist:chase"));
    run(&mut c, 5);
    assert_eq!(f(&c, "lights.par.intensity"), 0.7, "HTP across playbacks");
    c.submit(Input::Command { cmd: Command::new(Origin::System, Op::Release { address: "lights.par.intensity".into() }).with_key("cuelist:chase") });
    run(&mut c, 5);
    assert_eq!(f(&c, "lights.par.intensity"), 0.4, "releasing one playback keeps the other");
    let p = c.explain("lights.par.intensity").unwrap();
    assert!(p.layers.iter().any(|l| l.source == "cuelist:main"));
}

#[test]
fn chat_caps_apply_to_booleans() {
    let mut c = core();
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::ModeSet { mode: "live".into() }) });
    run(&mut c, 5);
    let mut cmd = Command::new(Origin::Chat, Op::Set { address: "lights.blackout".into(), value: Value::Bool(true) });
    cmd.actor = Some(Actor { platform: "twitch".into(), id: "v".into(), name: "viewer".into(), roles: vec![] });
    c.submit(Input::Command { cmd });
    run(&mut c, 5);
    assert_eq!(c.get("lights.blackout"), Some(&Value::Bool(false)));
}

#[test]
fn pickup_fader_follows_external_changes_without_jumps() {
    let mut files = project();
    files.push(file("controllers", "faders", "[[binding]]\ntarget = \"mixer.16r.ch.3.fader\"\nsignal = \"midi.xtouch.fader\"\ntakeover = \"pickup\""));
    let mut c = Core::new(Config::build(&files), 1_000 * MS);
    let fader = |c: &mut Core, v: f32| {
        c.submit(Input::Signal { name: "midi.xtouch.fader".into(), value: v });
        run(c, 5);
    };
    c.submit(Input::Publish { address: "mixer.16r.ch.3.fader".into(), value: Value::Float(0.6) });
    fader(&mut c, 0.1);
    fader(&mut c, 0.2);
    assert_eq!(f(&c, "mixer.16r.ch.3.fader"), 0.6, "no jump before pickup");
    fader(&mut c, 0.65);
    assert!((f(&c, "mixer.16r.ch.3.fader") - 0.65).abs() < 1e-6, "picked up after crossing");
    fader(&mut c, 0.7);
    assert!((f(&c, "mixer.16r.ch.3.fader") - 0.7).abs() < 1e-6);
    // console-side change: the mixer adapter publishes the base and releases overrides
    c.submit(Input::Publish { address: "mixer.16r.ch.3.fader".into(), value: Value::Float(0.3) });
    c.submit(Input::Command { cmd: Command::new(Origin::Mixer, Op::Release { address: "mixer.16r.ch.3.fader".into() }) });
    run(&mut c, 5);
    assert_eq!(f(&c, "mixer.16r.ch.3.fader"), 0.3, "UC Surface move wins");
    fader(&mut c, 0.72);
    assert_eq!(f(&c, "mixer.16r.ch.3.fader"), 0.3, "pickup dropped: no jump from the X-TOUCH");
    fader(&mut c, 0.35);
    fader(&mut c, 0.28);
    assert!((f(&c, "mixer.16r.ch.3.fader") - 0.28).abs() < 1e-6, "picked up again");
}

fn light_actions(out: &[Output]) -> Vec<String> {
    out.iter()
        .filter_map(|o| match o {
            Output::Action(c) => Some(c.op.describe()).filter(|d| d.starts_with("lights.")),
            _ => None,
        })
        .collect()
}

#[test]
fn one_shot_preset_leaves_its_light_cue_running_but_held_preset_releases_it() {
    let mut c = core();
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetFire { name: "safe".into(), payload: Value::Null }) });
    let out = run(&mut c, 2000);
    let lights = light_actions(&out);
    assert!(lights.iter().any(|a| a.starts_with("lights.cue")), "{lights:?}");
    assert!(!lights.iter().any(|a| a.starts_with("lights.release")), "one-shot preset must not switch its look off: {lights:?}");

    // `hype` holds 8 s: its cue is released when the hold ends
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetFire { name: "hype".into(), payload: Value::Null }) });
    let lights = light_actions(&run(&mut c, 7000));
    assert!(!lights.iter().any(|a| a.starts_with("lights.release")), "{lights:?}");
    let lights = light_actions(&run(&mut c, 2000));
    assert!(lights.iter().any(|a| a.starts_with("lights.release")), "{lights:?}");
}

#[test]
fn wildcard_addresses_in_commands_hit_every_match() {
    let mut c = core();
    run(&mut c, 5);
    let targets: Vec<String> = ["duo", "wide"].iter().map(|s| format!("scene.{s}.node.cam_face.offset_y")).filter(|a| c.get(a).is_some()).collect();
    assert!(!targets.is_empty(), "scene node params are declared");
    // a rule's `do` with a wildcard address
    c.submit(Input::Event { event: Event::new("test.wild", Origin::Sim, Value::Null) });
    run(&mut c, 5);
    for a in &targets {
        assert_eq!(f(&c, a), 5.0, "{a}");
    }
    // a manual release with the same pattern clears every match
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Release { address: "scene.*.node.cam_face.offset_y".into() }) });
    run(&mut c, 5);
    for a in &targets {
        assert_eq!(f(&c, a), 0.0, "{a} released");
    }
    // a pattern that matches nothing is an error, not a silent no-op
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Set { address: "nothing.*.here".into(), value: Value::Float(1.0) }) });
    let out = run(&mut c, 5);
    assert!(out.iter().any(|o| matches!(o, Output::Ack { ok: false, error: Some(e), .. } if e.contains("matches nothing"))));
    // chat still can't reach the mixer through a wildcard
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::ModeSet { mode: "live".into() }) });
    c.submit(Input::Declare { address: "mixer.16r.ch.1.fader".into(), meta: Meta::float(0.5, [0.0, 1.0]) });
    run(&mut c, 5);
    let mut cmd = Command::new(Origin::Chat, Op::Set { address: "mixer.*.ch.1.fader".into(), value: Value::Float(1.0) });
    cmd.actor = Some(Actor { platform: "twitch".into(), id: "v".into(), name: "viewer".into(), roles: vec![] });
    c.submit(Input::Command { cmd });
    run(&mut c, 5);
    assert_eq!(f(&c, "mixer.16r.ch.1.fader"), 0.5);
}

#[test]
fn patch_trigger_publishes_its_payload_in_the_same_tick_as_the_edge() {
    let mut c = core();
    let fire = |c: &mut Core, payload: Value, user: &str| {
        let mut cmd = Command::new(Origin::Cli, Op::Trigger { address: "patch.sparks".into(), payload });
        cmd.actor = Some(Actor { platform: "twitch".into(), id: user.into(), name: user.into(), roles: vec![Role::Everyone] });
        c.submit(Input::Command { cmd });
        c.step();
    };
    fire(&mut c, Value::map().with("bits", 5000).with("user", "drumfan"), "u1");
    assert!(c.get("patch.sparks.active").unwrap().truthy());
    assert_eq!(f(&c, "patch.sparks.payload.bits"), 5000.0);
    assert_eq!(f(&c, "patch.sparks.payload.amount"), 5000.0);
    let first_user = f(&c, "patch.sparks.payload.user_hash");
    assert!(first_user > 0.0 && first_user < 1.0);
    // a stacked retrigger replaces the payload (fields it lacks go back to 0)
    fire(&mut c, Value::map().with("tier", 2), "u2");
    assert_eq!(f(&c, "patch.sparks.payload.tier"), 2.0);
    assert_eq!(f(&c, "patch.sparks.payload.bits"), 0.0);
    assert_ne!(f(&c, "patch.sparks.payload.user_hash"), first_user);
    // only patch triggers carry payload state
    c.submit(Input::Command { cmd: Command::new(Origin::Cli, Op::Trigger { address: "fx.glitch".into(), payload: Value::map().with("bits", 1) }) });
    c.step();
    assert!(c.get("fx.glitch.payload.bits").is_none());
}

#[test]
fn quick_effect_settings_last_through_the_fade_toggles_hold_effects_and_looks_are_held() {
    let files = vec![
        file("project", "project", "schema = 1"),
        file("presets", "flash", "fx = [{ name = \"fade_to_black\", level = 1.0, hold = \"0.2s\", color_r = 1.0 }]"),
        file("presets", "dreamy", "toggle = true\nfx = [{ name = \"blur\", radius = 40.0 }]"),
        file("presets", "warm", "lights = { look = \"warm\", hold = \"0.5s\" }"),
    ];
    let cfg = Config::build(&files);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    let mut c = Core::new(cfg, 1_000 * MS);
    let press =
        |c: &mut Core, name: &str| c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetFire { name: name.into(), payload: Value::Null }) });
    let active = |c: &Core, name: &str| c.get(&format!("preset.{name}.active")).is_some_and(Value::truthy);

    // once: the effect's own settings don't make it latch, and they last through its fade-out
    press(&mut c, "flash");
    run(&mut c, 100);
    assert!(active(&c, "flash"));
    assert_eq!(f(&c, "fx.fade_to_black.color_r"), 1.0);
    run(&mut c, 300);
    assert!(!active(&c, "flash"), "a one-shot ends by itself");
    assert!(f(&c, "fx.fade_to_black.env") > 0.0, "still fading out");
    assert_eq!(f(&c, "fx.fade_to_black.color_r"), 1.0, "the color holds while it fades");
    run(&mut c, 500);
    assert_eq!(f(&c, "fx.fade_to_black.env"), 0.0);
    assert_eq!(f(&c, "fx.fade_to_black.color_r"), 0.0, "the setting goes once the effect is gone");

    // until pressed again: the effect stays up past the default two-second burst
    press(&mut c, "dreamy");
    run(&mut c, 3_000);
    assert!(active(&c, "dreamy"));
    assert_eq!(f(&c, "fx.blur.env"), 1.0);
    assert_eq!(f(&c, "fx.blur.radius"), 40.0);
    press(&mut c, "dreamy");
    run(&mut c, 100);
    assert!(!active(&c, "dreamy"));
    assert!(f(&c, "fx.blur.env") < 1.0 && f(&c, "fx.blur.env") > 0.0, "fading out");
    assert_eq!(f(&c, "fx.blur.radius"), 40.0);
    run(&mut c, 600);
    assert_eq!(f(&c, "fx.blur.env"), 0.0);
    assert_eq!(f(&c, "fx.blur.radius"), 0.0);

    // a light look goes to the lights with the preset's priority and is released with it,
    // even for a one-shot preset (a look has no course of its own: it lasts its `hold`)
    let lights = |out: &[Output], name: &str| -> Vec<Value> {
        out.iter()
            .filter_map(|o| match o {
                Output::Action(c) => match &c.op {
                    Op::Action { name: n, args } if n == name => Some(args.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    };
    press(&mut c, "warm");
    let out = run(&mut c, 50);
    let cue = lights(&out, "lights.cue");
    assert_eq!(cue.len(), 1, "{out:?}");
    assert_eq!(cue[0].get_path("look").and_then(Value::as_str), Some("warm"));
    assert_eq!(cue[0].get_path("priority").and_then(Value::as_i64), Some(200));
    assert!(lights(&out, "lights.release").is_empty());
    let out = run(&mut c, 500);
    let rel = lights(&out, "lights.release");
    assert_eq!(rel.len(), 1, "{out:?}");
    assert_eq!(rel[0].get_path("look").and_then(Value::as_str), Some("warm"));
}

#[test]
fn a_knob_turns_what_is_running_and_the_next_firing_uses_it() {
    let files = vec![
        file("project", "project", "schema = 1"),
        file(
            "presets",
            "chill",
            "toggle = true\nset = { \"fx.vhs.amount\" = 0.5 }\n[[knob]]\nlabel = \"Tape look\"\ntarget = \"fx.vhs.amount\"\nmin = 0\nmax = 1\nstep = 0.05",
        ),
        file(
            "presets",
            "shake",
            "fx = [{ name = \"shake\", hold = \"2s\" }]\n[[knob]]\nlabel = \"How far\"\ntarget = \"fx.shake.strength\"\nmin = 0.01\nmax = 0.15",
        ),
        file(
            "presets",
            "dreamy",
            "toggle = true\nfx = [{ name = \"blur\" }]\n[[knob]]\nlabel = \"How blurry\"\ntarget = \"fx.blur.radius\"\nmin = 0\nmax = 96",
        ),
    ];
    let cfg = Config::build(&files);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    let mut c = Core::new(cfg, 1_000 * MS);
    let press =
        |c: &mut Core, name: &str| c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetFire { name: name.into(), payload: Value::Null }) });
    let turn = |c: &mut Core, origin: Origin, name: &str, target: &str, v: f64, save: bool| {
        let args = Value::map().with("name", name).with("target", target).with("value", v).with("save", save);
        c.submit(Input::Command { cmd: Command::new(origin, Op::Action { name: "preset.knob".into(), args }) });
    };
    let saved = |out: &[Output]| -> Vec<Value> {
        out.iter()
            .filter_map(|o| match o {
                Output::Action(c) => match &c.op {
                    Op::Action { name, args } if name == "preset.knob" => Some(args.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    };

    // while it's held, the held value follows the knob at once (fitted to its step)
    press(&mut c, "chill");
    run(&mut c, 20);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.5);
    turn(&mut c, Origin::Ui, "chill", "fx.vhs.amount", 0.83, false);
    let out = run(&mut c, 20);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.85);
    assert!(saved(&out).is_empty(), "no file write while the knob is still moving");
    press(&mut c, "chill");
    run(&mut c, 20);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.0, "released");
    // a knob still moving isn't the quick effect's value yet: the next firing is as the file says
    press(&mut c, "chill");
    run(&mut c, 20);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.5);
    // a settled knob goes to the file (fitted) and is the value from then on
    turn(&mut c, Origin::Ui, "chill", "fx.vhs.amount", 2.0, true);
    let out = run(&mut c, 20);
    assert_eq!(f(&c, "fx.vhs.amount"), 1.0);
    let w = saved(&out);
    assert_eq!(w.len(), 1, "{out:?}");
    assert_eq!(w[0].get_path("value"), Some(&Value::Float(1.0)));
    assert_eq!(c.query("presets", &Value::Null).unwrap().get_path("0.knobs.0.value"), Some(&Value::Float(1.0)));
    press(&mut c, "chill");
    run(&mut c, 20);
    press(&mut c, "chill");
    run(&mut c, 20);
    assert_eq!(f(&c, "fx.vhs.amount"), 1.0, "the next firing uses the saved knob");

    // a built-in effect's setting the file didn't have yet: held while it runs, gone with it
    press(&mut c, "shake");
    run(&mut c, 50);
    turn(&mut c, Origin::Ui, "shake", "fx.shake.strength", 0.12, false);
    run(&mut c, 20);
    assert_eq!(f(&c, "fx.shake.strength"), 0.12);
    run(&mut c, 3_000);
    assert_eq!(f(&c, "fx.shake.strength"), 0.0, "gone with the effect");
    turn(&mut c, Origin::Ui, "shake", "fx.shake.strength", 0.12, true);
    press(&mut c, "shake");
    run(&mut c, 50);
    assert_eq!(f(&c, "fx.shake.strength"), 0.12, "saved while idle: the next firing has it");
    // … and on an effect that stays up until pressed again, it fades out with the effect
    press(&mut c, "dreamy");
    run(&mut c, 50);
    turn(&mut c, Origin::Ui, "dreamy", "fx.blur.radius", 50.0, false);
    run(&mut c, 20);
    assert_eq!(f(&c, "fx.blur.radius"), 50.0);
    press(&mut c, "dreamy");
    run(&mut c, 1_500);
    assert_eq!(f(&c, "fx.blur.env"), 0.0);
    assert_eq!(f(&c, "fx.blur.radius"), 0.0, "not left behind after it's gone");

    // viewers can't turn knobs, and only declared knobs turn
    turn(&mut c, Origin::Chat, "chill", "fx.vhs.amount", 0.1, false);
    turn(&mut c, Origin::Ui, "chill", "fx.grade.warmth", 0.1, false);
    run(&mut c, 20);
    assert_eq!(f(&c, "fx.vhs.amount"), 1.0);
    assert!(c.get("fx.grade.warmth").is_none());
}
