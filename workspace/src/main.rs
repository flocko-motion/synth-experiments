//! ws — the global workspace mixer.
//!
//! `ws daemon` runs the clock, click and mixer and plays to the default audio
//! device. Every other subcommand talks to the running daemon over a unix
//! socket. See `ws help`.

mod log;

mod api;
mod catalog;
mod click;
mod daemon;
mod gen;
mod http;
mod ring;
mod sys;
mod ui;

use gen_api::{Scale, Tonality};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

fn usage() -> String {
    format!(
        "\
usage: ws daemon [--bpm N] [--key ROOT] [--scale NAME] [--ahead MS] [--click]
                 [--http ADDR | --no-http] [--catalog DIR] [--ui DIR]
       ws COMMAND [ARGS...]

daemon options:
  --click         start with the metronome on (off by default; ws click on)
  --http ADDR     serve the HTTP API here (default {DEFAULT_HTTP}, loopback only)
  --catalog DIR   where to look for generator binaries (default target/release)
  --ui DIR        serve the web UI from this directory at / (default workspace/ui)

commands (sent to the running daemon):
  status                     transport, buffer and generators (default)
  pos                        audible position: epoch.cycle.bar.beat.tick
  add [--name NAME] COMMAND [ARGS..]
                             start a generator (COMMAND gets --pipe appended);
                             it's restarted whenever it crashes, hangs or
                             garbles its output. NAME defaults to the
                             program's name, with -2, -3, … if taken
  rm NAME                    stop and remove it
  restart NAME               restart it now, e.g. after rebuilding
  gain NAME|master|click DB  set a level, e.g. ws gain bass -3
  mute NAME / unmute NAME
  solo NAME / unsolo NAME    while any generator is soloed, only soloed ones
                             are heard (send-effect returns stay on)
  effect [--name NAME] [--insert GENERATOR] COMMAND [ARGS..]
                             add an effect: a send effect (its own bus) or,
                             with --insert, into a generator's chain
  send GENERATOR EFFECT AMOUNT   send level 0..1 to a send effect (0 removes)
  bypass EFFECT on|off       bypass an insert effect
  duck NAME SENSITIVITY      how much ducking turns a generator or effect
                             down, 0..1 (send effects: the whole return;
                             inserts: only what they add)
  params NAME                list a generator's or effect's parameters
  set NAME PARAM VALUE [--glide BARS]
                             set one, e.g. ws set acid cutoff 0.5; with
                             --glide it slides there over that many bars
  bpm N                      change tempo
  ahead MS                   render-ahead buffer: how far in the future
                             changes land (smaller = sooner, less margin)
  key ROOT [SCALE]           change key, e.g. ws key F# phrygian
  click on|off
  play / stop                start or stop the transport (stop lets voices ring)
  seek BAR                   jump to a bar, from 1
  url                        the HTTP API's address
  quit                       stop the daemon and its generators

grid: tick (16th) -> beat -> bar -> cycle (4 bars) -> epoch (16 bars)
scales: {}
socket: $WS_SOCKET, default {}",
        api::scale_names(),
        default_socket().display()
    )
}

const DEFAULT_HTTP: &str = "127.0.0.1:7700";

fn default_socket() -> PathBuf {
    std::env::temp_dir().join("synth-ws.sock")
}

fn socket() -> PathBuf {
    std::env::var_os("WS_SOCKET").map(PathBuf::from).unwrap_or_else(default_socket)
}

fn fail(msg: &str) -> ! {
    eprintln!("ws: {msg}");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("daemon") => run_daemon(&args[1..]),
        Some("help" | "-h" | "--help") => println!("{}", usage()),
        Some(_) => send(&args),
        None => send(&["status".to_string()]),
    }
}

fn run_daemon(args: &[String]) {
    let mut opts = daemon::Opts {
        socket: socket(),
        bpm: 148.0,
        tonality: Tonality { root: 2, scale: Scale::Phrygian },
        ahead_ms: 200.0,
        click: false,
        http: Some(DEFAULT_HTTP.parse().unwrap()),
        catalog: PathBuf::from("target/release"),
        ui: PathBuf::from("workspace/ui"),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().unwrap_or_else(|| fail(&format!("{a} needs a value")));
        match a.as_str() {
            "--bpm" => opts.bpm = val().parse().ok().filter(|b| (20.0..=300.0).contains(b)).unwrap_or_else(|| fail("--bpm takes 20-300")),
            "--key" => opts.tonality.root = Tonality::parse_root(&val()).unwrap_or_else(|| fail("--key takes a note name like F# or Bb")),
            "--scale" => opts.tonality.scale = Scale::parse(&val()).unwrap_or_else(|| fail(&format!("--scale takes one of {}", api::scale_names()))),
            "--ahead" => opts.ahead_ms = val().parse().ok().filter(|ms| (20.0..=5000.0).contains(ms)).unwrap_or_else(|| fail("--ahead takes 20-5000 ms")),
            "--click" => opts.click = true,
            "--no-click" => opts.click = false, // the default; kept for old scripts
            "--http" => opts.http = Some(val().parse().unwrap_or_else(|_| fail("--http takes an address like 127.0.0.1:7700"))),
            "--no-http" => opts.http = None,
            "--catalog" => opts.catalog = PathBuf::from(val()),
            "--ui" => opts.ui = PathBuf::from(val()),
            _ => fail(&format!("{a}: unknown option\n{}", usage())),
        }
    }
    if let Err(e) = daemon::run(opts) {
        fail(&e);
    }
}

fn send(args: &[String]) {
    let path = socket();
    let mut conn = UnixStream::connect(&path)
        .unwrap_or_else(|_| fail(&format!("no workspace running at {}; start one with: ws daemon", path.display())));
    let cwd = std::env::current_dir().unwrap_or_else(|e| fail(&format!("current dir: {e}")));
    let mut req = cwd.to_string_lossy().into_owned();
    for a in args {
        req.push('\0');
        req.push_str(a);
    }
    let mut reply = String::new();
    let io = conn
        .write_all(req.as_bytes())
        .and_then(|_| conn.shutdown(Shutdown::Write))
        .and_then(|_| conn.read_to_string(&mut reply).map(|_| ()));
    if let Err(e) = io {
        fail(&format!("talking to the workspace: {e}"));
    }
    match reply.split_once('\n') {
        Some(("ok", text)) => print!("{text}"),
        Some(("error", text)) => fail(text.trim_end()),
        _ => fail("garbled reply from the workspace"),
    }
}
