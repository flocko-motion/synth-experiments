//! psybass — generative psytrance kick + bass.
//!
//! Follows the host's transport: a 16th-note boundary in the host's position
//! triggers that slot of the current 8-bar phrase. A phrase is composed when
//! the position enters it, using the host's tonality, so fills land on the
//! host's bar numbers and key changes are picked up immediately.
//!
//! Chords from the workspace score: where a bar has one, the bass line moves
//! to the chord root, its notes counted as scale degrees from the chord's
//! degree. The kick stays on the key root.
//!
//! Dynamics and feel from the score, per 16th: `break` takes kick and bass
//! out (no duck events either); `quiet` drops bass notes and turns both
//! down, by its intensity; `rise` opens the bass filter towards the
//! region's end; `peak` opens it a bit and plays at full velocity. Half
//! feel puts the kick on beats 1 and 3 and the bass on the 8th off-beats.
//! Double and quad feel gallop the bass in 32nds: every 16th it plays
//! becomes two 32nds at its pitch, the second softer. Triplet feel rolls it
//! in 16th triplets: the five slots after the kick's take the notes of the
//! beat's 16ths (the one each falls in, or the nearest that plays). Under
//! every feel but half the kick stays four on the floor with the same duck
//! events, the bass leaves the kick's 16th alone, and the big fill keeps
//! its straight 16ths.
//!
//! Turn `tempo` off to keep the kick four on the floor in every feel.
//!
//! Assumes 4/4, as psytrance does.

use dsp::{saw, SubClock, Svf};
use gen_api::{Config, Context, Entry, Event, EventKind, Feel, Generator, Info, Mark, Param, Rng, ScoreBar, StepClock, Tonality};
use std::f64::consts::PI;

const PHRASE_BARS: u64 = 8;
const PHRASE_SLOTS: usize = PHRASE_BARS as usize * 16;

/// Fixed output stage: soft clip, then a -1 dBFS ceiling.
const DRIVE: usize = 0;

/// The built-in output saturation, exposed.
/// Drive moves slowly when randomized: a sudden jump in kick and bass
/// saturation is a level jump.
const TEMPO: usize = 1;
const PARAMS: [Param; 2] = [
    Param::continuous("drive", "Drive", 0.5, 4.0, 1.25).glide(2.0),
    // The kick follows the score's tempo feel (8ths on double, 16ths on
    // quad, beats 1 and 3 on half); off, it stays four on the floor. The
    // bass dodges whatever the kick plays either way.
    Param::toggle("tempo", "Tempo", true).glide(0.0),
];
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

/// The big fill: the last two beats of every 16th bar (kick out, a run
/// on straight 16ths). Same rule as in `compose`.
fn big_fill(beat_abs: u64) -> bool {
    (beat_abs / 4 + 1).is_multiple_of(16) && beat_abs % 4 >= 2
}

/// Subdivisions per beat the bass plays at on beat `beat_abs`: 8 (32nds)
/// in double and quad feel, 6 (16th triplets) in triplet feel, otherwise
/// and in the big fill 4, the StepClock's 16ths.
fn sub_rate(ctx: &Context, beat_abs: u64) -> u64 {
    if big_fill(beat_abs) {
        return 4;
    }
    match ctx.score.bar(beat_abs / 4).feel {
        Feel::Double | Feel::Quad => 8,
        Feel::Triplet => 6,
        Feel::Normal | Feel::Half => 4,
    }
}

/// The score's marks on a bass note: `quiet` thins (when `draw` is under
/// half its intensity) and softens, `rise` opens the filter by `progress`,
/// `peak` opens it a bit and plays at full velocity. `break` is handled by
/// the caller.
fn shape(note: Option<NoteEvent>, bar: &ScoreBar, draw: f64, progress: f64) -> Option<NoteEvent> {
    let intensity = bar.intensity as f64;
    let mut note = note;
    match bar.mark {
        Mark::Break => note = None,
        Mark::Quiet => {
            if draw < 0.5 * intensity {
                note = None;
            }
            if let Some(ev) = &mut note {
                ev.vel *= 1.0 - 0.45 * intensity;
                ev.cutoff *= 1.0 - 0.3 * intensity;
            }
        }
        Mark::Rise => {
            if let Some(ev) = &mut note {
                ev.cutoff *= 1.0 + 2.0 * progress * intensity;
                ev.env_amt *= 1.0 + progress * intensity;
            }
        }
        Mark::Peak => {
            if let Some(ev) = &mut note {
                ev.cutoff *= 1.0 + 0.5 * intensity;
                ev.vel = ev.vel.max(0.95);
            }
        }
        Mark::Normal => {}
    }
    note
}

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
    gain: f64,
    hp_in: f64,
    hp_out: f64,
}

impl Kick {
    fn trigger(&mut self, len: usize, root_hz: f64, gain: f64) {
        *self = Kick { len, root_hz, gain, ..Kick::default() };
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
        ((body * amp * 1.8).tanh() + click) * 0.95 * self.gain
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
    /// 32nds and 16th triplets for the bass in double, quad and triplet
    /// feel.
    sub: SubClock,
    /// Bass notes played so far, and how many started in the kick's 16th
    /// (for tests).
    notes: u64,
    notes_on_kick: u64,
    /// Seeds the per-16th draws that thin the line out in quiet parts.
    thin_seed: u64,
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

    fn ensure_phrase(&mut self, index: u64, t: &Tonality) {
        if self.phrase.as_ref().map(|p| p.index) != Some(index) {
            self.phrase = Some(self.compose(index, t));
        }
    }

    /// A StepClock 16th: the kick, and the bass unless the SubClock plays
    /// this beat (double, quad and triplet feel).
    fn trigger(&mut self, slot: u64, frame: usize, ctx: &Context) {
        self.ensure_phrase(slot / PHRASE_SLOTS as u64, &ctx.tonality);
        let ph = self.phrase.as_ref().unwrap();
        let i = (slot % PHRASE_SLOTS as u64) as usize;
        let (mut kick, mut note) = (ph.kicks[i], ph.notes[i]);
        let beat_kick = ph.kicks[i - i % 4];
        if sub_rate(ctx, slot / 4) != 4 {
            note = None;
        }

        // The score: dynamics and feel at this 16th.
        let bar = ctx.score.bar(slot / 16);
        // Double feel: the kick on every 8th, quad: on every 16th, in beats
        // that have one (not the big fill's last two). With `tempo` off the
        // kick plays every bar as normal feel.
        let kick_feel = if ctx.param(TEMPO) != 0.0 { bar.feel } else { Feel::Normal };
        match kick_feel {
            Feel::Double => kick = beat_kick && slot % 2 == 0,
            Feel::Quad => kick = beat_kick,
            _ => {}
        }
        let intensity = bar.intensity as f64;
        let progress = ctx.score.progress(slot as f64 / 4.0, 4);
        let draw = Rng::new(self.thin_seed ^ slot.wrapping_mul(0x9E37_79B9_7F4A_7C15)).f();
        let mut kick_gain = 1.0;
        match bar.mark {
            Mark::Break => kick = false,
            Mark::Quiet => kick_gain = 1.0 - 0.4 * intensity,
            _ => {}
        }
        note = shape(note, &bar, draw, progress);
        if bar.feel == Feel::Half {
            kick &= kick_feel != Feel::Half || slot % 8 == 0;
            if slot % 4 != 2 {
                note = None;
            }
        }

        let six = 60.0 / ctx.transport.bpm / 4.0;
        let root = root_hz(&ctx.tonality);
        if kick {
            // A 16th apart in quad feel: end before the next one.
            let len = if kick_feel == Feel::Quad { six * 0.9 } else { six * 1.05 };
            let level = if slot.is_multiple_of(4) { 1.0 } else { 0.85 };
            self.kick.trigger((len * self.sr) as usize, root, kick_gain * level);
            // Fills without kick send nothing, so nothing ducks there. The
            // extra kicks of double and quad duck less, so it doesn't choke.
            let strength = if slot.is_multiple_of(4) { kick_gain } else { 0.6 * kick_gain };
            self.events.push(Event { frame: frame as u32, kind: EventKind::Duck { strength: strength as f32 } });
        }
        if let Some(ev) = note {
            self.play(ev, slot / 16, six, slot.is_multiple_of(4), ctx);
        }
    }

    /// SubClock subdivision `n` at `per_beat` (8 or 6) per beat: a 32nd of
    /// the gallop, or a 16th-triplet slot, bass only.
    fn trigger_sub(&mut self, n: u64, per_beat: u64, ctx: &Context) {
        let (beat, k) = (n / per_beat, n % per_beat);
        let beat_slot = beat * 4;
        self.ensure_phrase(beat_slot / PHRASE_SLOTS as u64, &ctx.tonality);
        let ph = self.phrase.as_ref().unwrap();
        let i = (beat_slot % PHRASE_SLOTS as u64) as usize;
        let six = 60.0 / ctx.transport.bpm / 4.0;
        // (note, its 16th's slot, the quiet draw's seed, length)
        // What the kick plays in this beat: the bass leaves those 32nds free.
        let kick_follows = ctx.param(TEMPO) != 0.0;
        let quad = kick_follows && ctx.score.bar(beat_slot / 16).feel == Feel::Quad;
        let kick_on_8ths = kick_follows && !quad;
        let (note, slot, seed, len) = if per_beat == 8 {
            // The 16th this 32nd halves; both halves share its quiet draw.
            // Never on a kick: in double the kick also takes 32nd 4 (the
            // 8th), in quad every even 32nd, so quad plays the four odd
            // ones, taking the beat's first note where its 16th has none.
            let s = (k / 2) as usize;
            let mut note = match (quad, s, k) {
                (true, _, k) if k % 2 == 0 => None,
                (true, s, _) => ph.notes[i + s].or_else(|| [1, 2, 3].iter().find_map(|&j| ph.notes[i + j])),
                (false, 0, _) => None,
                (false, _, 4) if kick_on_8ths => None,
                (false, s, _) => ph.notes[i + s],
            };
            if let Some(ev) = &mut note {
                if k % 2 == 1 && !quad {
                    ev.vel *= 0.85;
                }
            }
            let slot = beat_slot + s as u64;
            (note, slot, slot.wrapping_mul(0x9E37_79B9_7F4A_7C15), six / 2.0)
        } else {
            // The 16th this slot falls in (slot 1 takes the first after the
            // kick's), or the nearest of the beat's that plays.
            let s = ((k * 4 / 6) as usize).max(1);
            let note = if k == 0 { None } else { [s, 1, 2, 3].iter().find_map(|&j| ph.notes[i + j]) };
            (note, beat_slot + s as u64, n.wrapping_mul(0xD1B5_4A32_D192_ED03) ^ 0x3A9C_5E1D, six * 4.0 / 6.0)
        };
        let bar = ctx.score.bar(slot / 16);
        let pos = n as f64 / per_beat as f64;
        let draw = Rng::new(self.thin_seed ^ seed).f();
        let note = shape(note, &bar, draw, ctx.score.progress(pos, 4));
        if let Some(mut ev) = note {
            // Fit the gate, release included, inside the shorter note.
            ev.gate = ev.gate.min(1.0 - RELEASE / len);
            let on_kick = match (per_beat, quad) {
                (8, true) => k % 2 == 0,
                (8, false) if kick_on_8ths => k % 4 == 0,
                (8, false) => k == 0,
                _ => k == 0,
            };
            self.play(ev, slot / 16, len, on_kick, ctx);
        }
    }

    /// Plays bass note `ev` in `bar`, `len` seconds long (gate and envelope
    /// are fractions of it); `on_kick`: it starts in the kick's 16th.
    fn play(&mut self, ev: NoteEvent, bar: u64, len: f64, on_kick: bool, ctx: &Context) {
        // The kick stays on the key root. Over a chord from the score the
        // bass line moves to the chord root, its notes counted as scale
        // degrees from the chord's (notes that aren't a degree of the
        // current key, after a key change mid-phrase, just move with the
        // root).
        let key = ctx.tonality;
        let root = root_hz(&key);
        let chord = ctx.score.bar(bar).chord;
        let freq = match (chord, key.degree_of(ev.semis)) {
            (None, _) => root * 2f64.powf(ev.semis as f64 / 12.0),
            (Some(_), Some(d)) => {
                let (pc, semis) = key.chord_degree(chord, d);
                root_hz(&Tonality { root: pc, ..key }) * 2f64.powf(semis as f64 / 12.0)
            }
            (Some(c), None) => root_hz(&Tonality { root: c.root, ..key }) * 2f64.powf(ev.semis as f64 / 12.0),
        };
        self.bass.trigger(freq, &ev, len, self.res, self.sr);
        self.notes += 1;
        if on_kick {
            self.notes_on_kick += 1;
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
            sub: SubClock::new(),
            notes: 0,
            notes_on_kick: 0,
            thin_seed: config.seed ^ 0x7417,
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

    /// Kick and rolling bass; sorted with the bass.
    fn category(&self) -> &'static str {
        "bass"
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
        let tr = &ctx.transport;
        let spb = tr.samples_per_beat(ctx.sample_rate);
        for (i, o) in out.iter_mut().enumerate() {
            while let Some(slot) = steps.due(i) {
                self.trigger(slot, i, ctx);
            }
            // The SubClock ticks every sample to stay in sync; it plays the
            // bass only on the beats sub_rate gives it.
            let beat = tr.beat + i as f64 / spb;
            let rate = sub_rate(ctx, (beat + 1e-9).max(0.0) as u64);
            if let Some(n) = self.sub.tick(beat, rate, spb, tr.playing) {
                if rate != 4 {
                    self.trigger_sub(n, rate, ctx);
                }
            }
            let x = self.kick.tick(self.sr, &mut self.noise) + self.bass.tick(self.sr);
            self.drive += (ctx.param(DRIVE) - self.drive) * self.param_smooth;
            *o = ((x * self.drive).tanh() * CEILING) as f32;
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
        let r = render_full(bars, score);
        (r.audio, r.events.len())
    }

    struct Rendered {
        audio: Vec<f32>,
        /// (block, frame, duck strength) of every event.
        events: Vec<(usize, u32, f32)>,
        notes: u64,
        notes_on_kick: u64,
    }

    /// `r` has `kicks` times the kicks (duck events) of `normal`: the beat's
    /// own, unchanged, plus the in-between ones at 0.6 of their strength.
    fn more_kicks(r: &Rendered, normal: &Rendered, kicks: usize, what: &str) {
        assert_eq!(r.events.len(), kicks * normal.events.len(), "{what}: kicks");
        assert!(normal.events.iter().all(|e| r.events.contains(e)), "{what}: the beat's kicks stay");
        let full = normal.events.iter().map(|e| e.2).fold(0.0, f32::max);
        let extra = r.events.iter().filter(|e| !normal.events.contains(e));
        assert!(extra.clone().all(|e| (e.2 - 0.6 * full).abs() < 1e-6), "{what}: in-between kicks duck at 0.6");
    }

    fn render_full(bars: usize, score: &Score) -> Rendered {
        render_with(bars, score, true)
    }

    /// `tempo`: the kick follows the feel.
    fn render_with(bars: usize, score: &Score, tempo: bool) -> Rendered {
        let mut g = Psybass::new(&Config { sample_rate: SR, seed: 7 });
        let ch = g.info().channels as usize;
        let mut params: Vec<f64> = g.params().iter().map(|p| p.default).collect();
        params[TEMPO] = tempo as u8 as f64;
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
            let mut evs = Vec::new();
            g.take_events(&mut evs);
            for e in evs {
                let EventKind::Duck { strength } = e.kind;
                events.push((b, e.frame, strength));
            }
        }
        Rendered { audio: out, events, notes: g.notes, notes_on_kick: g.notes_on_kick }
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
    fn break_takes_kick_and_bass_out() {
        let (normal, ducks) = render(4, &Score::EMPTY);
        let (brk, brk_ducks) = render(4, &score(Mark::Break, Feel::Normal));
        assert!(ducks > 0 && rms(&normal) > 0.05);
        assert_eq!(brk_ducks, 0, "no kick, no duck events");
        assert!(rms(&brk) < 1e-6);
    }

    #[test]
    fn quiet_is_quieter_and_half_time_kicks_on_1_and_3() {
        let (normal, ducks) = render(4, &Score::EMPTY);
        assert!(rms(&render(4, &score(Mark::Quiet, Feel::Normal)).0) < rms(&normal) * 0.85);
        assert_eq!(render(4, &score(Mark::Normal, Feel::Half)).1 * 2, ducks);
    }

    #[test]
    fn double_gallops_and_triplet_rolls_with_the_kick_untouched() {
        // 4 bars: no fills. Normal plays the pattern's notes per beat.
        let normal = render_full(4, &score(Mark::Normal, Feel::Normal));
        assert_eq!(normal.audio, render(4, &Score::EMPTY).0, "normal feel is no score");
        assert!(normal.notes > 0);
        for (feel, kicks) in [(Feel::Double, 2), (Feel::Quad, 4)] {
            let r = render_full(4, &score(Mark::Normal, feel));
            assert!(r.notes > normal.notes, "{feel:?}: 32nds");
            assert_eq!(r.notes_on_kick, 0, "{feel:?}: no note starts on a kick");
            more_kicks(&r, &normal, kicks, &format!("{feel:?}"));
        }
        assert_eq!(render_full(4, &score(Mark::Normal, Feel::Quad)).notes, 4 * 16, "quad: the four odd 32nds of every beat");
        let r = render_full(4, &score(Mark::Normal, Feel::Triplet));
        assert_eq!(r.notes, 5 * 16, "five per beat");
        assert_eq!(r.notes_on_kick, 0, "slot 0 never plays");
        assert_eq!(r.events, normal.events, "triplet: same kicks, same ducks");
    }

    #[test]
    fn tempo_off_keeps_four_on_the_floor_and_the_bass_still_dodges_it() {
        let normal = render_full(16, &score(Mark::Normal, Feel::Normal));
        for feel in [Feel::Half, Feel::Double, Feel::Quad, Feel::Triplet] {
            let r = render_with(16, &score(Mark::Normal, feel), false);
            assert_eq!(r.events, normal.events, "{feel:?}: the same kicks as normal feel");
            // Only the big fill's run, where there's no kick, starts on a
            // beat; half feel keeps the bass on the 8th off-beats, even there.
            assert_eq!(r.notes_on_kick, if feel == Feel::Half { 0 } else { 2 }, "{feel:?}");
        }
        let follow = render_full(4, &score(Mark::Normal, Feel::Double));
        let off = render_with(4, &score(Mark::Normal, Feel::Double), false);
        assert!(off.notes > follow.notes, "the 8th's 32nd is free again with the kick on the beat only");
    }

    #[test]
    fn kick_and_ducks_hold_under_every_mark_with_fast_feels() {
        // 16 bars take in the small fill (bar 8) and the big fill (bar 16).
        for mark in Mark::ALL {
            let normal = render_full(16, &score(mark, Feel::Normal));
            for (feel, kicks) in [(Feel::Double, 2), (Feel::Quad, 4), (Feel::Triplet, 1)] {
                let r = render_full(16, &score(mark, feel));
                more_kicks(&r, &normal, kicks, &format!("{mark:?} {feel:?}"));
                assert!(r.audio.iter().all(|x| x.is_finite() && x.abs() <= 1.0), "{mark:?} {feel:?}");
                // Only the big fill's run, where there's no kick, starts
                // in a kick's 16th: once on each of its two beats.
                let fill = match mark {
                    Mark::Normal => 2,
                    Mark::Break => 0,
                    _ => r.notes_on_kick.min(2), // quiet may thin it
                };
                assert_eq!(r.notes_on_kick, fill, "{mark:?} {feel:?}");
            }
            if mark == Mark::Break {
                assert_eq!(render_full(16, &score(mark, Feel::Triplet)).notes, 0);
            }
        }
    }
}
