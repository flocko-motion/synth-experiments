//! Finding grid steps (16ths) inside a block.
//!
//! ```ignore
//! fn process(&mut self, ctx: &Context, out: &mut [f32]) {
//!     let mut steps = self.clock.block(ctx);
//!     for (i, o) in out.iter_mut().enumerate() {
//!         while let Some(step) = steps.due(i) {
//!             self.trigger(step, ctx); // step = absolute 16th since 0
//!         }
//!         *o = self.render_one_sample();
//!     }
//!     self.clock.finish(steps);
//! }
//! ```
//!
//! The clock remembers the next step it hasn't fired. If a block starts
//! somewhere that step can't be the next one (first block, a seek, a loop,
//! a restart, blocks skipped by a lagging host), it resumes at the next step
//! boundary instead of firing a burst of missed steps.

use crate::{Context, TICKS_PER_BEAT};

const EPS: f64 = 1e-9;

/// Per-generator state: the next step not yet fired.
#[derive(Clone, Copy, Debug, Default)]
pub struct StepClock {
    next: Option<u64>,
}

/// The steps of one block. Get it from [`StepClock::block`], ask it with
/// [`Steps::due`] for every frame in order, and hand it back with
/// [`StepClock::finish`].
#[derive(Clone, Copy, Debug)]
pub struct Steps {
    start: f64,    // block start, in steps
    step_len: f64, // frames per step
    next: u64,
    playing: bool,
}

impl StepClock {
    pub fn new() -> StepClock {
        StepClock::default()
    }

    pub fn block(&self, ctx: &Context) -> Steps {
        let tr = &ctx.transport;
        let start = tr.beat * TICKS_PER_BEAT as f64;
        let next = match self.next {
            Some(n) if (n as f64) >= start - EPS && (n as f64) <= start + 1.0 + EPS => n,
            _ => (start - EPS).ceil().max(0.0) as u64,
        };
        Steps {
            start,
            step_len: tr.samples_per_beat(ctx.sample_rate) / TICKS_PER_BEAT as f64,
            next,
            playing: tr.playing,
        }
    }

    pub fn finish(&mut self, steps: Steps) {
        self.next = Some(steps.next);
    }
}

impl Steps {
    /// The next step due at or before `frame` (index within the block), if
    /// any. Call repeatedly until `None`, for every frame in order. While the
    /// transport is stopped nothing is due.
    pub fn due(&mut self, frame: usize) -> Option<u64> {
        if !self.playing {
            return None;
        }
        let pos = self.start + frame as f64 / self.step_len;
        if self.next as f64 <= pos + EPS {
            self.next += 1;
            Some(self.next - 1)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Scale, Tonality, Transport};

    fn ctx(beat: f64, playing: bool) -> Context {
        Context {
            sample_rate: 48_000.0,
            transport: Transport { bpm: 120.0, beat, beats_per_bar: 4, playing },
            tonality: Tonality { root: 0, scale: Scale::Minor },
        }
    }

    /// Run blocks of `frames` starting at the given beats; collect
    /// (block, frame, step).
    fn run(clock: &mut StepClock, starts: &[f64], frames: usize, playing: bool) -> Vec<(usize, usize, u64)> {
        let mut got = Vec::new();
        for (b, &beat) in starts.iter().enumerate() {
            let mut steps = clock.block(&ctx(beat, playing));
            for i in 0..frames {
                while let Some(s) = steps.due(i) {
                    got.push((b, i, s));
                }
            }
            clock.finish(steps);
        }
        got
    }

    // 120 bpm at 48 kHz: 6000 frames per step.

    #[test]
    fn fires_each_step_once_across_blocks() {
        let mut c = StepClock::new();
        let starts: Vec<f64> = (0..6).map(|b| b as f64 * 4000.0 / 24000.0).collect();
        let got = run(&mut c, &starts, 4000, true);
        let steps: Vec<u64> = got.iter().map(|g| g.2).collect();
        assert_eq!(steps, vec![0, 1, 2, 3]);
        assert_eq!(got[1], (1, 2000, 1)); // step 1 at frame 6000 = block 1 frame 2000
    }

    #[test]
    fn seek_resumes_at_next_boundary() {
        let mut c = StepClock::new();
        // block 0 at 0, then jump to beat 10.1 (step 40.4) → next is 41
        let got = run(&mut c, &[0.0, 10.1], 6000, true);
        assert_eq!(got.iter().map(|g| g.2).collect::<Vec<_>>(), vec![0, 41]);
    }

    #[test]
    fn stopped_fires_nothing() {
        let mut c = StepClock::new();
        assert!(run(&mut c, &[0.0, 0.25], 6000, false).is_empty());
    }
}
