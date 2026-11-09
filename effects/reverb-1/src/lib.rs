//! reverb — stereo reverb effect, built for a send bus: fully wet by
//! default, with a low cut so kick and bass stay out of the tail. Lower
//! `mix` to use it as an insert. Stereo in, stereo out.
//!
//! Signal path: low cut → predelay → [`dsp::Reverb`] (allpass diffusion
//! into a 4-line FDN) → width → dry/wet mix.
//!
//! It never sees duck events: it declares a duck sensitivity and the mixer
//! ducks it like any other plugin.

use dsp::{soft_limit, DelayLine, Reverb, Svf};
use gen_api::{Config, Context, Generator, Info, Param};

const MIX: usize = 0;
const DECAY: usize = 1;
const DAMP: usize = 2;
const LOWCUT: usize = 3;
const PREDELAY: usize = 4;
const WIDTH: usize = 5;

const PARAMS: [Param; 6] = [
    Param::continuous("mix", "Mix", 0.0, 1.0, 1.0),
    Param::continuous("decay", "Decay", 0.3, 12.0, 2.5).log().unit("s").glide(2.0),
    Param::continuous("damp", "Damping", 1000.0, 16000.0, 6000.0).log().unit("Hz").glide(2.0),
    Param::continuous("lowcut", "Low cut", 20.0, 1000.0, 200.0).log().unit("Hz"),
    Param::continuous("predelay", "Predelay", 0.0, 250.0, 20.0).unit("ms").glide(0.0),
    Param::continuous("width", "Width", 0.0, 1.0, 1.0).glide(2.0),
];

const MAX_PREDELAY_MS: f64 = 260.0;

/// Wet level, so a send at unity comes back at about the input's loudness.
const WET_GAIN: f64 = 2.2;

pub struct ReverbFx {
    sr: f64,
    reverb: Reverb,
    lowcut: [Svf; 2],
    pre: [DelayLine; 2],
    /// Settings the reverb was last set to, so it's only recomputed on change.
    set_to: (f64, f64),
    predelay: f64, // samples, smoothed
    mix: f64,
    width: f64,
    smooth: f64,
}

impl Generator for ReverbFx {
    fn new(config: &Config) -> Self {
        let sr = config.sample_rate;
        let n = (MAX_PREDELAY_MS * sr / 1000.0) as usize;
        let mut reverb = Reverb::new(sr);
        let set_to = (PARAMS[DECAY].default, PARAMS[DAMP].default);
        reverb.set(set_to.0, set_to.1, sr);
        ReverbFx {
            sr,
            reverb,
            lowcut: [Svf::default(); 2],
            pre: [DelayLine::new(n), DelayLine::new(n)],
            set_to,
            predelay: PARAMS[PREDELAY].default * sr / 1000.0,
            mix: PARAMS[MIX].default,
            width: PARAMS[WIDTH].default,
            smooth: 1.0 - (-1.0 / (0.03 * sr)).exp(),
        }
    }

    fn info(&self) -> Info {
        Info { name: "reverb", description: "Stereo FDN reverb with low cut and predelay, fully wet for a send (effect, stereo)", channels: 2 }
    }

    fn params(&self) -> &'static [Param] {
        &PARAMS
    }

    fn inputs(&self) -> u16 {
        2
    }

    /// A reverb return breathing with the kick is part of the sound; the
    /// mixer ducks it by this much.
    fn duck_sensitivity(&self) -> f64 {
        0.6
    }

    fn category(&self) -> &'static str {
        "reverb"
    }

    fn process(&mut self, ctx: &Context, out: &mut [f32]) {
        let sr = self.sr;
        let settings = (ctx.param(DECAY), ctx.param(DAMP));
        if settings != self.set_to {
            self.reverb.set(settings.0, settings.1, sr);
            self.set_to = settings;
        }
        for f in self.lowcut.iter_mut() {
            f.set(ctx.param(LOWCUT), 0.0, sr);
        }
        let predelay_target = ctx.param(PREDELAY) * sr / 1000.0;

        for (o, x) in out.chunks_exact_mut(2).zip(ctx.input.chunks_exact(2)) {
            self.mix += (ctx.param(MIX) - self.mix) * self.smooth;
            self.width += (ctx.param(WIDTH) - self.width) * self.smooth;
            self.predelay += (predelay_target - self.predelay) * self.smooth;

            let dry = [x[0] as f64, x[1] as f64];
            let mut send = [0.0; 2];
            for k in 0..2 {
                // Read first: at 0 ms predelay the send is one sample late.
                send[k] = self.pre[k].read(self.predelay);
                self.pre[k].write(self.lowcut[k].tick(dry[k]).2);
            }

            let (l, r) = self.reverb.tick(send[0], send[1]);
            let (mid, side) = ((l + r) * 0.5, (l - r) * 0.5 * self.width);
            let wet = [(mid + side) * WET_GAIN, (mid - side) * WET_GAIN];
            for k in 0..2 {
                o[k] = soft_limit(dry[k] * (1.0 - self.mix) + wet[k] * self.mix) as f32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gen_api::{Scale, Tonality, Transport};

    const SR: f64 = 48_000.0;

    /// Run `input` (stereo, interleaved) through in 512-frame blocks.
    fn run(input: &[f32], params: &[f64]) -> Vec<f32> {
        let mut fx = ReverbFx::new(&Config { sample_rate: SR, seed: 1 });
        let mut out = vec![0.0f32; input.len()];
        for (b, (o, i)) in out.chunks_mut(1024).zip(input.chunks(1024)).enumerate() {
            let ctx = Context {
                score: &gen_api::Score::EMPTY,
                sample_rate: SR,
                transport: Transport { bpm: 145.0, beat: b as f64 * 512.0 / (SR * 60.0 / 145.0), beats_per_bar: 4, playing: true },
                tonality: Tonality { root: 0, scale: Scale::Minor },
                params,
                input: i,
            };
            fx.process(&ctx, o);
        }
        out
    }

    fn defaults() -> Vec<f64> {
        PARAMS.iter().map(|p| p.default).collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn silence_in_silence_out() {
        assert!(run(&vec![0.0; 48_000], &defaults()).iter().all(|&x| x == 0.0));
    }

    #[test]
    fn dry_passes_at_zero_mix() {
        let mut p = defaults();
        p[MIX] = 0.0;
        let input: Vec<f32> = (0..192_000).map(|i| ((i as f32) * 0.01).sin() * 0.5).collect();
        let out = run(&input, &p);
        // Once the mix smoother has settled, output is the input.
        let err = out[96_000..].iter().zip(&input[96_000..]).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
        assert!(err < 1e-4, "err {err}");
    }

    #[test]
    fn wet_level_tracks_input_and_stays_in_range() {
        // Noise burst for 2 s, then silence for 4 s.
        let mut s = 1u32;
        let input: Vec<f32> = (0..6 * 48_000 * 2)
            .map(|i| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                if i < 2 * 48_000 * 2 { ((s >> 8) as f32 / (1 << 24) as f32 * 2.0 - 1.0) * 0.3 } else { 0.0 }
            })
            .collect();
        let out = run(&input, &defaults());
        assert!(out.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
        let burst = 2 * 48_000 * 2;
        let ratio = rms(&out[burst / 2..burst]) / rms(&input[burst / 2..burst]);
        eprintln!("wet/dry rms ratio {ratio:.3}");
        assert!((0.25..4.0).contains(&ratio), "ratio {ratio}");
        // Tail decays: the last second is much quieter than the one after the burst.
        let first = rms(&out[burst..burst + 96_000]);
        let last = rms(&out[out.len() - 96_000..]);
        assert!(last < first * 0.05, "first {first} last {last}");
    }
}
