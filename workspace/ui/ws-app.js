// The page: transport on top, the catalog on the left, the channel strips
// and master on the right. Owns the live state from the event stream and
// hands it down; components send changes straight to the API and the next
// state event confirms them.

import { LitElement, html, css } from './vendor/lit-all.min.js';
import { repeat } from './vendor/lit-all.min.js';
import { api, subscribe } from './api.js';
import './ws-transport.js';
import './ws-catalog.js';
import './ws-channel.js';
import './ws-master.js';

class WsApp extends LitElement {
  static properties = {
    state: { state: true },
    connected: { state: true },
    toasts: { state: true },
  };

  static styles = css`
    :host { display: grid; grid-template-rows: auto 1fr; height: 100vh; }
    .body { display: grid; grid-template-columns: 280px 1fr; min-height: 0; }
    ws-catalog { border-right: 1px solid var(--edge); min-height: 0; }
    .board { display: flex; min-width: 0; min-height: 0; }
    .strips {
      flex: 1; display: flex; gap: 8px; padding: 12px;
      overflow-x: auto; overflow-y: hidden; align-items: stretch;
    }
    ws-master { border-left: 1px solid var(--edge); padding: 12px; }
    .empty {
      margin: auto; color: var(--dim); text-align: center; max-width: 280px;
    }
    .offline {
      position: fixed; inset: auto 0 0 0; padding: 8px 16px; text-align: center;
      background: #3a1515; color: #fecaca; font-size: 13px;
    }
    .toasts { position: fixed; right: 16px; bottom: 16px; display: grid; gap: 8px; max-width: min(420px, calc(100vw - 32px)); }
    .toast {
      background: #3a1515; color: #fecaca; border: 1px solid #7f1d1d; border-radius: 6px;
      padding: 8px 12px; font-size: 13px; cursor: pointer; white-space: pre-wrap;
    }
    @media (max-width: 720px) {
      .body { grid-template-columns: 1fr; grid-template-rows: auto 1fr; }
      ws-catalog { border-right: 0; border-bottom: 1px solid var(--edge); max-height: 35vh; }
      .board { flex-direction: column; }
      ws-master { border-left: 0; border-top: 1px solid var(--edge); }
    }
  `;

  constructor() {
    super();
    this.state = null;
    this.connected = false;
    this.toasts = [];
  }

  connectedCallback() {
    super.connectedCallback();
    this.unsubscribe = subscribe({
      onState: (s) => { this.state = s; this.connected = true; },
      onConnection: (up) => { this.connected = up; },
    });
    // Components report failed requests by bubbling `ws-error`.
    this.addEventListener('ws-error', (e) => this.toast(e.detail));
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    this.unsubscribe?.();
  }

  toast(message) {
    const id = Math.random();
    this.toasts = [...this.toasts, { id, message }];
    setTimeout(() => this.dismiss(id), 6000);
  }

  dismiss(id) {
    this.toasts = this.toasts.filter((t) => t.id !== id);
  }

  async add(e) {
    const { generator, done } = e.detail;
    try {
      await api.add({ generator });
    } catch (err) {
      this.toast(`${generator}: ${err.message}`);
    } finally {
      done?.();
    }
  }

  render() {
    const s = this.state;
    const gens = s?.generators ?? [];
    return html`
      <ws-transport .state=${s}></ws-transport>
      <div class="body">
        <ws-catalog @add=${this.add}></ws-catalog>
        <div class="board">
          <div class="strips">
            ${gens.length === 0
              ? html`<p class="empty">No channels yet. Add a generator from the list; the same one can be added more than once.</p>`
              : repeat(gens, (g) => g.name, (g) => html`<ws-channel .gen=${g}></ws-channel>`)}
          </div>
          <ws-master .state=${s}></ws-master>
        </div>
      </div>
      ${this.connected ? '' : html`<div class="offline">Not connected to the workspace daemon; retrying…</div>`}
      <div class="toasts">
        ${this.toasts.map((t) => html`<div class="toast" @click=${() => this.dismiss(t.id)}>${t.message}</div>`)}
      </div>
    `;
  }
}

customElements.define('ws-app', WsApp);
