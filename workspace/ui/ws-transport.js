// Transport bar: play/stop, tempo, key, click, and the position display
// (LCD counter plus sequencer LEDs for tick, beat, bar and cycle).
//
// State events arrive about 20 times a second; between them the position is
// extrapolated from `beat` and `bpm` on every animation frame, and the
// component re-renders only when the 16th changes.
//
// Changes land `latency_ms` after you make them (the mixer renders ahead).
// An outlined LED on the 16th row marks where a change made now is heard.

import { LitElement, html, css, nothing } from './vendor/lit-all.min.js';
import { live } from './vendor/lit-all.min.js';
import { api } from './api.js';
import { paramColor } from './ws-knob.js';

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

/** Latency in beats as a musician would say it: "½ beat", "1¼ beats". */
function beatsText(b) {
  if (b == null) return '–';
  const quarters = Math.round(b * 4);
  if (quarters === 0) return '<¼ beat';
  const whole = Math.floor(quarters / 4);
  const frac = ['', '¼', '½', '¾'][quarters % 4];
  const n = `${whole || ''}${frac}` || '0';
  return `${n} beat${quarters === 4 ? '' : quarters > 4 ? 's' : ''}`;
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
    .row.steps .led.land { outline: 1.5px solid var(--accent); outline-offset: 1px; }
    .latency { display: grid; justify-items: start; line-height: 1.2; }
    .latency .big { font: 600 13px var(--mono); color: var(--accent); }
    .latency .small { font: 10px var(--sans); color: var(--dim); }
    .buffer { display: grid; gap: 3px; font-size: 10px; color: var(--dim); }
    .buffer label { font-size: 10px; gap: 4px; }
    .buffer input { width: 52px; padding: 2px 4px; font: 11px var(--mono); }
    .underruns { font: 10px var(--mono); }
    .underruns.recent { color: var(--danger); font-weight: 700; }
    .row.beats .led { --c: var(--led-beat); }
    .row.bars .led { --c: var(--led-bar); }
    .row.cycles .led { --c: var(--led-cycle); }
    .duck { position: relative; height: 6px; width: 200px; background: var(--led-off); border-radius: 2px; overflow: hidden; }
    .duck .fill {
      position: absolute; inset: 0 auto 0 0; background: var(--c);
      box-shadow: 0 0 6px var(--c); transition: width 50ms linear;
    }

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
      // Remember when underruns last went up, to flag them for a while.
      const u = this.state.underruns;
      if (this.lastUnderruns !== undefined && u > this.lastUnderruns) this.underrunAt = performance.now();
      this.lastUnderruns = u;
      const { beat, bpm, playing, latency_beats } = this.state;
      this.anchor = { beat, bpm, playing, ahead: latency_beats ?? 0, at: performance.now() };
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
      p.land = Math.floor((beat + a.ahead) * 4) % 16;
      if (p.ticks !== this.pos.ticks || p.land !== this.pos.land) this.pos = p;
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

  setBuffer(e) {
    const ms = Math.round(Number(e.target.value));
    if (!(ms >= 20 && ms <= 2000)) {
      e.target.value = String(Math.round(this.state.ahead_ms));
      this.dispatchEvent(new CustomEvent('ws-error', { detail: 'buffer must be 20-2000 ms', bubbles: true, composed: true }));
      return;
    }
    this.send({ ahead_ms: ms });
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
      if (i === opts.land) cls.push('land');
      return html`<span class=${cls.join(' ')}></span>`;
    });
  }

  steps(current, land) {
    // 16 steps in four groups of four, like a step sequencer.
    const all = this.leds(16, current, { downbeats: true, land });
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
        <div class="row steps"><span class="name">16th</span>${this.steps(p.step, playing ? p.land : undefined)}</div>
        <div class="row beats"><span class="name">beat</span>${this.leds(4, p.beat)}</div>
        <div class="row bars"><span class="name">bar</span>${this.leds(4, p.bar)}</div>
        <div class="row cycles"><span class="name">cycle</span>${this.leds(4, p.cycle)}</div>
        <div class="row" title="Duck envelope: how far the kick is turning things down right now">
          <span class="name">duck</span>
          <span class="duck" style="--c:${paramColor('duck')}"><span class="fill" style="width:${Math.round((s?.duck ?? 0) * 100)}%"></span></span>
        </div>
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
        <div class="latency" title="How far in the future a fader, knob or transport change lands: ${Math.round(s.latency_ms ?? s.ahead_ms)} ms measured, ${Math.round(s.ahead_ms)} ms target, plus the audio device's own few ms. The outlined LED marks the step.">
          <span class="small">changes land</span>
          <span class="big">${beatsText(s.latency_beats)} later</span>
          <span class="small">${Math.round(s.latency_ms ?? s.ahead_ms)} ms</span>
        </div>
        <div class="buffer" title="Render-ahead buffer: smaller means changes land sooner, but heavy plugins may glitch (underruns). Shrinking takes effect as the buffer drains, without a cut.">
          <label>buffer
            <input type="number" min="20" max="2000" step="10" .value=${live(String(Math.round(s.ahead_ms)))}
              @change=${(e) => this.setBuffer(e)}> ms
          </label>
          <span class="underruns ${this.underrunAt && performance.now() - this.underrunAt < 5000 ? 'recent' : ''}"
            title="Times the audio device found the buffer empty (audible glitches). Red for 5 s after a new one.">${s.underruns} underruns</span>
        </div>
        <div class="meta">${s.sample_rate / 1000} kHz</div>` : nothing}
    `;
  }
}

customElements.define('ws-transport', WsTransport);
