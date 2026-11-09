# Sequencer

Status: **built**: the banks and cueing (`workspace/src/sequencer.rs`),
the HTTP API, the CLI, the web UI's sequencer band, and the score reaching
plugins (`Context::score`, pipe v5). Key regions already change
`Context::tonality` for every plugin. `bass`, `psybass` (not its kick) and
`acid` follow chords: the line moves to the chord root and counts its scale
degrees from there (`Tonality::chord_degree`). `psybass`, `acid` and
`percussion` react to dynamics marks and tempo feel (see their module
docs); the other generators are being done the same way.

In the API, the layers are named `marks` (dynamics), `chords` and `keys`
(harmony) and `feels` (tempo); feels are `half`, `double`, `quad` and
`triplet`. Which bank plays isn't saved, so a restarted daemon plays A in
every lane.

The workspace owns the score as well as the clock: what's happening
musically (a quiet part, a rise, the chord, half-time) on a grid every
plugin agrees on. Generators keep their own musical decisions; the score
tells them *where they are*, and each reacts in its own way. A human edits
the score in the UI or the HTTP API; plugins can switch banks through
events.

Loop, not song: each lane plays a 16-bar grid over and over, and you
perform by switching banks. There is no song mode and no bank chaining.

## Model

```
             1   2   3   4   5   6   7   8   9  10  11  12  13  14  15  16
dynamics  A  ·   ·   ·   ·   [quiet·········]   [rise·························]   A B C D
harmony   B  [1···········] [6············] [7············] [1·············]   A B C D
tempo     A  [1×·····································] [½×············] [2×··]   A B C D
```

- **Lane**: one kind of information. There are three: `dynamics`,
  `harmony` and `tempo`.
- **Bank**: one lane's content for one epoch (16 bars). Each lane has 4
  banks, A to D, and plays one of them. Lanes switch banks independently,
  so dynamics B can run over harmony A.
- **Region**: a span of whole bars inside a bank, with a kind and values.
  Regions in a lane never overlap; bars outside every region are the
  lane's neutral state. Regions don't wrap from bar 16 to bar 1.
- **Marker**: chords and keys have no length. Each sits on one bar and
  holds until the next one; around the loop, the bank's last marker holds
  into the bars before its first. Into the next epoch, the last one holds
  until the next bank's first. In the API they're regions with `bars: 1`
  (which may be left out); spans sent or saved for them are cut to their
  first bar.

Resolution is one bar for every lane, so all grids line up.

## Lanes

### dynamics

| Kind | Meaning |
|---|---|
| (none) | Normal: plugins play as they would without a score. |
| `quiet` | Take things away. |
| `rise` | Build towards the region's end. |
| `peak` | Everything in, full energy. |
| `break` | Breakdown: kick and bass out, pads and FX carry it. |

Every region has an `intensity`, 0..1, default 1: a `quiet` at 1 is
nearly silent, at 0.3 it's a bit thinner. How a plugin reacts is its own
business. For example, fx lands a riser on the last bar of a `rise`, kick
drops out in a `break`, percussion thins out in a `quiet`, and atmos opens
up in a `break`.

New kinds get added as plugins need them. Plugins treat unknown kinds as
normal.

### harmony

This lane has two layers on one grid:

- **Key**: root and scale, as `ws key` sets them today. Usually empty,
  which means the daemon's key (`ws key`, `PATCH /api/...`) applies. A key
  marker modulates from its bar on, for that bank only.
- **Chord**: a scale degree with an optional quality, relative to the key
  at that bar, held until the next chord. Because chords are degrees, a
  bank works in any key.

Chord notation: `[b|#]<degree 1-7>[quality]`.

| Quality | Chord |
|---|---|
| (none) | Diatonic triad: whatever the scale gives on that degree |
| `M` | Major |
| `m` | Minor |
| `dim` | Diminished |
| `aug` | Augmented |
| `sus2`, `sus4` | Suspended |
| `7`, `M7`, `m7`, `m7b5`, `dim7` | Sevenths |

Examples in E phrygian: `1` is E minor, `2` is F major, `5` is B
diminished (or write `5dim`, which is the same thing), `3m` is G minor
(borrowed). `#` and `b` move the chord root a semitone up or down from
the scale degree, for chromatic chords outside the scale; diatonic chords
never need them.

The host resolves the notation, so plugins never parse it. They get the
chord's root pitch class and its pitch classes as a 12-bit set.

Without any chord marker, bars have no chord: plugins compose freely in the
key, as they do now. atmos follows the chord lane where it has content and
composes its own progression where it doesn't; acid and bass can lean
towards chord tones as much as they like.

### tempo

The feel per bar: `1×` (none), `½×`, `2×`, `4×`, `3:2` (triplet).

This is a **hint, not a clock change**. BPM, bar length, the grid, delay
times and ducking all stay the same; plugins decide what half-time means
for them (kick on every other beat, hats in 32nds, acid in triplet 8ths).
`StepClock` gets a feel-aware mode so plugins get the right steps for
free.

## Banks and switching

- **Editing the playing bank applies now**, at the render head, like any
  other change. A region that's playing when it's edited just changes;
  plugins see a new revision and re-plan.
- **Editing any other bank is for later**: it's stored and plays once
  that bank is switched to.
- **Switching means cueing**: you cue the bank a lane plays next, and the
  switch lands at the end of the current epoch, on bar 1 of the new bank.
  One cue per lane; cueing again replaces it, and cueing the playing bank
  cancels it.
- A cued bank is visible in the UI (blinking) and to plugins (it's the
  next epoch in their timeline) for the rest of the epoch, so fx can
  build into it and fills can set it up.

## What plugins see

Plugins never deal with banks. The host turns the playing and cued banks
into a **timeline**: the score for the 32 bars from the start of the
current epoch, that is this epoch's playing bank followed by the next
epoch's (the cued bank, or the same one again). Generators plan
per cycle and epoch, so this is enough for acid composing at bar 1 to know
a `quiet` is coming at bar 5, or for fx to know where a `rise` ends.

```rust
pub struct Context<'a> {
    // ... as today; `tonality` is now the key at the current bar,
    // from the harmony lane, so plugins that know nothing about the
    // sequencer still follow modulations.
    pub score: &'a Score,
}

pub struct Score {
    /// Changes whenever the timeline changes: an edit of the playing
    /// or cued bank, a cue, or the next epoch starting. Re-plan when it does.
    pub revision: u32,
    /// Absolute bar of timeline bar 0 (the current epoch's first bar).
    pub first_bar: u64,
    pub bars: [ScoreBar; 32],
}

pub struct ScoreBar {
    pub dynamics: Dynamics,   // kind, intensity, region start and length
    pub key: Tonality,
    pub chord: Option<Chord>, // root pitch class, pitch-class set
    pub feel: Feel,
}
```

Helpers do the arithmetic, so every plugin agrees on it:
`score.now(ctx)` gives the current bar with the region's `progress`
(0..1, sample-accurate within the block), and `score.next_change(lane)`
gives the next bar where a lane changes.

Determinism holds: the score is part of the context sequence, so the same
seed and the same contexts still give the same output.

## Wire: pipe protocol v5

The timeline is fixed size and small, so it can travel in full whenever
it changes:

- v5 is negotiated, so v4 plugins keep working: the host's hello carries
  the highest version it speaks, a v5 generator answers `WSG5`, and only
  those get the score.
- Bit 1 of request byte 4 (bit 0 is `playing`) says "score follows". When
  set, the request carries the 32-bar timeline (16 bytes per bar, 528 in
  all) after the parameter snapshot; the layout is in
  `gen_api::pipe::encode_score`.
- The host sends it on the first request, after a restart, and whenever
  `revision` changes. gen-api keeps the last one and hands it to
  `process`.
- The `root`/`scale` header bytes carry the key at the block's first bar,
  so v4 semantics stay intact.

## Plugins controlling the sequencer

A new event kind travels upstream like `duck`:

| Kind | Fields | Meaning |
|---|---|---|
| 2 cue | `reserved` = lane, `value` = bank | Cue a lane's next bank |

So an "arranger" plugin is just another plugin. Writing regions from a
plugin doesn't fit in a 12-byte event and is left open (see below).
Conflicts are last-write-wins.

## HTTP API and persistence

```
GET    /api/sequencer                        every lane: banks, playing, cued
PUT    /api/sequencer/{lane}/banks/{A-D}     replace a bank's regions
DELETE /api/sequencer/{lane}/banks/{A-D}     clear a bank
POST   /api/sequencer/{lane}/banks/{A-D}/repeat  {"times": 2}   (2, 4 or 8)
POST   /api/sequencer/{lane}/cue             {"bank": "B"}
POST   /api/sequencer/copy                   {"from": "A", "to": "C"}
                                             optional "lanes": ["harmony"]
```

Copying replaces the target bank's regions with the source's, in every
lane unless `lanes` narrows it, so a whole scene (A in all three lanes)
can be duplicated in one call and then varied. Copying or clearing a bank
follows the editing rule: the playing bank changes now, any other is for
later. A bank can't be copied onto itself.

Repeat fills a bank from its own start: `times` 2 copies bars 1-8 onto
9-16, 4 repeats bars 1-4 four times, 8 repeats bars 1-2. Regions reaching
past the repeated part are cut at its end.

The CLI gets the same verbs (`ws cue harmony B`, `ws copy A C`,
`ws copy A C harmony`, `ws repeat harmony A 4`, `ws clear dynamics B`). Banks are stored in
`<state dir>/sequencer.json`, written the same way as `favorites.json`, so
they survive restarts.

## UI

One full-width band above the channel strips. Its header line holds the
toolbar: a hint, or the editor for the picked region, and "copy all
lanes". Below it, one row per lane, grouped: dynamics and tempo, then
harmony with the key above the chords. Each row has:

- a 16-bar grid with the playhead running across it
- drag across bars to make a region, then pick its kind (dynamics), chord
  or key (harmony), or feel (tempo); click a region to edit it
- drag a region to move it, drag its edges to resize it; whatever it lands
  on is trimmed away, and the grid shows the result while dragging
- right-click a region, or select it and press Delete, to remove it
- the copy menu also repeats the shown bank's first 8, 4 or 2 bars (2×,
  4×, 8×) over the rest
- A B C D on the right: the playing bank lit, a cued one blinking;
  click to cue, click the playing one to cancel
- the grid shows the bank being edited, which can be any of the four:
  editing the playing one is live, editing another prepares it
- copy and clear for a bank, per lane or across all lanes

## Open

- **Plugins writing regions**: needs a bigger event payload, or a plugin
  side channel.
- **Existing grid rules**: kick and bass drop out at the end of every 16th
  bar, and fx fires risers every 16 bars. Once the score exists, those
  should probably come from dynamics regions instead, falling back to the
  current rule when the lane is empty.
