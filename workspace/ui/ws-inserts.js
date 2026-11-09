// A generator's insert chain, in signal order: one slot per effect in the
// effect's colour, with bypass and remove. Clicking a slot opens its
// parameters and lets it move up or down the chain. "+ insert" adds an
// effect from the catalog at the end.
//
// A bypassed, restarting or late insert passes audio through, so the
// channel keeps sounding.

import { LitElement, html, css, nothing } from './vendor/lit-all.min.js';
import { api } from './api.js';
import { colorOf } from './colors.js';
import './ws-params.js';
import { duckParam, DUCK_ID } from './ws-knob.js';

class WsInserts extends LitElement {
  static properties = {
    owner: { type: String }, // the generator
    inserts: { attribute: false }, // Effect objects, in chain order
    catalog: { attribute: false }, // catalog entries with kind "effect"
    open: { state: true }, // name of the slot showing its parameters
    adding: { state: true },
  };

  static styles = css`
    :host { display: grid; gap: 3px; }
    .slot {
      display: grid; grid-template-columns: 1fr auto auto; align-items: center;
      height: 20px; border-radius: 3px; overflow: hidden;
      background: color-mix(in srgb, var(--c) 18%, var(--panel-2));
      border-left: 3px solid var(--c);
    }
    .slot.bypass { background: var(--panel-2); border-left-color: color-mix(in srgb, var(--c) 35%, var(--led-off)); }
    .slot.bypass .label { color: var(--dim); text-decoration: line-through; }
    .label {
      font: 600 9px/20px var(--sans); padding: 0 4px; cursor: pointer; text-align: left;
      white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
      background: none; border: 0; color: var(--text);
    }
    .label .warn { color: var(--warn); }
    .icon {
      font: 11px/1 var(--sans); background: none; border: 0; color: var(--dim); cursor: pointer;
      width: 18px; height: 20px; padding: 0;
    }
    .icon:hover { color: var(--text); }
    .icon.on { color: var(--c); }
    .icon.x:hover { color: var(--danger); }
    .detail {
      display: grid; gap: 4px; padding: 4px 2px; border-radius: 3px;
      background: color-mix(in srgb, var(--c) 8%, var(--panel));
      border: 1px solid color-mix(in srgb, var(--c) 30%, transparent);
    }
    .order { display: flex; gap: 4px; }
    .order button, select {
      font: 600 10px var(--sans); color: var(--text); background: var(--panel-2);
      border: 1px solid var(--edge); border-radius: 4px; padding: 2px; cursor: pointer;
    }
    .order button { flex: 1; }
    .order button:disabled { opacity: .35; cursor: default; }
    select { width: 100%; color: var(--dim); }
  `;

  constructor() {
    super();
    this.inserts = [];
    this.catalog = [];
    this.open = null;
    this.adding = false;
  }

  fail(e, what) {
    this.dispatchEvent(new CustomEvent('ws-error', { detail: `${what}: ${e.message}`, bubbles: true, composed: true }));
  }

  async run(what, fn) {
    try { await fn(); } catch (e) { this.fail(e, what); }
  }

  async add(e) {
    const effect = e.target.value;
    e.target.value = '';
    if (!effect) return;
    this.adding = true;
    try {
      const fx = await api.addInsert(this.owner, { effect });
      this.open = fx.name;
    } catch (err) {
      this.fail(err, `${this.owner} + ${effect}`);
    } finally {
      this.adding = false;
    }
  }

  paramsOf(fx) {
    const duck = duckParam(fx);
    return duck ? [...fx.params, duck] : fx.params;
  }

  setParams(fx, values, glideBars) {
    // Duck sensitivity is a mixer setting; inserts duck only what they add.
    const { [DUCK_ID]: duck, ...rest } = values;
    if (duck !== undefined) this.run(fx.name, () => api.patchEffect(fx.name, { duck_sensitivity: duck }));
    if (Object.keys(rest).length) this.run(fx.name, () => api.effectParams(fx.name, rest, glideBars));
  }

  slot(fx, i) {
    const open = this.open === fx.name;
    const trouble = fx.state !== 'ok' && fx.state !== 'muted';
    const tip = `${fx.name}${fx.description ? ': ' + fx.description : ''}\nstate: ${fx.state}${fx.bypass ? ' (bypassed)' : ''}${fx.last_error ? '\nlast error: ' + fx.last_error.message : ''}`;
    return html`
      <div class="slot ${fx.bypass ? 'bypass' : ''}" style="--c:${colorOf(fx)}">
        <button class="label" title=${tip} @click=${() => (this.open = open ? null : fx.name)}>
          ${trouble ? html`<span class="warn">●</span> ` : nothing}${fx.name}
        </button>
        <button class="icon ${fx.bypass ? '' : 'on'}" title=${fx.bypass ? 'Bypassed; click to enable' : 'Active; click to bypass'}
          @click=${() => this.run(fx.name, () => api.patchEffect(fx.name, { bypass: !fx.bypass }))}>⏻</button>
        <button class="icon x" title="Remove insert"
          @click=${() => this.run(fx.name, () => api.removeEffect(fx.name))}>✕</button>
      </div>
      ${open ? html`
        <div class="detail" style="--c:${colorOf(fx)}">
          <ws-params .params=${this.paramsOf(fx)} @change=${(e) => this.setParams(fx, e.detail, e.glideBars)}
            @randomize=${() => this.run(fx.name, () => api.randomizeEffect(fx.name))}></ws-params>
          <div class="order">
            <button title="Earlier in the chain" ?disabled=${i === 0}
              @click=${() => this.run(fx.name, () => api.patchEffect(fx.name, { position: i - 1 }))}>▲</button>
            <button title="Later in the chain" ?disabled=${i === this.inserts.length - 1}
              @click=${() => this.run(fx.name, () => api.patchEffect(fx.name, { position: i + 1 }))}>▼</button>
            <button title="Restart" @click=${() => this.run(fx.name, () => api.restartEffect(fx.name))}>↻</button>
          </div>
        </div>` : nothing}
    `;
  }

  render() {
    return html`
      ${this.inserts.map((fx, i) => this.slot(fx, i))}
      ${this.catalog.length
        ? html`<select title="Add an insert effect to this channel" ?disabled=${this.adding} @change=${this.add}>
            <option value="">${this.adding ? 'adding…' : '+ insert'}</option>
            ${this.catalog.map((c) => html`<option value=${c.name}>${c.name}</option>`)}
          </select>`
        : nothing}
    `;
  }
}

customElements.define('ws-inserts', WsInserts);
