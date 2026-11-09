// One channel strip. Either a generator channel (parameters, insert chain,
// send boxes, fader, mute, pre-listen, remove) or a send effect's return
// (parameters, return fader, mute, remove), in the effect's colour.

import { LitElement, html, css, nothing } from './vendor/lit-all.min.js';
import { api } from './api.js';
import './ws-fader.js';
import './ws-params.js';
import './ws-inserts.js';
import './ws-send.js';
import { colorOf } from './colors.js';
import { duckParam, DUCK_ID } from './ws-knob.js';

/** How the ✕ can take a channel out: the API's `exit` modes. */
const EXITS = [
  { mode: 'now', label: 'now', title: 'Remove straight away' },
  { mode: 'cycle', label: 'cycle', title: 'Cut at the next cycle line (every 4 bars)' },
  { mode: 'epoch', label: 'epoch', title: 'Cut at the next epoch line (every 16 bars)' },
  { mode: 'fade_cycle', label: 'fade cycle', title: 'Fade out, gone at the next cycle line' },
  { mode: 'fade_epoch', label: 'fade epoch', title: 'Fade out, gone at the next epoch line' },
];

/** A beat as `epoch.cycle.bar`, 1-based like the transport's counter. */
function lineName(beat, beatsPerBar = 4) {
  const bars = Math.round(beat / beatsPerBar);
  return `${Math.floor(bars / 16) + 1}.${(Math.floor(bars / 4) % 4) + 1}.${(bars % 4) + 1}`;
}

/** The endpoints for each kind of strip. */
const OPS = {
  generator: { patch: api.patch, params: api.params, randomize: api.randomize, remove: api.remove },
  return: { patch: api.patchEffect, params: api.effectParams, randomize: api.randomizeEffect, remove: api.removeEffect },
};

class WsChannel extends LitElement {
  static properties = {
    gen: { attribute: false }, // a Generator, or an Effect with role "send"
    kind: { type: String }, // "generator" or "return"
    sendTo: { attribute: false }, // send Effects (generator strips)
    inserts: { attribute: false }, // this generator's insert Effects, in order
    fxCatalog: { attribute: false }, // catalog entries with kind "effect"
    duck: { type: Number }, // the global duck level (return strips are ducked by it)

    menu: { state: true }, // the ✕'s exit menu is open
    selected: { type: Boolean, reflect: true }, // picked by a click on its background (ws-app)
    group: { attribute: false }, // the other selected strips of this kind: changes here are copied to them
  };

  static styles = css`
    :host {
      flex: 0 0 108px; display: flex; flex-direction: column; gap: 8px;
      background: var(--panel); border: 1px solid var(--edge); border-radius: 8px; padding: 8px 6px;
      min-height: 0;
    }
    :host([muted]) { background: color-mix(in srgb, var(--panel) 85%, var(--mute) 5%); }
    /* Effect returns: a cooler, darker base with faint hatching, so they
       read as a different kind of strip than the channels. */
    :host([kind=return]) {
      background:
        repeating-linear-gradient(135deg, transparent 0 6px, rgb(255 255 255 / 2.5%) 6px 7px),
        var(--fx-bg);
      border-color: var(--fx-edge);
      border-top-color: var(--plugin, var(--fx-edge));
    }
    .tag {
      font: 700 8px/1 var(--sans); letter-spacing: 1px; text-transform: uppercase; text-align: center;
      color: var(--fx-tag); margin: -2px 0 -4px;
    }
    /* The plugin's colour: a band across the top and the name label. */
    :host { border-top: 3px solid var(--plugin, var(--edge)); }
    .top { display: flex; justify-content: space-between; align-items: center; gap: 4px; }
    .state { display: flex; align-items: center; gap: 5px; font-size: 11px; color: var(--dim); overflow: hidden; }
    .dot { width: 8px; height: 8px; border-radius: 50%; flex: none; background: var(--led-beat); }
    .dot.warn { background: var(--warn); }
    .dot.bad { background: var(--danger); }
    .dot.idle { background: var(--dim); }
    .dot.enter { background: var(--led-cycle); animation: pulse 0.8s ease-in-out infinite alternate; }
    @keyframes pulse { from { opacity: .35; } to { opacity: 1; } }
    .label { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .label.leaving { color: var(--danger); font-weight: 700; }
    .x {
      font: inherit; background: none; border: 0; color: var(--dim); cursor: pointer; padding: 0 2px; font-size: 14px;
    }
    .x:hover { color: var(--danger); }
    .x.open, .x.exiting { color: #fff; background: var(--danger); border-radius: 3px; }
    .xwrap { position: relative; }
    .menu {
      position: absolute; right: 0; top: calc(100% + 2px); z-index: 10; min-width: 96px;
      display: grid; padding: 3px; gap: 1px;
      background: var(--panel-2); border: 1px solid var(--edge); border-radius: 5px;
      box-shadow: 0 6px 16px rgb(0 0 0 / 45%);
    }
    .menu .head { font: 700 8px/1 var(--sans); letter-spacing: 1px; text-transform: uppercase; color: var(--dim); padding: 4px 6px 3px; }
    .menu button {
      font: 500 11px var(--sans); color: var(--text); background: none; border: 0; border-radius: 3px;
      text-align: left; padding: 4px 6px; cursor: pointer; white-space: nowrap;
    }
    .menu button:hover, .menu button:focus-visible { background: var(--danger); color: #fff; outline: none; }
    .menu button.current { color: var(--danger); }
    .menu button.current:hover { color: #fff; }
    .dot.exit { background: var(--danger); animation: pulse 0.5s ease-in-out infinite alternate; }
    .err {
      font-size: 10px; color: var(--danger); line-height: 1.2;
      display: -webkit-box; -webkit-line-clamp: 3; -webkit-box-orient: vertical; overflow: hidden;
    }
    ws-fader { margin-top: auto; }
    /* Long parameter lists scroll; a thin visible scrollbar shows there's more. */
    .params { max-height: 50vh; overflow-y: auto; overflow-x: hidden; scrollbar-width: thin; scrollbar-color: var(--edge) transparent; }
    .inserts { max-height: 30vh; overflow-y: auto; overflow-x: hidden; }
    /* Sends sit right above the fader (which is bottom-aligned), so every
       strip's sends line up across the board. */
    .sends { display: grid; gap: 2px; max-height: 20vh; overflow-y: auto; margin-top: auto; flex: none; }
    .sends + ws-fader { margin-top: 0; }
    .buttons { display: flex; gap: 4px; justify-content: center; }
    button.b {
      font: 600 10px/14px var(--sans); color: var(--text); background: var(--panel-2); border: 1px solid var(--edge);
      border-radius: 3px; padding: 0; cursor: pointer; flex: 1;
    }
    button.b:hover { border-color: var(--dim); }
    button.mute.on { background: var(--mute); border-color: var(--mute); color: #1f1300; }
    .name { cursor: grab; }
    :host([dragging]) { opacity: .4; }
    :host([selected]) { outline: 2px solid var(--accent); outline-offset: 1px; }
    /* Shift-click selects strips; it shouldn't select their text too. */
    :host { user-select: none; -webkit-user-select: none; }
    /* Where a dragged strip will land. */
    :host([drop=before]) { box-shadow: -5px 0 0 -1px var(--accent); }
    :host([drop=after]) { box-shadow: 5px 0 0 -1px var(--accent); }
    button.solo.on { background: var(--led-cycle); border-color: var(--led-cycle); color: #06142b; }


    .name {
      text-align: center; font: 600 11px/16px var(--sans); padding: 0 2px;
      background: var(--panel-2); border-radius: 3px;
      white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
    }
    .name {
      color: var(--plugin, var(--text));
      background: color-mix(in srgb, var(--plugin, var(--panel-2)) 14%, var(--panel-2));
    }
  `;

  constructor() {
    super();
    this.kind = 'generator';
    this.sendTo = [];
    this.inserts = [];
    this.fxCatalog = [];
    this.menu = false;
    this.closeMenu = (e) => {
      if (e.type === 'keydown' ? e.key === 'Escape' : !e.composedPath().includes(this)) this.menu = false;
    };
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    this.listen(false);
  }

  /** Outside clicks and Escape close the exit menu. */
  listen(on) {
    const f = on ? 'addEventListener' : 'removeEventListener';
    document[f]('pointerdown', this.closeMenu, true);
    document[f]('keydown', this.closeMenu);
  }

  get ops() {
    return OPS[this.kind] ?? OPS.generator;
  }

  updated(changed) {
    if (changed.has('menu')) {
      this.listen(this.menu);
      if (this.menu) this.renderRoot.querySelector('.menu button')?.focus();
    }
    this.toggleAttribute('muted', !!this.gen?.muted);
    this.setAttribute('kind', this.kind);
    if (this.gen) this.style.setProperty('--plugin', colorOf(this.gen));

  }

  fail(e) {
    this.dispatchEvent(new CustomEvent('ws-error', { detail: `${this.gen.name}: ${e.message}`, bubbles: true, composed: true }));
  }

  async run(fn) {
    try { await fn(); } catch (e) { this.fail(e); }
  }

  /** This strip and the other selected ones of its kind. */
  get targets() {
    return [this.gen, ...(this.selected ? this.group ?? [] : [])];
  }

  /**
   * A change, copied to every selected strip. Faders move together keeping
   * their offsets (relative to where each was when the move began); every
   * other field is set to the same value.
   */
  patch(p) {
    const targets = this.targets;
    if (p.gain_db === undefined || targets.length === 1) {
      for (const g of targets) this.run(() => this.ops.patch(g.name, p));
      return;
    }
    // One gesture: changes less than 400 ms apart share the starting levels.
    clearTimeout(this.gainGesture?.timer);
    const base = this.gainGesture?.base ?? new Map(targets.map((g) => [g.name, g.gain_db]));
    this.gainGesture = { base, timer: setTimeout(() => (this.gainGesture = null), 400) };
    const delta = p.gain_db - base.get(this.gen.name);
    for (const g of targets) {
      const from = base.get(g.name) ?? g.gain_db;
      const gain_db = g === this.gen ? p.gain_db : Math.min(24, Math.max(-96, from + delta));
      this.run(() => this.ops.patch(g.name, { ...p, gain_db }));
    }
  }

  setParams(values, glideBars) {
    // The duck knob is a mixer setting, not a plugin param: it goes to the
    // effect or generator itself.
    const { [DUCK_ID]: duck, ...rest } = values;
    for (const g of this.targets) {
      if (duck !== undefined) this.run(() => this.ops.patch(g.name, { duck_sensitivity: duck }));
      // Other selected strips take the params they also have (same id).
      const ids = g === this.gen ? null : new Set((g.params ?? []).map((d) => d.id));
      const mine = ids ? Object.fromEntries(Object.entries(rest).filter(([id]) => ids.has(id))) : rest;
      if (Object.keys(mine).length) this.run(() => this.ops.params(g.name, mine, glideBars));
    }
  }

  randomize() {
    for (const g of this.targets) this.run(() => this.ops.randomize(g.name));
  }

  setSend(effect, amount) {
    return this.patch({ sends: { [effect]: amount } });
  }

  /** Takes the channel out as `mode` says; again while it's exiting
   *  replaces the pending exit (e.g. `now` to cut a slow fade short). */
  // Not `remove`: that would shadow Element.remove(), which Lit calls to
  // take the strip out of the page.
  removeAs(mode) {
    this.menu = false;
    for (const g of this.targets) this.run(() => this.ops.remove(g.name, mode));
  }

  // Dragging the name label reorders strips; ws-app handles the drop.
  dragStart(e) {
    e.dataTransfer.effectAllowed = 'move';
    e.dataTransfer.setData('text/plain', this.gen.name);
    this.toggleAttribute('dragging', true);
    this.dispatchEvent(new CustomEvent('strip-drag', { detail: { name: this.gen.name, kind: this.kind }, bubbles: true, composed: true }));
  }

  dragEnd() {
    this.toggleAttribute('dragging', false);
    this.dispatchEvent(new CustomEvent('strip-drag', { detail: null, bubbles: true, composed: true }));
  }

  dot(state) {
    if (state === 'ok') return '';
    if (state === 'entering') return 'enter';
    if (state === 'exiting') return 'exit';
    if (state === 'muted' || state === 'starting') return 'idle';
    if (state === 'late' || state.startsWith('retry')) return 'warn';
    return 'bad';
  }

  stateTitle(g) {
    if (g.state === 'entering') return `entering: ${g.entry?.mode === 'fade' ? `fading in over ${g.entry.bars} bars` : 'waiting for the next bar'}`;
    if (g.exiting) {
      const how = g.exiting.mode.startsWith('fade') ? 'fading out' : 'playing';
      return `exiting: ${how}, gone at ${lineName(g.exiting.at)}`;
    }
    return `${g.state} · ${g.channels === 2 ? 'stereo' : 'mono'} · ${g.late} late blocks · ${g.restarts} restarts`;
  }

  render() {
    const g = this.gen;
    if (!g) return nothing;
    const err = g.last_error;
    const isReturn = this.kind === 'return';
    const empty = html`<span></span>`;
    // Returns always have a duck knob; channels once the API lets their
    // sensitivity be set (it then reports declared_duck_sensitivity).
    const duck = isReturn || g.declared_duck_sensitivity !== undefined ? duckParam(g) : null;
    const params = duck ? [...(g.params ?? []), duck] : (g.params ?? []);
    // Ducking: what turns this strip down right now, and (generators) how much
    // it ducks the others. The source bar appears once it has ducked anything.
    const gr = isReturn ? (this.duck ?? 0) * (g.duck_sensitivity ?? 0) : (g.duck_level ?? 0) * (g.duck_sensitivity ?? 0);
    return html`
      ${isReturn ? html`<div class="tag">fx return</div>` : nothing}
      <div class="top">
        <span class="state" title=${this.stateTitle(g)}><span class="dot ${this.dot(g.state)}"></span><span class="label ${g.exiting ? 'leaving' : ''}">${g.exiting ? EXITS.find((x) => x.mode === g.exiting.mode)?.label ?? g.state : g.state}</span></span>
        <span class="xwrap">
          <button class="x ${this.menu ? 'open' : ''} ${g.exiting ? 'exiting' : ''}" aria-haspopup="menu" aria-expanded=${this.menu}
            title=${isReturn ? 'Remove this send effect (drops every send to it)…' : 'Remove channel (and its inserts)…'}
            @click=${() => (this.menu = !this.menu)}>✕</button>
          ${this.menu
            ? html`<div class="menu" role="menu">
                <div class="head">${g.exiting ? 'exiting · change' : 'remove'}</div>
                ${EXITS.map((x) => html`<button role="menuitem" class=${g.exiting?.mode === x.mode ? 'current' : ''}
                  title=${x.title} @click=${() => this.removeAs(x.mode)}>${x.label}</button>`)}
              </div>`
            : nothing}
        </span>
      </div>
      ${err && err.seconds_ago < 60 ? html`<div class="err" title=${err.message}>${err.message}</div>` : nothing}
      <ws-params class="params" .params=${params} @change=${(e) => this.setParams(e.detail, e.glideBars)}
        @randomize=${() => this.randomize()}></ws-params>
      ${isReturn
        ? empty
        : html`<ws-inserts class="inserts" .owner=${g.name} .inserts=${this.inserts} .catalog=${this.fxCatalog}></ws-inserts>`}
      ${!isReturn && this.sendTo.length
        ? html`<div class="sends">${this.sendTo.map((fx) => html`
            <ws-send .effect=${fx.name} .color=${colorOf(fx)} .amount=${g.sends?.[fx.name] ?? 0}
              @change=${(e) => this.setSend(fx.name, e.detail)}></ws-send>`)}
          </div>`
        : empty}
      <ws-fader .db=${g.gain_db} .peak=${g.peak} .gr=${gr} .src=${isReturn ? 0 : g.duck_source ?? 0} ?dimmed=${g.muted}
        @change=${(e) => this.patch({ gain_db: e.detail })}></ws-fader>
      <div class="buttons">
        <button class="b mute ${g.muted ? 'on' : ''}" title="Mute" @click=${() => this.patch({ muted: !g.muted })}>M</button>
        ${!isReturn
          ? html`<button class="b solo ${g.solo ? 'on' : ''}" title="Solo: while any channel is soloed, only soloed ones are heard (sends too); effect returns stay"
              @click=${() => this.patch({ solo: !g.solo })}>S</button>`
          : nothing}
      </div>
      <div class="name" draggable="true" @dragstart=${this.dragStart} @dragend=${this.dragEnd}
        title=${(g.description ? g.description + '\n' : '') + g.command.join(' ') + '\ndrag to reorder'}>${g.name}</div>
    `;
  }
}

customElements.define('ws-channel', WsChannel);
