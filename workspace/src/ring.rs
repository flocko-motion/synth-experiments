//! Single-producer single-consumer sample ring between the mixer thread and
//! the audio callback. Lock-free and allocation-free on both sides, so the
//! callback never blocks.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

pub struct Ring {
    buf: Box<[AtomicU32]>,
    /// Total samples ever written / read; the difference is the fill level.
    head: AtomicUsize,
    tail: AtomicUsize,
}

impl Ring {
    pub fn new(capacity: usize) -> Arc<Ring> {
        Arc::new(Ring {
            buf: (0..capacity).map(|_| AtomicU32::new(0)).collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        })
    }

    pub fn len(&self) -> usize {
        self.head.load(Ordering::Acquire) - self.tail.load(Ordering::Acquire)
    }

    pub fn free(&self) -> usize {
        self.buf.len() - self.len()
    }

    /// Producer side. Writes as much of `xs` as fits; returns how much.
    pub fn push(&self, xs: &[f32]) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let n = xs.len().min(self.buf.len() - (head - tail));
        for (i, x) in xs[..n].iter().enumerate() {
            self.buf[(head + i) % self.buf.len()].store(x.to_bits(), Ordering::Relaxed);
        }
        self.head.store(head + n, Ordering::Release);
        n
    }

    /// Consumer side. Fills as much of `out` as is available; returns how
    /// much.
    pub fn pop(&self, out: &mut [f32]) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let n = out.len().min(head - tail);
        for (i, o) in out[..n].iter_mut().enumerate() {
            *o = f32::from_bits(self.buf[(tail + i) % self.buf.len()].load(Ordering::Relaxed));
        }
        self.tail.store(tail + n, Ordering::Release);
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_respects_capacity() {
        let r = Ring::new(5);
        assert_eq!(r.push(&[1.0, 2.0, 3.0, 4.0]), 4);
        let mut out = [0.0; 3];
        assert_eq!(r.pop(&mut out), 3);
        assert_eq!(out, [1.0, 2.0, 3.0]);
        assert_eq!(r.push(&[5.0, 6.0, 7.0, 8.0, 9.0]), 4);
        let mut out = [0.0; 8];
        assert_eq!(r.pop(&mut out), 5);
        assert_eq!(out[..5], [4.0, 5.0, 6.0, 7.0, 8.0]);
    }
}
