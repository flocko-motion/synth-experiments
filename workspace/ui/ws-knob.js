// A rotary knob for one plugin parameter (continuous or integer). Drag up
// or right to turn it up, down or left to turn it down (Shift for fine), use
// the wheel or the arrow keys, and double-click to go back to the default. While it's held, incoming values
// don't move it. Changes go out as throttled `change` events with `detail`
// as the new value in plain units.
//
// `log` parameters (frequencies) map travel logarithmically when the range
// is positive.
//
// Knobs are coloured by what the parameter does (see paramColor), so a
// cutoff looks the same on every plugin.

import { LitElement, html, css, svg } from './vendor/lit-all.min.js';
import { throttle } from './api.js';

const SWEEP = 270; // degrees of travel, centred at the top
const DRAG_PX = 160; // pixels of drag for the full range

/**
 * Colour families by parameter label, first match wins: specific ones
 * before generic ones, so "Delay mix" is a delay and not a mix.
 */
const FAMILIES = [
  [/duck|sidechain|pump/, 315], // magenta: ducking (also the transport's duck meter)
  [/delay|echo|feedback/, 205], // blue: echoes
  [/reverb|room|size|space|width|stereo|pan/, 255], // violet: space
  [/cut|damp|filter|freq|tone|bright|lowpass|highpass|\bhp\b|\blp\b/, 28], // orange: filter
  [/reso|emphasis|\bq\b/, 330], // pink: resonance
  [/drive|dist|sat|crush|fold|grit/, 2], // red: drive
  [/attack|decay|sustain|release|env|curve|glide|slide|porta/, 140], // green: envelopes
  [/rate|speed|lfo|swing|groove|shuffle/, 175], // teal: time and motion
  [/pitch|tune|detune|octave|semi|note|transpose/, 90], // lime: pitch
  [/density|prob|chance|random|variation|pattern|steps|seed/, 285], // purple: pattern
  [/mix|depth|amount|level|volume|gain|wet|dry/, 48], // yellow: amounts
];

/**
 * An effect's duck sensitivity as a knob parameter (a mixer setting, not a
 * plugin parameter: set it with PATCH /api/effects/{name}). Null while the
 * effect isn't running.
 */
export const DUCK_ID = 'duck_sensitivity';
export function duckParam(fx) {
  if (fx.duck_sensitivity == null) return null;
  return {
    id: DUCK_ID, name: 'Duck', min: 0, max: 1, kind: 'continuous', log: false, unit: '',
    default: fx.declared_duck_sensitivity ?? fx.duck_sensitivity, value: fx.duck_sensitivity,
  };
}

/** The colour for a parameter label: by family, else a stable hue per label. */
export function paramColor(label) {
  const l = label.toLowerCase();
  const family = FAMILIES.find(([re]) => re.test(l));
  let hue = family?.[1];
  if (hue === undefined) {
    let h = 2166136261;
    for (const c of l) h = Math.imul(h ^ c.charCodeAt(0), 16777619);
    hue = (h >>> 0) % 360;
  }
  return `hsl(${hue} 72% 60%)`;
}

class WsKnob extends LitElement {
  static properties = {
    param: { attribute: false }, // a Param from the API
    local: { state: true }, // the value while being turned
  };

  static styles = css`
    :host { display: grid; justify-items: center; gap: 1px; width: 46px; user-select: none; }
    .knob { width: 30px; height: 30px; cursor: move; touch-action: none; outline: none; border-radius: 50%; }
    .knob:focus-visible { box-shadow: 0 0 0 2px var(--knob, var(--accent)); }
    .track { fill: none; stroke: color-mix(in srgb, var(--knob, var(--accent)) 18%, var(--led-off)); stroke-width: 3; stroke-linecap: round; }
    .arc { fill: none; stroke: var(--knob, var(--accent)); stroke-width: 3; stroke-linecap: round; }
    .cap { fill: var(--panel-2); stroke: var(--edge); }
    .tick { stroke: var(--knob, var(--text)); stroke-width: 2; stroke-linecap: round; }
    .name, .val {
      font-size: 9px; line-height: 1.15; max-width: 46px; padding: 0 1px; box-sizing: border-box; text-align: center;
      white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
    }
    .name { color: var(--dim); }
    .val { font-family: var(--mono); color: var(--text); font-variant-numeric: tabular-nums; }
  `;

  constructor() {
    super();
    this.local = null;
    this.emit = throttle((v) => this.dispatchEvent(new CustomEvent('change', { detail: v })), 50);
  }

  get logScale() {
    const p = this.param;
    return p.log && p.min > 0 && p.max > p.min;
  }

  /** Value in plain units to knob travel 0..1. */
  toNorm(v) {
    const { min, max } = this.param;
    if (max === min) return 0;
    const t = this.logScale ? Math.log(v / min) / Math.log(max / min) : (v - min) / (max - min);
    return Math.min(1, Math.max(0, t));
  }

  fromNorm(t) {
    const { min, max, kind } = this.param;
    t = Math.min(1, Math.max(0, t));
    let v = this.logScale ? min * Math.pow(max / min, t) : min + t * (max - min);
    if (kind === 'integer') v = Math.round(v);
    return v;
  }

  set(v) {
    if (v === this.local) return;
    this.local = v;
    this.emit(v);
  }

  nudge(steps, fine) {
    const p = this.param;
    const v = this.local ?? p.value;
    if (p.kind === 'integer') this.set(Math.min(p.max, Math.max(p.min, Math.round(v) + steps)));
    else this.set(this.fromNorm(this.toNorm(v) + steps * (fine ? 0.002 : 0.02)));
  }

  release() {
    this.dragging = null;
    // Keep showing our value until the server's catches up.
    clearTimeout(this.releaseTimer);
    this.releaseTimer = setTimeout(() => { if (!this.dragging) this.local = null; }, 300);
  }

  down(e) {
    e.currentTarget.setPointerCapture(e.pointerId);
    e.currentTarget.focus();
    clearTimeout(this.releaseTimer);
    this.dragging = { x: e.clientX, y: e.clientY, t: this.toNorm(this.local ?? this.param.value) };
    this.local = this.local ?? this.param.value;
  }

  move(e) {
    const d = this.dragging;
    if (!d) return;
    const scale = e.shiftKey ? DRAG_PX * 8 : DRAG_PX;
    // Re-anchor every move so Shift can be pressed mid-drag.
    // Up and right turn it up; both axes count, so diagonal drags work too.
    d.t += (d.y - e.clientY + (e.clientX - d.x)) / scale;
    d.x = e.clientX;
    d.y = e.clientY;
    this.set(this.fromNorm(d.t));
  }

  wheel(e) {
    e.preventDefault();
    this.nudge(e.deltaY < 0 ? 1 : -1, e.shiftKey);
    this.release();
  }

  key(e) {
    const steps = { ArrowUp: 1, ArrowRight: 1, ArrowDown: -1, ArrowLeft: -1 }[e.key];
    if (steps) {
      e.preventDefault();
      this.nudge(steps, e.shiftKey);
      this.release();
    } else if (e.key === 'Home' || e.key === 'Delete' || e.key === 'Backspace') {
      e.preventDefault();
      this.reset();
    }
  }

  reset() {
    this.set(this.param.default);
    this.release();
  }

  format(v) {
    const p = this.param;
    let s;
    if (p.kind === 'integer') s = String(Math.round(v));
    else if (p.unit === 'Hz' && Math.abs(v) >= 1000) s = (v / 1000).toFixed(v >= 10000 ? 1 : 2) + 'k';
    else {
      const range = Math.abs(p.max - p.min);
      s = v.toFixed(range <= 2 ? 2 : range <= 20 ? 1 : 0);
    }
    // Short units fit under the knob; long ones ("of a step") go in the tooltip.
    return p.unit && p.unit.length <= 3 ? `${s} ${p.unit}` : s;
  }

  render() {
    const p = this.param;
    if (!p) return html``;
    const v = this.local ?? p.value;
    const t = this.toNorm(v);
    // Arcs start from the default's position, so bipolar params read as +/-.
    const t0 = this.toNorm(Math.min(p.max, Math.max(p.min, p.default)));
    const ang = (u) => ((u - 0.5) * SWEEP * Math.PI) / 180;
    const pt = (u, r) => [15 + r * Math.sin(ang(u)), 15 - r * Math.cos(ang(u))];
    const arc = (a, b, r) => {
      const [x1, y1] = pt(a, r), [x2, y2] = pt(b, r);
      const large = Math.abs(b - a) * SWEEP > 180 ? 1 : 0;
      return `M${x1} ${y1} A${r} ${r} 0 ${large} 1 ${x2} ${y2}`;
    };
    const [tx, ty] = pt(t, 9);
    const [cx, cy] = pt(t, 4);
    const long = p.unit && p.unit.length > 3 ? ` ${p.unit}` : '';
    return html`
      <svg class="knob" viewBox="0 0 30 30" tabindex="0" role="slider" style="--knob:${paramColor(p.name)}"
        aria-label=${p.name} aria-valuemin=${p.min} aria-valuemax=${p.max} aria-valuenow=${v}
        @pointerdown=${this.down} @pointermove=${this.move} @pointerup=${this.release} @pointercancel=${this.release}
        @dblclick=${this.reset} @wheel=${this.wheel} @keydown=${this.key}>
        <title>${p.name}: ${this.format(v)}${long} (default ${this.format(p.default)}; double-click to reset, Shift for fine)</title>
        ${svg`
          <path class="track" d=${arc(0, 1, 13)}></path>
          ${Math.abs(t - t0) > 0.001 ? svg`<path class="arc" d=${arc(Math.min(t, t0), Math.max(t, t0), 13)}></path>` : ''}
          <circle class="cap" cx="15" cy="15" r="9.5"></circle>
          <line class="tick" x1=${cx} y1=${cy} x2=${tx} y2=${ty}></line>
        `}
      </svg>
      <span class="val">${this.format(v)}</span>
      <span class="name" title=${p.name}>${p.name}</span>
    `;
  }
}

customElements.define('ws-knob', WsKnob);
