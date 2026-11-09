//! filter — DJ-style sweep filter on one knob, for builds and breakdowns.
//! Stereo in, stereo out; meant as an insert.
//!
//! `filter` is bipolar: below the centre a lowpass closes (towards 30 Hz at
//! -1), above it a highpass opens (towards 12 kHz at +1), and at 0 both sit
//! outside the audio band, so the signal passes untouched. The two run in
//! series all the time, so sweeping through the centre never switches
//! anything. Resonance fades in as the knob leaves the centre, so a neutral
//! filter never rings.
//!
//! Randomize slides the filter over 4 bars: a ready-made build or
//! breakdown.

use dsp::{soft_limit, Svf};
use gen_api::{Config, Context, Generator, Info, Param};

const FILTER: usize = 0;
const RESONANCE: usize = 1;
const SLOPE: usize = 2;
const DRIVE: usize = 3;

const SLOPES: &[&str] = &["12 dB", "24 dB"];

const PARAMS: [Param; 4] = [
    // -1 lowpass closed … 0 open … +1 highpass closed.
    Param::continuous("filter", "Filter", -1.0, 1.0, 0.0).glide(4.0),
    Param::continuous("resonance", "Resonance", 0.0, 1.0, 0.3).glide(2.0),
    Param::choice("slope", "Slope", SLOPES, 1).glide(0.0),
    // 0 = clean (bit-exact bypass of the saturation), 1 = heavy.
    Param::continuous("drive", "Drive", 0.0, 1.0, 0.0),
];

/// Lowpass range: open at 0, closed at -1.
const LP_OPEN: f64 = 20_000.0;
const LP_CLOSED: f64 = 30.0;
/// Highpass range: open at 0, closed at +1.
const HP_OPEN: f64 = 8.0; // well below the kick, so the open filter keeps its sub
const HP_CLOSED: f64 = 12_000.0;
/// How far from the centre resonance reaches full strength.
const RES_RAMP: f64 = 0.15;
/// Svf resonance for a Butterworth (maximally flat) response; the knob adds
/// on top of it. Below this, the stages droop well above their cutoff.
const FLAT: f64 = 1.0 - std::f64::consts::FRAC_1_SQRT_2;

/// Cutoffs and resonances (lowpass, highpass) for a knob position.
fn settings(x: f64, res: f64) -> (f64, f64, f64, f64) {
    let ramp = (x.abs() / RES_RAMP).min(1.0);
    let lp = if x < 0.0 { LP_OPEN * (LP_CLOSED / LP_OPEN).powf(-x) } else { LP_OPEN };
    let hp = if x > 0.0 { HP_OPEN * (HP_CLOSED / HP_OPEN).powf(x) } else { HP_OPEN };
    // Resonance on the active side only; the other side stays flat.
    let peak = FLAT + (0.95 - FLAT) * res * ramp;
    let (lp_res, hp_res) = if x < 0.0 { (peak, FLAT) } else { (FLAT, peak) };
    (lp, lp_res, hp, hp_res)
}

/// Blend towards a normalised tanh as `amount` goes 0 → 1; exactly `y` at
/// 0, so a clean filter leaves loud peaks alone.
fn saturate(y: f64, amount: f64) -> f64 {
    if amount <= 0.0 {
        return y;
    }
    let g = 1.0 + 5.0 * amount;
    y + amount * ((g * y).tanh() / g.tanh() - y)
}

/// One channel: two lowpass and two highpass stages; the 12 dB slope uses
/// one of each.
#[derive(Default, Clone, Copy)]
struct Channel {
    lp: [Svf; 2],
    hp: [Svf; 2],
}

pub struct Filter {
    sr: f64,
    ch: [Channel; 2],
    x: f64,
    res: f64,
    drive: f64,
    smooth: f64,
}

impl Generator for Filter {
    fn new(config: &Config) -> Self {
        let sr = config.sample_rate;
        Filter {
            sr,
            ch: [Channel::default(); 2],
            x: 0.0,
            res: PARAMS[RESONANCE].default,
            drive: PARAMS[DRIVE].default,
            smooth: 1.0 - (-1.0 / (0.02 * sr)).exp(),
        }
    }

    fn info(&self) -> Info {
        Info { name: "filter", description: "DJ sweep filter on one knob: lowpass below the centre, highpass above (effect, stereo)", channels: 2 }
    }

    fn params(&self) -> &'static [Param] {
        &PARAMS
    }

    fn inputs(&self) -> u16 {
        2
    }

    fn category(&self) -> &'static str {
        "filter"
    }

    fn process(&mut self, ctx: &Context, out: &mut [f32]) {
        let sr = self.sr;
        let steep = ctx.param(SLOPE) != 0.0;
        // At 24 dB each stage gets a gentler resonance, so the peak is about
        // as tall as with one stage.
        let stage_res = if steep { 0.7 } else { 1.0 };
        for (o, x) in out.chunks_exact_mut(2).zip(ctx.input.chunks_exact(2)) {
            let s = self.smooth;
            self.x += (ctx.param(FILTER) - self.x) * s;
            self.res += (ctx.param(RESONANCE) - self.res) * s;
            self.drive += (ctx.param(DRIVE) - self.drive) * s;
            let (lp, lp_res, hp, hp_res) = settings(self.x, self.res * stage_res);
            let drive = self.drive;
            for (k, c) in self.ch.iter_mut().enumerate() {
                let mut y = x[k] as f64;
                let n = if steep { 2 } else { 1 };
                for st in 0..n {
                    y = c.lp[st].lp(y, lp, lp_res, sr);
                }
                for st in 0..n {
                    c.hp[st].set(hp, hp_res, sr);
                    y = c.hp[st].tick(y).2;
                }
                o[k] = soft_limit(saturate(y, drive)) as f32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gen_api::{Scale, Tonality, Transport};

    const SR: f64 = 48_000.0;

    /// RMS gain of a sine at `hz` through the filter, measured after it
    /// settles.
    fn gain(hz: f64, params: &[f64]) -> f64 {
        let mut f = Filter::new(&Config { sample_rate: SR, seed: 0 });
        let frames = 48_000;
        let input: Vec<f32> = (0..frames).flat_map(|i| {
            let v = (2.0 * std::f64::consts::PI * hz * i as f64 / SR).sin() as f32 * 0.25;
            [v, v]
        }).collect();
        let mut out = vec![0.0f32; input.len()];
        for (o, i) in out.chunks_mut(1024).zip(input.chunks(1024)) {
            let ctx = Context {
                score: &gen_api::Score::EMPTY,
                sample_rate: SR,
                transport: Transport { bpm: 145.0, beat: 0.0, beats_per_bar: 4, playing: true },
                tonality: Tonality { root: 0, scale: Scale::Minor },
                params,
                input: i,
            };
            f.process(&ctx, o);
        }
        let rms = |x: &[f32]| (x.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
        let half = input.len() / 2;
        rms(&out[half..]) / rms(&input[half..])
    }

    fn with(filter: f64) -> Vec<f64> {
        let mut p: Vec<f64> = PARAMS.iter().map(|p| p.default).collect();
        p[FILTER] = filter;
        p
    }

    #[test]
    fn centre_is_transparent() {
        for hz in [40.0, 1000.0, 10_000.0] {
            let g = gain(hz, &with(0.0));
            assert!((g - 1.0).abs() < 0.05, "{hz} Hz: gain {g}");
        }
    }

    #[test]
    fn lowpass_below_highpass_above() {
        assert!(gain(5000.0, &with(-0.7)) < 0.05);
        assert!(gain(60.0, &with(-0.7)) > 0.7);
        assert!(gain(100.0, &with(0.7)) < 0.05);
        assert!(gain(8000.0, &with(0.7)) > 0.7);
    }

    #[test]
    fn stays_in_range_at_full_resonance_and_drive() {
        let mut p = with(-0.5);
        p[RESONANCE] = 1.0;
        p[DRIVE] = 1.0;
        for slope in [0.0, 1.0] {
            p[SLOPE] = slope;
            // Input is a 0.25 sine (RMS 0.177): output within ±1 caps the
            // RMS gain at 1 / 0.177.
            let g = gain(300.0, &p);
            assert!(g.is_finite() && g <= 1.0 / (0.25 / 2f64.sqrt()), "gain {g}");
        }
    }
}
