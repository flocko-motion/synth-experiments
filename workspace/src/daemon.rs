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
use crate::gen::{adapt_channels, Slot, Spawned};
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

/// Duck envelope: how fast it reaches an event's strength (a short ramp
/// instead of a jump, so pads don't click) and how long it takes to recover,
/// in beats so it breathes with the tempo.
const DUCK_ATTACK_SECS: f64 = 0.002;
const DUCK_RELEASE_BEATS: f64 = 0.6;

/// Scales each frame of interleaved `buf` by `1 - sensitivity * env[frame]`.
fn apply_duck(buf: &mut [f32], channels: usize, env: &[f32], sensitivity: f32) {
    if sensitivity <= 0.0 {
        return;
    }
    for (frame, e) in buf.chunks_exact_mut(channels).zip(env) {
        let k = 1.0 - sensitivity * e;
        for x in frame {
            *x *= k;
        }
    }
}

/// What every stage of a block shares.
struct Stage {
    seq: u64,
    req: Request,
    channels: usize,
    deadline: Instant,
    block_secs: f64,
}

/// Runs each generator's buffer through its insert chain. Chains advance together: every
/// chain's first effect renders in parallel, then every second effect, and
/// so on, so effects add no latency and stay sample-aligned with the grid.
/// An effect that is bypassed, restarting, late or broken passes its input
/// through unchanged, so a channel never goes silent because of an effect.
fn run_chains(fx: &mut [Slot], chains: &[Vec<String>], bufs: &mut [Vec<f32>], st: &Stage) {
    let depth = chains.iter().map(Vec::len).max().unwrap_or(0);
    for d in 0..depth {
        let mut asked = Vec::new();
        for (c, names) in chains.iter().enumerate() {
            let Some(name) = names.get(d) else { continue };
            let Some(fi) = fx.iter().position(|s| &s.name == name) else { continue };
            fx[fi].peak *= PEAK_FALLOFF;
            if fx[fi].bypass {
                continue;
            }
            fx[fi].request(st.seq, st.req, &bufs[c], st.channels);
            asked.push((c, fi));
        }
        let short = Instant::now() + Duration::from_secs_f64(st.block_secs / 3.0);
        for (c, fi) in asked {
            let slot = &mut fx[fi];
            let Some(mut chunk) = slot.collect(st.seq, st.deadline, short) else { continue };
            slot.sanitize(&mut chunk);
            let Some(och) = slot.channels() else { continue };
            let out = adapt_channels(&chunk, och, st.channels);
            slot.peak = out.iter().fold(slot.peak, |p, s| p.max(s.abs()));
            bufs[c] = out;
        }
    }
}

fn add_scaled(into: &mut [f32], from: &[f32], gain: f32) {
    for (o, x) in into.iter_mut().zip(from) {
        *o += x * gain;
    }
}

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
    /// Every effect slot. An effect is either an insert in one generator's
    /// chain (`owner` is set) or a send effect: its own bus, fed by every
    /// generator's send to it, returned to the mix through its own fader.
    pub fx: Vec<Slot>,
    /// Background restarts report here.
    spawned_tx: Sender<Spawned>,
    click: Click,
    pub click_on: bool,
    pub click_db: f64,
    pub master_db: f64,
    pub master_peak: f32,
    /// The shared duck envelope, 0..1, and its attack target (0 when
    /// releasing).
    duck_env: f32,
    duck_target: f32,
    /// (frame, envelope) at each block's start, so the API can report the
    /// envelope at the audible position rather than the render position.
    duck_history: std::collections::VecDeque<(u64, f32)>,
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

    /// The duck envelope as heard now (the block that is playing).
    fn audible_duck(&self) -> f32 {
        let played = self.played.load(Ordering::Relaxed);
        self.duck_history.iter().rev().find(|(f, _)| *f <= played).map_or(0.0, |&(_, e)| e)
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
            duck: self.audible_duck(),
            ahead_ms: self.ahead_frames as f64 * 1000.0 / self.sample_rate as f64,
            underruns: self.underruns.load(Ordering::Relaxed),
            sample_rate: self.sample_rate,
            generators: self.gens.iter().map(api::Generator::of).collect(),
            effects: self.fx.iter().map(api::Effect::of).collect(),
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
            let routing = self.chain_text(&g.inserts)
                + &g.sends.iter().map(|(fx, a)| format!("  send {fx} {a:.2}")).collect::<String>();
            if !routing.is_empty() {
                s.push_str(&format!("{:w$} {routing}\n", ""));
            }
        }
        for f in self.fx.iter().filter(|f| f.owner.is_none()) {
            s.push_str(&format!(
                "send fx {}  return {:+.1} dB  {}{}\n",
                f.name,
                f.gain_db,
                f.state_label(),
                f.last_error.as_ref().map_or(String::new(), |(why, at)| format!("  (last error {} ago: {why})", fmt_ago(at.elapsed())))
            ));
        }
        s
    }

    /// `  -> duck (ok) -> echo (bypass)` for a chain, or empty.
    fn chain_text(&self, names: &[String]) -> String {
        names
            .iter()
            .map(|n| match self.fx.iter().find(|f| &f.name == n) {
                Some(f) if f.bypass => format!("  -> {n} (bypass)"),
                Some(f) => format!("  -> {n} ({})", f.state_label()),
                None => format!("  -> {n} (missing)"),
            })
            .collect()
    }

    /// The duck envelope for the next `n` frames, from this block's duck
    /// events (frame, strength). An event raises the envelope to its
    /// strength (never lowers it); then it recovers exponentially.
    fn duck_envelope(&mut self, n: usize, events: &mut [(u32, f32)]) -> Vec<f32> {
        events.sort_by_key(|e| e.0);
        let sr = self.sample_rate as f64;
        let attack_step = (1.0 / (DUCK_ATTACK_SECS * sr)) as f32;
        // Time constant a quarter of the release: about 2% left at the end.
        let tau = DUCK_RELEASE_BEATS * self.samples_per_beat() / 4.0;
        let coef = (-1.0 / tau).exp() as f32;
        let mut env = Vec::with_capacity(n);
        let mut k = 0;
        for i in 0..n {
            while k < events.len() && events[k].0 as usize <= i {
                let s = events[k].1;
                if s > self.duck_env && s > self.duck_target {
                    self.duck_target = s;
                }
                k += 1;
            }
            if self.duck_target > 0.0 {
                self.duck_env += attack_step;
                if self.duck_env >= self.duck_target {
                    self.duck_env = self.duck_target;
                    self.duck_target = 0.0;
                }
            } else {
                self.duck_env *= coef;
            }
            env.push(self.duck_env);
        }
        env
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
            g.request(seq, req, &[], 0);
        }

        // Wait for chunks at most until half of what's buffered would have
        // played; past that we'd rather drop one process's block than the
        // whole output. Every stage of the block shares this deadline.
        let buffered = Duration::from_secs_f64(self.ring.len() as f64 / ch as f64 / self.sample_rate as f64);
        let now = Instant::now();
        let block_secs = n as f64 / self.sample_rate as f64;
        let st = Stage { seq, req, channels: ch, deadline: now + (buffered / 2).max(Duration::from_millis(2)), block_secs };
        // For processes already behind: a few ms, well under a block's
        // duration, so the mixer always outpaces the device.
        let short_deadline = now + Duration::from_secs_f64(block_secs / 3.0);

        // 1. generators, each into its own buffer at the output's width
        let silence = vec![0.0f32; n * ch];
        let mut chans: Vec<Vec<f32>> = Vec::with_capacity(self.gens.len());
        for g in &mut self.gens {
            g.peak *= PEAK_FALLOFF;
            let buf = match (g.collect(seq, st.deadline, short_deadline), g.channels()) {
                (Some(mut chunk), Some(gch)) => {
                    g.sanitize(&mut chunk);
                    adapt_channels(&chunk, gch, ch)
                }
                _ => silence.clone(),
            };
            chans.push(buf);
        }

        // 2. generator insert chains
        let chains: Vec<Vec<String>> = self.gens.iter().map(|g| g.inserts.clone()).collect();
        run_chains(&mut self.fx, &chains, &mut chans, &st);

        // 3. the duck envelope, from every generator's and insert's events
        let mut events: Vec<(u32, f32)> = self.gens.iter().chain(self.fx.iter().filter(|f| f.owner.is_some())).flat_map(|s| s.ducks.iter().copied()).collect();
        let env = self.duck_envelope(n, &mut events);
        let ducking = env.iter().any(|&e| e > 1e-4);
        // The block's peak, so a meter sees the dip even between polls.
        let top = env.iter().copied().fold(0.0, f32::max);
        self.duck_history.push_back((self.frame, top));
        while self.duck_history.len() > 1024 {
            self.duck_history.pop_front();
        }

        // 4. faders into the mix, ducked post-fader by each channel's
        //    sensitivity (so its sends are ducked too); sends (linear
        //    amounts) into each send effect's input
        self.mix.clear();
        self.mix.resize(n * ch, 0.0);
        let mut send_in: Vec<Option<Vec<f32>>> = self.fx.iter().map(|f| f.owner.is_none().then(|| silence.clone())).collect();
        for (g, buf) in self.gens.iter_mut().zip(chans.iter_mut()) {
            let gain = g.gain();
            if gain == 0.0 {
                continue;
            }
            if ducking {
                apply_duck(buf, ch, &env, g.duck_sensitivity().unwrap_or(0.0) as f32);
            }
            g.peak = buf.iter().fold(g.peak, |p, s| p.max(s.abs() * gain));
            add_scaled(&mut self.mix, buf, gain);
            for (fx, amount) in &g.sends {
                if let Some(Some(input)) = self.fx.iter().position(|f| &f.name == fx).map(|i| send_in[i].as_mut()) {
                    add_scaled(input, buf, gain * *amount as f32);
                }
            }
        }

        // 5. send effects, all in parallel; each returns through its own
        //    fader, ducked by its own sensitivity. A send effect that's restarting or late returns nothing
        //    (passing its input through would double the dry signal).
        for (f, input) in self.fx.iter_mut().zip(&send_in) {
            if let Some(input) = input {
                f.peak *= PEAK_FALLOFF;
                f.request(seq, req, input, ch);
            }
        }
        for (f, input) in self.fx.iter_mut().zip(&send_in) {
            if input.is_none() {
                continue;
            }
            let (Some(mut chunk), Some(fch)) = (f.collect(seq, st.deadline, short_deadline), f.channels()) else { continue };
            f.sanitize(&mut chunk);
            let gain = f.gain();
            if gain == 0.0 {
                continue;
            }
            let mut out = adapt_channels(&chunk, fch, ch);
            if ducking {
                apply_duck(&mut out, ch, &env, f.duck_sensitivity().unwrap_or(0.0) as f32);
            }
            f.peak = out.iter().fold(f.peak, |p, s| p.max(s.abs() * gain));
            add_scaled(&mut self.mix, &out, gain);
        }

        // 6. click (not through the master chain), master gain, soft clip
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
        fx: Vec::new(),
        spawned_tx,
        click: Click::new(sample_rate as f64),
        click_on: opts.click,
        click_db: -12.0,
        master_db: 0.0,
        master_peak: 0.0,
        duck_env: 0.0,
        duck_target: 0.0,
        duck_history: std::collections::VecDeque::new(),
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
    let reapers: Vec<_> = m.gens.iter_mut().chain(m.fx.iter_mut()).filter_map(Slot::shutdown).collect();
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
            if let Some(g) = m.gens.iter_mut().chain(m.fx.iter_mut()).find(|g| g.name == s.name) {
                g.spawned(s);
            }
        }
        for g in m.gens.iter_mut().chain(m.fx.iter_mut()) {
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
