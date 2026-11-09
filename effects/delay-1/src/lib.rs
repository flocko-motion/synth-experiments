//! delay — tempo-synced stereo delay, built for a send bus: fully wet by
//! default. Ping-pong (repeats alternate left and right) or plain stereo.
//! Stereo in, stereo out.
//!
//! Each repeat runs through a low cut, a high cut and soft saturation in
//! the feedback loop, so echoes get thinner and darker and sit behind the
//! source; with `drive` up they smear and build like a tape delay. The
//! saturation also keeps high feedback from running away.
//!
//! The delay time follows the host tempo and glides when the tempo or the
//! `time` choice changes. Lower `mix` to use it as an insert. Like every
//! plugin, it never sees duck events: it declares a duck sensitivity and
//! the mixer ducks it.

use dsp::{soft_limit, DelayLine, Svf};
use gen_api::{Config, Context, Generator, Info, Param};

const TIME: usize = 0;
const MODE: usize = 1;
const FEEDBACK: usize = 2;
const MIX: usize = 3;
const LOWCUT: usize = 4;
const HIGHCUT: usize = 5;
const WIDTH: usize = 6;
const DRIVE: usize = 7;

const TIMES: &[&str] = &["1/16", "1/8T", "1/8", "1/8.", "1/4", "1/4.", "1/2"];
/// Beats per repeat for each `time` choice.
const TIME_BEATS: [f64; 7] = [0.25, 1.0 / 3.0, 0.5, 0.75, 1.0, 1.5, 2.0];
const MODES: &[&str] = &["ping-pong", "stereo"];

const PARAMS: [Param; 8] = [
    Param::choice("time", "Time", TIMES, 3), // dotted 8th, the psy default
    Param::choice("mode", "Mode", MODES, 0),
    Param::continuous("feedback", "Feedback", 0.0, 0.95, 0.45),
    Param::continuous("mix", "Mix", 0.0, 1.0, 1.0),
    Param::continuous("lowcut", "Low cut", 20.0, 2000.0, 300.0).log().unit("Hz"),
    Param::continuous("highcut", "High cut", 1000.0, 16000.0, 4500.0).log().unit("Hz"),
    Param::continuous("width", "Width", 0.0, 1.0, 1.0),
    Param::continuous("drive", "Drive", 0.5, 4.0, 1.0).unit("x"),
];

/// Longest delay: a half note at 40 bpm.
const MAX_SECONDS: f64 = 3.0;

pub struct Delay {
    sr: f64,
    lines: [DelayLine; 2],
    lowcut: [Svf; 2],
    highcut: [Svf; 2],
    time: f64, // samples, smoothed
    feedback: f64,
    mix: f64,
    width: f64,
    drive: f64,
    smooth: f64,
    time_smooth: f64,
}

impl Generator for Delay {
    fn new(config: &Config) -> Self {
        let sr = config.sample_rate;
        let max = (MAX_SECONDS * sr) as usize;
        let coef = |tau: f64| 1.0 - (-1.0 / (tau * sr)).exp();
        Delay {
            sr,
            lines: [DelayLine::new(max), DelayLine::new(max)],
            lowcut: [Svf::default(); 2],
            highcut: [Svf::default(); 2],
            time: 0.0,
            feedback: PARAMS[FEEDBACK].default,
            mix: PARAMS[MIX].default,
            width: PARAMS[WIDTH].default,
            drive: PARAMS[DRIVE].default,
            smooth: coef(0.03),
            time_smooth: coef(0.08),
        }
    }

    fn info(&self) -> Info {
        Info { name: "delay", description: "Tempo-synced ping-pong/stereo delay with filtered, saturated feedback (effect, stereo)", channels: 2 }
    }

    fn params(&self) -> &'static [Param] {
        &PARAMS
    }

    fn inputs(&self) -> u16 {
        2
    }

    /// Echoes clearing out on the kick; the mixer ducks the return by this
    /// much.
    fn duck_sensitivity(&self) -> f64 {
        0.5
    }

    fn category(&self) -> &'static str {
        "delay"
    }

    fn process(&mut self, ctx: &Context, out: &mut [f32]) {
        let sr = self.sr;
        let target = TIME_BEATS[ctx.param(TIME) as usize] * ctx.transport.samples_per_beat(sr);
        if self.time == 0.0 {
            self.time = target; // first block: no glide up from zero
        }
        let ping_pong = ctx.param(MODE) == 0.0;
        for k in 0..2 {
            self.lowcut[k].set(ctx.param(LOWCUT), 0.0, sr);
            self.highcut[k].set(ctx.param(HIGHCUT), 0.0, sr);
        }

        for (o, x) in out.chunks_exact_mut(2).zip(ctx.input.chunks_exact(2)) {
            let s = self.smooth;
            self.feedback += (ctx.param(FEEDBACK) - self.feedback) * s;
            self.mix += (ctx.param(MIX) - self.mix) * s;
            self.width += (ctx.param(WIDTH) - self.width) * s;
            self.drive += (ctx.param(DRIVE) - self.drive) * s;
            self.time += (target - self.time) * self.time_smooth;

            let dry = [x[0] as f64, x[1] as f64];
            let echo = [self.lines[0].read(self.time), self.lines[1].read(self.time)];
            // Everything entering the lines goes through the loop filters
            // and saturation, so each repeat is darker than the last.
            let drive = self.drive;
            let mut tone = |k: usize, v: f64| {
                let v = self.highcut[k].tick(self.lowcut[k].tick(v).2).0;
                (v * drive).tanh() / drive
            };
            let (into_l, into_r) = if ping_pong {
                // Input (mono) → left → right → left …
                (tone(0, (dry[0] + dry[1]) * 0.5 + echo[1] * self.feedback), tone(1, echo[0]))
            } else {
                (tone(0, dry[0] + echo[0] * self.feedback), tone(1, dry[1] + echo[1] * self.feedback))
            };
            self.lines[0].write(into_l);
            self.lines[1].write(into_r);

            let (mid, side) = ((echo[0] + echo[1]) * 0.5, (echo[0] - echo[1]) * 0.5 * self.width);
            let wet = [mid + side, mid - side];
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

    /// Run stereo `input` through in 512-frame blocks at 120 bpm.
    fn run(input: &[f32], params: &[f64]) -> Vec<f32> {
        let mut fx = Delay::new(&Config { sample_rate: SR, seed: 1 });
        let mut out = vec![0.0f32; input.len()];
        for (b, (o, i)) in out.chunks_mut(1024).zip(input.chunks(1024)).enumerate() {
            let ctx = Context {
                sample_rate: SR,
                transport: Transport { bpm: 120.0, beat: b as f64 * 512.0 / 24_000.0, beats_per_bar: 4, playing: true },
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

    /// A 1 ms click at frame 0, in both channels.
    fn click(frames: usize) -> Vec<f32> {
        (0..frames * 2).map(|i| if i < 96 { 0.8 } else { 0.0 }).collect()
    }

    /// Frame of the loudest sample of channel `ch` within frames lo..hi.
    fn peak_at(x: &[f32], ch: usize, lo: usize, hi: usize) -> (usize, f32) {
        (lo..hi).map(|f| (f, x[f * 2 + ch].abs())).fold((0, 0.0), |a, b| if b.1 > a.1 { b } else { a })
    }

    #[test]
    fn silence_in_silence_out() {
        assert!(run(&vec![0.0; 96_000], &defaults()).iter().all(|&x| x == 0.0));
    }

    #[test]
    fn ping_pong_repeats_on_the_dotted_eighth() {
        // 120 bpm: a dotted 8th is 0.375 s = 18 000 frames.
        let out = run(&click(60_000), &defaults());
        let (fl, pl) = peak_at(&out, 0, 1_000, 30_000);
        let (fr, pr) = peak_at(&out, 1, 1_000, 30_000);
        assert!((fl as i64 - 18_000).abs() < 200, "first echo left at {fl}");
        assert!(pl > 3.0 * pr, "first echo should be left: {pl} vs {pr}");
        let (fr2, pr2) = peak_at(&out, 1, 30_000, 45_000);
        assert!((fr2 as i64 - 36_000).abs() < 300, "second echo right at {fr2} ({fr}, {pr2})");
        let (_, pl3) = peak_at(&out, 0, 45_000, 60_000);
        assert!(pl3 < pl, "repeats decay: {pl3} vs {pl}");
    }

    #[test]
    fn stays_bounded_at_max_feedback_and_drive() {
        let mut p = defaults();
        p[FEEDBACK] = 0.95;
        p[DRIVE] = 4.0;
        p[LOWCUT] = 20.0;
        p[HIGHCUT] = 16_000.0;
        let input: Vec<f32> = (0..20 * 48_000 * 2).map(|i| if i < 48_000 { ((i as f32) * 0.05).sin() } else { 0.0 }).collect();
        let out = run(&input, &p);
        assert!(out.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
    }

    #[test]
    fn dry_passes_at_zero_mix() {
        let mut p = defaults();
        p[MIX] = 0.0;
        let input: Vec<f32> = (0..192_000).map(|i| ((i as f32) * 0.01).sin() * 0.5).collect();
        let out = run(&input, &p);
        let err = out[96_000..].iter().zip(&input[96_000..]).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
        assert!(err < 1e-4, "err {err}");
    }
}
