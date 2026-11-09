//! percussion — generative psytrance percussion: off-beat open hat, closed
//! hats, shaker, clap, ride, panned percs and snare rolls. Stereo.
//!
//! Structure follows the shared grid (see gen_api::Position):
//! - per epoch (16 bars): which layers play and in what style
//! - per cycle (4 bars): the actual pattern, with a fill at its end
//! - the last bar of an epoch often carries a snare roll into the next one
//!
//! The workspace score, per 16th: `quiet` drops closed hats, percs and
//! ride (and clap and snare when strong) and turns down, by its intensity;
//! `break` keeps only the open hat and shaker (thinned by intensity);
//! `rise` builds a snare roll, 8ths from the region's middle and 16ths
//! getting louder in its last bar; `peak` fills in closed hats and adds the
//! ride. Feel: half keeps hats and shaker on the 8ths, the open hat on
//! beats 1 and 3 and the clap on 3; double puts closed hats on every 32nd,
//! doubles shaker and percs and adds snare ghosts on the off-beat 16ths;
//! quad does the same with a 64th hat shimmer; triplet moves the closed
//! hats to a triplet grid. Percs follow the score's chord root.

use dsp::{decay_coef, midi_hz, pan_gains, Svf};
use gen_api::{Config, Context, Entry, Feel, Generator, Info, Mark, Param, Rng, StepClock};
use std::f64::consts::PI;

const SLOTS: usize = 64; // 16ths in a cycle (4 bars)
const CYCLES_PER_EPOCH: u64 = 4;

/// Fixed output stage: soft clip, then a -1 dBFS ceiling.
const DRIVE: usize = 0;

/// The built-in output saturation, exposed.
const PARAMS: [Param; 1] = [Param::continuous("drive", "Drive", 0.5, 4.0, 1.0).glide(2.0)];
const CEILING: f64 = 0.891;

/// Closed-hat velocity per 16th within a beat; slot 2 belongs to the open hat.
const CHAT_STYLES: [(&str, [f64; 4]); 3] = [
    ("16ths", [0.45, 0.7, 0.0, 0.8]),
    ("1+3  ", [0.0, 0.65, 0.0, 0.8]),
    ("last ", [0.0, 0.0, 0.0, 0.8]),
];
const CHAT_WEIGHTS: [f64; 3] = [3.0, 2.0, 1.0];
const SHAKER_VEL: [f64; 4] = [0.5, 0.85, 0.6, 0.95];

// ---------------------------------------------------------------- voices

/// Six inharmonic squares (the 808 recipe) plus noise, band-limited.
const METAL_HZ: [f64; 6] = [205.3, 304.4, 369.6, 522.7, 540.0, 800.0];

#[derive(Clone, Copy)]
struct MetalSound {
    pitch: f64, // multiplier on METAL_HZ
    noise: f64, // 0 = all squares, 1 = all noise
    bp: f64,
    hp: f64,
    decay: f64, // time constant, s
    gain: f64,
}

struct Metal {
    s: MetalSound,
    phases: [f64; 6],
    amp: f64,
    coef: f64,
    bp: Svf,
    hp: Svf,
}

impl Metal {
    fn new(s: MetalSound, sr: f64) -> Metal {
        Metal { s, phases: [0.0, 0.13, 0.31, 0.47, 0.62, 0.89], amp: 0.0, coef: 0.0, bp: Svf::new(s.bp, 0.3, sr), hp: Svf::new(s.hp, 0.1, sr) }
    }

    fn trigger(&mut self, vel: f64, decay_scale: f64, sr: f64) {
        self.amp = vel;
        self.coef = decay_coef(self.s.decay * decay_scale, sr);
    }

    /// Cut a ringing sound short without a click.
    fn choke(&mut self, sr: f64) {
        self.coef = self.coef.min(decay_coef(0.006, sr));
    }

    fn tick(&mut self, sr: f64, noise: &mut Rng) -> f64 {
        if self.amp < 1e-5 {
            return 0.0;
        }
        let mut sq = 0.0;
        for (ph, hz) in self.phases.iter_mut().zip(METAL_HZ) {
            *ph = (*ph + hz * self.s.pitch / sr).fract();
            sq += if *ph < 0.5 { 1.0 } else { -1.0 };
        }
        let x = sq / 6.0 * (1.0 - self.s.noise) + noise.range(-1.0, 1.0) * self.s.noise;
        let (_, b, _) = self.bp.tick(x);
        let (_, _, h) = self.hp.tick(b);
        let y = h * self.amp * self.s.gain;
        self.amp *= self.coef;
        y
    }
}

/// Bandpassed noise with a soft attack — shaker.
struct Shaker {
    t: usize,
    vel: f64,
    bp: Svf,
}

impl Shaker {
    fn tick(&mut self, sr: f64, noise: &mut Rng) -> f64 {
        if self.vel == 0.0 {
            return 0.0;
        }
        let t = self.t as f64 / sr;
        self.t += 1;
        if t > 0.15 {
            self.vel = 0.0;
        }
        let env = (t / 0.004).min(1.0) * (-t / 0.02).exp();
        let (_, b, _) = self.bp.tick(noise.range(-1.0, 1.0));
        b * env * self.vel * 0.9
    }
}

/// Three quick noise bursts and a tail — clap.
struct Clap {
    t: usize,
    vel: f64,
    bp: Svf,
}

impl Clap {
    fn tick(&mut self, sr: f64, noise: &mut Rng) -> f64 {
        if self.vel == 0.0 {
            return 0.0;
        }
        let t = self.t as f64 / sr;
        self.t += 1;
        if t > 0.4 {
            self.vel = 0.0;
        }
        let mut env = 0.0;
        for k in 0..3 {
            let tk = t - k as f64 * 0.009;
            if tk >= 0.0 {
                env += (-tk / 0.003).exp();
            }
        }
        if t >= 0.02 {
            env += 0.6 * (-(t - 0.02) / 0.07).exp();
        }
        let (_, b, _) = self.bp.tick(noise.range(-1.0, 1.0));
        b * env * self.vel * 1.4
    }
}

/// Tone + highpassed noise — snare for rolls and fills.
struct Snare {
    t: usize,
    vel: f64,
    decay: f64,
    phase: f64,
    hp: Svf,
}

impl Snare {
    fn tick(&mut self, sr: f64, noise: &mut Rng) -> f64 {
        if self.vel == 0.0 {
            return 0.0;
        }
        let t = self.t as f64 / sr;
        self.t += 1;
        if t > self.decay * 8.0 {
            self.vel = 0.0;
        }
        self.phase += (180.0 + 70.0 * (-t / 0.01).exp()) / sr;
        let tone = (2.0 * PI * self.phase).sin() * (-t / 0.04).exp() * 0.5;
        let (_, _, h) = self.hp.tick(noise.range(-1.0, 1.0));
        let body = h * (-t / self.decay).exp();
        (tone + body) * self.vel * 0.8
    }
}

/// Short pitched blip with a pitch drop — the "psy perc".
#[derive(Default)]
struct Perc {
    t: usize,
    vel: f64,
    hz: f64,
    pan: f64,
    phase: f64,
}

impl Perc {
    fn tick(&mut self, sr: f64) -> f64 {
        if self.vel == 0.0 {
            return 0.0;
        }
        let t = self.t as f64 / sr;
        self.t += 1;
        if t > 0.3 {
            self.vel = 0.0;
        }
        self.phase += self.hz * (1.0 + 1.2 * (-t / 0.004).exp()) / sr;
        let y = ((2.0 * PI * self.phase).sin() * 2.0).tanh();
        y * (-t / 0.035).exp() * self.vel * 0.22
    }
}

// ---------------------------------------------------------------- composition

#[derive(Clone, Copy, Default)]
struct Step {
    ohat: f64,
    chat: f64,
    shaker: f64,
    clap: f64,
    ride: f64,
    snare: f64,
    perc: f64,
    /// Scale degree, resolved against the host key when it plays.
    perc_degree: f64,
    perc_pan: f64,
}

struct Epoch {
    index: u64,
    chat_style: usize,
    shaker: bool,
    clap: bool,
    ride: bool,
    perc_density: f64,
    roll: bool,
}

struct Cycle {
    index: u64,
    steps: [Step; SLOTS],
}

// ---------------------------------------------------------------- generator

pub struct Percussion {
    sr: f64,
    rng: Rng,
    noise: Rng,
    epoch: Option<Epoch>,
    cycle: Option<Cycle>,
    clock: StepClock,
    drive: f64,
    param_smooth: f64,
    ohat: Metal,
    chat: Metal,
    ride: Metal,
    shaker: Shaker,
    clap: Clap,
    snare: Snare,
    perc: Perc,
    /// Seeds the per-16th draws that thin layers out.
    thin_seed: u64,
    /// Hits due between 16ths (double, quad and triplet feel). Fixed size,
    /// so `process` never allocates; a full queue drops new hits.
    pending: [Pending; MAX_PENDING],
    pending_len: usize,
}

const MAX_PENDING: usize = 32;

#[derive(Clone, Copy, Default)]
enum Hit {
    #[default]
    Chat,
    Shaker,
    /// The last perc again, same pitch and pan.
    Perc,
}

#[derive(Clone, Copy, Default)]
struct Pending {
    /// Samples to go.
    t: f64,
    hit: Hit,
    vel: f64,
}

const PAN_OHAT: f64 = -0.1;
const PAN_CHAT: f64 = 0.3;
const PAN_SHAKER: f64 = -0.4;
const PAN_RIDE: f64 = 0.45;

impl Percussion {
    fn compose_epoch(&mut self, index: u64) -> Epoch {
        let r = &mut self.rng;
        let e = Epoch {
            index,
            chat_style: r.weighted(&CHAT_WEIGHTS),
            shaker: r.chance(0.6),
            clap: r.chance(0.5),
            ride: r.chance(0.35),
            perc_density: if r.chance(0.75) { r.range(0.08, 0.3) } else { 0.0 },
            roll: r.chance(0.65),
        };
        let on = |b: bool| if b { "on " } else { "off" };
        let first_bar = index * CYCLES_PER_EPOCH * 4 + 1;
        eprintln!(
            "  bars {:>4}-{:<4} hats {}  shaker {}  clap {}  ride {}  percs {:>2.0}%  roll {}",
            first_bar,
            first_bar + 15,
            CHAT_STYLES[e.chat_style].0,
            on(e.shaker),
            on(e.clap),
            on(e.ride),
            e.perc_density * 100.0,
            on(e.roll)
        );
        e
    }

    fn compose_cycle(&mut self, index: u64) -> Cycle {
        let epoch_index = index / CYCLES_PER_EPOCH;
        if self.epoch.as_ref().map(|e| e.index) != Some(epoch_index) {
            self.epoch = Some(self.compose_epoch(epoch_index));
        }
        let e = self.epoch.as_ref().unwrap();
        let r = &mut self.rng;
        let last_in_epoch = index % CYCLES_PER_EPOCH == CYCLES_PER_EPOCH - 1;
        let roll = e.roll && last_in_epoch;

        // One bar of percs, repeated, with the last bar varied.
        let n_pitches = 2 + (r.next_u64() % 2) as usize;
        // Percs are tuned: scale degrees from the root up about an octave
        // and a half, so they sit in key with the bass and acid.
        let pitches: Vec<f64> = (0..n_pitches).map(|_| (r.next_u64() % 12) as f64).collect();
        let mut perc_bar = [(0.0, 0.0, 0.0); 16];
        let mut side = if r.chance(0.5) { 1.0 } else { -1.0 };
        let mut roll_perc = |r: &mut Rng| {
            side = -side;
            (r.range(0.6, 0.9), *r.pick(&pitches), side * r.range(0.4, 0.8))
        };
        for (s, p) in perc_bar.iter_mut().enumerate() {
            if s % 4 != 0 && r.chance(e.perc_density) {
                *p = roll_perc(r);
            }
        }

        let mut steps = [Step::default(); SLOTS];
        for bar in 0..4 {
            for beat in 0..4 {
                for s in 0..4 {
                    let i = bar * 16 + beat * 4 + s;
                    let st = &mut steps[i];
                    let jitter = r.range(0.9, 1.05);
                    if s == 2 {
                        st.ohat = r.range(0.85, 1.0);
                    }
                    st.chat = CHAT_STYLES[e.chat_style].1[s] * jitter;
                    if e.shaker {
                        st.shaker = SHAKER_VEL[s] * r.range(0.85, 1.0);
                    }
                    if e.clap && s == 0 && beat % 2 == 1 {
                        st.clap = 0.85;
                    }
                    if e.ride && s == 0 {
                        st.ride = r.range(0.5, 0.65);
                    }
                    let mut p = perc_bar[beat * 4 + s];
                    if bar == 3 && s % 4 != 0 && r.chance(0.3) {
                        p = if r.chance(e.perc_density.max(0.1)) { roll_perc(r) } else { (0.0, 0.0, 0.0) };
                    }
                    (st.perc, st.perc_degree, st.perc_pan) = p;
                }
            }
        }

        let last_bar = &mut steps[48..64];
        if roll {
            // Snare roll into the next epoch: 8ths, then 16ths, rising.
            for (j, st) in last_bar.iter_mut().enumerate() {
                if j < 8 && j % 2 == 1 {
                    continue;
                }
                st.snare = 0.25 + 0.75 * (j as f64 / 15.0).powf(1.5);
                st.clap = 0.0;
            }
        } else if r.chance(0.5) {
            // Small fill on the last beat.
            if r.chance(0.5) {
                last_bar[14].snare = 0.5;
                last_bar[15].snare = 0.7;
            } else {
                last_bar[15].ohat = 0.8;
                last_bar[15].chat = 0.0;
            }
        }
        Cycle { index, steps }
    }

    /// Applies the score's dynamics and feel at `slot` (an absolute 16th)
    /// to its step. `six` is a 16th in samples.
    fn shape(&mut self, st: &mut Step, slot: u64, ctx: &Context, six: f64) {
        let bar = ctx.score.bar(slot / 16);
        let k = bar.intensity as f64;
        let p = ctx.score.progress(slot as f64 / 4.0, 4);
        let draw = Rng::new(self.thin_seed ^ slot.wrapping_mul(0x9E37_79B9_7F4A_7C15)).f();
        let s16 = slot % 16;
        let mut gain = 1.0;
        match bar.mark {
            Mark::Quiet => {
                if draw < k {
                    (st.chat, st.perc, st.ride) = (0.0, 0.0, 0.0);
                }
                if k > 0.5 {
                    (st.clap, st.snare) = (0.0, 0.0);
                }
                gain = 1.0 - 0.4 * k;
            }
            Mark::Break => {
                (st.clap, st.snare, st.perc) = (0.0, 0.0, 0.0);
                if draw < k {
                    (st.chat, st.ride) = (0.0, 0.0);
                }
                gain = 1.0 - 0.4 * k;
            }
            Mark::Rise => {
                let last_bar = slot / 16 == bar.mark_start + bar.mark_bars as u64 - 1;
                if last_bar {
                    st.snare = st.snare.max((0.35 + 0.6 * s16 as f64 / 15.0) * k);
                } else if p >= 0.5 && slot % 2 == 0 {
                    st.snare = st.snare.max((0.2 + 0.4 * p) * k);
                }
                st.chat *= 1.0 + 0.3 * p * k;
            }
            Mark::Peak => {
                if st.chat == 0.0 && s16 % 4 != 2 {
                    st.chat = 0.4 * k;
                }
                if st.ride == 0.0 && s16 % 4 == 2 {
                    st.ride = 0.45 * k;
                }
                gain = 1.0 + 0.1 * k;
            }
            Mark::Normal => {}
        }
        match bar.feel {
            Feel::Half => {
                if slot % 2 == 1 {
                    (st.chat, st.shaker) = (0.0, 0.0);
                }
                if (slot / 4) % 2 == 1 {
                    st.ohat = 0.0;
                }
                let clap_layer = self.epoch.as_ref().is_some_and(|e| e.clap);
                st.clap = if s16 == 8 && clap_layer && bar.mark != Mark::Break { 0.9 } else { 0.0 };
            }
            Feel::Double | Feel::Quad => {
                // Closed hats on every 32nd (64th in quad), unless quiet or
                // break thinned the hats out here; not on the open hat's
                // own onset, which a closed hat would choke.
                let n = if bar.feel == Feel::Double { 2 } else { 4 };
                let thinned = matches!(bar.mark, Mark::Quiet | Mark::Break) && draw < k;
                if !thinned {
                    let hat = if st.chat > 0.0 { st.chat } else { 0.4 };
                    if st.ohat == 0.0 {
                        st.chat = hat;
                    }
                    for j in 1..n {
                        let accent = if j * 2 == n { 0.75 } else { 0.5 };
                        self.queue(six * j as f64 / n as f64, Hit::Chat, hat * accent * gain);
                    }
                }
                if st.shaker > 0.0 {
                    self.queue(six / 2.0, Hit::Shaker, st.shaker * 0.7 * gain);
                }
                if st.perc > 0.0 {
                    self.queue(six / 2.0, Hit::Perc, st.perc * 0.75 * gain);
                }
                // Light snare ghosts on the 16ths between beats.
                if st.snare == 0.0 && s16 % 4 != 0 && bar.mark != Mark::Break && !(bar.mark == Mark::Quiet && k > 0.5) {
                    st.snare = 0.12 + 0.1 * draw;
                }
            }
            Feel::Triplet => {
                if s16 % 4 == 0 && !(bar.mark == Mark::Break && draw < k) {
                    let vel = [0.6, 0.4, 0.5, 0.6, 0.4, 0.5];
                    for (j, v) in vel.iter().enumerate() {
                        self.queue(4.0 * six * j as f64 / 6.0, Hit::Chat, v * gain);
                    }
                }
                st.chat = 0.0;
            }
            _ => {}
        }
        for v in [&mut st.ohat, &mut st.chat, &mut st.shaker, &mut st.clap, &mut st.ride, &mut st.snare, &mut st.perc] {
            *v = (*v * gain).min(1.0);
        }
    }

    fn queue(&mut self, t: f64, hit: Hit, vel: f64) {
        if self.pending_len < MAX_PENDING {
            self.pending[self.pending_len] = Pending { t, hit, vel: vel.min(1.0) };
            self.pending_len += 1;
        }
    }

    /// Counts the queue down by one sample and plays what's due.
    fn play_pending(&mut self) {
        let sr = self.sr;
        let mut i = 0;
        while i < self.pending_len {
            let p = &mut self.pending[i];
            p.t -= 1.0;
            if p.t > 0.0 {
                i += 1;
                continue;
            }
            let Pending { hit, vel, .. } = *p;
            self.pending_len -= 1;
            self.pending[i] = self.pending[self.pending_len];
            match hit {
                Hit::Chat => {
                    self.ohat.choke(sr);
                    self.chat.trigger(vel, 1.0, sr);
                }
                Hit::Shaker => {
                    self.shaker.t = 0;
                    self.shaker.vel = vel;
                }
                Hit::Perc => self.perc = Perc { vel, hz: self.perc.hz, pan: self.perc.pan, ..Perc::default() },
            }
        }
    }

    fn trigger(&mut self, slot: u64, ctx: &Context) {
        let index = slot / SLOTS as u64;
        if self.cycle.as_ref().map(|c| c.index) != Some(index) {
            self.cycle = Some(self.compose_cycle(index));
        }
        let mut st = self.cycle.as_ref().unwrap().steps[(slot % SLOTS as u64) as usize];
        let sr = self.sr;
        let six = 60.0 / ctx.transport.bpm / 4.0;
        self.shape(&mut st, slot, ctx, six * sr);

        if st.ohat > 0.0 {
            self.ohat.trigger(st.ohat, 1.0, sr);
        }
        if st.chat > 0.0 {
            self.ohat.choke(sr);
            self.chat.trigger(st.chat, 1.0, sr);
        }
        if st.ride > 0.0 {
            self.ride.trigger(st.ride, 1.0, sr);
        }
        if st.shaker > 0.0 {
            self.shaker.t = 0;
            self.shaker.vel = st.shaker;
        }
        if st.clap > 0.0 {
            self.clap.t = 0;
            self.clap.vel = st.clap;
        }
        if st.snare > 0.0 {
            self.snare.t = 0;
            self.snare.vel = st.snare;
            self.snare.phase = 0.0;
            self.snare.decay = (six * 0.5).min(0.08);
        }
        if st.perc > 0.0 {
            // C4..B4 for the root, then up the scale.
            let t = &ctx.tonality;
            let (root, semis) = t.chord_degree(ctx.score.bar(slot / 16).chord, st.perc_degree as i32);
            let hz = midi_hz(60.0 + root as f64 + semis as f64);
            self.perc = Perc { vel: st.perc, hz, pan: st.perc_pan, ..Perc::default() };
        }
    }
}

impl Generator for Percussion {
    fn new(config: &Config) -> Self {
        let sr = config.sample_rate;
        let mut rng = Rng::new(config.seed ^ 0x9E2C);
        // Sound design varies a little per seed.
        let tone = rng.range(0.9, 1.15);
        Percussion {
            sr,
            noise: Rng::new(config.seed ^ 0x401_5E),
            epoch: None,
            cycle: None,
            clock: StepClock::new(),
            drive: PARAMS[DRIVE].default,
            param_smooth: 1.0 - (-1.0 / (0.02 * sr)).exp(),
            ohat: Metal::new(MetalSound { pitch: 1.6 * tone, noise: 0.35, bp: 9000.0, hp: 6500.0, decay: 0.07, gain: 3.2 }, sr),
            chat: Metal::new(MetalSound { pitch: 1.6 * tone, noise: 0.45, bp: 10000.0, hp: 7500.0, decay: 0.012, gain: 2.6 }, sr),
            ride: Metal::new(MetalSound { pitch: 1.2 * tone, noise: 0.15, bp: 5500.0, hp: 3500.0, decay: 0.35, gain: 0.7 }, sr),
            shaker: Shaker { t: 0, vel: 0.0, bp: Svf::new(rng.range(6000.0, 8000.0), 0.35, sr) },
            clap: Clap { t: 0, vel: 0.0, bp: Svf::new(rng.range(1000.0, 1500.0), 0.4, sr) },
            snare: Snare { t: 0, vel: 0.0, decay: 0.08, phase: 0.0, hp: Svf::new(1800.0, 0.1, sr) },
            perc: Perc::default(),
            thin_seed: config.seed ^ 0x9E2C,
            pending: [Pending::default(); MAX_PENDING],
            pending_len: 0,
            rng,
        }
    }

    fn params(&self) -> &'static [Param] {
        &PARAMS
    }

    /// Off-beat hats already sit between the kicks; duck only lightly.
    fn duck_sensitivity(&self) -> f64 {
        0.2
    }

    fn category(&self) -> &'static str {
        "drums"
    }

    /// Patterns start on the downbeat.
    fn entry(&self) -> Entry {
        Entry::Bar
    }

    fn info(&self) -> Info {
        Info {
            name: "percussion",
            description: "Psytrance hats, shaker, clap, ride, percs and snare rolls (stereo)",
            channels: 2,
        }
    }

    fn process(&mut self, ctx: &Context, out: &mut [f32]) {
        let mut steps = self.clock.block(ctx);
        let sr = self.sr;
        let pans = [pan_gains(PAN_OHAT), pan_gains(PAN_CHAT), pan_gains(PAN_RIDE), pan_gains(PAN_SHAKER)];
        for (i, frame) in out.chunks_exact_mut(2).enumerate() {
            while let Some(slot) = steps.due(i) {
                self.trigger(slot, ctx);
            }
            if self.pending_len > 0 {
                self.play_pending();
            }
            let n = &mut self.noise;
            let mono = [self.ohat.tick(sr, n), self.chat.tick(sr, n), self.ride.tick(sr, n), self.shaker.tick(sr, n)];
            let center = self.clap.tick(sr, n) + self.snare.tick(sr, n);
            let (mut l, mut r) = (center * 0.707, center * 0.707);
            for (x, (gl, gr)) in mono.iter().zip(pans) {
                l += x * gl;
                r += x * gr;
            }
            let (pl, pr) = pan_gains(self.perc.pan);
            let p = self.perc.tick(sr);
            l += p * pl;
            r += p * pr;
            self.drive += (ctx.param(DRIVE) - self.drive) * self.param_smooth;
            frame[0] = ((l * self.drive).tanh() * CEILING) as f32;
            frame[1] = ((r * self.drive).tanh() * CEILING) as f32;
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
        let mut g = Percussion::new(&Config { sample_rate: SR, seed: 7 });
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
    fn double_and_quad_are_clearly_busier() {
        let normal = rms(&render(8, &Score::EMPTY).0);
        let double = render(8, &score(Mark::Normal, Feel::Double)).0;
        let quad = render(8, &score(Mark::Normal, Feel::Quad)).0;
        assert!(rms(&double) > normal * 1.15, "double: {} vs {normal}", rms(&double));
        assert!(rms(&quad) > normal * 1.15, "quad: {} vs {normal}", rms(&quad));
        assert_ne!(quad, double, "quad shimmers in 64ths");
    }

    #[test]
    fn break_thins_out_and_feel_changes_the_hats() {
        let (normal, _) = render(8, &Score::EMPTY);
        assert!(rms(&render(8, &score(Mark::Break, Feel::Normal)).0) < rms(&normal) * 0.8);
        for feel in [Feel::Half, Feel::Double, Feel::Quad, Feel::Triplet] {
            assert_ne!(render(8, &score(Mark::Normal, feel)).0, normal, "{feel:?}");
        }
    }
}
