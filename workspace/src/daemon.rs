//! The workspace daemon.
//!
//! Three kinds of thread:
//!
//! - **audio** (cpal callback): copies mixed samples from the [`Ring`] to the
//!   device. It never blocks, allocates or waits on a generator.
//! - **mixer** (main thread): owns the clock and all state. It keeps the ring
//!   `ahead` ms full. For each block it sends one request to every generator,
//!   collects the chunks, adds the click, and pushes the mix into the ring.
//! - **control**: accepts `ws` commands on a unix socket and runs them on the
//!   mixer's state as closures, between blocks.
//!
//! Generators are expected to break; see [`crate::gen`] for how they are
//! restarted.
//!
//! Because the mixer renders ahead, a tempo, key or transport change is heard
//! `ahead` ms after the command is given.

use crate::click::Click;
use crate::gen::{Proc, Slot, Spawned, Spec};
use crate::ring::Ring;
use crate::sys;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use gen_api::pipe::Request;
use gen_api::{Position, Scale, Tonality, Transport};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Frames per block sent to generators.
pub const BLOCK: usize = 512;

pub struct Opts {
    pub socket: PathBuf,
    pub bpm: f64,
    pub tonality: Tonality,
    pub ahead_ms: f64,
    pub click: bool,
}

/// A command from the control thread, run on the mixer's state.
type Job = Box<dyn FnOnce(&mut Mixer) -> Result<String, String> + Send>;

pub struct Mixer {
    pub sample_rate: u32,
    channels: usize,
    ring: Arc<Ring>,
    /// Frames the device has taken from the ring, i.e. roughly what is
    /// audible now.
    played: Arc<AtomicU64>,
    underruns: Arc<AtomicU64>,
    ahead_frames: usize,

    /// Next frame to render.
    frame: u64,
    seq: u64,
    // The clock: beat = anchor_beat + (frame - anchor_frame) / samples per
    // beat. Re-anchored on every tempo or transport change, so position never
    // accumulates rounding error.
    anchor_frame: u64,
    anchor_beat: f64,
    pub bpm: f64,
    beats_per_bar: u32,
    pub playing: bool,
    pub tonality: Tonality,

    pub gens: Vec<Slot>,
    /// Background restarts report here.
    spawned_tx: Sender<Spawned>,
    click: Click,
    pub click_on: bool,
    pub click_db: f64,
    pub master_db: f64,
    quit: bool,
    mix: Vec<f32>,
}

fn db_to_gain(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

impl Mixer {
    fn samples_per_beat(&self) -> f64 {
        self.sample_rate as f64 * 60.0 / self.bpm
    }

    fn beat_at(&self, frame: u64) -> f64 {
        if !self.playing {
            return self.anchor_beat;
        }
        self.anchor_beat + frame.saturating_sub(self.anchor_frame) as f64 / self.samples_per_beat()
    }

    fn reanchor(&mut self) {
        self.anchor_beat = self.beat_at(self.frame);
        self.anchor_frame = self.frame;
    }

    pub fn set_bpm(&mut self, bpm: f64) {
        self.reanchor();
        self.bpm = bpm;
    }

    pub fn set_playing(&mut self, playing: bool) {
        self.reanchor();
        self.playing = playing;
        self.click.reset();
    }

    /// Jump to the start of a bar, counted from 0.
    pub fn seek_bar(&mut self, bar: u64) {
        self.anchor_frame = self.frame;
        self.anchor_beat = (bar * self.beats_per_bar as u64) as f64;
        self.click.reset();
    }

    pub fn audible_beat(&self) -> f64 {
        self.beat_at(self.played.load(Ordering::Relaxed))
    }

    pub fn audible_position(&self) -> Position {
        Position::at(self.audible_beat(), self.beats_per_bar)
    }

    pub fn find(&mut self, name: &str) -> Result<&mut Slot, String> {
        self.gens.iter_mut().find(|g| g.name == name).ok_or_else(|| format!("no generator named {name}"))
    }

    pub fn status(&self) -> String {
        let mut s = format!(
            "{} {}   {} bpm   {} {}   click {}   master {:+.1} dB   ahead {:.0} ms   underruns {}\n",
            if self.playing { "▶" } else { "■" },
            self.audible_position(),
            fmt_num(self.bpm),
            self.tonality.root_name(),
            self.tonality.scale.name(),
            if self.click_on { format!("{:+.1} dB", self.click_db) } else { "off".into() },
            self.master_db,
            self.ahead_frames as f64 * 1000.0 / self.sample_rate as f64,
            self.underruns.load(Ordering::Relaxed),
        );
        if self.gens.is_empty() {
            s.push_str("no generators; add one with: ws add NAME COMMAND [ARGS...]\n");
            return s;
        }
        let w = self.gens.iter().map(|g| g.name.len()).max().unwrap_or(4).max(4);
        s.push_str(&format!("{:w$}  CH  GAIN      STATE        LATE  RESTARTS  COMMAND\n", "NAME"));
        for g in &self.gens {
            s.push_str(&format!(
                "{:w$}  {:<2}  {:<8}  {:<11}  {:>4}  {:>8}  {}\n",
                g.name,
                g.channels().map_or("-".into(), |c| c.to_string()),
                format!("{:+.1} dB", g.gain_db),
                g.state_label(),
                g.late,
                g.restarts,
                g.command()
            ));
            if let Some((why, at)) = &g.last_error {
                s.push_str(&format!("{:w$}  └ {} ago: {why}\n", "", fmt_ago(at.elapsed())));
            }
        }
        s
    }

    fn render_block(&mut self) {
        let n = BLOCK;
        let ch = self.channels;
        let beat0 = self.beat_at(self.frame);
        let req = Request {
            frames: n as u32,
            frame: self.frame,
            transport: Transport { bpm: self.bpm, beat: beat0, beats_per_bar: self.beats_per_bar, playing: self.playing },
            tonality: self.tonality,
        };
        let seq = self.seq;
        for g in &mut self.gens {
            g.request(seq, req);
        }

        // Wait for chunks at most until half of what's buffered would have
        // played; past that we'd rather drop one generator's block than the
        // whole output.
        let buffered = Duration::from_secs_f64(self.ring.len() as f64 / ch as f64 / self.sample_rate as f64);
        let now = Instant::now();
        let deadline = now + (buffered / 2).max(Duration::from_millis(2));
        // For generators already behind: a few ms, well under a block's
        // duration, so the mixer always outpaces the device.
        let short_deadline = now + Duration::from_secs_f64(n as f64 / self.sample_rate as f64 / 3.0);

        self.mix.clear();
        self.mix.resize(n * ch, 0.0);
        for g in &mut self.gens {
            let Some(mut chunk) = g.collect(seq, deadline, short_deadline) else { continue };
            g.sanitize(&mut chunk);
            let gain = g.gain();
            let Some(gch) = g.channels() else { continue };
            if gain == 0.0 {
                continue;
            }
            for (f, out) in self.mix.chunks_exact_mut(ch).enumerate() {
                let src = &chunk[f * gch..(f + 1) * gch];
                for (c, o) in out.iter_mut().enumerate() {
                    // mono to every channel; otherwise channel by channel,
                    // repeating the last one on wider devices
                    *o += src[c.min(gch - 1)] * gain;
                }
            }
        }

        if self.click_on {
            let (playing, spb, gain) = (self.playing, self.samples_per_beat(), db_to_gain(self.click_db));
            self.click.render(&mut self.mix, ch, self.beats_per_bar, gain, |i| playing.then(|| beat0 + i as f64 / spb));
        }

        // Soft clip so stacked generators saturate instead of wrapping.
        let master = db_to_gain(self.master_db);
        for x in &mut self.mix {
            *x = (*x * master).tanh();
        }

        self.ring.push(&self.mix);
        self.frame += n as u64;
        self.seq += 1;
    }
}

fn fmt_ago(d: Duration) -> String {
    match d.as_secs() {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
    }
}

fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{x:.0}")
    } else {
        format!("{x}")
    }
}

pub fn run(opts: Opts) -> Result<(), String> {
    if UnixStream::connect(&opts.socket).is_ok() {
        return Err(format!("a workspace is already running on {}", opts.socket.display()));
    }
    let _ = std::fs::remove_file(&opts.socket);

    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no audio output device")?;
    let supported = device.default_output_config().map_err(|e| format!("output config: {e}"))?;
    if supported.sample_format() != cpal::SampleFormat::F32 {
        return Err(format!("output device wants {:?} samples; only f32 is supported", supported.sample_format()));
    }
    let config: cpal::StreamConfig = supported.into();
    let sample_rate = config.sample_rate.0;
    let channels = config.channels as usize;
    let ahead_frames = (opts.ahead_ms / 1000.0 * sample_rate as f64) as usize;

    // Room for the target fill plus one block, so a full block always fits.
    let ring = Ring::new((ahead_frames + 2 * BLOCK) * channels);
    let played = Arc::new(AtomicU64::new(0));
    let underruns = Arc::new(AtomicU64::new(0));

    let stream = {
        let (ring, played, underruns) = (ring.clone(), played.clone(), underruns.clone());
        device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _| {
                    let n = ring.pop(data);
                    if n < data.len() {
                        data[n..].fill(0.0);
                        // Before the first real samples, silence is expected.
                        if played.load(Ordering::Relaxed) > 0 {
                            underruns.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    played.fetch_add((n / channels) as u64, Ordering::Relaxed);
                },
                |e| eprintln!("audio: {e}"),
                None,
            )
            .map_err(|e| format!("open output stream: {e}"))?
    };

    sys::install_stop_handlers();
    let listener = UnixListener::bind(&opts.socket).map_err(|e| format!("{}: {e}", opts.socket.display()))?;
    let (job_tx, job_rx) = mpsc::channel::<(Job, Sender<Result<String, String>>)>();
    thread::Builder::new()
        .name("control".into())
        .spawn(move || control(listener, job_tx, sample_rate))
        .map_err(|e| e.to_string())?;

    let (spawned_tx, spawned_rx) = mpsc::channel();
    let mut m = Mixer {
        sample_rate,
        channels,
        ring,
        played,
        underruns,
        ahead_frames,
        frame: 0,
        seq: 0,
        anchor_frame: 0,
        anchor_beat: 0.0,
        bpm: opts.bpm,
        beats_per_bar: 4,
        playing: true,
        tonality: opts.tonality,
        gens: Vec::new(),
        spawned_tx,
        click: Click::new(sample_rate as f64),
        click_on: opts.click,
        click_db: -12.0,
        master_db: 0.0,
        quit: false,
        mix: Vec::new(),
    };

    eprintln!(
        "workspace  {} Hz  {} ch  {} bpm  {} {}  ahead {} ms",
        sample_rate,
        channels,
        fmt_num(opts.bpm),
        opts.tonality.root_name(),
        opts.tonality.scale.name(),
        opts.ahead_ms
    );
    eprintln!("listening on {}", opts.socket.display());

    // Prime the buffer before the device starts pulling.
    fill(&mut m);
    stream.play().map_err(|e| format!("start output: {e}"))?;

    mix_loop(&mut m, job_rx, spawned_rx);

    drop(stream);
    let _ = std::fs::remove_file(&opts.socket);
    // Close every generator's stdin at once, then wait until all are gone.
    let reapers: Vec<_> = m.gens.iter_mut().filter_map(Slot::shutdown).collect();
    for r in reapers {
        let _ = r.join();
    }
    eprintln!("workspace stopped");
    Ok(())
}

fn fill(m: &mut Mixer) {
    let ch = m.channels;
    while m.ring.len() < m.ahead_frames * ch && m.ring.free() >= BLOCK * ch {
        m.render_block();
    }
}

fn mix_loop(m: &mut Mixer, jobs: Receiver<(Job, Sender<Result<String, String>>)>, spawned: Receiver<Spawned>) {
    loop {
        if sys::stop_requested() {
            eprintln!("shutting down");
            return;
        }
        while let Ok(s) = spawned.try_recv() {
            if let Some(g) = m.gens.iter_mut().find(|g| g.name == s.name) {
                g.spawned(s);
            }
        }
        for g in &mut m.gens {
            g.maybe_restart(&m.spawned_tx);
        }
        fill(m);
        // Sleep until a command arrives or about a block's worth of audio has
        // played.
        match jobs.recv_timeout(Duration::from_millis(2)) {
            Ok((job, reply)) => {
                let _ = reply.send(job(m));
                if m.quit {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

// ---------------------------------------------------------------- control

/// Request: NUL-separated arguments `cwd, command, args...`, then EOF.
/// Reply: `ok\n` or `error\n`, then text for the user.
fn control(listener: UnixListener, jobs: Sender<(Job, Sender<Result<String, String>>)>, sample_rate: u32) {
    for conn in listener.incoming() {
        let Ok(mut conn) = conn else { continue };
        let mut raw = Vec::new();
        if conn.read_to_end(&mut raw).is_err() {
            continue;
        }
        let args: Vec<String> = String::from_utf8_lossy(&raw).split('\0').map(str::to_owned).collect();
        let result = match args.split_first() {
            Some((cwd, rest)) if !rest.is_empty() => handle(Path::new(cwd), rest, &jobs, sample_rate),
            _ => Err("empty request".into()),
        };
        let reply = match result {
            Ok(text) => format!("ok\n{text}"),
            Err(text) => format!("error\n{text}"),
        };
        let _ = conn.write_all(reply.as_bytes());
    }
}

/// Runs `f` on the mixer and waits for its answer.
fn on_mixer(
    jobs: &Sender<(Job, Sender<Result<String, String>>)>,
    f: impl FnOnce(&mut Mixer) -> Result<String, String> + Send + 'static,
) -> Result<String, String> {
    let (tx, rx) = mpsc::channel();
    jobs.send((Box::new(f), tx)).map_err(|_| "mixer stopped".to_string())?;
    rx.recv().map_err(|_| "mixer stopped".to_string())?
}

fn parse_db(s: &str) -> Result<f64, String> {
    let db: f64 = s.trim_end_matches("dB").trim_end_matches("db").parse().map_err(|_| format!("{s}: not a dB value"))?;
    if !(-96.0..=24.0).contains(&db) {
        return Err("gain must be between -96 and +24 dB".into());
    }
    Ok(db)
}

fn handle(
    cwd: &Path,
    args: &[String],
    jobs: &Sender<(Job, Sender<Result<String, String>>)>,
    sample_rate: u32,
) -> Result<String, String> {
    let cmd = args[0].as_str();
    let rest = &args[1..];
    let arg = |i: usize, what: &str| rest.get(i).cloned().ok_or_else(|| format!("{cmd}: missing {what}"));
    match cmd {
        "status" => on_mixer(jobs, |m| Ok(m.status())),
        "pos" => on_mixer(jobs, |m| {
            let beat = m.audible_beat();
            let pos = m.audible_position();
            Ok(format!("{pos}  bar {}  beat {beat:.3}  {} bpm\n", pos.absolute_bar() + 1, fmt_num(m.bpm)))
        }),
        "add" => {
            let name = arg(0, "NAME")?;
            if name == "master" || name == "click" || name.is_empty() || name.contains(char::is_whitespace) {
                return Err(format!("{name:?} can't be used as a generator name"));
            }
            if rest.len() < 2 {
                return Err("usage: ws add NAME COMMAND [ARGS...]".into());
            }
            let n = name.clone();
            on_mixer(jobs, move |m| match m.gens.iter().any(|g| g.name == n) {
                true => Err(format!("{n} already exists; ws rm {n} first")),
                false => Ok(String::new()),
            })?;
            // Spawn and handshake here, not on the mixer, so a slow start
            // doesn't hold up audio. A first start that fails is reported
            // and not added; later crashes are restarted.
            let spec = Spec { argv: rest[1..].to_vec(), cwd: cwd.to_owned(), sample_rate, max_frames: BLOCK as u32 };
            let proc = Proc::spawn(&name, &spec)?;
            let msg = format!("added {name} ({} ch)\n", proc.channels);
            let slot = Slot::new(&name, spec, proc);
            on_mixer(jobs, move |m| {
                if m.gens.iter().any(|g| g.name == slot.name) {
                    return Err(format!("{} already exists", slot.name));
                }
                m.gens.push(slot);
                Ok(msg)
            })
        }
        "restart" => {
            let name = arg(0, "NAME")?;
            on_mixer(jobs, move |m| {
                m.find(&name)?.restart_now();
                Ok(format!("restarting {name}\n"))
            })
        }
        "rm" => {
            let name = arg(0, "NAME")?;
            on_mixer(jobs, move |m| {
                let before = m.gens.len();
                m.gens.retain(|g| g.name != name);
                if m.gens.len() == before {
                    return Err(format!("no generator named {name}"));
                }
                Ok(format!("removed {name}\n"))
            })
        }
        "gain" => {
            let name = arg(0, "NAME")?;
            let db = parse_db(&arg(1, "DB")?)?;
            on_mixer(jobs, move |m| {
                match name.as_str() {
                    "master" => m.master_db = db,
                    "click" => m.click_db = db,
                    _ => m.find(&name)?.gain_db = db,
                }
                Ok(format!("{name} {db:+.1} dB\n"))
            })
        }
        "mute" | "unmute" => {
            let name = arg(0, "NAME")?;
            let mute = cmd == "mute";
            on_mixer(jobs, move |m| {
                m.find(&name)?.muted = mute;
                Ok(format!("{name} {}\n", if mute { "muted" } else { "unmuted" }))
            })
        }
        "bpm" => {
            let bpm: f64 = arg(0, "BPM")?.parse().map_err(|_| "bpm: not a number".to_string())?;
            if !(20.0..=300.0).contains(&bpm) {
                return Err("bpm must be 20-300".into());
            }
            on_mixer(jobs, move |m| {
                m.set_bpm(bpm);
                Ok(format!("{} bpm\n", fmt_num(bpm)))
            })
        }
        "key" => {
            let root = Tonality::parse_root(&arg(0, "ROOT")?).ok_or("key: expected a note name like F# or Bb")?;
            let scale = match rest.get(1) {
                Some(s) => Some(Scale::parse(s).ok_or_else(|| format!("key: unknown scale {s}; {}", scale_names()))?),
                None => None,
            };
            on_mixer(jobs, move |m| {
                m.tonality = Tonality { root, scale: scale.unwrap_or(m.tonality.scale) };
                Ok(format!("{} {}\n", m.tonality.root_name(), m.tonality.scale.name()))
            })
        }
        "click" => {
            let on = match arg(0, "on|off")?.as_str() {
                "on" => true,
                "off" => false,
                other => return Err(format!("click: expected on or off, not {other}")),
            };
            on_mixer(jobs, move |m| {
                m.click_on = on;
                Ok(format!("click {}\n", if on { "on" } else { "off" }))
            })
        }
        "play" | "stop" => {
            let playing = cmd == "play";
            on_mixer(jobs, move |m| {
                m.set_playing(playing);
                Ok(format!("{} at {}\n", if playing { "playing" } else { "stopped" }, m.audible_position()))
            })
        }
        "seek" => {
            let bar: u64 = arg(0, "BAR")?.parse().ok().filter(|&b| b >= 1).ok_or("seek: BAR is a bar number from 1")?;
            on_mixer(jobs, move |m| {
                m.seek_bar(bar - 1);
                Ok(format!("seek to bar {bar}\n"))
            })
        }
        "quit" => on_mixer(jobs, |m| {
            m.quit = true;
            Ok("workspace stopping\n".into())
        }),
        other => Err(format!("unknown command {other}; see ws help")),
    }
}

pub fn scale_names() -> String {
    Scale::ALL.iter().map(|s| s.name()).collect::<Vec<_>>().join(", ")
}
