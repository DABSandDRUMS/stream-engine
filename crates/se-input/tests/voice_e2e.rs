//! End to end: a spoken phrase (generated with espeak-ng) → WAV → Whisper → grammar → intent.
//! Needs `espeak-ng` and the whisper model in `~/.local/share/stream-engine/models/` (the engine
//! downloads it on first start). Run with `cargo test -p se-input --test voice_e2e -- --ignored`.

use se_input::voice::grammar::{self, Intent, Vocab};
use se_input::voice::{Transcriber, audio, model_path};
use std::path::PathBuf;

fn vocab() -> Vocab {
    let n = |xs: &[(&str, Option<&str>)]| xs.iter().map(|(a, l)| (a.to_string(), Vocab::forms(a, *l))).collect();
    Vocab {
        scenes: n(&[("duo", None), ("wide", None), ("kit", None), ("drums", None), ("room", None), ("brb", Some("be right back"))]),
        presets: n(&[
            ("hype", Some("HYPE")),
            ("confetti", None),
            ("chill", None),
            ("chorus_blast", None),
            ("sub_big", None),
            ("blackout", None),
            ("strobe", None),
        ]),
        modes: n(&[("offline", None), ("preshow", None), ("live", None), ("brb", None), ("rehearsal", None)]),
        pages: Vec::new(),
    }
}

fn say(text: &str) -> Vec<f32> {
    let wav = std::env::temp_dir().join(format!("se-voice-{}.wav", text.replace(' ', "_")));
    let ok = std::process::Command::new("espeak-ng").args(["-v", "en-us", "-s", "150", "-w"]).arg(&wav).arg(text).status().expect("espeak-ng").success();
    assert!(ok, "espeak-ng failed");
    let bytes = std::fs::read(&wav).unwrap();
    let _ = std::fs::remove_file(&wav);
    audio::read_wav(&bytes).unwrap()
}

#[test]
#[ignore = "needs espeak-ng and the downloaded whisper model"]
fn spoken_commands_become_intents() {
    let data = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/stream-engine")).unwrap();
    let model = model_path(&data, "base.en");
    assert!(model.is_file(), "missing {} (start the engine once to download it)", model.display());
    let t = Transcriber::load(&model, 4, false).unwrap();
    let v = vocab();
    for (phrase, want) in [
        ("preset hype", Intent::Preset { name: "hype".into() }),
        ("scene wide", Intent::Scene { name: "wide".into(), cut: false }),
        ("take", Intent::Take),
        ("panic", Intent::Panic),
        ("next scene", Intent::Next),
        ("add marker", Intent::Marker { label: None }),
    ] {
        let pcm = say(phrase);
        let t0 = std::time::Instant::now();
        let text = t.transcribe(&pcm, &v.prompt()).unwrap();
        let got = grammar::parse(&text, &v).map(|x| x.0);
        eprintln!("{phrase:?} → {text:?} → {got:?} ({} ms)", t0.elapsed().as_millis());
        assert_eq!(got, Some(want), "phrase {phrase:?} transcribed as {text:?}");
    }
}
