//! The workspace's operations, shared by the `ws` CLI (unix socket) and the
//! HTTP API (see `workspace/openapi.yaml`, which this module implements).
//!
//! Every operation runs on the mixer's state as a closure between blocks, so
//! there are no locks around the mixer.

use crate::catalog::Catalog;
use crate::daemon::{Mixer, BLOCK};
use crate::gen::{ParamDesc, Proc, Slot, Spec};
use gen_api::{Scale, Tonality};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;

pub type Job = Box<dyn FnOnce(&mut Mixer) + Send>;

/// Errors carry their HTTP meaning.
#[derive(Debug)]
pub enum ApiError {
    /// 400: malformed request.
    BadRequest(String),
    /// 404
    NotFound(String),
    /// 409: e.g. the name is taken.
    Conflict(String),
    /// 422: well-formed but unusable, e.g. a command that won't start.
    Unprocessable(String),
    /// 503: the mixer has stopped.
    Unavailable,
}

impl ApiError {
    pub fn status(&self) -> u16 {
        match self {
            ApiError::BadRequest(_) => 400,
            ApiError::NotFound(_) => 404,
            ApiError::Conflict(_) => 409,
            ApiError::Unprocessable(_) => 422,
            ApiError::Unavailable => 503,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::BadRequest(m) | ApiError::NotFound(m) | ApiError::Conflict(m) | ApiError::Unprocessable(m) => f.write_str(m),
            ApiError::Unavailable => f.write_str("the workspace is stopping"),
        }
    }
}

type Result<T> = std::result::Result<T, ApiError>;

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::BadRequest(msg.into())
}

// ---------------------------------------------------------------- views

#[derive(Serialize)]
pub struct State {
    pub playing: bool,
    pub position: PositionView,
    /// Audible position in quarter notes since 0.
    pub beat: f64,
    pub bpm: f64,
    pub root: String,
    pub scale: String,
    pub click: bool,
    pub click_db: f64,
    pub master_db: f64,
    pub master_peak: f32,
    /// The shared duck envelope as heard now (its peak within the playing
    /// block), 0..1.
    pub duck: f32,
    pub ahead_ms: f64,
    pub underruns: u64,
    pub sample_rate: u32,
    pub generators: Vec<Generator>,
    pub effects: Vec<Effect>,
}

#[derive(Serialize)]
pub struct PositionView {
    pub epoch: u64,
    pub cycle: u32,
    pub bar: u32,
    pub beat: u32,
    pub tick: u32,
    pub absolute_bar: u64,
    pub text: String,
}

#[derive(Serialize)]
pub struct Generator {
    pub name: String,
    pub command: Vec<String>,
    /// From the generator's handshake; empty while it isn't running.
    pub description: String,
    pub channels: Option<usize>,
    /// Input channels (effects); null while not running.
    pub inputs: Option<usize>,
    /// The running generator's parameters with their current values;
    /// empty while it isn't running.
    pub params: Vec<Param>,
    pub gain_db: f64,
    pub muted: bool,
    pub state: String,
    pub late: u64,
    pub restarts: u64,
    pub peak: f32,
    pub last_error: Option<LastError>,
    /// How much the duck envelope turns this channel down, 0..1, as the
    /// generator declares it; null while not running.
    pub duck_sensitivity: Option<f64>,
    /// Insert effects, by name, in signal order.
    pub inserts: Vec<String>,
    /// Post-fader send amounts (linear 0..1) to send effects, by name.
    /// Absent means 0.
    pub sends: BTreeMap<String, f64>,
}

#[derive(Serialize)]
pub struct Effect {
    pub name: String,
    pub command: Vec<String>,
    pub description: String,
    /// "send": its own bus, fed by every generator's send to it and
    /// returned through its fader. "insert": in `owner`'s chain.
    pub role: &'static str,
    pub owner: Option<String>,
    pub channels: Option<usize>,
    pub inputs: Option<usize>,
    pub params: Vec<Param>,
    /// Send effects: return level. Insert effects: unused.
    pub gain_db: f64,
    pub muted: bool,
    /// Insert effects: passing input straight through.
    pub bypass: bool,
    pub state: String,
    pub late: u64,
    pub restarts: u64,
    pub peak: f32,
    pub last_error: Option<LastError>,
    /// Send effects: how much the duck envelope turns the return down.
    pub duck_sensitivity: Option<f64>,
}

fn params_of(g: &Slot) -> Vec<Param> {
    g.params().unwrap_or_default().iter().map(|d| Param { desc: d.clone(), value: g.value_of(d) }).collect()
}

fn last_error_of(g: &Slot) -> Option<LastError> {
    g.last_error.as_ref().map(|(m, at)| LastError { message: m.clone(), seconds_ago: at.elapsed().as_secs_f64() })
}

impl Effect {
    pub fn of(f: &Slot) -> Effect {
        Effect {
            name: f.name.clone(),
            command: f.spec.argv.clone(),
            description: f.description().unwrap_or_default().to_owned(),
            role: if f.owner.is_some() { "insert" } else { "send" },
            owner: f.owner.clone(),
            channels: f.channels(),
            inputs: f.inputs(),
            params: params_of(f),
            gain_db: f.gain_db,
            muted: f.muted,
            bypass: f.bypass,
            state: f.state_label(),
            late: f.late,
            restarts: f.restarts,
            peak: f.peak,
            last_error: last_error_of(f),
            duck_sensitivity: f.duck_sensitivity(),
        }
    }
}

#[derive(Serialize)]
pub struct Param {
    #[serde(flatten)]
    pub desc: ParamDesc,
    pub value: f64,
}

#[derive(Serialize)]
pub struct LastError {
    pub message: String,
    pub seconds_ago: f64,
}

impl Generator {
    pub fn of(g: &Slot) -> Generator {
        Generator {
            name: g.name.clone(),
            command: g.spec.argv.clone(),
            description: g.description().unwrap_or_default().to_owned(),
            channels: g.channels(),
            inputs: g.inputs(),
            params: params_of(g),
            gain_db: g.gain_db,
            muted: g.muted,
            state: g.state_label(),
            late: g.late,
            restarts: g.restarts,
            peak: g.peak,
            last_error: last_error_of(g),
            duck_sensitivity: g.duck_sensitivity(),
            inserts: g.inserts.clone(),
            sends: g.sends.clone(),
        }
    }
}

// ---------------------------------------------------------------- inputs

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TransportPatch {
    pub playing: Option<bool>,
    pub bpm: Option<f64>,
    pub root: Option<String>,
    pub scale: Option<String>,
    pub click: Option<bool>,
    pub click_db: Option<f64>,
    pub master_db: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seek {
    /// From 1.
    pub bar: u64,
}

/// Exactly one of `generator` and `command`.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NewGenerator {
    /// Left out: derived from the generator or command, with `-2`, `-3`, …
    /// appended if taken.
    pub name: Option<String>,
    /// A generator from the catalog, by name.
    pub generator: Option<String>,
    /// Any pipe-protocol program and its arguments; `--pipe` is appended.
    pub command: Option<Vec<String>>,
    /// Appended as `--seed N`.
    pub seed: Option<u64>,
    pub gain_db: Option<f64>,
    pub muted: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GeneratorPatch {
    pub gain_db: Option<f64>,
    pub muted: Option<bool>,
    /// Send amounts by send-effect name, linear 0..1; 0 or null removes.
    pub sends: Option<BTreeMap<String, Option<f64>>>,
}

/// Exactly one of `effect` and `command`.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NewEffect {
    pub name: Option<String>,
    /// An effect from the catalog, by name.
    pub effect: Option<String>,
    pub command: Option<Vec<String>>,
    pub seed: Option<u64>,
    /// Initial parameter values by id.
    pub params: Option<BTreeMap<String, f64>>,
    /// Send effects: return level.
    pub gain_db: Option<f64>,
    pub muted: Option<bool>,
    /// Inserts: index in the chain, 0 = first; default last.
    pub position: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EffectPatch {
    pub gain_db: Option<f64>,
    pub muted: Option<bool>,
    /// Inserts only.
    pub bypass: Option<bool>,
    /// Inserts only: move to this index in the chain.
    pub position: Option<usize>,
}

// ---------------------------------------------------------------- validation

fn check_bpm(bpm: f64) -> Result<f64> {
    if (20.0..=300.0).contains(&bpm) {
        Ok(bpm)
    } else {
        Err(bad("bpm must be 20-300"))
    }
}

fn check_db(db: f64) -> Result<f64> {
    if (-96.0..=24.0).contains(&db) {
        Ok(db)
    } else {
        Err(bad("gain must be between -96 and +24 dB"))
    }
}

fn parse_root(s: &str) -> Result<u8> {
    Tonality::parse_root(s).ok_or_else(|| bad(format!("{s}: expected a note name like F# or Bb")))
}

fn parse_scale(s: &str) -> Result<Scale> {
    Scale::parse(s).ok_or_else(|| bad(format!("unknown scale {s}; one of {}", scale_names())))
}

pub fn scale_names() -> String {
    Scale::ALL.iter().map(|s| s.name()).collect::<Vec<_>>().join(", ")
}

/// Names are used in URLs and log prefixes.
fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && name != "master"
        && name != "click";
    if ok {
        Ok(())
    } else {
        Err(bad(format!("{name:?}: names are 1-32 of A-Z a-z 0-9 - _ . (and not master or click)")))
    }
}

/// `base`, or `base-2`, `base-3`, … whichever is free first.
fn free_name(m: &Mixer, base: &str) -> String {
    // Generators and effects share one namespace (log lines are tagged by
    // name).
    let taken = |n: &str| m.gens.iter().chain(&m.fx).any(|g| g.name == n);
    if !taken(base) {
        return base.to_owned();
    }
    (2..).map(|i| format!("{base}-{i}")).find(|n| !taken(n)).unwrap()
}

/// A valid name derived from a program path: `target/release/psybass` →
/// `psybass`.
fn base_name(program: &str) -> String {
    let file = Path::new(program).file_stem().and_then(|s| s.to_str()).unwrap_or("gen");
    let clean: String = file.chars().filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).take(24).collect();
    if clean.is_empty() || clean == "master" || clean == "click" {
        "gen".into()
    } else {
        clean
    }
}

fn find<'a>(m: &'a mut Mixer, name: &str) -> Result<&'a mut Slot> {
    m.gens.iter_mut().find(|g| g.name == name).ok_or_else(|| ApiError::NotFound(format!("no generator named {name}")))
}

fn find_fx<'a>(m: &'a mut Mixer, name: &str) -> Result<&'a mut Slot> {
    m.fx.iter_mut().find(|g| g.name == name).ok_or_else(|| ApiError::NotFound(format!("no effect named {name}")))
}

fn taken(m: &Mixer, name: &str) -> bool {
    m.gens.iter().chain(&m.fx).any(|g| g.name == name)
}

/// Checks param ids against the running proc (if it is running) and stores
/// the values.
fn apply_params(g: &mut Slot, values: BTreeMap<String, f64>) -> Result<()> {
    if let Some((id, _)) = values.iter().find(|(_, v)| !v.is_finite()) {
        return Err(bad(format!("{id}: not a number")));
    }
    if let Some(params) = g.params() {
        if let Some(id) = values.keys().find(|id| !params.iter().any(|d| &d.id == *id)) {
            let known: Vec<&str> = params.iter().map(|d| d.id.as_str()).collect();
            return Err(ApiError::NotFound(format!("{} has no parameter {id}; it has: {}", g.name, known.join(", "))));
        }
    }
    g.values.extend(values);
    Ok(())
}

/// Removes effect slots and every reference to them.
fn drop_effects(m: &mut Mixer, names: &[String]) {
    m.fx.retain(|f| !names.contains(&f.name));
    for g in &mut m.gens {
        g.inserts.retain(|n| !names.contains(n));
        g.sends.retain(|n, _| !names.contains(n));
    }
}

// ---------------------------------------------------------------- operations

/// A handle for running operations on the mixer, cheap to clone.
#[derive(Clone)]
pub struct Ctl {
    pub jobs: Sender<Job>,
    pub sample_rate: u32,
    /// The HTTP API's address, if it's being served.
    pub http_url: Option<String>,
    pub catalog: Arc<Catalog>,
}

impl Ctl {
    /// Runs `f` on the mixer between blocks and returns its result.
    pub fn on_mixer<T: Send + 'static>(&self, f: impl FnOnce(&mut Mixer) -> T + Send + 'static) -> Result<T> {
        let (tx, rx) = mpsc::channel();
        self.jobs
            .send(Box::new(move |m| {
                let _ = tx.send(f(m));
            }))
            .map_err(|_| ApiError::Unavailable)?;
        rx.recv().map_err(|_| ApiError::Unavailable)
    }

    pub fn state(&self) -> Result<State> {
        self.on_mixer(|m| m.state())
    }

    /// Validates every field first, then applies them together.
    pub fn patch_transport(&self, p: TransportPatch) -> Result<State> {
        let bpm = p.bpm.map(check_bpm).transpose()?;
        let root = p.root.as_deref().map(parse_root).transpose()?;
        let scale = p.scale.as_deref().map(parse_scale).transpose()?;
        let click_db = p.click_db.map(check_db).transpose()?;
        let master_db = p.master_db.map(check_db).transpose()?;
        self.on_mixer(move |m| {
            if let Some(bpm) = bpm {
                m.set_bpm(bpm);
            }
            if let Some(playing) = p.playing {
                m.set_playing(playing);
            }
            if let Some(root) = root {
                m.tonality.root = root;
            }
            if let Some(scale) = scale {
                m.tonality.scale = scale;
            }
            if let Some(on) = p.click {
                m.click_on = on;
            }
            if let Some(db) = click_db {
                m.click_db = db;
            }
            if let Some(db) = master_db {
                m.master_db = db;
            }
            m.state()
        })
    }

    pub fn seek(&self, s: Seek) -> Result<State> {
        if s.bar < 1 {
            return Err(bad("bar counts from 1"));
        }
        self.on_mixer(move |m| {
            m.seek_bar(s.bar - 1);
            m.state()
        })
    }

    pub fn generators(&self) -> Result<Vec<Generator>> {
        self.on_mixer(|m| m.gens.iter().map(Generator::of).collect())
    }

    /// Starts the generator and adds it. Spawning and the handshake happen
    /// on the calling thread, not the mixer, so a slow start doesn't hold up
    /// audio. A first start that fails is reported and nothing is added;
    /// later crashes are restarted.
    pub fn add(&self, g: NewGenerator, cwd: &Path) -> Result<Generator> {
        let gain_db = g.gain_db.map(check_db).transpose()?;
        let (mut slot, explicit) = self.spawn_slot(g.name, g.generator, g.command, g.seed, cwd, "generator")?;
        if slot.inputs() != Some(0) {
            return Err(ApiError::Unprocessable(format!(
                "{} is an effect; add it with POST /api/effects (send) or POST /api/generators/{{name}}/inserts",
                slot.name
            )));
        }
        slot.gain_db = gain_db.unwrap_or(0.0);
        slot.muted = g.muted.unwrap_or(false);
        self.on_mixer(move |m| {
            if taken(m, &slot.name) {
                if explicit {
                    return Err(ApiError::Conflict(format!("{} already exists", slot.name)));
                }
                slot.name = free_name(m, &slot.name);
            }
            let view = Generator::of(&slot);
            m.gens.push(slot);
            Ok(view)
        })?
    }

    /// Resolves the program, picks a name and starts the process. Spawning
    /// and the handshake happen on the calling thread, not the mixer, so a
    /// slow start doesn't hold up audio.
    fn spawn_slot(
        &self,
        name: Option<String>,
        from_catalog: Option<String>,
        command: Option<Vec<String>>,
        seed: Option<u64>,
        cwd: &Path,
        what: &str,
    ) -> Result<(Slot, bool)> {
        let mut argv = match (from_catalog, command) {
            (Some(c), None) => {
                let path = self.catalog.path_of(&c).ok_or_else(|| ApiError::NotFound(format!("no {what} {c} in the catalog")))?;
                vec![path.to_string_lossy().into_owned()]
            }
            (None, Some(cmd)) if cmd.first().is_some_and(|p| !p.is_empty()) => cmd,
            (None, Some(_)) => return Err(bad("command is empty")),
            _ => return Err(bad(format!("give exactly one of {what} and command"))),
        };
        if let Some(seed) = seed {
            argv.extend(["--seed".into(), seed.to_string()]);
        }
        let explicit = name.is_some();
        let name = match name {
            Some(n) => {
                check_name(&n)?;
                n
            }
            None => base_name(&argv[0]),
        };
        // Pick (or check) the name now so log lines carry it from the start;
        // re-checked on insert in case another add got there first.
        let wanted = name.clone();
        let free = self.on_mixer(move |m| free_name(m, &wanted))?;
        if explicit && free != name {
            return Err(ApiError::Conflict(format!("{name} already exists")));
        }
        let spec = Spec { argv, cwd: cwd.to_owned(), sample_rate: self.sample_rate, max_frames: BLOCK as u32 };
        let proc = Proc::spawn(&free, &spec).map_err(ApiError::Unprocessable)?;
        Ok((Slot::new(&free, spec, proc), explicit))
    }

    pub fn effects(&self) -> Result<Vec<Effect>> {
        self.on_mixer(|m| m.fx.iter().map(Effect::of).collect())
    }

    /// Adds an effect: a send effect when `owner` is None, otherwise an
    /// insert in that generator's chain.
    pub fn add_effect(&self, e: NewEffect, owner: Option<String>, cwd: &Path) -> Result<Effect> {
        let gain_db = e.gain_db.map(check_db).transpose()?;
        if let Some(o) = owner.clone() {
            self.on_mixer(move |m| find(m, &o).map(|_| ()))??;
        }
        let (mut slot, explicit) = self.spawn_slot(e.name, e.effect, e.command, e.seed, cwd, "effect")?;
        if slot.inputs() == Some(0) {
            return Err(ApiError::Unprocessable(format!("{} is a generator, not an effect (it takes no input)", slot.name)));
        }
        if let Some(values) = e.params {
            apply_params(&mut slot, values)?;
        }
        slot.gain_db = gain_db.unwrap_or(0.0);
        slot.muted = e.muted.unwrap_or(false);
        slot.owner = owner;
        self.on_mixer(move |m| {
            if taken(m, &slot.name) {
                if explicit {
                    return Err(ApiError::Conflict(format!("{} already exists", slot.name)));
                }
                slot.name = free_name(m, &slot.name);
            }
            if let Some(o) = &slot.owner {
                let g = find(m, o)?;
                let at = e.position.unwrap_or(g.inserts.len()).min(g.inserts.len());
                g.inserts.insert(at, slot.name.clone());
            }
            let view = Effect::of(&slot);
            m.fx.push(slot);
            Ok(view)
        })?
    }

    pub fn patch_effect(&self, name: String, p: EffectPatch) -> Result<Effect> {
        let gain_db = p.gain_db.map(check_db).transpose()?;
        self.on_mixer(move |m| {
            let f = find_fx(m, &name)?;
            let owner = f.owner.clone();
            if owner.is_none() && (p.bypass.is_some() || p.position.is_some()) {
                return Err(bad("bypass and position are for insert effects; mute a send effect or set its gain_db"));
            }
            if let Some(db) = gain_db {
                f.gain_db = db;
            }
            if let Some(muted) = p.muted {
                f.muted = muted;
            }
            if let Some(b) = p.bypass {
                f.bypass = b;
            }
            let view_name = f.name.clone();
            if let (Some(pos), Some(o)) = (p.position, owner) {
                let g = find(m, &o)?;
                g.inserts.retain(|n| *n != view_name);
                let at = pos.min(g.inserts.len());
                g.inserts.insert(at, view_name.clone());
            }
            Ok(Effect::of(find_fx(m, &view_name)?))
        })?
    }

    pub fn set_effect_params(&self, name: String, values: BTreeMap<String, f64>) -> Result<Effect> {
        if values.is_empty() {
            return Err(bad("no parameters given"));
        }
        self.on_mixer(move |m| {
            let f = find_fx(m, &name)?;
            apply_params(f, values)?;
            Ok(Effect::of(f))
        })?
    }

    pub fn remove_effect(&self, name: String) -> Result<()> {
        self.on_mixer(move |m| {
            find_fx(m, &name)?;
            drop_effects(m, &[name]);
            Ok(())
        })?
    }

    pub fn restart_effect(&self, name: String) -> Result<Effect> {
        self.on_mixer(move |m| {
            let f = find_fx(m, &name)?;
            f.restart_now();
            Ok(Effect::of(f))
        })?
    }

    pub fn patch_generator(&self, name: String, p: GeneratorPatch) -> Result<Generator> {
        let gain_db = p.gain_db.map(check_db).transpose()?;
        if let Some((fx, _)) = p.sends.iter().flatten().find(|(_, a)| a.is_some_and(|a| !(0.0..=1.0).contains(&a))) {
            return Err(bad(format!("send to {fx}: amounts are 0..1")));
        }
        self.on_mixer(move |m| {
            if let Some(sends) = &p.sends {
                for fx in sends.keys() {
                    match m.fx.iter().find(|f| &f.name == fx) {
                        None => return Err(ApiError::NotFound(format!("no effect named {fx}"))),
                        Some(f) if f.owner.is_some() => return Err(bad(format!("{fx} is an insert effect; only send effects take sends"))),
                        Some(_) => {}
                    }
                }
            }
            let g = find(m, &name)?;
            for (fx, amount) in p.sends.into_iter().flatten() {
                match amount {
                    Some(a) if a > 0.0 => g.sends.insert(fx, a),
                    _ => g.sends.remove(&fx),
                };
            }
            if let Some(db) = gain_db {
                g.gain_db = db;
            }
            if let Some(muted) = p.muted {
                g.muted = muted;
            }
            Ok(Generator::of(g))
        })?
    }

    /// Sets parameters by id. Values are clamped to each param's range.
    /// While the generator is restarting the ids can't be checked; values
    /// are kept and apply once it's back.
    pub fn set_params(&self, name: String, values: BTreeMap<String, f64>) -> Result<Generator> {
        if values.is_empty() {
            return Err(bad("no parameters given"));
        }
        self.on_mixer(move |m| {
            let g = find(m, &name)?;
            apply_params(g, values)?;
            Ok(Generator::of(g))
        })?
    }

    /// Removes a generator and its insert effects.
    pub fn remove(&self, name: String) -> Result<()> {
        self.on_mixer(move |m| {
            let inserts = find(m, &name)?.inserts.clone();
            drop_effects(m, &inserts);
            m.gens.retain(|g| g.name != name);
            Ok(())
        })?
    }

    fn is_effect(&self, name: &str) -> Result<bool> {
        let n = name.to_owned();
        self.on_mixer(move |m| m.fx.iter().any(|f| f.name == n))
    }

    pub fn restart(&self, name: String) -> Result<Generator> {
        self.on_mixer(move |m| {
            let g = find(m, &name)?;
            g.restart_now();
            Ok(Generator::of(g))
        })?
    }

    pub fn quit(&self) -> Result<()> {
        self.on_mixer(|m| m.quit = true)
    }

    // ------------------------------------------------------------ CLI

    /// Runs a `ws` CLI command; the reply is text for the terminal.
    pub fn command(&self, cwd: &Path, args: &[String]) -> Result<String> {
        let cmd = args[0].as_str();
        let rest = &args[1..];
        let arg = |i: usize, what: &str| rest.get(i).cloned().ok_or_else(|| bad(format!("{cmd}: missing {what}")));
        let num = |s: String, what: &str| s.trim_end_matches("dB").trim_end_matches("db").parse::<f64>().map_err(|_| bad(format!("{s}: not {what}")));
        let transport = |p: TransportPatch| self.patch_transport(p).map(|_| ());
        match cmd {
            "status" => self.on_mixer(|m| m.status()),
            "pos" => self.on_mixer(|m| {
                let s = m.state();
                format!("{}  bar {}  beat {:.3}  {} bpm\n", s.position.text, s.position.absolute_bar + 1, s.beat, s.bpm)
            }),
            "url" => self.http_url.clone().map(|u| format!("{u}\n")).ok_or_else(|| ApiError::NotFound("the web UI is off (started with --no-http)".into())),
            "add" => {
                // ws add [--name NAME] COMMAND [ARGS...]
                let (name, cmd) = match rest {
                    [flag, name, cmd @ ..] if flag == "--name" => (Some(name.clone()), cmd),
                    cmd => (None, cmd),
                };
                if cmd.is_empty() {
                    return Err(bad("usage: ws add [--name NAME] COMMAND [ARGS...]"));
                }
                let g = self.add(NewGenerator { name, command: Some(cmd.to_vec()), ..Default::default() }, cwd)?;
                Ok(format!("added {} ({} ch)\n", g.name, g.channels.unwrap_or(0)))
            }
            "rm" => {
                let name = arg(0, "NAME")?;
                if self.is_effect(&name)? { self.remove_effect(name.clone())? } else { self.remove(name.clone())? }
                Ok(format!("removed {name}\n"))
            }
            "restart" => {
                let name = arg(0, "NAME")?;
                if self.is_effect(&name)? { self.restart_effect(name.clone()).map(|_| ())? } else { self.restart(name.clone()).map(|_| ())? }
                Ok(format!("restarting {name}\n"))
            }
            "effect" => {
                // ws effect [--name NAME] [--insert GENERATOR] COMMAND [ARGS...]
                let mut rest = rest;
                let (mut name, mut owner) = (None, None);
                loop {
                    match rest {
                        [flag, v, more @ ..] if flag == "--name" => (name, rest) = (Some(v.clone()), more),
                        [flag, v, more @ ..] if flag == "--insert" => (owner, rest) = (Some(v.clone()), more),
                        _ => break,
                    }
                }
                if rest.is_empty() {
                    return Err(bad("usage: ws effect [--name NAME] [--insert GENERATOR] COMMAND [ARGS...]"));
                }
                let e = self.add_effect(NewEffect { name, command: Some(rest.to_vec()), ..Default::default() }, owner, cwd)?;
                Ok(match &e.owner {
                    Some(o) => format!("added {} as an insert on {o}\n", e.name),
                    None => format!("added send effect {}; send to it with: ws send GENERATOR {} AMOUNT\n", e.name, e.name),
                })
            }
            "send" => {
                let (gen, fx) = (arg(0, "GENERATOR")?, arg(1, "EFFECT")?);
                let amount = num(arg(2, "AMOUNT (0..1)")?, "a number")?;
                self.patch_generator(gen.clone(), GeneratorPatch { sends: Some(BTreeMap::from([(fx.clone(), Some(amount))])), ..Default::default() })?;
                Ok(format!("{gen} -> {fx} {amount}\n"))
            }
            "bypass" => {
                let on = match arg(1, "on|off")?.as_str() {
                    "on" => true,
                    "off" => false,
                    other => return Err(bad(format!("bypass: expected on or off, not {other}"))),
                };
                let e = self.patch_effect(arg(0, "EFFECT")?, EffectPatch { bypass: Some(on), ..Default::default() })?;
                Ok(format!("{} bypass {}\n", e.name, if on { "on" } else { "off" }))
            }
            "gain" => {
                let name = arg(0, "NAME")?;
                let db = num(arg(1, "DB")?, "a dB value")?;
                match name.as_str() {
                    "master" => transport(TransportPatch { master_db: Some(db), ..Default::default() })?,
                    "click" => transport(TransportPatch { click_db: Some(db), ..Default::default() })?,
                    _ if self.is_effect(&name)? => self.patch_effect(name.clone(), EffectPatch { gain_db: Some(db), ..Default::default() }).map(|_| ())?,
                    _ => self.patch_generator(name.clone(), GeneratorPatch { gain_db: Some(db), ..Default::default() }).map(|_| ())?,
                }
                Ok(format!("{name} {db:+.1} dB\n"))
            }
            "mute" | "unmute" => {
                let muted = cmd == "mute";
                let name = arg(0, "NAME")?;
                if self.is_effect(&name)? {
                    self.patch_effect(name.clone(), EffectPatch { muted: Some(muted), ..Default::default() })?;
                } else {
                    self.patch_generator(name.clone(), GeneratorPatch { muted: Some(muted), ..Default::default() })?;
                }
                Ok(format!("{name} {cmd}d\n"))
            }
            "bpm" => {
                let bpm = num(arg(0, "BPM")?, "a number")?;
                transport(TransportPatch { bpm: Some(bpm), ..Default::default() })?;
                Ok(format!("{bpm} bpm\n"))
            }
            "key" => {
                let s = self.patch_transport(TransportPatch { root: Some(arg(0, "ROOT")?), scale: rest.get(1).cloned(), ..Default::default() })?;
                Ok(format!("{} {}\n", s.root, s.scale))
            }
            "click" => {
                let on = match arg(0, "on|off")?.as_str() {
                    "on" => true,
                    "off" => false,
                    other => return Err(bad(format!("click: expected on or off, not {other}"))),
                };
                transport(TransportPatch { click: Some(on), ..Default::default() })?;
                Ok(format!("click {}\n", if on { "on" } else { "off" }))
            }
            "play" | "stop" => {
                let s = self.patch_transport(TransportPatch { playing: Some(cmd == "play"), ..Default::default() })?;
                Ok(format!("{} at {}\n", if s.playing { "playing" } else { "stopped" }, s.position.text))
            }
            "seek" => {
                let bar = arg(0, "BAR")?.parse().map_err(|_| bad("seek: BAR is a bar number from 1"))?;
                self.seek(Seek { bar })?;
                Ok(format!("seek to bar {bar}\n"))
            }
            "params" => {
                let name = arg(0, "NAME")?;
                let found = self.generators()?.into_iter().find(|g| g.name == name).map(|g| (g.params, g.channels));
                let found = match found {
                    Some(f) => Some(f),
                    None => self.effects()?.into_iter().find(|e| e.name == name).map(|e| (e.params, e.channels)),
                };
                let (params, channels) = found.ok_or_else(|| ApiError::NotFound(format!("no generator or effect named {name}")))?;
                if params.is_empty() {
                    return Ok(format!("{name} has no parameters{}\n", if channels.is_none() { " (not running)" } else { "" }));
                }
                let w = params.iter().map(|p| p.desc.id.len()).max().unwrap_or(2);
                Ok(params
                    .iter()
                    .map(|p| {
                        let range = match &p.desc.labels {
                            Some(l) => l.join("|"),
                            None => format!("{} .. {}", p.desc.min, p.desc.max),
                        };
                        format!("{:w$}  {:>8}  {:<22} {} {}\n", p.desc.id, p.value, range, p.desc.name, p.desc.unit)
                    })
                    .collect())
            }
            "set" => {
                let (name, id) = (arg(0, "NAME")?, arg(1, "PARAM")?);
                let value = num(arg(2, "VALUE")?, "a number")?;
                let values = BTreeMap::from([(id.clone(), value)]);
                let params = if self.is_effect(&name)? { self.set_effect_params(name.clone(), values)?.params } else { self.set_params(name.clone(), values)?.params };
                let actual = params.iter().find(|p| p.desc.id == id).map_or(value, |p| p.value);
                Ok(format!("{name} {id} = {actual}\n"))
            }
            "quit" => self.quit().map(|_| "workspace stopping\n".into()),
            other => Err(bad(format!("unknown command {other}; see ws help"))),
        }
    }
}
