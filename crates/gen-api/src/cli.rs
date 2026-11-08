//! Shared command line for every plugin binary. The CLI is a minimal host:
//! it plays the role the workspace plays — fixed tempo, fixed key, a
//! transport starting at bar 1 — and writes the result as WAV.
//!
//! ```text
//! <plugin> [bpm] [bars] [--seed N] [--key ROOT] [--scale NAME] [--set ID=VALUE]... [--out FILE]
//! <plugin> --demo [--seed N] [--key ROOT] [--scale NAME] [--set ID=VALUE]...
//! <plugin> --info [--json]
//! <plugin> [--seed N] --pipe        (run under the workspace, see `pipe`)
//! ```
//!
//! - no bars: endless
//! - audio goes to stdout unless `--out FILE`; status goes to stderr
//! - refuses to write audio to a terminal, except in demo mode
//! - key defaults to a root picked from the seed, scale to phrygian
//! - `--set` sets a parameter; `--help` lists them with their ranges
//! - `--demo`: endless at 148 bpm; on a terminal it plays through sox's
//!   `play` itself, otherwise it writes to stdout like any other run
//!
//! Effects (plugins with inputs) read WAV from stdin and stop when it ends,
//! or after `bars`. Give them the same bpm as their source, which is assumed
//! to start at bar 1:
//!
//! ```text
//! acid 145 | duck 145 | play -q --ignore-length -t wav -
//! acid | duck --demo
//! ```

use crate::wav::{self, WavReader};
use crate::{describe_json, pipe, Config, Context, Generator, Param, ParamKind, Rng, Scale, Tonality, Transport};
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
    sets: Vec<String>,
    out: Option<String>,
    info: bool,
    json: bool,
    demo: bool,
    help: bool,
}

fn exe() -> String {
    std::env::args()
        .next()
        .and_then(|p| p.rsplit('/').next().map(str::to_owned))
        .unwrap_or_else(|| "plugin".into())
}

fn play_hint() -> String {
    format!("{} | {} -", exe(), PLAYER.join(" "))
}

fn usage(params: &[Param], effect: bool) -> ! {
    let scales: Vec<_> = Scale::ALL.iter().map(|s| s.name()).collect();
    let e = exe();
    eprintln!("usage: {e} [bpm] [bars] [--seed N] [--key ROOT] [--scale NAME] [--set ID=VALUE]... [--out FILE]");
    eprintln!("       {e} --demo [--seed N] [--key ROOT] [--scale NAME] [--set ID=VALUE]...");
    eprintln!("       {e} --info [--json]");
    eprintln!("       {e} [--seed N] --pipe      (run under the workspace)");
    eprintln!("  bpm defaults to {DEFAULT_BPM}; no bars = endless");
    eprintln!("  --demo plays endlessly at {DEFAULT_BPM} bpm through sox");
    eprintln!("  audio goes to stdout unless --out is given");
    eprintln!("  scales: {}", scales.join(", "));
    if effect {
        eprintln!("  effect: reads WAV on stdin, e.g.  acid | {e} --demo");
    } else {
        eprintln!("  listen:  {}", play_hint());
    }
    if !params.is_empty() {
        eprintln!("  params (--set ID=VALUE):");
        for p in params {
            eprintln!("    {:<12} {}", p.id, describe_param(p));
        }
    }
    std::process::exit(2);
}

fn describe_param(p: &Param) -> String {
    let unit = if p.unit.is_empty() { String::new() } else { format!(" {}", p.unit) };
    match p.kind {
        ParamKind::Toggle => format!("{}: on/off (default {})", p.name, if p.default >= 0.5 { "on" } else { "off" }),
        ParamKind::Choice(labels) => {
            format!("{}: {} (default {})", p.name, labels.join(" | "), labels.get(p.default as usize).unwrap_or(&"?"))
        }
        _ => format!("{}: {}..{}{unit} (default {})", p.name, p.min, p.max, p.default),
    }
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
        sets: Vec::new(),
        out: None,
        info: false,
        json: false,
        demo: false,
        help: false,
    };
    let mut pos = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| fail(&format!("{arg} needs a value")));
        match arg.as_str() {
            "--seed" => a.seed = Some(val().parse().unwrap_or_else(|_| fail("--seed takes a number"))),
            "--key" => a.root = Some(Tonality::parse_root(&val()).unwrap_or_else(|| fail("--key takes a note name like F# or Bb"))),
            "--scale" => {
                let v = val();
                a.scale = Scale::parse(&v).unwrap_or_else(|| fail(&format!("unknown scale {v}")))
            }
            "--set" => a.sets.push(val()),
            "--out" | "-o" => a.out = Some(val()),
            "--info" => a.info = true,
            "--json" => a.json = true,
            "--demo" => a.demo = true,
            "-h" | "--help" => a.help = true,
            _ => pos.push(arg),
        }
    }
    if a.demo && (!pos.is_empty() || a.out.is_some()) {
        fail(&format!("--demo takes no bpm, bars or --out; it plays endlessly at {DEFAULT_BPM} bpm"));
    }
    if pos.len() > 2 {
        fail("too many arguments; see --help");
    }
    if let Some(s) = pos.first() {
        a.bpm = s.parse().unwrap_or_else(|_| fail(&format!("{s}: bpm must be a number")));
    }
    if let Some(s) = pos.get(1) {
        a.bars = Some(s.parse().unwrap_or_else(|_| fail(&format!("{s}: bars must be a whole number"))));
    }
    if !(60.0..=250.0).contains(&a.bpm) {
        fail("bpm must be 60-250");
    }
    if a.bars == Some(0) {
        fail("bars must be >= 1");
    }
    a
}

/// Defaults, overridden by `--set ID=VALUE`.
fn param_values(params: &[Param], sets: &[String]) -> Vec<f64> {
    let mut values: Vec<f64> = params.iter().map(|p| p.default).collect();
    for s in sets {
        let (id, v) = s.split_once('=').unwrap_or_else(|| fail(&format!("--set {s}: expected ID=VALUE")));
        let Some(i) = params.iter().position(|p| p.id == id) else {
            let ids: Vec<_> = params.iter().map(|p| p.id).collect();
            if ids.is_empty() {
                fail(&format!("--set {id}: this plugin has no parameters"));
            }
            fail(&format!("--set {id}: unknown parameter; known: {}", ids.join(", ")));
        };
        values[i] = params[i].parse(v).unwrap_or_else(|| fail(&format!("--set {s}: {}", describe_param(&params[i]))));
    }
    values
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
            other => fail(&format!("{other}: not used in --pipe mode (the workspace sets tempo, key and params)")),
        }
    }
    match pipe::serve::<G>(seed.unwrap_or_else(clock_seed)) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
        Err(e) => fail(&format!("pipe: {e}")),
    }
}

/// Run a plugin with the standard command line. Never returns on error.
pub fn run<G: Generator>() {
    if std::env::args().any(|a| a == "--pipe") {
        return run_pipe::<G>();
    }
    let args = parse_args();
    let seed = args.seed.unwrap_or_else(clock_seed);
    let mut gen = G::new(&Config { sample_rate: SAMPLE_RATE as f64, seed });
    let info = gen.info();
    let params = gen.params();
    let inputs = gen.inputs() as usize;
    let effect = inputs > 0;

    if args.help {
        usage(params, effect);
    }
    if args.info {
        if args.json {
            println!("{}", describe_json(&gen));
        } else {
            println!("{}\n{}\nchannels: {}", info.name, info.description, info.channels);
        }
        return;
    }
    let values = param_values(params, &args.sets);

    // Effects read their input first: it decides the sample rate and may
    // bound the length.
    let mut reader = None;
    let mut sample_rate = SAMPLE_RATE;
    if effect {
        if io::stdin().is_terminal() {
            fail(&format!("{} is an effect and reads WAV on stdin, e.g.\n  acid | {} --demo", info.name, exe()));
        }
        let r = WavReader::new(io::stdin().lock()).unwrap_or_else(|e| fail(&e.to_string()));
        if r.sample_rate != SAMPLE_RATE {
            sample_rate = r.sample_rate;
            gen = G::new(&Config { sample_rate: sample_rate as f64, seed });
        }
        reader = Some(r);
    }

    let to_file = args.out.as_deref().filter(|o| *o != "-");
    if to_file.is_some() && args.bars.is_none() && reader.as_ref().and_then(|r| r.frames()).is_none() {
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
    // apart from the plugin's own random stream.
    let root = args.root.unwrap_or_else(|| (Rng::new(seed ^ 0x6B65_79).next_u64() % 12) as u8);
    let sr = sample_rate as f64;
    let mut transport = Transport { bpm: args.bpm, beat: 0.0, beats_per_bar: BEATS_PER_BAR, playing: true };
    let tonality = Tonality { root, scale: args.scale };
    let spb = transport.samples_per_beat(sr);

    let bar_frames = args.bars.map(|b| (b as f64 * BEATS_PER_BAR as f64 * spb).round() as u64);
    let total_frames = match (bar_frames, reader.as_ref().and_then(|r| r.frames())) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };

    let length = match total_frames {
        Some(n) => format!("{:.1}s", n as f64 / sr),
        None if effect => "until input ends".into(),
        None => "endless".into(),
    };
    let set: Vec<String> = params.iter().zip(&values).filter(|(p, v)| **v != p.default).map(|(p, v)| format!("{}={v}", p.id)).collect();
    eprintln!(
        "{}  seed {seed}  key {} {}  {} bpm  {length}{}",
        info.name,
        tonality.root_name(),
        tonality.scale.name(),
        args.bpm,
        if set.is_empty() { String::new() } else { format!("  {}", set.join(" ")) }
    );

    let sink: Box<dyn Write> = match (to_file, player.as_mut()) {
        (Some(path), _) => Box::new(File::create(path).unwrap_or_else(|e| fail(&format!("{path}: {e}")))),
        (None, Some(child)) => Box::new(child.stdin.take().expect("player stdin is piped")),
        (None, None) => Box::new(io::stdout().lock()),
    };
    let mut w = BufWriter::new(sink);
    let ch = info.channels as usize;

    let result = (|| -> io::Result<()> {
        wav::write_header(&mut w, sample_rate, info.channels, total_frames)?;
        let mut buf = vec![0.0f32; BLOCK_FRAMES * ch];
        let mut input = vec![0.0f32; BLOCK_FRAMES * inputs];
        let mut scratch = Vec::new();
        let mut frame: u64 = 0;
        loop {
            let mut n = match total_frames {
                Some(t) => (t - frame).min(BLOCK_FRAMES as u64) as usize,
                None => BLOCK_FRAMES,
            };
            if let Some(r) = reader.as_mut() {
                n = r.read(n, inputs, &mut input)?;
            }
            if n == 0 {
                break;
            }
            // Position from the frame count, not accumulated: no drift.
            transport.beat = frame as f64 / spb;
            let ctx = Context { sample_rate: sr, transport, tonality, params: &values, input: &input[..n * inputs] };
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
        Err(e) => fail(&format!("{e}")),
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
