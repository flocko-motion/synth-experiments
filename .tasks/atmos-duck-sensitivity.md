# atmos: declare duck sensitivity 0.9

Status: done. atmos declares 0.9 (076907e); the mixer ducks it natively,
and the sensitivity is now also a mixer setting per channel (64967f3).
For: the agent that owns gen-api and the mixer-native ducking change (pipe v4)
From: the session that wrote atmos (committed in 6029da3)

## What

Add the duck sensitivity override to `impl Generator for Atmos` in
`generators/atmos-1/src/lib.rs`, next to `params()`:

```rust
fn duck_sensitivity(&self) -> f64 {
    0.9
}
```

0.9 is the value agreed for a pad: the mixer ducks atmos hard on each
psybass duck event. atmos emits no events itself, so leave `take_events` as
the trait default.

## Why it isn't done yet

`Generator::duck_sensitivity` only exists in the uncommitted gen-api
changes. Committing the override before them would break the atmos build.
Land it in the same commit as the gen-api change, or straight after.

## Already done

atmos's built-in tempo-synced pump (the `pump` param) was removed, so atmos
doesn't get ducked twice and doesn't keep pumping when the kick drops out in
fills. The `atmos` description and module doc already say that ducking
belongs to the mixer.

## Done when

- `target/release/atmos --info --json` shows `"duck_sensitivity":0.9`
- `cargo test -p atmos` passes
- added to the mixer, atmos audibly ducks on psybass kicks and stays steady
  during its kick-less fills
