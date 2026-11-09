// The controls for a plugin's parameters: knobs for continuous and integer
// parameters, buttons for toggles, menus for choices. Emits `change` with
// `detail` = `{ [id]: value, ... }`. Used in channel strips and insert
// panels.
//
// Next to random (R) sit the standard toggles many plugins share, fills
// (F) and tempo (T), on every channel strip: lit when on, greyed when the
// plugin doesn't have one, explained on hover. They're left out of the
// grid. Insert panels (`standard` off) show only R.
//
// The random button asks the mixer to randomize the plugin (it picks the
// values and slides each param over the plugin's own random_glide_bars;
// knobs follow the state). Emits `randomize`. Mixer settings shown
// alongside (the effect's Duck knob) are never randomised.

import { LitElement, html, css, nothing } from './vendor/lit-all.min.js';
import { live } from './vendor/lit-all.min.js';
import { DUCK_ID } from './ws-knob.js';

/** Toggles many plugins share, as buttons in the tools row. */
const STANDARD = [
  { id: 'fills', label: 'F', title: 'Fills: drop-outs and runs at the end of phrases' },
  { id: 'tempo', label: 'T', title: "Tempo: follow the score's tempo feel (half, double, quad, triplet)" },
];
const isStandard = (p) => p.kind === 'toggle' && STANDARD.some((t) => t.id === p.id);

class WsParams extends LitElement {
  static properties = {
    params: { attribute: false },
    standard: { type: Boolean }, // show the fills and tempo buttons
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
    .tools button.on { background: var(--accent); border-color: var(--accent); color: #1a1205; }
  `;

  constructor() {
    super();
    this.standard = true;
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
    this.dispatchEvent(new CustomEvent('randomize'));
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

  toggleButton(t) {
    const p = (this.params ?? []).find((d) => d.id === t.id && d.kind === 'toggle');
    if (!p) return html`<button disabled title="${t.title}. This plugin doesn't have it.">${t.label}</button>`;
    const on = p.value >= 0.5;
    return html`<button class=${on ? 'on' : ''} aria-pressed=${on} title="${t.title} (${on ? 'on' : 'off'})"
      @click=${() => this.send({ [t.id]: on ? 0 : 1 })}>${t.label}</button>`;
  }

  render() {
    const canRoll = this.pluginParams.length > 0;
    // The tools row comes first, so it stays visible when the panel scrolls.
    return html`<div class="grid">
      <div class="tools">
        <button title="Random: new values for every parameter, sliding there" ?disabled=${!canRoll} @click=${this.randomize}>R</button>
        ${this.standard ? STANDARD.map((t) => this.toggleButton(t)) : nothing}
      </div>
      ${(this.params ?? []).filter((p) => !(this.standard && isStandard(p))).map((p) => this.control(p))}
    </div>`;
  }
}

customElements.define('ws-params', WsParams);
