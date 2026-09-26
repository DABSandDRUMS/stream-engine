//! Replays recorded/protocol fixture streams through the same decoding used at runtime, and
//! checks the example project's controllers files.

use se_input::config::{self, Action, Parsed};
use se_input::deck::proto;
use se_input::midi::controls::{Decoder, Ev};
use se_input::midi::parse::Parser;
use se_input::midi::{decoder_for, read_fixture};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
}

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../project-example/controllers").join(name)
}

fn device(file: &str) -> config::MidiDeviceCfg {
    let t: toml::Table = std::fs::read_to_string(example(file)).unwrap().parse().unwrap();
    let stem = file.trim_end_matches(".toml");
    match config::parse_file(stem, &format!("controllers/{file}"), &t).unwrap() {
        Parsed::Midi(m) => *m,
        _ => panic!("{file} is not a MIDI device"),
    }
}

/// Replay a fixture: `(control name, event)` in order.
fn replay(dec: &mut Decoder, name: &str) -> Vec<(String, Ev)> {
    let mut p = Parser::default();
    let mut out = Vec::new();
    for (_, bytes) in read_fixture(&fixture(name)) {
        let mut evs = Vec::new();
        p.feed(&bytes, |m| dec.decode(&m, &mut evs));
        for e in evs {
            let idx = match &e {
                Ev::Value { idx, .. } | Ev::Delta { idx, .. } | Ev::Press { idx, .. } | Ev::Release { idx } | Ev::Touch { idx, .. } => *idx,
                Ev::Program { .. } => usize::MAX,
            };
            let n = dec.defs.get(idx).map(|d| d.name.clone()).unwrap_or_default();
            out.push((n, e));
        }
    }
    out
}

#[test]
fn xtouch_mini_mc_stream_names_every_control() {
    let cfg = device("xtouch.toml");
    let mut dec = decoder_for(&cfg);
    let evs = replay(&mut dec, "xtouch_mini_mc.txt");
    let deltas: Vec<i32> =
        evs.iter().filter(|(n, _)| n == "enc.1").filter_map(|(_, e)| if let Ev::Delta { steps, .. } = e { Some(*steps) } else { None }).collect();
    assert_eq!(deltas, vec![1, 1, 1, -1, 4]);
    let presses: Vec<&str> = evs.iter().filter(|(_, e)| matches!(e, Ev::Press { .. })).map(|(n, _)| n.as_str()).collect();
    assert_eq!(presses, vec!["push.1", "btn.1", "btn.16", "layer.b"]);
    assert_eq!(evs.iter().filter(|(_, e)| matches!(e, Ev::Release { .. })).count(), 4);
    let fader: Vec<f32> =
        evs.iter().filter(|(n, _)| n == "fader").filter_map(|(_, e)| if let Ev::Value { value, .. } = e { Some(*value) } else { None }).collect();
    assert_eq!(fader.len(), 4);
    assert_eq!(fader[0], 0.0);
    assert!((fader[2] - 8192.0 / 16383.0).abs() < 1e-6);
    assert_eq!(fader[3], 1.0);
    // no auto-created controls: the profile covers the whole surface
    assert!(dec.defs.iter().all(|d| !d.auto));
    // the example maps: btn.1 fires hype, enc.1 adjusts the RGB split, the fader is a pickup binding
    assert!(cfg.maps.iter().any(|m| m.control == "btn.1" && m.action == Action::Preset("hype".into())));
    assert!(cfg.maps.iter().any(|m| m.control == "enc.1" && m.target.as_deref() == Some("fx.rgb_split.amount")));
    assert_eq!(cfg.init, vec![vec![0xB0, 0x7F, 0x01]]);
}

#[test]
fn fbv_footswitch_fires_once_and_pedal_is_continuous() {
    let cfg = device("fbv.toml");
    let mut dec = decoder_for(&cfg);
    let evs = replay(&mut dec, "fbv_express.txt");
    let fs: Vec<&Ev> = evs.iter().filter(|(n, _)| n == "fs_a").map(|(_, e)| e).collect();
    assert!(matches!(fs.as_slice(), [Ev::Press { .. }, Ev::Release { .. }]), "{fs:?}");
    let pedal: Vec<f32> =
        evs.iter().filter(|(n, _)| n == "pedal").filter_map(|(_, e)| if let Ev::Value { value, .. } = e { Some(*value) } else { None }).collect();
    assert_eq!(pedal.first().copied(), Some(0.0));
    assert_eq!(pedal.last().copied(), Some(1.0));
    assert!(cfg.maps.iter().any(|m| m.control == "fs_a" && m.action == Action::Preset("hype".into())));
}

#[test]
fn edrum_notes_map_to_pads_and_mtc_is_not_a_control() {
    let cfg = device("studio24c.toml");
    assert_eq!(cfg.drums.get(&36).map(String::as_str), Some("kick"));
    assert_eq!(cfg.drums.get(&38).map(String::as_str), Some("snare"));
    assert_eq!(cfg.drums.get(&42).map(String::as_str), Some("hat_closed"));
    assert_eq!(cfg.drum_channel, Some(9));
    let mut dec = decoder_for(&cfg);
    let evs = replay(&mut dec, "studio24c_edrum.txt");
    let presses: Vec<(String, f32)> =
        evs.iter().filter_map(|(n, e)| if let Ev::Press { velocity, .. } = e { Some((n.clone(), *velocity)) } else { None }).collect();
    assert_eq!(presses.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), vec!["ch10.note.36", "ch10.note.38", "ch10.note.42"]);
    assert!((presses[0].1 - 100.0 / 127.0).abs() < 1e-6);
    // quarter frames and the MTC full frame are raw-only (Timelines), never controls
    assert!(dec.defs.iter().all(|d| d.name.contains("note")));
    // the raw stream keeps them intact
    let mut p = Parser::default();
    let mut raw = Vec::new();
    for (_, b) in read_fixture(&fixture("studio24c_edrum.txt")) {
        p.feed(&b, |m| raw.push(m.to_bytes()));
    }
    assert!(raw.contains(&vec![0xF1, 0x23]));
    assert!(raw.contains(&vec![0xF0, 0x7F, 0x7F, 0x01, 0x01, 0x21, 0x02, 0x03, 0x04, 0xF7]));
}

#[test]
fn hires_cc_and_nrpn_stream() {
    let t: toml::Table = "kind = \"midi\"\nmatch = \"x\"\n[control.volume]\ncc = 7\nhires = true\n".parse().unwrap();
    let Parsed::Midi(cfg) = config::parse_file("x", "controllers/x.toml", &t).unwrap() else { panic!() };
    let mut dec = decoder_for(&cfg);
    let evs = replay(&mut dec, "generic_hires.txt");
    let vol: Vec<u16> = evs.iter().filter(|(n, _)| n == "volume").filter_map(|(_, e)| if let Ev::Value { raw, .. } = e { Some(*raw) } else { None }).collect();
    assert_eq!(vol, vec![0x40 << 7, (0x40 << 7) | 0x7F, 0x50 << 7, 0x50 << 7]);
    let nrpn: Vec<&Ev> = evs.iter().filter(|(n, _)| n == "nrpn.130").map(|(_, e)| e).collect();
    assert!(matches!(nrpn.as_slice(), [Ev::Value { raw: 8192, .. }, Ev::Value { raw: 8193, .. }, Ev::Delta { steps: 1, .. }]), "{nrpn:?}");
}

#[test]
fn streamdeck_reports_give_key_edges() {
    let mut prev = vec![false; 15];
    let mut edges = Vec::new();
    for (_, report) in read_fixture(&fixture("streamdeck_keys.txt")) {
        let keys = proto::parse_keys(&report, 15).unwrap();
        for (k, (now, was)) in keys.iter().zip(&prev).enumerate() {
            if now != was {
                edges.push((k, *now));
            }
        }
        prev = keys;
    }
    assert_eq!(edges, vec![(5, true), (5, false), (0, true), (14, true), (0, false), (14, false)]);
}

#[test]
fn example_project_controllers_all_parse() {
    let dir = example("");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "toml") {
            let t: toml::Table = std::fs::read_to_string(&p).unwrap().parse().unwrap();
            let stem = p.file_stem().unwrap().to_string_lossy().to_string();
            config::parse_file(&stem, &format!("controllers/{stem}.toml"), &t).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            n += 1;
        }
    }
    assert!(n >= 4);
    let t: toml::Table = std::fs::read_to_string(example("deck.toml")).unwrap().parse().unwrap();
    let Parsed::Deck(d) = config::parse_file("deck", "controllers/deck.toml", &t).unwrap() else { panic!() };
    assert_eq!(d.pages.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec!["show", "mix", "fx", "songs"]);
    assert_eq!(d.page("show").unwrap().keys[&5].action, Action::Preset("hype".into()));
}

#[test]
fn recorded_streamdeck_feature_reports() {
    // read from the Original V2 on this machine
    let f = read_fixture(&fixture("streamdeck_features.txt"));
    let get = |id: u64| f.iter().find(|(i, _)| *i == id).map(|(_, b)| b.clone()).unwrap();
    assert_eq!(proto::parse_serial(&get(6)).as_deref(), Some("AL46J2C62768"));
    assert_eq!(proto::parse_firmware(&get(5)).as_deref(), Some("1.03.000"));
    assert_eq!(get(6).len(), proto::FEATURE_LEN);
}
