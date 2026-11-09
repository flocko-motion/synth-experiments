//! fx — transition effects on the 16-bar grid: risers, impacts,
//! downlifters and zaps. Stereo.
//!
//! - **Riser**: band-passed noise (plus, for some seeds, a pitched saw) that
//!   sweeps up and widens over the last `length` bars of every 16, ending
//!   exactly on the next section's downbeat.
//! - **Impact**: on bar 1 of every section after the first, a sub drop
//!   tuned to the key's root plus a noise burst.
//! - **Downlifter**: band-passed noise sweeping down over the two bars after
//!   each impact.
//! - **Zaps**: short laser sweeps on off-beat 16ths, panned left and right;
//!   each cycle seeds a few candidate spots and `zaps` sets how many fire.
//!
//! Everything hangs on the host's grid (the same 16-bar sections `kick` and
//! `bass` fill on), so the riser ends where the kick's gap ends.

use dsp::{decay_coef, midi_hz, pan_gains, saw, soft_limit, OnePole, Svf};
use gen_api::{Config, Context, Entry, Generator, Info, Param, Rng, StepClock, Tonality, BARS_PER_CYCLE, CYCLES_PER_EPOCH, TICKS_PER_BEAT};
use std::f64::consts::PI;

const SECTION_BARS: u64 = (BARS_PER_CYCLE * CYCLES_PER_EPOCH) as u64;

const RISER: usize = 0;
const LENGTH: usize = 1;
const IMPACT: usize = 2;
const DOWNLIFTER: usize = 3;
const ZAPS: usize = 4;
const TONE: usize = 5;
const WIDTH: usize = 6;
const DRIVE: usize = 7;

const LENGTHS: &[&str] = &["2 bars", "4 bars", "8 bars"];
const LENGTH_BARS: [f64; 3] = [2.0, 4.0, 8.0];

const PARAMS: [Param; 8] = [
    Param::continuous("riser", "Riser", 0.0, 1.0, 0.7).glide(2.0),
    Param::choice("length", "Riser length", LENGTHS, 1).glide(0.0),
    Param::continuous("impact", "Impact", 0.0, 1.0, 0.8).glide(2.0),
    Param::continuous("downlifter", "Downlifter", 0.0, 1.0, 0.5).glide(2.0),
    // How many of each cycle's candidate zaps fire.
    Param::continuous("zaps", "Zaps", 0.0, 1.0, 0.3).glide(2.0),
    // Brightness of the noise sweeps.
    Param::continuous("tone", "Tone", -2.0, 1.0, 0.0).unit("oct").glide(2.0),
    Param::continuous("width", "Width", 0.0, 1.0, 1.0).glide(2.0),
    Param::continuous("drive", "Drive", 0.5, 4.0, 1.0).unit("x"),
];

const ZAP_CANDIDATES: usize = 6;
const ZAP_VOICES: usize = 4;

/// Bass register root (D1..C#2), as in psybass, kick and bass.
fn root_midi(t: &Tonality) -> i32 {
    26 + (t.root as i32 + 10) % 12
}

// ---------------------------------------------------------------- voices

#[derive(Clone, Copy, Default)]
struct Zap {
    t: f64, // seconds since start; >= len means idle
    len: f64,
    f0: f64,
    f1: f64,
    phase: f64,
    pan: f64,
    level: f64,
}

#[derive(Clone, Copy)]
struct Candidate {
    step: u64, // within the cycle
    draw: f64, // fires when below the `zaps` param
    zap: Zap,
}

/// Per-seed character.
#[derive(Clone, Copy)]
struct Character {
    /// Level of the pitched saw in the riser (0 for noise only).
    riser_saw: f64,
    /// Exponent of the riser's level curve: higher stays quiet longer.
    riser_curve: f64,
    /// Sub drop start, Hz above the root.
    sub_drop: f64,
}

pub struct Fx {
    sr: f64,
    rng: Rng,
    noise: Rng,
    character: Character,
    clock: StepClock,
    cycle: Option<(u64, [Candidate; ZAP_CANDIDATES])>,
    // riser
    riser_bp: [Svf; 2],
    riser_amp: f64,
    saw_phase: f64,
    // impact
    impact_t: Option<f64>,
    impact_phase: f64,
    impact_root: f64,
    burst_lp: [OnePole; 2],
    // downlifter
    down_t: Option<f64>,
    down_len: f64,
    down_bp: [Svf; 2],
    zaps: [Zap; ZAP_VOICES],
    next_zap: usize,
    params: [f64; PARAMS.len()],
    param_smooth: f64,
    amp_smooth: f64,
}

impl Fx {
    fn compose_cycle(&mut self, index: u64, cycle_steps: u64) -> (u64, [Candidate; ZAP_CANDIDATES]) {
        let r = &mut self.rng;
        let mut c = [Candidate { step: 0, draw: 1.0, zap: Zap::default() }; ZAP_CANDIDATES];
        for (i, cand) in c.iter_mut().enumerate() {
            // An off-beat 16th, never on the kick.
            let beat = r.next_u64() % (cycle_steps / TICKS_PER_BEAT as u64);
            let tick = 1 + r.next_u64() % (TICKS_PER_BEAT as u64 - 1);
            *cand = Candidate {
                step: beat * TICKS_PER_BEAT as u64 + tick,
                draw: r.f(),
                zap: Zap {
                    t: 0.0,
                    len: r.range(0.05, 0.22),
                    f0: r.range(1800.0, 5500.0),
                    f1: r.range(90.0, 320.0),
                    phase: 0.0,
                    pan: if i % 2 == 0 { -0.7 } else { 0.7 } * r.range(0.5, 1.0),
                    level: r.range(0.5, 0.9),
                },
            };
        }
        (index, c)
    }

    fn step(&mut self, step: u64, ctx: &Context) {
        let tr = &ctx.transport;
        let tpb = TICKS_PER_BEAT as u64;
        let bar_steps = tr.beats_per_bar.max(1) as u64 * tpb;
        let cycle_steps = bar_steps * BARS_PER_CYCLE as u64;
        let section_steps = bar_steps * SECTION_BARS;
        let spb = 60.0 / tr.bpm;

        // Impact and downlifter on the downbeat of every section but the first.
        if step % section_steps == 0 && step > 0 {
            self.impact_t = Some(0.0);
            self.impact_phase = 0.0;
            self.impact_root = midi_hz(root_midi(&ctx.tonality) as f64);
            self.down_t = Some(0.0);
            self.down_len = 2.0 * tr.beats_per_bar.max(1) as f64 * spb;
        }

        let cycle = step / cycle_steps;
        if self.cycle.as_ref().map(|c| c.0) != Some(cycle) {
            self.cycle = Some(self.compose_cycle(cycle, cycle_steps));
        }
        let in_cycle = step % cycle_steps;
        let zaps = ctx.param(ZAPS);
        let candidates = self.cycle.as_ref().unwrap().1;
        for c in candidates.iter().filter(|c| c.step == in_cycle && c.draw < zaps) {
            self.zaps[self.next_zap] = c.zap;
            self.next_zap = (self.next_zap + 1) % ZAP_VOICES;
        }
    }
}

impl Generator for Fx {
    fn new(config: &Config) -> Self {
        let sr = config.sample_rate;
        let mut rng = Rng::new(config.seed ^ 0xF0F0);
        let character = Character {
            riser_saw: if rng.chance(0.5) { rng.range(0.15, 0.35) } else { 0.0 },
            riser_curve: rng.range(1.8, 3.0),
            sub_drop: rng.range(90.0, 200.0),
        };
        let coef = |tau: f64| 1.0 - decay_coef(tau, sr);
        Fx {
            sr,
            rng,
            noise: Rng::new(config.seed ^ 0x0F0F),
            character,
            clock: StepClock::new(),
            cycle: None,
            riser_bp: [Svf::default(); 2],
            riser_amp: 0.0,
            saw_phase: 0.0,
            impact_t: None,
            impact_phase: 0.0,
            impact_root: 55.0,
            burst_lp: [OnePole::new(2500.0, sr); 2],
            down_t: None,
            down_len: 1.0,
            down_bp: [Svf::default(); 2],
            zaps: [Zap { t: 1.0, len: 0.0, ..Zap::default() }; ZAP_VOICES],
            next_zap: 0,
            params: PARAMS.map(|p| p.default),
            param_smooth: coef(0.03),
            amp_smooth: coef(0.003),
        }
    }

    fn info(&self) -> Info {
        Info { name: "fx", description: "Transition FX on the 16-bar grid: risers, impacts, downlifters and zaps (stereo)", channels: 2 }
    }

    fn params(&self) -> &'static [Param] {
        &PARAMS
    }

    fn category(&self) -> &'static str {
        "fx"
    }

    /// A riser joined mid-way would start loud; wait for the bar.
    fn entry(&self) -> Entry {
        Entry::Bar
    }

    /// Risers and zaps pump a little with the kick; the impact lands on a
    /// beat the kick usually skips (the bar-16 gap ends there), so it keeps
    /// most of its punch.
    fn duck_sensitivity(&self) -> f64 {
        0.3
    }

    fn process(&mut self, ctx: &Context, out: &mut [f32]) {
        let sr = self.sr;
        let dt = 1.0 / sr;
        let tr = &ctx.transport;
        let spb = tr.samples_per_beat(sr);
        let section_beats = SECTION_BARS as f64 * tr.beats_per_bar.max(1) as f64;
        let mut steps = self.clock.block(ctx);

        for (i, frame) in out.chunks_exact_mut(2).enumerate() {
            while let Some(step) = steps.due(i) {
                self.step(step, ctx);
            }
            for (k, v) in self.params.iter_mut().enumerate() {
                if k != LENGTH {
                    *v += (ctx.param(k) - *v) * self.param_smooth;
                }
            }
            let p = self.params;
            let width = p[WIDTH];
            let mut l = 0.0;
            let mut r = 0.0;

            // Riser: position within its window at the end of the section.
            let beat = tr.beat + i as f64 / spb;
            let riser_beats = LENGTH_BARS[ctx.param(LENGTH) as usize] * tr.beats_per_bar.max(1) as f64;
            let u = ((beat % section_beats) - (section_beats - riser_beats)) / riser_beats;
            let in_riser = tr.playing && (0.0..1.0).contains(&u);
            let target = if in_riser { u.powf(self.character.riser_curve) * p[RISER] } else { 0.0 };
            self.riser_amp += (target - self.riser_amp) * self.amp_smooth;
            if self.riser_amp > 1e-5 {
                let u = u.clamp(0.0, 1.0);
                let fc = 250.0 * (u * 5.0 + p[TONE]).exp2();
                let n = [self.noise.f() * 2.0 - 1.0, self.noise.f() * 2.0 - 1.0];
                // Starts narrow, widens as it rises.
                let w = width * u;
                let mid = (n[0] + n[1]) * 0.5;
                let side = (n[0] - n[1]) * 0.5 * w;
                let src = [mid + side, mid - side];
                let saw_hz = midi_hz((root_midi(&ctx.tonality) + 24) as f64 + 24.0 * u);
                let s = saw(self.saw_phase, saw_hz * dt) * self.character.riser_saw;
                self.saw_phase = (self.saw_phase + saw_hz * dt).fract();
                for k in 0..2 {
                    self.riser_bp[k].set(fc, 0.55, sr);
                    let y = self.riser_bp[k].tick(src[k] + s).1;
                    let v = y * self.riser_amp * 1.1;
                    if k == 0 { l += v } else { r += v }
                }
            }

            // Impact: sub drop + noise burst.
            if let Some(t) = self.impact_t {
                let f = self.impact_root + self.character.sub_drop * (-t / 0.07).exp();
                self.impact_phase += f * dt;
                let sub = (2.0 * PI * self.impact_phase).sin() * (-t / 0.7).exp() * (t / 0.002).min(1.0);
                let burst = (-t / 0.18).exp();
                let level = p[IMPACT];
                for k in 0..2 {
                    let n = self.noise.f() * 2.0 - 1.0;
                    let b = self.burst_lp[k].lp(n) * burst * 0.8;
                    let v = (sub * 0.9 + b) * level;
                    if k == 0 { l += v } else { r += v }
                }
                self.impact_t = if t < 4.0 { Some(t + dt) } else { None };
            }

            // Downlifter: noise sweeping down after the impact.
            if let Some(t) = self.down_t {
                let u = t / self.down_len;
                let fc = 7000.0 * (p[TONE]).exp2() * (220.0f64 / 7000.0).powf(u);
                let amp = (1.0 - u).max(0.0).powf(1.5) * (t / 0.01).min(1.0) * p[DOWNLIFTER] * 0.6;
                let n = [self.noise.f() * 2.0 - 1.0, self.noise.f() * 2.0 - 1.0];
                let mid = (n[0] + n[1]) * 0.5;
                let side = (n[0] - n[1]) * 0.5 * width;
                for (k, src) in [mid + side, mid - side].into_iter().enumerate() {
                    self.down_bp[k].set(fc, 0.5, sr);
                    let v = self.down_bp[k].tick(src).1 * amp;
                    if k == 0 { l += v } else { r += v }
                }
                self.down_t = if u < 1.0 { Some(t + dt) } else { None };
            }

            // Zaps.
            for z in self.zaps.iter_mut() {
                if z.t >= z.len {
                    continue;
                }
                let f = z.f0 * (z.f1 / z.f0).powf(z.t / z.len);
                z.phase = (z.phase + f * dt).fract();
                let env = (z.t / 0.002).min(1.0) * (1.0 - z.t / z.len).powi(2);
                let v = ((2.0 * PI * z.phase).sin() * 3.0).tanh() * env * z.level * 0.35;
                let (gl, gr) = pan_gains(z.pan * width);
                l += v * gl;
                r += v * gr;
                z.t += dt;
            }

            let drive = p[DRIVE];
            frame[0] = soft_limit((l * drive).tanh() / drive) as f32;
            frame[1] = soft_limit((r * drive).tanh() / drive) as f32;
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
    const BAR: usize = 4 * BEAT;

    fn render(bars: usize, params: &[f64], playing: bool) -> Vec<f32> {
        let mut g = Fx::new(&Config { sample_rate: SR, seed: 5 });
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

    /// RMS of bars lo..hi (0-based), both channels.
    fn rms(x: &[f32], lo: usize, hi: usize) -> f64 {
        let s = &x[lo * BAR * 2..hi * BAR * 2];
        (s.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / s.len() as f64).sqrt()
    }

    fn quiet() -> Vec<f64> {
        let mut p = defaults();
        p[ZAPS] = 0.0;
        p
    }

    #[test]
    fn deterministic_and_in_range() {
        let a = render(18, &defaults(), true);
        assert_eq!(a, render(18, &defaults(), true));
        assert!(a.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
    }

    #[test]
    fn riser_builds_into_bar_17_then_impact() {
        let out = render(18, &quiet(), true);
        // 4-bar riser over bars 13-16 (0-based 12..16), louder towards the end.
        assert!(rms(&out, 0, 12) < 1e-4, "quiet before the riser");
        assert!(rms(&out, 15, 16) > 2.0 * rms(&out, 12, 13));
        // Impact on bar 17's downbeat: the first 200 ms are loud.
        let at = 16 * BAR * 2;
        let peak = out[at..at + 9600 * 2].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.3, "impact peak {peak}");
    }

    #[test]
    fn silent_when_stopped() {
        assert!(render(17, &defaults(), false).iter().all(|&x| x == 0.0));
    }

    #[test]
    fn zaps_follow_the_param() {
        let mut none = quiet();
        none[RISER] = 0.0;
        let mut all = none.clone();
        all[ZAPS] = 1.0;
        assert!(rms(&render(8, &none, true), 0, 8) < 1e-6);
        assert!(rms(&render(8, &all, true), 0, 8) > 1e-3);
    }
}
