// One channel strip: a generator in the mix, with its state, volume fader,
// meter, mute, restart and remove.

import { LitElement, html, css, nothing } from './vendor/lit-all.min.js';
import { api } from './api.js';
import './ws-fader.js';

class WsChannel extends LitElement {
  static properties = {
    gen: { attribute: false },
    confirming: { state: true },
  };

  static styles = css`
    :host {
      flex: 0 0 96px; display: grid; grid-template-rows: auto auto 1fr auto auto; gap: 8px;
      background: var(--panel); border: 1px solid var(--edge); border-radius: 8px; padding: 8px 6px;
      min-height: 0;
    }
    :host([muted]) { background: color-mix(in srgb, var(--panel) 85%, var(--mute) 5%); }
    .top { display: flex; justify-content: space-between; align-items: center; gap: 4px; }
    .state { display: flex; align-items: center; gap: 5px; font-size: 11px; color: var(--dim); overflow: hidden; }
    .dot { width: 8px; height: 8px; border-radius: 50%; flex: none; background: var(--led-beat); }
    .dot.warn { background: var(--warn); }
    .dot.bad { background: var(--danger); }
    .dot.idle { background: var(--dim); }
    .label { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .x {
      font: inherit; background: none; border: 0; color: var(--dim); cursor: pointer; padding: 0 2px; font-size: 14px;
    }
    .x:hover { color: var(--danger); }
    .err {
      font-size: 10px; color: var(--danger); line-height: 1.2;
      display: -webkit-box; -webkit-line-clamp: 3; -webkit-box-orient: vertical; overflow: hidden;
    }
    .stats { font: 10px var(--mono); color: var(--dim); text-align: center; }
    .buttons { display: flex; gap: 4px; justify-content: center; }
    button.b {
      font: 600 11px var(--sans); color: var(--text); background: var(--panel-2); border: 1px solid var(--edge);
      border-radius: 4px; padding: 4px 0; cursor: pointer; flex: 1;
    }
    button.b:hover { border-color: var(--dim); }
    button.mute.on { background: var(--mute); border-color: var(--mute); color: #1f1300; }
    .name {
      text-align: center; font-weight: 600; font-size: 12px; padding: 4px 2px;
      background: var(--panel-2); border-radius: 4px;
      white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
    }
    .confirm { display: grid; gap: 4px; font-size: 11px; text-align: center; }
    .confirm .yes { background: var(--danger); border-color: var(--danger); }
  `;

  constructor() {
    super();
    this.confirming = false;
  }

  updated() {
    this.toggleAttribute('muted', !!this.gen?.muted);
  }

  fail(e) {
    this.dispatchEvent(new CustomEvent('ws-error', { detail: `${this.gen.name}: ${e.message}`, bubbles: true, composed: true }));
  }

  async patch(p) {
    try { await api.patch(this.gen.name, p); } catch (e) { this.fail(e); }
  }

  async restart() {
    try { await api.restart(this.gen.name); } catch (e) { this.fail(e); }
  }

  async removeChannel() {
    this.confirming = false;
    try { await api.remove(this.gen.name); } catch (e) { this.fail(e); }
  }

  dot(state) {
    if (state === 'ok') return '';
    if (state === 'muted' || state === 'starting') return 'idle';
    if (state === 'late' || state.startsWith('retry')) return 'warn';
    return 'bad';
  }

  render() {
    const g = this.gen;
    if (!g) return nothing;
    const err = g.last_error;
    return html`
      <div class="top">
        <span class="state" title=${g.state}><span class="dot ${this.dot(g.state)}"></span><span class="label">${g.state}</span></span>
        <button class="x" title="Remove channel" @click=${() => (this.confirming = true)}>✕</button>
      </div>
      ${err && err.seconds_ago < 60
        ? html`<div class="err" title=${err.message}>${err.message}</div>`
        : html`<div class="stats" title="channels · late blocks · restarts">${g.channels === 2 ? 'st' : g.channels === 1 ? 'mono' : '–'} · L${g.late} · R${g.restarts}</div>`}
      <ws-fader .db=${g.gain_db} .peak=${g.peak} ?dimmed=${g.muted}
        @change=${(e) => this.patch({ gain_db: e.detail })}></ws-fader>
      ${this.confirming
        ? html`<div class="confirm">Remove?
            <div class="buttons">
              <button class="b yes" @click=${this.removeChannel}>Yes</button>
              <button class="b" @click=${() => (this.confirming = false)}>No</button>
            </div></div>`
        : html`<div class="buttons">
            <button class="b mute ${g.muted ? 'on' : ''}" title="Mute" @click=${() => this.patch({ muted: !g.muted })}>M</button>
            <button class="b" title="Restart, e.g. after rebuilding" @click=${this.restart}>↻</button>
          </div>`}
      <div class="name" title=${g.command.join(' ')}>${g.name}</div>
    `;
  }
}

customElements.define('ws-channel', WsChannel);
