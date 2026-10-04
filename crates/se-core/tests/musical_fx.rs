//! Consumer-visible musical AUTO FX behavior; no output services or player are involved.
use se_core::config::SourceFile;
use se_core::{Config, Core, Input, Output};
use se_proto::{Command, Event, Op, Origin, Value};

const MS: u64 = 1_000_000;

fn file(kind: &str, name: &str, src: &str) -> SourceFile {
    SourceFile { kind: kind.into(), name: name.into(), path: format!("{kind}/{name}.toml"), table: toml::from_str(src).unwrap() }
}

fn library(name: &str, metadata: &str) -> SourceFile {
    file("presets", name, &format!("hold = \"20s\"\nconflict = \"stack\"\nauto_fx = {{ {metadata} }}\nfx = [{{ name = \"patch.{name}\", attack = \"2s\", release = \"3s\" }}]"))
}

fn config(tuning: &str, presets: &[SourceFile]) -> Config {
    let mut files = vec![file("project", "project", &format!("schema = 1\ntick_hz = 30\nseed = 991\n[context.musical_fx]\n{tuning}"))];
    files.extend_from_slice(presets);
    Config::build(&files)
}

fn core(tuning: &str, presets: &[SourceFile]) -> Core {
    let cfg = config(tuning, presets);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    Core::new(cfg, 1_000 * MS)
}

fn command(c: &mut Core, op: Op) {
    c.submit(Input::Command { cmd: Command::new(Origin::Deck, op) });
}

fn auto(c: &mut Core, enabled: bool) {
    command(c, Op::Action { name: if enabled { "fx.auto.on" } else { "fx.auto.off" }.into(), args: Value::Null });
}

fn song(c: &mut Core, lufs: f32) {
    c.submit(Input::Signals { values: vec![
        ("music.level".into(), if lufs > -70.0 { 10f32.powf(lufs / 20.0) } else { 0.0 }),
        ("music.lufs".into(), lufs), ("music.flux".into(), 0.5),
        ("music.kick".into(), 0.2), ("music.snare".into(), 0.2), ("music.hat".into(), 0.2),
        ("music.bass".into(), 0.1), ("music.mid".into(), 0.1), ("music.high".into(), 0.1), ("music.centroid".into(), 0.5),
    ] });
}

fn run(c: &mut Core, ms: u64) -> Vec<Event> {
    run_grid(c, ms, None)
}

fn run_grid(c: &mut Core, ms: u64, grid_start: Option<f64>) -> Vec<Event> {
    let mut events = Vec::new();
    for step in 0..(ms * MS).div_ceil(c.period()) {
        if let Some(start) = grid_start {
            let position = start + step as f64 * c.period() as f64 / 1e9 * 2.0;
            c.submit(Input::Signals { values: vec![("beat.position".into(), position as f32), ("beat.bpm".into(), 120.0), ("beat.confidence".into(), 0.9)] });
        }
        c.step();
        events.extend(c.drain_outputs().into_iter().filter_map(|o| if let Output::Event(e) = o { Some(e) } else { None }));
    }
    events
}

fn musical(events: &[Event]) -> Vec<&Event> {
    events.iter().filter(|e| e.ty == "context.musical_fx").collect()
}

fn text<'a>(c: &'a Core, address: &str) -> &'a str {
    c.get(address).and_then(Value::as_str).unwrap()
}

fn on(c: &Core, address: &str) -> bool {
    c.get(address).is_some_and(Value::truthy)
}

fn first_moment(c: &mut Core) -> Event {
    auto(c, true);
    song(c, -20.0);
    let events = run(c, 15_000);
    let picked = musical(&events);
    assert_eq!(picked.len(), 1, "{events:?}");
    (*picked[0]).clone()
}

#[test]
fn musical_fx_needs_actual_song_audio_not_chat_drums_or_song_notification() {
    let mut c = core("", &[library("a", "")]);
    auto(&mut c, true);
    c.submit(Input::Signals { values: vec![("twitch.chat_rate".into(), 100.0), ("band.level".into(), 0.5), ("mic.level".into(), 0.5)] });
    c.submit(Input::Event { event: Event::new("queue.song_started", Origin::System, Value::Null) });
    c.submit(Input::Event { event: Event::new("twitch.raid", Origin::Twitch, Value::map().with("viewers", 1000i64)) });
    assert!(musical(&run(&mut c, 180_000)).is_empty());
    assert_eq!(text(&c, "context.fx.reason"), "no-song");
    song(&mut c, -20.0);
    assert!(musical(&run(&mut c, 11_000)).is_empty());
    let events = run(&mut c, 4_000);
    assert_eq!(musical(&events).len(), 1, "drum RMS is not speech over the playing song");
    song(&mut c, -90.0);
    run(&mut c, 50);
    assert_eq!(text(&c, "context.fx.reason"), "no-song");
    assert!(!on(&c, "preset.a.active"));
    run(&mut c, 3_100);
    assert!(!on(&c, "patch.a.active"));
}

#[test]
fn musical_fx_steady_song_is_intermittent_with_full_release_then_quiet_gap() {
    let presets = [library("a", ""), library("b", ""), library("c", "")];
    let mut c = core("", &presets);
    auto(&mut c, true);
    song(&mut c, -20.0);
    c.submit(Input::Publish { address: "lx.color.a".into(), value: Value::from("#aabbcc") });
    c.submit(Input::Publish { address: "lx.color.b".into(), value: Value::from("#123456") });
    c.submit(Input::Publish { address: "lx.color.e".into(), value: Value::from("#fedcba") });
    let events = run(&mut c, 600_000);
    let picks = musical(&events);
    assert!(picks.len() >= 6 && picks.len() <= 9, "steady music should have restrained occasional moments: {picks:?}");
    assert!(picks[0].ts >= 13_000 * MS);
    for pair in picks.windows(2) {
        assert!(pair[1].ts - pair[0].ts >= 68_000 * MS, "20s hold + 3s release + 45s quiet");
    }
    for triple in picks.windows(3) {
        let names: Vec<_> = triple.iter().map(|e| e.payload.get_path("preset").unwrap().as_str().unwrap()).collect();
        assert!(names[0] != names[1] && names[0] != names[2] && names[1] != names[2], "last two choices should be excluded: {names:?}");
    }
    for e in picks {
        let level = e.payload.get_path("level").and_then(Value::as_f64).unwrap();
        assert!((0.18..=0.4).contains(&level));
    }
    assert_eq!(text(&c, "lx.color.a"), "#aabbcc");
    assert_eq!(text(&c, "lx.color.b"), "#123456");
    assert_eq!(text(&c, "lx.color.e"), "#fedcba");
}

#[test]
fn musical_fx_phrase_grid_aligns_and_stalled_or_absent_grid_falls_back() {
    let mut c = core("", &[library("a", "")]);
    auto(&mut c, true);
    song(&mut c, -20.0);
    let events = run_grid(&mut c, 18_000, Some(0.0));
    let picks = musical(&events);
    assert_eq!(picks.len(), 1);
    let elapsed = (picks[0].ts - 1_000 * MS) as f64 / 1e9;
    assert!((16.0..16.15).contains(&elapsed), "next 32-beat phrase at 120 BPM: {elapsed}");

    for stalled in [false, true] {
        let mut fallback = core("", &[library("a", "")]);
        auto(&mut fallback, true);
        song(&mut fallback, -20.0);
        if stalled {
            fallback.submit(Input::Signals { values: vec![("beat.position".into(), 17.0), ("beat.bpm".into(), 120.0), ("beat.confidence".into(), 0.9)] });
        }
        let events = run(&mut fallback, 15_000);
        assert_eq!(musical(&events).len(), 1, "fallback must not wait forever for a stalled clock");
    }
}

#[test]
fn musical_fx_seek_rebases_phrase_without_catchup_or_gap_bypass() {
    let mut c = core("", &[library("a", ""), library("b", "")]);
    auto(&mut c, true);
    song(&mut c, -20.0);
    assert!(musical(&run_grid(&mut c, 14_000, Some(0.0))).is_empty());
    assert!(musical(&run_grid(&mut c, 7_000, Some(2.0))).is_empty(), "backward seek must rebase the scheduled phrase");
    let events = run_grid(&mut c, 10_000, Some(16.0));
    assert_eq!(musical(&events).len(), 1);
    c.submit(Input::Event { event: Event::new("music.drop", Origin::System, Value::map().with("strength", 1.0)) });
    let events = run_grid(&mut c, 50_000, Some(200.0));
    assert!(musical(&events).is_empty(), "forward seeks/drops never bypass the post-release quiet gap");
}

#[test]
fn musical_fx_mood_relative_energy_and_small_pool_antirepeat_are_deterministic() {
    let presets = [
        library("a", "moods = [\"groove\"], energy = [0.35, 0.65], weight = 3"),
        library("b", "moods = [\"groove\"], energy = [0.35, 0.65]"),
        library("wrong_mood", "moods = [\"heavy\"]"),
        library("wrong_energy", "energy = [0.8, 1.0]"),
    ];
    let mut sequences = Vec::new();
    for _ in 0..2 {
        let mut c = core("", &presets);
        c.submit(Input::Publish { address: "song.current.genres".into(), value: Value::List(vec![Value::from("funk")]) });
        auto(&mut c, true);
        song(&mut c, -20.0);
        let events = run(&mut c, 360_000);
        let names: Vec<_> = musical(&events).iter().map(|e| e.payload.get_path("preset").and_then(Value::as_str).unwrap().to_string()).collect();
        assert!(names.len() >= 4);
        assert!(names.iter().all(|n| n == "a" || n == "b"), "mood and relative-energy filters: {names:?}");
        assert!(names.windows(2).all(|p| p[0] != p[1]), "two-choice pool relaxes oldest, not newest: {names:?}");
        sequences.push(names);
    }
    assert_eq!(sequences[0], sequences[1]);
}

#[test]
fn musical_fx_uses_song_baseline_not_absolute_loudness_and_chat_only_modulates() {
    let presets = [library("a", "energy = [0.4, 0.6]")];
    let mut levels = Vec::new();
    for lufs in [-32.0, -9.0] {
        let mut c = core("", &presets);
        auto(&mut c, true);
        song(&mut c, lufs);
        let events = run(&mut c, 15_000);
        let picks = musical(&events);
        assert_eq!(picks.len(), 1);
        let energy = picks[0].payload.get_path("energy").and_then(Value::as_f64).unwrap();
        assert!((0.49..0.51).contains(&energy));
        levels.push(picks[0].payload.get_path("level").and_then(Value::as_f64).unwrap());
    }
    assert!((levels[0] - levels[1]).abs() < 0.001);
    let mut hot = core("", &presets);
    auto(&mut hot, true);
    song(&mut hot, -20.0);
    hot.submit(Input::Signals { values: vec![("twitch.chat_rate".into(), 100.0), ("band.level".into(), 0.2)] });
    let events = run(&mut hot, 15_000);
    let picks = musical(&events);
    let heated = picks[0].payload.get_path("level").and_then(Value::as_f64).unwrap();
    assert!(heated > levels[0] && heated - levels[0] < 0.05, "room activity modulates gently");
}

#[test]
fn musical_fx_owns_only_its_generation_and_speech_or_auto_off_fades_it() {
    for stop in ["speech", "auto-off"] {
        let mut c = core("", &[library("a", "")]);
        first_moment(&mut c);
        run(&mut c, 2_000);
        let strength = c.get("patch.a.env").and_then(Value::as_f64).unwrap();
        assert!((0.18..=0.4).contains(&strength), "musical patch amplitude must apply level");
        if stop == "speech" { c.submit(Input::Signal { name: "mic.talking".into(), value: 1.0 }); } else { auto(&mut c, false); }
        run(&mut c, 1_000);
        let fading = c.get("patch.a.env").and_then(Value::as_f64).unwrap();
        assert!(fading > 0.0 && fading < strength, "existing release fade, not a cut");
        assert_eq!(text(&c, "context.fx.reason"), if stop == "speech" { "speech" } else { "off" });
        run(&mut c, 2_200);
        assert!(!on(&c, "patch.a.active"));

        let mut shared = core("", &[library("a", "")]);
        first_moment(&mut shared);
        command(&mut shared, Op::PresetFire { name: "a".into(), payload: Value::Null });
        command(&mut shared, Op::Trigger { address: "patch.unrelated".into(), payload: Value::map().with("hold", "latch") });
        run(&mut shared, 100);
        assert_eq!(shared.active_presets().iter().filter(|(name, _)| name == "a").count(), 2);
        if stop == "speech" { shared.submit(Input::Signal { name: "mic.talking".into(), value: 1.0 }); } else { auto(&mut shared, false); }
        run(&mut shared, 4_000);
        assert_eq!(shared.active_presets().iter().filter(|(name, _)| name == "a").count(), 1, "manual SAME preset generation survives");
        assert!(on(&shared, "patch.a.active") && on(&shared, "patch.unrelated.active"));
    }
}

#[test]
fn musical_fx_global_off_keeps_existing_full_kill_and_reenable_discards_old_phrase() {
    let mut c = core("", &[library("a", "")]);
    first_moment(&mut c);
    command(&mut c, Op::PresetFire { name: "a".into(), payload: Value::Null });
    run(&mut c, 100);
    command(&mut c, Op::Action { name: "fx.off".into(), args: Value::Null });
    run(&mut c, 3_100);
    assert!(!on(&c, "patch.a.active") && !on(&c, "preset.a.active"));

    let mut pending = core("", &[library("a", "")]);
    auto(&mut pending, true);
    song(&mut pending, -20.0);
    assert!(musical(&run_grid(&mut pending, 14_000, Some(0.0))).is_empty());
    auto(&mut pending, false);
    auto(&mut pending, true);
    assert!(musical(&run_grid(&mut pending, 5_000, Some(28.0))).is_empty(), "a re-enabled director does not resurrect its old phrase opportunity");
}

#[test]
fn musical_fx_configuration_edits_deletion_and_restart_do_not_orphan_an_effect() {
    let p = library("a", "");
    let mut c = core("", &[p.clone()]);
    first_moment(&mut c);
    let saved = c.runtime_state();
    assert!(saved.presets.iter().all(|p| p.name != "a"), "ephemeral director instances must not persist as manual presets");
    let mut restarted = core("", &[p.clone()]);
    restarted.restore(&saved);
    run(&mut restarted, 100);
    assert!(on(&restarted, "fx.auto"));
    assert!(!on(&restarted, "patch.a.active") && !on(&restarted, "preset.a.active"));
    assert_eq!(text(&restarted, "context.fx.preset"), "");

    c.apply_config(config("quiet_gap = \"60s\"", &[p.clone()]));
    assert!(!on(&c, "preset.a.active"));
    assert!(musical(&run(&mut c, 4_000)).is_empty());
    assert!(!on(&c, "patch.a.active"));
    assert!(musical(&run(&mut c, 40_000)).is_empty());

    let mut deleted = core("", &[p]);
    first_moment(&mut deleted);
    deleted.apply_config(config("", &[]));
    run(&mut deleted, 3_100);
    assert!(!on(&deleted, "patch.a.active"));
    assert_eq!(text(&deleted, "context.fx.preset"), "");
    assert!(musical(&run(&mut deleted, 100_000)).is_empty());
    assert_eq!(text(&deleted, "context.fx.reason"), "no-match");
}

#[test]
fn musical_fx_no_match_and_separate_hour_ceiling_are_observable() {
    let mut unmatched = core("", &[library("heavy", "moods = [\"heavy\"]")]);
    auto(&mut unmatched, true);
    song(&mut unmatched, -20.0);
    assert!(musical(&run(&mut unmatched, 20_000)).is_empty());
    assert_eq!(text(&unmatched, "context.fx.reason"), "no-match");
    let mut capped = core("max_per_hour = 2", &[library("a", ""), library("b", "")]);
    auto(&mut capped, true);
    song(&mut capped, -20.0);
    let events = run(&mut capped, 240_000);
    let picks = musical(&events);
    assert_eq!(picks.len(), 2);
    assert_eq!(text(&capped, "context.fx.reason"), "quiet");
    assert!(capped.get("context.fx.next_at").and_then(Value::as_i64).unwrap() as u64 >= picks[0].ts + 3_600_000 * MS);
    assert_eq!(capped.signals().get("context.budget"), Some(6.0), "musical director does not spend novelty budget");
}

#[test]
fn musical_fx_configuration_rejects_unsafe_library_and_invalid_tuning_visibly() {
    for invalid in [
        "auto_fx = { moods = [\"comic\"] }\nhold = \"20s\"\nfx = [{ name = \"patch.a\" }]",
        "auto_fx = { energy = [0.8, 0.2] }\nhold = \"20s\"\nfx = [{ name = \"patch.a\" }]",
        "auto_fx = { weight = 0 }\nhold = \"20s\"\nfx = [{ name = \"patch.a\" }]",
        "auto_fx = { weight = nan }\nhold = \"20s\"\nfx = [{ name = \"patch.a\" }]",
        "auto_fx = { energy = [0.0, inf] }\nhold = \"20s\"\nfx = [{ name = \"patch.a\" }]",
        "auto_fx = {}\nhold = \"0ms\"\nfx = [{ name = \"patch.a\" }]",
        "auto_fx = {}\nhold = \"31s\"\nfx = [{ name = \"patch.a\" }]",
        "auto_fx = {}\nfx = [{ name = \"patch.a\" }]",
        "auto_fx = {}\nhold = \"20s\"\nfx = [{ name = \"patch.a\", hold = \"latch\" }]",
        "auto_fx = {}\nhold = \"20s\"\nfx = [{ name = \"patch.a\" }]\nlights = { look = \"hype\" }",
        "auto_fx = {}\nhold = \"20s\"\nfx = [{ name = \"patch.a\" }]\nset = { \"palette.accent\" = \"#ffffff\" }",
        "auto_fx = {}\nhold = \"20s\"\nfx = [{ name = \"patch.a\" }]\nsound = \"horn\"",
    ] {
        assert!(!config("", &[file("presets", "bad", invalid)]).errors.is_empty(), "{invalid}");
    }
    for tuning in ["quiet_gap = \"44s\"", "start_delay = \"1s\"", "phrase_beats = 31", "max_per_hour = 49", "surprise = true"] {
        let cfg = config(tuning, &[library("a", "")]);
        assert!(!cfg.errors.is_empty(), "{tuning}");
        let mut c = Core::new(cfg, 1_000 * MS);
        auto(&mut c, true);
        song(&mut c, -20.0);
        assert!(musical(&run(&mut c, 15_000)).is_empty());
        assert_eq!(text(&c, "context.fx.reason"), "error");
        assert!(!text(&c, "health.context.fx").is_empty());
    }
    let short = file("presets", "accent", "auto_fx = {}\nhold = \"2s\"\nfx = [{ name = \"patch.accent\", hold = \"1s\" }]");
    assert!(config("", &[short]).errors.is_empty(), "short authored accents remain valid; the quiet gap controls density");
}

#[test]
fn musical_fx_disabled_constituents_are_excluded_before_weighted_selection() {
    let presets = [
        library("a", "weight = 1000"),
        library("b", ""),
        file("presets", "composite", "hold = \"20s\"\nauto_fx = { weight = 1000 }\nfx = [{ name = \"patch.a\" }, { name = \"glitch\" }]"),
    ];
    let mut c = core("", &presets);
    command(&mut c, Op::SetBase { address: "patch.a.auto.enabled".into(), value: Value::Bool(false) });
    auto(&mut c, true);
    song(&mut c, -20.0);
    let events = run(&mut c, 240_000);
    let picks = musical(&events);
    assert!(picks.len() >= 3, "ineligible high-weight presets must not starve the eligible candidate");
    assert!(picks.iter().all(|e| e.payload.get_path("preset").and_then(Value::as_str) == Some("b")));
    assert!(events.iter().all(|e| e.ty != "patch.a.trigger" && e.ty != "fx.glitch.trigger"));
}

#[test]
fn musical_fx_cooldown_survives_selection_retries_and_hot_reload() {
    let presets = [library("a", "")];
    let mut c = core("", &presets);
    command(&mut c, Op::SetBase { address: "patch.a.auto.interval".into(), value: Value::Float(240.0) });
    let first = first_moment(&mut c);
    run(&mut c, 30_000);
    c.apply_config(config("", &presets));
    assert!(musical(&run(&mut c, 180_000)).is_empty(), "reload/repeated selection must not reset last successful auto start");
    let events = run(&mut c, 100_000);
    let next = musical(&events);
    assert_eq!(next.len(), 1, "effect becomes eligible again without operator intervention");
    assert!(next[0].ts - first.ts >= 240_000 * MS);
}
