//! Generator subprocesses, and keeping them alive.
//!
//! Generators are written quickly and expected to break. A [`Slot`] is the
//! mixer's lasting entry for one: name, command, gain. It outlives any number
//! of [`Proc`]s, the actual subprocesses. When a proc crashes, hangs, garbles
//! its output or produces NaN, the slot kills it and starts a fresh one after
//! a backoff. The new proc joins at the current position, like a late join.
//!
//! The mixer never touches pipes. A worker thread per proc does the I/O: the
//! mixer hands it a request, carries on, and later collects the chunk with a
//! deadline. A chunk that misses its deadline is mixed as silence and dropped
//! when it turns up.

use crate::log::wlog;
use crate::sys;
use gen_api::pipe::{decode_chunk_header, read_gen_hello, write_request, Hello, Request, CHUNK_HEADER_LEN};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// No chunk at all for this long while requests are pending: hung.
const HANG_TIMEOUT: Duration = Duration::from_secs(1);
/// Requests a proc may have outstanding. Past this the mixer skips it for a
/// block instead of queueing; to the generator that looks like a seek.
const MAX_IN_FLIGHT: u32 = 2;
/// Consecutive blocks containing NaN/inf before the proc is restarted.
const NAN_LIMIT: u32 = 40;
/// Time a proc gets to exit after its stdin closes, before it's killed.
const EXIT_GRACE: Duration = Duration::from_millis(300);
/// A proc that ran this long resets the restart backoff.
const STABLE_AFTER: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// stderr lines per second per generator before the rest are dropped.
const LOG_LINES_PER_SEC: u32 = 40;
const LOG_LINE_MAX: usize = 400;

/// How to (re)start a generator.
#[derive(Clone)]
pub struct Spec {
    pub argv: Vec<String>,
    /// Where `ws add` ran; relative paths resolve against it.
    pub cwd: PathBuf,
    pub sample_rate: u32,
    pub max_frames: u32,
}

struct Job {
    seq: u64,
    req: Request,
    /// Every parameter's value, in description order.
    params: Vec<f64>,
    /// Effects: `frames * inputs` samples, interleaved. Empty means silence.
    input: Vec<f32>,
}

/// Remixes interleaved audio from `from` to `to` channels: mono goes to
/// every channel, more to fewer is averaged, otherwise channels map by index
/// and the last one repeats.
pub fn adapt_channels(src: &[f32], from: usize, to: usize) -> Vec<f32> {
    if from == to || from == 0 {
        return src.to_vec();
    }
    let frames = src.len() / from;
    let mut out = Vec::with_capacity(frames * to);
    for f in src.chunks_exact(from) {
        if to < from {
            // Average groups so stereo -> mono keeps both sides.
            for c in 0..to {
                let group: Vec<f32> = f.iter().skip(c).step_by(to).copied().collect();
                out.push(group.iter().sum::<f32>() / group.len() as f32);
            }
        } else {
            for c in 0..to {
                out.push(f[c.min(from - 1)]);
            }
        }
    }
    out
}

/// What a generator says about itself in the handshake (the same JSON as
/// `--info --json`).
#[derive(Deserialize, Clone, Debug)]
pub struct Description {
    #[allow(dead_code)] // part of the format; the slot has its own name
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub params: Vec<ParamDesc>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ParamDesc {
    pub id: String,
    pub name: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    /// continuous, integer, toggle or choice
    pub kind: String,
    #[serde(default)]
    pub log: bool,
    #[serde(default)]
    pub unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
}

impl ParamDesc {
    /// Clamps to the range; integer, toggle and choice params snap to
    /// whole numbers.
    pub fn clamp(&self, v: f64) -> f64 {
        let v = if v.is_finite() { v.clamp(self.min, self.max) } else { self.default };
        if self.kind == "continuous" {
            v
        } else {
            v.round()
        }
    }
}

/// The handshake's result.
struct Ready {
    channels: usize,
    inputs: usize,
    desc: Description,
}

struct Chunk {
    seq: u64,
    samples: Vec<f32>,
}

/// One running generator subprocess.
pub struct Proc {
    name: String,
    pub channels: usize,
    /// Input channels; non-zero for effects.
    pub inputs: usize,
    pub desc: Description,
    jobs: Option<Sender<Job>>,
    chunks: Receiver<Result<Chunk, String>>,
    child: Option<Child>,
    started: Instant,
    in_flight: u32,
    last_progress: Instant,
}

impl Proc {
    /// Starts the generator (with `--pipe` appended) and completes the
    /// handshake.
    pub fn spawn(name: &str, spec: &Spec) -> Result<Proc, String> {
        let (program, args) = spec.argv.split_first().ok_or("no command given")?;
        let program_path = if program.contains('/') { spec.cwd.join(program) } else { program.into() };
        let mut child = Command::new(&program_path)
            .args(args)
            .arg("--pipe")
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own group: Ctrl-C on the daemon's terminal doesn't hit it, and
            // kill_group takes down anything it spawned.
            .process_group(0)
            .spawn()
            .map_err(|e| format!("{program}: {e}"))?;

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let tag = name.to_owned();
        thread::spawn(move || relay_log(&tag, stderr));

        let (ready_tx, ready_rx) = mpsc::channel();
        let (job_tx, job_rx) = mpsc::channel();
        let (chunk_tx, chunk_rx) = mpsc::channel();
        let hello = Hello { sample_rate: spec.sample_rate, max_frames: spec.max_frames };
        let worker_spawned = thread::Builder::new()
            .name(format!("gen-{name}"))
            .spawn(move || worker(stdin, stdout, hello, ready_tx, job_rx, chunk_tx));

        let handshake = match worker_spawned {
            Err(e) => Err(e.to_string()),
            Ok(_) => match ready_rx.recv_timeout(HANDSHAKE_TIMEOUT) {
                Ok(Ok(ch)) => Ok(ch),
                Ok(Err(e)) => Err(format!("{program}: handshake failed: {e} (does it support --pipe?)")),
                Err(_) => Err(format!("{program}: no handshake within {}s", HANDSHAKE_TIMEOUT.as_secs())),
            },
        };
        let ready = match handshake {
            Ok(r) => r,
            Err(e) => {
                sys::kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };

        let now = Instant::now();
        Ok(Proc {
            name: name.to_owned(),
            channels: ready.channels,
            inputs: ready.inputs,
            desc: ready.desc,
            jobs: Some(job_tx),
            chunks: chunk_rx,
            child: Some(child),
            started: now,
            in_flight: 0,
            last_progress: now,
        })
    }

    /// Closes stdin and reaps the process off-thread: it gets `grace` to
    /// exit, then its whole group is killed. Join the handle to wait for it.
    fn reap(&mut self, grace: Duration) -> Option<JoinHandle<()>> {
        self.jobs = None;
        let mut child = self.child.take()?;
        let name = self.name.clone();
        Some(thread::spawn(move || {
            let pgid = child.id();
            let until = Instant::now() + grace;
            let status = loop {
                match child.try_wait() {
                    Ok(Some(s)) => break Some(s),
                    Ok(None) if Instant::now() < until => thread::sleep(Duration::from_millis(10)),
                    _ => break None,
                }
            };
            // Also clears out anything the generator left running.
            sys::kill_group(pgid);
            let status = status.or_else(|| child.wait().ok());
            if let Some(s) = status.filter(|s| !s.success()) {
                wlog!("[{name}] exited ({s})");
            }
        }))
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.reap(EXIT_GRACE);
    }
}

pub enum State {
    Running(Proc),
    /// A fresh proc is being spawned off-thread.
    Starting,
    /// Waiting to restart.
    Backoff(Instant),
}

/// The mixer's lasting entry for a generator.
pub struct Slot {
    pub name: String,
    pub spec: Spec,
    pub gain_db: f64,
    pub muted: bool,
    /// Blocks mixed as silence because the chunk was late or skipped.
    pub late: u64,
    pub restarts: u64,
    pub last_error: Option<(String, Instant)>,
    /// Generators: effects in this generator's insert chain, by name, in
    /// order.
    pub inserts: Vec<String>,
    /// Generators: post-fader sends to send effects, linear 0..1, by effect
    /// name.
    pub sends: BTreeMap<String, f64>,
    /// Effects: the generator whose insert chain this effect is in; None
    /// for a send effect.
    pub owner: Option<String>,
    /// Insert effects: pass input straight through.
    pub bypass: bool,
    /// Parameter values set by the user, by id; kept across restarts.
    /// Unset params use the generator's default.
    pub values: BTreeMap<String, f64>,
    /// Output level after gain, falling back slowly; for meters.
    pub peak: f32,
    pub state: State,
    /// Bumped on every (re)start, so a stale spawn result is ignored.
    pub generation: u64,
    backoff: u32,
    nan_blocks: u32,
    on_time: bool,
    asked: bool,
}

/// What a background spawn reports back to the mixer.
pub struct Spawned {
    pub name: String,
    pub generation: u64,
    pub result: Result<Proc, String>,
}

impl Slot {
    pub fn new(name: &str, spec: Spec, proc: Proc) -> Slot {
        Slot {
            name: name.to_owned(),
            spec,
            gain_db: 0.0,
            muted: false,
            late: 0,
            restarts: 0,
            last_error: None,
            inserts: Vec::new(),
            sends: BTreeMap::new(),
            owner: None,
            bypass: false,
            values: BTreeMap::new(),
            peak: 0.0,
            state: State::Running(proc),
            generation: 0,
            backoff: 0,
            nan_blocks: 0,
            on_time: true,
            asked: false,
        }
    }

    pub fn command(&self) -> String {
        self.spec.argv.join(" ")
    }

    /// The running proc's parameters, if it's running.
    pub fn params(&self) -> Option<&[ParamDesc]> {
        match &self.state {
            State::Running(p) => Some(&p.desc.params),
            _ => None,
        }
    }

    pub fn description(&self) -> Option<&str> {
        match &self.state {
            State::Running(p) => Some(&p.desc.description),
            _ => None,
        }
    }

    pub fn inputs(&self) -> Option<usize> {
        match &self.state {
            State::Running(p) => Some(p.inputs),
            _ => None,
        }
    }

    /// The value sent for a param: the user's, clamped, or the default.
    pub fn value_of(&self, d: &ParamDesc) -> f64 {
        self.values.get(&d.id).map_or(d.default, |&v| d.clamp(v))
    }

    pub fn channels(&self) -> Option<usize> {
        match &self.state {
            State::Running(p) => Some(p.channels),
            _ => None,
        }
    }

    pub fn state_label(&self) -> String {
        match &self.state {
            State::Running(_) if self.nan_blocks > 0 => "nan".into(),
            State::Running(_) if self.muted => "muted".into(),
            State::Running(_) if !self.on_time => "late".into(),
            State::Running(_) => "ok".into(),
            State::Starting => "starting".into(),
            State::Backoff(at) => format!("retry {:.1}s", at.saturating_duration_since(Instant::now()).as_secs_f64()),
        }
    }

    pub fn gain(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            10f64.powf(self.gain_db / 20.0) as f32
        }
    }

    /// Sends a block request. `input` is the effect's input audio with
    /// `input_channels` channels (adapted to what the effect takes); empty
    /// for generators.
    pub fn request(&mut self, seq: u64, req: Request, input: &[f32], input_channels: usize) {
        self.asked = false;
        self.drain();
        let State::Running(p) = &mut self.state else { return };
        if p.in_flight >= MAX_IN_FLIGHT {
            self.late += 1;
            return self.check_hang();
        }
        let params = p.desc.params.iter().map(|d| self.values.get(&d.id).map_or(d.default, |&v| d.clamp(v))).collect();
        let input = if p.inputs == 0 || input.is_empty() { Vec::new() } else { adapt_channels(input, input_channels, p.inputs) };
        let sent = p.jobs.as_ref().is_some_and(|j| j.send(Job { seq, req, params, input }).is_ok());
        if !sent {
            return self.fail("exited".into(), false);
        }
        p.in_flight += 1;
        self.asked = true;
    }

    /// The chunk for `seq`. Waits until `deadline` while the proc keeps up;
    /// once it's behind, only until `short_deadline`, so one stuck generator
    /// can't drain the output buffer.
    pub fn collect(&mut self, seq: u64, deadline: Instant, short_deadline: Instant) -> Option<Vec<f32>> {
        if !self.asked {
            return None;
        }
        let until = if self.on_time { deadline } else { short_deadline.min(deadline) };
        loop {
            let State::Running(p) = &mut self.state else { return None };
            match p.chunks.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(Ok(c)) => {
                    p.in_flight = p.in_flight.saturating_sub(1);
                    p.last_progress = Instant::now();
                    if c.seq == seq {
                        self.on_time = true;
                        return Some(c.samples);
                    }
                    // an earlier, late chunk: drop it
                }
                Ok(Err(why)) => {
                    let kill_now = why.starts_with("garbled");
                    self.fail(why, kill_now);
                    return None;
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.on_time = false;
                    self.late += 1;
                    self.check_hang();
                    return None;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.fail("exited".into(), false);
                    return None;
                }
            }
        }
    }

    /// Takes chunks that arrived after their block was mixed, so a slow
    /// generator's in-flight count and progress stay current even while
    /// it's being skipped.
    fn drain(&mut self) {
        loop {
            let State::Running(p) = &mut self.state else { return };
            match p.chunks.try_recv() {
                Ok(Ok(_)) => {
                    p.in_flight = p.in_flight.saturating_sub(1);
                    p.last_progress = Instant::now();
                }
                Ok(Err(why)) => {
                    let kill_now = why.starts_with("garbled");
                    return self.fail(why, kill_now);
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => return self.fail("exited".into(), false),
            }
        }
    }

    /// Replaces NaN/inf with silence and clamps; restarts a proc that keeps
    /// producing them.
    pub fn sanitize(&mut self, samples: &mut [f32]) {
        let mut bad = false;
        for s in samples.iter_mut() {
            if s.is_finite() {
                *s = s.clamp(-1.0, 1.0);
            } else {
                *s = 0.0;
                bad = true;
            }
        }
        if !bad {
            self.nan_blocks = 0;
            return;
        }
        self.nan_blocks += 1;
        if self.nan_blocks >= NAN_LIMIT {
            self.fail("NaN/inf output".into(), true);
        }
    }

    fn check_hang(&mut self) {
        if let State::Running(p) = &self.state {
            if p.in_flight > 0 && p.last_progress.elapsed() > HANG_TIMEOUT {
                self.fail(format!("hung (no output for {}s)", HANG_TIMEOUT.as_secs()), true);
            }
        }
    }

    /// Drops the current proc and schedules a restart.
    pub fn fail(&mut self, why: String, kill_now: bool) {
        if let State::Running(mut p) = std::mem::replace(&mut self.state, State::Starting) {
            if p.started.elapsed() > STABLE_AFTER {
                self.backoff = 0;
            }
            p.reap(if kill_now { Duration::ZERO } else { EXIT_GRACE });
        }
        let delay = (BACKOFF_MIN * 2u32.pow(self.backoff.min(6))).min(BACKOFF_MAX);
        self.backoff += 1;
        wlog!("[{}] {why}; restarting in {:.2}s", self.name, delay.as_secs_f64());
        self.last_error = Some((why, Instant::now()));
        self.nan_blocks = 0;
        self.on_time = true;
        self.state = State::Backoff(Instant::now() + delay);
    }

    /// Restart now, e.g. after rebuilding the generator.
    pub fn restart_now(&mut self) {
        if let State::Running(mut p) = std::mem::replace(&mut self.state, State::Backoff(Instant::now())) {
            p.reap(EXIT_GRACE);
        }
        self.state = State::Backoff(Instant::now());
        self.backoff = 0;
    }

    /// Kicks off a background spawn if the backoff is over.
    pub fn maybe_restart(&mut self, done: &Sender<Spawned>) {
        let State::Backoff(at) = self.state else { return };
        if Instant::now() < at {
            return;
        }
        self.state = State::Starting;
        self.generation += 1;
        let (name, spec, generation, done) = (self.name.clone(), self.spec.clone(), self.generation, done.clone());
        thread::spawn(move || {
            let result = Proc::spawn(&name, &spec);
            let _ = done.send(Spawned { name, generation, result });
        });
    }

    /// Takes a background spawn's result, if it's for this slot's current
    /// attempt.
    pub fn spawned(&mut self, s: Spawned) {
        if s.generation != self.generation || !matches!(self.state, State::Starting) {
            return; // stale: removed, restarted again, … (the proc drops)
        }
        match s.result {
            Ok(p) => {
                wlog!("[{}] restarted", self.name);
                self.restarts += 1;
                self.on_time = true;
                self.state = State::Running(p);
            }
            Err(e) => self.fail(e, false),
        }
    }

    /// Stops the proc for good; join the handle to wait until it's gone.
    pub fn shutdown(&mut self) -> Option<JoinHandle<()>> {
        match std::mem::replace(&mut self.state, State::Backoff(Instant::now() + BACKOFF_MAX)) {
            State::Running(mut p) => p.reap(EXIT_GRACE),
            _ => None,
        }
    }
}

/// Copies the generator's stderr to ours, prefixed with its name. Never
/// stops reading before EOF (a full pipe would block the generator): bytes
/// that aren't UTF-8 are replaced, long lines are cut, and floods are
/// thinned to a few lines per second.
fn relay_log(tag: &str, mut stderr: ChildStderr) {
    let mut buf = [0u8; 4096];
    let mut line: Vec<u8> = Vec::new();
    let mut window = Instant::now();
    let (mut shown, mut dropped) = (0u32, 0u64);
    let emit = |line: &[u8], shown: &mut u32, dropped: &mut u64, window: &mut Instant| {
        if window.elapsed() >= Duration::from_secs(1) {
            if *dropped > 0 {
                wlog!("[{tag}] … {dropped} lines dropped");
            }
            *window = Instant::now();
            *shown = 0;
            *dropped = 0;
        }
        if *shown >= LOG_LINES_PER_SEC {
            *dropped += 1;
            return;
        }
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end();
        if text.is_empty() {
            return;
        }
        *shown += 1;
        wlog!("[{tag}] {text}");
    };
    loop {
        let n = match stderr.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\n' {
                emit(&line, &mut shown, &mut dropped, &mut window);
                line.clear();
            } else if line.len() < LOG_LINE_MAX {
                line.push(b);
            }
        }
    }
    if !line.is_empty() {
        emit(&line, &mut shown, &mut dropped, &mut window);
    }
}

fn worker(
    mut stdin: ChildStdin,
    mut stdout: ChildStdout,
    hello: Hello,
    ready: Sender<Result<Ready, String>>,
    jobs: Receiver<Job>,
    chunks: Sender<Result<Chunk, String>>,
) {
    let handshake = (|| -> Result<Ready, String> {
        stdin.write_all(&hello.encode()).and_then(|_| stdin.flush()).map_err(|e| e.to_string())?;
        let (h, json) = read_gen_hello(&mut stdout).map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => "exited before answering".to_string(),
            _ => e.to_string(),
        })?;
        let desc: Description = serde_json::from_str(&json).map_err(|e| format!("bad description JSON: {e}"))?;
        if desc.params.len() != h.params as usize {
            return Err(format!("hello says {} params, description lists {}", h.params, desc.params.len()));
        }
        Ok(Ready { channels: h.channels as usize, inputs: h.inputs as usize, desc })
    })();
    let (channels, inputs) = match handshake {
        Ok(r) => {
            let dims = (r.channels, r.inputs);
            let _ = ready.send(Ok(r));
            dims
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    let mut bytes = Vec::new();
    let mut silence = Vec::new();
    for job in jobs {
        let result = (|| -> Result<Chunk, String> {
            let want = job.req.frames as usize * inputs;
            let input = if job.input.len() == want {
                &job.input[..]
            } else {
                silence.resize(want, 0.0);
                &silence[..]
            };
            write_request(&mut stdin, &job.req, &job.params, input).and_then(|_| stdin.flush()).map_err(|_| "exited".to_string())?;
            let mut header = [0u8; CHUNK_HEADER_LEN];
            stdout.read_exact(&mut header).map_err(|_| "exited".to_string())?;
            let frames = decode_chunk_header(&header).map_err(|_| "garbled output (something printed to stdout?)".to_string())?;
            if frames != job.req.frames {
                return Err(format!("garbled output (asked for {} frames, got {frames})", job.req.frames));
            }
            bytes.resize(frames as usize * channels * 4, 0);
            stdout.read_exact(&mut bytes).map_err(|_| "exited".to_string())?;
            let samples = bytes.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
            Ok(Chunk { seq: job.seq, samples })
        })();
        let failed = result.is_err();
        if chunks.send(result).is_err() || failed {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::adapt_channels;

    #[test]
    fn adapts_channel_counts() {
        assert_eq!(adapt_channels(&[1.0, 2.0], 1, 2), vec![1.0, 1.0, 2.0, 2.0]);
        assert_eq!(adapt_channels(&[1.0, 3.0, 2.0, 4.0], 2, 1), vec![2.0, 3.0]);
        assert_eq!(adapt_channels(&[1.0, 2.0], 2, 2), vec![1.0, 2.0]);
    }
}
