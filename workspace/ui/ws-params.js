// The controls for a plugin's parameters: knobs for continuous and integer
// parameters, buttons for toggles, menus for choices. Emits `change` with
// `detail` = `{ [id]: value, ... }`. Used in channel strips and insert
// panels.
//
// The random button asks the mixer to randomize the plugin (it picks the
// values and slides each param over the plugin's own random_glide_bars;
// knobs follow the state). Emits `randomize`. The undo button slides back
// to the values from before the last random. Mixer settings shown
// alongside (the effect's Duck knob) are never randomised.

import { LitElement, html, css, nothing } from './vendor/lit-all.min.js';
import { live } from './vendor/lit-all.min.js';
import { DUCK_ID } from './ws-knob.js';

const UNDO_GLIDE_BARS = 1; // undo slides back over this long

class WsParams extends LitElement {
  static properties = {
    params: { attribute: false },
    previous: { state: true }, // values before the last roll, for undo
  };

  static styles = css`
    :host { display: block; }
    .grid { display: flex; flex-wrap: wrap; justify-content: center; gap: 6px 4px; }
    .wide { flex: 0 0 100%; display: grid; gap: 2px; }
    .name { font-size: 9px; color: var(--dim); text-align: center; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    select, button {
      font: 600 10px var(--sans); color: var(--text); background: var(--panel-2);
      border: 1px solid var(--edge); border-radius: 4px; padding: 3px 2px; width: 100%; cursor: pointer;
    }
    button.on { background: var(--accent); border-color: var(--accent); color: #1a1205; }
    .tools { flex: 0 0 100%; display: flex; gap: 4px; }
    .tools button { flex: 1; font: 600 9px/1 var(--sans); letter-spacing: .5px; text-transform: uppercase; padding: 3px 0; color: var(--dim); }
    .tools button:hover:not(:disabled) { color: var(--text); border-color: var(--dim); }
    .tools button:disabled { opacity: .35; cursor: default; }
  `;

  constructor() {
    super();
    this.previous = null;
  }

  /** `glideBars` makes the mixer slide there instead of jumping. */
  send(values, glideBars) {
    const ev = new CustomEvent('change', { detail: values });
    ev.glideBars = glideBars;
    this.dispatchEvent(ev);
  }

  /** Every plugin parameter, not mixer settings shown alongside. */
  get pluginParams() {
    return (this.params ?? []).filter((p) => p.id !== DUCK_ID);
  }

  randomize() {
    const params = this.pluginParams;
    if (!params.length) return;
    this.previous = Object.fromEntries(params.map((p) => [p.id, p.value]));
    this.dispatchEvent(new CustomEvent('randomize'));
  }

  undo() {
    if (!this.previous) return;
    this.send(this.previous, UNDO_GLIDE_BARS);
    this.previous = null;
  }

  control(p) {
    switch (p.kind) {
      case 'toggle': {
        const on = p.value >= 0.5;
        return html`<div class="wide">
          <button class=${on ? 'on' : ''} title=${p.name} aria-pressed=${on} @click=${() => this.send({ [p.id]: on ? 0 : 1 })}>${p.name}</button>
        </div>`;
      }
      case 'choice':
        return html`<div class="wide">
          <span class="name" title=${p.name}>${p.name}</span>
          <select title=${p.name} .value=${live(String(Math.round(p.value)))} @change=${(e) => this.send({ [p.id]: Number(e.target.value) })}>
            ${(p.labels ?? []).map((label, i) => html`<option value=${i}>${label}</option>`)}
          </select>
        </div>`;
      default:
        return html`<ws-knob .param=${p} @change=${(e) => this.send({ [p.id]: e.detail })}></ws-knob>`;
    }
  }

  render() {
    if (!this.params?.length) return nothing;
    const canRoll = this.pluginParams.length > 0;
    // The tools row comes first, so it stays visible when the panel scrolls.
    return html`<div class="grid">
      ${canRoll ? html`<div class="tools">
        <button title="Random values for every parameter, sliding there" @click=${this.randomize}>random</button>
        <button title="Undo the last random" ?disabled=${!this.previous} @click=${this.undo}>undo</button>
      </div>` : nothing}
      ${this.params.map((p) => this.control(p))}
    </div>`;
  }
}

customElements.define('ws-params', WsParams);
