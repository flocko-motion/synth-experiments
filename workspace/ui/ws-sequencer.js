// The sequencer band (docs/sequencer.md): one row per lane layer, each a
// 16-bar grid with the playhead, and the lane's banks A-D on the right.
//
// The grid shows the bank being edited, which is the playing one unless you
// pick another. Editing the playing bank is live; editing another prepares
// it, and CUE makes it play from the next epoch. Drag across empty bars to
// make a region; drag a region to move it, or its edges to resize it (what
// it lands on is trimmed away, as the grid shows while dragging); click it
// to change it; right-click it, or select it and press Delete, to remove
// it. The daemon owns
// everything: every edit replaces the whole bank (PUT) and the next state
// event shows it.

import { LitElement, html, css, nothing } from './vendor/lit-all.min.js';
import { api } from './api.js';

const BANKS = ['A', 'B', 'C', 'D'];
const ROOTS = ['C', 'C#', 'D', 'D#', 'E', 'F', 'F#', 'G', 'G#', 'A', 'A#', 'B'];
const SCALES = ['phrygian', 'minor', 'harmonic-minor', 'dorian', 'major'];
const MARKS = ['quiet', 'rise', 'peak', 'break'];
const FEELS = ['half', 'double', 'quad', 'triplet'];
const FEEL_LABEL = { half: '½×', double: '2×', quad: '4×', triplet: '3:2' };
const QUALITIES = ['', 'M', 'm', 'dim', 'aug', 'sus2', 'sus4', '7', 'M7', 'm7', 'm7b5', 'dim7'];
/** Display order: dynamics and tempo together, then harmony with the key
 * above the chords counted from it. */
const LANE_ORDER = ['dynamics', 'tempo', 'harmony'];
const GROUP_STARTS = ['harmony'];
const LAYER_ORDER = ['marks', 'feels', 'keys', 'chords'];
const LAYER_LABEL = { marks: 'dynamics', chords: 'chords', keys: 'key', feels: 'tempo' };
const MARK_COLOR = { quiet: '#60a5fa', rise: '#f2a03d', peak: '#f472b6', break: '#a78bfa' };

/** Chords and keys are one-bar markers that hold until the next one. */
const isMarker = (layer) => layer === 'chords' || layer === 'keys';

/**
 * Markers drawn as what they hold: each up to the next marker, plus (when
 * the first marker isn't on bar 1) the last one carried around the loop
 * into the bars before it. `index` is the marker's index in `markers`.
 */
function held(markers) {
  const spans = markers.map((r, i) => ({ ...r, bars: (markers[i + 1]?.start ?? 17) - r.start, index: i }));
  if (markers.length && markers[0].start > 1) {
    spans.unshift({ ...markers.at(-1), start: 1, bars: markers[0].start - 1, index: markers.length - 1, carry: true });
  }
  return spans;
}

/** "b7M7" → { shift: 'b', degree: 7, quality: 'M7' }. */
function splitChord(v) {
  const m = /^([b#]?)([1-7])(.*)$/.exec(v ?? '');
  return m ? { shift: m[1], degree: +m[2], quality: m[3] } : { shift: '', degree: 1, quality: '' };
}

/**
 * `regions` with `r` put in: whatever it covers of other regions is cut
 * away (they're trimmed or split), so regions never overlap.
 */
function place(regions, r) {
  const end = r.start + r.bars - 1;
  const out = [];
  for (const o of regions) {
    const oEnd = o.start + o.bars - 1;
    if (oEnd < r.start || o.start > end) { out.push(o); continue; }
    if (o.start < r.start) out.push({ ...o, bars: r.start - o.start });
    if (oEnd > end) out.push({ ...o, start: end + 1, bars: oEnd - end });
  }
  out.push(r);
  return out.sort((a, b) => a.start - b.start);
}

class WsSequencer extends LitElement {
  static properties = {
    seq: { attribute: false },     // state.sequencer
    beat: { attribute: false },    // state.beat, the audible position
    editing: { state: true },      // lane → bank shown in its grid, when not the playing one
    drag: { state: true },         // { lane, layer, mode, from, to, index?, orig? } while painting, moving or resizing
    editor: { state: true },       // { lane, layer, start, bars, index? }: the region being edited
    confirmClear: { state: true }, // lane whose CLEAR was clicked once
    copyFrom: { state: true },
    copyTo: { state: true },
    collapsed: { state: true },    // only the head line shows; click the title to toggle
  };

  static styles = css`
    :host {
      display: block; background: var(--panel); border-bottom: 1px solid var(--edge);
      padding: 5px 16px 6px; user-select: none; min-width: 0;
    }
    .head { display: flex; align-items: center; gap: 12px; margin-bottom: 3px; font-size: 11px; color: var(--dim); }
    .title { font: 600 10px var(--sans); letter-spacing: 1px; text-transform: uppercase; color: var(--text); cursor: pointer; }
    .title:hover { color: var(--accent); }
    .title::before { content: '▾'; display: inline-block; width: 11px; }
    :host([collapsed]) .title::before { content: '▸'; }
    :host([collapsed]) .head { margin-bottom: 0; }
    .head .spacer { flex: 1; }
    /* The toolbar takes what's left of the line; the rest keeps its size. */
    .head > :not(.editor) { flex: none; }
    .head > .editor { flex: 1 1 0; }
    .lane { display: grid; grid-template-columns: 84px 1fr 236px; gap: 0 10px; align-items: stretch; padding: 2px 0; }
    .lane + .lane { border-top: 1px dashed color-mix(in srgb, var(--edge) 70%, transparent); }
    /* Groups: dynamics with tempo, then harmony (key and chords). */
    .lane.group + .lane.group, .lane + .lane.group { border-top: 1px solid var(--edge); margin-top: 3px; padding-top: 5px; }
    .labels, .grids { display: grid; gap: 2px; }
    .label { font: 600 10px/18px var(--sans); color: var(--dim); text-transform: uppercase; letter-spacing: .5px; white-space: nowrap; }
    .track {
      position: relative; height: 18px; border-radius: 3px; cursor: crosshair; touch-action: none;
      display: grid; grid-template-columns: repeat(16, 1fr);
      background: repeating-linear-gradient(90deg, transparent 0 calc(100% / 16 - 1px), var(--edge) 0 calc(100% / 16));
      background-color: var(--panel-2);
    }
    .track.other { background-color: #1f2228; outline: 1px dashed var(--edge); }
    .cell.cycle { border-left: 1px solid #4a4f56; }
    .region {
      grid-row: 1; margin: 2px 1px; border-radius: 3px; overflow: hidden; cursor: pointer;
      font: 600 10px/14px var(--mono); padding: 0 7px; white-space: nowrap; text-overflow: ellipsis;
      color: #111; position: relative; z-index: 1; cursor: grab;
    }
    .grip { position: absolute; top: 0; bottom: 0; width: 6px; cursor: ew-resize; z-index: 2; }
    .grip.l { left: 0; }
    .grip.r { right: 0; }
    .grip:hover { background: rgb(0 0 0 / .25); }
    .track.dragging, .track.dragging .region { cursor: grabbing; }
    .track.resizing, .track.resizing .region { cursor: ew-resize; }
    .region.chord { background: #d8dadd; }
    .region.key { background: #7f8db0; }
    .region.feel { background: #4ade80; }
    .region.mark { color: #111; }
    .region.sel { outline: 2px solid var(--text); outline-offset: 1px; }
    .region.marker { border-left: 3px solid rgb(0 0 0 / .45); border-radius: 1px 3px 3px 1px; }
    .region.carry { opacity: .45; font-style: italic; border-left: 0; cursor: crosshair; }
    .paint { grid-row: 1; margin: 1px 0; background: color-mix(in srgb, var(--accent) 35%, transparent); border: 1px solid var(--accent); border-radius: 3px; z-index: 2; pointer-events: none; }
    .playhead { position: absolute; top: -2px; bottom: -2px; width: 2px; background: var(--led-beat); z-index: 3; pointer-events: none; box-shadow: 0 0 4px var(--led-beat); }
    .other .playhead { opacity: .35; }
    .side { display: flex; align-items: center; gap: 4px; }
    .side select { width: 62px; }
    button, select {
      font: 600 10px var(--sans); color: var(--text); background: var(--panel-2);
      border: 1px solid var(--edge); border-radius: 4px; padding: 2px 6px; cursor: pointer;
    }
    button:hover { border-color: var(--dim); }
    button:disabled { opacity: .35; cursor: default; }
    .bank { width: 26px; position: relative; font: 700 11px var(--mono); }
    .bank.filled::after { content: ''; position: absolute; right: 3px; top: 3px; width: 4px; height: 4px; border-radius: 50%; background: var(--dim); }
    .bank.playing { background: var(--accent); border-color: var(--accent); color: #1a1205; }
    .bank.playing.filled::after { background: #1a1205; }
    .bank.cued { border-color: var(--accent); animation: blink .5s steps(1) infinite; }
    .bank.editing { box-shadow: 0 0 0 1px var(--text); }
    @keyframes blink { 50% { background: color-mix(in srgb, var(--accent) 45%, var(--panel-2)); } }
    .cue.on { background: var(--accent); border-color: var(--accent); color: #1a1205; }
    .danger { color: var(--danger); }
    /* Always there at one fixed height, so the band never resizes when a
       region is picked; one line that scrolls sideways if it must. */
    .editor {
      flex: 1; min-width: 0; padding: 0 6px; height: 24px; box-sizing: border-box;
      border: 1px solid var(--edge); border-radius: 4px; background: var(--panel-2);
      display: flex; flex-wrap: nowrap; align-items: center; gap: 8px; font-size: 10px;
      overflow-x: auto; overflow-y: hidden; scrollbar-width: none; white-space: nowrap;
    }
    .editor button, .editor select { padding: 1px 5px; flex: none; }
    .editor .hint { color: var(--dim); overflow: hidden; text-overflow: ellipsis; }
    .editor .group { display: flex; gap: 2px; align-items: center; flex: none; }
    .editor button.on { background: var(--accent); border-color: var(--accent); color: #1a1205; }
    .editor input[type=range] { width: 90px; height: 12px; accent-color: var(--accent); }
    .editor .spacer { flex: 1; }
    @media (max-width: 720px) {
      .lane { grid-template-columns: 60px 1fr; }
      .side { grid-column: 2; flex-wrap: wrap; padding-top: 4px; }
    }
  `;

  constructor() {
    super();
    this.editing = {};
    this.drag = null;
    this.editor = null;
    this.confirmClear = null;
    this.copyFrom = 'A';
    this.copyTo = 'B';
    try { this.collapsed = localStorage.getItem('ws-seq-collapsed') === '1'; } catch { this.collapsed = false; }
  }

  toggle() {
    this.collapsed = !this.collapsed;
    try { localStorage.setItem('ws-seq-collapsed', this.collapsed ? '1' : '0'); } catch {}
  }

  updated() {
    this.toggleAttribute('collapsed', this.collapsed);
  }

  connectedCallback() {
    super.connectedCallback();
    // Delete or Backspace removes the selected region, unless typing.
    this.onKey = (e) => {
      if ((e.key !== 'Delete' && e.key !== 'Backspace') || !this.current()) return;
      if (e.composedPath().some((el) => ['INPUT', 'SELECT', 'TEXTAREA'].includes(el.tagName))) return;
      e.preventDefault();
      this.remove();
    };
    window.addEventListener('keydown', this.onKey);
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    window.removeEventListener('keydown', this.onKey);
  }

  fail(err) {
    this.dispatchEvent(new CustomEvent('ws-error', { detail: `sequencer: ${err.message}`, bubbles: true, composed: true }));
  }

  laneOf(name) {
    return this.seq?.lanes.find((l) => l.lane === name);
  }

  /** The bank a lane's grid shows. */
  shown(l) {
    return this.editing[l.lane] ?? l.playing;
  }

  async saveLayer(laneName, layer, regions) {
    const l = this.laneOf(laneName);
    const bank = this.shown(l);
    const content = { ...l.banks[bank], [layer]: regions };
    try {
      await api.setBank(laneName, bank, content);
    } catch (err) {
      this.fail(err);
    }
  }

  regionsOf(l, layer) {
    return l.banks[this.shown(l)]?.[layer] ?? [];
  }

  // ------------------------------------------------------------ painting

  barAt(track, e) {
    const r = track.getBoundingClientRect();
    return Math.min(16, Math.max(1, Math.floor(((e.clientX - r.left) / r.width) * 16) + 1));
  }

  down(e, lane, layer) {
    if (e.button !== 0) return;
    const track = e.currentTarget;
    track.setPointerCapture(e.pointerId);
    const bar = this.barAt(track, e);
    if (isMarker(layer)) {
      // Its own bar picks a marker up; any other bar starts a new one.
      const markers = this.regionsOf(this.laneOf(lane), layer);
      const index = markers.findIndex((r) => r.start === bar);
      this.drag = index < 0
        ? { lane, layer, mode: 'paint', from: bar, to: bar, track }
        : { lane, layer, mode: 'move', index, orig: markers[index], from: bar, to: bar, track };
      return;
    }
    const path = e.composedPath();
    const hit = path.find((el) => el.dataset?.index !== undefined);
    if (!hit) {
      this.drag = { lane, layer, mode: 'paint', from: bar, to: bar, track };
      return;
    }
    const index = +hit.dataset.index;
    const grip = path.find((el) => el.dataset?.grip)?.dataset.grip;
    const mode = grip === 'l' ? 'start' : grip === 'r' ? 'end' : 'move';
    this.drag = { lane, layer, mode, index, orig: this.regionsOf(this.laneOf(lane), layer)[index], from: bar, to: bar, track };
  }

  /** Where a moved or resized region would land, within bars 1-16. */
  dragged(d) {
    const o = d.orig;
    const end = o.start + o.bars - 1;
    const delta = d.to - d.from;
    if (d.mode === 'move') return { ...o, start: Math.min(17 - o.bars, Math.max(1, o.start + delta)) };
    if (d.mode === 'start') {
      const start = Math.min(end, Math.max(1, o.start + delta));
      return { ...o, start, bars: end - start + 1 };
    }
    const last = Math.max(o.start, Math.min(16, end + delta));
    return { ...o, bars: last - o.start + 1 };
  }

  /** A layer's regions as the grid shows them: mid-drag, with the dragged one where it would land. */
  shownRegions(l, layer) {
    const regions = this.regionsOf(l, layer);
    const d = this.drag;
    if (!d || d.mode === 'paint' || d.lane !== l.lane || d.layer !== layer || d.to === d.from) return { regions, moving: null };
    const moving = this.dragged(d);
    return { regions: place(regions.filter((_, i) => i !== d.index), moving), moving };
  }

  move(e) {
    if (!this.drag) return;
    const to = this.barAt(this.drag.track, e);
    if (to !== this.drag.to) this.drag = { ...this.drag, to };
  }

  async up() {
    const d = this.drag;
    if (!d) return;
    this.drag = null;
    const l = this.laneOf(d.lane);
    if (d.mode === 'paint') {
      this.editor = isMarker(d.layer)
        ? { lane: d.lane, layer: d.layer, start: d.from, bars: 1 }
        : { lane: d.lane, layer: d.layer, start: Math.min(d.from, d.to), bars: Math.abs(d.to - d.from) + 1 };
      return;
    }
    if (d.to === d.from) { // a click: edit it
      this.editor = { lane: d.lane, layer: d.layer, start: d.orig.start, bars: d.orig.bars, index: d.index };
      return;
    }
    const r = this.dragged(d);
    const next = place(this.regionsOf(l, d.layer).filter((_, i) => i !== d.index), r);
    this.editor = { lane: d.lane, layer: d.layer, start: r.start, bars: r.bars, index: next.indexOf(r) };
    await this.saveLayer(d.lane, d.layer, next);
  }

  /** Right-click on a region removes it. */
  async contextMenu(e, lane, layer) {
    const hit = e.composedPath().find((el) => el.dataset?.index !== undefined);
    if (!hit) return;
    e.preventDefault();
    const index = +hit.dataset.index;
    if (this.editor?.lane === lane && this.editor.layer === layer) this.editor = null;
    await this.saveLayer(lane, layer, this.regionsOf(this.laneOf(lane), layer).filter((_, i) => i !== index));
  }

  // ------------------------------------------------------------- editing

  /** The region open in the editor, if it exists yet. */
  current() {
    const ed = this.editor;
    if (!ed || ed.index === undefined) return null;
    return this.regionsOf(this.laneOf(ed.lane), ed.layer)[ed.index] ?? null;
  }

  /** Puts the edited span in with `value` (and intensity, for marks). */
  async apply(value, extra = {}) {
    const ed = this.editor;
    const l = this.laneOf(ed.lane);
    const prev = this.current();
    const r = { start: ed.start, bars: ed.bars, value, ...extra };
    if (ed.layer === 'marks' && r.intensity === undefined && prev?.intensity !== undefined) r.intensity = prev.intensity;
    const others = this.regionsOf(l, ed.layer).filter((_, i) => i !== ed.index);
    const next = place(others, r);
    this.editor = { ...ed, index: next.indexOf(r) };
    await this.saveLayer(ed.lane, ed.layer, next);
  }

  async remove() {
    const ed = this.editor;
    const l = this.laneOf(ed.lane);
    this.editor = null;
    if (ed.index === undefined) return;
    await this.saveLayer(ed.lane, ed.layer, this.regionsOf(l, ed.layer).filter((_, i) => i !== ed.index));
  }

  editorControls(ed) {
    const cur = this.current();
    switch (ed.layer) {
      case 'marks': {
        const intensity = cur?.intensity ?? 1;
        return html`
          <div class="group">${MARKS.map((m) => html`<button class=${cur?.value === m ? 'on' : ''} @click=${() => this.apply(m)}>${m}</button>`)}</div>
          <label class="group">intensity
            <input type="range" min="0" max="1" step="0.05" .value=${String(intensity)} ?disabled=${!cur}
              @change=${(e) => this.apply(cur.value, { intensity: +e.target.value })}>
            ${Math.round(intensity * 100)}%</label>`;
      }
      case 'feels':
        return html`<div class="group">${FEELS.map((f) => html`<button class=${cur?.value === f ? 'on' : ''} @click=${() => this.apply(f)}>${FEEL_LABEL[f]} ${f}</button>`)}</div>`;
      case 'chords': {
        const c = splitChord(cur?.value);
        const set = (part) => {
          const n = { ...c, ...part };
          this.apply(`${n.shift}${n.degree}${n.quality}`);
        };
        return html`
          <div class="group">
            ${[['b', '♭'], ['', '♮'], ['#', '♯']].map(([s, label]) => html`<button class=${cur && c.shift === s ? 'on' : ''} @click=${() => set({ shift: s })}>${label}</button>`)}
          </div>
          <div class="group">${[1, 2, 3, 4, 5, 6, 7].map((d) => html`<button class=${cur && c.degree === d ? 'on' : ''} @click=${() => set({ degree: d })}>${d}</button>`)}</div>
          <div class="group">${QUALITIES.map((q) => html`<button class=${cur && c.quality === q ? 'on' : ''} @click=${() => set({ quality: q })}>${q || 'diatonic'}</button>`)}</div>`;
      }
      case 'keys': {
        const [root, scale] = (cur?.value ?? `${this.root ?? 'E'} ${this.scale ?? 'phrygian'}`).split(' ');
        const pick = (r, s) => this.apply(`${r} ${s}`);
        return html`
          <div class="group">
            <select .value=${root} @change=${(e) => pick(e.target.value, scale)}>${ROOTS.map((r) => html`<option ?selected=${r === root}>${r}</option>`)}</select>
            <select .value=${scale} @change=${(e) => pick(root, e.target.value)}>${SCALES.map((s) => html`<option ?selected=${s === scale}>${s}</option>`)}</select>
            ${cur ? nothing : html`<button @click=${() => pick(root, scale)}>set</button>`}
          </div>`;
      }
    }
  }

  renderEditor() {
    const ed = this.editor;
    const l = ed && this.laneOf(ed.lane);
    if (!l) return html`<div class="editor"><span class="hint">drag across bars to add · drag a region to move, its edges to resize · chords and keys: click a bar to add one, it holds until the next · click to edit · right-click or Delete removes</span></div>`;
    return html`<div class="editor">
      ${this.editorControls(ed)}
      <span class="spacer"></span>
      ${this.current() ? html`<button class="danger" @click=${this.remove}>delete</button>` : nothing}
      <button @click=${() => (this.editor = null)}>done</button>
    </div>`;
  }

  // --------------------------------------------------------------- banks

  async cue(l, bank) {
    try {
      await api.cue(l.lane, bank);
    } catch (err) {
      this.fail(err);
    }
  }

  async copy(from, to, lanes) {
    try {
      await api.copyBanks(from, to, lanes);
    } catch (err) {
      this.fail(err);
    }
  }

  /** The copy menu: a bank name copies onto it, x2/x4/x8 repeats the start. */
  async copyPick(l, shown, v) {
    if (!v) return;
    if (!v.startsWith('x')) return this.copy(shown, v, [l.lane]);
    if (this.editor?.lane === l.lane) this.editor = null;
    try {
      await api.repeatBank(l.lane, shown, +v.slice(1));
    } catch (err) {
      this.fail(err);
    }
  }

  async clear(l) {
    if (this.confirmClear !== l.lane) {
      this.confirmClear = l.lane;
      setTimeout(() => { if (this.confirmClear === l.lane) this.confirmClear = null; }, 2500);
      return;
    }
    this.confirmClear = null;
    this.editor = null;
    try {
      await api.clearBank(l.lane, this.shown(l));
    } catch (err) {
      this.fail(err);
    }
  }

  pickBank(l, b) {
    this.editing = { ...this.editing, [l.lane]: b === l.playing ? undefined : b };
    if (this.editor?.lane === l.lane) this.editor = null;
  }

  // -------------------------------------------------------------- render

  regionLabel(layer, r) {
    if (layer === 'feels') return FEEL_LABEL[r.value] ?? r.value;
    if (layer === 'marks') return r.intensity !== undefined && r.intensity < 1 ? `${r.value} ${Math.round(r.intensity * 100)}%` : r.value;
    return r.value;
  }

  regionStyle(layer, r) {
    const span = `grid-column: ${r.start} / span ${r.bars};`;
    if (layer !== 'marks') return span;
    const c = MARK_COLOR[r.value] ?? '#888';
    const a = Math.round(35 + 65 * (r.intensity ?? 1));
    // A rise fades in towards its end; the rest are flat.
    const bg = r.value === 'rise'
      ? `linear-gradient(90deg, color-mix(in srgb, ${c} 20%, transparent), color-mix(in srgb, ${c} ${a}%, transparent))`
      : `color-mix(in srgb, ${c} ${a}%, transparent)`;
    return `${span} background: ${bg};`;
  }

  track(l, layer, x) {
    const other = this.shown(l) !== l.playing;
    const { regions, moving } = this.shownRegions(l, layer);
    const mine = this.drag?.lane === l.lane && this.drag.layer === layer ? this.drag : null;
    const d = mine?.mode === 'paint' ? mine : null;
    const ed = this.editor;
    const cls = { marks: 'mark', chords: 'chord', keys: 'key', feels: 'feel' }[layer];
    const dragCls = mine?.mode === 'move' ? 'dragging' : mine && mine.mode !== 'paint' ? 'resizing' : '';
    const selected = (r, i) => (moving ? r.start === moving.start && !r.carry : !mine && ed?.lane === l.lane && ed.layer === layer && ed.index === i && !r.carry);
    const marker = isMarker(layer);
    const shown = marker ? held(regions) : regions.map((r, i) => ({ ...r, index: i }));
    const title = (r) => marker
      ? r.carry
        ? `${r.value}: the last ${layer === 'keys' ? 'key' : 'chord'}, held around the loop; click a bar to add one`
        : `${r.value} from bar ${r.start}, held to ${r.start + r.bars - 1}: drag its bar to move, right-click to delete; click another bar to add one`
      : `${r.value}, bars ${r.start}–${r.start + r.bars - 1}: drag to move, drag an edge to resize, right-click to delete`;
    return html`<div class="track ${other ? 'other' : ''} ${dragCls}"
        @pointerdown=${(e) => this.down(e, l.lane, layer)} @pointermove=${this.move} @pointerup=${this.up}
        @pointercancel=${() => (this.drag = null)} @contextmenu=${(e) => this.contextMenu(e, l.lane, layer)}>
      ${shown.map((r) => html`<div class="region ${cls} ${marker ? (r.carry ? 'carry' : 'marker') : ''} ${selected(r, r.index) ? 'sel' : ''}"
          data-index=${r.index} style=${this.regionStyle(layer, r)} title=${title(r)}>
        ${marker ? nothing : html`<span class="grip l" data-grip="l"></span>`}${r.carry ? '↻ ' : ''}${this.regionLabel(layer, r)}${marker ? nothing : html`<span class="grip r" data-grip="r"></span>`}</div>`)}
      ${d ? html`<div class="paint" style="grid-column: ${Math.min(d.from, d.to)} / ${Math.max(d.from, d.to) + 1}"></div>` : nothing}
      ${ed && ed.index === undefined && ed.lane === l.lane && ed.layer === layer ? html`<div class="paint" style="grid-column: ${ed.start} / span ${ed.bars}"></div>` : nothing}
      ${x === null ? nothing : html`<div class="playhead" style="left: calc(${(x * 100).toFixed(3)}% - 1px)"></div>`}
    </div>`;
  }

  lane(l, x, groupStart) {
    const shown = this.shown(l);
    const isCued = l.cued === shown;
    const layers = [...l.layers].sort((a, b) => LAYER_ORDER.indexOf(a) - LAYER_ORDER.indexOf(b));
    return html`<div class="lane ${groupStart ? 'group' : ''}">
      <div class="labels">${layers.map((layer) => html`<div class="label">${LAYER_LABEL[layer]}</div>`)}</div>
      <div class="grids">${layers.map((layer) => this.track(l, layer, x))}</div>
      <div class="side">
        ${BANKS.map((b) => {
          const filled = Object.keys(l.banks[b] ?? {}).length > 0;
          const cls = [b === l.playing && 'playing', b === l.cued && 'cued', b === shown && 'editing', filled && 'filled'].filter(Boolean).join(' ');
          const title = `${b}${b === l.playing ? ' (playing)' : b === l.cued ? ' (cued)' : ''}: click to show and edit`;
          return html`<button class="bank ${cls}" title=${title} @click=${() => this.pickBank(l, b)}>${b}</button>`;
        })}
        <button class="cue ${isCued ? 'on' : ''}" ?disabled=${shown === l.playing && !l.cued}
          title=${isCued ? 'cancel the cue' : shown === l.playing ? 'cancel the cue' : `play ${shown} from the next epoch`}
          @click=${() => this.cue(l, isCued || shown === l.playing ? l.playing : shown)}>${isCued ? 'cued' : 'cue'}</button>
        <select title="copy the shown bank onto another, or repeat its start over the rest" .value=${''}
          @change=${(e) => { const v = e.target.value; e.target.value = ''; this.copyPick(l, shown, v); }}>
          <option value="">copy→</option>
          <optgroup label="onto bank">
            ${BANKS.filter((b) => b !== shown).map((b) => html`<option value=${b}>${b}</option>`)}
          </optgroup>
          <optgroup label="repeat the start">
            <option value="x2">2× first 8 bars</option>
            <option value="x4">4× first 4 bars</option>
            <option value="x8">8× first 2 bars</option>
          </optgroup>
        </select>
        <button class=${this.confirmClear === l.lane ? 'danger' : ''} title="empty the shown bank" @click=${() => this.clear(l)}>
          ${this.confirmClear === l.lane ? 'sure?' : 'clear'}</button>
      </div>
    </div>`;
  }

  render() {
    const s = this.seq;
    const title = html`<span class="title" role="button" tabindex="0" aria-expanded=${!this.collapsed}
      title=${this.collapsed ? 'Show the sequencer' : 'Hide the sequencer'}
      @click=${this.toggle} @keydown=${(e) => (e.key === 'Enter' || e.key === ' ') && (e.preventDefault(), this.toggle())}>Sequencer</span>`;
    if (!s) return html`<div class="head">${title} waiting for the daemon…</div>`;
    const epochBeats = s.bars * 4;
    const x = this.beat == null ? null : (((this.beat % epochBeats) + epochBeats) % epochBeats) / epochBeats;
    return html`
      <div class="head">
        ${title}
        ${this.collapsed ? nothing : html`
          ${this.renderEditor()}
          <span>copy all lanes</span>
          <select .value=${this.copyFrom} @change=${(e) => (this.copyFrom = e.target.value)}>${BANKS.map((b) => html`<option ?selected=${b === this.copyFrom}>${b}</option>`)}</select>
          →
          <select .value=${this.copyTo} @change=${(e) => (this.copyTo = e.target.value)}>${BANKS.map((b) => html`<option ?selected=${b === this.copyTo}>${b}</option>`)}</select>
          <button ?disabled=${this.copyFrom === this.copyTo} @click=${() => this.copy(this.copyFrom, this.copyTo)}>copy</button>`}
      </div>
      ${this.collapsed ? nothing : [...s.lanes]
        .sort((a, b) => LANE_ORDER.indexOf(a.lane) - LANE_ORDER.indexOf(b.lane))
        .map((l) => this.lane(l, x, GROUP_STARTS.includes(l.lane)))}
    `;
  }
}

customElements.define('ws-sequencer', WsSequencer);
