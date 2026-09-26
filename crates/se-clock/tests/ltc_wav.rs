//! LTC through a WAV file at ±10 % speed: our encoder → WAV → our decoder, and the same file
//! through libltc's `ltcdump` when it is installed (`~/.local/share/stream-engine/tools/ltc`).

use se_clock::timecode::ltc::{LtcDecoder, LtcEncoder, LtcFrame};
use se_clock::timecode::{FrameRate, Timecode, wav};
use std::path::PathBuf;

fn render(rate: FrameRate, speed: f64, start: &str, frames: u64) -> Vec<f32> {
    let mut enc = LtcEncoder::new(48_000, 0.5);
    let first = Timecode::parse(start, rate).unwrap().to_frames();
    let mut out = Vec::new();
    for i in 0..frames {
        let frame = LtcFrame { user_bits: 0x1234_5678, ..LtcFrame::new(Timecode::from_frames(first + i, rate)) };
        enc.encode_frame(&frame, rate.fps() * speed, &mut out);
    }
    out
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("se-clock-ltc-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

fn ltcdump() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("HOME")?).join(".local/share/stream-engine/tools/ltc/bin/ltcdump");
    p.exists().then_some(p)
}

#[test]
fn ltc_wav_decodes_at_plus_minus_10_percent() {
    for (rate, start) in
        [(FrameRate::Fps25, "10:00:00:00"), (FrameRate::Fps2997Df, "00:00:58;00"), (FrameRate::Fps30, "01:59:59:00"), (FrameRate::Fps24, "00:00:00:00")]
    {
        for speed in [0.9, 1.0, 1.1] {
            let audio = render(rate, speed, start, 150);
            let path = tmp(&format!("ltc-{}-{speed}.wav", rate.as_str()));
            std::fs::write(&path, wav::encode_pcm16(&audio, 48_000, 1)).unwrap();

            let w = wav::decode(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(w.sample_rate, 48_000);
            let mut dec = LtcDecoder::new(w.sample_rate, rate);
            let mut got = Vec::new();
            dec.feed(&w.channel(0), |f| got.push(f));
            assert!(got.len() >= 148, "{rate} x{speed}: decoded {} of 150", got.len());
            let first = Timecode::parse(start, rate).unwrap().to_frames();
            for (k, g) in got.iter().enumerate() {
                let expect = Timecode::from_frames(first + (150 - got.len() + k) as u64, rate);
                assert_eq!(g.frame.tc, expect, "{rate} x{speed} frame {k}");
                assert_eq!(g.frame.user_bits, 0x1234_5678);
                assert!((g.speed(48_000) - speed).abs() < 0.01, "{rate} x{speed}: speed {}", g.speed(48_000));
            }
            if rate == FrameRate::Fps2997Df {
                assert!(got.iter().any(|g| g.frame.tc.to_string() == "00:00:59;29"));
                assert!(got.iter().any(|g| g.frame.tc.to_string() == "00:01:00;02"), "drop-frame skip decoded");
            }

            if let Some(tool) = ltcdump() {
                let fps = match rate {
                    FrameRate::Fps2997Df => "30000/1001".to_string(),
                    r => r.nominal().to_string(),
                };
                let out = std::process::Command::new(&tool).arg("-f").arg(&fps).arg(&path).output().unwrap();
                assert!(out.status.success(), "ltcdump failed: {}", String::from_utf8_lossy(&out.stderr));
                let text = String::from_utf8_lossy(&out.stdout);
                let labels: Vec<String> = text
                    .lines()
                    .filter(|l| !l.starts_with('#'))
                    .filter_map(|l| l.split_whitespace().nth(1))
                    .map(|s| {
                        // ltcdump prints `.` before the frame number when the DF bit is set
                        let mut s = s.to_string();
                        if rate.drop_frame() {
                            s.replace_range(8..9, ";");
                        }
                        s
                    })
                    .collect();
                assert!(labels.len() >= 148, "{rate} x{speed}: ltcdump decoded {}:\n{text}", labels.len());
                let ours: Vec<String> = got.iter().map(|g| g.frame.tc.to_string()).collect();
                for l in &labels {
                    assert!(ours.contains(l), "{rate} x{speed}: ltcdump saw {l} we did not");
                }
                assert!(!text.lines().skip(3).any(|l| l.starts_with("#DISCONTINUITY")), "{rate} x{speed}: ltcdump reports a gap:\n{text}");
            } else {
                eprintln!("ltcdump not installed; external decode skipped");
            }
            let _ = std::fs::remove_file(&path);
        }
    }
}
