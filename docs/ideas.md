# Ideas

Things we decided not to build yet, with enough context to pick them up
later. Newest first within each section.

## Mixing

### Per-source duck envelopes

Duck sensitivity is a mixer setting on every channel now. Release and
curve of the duck envelope are still global mix settings; they could
become per source (a long kick ducks longer than a snare).

### Insert effect chains

The mixer routes effects through send buses only: each channel has a send
amount per effect bus. Insert chains (the whole signal goes *through* an
effect) were left out for simplicity. Ducking no longer needs them, since
it's native to the mixer, but distortion or filter effects on a single
generator would.

## Generators

### Arrangement signals

Generators play all the time. Psytrance lives on taking things away:
kick and bass drop out for breaks, hats thin out in intros, FX build into
drops. The host could send a section (intro/build/drop/break) or an energy
level in the context, so all generators agree. FX generators are useless
without it. The event channel added for ducking could also carry these
from generator to host.

### psybass phrases on the shared grid

psybass composes 8-bar phrases, which don't line up with the grid's 4-bar
cycles. Its 16-bar fills do line up with epochs. Moving it to cycles and
epochs would make it consistent with percussion and acid.

### More generators

Still missing for a full psytrance track: plucks/arps, FX (risers, sweeps,
impacts: needs arrangement signals), pads/atmos for breakdowns.

## Plugin system

### Sample-accurate parameter changes

Parameter values arrive as a snapshot once per block (about 10 ms).
Change events with a sample offset, as CLAP has them, can come later if
something needs finer automation.

### Plugin directory and search path

The mixer finds generators by repo convention (`generators/*`, binaries in
`target/release`). A plugin search path (`$WS_PLUGIN_PATH`, defaulting to
`./plugins`) only pays off for plugins from outside this repo, shipping
the mixer without sources, or freezing versions during a session.
Keep the name-to-binary lookup in one function so this stays a small
change.

### CLAP adapter

The plugin API copies CLAP's models (parameters, ports, transport) so a
generic wrapper could turn any `Generator` into a CLAP plugin for Bitwig
or Reaper. Only needed if DAW use becomes a goal. Key and scale would need
a CLAP extension or a parameter, since CLAP's transport has no tonality.
