// The controls for a plugin's parameters: knobs for continuous and integer
// parameters, buttons for toggles, menus for choices. Emits `change` with
// `detail` = `{ [id]: value, ... }`. Used in channel strips and insert
// panels.
//
// A random button sets every parameter to a random value in its range
// (log-uniform for `log` params, a random label for choices, 50/50 for
// toggles); Shift-click only nudges each one around where it is. The undo
// button restores the values from before the last roll. Mixer settings shown
// alongside (the effect's Duck knob) are never randomised.

import { LitElement, html, css, nothing } from './vendor/lit-all.min.js';
import { live } from './vendor/lit-all.min.js';
import { DUCK_ID } from './ws-knob.js';
import './ws-knob.js';

const NUDGE = 0.15; // Shift-click: at most this much of the travel either way

/** Position 0..1 along the control's travel, for nudging. */
function toNorm(p, v) {
  if (p.max === p.min) return 0;
  if (p.log && p.min > 0) return Math.log(v / p.min) / Math.log(p.max / p.min);
  return (v - p.min) / (p.max - p.min);
}

function fromNorm(p, t) {
  t = Math.min(1, Math.max(0, t));
  const v = p.log && p.min > 0 ? p.min * Math.pow(p.max / p.min, t) : p.min + t * (p.max - p.min);
  return p.kind === 'integer' ? Math.round(v) : v;
}

/** A random value for a parameter: across its range, or a nudge from `p.value`. */
function roll(p, nudge) {
  const u = Math.random();
  switch (p.kind) {
    case 'toggle':
      return nudge ? p.value : u < 0.5 ? 0 : 1;
    case 'choice':
      return nudge ? p.value : Math.floor(u * Math.max(1, p.labels?.length ?? 0));
    case 'integer':
      if (nudge) return fromNorm(p, toNorm(p, p.value) + (u * 2 - 1) * NUDGE);
      return Math.min(p.max, Math.floor(p.min + u * (p.max - p.min + 1)));
    default:
      return fromNorm(p, nudge ? toNorm(p, p.value) + (u * 2 - 1) * NUDGE : u);
  }
}

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

  send(values) {
    this.dispatchEvent(new CustomEvent('change', { detail: values }));
  }

  /** Every plugin parameter, not mixer settings shown alongside. */
  get pluginParams() {
    return (this.params ?? []).filter((p) => p.id !== DUCK_ID);
  }

  randomize(e) {
    const params = this.pluginParams;
    if (!params.length) return;
    this.previous = Object.fromEntries(params.map((p) => [p.id, p.value]));
    this.send(Object.fromEntries(params.map((p) => [p.id, roll(p, e.shiftKey)])));
  }

  undo() {
    if (!this.previous) return;
    this.send(this.previous);
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
        <button title="Random values for every parameter (Shift: just nudge them)" @click=${this.randomize}>random</button>
        <button title="Undo the last random" ?disabled=${!this.previous} @click=${this.undo}>undo</button>
      </div>` : nothing}
      ${this.params.map((p) => this.control(p))}
    </div>`;
  }
}

customElements.define('ws-params', WsParams);
