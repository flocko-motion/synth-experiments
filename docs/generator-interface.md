# Generator ↔ mixer interface

Status: **draft**. Everything described here exists in `crates/gen-api`: the
in-process Rust API (`lib.rs`), the standard CLI (`cli.rs`) and the pipe
protocol a host uses to run generators as subprocesses (`pipe.rs`).

## Roles

| | Owns | Doesn't own |
|---|---|---|
| **Mixer** (host) | Time (tempo, position, start/stop), harmony (key, scale), sample rate, mixing and output | What a generator plays |
| **Generator** | Its sound and its musical decisions (patterns, motifs, fills, filter movement) | The clock, the key |

A generator never runs its own clock. It reacts to the position the host
reports, so any number of generators stay in sync and agree on where bar 1,
bar 17 and every phrase boundary are.

The standalone CLI (`gen_api::cli`) is the reference host: fixed tempo, fixed
key, a transport starting at bar 1, WAV out.

## The API

Defined in `crates/gen-api/src/lib.rs`.

```rust
pub trait Generator {
    fn new(config: &Config) -> Self where Self: Sized;
    fn info(&self) -> Info;
    fn process(&mut self, ctx: &Context, out: &mut [f32]);
}
```

### Construction: `Config`

| Field | Meaning |
|---|---|
| `sample_rate: f64` | Fixed for the generator's lifetime. |
| `seed: u64` | Every random choice the generator makes derives from this. |

### Discovery: `Info`

Besides `Info`, a plugin can override `params()` (see
[Parameters](#parameters)) and `inputs()` (see [Effects](#effects)).

| Field | Meaning |
|---|---|
| `name` | Short identifier, e.g. `psybass`. |
| `description` | One line for humans. |
| `channels` | Interleaved channel count of `out`. |

Exposed on the CLI as `--info`.

### Per block: `Context`

The host calls `process` once per block. `out.len()` is
`frames × info().channels`; the block size can vary from call to call.

| Field | Meaning |
|---|---|
| `sample_rate` | Same as in `Config`. |
| `transport.bpm` | Tempo, constant within the block. |
| `transport.beat` | Musical position of the block's **first sample**, in quarter notes since bar 1 beat 1 (= `0.0`). |
| `transport.beats_per_bar` | Time signature numerator; 4 for everything so far. |
| `transport.playing` | `false`: trigger nothing new, let sounding voices ring out. |
| `tonality.root` | Pitch class, 0 = C … 11 = B. |
| `tonality.scale` | `Phrygian`, `Minor`, `HarmonicMinor`, `Dorian` or `Major`. `Tonality::degree(n)` maps a scale degree to semitones above the root. |
| `params` | Every parameter's current value, in `params()` order; `ctx.param(i)`. Empty if the plugin has none. |
| `input` | Effects only: this block's input audio, interleaved. Empty for generators. |

`transport.beat` is the click: it tells the generator both *where* it is
(pattern position) and, together with `bpm`, *when* the next subdivision
falls.

`transport.position()` turns `beat` into the shared grid, where each level
is four of the one below:

```
tick (16th) → beat → bar → cycle (4 bars) → epoch (16 bars)
```

It prints like a DAW counter, `epoch.cycle.bar.beat.tick`, 1-based:
`1.1.1.1.1` is the start, `2.1.1.1.1` is bar 17. `absolute_bar()` gives
bars since 0. Generators should hang structure on this grid (fills at the
end of a cycle or epoch) so they all agree on it.

## Contract

A generator must:

1. **Follow the host's position.** Decisions tied to the musical grid ("fill
   on the last bar of every 16") use host bars, not a count of blocks or
   samples the generator received.
2. **Survive discontinuities.** Blocks are normally contiguous: each block's
   `beat` follows on from the previous one. When it doesn't (seek, loop),
   resync to the new position without panicking or glitching.
3. **Follow changes.** Tempo and tonality can change between blocks. Apply
   them from the next musically sensible point (psybass: the next note).
4. **Be deterministic.** Same seed + same sequence of contexts → same
   output, sample for sample.
5. **Overwrite `out`** and keep samples within [-1, 1].

Not required yet, but worth keeping in mind for a real-time mixer: no
allocation, locking or I/O inside `process`. psybass currently prints a line
to stderr when it composes a phrase.

### Finding grid boundaries in a block

Use `gen_api::StepClock`; it covers rules 2 and 4 above for step triggering:

```rust
fn process(&mut self, ctx: &Context, out: &mut [f32]) {
    let mut steps = self.clock.block(ctx);
    for (i, o) in out.iter_mut().enumerate() {
        while let Some(step) = steps.due(i) {
            self.trigger(step, ctx); // absolute 16th since 0
        }
        *o = self.render_one_sample();
    }
    self.clock.finish(steps);
}
```

How it works: with `spx = sample_rate × 60 / bpm / 4` samples per 16th and
the block starting at 16th position `p0 = beat × 4`, sample `i` sits at
`p0 + i / spx`, and step `n` is due at the first sample where that reaches
`n`. The clock keeps the next unfired step as state. If a block's `p0` isn't
within one 16th of it (first block, seek, loop, restart, skipped blocks), it
resumes at `ceil(p0)` instead of firing the missed steps. While the
transport is stopped, nothing is due.

## Shared DSP

`crates/dsp` holds the building blocks generators share: `Svf` (TPT
state-variable filter, LP/BP/HP), `Ladder` (4-pole resonant lowpass with
saturation in the loop), `OnePole`, band-limited `saw` and `square`
(PolyBLEP), `Reverb` (stereo FDN, for pads and tails), `decay_coef`,
`pan_gains`, `midi_hz`.

## Standard CLI

Every generator binary's `main` is
`gen_api::cli::run::<MyGenerator>()`, so they all behave identically:

```
<gen> [bpm] [bars] [--seed N] [--key ROOT] [--scale NAME] [--out FILE] [--info]
<gen> --demo [--seed N] [--key ROOT] [--scale NAME]
<gen> [--seed N] --pipe        run under a host, see below
```

- bpm defaults to 148; no bars means endless.
- Audio (48 kHz, 24-bit WAV) goes to stdout unless `--out FILE`; status goes
  to stderr. Writing audio to a terminal is refused.
- Endless streams carry an "unknown length" WAV header (sizes `0xFFFFFFFF`);
  sox needs `--ignore-length`.
- `--key` defaults to a root derived from the seed; `--scale` to phrygian.
- `--demo`: endless at 148 bpm. On a terminal it starts sox's `play` and
  streams into it; otherwise it writes to stdout.

## Running under a host: the pipe protocol

Rust has no stable ABI, so a host can't load a separately compiled Rust
trait object. Instead, the host starts the generator binary with `--pipe`
and talks to it over stdin/stdout in lockstep. This isolates crashes and
works for generators in any language. `gen-api` implements the generator
side, so a Rust generator gets it for free from `cli::run`. In `--pipe` mode
the only other flag accepted is `--seed`; tempo and key come from the host.

All values are little-endian. The generator's stderr is free for logging,
and the host prefixes it with the generator's name.

Protocol version 4:

```text
host -> gen   hello     16 bytes   "WSH4" | u32 sample_rate | u32 max_frames | u32 reserved
gen  -> host  hello     16 bytes   "WSG4" | u16 channels | u16 inputs | u16 params
                                   | u16 reserved | u32 json_len
                        + json_len bytes: the plugin description (as `--info --json`)
then, repeated:
host -> gen   request   32 bytes   header, see below
                        + params × f64       snapshot of every parameter
                        + frames × inputs × f32, interleaved   (effects only)
gen  -> host  chunk     12 bytes   "WSC4" | u32 frames | u32 events
                         + events × 12 bytes   u32 frame | u16 kind | u16 reserved | f32 value
                         + frames × channels × f32, interleaved
```

The description travels in the handshake so the host always has the
metadata of the exact process it talks to, also for plugins that aren't in
any catalog. For a plugin without inputs or params a request is just the
32-byte header. `gen_api::pipe` has host-side helpers: `Hello::encode`,
`read_gen_hello`, `write_request`, `decode_chunk_header`.

The chunk header lets the host notice a corrupted stream (typically a
debug print on stdout). The host then kills and restarts the generator.
`gen-api` points fd 1 at stderr in pipe mode, so `println!` in a Rust
generator goes to the log. Generators in other languages must log to stderr
only. Generators answer requests strictly in order, and the host may send
the next request before reading the previous chunk.

Request layout:

| Offset | Type | Field |
|---|---|---|
| 0 | u32 | `frames` (≤ `max_frames`) |
| 4 | u8 | `playing` (0/1) |
| 5 | u8 | root pitch class, 0 = C |
| 6 | u8 | scale, index into `Scale::ALL` |
| 7 | u8 | `beats_per_bar` |
| 8 | f64 | `bpm` |
| 16 | f64 | `beat`: position of the first frame |
| 24 | u64 | `frame`: absolute sample index of the first frame |

The host closes stdin to stop the generator, which then exits (it's killed
after 300 ms if it doesn't). A generator in another language only needs to
implement this: read 16 bytes, write 16 plus its JSON, then loop reading
`32 + params × 8 + frames × inputs × 4` and writing `12 + frames × channels × 4`
(plus 12 bytes per event, if it sends any).

Expect skipped requests: a generator that falls behind gets a gap in
`frame`/`beat`, which it treats like a seek. A restarted generator's first
request is wherever the workspace is now.

Alternatives considered: a dynamic library with a C ABI (fastest, but needs
`unsafe` glue, and a crashing plugin takes the host down) and WASM
components (sandboxed, but a heavy dependency and a more involved plugin
build).

## Parameters

Plugins declare parameters by overriding `Generator::params`; the model
follows CLAP's:

```rust
const PARAMS: [Param; 2] = [
    Param::continuous("cutoff", "Cutoff offset", -2.0, 2.0, 0.0).unit("oct"),
    Param::choice("rate", "Rate", &["1/4", "1/8", "1/2"], 0),
];
fn params(&self) -> &'static [Param] { &PARAMS }
// in process(): ctx.param(0)
```

| Field | Meaning |
|---|---|
| `id` | Stable, unique within the plugin. Hosts address and store values by it (the workspace API: `PATCH /api/generators/{name}/params` with `{"cutoff": 1.5}`), and values survive restarts by it. Never rename. |
| `name`, `unit` | For humans. |
| `min`, `max`, `default` | Plain units. |
| `kind` | `Continuous`, `Integer`, `Toggle` (0/1) or `Choice(labels)` (index). |
| `log` | UI hint: map knob travel logarithmically. |

Rules:

- Values arrive as a full snapshot with every block, in declaration order,
  already clamped to the range (and rounded for integer kinds). Hosts send
  the defaults until something is set, so the first block is complete.
- Values change in steps once per block; smooth continuous parameters
  inside the plugin to avoid zipper noise.
- Automation lives in the host: it just sends different values.
- The seed is not a parameter. It's fixed for an instance; a different seed
  means a new instance.

On the CLI: `--set ID=VALUE` (numbers, `on`/`off`, or a choice label),
listed with ranges by `--help`; `--info --json` prints the description.

## Events and ducking

Plugins can tell the host about things happening at a frame within the
block, through `Generator::take_events`, which the host calls after every
`process`. On the wire they travel in the chunk, between header and audio,
in frame order. Event kinds:

| Kind | Value | Meaning |
|---|---|---|
| 1 duck | strength 0..1 | "Duck the other channels now", typically a kick |

Hosts skip kinds they don't know, so newer plugins work with older hosts.

Ducking is native to the mixer, not a plugin. psybass sends a duck event for
every kick it actually plays; fills without kick send none, so nothing
ducks there. Every plugin declares a fixed `Generator::duck_sensitivity`
(also in the description JSON as `duck_sensitivity`), and the mixer ducks
its channel by `sensitivity × strength`, recovering along a duck envelope
whose release and curve are mix settings:

| Plugin | Sensitivity |
|---|---|
| psybass | 0 (it's the kick) |
| percussion | 0.2 |
| acid | 0.6 |
| atmos | 0.9 |

The mixer gets every chunk of a block before mixing, so a duck can apply in
the same block, sample-accurate. Adjustable sensitivity per channel is
captured in [ideas.md](ideas.md).

## Effects

An effect is a plugin with `Generator::inputs` > 0. It reads its input from
`ctx.input` (interleaved, `inputs` channels, the same frame count as
`out`); the host adapts channel counts, mono to every input and stereo to
mono by averaging.

The context describes the position of the input audio. The workspace runs
effects one block behind their source so all plugins still render in
parallel; don't assume output is sample-aligned with a live transport.

On the CLI an effect reads WAV from stdin and stops when it ends (or after
`bars`). Give it the same bpm as its source, which is assumed to start at
bar 1:

```text
acid 145 | duck 145 | play -q --ignore-length -t wav -
acid | duck --demo
```

## Open questions

- **Beyond transport and tonality.** Should the mixer send arrangement
  signals, such as section (intro/drop/break), energy, or "fill now"? Or
  per-generator parameters?
- **Channel layout.** Mono and interleaved stereo are covered by
  `channels`; does the mixer need anything more?
- **Phrase length.** psybass hard-codes 8-bar phrases and 4/4, which doesn't
  line up with the shared grid's 4-bar cycles and 16-bar epochs. Should it
  move to cycles and epochs?
- **Versioning.** The pipe magics (`WSH2`, `WSG2`, `WSC2`) carry a version digit;
  the protocol has no channel for name or description yet, so the host
  learns those from `--info`.
- **Logging.** Generators print to stderr today; a mixer may want
  structured status instead.
