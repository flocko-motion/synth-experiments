// Client for the workspace HTTP API (workspace/openapi.yaml).
//
// The API base is the page's own origin, or `?api=http://127.0.0.1:7700`
// when the UI is served from elsewhere (the daemon allows local dev servers
// cross-origin).

const BASE = (new URLSearchParams(location.search).get('api') || '').replace(/\/$/, '');

export class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function call(method, path, body) {
  const init = { method, headers: {} };
  if (body !== undefined) {
    init.headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  let res;
  try {
    res = await fetch(BASE + path, init);
  } catch (e) {
    throw new ApiError(0, 'the workspace daemon is not reachable');
  }
  if (res.status === 204) return null;
  const data = await res.json().catch(() => null);
  if (!res.ok) throw new ApiError(res.status, data?.error || `${method} ${path}: ${res.status}`);
  return data;
}

const gen = (name) => `/api/generators/${encodeURIComponent(name)}`;

export const api = {
  state: () => call('GET', '/api/state'),
  catalog: () => call('GET', '/api/catalog'),
  transport: (patch) => call('PATCH', '/api/transport', patch),
  seek: (bar) => call('POST', '/api/transport/seek', { bar }),
  add: (spec) => call('POST', '/api/generators', spec),
  patch: (name, patch) => call('PATCH', gen(name), patch),
  remove: (name) => call('DELETE', gen(name)),
  restart: (name) => call('POST', gen(name) + '/restart'),
};

/**
 * Subscribes to `/api/events`. Calls `onState(state)` about 20 times a
 * second and `onConnection(bool)` when the stream drops or comes back;
 * EventSource reconnects on its own. Returns a function that closes it.
 */
export function subscribe({ onState, onLog, onConnection }) {
  const es = new EventSource(BASE + '/api/events');
  es.addEventListener('state', (e) => onState?.(JSON.parse(e.data)));
  es.addEventListener('log', (e) => onLog?.(JSON.parse(e.data)));
  es.onopen = () => onConnection?.(true);
  es.onerror = () => onConnection?.(false);
  return () => es.close();
}

/** Linear peak (0..1) to dBFS, floored. */
export function toDb(lin, floor = -60) {
  return lin > 0 ? Math.max(floor, 20 * Math.log10(lin)) : floor;
}

/**
 * Wraps `fn` so it runs at most every `ms`, always with the latest
 * arguments, and once more after the last call (so the final value lands).
 */
export function throttle(fn, ms) {
  let last = 0, timer = null, args = null;
  const run = () => { last = performance.now(); timer = null; fn(...args); };
  return (...a) => {
    args = a;
    const wait = ms - (performance.now() - last);
    if (wait <= 0 && !timer) run();
    else if (!timer) timer = setTimeout(run, Math.max(0, wait));
  };
}
