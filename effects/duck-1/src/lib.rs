//! duck — tempo-synced ducking, the psytrance "sidechain to the kick"
//! without needing the kick: the gain dips on every beat (or 8th, or half
//! note) from the host's grid and recovers along a curve. Stereo in, stereo
//! out.

use gen_api::{Config, Context, Generator, Info, Param};

const DEPTH: usize = 0;
const RELEASE: usize = 1;
const CURVE: usize = 2;
const RATE: usize = 3;

const PARAMS: [Param; 4] = [
    Param::continuous("depth", "Depth", 0.0, 1.0, 0.7),
    Param::continuous("release", "Release", 0.1, 1.0, 0.6).unit("of a step"),
    Param::continuous("curve", "Curve", 0.5, 4.0, 1.6),
    Param::choice("rate", "Rate", &["1/4", "1/8", "1/2"], 0),
];

/// Beats per dip for each `rate` choice.
const RATE_BEATS: [f64; 3] = [1.0, 0.5, 2.0];

pub struct Duck {
    gain: f64,
    depth: f64,
    gain_smooth: f64,
    param_smooth: f64,
}

impl Generator for Duck {
    fn new(config: &Config) -> Self {
        let coef = |tau: f64| 1.0 - (-1.0 / (tau * config.sample_rate)).exp();
        Duck { gain: 1.0, depth: PARAMS[DEPTH].default, gain_smooth: coef(0.001), param_smooth: coef(0.02) }
    }

    fn info(&self) -> Info {
        Info { name: "duck", description: "Tempo-synced ducking on the beat, like a kick sidechain (effect, stereo)", channels: 2 }
    }

    fn params(&self) -> &'static [Param] {
        &PARAMS
    }

    fn inputs(&self) -> u16 {
        2
    }

    /// An insert follows its channel's ducking; as a send effect its return
    /// shouldn't be ducked a second time on top of its own dips.
    fn duck_sensitivity(&self) -> f64 {
        0.0
    }

    fn process(&mut self, ctx: &Context, out: &mut [f32]) {
        let tr = &ctx.transport;
        let spb = tr.samples_per_beat(ctx.sample_rate);
        let step = RATE_BEATS[ctx.param(RATE) as usize];
        let release = ctx.param(RELEASE);
        let curve = ctx.param(CURVE);

        for (i, (o, x)) in out.chunks_exact_mut(2).zip(ctx.input.chunks_exact(2)).enumerate() {
            self.depth += (ctx.param(DEPTH) - self.depth) * self.param_smooth;
            let target = if tr.playing {
                // How far through the current step, 0..1, then through the dip.
                let phase = ((tr.beat + i as f64 / spb) / step).fract();
                let x = phase / release;
                let env = if x < 1.0 { (1.0 - x).powf(curve) } else { 0.0 };
                1.0 - self.depth * env
            } else {
                1.0
            };
            // A very short smoother turns the jump at each step into a fast
            // but click-free dip.
            self.gain += (target - self.gain) * self.gain_smooth;
            o[0] = (x[0] as f64 * self.gain) as f32;
            o[1] = (x[1] as f64 * self.gain) as f32;
        }
    }
}
