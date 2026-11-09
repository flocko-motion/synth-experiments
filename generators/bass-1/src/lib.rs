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
//! Under double/quad feel the kick plays 8ths/16ths (see `kick`), so the
//! gallop drops any 32nd that would land on one.
//!
//! The workspace score: out in a `break`, thinner in a `quiet`, off-8ths
//! only in half-time feel, the filter opening through a `rise` and a little
//! in a `peak`. Double and quad feel gallop in 32nds: every 16th the line
//! plays becomes two 32nds at its pitch, the second softer. Triplet feel
//! rolls in 16th triplets: the five slots after the kick's each play the
//! motif note of the 16th they fall in (a quiet keeps only those its
//! thinned pattern plays). The kick's 16th stays empty and the big fill
//! stays on straight 16ths under every feel.
//!
//! Notes are stored as scale degrees and every param applies on the next
//! note, so key changes and knob moves are heard right away.
//!
//! Chords from the workspace score: where a bar has one, the line moves to
//! the chord root and its scale degrees count from the chord's degree, so
//! spice notes stay in the key (a chord root outside the scale transposes
//! the line instead).

use dsp::{midi_hz, saw, SubClock, Svf};
use gen_api::{Config, Context, Entry, Feel, Generator, Info, Mark, Param, Rng, StepClock, Tonality, TICKS_PER_BEAT};

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

/// Scale degree `degree` of the line at `bar`: from the key's root, or
/// from the chord's root where the score has a chord.
fn note_midi(ctx: &Context, bar: u64, degree: i32) -> i32 {
    let key = ctx.tonality;
    let (root, semis) = key.chord_degree(ctx.score.bar(bar).chord, degree);
    root_midi(&Tonality { root, ..key }) + semis
}

/// Subdivisions per beat the line plays at on beat `beat_abs`: 8 (32nds)
/// in double and quad feel, 6 (16th triplets) in triplet feel, otherwise
/// and in the big fill 4, the StepClock's 16ths.
fn sub_rate(ctx: &Context, beat_abs: u64) -> u64 {
    let bpb = ctx.transport.beats_per_bar.max(1) as u64;
    if ctx.param(FILLS) != 0.0 && big_fill(beat_abs, bpb) {
        return TICKS_PER_BEAT as u64;
    }
    match ctx.score.bar(beat_abs / bpb).feel {
        Feel::Double | Feel::Quad => 8,
        Feel::Triplet => 6,
        Feel::Normal | Feel::Half => TICKS_PER_BEAT as u64,
    }
}

/// How a note divides its 16th.
#[derive(Clone, Copy, PartialEq)]
enum Div {
    Sixteenth,
    /// One of the gallop's two 32nds; `second` is the softer one.
    ThirtySecond { second: bool },
    /// 16th-triplet slot 0-5 of the beat.
    Triplet(u64),
}

impl Div {
    /// The note's length in 16ths.
    fn sixteenths(self) -> f64 {
        match self {
            Div::Sixteenth => 1.0,
            Div::ThirtySecond { .. } => 0.5,
            Div::Triplet(_) => 4.0 / 6.0,
        }
    }
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
    /// 32nds and 16th triplets in double, quad and triplet feel.
    sub: SubClock,
    voice: Voice,
    drive: f64,
    param_smooth: f64,
    /// Notes played so far, and how many started in the kick's 16th (for
    /// tests).
    notes: u64,
    notes_on_kick: u64,
    /// Notes dropped because a double/quad kick sits there (for tests).
    notes_dropped: u64,
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

    /// A StepClock 16th. Beats the SubClock plays (double, quad and
    /// triplet feel) are skipped here, so no position fires twice.
    fn trigger(&mut self, step: u64, ctx: &Context) {
        let tpb = TICKS_PER_BEAT as u64;
        if sub_rate(ctx, step / tpb) != tpb {
            return;
        }
        self.play(step, Div::Sixteenth, step as f64 / tpb as f64, false, ctx);
    }

    /// SubClock subdivision `n` at `per_beat` (8 or 6) per beat: a 32nd of
    /// the gallop or a 16th-triplet slot, played as part of the 16th it
    /// falls in.
    fn trigger_sub(&mut self, n: u64, per_beat: u64, ctx: &Context) {
        let tpb = TICKS_PER_BEAT as u64;
        let (beat, k) = (n / per_beat, n % per_beat);
        let (step, div) = match per_beat {
            8 => (n / 2, Div::ThirtySecond { second: k % 2 == 1 }),
            _ => (beat * tpb + k * tpb / per_beat, Div::Triplet(k)),
        };
        // Double-time kicks land on 8ths and quad kicks on 16ths (see
        // `kick`), so no 32nd may start on one: double drops the gallop's
        // first 32nd on 16th 2, quad plays only the 32nds between kicks.
        let on_kick = per_beat == 8
            && match ctx.score.at(n as f64 / 8.0, ctx.transport.beats_per_bar).feel {
                Feel::Quad => k % 2 == 0,
                _ => k % 4 == 0,
            };
        self.play(step, div, n as f64 / per_beat as f64, on_kick, ctx);
    }

    /// Plays the line at 16th `step`, divided as `div`; `beat_pos` is the
    /// note's exact position in quarter notes. `kick_mute`: the note falls
    /// on a kick and is dropped (counted in `notes_dropped`).
    fn play(&mut self, step: u64, div: Div, beat_pos: f64, kick_mute: bool, ctx: &Context) {
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
        // The workspace score: out in a break; a quiet thins the line to
        // the off-8th (above intensity 0.5) or a gallop; half-time feel
        // plays off-8ths only; a rise opens the filter towards its end, a
        // peak opens it a little.
        let sb = ctx.score.at(beat_pos, tr.beats_per_bar);
        let intensity = sb.intensity as f64;
        if sb.mark == Mark::Break {
            return;
        }
        let pattern = match sb.mark {
            Mark::Quiet if intensity > 0.5 => 4, // offbeat
            Mark::Quiet => pattern.max(1),       // gallop or sparser
            _ if sb.feel == Feel::Half => 4,
            _ => pattern,
        };
        let score_oct = match sb.mark {
            Mark::Rise => 1.5 * intensity * ctx.score.progress(beat_pos, tr.beats_per_bar),
            Mark::Peak => 0.4 * intensity,
            Mark::Quiet => -0.5 * intensity,
            _ => 0.0,
        };

        // (degree, velocity, cutoff boost)
        let note = if let Div::Triplet(t) = div {
            // The roll: slot 0 is the kick's; slots 1-5 take the motif note
            // of the 16th they fall in. A quiet keeps only the slots whose
            // 16th its thinned pattern plays.
            let small = fills && last_bar && ph.small_fill && beat_in_bar == bpb - 1;
            if t == 0 || (sb.mark == Mark::Quiet && !(s >= 1 && PATTERNS[pattern].0[s as usize - 1])) {
                None
            } else if small {
                Some((ph.fill[s as usize], 0.95, 1.6))
            } else {
                let m = ph.motif[(in_phrase % MOTIF_SLOTS as u64) as usize];
                let degree = if m.draw < ph.spice * ctx.param(SPICE) { m.spice } else { 0 };
                Some((degree, if t == 2 * ph.accent - 1 { 1.0 } else { m.vel }, 1.0))
            }
        } else if fills && big_fill(beat_abs, bpb) {
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
        if kick_mute {
            self.notes_dropped += 1;
            return;
        }
        // The gallop's second 32nd is the softer echo of the first.
        let vel = if div == (Div::ThirtySecond { second: true }) { vel * 0.85 } else { vel };

        let sr = self.sr;
        let six = 60.0 / tr.bpm / tpb as f64;
        // The note's own length: a 16th, a 32nd or a triplet 16th. Shorter
        // ones are gated to end (release included) before the next starts.
        let len = six * div.sixteenths();
        let gate = len * (ph.gate * ctx.param(LENGTH)).min(0.95);
        let gate = if div == Div::Sixteenth { gate } else { gate.min(len - RELEASE) };
        let midi = note_midi(ctx, step / bar_steps, degree);
        let cutoff = ph.cut_from + (ph.cut_to - ph.cut_from) * in_phrase as f64 / phrase_steps as f64;
        self.notes += 1;
        if (beat_pos + 1e-9).fract() < 0.25 - 1e-6 {
            self.notes_on_kick += 1;
        }
        self.voice = Voice {
            active: true,
            dt: midi_hz(midi as f64) / sr,
            p2: 0.37,
            gate,
            env_decay: len * ph.env_decay,
            vel,
            cutoff: cutoff * boost * (ctx.param(CUTOFF) + score_oct).exp2(),
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
            sub: SubClock::new(),
            voice: Voice::default(),
            drive: PARAMS[DRIVE].default,
            param_smooth: 1.0 - (-1.0 / (0.02 * sr)).exp(),
            notes: 0,
            notes_on_kick: 0,
            notes_dropped: 0,
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
        let tr = &ctx.transport;
        let spb = tr.samples_per_beat(ctx.sample_rate);
        for (i, o) in out.iter_mut().enumerate() {
            while let Some(step) = steps.due(i) {
                self.trigger(step, ctx);
            }
            // The SubClock ticks every sample to stay in sync; it plays only
            // the beats sub_rate gives it, the StepClock the rest.
            let beat = tr.beat + i as f64 / spb;
            let rate = sub_rate(ctx, (beat + 1e-9).max(0.0) as u64);
            if let Some(n) = self.sub.tick(beat, rate, spb, tr.playing) {
                if rate != TICKS_PER_BEAT as u64 {
                    self.trigger_sub(n, rate, ctx);
                }
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
        render_with(bars, params, seed, &gen_api::Score::EMPTY)
    }

    fn render_with(bars: usize, params: &[f64], seed: u64, score: &gen_api::Score) -> Vec<f32> {
        render_counting(bars, params, seed, score).0
    }

    /// Renders; returns the audio, the notes played and how many of them
    /// started in the kick's 16th.
    fn render_counting(bars: usize, params: &[f64], seed: u64, score: &gen_api::Score) -> (Vec<f32>, u64, u64, u64) {
        let mut g = Bass::new(&Config { sample_rate: SR, seed });
        let block = 480;
        let mut out = vec![0.0f32; bars * 4 * BEAT];
        for (b, chunk) in out.chunks_mut(block).enumerate() {
            let ctx = Context {
                score,
                sample_rate: SR,
                transport: Transport { bpm: 120.0, beat: (b * block) as f64 / BEAT as f64, beats_per_bar: 4, playing: true },
                tonality: Tonality { root: 9, scale: Scale::Phrygian },
                params,
                input: &[],
            };
            g.process(&ctx, chunk);
        }
        (out, g.notes, g.notes_on_kick, g.notes_dropped)
    }

    /// A score whose first `bars` bars carry `feel`.
    fn feel_score(feel: Feel, bars: usize) -> gen_api::Score {
        let mut s = gen_api::Score::EMPTY;
        s.bars.iter_mut().take(bars).for_each(|b| b.feel = feel);
        s
    }

    fn defaults() -> Vec<f64> {
        PARAMS.iter().map(|p| p.default).collect()
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0, |m, v| m.max(v.abs()))
    }

    #[test]
    fn follows_chord_roots_in_the_key() {
        let key = Tonality { root: 9, scale: Scale::Phrygian }; // A Bb C D E F G
        let mut score = gen_api::Score { first_bar: 16, ..gen_api::Score::EMPTY };
        score.bars[2].chord = Some(gen_api::Chord { root: 0, tones: 0b1000_1001_0001 }); // C at bar 18
        score.bars[3].chord = Some(gen_api::Chord { root: 1, tones: 0b0001 }); // C#, outside the scale
        let params = defaults();
        let ctx = Context {
            score: &score,
            sample_rate: SR,
            transport: Transport { bpm: 120.0, beat: 0.0, beats_per_bar: 4, playing: true },
            tonality: key,
            params: &params,
            input: &[],
        };
        let a = root_midi(&key);
        assert_eq!(note_midi(&ctx, 17, 0), a, "no chord: the key's root");
        let c = note_midi(&ctx, 18, 0);
        assert_eq!(c.rem_euclid(12), 0, "the chord root");
        assert_eq!(note_midi(&ctx, 18, 1) - c, 2, "next degree up from C in A phrygian is D");
        assert_eq!(note_midi(&ctx, 18, 3) - c, 5, "and the 4th up is F");
        let cs = note_midi(&ctx, 19, 0);
        assert_eq!((cs.rem_euclid(12), note_midi(&ctx, 19, 1) - cs), (1, 1), "outside the scale: the key's shape, transposed");
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
    fn follows_the_score_marks() {
        use gen_api::ScoreBar;
        let mut score = gen_api::Score::EMPTY;
        score.bars[0] = ScoreBar { mark: Mark::Break, intensity: 1.0, ..ScoreBar::NONE };
        score.bars[1] = ScoreBar { mark: Mark::Quiet, intensity: 1.0, ..ScoreBar::NONE };
        let mut p = defaults();
        p[FILLS] = 0.0;
        let out = render_with(3, &p, 2, &score);
        let bar = 4 * BEAT;
        assert!(peak(&out[..bar]) < 1e-6, "break: silent");
        // Quiet at full intensity: only the off-8th of each beat.
        let six = BEAT / 4;
        for beat in 4..8 {
            let at = beat * BEAT;
            assert!(peak(&out[at + six + 100..at + 2 * six]) < 1e-3, "16th 2 of beat {beat}");
            assert!(peak(&out[at + 2 * six..at + 2 * six + 2000]) > 0.02, "off 8th of beat {beat}");
        }
        assert!(peak(&out[2 * bar..]) > 0.2, "unmarked bar plays as usual");
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

    #[test]
    fn double_and_quad_gallop_in_32nds() {
        let mut p = defaults();
        p[FILLS] = 0.0;
        for seed in 0..4 {
            let (normal, n, _, _) = render_counting(4, &p, seed, &feel_score(Feel::Normal, 4));
            assert_eq!(normal, render(4, &p, seed), "normal feel is no score");
            for feel in [Feel::Double, Feel::Quad] {
                let (out, d, on_kick, dropped) = render_counting(4, &p, seed, &feel_score(feel, 4));
                assert_eq!(d + dropped, 2 * n, "seed {seed} {feel:?}: two 32nds per 16th, less those on a kick");
                if feel == Feel::Quad {
                    assert_eq!(dropped, n, "quad: only the 32nd after each kick");
                }
                assert_eq!(on_kick, 0, "seed {seed} {feel:?}");
                assert!(out.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
                // Silent where the kick plays (8ths on double, 16ths on
                // quad); between kicks, double keeps its gallop pairs.
                let (six, half) = (BEAT / 4, BEAT / 8);
                for beat in 0..16 {
                    let at = beat * BEAT;
                    for k in 0..4 {
                        let a = peak(&out[at + k * six + 10..at + k * six + 400]) > 0.02;
                        let b = peak(&out[at + k * six + half + 10..at + k * six + half + 400]) > 0.02;
                        let kick = feel == Feel::Quad || k % 2 == 0;
                        if kick {
                            assert!(!a, "seed {seed} {feel:?} beat {beat}: a note on the kick at 16th {k}");
                        } else {
                            assert_eq!(a, b, "seed {seed} {feel:?} beat {beat} 16th {k}: gallop pair");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn triplet_rolls_five_per_beat_after_the_kick() {
        let mut p = defaults();
        p[FILLS] = 0.0;
        for seed in 0..4 {
            let (out, n, on_kick, _) = render_counting(4, &p, seed, &feel_score(Feel::Triplet, 4));
            assert_eq!(n, 5 * 16, "seed {seed}");
            assert_eq!(on_kick, 16, "only slot 1 starts inside the kick's 16th; slot 0 never plays");
            assert!(out.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
            let trip = BEAT / 6;
            for beat in 0..16 {
                let at = beat * BEAT;
                assert!(peak(&out[at + 10..at + trip - 10]) < 1e-3, "seed {seed} beat {beat}: slot 0 silent");
                for t in 1..6 {
                    assert!(peak(&out[at + t * trip + 10..at + t * trip + 600]) > 0.02, "seed {seed} beat {beat} slot {t}");
                }
            }
        }
    }

    #[test]
    fn feel_changes_on_the_bar_keep_the_grid() {
        // Normal, double, triplet, normal bars in turn and a double bar 16:
        // nothing doubles up or drops at the switches, and the big fill
        // (bar 16's last two beats) stays on its straight 16ths.
        let p = defaults();
        let mut score = gen_api::Score::EMPTY;
        let feels = [Feel::Normal, Feel::Double, Feel::Triplet, Feel::Normal];
        for (i, b) in score.bars.iter_mut().enumerate().take(16) {
            b.feel = feels[i % 4];
        }
        score.bars[15].feel = Feel::Double;
        let (out, n, _, dropped) = render_counting(16, &p, 3, &score);
        assert!(out.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
        let normal = |bars: usize| render_counting(bars, &p, 3, &gen_api::Score::EMPTY).1;
        let expected: u64 = (0..16)
            .map(|bar| {
                let plain = normal(bar + 1) - normal(bar);
                match score.bars[bar].feel {
                    Feel::Double if bar == 15 => 2 * (plain - 8) + 8,
                    Feel::Double => 2 * plain,
                    Feel::Triplet => 20,
                    _ => plain,
                }
            })
            .sum();
        assert_eq!(n + dropped, expected, "played plus dropped-for-the-kick");
    }
}
