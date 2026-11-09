// The plugins available to add (`/api/catalog`), in two sections:
// generators (+ adds a channel) and effects (+ adds a send effect, a bus every
// channel can send to; inserts are added from a channel strip). Refreshes
// every few seconds, since newly built plugins appear in the catalog
// directory, and on the refresh button.
//
// Emits `add` with `{ generator, done }`, `add-send` with `{ effect, done }`,
// and `catalog` with the entries after every refresh.

import { LitElement, html, css } from './vendor/lit-all.min.js';
import { api } from './api.js';
import { pluginColor, setCatalog } from './colors.js';

const REFRESH_MS = 5000;

/** Plugin categories in display order (gen_api::CATEGORIES); unknown ones sort after. */
const CATEGORIES = ['drums', 'bass', 'lead', 'pad', 'fx', 'dynamics', 'filter', 'distortion', 'modulation', 'delay', 'reverb', 'other'];
const rank = (c) => { const i = CATEGORIES.indexOf(c ?? 'other'); return i < 0 ? CATEGORIES.length : i; };
const byCategory = (a, b) => rank(a.category) - rank(b.category) || (a.category ?? '').localeCompare(b.category ?? '') || a.name.localeCompare(b.name);

/** How a generator comes into a playing mix, in words. */
function entryText(entry) {
  if (!entry || entry.mode === 'now') return 'starts straight away';
  if (entry.mode === 'bar') return 'enters on the next bar';
  if (entry.mode === 'fade') return `fades in over ${entry.bars} bar${entry.bars === 1 ? '' : 's'}`;
  return entry.mode;
}

class WsCatalog extends LitElement {
  static properties = {
    entries: { state: true },
    loading: { state: true },
    error: { state: true },
    adding: { state: true }, // name -> count of adds in flight
    open: { state: true }, // the one open category group, "kind:category"
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
    ul { list-style: none; margin: 0; padding: 4px; overflow-y: auto; flex: 1; }
    li.entry {
      display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 0 6px; align-items: center;
      padding: 3px 6px; border-radius: 4px; cursor: pointer; user-select: none; outline: none;
      border-left: 3px solid var(--c); margin-bottom: 2px;
    }
    li.entry:hover, li.entry:focus-visible { background: color-mix(in srgb, var(--c) 12%, var(--panel-2)); }
    li.entry:active { background: color-mix(in srgb, var(--c) 24%, var(--panel-2)); }
    li.entry.busy { cursor: progress; }
    .name { font-weight: 600; font-size: 12px; color: var(--c); }
    .top { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; line-height: 1.3; }
    .ch { font-size: 9px; color: var(--dim); margin-left: 4px; }
    .cat { font-size: 9px; color: var(--c); opacity: .75; margin-left: 4px; }
    .desc { grid-column: 1; font-size: 10px; line-height: 1.3; color: var(--dim); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .add {
      grid-row: 1 / span 2; grid-column: 2; font-size: 13px; line-height: 1; padding: 2px 6px;
      color: var(--dim); border: 1px solid var(--edge); border-radius: 4px;
    }
    li.entry:hover .add { color: var(--c); border-color: var(--c); }
    li.group {
      display: flex; align-items: center; gap: 6px; padding: 5px 6px; margin: 1px 0;
      border-radius: 4px; cursor: pointer; user-select: none; outline: none;
      font: 600 10px var(--sans); text-transform: uppercase; letter-spacing: 1px; color: var(--text);
    }
    li.group:hover, li.group:focus-visible { background: var(--panel-2); }
    li.group .arrow { width: 8px; color: var(--dim); transition: transform 120ms; }
    li.group.open .arrow { transform: rotate(90deg); }
    li.group .count { margin-left: auto; color: var(--dim); font-weight: 400; }
    li.group .dots { display: flex; gap: 2px; }
    li.group .dots span { width: 6px; height: 6px; border-radius: 50%; }
    li.section { display: block; background: none; padding: 6px 6px 2px; margin: 0; font-size: 10px; text-transform: uppercase; letter-spacing: 1px; color: var(--dim); font-weight: 600; }
    .note { padding: 12px; color: var(--dim); font-size: 13px; }
    .note.err { color: var(--danger); }
  `;

  constructor() {
    super();
    this.entries = [];
    this.loading = false;
    this.error = null;
    this.adding = {};
    try { this.open = localStorage.getItem('ws-catalog-open'); } catch { this.open = null; }
  }

  toggle(key) {
    this.open = (this.open ?? this.defaultOpen) === key ? '' : key;
    try { localStorage.setItem('ws-catalog-open', this.open ?? ''); } catch {}
  }

  /**
   * One accordion group per category; only one group is open at a time.
   * The header shows the category, a dot per plugin in its colour, and a count.
   */
  groups(entries, kind, verb, onAdd) {
    const cats = [];
    for (const e of entries) {
      const c = e.category ?? 'other';
      if (!cats.length || cats.at(-1).name !== c) cats.push({ name: c, items: [] });
      cats.at(-1).items.push(e);
    }
    return cats.map(({ name, items }) => {
      const key = `${kind}:${name}`;
      const open = (this.open ?? this.defaultOpen) === key;
      const key_ = (ev) => { if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); this.toggle(key); } };
      return html`
        <li class="group ${open ? 'open' : ''}" role="button" tabindex="0" aria-expanded=${open}
          @click=${() => this.toggle(key)} @keydown=${key_}>
          <span class="arrow">▸</span>${name}
          ${open ? '' : html`<span class="dots">${items.map((e) => html`<span style="background:${pluginColor(e.name)}" title=${e.name}></span>`)}</span>`}
          <span class="count">${items.length}</span>
        </li>
        ${open ? items.map((e) => this.item(e, verb, () => onAdd(e))) : ''}`;
    });
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
      const entries = await api.catalog();
      setCatalog(entries.map((e) => e.name));
      this.entries = entries;
      this.error = null;
      this.dispatchEvent(new CustomEvent('catalog', { detail: this.entries, bubbles: true, composed: true }));
    } catch (e) {
      this.error = e.message;
    } finally {
      this.loading = false;
    }
  }

  add(name, event = 'add', key = 'generator') {
    // Several adds of the same generator may be in flight; each becomes its
    // own channel (psybass, psybass-2, …).
    this.adding = { ...this.adding, [name]: (this.adding[name] || 0) + 1 };
    const done = () => {
      const n = (this.adding[name] || 1) - 1;
      this.adding = { ...this.adding, [name]: n };
    };
    this.dispatchEvent(new CustomEvent(event, { detail: { [key]: name, done }, bubbles: true, composed: true }));
  }

  item(e, verb, onAdd) {
    // The whole entry is the button: click (or Enter/Space) adds it.
    const key = (ev) => { if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); onAdd(); } };
    return html`
      <li class="entry ${this.adding[e.name] ? 'busy' : ''}" style="--c:${pluginColor(e.name)}"
        role="button" tabindex="0" title="${verb} ${e.name}${e.kind === 'effect' ? '' : ' (' + entryText(e.entry) + ')'}\n${e.description}\n${e.path}" @click=${onAdd} @keydown=${key}>
        <span class="top"><span class="name">${e.name}</span><span class="ch">${e.channels === 2 ? 'stereo' : 'mono'}</span></span>
        <span class="desc">${e.description}</span>
        <span class="add" aria-hidden="true">${this.adding[e.name] ? '…' : '+'}</span>
      </li>`;
  }

  render() {
    const gens = this.entries.filter((e) => e.kind !== 'effect').sort(byCategory);
    const fxs = this.entries.filter((e) => e.kind === 'effect').sort(byCategory);
    // First visit: open the first generator group.
    this.defaultOpen = gens.length ? `generator:${gens[0].category ?? 'other'}` : null;
    return html`
      <header>
        <h2>Plugins</h2>
        <button class="refresh ${this.loading ? 'spin' : ''}" title="Refresh the list" @click=${this.refresh}>↻</button>
      </header>
      ${this.error ? html`<div class="note err">${this.error}</div>` : ''}
      ${!this.error && this.entries.length === 0 && !this.loading
        ? html`<div class="note">No plugins found in the catalog directory. Build some with <code>make build</code>.</div>`
        : ''}
      <ul>
        ${gens.length ? html`<li class="section">Generators</li>` : ''}
        ${this.groups(gens, 'generator', 'Add a channel with', (e) => this.add(e.name))}
        ${fxs.length ? html`<li class="section">Effects</li>` : ''}
        ${this.groups(fxs, 'effect', 'Add a send effect (bus) with', (e) => this.add(e.name, 'add-send', 'effect'))}
      </ul>
    `;
  }
}

customElements.define('ws-catalog', WsCatalog);
