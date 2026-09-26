//! Timelines end to end through the core (§2.7, M9 acceptance): media position following,
//! MTC chase with tracked state across locates, internal transport, regions, record mode,
//! and replay determinism.

use se_clock::timecode::mtc::{MtcDecoder, MtcGenerator};
use se_clock::timecode::{FrameRate, ObsThrottle, TcObs};
use se_core::config::SourceFile;
use se_core::{Config, Core, Input, Output};
use se_proto::{Command, Op, Origin, Ts, Value};

const MS: u64 = 1_000_000;
const T0: Ts = 1_000 * MS;

fn file(kind: &str, name: &str, src: &str) -> SourceFile {
    SourceFile { kind: kind.into(), name: name.into(), path: format!("{kind}/{name}.toml"), table: toml::from_str(src).unwrap() }
}

fn core(extra: Vec<SourceFile>) -> Core {
    let mut files = vec![
        file("project", "project", "schema = 1\nname = \"tl\""),
        file("presets", "chorus_blast", "hold = \"4s\"\nfx = [{ name = \"rgb_split\" }]\nset = { \"fx.rgb_split.amount\" = 0.9 }"),
        file("presets", "wash", "set = { \"fx.grade.amount\" = 0.7 }"),
    ];
    files.extend(extra);
    let c = Config::build(&files);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    let core = Core::new(c, T0);
    assert!(core.config.errors.is_empty(), "{:?}", core.config.errors);
    core
}

fn cmd(text: &str) -> Input {
    Input::Command { cmd: Command::new(Origin::Ui, Op::parse(text).unwrap()) }
}

fn f(c: &Core, a: &str) -> f64 {
    c.get(a).and_then(Value::as_f64).unwrap_or(f64::NAN)
}

fn actions(out: &[Output], prefix: &str) -> Vec<(String, Value)> {
    out.iter()
        .filter_map(|o| match o {
            Output::Action(Command { op: Op::Action { name, args }, .. }) if name.starts_with(prefix) => Some((name.clone(), args.clone())),
            _ => None,
        })
        .collect()
}

fn events<'a>(out: &'a [Output], ty: &str) -> Vec<&'a se_proto::Event> {
    out.iter().filter_map(|o| if let Output::Event(e) = o { (e.ty == ty).then_some(e) } else { None }).collect()
}

// ---- media: the nightly-song chorus cue ----------------------------------------------------

const NIGHTLY: &str = "media = \"yt:VIDEO_ID\"\ncues = [\n  { at = \"1:12.400\", do = [\"preset.fire chorus_blast\"], label = \"chorus\" },\n  { at = \"1:43.000\", do = [\"lights.cue blackout\"] },\n]";

/// Drive `song.position` like the Songs player page: 30 Hz, extrapolated, with report jitter.
fn play_song(c: &mut Core, from: f64, seconds: f64, out: &mut Vec<Output>) -> Vec<(f64, Ts)> {
    let mut log = Vec::new();
    let start_tick = c.tick_index();
    let ticks = (seconds * 1e9 / c.period() as f64) as u64;
    for k in 0..ticks {
        let t = k as f64 * c.period() as f64 / 1e9;
        // a report every ~33 ms, ±15 ms of extrapolation error
        if k % 8 == 0 {
            let jitter = (((k / 8) * 7919) % 31) as f64 / 1000.0 - 0.015;
            c.submit(Input::Signal { name: "song.position".into(), value: (from + t + jitter) as f32 });
        }
        c.step();
        let o = c.drain_outputs();
        for e in events(&o, "timeline.cue") {
            log.push((e.payload.get_path("at").and_then(Value::as_f64).unwrap(), c.now()));
        }
        out.extend(o);
        let _ = start_tick;
    }
    log
}

#[test]
fn chorus_cue_fires_on_time_from_youtube_position() {
    let mut c = core(vec![file("timelines", "nightly_song", NIGHTLY)]);
    c.submit(Input::Publish { address: "song.media".into(), value: "yt:VIDEO_ID".into() });
    c.submit(Input::Publish { address: "song.state".into(), value: "playing".into() });
    c.step();
    let song_start = c.now();
    let mut out = Vec::new();
    let fired = play_song(&mut c, 0.0, 110.0, &mut out);
    assert_eq!(fired.len(), 2, "{fired:?}");
    for (at, ts) in &fired {
        let song_time = (*ts - song_start) as f64 / 1e9;
        assert!((song_time - at).abs() < 0.03, "cue at {at} fired at song time {song_time}");
    }
    let chorus = events(&out, "timeline.cue")[0];
    assert_eq!(chorus.payload.get_path("label"), Some(&Value::Str("chorus".into())));
    assert_eq!(events(&out, "preset.chorus_blast.fired").len(), 1);
    assert_eq!(
        actions(&out, "lights.cue"),
        vec![("lights.cue".to_string(), Value::map().with("args", vec![Value::Str("blackout".into())]).with("priority", 200))],
        "cue-list playbacks run at the timeline's priority"
    );
    assert_eq!(c.get("timeline.nightly_song.source"), Some(&Value::Str("media:yt:VIDEO_ID".into())));
    assert_eq!(c.get("timeline.nightly_song.locked"), Some(&Value::Bool(true)));
    assert!((f(&c, "timeline.nightly_song.time") - 110.0).abs() < 0.05);
}

#[test]
fn media_timeline_waits_for_its_video_and_holds_on_pause() {
    let mut c = core(vec![file("timelines", "nightly_song", NIGHTLY)]);
    c.submit(Input::Publish { address: "song.media".into(), value: "yt:OTHER".into() });
    c.submit(Input::Publish { address: "song.state".into(), value: "playing".into() });
    let mut out = Vec::new();
    play_song(&mut c, 70.0, 5.0, &mut out);
    assert!(events(&out, "timeline.cue").is_empty(), "another video is playing");
    assert_eq!(c.get("timeline.nightly_song.status"), Some(&Value::Str("waiting".into())));
    // our video starts mid-song (seek): tracked state, then the chorus fires when crossed
    c.submit(Input::Publish { address: "song.media".into(), value: "yt:VIDEO_ID".into() });
    let mut out = Vec::new();
    play_song(&mut c, 71.0, 2.0, &mut out);
    assert_eq!(events(&out, "timeline.cue").len(), 1);
    // pause: position freezes, state stays
    c.submit(Input::Publish { address: "song.state".into(), value: "paused".into() });
    for _ in 0..240 {
        c.step();
    }
    let held = f(&c, "timeline.nightly_song.time");
    for _ in 0..240 {
        c.step();
    }
    assert_eq!(f(&c, "timeline.nightly_song.time"), held, "paused");
    assert_eq!(c.get("timeline.nightly_song.active"), Some(&Value::Bool(true)));
    assert_eq!(c.get("timeline.nightly_song.playing"), Some(&Value::Bool(false)));
    // the song changes: the timeline releases
    // (published state is read as resolved at the next tick)
    c.submit(Input::Publish { address: "song.media".into(), value: "yt:NEXT".into() });
    c.drain_outputs();
    c.step();
    c.step();
    let o = c.drain_outputs();
    assert_eq!(events(&o, "timeline.stopped").len(), 1);
    assert_eq!(c.get("timeline.nightly_song.active"), Some(&Value::Bool(false)));
}

// ---- MTC: cue list + automation lane, tracked state across locates -------------------------

const MTC_LIGHTS: &str = r#"
source = "mtc"
fps = "25"
priority = 250

[[track]]
name = "main"
cues = [
  { at = "00:00:10:00", do = ["lights.cue cuelist=main cue=1"] },
  { at = "00:00:15:00", do = ["set fx.haze.amount 0.5", "emit show.flash"] },
  { at = "00:00:20:00", do = ["lights.cue cuelist=main cue=2"] },
  { at = "00:00:30:00", do = ["lights.cue cuelist=main cue=3"] },
]

[[track]]
name = "front"
type = "automation"
address = "lights.group.front.master"
keys = [
  { at = "00:00:00:00", value = 0.0 },
  { at = "00:00:10:00", value = 1.0 },
  { at = "00:00:40:00", value = 0.25 },
]
"#;

/// MTC transport feeding the core like the I/O glue: generator → bytes → decoder → throttle →
/// `Input::Timecode`. Returns everything the core output.
struct MtcSim {
    g: MtcGenerator,
    dec: MtcDecoder,
    thr: ObsThrottle,
    pos: f64,
}

impl MtcSim {
    fn new(start: f64) -> Self {
        MtcSim { g: MtcGenerator::new(FrameRate::Fps25), dec: MtcDecoder::new(), thr: ObsThrottle::new(60 * MS, 0.1), pos: start }
    }
    fn run(&mut self, c: &mut Core, seconds: f64, running: bool, out: &mut Vec<Output>) {
        let ticks = (seconds * 1e9 / c.period() as f64).round() as u64;
        for _ in 0..ticks {
            // MIDI arrives during the tick interval: stamp with the core's upcoming tick time
            let ts = c.now() + c.period();
            let dt = c.period() as f64 / 1e9;
            let mut obs: Vec<TcObs> = Vec::new();
            let (g, dec) = (&mut self.g, &mut self.dec);
            g.poll(self.pos, running, |m| dec.feed(m, ts, |u| obs.push(u.obs())));
            for o in obs {
                if self.thr.pass(&o) {
                    c.submit(Input::Timecode { source: "mtc".into(), obs: o });
                }
            }
            if running {
                self.pos += dt;
            }
            c.step();
            out.extend(c.drain_outputs());
        }
    }
    fn locate(&mut self, to: f64) {
        self.pos = to;
    }
}

fn lights(out: &[Output]) -> Vec<String> {
    actions(out, "lights.")
        .into_iter()
        .map(|(n, a)| {
            format!("{n} {} {}", a.get_path("cuelist").map(|v| v.to_string()).unwrap_or_default(), a.get_path("cue").map(|v| v.to_string()).unwrap_or_default())
        })
        .collect()
}

#[test]
fn cue_list_and_lane_follow_mtc_and_survive_locate_jumps() {
    let mut c = core(vec![file("timelines", "mtc_lights", MTC_LIGHTS)]);
    let mut mtc = MtcSim::new(5.0);
    let mut out = Vec::new();
    mtc.run(&mut c, 17.0, true, &mut out);
    // 5 s → 22 s: cues at 10, 15, 20 fire in order, on time
    assert_eq!(lights(&out), vec!["lights.cue main 1", "lights.cue main 2"]);
    let cues = events(&out, "timeline.cue");
    assert_eq!(cues.len(), 3);
    assert_eq!(events(&out, "show.flash").len(), 1);
    assert_eq!(f(&c, "fx.haze.amount"), 0.5);
    assert_eq!(c.get("timeline.mtc_lights.locked"), Some(&Value::Bool(true)));
    let t = f(&c, "timeline.mtc_lights.time");
    assert!((t - 22.0).abs() < 0.05, "{t}");
    // lane: 1.0 → 0.25 over 10..40 s
    let lane = |t: f64| if t <= 10.0 { t / 10.0 } else { 1.0 - 0.75 * (t - 10.0) / 30.0 };
    assert!((f(&c, "lights.group.front.master") - lane(t)).abs() < 0.005);
    assert_eq!(c.explain("lights.group.front.master").unwrap().layers.iter().filter(|l| l.source.contains("timeline:mtc_lights")).count(), 1);

    // locate forward past cue 3 (full-frame message, then running again): tracked state, no skipped cues
    mtc.run(&mut c, 0.1, false, &mut Vec::new());
    mtc.locate(35.0);
    let mut out = Vec::new();
    mtc.run(&mut c, 2.0, true, &mut out);
    assert_eq!(lights(&out), vec!["lights.locate main 3"], "latest cue of the list, recalled instantly");
    assert!(events(&out, "timeline.cue").is_empty(), "skipped cues do not fire");
    assert!(events(&out, "show.flash").is_empty());
    assert_eq!(events(&out, "timeline.jump").len(), 1);
    assert_eq!(f(&c, "fx.haze.amount"), 0.5, "tracked set survives");
    let t = f(&c, "timeline.mtc_lights.time");
    assert!((f(&c, "lights.group.front.master") - lane(t)).abs() < 0.005, "lane follows the new time");

    // locate back before the haze cue: the set is reverted, cue list 1 recalled
    mtc.locate(12.0);
    let mut out = Vec::new();
    mtc.run(&mut c, 1.0, true, &mut out);
    assert_eq!(lights(&out), vec!["lights.locate main 1"]);
    assert!(f(&c, "fx.haze.amount").is_nan() || f(&c, "fx.haze.amount") == 0.0, "haze reverted: {}", f(&c, "fx.haze.amount"));
    let t = f(&c, "timeline.mtc_lights.time");
    assert!((t - 13.0).abs() < 0.05, "{t}");
    assert!((f(&c, "lights.group.front.master") - lane(t)).abs() < 0.005);

    // and plays on normally from there
    let mut out = Vec::new();
    mtc.run(&mut c, 3.0, true, &mut out);
    assert_eq!(events(&out, "show.flash").len(), 1, "cue at 15 s fires again after the backward locate");

    // locate before the first cue: the cue list is released
    mtc.locate(2.0);
    let mut out = Vec::new();
    mtc.run(&mut c, 1.0, true, &mut out);
    assert_eq!(lights(&out), vec!["lights.release main "]);
}

#[test]
fn mtc_dropout_freewheels_then_holds() {
    let mut c = core(vec![file("timelines", "mtc_lights", MTC_LIGHTS)]);
    let mut mtc = MtcSim::new(8.0);
    let mut out = Vec::new();
    mtc.run(&mut c, 1.5, true, &mut out);
    assert!(lights(&out).is_empty());
    // MTC cable pulled at 9.5 s: the timeline keeps running (freewheel 2 s) and fires cue 1
    let mut out = Vec::new();
    for _ in 0..(1000 * MS / c.period()) {
        c.step();
        out.extend(c.drain_outputs());
    }
    assert_eq!(c.get("timeline.mtc_lights.status"), Some(&Value::Str("freewheel".into())));
    assert_eq!(lights(&out), vec!["lights.cue main 1"], "freewheel fires cue 1 at 10 s");
    for _ in 0..(1500 * MS / c.period()) {
        c.step();
    }
    assert_eq!(c.get("timeline.mtc_lights.status"), Some(&Value::Str("lost".into())));
    let held = f(&c, "timeline.mtc_lights.time");
    assert!((held - 11.5).abs() < 0.1, "held at the end of freewheel: {held}");
    assert_eq!(c.get("timeline.mtc_lights.active"), Some(&Value::Bool(true)), "state held after loss");
    assert!(f(&c, "lights.group.front.master") > 0.9);
}

// ---- internal transport, regions, record mode ----------------------------------------------

const SHOW: &str = r#"
length = "20s"
[[track]]
name = "hits"
cues = [{ at = "2s", do = ["emit show.hit n=1"] }, { at = "6s", do = ["emit show.hit n=2"] }]
[[track]]
name = "wash"
regions = [{ start = "4s", end = "8s", preset = "wash", label = "verse" }, { start = "10s", end = "12s", fx = "strobe" }]
[[track]]
name = "level"
address = "fx.level.amount"
curve = "smoothstep"
keys = [{ at = "0s", value = 0.0 }, { at = "10s", value = 1.0 }]
"#;

fn run(c: &mut Core, ms: u64) -> Vec<Output> {
    let mut out = Vec::new();
    for _ in 0..(ms * MS).div_ceil(c.period()) {
        c.step();
        out.extend(c.drain_outputs());
    }
    out
}

#[test]
fn internal_transport_regions_and_locate() {
    let mut c = core(vec![file("timelines", "show", SHOW)]);
    run(&mut c, 100);
    assert_eq!(c.get("timeline.show.status"), Some(&Value::Str("stopped".into())));
    c.submit(cmd("timeline.play show"));
    let out = run(&mut c, 5000);
    assert_eq!(events(&out, "show.hit").len(), 1);
    assert_eq!(c.get("preset.wash.active"), Some(&Value::Bool(true)), "region entered at 4 s");
    let out = run(&mut c, 4000);
    assert_eq!(events(&out, "show.hit").len(), 1);
    assert_eq!(c.get("preset.wash.active"), Some(&Value::Bool(false)), "region left at 8 s");
    assert!((f(&c, "fx.level.amount") - 0.972).abs() < 0.01, "smoothstep at 9 s");
    // strobe region 10–12 s holds the trigger for the whole span
    run(&mut c, 1500);
    assert_eq!(c.get("fx.strobe.active"), Some(&Value::Bool(true)));
    run(&mut c, 1400);
    assert_eq!(c.get("fx.strobe.active"), Some(&Value::Bool(true)), "held until the region ends at 12 s");
    run(&mut c, 700);
    assert_eq!(c.get("fx.strobe.active"), Some(&Value::Bool(false)), "released at 12 s (+500 ms release)");
    // pause holds, locate into a region applies it, play resumes
    c.submit(cmd("timeline.pause show"));
    run(&mut c, 500);
    let t = f(&c, "timeline.show.time");
    run(&mut c, 500);
    assert_eq!(f(&c, "timeline.show.time"), t);
    c.submit(cmd("timeline.locate show 5s"));
    let out = run(&mut c, 50);
    assert!(events(&out, "show.hit").is_empty(), "locate past a cue does not fire it");
    assert_eq!(c.get("preset.wash.active"), Some(&Value::Bool(true)));
    assert_eq!(f(&c, "timeline.show.time"), 5.0);
    c.submit(cmd("timeline.play show"));
    let out = run(&mut c, 1500);
    assert_eq!(events(&out, "show.hit").len(), 1, "cue at 6 s");
    // ends at length and holds
    let out = run(&mut c, 15_000);
    assert_eq!(events(&out, "timeline.ended").len(), 1);
    assert_eq!(f(&c, "timeline.show.time"), 20.0);
    // stop releases everything and rewinds
    c.submit(cmd("timeline.stop show"));
    run(&mut c, 50);
    assert_eq!(c.get("timeline.show.active"), Some(&Value::Bool(false)));
    assert!(c.explain("fx.level.amount").unwrap().layers.iter().all(|l| !l.source.contains("timeline")));
    assert_eq!(f(&c, "timeline.show.time"), 0.0);
    assert!(c.submit_ok_error("timeline.locate nope 1s").contains("unknown timeline"));
}

/// Small helper trait: run one command and return its ack error.
trait AckErr {
    fn submit_ok_error(&mut self, text: &str) -> String;
}

impl AckErr for Core {
    fn submit_ok_error(&mut self, text: &str) -> String {
        let c = Command::new(Origin::Ui, Op::parse(text).unwrap());
        let id = c.id;
        self.submit(Input::Command { cmd: c });
        self.step();
        self.drain_outputs()
            .into_iter()
            .find_map(|o| match o {
                Output::Ack { id: i, error, .. } if i == id => Some(error.unwrap_or_default()),
                _ => None,
            })
            .unwrap_or_default()
    }
}

#[test]
fn looping_timeline_refires_cues_each_pass() {
    let mut c = core(vec![file("timelines", "loop", "length = \"1s\"\nloop = true\ncues = [{ at = \"0.5s\", do = [\"emit tick\"] }]")]);
    c.submit(cmd("timeline.play loop"));
    let out = run(&mut c, 3200);
    assert_eq!(events(&out, "tick").len(), 3);
    assert_eq!(events(&out, "timeline.looped").len(), 3);
}

#[test]
fn record_mode_turns_taps_and_moves_into_cues_and_keys() {
    let mut c = core(vec![file("timelines", "rec", "length = \"30s\"\n[[track]]\nname = \"fader\"\naddress = \"mixer.16r.ch.3.fader\"\nkeys = []")]);
    c.submit(cmd("timeline.play rec"));
    run(&mut c, 1000);
    c.submit(cmd("timeline.record rec"));
    run(&mut c, 500);
    c.submit(cmd("preset.fire wash"));
    run(&mut c, 500);
    c.submit(cmd("timeline.tap rec label=chorus"));
    // a fader move: ramp 0 → 1 over 1 s, then hold
    for i in 0..=10 {
        c.submit(cmd(&format!("set mixer.16r.ch.3.fader {}", i as f64 / 10.0)));
        run(&mut c, 100);
    }
    run(&mut c, 500);
    assert_eq!(c.get("timeline.rec.recording"), Some(&Value::Bool(true)));
    c.submit(cmd("timeline.record rec"));
    let out = run(&mut c, 20);
    let commit = actions(&out, "timeline.record.commit");
    assert_eq!(commit.len(), 1);
    let a = &commit[0].1;
    let cues = a.get_path("cues").and_then(Value::as_list).unwrap();
    assert_eq!(cues.len(), 2, "{cues:?}");
    assert_eq!(cues[0].get_path("do.0"), Some(&Value::Str("preset.fire wash".into())));
    assert!((cues[0].get_path("at").unwrap().as_f64().unwrap() - 1.5).abs() < 0.01);
    assert_eq!(cues[1].get_path("label"), Some(&Value::Str("chorus".into())));
    let lanes = a.get_path("lanes").and_then(Value::as_list).unwrap();
    assert_eq!(lanes[0].get_path("address"), Some(&Value::Str("mixer.16r.ch.3.fader".into())));
    let keys = lanes[0].get_path("keys").and_then(Value::as_list).unwrap();
    assert!(keys.len() <= 4 && keys.len() >= 2, "a linear ramp thins to its ends: {keys:?}");
    assert_eq!(keys.last().unwrap().get_path("value").and_then(Value::as_f64), Some(1.0));
    assert_eq!(a.get_path("track"), Some(&Value::Str("recorded".into())));
    assert_eq!(events(&out, "timeline.recorded").len(), 1);
}

// ---- replay determinism --------------------------------------------------------------------

#[test]
fn replay_reproduces_timeline_state_and_actions() {
    let files = || vec![file("timelines", "nightly_song", NIGHTLY), file("timelines", "mtc_lights", MTC_LIGHTS), file("timelines", "show", SHOW)];
    let mut live = core(files());
    let mut log: Vec<(u64, Input)> = Vec::new();
    let mut live_actions = Vec::new();
    live.submit(Input::Publish { address: "song.media".into(), value: "yt:VIDEO_ID".into() });
    live.submit(Input::Publish { address: "song.state".into(), value: "playing".into() });
    live.submit(cmd("timeline.play show"));
    let mut out = Vec::new();
    play_song(&mut live, 68.0, 6.0, &mut out);
    log.extend(live.drain_applied());
    let mut mtc = MtcSim::new(8.0);
    mtc.run(&mut live, 14.0, true, &mut out);
    log.extend(live.drain_applied());
    mtc.locate(33.0);
    mtc.run(&mut live, 2.0, true, &mut out);
    log.extend(live.drain_applied());
    live_actions.extend(lights(&out));
    let target = live.tick_index();
    assert!(log.iter().any(|(_, i)| matches!(i, Input::Timecode { source, .. } if source == "media")), "song.position recorded as observations");
    assert!(!log.iter().any(|(_, i)| matches!(i, Input::Signal { .. })));

    let mut rep = core(files());
    let mut rep_out = Vec::new();
    let mut i = 0;
    while rep.tick_index() < target {
        while i < log.len() && log[i].0 <= rep.tick_index() + 1 {
            rep.submit(log[i].1.clone());
            i += 1;
        }
        rep.step();
        rep_out.extend(rep.drain_outputs());
    }
    assert_eq!(lights(&rep_out), live_actions);
    for a in [
        "timeline.nightly_song.time",
        "timeline.mtc_lights.time",
        "timeline.show.time",
        "lights.group.front.master",
        "fx.haze.amount",
        "fx.level.amount",
        "fx.rgb_split.amount",
    ] {
        assert_eq!(rep.get(a), live.get(a), "{a}");
    }
    for a in ["timeline.nightly_song.status", "timeline.mtc_lights.status", "preset.chorus_blast.active", "preset.wash.active"] {
        assert_eq!(rep.get(a), live.get(a), "{a}");
    }
}

#[test]
fn broken_timeline_keeps_last_good_and_reports() {
    let mut c = core(vec![file("timelines", "show", SHOW)]);
    let bad = Config::build(&[file("project", "project", "schema = 1"), file("timelines", "show", "source = \"bogus\"")]);
    c.submit(Input::Config { config: Box::new(bad) });
    run(&mut c, 10);
    assert!(c.config.errors.iter().any(|e| e.file == "timelines/show.toml" && e.msg.contains("unknown source")));
    c.submit(cmd("timeline.play show"));
    run(&mut c, 2500);
    assert_eq!(c.get("timeline.show.status"), Some(&Value::Str("playing".into())), "last good definition still runs");
    let q = c.query("timelines", &Value::Null).unwrap();
    let list = q.as_list().unwrap();
    assert_eq!(list[0].get_path("name"), Some(&Value::Str("show".into())));
    assert_eq!(list[0].get_path("tracks.0.cues.0.at"), Some(&Value::Float(2.0)));
}

#[test]
fn panic_stops_timelines() {
    let mut c = core(vec![file("timelines", "show", SHOW)]);
    c.submit(cmd("timeline.play show"));
    run(&mut c, 5000);
    c.submit(Input::Command { cmd: Command::new(Origin::Ui, Op::Panic) });
    run(&mut c, 50);
    assert_eq!(c.get("timeline.show.active"), Some(&Value::Bool(false)));
    let v = f(&c, "fx.level.amount");
    run(&mut c, 1000);
    assert_eq!(f(&c, "fx.level.amount"), v, "automation no longer writes");
}

#[test]
fn manual_scrub_and_jog() {
    let src = "source = \"manual\"\nlength = \"10s\"\nscrub = \"midi.xtouch.fader.9\"\ncues = [{ at = \"5.1s\", do = [\"emit hit\"] }, { at = \"2s\", do = [\"set fx.x.amount 0.4\"] }]";
    let mut c = core(vec![file("timelines", "jog", src)]);
    c.submit(Input::Signal { name: "midi.xtouch.fader.9".into(), value: 0.5 });
    let out = run(&mut c, 20);
    assert_eq!(f(&c, "timeline.jog.time"), 5.0);
    assert!(events(&out, "hit").is_empty(), "a scrub jump applies tracked state only");
    assert_eq!(f(&c, "fx.x.amount"), 0.4);
    c.submit(Input::Signal { name: "midi.xtouch.fader.9".into(), value: 0.52 });
    let out = run(&mut c, 20);
    assert_eq!(events(&out, "hit").len(), 1, "small forward scrub crosses the cue");
    let log = c.drain_applied();
    assert_eq!(
        log.iter().filter(|(_, i)| matches!(i, Input::Timecode { source, .. } if source == "scrub:jog")).count(),
        2,
        "scrub positions recorded for replay"
    );
    // jog back 4 s: before the set → reverted
    c.submit(cmd("timeline.jog jog delta=-4s"));
    run(&mut c, 20);
    assert!((f(&c, "timeline.jog.time") - 1.2).abs() < 1e-6);
    assert!(c.get("fx.x.amount").and_then(Value::as_f64).is_none_or(|v| v == 0.0));
    c.submit(cmd("timeline.jog jog 1"));
    run(&mut c, 20);
    assert!((f(&c, "timeline.jog.time") - 2.2).abs() < 1e-6);
    assert_eq!(f(&c, "fx.x.amount"), 0.4, "jog forward across the set fires it");
}
