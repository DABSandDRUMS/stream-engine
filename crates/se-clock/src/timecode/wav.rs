//! Minimal RIFF/WAVE support for LTC audio: 16-bit PCM encode (what every LTC tool reads)
//! and decode of 16/24/32-bit PCM and 32-bit float files to interleaved f32.

/// Encode interleaved f32 samples as 16-bit PCM WAV.
pub fn encode_pcm16(samples: &[f32], sample_rate: u32, channels: u16) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&sample_rate.to_le_bytes());
    b.extend_from_slice(&(sample_rate * channels as u32 * 2).to_le_bytes());
    b.extend_from_slice(&(channels * 2).to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

/// Decoded audio: interleaved samples.
#[derive(Clone, Debug, PartialEq)]
pub struct Wav {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

impl Wav {
    /// One channel (0-based) as mono samples.
    pub fn channel(&self, ch: u16) -> Vec<f32> {
        self.samples.iter().skip(ch as usize).step_by(self.channels.max(1) as usize).copied().collect()
    }
}

/// Decode PCM 16/24/32-bit or IEEE float 32-bit WAV (including WAVE_FORMAT_EXTENSIBLE).
pub fn decode(bytes: &[u8]) -> Result<Wav, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let mut i = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len = u32_at(i + 4) as usize;
        let body = i + 8;
        let end = (body + len).min(bytes.len());
        if id == b"fmt " {
            if len < 16 {
                return Err("short fmt chunk".into());
            }
            let mut tag = u16_at(body);
            if tag == 0xFFFE && len >= 26 {
                tag = u16_at(body + 24); // sub-format GUID starts with the format tag
            }
            fmt = Some((tag, u16_at(body + 2), u32_at(body + 4), u16_at(body + 14)));
        } else if id == b"data" {
            let (tag, channels, sample_rate, bits) = fmt.ok_or("data before fmt")?;
            let data = &bytes[body..end];
            let samples: Vec<f32> = match (tag, bits) {
                (1, 16) => data.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c) as f32 / 32768.0).collect(),
                (1, 24) => data.as_chunks::<3>().0.iter().map(|c| (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f32 / 8_388_608.0).collect(),
                (1, 32) => data.as_chunks::<4>().0.iter().map(|c| i32::from_le_bytes(*c) as f32 / 2_147_483_648.0).collect(),
                (3, 32) => data.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect(),
                _ => return Err(format!("unsupported WAV format tag {tag} with {bits} bits")),
            };
            return Ok(Wav { sample_rate, channels: channels.max(1), samples });
        }
        i = body + len + (len & 1);
    }
    Err("no data chunk".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm16_roundtrip() {
        let s: Vec<f32> = (0..100).map(|i| ((i as f32) * 0.1).sin() * 0.5).collect();
        let w = decode(&encode_pcm16(&s, 48_000, 2)).unwrap();
        assert_eq!((w.sample_rate, w.channels, w.samples.len()), (48_000, 2, 100));
        assert!(w.samples.iter().zip(&s).all(|(a, b)| (a - b).abs() < 1e-4));
        assert_eq!(w.channel(1).len(), 50);
    }
}
