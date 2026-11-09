//! psybass — generative psytrance kick + bass.
//!
//! Follows the host's transport: a 16th-note boundary in the host's position
//! triggers that slot of the current 8-bar phrase. A phrase is composed when
//! the position enters it, using the host's tonality, so fills land on the
//! host's bar numbers and key changes are picked up immediately.
//!
//! Assumes 4/4, as psytrance does.

use dsp::{saw, Svf};
use gen_api::{Config, Context, Entry, Event, EventKind, Generator, Info, Param, Rng, StepClock, Tonality};
use std::f64::consts::PI;

const PHRASE_BARS: u64 = 8;
const PHRASE_SLOTS: usize = PHRASE_BARS as usize * 16;

/// Fixed output stage: soft clip, then a -1 dBFS ceiling.
const DRIVE: usize = 0;

/// The built-in output saturation, exposed.
const PARAMS: [Param; 1] = [Param::continuous("drive", "Drive", 0.5, 4.0, 1.25)];
const CEILING: f64 = 0.891;

/// Which of the three off-kick 16ths in a beat carry a bass note.
/// Slot 0 always belongs to the kick. Weights are relative.
const PATTERNS: [(&str, [bool; 3], f64); 5] = [
    ("rolling  K B B B", [true, true, true], 5.0),
    ("gallop   K - B B", [false, true, true], 3.0),
    ("skip     K B - B", [true, false, true], 1.5),
    ("triplet  K B B -", [true, true, false], 1.0),
    // Um-pah: offbeat-only bass is a house/prog figure, not psy. Kept as a
    // rare change of texture (~2% of phrases).
    ("offbeat  K - B -", [false, true, false], 0.2),
];

// ---------------------------------------------------------------- composition

#[derive(Clone, Copy)]
struct NoteEvent {
    semis: i32,
    vel: f64,
    gate: f64,      // fraction of a 16th
    env_decay: f64, // fraction of a 16th
    cutoff: f64,    // base cutoff Hz
    env_amt: f64,   // filter env depth Hz
}

struct Phrase {
    index: u64,
    kicks: [bool; PHRASE_SLOTS],
    notes: [Option<NoteEvent>; PHRASE_SLOTS],
}

/// Bass register root: D1 (36.7 Hz) up to C#2 (69.3 Hz).
fn root_hz(t: &Tonality) -> f64 {
    let midi = 26 + (t.root as i32 + 10) % 12;
    440.0 * 2f64.powf((midi - 69) as f64 / 12.0)
}

// ---------------------------------------------------------------- voices

/// Short, punchy kick tuned to the root, about one 16th long so it's gone
/// before the first bass note lands.
#[derive(Default)]
struct Kick {
    i: usize,
    len: usize,
    phase: f64,
    root_hz: f64,
    hp_in: f64,
    hp_out: f64,
}

impl Kick {
    fn trigger(&mut self, len: usize, root_hz: f64) {
        *self = Kick { len, root_hz, ..Kick::default() };
    }

    fn tick(&mut self, sr: f64, noise: &mut Rng) -> f64 {
        if self.i >= self.len {
            return 0.0;
        }
        let t = self.i as f64 / sr;
        let u = self.i as f64 / self.len as f64;
        self.i += 1;
        // two-stage pitch drop: fast click sweep + body sweep
        let f = self.root_hz + 190.0 * (-t / 0.018).exp() + 900.0 * (-t / 0.0018).exp();
        self.phase += f / sr;
        let body = (2.0 * PI * self.phase).sin();
        let attack = (t / 0.0004).min(1.0);
        let amp = attack * (1.0 - u).powf(1.6);
        // click: highpassed noise burst
        let n = noise.range(-1.0, 1.0);
        let hp = 0.9 * (self.hp_out + n - self.hp_in);
        self.hp_in = n;
        self.hp_out = hp;
        let click = hp * (-t / 0.0015).exp() * 0.35;
        ((body * amp * 1.8).tanh() + click) * 0.95
    }
}

const RELEASE: f64 = 0.004;

/// Mono bass voice: two detuned saws into a resonant 4-pole lowpass.
/// Notes are shorter than a 16th, so retriggering never cuts one off.
#[derive(Default)]
struct Bass {
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

impl Bass {
    fn trigger(&mut self, freq: f64, ev: &NoteEvent, six: f64, res: f64, sr: f64) {
        *self = Bass {
            active: true,
            dt: freq / sr,
            p2: 0.37,
            gate: six * ev.gate,
            env_decay: six * ev.env_decay,
            vel: ev.vel,
            cutoff: ev.cutoff,
            env_amt: ev.env_amt,
            res,
            ..Bass::default()
        };
    }

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

pub struct Psybass {
    sr: f64,
    rng: Rng,
    noise: Rng,
    res: f64,
    cut_from: f64,
    phrase: Option<Phrase>,
    clock: StepClock,
    drive: f64,
    param_smooth: f64,
    /// Duck events for the block being rendered: one per kick played.
    events: Vec<Event>,
    kick: Kick,
    bass: Bass,
}

impl Psybass {
    fn compose(&mut self, index: u64, t: &Tonality) -> Phrase {
        let rng = &mut self.rng;
        let start_bar = index * PHRASE_BARS;

        let weights: Vec<f64> = PATTERNS.iter().map(|p| p.2).collect();
        let (pname, pat, _) = PATTERNS[rng.weighted(&weights)];

        // 2-bar motif: mostly the root, spiced with octave, fifth, second,
        // third and the seventh below.
        let spice_notes = [12, 12, t.degree(4), t.degree(1), t.degree(2), t.degree(-1)];
        let fill_notes = [0, t.degree(1), t.degree(2), t.degree(4), 12];
        let mut motif = [0i32; 32];
        let spice = rng.range(0.04, 0.18);
        for m in motif.iter_mut() {
            if rng.chance(spice) {
                *m = *rng.pick(&spice_notes);
            }
        }
        let accent = (rng.next_u64() % 3) as usize + 1;

        let cut_from = self.cut_from;
        let cut_to = rng.range(160.0, 520.0);
        let env_amt = rng.range(700.0, 2600.0);
        let env_decay = rng.range(0.25, 0.7);
        let gate = rng.range(0.55, 0.9);

        eprintln!(
            "  bars {:>4}-{:<4} {}  cutoff {:>3.0}->{:<3.0}Hz",
            start_bar + 1,
            start_bar + PHRASE_BARS,
            pname,
            cut_from,
            cut_to
        );

        let mut ph = Phrase { index, kicks: [false; PHRASE_SLOTS], notes: [None; PHRASE_SLOTS] };
        for bar_in in 0..PHRASE_BARS as usize {
            let last_bar = bar_in == PHRASE_BARS as usize - 1;
            let big_fill = last_bar && (start_bar as usize + bar_in + 1) % 16 == 0;
            let small_fill = last_bar && !big_fill && rng.chance(0.6);

            for beat in 0..4 {
                let beat_slot = bar_in * 16 + beat * 4;
                let in_fill = (big_fill && beat >= 2) || (small_fill && beat == 3);
                // big fill: no kick for the last two beats
                ph.kicks[beat_slot] = !(big_fill && beat >= 2);

                for s in 0..4 {
                    let slot = beat_slot + s;
                    let cutoff = cut_from + (cut_to - cut_from) * slot as f64 / PHRASE_SLOTS as f64;
                    let (play, semis) = if in_fill && big_fill {
                        // rising run through the scale on straight 16ths
                        (true, t.degree(((beat - 2) * 4 + s) as i32))
                    } else if in_fill {
                        (s != 0 || rng.chance(0.3), *rng.pick(&fill_notes))
                    } else if s == 0 {
                        (false, 0)
                    } else {
                        (pat[s - 1], motif[slot % 32])
                    };
                    if !play {
                        continue;
                    }
                    let vel = if s == accent { 1.0 } else { rng.range(0.82, 0.95) };
                    ph.notes[slot] = Some(NoteEvent {
                        semis,
                        vel,
                        gate,
                        env_decay,
                        cutoff: if in_fill { cutoff * 1.6 } else { cutoff },
                        env_amt,
                    });
                }
            }
        }
        self.cut_from = cut_to;
        ph
    }

    fn trigger(&mut self, slot: u64, frame: usize, ctx: &Context) {
        let index = slot / PHRASE_SLOTS as u64;
        if self.phrase.as_ref().map(|p| p.index) != Some(index) {
            self.phrase = Some(self.compose(index, &ctx.tonality));
        }
        let ph = self.phrase.as_ref().unwrap();
        let i = (slot % PHRASE_SLOTS as u64) as usize;
        let (kick, note) = (ph.kicks[i], ph.notes[i]);

        let six = 60.0 / ctx.transport.bpm / 4.0;
        let root = root_hz(&ctx.tonality);
        if kick {
            self.kick.trigger((six * 1.05 * self.sr) as usize, root);
            // Fills without kick send nothing, so nothing ducks there.
            self.events.push(Event { frame: frame as u32, kind: EventKind::Duck { strength: 1.0 } });
        }
        if let Some(ev) = note {
            let freq = root * 2f64.powf(ev.semis as f64 / 12.0);
            self.bass.trigger(freq, &ev, six, self.res, self.sr);
        }
    }
}

impl Generator for Psybass {
    fn new(config: &Config) -> Self {
        let mut rng = Rng::new(config.seed);
        let res = rng.range(0.25, 0.6);
        let cut_from = rng.range(180.0, 400.0);
        Psybass {
            sr: config.sample_rate,
            rng,
            noise: Rng::new(config.seed ^ 0xC11C),
            res,
            cut_from,
            phrase: None,
            clock: StepClock::new(),
            drive: PARAMS[DRIVE].default,
            events: Vec::new(),
            param_smooth: 1.0 - (-1.0 / (0.02 * config.sample_rate)).exp(),
            kick: Kick::default(),
            bass: Bass::default(),
        }
    }

    fn info(&self) -> Info {
        Info {
            name: "psybass",
            description: "Psytrance kick + rolling bass, tuned to the host key",
            channels: 1,
        }
    }

    fn params(&self) -> &'static [Param] {
        &PARAMS
    }

    /// The kick is what everything else ducks away from.
    fn duck_sensitivity(&self) -> f64 {
        0.0
    }

    /// Kick and bass come in on the downbeat.
    fn entry(&self) -> Entry {
        Entry::Bar
    }

    fn take_events(&mut self, out: &mut Vec<Event>) {
        out.append(&mut self.events);
    }

    fn process(&mut self, ctx: &Context, out: &mut [f32]) {
        let mut steps = self.clock.block(ctx);
        for (i, o) in out.iter_mut().enumerate() {
            while let Some(slot) = steps.due(i) {
                self.trigger(slot, i, ctx);
            }
            let x = self.kick.tick(self.sr, &mut self.noise) + self.bass.tick(self.sr);
            self.drive += (ctx.param(DRIVE) - self.drive) * self.param_smooth;
            *o = ((x * self.drive).tanh() * CEILING) as f32;
        }
        self.clock.finish(steps);
    }
}
