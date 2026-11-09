//! kick — psytrance bass drum on its own, tuned to the host key. Mono.
//!
//! Four on the floor. With `fills` on, it drops out for the last two beats
//! of every 16th bar, the same grid rule `bass` uses for its run, so a kick
//! and a bass with any seeds agree on where the gap is. psybass is the
//! combined kick + bass.
//!
//! Sound: a sine with a two-stage pitch drop (body sweep and a fast click
//! sweep) settling on the key's root in the bass register, plus a
//! highpassed noise click, then drive. Every kick sends a duck event, which
//! is what the mixer ducks everything else from.
//!
//! The workspace score: no kick in a `break` (so nothing ducks either),
//! softer kicks in a `quiet`, and beats 1 and 3 only in half-time feel or a
//! deep `quiet`. Double-time feel drives on 8ths, quad rolls on 16ths (the
//! in-between kicks a little softer, ducking at 0.6, and every kick cut
//! short before the next). Turn `tempo` off to keep four on the floor under
//! every feel. It stays on the key's root: psy kicks don't follow chords.

use dsp::OnePole;
use gen_api::{Config, Context, Entry, Event, EventKind, Feel, Generator, Info, Mark, Param, Rng, ScoreBar, StepClock, Tonality, TICKS_PER_BEAT};
use std::f64::consts::PI;

const TUNE: usize = 0;
const PUNCH: usize = 1;
const SWEEP: usize = 2;
const LENGTH: usize = 3;
const CLICK: usize = 4;
const DRIVE: usize = 5;
const FILLS: usize = 6;
const TEMPO: usize = 7;

const PARAMS: [Param; 8] = [
    // Semitones away from the key's root.
    Param::integer("tune", "Tune", -12, 12, 0).unit("st").glide(0.0),
    // Depth of the body's pitch drop.
    Param::continuous("punch", "Punch", 0.0, 1.0, 0.5),
    Param::continuous("sweep", "Sweep", 5.0, 60.0, 18.0).log().unit("ms"),
    // Psy kicks are about a 16th long, gone before the bass note.
    Param::continuous("length", "Length", 0.5, 3.0, 1.05).unit("16th"),
    Param::continuous("click", "Click", 0.0, 1.0, 0.35),
    Param::continuous("drive", "Drive", 0.5, 4.0, 1.25).unit("x"),
    Param::toggle("fills", "Fills", true).glide(0.0),
    // Follow the score's tempo feel (8ths on double, 16ths on quad, beats
    // 1 and 3 on half); off keeps four on the floor whatever the others do.
    Param::toggle("tempo", "Tempo", true).glide(0.0),
];

/// Soft clip, then a -1 dBFS ceiling.
const CEILING: f64 = 0.891;

/// The shared grid rule: the last two beats of every 16th bar are the big
/// fill. `bass` has the same function; keep them in step.
fn big_fill(beat_abs: u64, beats_per_bar: u64) -> bool {
    let bar = beat_abs / beats_per_bar;
    (bar + 1) % 16 == 0 && beat_abs % beats_per_bar >= beats_per_bar.saturating_sub(2)
}

/// How the score shapes the kick on beat `beat_in_bar`: its level, or
/// `None` for no kick (and no duck event).
/// - break: out
/// - quiet: softer with intensity, and above 0.8 only beats 1 and 3
/// - half-time feel: only beats 1 and 3
fn score_level(sb: &ScoreBar, beat_in_bar: u64) -> Option<f64> {
    let i = sb.intensity as f64;
    let half = sb.feel == Feel::Half || (sb.mark == Mark::Quiet && i >= 0.8);
    match sb.mark {
        Mark::Break => None,
        _ if half && beat_in_bar % 2 == 1 => None,
        Mark::Quiet => Some(1.0 - 0.5 * i),
        _ => Some(1.0),
    }
}

/// Bass register root: D1 (36.7 Hz) up to C#2 (69.3 Hz), as in psybass.
fn root_midi(t: &Tonality) -> i32 {
    26 + (t.root as i32 + 10) % 12
}

/// Per-seed variation: small, so every seed is still a solid psy kick.
#[derive(Clone, Copy)]
struct Character {
    /// Exponent of the amplitude fall-off: higher is tighter.
    tail: f64,
    /// Click sweep start, Hz.
    click_hz: f64,
    /// Noise click colour: highpass cutoff.
    click_hp: f64,
}

#[derive(Default)]
struct Voice {
    i: usize,
    len: usize,
    phase: f64,
    root_hz: f64,
    level: f64,
}

pub struct Kick {
    sr: f64,
    noise: Rng,
    character: Character,
    clock: StepClock,
    voice: Voice,
    click_hp: OnePole,
    params: [f64; PARAMS.len()],
    param_smooth: f64,
    events: Vec<Event>,
}

impl Kick {
    fn tick(&mut self) -> f64 {
        let v = &mut self.voice;
        if v.i >= v.len {
            return 0.0;
        }
        let sr = self.sr;
        let p = &self.params;
        let t = v.i as f64 / sr;
        let u = v.i as f64 / v.len as f64;
        v.i += 1;
        let body_hz = 40.0 + 360.0 * p[PUNCH];
        let f = v.root_hz
            + body_hz * (-t / (p[SWEEP] / 1000.0)).exp()
            + self.character.click_hz * p[CLICK] * (-t / 0.0018).exp();
        v.phase += f / sr;
        let body = (2.0 * PI * v.phase).sin();
        let attack = (t / 0.0004).min(1.0);
        let amp = attack * (1.0 - u).powf(self.character.tail);
        let n = self.noise.range(-1.0, 1.0);
        let click = self.click_hp.hp(n) * (-t / 0.0015).exp() * p[CLICK];
        ((body * amp * 1.8).tanh() + click) * 0.95 * v.level
    }
}

impl Generator for Kick {
    fn new(config: &Config) -> Self {
        let sr = config.sample_rate;
        let mut rng = Rng::new(config.seed ^ 0x6B1C);
        let character = Character { tail: rng.range(1.3, 2.0), click_hz: rng.range(700.0, 1200.0), click_hp: rng.range(1500.0, 4000.0) };
        Kick {
            sr,
            noise: Rng::new(config.seed ^ 0xC11C),
            click_hp: OnePole::new(character.click_hp, sr),
            character,
            clock: StepClock::new(),
            voice: Voice::default(),
            params: PARAMS.map(|p| p.default),
            param_smooth: 1.0 - (-1.0 / (0.02 * sr)).exp(),
            events: Vec::new(),
        }
    }

    fn info(&self) -> Info {
        Info { name: "kick", description: "Psytrance bass drum on its own, tuned to the host key; sends the duck events", channels: 1 }
    }

    fn params(&self) -> &'static [Param] {
        &PARAMS
    }

    /// The kick is what everything else ducks away from.
    fn duck_sensitivity(&self) -> f64 {
        0.0
    }

    fn entry(&self) -> Entry {
        Entry::Bar
    }

    fn category(&self) -> &'static str {
        "drums"
    }

    fn take_events(&mut self, out: &mut Vec<Event>) {
        out.append(&mut self.events);
    }

    fn process(&mut self, ctx: &Context, out: &mut [f32]) {
        let tr = &ctx.transport;
        let six = 60.0 / tr.bpm / TICKS_PER_BEAT as f64;
        let mut steps = self.clock.block(ctx);
        for (i, o) in out.iter_mut().enumerate() {
            while let Some(step) = steps.due(i) {
                let tick = step % TICKS_PER_BEAT as u64;
                let beat = step / TICKS_PER_BEAT as u64;
                let bpb = tr.beats_per_bar.max(1) as u64;
                let mut bar = ctx.score.bar(beat / bpb);
                if ctx.param(TEMPO) == 0.0 {
                    bar.feel = Feel::Normal;
                }
                // Feel: 8ths on double, 16ths on quad; otherwise the beat.
                let spacing = match bar.feel {
                    Feel::Double => 2,
                    Feel::Quad => 1,
                    _ => TICKS_PER_BEAT as u64,
                };
                if tick % spacing != 0 {
                    continue;
                }
                if ctx.param(FILLS) != 0.0 && big_fill(beat, bpb) {
                    continue;
                }
                let Some(level) = score_level(&bar, beat % bpb) else { continue };
                // In-between kicks are a touch softer and duck less, so
                // faster pumping doesn't choke the mix.
                let (level, duck) = if tick == 0 { (level, level) } else { (level * 0.85, level * 0.6) };
                // Tune and length apply per hit, so they never bend a kick
                // that's already sounding.
                self.params[TUNE] = ctx.param(TUNE);
                self.params[LENGTH] = ctx.param(LENGTH);
                let root_hz = dsp::midi_hz((root_midi(&ctx.tonality) as f64) + self.params[TUNE]);
                // Never longer than the gap to the next kick.
                let len = self.params[LENGTH].min(spacing as f64 - 0.05);
                self.voice = Voice { len: (six * len * self.sr) as usize, root_hz, level, ..Voice::default() };
                self.events.push(Event { frame: i as u32, kind: EventKind::Duck { strength: duck as f32 } });
            }
            for k in [PUNCH, SWEEP, CLICK, DRIVE] {
                self.params[k] += (ctx.param(k) - self.params[k]) * self.param_smooth;
            }
            let x = self.tick();
            *o = ((x * self.params[DRIVE]).tanh() * CEILING) as f32;
        }
        self.clock.finish(steps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gen_api::{Scale, Transport};

    const SR: f64 = 48_000.0;

    /// Render `bars` at 120 bpm (one beat = 24 000 frames); returns audio
    /// and the frames of all duck events.
    fn render(bars: usize, params: &[f64]) -> (Vec<f32>, Vec<usize>) {
        render_with(bars, params, &gen_api::Score::EMPTY)
    }

    fn render_with(bars: usize, params: &[f64], score: &gen_api::Score) -> (Vec<f32>, Vec<usize>) {
        let mut k = Kick::new(&Config { sample_rate: SR, seed: 3 });
        let block = 480;
        let mut out = vec![0.0f32; bars * 4 * 24_000];
        let mut ducks = Vec::new();
        let mut ev = Vec::new();
        for (b, chunk) in out.chunks_mut(block).enumerate() {
            let ctx = Context {
                score,
                sample_rate: SR,
                transport: Transport { bpm: 120.0, beat: (b * block) as f64 / 24_000.0, beats_per_bar: 4, playing: true },
                tonality: Tonality { root: 9, scale: Scale::Phrygian },
                params,
                input: &[],
            };
            k.process(&ctx, chunk);
            k.take_events(&mut ev);
            ducks.extend(ev.drain(..).map(|e| b * block + e.frame as usize));
        }
        (out, ducks)
    }

    fn defaults() -> Vec<f64> {
        PARAMS.iter().map(|p| p.default).collect()
    }

    #[test]
    fn four_on_the_floor_with_the_bar_16_gap() {
        let (out, ducks) = render(17, &defaults());
        // 17 bars × 4 beats, minus beats 3 and 4 of bar 16.
        assert_eq!(ducks.len(), 17 * 4 - 2);
        assert!(ducks.iter().all(|f| f % 24_000 == 0));
        let gap = (15 * 4 + 2) * 24_000;
        assert!(!ducks.contains(&gap) && !ducks.contains(&(gap + 24_000)));
        assert!(out.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
        assert!(out.iter().fold(0.0f32, |m, x| m.max(x.abs())) > 0.5);
    }

    #[test]
    fn fills_off_keeps_every_beat() {
        let mut p = defaults();
        p[FILLS] = 0.0;
        assert_eq!(render(16, &p).1.len(), 16 * 4);
    }

    #[test]
    fn follows_the_score() {
        let mut score = gen_api::Score::EMPTY;
        score.bars[0] = ScoreBar { mark: Mark::Break, intensity: 1.0, ..ScoreBar::NONE };
        score.bars[1] = ScoreBar { feel: Feel::Half, ..ScoreBar::NONE };
        score.bars[2] = ScoreBar { mark: Mark::Quiet, intensity: 0.4, ..ScoreBar::NONE };
        let (out, ducks) = render_with(4, &defaults(), &score);
        let beats: Vec<usize> = ducks.iter().map(|f| f / 24_000).collect();
        // Break: nothing in bar 1. Half: beats 1 and 3 of bar 2. Quiet 0.4
        // and bar 4: all four.
        assert_eq!(beats, vec![4, 6, 8, 9, 10, 11, 12, 13, 14, 15]);
        let peak = |bar: usize| out[bar * 96_000..bar * 96_000 + 4_000].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak(2) < peak(3) * 0.95, "quiet is softer: {} vs {}", peak(2), peak(3));
    }

    #[test]
    fn eighths_on_double_sixteenths_on_quad() {
        let mut score = gen_api::Score::EMPTY;
        score.bars[0] = ScoreBar { feel: Feel::Double, ..ScoreBar::NONE };
        score.bars[1] = ScoreBar { feel: Feel::Quad, ..ScoreBar::NONE };
        let mut ev = Vec::new();
        let mut k = Kick::new(&Config { sample_rate: SR, seed: 3 });
        let params = defaults();
        let block = 480;
        let mut out = vec![0.0f32; block];
        let mut frames = Vec::new();
        for b in 0..3 * 96_000 / block {
            let ctx = Context {
                sample_rate: SR,
                transport: Transport { bpm: 120.0, beat: (b * block) as f64 / 24_000.0, beats_per_bar: 4, playing: true },
                tonality: Tonality { root: 9, scale: Scale::Phrygian },
                params: &params,
                input: &[],
                score: &score,
            };
            k.process(&ctx, &mut out);
            assert!(out.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
            k.take_events(&mut ev);
            frames.extend(ev.drain(..).map(|e| (b * block + e.frame as usize, e.kind)));
        }
        let six = 6_000;
        let count = |bar: usize| frames.iter().filter(|f| f.0 / 96_000 == bar).count();
        assert_eq!((count(0), count(1), count(2)), (8, 16, 4), "8ths, 16ths, then the beat");
        assert!(frames.iter().filter(|f| f.0 / 96_000 < 2).all(|f| f.0 % six == 0), "on the grid");
        let strengths: Vec<f32> = frames[..2].iter().map(|f| match f.1 { EventKind::Duck { strength } => strength }).collect();
        assert_eq!(strengths, vec![1.0, 0.6], "the in-between kick ducks less");
    }

    #[test]
    fn tempo_off_keeps_four_on_the_floor() {
        let mut score = gen_api::Score::EMPTY;
        score.bars[0] = ScoreBar { feel: Feel::Double, ..ScoreBar::NONE };
        score.bars[1] = ScoreBar { feel: Feel::Half, ..ScoreBar::NONE };
        let mut p = defaults();
        p[TEMPO] = 0.0;
        assert_eq!(render_with(2, &p, &score).1.len(), 8);
    }

    #[test]
    fn grid_rule_matches_bar_16() {
        assert!(!big_fill(61, 4));
        assert!(big_fill(62, 4) && big_fill(63, 4));
        assert!(!big_fill(64, 4));
        assert!(big_fill(127, 4));
    }
}
