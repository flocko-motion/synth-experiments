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

    /// `fc` is clamped to 5 Hz … 0.45 × `sr`.
    pub fn set(&mut self, fc: f64, res: f64, sr: f64) {
        let fc = fc.clamp(5.0, sr * 0.45);
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

// ---------------------------------------------------------------- delay line

/// A delay line with fractional (linearly interpolated) reads, so delay
/// times can glide. Allocated in [`DelayLine::new`].
#[derive(Clone, Debug)]
pub struct DelayLine {
    buf: Vec<f64>,
    w: usize,
}

impl DelayLine {
    /// Room for delays up to `max` samples.
    pub fn new(max: usize) -> DelayLine {
        DelayLine { buf: vec![0.0; max + 2], w: 0 }
    }

    /// The sample written `delay` samples ago, clamped to 1..max. Read
    /// before [`DelayLine::write`] for the current sample.
    pub fn read(&self, delay: f64) -> f64 {
        let n = self.buf.len();
        let delay = delay.clamp(1.0, n as f64 - 2.0);
        let pos = self.w as f64 + n as f64 - delay;
        let i0 = pos.floor() as usize % n;
        let frac = pos.fract();
        self.buf[i0] * (1.0 - frac) + self.buf[(i0 + 1) % n] * frac
    }

    pub fn write(&mut self, x: f64) {
        self.buf[self.w] = x;
        self.w = (self.w + 1) % self.buf.len();
    }
}

// ---------------------------------------------------------------- sub clock

/// Finds subdivision boundaries at any rate per beat (16ths, triplets,
/// 32nds, …), sample by sample, for generators that need steps finer or
/// odder than `gen_api::StepClock`'s 16ths, or a rate that changes with the
/// score's feel.
///
/// Call [`SubClock::tick`] once per sample, in order, with that sample's
/// musical position. It returns the subdivision index when that sample is
/// the first of a subdivision. Blocks that don't follow on (first block,
/// seek, loop) and rate changes resync silently, except when the sample
/// sits exactly on a boundary, so an entry on a bar line, or a feel change
/// there, keeps its downbeat. While stopped nothing fires.
#[derive(Clone, Copy, Debug, Default)]
pub struct SubClock {
    /// Last subdivision seen, and the rate it was counted at.
    last: Option<(i64, u64)>,
}

impl SubClock {
    pub fn new() -> SubClock {
        SubClock::default()
    }

    /// `beat`: this sample's position in quarter notes; `per_beat`:
    /// subdivisions per beat (4 = 16ths, 6 = 16th triplets, 8 = 32nds);
    /// `samples_per_beat`: from the tempo. Returns the index of the
    /// subdivision starting here, counted from 0 at beat 0.
    pub fn tick(&mut self, beat: f64, per_beat: u64, samples_per_beat: f64, playing: bool) -> Option<u64> {
        let per_beat = per_beat.max(1);
        let pos = beat.max(0.0) * per_beat as f64;
        let n = (pos + 1e-9).floor() as i64;
        let due = match self.last {
            Some((last, rate)) if rate == per_beat && n == last + 1 => true,
            Some((last, rate)) if rate == per_beat && n == last => false,
            // Resync: fire only when this sample is the boundary itself.
            _ => (pos - n as f64) * samples_per_beat / (per_beat as f64) < 1.0,
        };
        self.last = Some((n, per_beat));
        (due && playing).then_some(n as u64)
    }
}

// ---------------------------------------------------------------- reverb

/// Schroeder allpass: smears transients without colouring the spectrum.
#[derive(Clone, Debug)]
struct Allpass {
    buf: Vec<f64>,
    i: usize,
    g: f64,
}

impl Allpass {
    fn new(len: usize, g: f64) -> Allpass {
        Allpass { buf: vec![0.0; len.max(1)], i: 0, g }
    }

    fn tick(&mut self, x: f64) -> f64 {
        let d = self.buf[self.i];
        let v = x + self.g * d;
        self.buf[self.i] = v;
        self.i = (self.i + 1) % self.buf.len();
        d - self.g * v
    }
}

const FDN_MS: [f64; 4] = [89.3, 107.9, 127.1, 149.7];
const DIFFUSE_MS: [f64; 4] = [4.7, 7.3, 11.3, 16.9];

/// Stereo reverb for pads and tails: input diffusion (allpasses) into a
/// 4-line feedback delay network with a Householder matrix and damping in
/// the loop. Returns the wet signal only. Buffers are allocated in
/// [`Reverb::new`]; `tick` doesn't allocate.
#[derive(Clone, Debug)]
pub struct Reverb {
    diffuse: [Vec<Allpass>; 2],
    lines: Vec<Vec<f64>>,
    w: usize,
    gains: [f64; 4],
    damp: [OnePole; 4],
}

impl Reverb {
    pub fn new(sr: f64) -> Reverb {
        let ms = |m: f64| (m * sr / 1000.0) as usize;
        // Slightly different diffusers per side decorrelate left and right.
        let side = |k: f64| DIFFUSE_MS.iter().map(|&m| Allpass::new(ms(m * k), 0.6)).collect();
        let mut r = Reverb {
            diffuse: [side(1.0), side(1.13)],
            lines: FDN_MS.iter().map(|&m| vec![0.0; ms(m)]).collect(),
            w: 0,
            gains: [0.0; 4],
            damp: [OnePole::default(); 4],
        };
        r.set(3.0, 6000.0, sr);
        r
    }

    /// `t60`: seconds for the tail to fall by 60 dB; `damp_hz`: lowpass in
    /// the loop, lower is darker.
    pub fn set(&mut self, t60: f64, damp_hz: f64, sr: f64) {
        for (k, line) in self.lines.iter().enumerate() {
            let secs = line.len() as f64 / sr;
            self.gains[k] = 10f64.powf(-3.0 * secs / t60.max(0.05));
            let z = self.damp[k].z;
            self.damp[k] = OnePole::new(damp_hz, sr);
            self.damp[k].z = z;
        }
    }

    pub fn tick(&mut self, l: f64, r: f64) -> (f64, f64) {
        let l = self.diffuse[0].iter_mut().fold(l, |x, a| a.tick(x));
        let r = self.diffuse[1].iter_mut().fold(r, |x, a| a.tick(x));
        let mut o = [0.0; 4];
        for (k, line) in self.lines.iter().enumerate() {
            o[k] = self.damp[k].lp(line[self.w % line.len()]);
        }
        let s = o.iter().sum::<f64>() * 0.5; // Householder: I - 2/N · 11ᵀ
        let input = [l, r, l, r];
        for (k, line) in self.lines.iter_mut().enumerate() {
            let n = line.len();
            line[self.w % n] = (o[k] - s) * self.gains[k] + input[k] * 0.5;
        }
        self.w = self.w.wrapping_add(1);
        (o[0] - o[2] * 0.6 + o[3] * 0.3, o[1] - o[3] * 0.6 + o[2] * 0.3)
    }
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

/// Linear up to 0.8, then bends smoothly towards ±1: keeps output in range
/// without colouring normal levels.
pub fn soft_limit(x: f64) -> f64 {
    let a = x.abs();
    if a <= 0.8 {
        x
    } else {
        x.signum() * (0.8 + 0.2 * ((a - 0.8) / 0.2).tanh())
    }
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
    fn reverb_tail_decays() {
        let sr = 48_000.0;
        let mut rv = Reverb::new(sr);
        rv.set(1.0, 8000.0, sr);
        let energy = |rv: &mut Reverb, n: usize, x: f64| {
            (0..n).map(|i| { let (l, r) = rv.tick(if i == 0 { x } else { 0.0 }, 0.0); l * l + r * r }).sum::<f64>()
        };
        let early = energy(&mut rv, 24_000, 1.0);
        let late = energy(&mut rv, 24_000, 0.0);
        let later = energy(&mut rv, 96_000, 0.0);
        assert!(early > 0.0 && early.is_finite());
        assert!(late < early * 0.1, "early {early} late {late}");
        assert!(later < late, "late {late} later {later}");
    }

    /// Indices fired over `frames` samples from `start_beat`, at 120 bpm
    /// (24 000 samples per beat), with the rate given per sample.
    fn sub_run(clock: &mut SubClock, start_beat: f64, frames: usize, rate: impl Fn(f64) -> u64) -> Vec<(usize, u64)> {
        let spb = 24_000.0;
        (0..frames)
            .filter_map(|i| {
                let beat = start_beat + i as f64 / spb;
                clock.tick(beat, rate(beat), spb, true).map(|n| (i, n))
            })
            .collect()
    }

    #[test]
    fn sub_clock_fires_every_subdivision_once() {
        let mut c = SubClock::new();
        let got = sub_run(&mut c, 0.0, 48_000, |_| 6); // two beats of 16th triplets
        assert_eq!(got.iter().map(|g| g.1).collect::<Vec<_>>(), (0..12).collect::<Vec<_>>());
        assert_eq!(got[1].0, 4_000, "second triplet 16th a sixth of a beat in");
    }

    #[test]
    fn sub_clock_resyncs_silently_off_the_grid_but_keeps_exact_boundaries() {
        let mut c = SubClock::new();
        assert!(sub_run(&mut c, 0.1, 100, |_| 4).is_empty(), "joined mid-16th: nothing");
        let mut c = SubClock::new();
        assert_eq!(sub_run(&mut c, 2.0, 1, |_| 4), vec![(0, 8)], "joined on a beat: fires");
        let mut c = SubClock::new();
        let mut stopped = Vec::new();
        for i in 0..24_000 {
            if let Some(n) = c.tick(i as f64 / 24_000.0, 4, 24_000.0, false) {
                stopped.push(n);
            }
        }
        assert!(stopped.is_empty(), "stopped: nothing");
    }

    #[test]
    fn sub_clock_follows_a_rate_change_on_the_beat() {
        // 16ths for beat 1, triplet 16ths from beat 2: beat 2's downbeat
        // fires once, as triplet index 6, and the grid continues from there.
        let mut c = SubClock::new();
        let got = sub_run(&mut c, 0.0, 48_000, |b| if b < 1.0 { 4 } else { 6 });
        assert_eq!(got.iter().map(|g| g.1).collect::<Vec<_>>(), vec![0, 1, 2, 3, 6, 7, 8, 9, 10, 11]);
        assert_eq!(got[4].0, 24_000);
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
