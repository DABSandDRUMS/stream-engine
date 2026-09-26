//! End-to-end synthesis with the real Kokoro model (fetched by scripts/fetch-tts-model.sh).
//!
//! Needs the model files; without them the test reports why and passes, unless
//! `SE_TTS_REQUIRE_MODEL=1` (CI with the model cache) makes their absence a failure.
//! `SE_TTS_MODEL_DIR` overrides the default `~/.local/share/stream-engine/models/kokoro`.
//! Writes `/tmp/se-tts-test.wav` (48 kHz) for listening.

use se_tts::config::{DEFAULT_MODEL, DEFAULT_VOICE};
use se_tts::kokoro::{SAMPLE_RATE, Synth, list_voices};
use se_tts::phonemize::Espeak;
use se_tts::resample::Upsampler2x;
use std::path::PathBuf;
use std::time::Instant;

const PHRASE: &str = "Thanks for the five hundred bits, let's go!";

fn model_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("SE_TTS_MODEL_DIR").map(PathBuf::from).unwrap_or_else(|| {
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
        data.join("stream-engine/models/kokoro")
    });
    let present = dir.join(DEFAULT_MODEL).is_file() && dir.join("voices").join(format!("{DEFAULT_VOICE}.bin")).is_file();
    if !present {
        let msg = format!("Kokoro model not found in {} — run scripts/fetch-tts-model.sh", dir.display());
        assert!(std::env::var_os("SE_TTS_REQUIRE_MODEL").is_none(), "{msg}");
        eprintln!("SKIPPED: {msg}");
        return None;
    }
    Some(dir)
}

/// 20 ms frame RMS values.
fn frames(x: &[f32], rate: u32) -> Vec<f64> {
    x.chunks(rate as usize / 50).map(|f| (f.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / f.len() as f64).sqrt()).collect()
}

#[test]
fn synthesizes_speech() {
    let Some(dir) = model_dir() else { return };
    let t0 = Instant::now();
    let mut synth = Synth::load(&dir.join(DEFAULT_MODEL), &dir.join("voices"), se_tts::INTRA_THREADS, Espeak::default()).expect("model loads");
    let load_s = t0.elapsed().as_secs_f64();
    assert!(list_voices(&dir.join("voices")).len() >= 20);

    let text = se_tts::text::normalize(PHRASE, 300);
    // warm-up run (first inference allocates ORT arenas), then the measured one
    synth.speak(&text, DEFAULT_VOICE, "en-us", 1.0, &|| false).unwrap();
    let t1 = Instant::now();
    let speech = synth.speak(&text, DEFAULT_VOICE, "en-us", 1.0, &|| false).unwrap().expect("not cancelled");
    let synth_s = t1.elapsed().as_secs_f64();
    let x = &speech.samples;
    let dur = x.len() as f64 / SAMPLE_RATE as f64;
    let rms = (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
    let peak = x.iter().fold(0f32, |m, v| m.max(v.abs()));
    println!("phonemes: {:?}", speech.phonemes);
    println!(
        "duration {dur:.2} s, RMS {rms:.4} ({:.1} dBFS), peak {peak:.3}, load {load_s:.2} s, synthesis {synth_s:.3} s, RTF {:.3}",
        20.0 * rms.log10(),
        synth_s / dur
    );

    assert_eq!(speech.phonemes.len(), 1, "one chunk");
    assert!(speech.phonemes[0].contains(", "), "clause comma reaches the model");
    assert!((1.5..=6.0).contains(&dur), "duration {dur:.2} s");
    assert!(x.iter().all(|v| v.is_finite()));
    assert!(rms > 0.02, "RMS {rms}");
    assert!(peak <= 1.0, "no clipping: peak {peak}");

    // speech-like: mostly active 20 ms frames, arranged in several separate bursts
    // (words/syllables with dips between them), not one constant tone or noise
    let f = frames(x, SAMPLE_RATE);
    let loud = f.iter().cloned().fold(0.0, f64::max);
    let active: Vec<bool> = f.iter().map(|r| *r > loud * 0.1).collect();
    let share = active.iter().filter(|a| **a).count() as f64 / f.len() as f64;
    let bursts = active.windows(2).filter(|w| !w[0] && w[1]).count() + usize::from(active[0]);
    println!("active frames {:.0} %, bursts {bursts}", share * 100.0);
    assert!((0.35..0.97).contains(&share), "active share {share:.2}");
    assert!(bursts >= 3, "{bursts} bursts");
    // the comma produces an audible pause somewhere in the middle
    let mid = &f[f.len() / 4..f.len() * 3 / 4];
    assert!(mid.iter().any(|r| *r < loud * 0.05), "pause at the comma");

    // speed changes duration in the right direction
    let fast = synth.speak(&text, DEFAULT_VOICE, "en-us", 1.4, &|| false).unwrap().unwrap().samples.len();
    assert!((fast as f64) < x.len() as f64 * 0.85, "speed 1.4: {fast} vs {}", x.len());
    // cancellation between chunks is honored
    assert!(synth.speak(&text, DEFAULT_VOICE, "en-us", 1.0, &|| true).unwrap().is_none());

    let wav = Upsampler2x::upsample(x);
    let spec = hound::WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    let mut w = hound::WavWriter::create("/tmp/se-tts-test.wav", spec).unwrap();
    wav.iter().for_each(|s| w.write_sample(*s).unwrap());
    w.finalize().unwrap();
}

#[test]
fn long_text_is_chunked_and_joined() {
    let Some(dir) = model_dir() else { return };
    let mut synth = Synth::load(&dir.join(DEFAULT_MODEL), &dir.join("voices"), se_tts::INTRA_THREADS, Espeak::default()).unwrap();
    let sentence = "Welcome back to the stream, everybody, tonight we are playing drums and testing the new lights";
    let text = se_tts::text::normalize(&std::iter::repeat_n(sentence, 8).collect::<Vec<_>>().join(". "), 0);
    let speech = synth.speak(&text, "bm_george", "en-gb", 1.2, &|| false).unwrap().unwrap();
    assert!(speech.phonemes.len() >= 2, "{} chunks", speech.phonemes.len());
    let dur = speech.samples.len() as f64 / SAMPLE_RATE as f64;
    assert!((15.0..90.0).contains(&dur), "{dur:.1} s");
    // bad voice names fail cleanly
    assert!(synth.speak("hi", "../../etc/passwd", "en-us", 1.0, &|| false).is_err());
    assert!(synth.speak("hi", "no_such_voice", "en-us", 1.0, &|| false).is_err());
}
