// A vertical fader in dB with a peak meter beside it, shared by channel
// strips and the master. While the fader is held, incoming values don't move
// it; changes are sent as `change` events with `detail` in dB, throttled.
// Double-click resets to 0 dB.

import { LitElement, html, css } from './vendor/lit-all.min.js';
import { live } from './vendor/lit-all.min.js';
import { toDb, throttle } from './api.js';

const MIN = -60; // the bottom of the fader means -96 dB (the API's floor)
const MAX = 12;
const FLOOR_DB = -96;

class WsFader extends LitElement {
  static properties = {
    db: { type: Number },
    peak: { type: Number },
    dimmed: { type: Boolean },
    held: { state: true },
    local: { state: true },
  };

  static styles = css`
    :host { display: grid; grid-template-rows: auto 1fr; justify-items: center; gap: 6px; min-height: 0; }
    .readout { font: 12px var(--mono); color: var(--text); font-variant-numeric: tabular-nums; }
    .body { display: flex; gap: 6px; height: 100%; min-height: 120px; }
    .meter {
      width: 8px; background: var(--led-off); border-radius: 2px; position: relative; overflow: hidden;
    }
    .meter .fill {
      position: absolute; inset: auto 0 0 0;
      background: linear-gradient(to top, #22c55e 0%, #22c55e 70%, #facc15 85%, #ef4444 100%);
      background-size: 100% var(--h); background-position: bottom;
      transition: height 60ms linear;
    }
    :host([dimmed]) .meter .fill { opacity: .35; }
    input[type=range] {
      writing-mode: vertical-lr; direction: rtl; height: 100%; width: 28px; margin: 0;
      accent-color: var(--accent); cursor: ns-resize;
    }
  `;

  constructor() {
    super();
    this.db = 0;
    this.peak = 0;
    this.held = false;
    this.local = null;
    this.emit = throttle((db) => {
      this.dispatchEvent(new CustomEvent('change', { detail: db }));
    }, 50);
  }

  toDb(v) {
    return v <= MIN ? FLOOR_DB : v;
  }

  input(e) {
    this.local = Number(e.target.value);
    this.emit(this.toDb(this.local));
  }

  release() {
    this.held = false;
    // Keep showing our value until the server's catches up.
    setTimeout(() => { if (!this.held) this.local = null; }, 300);
  }

  reset() {
    this.local = 0;
    this.emit(0);
    this.release();
  }

  render() {
    const v = this.local ?? Math.max(MIN, this.db);
    const shown = this.toDb(v);
    const pct = ((toDb(this.peak, MIN) - MIN) / (0 - MIN)) * 100;
    const h = this.offsetHeight || 200;
    return html`
      <span class="readout">${shown <= FLOOR_DB ? '−∞' : (shown > 0 ? '+' : '') + shown.toFixed(1)}</span>
      <div class="body">
        <div class="meter" title="peak"><div class="fill" style="height:${Math.max(0, pct)}%; --h:${h}px"></div></div>
        <input type="range" min=${MIN} max=${MAX} step="0.5" .value=${live(String(v))}
          @pointerdown=${() => (this.held = true)} @pointerup=${this.release} @pointercancel=${this.release}
          @input=${this.input} @dblclick=${this.reset}
          aria-label="volume in dB">
      </div>
    `;
  }
}

customElements.define('ws-fader', WsFader);
