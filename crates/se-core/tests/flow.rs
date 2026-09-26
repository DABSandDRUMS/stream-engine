use se_core::config::SourceFile;
use se_core::{Config, Core, Input, Output};
use se_proto::{Actor, Command, Event, Meta, Op, Origin, Role, Value};

const MS: u64 = 1_000_000;

fn file(kind: &str, name: &str, src: &str) -> SourceFile {
    SourceFile { kind: kind.into(), name: name.into(), path: format!("{kind}/{name}.toml"), table: toml::from_str(src).unwrap() }
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
