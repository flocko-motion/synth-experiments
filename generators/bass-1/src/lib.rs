//! bass — psytrance rolling bass on its own, tuned to the host key. Mono.
//!
//! psybass's bass line without the kick: notes fill the 16ths between the
//! beats and never land on a beat, so it locks with `kick` (or any
//! four-on-the-floor kick) at any seeds. psybass is the combined kick + bass.
//!
//! Structure, in 8-bar phrases on the host grid: a pattern (rolling,
//! gallop, …), a 2-bar motif that's mostly the root with some spice notes,
//! and a filter that drifts from one phrase's cutoff to the next. With
//! `fills` on: the last two beats of every 16th bar are a rising run (the
//! same grid rule `kick` drops out on), and some phrases end with a small
//! fill on the last beat.
//!
//! Notes are stored as scale degrees and every param applies on the next
//! note, so key changes and knob moves are heard right away.

use dsp::{midi_hz, saw, Svf};
use gen_api::{Config, Context, Entry, Generator, Info, Param, Rng, StepClock, Tonality, TICKS_PER_BEAT};

const PHRASE_BARS: u64 = 8;
const MOTIF_SLOTS: usize = 32;

const PATTERN: usize = 0;
const CUTOFF: usize = 1;
const RESONANCE: usize = 2;
const ENV: usize = 3;
const LENGTH: usize = 4;
const SPICE: usize = 5;
const DRIVE: usize = 6;
const FILLS: usize = 7;

/// "auto" lets each phrase pick; the rest fix the pattern.
const PATTERN_NAMES: &[&str] = &["auto", "rolling", "gallop", "skip", "triplet", "offbeat"];

const PARAMS: [Param; 8] = [
    Param::choice("pattern", "Pattern", PATTERN_NAMES, 0).glide(0.0),
    Param::continuous("cutoff", "Cutoff offset", -2.0, 2.0, 0.0).unit("oct").glide(2.0),
    Param::continuous("resonance", "Resonance offset", -0.3, 0.35, 0.0),
    // Filter envelope depth, times the phrase's own.
    Param::continuous("env", "Envelope", 0.0, 2.0, 1.0).unit("x"),
    // Note length, times the phrase's own.
    Param::continuous("length", "Length", 0.5, 1.3, 1.0).unit("x"),
    // How often the motif leaves the root, times the phrase's own.
    Param::continuous("spice", "Spice", 0.0, 3.0, 1.0).unit("x"),
    Param::continuous("drive", "Drive", 0.5, 4.0, 1.25).unit("x"),
    Param::toggle("fills", "Fills", true).glide(0.0),
];

/// Which of the three off-beat 16ths carry a note, and how often "auto"
/// picks the pattern. Same set and weights as psybass.
const PATTERNS: [([bool; 3], f64); 5] = [
    ([true, true, true], 5.0),   // rolling  K B B B
    ([false, true, true], 3.0),  // gallop   K - B B
    ([true, false, true], 1.5),  // skip     K B - B
    ([true, true, false], 1.0),  // triplet  K B B -
    ([false, true, false], 0.2), // offbeat  K - B -
];

/// Spice notes as scale degrees: octave (twice as likely), 5th, 2nd, 3rd,
/// 7th below.
const SPICE_DEGREES: [i32; 6] = [7, 7, 4, 1, 2, -1];
/// Small-fill notes: root, 2nd, 3rd, 5th, octave.
const FILL_DEGREES: [i32; 5] = [0, 1, 2, 4, 7];

/// Soft clip, then a -1 dBFS ceiling.
const CEILING: f64 = 0.891;
const RELEASE: f64 = 0.004;

/// The shared grid rule: the last two beats of every 16th bar are the big
/// fill. `kick` has the same function; keep them in step.
fn big_fill(beat_abs: u64, beats_per_bar: u64) -> bool {
    let bar = beat_abs / beats_per_bar;
    (bar + 1) % 16 == 0 && beat_abs % beats_per_bar >= beats_per_bar.saturating_sub(2)
}

/// Bass register root: D1 (36.7 Hz) up to C#2 (69.3 Hz), as in psybass.
fn root_midi(t: &Tonality) -> i32 {
    26 + (t.root as i32 + 10) % 12
}

// ---------------------------------------------------------------- composition

#[derive(Clone, Copy, Default)]
struct MotifSlot {
    /// Compared against the spice probability when the note plays.
    draw: f64,
    spice: i32,
    vel: f64,
}

struct Phrase {
    index: u64,
    pattern: usize,
    motif: [MotifSlot; MOTIF_SLOTS],
    spice: f64,
    accent: u64,
    cut_from: f64,
    cut_to: f64,
    env_amt: f64,
    env_decay: f64, // fraction of a 16th
    gate: f64,      // fraction of a 16th
    small_fill: bool,
    fill: [i32; 4],
}

// ---------------------------------------------------------------- voice

/// Mono bass voice: two detuned saws into a resonant 4-pole lowpass.
/// Notes are shorter than a 16th, so retriggering never cuts one off.
#[derive(Default)]
struct Voice {
    active: bool,
    i: usize,
    dt: f64,
    p1: f64,
    p2: f64,
    f1: Svf,
    f2: Svf,
    gate: f64,
    env_decay: f64,
    vel: f64,
    cutoff: f64,
    env_amt: f64,
    res: f64,
}

impl Voice {
    fn tick(&mut self, sr: f64) -> f64 {
        if !self.active {
            return 0.0;
        }
        let t = self.i as f64 / sr;
        self.i += 1;
        if t > self.gate + RELEASE {
            self.active = false;
            return 0.0;
        }
        let dt2 = self.dt * 1.004;
        let s1 = saw(self.p1, self.dt);
        let s2 = saw(self.p2, dt2);
        self.p1 = (self.p1 + self.dt).fract();
        self.p2 = (self.p2 + dt2).fract();
        let osc = 0.65 * s1 + 0.35 * s2;
        let fc = self.cutoff + self.env_amt * self.vel * (-t / self.env_decay).exp();
        // resonance only on the second stage
        let y = self.f2.lp(self.f1.lp(osc, fc, 0.0, sr), fc, self.res, sr);
        let attack = (t / 0.0015).min(1.0);
        let rel = if t > self.gate { 1.0 - (t - self.gate) / RELEASE } else { 1.0 };
        (y * 1.6).tanh() * attack * rel.max(0.0) * self.vel * 0.55
    }
}

// ---------------------------------------------------------------- generator

pub struct Bass {
    sr: f64,
    rng: Rng,
    res: f64,
    next_cut: f64,
    phrase: Option<Phrase>,
    clock: StepClock,
    voice: Voice,
    drive: f64,
    param_smooth: f64,
}

impl Bass {
    fn compose(&mut self, index: u64) -> Phrase {
        let r = &mut self.rng;
        let w: Vec<f64> = PATTERNS.iter().map(|p| p.1).collect();
        let pattern = r.weighted(&w);
        let mut motif = [MotifSlot::default(); MOTIF_SLOTS];
        for m in motif.iter_mut() {
            *m = MotifSlot { draw: r.f(), spice: *r.pick(&SPICE_DEGREES), vel: r.range(0.82, 0.95) };
        }
        let cut_from = self.next_cut;
        let cut_to = r.range(160.0, 520.0);
        self.next_cut = cut_to;
        let mut fill = [0; 4];
        fill.iter_mut().for_each(|f| *f = *r.pick(&FILL_DEGREES));
        let ph = Phrase {
            index,
            pattern,
            motif,
            spice: r.range(0.04, 0.18),
            accent: r.next_u64() % 3 + 1,
            cut_from,
            cut_to,
            env_amt: r.range(700.0, 2600.0),
            env_decay: r.range(0.25, 0.7),
            gate: r.range(0.55, 0.9),
            small_fill: r.chance(0.6),
            fill,
        };
        let first = index * PHRASE_BARS + 1;
        eprintln!(
            "  bars {:>4}-{:<4} {:<8} cutoff {:>3.0}->{:<3.0}Hz",
            first,
            first + PHRASE_BARS - 1,
            PATTERN_NAMES[pattern + 1],
            cut_from,
            cut_to
        );
        ph
    }

    fn trigger(&mut self, step: u64, ctx: &Context) {
        let tr = &ctx.transport;
        let bpb = tr.beats_per_bar.max(1) as u64;
        let tpb = TICKS_PER_BEAT as u64;
        let bar_steps = bpb * tpb;
        let phrase_steps = PHRASE_BARS * bar_steps;
        let index = step / phrase_steps;
        if self.phrase.as_ref().map(|p| p.index) != Some(index) {
            self.phrase = Some(self.compose(index));
        }
        let ph = self.phrase.as_ref().unwrap();

        let in_phrase = step % phrase_steps;
        let beat_abs = step / tpb;
        let beat_in_bar = beat_abs % bpb;
        let s = step % tpb;
        let last_bar = in_phrase / bar_steps == PHRASE_BARS - 1;
        let fills = ctx.param(FILLS) != 0.0;
        let pattern = match ctx.param(PATTERN) as usize {
            0 => ph.pattern,
            n => n - 1,
        };

        // (degree, velocity, cutoff boost)
        let note = if fills && big_fill(beat_abs, bpb) {
            // Rising run through the scale on straight 16ths, kick-free.
            let k = (beat_in_bar - (bpb - 2)) * tpb + s;
            Some((k as i32, if s == 0 { 1.0 } else { 0.92 }, 1.6))
        } else if s == 0 {
            None // the kick's 16th
        } else if fills && last_bar && ph.small_fill && beat_in_bar == bpb - 1 {
            Some((ph.fill[s as usize], 0.95, 1.6))
        } else if PATTERNS[pattern].0[s as usize - 1] {
            let m = ph.motif[(in_phrase % MOTIF_SLOTS as u64) as usize];
            let degree = if m.draw < ph.spice * ctx.param(SPICE) { m.spice } else { 0 };
            Some((degree, if s == ph.accent { 1.0 } else { m.vel }, 1.0))
        } else {
            None
        };
        let Some((degree, vel, boost)) = note else { return };

        let sr = self.sr;
        let six = 60.0 / tr.bpm / tpb as f64;
        let midi = root_midi(&ctx.tonality) + ctx.tonality.degree(degree);
        let cutoff = ph.cut_from + (ph.cut_to - ph.cut_from) * in_phrase as f64 / phrase_steps as f64;
        self.voice = Voice {
            active: true,
            dt: midi_hz(midi as f64) / sr,
            p2: 0.37,
            gate: six * (ph.gate * ctx.param(LENGTH)).min(0.95),
            env_decay: six * ph.env_decay,
            vel,
            cutoff: cutoff * boost * ctx.param(CUTOFF).exp2(),
            env_amt: ph.env_amt * ctx.param(ENV),
            res: (self.res + ctx.param(RESONANCE)).clamp(0.0, 0.95),
            ..Voice::default()
        };
    }
}

impl Generator for Bass {
    fn new(config: &Config) -> Self {
        let sr = config.sample_rate;
        let mut rng = Rng::new(config.seed ^ 0xBA55);
        let res = rng.range(0.25, 0.6);
        let next_cut = rng.range(180.0, 400.0);
        Bass {
            sr,
            rng,
            res,
            next_cut,
            phrase: None,
            clock: StepClock::new(),
            voice: Voice::default(),
            drive: PARAMS[DRIVE].default,
            param_smooth: 1.0 - (-1.0 / (0.02 * sr)).exp(),
        }
    }

    fn info(&self) -> Info {
        Info { name: "bass", description: "Psytrance rolling bass on its own, between the kicks, tuned to the host key", channels: 1 }
    }

    fn params(&self) -> &'static [Param] {
        &PARAMS
    }

    /// Notes sit between the kicks already; a little ducking tucks the
    /// tails under them.
    fn duck_sensitivity(&self) -> f64 {
        0.3
    }

    fn entry(&self) -> Entry {
        Entry::Bar
    }

    fn category(&self) -> &'static str {
        "bass"
    }

    fn process(&mut self, ctx: &Context, out: &mut [f32]) {
        let mut steps = self.clock.block(ctx);
        for (i, o) in out.iter_mut().enumerate() {
            while let Some(step) = steps.due(i) {
                self.trigger(step, ctx);
            }
            self.drive += (ctx.param(DRIVE) - self.drive) * self.param_smooth;
            let x = self.voice.tick(self.sr);
            *o = ((x * self.drive).tanh() * CEILING) as f32;
        }
        self.clock.finish(steps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gen_api::{Scale, Transport};

    const SR: f64 = 48_000.0;
    const BEAT: usize = 24_000; // 120 bpm

    fn render(bars: usize, params: &[f64], seed: u64) -> Vec<f32> {
        let mut g = Bass::new(&Config { sample_rate: SR, seed });
        let block = 480;
        let mut out = vec![0.0f32; bars * 4 * BEAT];
        for (b, chunk) in out.chunks_mut(block).enumerate() {
            let ctx = Context {
                sample_rate: SR,
                transport: Transport { bpm: 120.0, beat: (b * block) as f64 / BEAT as f64, beats_per_bar: 4, playing: true },
                tonality: Tonality { root: 9, scale: Scale::Phrygian },
                params,
                input: &[],
            };
            g.process(&ctx, chunk);
        }
        out
    }

    fn defaults() -> Vec<f64> {
        PARAMS.iter().map(|p| p.default).collect()
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0, |m, v| m.max(v.abs()))
    }

    #[test]
    fn deterministic_and_in_range() {
        let a = render(8, &defaults(), 4);
        assert_eq!(a, render(8, &defaults(), 4));
        assert!(a.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
        assert!(peak(&a) > 0.2);
    }

    #[test]
    fn silent_on_the_kick_outside_the_big_fill() {
        // The first ~1.5 ms of each beat (the kick's attack) must be silent:
        // the previous 16th's note has ended and none starts on the beat.
        for seed in 0..4 {
            let out = render(16, &defaults(), seed);
            for beat in 1..16 * 4 {
                if big_fill(beat as u64, 4) {
                    continue;
                }
                let at = beat * BEAT;
                assert!(peak(&out[at + 10..at + 70]) < 1e-3, "seed {seed} beat {beat}");
            }
        }
    }

    #[test]
    fn big_fill_plays_on_the_beat() {
        let out = render(16, &defaults(), 1);
        let at = (15 * 4 + 2) * BEAT; // bar 16, beat 3
        assert!(peak(&out[at..at + 2000]) > 0.05);
    }

    #[test]
    fn offbeat_pattern_plays_only_the_off_8th() {
        let mut p = defaults();
        p[PATTERN] = 5.0; // offbeat
        p[FILLS] = 0.0;
        let out = render(4, &p, 2);
        let six = BEAT / 4;
        for beat in 0..16 {
            let at = beat * BEAT;
            assert!(peak(&out[at + six + 100..at + 2 * six]) < 1e-3, "16th 2 of beat {beat}");
            assert!(peak(&out[at + 2 * six..at + 2 * six + 2000]) > 0.02, "off 8th of beat {beat}");
            assert!(peak(&out[at + 3 * six + 100..at + 4 * six]) < 1e-3, "16th 4 of beat {beat}");
        }
    }
}
