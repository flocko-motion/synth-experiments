//! atmos — generative pad and texture layer. Stereo.
//!
//! Sound: four voices of five detuned band-limited saws each (a supersaw
//! pad) through a stereo state-variable lowpass, plus filtered noise "air",
//! and a long FDN reverb. It has no pump of its own: ducking is the mixer's
//! job, driven by the kick's duck events.
//!
//! Structure, on the shared grid:
//! - per epoch (16 bars): a four-chord progression, one chord per cycle
//!   (new, kept or rotated from the previous epoch), a colour tone (octave,
//!   7th or 9th), sometimes sus chords, and the shape of the filter swell
//!   across the epoch
//! - per cycle (4 bars): the chord; voices glide to the nearest inversion,
//!   so changes are legato
//!
//! Chords are stored as scale degrees and voiced every block from the host
//! tonality, so key and scale changes glide in right away. When the
//! transport stops the pad releases slowly; it never cuts off.

use dsp::{decay_coef, midi_hz, saw, OnePole, Reverb, Svf};
use gen_api::{Config, Context, Generator, Info, Param, Rng, StepClock, Tonality, BARS_PER_CYCLE, CYCLES_PER_EPOCH, TICKS_PER_BEAT};
use std::f64::consts::PI;

const EPOCH_BARS: f64 = (BARS_PER_CYCLE * CYCLES_PER_EPOCH) as f64;

/// Soft clip, then a -1 dBFS ceiling.
const CEILING: f64 = 0.891;

const BRIGHTNESS: usize = 0;
const MOTION: usize = 1;
const WIDTH: usize = 2;
const AIR: usize = 3;
const SPACE: usize = 4;
const DRIVE: usize = 5;

const PARAMS: [Param; 6] = [
    Param::continuous("brightness", "Brightness", -2.0, 2.0, 0.0).unit("oct"),
    // How far the filter swell and LFO move.
    Param::continuous("motion", "Motion", 0.0, 1.0, 0.5),
    Param::continuous("width", "Width", 0.0, 1.0, 0.7),
    Param::continuous("air", "Air", 0.0, 1.0, 0.3),
    Param::continuous("space", "Reverb mix", 0.0, 1.0, 0.35),
    Param::continuous("drive", "Drive", 0.5, 4.0, 1.0).unit("x"),
];

/// Progressions as scale degrees of the chord roots, one per cycle. They
/// read as i–VI–VII and friends in minor and phrygian, and stay diatonic in
/// any scale.
const PROGRESSIONS: [[i32; 4]; 8] = [
    [0, 0, 5, 6],
    [0, 5, 2, 6],
    [0, 3, 5, 4],
    [0, 6, 5, 6],
    [0, 5, 3, 4],
    [0, 1, 0, 6],
    [0, 3, 0, 5],
    [0, 5, 6, 4],
];

/// Colour tone above the chord root, in scale degrees, and its weight:
/// octave, 7th, 9th.
const COLOURS: [(i32, f64, &str); 3] = [(7, 4.0, "oct"), (6, 3.0, "7th"), (8, 3.0, "9th")];

/// Triad voices sit in [LOW, LOW + 12), the colour tone above them.
const LOW: i32 = 55; // G3

const SAWS: usize = 5;
const VOICES: usize = 4;

const SHAPES: [&str; 4] = ["rise", "arch", "wave", "breathe"];

/// 0..1 at position `t` (0..1) through the epoch.
fn swell(shape: usize, t: f64) -> f64 {
    match shape {
        0 => t.powf(1.5),
        1 => (PI * t).sin().powf(1.2),
        2 => 0.5 - 0.5 * (4.0 * PI * t).cos(),
        _ => 0.5 + 0.5 * (2.0 * PI * t).cos(),
    }
}

// ---------------------------------------------------------------- composition

/// Per-seed sound character.
#[derive(Clone, Copy)]
struct Character {
    detune_cents: f64,
    /// LFO period in bars.
    lfo_bars: f64,
    t60: f64,
}

struct Epoch {
    index: u64,
    progression: [i32; 4],
    colour: i32,
    sus: bool,
    shape: usize,
}

/// A chord as scale degrees: three triad tones and a colour tone.
#[derive(Clone, Copy, PartialEq)]
struct Chord([i32; VOICES]);

impl Chord {
    fn new(root: i32, colour: i32, sus: bool) -> Chord {
        Chord([root, root + if sus { 3 } else { 2 }, root + 4, root + colour])
    }

    /// MIDI notes: the triad folded into one octave above [`LOW`] and
    /// sorted, so voices move to the nearest inversion; the colour tone
    /// folded into the octave above the lowest triad note.
    fn voice(&self, t: &Tonality) -> [i32; VOICES] {
        let fold = |degree: i32, low: i32| {
            let note = t.root as i32 + t.degree(degree);
            low + (note - low).rem_euclid(12)
        };
        let mut triad = [fold(self.0[0], LOW), fold(self.0[1], LOW), fold(self.0[2], LOW)];
        triad.sort_unstable();
        [triad[0], triad[1], triad[2], fold(self.0[3], triad[0] + 7)]
    }
}

// ---------------------------------------------------------------- voice

/// Five detuned saws on one note, rendered as mid and side so width is a
/// cheap per-sample crossfade.
struct Voice {
    phase: [f64; SAWS],
    lf: f64, // log2 of the current frequency
    lf_target: f64,
}

/// Saw positions across the stereo field, -1..1.
const SPREAD: [f64; SAWS] = [-1.0, 0.5, 0.0, -0.5, 1.0];
/// Detune of each saw, as a fraction of the character's detune.
const DETUNE: [f64; SAWS] = [-1.0, -0.45, 0.0, 0.5, 0.95];

impl Voice {
    fn new(rng: &mut Rng) -> Voice {
        let mut phase = [0.0; SAWS];
        phase.iter_mut().for_each(|p| *p = rng.f());
        Voice { phase, lf: 8.0, lf_target: 8.0 }
    }

    /// Returns (mid, side).
    fn tick(&mut self, ratios: &[f64; SAWS], glide: f64, sr: f64) -> (f64, f64) {
        self.lf += (self.lf_target - self.lf) * glide;
        let base = self.lf.exp2() / sr;
        let (mut mid, mut side) = (0.0, 0.0);
        for k in 0..SAWS {
            let dt = base * ratios[k];
            let s = saw(self.phase[k], dt);
            self.phase[k] = (self.phase[k] + dt).fract();
            mid += s;
            side += s * SPREAD[k];
        }
        (mid, side)
    }
}

// ---------------------------------------------------------------- generator

/// Parameter values, smoothed per sample.
#[derive(Clone, Copy)]
struct Smoothed([f64; PARAMS.len()]);

pub struct Atmos {
    sr: f64,
    rng: Rng,
    noise: Rng,
    character: Character,
    clock: StepClock,
    epoch: Option<Epoch>,
    cycle: Option<u64>,
    chord: Option<Chord>,
    voices: [Voice; VOICES],
    ratios: [f64; SAWS],
    filters: [Svf; 2],
    air: [Svf; 2],
    air_hp: [OnePole; 2],
    reverb: Reverb,
    params: Smoothed,
    amp: f64,
    cutoff_oct: f64,
    // coefficients
    glide: f64,
    param_smooth: f64,
    cutoff_smooth: f64,
    attack: f64,
    release: f64,
}

impl Atmos {
    fn compose_epoch(&mut self, index: u64) -> Epoch {
        let r = &mut self.rng;
        let (progression, how) = match &self.epoch {
            Some(prev) if r.chance(0.45) => {
                if r.chance(0.55) {
                    (prev.progression, "kept")
                } else {
                    let p = prev.progression;
                    ([p[2], p[3], p[0], p[1]], "rotated")
                }
            }
            _ => (*r.pick(&PROGRESSIONS), "new"),
        };
        let w: Vec<f64> = COLOURS.iter().map(|c| c.1).collect();
        let colour = r.weighted(&w);
        let sus = r.chance(0.25);
        let shape = (r.next_u64() % SHAPES.len() as u64) as usize;
        let first = index * EPOCH_BARS as u64 + 1;
        let chords: Vec<String> = progression.iter().map(|d| (d + 1).to_string()).collect();
        eprintln!(
            "  bars {:>4}-{:<4} chords {:<9} {:<8} {}{:<5} swell {}",
            first,
            first + EPOCH_BARS as u64 - 1,
            chords.join("-"),
            how,
            COLOURS[colour].2,
            if sus { " sus" } else { "" },
            SHAPES[shape]
        );
        Epoch { index, progression, colour: COLOURS[colour].0, sus, shape }
    }

    fn step(&mut self, step: u64, ctx: &Context) {
        let cycle_steps = (TICKS_PER_BEAT * ctx.transport.beats_per_bar * BARS_PER_CYCLE) as u64;
        let cycle = step / cycle_steps;
        if self.cycle == Some(cycle) {
            return;
        }
        self.cycle = Some(cycle);
        let epoch = cycle / CYCLES_PER_EPOCH as u64;
        if self.epoch.as_ref().map(|e| e.index) != Some(epoch) {
            self.epoch = Some(self.compose_epoch(epoch));
        }
        let e = self.epoch.as_ref().unwrap();
        let root = e.progression[(cycle % CYCLES_PER_EPOCH as u64) as usize];
        let first = self.chord.is_none();
        self.chord = Some(Chord::new(root, e.colour, e.sus));
        self.retarget(&ctx.tonality);
        if first {
            self.voices.iter_mut().for_each(|v| v.lf = v.lf_target);
        }
    }

    fn retarget(&mut self, t: &Tonality) {
        if let Some(chord) = self.chord {
            for (v, note) in self.voices.iter_mut().zip(chord.voice(t)) {
                v.lf_target = midi_hz(note as f64).log2();
            }
        }
    }
}

impl Generator for Atmos {
    fn new(config: &Config) -> Self {
        let sr = config.sample_rate;
        let mut rng = Rng::new(config.seed ^ 0xA7_0505);
        let character = Character {
            detune_cents: rng.range(9.0, 22.0),
            lfo_bars: *rng.pick(&[1.0, 2.0, 4.0]),
            t60: rng.range(3.0, 6.5),
        };
        let mut ratios = [1.0; SAWS];
        for (r, d) in ratios.iter_mut().zip(DETUNE) {
            *r = (d * character.detune_cents / 1200.0).exp2();
        }
        let voices = [Voice::new(&mut rng), Voice::new(&mut rng), Voice::new(&mut rng), Voice::new(&mut rng)];
        let mut reverb = Reverb::new(sr);
        reverb.set(character.t60, 5000.0, sr);
        let smooth = |tau: f64| 1.0 - decay_coef(tau, sr);
        Atmos {
            sr,
            noise: Rng::new(config.seed ^ 0x0A1E),
            character,
            clock: StepClock::new(),
            epoch: None,
            cycle: None,
            chord: None,
            voices,
            ratios,
            filters: [Svf::default(); 2],
            air: [Svf::default(); 2],
            air_hp: [OnePole::new(2500.0, sr); 2],
            reverb,
            params: Smoothed(PARAMS.map(|p| p.default)),
            amp: 0.0,
            cutoff_oct: 0.0,
            glide: smooth(0.15),
            param_smooth: smooth(0.03),
            cutoff_smooth: smooth(0.08),
            attack: smooth(1.2),
            release: smooth(2.5),
            rng,
        }
    }

    fn params(&self) -> &'static [Param] {
        &PARAMS
    }

    fn info(&self) -> Info {
        Info {
            name: "atmos",
            description: "Supersaw pad on cycle-long chords with filter swells, air and reverb (stereo)",
            channels: 2,
        }
    }

    fn process(&mut self, ctx: &Context, out: &mut [f32]) {
        let sr = self.sr;
        let tr = &ctx.transport;
        let spb = tr.samples_per_beat(sr);
        let epoch_beats = EPOCH_BARS * tr.beats_per_bar as f64;
        let lfo_beats = self.character.lfo_bars * tr.beats_per_bar as f64;
        self.retarget(&ctx.tonality);
        let mut steps = self.clock.block(ctx);

        for (i, frame) in out.chunks_exact_mut(2).enumerate() {
            while let Some(step) = steps.due(i) {
                self.step(step, ctx);
            }
            for (k, v) in self.params.0.iter_mut().enumerate() {
                *v += (ctx.param(k) - *v) * self.param_smooth;
            }
            let p = self.params.0;

            let beat = tr.beat + i as f64 / spb;
            let (shape, sounding) = match &self.epoch {
                Some(e) => (e.shape, true),
                None => (0, false),
            };
            let sw = swell(shape, (beat / epoch_beats).fract());
            let lfo = (2.0 * PI * beat / lfo_beats).sin();

            let target = if tr.playing && sounding { 1.0 } else { 0.0 };
            self.amp += (target - self.amp) * if target > self.amp { self.attack } else { self.release };

            let gain = self.amp;

            // Pad: supersaw → stereo lowpass.
            let (mut mid, mut side) = (0.0, 0.0);
            for v in self.voices.iter_mut() {
                let (m, s) = v.tick(&self.ratios, self.glide, sr);
                mid += m;
                side += s;
            }
            let width = p[WIDTH] * 0.5;
            let (l, r) = ((mid + side * width) * 0.05, (mid - side * width) * 0.05);
            let oct = p[BRIGHTNESS] + 0.6 + p[MOTION] * (sw * 2.6 + lfo * 0.5);
            self.cutoff_oct += (oct - self.cutoff_oct) * self.cutoff_smooth;
            let fc = 500.0 * self.cutoff_oct.exp2();
            let mut pad = [l, r];
            for (x, f) in pad.iter_mut().zip(self.filters.iter_mut()) {
                *x = f.lp(*x, fc, 0.15, sr) * 0.78;
            }

            // Air: independent noise per side, band-passed and moving with
            // the swell.
            let afc = 3500.0 * (p[MOTION] * (sw * 1.2 + lfo * 0.3)).exp2();
            let mut air = [0.0; 2];
            for k in 0..2 {
                let n = self.noise.f() * 2.0 - 1.0;
                self.air[k].set(afc, 0.4, sr);
                air[k] = self.air_hp[k].hp(self.air[k].tick(n).1) * p[AIR] * 0.35;
            }

            let dry = [(pad[0] + air[0]) * gain, (pad[1] + air[1]) * gain];
            let (wl, wr) = self.reverb.tick(dry[0], dry[1]);
            let space = p[SPACE];
            let drive = p[DRIVE];
            let norm = drive.tanh();
            let mix = |d: f64, w: f64| ((d * (1.0 - 0.5 * space) + w * space * 0.6) * drive).tanh() / norm;
            frame[0] = (mix(dry[0], wl).tanh() * CEILING) as f32;
            frame[1] = (mix(dry[1], wr).tanh() * CEILING) as f32;
        }
        self.clock.finish(steps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gen_api::{Scale, Transport};

    fn render(seed: u64, bars: usize, playing: bool) -> Vec<f32> {
        let sr = 48_000.0;
        let mut g = Atmos::new(&Config { sample_rate: sr, seed });
        let params: Vec<f64> = PARAMS.iter().map(|p| p.default).collect();
        let block = 512;
        let bpm = 145.0;
        let total = (bars as f64 * 4.0 * sr * 60.0 / bpm) as usize;
        let mut out = vec![0.0f32; total / block * block * 2];
        for (b, chunk) in out.chunks_mut(block * 2).enumerate() {
            let beat = (b * block) as f64 / (sr * 60.0 / bpm);
            let ctx = Context {
                sample_rate: sr,
                transport: Transport { bpm, beat, beats_per_bar: 4, playing },
                tonality: Tonality { root: 2, scale: Scale::Phrygian },
                params: &params,
                input: &[],
            };
            g.process(&ctx, chunk);
        }
        out
    }

    #[test]
    fn deterministic_and_in_range() {
        let a = render(7, 20, true);
        let b = render(7, 20, true);
        assert_eq!(a, b);
        let peak = a.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.05 && peak <= 1.0, "peak {peak}");
        assert!(a.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn silent_when_stopped() {
        assert!(render(3, 2, false).iter().all(|&x| x == 0.0));
    }

    #[test]
    fn voicing_stays_in_register() {
        for scale in Scale::ALL {
            for root in 0..12 {
                let t = Tonality { root, scale };
                for prog in PROGRESSIONS {
                    for d in prog {
                        let v = Chord::new(d, 6, false).voice(&t);
                        assert!(v[0] >= LOW && v[2] < LOW + 12 && v[0] <= v[1] && v[1] <= v[2]);
                        assert!(v[3] > v[0] && v[3] < v[0] + 20);
                    }
                }
            }
        }
    }
}
