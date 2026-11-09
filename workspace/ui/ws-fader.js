// A vertical fader in dB with a peak meter beside it, shared by channel
// strips and the master. `gr` (0..1) adds a ducking column: filling down
// from the top, how much this strip is being ducked; `src` (0..1) fills it up
// from the bottom, how much this strip is ducking the others.
//
// Drag the cap to move it (relative, so it never jumps; Shift for fine).
// Clicking the track glides the cap there, and you can keep dragging from
// there. Wheel and arrow keys work too (Page Up/Down for 6 dB), and
// double-click resets to 0 dB. While it's held, incoming values don't move
// it. Changes go out as throttled `change` events with `detail` in dB.

import { LitElement, html, css } from './vendor/lit-all.min.js';
import { toDb, throttle } from './api.js';
import { paramColor } from './ws-knob.js';

const MIN = -60; // the bottom of the fader means -96 dB (the API's floor)
const MAX = 12;
const FLOOR_DB = -96;
const GLIDE_MS = 140;
const pct01 = (v) => Math.round(Math.min(1, Math.max(0, v ?? 0)) * 100);

class WsFader extends LitElement {
  static properties = {
    db: { type: Number },
    peak: { type: Number },
    gr: { type: Number }, // gain reduction from ducking, 0..1; undefined hides the column
    src: { type: Number }, // how much this strip ducks the others, 0..1 (fills from the bottom)
    dimmed: { type: Boolean },
    local: { state: true },
  };

  static styles = css`
    :host { display: grid; justify-items: center; gap: 6px; --h: 150px; }
    .readout { font: 12px var(--mono); color: var(--text); font-variant-numeric: tabular-nums; }
    .body { display: flex; gap: 6px; height: var(--h); }
    .meter { width: 6px; background: var(--led-off); border-radius: 2px; position: relative; overflow: hidden; }
    .meter .fill {
      position: absolute; inset: auto 0 0 0;
      background: linear-gradient(to top, #22c55e 0%, #22c55e 70%, #facc15 85%, #ef4444 100%);
      background-size: 100% var(--h); background-position: bottom;
      transition: height 60ms linear;
    }
    :host([dimmed]) .meter .fill { opacity: .35; }
    .gr { width: 4px; background: var(--led-off); border-radius: 2px; position: relative; overflow: hidden; }
    .gr .fill { position: absolute; left: 0; right: 0; background: var(--c); box-shadow: 0 0 4px var(--c); transition: height 50ms linear; }
    .gr .fill.in { top: 0; }
    .gr .fill.out { bottom: 0; filter: brightness(1.25); }
    .fader {
      position: relative; width: 34px; height: 100%; cursor: pointer; touch-action: none; outline: none;
    }
    .fader:focus-visible .cap { box-shadow: 0 0 0 2px var(--accent); }
    .groove {
      position: absolute; left: 50%; top: 9px; bottom: 9px; width: 4px; transform: translateX(-50%);
      background: #0b0c0d; border-radius: 2px; box-shadow: inset 0 0 2px #000;
    }
    .level {
      position: absolute; left: 50%; bottom: 9px; width: 4px; transform: translateX(-50%);
      background: color-mix(in srgb, var(--accent) 55%, transparent); border-radius: 2px;
    }
    .zero { position: absolute; left: 3px; right: 3px; height: 1px; background: var(--dim); opacity: .6; }
    .cap {
      position: absolute; left: 0; right: 0; height: 18px; transform: translateY(50%);
      border-radius: 3px; cursor: grab;
      background: linear-gradient(#5a5f66, #3a3e44 45%, #2d3035 55%, #44494f);
      border: 1px solid #1a1c1f; box-shadow: 0 2px 4px #0008;
    }
    .cap::after {
      content: ""; position: absolute; left: 4px; right: 4px; top: 50%; height: 2px; transform: translateY(-50%);
      background: var(--accent); border-radius: 1px;
    }
    .fader.held .cap { cursor: grabbing; }
  `;

  constructor() {
    super();
    this.db = 0;
    this.peak = 0;
    this.local = null;
    this.emit = throttle((db) => this.dispatchEvent(new CustomEvent('change', { detail: db })), 50);
  }

  toDb(v) {
    return v <= MIN ? FLOOR_DB : v;
  }

  /** Fader value (MIN..MAX) to 0..1 from the bottom. */
  norm(v) {
    return (Math.min(MAX, Math.max(MIN, v)) - MIN) / (MAX - MIN);
  }

  get value() {
    return this.local ?? Math.max(MIN, this.db);
  }

  set(v) {
    v = Math.round(Math.min(MAX, Math.max(MIN, v)) * 10) / 10;
    if (v === this.local) return;
    this.local = v;
    this.emit(this.toDb(v));
  }

  /** The usable travel of the cap, in pixels, and the fader's box. */
  geometry() {
    const box = this.renderRoot.querySelector('.fader').getBoundingClientRect();
    return { box, travel: box.height - 18 };
  }

  valueAt(clientY) {
    const { box, travel } = this.geometry();
    const t = (box.bottom - 9 - clientY) / travel;
    return MIN + Math.min(1, Math.max(0, t)) * (MAX - MIN);
  }

  glide(to) {
    cancelAnimationFrame(this.glideRaf);
    const from = this.value;
    const start = performance.now();
    const step = (now) => {
      const k = Math.min(1, (now - start) / GLIDE_MS);
      const eased = 1 - Math.pow(1 - k, 3);
      this.set(from + (to - from) * eased);
      if (k < 1) this.glideRaf = requestAnimationFrame(step);
    };
    this.glideRaf = requestAnimationFrame(step);
  }

  down(e) {
    if (e.button !== 0) return;
    const el = e.currentTarget;
    el.setPointerCapture(e.pointerId);
    el.focus();
    el.classList.add('held');
    clearTimeout(this.releaseTimer);
    const onCap = e.target.classList.contains('cap');
    // On the cap: drag from where it is. On the track: glide to the click
    // and keep dragging from there.
    const target = onCap ? this.value : this.valueAt(e.clientY);
    if (!onCap) this.glide(target);
    this.local = this.local ?? this.value;
    this.dragging = { y: e.clientY, v: target };
  }

  move(e) {
    const d = this.dragging;
    if (!d) return;
    const dy = d.y - e.clientY;
    if (dy === 0) return;
    cancelAnimationFrame(this.glideRaf);
    const { travel } = this.geometry();
    d.v += (dy / travel) * (MAX - MIN) * (e.shiftKey ? 0.125 : 1);
    d.y = e.clientY;
    this.set(d.v);
  }

  release(e) {
    e?.currentTarget?.classList.remove('held');
    this.dragging = null;
    // Keep showing our value until the server's catches up.
    clearTimeout(this.releaseTimer);
    this.releaseTimer = setTimeout(() => { if (!this.dragging) this.local = null; }, GLIDE_MS + 300);
  }

  nudge(dbStep) {
    cancelAnimationFrame(this.glideRaf);
    this.set(this.value + dbStep);
    this.release();
  }

  wheel(e) {
    e.preventDefault();
    this.nudge((e.deltaY < 0 ? 1 : -1) * (e.shiftKey ? 0.1 : 0.5));
  }

  key(e) {
    const steps = { ArrowUp: 0.5, ArrowRight: 0.5, ArrowDown: -0.5, ArrowLeft: -0.5, PageUp: 6, PageDown: -6 }[e.key];
    if (steps) {
      e.preventDefault();
      this.nudge(e.shiftKey ? steps / 5 : steps);
    } else if (e.key === 'Home') {
      e.preventDefault();
      this.reset();
    }
  }

  reset() {
    this.glide(0);
    this.release();
  }

  render() {
    const v = this.value;
    const shown = this.toDb(v);
    const pct = ((toDb(this.peak, MIN) - MIN) / (0 - MIN)) * 100;
    const pos = (n) => `calc(9px + ${n} * (100% - 18px))`;
    return html`
      <span class="readout">${shown <= FLOOR_DB ? '−∞' : (shown > 0 ? '+' : '') + shown.toFixed(1)}</span>
      <div class="body">
        <div class="meter" title="peak"><div class="fill" style="height:${Math.max(0, pct)}%"></div></div>
        ${this.gr === undefined ? '' : html`
          <div class="gr" style="--c:${paramColor('duck')}"
            title="ducking: from the top, how much this is ducked by others (${Math.round(this.gr * 100)}% now); from the bottom, how much it ducks the others (${Math.round((this.src ?? 0) * 100)}% now)">
            <div class="fill in" style="height:${pct01(this.gr)}%"></div>
            ${this.src ? html`<div class="fill out" style="height:${pct01(this.src)}%"></div>` : ''}
          </div>`}
        <div class="fader" tabindex="0" role="slider" aria-label="volume in dB"
          aria-valuemin=${FLOOR_DB} aria-valuemax=${MAX} aria-valuenow=${shown}
          title="drag the cap, click to glide there, Shift for fine, double-click for 0 dB"
          @pointerdown=${this.down} @pointermove=${this.move} @pointerup=${this.release} @pointercancel=${this.release}
          @dblclick=${this.reset} @wheel=${this.wheel} @keydown=${this.key}>
          <div class="groove"></div>
          <div class="level" style="height:calc(${this.norm(v)} * (100% - 18px))"></div>
          <div class="zero" style="bottom:${pos(this.norm(0))}"></div>
          <div class="cap" style="bottom:${pos(this.norm(v))}"></div>
        </div>
      </div>
    `;
  }
}

customElements.define('ws-fader', WsFader);
