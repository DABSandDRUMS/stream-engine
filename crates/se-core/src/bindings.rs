//! Bindings (§2.6): `param ← signal` with shaping, scopes, and takeover.

use crate::config::{BindingDef, Curve, Takeover};
use se_expr::Expr;

/// When a binding applies.
#[derive(Clone, Debug)]
pub enum BindScope {
    Always,
    Scene(String),
    Preset(String),
    Mode(String),
    Expr(Expr),
}

impl BindScope {
    pub fn parse(s: Option<&str>) -> Result<BindScope, String> {
        let Some(s) = s.map(str::trim) else { return Ok(BindScope::Always) };
        if s.is_empty() || s == "always" {
            return Ok(BindScope::Always);
        }
        let simple = |p: &str| s.strip_prefix(p).filter(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        if let Some(n) = simple("scene.") {
            return Ok(BindScope::Scene(n.into()));
        }
        if let Some(n) = simple("preset.") {
            return Ok(BindScope::Preset(n.into()));
        }
        if let Some(n) = simple("mode.") {
            return Ok(BindScope::Mode(n.into()));
        }
        Expr::parse(s).map(BindScope::Expr).map_err(|e| e.to_string())
    }
}

/// Runtime state of one binding.
pub struct BindingRt {
    pub def: BindingDef,
    pub scope: BindScope,
    /// Resolved target param ids (rebuilt when the state generation changes).
    pub targets: Vec<usize>,
    pub targets_gen: u64,
    pub signal: Option<usize>,
    pub signal_gen: u64,
    env: f64,
    peak: f64,
    norm_window_s: Option<f64>,
    /// Takeover bookkeeping.
    picked: bool,
    last_in: Option<f64>,
    last_out: Option<f64>,
    pub active: bool,
    /// Last shaped output (for scopes and the UI).
    pub output: f64,
}

impl BindingRt {
    pub fn new(def: BindingDef) -> Result<BindingRt, String> {
        let scope = BindScope::parse(def.scope.as_deref()).map_err(|e| format!("binding `{}` scope: {e}", def.name))?;
        let norm_window_s = match &def.auto_normalize {
            None => None,
            Some(se_proto::Value::Bool(false)) => None,
            Some(se_proto::Value::Bool(true)) => Some(10.0),
            Some(se_proto::Value::Str(s)) => {
                Some(se_proto::parse_duration_ms(s).ok_or_else(|| format!("binding `{}`: bad auto_normalize `{s}`", def.name))? as f64 / 1000.0)
            }
            Some(v) => v.as_f64(),
        };
        Ok(BindingRt {
            def,
            scope,
            targets: Vec::new(),
            targets_gen: u64::MAX,
            signal: None,
            signal_gen: u64::MAX,
            env: 0.0,
            peak: 0.0,
            norm_window_s,
            picked: false,
            last_in: None,
            last_out: None,
            active: false,
            output: 0.0,
        })
    }

    /// Reset smoothing/takeover when the binding leaves scope.
    pub fn deactivate(&mut self) {
        self.active = false;
        self.env = 0.0;
        self.picked = false;
        self.last_in = None;
        self.last_out = None;
    }

    /// Shape one raw signal sample. `dt` in seconds.
    pub fn shape(&mut self, raw: f64, dt: f64) -> f64 {
        let d = &self.def;
        let mut x = raw;
        if d.invert {
            x = 1.0 - x;
        }
        if let Some(w) = self.norm_window_s {
            // Adaptive gain: follow the running peak with exponential decay over the window,
            // so quiet and loud material feel the same.
            let decay = (-dt / w.max(0.05)).exp();
            self.peak = (self.peak * decay).max(x.abs()).max(1e-4);
            x /= self.peak;
        }
        if let Some(g) = d.gate
            && x < g
        {
            x = 0.0;
        }
        x = x * d.gain + d.offset;
        x = apply_curve(d.curve, x);
        let a = d.attack.map(|v| v.ms() as f64 / 1000.0).unwrap_or(0.0);
        let r = d.release.map(|v| v.ms() as f64 / 1000.0).unwrap_or(0.0);
        let tc = if x > self.env { a } else { r };
        self.env = if tc <= 0.0 { x } else { self.env + (x - self.env) * (1.0 - (-dt / tc).exp()) };
        let mut out = self.env;
        if let Some([lo, hi]) = d.range {
            out = lo + out * (hi - lo);
        }
        self.output = out;
        out
    }

    /// Apply takeover for physical controls. `current` is the target's current value.
    /// Returns `None` while the control hasn't picked up the value yet.
    pub fn takeover(&mut self, out: f64, current: Option<f64>) -> Option<f64> {
        let mode = self.def.takeover.unwrap_or(Takeover::Jump);
        let prev_in = self.last_in.replace(out);
        let result = match (mode, current) {
            (Takeover::Jump, _) | (_, None) => Some(out),
            (Takeover::Pickup, Some(cur)) => {
                // If the target moved elsewhere since our last write, drop the pickup.
                if let Some(lo) = self.last_out
                    && (lo - cur).abs() > 1e-6
                {
                    self.picked = false;
                }
                if !self.picked {
                    let tol = self.def.range.map(|[a, b]| (b - a).abs() * 0.01).unwrap_or(0.01);
                    let crossed = prev_in.is_some_and(|p| (p - cur).signum() != (out - cur).signum());
                    if (out - cur).abs() <= tol || crossed {
                        self.picked = true;
                    }
                }
                self.picked.then_some(out)
            }
            (Takeover::Scale, Some(cur)) => match prev_in {
                None => None,
                Some(p) if (out - p).abs() < 1e-9 => None,
                Some(p) => {
                    let [lo, hi] = self.def.range.unwrap_or([0.0, 1.0]);
                    // Scale the remaining travel so control and value meet at the end stop.
                    let v = if out > p {
                        let room_ctl = (hi - p).max(1e-9);
                        cur + (out - p) * (hi - cur) / room_ctl
                    } else {
                        let room_ctl = (p - lo).max(1e-9);
                        cur - (p - out) * (cur - lo) / room_ctl
                    };
                    Some(v.clamp(lo.min(hi), hi.max(lo)))
                }
            },
        };
        if let Some(v) = result {
            self.last_out = Some(v);
        }
        result
    }
}

pub fn apply_curve(c: Curve, x: f64) -> f64 {
    match c {
        Curve::Linear => x,
        Curve::Exp => x.signum() * x * x,
        Curve::Log => x.signum() * x.abs().sqrt(),
        Curve::Smoothstep => {
            let t = x.clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        }
        Curve::FaderTaper => fader_taper(x),
    }
}

/// Audio fader taper: control position 0–1 → linear gain, with 0.75 ≈ unity (0 dB),
/// top = +10 dB, and a steep fall to -∞ at the bottom (the usual console law).
pub fn fader_taper(pos: f64) -> f64 {
    let p = pos.clamp(0.0, 1.0);
    if p <= 0.0 {
        return 0.0;
    }
    let db = if p >= 0.75 {
        (p - 0.75) / 0.25 * 10.0
    } else if p >= 0.25 {
        (p - 0.75) / 0.5 * 30.0
    } else {
        -30.0 - (0.25 - p) / 0.25 * 60.0
    };
    10f64.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Dur;

    fn rt(f: impl FnOnce(&mut BindingDef)) -> BindingRt {
        let mut d = BindingDef { name: "t".into(), target: "a".into(), signal: "s".into(), ..Default::default() };
        f(&mut d);
        BindingRt::new(d).unwrap()
    }

    #[test]
    fn range_gate_and_gain() {
        let mut b = rt(|d| {
            d.range = Some([0.0, 12.0]);
            d.gate = Some(0.2);
        });
        assert_eq!(b.shape(0.1, 0.01), 0.0);
        assert_eq!(b.shape(0.5, 0.01), 6.0);
    }

    #[test]
    fn attack_release_smooths() {
        let mut b = rt(|d| {
            d.attack = Some(Dur(100));
            d.release = Some(Dur(100));
        });
        let v = b.shape(1.0, 0.1);
        assert!((v - (1.0 - (-1.0f64).exp())).abs() < 1e-9);
        let mut last = v;
        for _ in 0..10 {
            last = b.shape(0.0, 0.1);
        }
        assert!(last < 0.01);
    }

    #[test]
    fn auto_normalize_same_feel_quiet_and_loud() {
        let mut loud = rt(|d| d.auto_normalize = Some(se_proto::Value::Bool(true)));
        let mut quiet = rt(|d| d.auto_normalize = Some(se_proto::Value::Bool(true)));
        let (mut lo, mut qo) = (0.0, 0.0);
        for i in 0..2000 {
            let s = ((i as f64) * 0.1).sin().abs();
            lo = loud.shape(0.8 * s, 1.0 / 240.0);
            qo = quiet.shape(0.1 * s, 1.0 / 240.0);
        }
        assert!((lo - qo).abs() < 0.05, "loud {lo} quiet {qo}");
    }

    #[test]
    fn pickup_waits_for_crossing() {
        let mut b = rt(|d| d.takeover = Some(Takeover::Pickup));
        assert_eq!(b.takeover(0.2, Some(0.6)), None);
        assert_eq!(b.takeover(0.4, Some(0.6)), None);
        assert_eq!(b.takeover(0.65, Some(0.6)), Some(0.65), "crossed the current value");
        assert_eq!(b.takeover(0.7, Some(0.65)), Some(0.7));
        // someone else moved the target → pickup lost
        assert_eq!(b.takeover(0.72, Some(0.3)), None);
    }

    #[test]
    fn scale_meets_at_end_stop() {
        let mut b = rt(|d| d.takeover = Some(Takeover::Scale));
        assert_eq!(b.takeover(0.5, Some(0.8)), None);
        let v = b.takeover(1.0, Some(0.8)).unwrap();
        assert!((v - 1.0).abs() < 1e-9);
    }

    #[test]
    fn taper_unity_at_three_quarters() {
        assert!((fader_taper(0.75) - 1.0).abs() < 1e-9);
        assert_eq!(fader_taper(0.0), 0.0);
        assert!(fader_taper(1.0) > 3.0);
        assert!(fader_taper(0.5) < fader_taper(0.6));
    }

    #[test]
    fn scopes_parse() {
        assert!(matches!(BindScope::parse(Some("preset.hype")).unwrap(), BindScope::Preset(n) if n == "hype"));
        assert!(matches!(BindScope::parse(Some("mode == 'live'")).unwrap(), BindScope::Expr(_)));
        assert!(matches!(BindScope::parse(None).unwrap(), BindScope::Always));
    }
}
