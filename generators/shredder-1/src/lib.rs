//! shredder — choppy distorted "guitar" rhythm synth, the hi-tech psy and
//! trance chug. Stereo.
//!
//! Sound: a power chord of detuned saws through a hard asymmetric
//! waveshaper and a cab filter (highpass, resonant lowpass, a mid bump),
//! which is what makes distortion read as guitar rather than fuzz. Two
//! takes, one per side, with their own detune and a few ms of offset,
//! give the double-tracked wall.
//!
//! Rhythm: every step of the grid (1/16, 1/16T or 1/32) is an open hit, a
//! palm-muted chug (short and dark: the pick filter stays closed before
//! the distortion) or a rest. Riffs are mostly the root with b2, 5th and
//! octave stabs, as scale degrees so they follow the key. Each epoch seeds
//! a base bar of per-step draws; cycles play it A A' A B, and `density`
//! and `mute` decide on every step which draws fire and which are chugs,
//! so both act straight away.
//!
//! Extras on the grid: some cycles end in a tremolo-picked burst at double
//! speed, and the last beat of every 16 bars is a held note that dives by
//! up to an octave (`dive`).

use dsp::{decay_coef, midi_hz, saw, soft_limit, DelayLine, OnePole, Svf};
use gen_api::{Config, Context, Entry, Generator, Info, Param, Rng, Tonality, BARS_PER_CYCLE, CYCLES_PER_EPOCH};

const RATE: usize = 0;
const DENSITY: usize = 1;
const CHORD: usize = 2;
const DISTORTION: usize = 3;
const MUTE: usize = 4;
const TONE: usize = 5;
const WIDTH: usize = 6;
const DIVE: usize = 7;
const OCTAVE: usize = 8;

const RATES: &[&str] = &["1/16", "1/16T", "1/32"];
/// Steps per beat for each rate.
const RATE_DIVS: [u64; 3] = [4, 6, 8];
/// Longest bar, in steps: 1/32 in 4/4.
const MAX_BAR_STEPS: usize = 32;

const CHORDS: &[&str] = &["single", "fifth", "power", "octave"];
/// Semitones of each chord shape, up to three notes.
const CHORD_NOTES: [&[i32]; 4] = [&[0], &[0, 7], &[0, 7, 12], &[0, 12]];

const PARAMS: [Param; 9] = [
    Param::choice("rate", "Rate", RATES, 0).glide(0.0),
    Param::continuous("density", "Density", 0.0, 1.0, 0.7).glide(1.0),
    Param::choice("chord", "Chord", CHORDS, 2).glide(0.0),
    Param::continuous("distortion", "Distortion", 0.0, 1.0, 0.6).glide(2.0),
    // Share of hits that are palm-muted chugs.
    Param::continuous("mute", "Mute", 0.0, 1.0, 0.5).glide(1.0),
    // Cab brightness.
    Param::continuous("tone", "Tone", -1.5, 1.0, 0.0).unit("oct").glide(2.0),
    Param::continuous("width", "Width", 0.0, 1.0, 0.8).glide(2.0),
    // Whammy dive at the end of every 16 bars, up to an octave.
    Param::continuous("dive", "Dive", 0.0, 1.0, 0.5),
    Param::integer("octave", "Octave", -1, 2, 0).unit("oct").glide(0.0),
];

/// Riff degrees and weights: root, b2 (phrygian), 5th, octave, 3rd, 6th,
/// 7th below.
const DEGREES: [(i32, f64); 7] = [(0, 60.0), (1, 10.0), (4, 10.0), (7, 8.0), (2, 5.0), (5, 4.0), (-1, 3.0)];

/// Detune of the two saws per chord note, in cents, per side.
const DETUNE: [[f64; 2]; 2] = [[-7.0, 5.0], [6.0, -9.0]];
/// The right take's offset, at full width.
const DOUBLE_MS: f64 = 9.0;

/// Chord root: the bass register (D1..C#2) two octaves up, D3..C#4.
fn root_midi(t: &Tonality) -> i32 {
    26 + (t.root as i32 + 10) % 12 + 24
}

// ---------------------------------------------------------------- riffs

#[derive(Clone, Copy, Default)]
struct Step {
    gate: f64, // fires when below `density`
    mute: f64, // a chug when below `mute`
    degree: i32,
    accent: bool,
}

type Bar = [Step; MAX_BAR_STEPS];

fn random_degree(r: &mut Rng) -> i32 {
    let w: Vec<f64> = DEGREES.iter().map(|d| d.1).collect();
    DEGREES[r.weighted(&w)].0
}

fn random_step(r: &mut Rng, i: usize) -> Step {
    // Downbeats and off-8ths are likelier to hit and to accent.
    let strong = i % 4 == 0;
    Step {
        gate: r.f() * if strong { 0.6 } else { 1.0 },
        mute: r.f(),
        degree: random_degree(r),
        accent: r.chance(if strong { 0.5 } else { 0.12 }),
    }
}

fn mutate(bar: &Bar, n: usize, r: &mut Rng) -> Bar {
    let mut b = *bar;
    for _ in 0..n {
        let i = (r.next_u64() % MAX_BAR_STEPS as u64) as usize;
        b[i] = random_step(r, i);
    }
    b
}

struct Cycle {
    index: u64,
    bars: [Bar; 4],
    tremolo: bool,
}

// ---------------------------------------------------------------- voice

/// One take (one side): chord oscillators → pick filter → distortion →
/// cab.
#[derive(Default)]
struct Take {
    phases: [[f64; 2]; 3],
    pick: Svf,
    pre_hp: OnePole,
    cab_lp: Svf,
    cab_mid: Svf,
    cab_hp: OnePole,
}

impl Take {
    fn new(sr: f64, rng: &mut Rng) -> Take {
        let mut t = Take { pre_hp: OnePole::new(120.0, sr), cab_hp: OnePole::new(90.0, sr), ..Take::default() };
        t.phases.iter_mut().flatten().for_each(|p| *p = rng.f());
        t
    }

    #[allow(clippy::too_many_arguments)]
    fn tick(&mut self, hz: f64, chord: &[i32], detune: &[f64; 2], pick_hz: f64, dist: f64, cab_hz: f64, sr: f64) -> f64 {
        let mut x = 0.0;
        for (n, &semis) in chord.iter().enumerate() {
            let f = hz * (semis as f64 / 12.0).exp2();
            for (k, &cents) in detune.iter().enumerate() {
                let dt = f * (cents / 1200.0).exp2() / sr;
                x += saw(self.phases[n][k], dt);
                self.phases[n][k] = (self.phases[n][k] + dt).fract();
            }
        }
        x /= (chord.len() * 2) as f64;
        // Palm mute lives here: a closed pick filter before the distortion.
        let x = self.pick.lp(self.pre_hp.hp(x), pick_hz, 0.1, sr);
        // Asymmetric waveshaper, then a second soft stage.
        let g = 1.0 + 60.0 * dist * dist;
        let bias = 0.25;
        let y = ((x * g + bias).tanh() - bias.tanh()) * 1.3;
        let y = (y * 1.5).tanh();
        // Cab: resonant lowpass, a mid bump around 1.8 kHz, highpass.
        self.cab_mid.set(1800.0, 0.4, sr);
        let mid = self.cab_mid.tick(y).1;
        let y = self.cab_lp.lp(y + 0.6 * mid, cab_hz, 0.35, sr);
        self.cab_hp.hp(y)
    }
}

// ---------------------------------------------------------------- generator

pub struct Shredder {
    sr: f64,
    rng: Rng,
    epoch: Option<(u64, Bar)>,
    cycle: Option<Cycle>,
    last: Option<(i64, u64)>,
    takes: [Take; 2],
    right_delay: DelayLine,
    // the note playing
    hz: f64,
    dive_to: f64, // semitones the current note bends by over `dive_len`
    dive_t: f64,
    dive_len: f64,
    gate_left: usize,
    muted: bool,
    vel: f64,
    amp: f64,
    pick_env: f64,
    // coefficients
    attack: f64,
    release_open: f64,
    release_mute: f64,
    pick_decay: f64,
    params: [f64; PARAMS.len()],
    smooth: f64,
}

impl Shredder {
    fn compose_cycle(&mut self, index: u64) -> Cycle {
        let epoch = index / CYCLES_PER_EPOCH as u64;
        if self.epoch.as_ref().map(|e| e.0) != Some(epoch) {
            let mut base = [Step::default(); MAX_BAR_STEPS];
            for (i, s) in base.iter_mut().enumerate() {
                *s = random_step(&mut self.rng, i);
            }
            self.epoch = Some((epoch, base));
        }
        let base = self.epoch.as_ref().unwrap().1;
        let r = &mut self.rng;
        let light = mutate(&base, 2 + (r.next_u64() % 3) as usize, r);
        let heavy = mutate(&base, 6 + (r.next_u64() % 5) as usize, r);
        Cycle { index, bars: [base, light, base, heavy], tremolo: r.chance(0.35) }
    }

    fn play(&mut self, degree: i32, muted: bool, accent: bool, steps: f64, step_len: f64, ctx: &Context) {
        let octave = ctx.param(OCTAVE) as i32;
        let midi = root_midi(&ctx.tonality) + ctx.tonality.degree(degree) + 12 * octave;
        self.hz = midi_hz(midi as f64);
        self.muted = muted;
        self.vel = if accent { 1.0 } else if muted { 0.75 } else { 0.85 };
        let gate = if muted { 0.4 } else { 0.85 } * steps * step_len;
        self.gate_left = gate as usize;
        self.pick_env = 1.0;
        self.dive_to = 0.0;
        self.dive_t = 0.0;
    }

    /// Step `n` (of `div` per beat), or the odd half-step between two
    /// steps when `half` (only used by tremolo bursts).
    fn step(&mut self, n: u64, half: bool, div: u64, ctx: &Context) {
        let tr = &ctx.transport;
        let bpb = tr.beats_per_bar.max(1) as u64;
        let bar_steps = div * bpb;
        let bar = n / bar_steps;
        let cycle = bar / BARS_PER_CYCLE as u64;
        if self.cycle.as_ref().map(|c| c.index) != Some(cycle) {
            self.cycle = Some(self.compose_cycle(cycle));
        }
        let density = ctx.param(DENSITY);
        if density <= 0.0 {
            return;
        }
        let c = self.cycle.as_ref().unwrap();
        let in_bar = n % bar_steps;
        let beat_in_bar = in_bar / div;
        let bar_in_cycle = (bar % BARS_PER_CYCLE as u64) as usize;
        let last_beat_of_cycle = bar_in_cycle == BARS_PER_CYCLE as usize - 1 && beat_in_bar == bpb - 1;
        let last_beat_of_epoch = last_beat_of_cycle && cycle % CYCLES_PER_EPOCH as u64 == CYCLES_PER_EPOCH as u64 - 1;
        let step_len = 60.0 / tr.bpm / div as f64 * self.sr;
        let idx = (in_bar as usize * MAX_BAR_STEPS / bar_steps as usize).min(MAX_BAR_STEPS - 1);
        let st = c.bars[bar_in_cycle][idx];

        if last_beat_of_epoch {
            // One held note for the whole beat, diving.
            if in_bar % div == 0 && !half {
                let degree = st.degree;
                self.play(degree, false, true, div as f64, step_len, ctx);
                self.dive_to = -12.0 * ctx.param(DIVE);
                self.dive_len = div as f64 * step_len;
            }
            return;
        }
        if last_beat_of_cycle && c.tremolo {
            // Tremolo picking: every half-step, open, on the riff's note.
            let degree = c.bars[bar_in_cycle][(beat_in_bar * div) as usize * MAX_BAR_STEPS / bar_steps as usize].degree;
            self.play(degree, false, !half && in_bar % div == 0, 0.5, step_len, ctx);
            return;
        }
        if half || st.gate >= density {
            return;
        }
        let muted = st.mute < ctx.param(MUTE);
        // Chugs are nearly always on the root, like a guitarist's open string.
        let degree = if muted && st.degree != 7 { 0 } else { st.degree };
        self.play(degree, muted, st.accent, 1.0, step_len, ctx);
    }
}

impl Generator for Shredder {
    fn new(config: &Config) -> Self {
        let sr = config.sample_rate;
        let mut rng = Rng::new(config.seed ^ 0x5ED0);
        let takes = [Take::new(sr, &mut rng), Take::new(sr, &mut rng)];
        let coef = |tau: f64| 1.0 - decay_coef(tau, sr);
        Shredder {
            sr,
            rng,
            epoch: None,
            cycle: None,
            last: None,
            takes,
            right_delay: DelayLine::new((DOUBLE_MS * 2.0 * sr / 1000.0) as usize),
            hz: 110.0,
            dive_to: 0.0,
            dive_t: 0.0,
            dive_len: 1.0,
            gate_left: 0,
            muted: false,
            vel: 0.0,
            amp: 0.0,
            pick_env: 0.0,
            attack: coef(0.0012),
            release_open: coef(0.03),
            release_mute: coef(0.008),
            pick_decay: decay_coef(0.04, sr),
            params: PARAMS.map(|p| p.default),
            smooth: coef(0.02),
        }
    }

    fn info(&self) -> Info {
        Info { name: "shredder", description: "Choppy distorted guitar-like rhythm synth: power-chord chugs, stabs and dives (stereo)", channels: 2 }
    }

    fn params(&self) -> &'static [Param] {
        &PARAMS
    }

    fn category(&self) -> &'static str {
        "lead"
    }

    fn entry(&self) -> Entry {
        Entry::Bar
    }

    fn duck_sensitivity(&self) -> f64 {
        0.6
    }

    fn process(&mut self, ctx: &Context, out: &mut [f32]) {
        let sr = self.sr;
        let tr = &ctx.transport;
        let spb = tr.samples_per_beat(sr);
        let div = RATE_DIVS[ctx.param(RATE) as usize];
        let fine = div * 2; // half-steps, for tremolo bursts
        let chord = CHORD_NOTES[ctx.param(CHORD) as usize];

        for (i, frame) in out.chunks_exact_mut(2).enumerate() {
            let n = ((tr.beat + i as f64 / spb).max(0.0) * fine as f64 + 1e-9).floor() as i64;
            match self.last {
                Some((last, d)) if d == fine && n == last + 1 => {
                    self.last = Some((n, fine));
                    if tr.playing {
                        self.step(n as u64 / 2, n % 2 == 1, div, ctx);
                    }
                }
                Some((last, d)) if d == fine && n == last => {}
                // First block, seek, loop or rate change: resync silently.
                _ => self.last = Some((n, fine)),
            }
            for (k, v) in self.params.iter_mut().enumerate() {
                *v += (ctx.param(k) - *v) * self.smooth;
            }
            let p = self.params;

            // Envelope: phases run on, so retriggers never click.
            let gate = self.gate_left > 0 && tr.playing;
            self.gate_left = self.gate_left.saturating_sub(1);
            let target = if gate { self.vel } else { 0.0 };
            let coef = if target > self.amp { self.attack } else if self.muted { self.release_mute } else { self.release_open };
            self.amp += (target - self.amp) * coef;
            self.pick_env *= self.pick_decay;

            let mut hz = self.hz;
            if self.dive_to != 0.0 {
                let u = (self.dive_t / self.dive_len).min(1.0);
                hz *= (self.dive_to * u * u / 12.0).exp2();
                self.dive_t += 1.0;
            }

            if self.amp < 1e-5 && !gate {
                // Silent: keep the delay line moving, skip the voice.
                self.right_delay.write(0.0);
                frame[0] = 0.0;
                frame[1] = 0.0;
                continue;
            }
            let pick_hz = if self.muted { 450.0 + 500.0 * self.pick_env } else { 2500.0 + 5000.0 * self.pick_env };
            let cab_hz = 3800.0 * p[TONE].exp2();
            let l = self.takes[0].tick(hz, chord, &DETUNE[0], pick_hz, p[DISTORTION], cab_hz, sr);
            let r = self.takes[1].tick(hz, chord, &DETUNE[1], pick_hz, p[DISTORTION], cab_hz, sr);
            let delay = 1.0 + p[WIDTH] * DOUBLE_MS * sr / 1000.0;
            let r_late = self.right_delay.read(delay);
            self.right_delay.write(r * self.amp);
            let l = l * self.amp;
            let w = p[WIDTH];
            let (ol, or) = (l * (1.0 + w) * 0.5 + r_late * (1.0 - w) * 0.5, r_late * (1.0 + w) * 0.5 + l * (1.0 - w) * 0.5);
            // Less distortion means less energy; make up for it so the
            // knob changes the character, not the level (1 at the default).
            let makeup = 0.2 * 0.853 / (0.35 + 0.65 * p[DISTORTION].sqrt());
            frame[0] = soft_limit(ol * makeup) as f32;
            frame[1] = soft_limit(or * makeup) as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gen_api::{Scale, Transport};

    const SR: f64 = 48_000.0;
    const BEAT: usize = 24_000; // 120 bpm
    const BAR: usize = 4 * BEAT;

    fn render(bars: usize, params: &[f64], playing: bool) -> Vec<f32> {
        let mut g = Shredder::new(&Config { sample_rate: SR, seed: 6 });
        let block = 480;
        let mut out = vec![0.0f32; bars * BAR * 2];
        for (b, chunk) in out.chunks_mut(block * 2).enumerate() {
            let ctx = Context {
                sample_rate: SR,
                transport: Transport { bpm: 120.0, beat: (b * block) as f64 / BEAT as f64, beats_per_bar: 4, playing },
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

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// RMS of the difference between neighbouring samples: a crude
    /// brightness measure (rises with high-frequency content).
    fn brightness(x: &[f32]) -> f64 {
        let d: Vec<f32> = x.windows(3).step_by(2).map(|w| w[2] - w[0]).collect();
        rms(&d) / rms(x)
    }

    #[test]
    fn deterministic_and_in_range() {
        let a = render(4, &defaults(), true);
        assert_eq!(a, render(4, &defaults(), true));
        assert!(a.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
        assert!(rms(&a) > 0.02, "rms {}", rms(&a));
    }

    #[test]
    fn silent_when_stopped_or_at_zero_density() {
        assert!(render(2, &defaults(), false).iter().all(|&x| x == 0.0));
        let mut p = defaults();
        p[DENSITY] = 0.0;
        assert!(render(2, &p, true).iter().all(|&x| x == 0.0));
    }

    #[test]
    fn palm_mute_is_darker_and_shorter() {
        let mut open = defaults();
        open[MUTE] = 0.0;
        let mut muted = defaults();
        muted[MUTE] = 1.0;
        let (a, b) = (render(3, &open, true), render(3, &muted, true));
        assert!(brightness(&b) < brightness(&a) * 0.8, "open {} muted {}", brightness(&a), brightness(&b));
        assert!(rms(&b) < rms(&a), "muted notes are shorter");
    }

    #[test]
    fn rests_between_chugs() {
        // At 1/16 with full mute, each chug's gate is 40% of the step and
        // the release is ~8 ms, so the end of every 16th is near-silent.
        let mut p = defaults();
        p[MUTE] = 1.0;
        p[DENSITY] = 1.0;
        let out = render(2, &p, true);
        let six = BEAT / 4;
        let quiet = (1..24).filter(|s| {
            let end = s * six; // start of the next step
            rms(&out[(end - 600) * 2..end * 2]) < 0.02
        });
        assert!(quiet.count() >= 20);
    }
}
