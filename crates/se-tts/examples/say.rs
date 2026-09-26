//! Offline TTS: synthesize text to a mono WAV with the same pipeline the engine uses.
//!
//! ```text
//! cargo run -p se-tts --example say -- --text "Thanks for the bits!" --voice af_heart --out /tmp/x.wav
//!   [--model-dir DIR] [--model model_quantized.onnx] [--lang en-us] [--speed 1.0] [--rate 48000|24000]
//! ```

use se_tts::config::{DEFAULT_MODEL, DEFAULT_VOICE};
use se_tts::kokoro::{SAMPLE_RATE, Synth};
use se_tts::phonemize::{Espeak, lang_for_voice};
use se_tts::resample::Upsampler2x;
use std::path::PathBuf;
use std::time::Instant;

struct Args {
    text: String,
    voice: String,
    out: PathBuf,
    model_dir: PathBuf,
    model: String,
    lang: Option<String>,
    speed: f32,
    rate: u32,
}

fn usage() -> ! {
    eprintln!(
        "usage: say --text TEXT [--voice {DEFAULT_VOICE}] [--out /tmp/tts.wav] [--model-dir DIR] [--model {DEFAULT_MODEL}] [--lang LANG] [--speed 1.0] [--rate 48000|24000]"
    );
    std::process::exit(2)
}

fn parse() -> Args {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    let mut a = Args {
        text: String::new(),
        voice: DEFAULT_VOICE.into(),
        out: PathBuf::from("/tmp/tts.wav"),
        model_dir: data.join("stream-engine/models/kokoro"),
        model: DEFAULT_MODEL.into(),
        lang: None,
        speed: 1.0,
        rate: 48_000,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--text" => a.text = val(),
            "--voice" => a.voice = val(),
            "--out" => a.out = val().into(),
            "--model-dir" => a.model_dir = val().into(),
            "--model" => a.model = val(),
            "--lang" => a.lang = Some(val()),
            "--speed" => a.speed = val().parse().unwrap_or_else(|_| usage()),
            "--rate" => a.rate = val().parse().ok().filter(|r| *r == 24_000 || *r == 48_000).unwrap_or_else(|| usage()),
            _ => usage(),
        }
    }
    if a.text.trim().is_empty() {
        usage();
    }
    a
}

fn main() -> anyhow::Result<()> {
    let a = parse();
    let espeak = Espeak::default();
    let t0 = Instant::now();
    let mut synth = Synth::load(&a.model_dir.join(&a.model), &a.model_dir.join("voices"), se_tts::INTRA_THREADS, espeak.clone())?;
    let load = t0.elapsed();
    let lang = a.lang.clone().or_else(|| lang_for_voice(&a.voice).map(str::to_string)).unwrap_or_else(|| "en-us".into());
    let text = se_tts::text::normalize(&a.text, 0);
    let t1 = Instant::now();
    let speech = synth.speak(&text, &a.voice, &lang, a.speed, &|| false)?.expect("not cancelled");
    let synth_s = t1.elapsed().as_secs_f64();
    let samples = if a.rate == 48_000 { Upsampler2x::upsample(&speech.samples) } else { speech.samples.clone() };
    let spec = hound::WavSpec { channels: 1, sample_rate: a.rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    let mut w = hound::WavWriter::create(&a.out, spec)?;
    for s in &samples {
        w.write_sample(*s)?;
    }
    w.finalize()?;
    let dur = speech.samples.len() as f64 / SAMPLE_RATE as f64;
    let rms = (samples.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / samples.len().max(1) as f64).sqrt();
    let peak = samples.iter().fold(0f32, |m, v| m.max(v.abs()));
    println!("espeak-ng {}, model {} ({})", espeak.version()?, a.model, synth.model.signature());
    println!("text:     {text}");
    for (i, p) in speech.phonemes.iter().enumerate() {
        println!("phonemes[{i}] ({} tokens): {p}", p.chars().count());
    }
    println!(
        "wrote {} ({} Hz): {dur:.2} s, RMS {rms:.4} ({:.1} dBFS), peak {peak:.3}; model load {:.2} s, synthesis {synth_s:.3} s, RTF {:.3}",
        a.out.display(),
        a.rate,
        20.0 * rms.log10(),
        load.as_secs_f64(),
        synth_s / dur
    );
    Ok(())
}
