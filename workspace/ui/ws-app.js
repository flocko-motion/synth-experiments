// The page: transport on top, the catalog on the left, then the channel
// strips, the send effects' return strips and the master. Owns the live state from the event stream and
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
    catalog: { state: true },
    localOrder: { state: true }, // { generator: [names], return: [names] } until the API keeps order
  };

  static styles = css`
    :host { display: grid; grid-template-rows: auto 1fr; height: 100vh; }
    .body { display: grid; grid-template-columns: 210px 1fr; min-height: 0; }
    ws-catalog { border-right: 1px solid var(--edge); min-height: 0; }
    .board { display: flex; min-width: 0; min-height: 0; }
    .strips {
      flex: 1; display: flex; gap: 8px; padding: 12px;
      overflow: auto; align-items: flex-start;
    }
    .row {
      display: flex; gap: 8px; align-items: stretch; flex: 1 0 auto;
    }
    .returns { display: flex; gap: 8px; padding-left: 12px; border-left: 1px dashed var(--edge); }
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
    }
  `;

  constructor() {
    super();
    this.state = null;
    this.connected = false;
    this.toasts = [];
    this.catalog = [];
    try { this.localOrder = JSON.parse(localStorage.getItem('ws-strip-order') || '{}'); } catch { this.localOrder = {}; }
  }

  /** Applies a browser-kept order (if any) on top of the daemon's order. */
  ordered(list, kind) {
    const order = this.localOrder?.[kind];
    if (!order?.length) return list;
    const rank = (n) => { const i = order.indexOf(n); return i < 0 ? Infinity : i; };
    return [...list].sort((a, b) => rank(a.name) - rank(b.name));
  }

  // Drag and drop of strips: channels among channels, returns among returns.
  stripAt(e) {
    return e.composedPath().find((el) => el.tagName === 'WS-CHANNEL');
  }

  clearDrop() {
    this.dropTarget?.removeAttribute('drop');
    this.dropTarget = null;
  }

  dragOver(e) {
    const d = this.drag;
    const strip = this.stripAt(e);
    if (!d || !strip || strip.kind !== d.kind || strip.gen?.name === d.name) return this.clearDrop();
    e.preventDefault();
    e.dataTransfer.dropEffect = 'move';
    const r = strip.getBoundingClientRect();
    const side = e.clientX < r.left + r.width / 2 ? 'before' : 'after';
    if (this.dropTarget !== strip) this.clearDrop();
    this.dropTarget = strip;
    strip.setAttribute('drop', side);
  }

  async drop(e) {
    const d = this.drag;
    const strip = this.dropTarget;
    const side = strip?.getAttribute('drop');
    this.clearDrop();
    if (!d || !strip) return;
    e.preventDefault();
    const list = d.kind === 'return' ? this.returnsNow : this.gensNow;
    const names = list.map((g) => g.name).filter((n) => n !== d.name);
    let i = names.indexOf(strip.gen.name) + (side === 'after' ? 1 : 0);
    names.splice(i, 0, d.name);
    // Show it right away; the daemon's order replaces this once it keeps order.
    this.setLocalOrder(d.kind, names);
    try {
      if (d.kind === 'return') await api.patchEffect(d.name, { position: i });
      else await api.patch(d.name, { position: i });
      this.setLocalOrder(d.kind, null); // the daemon has it
    } catch (err) {
      if (err.status !== 400) this.toast(`${d.name}: ${err.message}`);
      // 400: the daemon doesn't keep order yet; keep it in this browser.
    }
  }

  setLocalOrder(kind, names) {
    const next = { ...this.localOrder };
    if (names) next[kind] = names; else delete next[kind];
    this.localOrder = next;
    try { localStorage.setItem('ws-strip-order', JSON.stringify(next)); } catch {}
  }

  connectedCallback() {
    super.connectedCallback();
    this.unsubscribe = subscribe({
      onState: (s) => { this.state = s; this.connected = true; },
      onConnection: (up) => { this.connected = up; },
    });
    // Components report failed requests by bubbling `ws-error`.
    this.addEventListener('ws-error', (e) => this.toast(e.detail));
    this.addEventListener('strip-drag', (e) => { this.drag = e.detail; if (!e.detail) this.clearDrop(); });
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
    const { generator, gain_db, done } = e.detail;
    try {
      await api.add(gain_db === undefined ? { generator } : { generator, gain_db });
    } catch (err) {
      this.toast(`${generator}: ${err.message}`);
    } finally {
      done?.();
    }
  }

  async addSend(e) {
    const { effect, gain_db, done } = e.detail;
    try {
      await api.addSend(gain_db === undefined ? { effect } : { effect, gain_db });
    } catch (err) {
      this.toast(`${effect}: ${err.message}`);
    } finally {
      done?.();
    }
  }

  render() {
    const s = this.state;
    const gens = this.ordered(s?.generators ?? [], 'generator');
    this.gensNow = gens;
    const effects = s?.effects ?? [];
    const byName = new Map(effects.map((fx) => [fx.name, fx]));
    const returns = this.ordered(effects.filter((fx) => fx.role === 'send'), 'return');
    this.returnsNow = returns;

    const sendTo = returns;
    const fxCatalog = this.catalog.filter((c) => c.kind === 'effect');
    const insertsOf = (g) => (g.inserts ?? []).map((n) => byName.get(n)).filter(Boolean);
    return html`
      <ws-transport .state=${s}></ws-transport>
      <div class="body">
        <ws-catalog @add=${this.add} @add-send=${this.addSend} @catalog=${(e) => (this.catalog = e.detail)}></ws-catalog>
        <div class="board">
          <div class="strips"><div class="row" @dragover=${this.dragOver} @drop=${this.drop}>
            ${repeat(gens, (g) => g.name, (g) => html`
              <ws-channel kind="generator" .gen=${g} .sendTo=${sendTo} .inserts=${insertsOf(g)} .fxCatalog=${fxCatalog}></ws-channel>`)}
            ${returns.length
              ? html`<div class="returns">
                  ${repeat(returns, (fx) => fx.name, (fx) => html`<ws-channel kind="return" .gen=${fx} .duck=${s.duck}></ws-channel>`)}
                </div>`
              : ''}
            <ws-master .state=${s}></ws-master>
          </div></div>
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
