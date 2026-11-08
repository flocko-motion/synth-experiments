//! Shared command line for every generator binary. The CLI is a minimal host:
//! it plays the role the mixer will play later — fixed tempo, fixed key, a
//! transport starting at bar 1 — and writes the result as WAV.
//!
//! ```text
//! <gen> [bpm] [bars] [--seed N] [--key ROOT] [--scale NAME] [--out FILE] [--info]
//! <gen> --demo [--seed N] [--key ROOT] [--scale NAME]
//! ```
//!
//! - no bars: endless
//! - audio goes to stdout unless `--out FILE`; status goes to stderr
//! - refuses to write audio to a terminal, except in demo mode
//! - key defaults to a root picked from the seed, scale to phrygian
//! - `--demo`: endless at 148 bpm; on a terminal it plays through sox's
//!   `play` itself, otherwise it writes to stdout like any other run

use crate::{pipe, wav, Config, Context, Generator, Rng, Scale, Tonality, Transport};
use std::fs::File;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::process::{Child, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const SAMPLE_RATE: u32 = 48_000;
const BLOCK_FRAMES: usize = 512;
const DEFAULT_BPM: f64 = 148.0;
const BEATS_PER_BAR: u32 = 4;
const PLAYER: [&str; 5] = ["play", "-q", "--ignore-length", "-t", "wav"];

struct Args {
    bpm: f64,
    bars: Option<u64>,
    seed: Option<u64>,
    root: Option<u8>,
    scale: Scale,
    out: Option<String>,
    info: bool,
    demo: bool,
}

fn exe() -> String {
    std::env::args()
        .next()
        .and_then(|p| p.rsplit('/').next().map(str::to_owned))
        .unwrap_or_else(|| "generator".into())
}

fn play_hint() -> String {
    format!("{} | {} -", exe(), PLAYER.join(" "))
}

fn usage() -> ! {
    let scales: Vec<_> = Scale::ALL.iter().map(|s| s.name()).collect();
    eprintln!("usage: {} [bpm] [bars] [--seed N] [--key ROOT] [--scale NAME] [--out FILE] [--info]", exe());
    eprintln!("       {} [--seed N] --pipe      (run under the workspace)", exe());
    eprintln!("       {} --demo [--seed N] [--key ROOT] [--scale NAME]", exe());
    eprintln!("  bpm defaults to {DEFAULT_BPM}; no bars = endless");
    eprintln!("  --demo plays endlessly at {DEFAULT_BPM} bpm through sox");
    eprintln!("  audio goes to stdout unless --out is given");
    eprintln!("  scales: {}", scales.join(", "));
    eprintln!("  listen:  {}", play_hint());
    std::process::exit(2);
}

fn fail(msg: &str) -> ! {
    eprintln!("{}: {msg}", exe());
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut a = Args {
        bpm: DEFAULT_BPM,
        bars: None,
        seed: None,
        root: None,
        scale: Scale::Phrygian,
        out: None,
        info: false,
        demo: false,
    };
    let mut pos = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "--seed" => a.seed = Some(val().parse().unwrap_or_else(|_| fail("--seed takes a number"))),
            "--key" => a.root = Some(Tonality::parse_root(&val()).unwrap_or_else(|| fail("--key takes a note name like F# or Bb"))),
            "--scale" => a.scale = Scale::parse(&val()).unwrap_or_else(|| usage()),
            "--out" | "-o" => a.out = Some(val()),
            "--info" => a.info = true,
            "--demo" => a.demo = true,
            "-h" | "--help" => usage(),
            _ => pos.push(arg),
        }
    }
    if a.demo && (!pos.is_empty() || a.out.is_some()) {
        fail(&format!("--demo takes no bpm, bars or --out; it plays endlessly at {DEFAULT_BPM} bpm"));
    }
    if pos.len() > 2 {
        usage();
    }
    if let Some(s) = pos.first() {
        a.bpm = s.parse().unwrap_or_else(|_| usage());
    }
    if let Some(s) = pos.get(1) {
        a.bars = Some(s.parse().unwrap_or_else(|_| usage()));
    }
    if !(60.0..=250.0).contains(&a.bpm) {
        fail("bpm must be 60-250");
    }
    if a.bars == Some(0) {
        fail("bars must be >= 1");
    }
    a
}

fn clock_seed() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64 % 1_000_000
}

/// Pipe mode takes only `--seed`; the host owns everything else.
fn run_pipe<G: Generator>() {
    let mut seed = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--seed" => seed = Some(it.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| fail("--seed takes a number"))),
            "--pipe" => {}
            other => fail(&format!("{other}: not used in --pipe mode (the workspace sets tempo and key)")),
        }
    }
    match pipe::serve::<G>(seed.unwrap_or_else(clock_seed)) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
        Err(e) => fail(&format!("pipe: {e}")),
    }
}

/// Run a generator with the standard command line. Never returns on error.
pub fn run<G: Generator>() {
    if std::env::args().any(|a| a == "--pipe") {
        return run_pipe::<G>();
    }
    let args = parse_args();
    let seed = args.seed.unwrap_or_else(clock_seed);
    let sample_rate = SAMPLE_RATE as f64;
    let mut gen = G::new(&Config { sample_rate, seed });
    let info = gen.info();

    if args.info {
        println!("{}\n{}\nchannels: {}", info.name, info.description, info.channels);
        return;
    }

    let to_file = args.out.as_deref().filter(|o| *o != "-");
    if to_file.is_some() && args.bars.is_none() {
        fail("endless output only goes to stdout; give a bar count to write a file");
    }
    let mut player: Option<Child> = None;
    if to_file.is_none() && io::stdout().is_terminal() {
        if args.demo {
            player = Some(spawn_player());
        } else {
            fail(&format!(
                "stdout is a terminal, not writing audio there.\n  try:     {} --demo\n  listen:  {}\n  or:      {} {} 32 --out take.wav",
                exe(),
                play_hint(),
                exe(),
                args.bpm
            ));
        }
    }

    // The host picks the key unless told; derived from the seed but kept
    // apart from the generator's own random stream.
    let root = args.root.unwrap_or_else(|| (Rng::new(seed ^ 0x6B65_79).next_u64() % 12) as u8);
    let mut ctx = Context {
        sample_rate,
        transport: Transport { bpm: args.bpm, beat: 0.0, beats_per_bar: BEATS_PER_BAR, playing: true },
        tonality: Tonality { root, scale: args.scale },
    };

    let spb = ctx.transport.samples_per_beat(sample_rate);
    let total_frames = args.bars.map(|b| (b as f64 * BEATS_PER_BAR as f64 * spb).round() as u64);

    let length = match (args.bars, total_frames) {
        (Some(b), Some(n)) => format!("{b} bars ({:.1}s)", n as f64 / sample_rate),
        _ => "endless".into(),
    };
    eprintln!(
        "{}  seed {seed}  key {} {}  {} bpm  {length}",
        info.name,
        ctx.tonality.root_name(),
        ctx.tonality.scale.name(),
        args.bpm
    );

    let sink: Box<dyn Write> = match (to_file, player.as_mut()) {
        (Some(path), _) => Box::new(File::create(path).unwrap_or_else(|e| fail(&format!("{path}: {e}")))),
        (None, Some(child)) => Box::new(child.stdin.take().expect("player stdin is piped")),
        (None, None) => Box::new(io::stdout().lock()),
    };
    let mut w = BufWriter::new(sink);
    let ch = info.channels as usize;

    let result = (|| -> io::Result<()> {
        wav::write_header(&mut w, SAMPLE_RATE, info.channels, total_frames)?;
        let mut buf = vec![0.0f32; BLOCK_FRAMES * ch];
        let mut scratch = Vec::new();
        let mut frame: u64 = 0;
        loop {
            let n = match total_frames {
                Some(t) => (t - frame).min(BLOCK_FRAMES as u64) as usize,
                None => BLOCK_FRAMES,
            };
            if n == 0 {
                break;
            }
            // Position from the frame count, not accumulated: no drift.
            ctx.transport.beat = frame as f64 / spb;
            let out = &mut buf[..n * ch];
            gen.process(&ctx, out);
            wav::write_samples(&mut w, out, &mut scratch)?;
            frame += n as u64;
        }
        w.flush()
    })();

    // Close the player's input so it can finish, then wait for it.
    drop(w);
    if let Some(mut child) = player {
        let _ = child.wait();
    }

    match result {
        Ok(()) => {
            if let Some(path) = to_file {
                eprintln!("-> {path}");
            }
        }
        // The player went away (closed, Ctrl-C'd): a normal way to stop.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
        Err(e) => fail(&format!("write failed: {e}")),
    }
}

/// Starts sox's `play` reading WAV from its stdin.
fn spawn_player() -> Child {
    Command::new(PLAYER[0])
        .args(&PLAYER[1..])
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| {
            fail(&format!(
                "--demo plays through sox's `play`, which failed to start ({e}).\n  install: brew install sox\n  or pipe: {} --demo | <player>",
                exe()
            ))
        })
}
