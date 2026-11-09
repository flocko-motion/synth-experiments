// A send box: how much of a channel goes to one effect. A tiny box in the
// effect's colour showing the amount. A quick click sets it to where you
// click (left edge 0, right edge 100); press and hold, or drag, to slide it
// up/down (or sideways) from where it is, Shift for fine. Double-click for
// 0; wheel and arrow keys work too. Emits throttled `change` events with
// `detail` in 0..1.
//
// Every loaded effect is a bus, so a strip has one box per effect.

import { LitElement, html, css } from './vendor/lit-all.min.js';
import { throttle } from './api.js';

const DRAG_PX = 120;
/** A press shorter than this that barely moves is a click, not a slide. */
const CLICK_MS = 250;
const CLICK_PX = 3;

class WsSend extends LitElement {
  static properties = {
    effect: { type: String },
    color: { type: String }, // the effect's plugin colour
    amount: { type: Number }, // 0..1
    preview: { type: Boolean, reflect: true }, // not routed yet
    local: { state: true },
  };

  static styles = css`
    :host { display: block; }
    .box {
      position: relative; height: 13px; border-radius: 2px; overflow: hidden; cursor: ew-resize;
      background: color-mix(in srgb, var(--c) 14%, var(--panel-2));
      border: 1px solid color-mix(in srgb, var(--c) 55%, transparent);
      touch-action: none; user-select: none; outline: none;
    }
    .box:focus-visible { box-shadow: 0 0 0 2px var(--c); }
    .fill { position: absolute; inset: 0 auto 0 0; background: color-mix(in srgb, var(--c) 55%, transparent); }
    .text {
      position: relative; display: flex; justify-content: space-between; gap: 4px; padding: 0 4px;
      font: 600 9px/11px var(--sans); color: var(--text);
    }
    .fx { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .amt { font-family: var(--mono); font-variant-numeric: tabular-nums; }
    :host([preview]) .box { border-style: dashed; }
  `;

  constructor() {
    super();
    this.amount = 0;
    this.local = null;
    this.emit = throttle((v) => this.dispatchEvent(new CustomEvent('change', { detail: v })), 50);
  }

  set(v) {
    v = Math.min(1, Math.max(0, v));
    if (v === this.local) return;
    this.local = v;
    this.emit(v);
  }

  up(e) {
    const d = this.dragging;
    // A quick click: jump to the clicked spot.
    if (d && !d.sliding && performance.now() - d.at < CLICK_MS) {
      const r = e.currentTarget.getBoundingClientRect();
      this.set(Math.round(((e.clientX - r.left) / r.width) * 100) / 100);
    }
    this.release();
  }

  release() {
    this.dragging = null;
    clearTimeout(this.releaseTimer);
    this.releaseTimer = setTimeout(() => { if (!this.dragging) this.local = null; }, 300);
  }

  down(e) {
    e.currentTarget.setPointerCapture(e.pointerId);
    e.currentTarget.focus();
    clearTimeout(this.releaseTimer);
    this.local = this.local ?? this.amount;
    this.dragging = { x: e.clientX, y: e.clientY, x0: e.clientX, y0: e.clientY, at: performance.now(), v: this.local, sliding: false };
  }

  move(e) {
    const d = this.dragging;
    if (!d) return;
    // Slide only once it's a hold or a real drag, so a click doesn't nudge.
    if (!d.sliding) {
      if (Math.hypot(e.clientX - d.x0, e.clientY - d.y0) < CLICK_PX && performance.now() - d.at < CLICK_MS) return;
      d.sliding = true;
    }
    // Up or right increases; whichever axis moved more wins.
    const dx = e.clientX - d.x, dy = d.y - e.clientY;
    const delta = Math.abs(dx) > Math.abs(dy) ? dx : dy;
    d.x = e.clientX;
    d.y = e.clientY;
    d.v = Math.min(1, Math.max(0, d.v + delta / (e.shiftKey ? DRAG_PX * 8 : DRAG_PX)));
    this.set(d.v);
  }

  nudge(dir, fine) {
    this.set((this.local ?? this.amount) + dir * (fine ? 0.005 : 0.05));
    this.release();
  }

  wheel(e) {
    e.preventDefault();
    this.nudge(e.deltaY < 0 ? 1 : -1, e.shiftKey);
  }

  key(e) {
    const dir = { ArrowUp: 1, ArrowRight: 1, ArrowDown: -1, ArrowLeft: -1 }[e.key];
    if (dir) { e.preventDefault(); this.nudge(dir, e.shiftKey); }
    else if (e.key === 'Delete' || e.key === 'Backspace' || e.key === 'Home') { e.preventDefault(); this.set(0); this.release(); }
  }

  render() {
    const v = this.local ?? this.amount;
    const pct = Math.round(v * 100);
    const tip = `send to ${this.effect}: ${pct}%${this.preview ? ' (preview: effect routing is not in the API yet)' : ''}\nclick to set, hold or drag to slide (Shift for fine), double-click for 0`;
    return html`
      <div class="box" style="--c:${this.color || 'var(--accent)'}" tabindex="0" role="slider"
        aria-label="send to ${this.effect}" aria-valuemin="0" aria-valuemax="100" aria-valuenow=${pct} title=${tip}
        @pointerdown=${this.down} @pointermove=${this.move} @pointerup=${this.up} @pointercancel=${this.release}
        @dblclick=${() => { this.set(0); this.release(); }} @wheel=${this.wheel} @keydown=${this.key}>
        <div class="fill" style="width:${pct}%"></div>
        <div class="text"><span class="fx">${this.effect}</span><span class="amt">${pct}</span></div>
      </div>
    `;
  }
}

customElements.define('ws-send', WsSend);
