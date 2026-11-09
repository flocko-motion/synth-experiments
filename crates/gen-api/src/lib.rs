//! The interface every generator implements, plus a shared CLI host.
//!
//! A generator is driven by a host (the CLI runner today, a mixer later). The
//! host owns time and harmony: it calls [`Generator::process`] once per block
//! and passes a [`Context`] with the tempo, the musical position of the first
//! sample in the block, and the current tonality. The generator owns only its
//! sound and its musical decisions.
//!
//! # Contract
//!
//! - Blocks are normally contiguous: each block's position follows on from the
//!   previous one. If it doesn't (the host seeked or looped), the generator
//!   must resync to the new position without glitching or panicking.
//! - Musical decisions follow the host's position, not an internal clock:
//!   "the last bar of every 16" means host bars 16, 32, ….
//! - Output is the same for the same seed and the same sequence of contexts.
//! - `process` overwrites `out`, which is interleaved with `info().channels`
//!   channels, and keeps samples within [-1, 1].
//! - Tempo and tonality may change between blocks; follow them from the next
//!   musically sensible point on.
//! - Parameter values (`ctx.params`) change in steps once per block; smooth
//!   continuous ones yourself.
//!
//! # Effects
//!
//! An effect is a generator that declares [`Generator::inputs`] > 0 and reads
//! `ctx.input`. The host adapts channel counts, so the input always has
//! exactly that many channels. The context describes the position of the
//! input audio; a host may run effects a block behind their source, so don't
//! assume output is sample-aligned with a live transport.
//!
//! # Making a generator
//!
//! Implement [`Generator`] and make the binary's main a one-liner:
//!
//! ```ignore
//! fn main() { gen_api::cli::run::<MyGenerator>() }
//! ```
//!
//! The binary then runs standalone (renders WAV, see [`cli`]) and, with
//! `--pipe`, as a workspace subprocess (see [`pipe`]).

pub mod cli;
pub mod clock;
pub mod pipe;
mod rng;
mod wav;

pub use clock::{StepClock, Steps};
pub use rng::Rng;

/// What the host tells the generator at construction time.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub sample_rate: f64,
    /// All of the generator's random choices derive from this.
    pub seed: u64,
}

/// Static description, used for discovery (`--info`) and by the host to size
/// buffers.
#[derive(Clone, Copy, Debug)]
pub struct Info {
    pub name: &'static str,
    pub description: &'static str,
    /// Interleaved channel count of the `out` buffer.
    pub channels: u16,
}

/// The grid above the beat is fixed for every host and generator: each level
/// is four of the one below. tick (16th) → beat → bar → cycle → epoch.
pub const TICKS_PER_BEAT: u32 = 4;
pub const BARS_PER_CYCLE: u32 = 4;
pub const CYCLES_PER_EPOCH: u32 = 4;

/// Where the music is, and how fast it moves. Valid for the start of a block;
/// constant within it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transport {
    pub bpm: f64,
    /// Position of the block's first sample, in quarter notes since bar 1
    /// beat 1 (= 0.0).
    pub beat: f64,
    pub beats_per_bar: u32,
    /// When false, generators should trigger nothing new but let sounding
    /// voices ring out.
    pub playing: bool,
}

impl Transport {
    pub fn samples_per_beat(&self, sample_rate: f64) -> f64 {
        sample_rate * 60.0 / self.bpm
    }

    /// The grid position of [`Transport::beat`].
    pub fn position(&self) -> Position {
        Position::at(self.beat, self.beats_per_bar)
    }
}

/// A point on the grid, each field counted from 0 within its parent level:
/// `tick` is the 16th within the beat, `beat` the beat within the bar, and so
/// on. `epoch` counts up from 0 without limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub epoch: u64,
    pub cycle: u32,
    pub bar: u32,
    pub beat: u32,
    pub tick: u32,
}

impl Position {
    /// Position of a beat count (quarter notes since 0). Negative beats clamp
    /// to 0.
    pub fn at(beat: f64, beats_per_bar: u32) -> Position {
        let ticks = (beat.max(0.0) * TICKS_PER_BEAT as f64).floor() as u64;
        let tick = (ticks % TICKS_PER_BEAT as u64) as u32;
        let beats = ticks / TICKS_PER_BEAT as u64;
        let bpb = beats_per_bar.max(1) as u64;
        let beat = (beats % bpb) as u32;
        let bars = beats / bpb;
        let bar = (bars % BARS_PER_CYCLE as u64) as u32;
        let cycles = bars / BARS_PER_CYCLE as u64;
        let cycle = (cycles % CYCLES_PER_EPOCH as u64) as u32;
        let epoch = cycles / CYCLES_PER_EPOCH as u64;
        Position { epoch, cycle, bar, beat, tick }
    }

    /// Bars since 0, ignoring the cycle/epoch split.
    pub fn absolute_bar(&self) -> u64 {
        (self.epoch * CYCLES_PER_EPOCH as u64 + self.cycle as u64) * BARS_PER_CYCLE as u64 + self.bar as u64
    }
}

/// `epoch.cycle.bar.beat.tick`, 1-based like a DAW counter: `1.1.1.1.1` is 0.
impl std::fmt::Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}.{}.{}", self.epoch + 1, self.cycle + 1, self.bar + 1, self.beat + 1, self.tick + 1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scale {
    Phrygian,
    Minor,
    HarmonicMinor,
    Dorian,
    Major,
}

impl Scale {
    pub const ALL: [Scale; 5] = [Scale::Phrygian, Scale::Minor, Scale::HarmonicMinor, Scale::Dorian, Scale::Major];

    pub fn intervals(self) -> [i32; 7] {
        match self {
            Scale::Phrygian => [0, 1, 3, 5, 7, 8, 10],
            Scale::Minor => [0, 2, 3, 5, 7, 8, 10],
            Scale::HarmonicMinor => [0, 2, 3, 5, 7, 8, 11],
            Scale::Dorian => [0, 2, 3, 5, 7, 9, 10],
            Scale::Major => [0, 2, 4, 5, 7, 9, 11],
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Scale::Phrygian => "phrygian",
            Scale::Minor => "minor",
            Scale::HarmonicMinor => "harmonic-minor",
            Scale::Dorian => "dorian",
            Scale::Major => "major",
        }
    }

    pub fn parse(s: &str) -> Option<Scale> {
        Scale::ALL.into_iter().find(|sc| sc.name().eq_ignore_ascii_case(s))
    }

    /// Index into [`Scale::ALL`]; the pipe protocol sends this.
    pub fn index(self) -> usize {
        Scale::ALL.iter().position(|&s| s == self).unwrap()
    }
}

const NOTE_NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

/// Key of the music: a root pitch class and a scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tonality {
    /// Pitch class, 0 = C … 11 = B.
    pub root: u8,
    pub scale: Scale,
}

impl Tonality {
    pub fn root_name(&self) -> &'static str {
        NOTE_NAMES[self.root as usize % 12]
    }

    /// Parses "C", "F#", "Bb", … into a pitch class.
    pub fn parse_root(s: &str) -> Option<u8> {
        let mut chars = s.chars();
        let base = match chars.next()?.to_ascii_uppercase() {
            'C' => 0,
            'D' => 2,
            'E' => 4,
            'F' => 5,
            'G' => 7,
            'A' => 9,
            'B' => 11,
            _ => return None,
        };
        let acc = match chars.as_str() {
            "" => 0,
            "#" | "s" => 1,
            "b" => -1,
            _ => return None,
        };
        Some(((base + acc + 12) % 12) as u8)
    }

    /// Semitones above the root of a scale degree; degree 0 is the root,
    /// 7 the octave, -1 the seventh below.
    pub fn degree(&self, degree: i32) -> i32 {
        self.scale.intervals()[degree.rem_euclid(7) as usize] + 12 * degree.div_euclid(7)
    }
}

/// Everything the host tells the generator for one block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Context<'a> {
    pub sample_rate: f64,
    pub transport: Transport,
    pub tonality: Tonality,
    /// Current value of every parameter, in [`Generator::params`] order,
    /// already clamped to its range. Changes in steps once per block;
    /// smooth continuous parameters yourself.
    pub params: &'a [f64],
    /// Effects only: the input audio for this block, interleaved with
    /// [`Generator::inputs`] channels, the same frame count as `out`.
    /// Empty for generators.
    pub input: &'a [f32],
}

impl Context<'_> {
    /// Value of parameter `index` (its position in [`Generator::params`]).
    pub fn param(&self, index: usize) -> f64 {
        self.params[index]
    }
}

/// How a parameter's value is interpreted. Values are always f64 in plain
/// units; integer-like kinds hold whole numbers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamKind {
    Continuous,
    Integer,
    /// 0 = off, 1 = on.
    Toggle,
    /// An index into the labels, 0-based.
    Choice(&'static [&'static str]),
}

/// A parameter a plugin exposes to the host, modelled on CLAP's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Param {
    /// Stable identifier, unique within the plugin. Hosts address and store
    /// values by it, so don't rename it.
    pub id: &'static str,
    pub name: &'static str,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub kind: ParamKind,
    /// Hint for UIs: map knob travel logarithmically (frequencies, times).
    pub log: bool,
    pub unit: &'static str,
}

impl Param {
    pub const fn continuous(id: &'static str, name: &'static str, min: f64, max: f64, default: f64) -> Param {
        Param { id, name, min, max, default, kind: ParamKind::Continuous, log: false, unit: "" }
    }

    pub const fn integer(id: &'static str, name: &'static str, min: i64, max: i64, default: i64) -> Param {
        Param { kind: ParamKind::Integer, ..Param::continuous(id, name, min as f64, max as f64, default as f64) }
    }

    pub const fn toggle(id: &'static str, name: &'static str, default: bool) -> Param {
        Param { kind: ParamKind::Toggle, ..Param::continuous(id, name, 0.0, 1.0, if default { 1.0 } else { 0.0 }) }
    }

    pub const fn choice(id: &'static str, name: &'static str, labels: &'static [&'static str], default: usize) -> Param {
        let max = if labels.is_empty() { 0.0 } else { (labels.len() - 1) as f64 };
        Param { kind: ParamKind::Choice(labels), ..Param::continuous(id, name, 0.0, max, default as f64) }
    }

    pub const fn log(self) -> Param {
        Param { log: true, ..self }
    }

    pub const fn unit(self, unit: &'static str) -> Param {
        Param { unit, ..self }
    }

    /// Clamp a value into range and round it as the kind requires.
    pub fn clamp(&self, v: f64) -> f64 {
        let v = if v.is_finite() { v.clamp(self.min, self.max) } else { self.default };
        match self.kind {
            ParamKind::Continuous => v,
            ParamKind::Integer | ParamKind::Choice(_) => v.round(),
            ParamKind::Toggle => {
                if v >= 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }

    /// Parse a value as typed on the command line: a number, `on`/`off` for
    /// toggles, or a label for choices.
    pub fn parse(&self, s: &str) -> Option<f64> {
        match self.kind {
            ParamKind::Toggle if s == "on" || s == "true" => return Some(1.0),
            ParamKind::Toggle if s == "off" || s == "false" => return Some(0.0),
            ParamKind::Choice(labels) => {
                if let Some(i) = labels.iter().position(|l| l.eq_ignore_ascii_case(s)) {
                    return Some(i as f64);
                }
            }
            _ => {}
        }
        s.parse::<f64>().ok().map(|v| self.clamp(v))
    }

    fn kind_name(&self) -> &'static str {
        match self.kind {
            ParamKind::Continuous => "continuous",
            ParamKind::Integer => "integer",
            ParamKind::Toggle => "toggle",
            ParamKind::Choice(_) => "choice",
        }
    }
}

/// Something a plugin tells the host, at a frame within the block it
/// belongs to. Returned alongside the audio by [`Generator::take_events`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Event {
    /// Frame within the block, 0-based.
    pub frame: u32,
    pub kind: EventKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EventKind {
    /// "Duck the other channels now": typically a kick. The host ducks every
    /// channel by its [`Generator::duck_sensitivity`] × `strength` (0..1)
    /// and lets it recover along its duck envelope.
    Duck { strength: f32 },
}

/// How a generator wants to come in when it's added to a mix that's already
/// playing. The host applies it; the generator just plays as usual.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Entry {
    /// Audible straight away.
    Now,
    /// Silent until the next bar line, then in at full level: drums, bass.
    Bar,
    /// Fades in over `bars` bars of playing time: pads, atmospheres.
    Fade { bars: f64 },
}

/// A plugin: a generator, or an effect (a generator with audio inputs).
pub trait Generator {
    fn new(config: &Config) -> Self
    where
        Self: Sized;

    fn info(&self) -> Info;

    /// The parameters this plugin exposes; `ctx.params` follows this order.
    fn params(&self) -> &'static [Param] {
        &[]
    }

    /// Input channels. 0 = generator; anything else makes it an effect that
    /// gets its input in `ctx.input`.
    fn inputs(&self) -> u16 {
        0
    }

    /// How strongly the host ducks this plugin when a duck event arrives,
    /// 0 (never: the kick itself) .. 1 (fully: pads, reverb). Fixed per
    /// plugin.
    fn duck_sensitivity(&self) -> f64 {
        0.0
    }

    /// How the host brings this generator into a running mix. Effects
    /// don't need one.
    fn entry(&self) -> Entry {
        Entry::Now
    }

    /// Render one block. See the crate docs for the contract.
    fn process(&mut self, ctx: &Context, out: &mut [f32]);

    /// Move the events of the block just rendered into `out`. The host calls
    /// this after every `process`, so events must not pile up.
    fn take_events(&mut self, _out: &mut Vec<Event>) {}
}

/// Machine-readable description: what `--info --json` prints and the pipe
/// handshake carries.
pub fn describe_json(g: &dyn Generator) -> String {
    let info = g.info();
    let params: Vec<String> = g
        .params()
        .iter()
        .map(|p| {
            let labels = match p.kind {
                ParamKind::Choice(labels) => {
                    format!(",\"labels\":[{}]", labels.iter().map(|l| json_str(l)).collect::<Vec<_>>().join(","))
                }
                _ => String::new(),
            };
            format!(
                "{{\"id\":{},\"name\":{},\"min\":{},\"max\":{},\"default\":{},\"kind\":\"{}\",\"log\":{},\"unit\":{}{}}}",
                json_str(p.id),
                json_str(p.name),
                json_num(p.min),
                json_num(p.max),
                json_num(p.default),
                p.kind_name(),
                p.log,
                json_str(p.unit),
                labels
            )
        })
        .collect();
    format!(
        "{{\"name\":{},\"description\":{},\"channels\":{},\"inputs\":{},\"duck_sensitivity\":{},\"entry\":{},\"params\":[{}]}}",
        json_str(info.name),
        json_str(info.description),
        info.channels,
        g.inputs(),
        json_num(g.duck_sensitivity().clamp(0.0, 1.0)),
        match g.entry() {
            Entry::Now => "{\"mode\":\"now\"}".to_owned(),
            Entry::Bar => "{\"mode\":\"bar\"}".to_owned(),
            Entry::Fade { bars } if bars.is_finite() && bars > 0.0 => format!("{{\"mode\":\"fade\",\"bars\":{}}}", json_num(bars)),
            Entry::Fade { .. } => "{\"mode\":\"now\"}".to_owned(),
        },
        params.join(",")
    )
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_num(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "null".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_counts_in_fours() {
        assert_eq!(Position::at(0.0, 4).to_string(), "1.1.1.1.1");
        assert_eq!(Position::at(0.25, 4).to_string(), "1.1.1.1.2");
        assert_eq!(Position::at(5.0, 4).to_string(), "1.1.2.2.1");
        // 16 bars = 64 beats = one epoch
        assert_eq!(Position::at(64.0, 4).to_string(), "2.1.1.1.1");
        assert_eq!(Position::at(63.99, 4).to_string(), "1.4.4.4.4");
        assert_eq!(Position::at(64.0 + 16.0 + 4.0, 4).absolute_bar(), 21);
    }
}
