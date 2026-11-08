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

use dsp::{decay_coef, midi_hz, saw, square, Ladder, OnePole};
use gen_api::{Config, Context, Generator, Info, Param, Rng, StepClock, BARS_PER_CYCLE, CYCLES_PER_EPOCH};
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

/// Offsets on top of the generator's own tweak, plus the built-in delay
/// and drive.
const PARAMS: [Param; 5] = [
    Param::continuous("cutoff", "Cutoff offset", -2.0, 2.0, 0.0).unit("oct"),
    Param::continuous("resonance", "Resonance offset", -0.4, 0.3, 0.0),
    Param::continuous("delay", "Delay mix", 0.0, 1.0, 0.4),
    Param::continuous("feedback", "Delay feedback", 0.0, 0.85, 0.42),
    // Multiplies the seed's own drive amount.
    Param::continuous("drive", "Drive", 0.25, 4.0, 1.0).unit("x"),
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
        }
    }

    fn note(&mut self, freq: f64, accent: bool, gate_samples: Option<usize>) {
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

// ---------------------------------------------------------------- generator

pub struct Acid {
    sr: f64,
    rng: Rng,
    character: Character,
    clock: StepClock,
    epoch: Option<Epoch>,
    cycle: Option<Cycle>,
    voice: Voice,
    delay: Delay,
    cutoff: f64,
    cutoff_smooth: f64,
    param_smooth: f64,
    drive: f64,
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

    fn trigger(&mut self, step: u64, ctx: &Context) {
        let index = step / CYCLE_STEPS as u64;
        if self.cycle.as_ref().map(|c| c.index) != Some(index) {
            self.cycle = Some(self.compose_cycle(index));
        }
        let st = self.cycle.as_ref().unwrap().steps[(step % CYCLE_STEPS as u64) as usize];
        if !st.gate {
            self.voice.rest();
            return;
        }
        let t = &ctx.tonality;
        // Bass register root is D1..C#2; the acid sits one or two octaves up.
        let root = 26 + (t.root as i32 + 10) % 12 + 12 * self.character.octave;
        let freq = midi_hz((root + t.degree(st.degree)) as f64);
        let six = ctx.transport.samples_per_beat(self.sr) / 4.0;
        let gate = if st.slide { None } else { Some((six * 0.55) as usize) };
        self.voice.note(freq, st.accent, gate);
        self.voice.slide_into = st.slide;
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
            epoch: None,
            cycle: None,
            cutoff: 300.0,
            cutoff_smooth: 1.0 - (-1.0 / (0.05 * sr)).exp(),
            param_smooth: 1.0 - (-1.0 / (0.02 * sr)).exp(),
            drive: PARAMS[DRIVE].default,
            rng,
        }
    }

    fn params(&self) -> &'static [Param] {
        &PARAMS
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

        for (i, frame) in out.chunks_exact_mut(2).enumerate() {
            while let Some(step) = steps.due(i) {
                self.trigger(step, ctx);
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
            let target = 120.0 * (v * 4.2 + ctx.param(CUTOFF)).exp2(); // 120 Hz … ~2.2 kHz, plus offset
            let res = (res + ctx.param(RESONANCE)).clamp(0.0, 1.05);
            let ps = self.param_smooth;
            self.delay.wet += (ctx.param(DELAY_MIX) - self.delay.wet) * ps;
            self.delay.feedback += (ctx.param(FEEDBACK) - self.delay.feedback) * ps;
            self.cutoff += (target - self.cutoff) * self.cutoff_smooth;

            self.drive += (ctx.param(DRIVE) - self.drive) * ps;
            let drive = self.character.drive * self.drive;
            let dry = self.voice.tick(&self.character, drive, self.cutoff, res, sr) * 0.38;
            let (wl, wr) = self.delay.tick(dry, delay_samples);
            frame[0] = ((dry + wl).tanh() * CEILING) as f32;
            frame[1] = ((dry + wr).tanh() * CEILING) as f32;
        }
        self.clock.finish(steps);
    }
}
