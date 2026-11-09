// Transport bar: play/stop, tempo, and the position display (LCD counter
// plus LEDs for the 16th and the beat; bars and cycles show in the
// sequencer grid, the duck level on the master strip). One slim row. Key
// and chords live in the sequencer; the host key and the metronome are set
// with `ws key` and `ws click`.
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
      display: flex; flex-wrap: wrap; align-items: center; gap: 6px 18px;
      padding: 5px 16px; background: var(--panel); border-bottom: 1px solid var(--edge);
    }
    .controls { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
    button {
      font: inherit; font-size: 12px; color: var(--text); background: var(--panel-2); border: 1px solid var(--edge);
      border-radius: 5px; padding: 3px 8px; cursor: pointer; min-width: 30px;
    }
    button:hover { border-color: var(--dim); }
    button.on { background: var(--accent); color: #1a1205; border-color: var(--accent); }
    .play.on { background: var(--led-beat); border-color: var(--led-beat); color: #052e14; }
    label { color: var(--dim); font-size: 12px; display: flex; align-items: center; gap: 6px; }
    input, select {
      font: inherit; color: var(--text); background: var(--panel-2); border: 1px solid var(--edge);
      border-radius: 5px; padding: 2px 5px; font-size: 12px;
    }
    input[type=number] { width: 58px; }

    .lcd {
      background: var(--lcd-bg); border: 1px solid #1d2b20; border-radius: 6px;
      padding: 3px 10px; box-shadow: inset 0 0 10px #000;
      display: flex; gap: 8px; align-items: flex-end;
    }
    .field { display: grid; justify-items: center; }
    .digits {
      font: 600 20px/1 var(--mono); color: var(--lcd); letter-spacing: 1px;
      text-shadow: 0 0 6px color-mix(in srgb, var(--lcd) 60%, transparent);
      font-variant-numeric: tabular-nums;
    }
    .digits.stopped { opacity: .55; }
    .unit { font: 7px/1.3 var(--sans); color: #4b7a57; text-transform: uppercase; letter-spacing: 1px; }
    .sep { font: 600 20px/1 var(--mono); color: #2f5a3a; align-self: flex-start; }

    .leds { display: grid; gap: 4px; }
    .row { display: flex; align-items: center; gap: 3px; }
    .row .name { width: 30px; font-size: 8px; color: var(--dim); text-transform: uppercase; letter-spacing: 1px; }
    .led {
      width: 14px; height: 7px; border-radius: 2px; background: var(--led-off);
      transition: background 60ms, box-shadow 60ms;
    }
    .group { display: flex; gap: 4px; }
    .group { gap: 3px; }
    .group + .group { margin-left: 5px; }
    .led.lit { box-shadow: 0 0 6px var(--c); background: var(--c); }
    .led.past { background: color-mix(in srgb, var(--c) 28%, var(--led-off)); }
    .row.steps .led { --c: var(--led-beat); width: 10px; }
    .row.steps .led.downbeat:not(.lit) { background: #3a3e43; }
    .row.steps .led.land { outline: 1.5px solid var(--accent); outline-offset: 1px; }
    .buffer { display: grid; gap: 1px; font-size: 10px; color: var(--dim); }
    .buffer label { font-size: 10px; gap: 4px; }
    .buffer input { width: 52px; padding: 2px 4px; font: 11px var(--mono); }
    .underruns { font: 10px var(--mono); }
    .underruns.recent { color: var(--danger); font-weight: 700; }
    .row.beats .led { --c: var(--led-beat); }
    .load { display: grid; gap: 3px; font-size: 10px; color: var(--dim); min-width: 92px; }
    .load b { font: 600 12px var(--mono); color: var(--led-beat); }
    .load b.warn { color: var(--warn); }
    .load b.hot { color: var(--danger); }
    .load .peak { font: 10px var(--mono); }
    .load .bar { position: relative; height: 4px; border-radius: 2px; background: var(--led-off); overflow: hidden; }
    .load .fill { position: absolute; inset: 0 auto 0 0; background: var(--led-beat); }
    .load .fill.warn { background: var(--warn); }
    .load .fill.hot { background: var(--danger); }
    .load .pk { position: absolute; top: 0; bottom: 0; width: 2px; margin-left: -1px; background: var(--text); opacity: .7; }

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

  /** DSP load: render time over playing time per block, with its peak. */
  renderLoad(s) {
    const load = s.dsp_load ?? 0, peak = s.dsp_peak ?? 0;
    const level = (x) => (x >= 0.9 ? 'hot' : x >= 0.7 ? 'warn' : '');
    const pct = (x) => `${Math.round(x * 100)}%`;
    const width = (x) => `${Math.min(100, x * 100).toFixed(1)}%`;
    return html`<div class="load" title="DSP load: how long the mixer takes to render a block (every generator and effect, running in parallel, then the mix) compared with how long that block plays. Near 100% the buffer drains and audio drops out; a bigger buffer only buys time. The tick marks the peak of the last few seconds.">
      <span>DSP <b class=${level(load)}>${pct(load)}</b> <span class="peak">peak ${pct(peak)}</span></span>
      <span class="bar"><span class="fill ${level(load)}" style="width: ${width(load)}"></span><span class="pk" style="left: ${width(peak)}"></span></span>
    </div>`;
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
      </div>

      <div class="controls">
        <label>BPM
          <input type="number" min="20" max="300" step="0.5" .value=${live(String(s?.bpm ?? ''))}
            @change=${(e) => this.send({ bpm: Number(e.target.value) })}>
        </label>
      </div>

      ${s ? html`
        <div class="buffer" title="Render-ahead buffer: smaller means changes land sooner, but heavy plugins may glitch (underruns). Shrinking takes effect as the buffer drains, without a cut.">
          <label>buffer
            <input type="number" min="20" max="2000" step="10" .value=${live(String(Math.round(s.ahead_ms)))}
              @change=${(e) => this.setBuffer(e)}> ms
          </label>
          <span class="underruns ${this.underrunAt && performance.now() - this.underrunAt < 5000 ? 'recent' : ''}"
            title="Times the audio device found the buffer empty (audible glitches). Red for 5 s after a new one.">${s.underruns} underruns</span>
        </div>
        ${this.renderLoad(s)}` : nothing}
    `;
  }
}

customElements.define('ws-transport', WsTransport);
