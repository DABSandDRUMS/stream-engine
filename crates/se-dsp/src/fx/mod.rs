//! Built-in effects (§8.5), one module per family. Create them through [`crate::registry`];
//! the structs are public for direct use and for the parameter tables (`PARAMS`).
//!
//! Conventions shared by every effect here:
//! * effects output the fully processed signal; wet/dry mixing is the slot's job;
//! * continuous parameters are smoothed (per sample, or per 16-sample control block for
//!   filter coefficients); choices/bools that change the signal path crossfade;
//! * performance effects pass the input through bit-exactly while not triggered.

pub mod delay;
pub mod drive;
pub mod dynamics;
pub mod filter;
pub mod modulation;
pub mod performance;
pub mod pitch;
pub mod reverb;
pub mod utility;

use crate::ParamSpec;

/// Default values of a parameter table (for an effect's initial state).
pub(crate) fn defaults<const N: usize>(specs: &[ParamSpec]) -> [f32; N] {
    let mut v = [0.0; N];
    for (d, s) in v.iter_mut().zip(specs) {
        *d = s.default;
    }
    v
}

/// Low-frequency oscillator phase (0–1), free-running or locked to the beat clock.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Lfo {
    phase: f64,
}

impl Lfo {
    /// Advance one sample. `sync = Some((beat, period_beats))` locks the phase to the running
    /// beat position (so all synced LFOs agree with the transport); otherwise runs at `hz`.
    #[inline]
    pub fn tick(&mut self, hz: f32, sr: f32, sync: Option<(f64, f64)>) -> f64 {
        match sync {
            Some((beat, period)) => self.phase = (beat / period.max(1e-6)).rem_euclid(1.0),
            None => {
                self.phase += hz as f64 / sr as f64;
                if self.phase >= 1.0 {
                    self.phase -= self.phase.floor();
                }
            }
        }
        self.phase
    }

    pub fn reset(&mut self) {
        self.phase = 0.0;
    }
}

/// Sine of a phase in turns.
#[inline]
pub(crate) fn sin_turns(p: f64) -> f32 {
    (p * std::f64::consts::TAU).sin() as f32
}
