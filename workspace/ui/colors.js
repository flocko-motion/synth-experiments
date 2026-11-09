// Plugin colours: one stable colour per plugin (by its catalog name), used
// on its catalog entry, the strips it runs on, insert slots and send boxes,
// so `reverb` looks the same everywhere and every copy of it shares that.
// The catalog calls setCatalog() so colours are kept apart.

/** A stable hue for a name. */
export function hueOf(name) {
  let h = 2166136261;
  for (const c of name.toLowerCase()) h = Math.imul(h ^ c.charCodeAt(0), 16777619);
  return (h >>> 0) % 360;
}

// Hues assigned for the current catalog, so no two plugins look alike:
// each keeps its own hue unless that's within MIN_GAP of one already taken
// (in name order), then it moves by the golden angle until it's clear. Stable
// as long as the catalog doesn't change.
const MIN_GAP = 30;
let assigned = new Map();

export function setCatalog(names) {
  const taken = [];
  const next = new Map();
  const near = (h) => taken.some((t) => Math.min(Math.abs(t - h), 360 - Math.abs(t - h)) < MIN_GAP);
  for (const name of [...new Set(names)].sort()) {
    let h = hueOf(name);
    for (let i = 0; i < 12 && near(h); i++) h = (h + 137.5) % 360;
    taken.push(h);
    next.set(name, h);
  }
  assigned = next;
}

export function pluginColor(plugin) {
  const h = assigned.get(plugin) ?? hueOf(plugin);
  return `hsl(${Math.round(h)} 68% 60%)`;
}

/** The plugin a running generator or effect is: its program's file name. */
export function pluginOf(g) {
  const program = g?.command?.[0] ?? g?.name ?? '';
  return program.split('/').pop() || g?.name || '';
}

/** The colour for a running generator or effect. */
export function colorOf(g) {
  return pluginColor(pluginOf(g));
}
