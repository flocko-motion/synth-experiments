//! laserhat — superfast psy hi-hat that sounds like tiny lasers. Stereo.
//!
//! Every hit is a short FM tone (inharmonic modulator, so it's metallic
//! rather than musical) whose pitch falls fast from very high, plus a
//! highpassed noise tick: a "tsew" more than a "tss". Hits run on 1/32
//! notes by default (or 1/16, 1/16T, 1/32T), bounce left and right, lean on
//! the off-beats and duck out of the way on the beat, where the kick is.
//!
//! Structure: each cycle (4 bars) seeds a one-bar pattern of per-slot
//! draws, plays it A A A B (B a mutation), and `density` decides how many
//! slots fire, so turning it changes the pattern straight away. With
//! `rolls` up, the last beat of a cycle can ratchet at double speed with
//! the pitch climbing into the next cycle.
//!
//! It counts its own subdivisions from the host position (the shared
//! StepClock only fires on 16ths) and resyncs silently after a seek.

use dsp::{pan_gains, soft_limit, OnePole};
use gen_api::{Config, Context, Entry, Generator, Info, Param, Rng, BARS_PER_CYCLE};
use std::f64::consts::PI;

const RATE: usize = 0;
const DENSITY: usize = 1;
const PITCH: usize = 2;
const SWEEP: usize = 3;
const DECAY: usize = 4;
const METAL: usize = 5;
const NOISE: usize = 6;
const SPREAD: usize = 7;
const ROLLS: usize = 8;

const RATES: &[&str] = &["1/16", "1/16T", "1/32", "1/32T"];
/// Hits per beat for each rate.
const RATE_DIVS: [u64; 4] = [4, 6, 8, 12];
/// Longest bar, in slots: 1/32T in 4/4.
const MAX_BAR_SLOTS: usize = 48;

const PARAMS: [Param; 9] = [
    Param::choice("rate", "Rate", RATES, 2).glide(0.0),
    Param::continuous("density", "Density", 0.0, 1.0, 0.7).glide(1.0),
    // Start of the laser sweep, octaves around 9 kHz.
    Param::continuous("pitch", "Pitch", -1.5, 1.0, 0.0).unit("oct").glide(2.0),
    // How far the pitch falls: 0 a static metallic tick, 1 a full laser.
    Param::continuous("sweep", "Sweep", 0.0, 1.0, 0.6).glide(2.0),
    Param::continuous("decay", "Decay", 3.0, 60.0, 16.0).log().unit("ms").glide(2.0),
    Param::continuous("metal", "Metal", 0.0, 1.0, 0.5).glide(2.0),
    Param::continuous("noise", "Noise", 0.0, 1.0, 0.25).glide(2.0),
    Param::continuous("spread", "Spread", 0.0, 1.0, 0.6).glide(2.0),
    Param::continuous("rolls", "Rolls", 0.0, 1.0, 0.3).glide(1.0),
];

const VOICES: usize = 6;
/// Modulator : carrier ratio; irrational, so the spectrum is metallic.
const MOD_RATIO: f64 = 1.414;

// ---------------------------------------------------------------- pattern

#[derive(Clone, Copy, Default)]
struct Slot {
    draw: f64, // fires when below the density
    vel: f64,
}

struct Cycle {
    index: u64,
    bars: [[Slot; MAX_BAR_SLOTS]; 2], // A and B
    /// Fires the roll when below `rolls`.
    roll_draw: f64,
}

// ---------------------------------------------------------------- voice

#[derive(Clone, Copy, Default)]
struct Voice {
    t: f64, // seconds since the hit; > 8 decays = idle
    f0: f64,
    f1: f64,
    pc: f64,
    pm: f64,
    vel: f64,
    pan: f64,
}

pub struct Laserhat {
    sr: f64,
    rng: Rng,
    noise: Rng,
    cycle: Option<Cycle>,
    /// Last subdivision fired (in units of the current rate) and that rate.
    last: Option<(i64, u64)>,
    voices: [Voice; VOICES],
    next_voice: usize,
    side: f64, // alternates the pan
    hp: [OnePole; 2],
    noise_hp: OnePole,
    params: [f64; PARAMS.len()],
    smooth: f64,
}

impl Laserhat {
    fn compose(&mut self, index: u64) -> Cycle {
        let r = &mut self.rng;
        let mut a = [Slot::default(); MAX_BAR_SLOTS];
        for s in a.iter_mut() {
            *s = Slot { draw: r.f(), vel: r.range(0.6, 1.0) };
        }
        let mut b = a;
        for _ in 0..8 {
            let i = (r.next_u64() % MAX_BAR_SLOTS as u64) as usize;
            b[i] = Slot { draw: r.f(), vel: r.range(0.6, 1.0) };
        }
        Cycle { index, bars: [a, b], roll_draw: r.f() }
    }

    fn hit(&mut self, vel: f64, lift: f64) {
        let p = &self.params;
        let f0 = 9000.0 * (p[PITCH] + lift).exp2();
        let f1 = f0 * (-3.0 * p[SWEEP]).exp2();
        self.side = -self.side;
        let pan = self.side * p[SPREAD] * (0.6 + 0.4 * self.noise.f());
        self.voices[self.next_voice] = Voice { t: 0.0, f0, f1, pc: 0.0, pm: 0.0, vel, pan };
        self.next_voice = (self.next_voice + 1) % VOICES;
    }

    /// Hit at subdivision `n` (of `div` per beat), if the pattern says so.
    fn slot(&mut self, n: u64, div: u64, ctx: &Context) {
        let bpb = ctx.transport.beats_per_bar.max(1) as u64;
        let bar_slots = div * bpb;
        let bar = n / bar_slots;
        let cycle = bar / BARS_PER_CYCLE as u64;
        if self.cycle.as_ref().map(|c| c.index) != Some(cycle) {
            self.cycle = Some(self.compose(cycle));
        }
        let c = self.cycle.as_ref().unwrap();
        let in_bar = n % bar_slots;
        let pos = in_bar % div; // position within the beat
        let last_bar = bar % BARS_PER_CYCLE as u64 == BARS_PER_CYCLE as u64 - 1;
        let roll = last_bar && in_bar / div == bpb - 1 && c.roll_draw < ctx.param(ROLLS);
        let slot = c.bars[last_bar as usize][(in_bar as usize * MAX_BAR_SLOTS / bar_slots as usize).min(MAX_BAR_SLOTS - 1)];

        if roll {
            // Every slot fires, climbing in pitch towards the next cycle.
            let u = pos as f64 / div as f64;
            self.hit(0.7 + 0.3 * u, 0.5 * u);
            return;
        }
        // On the beat: thin it out, the kick lives there.
        let on_beat = pos == 0;
        let off_eighth = pos * 2 == div;
        let density = ctx.param(DENSITY) * if on_beat { 0.4 } else { 1.0 };
        if off_eighth || slot.draw < density {
            let vel = if off_eighth { 1.0 } else if on_beat { 0.45 * slot.vel } else { 0.75 * slot.vel };
            self.hit(vel, 0.0);
        }
    }
}

impl Generator for Laserhat {
    fn new(config: &Config) -> Self {
        let sr = config.sample_rate;
        Laserhat {
            sr,
            rng: Rng::new(config.seed ^ 0x1A5E),
            noise: Rng::new(config.seed ^ 0x7177),
            cycle: None,
            last: None,
            voices: [Voice { t: 1.0, ..Voice::default() }; VOICES],
            next_voice: 0,
            side: 1.0,
            hp: [OnePole::new(2500.0, sr); 2],
            noise_hp: OnePole::new(6000.0, sr),
            params: PARAMS.map(|p| p.default),
            smooth: 1.0 - (-1.0 / (0.02 * sr)).exp(),
        }
    }

    fn info(&self) -> Info {
        Info { name: "laserhat", description: "Superfast psy hi-hat of tiny laser zaps on 1/32s, with rolls (stereo)", channels: 2 }
    }

    fn params(&self) -> &'static [Param] {
        &PARAMS
    }

    fn category(&self) -> &'static str {
        "drums"
    }

    fn entry(&self) -> Entry {
        Entry::Bar
    }

    fn duck_sensitivity(&self) -> f64 {
        0.2
    }

    fn process(&mut self, ctx: &Context, out: &mut [f32]) {
        let sr = self.sr;
        let dt = 1.0 / sr;
        let tr = &ctx.transport;
        let spb = tr.samples_per_beat(sr);
        let div = RATE_DIVS[ctx.param(RATE) as usize];
        // Rolls run at double speed, so count at twice the rate and let
        // `slot` see every other subdivision unless rolling.
        let fine = div * 2;

        for (i, frame) in out.chunks_exact_mut(2).enumerate() {
            let n = ((tr.beat + i as f64 / spb).max(0.0) * fine as f64 + 1e-9).floor() as i64;
            match self.last {
                Some((last, d)) if d == fine && n == last + 1 => {
                    self.last = Some((n, fine));
                    if tr.playing {
                        let roll_pos = n as u64 % 2 == 1;
                        if !roll_pos {
                            self.slot(n as u64 / 2, div, ctx);
                        } else if self.rolling(n as u64 / 2, div, ctx) {
                            let u = ((n as u64 / 2) % div) as f64 / div as f64;
                            self.hit(0.6 + 0.3 * u, 0.5 * u + 0.05);
                        }
                    }
                }
                Some((last, d)) if d == fine && n == last => {}
                // First block, seek, loop or rate change: resync silently.
                _ => self.last = Some((n, fine)),
            }

            for (k, v) in self.params.iter_mut().enumerate() {
                if k != RATE {
                    *v += (ctx.param(k) - *v) * self.smooth;
                }
            }
            let p = self.params;
            let decay = p[DECAY] / 1000.0;
            let index = 0.5 + 4.5 * p[METAL];
            let (mut l, mut r) = (0.0, 0.0);
            // One noise source for the ticks of all voices.
            let noise = self.noise_hp.hp(self.noise.f() * 2.0 - 1.0) * p[NOISE] * 2.0;
            for v in self.voices.iter_mut() {
                if v.t > decay * 8.0 {
                    continue;
                }
                let f = v.f1 + (v.f0 - v.f1) * (-v.t / (decay * 0.45)).exp();
                v.pm = (v.pm + f * MOD_RATIO * dt).fract();
                v.pc = (v.pc + f * dt).fract();
                let tone = (2.0 * PI * v.pc + index * (2.0 * PI * v.pm).sin()).sin();
                let tick = noise * (-v.t / 0.0015).exp();
                let env = (v.t / 0.0003).min(1.0) * (-v.t / decay).exp();
                let x = (tone * env + tick) * v.vel * 0.45;
                let (gl, gr) = pan_gains(v.pan);
                l += x * gl;
                r += x * gr;
                v.t += dt;
            }
            frame[0] = soft_limit(self.hp[0].hp(l)) as f32;
            frame[1] = soft_limit(self.hp[1].hp(r)) as f32;
        }
    }
}

impl Laserhat {
    /// Whether subdivision `n` is in a roll (last beat of a cycle, when the
    /// cycle's draw is under `rolls`).
    fn rolling(&mut self, n: u64, div: u64, ctx: &Context) -> bool {
        let bpb = ctx.transport.beats_per_bar.max(1) as u64;
        let bar_slots = div * bpb;
        let bar = n / bar_slots;
        let last_bar = bar % BARS_PER_CYCLE as u64 == BARS_PER_CYCLE as u64 - 1;
        let last_beat = (n % bar_slots) / div == bpb - 1;
        let cycle = bar / BARS_PER_CYCLE as u64;
        last_bar && last_beat && self.cycle.as_ref().is_some_and(|c| c.index == cycle && c.roll_draw < ctx.param(ROLLS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gen_api::{Scale, Tonality, Transport};

    const SR: f64 = 48_000.0;
    const BEAT: usize = 24_000; // 120 bpm

    /// Render; also returns how many hits fired.
    fn render(bars: usize, params: &[f64], playing: bool) -> (Vec<f32>, usize) {
        let mut g = Laserhat::new(&Config { sample_rate: SR, seed: 8 });
        let block = 480;
        let mut out = vec![0.0f32; bars * 4 * BEAT * 2];
        let mut hits = 0;
        for (b, chunk) in out.chunks_mut(block * 2).enumerate() {
            let ctx = Context {
                sample_rate: SR,
                transport: Transport { bpm: 120.0, beat: (b * block) as f64 / BEAT as f64, beats_per_bar: 4, playing },
                tonality: Tonality { root: 9, scale: Scale::Phrygian },
                params,
                input: &[],
            };
            let before = g.next_voice;
            g.process(&ctx, chunk);
            // Count hits by voice-ring advance (VOICES never wraps within
            // one 10 ms block at these rates).
            hits += (g.next_voice + VOICES - before) % VOICES;
        }
        (out, hits)
    }

    fn defaults() -> Vec<f64> {
        PARAMS.iter().map(|p| p.default).collect()
    }

    #[test]
    fn deterministic_and_in_range() {
        let (a, _) = render(4, &defaults(), true);
        assert_eq!(a, render(4, &defaults(), true).0);
        assert!(a.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
        assert!(a.iter().fold(0.0f32, |m, v| m.max(v.abs())) > 0.05);
    }

    #[test]
    fn density_sets_how_many_hits() {
        let mut p = defaults();
        p[ROLLS] = 0.0;
        p[DENSITY] = 0.0;
        let (_, sparse) = render(4, &p, true);
        p[DENSITY] = 1.0;
        let (_, full) = render(4, &p, true);
        // Density 0 still plays the off-8th of every beat: 16 beats.
        assert_eq!(sparse, 16);
        // Density 1: every off-beat 32nd plus 40% of the on-beat ones.
        assert!(full >= 16 * 7 && full <= 16 * 8, "full {full}");
    }

    #[test]
    fn rolls_add_hits_on_the_last_beat() {
        let mut p = defaults();
        p[DENSITY] = 0.0;
        p[ROLLS] = 0.0;
        let (_, plain) = render(8, &p, true);
        p[ROLLS] = 1.0;
        let (_, rolled) = render(8, &p, true);
        // Two cycles; each roll fills its beat at 1/64: 16 hits instead of 1.
        assert_eq!(rolled, plain + 2 * 15);
    }

    #[test]
    fn silent_when_stopped() {
        assert!(render(2, &defaults(), false).0.iter().all(|&x| x == 0.0));
    }
}
