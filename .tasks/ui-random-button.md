# UI: "random" button for plugin params

Status: done (2026-11-09) by synth-experiments-71, uncommitted in
workspace/ui and waiting for the user's review. RANDOM button on every
generator strip, send return and insert panel, sent as one PATCH. UNDO goes
back one roll. Shift-click nudges each param by up to ±15% of its travel.
The effect's Duck knob is excluded because it's a mixer setting. A per-param
random hint isn't implemented; if one gets added to gen-api later, tell -71.
For: synth-experiments-71 (owns workspace/ui)
From: synth-experiments-c5 (atmos, reverb, delay)
Asked for by: the user, 2026-11-09

## What

Give every plugin in the web UI (generators and effects, inserts and sends)
a "random" button next to its knobs. One click sets each of that plugin's
params to a random value within its range, through the same API call the
knobs use (PATCH the param values). The knobs then show the new values.

No API change is needed (confirmed by synth-experiments-5f, who owns the
daemon and its HTTP API). `PATCH /api/generators/{name}/params` and
`PATCH /api/effects/{name}/params` take any subset of params, and the host
clamps each value to its range and snaps integer, toggle and choice params to
whole numbers. Randomising a whole card is one PATCH with every param in the
body, applied atomically between two blocks.

## How to pick values

Use the param description the API already returns (`min`, `max`,
`default`, `kind`, `log`, `labels`):

- `continuous`: uniform in [min, max]; log-uniform when `log` is true
  (frequencies, times), otherwise the knob's low end barely gets picked
- `integer`: uniform over the whole numbers in range
- `choice`: a random label
- `toggle`: on or off, 50/50

## Open points for you and the user

- **Some params are risky to randomise**: a send effect's `mix` (a send
  should stay fully wet), `drive` at maximum, a reverb `decay` of 12 s. For
  v1, randomise everything. If that proves annoying, ask the gen-api owner
  (synth-experiments-07) to add a per-param hint such as `random: false` or
  a narrower random range in `Param`, and have the UI respect it.
- **Undo**: a way back to the previous values (one-level undo, or "reset to
  defaults") makes the button safe to press while music is playing.
- **Degree**: possibly a second gesture (shift-click?) that only nudges
  values around the current ones instead of across the whole range.

## Done when

- every plugin card has the button, generators and effects alike
- one click moves every knob, and the change is audible right away
- log-scaled params (`reverb` decay, damp, lowcut) land evenly across
  their knob travel, not mostly at the top end
