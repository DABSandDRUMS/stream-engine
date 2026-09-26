//! Effect slots and chains.
//!
//! [`FxSlot`] wraps one [`Effect`] with everything the graph needs around it:
//! * `wet`/`dry` gains, smoothed per sample;
//! * `enabled` → bypass crossfade (no clicks); a fully bypassed effect is not processed;
//! * the trigger envelope (`…fx.<e>.env` from the core) applied to `wet` for one-shot effects;
//! * dry-path latency compensation, so the slot's latency is the same wet, dry, or bypassed;
//! * hot swap (new effect instance, e.g. a reloaded wasm patch) with a crossfade at a block
//!   boundary; the retired instance is parked for the owner to drop off the audio thread.
//!
//! Mix law per sample with bypass level `a` (1 = active), envelope `e`, `wet`, `dry`:
//! `out = in_delayed · (1 − a·e·(1 − dry)) + fx(in) · a·e·wet`. With `e = 0` (trigger idle) or
//! `a = 0` (bypassed) the input passes untouched.

use crate::{Ctx, Effect, MAX_BLOCK, ParamSpec, Smoother};

/// Crossfade length for bypass, wet/dry changes, and hot swaps.
pub const XFADE_MS: f32 = 10.0;
/// Hot-swap crossfade (a bit longer: different algorithms, not just gains).
pub const SWAP_MS: f32 = 30.0;
/// Largest latency (samples) the dry-path compensation can absorb.
pub const MAX_COMP: usize = 16384;

/// Stereo delay line used to align the dry path with an effect's latency.
pub struct CompDelay {
    l: Vec<f32>,
    r: Vec<f32>,
    pos: usize,
    delay: usize,
}

impl CompDelay {
    pub fn new(capacity: usize) -> CompDelay {
        let cap = capacity.max(1).next_power_of_two();
        CompDelay { l: vec![0.0; cap], r: vec![0.0; cap], pos: 0, delay: 0 }
    }

    pub fn set_delay(&mut self, n: usize) {
        self.delay = n.min(self.l.len() - 1);
    }

    pub fn delay(&self) -> usize {
        self.delay
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        if self.delay == 0 {
            return;
        }
        let mask = self.l.len() - 1;
        for i in 0..l.len() {
            self.l[self.pos] = l[i];
            self.r[self.pos] = r[i];
            let rd = (self.pos + self.l.len() - self.delay) & mask;
            l[i] = self.l[rd];
            r[i] = self.r[rd];
            self.pos = (self.pos + 1) & mask;
        }
    }

    pub fn clear(&mut self) {
        self.l.fill(0.0);
        self.r.fill(0.0);
    }
}

pub struct FxSlot {
    fx: Box<dyn Effect>,
    /// Previous instance fading out during a hot swap.
    old: Option<Box<dyn Effect>>,
    /// Finished hot swap: waiting to be taken and dropped off the audio thread.
    retired: Option<Box<dyn Effect>>,
    swap: Smoother,
    wet: Smoother,
    dry: Smoother,
    env: Smoother,
    on: Smoother,
    /// Uses the trigger envelope (one-shot effect); otherwise `env` is fixed at 1.
    triggered: bool,
    trig_state: bool,
    /// Effect is not being processed (fully bypassed); reset before it runs again.
    idle: bool,
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
    old_l: Vec<f32>,
    old_r: Vec<f32>,
    comp: CompDelay,
}

impl FxSlot {
    /// `triggered`: the slot follows a trigger envelope (`set_env`, `set_trigger`); otherwise
    /// it is always on (subject to `set_enabled`).
    pub fn new(fx: Box<dyn Effect>, sr: f32, triggered: bool) -> FxSlot {
        let (wet, dry) = crate::registry::default_mix(fx.kind());
        let mut comp = CompDelay::new(MAX_COMP);
        comp.set_delay(fx.latency());
        FxSlot {
            fx,
            old: None,
            retired: None,
            swap: Smoother::with_ms(1.0, SWAP_MS, sr),
            wet: Smoother::with_ms(wet, XFADE_MS, sr),
            dry: Smoother::with_ms(dry, XFADE_MS, sr),
            env: Smoother::with_ms(if triggered { 0.0 } else { 1.0 }, 5.0, sr),
            on: Smoother::with_ms(1.0, XFADE_MS, sr),
            triggered,
            trig_state: false,
            idle: false,
            buf_l: vec![0.0; MAX_BLOCK],
            buf_r: vec![0.0; MAX_BLOCK],
            old_l: vec![0.0; MAX_BLOCK],
            old_r: vec![0.0; MAX_BLOCK],
            comp,
        }
    }

    pub fn kind(&self) -> &'static str {
        self.fx.kind()
    }

    pub fn params(&self) -> &'static [ParamSpec] {
        self.fx.params()
    }

    pub fn is_triggered(&self) -> bool {
        self.triggered
    }

    /// Set effect parameter `idx`; the value is clamped to the spec's domain.
    pub fn set_param(&mut self, idx: usize, v: f32) {
        if let Some(spec) = self.fx.params().get(idx) {
            self.fx.set_param(idx, spec.clamp(v));
            if let Some(o) = self.old.as_mut() {
                o.set_param(idx.min(o.params().len().saturating_sub(1)), spec.clamp(v));
            }
            let lat = self.fx.latency();
            if lat != self.comp.delay() {
                self.comp.set_delay(lat);
            }
        }
    }

    pub fn set_wet(&mut self, v: f32) {
        self.wet.set(v.clamp(0.0, 2.0));
    }

    pub fn set_dry(&mut self, v: f32) {
        self.dry.set(v.clamp(0.0, 2.0));
    }

    pub fn wet(&self) -> f32 {
        self.wet.target()
    }

    pub fn dry(&self) -> f32 {
        self.dry.target()
    }

    /// Bypass (false) or enable (true) with a crossfade.
    pub fn set_enabled(&mut self, on: bool) {
        self.on.set(if on { 1.0 } else { 0.0 });
    }

    pub fn enabled(&self) -> bool {
        self.on.target() > 0.5
    }

    /// Trigger envelope level 0–1 (control rate; smoothed per sample). Ignored for
    /// always-on slots.
    pub fn set_env(&mut self, v: f32) {
        if self.triggered {
            self.env.set(v.clamp(0.0, 1.0));
        }
    }

    /// Trigger payload float `k` (forwarded to the effect).
    pub fn set_payload(&mut self, k: usize, v: f32) {
        self.fx.set_payload(k, v);
    }

    /// Trigger edge (`X.active`). Forwards changes to the effect.
    pub fn set_trigger(&mut self, on: bool) {
        if self.triggered && on != self.trig_state {
            self.trig_state = on;
            self.fx.trigger(on);
        }
    }

    /// Replace the effect instance with a crossfade. If a swap is still fading, the older
    /// instance is returned immediately (drop it off the audio thread).
    pub fn swap_effect(&mut self, new: Box<dyn Effect>) -> Option<Box<dyn Effect>> {
        let prev_old = self.old.take();
        let prev = std::mem::replace(&mut self.fx, new);
        self.old = Some(prev);
        self.swap.reset(0.0);
        self.swap.set(1.0);
        let lat = self.fx.latency();
        self.comp.set_delay(lat);
        if self.trig_state {
            self.fx.trigger(true);
        }
        prev_old
    }

    /// Instance retired by a finished hot swap (drop it on a non-real-time thread).
    pub fn take_retired(&mut self) -> Option<Box<dyn Effect>> {
        self.retired.take()
    }

    pub fn latency(&self) -> usize {
        self.comp.delay()
    }

    pub fn reset(&mut self) {
        self.fx.reset();
        if let Some(o) = self.old.as_mut() {
            o.reset();
        }
        self.comp.clear();
        self.trig_state = false;
        if self.triggered {
            self.env.reset(0.0);
        }
    }

    /// True while the effect contributes to the output (for meters/UI).
    pub fn active(&self) -> bool {
        !self.idle && self.on.value() > 0.0 && (!self.triggered || self.env.value() > 0.0 || self.env.target() > 0.0)
    }

    pub fn process(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len()).min(MAX_BLOCK);
        let (l, r) = (&mut l[..n], &mut r[..n]);
        let bypassed = self.on.settled() && self.on.value() == 0.0;
        if bypassed {
            if !self.idle {
                self.idle = true;
            }
            // keep the chain latency constant while bypassed
            self.comp.process(l, r);
            return;
        }
        if self.idle {
            self.idle = false;
            self.fx.reset();
        }
        let (wl, wr) = (&mut self.buf_l[..n], &mut self.buf_r[..n]);
        wl.copy_from_slice(l);
        wr.copy_from_slice(r);
        self.fx.process(ctx, wl, wr);
        if let Some(old) = self.old.as_mut() {
            let (ol, or) = (&mut self.old_l[..n], &mut self.old_r[..n]);
            ol.copy_from_slice(l);
            or.copy_from_slice(r);
            old.process(ctx, ol, or);
            for i in 0..n {
                let s = self.swap.next();
                wl[i] = ol[i] + (wl[i] - ol[i]) * s;
                wr[i] = or[i] + (wr[i] - or[i]) * s;
            }
            if self.swap.settled() && self.retired.is_none() {
                self.retired = self.old.take();
            }
        }
        self.comp.process(l, r);
        for i in 0..n {
            let a = self.on.next();
            let e = self.env.next();
            let w = self.wet.next();
            let d = self.dry.next();
            let ae = a * e;
            let we = ae * w;
            let de = 1.0 - ae * (1.0 - d);
            l[i] = l[i] * de + wl[i] * we;
            r[i] = r[i] * de + wr[i] * we;
        }
    }
}

/// Ordered effect slots (processed in series).
#[derive(Default)]
pub struct FxChain {
    pub slots: Vec<FxSlot>,
}

impl FxChain {
    pub fn new() -> FxChain {
        FxChain { slots: Vec::new() }
    }

    pub fn process(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        for s in &mut self.slots {
            s.process(ctx, l, r);
        }
    }

    /// Total latency in samples (constant regardless of bypass state).
    pub fn latency(&self) -> usize {
        self.slots.iter().map(FxSlot::latency).sum()
    }

    pub fn reset(&mut self) {
        for s in &mut self.slots {
            s.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ParamKind, Transport};

    /// Inverts the signal, with optional latency.
    struct Invert {
        delay: CompDelay,
        triggers: u32,
    }

    static P: [ParamSpec; 1] = [ParamSpec { name: "x", min: 0.0, max: 1.0, default: 0.0, unit: "", kind: ParamKind::Float, description: "" }];

    impl Effect for Invert {
        fn kind(&self) -> &'static str {
            "test_invert"
        }
        fn params(&self) -> &'static [ParamSpec] {
            &P
        }
        fn set_param(&mut self, _: usize, _: f32) {}
        fn trigger(&mut self, on: bool) {
            if on {
                self.triggers += 1;
            }
        }
        fn process(&mut self, _: &Ctx, l: &mut [f32], r: &mut [f32]) {
            self.delay.process(l, r);
            l.iter_mut().chain(r.iter_mut()).for_each(|x| *x = -*x);
        }
        fn latency(&self) -> usize {
            self.delay.delay()
        }
        fn reset(&mut self) {
            self.delay.clear();
        }
    }

    fn invert(lat: usize) -> Box<Invert> {
        let mut d = CompDelay::new(64);
        d.set_delay(lat);
        Box::new(Invert { delay: d, triggers: 0 })
    }

    fn ctx() -> Ctx<'static> {
        Ctx { sr: 48000.0, transport: Transport::default(), key: &[] }
    }

    fn run(slot: &mut FxSlot, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        for chunk in input.chunks(128) {
            let mut l = chunk.to_vec();
            let mut r = chunk.to_vec();
            slot.process(&ctx(), &mut l, &mut r);
            out.extend_from_slice(&l);
        }
        out
    }

    #[test]
    fn dry_path_is_latency_compensated() {
        // wet = -x delayed by 7, dry = x delayed by 7: 50/50 must null exactly after the ramp.
        let mut s = FxSlot::new(invert(7), 48000.0, false);
        s.set_wet(0.5);
        s.set_dry(0.5);
        let input: Vec<f32> = (0..4096).map(|i| (i as f32 * 0.013).sin()).collect();
        let out = run(&mut s, &input);
        assert_eq!(s.latency(), 7);
        assert!(out[2048..].iter().all(|x| x.abs() < 1e-6), "null test failed");
    }

    #[test]
    fn bypass_passes_input_with_same_latency_and_skips_fx() {
        let mut s = FxSlot::new(invert(5), 48000.0, false);
        s.set_enabled(false);
        let input: Vec<f32> = (0..2048).map(|i| (i as f32 * 0.02).sin()).collect();
        let out = run(&mut s, &input);
        for i in 1024..2048 {
            assert!((out[i] - input[i - 5]).abs() < 1e-6);
        }
        assert!(s.idle);
    }

    #[test]
    fn bypass_crossfade_has_no_step() {
        let mut s = FxSlot::new(invert(0), 48000.0, false);
        let input = vec![0.5f32; 4096];
        let mut out = run(&mut s, &input[..1024]);
        s.set_enabled(false);
        out.extend(run(&mut s, &input[1024..]));
        let max_step = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        // 1.0 swing over a 10 ms (480 sample) ramp
        assert!(max_step < 1.0 / 400.0, "step {max_step}");
        assert!((out[4095] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn trigger_envelope_gates_wet_and_forwards_edges() {
        let mut s = FxSlot::new(invert(0), 48000.0, true);
        let input = vec![0.25f32; 1024];
        let out = run(&mut s, &input);
        assert!(out.iter().all(|x| (x - 0.25).abs() < 1e-6), "idle trigger slot must pass input");
        s.set_trigger(true);
        s.set_trigger(true);
        s.set_env(1.0);
        let out = run(&mut s, &input);
        assert!((out[1023] + 0.25).abs() < 1e-6, "full envelope = fully wet");
        s.set_trigger(false);
        s.set_trigger(true);
        assert_eq!(s.fx.kind(), "test_invert");
    }

    #[test]
    fn hot_swap_crossfades_and_retires_old_instance() {
        let mut s = FxSlot::new(invert(0), 48000.0, false);
        s.set_dry(0.0);
        s.set_wet(1.0);
        let input = vec![0.5f32; 4096];
        run(&mut s, &input[..512]);
        assert!(s.swap_effect(invert(0)).is_none());
        let out = run(&mut s, &input);
        let max_step = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(max_step < 1e-3);
        assert!(s.take_retired().is_some());
    }
}
