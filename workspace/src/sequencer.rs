//! The sequencer: the score the workspace owns, see docs/sequencer.md.
//!
//! Three lanes (dynamics, harmony, tempo), each with four banks of one
//! epoch (16 bars). A bank holds regions, spans of whole bars, in one or
//! more layers: dynamics has `marks`, harmony has `chords` and `keys`,
//! tempo has `feels`. Chords and keys are markers rather than spans: one
//! bar long, each holds until the next one, and around the loop the last
//! one holds into the bars before the first. Each lane plays one bank in a loop; a cued bank takes
//! over at the next epoch boundary the mixer renders.
//!
//! Bank contents are kept on disk (`<state dir>/sequencer.json`); which
//! bank plays is not, so a restarted daemon plays bank A in every lane.
//!
//! Plugins get it as a [`gen_api::Score`]: the 32 bars from the start of
//! the rendered epoch, chords resolved to pitch classes (see
//! [`Sequencer::timeline`]).

use crate::log::wlog;
use gen_api::{Scale, Tonality};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const BARS: u8 = 16;
pub const BANKS: usize = 4;
const BANK_NAMES: [&str; BANKS] = ["A", "B", "C", "D"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    Dynamics,
    Harmony,
    Tempo,
}

impl Lane {
    pub const ALL: [Lane; 3] = [Lane::Dynamics, Lane::Harmony, Lane::Tempo];

    pub fn name(self) -> &'static str {
        match self {
            Lane::Dynamics => "dynamics",
            Lane::Harmony => "harmony",
            Lane::Tempo => "tempo",
        }
    }

    pub fn parse(s: &str) -> Option<Lane> {
        Lane::ALL.into_iter().find(|l| l.name().eq_ignore_ascii_case(s))
    }

    /// The layers a bank of this lane has, in display order.
    pub fn layers(self) -> &'static [Layer] {
        match self {
            Lane::Dynamics => &[Layer::Marks],
            Lane::Harmony => &[Layer::Chords, Layer::Keys],
            Lane::Tempo => &[Layer::Feels],
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    Marks,
    Chords,
    Keys,
    Feels,
}

pub const MARKS: [&str; 4] = ["quiet", "rise", "peak", "break"];
pub const FEELS: [&str; 4] = ["half", "double", "quad", "triplet"];
pub const QUALITIES: [&str; 12] = ["", "M", "m", "dim", "aug", "sus2", "sus4", "7", "M7", "m7", "m7b5", "dim7"];

impl Layer {
    /// Chords and keys: one-bar markers that hold until the next one.
    pub fn is_marker(self) -> bool {
        matches!(self, Layer::Chords | Layer::Keys)
    }

    pub fn name(self) -> &'static str {
        match self {
            Layer::Marks => "marks",
            Layer::Chords => "chords",
            Layer::Keys => "keys",
            Layer::Feels => "feels",
        }
    }

    /// Checks a region's value for this layer and returns it in canonical
    /// form ("e PHRYGIAN" → "E phrygian", "Rise" → "rise").
    fn check_value(self, v: &str) -> Result<String, String> {
        let v = v.trim();
        match self {
            Layer::Marks => one_of(v, &MARKS, "a mark"),
            Layer::Feels => one_of(v, &FEELS, "a feel"),
            Layer::Chords => parse_chord(v).map(|c| c.to_string()),
            Layer::Keys => {
                let mut parts = v.split_whitespace();
                let (Some(root), Some(scale), None) = (parts.next(), parts.next(), parts.next()) else {
                    return Err(format!("key {v:?}: expected ROOT SCALE, e.g. \"F# phrygian\""));
                };
                let root = Tonality::parse_root(root).ok_or_else(|| format!("key {v:?}: unknown root {root}"))?;
                let scale = Scale::parse(scale).ok_or_else(|| format!("key {v:?}: unknown scale {scale}"))?;
                let t = Tonality { root, scale };
                Ok(format!("{} {}", t.root_name(), t.scale.name()))
            }
        }
    }
}

/// A stored key region's value, already canonical ("F# phrygian").
fn parse_key(v: &str) -> Option<Tonality> {
    let (root, scale) = v.split_once(' ')?;
    Some(Tonality { root: Tonality::parse_root(root)?, scale: Scale::parse(scale)? })
}

fn one_of(v: &str, allowed: &[&str], what: &str) -> Result<String, String> {
    allowed
        .iter()
        .find(|a| a.eq_ignore_ascii_case(v))
        .map(|a| a.to_string())
        .ok_or_else(|| format!("{v:?} is not {what}; one of {}", allowed.join(", ")))
}

/// A chord as a scale degree: `[b|#]<1-7>[quality]`, e.g. `1`, `3m`,
/// `5dim`, `b7M7`. No quality means the diatonic triad on that degree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    /// -1 flat, 0, +1 sharp.
    pub shift: i8,
    /// 1..=7
    pub degree: u8,
    /// Index into [`QUALITIES`].
    pub quality: u8,
}

impl std::fmt::Display for Chord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shift = match self.shift {
            -1 => "b",
            1 => "#",
            _ => "",
        };
        write!(f, "{shift}{}{}", self.degree, QUALITIES[self.quality as usize])
    }
}

impl Chord {
    /// The chord's notes in `key`: the root is the scale degree moved by
    /// the shift, and a chord without a quality stacks the scale's thirds
    /// from the (unshifted) degree.
    pub fn resolve(&self, key: Tonality) -> gen_api::Chord {
        let d = self.degree as i32 - 1;
        let root = (key.root as i32 + key.degree(d) + self.shift as i32).rem_euclid(12);
        let intervals: Vec<i32> = match QUALITIES[self.quality as usize] {
            "" => [0, 2, 4].iter().map(|k| key.degree(d + k) - key.degree(d)).collect(),
            "M" => vec![0, 4, 7],
            "m" => vec![0, 3, 7],
            "dim" => vec![0, 3, 6],
            "aug" => vec![0, 4, 8],
            "sus2" => vec![0, 2, 7],
            "sus4" => vec![0, 5, 7],
            "7" => vec![0, 4, 7, 10],
            "M7" => vec![0, 4, 7, 11],
            "m7" => vec![0, 3, 7, 10],
            "m7b5" => vec![0, 3, 6, 10],
            _ => vec![0, 3, 6, 9], // dim7
        };
        let tones = intervals.iter().fold(0u16, |t, i| t | 1 << (root + i).rem_euclid(12));
        gen_api::Chord { root: root as u8, tones }
    }
}

pub fn parse_chord(s: &str) -> Result<Chord, String> {
    let bad = || format!("chord {s:?}: expected [b|#]DEGREE[QUALITY], DEGREE 1-7, QUALITY one of {}", QUALITIES[1..].join(" "));
    let (shift, rest) = match s.as_bytes().first() {
        Some(b'b') => (-1, &s[1..]),
        Some(b'#') => (1, &s[1..]),
        _ => (0, s),
    };
    let mut chars = rest.chars();
    let degree = chars.next().and_then(|c| c.to_digit(10)).filter(|d| (1..=7).contains(d)).ok_or_else(bad)? as u8;
    let q = chars.as_str();
    // Case matters for M (major) vs m (minor); the rest are matched as typed.
    let quality = QUALITIES.iter().position(|&x| x == q).ok_or_else(bad)? as u8;
    Ok(Chord { shift, degree, quality })
}

/// A span of whole bars in a bank. `start` counts from 1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Region {
    pub start: u8,
    /// Always 1 for markers (chords, keys); may be left out for them.
    #[serde(default = "one_bar")]
    pub bars: u8,
    pub value: String,
    /// Marks only: 0..1, absent means 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intensity: Option<f64>,
}

fn one_bar() -> u8 {
    1
}

/// One bank: regions per layer. Layers without regions are left out.
pub type Bank = BTreeMap<Layer, Vec<Region>>;

/// Validates a bank for `lane`: only that lane's layers, regions inside the
/// 16 bars and not overlapping within a layer. Returns it with values in
/// canonical form and regions sorted.
pub fn check_bank(lane: Lane, mut bank: Bank) -> Result<Bank, String> {
    if let Some(layer) = bank.keys().find(|l| !lane.layers().contains(l)) {
        let names: Vec<_> = lane.layers().iter().map(|l| l.name()).collect();
        return Err(format!("{} has no layer {}; it has {}", lane.name(), layer.name(), names.join(", ")));
    }
    bank.retain(|_, regions| !regions.is_empty());
    for (layer, regions) in bank.iter_mut() {
        for r in regions.iter_mut() {
            // A marker covers its bar only (also turns spans saved before
            // chords and keys became markers into markers at their start).
            if layer.is_marker() {
                r.bars = 1;
            }
            if r.bars == 0 || r.start == 0 || r.start as u32 + r.bars as u32 - 1 > BARS as u32 {
                return Err(format!("{} region at bar {} for {} bars: regions lie within bars 1-{BARS}", layer.name(), r.start, r.bars));
            }
            r.value = layer.check_value(&r.value)?;
            match (layer, r.intensity) {
                (Layer::Marks, Some(i)) if !(0.0..=1.0).contains(&i) => return Err(format!("intensity {i}: 0..1")),
                (Layer::Marks, _) | (_, None) => {}
                (_, Some(_)) => return Err(format!("only marks have an intensity, not {}", layer.name())),
            }
        }
        regions.sort_by_key(|r| r.start);
        for w in regions.windows(2) {
            if w[0].start + w[0].bars > w[1].start {
                return Err(format!("{} regions at bars {} and {} overlap", layer.name(), w[0].start, w[1].start));
            }
        }
    }
    Ok(bank)
}

pub fn parse_bank(s: &str) -> Option<usize> {
    BANK_NAMES.iter().position(|b| b.eq_ignore_ascii_case(s))
}

pub fn bank_name(i: usize) -> &'static str {
    BANK_NAMES[i]
}

#[derive(Clone, Default)]
struct LaneState {
    banks: [Bank; BANKS],
    playing: usize,
    cued: Option<usize>,
    /// The last switch: the bank that played before and the beat the new
    /// one took over. The mixer switches at its render head, ahead of what
    /// is heard, so until the audible beat reaches it, views still show
    /// the old bank playing and the new one cued.
    switched: Option<(usize, f64)>,
}

/// What's kept on disk: every lane's banks.
type Saved = BTreeMap<Lane, BTreeMap<String, Bank>>;

#[derive(Default)]
pub struct Sequencer {
    lanes: [LaneState; 3],
    /// The epoch of the last rendered block, to notice crossing into the
    /// next one.
    rendered_epoch: Option<i64>,
    /// The timeline plugins get, rebuilt when `dirty` or the host key
    /// changes.
    score: Arc<gen_api::Score>,
    score_key: Option<Tonality>,
    dirty: bool,
}

impl Sequencer {
    /// Loads bank contents from `path`. A missing file is an empty
    /// sequencer; an unreadable one, or one that doesn't validate, is
    /// logged and moved aside, never silently overwritten.
    pub fn load(path: &Path) -> Sequencer {
        let mut s = Sequencer::default();
        let Ok(text) = std::fs::read_to_string(path) else { return s };
        let loaded = serde_json::from_str::<Saved>(&text).map_err(|e| e.to_string()).and_then(|saved| {
            for (lane, banks) in saved {
                for (name, bank) in banks {
                    let i = parse_bank(&name).ok_or_else(|| format!("{}: no bank {name}", lane.name()))?;
                    s.lanes[lane.index()].banks[i] = check_bank(lane, bank)?;
                }
            }
            Ok(())
        });
        if let Err(e) = loaded {
            let aside = path.with_extension("json.broken");
            let _ = std::fs::rename(path, &aside);
            wlog!("sequencer: {} is not valid ({e}); moved to {}", path.display(), aside.display());
            return Sequencer::default();
        }
        s
    }

    /// The bank contents, to save with [`save`]. Cheap enough to clone on
    /// every edit (a few dozen regions at most).
    pub fn snapshot(&self) -> Snapshot {
        let mut saved = Saved::new();
        for lane in Lane::ALL {
            let banks: BTreeMap<_, _> = self.lanes[lane.index()]
                .banks
                .iter()
                .enumerate()
                .filter(|(_, b)| !b.is_empty())
                .map(|(i, b)| (bank_name(i).to_owned(), b.clone()))
                .collect();
            if !banks.is_empty() {
                saved.insert(lane, banks);
            }
        }
        Snapshot(saved)
    }

    pub fn set_bank(&mut self, lane: Lane, bank: usize, content: Bank) {
        self.lanes[lane.index()].banks[bank] = content;
        self.dirty = true;
    }

    /// Copies bank `from` onto `to` in each of `lanes`.
    pub fn copy(&mut self, from: usize, to: usize, lanes: &[Lane]) {
        for &lane in lanes {
            let l = &mut self.lanes[lane.index()];
            l.banks[to] = l.banks[from].clone();
        }
        self.dirty = true;
    }

    /// Repeats the first `16 / times` bars of a bank over all 16: `times`
    /// 2 copies the first half onto the second, 4 the first quarter, 8 the
    /// first eighth. Regions reaching past the repeated part are cut at its
    /// end.
    pub fn repeat(&mut self, lane: Lane, bank: usize, times: u8) {
        let span = BARS / times;
        let content = &mut self.lanes[lane.index()].banks[bank];
        for regions in content.values_mut() {
            let first: Vec<Region> = regions
                .iter()
                .filter(|r| r.start <= span)
                .map(|r| Region { bars: r.bars.min(span - r.start + 1), ..r.clone() })
                .collect();
            *regions = (0..times).flat_map(|k| first.iter().map(move |r| Region { start: r.start + k * span, ..r.clone() })).collect();
        }
        content.retain(|_, regions| !regions.is_empty());
        self.dirty = true;
    }

    /// Cues `bank` to play from the next epoch; cueing the playing bank
    /// cancels the cue.
    pub fn cue(&mut self, lane: Lane, bank: usize) {
        let l = &mut self.lanes[lane.index()];
        l.cued = (bank != l.playing).then_some(bank);
        self.dirty = true;
    }

    /// Called by the mixer for every block it renders: on entering a new
    /// epoch (playing on, or a seek), cued banks take over.
    pub fn advance(&mut self, beat: f64, beats_per_bar: u32) {
        let epoch_beats = (beats_per_bar * BARS as u32) as f64;
        let epoch = (beat / epoch_beats).floor() as i64;
        if self.rendered_epoch != Some(epoch) {
            self.dirty = true;
        }
        if self.rendered_epoch.is_some_and(|e| e != epoch) {
            for l in &mut self.lanes {
                if let Some(next) = l.cued.take() {
                    l.switched = Some((l.playing, epoch as f64 * epoch_beats));
                    l.playing = next;
                }
            }
        }
        self.rendered_epoch = Some(epoch);
    }

    /// The score for plugins: 32 bars from the start of the rendered
    /// epoch, the playing banks and then the next epoch's (the cued ones,
    /// or the same again). Chords are resolved in the key at their bar,
    /// which is the bank's key region or else `host_key`. Rebuilt only when
    /// something changed, with a new revision.
    pub fn timeline(&mut self, host_key: Tonality) -> Arc<gen_api::Score> {
        if !self.dirty && self.score_key == Some(host_key) {
            return self.score.clone();
        }
        let epoch = self.rendered_epoch.unwrap_or(0).max(0) as u64;
        let first_bar = epoch * BARS as u64;
        let mut score = gen_api::Score { revision: self.score.revision.wrapping_add(1), first_bar, ..gen_api::Score::EMPTY };
        for (i, bar) in score.bars.iter_mut().enumerate() {
            bar.mark_start = first_bar + i as u64;
        }
        for half in 0..2 {
            let offset = half * BARS as usize;
            let bank = |lane: Lane| {
                let l = &self.lanes[lane.index()];
                &l.banks[if half == 0 { l.playing } else { l.cued.unwrap_or(l.playing) }]
            };
            let regions = |lane, layer| bank(lane).get(&layer).map(Vec::as_slice).unwrap_or(&[]);
            let bars = &mut score.bars[offset..offset + BARS as usize];
            for r in regions(Lane::Dynamics, Layer::Marks) {
                let mark = match r.value.as_str() {
                    "quiet" => gen_api::Mark::Quiet,
                    "rise" => gen_api::Mark::Rise,
                    "peak" => gen_api::Mark::Peak,
                    _ => gen_api::Mark::Break,
                };
                for b in &mut bars[r.start as usize - 1..][..r.bars as usize] {
                    b.mark = mark;
                    b.intensity = r.intensity.unwrap_or(1.0) as f32;
                    b.mark_start = first_bar + (offset + r.start as usize - 1) as u64;
                    b.mark_bars = r.bars as u32;
                }
            }
            for r in regions(Lane::Tempo, Layer::Feels) {
                let feel = match r.value.as_str() {
                    "half" => gen_api::Feel::Half,
                    "double" => gen_api::Feel::Double,
                    "quad" => gen_api::Feel::Quad,
                    _ => gen_api::Feel::Triplet,
                };
                bars[r.start as usize - 1..][..r.bars as usize].iter_mut().for_each(|b| b.feel = feel);
            }
        }
        // Chords and keys are markers that hold until the next one. The
        // current epoch starts with whatever its bank's last marker left
        // (it loops), and the next epoch carries on from the current one.
        let harmony = &self.lanes[Lane::Harmony.index()];
        let banks = [&harmony.banks[harmony.playing], &harmony.banks[harmony.cued.unwrap_or(harmony.playing)]];
        let markers = |layer: Layer, i: usize| -> Option<&Region> {
            let bank = banks[i / BARS as usize];
            bank.get(&layer)?.iter().find(|r| r.start as usize - 1 == i % BARS as usize)
        };
        let last = |layer: Layer| banks[0].get(&layer).and_then(|r| r.last());
        let mut key = last(Layer::Keys).and_then(|r| parse_key(&r.value));
        let mut chord = last(Layer::Chords).and_then(|r| parse_chord(&r.value).ok());
        for (i, bar) in score.bars.iter_mut().enumerate() {
            if let Some(r) = markers(Layer::Keys, i) {
                key = parse_key(&r.value);
            }
            if let Some(r) = markers(Layer::Chords, i) {
                chord = parse_chord(&r.value).ok();
            }
            bar.key = key;
            bar.chord = chord.map(|c| c.resolve(key.unwrap_or(host_key)));
        }
        self.score = Arc::new(score);
        self.score_key = Some(host_key);
        self.dirty = false;
        self.score.clone()
    }

    /// The API view, as heard at `audible_beat`.
    pub fn view(&self, audible_beat: f64) -> View {
        let lanes = Lane::ALL
            .iter()
            .map(|&lane| {
                let l = &self.lanes[lane.index()];
                let (playing, cued) = match l.switched {
                    Some((before, at)) if audible_beat < at => (before, l.cued.or(Some(l.playing))),
                    _ => (l.playing, l.cued),
                };
                LaneView {
                    lane,
                    layers: lane.layers().to_vec(),
                    playing: bank_name(playing),
                    cued: cued.map(bank_name),
                    banks: (0..BANKS).map(|i| (bank_name(i), l.banks[i].clone())).collect(),
                }
            })
            .collect();
        View { bars: BARS, lanes }
    }
}

pub struct Snapshot(Saved);

impl Snapshot {
    /// Writes a temp file and renames it over the old one, so a crash
    /// can't leave a half-written file.
    pub fn save(&self, path: &PathBuf) -> Result<(), String> {
        let dir = path.parent().ok_or("sequencer path has no directory")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&self.0).map_err(|e| e.to_string())? + "\n";
        std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[derive(Serialize)]
pub struct View {
    /// Bars per bank.
    pub bars: u8,
    pub lanes: Vec<LaneView>,
}

#[derive(Serialize)]
pub struct LaneView {
    pub lane: Lane,
    pub layers: Vec<Layer>,
    pub playing: &'static str,
    pub cued: Option<&'static str>,
    pub banks: BTreeMap<&'static str, Bank>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(start: u8, bars: u8, value: &str) -> Region {
        Region { start, bars, value: value.into(), intensity: None }
    }

    #[test]
    fn chords_parse_and_print_canonically() {
        for s in ["1", "3m", "5dim", "b7M7", "#4", "2sus4", "6m7b5"] {
            assert_eq!(parse_chord(s).unwrap().to_string(), s);
        }
        for s in ["", "0", "8", "3x", "bb3", "m3", "3 m"] {
            assert!(parse_chord(s).is_err(), "{s}");
        }
        assert_ne!(parse_chord("3M").unwrap(), parse_chord("3m").unwrap());
    }

    #[test]
    fn banks_are_checked_and_canonical() {
        let mut b = Bank::new();
        b.insert(Layer::Keys, vec![region(9, 8, "f# PHRYGIAN")]);
        b.insert(Layer::Chords, vec![region(5, 4, "6"), region(1, 4, "1")]);
        let b = check_bank(Lane::Harmony, b).unwrap();
        assert_eq!(b[&Layer::Keys][0].value, "F# phrygian");
        assert_eq!(b[&Layer::Chords][0].start, 1);

        let bad = |layer, regions| check_bank(Lane::Harmony, Bank::from([(layer, regions)])).is_err();
        assert!(bad(Layer::Chords, vec![region(4, 1, "1"), region(4, 1, "5")]), "two chords on one bar");
        assert!(bad(Layer::Chords, vec![region(17, 1, "1")]), "past bar 16");
        let mark = |regions| check_bank(Lane::Dynamics, Bank::from([(Layer::Marks, regions)]));
        assert!(mark(vec![region(1, 4, "rise"), region(4, 2, "peak")]).is_err(), "overlapping marks");
        assert!(mark(vec![region(15, 3, "rise")]).is_err(), "a mark past bar 16");
        let held = check_bank(Lane::Harmony, Bank::from([(Layer::Chords, vec![region(5, 4, "1")])])).unwrap();
        assert_eq!(held[&Layer::Chords][0].bars, 1, "a chord span becomes a marker at its start");
        assert!(bad(Layer::Marks, vec![region(1, 4, "rise")]), "not a harmony layer");
        assert!(bad(Layer::Marks, vec![]), "not a harmony layer, even empty");
        let mut loud = region(1, 1, "1");
        loud.intensity = Some(0.5);
        assert!(bad(Layer::Chords, vec![loud]), "intensity on a chord");
    }

    #[test]
    fn repeat_copies_the_first_part_over_the_rest() {
        let mut s = Sequencer::default();
        let bank = Bank::from([(Layer::Chords, vec![region(1, 2, "1"), region(3, 3, "6"), region(9, 4, "5")])]);
        s.set_bank(Lane::Harmony, 0, bank);
        s.repeat(Lane::Harmony, 0, 4);
        let starts: Vec<_> = s.lanes[1].banks[0][&Layer::Chords].iter().map(|r| (r.start, r.bars, r.value.as_str())).collect();
        assert_eq!(starts, [(1, 2, "1"), (3, 2, "6"), (5, 2, "1"), (7, 2, "6"), (9, 2, "1"), (11, 2, "6"), (13, 2, "1"), (15, 2, "6")]);
        assert!(check_bank(Lane::Harmony, s.lanes[1].banks[0].clone()).is_ok());
        s.set_bank(Lane::Harmony, 1, Bank::from([(Layer::Chords, vec![region(5, 2, "1")])]));
        s.repeat(Lane::Harmony, 1, 4);
        assert!(s.lanes[1].banks[1].is_empty(), "nothing in the first quarter: empty");
    }

    #[test]
    fn chords_resolve_in_the_key() {
        let e_phr = Tonality { root: 4, scale: Scale::Phrygian };
        let pcs = |c: &str| {
            let r = parse_chord(c).unwrap().resolve(e_phr);
            (r.root, (0..12).filter(|&p| r.contains(p)).collect::<Vec<_>>())
        };
        assert_eq!(pcs("1"), (4, vec![4, 7, 11])); // E G B
        assert_eq!(pcs("2"), (5, vec![0, 5, 9])); // F A C
        assert_eq!(pcs("5"), (11, vec![2, 5, 11])); // B D F, diminished
        assert_eq!(pcs("5dim"), pcs("5"));
        assert_eq!(pcs("3m"), (7, vec![2, 7, 10])); // G Bb D
        assert_eq!(pcs("b7"), (1, vec![1, 4, 8])); // D minor (D F A) moved down: C# E G#
    }

    #[test]
    fn chords_and_keys_hold_until_the_next_one_around_the_loop() {
        let mut s = Sequencer::default();
        let key = Tonality { root: 4, scale: Scale::Phrygian }; // E F G A B C D
        s.set_bank(Lane::Harmony, 0, Bank::from([
            (Layer::Chords, vec![region(5, 1, "6"), region(13, 1, "4")]),
            (Layer::Keys, vec![region(1, 1, "E phrygian"), region(9, 1, "A phrygian")]),
        ]));
        s.advance(0.0, 4);
        let t = s.timeline(key);
        let root = |bar| t.bar(bar).chord.map(|c| c.root);
        assert_eq!(root(0), Some(9), "before the first chord: the last one, from the loop (4 of E phrygian = A)");
        assert_eq!(t.bar(8).key.unwrap().root, 9);
        assert_eq!(t.bar(16).key.unwrap().root, 4, "the next epoch's bar 1 key marker");
        assert_eq!(root(4), Some(0), "6 = C in E phrygian");
        assert_eq!(root(7), Some(0), "held");
        assert_eq!(root(9), Some(5), "the key changed at bar 9: 6 of A phrygian is F");
        assert_eq!(root(12), Some(2), "4 of A phrygian is D");
        assert_eq!(root(19), Some(9), "into the next epoch until its first chord, now back in E");
        assert_eq!(root(20), Some(0));

        // Keys loop too: with only a key at bar 9, it also holds bars 1-8.
        s.set_bank(Lane::Harmony, 0, Bank::from([(Layer::Keys, vec![region(9, 1, "A phrygian")])]));
        assert_eq!(s.timeline(key).bar(0).key.unwrap().root, 9);
    }

    #[test]
    fn timeline_has_the_playing_bank_then_the_cued_one() {
        let mut s = Sequencer::default();
        let key = Tonality { root: 4, scale: Scale::Phrygian };
        let chords = |c: &str| Bank::from([(Layer::Chords, vec![region(1, 16, c)])]);
        s.set_bank(Lane::Harmony, 0, chords("1"));
        s.set_bank(Lane::Harmony, 1, chords("6"));
        s.set_bank(Lane::Dynamics, 0, Bank::from([(Layer::Marks, vec![region(9, 8, "rise")])]));
        s.advance(64.0 * 3.0 + 1.0, 4); // epoch 3
        let t = s.timeline(key);
        assert_eq!(t.first_bar, 48);
        assert_eq!(t.bar(48).chord.unwrap().root, 4);
        assert_eq!(t.bar(64).chord.unwrap().root, 4, "nothing cued: the same bank again");
        assert_eq!(t.bar(56).mark, gen_api::Mark::Rise);
        assert_eq!((t.bar(60).mark_start, t.bar(60).mark_bars), (56, 8));
        assert_eq!(t.progress(60.0 * 4.0, 4), 0.5);

        let rev = t.revision;
        assert_eq!(s.timeline(key).revision, rev, "unchanged: not rebuilt");
        s.cue(Lane::Harmony, 1);
        let t = s.timeline(key);
        assert!(t.revision != rev);
        assert_eq!(t.bar(64).chord.unwrap().root, 0, "the cued bank next: 6 is C in E phrygian");
        assert_eq!(t.bar(63).chord.unwrap().root, 4);
    }

    #[test]
    fn cues_land_on_the_next_epoch_and_show_when_heard() {
        let mut s = Sequencer::default();
        s.advance(10.0, 4);
        s.cue(Lane::Harmony, 2);
        s.advance(63.9, 4);
        assert_eq!(s.view(63.9).lanes[1].playing, "A");
        s.advance(64.0, 4); // epoch 2 renders
        let v = s.view(60.0); // still hearing epoch 1
        assert_eq!((v.lanes[1].playing, v.lanes[1].cued), ("A", Some("C")));
        let v = s.view(64.5);
        assert_eq!((v.lanes[1].playing, v.lanes[1].cued), ("C", None));
        assert_eq!(v.lanes[0].playing, "A", "other lanes stay");

        s.cue(Lane::Harmony, 0);
        s.cue(Lane::Harmony, 2); // cueing the playing bank cancels
        assert_eq!(s.view(70.0).lanes[1].cued, None);
    }
}
