//! The workspace daemon.
//!
//! Three kinds of thread:
//!
//! - **audio** (cpal callback): copies mixed samples from the [`Ring`] to the
//!   device. It never blocks, allocates or waits on a generator.
//! - **mixer** (main thread): owns the clock and all state. It keeps the ring
//!   `ahead` ms full. For each block it sends one request to every generator,
//!   collects the chunks, adds the click, and pushes the mix into the ring.
//! - **control**: `ws` commands on a unix socket and the HTTP API (see
//!   [`crate::api`], [`crate::http`]). Both run operations on the mixer's
//!   state as closures, between blocks.
//!
//! Generators are expected to break; see [`crate::gen`] for how they are
//! restarted.
//!
//! Because the mixer renders ahead, a tempo, key or transport change is heard
//! `ahead` ms after the command is given.

use crate::api::{self, ApiError, Ctl, Job};
use crate::catalog::Catalog;
use crate::click::Click;
use crate::gen::{Slot, Spawned};
use crate::log::wlog;
use crate::ring::Ring;
use crate::sys;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use gen_api::pipe::Request;
use gen_api::{Position, Tonality, Transport};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::net::SocketAddr;
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
    /// Where to serve the HTTP API; None turns it off.
    pub http: Option<SocketAddr>,
    /// Where the catalog looks for generator binaries.
    pub catalog: PathBuf,
    /// Where the web UI's files are served from.
    pub ui: PathBuf,
}

/// Per-block falloff of the level meters (about -20 dB in 0.4 s).
const PEAK_FALLOFF: f32 = 0.93;

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
    pub master_peak: f32,
    pub quit: bool,
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

    pub fn state(&self) -> api::State {
        let beat = self.audible_beat();
        let p = Position::at(beat, self.beats_per_bar);
        api::State {
            playing: self.playing,
            position: api::PositionView {
                epoch: p.epoch,
                cycle: p.cycle,
                bar: p.bar,
                beat: p.beat,
                tick: p.tick,
                absolute_bar: p.absolute_bar(),
                text: p.to_string(),
            },
            beat,
            bpm: self.bpm,
            root: self.tonality.root_name().to_owned(),
            scale: self.tonality.scale.name().to_owned(),
            click: self.click_on,
            click_db: self.click_db,
            master_db: self.master_db,
            master_peak: self.master_peak,
            ahead_ms: self.ahead_frames as f64 * 1000.0 / self.sample_rate as f64,
            underruns: self.underruns.load(Ordering::Relaxed),
            sample_rate: self.sample_rate,
            generators: self.gens.iter().map(api::Generator::of).collect(),
        }
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
            g.peak *= PEAK_FALLOFF;
            let Some(mut chunk) = g.collect(seq, deadline, short_deadline) else { continue };
            g.sanitize(&mut chunk);
            let gain = g.gain();
            let Some(gch) = g.channels() else { continue };
            if gain == 0.0 {
                continue;
            }
            g.peak = chunk.iter().fold(g.peak, |p, s| p.max(s.abs() * gain));
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
        let mut peak = self.master_peak * PEAK_FALLOFF;
        for x in &mut self.mix {
            *x = (*x * master).tanh();
            peak = peak.max(x.abs());
        }
        self.master_peak = peak;

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
                |e| wlog!("audio: {e}"),
                None,
            )
            .map_err(|e| format!("open output stream: {e}"))?
    };

    sys::install_stop_handlers();
    let listener = UnixListener::bind(&opts.socket).map_err(|e| format!("{}: {e}", opts.socket.display()))?;
    let http = match opts.http.map(crate::http::bind) {
        Some(Ok(l)) => Some(l),
        Some(Err(e)) => {
            crate::http::log_bind_failure(&e);
            None
        }
        None => None,
    };
    let http_url = http.as_ref().and_then(|l| l.local_addr().ok()).map(|a| format!("http://{a}"));
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let catalog_dir = cwd.join(&opts.catalog);
    let ui_dir = cwd.join(&opts.ui);
    let ctl = Ctl { jobs: job_tx, sample_rate, http_url: http_url.clone(), catalog: Arc::new(Catalog::new(catalog_dir)) };
    {
        let ctl = ctl.clone();
        thread::Builder::new().name("control".into()).spawn(move || control(listener, ctl)).map_err(|e| e.to_string())?;
    }
    if let Some(l) = http {
        let ctl = ctl.clone();
        let (cwd, ui) = (cwd.clone(), ui_dir.clone());
        thread::Builder::new().name("http".into()).spawn(move || crate::http::serve(l, ctl, cwd, ui)).map_err(|e| e.to_string())?;
    }

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
        master_peak: 0.0,
        quit: false,
        mix: Vec::new(),
    };

    wlog!(
        "workspace  {} Hz  {} ch  {} bpm  {} {}  ahead {} ms",
        sample_rate,
        channels,
        fmt_num(opts.bpm),
        opts.tonality.root_name(),
        opts.tonality.scale.name(),
        opts.ahead_ms
    );
    wlog!("listening on {}", opts.socket.display());
    match &http_url {
        Some(u) => {
            wlog!("http api on {u}  (spec: {u}/api/openapi.yaml, catalog: {})", ctl.catalog.dir().display());
            let missing = if ui_dir.is_dir() { "" } else { "  (missing)" };
            wlog!("web ui on {u}/  from {}{missing}", ui_dir.display());
        }
        None => {}
    }

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
    wlog!("workspace stopped");
    Ok(())
}

fn fill(m: &mut Mixer) {
    let ch = m.channels;
    while m.ring.len() < m.ahead_frames * ch && m.ring.free() >= BLOCK * ch {
        m.render_block();
    }
}

fn mix_loop(m: &mut Mixer, jobs: Receiver<Job>, spawned: Receiver<Spawned>) {
    loop {
        if sys::stop_requested() {
            wlog!("shutting down");
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
            Ok(job) => {
                job(m);
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

/// `ws` CLI requests over the unix socket.
/// Request: NUL-separated arguments `cwd, command, args...`, then EOF.
/// Reply: `ok\n` or `error\n`, then text for the terminal.
fn control(listener: UnixListener, ctl: Ctl) {
    for conn in listener.incoming() {
        let Ok(mut conn) = conn else { continue };
        let ctl = ctl.clone();
        // A thread per request: `add` can take seconds to start a generator.
        thread::spawn(move || {
            let mut raw = Vec::new();
            if conn.read_to_end(&mut raw).is_err() {
                return;
            }
            let args: Vec<String> = String::from_utf8_lossy(&raw).split('\0').map(str::to_owned).collect();
            let result = match args.split_first() {
                Some((cwd, rest)) if !rest.is_empty() => ctl.command(Path::new(cwd), rest),
                _ => Err(ApiError::BadRequest("empty request".into())),
            };
            let reply = match result {
                Ok(text) => format!("ok\n{text}"),
                Err(e) => format!("error\n{e}"),
            };
            let _ = conn.write_all(reply.as_bytes());
        });
    }
}
