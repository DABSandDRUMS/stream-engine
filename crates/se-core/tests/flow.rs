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
fn chat_scoped_layers_release_independently_and_cannot_impersonate_manual() {
    let mut c = core();
    c.submit(Input::Declare { address: "fx.vhs.amount".into(), meta: Meta::float(0.0, [0.0, 1.0]) });
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::ModeSet { mode: "live".into() }) });
    run(&mut c, 5);
    let chat = |actor: &str, key: &str, value: Option<f64>| {
        let op = match value {
            Some(value) => Op::Set { address: "fx.vhs.amount".into(), value: Value::Float(value) },
            None => Op::Release { address: "fx.vhs.amount".into() },
        };
        Input::Command { cmd: Command::new(Origin::Chat, op).with_key(key).with_actor(Some(Actor {
            platform: "twitch".into(), id: actor.into(), name: actor.into(), roles: vec![],
        })) }
    };
    c.submit(chat("a", "layer:base", Some(0.2)));
    c.submit(chat("a", "layer:accent", Some(0.9)));
    run(&mut c, 5);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.5, "scoped keys retain chat caps");
    c.submit(chat("b", "layer:base", Some(0.3)));
    run(&mut c, 5);
    c.submit(chat("a", "layer:accent", None));
    run(&mut c, 5);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.3, "releasing actor a's accent preserves actor b");
    c.submit(chat("b", "layer:base", None));
    run(&mut c, 5);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.2, "actor a's base survived its accent");
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Set {
        address: "fx.vhs.amount".into(), value: Value::Float(0.8),
    }) });
    c.submit(chat("a", "manual", Some(0.9)));
    run(&mut c, 5);
    c.submit(chat("a", "manual", None));
    run(&mut c, 5);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.8, "chat key cannot overwrite or release the real manual owner");
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Release { address: "fx.vhs.amount".into() }) });
    run(&mut c, 5);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.2);
    run(&mut c, 10_100);
    assert_eq!(f(&c, "fx.vhs.amount"), 0.0, "scoped chat overrides still expire");
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
fn toggle_flips_switches_and_numbers_like_a_set_from_the_same_origin() {
    let mut c = core();
    run(&mut c, 5);
    let vis = "scene.duo.node.cam_face.visible";
    assert_eq!(c.get(vis), Some(&Value::Bool(true)));
    let ui = |line: &str| Input::Command { cmd: Command::new(Origin::Ui, Op::parse(line).unwrap()) };
    c.submit(ui(&format!("toggle {vis}")));
    run(&mut c, 5);
    assert_eq!(c.get(vis), Some(&Value::Bool(false)), "a switch flips");
    c.submit(ui(&format!("toggle address={vis}")));
    run(&mut c, 5);
    assert_eq!(c.get(vis), Some(&Value::Bool(true)), "and flips back");
    // numbers go between 0 and their declared max (1 without a range)
    c.submit(Input::Declare { address: "fx.shake.amount".into(), meta: Meta::float(0.0, [0.0, 12.0]) });
    run(&mut c, 5);
    c.submit(ui("toggle fx.shake.amount"));
    run(&mut c, 5);
    assert_eq!(f(&c, "fx.shake.amount"), 12.0);
    c.submit(ui("toggle fx.shake.amount"));
    run(&mut c, 5);
    assert_eq!(f(&c, "fx.shake.amount"), 0.0);
    // it lands as an override at the origin's priority, so releasing it restores the base
    c.submit(ui(&format!("toggle {vis}")));
    run(&mut c, 5);
    assert_eq!(c.get(vis), Some(&Value::Bool(false)));
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Release { address: vis.into() }) });
    run(&mut c, 5);
    assert_eq!(c.get(vis), Some(&Value::Bool(true)));
    // wildcards flip every match; unknown settings are an error
    c.submit(ui("toggle scene.*.node.cam_face.visible"));
    run(&mut c, 5);
    assert_eq!(c.get(vis), Some(&Value::Bool(false)));
    c.submit(ui("toggle nothing.here"));
    let out = run(&mut c, 5);
    assert!(out.iter().any(|o| matches!(o, Output::Ack { ok: false, error: Some(e), .. } if e.contains("matches nothing"))));
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
fn a_saved_action_held_until_released_keeps_its_steps_until_let_go() {
    let files = vec![
        file("project", "project", "schema = 1"),
        file("scenes", "duo", "[canvas.wide]\nnodes = [{ src = \"cam_face\" }]"),
        file("presets", "hide", "until_released = true\ndo = [\"set scene.duo.node.cam_face.visible false\"]"),
        file("presets", "blip", "do = [\"set scene.duo.node.cam_face.visible false\"]"),
    ];
    let cfg = Config::build(&files);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    let mut c = Core::new(cfg, 1_000 * MS);
    let vis = "scene.duo.node.cam_face.visible";
    let ui = |op: Op| Input::Command { cmd: Command::new(Origin::Ui, op) };
    run(&mut c, 5);
    c.submit(ui(Op::PresetFire { name: "hide".into(), payload: Value::Null }));
    run(&mut c, 5_000);
    assert!(c.get("preset.hide.active").is_some_and(Value::truthy), "stays on while held");
    assert_eq!(c.get(vis), Some(&Value::Bool(false)));
    c.submit(ui(Op::PresetRelease { name: "hide".into() }));
    run(&mut c, 5);
    assert_eq!(c.get(vis), Some(&Value::Bool(true)), "letting go undoes its steps");
    // without it, a list of steps is a one-shot that ends by itself
    c.submit(ui(Op::PresetFire { name: "blip".into(), payload: Value::Null }));
    run(&mut c, 50);
    assert!(!c.get("preset.blip.active").is_some_and(Value::truthy));
}

#[test]
fn quick_effect_settings_last_through_the_fade_toggles_hold_effects_and_timed_looks_end() {
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

    // A temporary look participates in the preset lifecycle, rather than latching it.
    press(&mut c, "warm");
    run(&mut c, 50);
    assert!(active(&c, "warm"));
    run(&mut c, 500);
    assert!(!active(&c, "warm"));
}

#[test]
fn temporary_lighting_presets_keep_live_stacks_and_chat_cannot_latch_them() {
    let files = vec![
        file("project", "project", "schema = 1\nstart_mode = \"live\"\n[safety]\nchat_ttl = \"1s\""),
        file("presets", "warm", "hold = \"0.5s\"\nconflict = \"stack\"\nlights = { look = \"warm\" }"),
        file("presets", "long", "hold = \"10s\"\nlights = { look = \"warm\" }"),
        file("presets", "latch", "toggle = true\nlights = { cue = \"main\" }"),
        file("presets", "once", "lights = { cue = \"main\" }"),
    ];
    let cfg = Config::build(&files);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    let mut c = Core::new(cfg, 1_000 * MS);
    let press = |c: &mut Core, name: &str, origin: Origin| {
        let mut cmd = Command::new(origin, Op::PresetFire { name: name.into(), payload: Value::Null });
        if origin == Origin::Chat {
            cmd.actor = Some(Actor { platform: "twitch".into(), id: "v".into(), name: "viewer".into(), roles: vec![] });
        }
        c.submit(Input::Command { cmd });
    };
    let active = |c: &Core, name: &str| c.get(&format!("preset.{name}.active")).is_some_and(Value::truthy);

    press(&mut c, "warm", Origin::Ui);
    run(&mut c, 300);
    press(&mut c, "warm", Origin::Ui);
    run(&mut c, 300);
    assert!(active(&c, "warm"), "expiration of the first stack keeps the second live");
    run(&mut c, 300);
    assert!(!active(&c, "warm"), "the last stack expires instead of becoming latched");

    for name in ["long", "latch", "once"] {
        press(&mut c, name, Origin::Chat);
    }
    run(&mut c, 100);
    for name in ["long", "latch", "once"] {
        assert!(active(&c, name), "{name} remains temporary but visible before chat TTL");
    }
    run(&mut c, 1_100);
    for name in ["long", "latch", "once"] {
        assert!(!active(&c, name), "{name} cannot outlive chat TTL");
    }
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

fn lane_core() -> Core {
    let files = [
        file("project", "project", "schema = 1"),
        file("presets", "big1", "lane = \"moment\"\nhold = \"1s\"\nset = { \"big1_level\" = 1.0 }"),
        file("presets", "big2", "lane = \"moment\"\nhold = \"1s\"\nset = { \"big2_level\" = 1.0 }"),
        file("presets", "drop", "quantize = \"bar\"\nhold = \"2s\"\nset = { \"drop_level\" = 1.0 }"),
    ];
    let cfg = Config::build(&files);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    Core::new(cfg, 1_000 * MS)
}

fn fire(c: &mut Core, name: &str) {
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetFire { name: name.into(), payload: Value::Null }) });
}

fn on(c: &Core, name: &str) -> bool {
    c.get(&format!("preset.{name}.active")).is_some_and(Value::truthy)
}

fn rejections(out: &[Output]) -> Vec<String> {
    out.iter().filter_map(|o| match o { Output::Ack { ok: false, error: Some(e), .. } => Some(e.clone()), _ => None }).collect()
}

#[test]
fn a_lane_runs_one_preset_at_a_time_and_the_next_waits_its_turn() {
    let mut c = lane_core();
    fire(&mut c, "big1");
    fire(&mut c, "big2");
    let out = run(&mut c, 20);
    assert!(rejections(&out).is_empty(), "a waiting firing is accepted: {out:?}");
    assert!(on(&c, "big1"));
    assert!(!on(&c, "big2"), "waits while big1 holds the lane");
    run(&mut c, 900);
    assert!(on(&c, "big1") && !on(&c, "big2"));
    run(&mut c, 150);
    assert!(!on(&c, "big1"));
    assert!(on(&c, "big2"), "starts once big1's hold ends");
    assert_eq!(f(&c, "big2_level"), 1.0);
    // big2 now holds the lane: eight more may wait, the ninth is refused
    for _ in 0..9 {
        fire(&mut c, "big1");
    }
    let refused = rejections(&run(&mut c, 20));
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(refused[0].contains("lane `moment` is full"), "{refused:?}");
}

#[test]
fn quantize_bar_starts_on_the_next_bar_and_without_tempo_at_once() {
    let mut c = lane_core();
    c.submit(Input::Signal { name: "beat.bpm".into(), value: 120.0 });
    c.submit(Input::Signal { name: "lfo.bar".into(), value: 0.75 });
    fire(&mut c, "drop");
    run(&mut c, 450);
    assert!(!on(&c, "drop"), "a quarter bar at 120 bpm is 0.5 s away");
    run(&mut c, 100);
    assert!(on(&c, "drop"), "started on the bar");

    let mut c = lane_core();
    fire(&mut c, "drop");
    run(&mut c, 20);
    assert!(on(&c, "drop"), "no tempo: starts at once");

    let bad = Config::build(&[file("presets", "soon", "quantize = \"soon\"")]);
    assert_eq!(bad.errors.len(), 1, "an unknown quantize is a config error");
    assert!(!bad.presets.contains_key("soon"));
}

#[test]
fn panic_clears_lane_queues() {
    let mut c = lane_core();
    fire(&mut c, "big1");
    fire(&mut c, "big2");
    run(&mut c, 20);
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Panic) });
    run(&mut c, 20);
    assert!(!on(&c, "big1"));
    run(&mut c, 1_500);
    assert!(!on(&c, "big2"), "the waiting firing went with the panic");
    fire(&mut c, "big2");
    run(&mut c, 20);
    assert!(on(&c, "big2"), "the lane is free again");
}

fn fired(out: &[Output]) -> Vec<se_proto::Event> {
    out.iter().filter_map(|o| if let Output::Event(e) = o { (e.ty.starts_with("preset.") && e.ty.ends_with(".fired")).then(|| e.clone()) } else { None }).collect()
}

#[test]
fn a_roulette_picks_by_weight_without_repeating_its_last_picks() {
    let mut files = vec![file("project", "project", "schema = 1")];
    for n in ["a", "b", "c", "d", "never"] {
        files.push(file("presets", n, &format!("hold = \"10ms\"\nset = {{ \"{n}_level\" = 1.0 }}")));
    }
    files.push(file(
        "presets",
        "surprise",
        "avoid_repeat = 2\npick = [{ name = \"a\", w = 3.0 }, { name = \"b\" }, { name = \"c\" }, { name = \"d\" }, { name = \"never\", w = 0.0 }]",
    ));
    let cfg = Config::build(&files);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    let mut c = Core::new(cfg, 1_000 * MS);
    let mut picks = Vec::new();
    for _ in 0..400 {
        fire(&mut c, "surprise");
        let out = run(&mut c, 20);
        assert!(rejections(&out).is_empty(), "{out:?}");
        let f = fired(&out);
        assert_eq!(f.len(), 1, "one pick per firing: {f:?}");
        assert!(f[0].ty != "preset.surprise.fired", "the roulette itself never runs");
        picks.push(f[0].ty.trim_start_matches("preset.").trim_end_matches(".fired").to_string());
    }
    for (i, p) in picks.iter().enumerate().skip(2) {
        assert!(!picks[i - 2..i].contains(p), "`{p}` repeated within avoid_repeat at {i}: {:?}", &picks[i - 2..=i]);
    }
    let count = |n: &str| picks.iter().filter(|p| *p == n).count();
    assert_eq!(count("never"), 0, "w = 0 is never picked");
    for n in ["b", "c", "d"] {
        assert!(count(n) > 0, "{n} picked");
    }
    assert!(count("a") > count("b") && count("a") > count("d"), "the heavier entry is picked more: {}", ["a", "b", "c", "d"].map(|n| count(n)).map(|n| n.to_string()).join(","));
    // the pick is traced under the firing, and releasing a roulette is a no-op
    fire(&mut c, "surprise");
    let f = fired(&run(&mut c, 20));
    let chain = c.trace_chain(f[0].causal.unwrap());
    assert!(chain.iter().any(|r| r.label.starts_with("roulette surprise picked ")), "{chain:?}");
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetRelease { name: "surprise".into() }) });
    assert!(rejections(&run(&mut c, 20)).is_empty());
}

#[test]
fn a_roulette_pick_waits_in_its_lane_and_carries_the_payload() {
    let files = [
        file("project", "project", "schema = 1"),
        file("presets", "big1", "lane = \"moment\"\nhold = \"1s\"\nset = { \"big1_level\" = 1.0 }"),
        file("presets", "big2", "lane = \"moment\"\nhold = \"1s\"\nset = { \"big2_level\" = 1.0 }"),
        file("presets", "spin", "pick = [{ name = \"big2\" }]"),
    ];
    let cfg = Config::build(&files);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    let mut c = Core::new(cfg, 1_000 * MS);
    fire(&mut c, "big1");
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetFire { name: "spin".into(), payload: Value::map().with("bits", 500) }) });
    let out = run(&mut c, 20);
    assert!(rejections(&out).is_empty(), "{out:?}");
    assert!(on(&c, "big1") && !on(&c, "big2"), "the pick waits while big1 holds its lane");
    let out = run(&mut c, 1_050);
    assert!(on(&c, "big2"), "starts once the lane frees");
    let big2 = fired(&out).into_iter().find(|e| e.ty == "preset.big2.fired").expect("big2 fired");
    assert_eq!(big2.payload.get_path("bits").and_then(Value::as_i64), Some(500));
}

#[test]
fn roulette_config_errors() {
    let base = || vec![file("presets", "a", "set = { \"a_level\" = 1.0 }")];
    let errs = |extra: SourceFile| {
        let mut files = base();
        files.push(extra);
        Config::build(&files).errors.into_iter().map(|e| e.msg).collect::<Vec<_>>()
    };
    let e = errs(file("presets", "r", "pick = [{ name = \"a\" }, { name = \"ghost\" }]"));
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(e[0].contains("unknown preset `ghost`"), "{e:?}");
    let mut files = base();
    files.push(file("presets", "inner", "pick = [{ name = \"a\" }]"));
    files.push(file("presets", "outer", "pick = [{ name = \"inner\" }, { name = \"a\" }]"));
    let e: Vec<String> = Config::build(&files).errors.into_iter().map(|e| e.msg).collect();
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(e[0].contains("`inner` is itself a roulette"), "{e:?}");
    for bad in ["pick = [{ name = \"r\" }, { name = \"a\" }]", "pick = [{ name = \"a\", w = 0.0 }]", "pick = [{ name = \"a\", w = -1.0 }]", "pick = [{ name = \"a\" }]\nset = { \"x\" = 1.0 }"] {
        let cfg = Config::build(&[base().remove(0), file("presets", "r", bad)]);
        assert_eq!(cfg.errors.len(), 1, "{bad}: {:?}", cfg.errors);
        assert!(!cfg.presets.contains_key("r"), "{bad}");
    }
}

#[test]
fn default_lighting_is_operator_only_and_retires_old_lighting_without_mixed_content() {
    let cfg = Config::build(&[
        file("presets", "warm", "toggle=true\nlights={look='warm'}\non_release=['emit unwanted.release']"),
        file("presets", "mixed", "toggle=true\nlights={look='warm'}\nset={held_value=0.6}\ndo=['wait 100ms', 'lights.flash', 'set later_value 0.7']\non_release=['lights.flash', 'emit mixed.released']"),
        file("presets", "queued_light", "quantize='bar'\ntoggle=true\nlights={look='warm'}"),
        file("presets", "queued_mixed", "quantize='bar'\ntoggle=true\nlights={look='warm'}\nset={queued_value=0.8}"),
    ]);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    let mut c = Core::new(cfg.clone(), 0);
    fire(&mut c, "warm");
    fire(&mut c, "mixed");
    run(&mut c, 10);
    for origin in [Origin::Rule, Origin::System, Origin::Timeline, Origin::Twitch] {
        c.submit(Input::Command { cmd: Command::new(origin, Op::Action { name: "lights.default".into(), args: Value::Null }).with_priority(Some(u16::MAX)) });
    }
    let denied = run(&mut c, 10);
    assert_eq!(rejections(&denied).len(), 4);
    assert!(on(&c, "warm") && on(&c, "mixed"));
    assert_eq!(c.get("lights.auto"), Some(&Value::Bool(true)));
    assert!(!denied.iter().any(|o| matches!(o, Output::Action(cmd) if matches!(&cmd.op, Op::Action { name, .. } if name == "lights.default"))));
    c.submit(Input::Signal { name: "beat.bpm".into(), value: 120.0 });
    c.submit(Input::Signal { name: "lfo.bar".into(), value: 0.75 });
    fire(&mut c, "queued_light");
    fire(&mut c, "queued_mixed");
    run(&mut c, 10);
    for _ in 0..2 {
        c.submit(Input::Command { cmd: Command::new(Origin::Deck, Op::Action { name: "lights.default".into(), args: Value::Null }) });
    }
    let out = run(&mut c, 600);
    assert!(!on(&c, "warm") && !on(&c, "queued_light"));
    assert!(on(&c, "mixed") && on(&c, "queued_mixed"));
    assert_eq!(f(&c, "held_value"), 0.6);
    assert_eq!(f(&c, "later_value"), 0.7, "delayed nonlighting content survives");
    assert_eq!(f(&c, "queued_value"), 0.8, "pending mixed content survives");
    assert_eq!(c.get("lights.auto"), Some(&Value::Bool(false)));
    assert!(!out.iter().any(|o| matches!(o, Output::Action(cmd) if matches!(&cmd.op, Op::Action { name, .. } if name == "lights.flash" || name == "lights.layer.select"))));
    assert!(!out.iter().any(|o| matches!(o, Output::Event(e) if e.ty == "unwanted.release")));
    let persisted = c.runtime_state();
    let mut c = Core::new(cfg, 0);
    c.restore(&persisted);
    run(&mut c, 10);
    assert_eq!(c.get("lights.auto"), Some(&Value::Bool(false)));
    assert!(on(&c, "mixed") && on(&c, "queued_mixed"));
    assert_eq!(f(&c, "held_value"), 0.6);
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetRelease { name: "mixed".into() }) });
    let out = run(&mut c, 10);
    assert!(out.iter().any(|o| matches!(o, Output::Event(e) if e.ty == "mixed.released")));
    assert!(!out.iter().any(|o| matches!(o, Output::Action(cmd) if matches!(&cmd.op, Op::Action { name, .. } if name == "lights.flash" || name == "lights.layer.release"))));
    c.submit(Input::Command { cmd: Command::new(Origin::Deck, Op::Action { name: "lights.auto.on".into(), args: Value::Null }) });
    run(&mut c, 10);
    assert_eq!(c.get("lights.auto"), Some(&Value::Bool(true)));
}

#[test]
fn maximum_priority_lighting_release_clears_system_and_manual_htp_owners_only_for_lights() {
    let mut c = Core::new(Config::default(), 0);
    for address in ["lights.fixture.intensity", "unrelated"] {
        for (origin, level) in [(Origin::System, 0.2), (Origin::Ui, 0.9)] {
            c.submit(Input::Command { cmd: Command::new(origin, Op::Set { address: address.into(), value: Value::Float(level) }) });
        }
    }
    run(&mut c, 10);
    for address in ["lights.fixture.intensity", "unrelated"] {
        c.submit(Input::Command { cmd: Command::new(Origin::System, Op::Release { address: address.into() }).with_priority(Some(u16::MAX)) });
    }
    run(&mut c, 10);
    assert_eq!(f(&c, "lights.fixture.intensity"), 0.0);
    assert_eq!(f(&c, "unrelated"), 0.2, "nonlighting release retains the system contribution");
}

#[test]
fn accepted_base_selection_supersedes_lighting_only_presets_but_not_their_other_content() {
    let cfg = Config::build(&[
        file("presets", "warm", "toggle=true\nlights={look='warm'}"),
        file("presets", "cool", "toggle=true\nlights={cue='cool'}"),
        file("presets", "mixed", "toggle=true\nlights={look='warm'}\nset={held_value=0.6}"),
        file("presets", "audio", "toggle=true\nlights={look='warm'}\nsound='airhorn'"),
    ]);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    let mut c = Core::new(cfg, 0);
    fire(&mut c, "warm");
    fire(&mut c, "mixed");
    fire(&mut c, "audio");
    run(&mut c, 10);
    fire(&mut c, "cool");
    let out = run(&mut c, 10);
    let owner = out.iter().find_map(|o| match o {
        Output::Action(cmd) => match &cmd.op {
            Op::Action { name, args } if name == "lights.layer.select" => args.get_path("owner").and_then(Value::as_str).map(str::to_string),
            _ => None,
        },
        _ => None,
    }).unwrap();
    assert!(on(&c, "warm"), "unacknowledged/failed selections must not supersede");
    c.submit(Input::Event { event: Event::new("lights.layer.selected", Origin::Ui, Value::map().with("layer", "base").with("priority", 299).with("source_owner", owner)) });
    run(&mut c, 10);
    assert!(!on(&c, "warm"));
    assert!(on(&c, "cool"));
    assert!(on(&c, "mixed"));
    assert!(on(&c, "audio"), "superseding lighting must not end mixed audio/lighting content");
    assert_eq!(f(&c, "held_value"), 0.6);
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetRelease { name: "mixed".into() }) });
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::PresetRelease { name: "audio".into() }) });
    let out = run(&mut c, 10);
    assert!(!out.iter().any(|o| matches!(o, Output::Action(cmd) if matches!(&cmd.op, Op::Action{name, ..} if name=="lights.layer.release"))), "a superseded mixed preset cannot release the replacement");
}

fn base_selection(out: &[Output]) -> Command {
    out.iter().find_map(|o| match o {
        Output::Action(cmd) if matches!(&cmd.op, Op::Action { name, .. } if name == "lights.layer.select") => Some(cmd.clone()),
        _ => None,
    }).expect("base selection")
}

fn base_selected(cmd: &Command, generation: i64) -> Input {
    let Op::Action { args, .. } = &cmd.op else { panic!("selection action") };
    let payload = Value::map().with("layer", "base").with("generation", generation).with("priority", cmd.priority() as i64)
        .with("source_owner", args.get_path("owner").cloned().unwrap_or(Value::Null));
    Input::Event { event: Event::new("lights.layer.selected", cmd.origin, payload).with_causal(Some(cmd.id)) }
}

#[test]
fn delayed_preset_selection_acknowledgment_preserves_the_newer_toggle() {
    let cfg = Config::build(&[
        file("presets", "old", "hold='300ms'\nlights={look='red'}"),
        file("presets", "new", "toggle=true\nlights={look='blue'}"),
    ]);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    let mut c = Core::new(cfg, 0);
    fire(&mut c, "old");
    let old = base_selection(&run(&mut c, 10));
    fire(&mut c, "new");
    let new = base_selection(&run(&mut c, 10));

    c.submit(base_selected(&old, 1));
    run(&mut c, 10);
    assert!(on(&c, "old"));
    assert!(on(&c, "new"), "an earlier selection cannot retire the newer pending preset");
    c.submit(base_selected(&new, 2));
    let out = run(&mut c, 400);
    assert!(!on(&c, "old"));
    assert!(on(&c, "new"));
    assert!(!out.iter().any(|o| matches!(o, Output::Action(cmd) if matches!(&cmd.op, Op::Action { name, .. } if name == "lights.layer.release"))),
        "acknowledgment and old expiry must not release the replacement");

    fire(&mut c, "new");
    let out = run(&mut c, 10);
    assert!(!on(&c, "new"), "the second firing must toggle off, not restart");
    let releases: Vec<_> = out.iter().filter_map(|o| match o {
        Output::Action(cmd) => match &cmd.op {
            Op::Action { name, args } if name == "lights.layer.release" => Some(args),
            _ => None,
        },
        _ => None,
    }).collect();
    let Op::Action { args: new_args, .. } = &new.op else { unreachable!() };
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0].get_path("owner"), new_args.get_path("owner"));
    assert!(!out.iter().any(|o| matches!(o, Output::Action(cmd) if matches!(&cmd.op, Op::Action { name, .. } if name == "lights.layer.select"))));
}

#[test]
fn external_selection_acknowledgments_do_not_retire_later_requests_or_replay_stale_generations() {
    let cfg = Config::build(&[
        file("presets", "old", "toggle=true\nlights={look='red'}"),
        file("presets", "new", "toggle=true\nlights={look='blue'}"),
    ]);
    let mut c = Core::new(cfg, 0);
    fire(&mut c, "old");
    let old = base_selection(&run(&mut c, 10));
    c.submit(base_selected(&old, 1));
    run(&mut c, 10);
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Action {
        name: "lights.layer.select".into(), args: Value::map().with("layer", "base").with("palette", "green").with("owner", "operator"),
    }) });
    let external = base_selection(&run(&mut c, 10));
    assert!(on(&c, "old"), "an unacknowledged external request cannot steal ownership");
    fire(&mut c, "new");
    let new = base_selection(&run(&mut c, 10));
    c.submit(base_selected(&external, 2));
    run(&mut c, 10);
    assert!(!on(&c, "old"));
    assert!(on(&c, "new"), "the external acknowledgment predates this pending preset request");
    c.submit(base_selected(&new, 3));
    run(&mut c, 10);
    // An acknowledgment's generation, not its delivery order or timestamp, is authoritative.
    c.submit(Input::Event { event: Event::new("lights.layer.selected", Origin::Ui,
        Value::map().with("layer", "base").with("priority", 300).with("generation", 2).with("source_owner", "operator")) });
    let out = run(&mut c, 10);
    assert!(on(&c, "new"), "a stale acknowledgment cannot supersede the accepted owner");
    assert!(!out.iter().any(|o| matches!(o, Output::Action(cmd) if matches!(&cmd.op, Op::Action { name, .. } if name == "lights.layer.release"))));
}

#[test]
fn runtime_restore_discards_legacy_generation_and_effect_activation_without_losing_unrelated_state() {
    let mut c = Core::new(Config::default(), 0);
    let values = [
        ("lights.stick1.intensity", "layer:rhythm:27", Value::Float(1.0)),
        ("lights.effect.lx_hit_pixel_run.active", "layer:accent:28", Value::Bool(true)),
        ("lights.effect.demo_aurora.active", "manual", Value::Bool(true)),
        ("lx.color.a", "cuelist:demo_aurora", Value::from("#ff0000")),
        ("unrelated_level", "manual", Value::Float(0.4)),
        ("lights.master", "manual", Value::Float(0.6)),
        ("lights.blackout", "manual", Value::Bool(true)),
    ];
    for (address, key, value) in &values {
        c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Set { address: (*address).into(), value: value.clone() }).with_key(*key) });
    }
    run(&mut c, 10);
    let mut saved = c.runtime_state();
    assert!(saved.overrides.iter().all(|(a, _)| matches!(a.as_str(), "unrelated_level" | "lights.master" | "lights.blackout")), "new snapshots must not save lighting playback");
    // Simulate the persisted DB produced by the old resolver, including a generation
    // with no corresponding active layer or playing cue list.
    saved.overrides.extend(values[..4].iter().map(|(a, key, value)| ((*a).into(), se_core::state::Override {
        key: (*key).into(), priority: 299, value: value.clone(), seq: 0, expires: None, anim: None, origin: Origin::Ui, causal: None,
    })));
    let mut restored = Core::new(Config::default(), 0);
    restored.submit(Input::Declare { address: "lights.stick1.intensity".into(), meta: Meta::float(0.0, [0.0, 1.0]).htp() });
    for name in ["lx_hit_pixel_run", "demo_aurora"] {
        restored.submit(Input::Declare { address: format!("lights.effect.{name}.active"), meta: Meta::boolean(false) });
    }
    run(&mut restored, 10);
    restored.restore(&saved);
    run(&mut restored, 10);
    assert_eq!(f(&restored, "lights.stick1.intensity"), 0.0);
    assert_eq!(restored.get("lights.effect.lx_hit_pixel_run.active"), Some(&Value::Bool(false)));
    assert_eq!(restored.get("lights.effect.demo_aurora.active"), Some(&Value::Bool(false)));
    assert!(restored.get("lx.color.a").is_none());
    assert_eq!(f(&restored, "unrelated_level"), 0.4);
    assert_eq!(f(&restored, "lights.master"), 0.6);
    assert_eq!(restored.get("lights.blackout"), Some(&Value::Bool(true)));
}

#[test]
fn rules_read_live_signals_and_event_fields_together() {
    let cfg = Config::build(&[file("rules", "signal", "when='music.drop'\nif='context.song > 0.7 && band.level > 0.2 && event.strength >= 0.8'\ndo=['set accepted_drop 1']")]);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    let mut c = Core::new(cfg, 0);
    c.submit(Input::Signal { name: "context.song".into(), value: 0.8 });
    c.submit(Input::Signal { name: "band.level".into(), value: 0.3 });
    c.submit(Input::Event { event: Event::new("music.drop", Origin::Audio, Value::map().with("strength", 0.5)) });
    run(&mut c, 10);
    assert!(c.get("accepted_drop").is_none());
    c.submit(Input::Signal { name: "context.song".into(), value: 0.8 });
    c.submit(Input::Event { event: Event::new("music.drop", Origin::Audio, Value::map().with("strength", 0.9)) });
    run(&mut c, 10);
    assert_eq!(f(&c, "accepted_drop"), 1.0);
}

#[test]
fn animation_uses_same_tick_owner_seed_beneath_an_operator_override() {
    let mut c = Core::new(Config::default(), 0);
    let address = "lights.effect.pulse.size";
    c.submit(Input::Declare { address: address.into(), meta: Meta::float(1.0, [0.0, 1.0]) });
    let send = |key: &str, priority, op| Input::Command {
        cmd: Command::new(Origin::Ui, op).with_priority(Some(priority)).with_key(key),
    };
    c.submit(send("operator", 300, Op::Set { address: address.into(), value: Value::Float(0.4) }));
    c.step();
    c.submit(send("layer:motion:1", 200, Op::Set { address: address.into(), value: Value::Float(0.0) }));
    c.submit(send("layer:motion:1", 200, Op::Animate { address: address.into(), to: Value::Float(1.0), ms: 1000, ease: se_proto::Ease::Linear }));
    c.step();
    let start = c.now();
    c.advance_to(start + 500 * MS);
    assert!((f(&c, address) - 0.4).abs() < 1e-6, "operator remains authoritative");
    c.submit(send("operator", 300, Op::Release { address: address.into() }));
    c.step();
    assert!((f(&c, address) - 0.5).abs() < 0.02, "hidden contribution must start at its own zero seed");
    c.advance_to(start + 1100 * MS);
    assert_eq!(f(&c, address), 1.0);
}

#[test]
fn fading_one_intensity_owner_does_not_copy_another_htp_owner() {
    let mut c = Core::new(Config::default(), 0);
    let address = "lights.par.intensity";
    c.submit(Input::Declare { address: address.into(), meta: Meta::float(0.0, [0.0, 1.0]).htp() });
    let send = |key: &str, op| Input::Command {
        cmd: Command::new(Origin::Ui, op).with_priority(Some(200)).with_key(key),
    };
    c.submit(send("quiet", Op::Set { address: address.into(), value: Value::Float(0.2) }));
    c.submit(send("bright", Op::Set { address: address.into(), value: Value::Float(0.8) }));
    c.step();
    c.submit(send("quiet", Op::Animate { address: address.into(), to: Value::Float(0.0), ms: 1000, ease: se_proto::Ease::Linear }));
    c.step();
    let start = c.now();
    c.advance_to(start + 500 * MS);
    assert_eq!(f(&c, address), 0.8);
    c.submit(send("bright", Op::Release { address: address.into() }));
    c.step();
    assert!((f(&c, address) - 0.1).abs() < 0.01, "quiet layer must fade from its own 0.2, not bright's 0.8");
    c.advance_to(start + 1100 * MS);
    assert_eq!(f(&c, address), 0.0);
}

#[test]
fn keyed_manual_trigger_retirement_preserves_other_owners_but_unkeyed_release_clears() {
    let mut c = Core::new(Config::default(), 0);
    let address = "lighting.accent";
    c.submit(Input::DeclareTrigger { address: address.into(), spec: se_core::triggers::TriggerSpec {
        attack_ms: 0, hold_ms: None, release_ms: 0, ..Default::default()
    } });
    let send = |key: &str, priority, op| Input::Command {
        cmd: Command::new(Origin::Ui, op).with_priority(Some(priority)).with_key(key),
    };
    c.submit(send("operator", 400, Op::Trigger { address: address.into(), payload: Value::Null }));
    c.step();
    assert_eq!(c.get("lighting.accent.active"), Some(&Value::Bool(true)));
    c.submit(send("retired-layer", 300, Op::Release { address: address.into() }));
    c.step();
    assert_eq!(c.get("lighting.accent.active"), Some(&Value::Bool(true)), "stale keyed release must not clear another owner's trigger");
    assert_eq!(f(&c, "lighting.accent.env"), 1.0);
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Release { address: address.into() }) });
    c.step();
    assert_eq!(c.get("lighting.accent.active"), Some(&Value::Bool(false)), "unkeyed manual release keeps its broad operator semantics");
    assert_eq!(f(&c, "lighting.accent.env"), 0.0);
}
