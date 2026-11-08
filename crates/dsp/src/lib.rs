//! Building blocks shared by the generators: filters, oscillators and small
//! helpers. Everything works in f64 and per sample; nothing allocates.

use std::f64::consts::PI;

// ---------------------------------------------------------------- filters

/// Zavalishin TPT state-variable filter: lowpass, bandpass and highpass from
/// one structure, stable under fast cutoff modulation.
///
/// Use [`Svf::new`] + [`Svf::tick`] for a fixed cutoff, or [`Svf::lp`] to
/// set the cutoff every sample.
#[derive(Clone, Copy, Debug, Default)]
pub struct Svf {
    ic1: f64,
    ic2: f64,
    a1: f64,
    a2: f64,
    a3: f64,
    k: f64,
}

impl Svf {
    /// `res` 0..1 (clamped to 0.97; 1 would self-oscillate forever).
    pub fn new(fc: f64, res: f64, sr: f64) -> Svf {
        let mut f = Svf::default();
        f.set(fc, res, sr);
        f
    }

    pub fn set(&mut self, fc: f64, res: f64, sr: f64) {
        let fc = fc.clamp(20.0, sr * 0.45);
        let g = (PI * fc / sr).tan();
        self.k = 2.0 - 2.0 * res.clamp(0.0, 0.97);
        self.a1 = 1.0 / (1.0 + g * (g + self.k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }

    /// Returns (lowpass, bandpass, highpass).
    pub fn tick(&mut self, x: f64) -> (f64, f64, f64) {
        let v3 = x - self.ic2;
        let v1 = self.a1 * self.ic1 + self.a2 * v3;
        let v2 = self.ic2 + self.a2 * self.ic1 + self.a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        (v2, v1, x - self.k * v1 - v2)
    }

    /// Set the cutoff and return the lowpass output, in one step.
    pub fn lp(&mut self, x: f64, fc: f64, res: f64, sr: f64) -> f64 {
        self.set(fc, res, sr);
        self.tick(x).0
    }
}

/// Four-pole (24 dB/oct) transistor-ladder lowpass with saturation inside the
/// feedback loop: the squelchy, resonant acid filter. Zero-delay-feedback
/// one-pole stages; the tanh nonlinearity uses the previous output, which is
/// stable and cheap.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ladder {
    s: [f64; 4],
    y: f64,
}

impl Ladder {
    /// `res` 0..~1.1: around 1 it starts to self-oscillate.
    pub fn tick(&mut self, x: f64, fc: f64, res: f64, sr: f64) -> f64 {
        let fc = fc.clamp(20.0, sr * 0.45);
        let g = (PI * fc / sr).tan();
        let gg = g / (1.0 + g);
        // Resonance gain, compensated a little so the passband doesn't
        // collapse at high resonance.
        let k = 4.0 * res;
        let mut u = (x * (1.0 + 0.5 * k) - k * self.y).tanh();
        for s in self.s.iter_mut() {
            let v = (u - *s) * gg;
            let out = v + *s;
            *s = out + v;
            u = out;
        }
        self.y = u;
        u
    }

    pub fn reset(&mut self) {
        *self = Ladder::default();
    }
}

/// One-pole lowpass/highpass, for smoothing and tone shaping.
#[derive(Clone, Copy, Debug, Default)]
pub struct OnePole {
    a: f64,
    z: f64,
}

impl OnePole {
    pub fn new(fc: f64, sr: f64) -> OnePole {
        OnePole { a: 1.0 - (-2.0 * PI * fc / sr).exp(), z: 0.0 }
    }

    pub fn lp(&mut self, x: f64) -> f64 {
        self.z += self.a * (x - self.z);
        self.z
    }

    pub fn hp(&mut self, x: f64) -> f64 {
        x - self.lp(x)
    }
}

// ---------------------------------------------------------------- oscillators

/// PolyBLEP correction for a discontinuity at phase 0 (`t` in 0..1, `dt` =
/// phase increment). Subtract from a naive saw; for a square, apply at both
/// edges.
pub fn poly_blep(t: f64, dt: f64) -> f64 {
    if t < dt {
        let x = t / dt;
        2.0 * x - x * x - 1.0
    } else if t > 1.0 - dt {
        let x = (t - 1.0) / dt;
        x * x + 2.0 * x + 1.0
    } else {
        0.0
    }
}

/// Band-limited sawtooth, -1..1.
pub fn saw(phase: f64, dt: f64) -> f64 {
    2.0 * phase - 1.0 - poly_blep(phase, dt)
}

/// Band-limited square (50% duty), -1..1.
pub fn square(phase: f64, dt: f64) -> f64 {
    let naive = if phase < 0.5 { 1.0 } else { -1.0 };
    naive + poly_blep(phase, dt) - poly_blep((phase + 0.5).fract(), dt)
}

// ---------------------------------------------------------------- helpers

/// Per-sample multiplier for an exponential decay with time constant `tau`
/// seconds.
pub fn decay_coef(tau: f64, sr: f64) -> f64 {
    (-1.0 / (tau * sr)).exp()
}

/// Equal-power pan gains (left, right) for `pan` in -1 (left) … 1 (right).
pub fn pan_gains(pan: f64) -> (f64, f64) {
    let a = (pan.clamp(-1.0, 1.0) + 1.0) * PI / 4.0;
    (a.cos(), a.sin())
}

/// Frequency of a MIDI note number.
pub fn midi_hz(note: f64) -> f64 {
    440.0 * 2f64.powf((note - 69.0) / 12.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_is_stable_at_high_resonance() {
        let mut f = Ladder::default();
        let mut peak: f64 = 0.0;
        for i in 0..48_000 {
            let x = if i % 200 < 100 { 0.8 } else { -0.8 };
            peak = peak.max(f.tick(x, 800.0, 1.05, 48_000.0).abs());
        }
        assert!(peak.is_finite() && peak < 2.0, "peak {peak}");
    }

    #[test]
    fn oscillators_stay_in_range() {
        let dt = 220.0 / 48_000.0;
        let mut p = 0.0;
        for _ in 0..10_000 {
            assert!(saw(p, dt).abs() <= 1.0 + 1e-9);
            assert!(square(p, dt).abs() <= 1.0 + 1e-9);
            p = (p + dt).fract();
        }
    }
}
