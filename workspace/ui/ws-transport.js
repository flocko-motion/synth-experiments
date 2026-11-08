// Transport bar: play/stop, tempo, key, click, and the position display
// (LCD counter plus sequencer LEDs for tick, beat, bar and cycle).
//
// State events arrive about 20 times a second; between them the position is
// extrapolated from `beat` and `bpm` on every animation frame, and the
// component re-renders only when the 16th changes.

import { LitElement, html, css, nothing } from './vendor/lit-all.min.js';
import { live } from './vendor/lit-all.min.js';
import { api } from './api.js';

const ROOTS = ['C', 'C#', 'D', 'D#', 'E', 'F', 'F#', 'G', 'G#', 'A', 'A#', 'B'];
const SCALES = ['phrygian', 'minor', 'harmonic-minor', 'dorian', 'major'];

/** The grid from openapi.yaml: each level is four of the one below. */
function grid(beat) {
  const ticks = Math.floor(Math.max(0, beat) * 4);
  return {
    ticks,
    tick: ticks % 4,
    beat: Math.floor(ticks / 4) % 4,
    bar: Math.floor(ticks / 16) % 4,
    cycle: Math.floor(ticks / 64) % 4,
    epoch: Math.floor(ticks / 256),
    step: ticks % 16, // 16th within the bar
  };
}

class WsTransport extends LitElement {
  static properties = {
    state: { attribute: false },
    pos: { state: true },
  };

  static styles = css`
    :host {
      display: flex; flex-wrap: wrap; align-items: center; gap: 16px 24px;
      padding: 12px 16px; background: var(--panel); border-bottom: 1px solid var(--edge);
    }
    .controls { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
    button {
      font: inherit; color: var(--text); background: var(--panel-2); border: 1px solid var(--edge);
      border-radius: 6px; padding: 6px 10px; cursor: pointer; min-width: 36px;
    }
    button:hover { border-color: var(--dim); }
    button.on { background: var(--accent); color: #1a1205; border-color: var(--accent); }
    .play.on { background: var(--led-beat); border-color: var(--led-beat); color: #052e14; }
    label { color: var(--dim); font-size: 12px; display: flex; align-items: center; gap: 6px; }
    input, select {
      font: inherit; color: var(--text); background: var(--panel-2); border: 1px solid var(--edge);
      border-radius: 6px; padding: 5px 6px;
    }
    input[type=number] { width: 64px; }

    .lcd {
      background: var(--lcd-bg); border: 1px solid #1d2b20; border-radius: 6px;
      padding: 6px 12px; box-shadow: inset 0 0 12px #000;
      display: flex; gap: 14px; align-items: flex-end;
    }
    .field { display: grid; justify-items: center; }
    .digits {
      font: 600 28px/1 var(--mono); color: var(--lcd); letter-spacing: 1px;
      text-shadow: 0 0 6px color-mix(in srgb, var(--lcd) 60%, transparent);
      font-variant-numeric: tabular-nums;
    }
    .digits.stopped { opacity: .55; }
    .unit { font: 10px/1.6 var(--sans); color: #4b7a57; text-transform: uppercase; letter-spacing: 1px; }
    .sep { font: 600 28px/1 var(--mono); color: #2f5a3a; }

    .leds { display: grid; gap: 5px; }
    .row { display: flex; align-items: center; gap: 4px; }
    .row .name { width: 44px; font-size: 10px; color: var(--dim); text-transform: uppercase; letter-spacing: 1px; }
    .led {
      width: 14px; height: 10px; border-radius: 2px; background: var(--led-off);
      transition: background 60ms, box-shadow 60ms;
    }
    .group { display: flex; gap: 4px; }
    .group + .group { margin-left: 6px; }
    .led.lit { box-shadow: 0 0 6px var(--c); background: var(--c); }
    .led.past { background: color-mix(in srgb, var(--c) 28%, var(--led-off)); }
    .row.steps .led { --c: var(--led-beat); width: 10px; }
    .row.steps .led.downbeat:not(.lit) { background: #3a3e43; }
    .row.beats .led { --c: var(--led-beat); }
    .row.bars .led { --c: var(--led-bar); }
    .row.cycles .led { --c: var(--led-cycle); }

    .meta { color: var(--dim); font-size: 12px; font-family: var(--mono); }
    .meta .bad { color: var(--danger); }
  `;

  constructor() {
    super();
    this.pos = grid(0);
    this.anchor = null; // { beat, bpm, playing, at }
  }

  willUpdate(changed) {
    if (changed.has('state') && this.state) {
      const { beat, bpm, playing } = this.state;
      this.anchor = { beat, bpm, playing, at: performance.now() };
    }
  }

  connectedCallback() {
    super.connectedCallback();
    const frame = () => {
      this.raf = requestAnimationFrame(frame);
      const a = this.anchor;
      if (!a) return;
      const beat = a.playing ? a.beat + ((performance.now() - a.at) / 60000) * a.bpm : a.beat;
      const p = grid(beat);
      if (p.ticks !== this.pos.ticks) this.pos = p;
    };
    this.raf = requestAnimationFrame(frame);
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    cancelAnimationFrame(this.raf);
  }

  async send(patch) {
    try {
      await api.transport(patch);
    } catch (e) {
      this.dispatchEvent(new CustomEvent('ws-error', { detail: e.message, bubbles: true, composed: true }));
    }
  }

  async rewind() {
    try {
      await api.seek(1);
    } catch (e) {
      this.dispatchEvent(new CustomEvent('ws-error', { detail: e.message, bubbles: true, composed: true }));
    }
  }

  leds(n, current, opts = {}) {
    return Array.from({ length: n }, (_, i) => {
      const cls = ['led'];
      if (i === current) cls.push('lit');
      else if (i < current) cls.push('past');
      if (opts.downbeats && i % 4 === 0) cls.push('downbeat');
      return html`<span class=${cls.join(' ')}></span>`;
    });
  }

  steps(current) {
    // 16 steps in four groups of four, like a step sequencer.
    const all = this.leds(16, current, { downbeats: true });
    return [0, 4, 8, 12].map((i) => html`<span class="group">${all.slice(i, i + 4)}</span>`);
  }

  render() {
    const s = this.state;
    const p = this.pos;
    const playing = s?.playing ?? false;
    const pad = (n, w = 1) => String(n).padStart(w, '0');
    return html`
      <div class="controls">
        <button title="Back to bar 1" @click=${this.rewind}>⏮</button>
        <button class="play ${playing ? 'on' : ''}" title="Play" @click=${() => this.send({ playing: true })}>▶</button>
        <button title="Stop" @click=${() => this.send({ playing: false })}>■</button>
      </div>

      <div class="lcd" title="epoch . cycle . bar . beat">
        <div class="field"><span class="digits ${playing ? '' : 'stopped'}">${pad(p.epoch + 1, 3)}</span><span class="unit">epoch</span></div>
        <span class="sep">.</span>
        <div class="field"><span class="digits ${playing ? '' : 'stopped'}">${p.cycle + 1}</span><span class="unit">cycle</span></div>
        <span class="sep">.</span>
        <div class="field"><span class="digits ${playing ? '' : 'stopped'}">${p.bar + 1}</span><span class="unit">bar</span></div>
        <span class="sep">.</span>
        <div class="field"><span class="digits ${playing ? '' : 'stopped'}">${p.beat + 1}</span><span class="unit">beat</span></div>
      </div>

      <div class="leds">
        <div class="row steps"><span class="name">16th</span>${this.steps(p.step)}</div>
        <div class="row beats"><span class="name">beat</span>${this.leds(4, p.beat)}</div>
        <div class="row bars"><span class="name">bar</span>${this.leds(4, p.bar)}</div>
        <div class="row cycles"><span class="name">cycle</span>${this.leds(4, p.cycle)}</div>
      </div>

      <div class="controls">
        <label>BPM
          <input type="number" min="20" max="300" step="0.5" .value=${live(String(s?.bpm ?? ''))}
            @change=${(e) => this.send({ bpm: Number(e.target.value) })}>
        </label>
        <label>Key
          <select .value=${live(s?.root ?? 'E')} @change=${(e) => this.send({ root: e.target.value })}>
            ${ROOTS.map((r) => html`<option ?selected=${s?.root === r}>${r}</option>`)}
          </select>
          <select .value=${live(s?.scale ?? 'phrygian')} @change=${(e) => this.send({ scale: e.target.value })}>
            ${SCALES.map((sc) => html`<option ?selected=${s?.scale === sc}>${sc}</option>`)}
          </select>
        </label>
        <button class=${s?.click ? 'on' : ''} title="Metronome" @click=${() => this.send({ click: !s?.click })}>Click</button>
      </div>

      ${s ? html`
        <div class="meta">
          ${s.sample_rate / 1000} kHz · ahead ${Math.round(s.ahead_ms)} ms
          · <span class=${s.underruns ? 'bad' : ''}>${s.underruns} underruns</span>
        </div>` : nothing}
    `;
  }
}

customElements.define('ws-transport', WsTransport);
