# Workspace

`ws` (in `workspace/`) is the host from [generator-interface.md](generator-interface.md):
a daemon that owns the clock, the click and the mix, and plays to the
default audio device. Generators run as subprocesses over the pipe protocol.

```sh
cargo build --release
target/release/ws daemon --bpm 148 --key E &      # or in its own terminal
target/release/ws add target/release/psybass --seed 4         # named psybass
target/release/ws add target/release/psybass                  # psybass-2
target/release/ws add --name hats target/release/percussion
target/release/ws                                  # status
target/release/ws pos                              # 2.1.3.4.1  bar 23 ...
target/release/ws bpm 150; target/release/ws key F# minor
target/release/ws gain psybass -3; target/release/ws mute hats
target/release/ws seek 17; target/release/ws stop; target/release/ws play
target/release/ws quit
```

The root Makefile wraps the common steps: `make daemon` (takes `BPM`, `KEY`,
`SCALE`), `make add psybass` (takes `SEED`, and `NAME` to pick the name), `make rm psybass`.

`ws help` lists everything. Relative generator paths resolve against the
directory `ws add` runs in. The socket is `$TMPDIR/synth-ws.sock`, or set
`$WS_SOCKET`.

## HTTP API

The daemon also serves a JSON API on `http://127.0.0.1:7700` (`--http ADDR`
to move it, `--no-http` to turn it off; `ws url` prints it). The contract is
[`workspace/openapi.yaml`](../workspace/openapi.yaml), also served at
`/api/openapi.yaml`, so clients can generate types from it.

```sh
A=http://127.0.0.1:7700
curl -s $A/api/catalog                                   # what can be added
curl -s -X POST $A/api/generators -H 'Content-Type: application/json' \
     -d '{"generator": "psybass", "seed": 4}'             # → 201, named psybass
curl -s -X PATCH $A/api/generators/psybass -H 'Content-Type: application/json' \
     -d '{"gain_db": -6}'
curl -s -X DELETE $A/api/generators/psybass              # → 204
curl -s -N $A/api/events                                 # live state + log (SSE)
```

- **Adding**: `generator` is a name from `/api/catalog`: executables in
  `--catalog DIR` (default `target/release`) that answer `--info`. Freshly
  built generators show up without a restart. `command` takes any
  pipe-protocol program instead. Leave out `name` and one is derived,
  with `-2`, `-3`, … appended if taken.
- **Web UI**: every other GET is served from `--ui DIR` (default
  `workspace/ui`, relative to where the daemon starts), read on each
  request, so new or edited files show up on reload with no rebuild.
  Requests can't reach files outside that directory, symlinks included.
- **Live updates**: `/api/events` streams `state` about 20 times a second
  and every log line, generator stderr included.
- **Access**: loopback only. Requests must carry a loopback `Host`, and
  an `Origin`, if present, of a localhost page. That allows dev servers on
  any port (CORS) and keeps other websites from starting programs.
  Bodies must be `application/json`.
- The CLI and the API share one implementation (`workspace/src/api.rs`).

## How it runs

- **Mixer thread** owns all state and keeps a ring buffer `--ahead` ms full
  (default 200). For each 512-frame block it sends a request to every
  generator, then collects the chunks, so generators render in parallel.
  Then it mixes them (mono goes to every channel), adds the click, soft-clips
  (`tanh`) and pushes the block into the ring.
- **Audio callback** only copies from the ring. It never waits on a
  generator. `underruns` in `ws status` counts callbacks the ring couldn't
  fill.
- **Late generators**: the mixer waits for a chunk until half the buffered
  audio would have played. If the chunk is later than that, that
  generator's block is mixed as silence (the `LATE` column) and the stale
  chunk is dropped when it arrives. Once a generator is behind, it gets only
  a few ms per block until it catches up.
- **Clock**: `beat = anchor_beat + (frame - anchor_frame) / samples_per_beat`,
  re-anchored on tempo, transport and seek changes, so position never
  drifts. Changes apply at the render head, so you hear them `--ahead` ms
  later.
- **Click**: off by default (`ws daemon --click` to start with it on). A beep per beat. Pitch and level rise for bar, cycle and epoch
  starts. `ws click on|off`, `ws gain click DB`.
- **Sample rate** is the device's (44.1 kHz on the built-in output), passed
  to generators in the hello.

## Crashing generators

Generators are written quickly and expected to break. A broken generator
never stops the mix: its slot goes silent and is restarted. The restart
joins at the current position, and gain and mute carry over. Restarts back
off from 0.25 s to 10 s for a generator that keeps failing, and the backoff
resets once a run lasts 10 s. `ws restart NAME` restarts one immediately,
for example after rebuilding it.

| Failure | Detected by | Action |
|---|---|---|
| crash, panic, exit | pipe closes | restart |
| hang | pending requests and no chunk for 1 s | kill, restart |
| too slow to keep up | chunks late | silence for late blocks; at most 2 requests in flight, then blocks are skipped (looks like a seek to the generator) |
| prints on stdout | bad chunk header | kill, restart (`gen-api` generators are protected: stdout goes to stderr) |
| NaN/inf | sample check | replaced with 0; restart after 40 blocks in a row (≈0.5 s) |
| floods stderr, binary junk | — | lossy UTF-8, lines cut at 400 chars, 40 lines/s, then "… N lines dropped" |
| leaves children, ignores EOF | — | each generator runs in its own process group, killed 300 ms after its stdin closes |

The first start in `ws add` is the exception: if the command doesn't
exist or fails the handshake, `add` reports the error and adds nothing.

`ws status` shows a generator's state (`ok`, `late`, `nan`, `muted`, `entering`,
`starting`, `retry 1.2s`), its `LATE` and `RESTARTS` counts and its last
error. Its stderr, including panic messages, goes to the daemon's log,
prefixed with `[name]`. Ctrl-C, SIGTERM or a closed terminal shut the
daemon down cleanly, generators included. Each generator runs in its own
process group, so Ctrl-C reaches only the daemon.
