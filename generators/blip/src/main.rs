//! blip — the smallest useful generator: a sine pluck on some 16ths, pitched
//! from the host's key. It exists to exercise hosts; the musical decisions
//! hang off the host's grid position, so two blips with different seeds lock
//! together.
//!
//!   target/release/blip 145 8 --out blip.wav
//!   ws add blip target/release/blip --seed 3

use gen_api::{Config, Context, Generator, Info, Position, Rng, TICKS_PER_BEAT};
use std::f64::consts::TAU;

struct Blip {
    sample_rate: f64,
    /// 16-step pattern of scale degrees; None = rest. Rerolled every cycle.
    pattern: [Option<i32>; 16],
    seed: u64,
    rolled_for: Option<u64>,
    freq: f64,
    phase: f64,
    env: f64,
    last_tick: Option<u64>,
}

impl Blip {
    fn roll(&mut self, cycle: u64) {
        let mut rng = Rng::new(self.seed ^ cycle.wrapping_mul(0xA24B_AED4_963E_E407));
        let density = rng.range(0.25, 0.6);
        for (i, step) in self.pattern.iter_mut().enumerate() {
            *step = (i == 0 || rng.chance(density)).then(|| *rng.pick(&[0, 2, 4, 7, 9, 11, 14]));
        }
        self.rolled_for = Some(cycle);
    }
}

impl Generator for Blip {
    fn new(config: &Config) -> Self {
        Blip {
            sample_rate: config.sample_rate,
            pattern: [None; 16],
            seed: config.seed,
            rolled_for: None,
            freq: 440.0,
            phase: 0.0,
            env: 0.0,
            last_tick: None,
        }
    }

    fn info(&self) -> Info {
        Info { name: "blip", description: "sine plucks on the 16th grid, new pattern every cycle", channels: 1 }
    }

    fn process(&mut self, ctx: &Context, out: &mut [f32]) {
        let t = &ctx.transport;
        let spb = t.samples_per_beat(ctx.sample_rate);
        let decay = (-1.0 / (0.09 * self.sample_rate)).exp();
        for (i, s) in out.iter_mut().enumerate() {
            let beat = t.beat + i as f64 / spb;
            let tick = (beat * TICKS_PER_BEAT as f64).floor() as u64;
            // Trigger on entering a new tick; position, not an internal
            // counter, so seeks and late joins land on the grid.
            if t.playing && self.last_tick != Some(tick) {
                self.last_tick = Some(tick);
                let pos = Position::at(beat, t.beats_per_bar);
                let cycle = pos.epoch * 4 + pos.cycle as u64;
                if self.rolled_for != Some(cycle) {
                    self.roll(cycle);
                }
                if let Some(deg) = self.pattern[(tick % 16) as usize] {
                    let semis = ctx.tonality.root as i32 + ctx.tonality.degree(deg) + 60;
                    self.freq = 440.0 * 2f64.powf((semis - 69) as f64 / 12.0);
                    self.env = if pos.beat == 0 && pos.tick == 0 { 1.0 } else { 0.6 };
                }
            }
            self.phase = (self.phase + self.freq / self.sample_rate).fract();
            *s = ((TAU * self.phase).sin() * self.env * 0.3) as f32;
            self.env *= decay;
        }
    }
}

fn main() {
    gen_api::cli::run::<Blip>()
}
