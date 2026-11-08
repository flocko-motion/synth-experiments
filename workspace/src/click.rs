//! The metronome. One short sine beep per beat. Pitch and level rise with the
//! grid level the beat starts: beat < bar < cycle < epoch, so you can hear
//! where you are.

use gen_api::Position;
use std::f64::consts::TAU;

pub struct Click {
    sample_rate: f64,
    phase: f64,
    freq: f64,
    env: f64,
    decay: f64,
    last_beat: Option<u64>,
}

impl Click {
    pub fn new(sample_rate: f64) -> Click {
        Click { sample_rate, phase: 0.0, freq: 1000.0, env: 0.0, decay: (-1.0 / (0.012 * sample_rate)).exp(), last_beat: None }
    }

    /// Forget the last beat, e.g. after a seek, so the next beat clicks.
    pub fn reset(&mut self) {
        self.last_beat = None;
    }

    /// Adds the click into interleaved `mix`. `beat_at(i)` is the beat
    /// position of frame `i`, or None while stopped.
    pub fn render(&mut self, mix: &mut [f32], channels: usize, beats_per_bar: u32, gain: f32, beat_at: impl Fn(usize) -> Option<f64>) {
        for (i, frame) in mix.chunks_exact_mut(channels).enumerate() {
            if let Some(beat) = beat_at(i) {
                let whole = beat.floor() as u64;
                if self.last_beat != Some(whole) {
                    self.last_beat = Some(whole);
                    let p = Position::at(whole as f64, beats_per_bar);
                    let (freq, level) = match (p.beat, p.bar, p.cycle) {
                        (0, 0, 0) => (2400.0, 1.0), // epoch
                        (0, 0, _) => (1800.0, 0.85), // cycle
                        (0, _, _) => (1500.0, 0.7), // bar
                        _ => (1000.0, 0.5),
                    };
                    self.freq = freq;
                    self.env = level;
                    self.phase = 0.0;
                }
            }
            if self.env < 1e-4 {
                continue;
            }
            let s = ((TAU * self.phase).sin() * self.env) as f32 * gain;
            self.phase = (self.phase + self.freq / self.sample_rate).fract();
            self.env *= self.decay;
            for x in frame {
                *x += s;
            }
        }
    }
}
