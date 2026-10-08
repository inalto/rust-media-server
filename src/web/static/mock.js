// Client-side mock of the metrics and recordings endpoints, for UI development and
// demos before the server implements them. Enable in the browser console with
//   localStorage.rmsMock = "1"   (disable: delete localStorage.rmsMock)
// Only GET /broadcasts/{name}/metrics, /metrics/summary and /recordings are mocked;
// everything else goes to the server.

export function mockEnabled() {
  try { return window.localStorage.getItem('rmsMock') === '1'; } catch (_) { return false; }
}

// Deterministic pseudo-random noise from an integer seed.
function noise(n) {
  let x = Math.imul(n ^ 0x9e3779b9, 0x85ebca6b);
  x ^= x >>> 13;
  x = Math.imul(x, 0xc2b2ae35);
  x ^= x >>> 16;
  return (x >>> 0) / 4294967296;
}

function viewersAt(ms) {
  const t = ms / 1000;
  // Live 40 min every hour (offline gaps show as zero), smooth audience curve + noise.
  if (Math.floor(t / 60) % 60 >= 40) return 0;
  const base = 42 + 26 * Math.sin(t / 2400) + 12 * Math.sin(t / 530);
  return Math.max(0, Math.round(base + 8 * (noise(Math.floor(t / 5)) - 0.5)));
}

function split(total, ms) {
  const r = noise(Math.floor(ms / 5000) + 7);
  const hls = Math.round(total * (0.22 + 0.08 * r));
  const dash = Math.round(total * 0.1);
  const ws = Math.round(total * 0.06);
  const webrtc = 0;
  const flv = Math.max(0, total - hls - dash - ws - webrtc);
  return { total, flv, ws_flv: ws, hls, dash, webrtc };
}

function series(params) {
  const now = Date.now();
  const to = params.get('to') ? Date.parse(params.get('to')) : now;
  const from = params.get('from') ? Date.parse(params.get('from')) : to - 3600e3;
  let step = Number(params.get('step')) || Math.max(5, Math.ceil((to - from) / 1000 / 600 / 5) * 5);
  const points = [];
  for (let t = Math.ceil(from / (step * 1000)) * step * 1000; t <= to; t += step * 1000) {
    // Aggregate by max over the bucket, like the server.
    let best = null;
    for (let s = 0; s < step; s += Math.max(5, step / 6)) {
      const v = split(viewersAt(t + s * 1000), t + s * 1000);
      if (!best || v.total > best.total) best = v;
    }
    points.push({ t: new Date(t).toISOString(), ...best });
  }
  return { from: new Date(from).toISOString(), to: new Date(to).toISOString(), step, points };
}

function summary(params) {
  const s = series(params);
  let peak = { value: 0, at: null };
  for (const p of s.points) if (p.total > peak.value) peak = { value: p.total, at: p.t };
  const last = s.points[s.points.length - 1] || split(0, Date.now());
  const { t: _t, ...current } = last;
  return {
    current,
    peak,
    sessions_total: Math.round(s.points.reduce((a, p) => a + p.total, 0) / 9),
    avg_watch_secs: 1264,
    live_since: new Date(Date.now() - 37 * 60e3).toISOString(),
    bytes_in: 3.27e9,
  };
}

function recordings() {
  const now = Date.now();
  const mk = (i, status, ageMin, dur, size, format, error) => ({
    id: `mock-${i}`,
    broadcast_id: 'mock',
    stream_session_id: null,
    path: `mock/rec-${i}.${format}`,
    format,
    status,
    started_at: new Date(now - ageMin * 60e3).toISOString(),
    ended_at: status === 'recording' ? null : new Date(now - ageMin * 60e3 + (dur || 0) * 1000).toISOString(),
    size_bytes: size,
    duration_secs: dur,
    error: error || null,
  });
  return [
    mk(1, 'recording', 12, null, 214e6, 'flv'),
    mk(2, 'processing', 95, 3120, 1.42e9, 'flv'),
    mk(3, 'ready', 1440, 5400, 2.61e9, 'mp4'),
    mk(4, 'failed', 2900, 61, 3.1e6, 'flv', 'ffmpeg remux failed'),
  ];
}

/** Mocked JSON for `path` (under /api/v1), or undefined to hit the server. */
export function mockResponse(method, path) {
  if (method !== 'GET') return undefined;
  const [p, q] = path.split('?');
  const params = new URLSearchParams(q || '');
  if (/^\/broadcasts\/[^/]+\/metrics$/.test(p)) return series(params);
  if (/^\/broadcasts\/[^/]+\/metrics\/summary$/.test(p)) return summary(params);
  if (/^\/broadcasts\/[^/]+\/recordings$/.test(p)) return recordings();
  return undefined;
}
