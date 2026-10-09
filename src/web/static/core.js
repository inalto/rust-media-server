// rms dashboard — shared helpers: DOM, icons, API client, formatting, dialogs,
// clipboard, uploads with progress, event bus and the broadcast store.
// Vanilla ES module, no build step (see index.html). User-visible texts come from
// i18n.js (t()).

import { mockEnabled, mockResponse } from './mock.js';
import { t, formatNumber, formatDate, formatBytes, formatDuration } from './i18n.js';

// ---------------------------------------------------------------------------
// DOM
// ---------------------------------------------------------------------------

/** Create an element: h('button', {class: 'btn', onclick: fn}, 'Text', child...). */
export function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'dataset') Object.assign(el.dataset, v);
    else if (k === 'style' && typeof v === 'object') Object.assign(el.style, v);
    else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
    else if (k === 'value' || k === 'checked' || k === 'selected' || k === 'indeterminate') el[k] = v;
    else if (v === true) el.setAttribute(k, '');
    else el.setAttribute(k, String(v));
  }
  append(el, children);
  return el;
}

function append(el, children) {
  for (const c of children.flat(Infinity)) {
    if (c == null || c === false) continue;
    el.append(c instanceof Node ? c : String(c));
  }
}

/** Replace all children of `el`. */
export function setChildren(el, ...children) {
  el.replaceChildren();
  append(el, children);
  return el;
}

export const $ = (sel, root = document) => root.querySelector(sel);

let idCounter = 0;
export const uid = (prefix = 'id') => `${prefix}-${++idCounter}`;

// ---------------------------------------------------------------------------
// Icons (inline SVG, stroke = currentColor)
// ---------------------------------------------------------------------------

const ICONS = {
  plus: '<path d="M12 5v14M5 12h14"/>',
  copy: '<rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>',
  check: '<path d="M20 6 9 17l-5-5"/>',
  eye: '<path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z"/><circle cx="12" cy="12" r="3"/>',
  eyeOff: '<path d="M17.94 17.94A10.07 10.07 0 0 1 12 19c-6.5 0-10-7-10-7a18.5 18.5 0 0 1 5.06-5.94M9.9 4.24A9.12 9.12 0 0 1 12 4c6.5 0 10 7 10 7a18.5 18.5 0 0 1-2.16 3.19M14.12 14.12a3 3 0 1 1-4.24-4.24M1 1l22 22"/>',
  refresh: '<path d="M21 12a9 9 0 1 1-2.64-6.36L21 8"/><path d="M21 3v5h-5"/>',
  trash: '<path d="M3 6h18M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6M10 11v6M14 11v6"/>',
  pause: '<rect x="6" y="4" width="4" height="16" rx="1"/><rect x="14" y="4" width="4" height="16" rx="1"/>',
  play: '<path d="M7 4.5v15l12-7.5z"/>',
  record: '<circle cx="12" cy="12" r="6" fill="currentColor"/>',
  stop: '<rect x="6" y="6" width="12" height="12" rx="1.5"/>',
  download: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M7 10l5 5 5-5M12 15V3"/>',
  upload: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M17 8l-5-5-5 5M12 3v12"/>',
  x: '<path d="M18 6 6 18M6 6l12 12"/>',
  back: '<path d="m15 18-6-6 6-6"/>',
  external: '<path d="M15 3h6v6M10 14 21 3M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/>',
  users: '<path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M23 21v-2a4 4 0 0 0-3-3.87M16 3.13a4 4 0 0 1 0 7.75"/>',
  sliders: '<path d="M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6"/>',
  logout: '<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4M16 17l5-5-5-5M21 12H9"/>',
  edit: '<path d="M12 20h9M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4z"/>',
  power: '<path d="M18.36 6.64a9 9 0 1 1-12.73 0M12 2v10"/>',
  alert: '<path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0zM12 9v4M12 17h.01"/>',
  info: '<circle cx="12" cy="12" r="10"/><path d="M12 16v-4M12 8h.01"/>',
  radio: '<circle cx="12" cy="12" r="2"/><path d="M16.24 7.76a6 6 0 0 1 0 8.49M7.76 16.24a6 6 0 0 1 0-8.49M19.07 4.93a10 10 0 0 1 0 14.14M4.93 19.07a10 10 0 0 1 0-14.14"/>',
  video: '<rect x="2" y="6" width="14" height="12" rx="2"/><path d="m22 8-6 4 6 4z"/>',
  code: '<path d="m16 18 6-6-6-6M8 6l-6 6 6 6"/>',
  link: '<path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/>',
  chart: '<path d="M3 3v18h18"/><path d="m19 9-5 5-4-4-3 3"/>',
  monitor: '<rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8M12 17v4"/>',
  activity: '<path d="M22 12h-4l-3 9L9 3l-3 9H2"/>',
  clock: '<circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/>',
  server: '<rect x="2" y="2" width="20" height="8" rx="2"/><rect x="2" y="14" width="20" height="8" rx="2"/><path d="M6 6h.01M6 18h.01"/>',
  key: '<circle cx="7.5" cy="15.5" r="5.5"/><path d="m21 2-9.6 9.6M15.5 7.5l3 3L22 7l-3-3"/>',
  film: '<rect x="2" y="2" width="20" height="20" rx="2.5"/><path d="M7 2v20M17 2v20M2 12h20M2 7h5M2 17h5M17 17h5M17 7h5"/>',
  text: '<path d="M4 7V4h16v3M9 20h6M12 4v16"/>',
  folder: '<path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>',
  sun: '<circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41"/>',
  moon: '<path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/>',
  themeSystem: '<circle cx="12" cy="12" r="9"/><path d="M12 3a9 9 0 0 1 0 18z" fill="currentColor"/>',
  layers: '<path d="m12 2 10 5-10 5L2 7z"/><path d="m2 17 10 5 10-5M2 12l10 5 10-5"/>',
  inbox: '<path d="M22 12h-6l-2 3h-4l-2-3H2"/><path d="M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z"/>',
};

/** Inline SVG icon (decorative: aria-hidden). */
export function icon(name, size = 18) {
  const span = document.createElement('span');
  span.className = 'icon';
  span.setAttribute('aria-hidden', 'true');
  span.innerHTML = `<svg width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" focusable="false">${ICONS[name] || ''}</svg>`;
  return span;
}

/** Button with an icon and a (visible or screen-reader-only) label. */
export function iconButton(name, label, onclick, { cls = 'btn', showLabel = true, attrs = {} } = {}) {
  return h('button', { type: 'button', class: cls + (showLabel ? '' : ' icon-only'), onclick, 'aria-label': showLabel ? null : label, title: showLabel ? null : label, ...attrs },
    icon(name), showLabel ? h('span', null, label) : null);
}

// ---------------------------------------------------------------------------
// Event bus + store
// ---------------------------------------------------------------------------

const listeners = new Map();
export const bus = {
  on(type, fn) {
    if (!listeners.has(type)) listeners.set(type, new Set());
    listeners.get(type).add(fn);
    return () => listeners.get(type).delete(fn);
  },
  emit(type, payload) {
    for (const fn of listeners.get(type) || []) {
      try { fn(payload); } catch (e) { console.error(e); }
    }
  },
};

/** Broadcasts by name (BroadcastView), kept fresh by the admin SSE. */
export const store = {
  broadcasts: new Map(),
  /** Insert/replace, preserving more recent viewer counts. */
  upsert(b) {
    this.broadcasts.set(b.name, b);
    bus.emit('broadcast', b);
  },
  remove(name) {
    if (this.broadcasts.delete(name)) bus.emit('deleted', name);
  },
  setViewers(name, counts) {
    const b = this.broadcasts.get(name);
    if (b) b.viewers = counts;
    bus.emit('viewers', { name, counts });
  },
  replaceAll(list) {
    const names = new Set(list.map((b) => b.name));
    for (const name of [...this.broadcasts.keys()]) if (!names.has(name)) this.remove(name);
    for (const b of list) this.broadcasts.set(b.name, b);
    bus.emit('list', list);
  },
  clear() { this.broadcasts.clear(); },
};

// ---------------------------------------------------------------------------
// API client
// ---------------------------------------------------------------------------

export class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

let unauthorizedHandler = () => {};
export function onUnauthorized(fn) { unauthorizedHandler = fn; }

/** JSON API call under /api/v1. Throws ApiError; 401 triggers the login view. */
export async function api(path, { method = 'GET', body, signal } = {}) {
  if (mockEnabled()) {
    const mocked = mockResponse(method, path);
    if (mocked !== undefined) return mocked;
  }
  let res;
  try {
    res = await fetch('/api/v1' + path, {
      method,
      signal,
      credentials: 'same-origin',
      headers: body !== undefined ? { 'Content-Type': 'application/json' } : {},
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
  } catch (e) {
    if (e && e.name === 'AbortError') throw e;
    throw new ApiError(0, 'network error');
  }
  if (res.status === 401 && !path.startsWith('/auth/')) {
    unauthorizedHandler();
    throw new ApiError(401, 'unauthorized');
  }
  if (res.status === 204) return null;
  let data = null;
  if ((res.headers.get('content-type') || '').includes('json')) data = await res.json().catch(() => null);
  if (!res.ok) throw new ApiError(res.status, (data && data.error) || res.statusText);
  return data;
}

// Server error messages (English) → user-facing text in the UI language.
const ERROR_TRANSLATIONS = [
  [/video not found/i, () => t('error.vodNotFound')],
  [/video on demand is disabled/i, () => t('error.vodDisabled')],
  [/video on demand needs ffmpeg/i, () => t('error.vodNoFfmpeg')],
  [/unknown ladder/i, () => t('error.vodUnknownLadder')],
  [/already being transcoded|is being transcoded/i, () => t('error.vodBusy')],
  [/source file is no longer available/i, () => t('error.vodNoSource')],
  [/unsupported file type: upload a video .*wmv/i, () => t('error.vodUnsupportedType')],
  [/broadcast not found/i, () => t('error.broadcastNotFound')],
  [/already exists/i, () => t('error.alreadyExists')],
  [/title must not be empty/i, () => t('error.titleRequired')],
  [/name must match/i, () => t('error.invalidName')],
  [/name cannot be changed/i, () => t('error.nameImmutable')],
  [/too long/i, () => t('error.tooLong')],
  [/already being recorded/i, () => t('error.alreadyRecording')],
  [/not being recorded/i, () => t('error.notRecording')],
  [/not live/i, () => t('error.notLive')],
  [/larger than the upload limit/i, () => t('error.fileTooLarge')],
  [/ffmpeg not available/i, () => t('error.noFfmpeg')],
  [/unsupported file type/i, () => t('error.unsupportedType')],
  [/could not process the video/i, () => t('error.cannotProcess')],
  [/timed out/i, () => t('error.timedOut')],
  [/file is empty/i, () => t('error.emptyFile')],
  [/could not find a free name/i, () => t('error.noFreeName')],
  [/ui_language must be/i, () => t('error.invalidLanguage')],
  [/ui_theme must be/i, () => t('error.invalidTheme')],
];

/** User-facing description of an error, in the UI language. */
export function describeError(err) {
  if (!err) return t('error.unknown');
  if (err.name === 'AbortError') return t('error.aborted');
  if (!(err instanceof ApiError)) return t('error.unexpected', { message: err.message || err });
  if (err.status === 0) return t('error.network');
  if (err.status === 401) return t('error.sessionExpired');
  if (err.status === 501) return t('error.notImplemented');
  for (const [re, text] of ERROR_TRANSLATIONS) if (re.test(err.message)) return text();
  if (err.status === 413) return t('error.fileTooLarge');
  if (err.status >= 500) return t('error.server', { status: err.status });
  return t('error.request', { status: err.status, message: err.message });
}

/**
 * Upload `file` as multipart field "file" with progress (XMLHttpRequest); `fields` are
 * extra text fields sent before the file. Returns {promise, abort}. Callbacks:
 * onProgress(loaded, total), onSent() when the last byte left the browser (server-side
 * processing starts).
 */
export function uploadFile(path, file, { onProgress, onSent, fields } = {}) {
  const xhr = new XMLHttpRequest();
  const promise = new Promise((resolve, reject) => {
    xhr.open('POST', '/api/v1' + path);
    xhr.responseType = 'json';
    xhr.upload.addEventListener('progress', (e) => onProgress && onProgress(e.loaded, e.lengthComputable ? e.total : file.size));
    xhr.upload.addEventListener('load', () => onSent && onSent());
    xhr.addEventListener('load', () => {
      const data = xhr.response;
      if (xhr.status === 401) {
        unauthorizedHandler();
        reject(new ApiError(401, 'unauthorized'));
      } else if (xhr.status >= 200 && xhr.status < 300) {
        resolve(data);
      } else {
        reject(new ApiError(xhr.status, (data && data.error) || xhr.statusText || 'upload failed'));
      }
    });
    xhr.addEventListener('error', () => reject(new ApiError(0, 'network error')));
    xhr.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
    const form = new FormData();
    for (const [k, v] of Object.entries(fields || {})) if (v != null && v !== '') form.append(k, v);
    form.append('file', file, file.name);
    xhr.send(form);
  });
  return { promise, abort: () => xhr.abort() };
}

// ---------------------------------------------------------------------------
// Formatting (UI language locale, see i18n.js)
// ---------------------------------------------------------------------------

const TIME = { hour: '2-digit', minute: '2-digit' };
const DAY_MONTH = { day: '2-digit', month: '2-digit' };

export const fmtInt = (n) => formatNumber(n, { maximumFractionDigits: 0 });

export const fmtBytes = formatBytes;

export function fmtBitrate(kbps) {
  if (kbps == null || !Number.isFinite(kbps)) return '—';
  return kbps >= 1000 ? `${formatNumber(kbps / 1000, { maximumFractionDigits: 1 })} Mbit/s` : `${formatNumber(kbps)} kbit/s`;
}

export const fmtDuration = formatDuration;

const toDate = (v) => (v instanceof Date ? v : new Date(v));
export function fmtDateTime(v) {
  if (!v) return '—';
  return formatDate(v);
}
export function fmtTime(v) {
  return formatDate(v, TIME);
}
/** "14:32" today, otherwise "07/10 14:32". */
export function fmtWhen(v) {
  const d = toDate(v);
  if (Number.isNaN(d.getTime())) return '—';
  const today = new Date();
  return d.toDateString() === today.toDateString() ? formatDate(d, TIME) : `${formatDate(d, DAY_MONTH)} ${formatDate(d, TIME)}`;
}
/** "da 12 min" / "for 12 min" since an instant. */
export function fmtSince(v) {
  const d = toDate(v);
  if (Number.isNaN(d.getTime())) return '';
  return t('time.since', { duration: fmtDuration((Date.now() - d.getTime()) / 1000) });
}

export const PROTOCOLS = [
  { key: 'flv', label: 'HTTP-FLV' },
  { key: 'ws_flv', label: 'WS-FLV' },
  { key: 'hls', label: 'LL-HLS' },
  { key: 'dash', label: 'LL-DASH' },
  { key: 'webrtc', label: 'WebRTC' },
];

/** Label of a broadcast status (offline / live / paused). */
export function statusLabel(status) {
  if (status === 'live') return t('status.live');
  if (status === 'paused') return t('status.paused');
  if (status === 'offline') return t('status.offline');
  return status || '—';
}

export function statusBadge(status) {
  return h('span', { class: `badge status-${status || 'offline'}` },
    status === 'live' ? h('span', { class: 'dot', 'aria-hidden': 'true' }) : null,
    statusLabel(status));
}

export function totalViewers(counts) {
  if (!counts) return 0;
  if (Number.isFinite(counts.total)) return counts.total;
  return PROTOCOLS.reduce((acc, p) => acc + (counts[p.key] || 0), 0);
}

/** Server-compatible slug (see db::broadcasts::slugify). */
export function slugify(title) {
  const map = { à: 'a', á: 'a', â: 'a', ä: 'a', ã: 'a', å: 'a', è: 'e', é: 'e', ê: 'e', ë: 'e', ì: 'i', í: 'i', î: 'i', ï: 'i',
    ò: 'o', ó: 'o', ô: 'o', ö: 'o', õ: 'o', ù: 'u', ú: 'u', û: 'u', ü: 'u', ç: 'c', ñ: 'n', ß: 'ss' };
  let out = '';
  let dash = false;
  for (const ch of title.toLowerCase()) {
    let m;
    if (/[a-z0-9_]/.test(ch)) m = ch;
    else if (map[ch]) m = map[ch];
    else { dash = true; continue; }
    if (dash && out) out += '-';
    dash = false;
    out += m;
  }
  return out.slice(0, 64).replace(/^[-_]+|[-_]+$/g, '');
}

export const NAME_RE = /^[a-z0-9_-]{1,64}$/;

// ---------------------------------------------------------------------------
// Toasts, dialogs, clipboard
// ---------------------------------------------------------------------------

export function toast(message, kind = 'info', timeout = 4500) {
  const region = document.getElementById('toasts');
  if (!region) return;
  const el = h('div', { class: `toast toast-${kind}`, role: kind === 'error' ? 'alert' : 'status' },
    icon(kind === 'error' ? 'alert' : kind === 'success' ? 'check' : 'info'),
    h('span', null, message),
    h('button', { type: 'button', class: 'toast-close', 'aria-label': t('toast.close'), onclick: () => el.remove() }, icon('x', 16)));
  region.append(el);
  if (timeout) setTimeout(() => el.remove(), timeout);
}

/** Modal dialog built on <dialog>. Returns the element; caller appends content. */
export function modal({ title, onClose, wide = false }) {
  const titleId = uid('dlg');
  const dlg = h('dialog', { class: 'modal' + (wide ? ' wide' : ''), 'aria-labelledby': titleId });
  const bodyEl = h('div', { class: 'modal-body' });
  dlg.append(
    h('header', { class: 'modal-head' },
      h('h2', { id: titleId }, title),
      h('button', { type: 'button', class: 'btn ghost icon-only', 'aria-label': t('common.close'), onclick: () => dlg.close('cancel') }, icon('x'))),
    bodyEl);
  dlg.addEventListener('close', () => { onClose && onClose(dlg.returnValue); dlg.remove(); });
  document.body.append(dlg);
  dlg.body = bodyEl;
  return dlg;
}

/** Confirmation dialog → Promise<boolean>. */
export function confirmDialog({ title, message, confirmLabel = t('common.confirm'), danger = false }) {
  return new Promise((resolve) => {
    let ok = false;
    const dlg = modal({ title, onClose: () => resolve(ok) });
    const cancel = h('button', { type: 'button', class: 'btn', onclick: () => dlg.close('cancel') }, t('common.cancel'));
    const confirm = h('button', { type: 'button', class: 'btn ' + (danger ? 'danger' : 'primary'), onclick: () => { ok = true; dlg.close('ok'); } }, confirmLabel);
    dlg.body.append(
      ...(Array.isArray(message) ? message : [message]).map((m) => (m instanceof Node ? m : h('p', null, m))),
      h('div', { class: 'modal-actions' }, cancel, confirm));
    dlg.showModal();
    (danger ? cancel : confirm).focus();
  });
}

export async function copyText(text) {
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch (_) { /* fall back below */ }
  const ta = h('textarea', { style: { position: 'fixed', top: '-1000px', opacity: '0' }, 'aria-hidden': 'true' });
  ta.value = text;
  document.body.append(ta);
  ta.select();
  let ok = false;
  try { ok = document.execCommand('copy'); } catch (_) { ok = false; }
  ta.remove();
  return ok;
}

/** Copy button: copies getText() and briefly shows a check mark. */
export function copyButton(getText, label = t('common.copy'), { showLabel = false, cls = 'btn' } = {}) {
  const btn = iconButton('copy', label, async () => {
    const ok = await copyText(typeof getText === 'function' ? getText() : getText);
    toast(ok ? t('toast.copied') : t('toast.copyFailed'), ok ? 'success' : 'error', 2500);
    if (ok) {
      btn.querySelector('.icon').replaceWith(icon('check'));
      setTimeout(() => { const i = btn.querySelector('.icon'); if (i) i.replaceWith(icon('copy')); }, 1500);
    }
  }, { showLabel, cls });
  return btn;
}

/** Read-only field with a copy button (and optional secret toggle / extra buttons). */
export function copyField({ label, value, secret = false, extra = [], hint, mono = true }) {
  const id = uid('f');
  const input = h('input', { id, type: secret ? 'password' : 'text', readonly: true, class: mono ? 'mono' : '', value: value || '', spellcheck: 'false', autocomplete: 'off', onfocus: (e) => e.target.select() });
  const buttons = [];
  if (secret) {
    const toggle = h('button', { type: 'button', class: 'btn icon-only', 'aria-pressed': 'false', 'aria-label': t('field.showKey'), title: t('field.showKey'), 'aria-controls': id }, icon('eye'));
    toggle.addEventListener('click', () => {
      const show = input.type === 'password';
      input.type = show ? 'text' : 'password';
      toggle.setAttribute('aria-pressed', String(show));
      toggle.setAttribute('aria-label', show ? t('field.hideKey') : t('field.showKey'));
      toggle.title = toggle.getAttribute('aria-label');
      toggle.querySelector('.icon').replaceWith(icon(show ? 'eyeOff' : 'eye'));
    });
    buttons.push(toggle);
  }
  buttons.push(copyButton(() => input.value, t('field.copyLabel', { label: label.toLowerCase() })));
  const wrap = h('div', { class: 'field' },
    h('label', { for: id }, label),
    h('div', { class: 'input-group' }, input, ...buttons, ...extra),
    hint ? h('p', { class: 'hint' }, hint) : null);
  wrap.input = input;
  // Only touch the DOM on real changes (keeps the user's selection on periodic refreshes).
  wrap.setValue = (v) => { if (input.value !== (v || '')) input.value = v || ''; };
  return wrap;
}

// ---------------------------------------------------------------------------
// Upload widget (waiting videos)
// ---------------------------------------------------------------------------

const VIDEO_EXT = ['mp4', 'mov', 'm4v', 'webm', 'mkv', 'avi'];

/**
 * File picker + upload with progress bar. `getLimits()` → {maxMb, ffmpeg}.
 * `onDone(response)` receives the API response.
 */
export function uploadWidget({ endpoint, getLimits, onDone, label = t('upload.defaultLabel') }) {
  const inputId = uid('file');
  const statusId = uid('st');
  const input = h('input', { id: inputId, type: 'file', class: 'file-input', accept: 'video/mp4,video/quicktime,video/x-m4v,video/webm,video/x-matroska,video/x-msvideo,.mp4,.mov,.m4v,.webm,.mkv,.avi', 'aria-describedby': statusId });
  const fileName = h('span', { class: 'file-name muted' }, t('upload.noFile'));
  const pick = h('label', { for: inputId, class: 'btn' }, icon('folder'), h('span', null, t('upload.choose')));
  const send = h('button', { type: 'button', class: 'btn primary', disabled: true }, icon('upload'), h('span', null, t('upload.send')));
  const cancel = h('button', { type: 'button', class: 'btn', hidden: true }, t('common.cancel'));
  const progress = h('progress', { max: '100', value: '0', hidden: true, 'aria-label': t('upload.progressLabel') });
  const status = h('p', { id: statusId, class: 'hint', 'aria-live': 'polite' });
  const limits = h('p', { class: 'hint' });
  const drop = h('div', { class: 'dropzone' },
    h('div', { class: 'dropzone-row' }, input, pick, fileName),
    h('div', { class: 'dropzone-row' }, send, cancel),
    progress, status);
  const root = h('div', { class: 'upload' }, h('p', { class: 'upload-title' }, label), drop, limits);
  let current = null;

  function renderLimits() {
    const { maxMb, ffmpeg } = getLimits();
    const max = maxMb ? t('upload.maxSize', { mb: fmtInt(maxMb) }) : '';
    limits.textContent = ffmpeg ? t('upload.limitsFfmpeg', { max }) : t('upload.limitsNoFfmpeg', { max });
  }

  function validate(file) {
    const { maxMb, ffmpeg } = getLimits();
    const ext = (file.name.split('.').pop() || '').toLowerCase();
    if (!VIDEO_EXT.includes(ext) && !(file.type || '').startsWith('video/')) return t('upload.unsupported');
    if (!ffmpeg && ext !== 'mp4') return t('upload.mp4Only');
    if (maxMb && file.size > maxMb * 1024 * 1024) return t('upload.tooLarge', { size: fmtBytes(file.size), max: fmtInt(maxMb) });
    return null;
  }

  function selected() {
    const file = input.files && input.files[0];
    status.textContent = '';
    status.classList.remove('error');
    if (!file) { fileName.textContent = t('upload.noFile'); send.disabled = true; return; }
    fileName.textContent = `${file.name} (${fmtBytes(file.size)})`;
    const err = validate(file);
    if (err) { status.textContent = err; status.classList.add('error'); }
    send.disabled = !!err;
  }

  function setBusy(busy) {
    send.disabled = busy;
    input.disabled = busy;
    pick.classList.toggle('disabled', busy);
    cancel.hidden = !busy;
    progress.hidden = !busy;
  }

  input.addEventListener('change', selected);
  for (const ev of ['dragenter', 'dragover']) drop.addEventListener(ev, (e) => { e.preventDefault(); drop.classList.add('drag'); });
  for (const ev of ['dragleave', 'drop']) drop.addEventListener(ev, () => drop.classList.remove('drag'));
  drop.addEventListener('drop', (e) => {
    e.preventDefault();
    if (current || !e.dataTransfer || !e.dataTransfer.files.length) return;
    input.files = e.dataTransfer.files;
    selected();
  });

  send.addEventListener('click', async () => {
    const file = input.files && input.files[0];
    if (!file || validate(file)) return;
    setBusy(true);
    progress.value = 0;
    progress.setAttribute('value', '0');
    status.classList.remove('error');
    status.textContent = t('upload.progressPct', { pct: 0 });
    current = uploadFile(endpoint, file, {
      onProgress(loaded, total) {
        const pct = total ? Math.min(100, Math.round((loaded / total) * 100)) : 0;
        progress.value = pct;
        status.textContent = t('upload.progress', { pct, loaded: fmtBytes(loaded), total: fmtBytes(total) });
      },
      onSent() {
        progress.removeAttribute('value'); // indeterminate while the server converts
        status.textContent = getLimits().ffmpeg ? t('upload.converting') : t('upload.saving');
      },
    });
    try {
      const res = await current.promise;
      status.textContent = t('upload.done');
      toast(t('upload.toastDone'), 'success');
      input.value = '';
      fileName.textContent = t('upload.noFile');
      onDone && onDone(res);
    } catch (e) {
      status.textContent = describeError(e);
      status.classList.add('error');
      if (e.name !== 'AbortError') toast(describeError(e), 'error');
    } finally {
      current = null;
      setBusy(false);
      send.disabled = !(input.files && input.files[0]);
    }
  });
  cancel.addEventListener('click', () => current && current.abort());

  renderLimits();
  root.refreshLimits = renderLimits;
  root.busy = () => !!current;
  return root;
}

/** Loads a classic script once (CDN libraries). */
const scriptCache = new Map();
export function loadScript(src, globalName) {
  if (globalName && window[globalName]) return Promise.resolve(window[globalName]);
  if (scriptCache.has(src)) return scriptCache.get(src);
  const p = new Promise((resolve, reject) => {
    const s = document.createElement('script');
    s.src = src;
    s.async = true;
    s.crossOrigin = 'anonymous';
    s.onload = () => resolve(globalName ? window[globalName] : true);
    s.onerror = () => { scriptCache.delete(src); s.remove(); reject(new Error('cannot load ' + src)); };
    document.head.append(s);
  });
  scriptCache.set(src, p);
  return p;
}

// ---------------------------------------------------------------------------
// Broadcast form pieces (create + edit dialogs)
// ---------------------------------------------------------------------------

/** Checkbox fieldset for the per-broadcast flags. */
export function flagFields(values = {}) {
  const item = (key, label, hint, def) => {
    const id = uid('cb');
    const hintId = uid('hint');
    return h('div', { class: 'check' },
      h('input', { type: 'checkbox', id, name: key, checked: values[key] ?? def, 'aria-describedby': hintId }),
      h('label', { for: id }, label, h('span', { id: hintId, class: 'hint' }, hint)));
  };
  return h('fieldset', { class: 'flags' },
    h('legend', null, t('flags.legend')),
    item('record', t('flags.record'), t('flags.recordHint'), false),
    item('enable_flv', t('flags.flv'), t('flags.flvHint'), true),
    item('enable_hls', t('flags.hls'), t('flags.hlsHint'), true),
    item('enable_dash', t('flags.dash'), t('flags.dashHint'), true),
    item('enable_webrtc', t('flags.webrtc'), t('flags.webrtcHint'), true));
}

export function readFlags(form) {
  const v = (k) => !!(form.elements[k] && form.elements[k].checked);
  return { record: v('record'), enable_flv: v('enable_flv'), enable_hls: v('enable_hls'), enable_dash: v('enable_dash'), enable_webrtc: v('enable_webrtc') };
}

/** Labelled text input / textarea. */
export function textField({ label, name, value = '', type = 'text', textarea = false, rows = 3, hint, attrs = {} }) {
  const id = uid('in');
  const hintId = hint ? uid('hint') : null;
  const control = textarea
    ? h('textarea', { id, name, rows: String(rows), 'aria-describedby': hintId, ...attrs })
    : h('input', { id, name, type, 'aria-describedby': hintId, ...attrs });
  control.value = value || '';
  const wrap = h('div', { class: 'field' }, h('label', { for: id }, label), control, hint ? h('p', { id: hintId, class: 'hint' }, hint) : null);
  wrap.control = control;
  return wrap;
}
