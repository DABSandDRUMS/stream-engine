//! Push-to-talk audio: short-lived ALSA capture (through the PipeWire ALSA plugin by default),
//! WAV reading, and resampling to Whisper's 16 kHz mono f32.

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub const RATE: u32 = 16_000;

/// Record from `device` until `stop` is set or `max_secs` elapse. Returns 16 kHz mono.
pub fn capture(device: &str, stop: Arc<AtomicBool>, max_secs: f32) -> Result<Vec<f32>, String> {
    let pcm = PCM::new(device, Direction::Capture, false).map_err(|e| format!("open capture `{device}`: {e}"))?;
    let rate = {
        let hwp = HwParams::any(&pcm).map_err(|e| e.to_string())?;
        hwp.set_access(Access::RWInterleaved).map_err(|e| format!("access: {e}"))?;
        hwp.set_format(Format::s16()).map_err(|e| format!("format: {e}"))?;
        hwp.set_channels(1).map_err(|e| format!("mono: {e}"))?;
        hwp.set_rate(RATE, ValueOr::Nearest).map_err(|e| format!("rate: {e}"))?;
        hwp.set_period_time_near(20_000, ValueOr::Nearest).map_err(|e| format!("period: {e}"))?;
        hwp.set_buffer_time_near(200_000, ValueOr::Nearest).map_err(|e| format!("buffer: {e}"))?;
        pcm.hw_params(&hwp).map_err(|e| format!("hw params: {e}"))?;
        hwp.get_rate().map_err(|e| e.to_string())?
    };
    let io = pcm.io_i16().map_err(|e| e.to_string())?;
    pcm.start().map_err(|e| format!("start: {e}"))?;
    let max = (max_secs.max(0.5) * rate as f32) as usize;
    let mut out: Vec<f32> = Vec::with_capacity(max.min(rate as usize * 10));
    let mut buf = vec![0i16; (rate / 50) as usize];
    while !stop.load(Ordering::Acquire) && out.len() < max {
        match io.readi(&mut buf) {
            Ok(n) => out.extend(buf[..n].iter().map(|s| *s as f32 / 32768.0)),
            Err(e) => {
                // overrun: recover and keep going
                pcm.try_recover(e, true).map_err(|e| format!("capture: {e}"))?;
            }
        }
    }
    let _ = pcm.drop();
    Ok(resample(&out, rate, RATE))
}

/// Windowed-sinc low-pass (when downsampling) + linear interpolation.
pub fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || x.is_empty() {
        return x.to_vec();
    }
    let filtered;
    let src: &[f32] = if to < from {
        let fc = 0.45 * to as f32 / from as f32; // normalized cutoff (cycles/sample)
        let taps = 31i32;
        let h: Vec<f32> = (-(taps / 2)..=taps / 2)
            .map(|n| {
                let n = n as f32;
                let sinc = if n == 0.0 { 2.0 * fc } else { (2.0 * std::f32::consts::PI * fc * n).sin() / (std::f32::consts::PI * n) };
                let w = 0.54 + 0.46 * (std::f32::consts::PI * n / (taps / 2) as f32).cos();
                sinc * w
            })
            .collect();
        let sum: f32 = h.iter().sum();
        filtered = (0..x.len())
            .map(|i| {
                let mut acc = 0.0;
                for (k, c) in h.iter().enumerate() {
                    let j = i as i64 + k as i64 - (taps / 2) as i64;
                    if j >= 0 && (j as usize) < x.len() {
                        acc += c * x[j as usize];
                    }
                }
                acc / sum
            })
            .collect::<Vec<f32>>();
        &filtered
    } else {
        x
    };
    let ratio = from as f64 / to as f64;
    let n = ((src.len() as f64) / ratio).floor() as usize;
    (0..n)
        .map(|i| {
            let p = i as f64 * ratio;
            let j = p.floor() as usize;
            let f = (p - j as f64) as f32;
            let a = src[j];
            let b = src.get(j + 1).copied().unwrap_or(a);
            a + (b - a) * f
        })
        .collect()
}

/// Read a WAV file (PCM 8/16/24/32-bit or float) as 16 kHz mono.
pub fn read_wav(bytes: &[u8]) -> Result<Vec<f32>, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let (mut fmt, mut data) = (None, None);
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len = u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        let body = &bytes[i + 8..(i + 8 + len).min(bytes.len())];
        match id {
            b"fmt " => fmt = Some(body),
            b"data" => data = Some(body),
            _ => {}
        }
        i += 8 + len + (len & 1);
    }
    let fmt = fmt.ok_or("WAV without fmt chunk")?;
    let data = data.ok_or("WAV without data chunk")?;
    if fmt.len() < 16 {
        return Err("short fmt chunk".into());
    }
    let mut tag = u16::from_le_bytes([fmt[0], fmt[1]]);
    let channels = u16::from_le_bytes([fmt[2], fmt[3]]).max(1) as usize;
    let rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
    let bits = u16::from_le_bytes([fmt[14], fmt[15]]);
    if tag == 0xFFFE && fmt.len() >= 26 {
        tag = u16::from_le_bytes([fmt[24], fmt[25]]); // WAVE_FORMAT_EXTENSIBLE sub-format
    }
    let bps = (bits as usize).div_ceil(8);
    if bps == 0 || rate == 0 {
        return Err("bad WAV format".into());
    }
    let frame = bps * channels;
    let sample = |b: &[u8]| -> f32 {
        match (tag, bits) {
            (1, 8) => (b[0] as f32 - 128.0) / 128.0,
            (1, 16) => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
            (1, 24) => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
            (1, 32) => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
            (3, 32) => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            _ => 0.0,
        }
    };
    if !matches!((tag, bits), (1, 8) | (1, 16) | (1, 24) | (1, 32) | (3, 32)) {
        return Err(format!("unsupported WAV encoding (format {tag}, {bits} bit)"));
    }
    let mono: Vec<f32> = data.chunks_exact(frame).map(|f| (0..channels).map(|c| sample(&f[c * bps..])).sum::<f32>() / channels as f32).collect();
    Ok(resample(&mono, rate, RATE))
}

/// RMS level (for "nothing was said" detection).
pub fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let mut b = Vec::new();
        let data_len = (samples.len() * 2) as u32;
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_len).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2 * channels as u32).to_le_bytes());
        b.extend_from_slice(&(2 * channels).to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            b.extend_from_slice(&s.to_le_bytes());
        }
        b
    }

    #[test]
    fn wav_stereo_48k_to_16k_mono() {
        // 0.5 s of a 440 Hz tone in both channels
        let n = 24_000;
        let s: Vec<i16> = (0..n)
            .flat_map(|i| {
                let v = ((i as f32 / 48_000.0 * 440.0 * std::f32::consts::TAU).sin() * 16000.0) as i16;
                [v, v]
            })
            .collect();
        let out = read_wav(&wav(48_000, 2, &s)).unwrap();
        assert!((out.len() as i64 - 8000).abs() <= 1, "{}", out.len());
        let r = rms(&out);
        assert!((r - 0.345).abs() < 0.03, "rms {r}");
    }

    #[test]
    fn resample_keeps_low_frequencies_and_rejects_garbage() {
        let x: Vec<f32> = (0..22_050).map(|i| (i as f32 / 22_050.0 * 200.0 * std::f32::consts::TAU).sin()).collect();
        let y = resample(&x, 22_050, 16_000);
        assert!((y.len() as i64 - 16_000).abs() <= 1);
        assert!((rms(&y) - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.02);
        assert!(read_wav(b"nope").is_err());
    }
}
