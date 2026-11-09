//! acid — generative 303-style acid line with a tempo-synced ping-pong delay.
//! Stereo.
//!
//! Sound: saw or square into a resonant 4-pole ladder with saturation in the
//! loop, a snappy filter envelope, accents (louder, more envelope, plus a
//! "wow" that builds up over consecutive accents) and slides (pitch glides,
//! envelopes don't retrigger), then drive.
//!
//! Structure, on the shared grid:
//! - per epoch (16 bars): a 16-step base pattern, either new, transposed or
//!   kept, and the shape of the filter tweak across the epoch
//! - per cycle (4 bars): call and response, bars A A' A B, where A' is a
//!   light and B a heavier mutation of the base pattern
//!
//! Notes are stored as scale degrees and resolved when they play, so key and
//! scale changes from the host apply immediately.
//!
//! Chords from the workspace score: where a bar has one, the line moves to
//! the chord root and its degrees count from the chord's degree, so it
//! stays in the key.
//!
//! Dynamics and feel from the score: `quiet` closes the filter, turns down
//! and drops notes, by its intensity; `break` keeps it going, filtered and
//! quieter; `rise` opens filter and resonance towards the region's end;
//! `peak` opens the filter a bit.
//!
//! Feel from the score (BPM, grid and the dotted-8th delay never change):
//! - half: only the 8ths play
//! - triplet: triplet 8ths, three notes per beat on the beat's thirds,
//!   taking their notes from the beat's first three 16ths; a note slides
//!   into the next triplet when its step slides and the next one is gated
//! - double and quad: the sequence runs twice as fast, a new step on
//!   every 32nd, so a doubled bar plays two bars of the line in order
//!   (slides and accents included); the synth itself doesn't change
//!
//! Turn `tempo` off to ignore the feel and play every bar as normal.

use dsp::{decay_coef, midi_hz, saw, square, Ladder, OnePole, SubClock};
use gen_api::{Config, Context, Entry, Feel, Generator, Info, Mark, Param, Rng, StepClock, BARS_PER_CYCLE, CYCLES_PER_EPOCH, TICKS_PER_BEAT};
use std::f64::consts::PI;

const BAR_STEPS: usize = 16;
const CYCLE_STEPS: usize = BAR_STEPS * BARS_PER_CYCLE as usize;
const EPOCH_BARS: f64 = (BARS_PER_CYCLE * CYCLES_PER_EPOCH) as f64;

/// Soft clip, then a -1 dBFS ceiling.
const CEILING: f64 = 0.891;

const CUTOFF: usize = 0;
const RESONANCE: usize = 1;
const DELAY_MIX: usize = 2;
const FEEDBACK: usize = 3;
const DRIVE: usize = 4;
const OCTAVE: usize = 5;
const TEMPO: usize = 6;

/// Offsets on top of the generator's own tweak, plus the built-in delay
/// and drive. Randomized, the cutoff sweeps over 4 bars like a hand on the
/// knob; the rest follow over 1-2 bars.
const PARAMS: [Param; 7] = [
    Param::continuous("cutoff", "Cutoff offset", -2.0, 2.0, 0.0).unit("oct").glide(4.0),
    Param::continuous("resonance", "Resonance offset", -0.4, 0.3, 0.0).glide(2.0),
    Param::continuous("delay", "Delay mix", 0.0, 1.0, 0.4).glide(2.0),
    Param::continuous("feedback", "Delay feedback", 0.0, 0.85, 0.42).glide(1.0),
    // Multiplies the seed's own drive amount.
    Param::continuous("drive", "Drive", 0.25, 4.0, 1.0).unit("x").glide(2.0),
    // Octaves up or down from the seed's own register; applies from the
    // next note.
    Param::integer("octave", "Octave", -2, 2, 0).unit("oct").glide(0.0),
    // Follow the score's tempo feel; off plays every bar as normal feel.
    Param::toggle("tempo", "Tempo", true).glide(0.0),
];

/// Scale degrees and their weights: root, octave, 2nd (the phrygian b2),
/// 5th, 3rd, 7th below, 6th.
const DEGREES: [(i32, f64); 7] = [(0, 45.0), (7, 14.0), (1, 12.0), (4, 9.0), (2, 8.0), (-1, 6.0), (5, 6.0)];

// ---------------------------------------------------------------- patterns

#[derive(Clone, Copy, Default)]
struct Step {
    gate: bool,
    degree: i32,
    accent: bool,
    /// Glide from this note into the next; the next must be gated.
    slide: bool,
}

type Bar = [Step; BAR_STEPS];

fn random_degree(r: &mut Rng) -> i32 {
    let w: Vec<f64> = DEGREES.iter().map(|d| d.1).collect();
    DEGREES[r.weighted(&w)].0
}

fn random_step(r: &mut Rng) -> Step {
    Step { gate: r.chance(0.8), degree: random_degree(r), accent: r.chance(0.25), slide: r.chance(0.18) }
}

fn mutate(bar: &Bar, n: usize, r: &mut Rng) -> Bar {
    let mut b = *bar;
    for _ in 0..n {
        let s = &mut b[(r.next_u64() % BAR_STEPS as u64) as usize];
        match r.next_u64() % 4 {
            0 => s.gate = !s.gate,
            1 => s.degree = random_degree(r),
            2 => s.accent = !s.accent,
            _ => s.slide = !s.slide,
        }
    }
    b
}

/// `.` rest, `x` note, `X` accent; a following `~` marks a slide.
fn bar_string(bar: &Bar) -> String {
    bar.iter()
        .map(|s| match (s.gate, s.accent, s.slide) {
            (false, _, _) => ".".to_string(),
            (true, false, false) => "x".to_string(),
            (true, true, false) => "X".to_string(),
            (true, false, true) => "x~".to_string(),
            (true, true, true) => "X~".to_string(),
        })
        .collect()
}

/// The slow filter movement across an epoch.
#[derive(Clone, Copy)]
struct Tweak {
    shape: usize,
    lo: f64,
    hi: f64,
    res_lo: f64,
    res_hi: f64,
}

const SHAPES: [&str; 4] = ["rise", "arch", "wave", "dip"];

impl Tweak {
    /// 0..1 at position `t` (0..1) through the epoch.
    fn value(&self, t: f64) -> f64 {
        let s = match self.shape {
            0 => t.powf(1.5),
            1 => (PI * t).sin().powf(1.2),
            2 => 0.5 - 0.5 * (4.0 * PI * t).cos(),
            _ => 0.5 + 0.5 * (2.0 * PI * t).cos(),
        };
        self.lo + (self.hi - self.lo) * s
    }

    fn res(&self, v: f64) -> f64 {
        self.res_lo + (self.res_hi - self.res_lo) * v
    }
}

struct Epoch {
    index: u64,
    base: Bar,
    tweak: Tweak,
}

struct Cycle {
    index: u64,
    steps: [Step; CYCLE_STEPS],
}

// ---------------------------------------------------------------- voice

/// Per-seed sound character.
#[derive(Clone, Copy)]
struct Character {
    square: bool,
    octave: i32, // octaves above the bass register
    drive: f64,
    env_oct: f64, // filter envelope depth, octaves
    decay: f64,   // filter envelope time constant, s
}

/// Monophonic 303-style voice.
struct Voice {
    phase: f64,
    lf: f64, // log2 of the current frequency
    lf_target: f64,
    gate: bool,
    gate_left: Option<usize>, // samples until gate off; None = held
    amp: f64,
    fenv: f64,
    wow: f64, // accent build-up
    accent: bool,
    /// The previous step asked to slide into the next one.
    slide_into: bool,
    filter: Ladder,
    // precomputed coefficients
    glide: f64,
    attack: f64,
    release: f64,
    fenv_coef: f64,
    fenv_coef_accent: f64,
    wow_coef: f64,
    /// Notes started (including slid-into ones), for the tests.
    notes: u64,
    /// Every note's frequency, in order (test builds only).
    #[cfg(test)]
    log: Vec<f64>,
}

impl Voice {
    fn new(c: &Character, sr: f64) -> Voice {
        let smooth = |tau: f64| 1.0 - (-1.0 / (tau * sr)).exp();
        Voice {
            phase: 0.0,
            lf: 7.0,
            lf_target: 7.0,
            gate: false,
            gate_left: None,
            amp: 0.0,
            fenv: 0.0,
            wow: 0.0,
            accent: false,
            slide_into: false,
            filter: Ladder::default(),
            glide: smooth(0.045),
            attack: smooth(0.002),
            release: smooth(0.008),
            fenv_coef: decay_coef(c.decay, sr),
            fenv_coef_accent: decay_coef(c.decay * 0.6, sr),
            wow_coef: decay_coef(0.25, sr),
            notes: 0,
            #[cfg(test)]
            log: Vec::new(),
        }
    }

    fn note(&mut self, freq: f64, accent: bool, gate_samples: Option<usize>) {
        self.notes += 1;
        #[cfg(test)]
        self.log.push(freq);
        self.lf_target = freq.log2();
        if !self.slide_into || !self.gate {
            // New note: jump to pitch, retrigger the envelopes.
            self.lf = self.lf_target;
            self.fenv = 1.0;
            self.accent = accent;
            if accent {
                self.wow += (1.0 - self.wow) * 0.6;
            }
        }
        self.gate = true;
        self.gate_left = gate_samples;
    }

    fn rest(&mut self) {
        self.gate = false;
        self.gate_left = None;
        self.slide_into = false;
    }

    fn tick(&mut self, c: &Character, drive: f64, cutoff: f64, res: f64, sr: f64) -> f64 {
        self.lf += (self.lf_target - self.lf) * self.glide;
        let dt = self.lf.exp2() / sr;
        let osc = if c.square { square(self.phase, dt) } else { saw(self.phase, dt) };
        self.phase = (self.phase + dt).fract();

        if let Some(n) = self.gate_left {
            if n == 0 {
                self.gate = false;
                self.gate_left = None;
            } else {
                self.gate_left = Some(n - 1);
            }
        }
        let target = if self.gate { 1.0 } else { 0.0 };
        self.amp += (target - self.amp) * if self.gate { self.attack } else { self.release };

        self.fenv *= if self.accent { self.fenv_coef_accent } else { self.fenv_coef };
        self.wow *= self.wow_coef;

        let env_oct = c.env_oct * if self.accent { 1.5 } else { 1.0 };
        let fc = cutoff * (self.fenv * env_oct + self.wow * 1.2).exp2();
        let res = res + if self.accent { 0.08 * self.fenv } else { 0.0 };
        let y = self.filter.tick(osc * 0.8, fc, res, sr);
        let y = (y * drive).tanh() / drive.tanh();
        y * self.amp * if self.accent { 1.3 } else { 1.0 }
    }
}

// ---------------------------------------------------------------- delay

/// Ping-pong delay: input → left → right → left …, with the repeats
/// band-limited so they sit behind the dry line.
struct Delay {
    l: Vec<f64>,
    r: Vec<f64>,
    w: usize,
    hp: OnePole,
    lp: OnePole,
    feedback: f64,
    wet: f64,
}

impl Delay {
    fn new(sr: f64) -> Delay {
        let n = (sr * 2.5) as usize;
        let (feedback, wet) = (PARAMS[FEEDBACK].default, PARAMS[DELAY_MIX].default);
        Delay { l: vec![0.0; n], r: vec![0.0; n], w: 0, hp: OnePole::new(200.0, sr), lp: OnePole::new(3200.0, sr), feedback, wet }
    }

    fn read(buf: &[f64], w: usize, delay: f64) -> f64 {
        let n = buf.len();
        let pos = w as f64 + n as f64 - delay;
        let i0 = pos.floor() as usize % n;
        let frac = pos.fract();
        buf[i0] * (1.0 - frac) + buf[(i0 + 1) % n] * frac
    }

    /// `delay` in samples (fractional, so tempo changes glide).
    fn tick(&mut self, x: f64, delay: f64) -> (f64, f64) {
        let delay = delay.clamp(1.0, self.l.len() as f64 - 2.0);
        let ol = Delay::read(&self.l, self.w, delay);
        let or = Delay::read(&self.r, self.w, delay);
        let xin = self.lp.lp(self.hp.hp(x));
        self.l[self.w] = xin + self.feedback * or;
        self.r[self.w] = ol;
        self.w = (self.w + 1) % self.l.len();
        (ol * self.wet, or * self.wet)
    }
}

/// The bar's tempo feel, or normal when the `tempo` toggle is off.
fn feel_of(bar: &gen_api::ScoreBar, ctx: &Context) -> Feel {
    if ctx.param(TEMPO) == 0.0 {
        Feel::Normal
    } else {
        bar.feel
    }
}

// ---------------------------------------------------------------- generator

pub struct Acid {
    sr: f64,
    rng: Rng,
    character: Character,
    clock: StepClock,
    /// Triplet 8ths and 32nds in triplet and double/quad bars; the 16th
    /// clock still fires the positions both share.
    sub: SubClock,
    epoch: Option<Epoch>,
    cycle: Option<Cycle>,
    voice: Voice,
    delay: Delay,
    cutoff: f64,
    cutoff_smooth: f64,
    param_smooth: f64,
    drive: f64,
    /// Seeds the per-step draws that thin the line out in quiet parts.
    thin_seed: u64,
    /// Output level from the score's dynamics, smoothed.
    level: f64,
}

impl Acid {
    fn compose_epoch(&mut self, index: u64) -> Epoch {
        let r = &mut self.rng;
        let (base, how) = match &self.epoch {
            Some(prev) if r.chance(0.5) => {
                if r.chance(0.5) {
                    let d = *r.pick(&[1, 2, 4, -1]);
                    let mut b = prev.base;
                    b.iter_mut().for_each(|s| s.degree += d);
                    (b, format!("transposed {d:+}"))
                } else {
                    (prev.base, "kept".to_string())
                }
            }
            _ => {
                let mut b: Bar = [Step::default(); BAR_STEPS];
                b.iter_mut().for_each(|s| *s = random_step(r));
                (b, "new".to_string())
            }
        };
        let lo = r.range(0.0, 0.3);
        let tweak = Tweak {
            shape: (r.next_u64() % SHAPES.len() as u64) as usize,
            lo,
            hi: r.range(0.6, 1.0),
            res_lo: r.range(0.55, 0.75),
            res_hi: r.range(0.8, 0.98),
        };
        let first = index * EPOCH_BARS as u64 + 1;
        eprintln!(
            "  bars {:>4}-{:<4} {:<26} {:<14} tweak {}",
            first,
            first + EPOCH_BARS as u64 - 1,
            bar_string(&base),
            how,
            SHAPES[tweak.shape]
        );
        Epoch { index, base, tweak }
    }

    fn compose_cycle(&mut self, index: u64) -> Cycle {
        let epoch_index = index / CYCLES_PER_EPOCH as u64;
        if self.epoch.as_ref().map(|e| e.index) != Some(epoch_index) {
            self.epoch = Some(self.compose_epoch(epoch_index));
        }
        let base = self.epoch.as_ref().unwrap().base;
        let r = &mut self.rng;
        let last_in_epoch = index % CYCLES_PER_EPOCH as u64 == CYCLES_PER_EPOCH as u64 - 1;

        let call_back = mutate(&base, 1 + (r.next_u64() % 3) as usize, r);
        let mut response = mutate(&base, 4 + (r.next_u64() % 4) as usize, r);
        if last_in_epoch {
            // Push into the next epoch: more accents, a run at the end.
            response.iter_mut().for_each(|s| s.accent |= r.chance(0.3));
            for (j, s) in response[12..].iter_mut().enumerate() {
                *s = Step { gate: true, degree: j as i32 * 2, accent: j == 3, slide: false };
            }
        }

        let mut steps = [Step::default(); CYCLE_STEPS];
        for (bar, pattern) in [base, call_back, base, response].iter().enumerate() {
            steps[bar * BAR_STEPS..(bar + 1) * BAR_STEPS].copy_from_slice(pattern);
        }
        // A slide needs a gated note to slide into, within this cycle.
        for i in 0..CYCLE_STEPS {
            let next_gated = i + 1 < CYCLE_STEPS && steps[i + 1].gate;
            if !steps[i].gate || !next_gated {
                steps[i].slide = false;
            }
        }
        Cycle { index, steps }
    }

    /// A 16th from the step clock. Composes cycles as they come up; in
    /// triplet and double/quad bars it plays only the subdivisions that
    /// fall on the 16th, the sub clock plays the rest (see [`Acid::sub`]).
    fn trigger(&mut self, step: u64, ctx: &Context) {
        let index = step / CYCLE_STEPS as u64;
        if self.cycle.as_ref().map(|c| c.index) != Some(index) {
            self.cycle = Some(self.compose_cycle(index));
        }
        let st = self.cycle.as_ref().unwrap().steps[(step % CYCLE_STEPS as u64) as usize];
        let sbar = ctx.score.bar(self.bar_of(step, ctx));
        let feel = feel_of(&sbar, ctx);
        match feel {
            Feel::Triplet => {
                if step.is_multiple_of(TICKS_PER_BEAT as u64) {
                    self.triplet(step / TICKS_PER_BEAT as u64 * 3, ctx);
                }
                return;
            }
            Feel::Double | Feel::Quad => {
                self.double_step(step * 2, ctx);
                return;
            }
            Feel::Normal | Feel::Half => {}
        }
        let thinned = self.quiet_thinned(step, ctx) || (feel == Feel::Half && step % 2 == 1);
        if !st.gate || thinned {
            self.voice.rest();
            return;
        }
        let six = ctx.transport.samples_per_beat(self.sr) / 4.0;
        let gate = if st.slide { None } else { Some((six * 0.55) as usize) };
        self.play(st, step, gate, ctx);
        self.voice.slide_into = st.slide;
    }

    /// A subdivision from the sub clock at `per_beat` per beat, in a bar of
    /// `feel`. Those on a 16th are left to [`Acid::trigger`].
    fn sub_trigger(&mut self, n: u64, per_beat: u64, feel: Feel, ctx: &Context) {
        match feel {
            Feel::Triplet if per_beat == 3 && !n.is_multiple_of(3) => self.triplet(n, ctx),
            Feel::Double | Feel::Quad if per_beat == 8 && n % 2 == 1 => self.double_step(n, ctx),
            _ => {}
        }
    }

    fn bar_of(&self, step: u64, ctx: &Context) -> u64 {
        step / (TICKS_PER_BEAT as u64 * ctx.transport.beats_per_bar.max(1) as u64)
    }

    /// The composed step, if its cycle is the current one.
    fn step_data(&self, step: u64) -> Option<Step> {
        let c = self.cycle.as_ref()?;
        (c.index == step / CYCLE_STEPS as u64).then(|| c.steps[(step % CYCLE_STEPS as u64) as usize])
    }

    /// A `quiet` mark drops notes, by its intensity, from a per-step draw.
    fn quiet_thinned(&self, step: u64, ctx: &Context) -> bool {
        let sbar = ctx.score.bar(self.bar_of(step, ctx));
        let draw = Rng::new(self.thin_seed ^ step.wrapping_mul(0x9E37_79B9_7F4A_7C15)).f();
        sbar.mark == Mark::Quiet && draw < 0.5 * sbar.intensity as f64
    }

    /// The 16th a triplet 8th takes its note from: the beat's first three.
    fn triplet_step(n: u64) -> u64 {
        n / 3 * TICKS_PER_BEAT as u64 + n % 3
    }

    /// Triplet 8th `n` (counted from beat 0).
    fn triplet(&mut self, n: u64, ctx: &Context) {
        let step = Acid::triplet_step(n);
        let Some(st) = self.step_data(step) else { return };
        if !st.gate || self.quiet_thinned(step, ctx) {
            self.voice.rest();
            return;
        }
        let next_gated = self.step_data(Acid::triplet_step(n + 1)).is_some_and(|s| s.gate);
        let slide = st.slide && next_gated;
        let slot = ctx.transport.samples_per_beat(self.sr) / 3.0;
        let gate = if slide { None } else { Some((slot * 0.55) as usize) };
        self.play(st, step, gate, ctx);
        self.voice.slide_into = slide;
    }

    /// 32nd `m` (counted from beat 0) in a double-time bar: the sequence
    /// runs twice as fast, one step per 32nd. Bar `b` of a cycle plays
    /// steps `32·b … 32·b + 31` of the cycle's 64 (wrapping), so a doubled
    /// bar plays two bars of the line in order, slides and accents
    /// included.
    fn double_step(&mut self, m: u64, ctx: &Context) {
        let bar_32nds = 8 * ctx.transport.beats_per_bar.max(1) as u64;
        let bar = m / bar_32nds;
        let cycle = bar / BARS_PER_CYCLE as u64;
        let seq = ((bar % BARS_PER_CYCLE as u64) * bar_32nds + m % bar_32nds) % CYCLE_STEPS as u64;
        let Some(st) = self.step_data(cycle * CYCLE_STEPS as u64 + seq) else { return };
        let here = m / 2; // the 16th it sounds in: for the bar's mark and chord
        if !st.gate || self.quiet_thinned(here, ctx) {
            self.voice.rest();
            return;
        }
        // compose_cycle already cleared slides with no gated step after.
        let slot = ctx.transport.samples_per_beat(self.sr) / 8.0;
        let gate = if st.slide { None } else { Some((slot * 0.55) as usize) };
        self.play(st, here, gate, ctx);
        self.voice.slide_into = st.slide;
    }

    /// Resolves the step's degree in the current key and chord and starts
    /// the note.
    fn play(&mut self, st: Step, step: u64, gate: Option<usize>, ctx: &Context) {
        let t = &ctx.tonality;
        // Bass register root is D1..C#2; the acid sits one or two octaves up,
        // moved by the octave knob (kept between the bass register and
        // four octaves above it).
        // Over a chord from the score, the line moves to the chord root and
        // its degrees count from the chord's degree.
        let octave = (self.character.octave + ctx.param(OCTAVE) as i32).clamp(0, 4);
        let (line_root, semis) = t.chord_degree(ctx.score.bar(self.bar_of(step, ctx)).chord, st.degree);
        let root = 26 + (line_root as i32 + 10) % 12 + 12 * octave;
        let freq = midi_hz((root + semis) as f64);
        self.voice.note(freq, st.accent, gate);
    }
}

impl Generator for Acid {
    fn new(config: &Config) -> Self {
        let sr = config.sample_rate;
        let mut rng = Rng::new(config.seed ^ 0xAC1D);
        let character = Character {
            square: rng.chance(0.4),
            octave: if rng.chance(0.5) { 1 } else { 2 },
            drive: rng.range(1.0, 2.2),
            env_oct: rng.range(1.5, 3.0),
            decay: rng.range(0.08, 0.3),
        };
        // Delay feedback and mix used to come from the seed and are params
        // now; the draws stay so every seed still composes the same lines.
        let _ = (rng.f(), rng.f());
        let delay = Delay::new(sr);
        Acid {
            sr,
            voice: Voice::new(&character, sr),
            delay,
            character,
            clock: StepClock::new(),
            sub: SubClock::new(),
            epoch: None,
            cycle: None,
            cutoff: 300.0,
            cutoff_smooth: 1.0 - (-1.0 / (0.05 * sr)).exp(),
            thin_seed: config.seed ^ 0xAC1D,
            level: 1.0,
            param_smooth: 1.0 - (-1.0 / (0.02 * sr)).exp(),
            drive: PARAMS[DRIVE].default,
            rng,
        }
    }

    fn params(&self) -> &'static [Param] {
        &PARAMS
    }

    fn duck_sensitivity(&self) -> f64 {
        0.6
    }

    fn category(&self) -> &'static str {
        "lead"
    }

    /// The line starts on the downbeat.
    fn entry(&self) -> Entry {
        Entry::Bar
    }

    fn info(&self) -> Info {
        Info {
            name: "acid",
            description: "303-style acid line with slides, accents, filter tweaks and ping-pong delay (stereo)",
            channels: 2,
        }
    }

    fn process(&mut self, ctx: &Context, out: &mut [f32]) {
        let sr = self.sr;
        let tr = &ctx.transport;
        let spb = tr.samples_per_beat(sr);
        let delay_samples = 3.0 * spb / 4.0; // dotted 8th
        let epoch_beats = EPOCH_BARS * tr.beats_per_bar as f64;
        let mut steps = self.clock.block(ctx);

        // The score's dynamics at the block's start: a filter offset in
        // octaves, extra resonance and a level, all smoothed below.
        let bar = ctx.score.at(tr.beat, tr.beats_per_bar);
        let (k, p) = (bar.intensity as f64, ctx.score.progress(tr.beat, tr.beats_per_bar));
        let (dyn_oct, dyn_res, level) = match bar.mark {
            Mark::Quiet => (-1.2 * k, 0.0, 1.0 - 0.5 * k),
            Mark::Break => (-1.5 * k, 0.0, 1.0 - 0.3 * k),
            Mark::Rise => (2.0 * p * k, 0.15 * p * k, 1.0),
            Mark::Peak => (0.6 * k, 0.05 * k, 1.0),
            Mark::Normal => (0.0, 0.0, 1.0),
        };

        for (i, frame) in out.chunks_exact_mut(2).enumerate() {
            while let Some(step) = steps.due(i) {
                self.trigger(step, ctx);
            }
            // Triplet 8ths and 32nds between the 16ths, after them so the
            // cycle is already composed. Other bars keep the sub clock in
            // step at 16ths and ignore it.
            let beat = tr.beat + i as f64 / spb;
            let feel = feel_of(&ctx.score.at(beat, tr.beats_per_bar), ctx);
            let per_beat = match feel {
                Feel::Triplet => 3,
                Feel::Double | Feel::Quad => 8,
                Feel::Normal | Feel::Half => 4,
            };
            if let Some(n) = self.sub.tick(beat, per_beat, spb, tr.playing) {
                self.sub_trigger(n, per_beat, feel, ctx);
            }
            // Filter tweak, from the position within the epoch.
            let t = ((tr.beat + i as f64 / spb) / epoch_beats).fract();
            let (v, res) = match &self.epoch {
                Some(e) => {
                    let v = e.tweak.value(t);
                    (v, e.tweak.res(v))
                }
                None => (0.3, 0.7),
            };
            let target = 120.0 * (v * 4.2 + ctx.param(CUTOFF) + dyn_oct).exp2(); // 120 Hz … ~2.2 kHz, plus offsets
            let res = (res + ctx.param(RESONANCE) + dyn_res).clamp(0.0, 1.05);
            self.level += (level - self.level) * self.param_smooth;
            let ps = self.param_smooth;
            self.delay.wet += (ctx.param(DELAY_MIX) - self.delay.wet) * ps;
            self.delay.feedback += (ctx.param(FEEDBACK) - self.delay.feedback) * ps;
            self.cutoff += (target - self.cutoff) * self.cutoff_smooth;

            self.drive += (ctx.param(DRIVE) - self.drive) * ps;
            let drive = self.character.drive * self.drive;
            let dry = self.voice.tick(&self.character, drive, self.cutoff, res, sr) * 0.38 * self.level;
            let (wl, wr) = self.delay.tick(dry, delay_samples);
            frame[0] = ((dry + wl).tanh() * CEILING) as f32;
            frame[1] = ((dry + wr).tanh() * CEILING) as f32;
        }
        self.clock.finish(steps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gen_api::{Scale, Score, ScoreBar, Tonality, Transport};

    const SR: f64 = 48_000.0;
    const BEAT: usize = 24_000; // 120 bpm

    /// A score whose bars 0..16 all carry `mark` (at intensity 1) and `feel`.
    fn score(mark: Mark, feel: Feel) -> Score {
        let mut s = Score::EMPTY;
        for (i, b) in s.bars.iter_mut().enumerate().take(16) {
            *b = ScoreBar { mark, intensity: 1.0, mark_start: 0, mark_bars: 16, feel, ..ScoreBar::NONE };
            if mark == Mark::Normal {
                b.mark_start = i as u64;
                b.mark_bars = 1;
            }
        }
        s
    }

    /// Renders `bars` bars; returns the audio and how many events came out.
    fn render(bars: usize, score: &Score) -> (Vec<f32>, usize) {
        let mut g = Acid::new(&Config { sample_rate: SR, seed: 7 });
        let ch = g.info().channels as usize;
        let params: Vec<f64> = g.params().iter().map(|p| p.default).collect();
        let block = 480;
        let mut out = vec![0.0f32; bars * 4 * BEAT * ch];
        let mut events = Vec::new();
        for (b, chunk) in out.chunks_mut(block * ch).enumerate() {
            let ctx = Context {
                score,
                sample_rate: SR,
                transport: Transport { bpm: 120.0, beat: (b * block) as f64 / BEAT as f64, beats_per_bar: 4, playing: true },
                tonality: Tonality { root: 4, scale: Scale::Phrygian },
                params: &params,
                input: &[],
            };
            g.process(&ctx, chunk);
            g.take_events(&mut events);
        }
        (out, events.len())
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn deterministic_and_in_range_under_every_mark_and_feel() {
        for mark in Mark::ALL {
            for feel in Feel::ALL {
                let s = score(mark, feel);
                let (a, _) = render(4, &s);
                assert_eq!(a, render(4, &s).0, "{mark:?} {feel:?}");
                assert!(a.iter().all(|x| x.is_finite() && x.abs() <= 1.0), "{mark:?} {feel:?}");
            }
        }
    }

    #[test]
    fn quiet_and_break_are_quieter() {
        let normal = rms(&render(8, &Score::EMPTY).0);
        assert!(normal > 0.01);
        assert!(rms(&render(8, &score(Mark::Quiet, Feel::Normal)).0) < normal * 0.8);
        assert!(rms(&render(8, &score(Mark::Break, Feel::Normal)).0) < normal);
    }

    /// Renders one bar of `feel` over a hand-set cycle where every step is
    /// a plain gated note; returns how many notes the voice started.
    fn notes_in_one_bar(feel: Feel) -> u64 {
        let s = score(Mark::Normal, feel);
        let mut g = Acid::new(&Config { sample_rate: SR, seed: 7 });
        g.cycle = Some(Cycle { index: 0, steps: [Step { gate: true, degree: 0, accent: false, slide: false }; CYCLE_STEPS] });
        let params: Vec<f64> = g.params().iter().map(|p| p.default).collect();
        let block = 480;
        let mut out = vec![0.0f32; block * 2];
        for b in 0..4 * BEAT / block {
            let ctx = Context {
                score: &s,
                sample_rate: SR,
                transport: Transport { bpm: 120.0, beat: (b * block) as f64 / BEAT as f64, beats_per_bar: 4, playing: true },
                tonality: Tonality { root: 4, scale: Scale::Phrygian },
                params: &params,
                input: &[],
            };
            g.process(&ctx, &mut out);
        }
        g.voice.notes
    }

    #[test]
    fn notes_per_beat_follow_the_feel() {
        assert_eq!(notes_in_one_bar(Feel::Normal), 16);
        assert_eq!(notes_in_one_bar(Feel::Half), 8);
        assert_eq!(notes_in_one_bar(Feel::Triplet), 12, "three per beat");
        assert_eq!(notes_in_one_bar(Feel::Double), 32, "two per 16th");
        assert_eq!(notes_in_one_bar(Feel::Quad), 32, "quad plays like double");
    }

    #[test]
    fn double_plays_the_cycle_twice_in_four_bars() {
        // One composed cycle (4 bars) of the seed's own line, at double
        // speed: the 64-step cycle runs through twice.
        let s = score(Mark::Normal, Feel::Double);
        let mut g = Acid::new(&Config { sample_rate: SR, seed: 7 });
        let params: Vec<f64> = g.params().iter().map(|p| p.default).collect();
        let block = 480;
        let mut out = vec![0.0f32; block * 2];
        for b in 0..16 * BEAT / block {
            let ctx = Context {
                score: &s,
                sample_rate: SR,
                transport: Transport { bpm: 120.0, beat: (b * block) as f64 / BEAT as f64, beats_per_bar: 4, playing: true },
                tonality: Tonality { root: 4, scale: Scale::Phrygian },
                params: &params,
                input: &[],
            };
            g.process(&ctx, &mut out);
        }
        let c = g.cycle.as_ref().unwrap();
        assert_eq!(c.index, 0);
        let gated = c.steps.iter().filter(|s| s.gate).count() as u64;
        assert!(gated > 0 && gated < CYCLE_STEPS as u64);
        assert_eq!(g.voice.notes, 2 * gated);
    }

    /// Note frequencies played over `bars` bars of seed 7 with `params`.
    fn note_log(bars: usize, s: &Score, tempo: bool) -> Vec<f64> {
        let mut g = Acid::new(&Config { sample_rate: SR, seed: 7 });
        let mut params: Vec<f64> = g.params().iter().map(|p| p.default).collect();
        params[TEMPO] = if tempo { 1.0 } else { 0.0 };
        let block = 480;
        let mut out = vec![0.0f32; block * 2];
        for b in 0..bars * 4 * BEAT / block {
            let ctx = Context {
                score: s,
                sample_rate: SR,
                transport: Transport { bpm: 120.0, beat: (b * block) as f64 / BEAT as f64, beats_per_bar: 4, playing: true },
                tonality: Tonality { root: 4, scale: Scale::Phrygian },
                params: &params,
                input: &[],
            };
            g.process(&ctx, &mut out);
        }
        g.voice.log
    }

    #[test]
    fn a_double_bar_plays_two_bars_of_the_line_in_order() {
        let normal = note_log(2, &Score::EMPTY, true);
        let double = note_log(1, &score(Mark::Normal, Feel::Double), true);
        assert!(normal.len() > 8);
        assert_eq!(double, normal, "the same notes, in the same order, twice as fast");
    }

    #[test]
    fn tempo_off_ignores_the_feel() {
        let plain = note_log(4, &Score::EMPTY, false);
        for feel in [Feel::Half, Feel::Double, Feel::Quad, Feel::Triplet] {
            assert_eq!(note_log(4, &score(Mark::Normal, feel), false), plain, "{feel:?}");
        }
    }

    #[test]
    fn other_feels_leave_earlier_bars_untouched() {
        // Normal feel goes the old way; a feel change from bar 2 on doesn't
        // reach back into bars 0-1.
        let empty = render(4, &Score::EMPTY).0;
        assert_eq!(empty, render(4, &score(Mark::Normal, Feel::Normal)).0);
        let two_bars = 2 * 4 * BEAT * 2;
        for feel in [Feel::Triplet, Feel::Double, Feel::Quad] {
            let mut s = Score::EMPTY;
            for b in s.bars.iter_mut().skip(2).take(2) {
                b.feel = feel;
            }
            let a = render(4, &s).0;
            assert_eq!(a[..two_bars], empty[..two_bars], "{feel:?}");
            assert_ne!(a[two_bars..], empty[two_bars..], "{feel:?} changes bars 2-3");
        }
    }
}
