//! Audio taps for other subsystems (e.g. LTC in for timelines): lock-free mono copies of an
//! input channel or a bus channel, with block timestamps on the master clock.
//!
//! ```ignore
//! let mut tap = se_audio_taps::open("input.ltc.0", 0.5);
//! // tap.samples: rtrb::Consumer<f32> at tap.rate; tap.clock: one BlockStamp per audio block
//! ```
//!
//! Names: `input.<input>.<ch>` (0-based channel of a configured `[audio.inputs.<input>]`,
//! raw capture before gain/delay/effects) or `bus.<bus>.<ch>` (0 = left, 1 = right; the bus
//! input sum before its effect chain). Opening works before the audio graph runs; the
//! producer is attached as soon as it does (and re-attached after config reloads).

use parking_lot::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Master-clock time of a sample in a tap stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockStamp {
    /// Index of the first sample of the block in this tap's stream (counts from 0).
    pub sample: u64,
    /// Master-clock ns (CLOCK_MONOTONIC) of that sample.
    pub ts: u64,
}

/// Consumer side of a tap.
pub struct Tap {
    pub rate: u32,
    pub samples: rtrb::Consumer<f32>,
    pub clock: rtrb::Consumer<BlockStamp>,
}

/// Producer side (lives in the audio graph).
pub struct TapProducer {
    pub name: String,
    pub samples: rtrb::Producer<f32>,
    pub clock: rtrb::Producer<BlockStamp>,
    /// Samples written so far (the next block's `BlockStamp::sample`).
    pub written: u64,
    /// Samples dropped because the consumer fell behind.
    pub dropped: u64,
}

impl TapProducer {
    /// Push one block (RT-safe). Drops what doesn't fit (consumer too slow).
    pub fn push(&mut self, block: &[f32], ts: u64) {
        let _ = self.clock.push(BlockStamp { sample: self.written, ts });
        let n = block.len().min(self.samples.slots());
        if let Ok(mut chunk) = self.samples.write_chunk_uninit(n) {
            let (a, b) = chunk.as_mut_slices();
            for (d, s) in a.iter_mut().chain(b.iter_mut()).zip(block) {
                d.write(*s);
            }
            // SAFETY: all `n` slots were initialised above.
            unsafe { chunk.commit_all() };
        }
        self.dropped += (block.len() - n) as u64;
        self.written += block.len() as u64;
    }
}

static RATE: AtomicU32 = AtomicU32::new(48000);
static PENDING: Mutex<Vec<TapProducer>> = parking_lot::const_mutex(Vec::new());
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Graph rate for new taps (set by the audio engine).
pub fn set_rate(rate: u32) {
    RATE.store(rate, Ordering::Relaxed);
}

/// Open a mono tap (`input.<input>.<ch>` or `bus.<bus>.<ch>`) with `seconds` of buffering.
pub fn open(name: &str, seconds: f32) -> Tap {
    let rate = RATE.load(Ordering::Relaxed);
    let cap = ((rate as f32 * seconds.clamp(0.05, 30.0)) as usize).max(1024);
    let (sp, sc) = rtrb::RingBuffer::new(cap);
    let (cp, cc) = rtrb::RingBuffer::new(1024);
    PENDING.lock().push(TapProducer { name: name.to_string(), samples: sp, clock: cp, written: 0, dropped: 0 });
    GENERATION.fetch_add(1, Ordering::Release);
    Tap { rate, samples: sc, clock: cc }
}

/// Producers opened since the last call (audio engine only).
pub fn take_new() -> Vec<TapProducer> {
    std::mem::take(&mut *PENDING.lock())
}

/// Bumped on every `open` (audio engine polls it).
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// Parsed tap source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TapName {
    Input { input: String, ch: usize },
    Bus { bus: String, ch: usize },
}

pub fn parse_name(name: &str) -> Option<TapName> {
    let (kind, rest) = name.split_once('.')?;
    let (what, ch) = rest.rsplit_once('.')?;
    let ch: usize = ch.parse().ok()?;
    match kind {
        "input" => Some(TapName::Input { input: what.to_string(), ch }),
        "bus" if ch < 2 => Some(TapName::Bus { bus: what.to_string(), ch }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(parse_name("input.ltc.0"), Some(TapName::Input { input: "ltc".into(), ch: 0 }));
        assert_eq!(parse_name("bus.band.1"), Some(TapName::Bus { bus: "band".into(), ch: 1 }));
        assert_eq!(parse_name("bus.band.2"), None);
        assert_eq!(parse_name("ltc"), None);
    }

    #[test]
    fn push_stamps_and_drops_when_full() {
        let (sp, mut sc) = rtrb::RingBuffer::new(1024);
        let (cp, mut cc) = rtrb::RingBuffer::new(16);
        let mut p = TapProducer { name: "t".into(), samples: sp, clock: cp, written: 0, dropped: 0 };
        let block = [0.25f32; 600];
        p.push(&block, 10);
        p.push(&block, 20);
        assert_eq!(cc.pop().unwrap(), BlockStamp { sample: 0, ts: 10 });
        assert_eq!(cc.pop().unwrap(), BlockStamp { sample: 600, ts: 20 });
        assert_eq!(sc.slots(), 1024);
        assert_eq!(p.dropped, 176);
        assert_eq!(sc.pop().unwrap(), 0.25);
    }
}
