// The master strip: output level and meter.

import { LitElement, html, css, nothing } from './vendor/lit-all.min.js';
import { api } from './api.js';
import './ws-fader.js';

class WsMaster extends LitElement {
  static properties = { state: { attribute: false } };

  static styles = css`
    :host { display: grid; grid-template-rows: 1fr auto; gap: 8px; width: 84px; background: var(--panel); min-height: 0; }
    .name {
      text-align: center; font-weight: 600; font-size: 12px; padding: 4px 2px;
      background: var(--accent); color: #1a1205; border-radius: 4px;
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
    return html`
      <ws-fader .db=${s.master_db} .peak=${s.master_peak} @change=${(e) => this.set(e.detail)}></ws-fader>
      <div class="name">master</div>
    `;
  }
}

customElements.define('ws-master', WsMaster);
