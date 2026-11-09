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

    armed: { state: true }, // the ✕ was clicked once; a second click removes
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
    .x {
      font: inherit; background: none; border: 0; color: var(--dim); cursor: pointer; padding: 0 2px; font-size: 14px;
    }
    .x:hover { color: var(--danger); }
    .x.armed { color: #fff; background: var(--danger); border-radius: 3px; }
    .err {
      font-size: 10px; color: var(--danger); line-height: 1.2;
      display: -webkit-box; -webkit-line-clamp: 3; -webkit-box-orient: vertical; overflow: hidden;
    }
    .stats { font: 10px var(--mono); color: var(--dim); text-align: center; }
    ws-fader { margin-top: auto; }
    /* Long parameter lists scroll; a thin visible scrollbar shows there's more. */
    .params { max-height: 50vh; overflow-y: auto; overflow-x: hidden; scrollbar-width: thin; scrollbar-color: var(--edge) transparent; }
    .inserts { max-height: 30vh; overflow-y: auto; overflow-x: hidden; }
    .sends { display: grid; gap: 3px; max-height: 20vh; overflow-y: auto; }
    .buttons { display: flex; gap: 4px; justify-content: center; }
    button.b {
      font: 600 11px var(--sans); color: var(--text); background: var(--panel-2); border: 1px solid var(--edge);
      border-radius: 4px; padding: 4px 0; cursor: pointer; flex: 1;
    }
    button.b:hover { border-color: var(--dim); }
    button.mute.on { background: var(--mute); border-color: var(--mute); color: #1f1300; }
    button.solo.on { background: var(--led-cycle); border-color: var(--led-cycle); color: #06142b; }

    .name {
      text-align: center; font-weight: 600; font-size: 12px; padding: 4px 2px;
      background: var(--panel-2); border-radius: 4px;
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
    this.armed = false;
  }

  get ops() {
    return OPS[this.kind] ?? OPS.generator;
  }

  updated() {
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

  patch(p) { return this.run(() => this.ops.patch(this.gen.name, p)); }
  setParams(values, glideBars) {
    // The duck knob is a mixer setting, not a plugin param: it goes to the
    // effect or generator itself.
    const { [DUCK_ID]: duck, ...rest } = values;
    if (duck !== undefined) this.run(() => this.ops.patch(this.gen.name, { duck_sensitivity: duck }));
    if (Object.keys(rest).length) this.run(() => this.ops.params(this.gen.name, rest, glideBars));
  }

  setSend(effect, amount) {
    return this.patch({ sends: { [effect]: amount } });
  }

  /** First click turns the ✕ red, a second click (within 3 s) removes. */
  clickRemove() {
    clearTimeout(this.disarm);
    if (!this.armed) {
      this.armed = true;
      this.disarm = setTimeout(() => (this.armed = false), 3000);
      return;
    }
    this.armed = false;
    return this.run(() => this.ops.remove(this.gen.name));
  }

  dot(state) {
    if (state === 'ok') return '';
    if (state === 'entering') return 'enter';
    if (state === 'muted' || state === 'starting') return 'idle';
    if (state === 'late' || state.startsWith('retry')) return 'warn';
    return 'bad';
  }

  render() {
    const g = this.gen;
    if (!g) return nothing;
    const err = g.last_error;
    const isReturn = this.kind === 'return';
    const ch = g.channels === 2 ? 'st' : g.channels === 1 ? 'mono' : '–';
    const empty = html`<span></span>`;
    // Returns always have a duck knob; channels once the API lets their
    // sensitivity be set (it then reports declared_duck_sensitivity).
    const duck = isReturn || g.declared_duck_sensitivity !== undefined ? duckParam(g) : null;
    const params = duck ? [...(g.params ?? []), duck] : (g.params ?? []);
    // Ducking: what turns this strip down right now, and (generators) how much
    // it ducks the others. The source bar appears once it has ducked anything.
    const gr = isReturn ? (this.duck ?? 0) * (g.duck_sensitivity ?? 0) : (g.duck_level ?? 0) * (g.duck_sensitivity ?? 0);
    const duckNote = !duck && g.duck_sensitivity != null ? ` · duck sensitivity ${g.duck_sensitivity}` : '';
    return html`
      ${isReturn ? html`<div class="tag">fx return</div>` : nothing}
      <div class="top">
        <span class="state" title=${g.state === 'entering' ? `entering: ${g.entry?.mode === 'fade' ? `fading in over ${g.entry.bars} bars` : 'waiting for the next bar'}` : g.state}><span class="dot ${this.dot(g.state)}"></span><span class="label">${g.state}</span></span>
        <button class="x ${this.armed ? 'armed' : ''}"
          title=${this.armed ? 'Click again to remove' : isReturn ? 'Remove this send effect (drops every send to it); click twice' : 'Remove channel (and its inserts); click twice'}
          @click=${this.clickRemove}>✕</button>
      </div>
      ${err && err.seconds_ago < 60
        ? html`<div class="err" title=${err.message}>${err.message}</div>`
        : html`<div class="stats" title="channels · late blocks · restarts${duckNote}">${ch} · L${g.late} · R${g.restarts}</div>`}
      ${params.length
        ? html`<ws-params class="params" .params=${params} @change=${(e) => this.setParams(e.detail, e.glideBars)}
          @randomize=${() => this.run(() => this.ops.randomize(this.gen.name))}></ws-params>`
        : empty}
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
        ${!isReturn && g.cue !== undefined
          ? html`<button class="b solo ${g.cue ? 'on' : ''}" title="Pre-listen: hear this channel on the cue output; the main mix is unchanged"
              @click=${() => this.patch({ cue: !g.cue })}>S</button>`
          : nothing}
      </div>
      <div class="name" title=${(g.description ? g.description + '\n' : '') + g.command.join(' ')}>${g.name}</div>
    `;
  }
}

customElements.define('ws-channel', WsChannel);
