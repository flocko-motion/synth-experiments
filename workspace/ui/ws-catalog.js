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

// Press and hold an entry to pick its starting volume before it's added.
const HOLD_MS = 180;
const VOL_MIN = -60; // the far left means -96 dB (silent)
const VOL_MAX = 6;

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
    picking: { state: true }, // { name, db } while choosing a starting volume
    favs: { state: true }, // starred plugin names (by the daemon; browser storage until it supports them)
    open: { state: true }, // the one open category group, "kind:category"
    collapsed: { state: true }, // folded to a slim strip; click the title to toggle
  };

  static styles = css`
    :host { display: flex; flex-direction: column; background: var(--panel); width: 210px; }
    /* Slim title like the sequencer's; clicking it folds the pane. */
    header {
      display: flex; align-items: center; justify-content: space-between;
      padding: 4px 8px 4px 12px; border-bottom: 1px solid var(--edge);
    }
    h2 {
      margin: 0; font: 600 10px var(--sans); text-transform: uppercase; letter-spacing: 1px; color: var(--text);
      cursor: pointer; white-space: nowrap;
    }
    h2:hover { color: var(--accent); }
    h2::before { content: '▾'; display: inline-block; width: 11px; }
    header .refresh { padding: 0 6px; font-size: 11px; border-radius: 4px; }
    /* Folded: a narrow strip with the title running down it. */
    :host([collapsed]) { width: 22px; }
    :host([collapsed]) header { padding: 6px 0; border-bottom: 0; justify-content: center; }
    :host([collapsed]) h2 { writing-mode: vertical-rl; }
    :host([collapsed]) h2::before { content: '▸'; height: 11px; width: auto; }
    :host([collapsed]) header .refresh, :host([collapsed]) ul, :host([collapsed]) .note { display: none; }
    /* Phones: the pane is a row above the strips, so fold it to its title line. */
    @media (max-width: 720px) {
      :host([collapsed]) header { padding: 4px 12px; justify-content: flex-start; }
      :host([collapsed]) h2 { writing-mode: horizontal-tb; }
      :host([collapsed]) h2::before { width: 11px; height: auto; }
    }
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
      position: relative; overflow: hidden; touch-action: none;
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
    .busy-mark { grid-row: 1 / span 2; grid-column: 2; font-size: 12px; color: var(--c); }
    /* Hold to choose the starting volume: the entry turns into a level bar. */
    li.entry.picking { cursor: ew-resize; background: var(--panel-2); }
    li.entry > .top, li.entry > .desc, li.entry > .busy-mark, li.entry > .star { position: relative; z-index: 1; }
    .star {
      grid-row: 1 / span 2; grid-column: 2; background: none; border: 0; padding: 2px 4px; cursor: pointer;
      font-size: 14px; line-height: 1; color: var(--dim); opacity: .5;
    }
    li.entry:hover .star, .star:focus-visible { opacity: 1; }
    .star:hover { color: var(--accent); }
    .star.on { color: var(--accent); opacity: 1; }
    .vol { position: absolute; inset: 0; pointer-events: none; z-index: 0; }
    .vol .fill { position: absolute; inset: 0 auto 0 0; background: color-mix(in srgb, var(--c) 45%, transparent); }
    .vol .zero { position: absolute; top: 0; bottom: 0; width: 1px; background: var(--text); opacity: .25; }

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
    try { this.favs = new Set(JSON.parse(localStorage.getItem('ws-favs') || '[]')); } catch { this.favs = new Set(); }
    try { this.open = localStorage.getItem('ws-catalog-open'); } catch { this.open = null; }
    try { this.collapsed = localStorage.getItem('ws-catalog-collapsed') === '1'; } catch { this.collapsed = false; }
  }

  updated() {
    this.toggleAttribute('collapsed', this.collapsed);
  }

  toggleCollapsed() {
    this.collapsed = !this.collapsed;
    try { localStorage.setItem('ws-catalog-collapsed', this.collapsed ? '1' : '0'); } catch {}
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
        ${open ? items.map((e) => this.item(e, verb, (db) => onAdd(e, db))) : ''}`;
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
      this.syncFavs(entries);
      this.entries = entries;
      this.error = null;
      this.dispatchEvent(new CustomEvent('catalog', { detail: this.entries, bubbles: true, composed: true }));
    } catch (e) {
      this.error = e.message;
    } finally {
      this.loading = false;
    }
  }

  add(name, event = 'add', key = 'generator', gain_db) {
    // Several adds of the same generator may be in flight; each becomes its
    // own channel (psybass, psybass-2, …).
    this.adding = { ...this.adding, [name]: (this.adding[name] || 0) + 1 };
    const done = () => {
      const n = (this.adding[name] || 1) - 1;
      this.adding = { ...this.adding, [name]: n };
    };
    this.dispatchEvent(new CustomEvent(event, { detail: { [key]: name, gain_db, done }, bubbles: true, composed: true }));
  }

  // Click: add at 0 dB. Press and hold: the entry becomes a level bar; move
  // left/right to choose the starting volume, release to add, Esc cancels.
  down(ev, e, onAdd) {
    if (ev.button !== 0) return;
    const el = ev.currentTarget;
    try { el.setPointerCapture(ev.pointerId); } catch {}
    this.press = { name: e.name, onAdd, el, x: ev.clientX, cancelled: false };
    clearTimeout(this.holdTimer);
    this.holdTimer = setTimeout(() => {
      if (this.press?.name === e.name) this.picking = { name: e.name, db: this.dbAt(this.press) };
    }, HOLD_MS);
  }

  /** The level under the pointer: the bar's edge follows the mouse across the entry. */
  dbAt(p) {
    const r = p.el.getBoundingClientRect();
    const t = Math.min(1, Math.max(0, (p.x - r.left) / r.width));
    return Math.round((VOL_MIN + t * (VOL_MAX - VOL_MIN)) * 2) / 2;
  }

  move(ev) {
    const p = this.press;
    if (!p) return;
    p.x = ev.clientX;
    if (this.picking) this.picking = { name: p.name, db: this.dbAt(p) };
  }

  up() {
    clearTimeout(this.holdTimer);
    const p = this.press;
    const picking = this.picking;
    this.press = null;
    this.picking = null;
    if (!p || p.cancelled) return;
    if (!picking) return p.onAdd(undefined);
    p.onAdd(picking.db <= VOL_MIN ? -96 : picking.db);
  }

  cancel() {
    clearTimeout(this.holdTimer);
    if (this.press) this.press.cancelled = true;
    this.picking = null;
  }

  /** True once the daemon keeps favourites (catalog entries carry `favorite`). */
  serverFavs(entries) {
    return entries.length > 0 && 'favorite' in entries[0];
  }

  /**
   * With daemon support, favourites come from the catalog; stars set in this
   * browser before that are pushed up once and then dropped locally.
   */
  syncFavs(entries) {
    if (!this.serverFavs(entries)) return;
    let local = [];
    try { local = JSON.parse(localStorage.getItem('ws-favs') || '[]'); } catch {}
    const names = new Set(entries.map((e) => e.name));
    const toPush = local.filter((n) => names.has(n) && !entries.find((e) => e.name === n)?.favorite);
    for (const n of toPush) api.setFavorite(n, true).catch(() => {});
    if (local.length) try { localStorage.removeItem('ws-favs'); } catch {}
    this.favs = new Set([...entries.filter((e) => e.favorite).map((e) => e.name), ...toPush]);
  }

  async toggleFav(name) {
    const on = !this.favs.has(name);
    const favs = new Set(this.favs);
    on ? favs.add(name) : favs.delete(name);
    this.favs = favs; // show it right away
    if (this.serverFavs(this.entries)) {
      try {
        await api.setFavorite(name, on);
      } catch (e) {
        this.dispatchEvent(new CustomEvent('ws-error', { detail: `favourite ${name}: ${e.message}`, bubbles: true, composed: true }));
        this.refresh();
      }
    } else {
      try { localStorage.setItem('ws-favs', JSON.stringify([...favs])); } catch {}
    }
  }

  /** The star: toggles the favourite and never adds the plugin. */
  star(e) {
    const on = this.favs.has(e.name);
    const stop = (ev) => ev.stopPropagation();
    return html`<button class="star ${on ? 'on' : ''}" aria-pressed=${on}
      title=${on ? 'Remove from favourites' : 'Add to favourites (shown at the top)'}
      @pointerdown=${stop} @pointerup=${stop} @keydown=${stop}
      @click=${(ev) => { ev.stopPropagation(); this.toggleFav(e.name); }}>${on ? '★' : '☆'}</button>`;
  }

  item(e, verb, onAdd) {
    const key = (ev) => {
      if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); onAdd(undefined); }
      else if (ev.key === 'Escape') this.cancel();
    };
    const pick = this.picking?.name === e.name ? this.picking : null;
    const pos = (db) => ((db - VOL_MIN) / (VOL_MAX - VOL_MIN)) * 100;
    return html`
      <li class="entry ${this.adding[e.name] ? 'busy' : ''} ${pick ? 'picking' : ''}" style="--c:${pluginColor(e.name)}"
        role="button" tabindex="0"
        title="${verb} ${e.name}${e.kind === 'effect' ? '' : ' (' + entryText(e.entry) + ')'}: click to add, or hold and move left/right to pick its volume (0 dB at the faint line)\n${e.description}\n${e.path}"
        @pointerdown=${(ev) => this.down(ev, e, onAdd)} @pointermove=${this.move} @pointerup=${this.up}
        @pointercancel=${this.cancel} @keydown=${key}>
        <span class="top"><span class="name">${e.name}</span><span class="ch">${e.channels === 2 ? 'stereo' : 'mono'}</span></span>
        <span class="desc">${e.description}</span>
        ${this.adding[e.name] ? html`<span class="busy-mark" aria-hidden="true">…</span>` : this.star(e)}
        ${pick ? html`<div class="vol">
          <div class="fill" style="width:${pos(pick.db)}%"></div>
          <div class="zero" style="left:${pos(0)}%"></div>
        </div>` : ''}
      </li>`;
  }

  render() {
    const gens = this.entries.filter((e) => e.kind !== 'effect').sort(byCategory);
    const fxs = this.entries.filter((e) => e.kind === 'effect').sort(byCategory);
    // Starred plugins, always open at the top (they also stay in their group).
    const favs = [...gens, ...fxs].filter((e) => this.favs.has(e.name));
    // First visit: open the first generator group.
    this.defaultOpen = gens.length ? `generator:${gens[0].category ?? 'other'}` : null;
    return html`
      <header>
        <h2 role="button" tabindex="0" aria-expanded=${!this.collapsed} title=${this.collapsed ? 'Show the plugins' : 'Hide the plugins'}
          @click=${this.toggleCollapsed} @keydown=${(e) => (e.key === 'Enter' || e.key === ' ') && (e.preventDefault(), this.toggleCollapsed())}>Plugins</h2>
        <button class="refresh ${this.loading ? 'spin' : ''}" title="Refresh the list" @click=${this.refresh}>↻</button>
      </header>
      ${this.error ? html`<div class="note err">${this.error}</div>` : ''}
      ${!this.error && this.entries.length === 0 && !this.loading
        ? html`<div class="note">No plugins found in the catalog directory. Build some with <code>make build</code>.</div>`
        : ''}
      <ul>
        ${favs.length ? html`<li class="section">★ Favorites</li>` : ''}
        ${favs.map((e) => e.kind === 'effect'
          ? this.item(e, 'Add a send effect (bus) with', (db) => this.add(e.name, 'add-send', 'effect', db))
          : this.item(e, 'Add a channel with', (db) => this.add(e.name, 'add', 'generator', db)))}
        ${gens.length ? html`<li class="section">Generators</li>` : ''}
        ${this.groups(gens, 'generator', 'Add a channel with', (e, db) => this.add(e.name, 'add', 'generator', db))}
        ${fxs.length ? html`<li class="section">Effects</li>` : ''}
        ${this.groups(fxs, 'effect', 'Add a send effect (bus) with', (e, db) => this.add(e.name, 'add-send', 'effect', db))}
      </ul>
    `;
  }
}

customElements.define('ws-catalog', WsCatalog);
