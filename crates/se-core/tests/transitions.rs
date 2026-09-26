//! Transition selection (§4.4): scene-pair pools with fallback to the target scene's pool, and
//! chat votes picking the next take's transition.

use se_core::config::SourceFile;
use se_core::{Config, Core, Input, Output};
use se_proto::{Actor, Command, Op, Origin, Role, Value};

const MS: u64 = 1_000_000;

fn file(kind: &str, name: &str, src: &str) -> SourceFile {
    SourceFile { kind: kind.into(), name: name.into(), path: format!("{kind}/{name}.toml"), table: toml::from_str(src).unwrap() }
}

const NODES: &str = "[canvas.wide]\nnodes = [{ src = \"cam\" }]\n";

fn core(project_extra: &str) -> Core {
    let files = vec![
        file("project", "project", &format!("schema = 1\n{project_extra}")),
        file("transitions", "zoomblur", "kind = \"shader\"\nshader = \"transitions/zoomblur.wgsl\"\nms = 800"),
        file("transitions", "glitch", "kind = \"shader\"\nshader = \"transitions/glitch.wgsl\"\nms = 600"),
        file("scenes", "a", &format!("key = 1\n{NODES}")),
        file(
            "scenes",
            "b",
            &format!(
                "key = 2\n{NODES}[transitions]\npool = [{{ name = \"morph\" }}]\nms = 500\n[transitions.from.a]\npool = [{{ name = \"zoomblur\" }}]\nms = 300\n[transitions.from.c]\nms = 250"
            ),
        ),
        file("scenes", "c", &format!("key = 3\n{NODES}[transitions]\nname = \"cut\"\n[transitions.from.b]\npool = [{{ name = \"glitch\" }}]")),
    ];
    let c = Config::build(&files);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    let mut core = Core::new(c, 1_000 * MS);
    run(&mut core, 5);
    core
}

fn run(c: &mut Core, ms: u64) -> Vec<Output> {
    let mut out = Vec::new();
    for _ in 0..(ms * MS).div_ceil(c.period()) {
        c.step();
        out.extend(c.drain_outputs());
    }
    out
}

fn ui(op: Op) -> Input {
    Input::Command { cmd: Command::new(Origin::Ui, op) }
}

/// Take `scene`; returns (transition, ms, chosen by).
fn take(c: &mut Core, scene: &str, transition: Option<&str>) -> (String, i64, String) {
    c.submit(ui(Op::SceneCut { scene: scene.into(), transition: transition.map(String::from) }));
    let out = run(c, 5);
    let e = out
        .iter()
        .find_map(|o| match o {
            Output::Event(e) if e.ty == "scene.take" => Some(e.clone()),
            _ => None,
        })
        .expect("scene.take");
    run(c, 1_000);
    let s = |k: &str| e.payload.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
    (s("transition"), e.payload.get_path("ms").and_then(Value::as_i64).unwrap_or(-1), s("by"))
}

fn cut_to(c: &mut Core, scene: &str) {
    c.submit(ui(Op::SceneCut { scene: scene.into(), transition: Some("cut".into()) }));
    run(c, 5);
}

fn vote(c: &mut Core, user: &str, name: &str) -> Vec<Output> {
    let actor = Actor { platform: "twitch".into(), id: format!("id-{user}"), name: user.into(), roles: vec![Role::Everyone] };
    let cmd = Command::new(Origin::Chat, Op::Action { name: "transition.vote".into(), args: Value::map().with("name", name) }).with_actor(Some(actor));
    c.submit(Input::Command { cmd });
    run(c, 5)
}

fn votes(c: &Core) -> Value {
    c.get("show.transition.votes").cloned().unwrap_or_default()
}

fn live(c: &mut Core) {
    c.submit(ui(Op::ModeSet { mode: "live".into() }));
    run(c, 5);
}

#[test]
fn scene_pair_pools_fall_back_to_the_target_scene() {
    let mut c = core("");
    assert_eq!(c.get("show.scene.program").and_then(Value::as_str), Some("a"));
    // a → b: the pair's own pool and duration
    assert_eq!(take(&mut c, "b", None), ("zoomblur".into(), 300, "pool".into()));
    // b → a: `a` has no transitions at all
    assert_eq!(take(&mut c, "a", None), ("fade".into(), 700, "default".into()));
    cut_to(&mut c, "c");
    // c → b: the pair only sets the duration; the pool is b's
    assert_eq!(take(&mut c, "b", None), ("morph".into(), 250, "pool".into()));
    // b → c: the pair's pool beats c's fixed `name = "cut"`
    assert_eq!(take(&mut c, "c", None), ("glitch".into(), 600, "pool".into()));
    // a → c: no pair, c's fixed transition
    cut_to(&mut c, "a");
    assert_eq!(take(&mut c, "c", None), ("cut".into(), 0, "fixed".into()));
}

#[test]
fn project_default_covers_scenes_without_their_own() {
    let mut c = core("[transitions]\npool = [{ name = \"glitch\" }]\nms = 450\n[transitions.from.b]\nname = \"zoomblur\"");
    // a → b: b's own pair pool wins over the project default (its duration too)
    assert_eq!(take(&mut c, "b", None), ("zoomblur".into(), 300, "pool".into()));
    // b → a: `a` has nothing; the project's pair for leaving `b` is fixed, with the project duration
    assert_eq!(take(&mut c, "a", None), ("zoomblur".into(), 450, "project".into()));
    // c → a: the project pool
    cut_to(&mut c, "c");
    assert_eq!(take(&mut c, "a", None), ("glitch".into(), 450, "project".into()));
    // a → c: c's fixed cut beats the default and stays instant despite the project duration
    assert_eq!(take(&mut c, "c", None), ("cut".into(), 0, "fixed".into()));
    // c → b: b's pool (the c pair only sets a duration)
    assert_eq!(take(&mut c, "b", None), ("morph".into(), 250, "pool".into()));
}

#[test]
fn chat_votes_beat_the_project_default() {
    let mut c = core("[transitions]\nname = \"zoomblur\"\n[transition_vote]\nwindow = \"30s\"");
    live(&mut c);
    cut_to(&mut c, "b");
    vote(&mut c, "ann", "glitch");
    assert_eq!(take(&mut c, "a", None), ("glitch".into(), 600, "vote".into()));
    cut_to(&mut c, "b");
    assert_eq!(take(&mut c, "a", None), ("zoomblur".into(), 800, "project".into()), "no votes left: the default");
}

#[test]
fn pair_pools_are_validated() {
    let files = vec![
        file("project", "project", "schema = 1\n[transitions]\nname = \"swoosh\"\n[transitions.from.gone]\npool = [{ name = \"fade\" }]"),
        file("scenes", "a", "[transitions.from.nowhere]\npool = [{ name = \"fade\" }]\n[transitions.from.b]\npool = [{ name = \"sparkles\" }]"),
        file("scenes", "b", ""),
    ];
    let c = Config::build(&files);
    let msgs: Vec<&str> = c.errors.iter().map(|e| e.msg.as_str()).collect();
    assert!(msgs.iter().any(|m| m.contains("unknown scene `nowhere`")), "{msgs:?}");
    assert!(msgs.iter().any(|m| m.contains("unknown transition `sparkles`")), "{msgs:?}");
    let project: Vec<&str> = c.errors.iter().filter(|e| e.file.ends_with("project.toml")).map(|e| e.msg.as_str()).collect();
    assert!(project.iter().any(|m| m.contains("unknown transition `swoosh`")) && project.iter().any(|m| m.contains("unknown scene `gone`")), "{project:?}");
}

#[test]
fn chat_votes_pick_the_next_take() {
    let mut c = core("[transition_vote]\nwindow = \"30s\"\nchoices = [\"fade\", \"zoomblur\", \"glitch\"]");
    live(&mut c);
    let out = vote(&mut c, "ann", "Glitch");
    assert!(out.iter().any(|o| matches!(o, Output::Event(e) if e.ty == "transition.vote")));
    vote(&mut c, "bob", "fade");
    vote(&mut c, "cat", "fade");
    // one vote per viewer: cat changes their mind
    vote(&mut c, "cat", "glitch");
    // not a choice: ignored
    vote(&mut c, "dan", "morph");
    assert_eq!(votes(&c), Value::map().with("fade", 1).with("glitch", 2));
    // the a → b pair pool would pick zoomblur; the vote wins (with the pair's duration)
    assert_eq!(take(&mut c, "b", None), ("glitch".into(), 300, "vote".into()));
    assert_eq!(votes(&c), Value::map(), "votes are spent by the take");
    assert_eq!(take(&mut c, "a", None).2, "default");
    // a transition named by the operator beats the vote, which waits for the next take
    vote(&mut c, "ann", "glitch");
    assert_eq!(take(&mut c, "b", Some("fade")).2, "command");
    assert_eq!(take(&mut c, "a", None), ("glitch".into(), 600, "vote".into()));
    // ties go to the transition voted for first
    vote(&mut c, "ann", "glitch");
    vote(&mut c, "bob", "fade");
    assert_eq!(take(&mut c, "b", None).0, "glitch");
}

#[test]
fn votes_expire_and_pause_outside_live() {
    let mut c = core("[transition_vote]\nwindow = \"30s\"");
    live(&mut c);
    vote(&mut c, "ann", "fade");
    assert_eq!(votes(&c), Value::map().with("fade", 1));
    run(&mut c, 31_000);
    assert_eq!(votes(&c), Value::map(), "the window ran out");
    assert_eq!(take(&mut c, "b", None).2, "pool");
    // chat effects pause in brb: the vote doesn't count
    c.submit(ui(Op::ModeSet { mode: "brb".into() }));
    run(&mut c, 5);
    vote(&mut c, "ann", "fade");
    assert_eq!(votes(&c), Value::map());
    // no [transition_vote]: voting is off
    let mut off = core("");
    live(&mut off);
    vote(&mut off, "ann", "fade");
    assert_eq!(votes(&off), Value::Null);
}
