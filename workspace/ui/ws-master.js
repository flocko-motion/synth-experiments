// The master strip: output level and meter. Looks like a channel strip and
// sits at the right end of the row of strips, pinned there while the
// channels scroll sideways, so its fader lines up with theirs. Also shows
// the global duck level (every generator's ducking combined).

import { LitElement, html, css, nothing } from './vendor/lit-all.min.js';
import { api, toDb } from './api.js';
import './ws-fader.js';
import { paramColor } from './ws-knob.js';

class WsMaster extends LitElement {
  static properties = { state: { attribute: false } };

  static styles = css`
    :host {
      flex: 0 0 108px; display: flex; flex-direction: column; gap: 8px;
      margin-left: auto; position: sticky; right: 0; z-index: 1;
      background: var(--panel); border: 1px solid var(--edge); border-top: 3px solid var(--accent);
      border-radius: 8px; padding: 8px 6px;
      box-shadow: -10px 0 14px var(--bg);
    }
    .tag {
      font: 700 8px/1 var(--sans); letter-spacing: 1px; text-transform: uppercase; text-align: center;
      color: var(--accent); opacity: .8;
    }
    /* Same height as a channel's M/restart row, so the faders line up. */
    .peak {
      font: 600 10px/14px var(--mono); color: var(--dim); text-align: center; padding: 0;
      border: 1px solid var(--edge); border-radius: 3px; background: var(--panel-2);
    }
    .peak.hot { color: var(--danger); border-color: var(--danger); }
    ws-fader { margin-top: auto; }
    .duck { display: grid; gap: 3px; font: 700 8px/1 var(--sans); letter-spacing: 1px; text-transform: uppercase; color: var(--dim); text-align: center; }
    .duck .bar { position: relative; height: 6px; background: var(--led-off); border-radius: 2px; overflow: hidden; }
    .duck .fill {
      position: absolute; inset: 0 auto 0 0; background: var(--c);
      box-shadow: 0 0 6px var(--c); transition: width 50ms linear;
    }
    .name {
      text-align: center; font: 600 11px/16px var(--sans); padding: 0 2px; border-radius: 3px;
      color: var(--accent); background: color-mix(in srgb, var(--accent) 14%, var(--panel-2));
    }
  `;

  async set(db) {
    try {
      await api.transport({ master_db: db });
    } catch (e) {
      this.dispatchEvent(new CustomEvent('ws-error', { detail: `master: ${e.message}`, bubbles: true, composed: true }));
    }
  }

  render() {
    const s = this.state;
    if (!s) return nothing;
    const peakDb = toDb(s.master_peak, -60);
    return html`
      <div class="tag">output</div>
      <div class="duck" title="Duck envelope: how far the kick is turning things down right now">duck
        <span class="bar" style="--c:${paramColor('duck')}"><span class="fill" style="width:${Math.round((s.duck ?? 0) * 100)}%"></span></span>
      </div>
      <ws-fader .db=${s.master_db} .peak=${s.master_peak} @change=${(e) => this.set(e.detail)}></ws-fader>
      <div class="peak ${peakDb > -1 ? 'hot' : ''}" title="output peak">${peakDb <= -60 ? '−∞' : peakDb.toFixed(1)} dB</div>
      <div class="name">master</div>
    `;
  }
}

customElements.define('ws-master', WsMaster);
