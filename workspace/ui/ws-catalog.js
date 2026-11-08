// The generators available to add (`/api/catalog`). Refreshes every few
// seconds, since newly built generators appear in the catalog directory, and
// on the refresh button. Emits `add` with `{ generator, done }`.

import { LitElement, html, css } from './vendor/lit-all.min.js';
import { api } from './api.js';

const REFRESH_MS = 5000;

class WsCatalog extends LitElement {
  static properties = {
    entries: { state: true },
    loading: { state: true },
    error: { state: true },
    adding: { state: true }, // name -> count of adds in flight
  };

  static styles = css`
    :host { display: flex; flex-direction: column; background: var(--panel); }
    header {
      display: flex; align-items: center; justify-content: space-between;
      padding: 10px 12px; border-bottom: 1px solid var(--edge);
    }
    h2 { margin: 0; font-size: 12px; text-transform: uppercase; letter-spacing: 1px; color: var(--dim); font-weight: 600; }
    button {
      font: inherit; color: var(--text); background: var(--panel-2); border: 1px solid var(--edge);
      border-radius: 6px; padding: 4px 8px; cursor: pointer;
    }
    button:hover:not(:disabled) { border-color: var(--dim); }
    button:disabled { opacity: .5; cursor: progress; }
    .refresh.spin { animation: spin .8s linear infinite; }
    @keyframes spin { to { transform: rotate(360deg); } }
    ul { list-style: none; margin: 0; padding: 6px; overflow-y: auto; flex: 1; }
    li {
      display: grid; grid-template-columns: 1fr auto; gap: 2px 8px; align-items: center;
      padding: 8px; border-radius: 6px;
    }
    li:hover { background: var(--panel-2); }
    .name { font-weight: 600; }
    .ch { font-size: 10px; color: var(--dim); border: 1px solid var(--edge); border-radius: 3px; padding: 0 4px; margin-left: 6px; }
    .desc { grid-column: 1; font-size: 12px; color: var(--dim); }
    .add { grid-row: 1 / span 2; grid-column: 2; font-size: 16px; padding: 2px 10px; }
    .note { padding: 12px; color: var(--dim); font-size: 13px; }
    .note.err { color: var(--danger); }
  `;

  constructor() {
    super();
    this.entries = [];
    this.loading = false;
    this.error = null;
    this.adding = {};
  }

  connectedCallback() {
    super.connectedCallback();
    this.refresh();
    this.timer = setInterval(() => this.refresh(), REFRESH_MS);
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    clearInterval(this.timer);
  }

  async refresh() {
    if (this.loading) return;
    this.loading = true;
    try {
      this.entries = await api.catalog();
      this.error = null;
    } catch (e) {
      this.error = e.message;
    } finally {
      this.loading = false;
    }
  }

  add(name) {
    // Several adds of the same generator may be in flight; each becomes its
    // own channel (psybass, psybass-2, …).
    this.adding = { ...this.adding, [name]: (this.adding[name] || 0) + 1 };
    const done = () => {
      const n = (this.adding[name] || 1) - 1;
      this.adding = { ...this.adding, [name]: n };
    };
    this.dispatchEvent(new CustomEvent('add', { detail: { generator: name, done }, bubbles: true, composed: true }));
  }

  render() {
    return html`
      <header>
        <h2>Generators</h2>
        <button class="refresh ${this.loading ? 'spin' : ''}" title="Refresh the list" @click=${this.refresh}>↻</button>
      </header>
      ${this.error ? html`<div class="note err">${this.error}</div>` : ''}
      ${!this.error && this.entries.length === 0 && !this.loading
        ? html`<div class="note">No generators found in the catalog directory. Build some with <code>make build</code>.</div>`
        : ''}
      <ul>
        ${this.entries.map((e) => html`
          <li title=${e.path}>
            <span><span class="name">${e.name}</span><span class="ch">${e.channels === 2 ? 'stereo' : 'mono'}</span></span>
            <span class="desc">${e.description}</span>
            <button class="add" title="Add a channel with ${e.name}" @click=${() => this.add(e.name)}>
              ${this.adding[e.name] ? '…' : '+'}
            </button>
          </li>`)}
      </ul>
    `;
  }
}

customElements.define('ws-catalog', WsCatalog);
