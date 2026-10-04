//! Auto sequence presets end to end through the core: in-order timing with per-step dwell,
//! random order without repeats and replay determinism, manual cuts restarting the dwell,
//! stop/panic/removal, immediate activation from a sequence scene, scene-entry stop, and restore.

use se_core::config::SourceFile;
use se_core::{Config, Core, Input, Output, RuntimeState};
use se_proto::{Actor, Command, Op, Origin, Role, Ts, Value};

const MS: u64 = 1_000_000;
const T0: Ts = 1_000 * MS;
const NODES: &str = "[canvas.wide]\nnodes = [{ src = \"cam\" }]\n";

const ACTIVE: &str = "show.autoseq.active";
const PRESET: &str = "show.autoseq.preset";
const STEP: &str = "show.autoseq.step";
const NEXT: &str = "show.autoseq.next";
const NEXT_AT: &str = "show.autoseq.next_at";
const DWELL_MS: &str = "show.autoseq.dwell_ms";

/// b → c (5 s) → d, 2 s default dwell, instant cuts.
const IN_ORDER: &str =
    "label = \"Cycle\"\ndwell = \"2s\"\ntransition = \"cut\"\n[[step]]\nscene = \"b\"\n[[step]]\nscene = \"c\"\ndwell = \"5s\"\n[[step]]\nscene = \"d\"";
const RANDOM: &str = "order = \"random\"\ndwell = \"1s\"\ntransition = \"cut\"\n[[step]]\nscene = \"b\"\n[[step]]\nscene = \"c\"\n[[step]]\nscene = \"d\"";

fn file(kind: &str, name: &str, src: &str) -> SourceFile {
    SourceFile { kind: kind.into(), name: name.into(), path: format!("{kind}/{name}.toml"), table: toml::from_str(src).unwrap() }
}

/// Scenes a..d (program starts on `a`) plus `autoseq/<name>.toml` files.
fn files(seqs: &[(&str, &str)]) -> Vec<SourceFile> {
    let mut v = vec![file("project", "project", "schema = 1\nname = \"autoseq\"")];
    for (i, s) in ["a", "b", "c", "d"].iter().enumerate() {
        v.push(file("scenes", s, &format!("key = {}\n{NODES}", i + 1)));
    }
    v.extend(seqs.iter().map(|(n, src)| file("autoseq", n, src)));
    v
}

fn core(seqs: &[(&str, &str)]) -> Core {
    let c = Config::build(&files(seqs));
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    let core = Core::new(c, T0);
    assert!(core.config.errors.is_empty(), "{:?}", core.config.errors);
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

/// Run and log every `autoseq.step` as (core time, scene).
fn watch(c: &mut Core, ms: u64) -> Vec<(Ts, String)> {
    let mut log = Vec::new();
    for _ in 0..(ms * MS).div_ceil(c.period()) {
        c.step();
        for e in events(&c.drain_outputs(), "autoseq.step") {
            log.push((c.now(), e.payload.get_path("scene").and_then(Value::as_str).unwrap().to_string()));
        }
    }
    log
}

fn cmd(text: &str) -> Input {
    Input::Command { cmd: Command::new(Origin::Ui, Op::parse(text).unwrap()) }
}

fn events(out: &[Output], ty: &str) -> Vec<se_proto::Event> {
    out.iter().filter_map(|o| if let Output::Event(e) = o { (e.ty == ty).then(|| e.clone()) } else { None }).collect()
}

fn s(c: &Core, a: &str) -> String {
    c.get(a).and_then(Value::as_str).unwrap_or("").to_string()
}

fn int(c: &Core, a: &str) -> i64 {
    c.get(a).and_then(Value::as_i64).unwrap_or(i64::MIN)
}

fn program(c: &Core) -> String {
    s(c, "show.scene.program")
}

fn active(c: &Core) -> bool {
    c.get(ACTIVE).is_some_and(Value::truthy)
}

#[test]
fn in_order_cycle_uses_per_step_dwell() {
    let mut c = core(&[("cycle", IN_ORDER)]);
    run(&mut c, 5);
    assert_eq!(program(&c), "a");
    c.submit(cmd("autoseq.play cycle"));
    let out = run(&mut c, 1);
    let started = c.now();
    assert_eq!(events(&out, "autoseq.started").len(), 1);
    let step = events(&out, "autoseq.step").remove(0);
    assert_eq!(step.payload.get_path("scene").and_then(Value::as_str), Some("b"), "not a step: switches to the first step at once");
    assert_eq!(step.payload.get_path("transition").and_then(Value::as_str), Some("cut"));
    assert_eq!((program(&c), s(&c, PRESET), active(&c)), ("b".into(), "cycle".into(), true));
    assert_eq!((int(&c, STEP), s(&c, NEXT), int(&c, DWELL_MS)), (0, "c".into(), 2_000));
    // next_at is on the same clock as show.transition.start
    assert_eq!(int(&c, NEXT_AT) - int(&c, "show.transition.start"), 2_000 * MS as i64);
    assert_eq!(int(&c, NEXT_AT), (started + 2_000 * MS) as i64);

    let log = watch(&mut c, 12_000);
    let scenes: Vec<&str> = log.iter().map(|(_, s)| s.as_str()).collect();
    assert_eq!(scenes, ["c", "d", "b", "c"], "{log:?}");
    let dwells = [2_000, 5_000, 2_000, 2_000];
    let mut prev = started;
    for ((at, scene), dwell) in log.iter().zip(dwells) {
        let late = at - prev - dwell * MS;
        assert!(late < c.period(), "{scene} after {} ms", (at - prev) / MS);
        prev = *at;
    }
    assert_eq!((s(&c, NEXT), int(&c, DWELL_MS)), ("d".into(), 5_000));
}

#[test]
fn random_never_repeats_and_replays() {
    let mut live = core(&[("shuffle", RANDOM)]);
    let mut log = Vec::new();
    run(&mut live, 20);
    live.submit(cmd("autoseq.play shuffle"));
    let first = watch(&mut live, 20_000);
    log.extend(live.drain_applied());
    // a manual cut in between: the scene it put on air is not picked next either
    live.submit(cmd("scene.cut c cut"));
    let mut second = Vec::new();
    for _ in 0..(10_000 * MS).div_ceil(live.period()) {
        live.step();
        for e in events(&live.drain_outputs(), "autoseq.step") {
            second.push((live.now(), e.payload.get_path("scene").and_then(Value::as_str).unwrap().to_string()));
        }
        assert_ne!(s(&live, NEXT), program(&live), "next is the scene on air");
    }
    log.extend(live.drain_applied());
    let target = live.tick_index();

    assert!(first.len() >= 15 && second.len() >= 8, "{} + {} switches", first.len(), second.len());
    assert_ne!(second[0].1, "c");
    for seg in [&first, &second] {
        for w in seg.windows(2) {
            assert_ne!(w[0].1, w[1].1, "random picked the scene already on air: {seg:?}");
        }
    }
    for scene in ["b", "c", "d"] {
        assert!(first.iter().any(|(_, s)| s == scene), "{scene} never shown");
    }
    let seen: Vec<(Ts, String)> = first.into_iter().chain(second).collect();

    let mut rep = core(&[("shuffle", RANDOM)]);
    let mut i = 0;
    let mut replayed = Vec::new();
    while rep.tick_index() < target {
        while i < log.len() && log[i].0 == rep.tick_index() + 1 {
            rep.submit(log[i].1.clone());
            i += 1;
        }
        rep.step();
        for e in events(&rep.drain_outputs(), "autoseq.step") {
            replayed.push((rep.now(), e.payload.get_path("scene").and_then(Value::as_str).unwrap().to_string()));
        }
    }
    assert_eq!(seen, replayed);
    for a in ["show.scene.program", STEP, NEXT, NEXT_AT, DWELL_MS] {
        assert_eq!(live.get(a), rep.get(a), "{a}");
    }
    assert_eq!(live.runtime_state(), rep.runtime_state());
}

#[test]
fn manual_cut_restarts_the_dwell() {
    let mut c = core(&[("cycle", "dwell = \"3s\"\ntransition = \"cut\"\n[[step]]\nscene = \"b\"\n[[step]]\nscene = \"c\"\n[[step]]\nscene = \"d\"")]);
    c.submit(cmd("autoseq.play cycle"));
    run(&mut c, 2_000);
    assert_eq!(program(&c), "b");
    // a cut to another step: its dwell starts now, the following step comes next
    c.submit(cmd("scene.cut d cut"));
    run(&mut c, 1);
    let cut_at = c.now();
    assert_eq!((int(&c, STEP), s(&c, NEXT)), (2, "b".into()));
    assert_eq!(int(&c, NEXT_AT), (cut_at + 3_000 * MS) as i64);
    run(&mut c, 2_900);
    assert_eq!(program(&c), "d", "the dwell restarted at the cut");
    let log = watch(&mut c, 200);
    assert_eq!(log.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(), ["b"]);
    // a cut to a scene outside the list: preset dwell, then the step after the last one shown
    c.submit(cmd("scene.cut a cut"));
    run(&mut c, 1);
    assert_eq!((program(&c), int(&c, STEP), s(&c, NEXT)), ("a".into(), -1, "c".into()));
    assert!(active(&c));
    let log = watch(&mut c, 3_100);
    assert_eq!(log.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(), ["c"]);
}

#[test]
fn stop_and_panic_stop_it() {
    let mut c = core(&[("cycle", IN_ORDER)]);
    c.submit(cmd("autoseq.play cycle"));
    run(&mut c, 500);
    c.submit(cmd("autoseq.stop"));
    let out = run(&mut c, 5);
    let stopped = events(&out, "autoseq.stopped");
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0].payload.get_path("reason").and_then(Value::as_str), Some("stop"));
    assert_eq!(stopped[0].payload.get_path("preset").and_then(Value::as_str), Some("cycle"));
    assert!(!active(&c));
    assert_eq!((s(&c, PRESET), int(&c, STEP), s(&c, NEXT), int(&c, NEXT_AT)), ("cycle".into(), -1, String::new(), 0));
    assert!(watch(&mut c, 5_000).is_empty(), "stopped: program stays");
    assert_eq!(program(&c), "b");

    // toggle plays the selected preset again; panic stops it
    c.submit(cmd("autoseq.toggle"));
    run(&mut c, 5);
    assert!(active(&c));
    assert_eq!(program(&c), "c", "starting again advances without waiting for the dwell");
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Panic) });
    let out = run(&mut c, 5);
    assert_eq!(events(&out, "autoseq.stopped")[0].payload.get_path("reason").and_then(Value::as_str), Some("panic"));
    assert!(!active(&c));
    assert!(watch(&mut c, 3_000).is_empty());
}

#[test]
fn deleting_the_preset_stops_it() {
    let mut c = core(&[("cycle", IN_ORDER), ("other", RANDOM)]);
    c.submit(cmd("autoseq.play cycle"));
    run(&mut c, 500);
    // an edit keeps it running with the new definition
    let edited = IN_ORDER.replace("dwell = \"2s\"", "dwell = \"4s\"");
    c.submit(Input::Config { config: Box::new(Config::build(&files(&[("cycle", edited.as_str()), ("other", RANDOM)]))) });
    let out = run(&mut c, 5);
    assert!(events(&out, "autoseq.stopped").is_empty());
    assert!(active(&c));
    c.submit(Input::Config { config: Box::new(Config::build(&files(&[("other", RANDOM)]))) });
    let out = run(&mut c, 5);
    let stopped = events(&out, "autoseq.stopped");
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0].payload.get_path("reason").and_then(Value::as_str), Some("removed"));
    assert!(!active(&c));
    assert_eq!(s(&c, PRESET), "");
    assert_eq!(program(&c), "b");
}

#[test]
fn starting_from_a_step_advances_immediately_then_uses_the_new_steps_dwell() {
    let mut c = core(&[("cycle", IN_ORDER)]);
    c.submit(cmd("scene.cut c cut"));
    run(&mut c, 100);
    c.submit(cmd("autoseq.select cycle"));
    run(&mut c, 5);
    assert_eq!((program(&c), active(&c)), ("c".into(), false), "select alone does not start or switch");

    c.submit(cmd("autoseq.play"));
    run(&mut c, 1);
    let started = c.now();
    assert_eq!((program(&c), int(&c, STEP), s(&c, NEXT), int(&c, DWELL_MS)), ("d".into(), 2, "b".into(), 2_000));
    assert_eq!(int(&c, NEXT_AT), (started + 2_000 * MS) as i64);
    run(&mut c, 1_900);
    assert_eq!(program(&c), "d");
    let log = watch(&mut c, 200);
    assert_eq!(log.iter().map(|(_, scene)| scene.as_str()).collect::<Vec<_>>(), ["b"]);
    assert!(log[0].0 - started - 2_000 * MS < c.period());
}

#[test]
fn starting_a_single_step_sequence_stays_without_a_spurious_switch() {
    let mut c = core(&[("solo", "dwell='1s'\ntransition='cut'\n[[step]]\nscene='a'")]);
    c.submit(cmd("autoseq.play solo"));
    let out = run(&mut c, 1);
    assert!(active(&c));
    assert_eq!((program(&c), int(&c, STEP), int(&c, DWELL_MS)), ("a".into(), 0, 1_000));
    assert!(events(&out, "scene.take").is_empty());
    assert!(watch(&mut c, 2_500).is_empty());
    assert_eq!(program(&c), "a");
}

#[test]
fn scene_entry_stop_prevents_a_due_switch_during_and_after_a_transition() {
    for transition in ["cut", "fade"] {
        let mut src = files(&[("cycle", IN_ORDER)]);
        let manual = src.iter_mut().find(|f| f.kind == "scenes" && f.name == "a").unwrap();
        manual.table.insert("on_enter".into(), toml::Value::Array(vec![toml::Value::String("autoseq.stop".into())]));
        let mut c = Core::new(Config::build(&src), T0);
        c.submit(cmd("autoseq.play cycle"));
        run(&mut c, 1_900);
        c.submit(cmd(&format!("scene.cut a {transition}")));
        run(&mut c, 1);
        assert_eq!(program(&c), "a");
        assert!(!active(&c), "scene entry stops before the transition completes");
        assert_eq!((s(&c, NEXT), int(&c, NEXT_AT), int(&c, DWELL_MS)), (String::new(), 0, 0));
        assert!(watch(&mut c, 6_000).is_empty(), "no scheduled scene switch survives");
        assert_eq!(program(&c), "a");

        c.submit(cmd("autoseq.toggle cycle"));
        run(&mut c, 1);
        assert!(active(&c));
        assert_eq!(program(&c), "b", "explicit start leaves the manual scene immediately");
    }
}


#[test]
fn chat_cannot_start_it() {
    let mut c = core(&[("cycle", IN_ORDER)]);
    let actor = Actor { platform: "twitch".into(), id: "id-v".into(), name: "v".into(), roles: vec![Role::Everyone] };
    let cmd = Command::new(Origin::Chat, Op::Action { name: "autoseq.play".into(), args: Value::map().with("name", "cycle") }).with_actor(Some(actor));
    c.submit(Input::Command { cmd });
    let out = run(&mut c, 5);
    assert!(out.iter().any(|o| matches!(o, Output::Ack { ok: false, error: Some(e), .. } if e == "chat cannot change scenes")));
    assert!(!active(&c));
    assert_eq!(program(&c), "a");
}

#[test]
fn restore_resumes_with_a_fresh_dwell() {
    let mut c = core(&[("cycle", IN_ORDER)]);
    c.submit(cmd("autoseq.play cycle"));
    run(&mut c, 2_500);
    assert_eq!(program(&c), "c");
    let rs: RuntimeState = c.runtime_state();
    assert_eq!((rs.autoseq.preset.as_str(), rs.autoseq.active, rs.autoseq.last), ("cycle", true, Some(1)));

    let mut back = core(&[("cycle", IN_ORDER)]);
    run(&mut back, 1);
    back.restore(&rs);
    let restored_at = back.now();
    run(&mut back, 1);
    assert_eq!((program(&back), active(&back), s(&back, NEXT)), ("c".into(), true, "d".into()));
    assert_eq!(int(&back, NEXT_AT), (restored_at + 5_000 * MS) as i64);
}

#[test]
fn missing_scenes_are_reported_and_skipped() {
    let seq = "transition = \"nope\"\ndwell = \"1s\"\n[[step]]\nscene = \"b\"\n[[step]]\nscene = \"gone\"\n[[step]]\nscene = \"c\"\ntransition = \"cut\"";
    let mut c = Core::new(Config::build(&files(&[("cycle", seq)])), T0);
    let errs: Vec<String> = c.config.errors.iter().filter(|e| e.file == "autoseq/cycle.toml").map(|e| e.msg.clone()).collect();
    assert!(errs.iter().any(|m| m.contains("unknown scene `gone`")), "{errs:?}");
    assert!(errs.iter().any(|m| m.contains("unknown transition `nope`")), "{errs:?}");
    let q = c.query("autoseq", &Value::Null).unwrap();
    let first = &q.as_list().unwrap()[0];
    assert_eq!(first.get_path("name").and_then(Value::as_str), Some("cycle"));
    assert_eq!(first.get_path("order").and_then(Value::as_str), Some("in_order"));
    assert_eq!(first.get_path("dwell_ms").and_then(Value::as_i64), Some(1_000));
    assert_eq!(first.get_path("steps.2.transition").and_then(Value::as_str), Some("cut"));
    assert!(first.get_path("steps.0.dwell_ms").is_some_and(Value::is_null));

    c.submit(cmd("autoseq.play cycle"));
    run(&mut c, 5);
    assert_eq!(s(&c, NEXT), "c", "the missing scene is skipped");
    let log = watch(&mut c, 2_100);
    assert_eq!(log.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(), ["c", "b"]);
}

#[test]
fn random_excludes_recent_scenes_and_preserves_history_across_restart() {
    let seq = "order='random'\navoid_repeat=2\ndwell='1s'\ntransition='cut'\n[[step]]\nscene='a'\n[[step]]\nscene='b'\n[[step]]\nscene='c'\n[[step]]\nscene='d'";
    let mut c = core(&[("mix", seq)]);
    c.submit(cmd("autoseq.play mix"));
    run(&mut c, 1);
    let mut shown = vec![program(&c)];
    for _ in 0..24 {
        c.submit(cmd("autoseq.next"));
        run(&mut c, 1);
        let next = program(&c);
        assert!(!shown.iter().rev().take(2).any(|s| s == &next), "{shown:?} -> {next}");
        shown.push(next);
    }
    let saved = c.runtime_state();
    let recent = shown[shown.len() - 2..].to_vec();
    let mut restored = core(&[("mix", seq)]);
    restored.restore(&saved);
    run(&mut restored, 1);
    assert_eq!(program(&restored), shown.last().unwrap().as_str());
    assert!(!recent.contains(&s(&restored, NEXT)));
    restored.submit(cmd("autoseq.next"));
    run(&mut restored, 1);
    assert!(!recent.contains(&program(&restored)));
}

#[test]
fn random_relaxes_oldest_exclusions_when_pool_shrinks_without_repeating_program() {
    let seq = "order='random'\navoid_repeat=3\ndwell='1s'\ntransition='cut'\n[[step]]\nscene='a'\n[[step]]\nscene='b'\n[[step]]\nscene='c'\n[[step]]\nscene='d'";
    let mut c = core(&[("mix", seq)]);
    c.submit(cmd("autoseq.play mix"));
    run(&mut c, 1);
    for _ in 0..5 {
        c.submit(cmd("autoseq.next"));
        run(&mut c, 1);
    }
    let before = program(&c);
    let other = ["a", "b", "c", "d"].into_iter().find(|s| *s != before).unwrap();
    let smaller = format!("order='random'\navoid_repeat=3\ndwell='1s'\ntransition='cut'\n[[step]]\nscene='{before}'\n[[step]]\nscene='{other}'");
    c.submit(Input::Config { config: Box::new(Config::build(&files(&[("mix", &smaller)]))) });
    run(&mut c, 1);
    assert_eq!(s(&c, NEXT), other);
    c.submit(cmd("autoseq.next"));
    run(&mut c, 1);
    assert_eq!(program(&c), other);
    assert_eq!(s(&c, NEXT), before);
}

#[test]
fn random_reload_keeps_pending_scene_and_dwell_with_recent_history() {
    let seq = "order='random'\navoid_repeat=2\ndwell='5s'\ntransition='cut'\n[[step]]\nscene='a'\n[[step]]\nscene='b'\n[[step]]\nscene='c'\n[[step]]\nscene='d'";
    let mut c = core(&[("mix", seq)]);
    c.submit(cmd("autoseq.play mix"));
    run(&mut c, 1);
    c.submit(cmd("autoseq.next"));
    run(&mut c, 1);
    let next = s(&c, NEXT);
    let due = int(&c, NEXT_AT);
    c.submit(Input::Config { config: Box::new(Config::build(&files(&[("mix", seq)]))) });
    run(&mut c, 1_000);
    assert_eq!(s(&c, NEXT), next);
    assert_eq!(int(&c, NEXT_AT), due);
    assert_eq!(int(&c, DWELL_MS), 5_000);
}
