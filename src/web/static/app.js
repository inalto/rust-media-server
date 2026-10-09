// rms dashboard — entry point: UI language, login, role-aware shell, hash router, SSE
// (live updates with reconnect/backoff and polling fallback), broadcast list, "new
// broadcast" dialog and the settings page. The broadcast detail lives in detail.js, the
// video on demand section (#/vod) in vod.js, the admin "Utenti" pages in users.js, the
// user portal in portal.js and the bucket browser in browser.js.
//
// Roles (POST /auth/login and GET /auth/check answer {role, user?, scopes?, key_id?}):
// `admin` (the admin token) gets the admin navigation (Dirette / Video on demand /
// Utenti / Impostazioni) and the admin SSE /api/v1/events; `user` (an API key with the
// `account` scope) gets the portal (I miei bucket / Chiavi API / Account) and the
// account SSE /api/v1/account/events. A key without the `account` scope is refused
// (logged out with a message): it is meant for the storage API only.
//
// Language: <html lang> (set by the server) at startup, then GET /api/v1/public/ui
// before the login view, then GET /settings after login; the admin SSE `settings`
// event and the Settings page switch it at runtime: the static shell is re-translated
// (data-i18n) and the current view is re-rendered without a reload.
//
// Theme (theme.js): this browser's override (top-bar toggle, localStorage `rms.theme`)
// wins over the server default `ui_theme` (<html data-theme>, then /public/ui,
// /settings, the SSE `settings` event and the Settings page), which defaults to
// `system` (prefers-color-scheme, followed live).

import {
  api, ApiError, bus, store, describeError, onUnauthorized, h, icon, iconButton, setChildren, toast, uid,
  modal, copyText, uploadWidget, flagFields, readFlags, textField, slugify, NAME_RE,
  fmtInt, fmtDuration, fmtSince, statusBadge, totalViewers, PROTOCOLS, confirmDialog,
} from './core.js';
import { mountDetail } from './detail.js';
import { mountVod, vodSettingsPanel, vodSystemLine } from './vod.js';
import { mountUsers, mountUserDetail } from './users.js';
import { mountBuckets, mountKeys, mountAccount } from './portal.js';
import { mountBrowser } from './browser.js';
import { accountCtx } from './accounts.js';
import { t, tn, setLanguage, getLanguage, onLanguageChange, translateDom } from './i18n.js';
import {
  initTheme, getThemeChoice, getThemeOverride, getServerTheme, setServerTheme, setThemeOverride,
  nextTheme, onThemeChange,
} from './theme.js';

// Before anything renders (the inline script in index.html already set the attribute).
initTheme({ storageKey: 'rms.theme' });

const POLL_MS = 10000;
const SSE_MAX_BACKOFF = 30000;

const app = document.getElementById('app');
const topbar = document.getElementById('topbar');
const connEl = document.getElementById('conn');
const themeBtn = document.getElementById('theme-toggle');
const brandEl = document.getElementById('brand');
const whoamiEl = document.getElementById('whoami');
let unmount = null;
let authed = false;
/** 'admin' | 'user' once logged in; `session` = the auth/check (or login) answer. */
let role = null;
let session = null;

// ---------------------------------------------------------------------------
// Theme toggle (top bar): system → light → dark, stored in this browser only
// ---------------------------------------------------------------------------

const THEME_NAMES = { system: () => t('theme.system'), light: () => t('theme.light'), dark: () => t('theme.dark') };
const THEME_ICONS = { system: 'themeSystem', light: 'sun', dark: 'moon' };
const themeName = (theme) => (THEME_NAMES[theme] || THEME_NAMES.system)();

function renderThemeToggle() {
  const current = getThemeChoice();
  const next = nextTheme(current);
  const names = { current: themeName(current), next: themeName(next) };
  setChildren(themeBtn, icon(THEME_ICONS[current], 16));
  themeBtn.setAttribute('aria-label', t('theme.toggleLabel', names));
  themeBtn.title = t('theme.toggleTitle', names);
  themeBtn.dataset.theme = current;
}

themeBtn.addEventListener('click', () => setThemeOverride(nextTheme(getThemeChoice())));
onThemeChange(renderThemeToggle);
renderThemeToggle();

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

function showLogin(message) {
  authed = false;
  role = null;
  session = null;
  stopLiveUpdates();
  if (unmount) { unmount(); unmount = null; }
  store.clear();
  topbar.hidden = true;
  document.title = t('login.docTitle');
  const err = h('p', { class: 'error', role: 'alert', hidden: !message }, message || '');
  const token = textField({
    label: t('login.tokenLabel'), name: 'token', type: 'password',
    hint: t('login.tokenHint'),
    attrs: { required: true, autocomplete: 'current-password', spellcheck: 'false' },
  });
  const submit = h('button', { type: 'submit', class: 'btn primary block' }, t('login.submit'));
  // Hidden username so password managers can store the token.
  const user = h('input', { type: 'text', name: 'username', autocomplete: 'username', value: 'admin', hidden: true, tabindex: '-1', 'aria-hidden': 'true' });
  const form = h('form', { class: 'stack', novalidate: true }, user, token, err, submit);
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const value = token.control.value.trim();
    err.hidden = true;
    if (!value) { err.textContent = t('login.emptyToken'); err.hidden = false; token.control.focus(); return; }
    submit.disabled = true;
    try {
      const res = await api('/auth/login', { method: 'POST', body: { token: value } });
      await start(res);
    } catch (ex) {
      err.textContent = ex instanceof ApiError && ex.status === 401 ? t('login.invalidToken')
        : ex instanceof ApiError && ex.status === 403 ? t('error.userSuspended') : describeError(ex);
      err.hidden = false;
      token.control.select();
    } finally { submit.disabled = false; }
  });
  setChildren(app, h('section', { class: 'login' },
    h('div', { class: 'login-card' },
      h('div', { class: 'brand big' }, icon('radio', 28), h('span', null, 'rms')),
      h('h1', null, t('login.title')),
      h('p', { class: 'muted' }, t('login.lead')),
      form,
      h('div', { class: 'login-help' },
        h('p', null, icon('user', 16), h('span', null, t('login.helpAdmin'))),
        h('p', null, icon('key', 16), h('span', null, t('login.helpKeys')))))));
  token.control.focus();
}

onUnauthorized(() => { if (authed) showLogin(t('error.sessionExpired')); });

document.getElementById('logout').addEventListener('click', logout);

/** Enter the panel after a login or a session check: `auth` = {role, user?, scopes?, key_id?}. */
async function start(auth) {
  if (!auth || !auth.role) auth = await api('/auth/check');
  if (auth && auth.role === 'user') return startUser(auth);
  const [list, settings] = await Promise.all([api('/broadcasts'), api('/settings').catch(() => null)]);
  if (settings && settings.ui_theme) setServerTheme(settings.ui_theme);
  if (settings && settings.ui_language) setLanguage(settings.ui_language);
  authed = true;
  role = 'admin';
  session = auth || { role: 'admin' };
  store.replaceAll(list);
  applyRole();
  topbar.hidden = false;
  startLiveUpdates();
  route();
}

/** User portal: only keys with the `account` scope may use the panel. */
async function startUser(auth) {
  if (!(auth.scopes || []).includes('account')) {
    try { await api('/auth/logout', { method: 'POST' }); } catch (_) { /* ignore */ }
    showLogin(t('login.noAccountScope'));
    return;
  }
  authed = true;
  role = 'user';
  session = auth;
  applyRole();
  topbar.hidden = false;
  startLiveUpdates();
  route();
}

/** Navigation, brand link and "who am I" of the current role. */
function applyRole() {
  for (const a of topbar.querySelectorAll('[data-role]')) a.hidden = a.dataset.role !== role;
  const user = role === 'user' && session && session.user;
  brandEl.href = user ? '#/buckets' : '#/';
  brandEl.setAttribute('aria-label', user ? t('nav.brandLabelUser') : t('nav.brandLabel'));
  whoamiEl.hidden = !user;
  if (user) {
    setChildren(whoamiEl, icon('user', 16), h('span', { class: 'whoami-name' }, user.display_name || user.username));
    whoamiEl.title = t('nav.whoami', { name: user.username });
    whoamiEl.setAttribute('aria-label', t('nav.whoami', { name: user.username }));
  }
}

async function logout() {
  try { await api('/auth/logout', { method: 'POST' }); } catch (_) { /* ignore */ }
  showLogin();
}

// ---------------------------------------------------------------------------
// Admin SSE with reconnect + polling fallback
// ---------------------------------------------------------------------------

let es = null;
let backoff = 1000;
let reconnectTimer = null;
let pollTimer = null;
let everConnected = false;

function setConn(state) {
  connEl.dataset.state = state;
  connEl.querySelector('.conn-label').textContent = state === 'live' ? t('conn.live') : state === 'polling' ? t('conn.polling') : t('conn.connecting');
  connEl.title = state === 'live' ? t('conn.liveTitle') : t('conn.pollingTitle');
}

async function refreshList() {
  // The portal's views reload their own data.
  if (role === 'user') { bus.emit('resync'); return; }
  try { store.replaceAll(await api('/broadcasts')); } catch (_) { /* 401 handled globally */ }
}

function startPolling() {
  if (pollTimer) return;
  pollTimer = setInterval(() => { if (!document.hidden) refreshList(); }, POLL_MS);
}

function stopPolling() { clearInterval(pollTimer); pollTimer = null; }

function connectSSE() {
  clearTimeout(reconnectTimer);
  if (!authed) return;
  if (!('EventSource' in window)) { setConn('polling'); startPolling(); return; }
  setConn(everConnected ? 'polling' : 'connecting');
  // Admin: every event; user: the account stream (`vod`, `vod_deleted`, `heartbeat`).
  es = new EventSource(role === 'user' ? '/api/v1/account/events' : '/api/v1/events');
  const parse = (ev) => { try { return JSON.parse(ev.data); } catch (_) { return null; } };
  es.addEventListener('open', () => {
    backoff = 1000;
    setConn('live');
    stopPolling();
    if (everConnected) refreshList(); // resync what we missed while disconnected
    everConnected = true;
  });
  es.addEventListener('broadcast', (ev) => { const b = parse(ev); if (b && b.name) store.upsert(b); });
  es.addEventListener('broadcast_deleted', (ev) => { const d = parse(ev); if (d && d.name) store.remove(d.name); });
  es.addEventListener('viewers', (ev) => { const d = parse(ev); if (d && d.name) store.setViewers(d.name, d.counts); });
  // Video on demand items (vod.js keeps its own list).
  es.addEventListener('vod', (ev) => { const v = parse(ev); if (v && v.id) bus.emit('vod', v); });
  es.addEventListener('vod_deleted', (ev) => { const d = parse(ev); if (d && d.id) bus.emit('vod_deleted', d.id); });
  es.addEventListener('stream', (ev) => {
    const d = parse(ev);
    if (!d || !d.name) return;
    const b = store.broadcasts.get(d.name);
    const title = b ? b.title : d.name;
    if (d.status === 'live') toast(t('toast.streamLive', { title }), 'success');
    else if (d.status === 'offline') toast(t('toast.streamOffline', { title }), 'info');
  });
  // UI language / default theme changed (here or in another dashboard): apply them
  // (a language change re-renders the view; the theme only needs the attribute).
  es.addEventListener('settings', (ev) => {
    const d = parse(ev);
    if (d && d.ui_theme) setServerTheme(d.ui_theme);
    if (d && d.ui_language) setLanguage(d.ui_language);
  });
  es.addEventListener('heartbeat', () => setConn('live'));
  es.addEventListener('error', async () => {
    if (!es) return;
    es.close();
    es = null;
    setConn('polling');
    startPolling();
    // A 401 closes the stream for good: check the session before reconnecting.
    try { await api('/auth/check'); } catch (e) {
      if (e instanceof ApiError && (e.status === 401 || e.status === 403)) { showLogin(e.status === 403 ? t('error.userSuspended') : t('error.sessionExpired')); return; }
    }
    if (!authed) return;
    reconnectTimer = setTimeout(connectSSE, backoff);
    backoff = Math.min(backoff * 2, SSE_MAX_BACKOFF);
  });
}

function startLiveUpdates() { everConnected = false; backoff = 1000; connectSSE(); }

function stopLiveUpdates() {
  clearTimeout(reconnectTimer);
  stopPolling();
  if (es) { es.close(); es = null; }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

export function navigate(hash) {
  if (location.hash === hash) route();
  else location.hash = hash;
}

function setNav(active) {
  for (const a of topbar.querySelectorAll('[data-nav]')) {
    if (a.dataset.nav === active) a.setAttribute('aria-current', 'page');
    else a.removeAttribute('aria-current');
  }
}

/** `#/a/b%20c/` → ['a', 'b c', ''] (segments of the hash path, decoded). */
function hashSegments(hash) {
  return hash.replace(/^#\/?/, '').split('?')[0].split('/').map((s) => { try { return decodeURIComponent(s); } catch (_) { return s; } });
}

/** Directory path of a browser hash: the segments after the bucket ("/a/b/"). */
function dirOf(segs) {
  const parts = segs.filter((s) => s !== '');
  return parts.length ? `/${parts.join('/')}/` : '/';
}

function route() {
  if (!authed) return;
  if (unmount) { unmount(); unmount = null; }
  const hash = location.hash || '#/';
  window.scrollTo(0, 0);
  if (role === 'user') routeUser(hash);
  else routeAdmin(hash);
  app.focus({ preventScroll: true });
}

function routeUser(hash) {
  const segs = hashSegments(hash);
  const ctx = accountCtx({ currentKeyId: session && session.key_id });
  if (segs[0] === 'buckets' && segs[1]) {
    setNav('buckets');
    unmount = mountBrowser(app, {
      ctx, bucketName: segs[1], dir: dirOf(segs.slice(2)), baseHash: `#/buckets/${encodeURIComponent(segs[1])}`,
      back: { href: '#/buckets', label: t('browser.backToBuckets') },
    });
  } else if (segs[0] === 'keys') {
    setNav('keys');
    unmount = mountKeys(app, { ctx, onRevokedSelf: () => showLogin(t('keys.revokedSelf')) });
  } else if (segs[0] === 'account') {
    setNav('account');
    unmount = mountAccount(app, { ctx, onLogout: logout });
  } else {
    if (segs[0] !== 'buckets') history.replaceState(null, '', '#/buckets');
    setNav('buckets');
    unmount = mountBuckets(app, { ctx });
  }
}

function routeAdmin(hash) {
  const m = hash.match(/^#\/b\/([^/?#]+)/);
  const segs = hashSegments(hash);
  if (segs[0] === 'users' && segs[1] && segs[2] === 'buckets' && segs[3]) {
    setNav('users');
    unmount = mountBrowser(app, {
      ctx: accountCtx({ userId: segs[1] }), bucketName: segs[3], dir: dirOf(segs.slice(4)),
      baseHash: `#/users/${encodeURIComponent(segs[1])}/buckets/${encodeURIComponent(segs[3])}`,
      back: { href: `#/users/${encodeURIComponent(segs[1])}`, label: t('browser.backToUser', { name: segs[1] }) },
    });
  } else if (segs[0] === 'users' && segs[1]) {
    setNav('users');
    unmount = mountUserDetail(app, segs[1], { navigate });
  } else if (segs[0] === 'users') {
    setNav('users');
    unmount = mountUsers(app, { navigate });
  } else if (m) {
    setNav('list');
    unmount = mountDetail(app, decodeURIComponent(m[1]), { navigate });
  } else if (hash.startsWith('#/settings')) {
    setNav('settings');
    unmount = mountSettings(app);
  } else if (hash.startsWith('#/vod')) {
    setNav('vod');
    unmount = mountVod(app);
  } else {
    if (segs[0]) history.replaceState(null, '', '#/');
    setNav('list');
    unmount = mountList(app);
  }
}

window.addEventListener('hashchange', route);

// ---------------------------------------------------------------------------
// Broadcast list
// ---------------------------------------------------------------------------

function mountList(root) {
  document.title = t('list.docTitle');
  const cards = new Map();
  const grid = h('div', { class: 'cards', role: 'list' });
  const summary = h('p', { class: 'muted', 'aria-live': 'polite' });
  const empty = h('div', { class: 'empty', hidden: true },
    icon('radio', 32),
    h('h2', null, t('list.emptyTitle')),
    h('p', { class: 'muted' }, t('list.emptyText')),
    h('button', { type: 'button', class: 'btn primary', onclick: () => createDialog() }, icon('plus'), h('span', null, t('list.new'))));
  setChildren(root, h('div', { class: 'page' },
    h('div', { class: 'page-head' },
      h('div', null, h('h1', null, t('list.title')), summary),
      h('button', { type: 'button', class: 'btn primary', onclick: () => createDialog() }, icon('plus'), h('span', null, t('list.new')))),
    grid, empty));

  function card(b) {
    const el = h('article', { class: 'bcard', role: 'listitem' });
    const refs = {
      badge: h('span'),
      rec: h('span', { class: 'rec-chip', hidden: true, title: t('card.recTitle') }, h('span', { class: 'dot', 'aria-hidden': 'true' }), t('common.rec')),
      viewers: h('strong'),
      viewersWrap: null,
      title: h('a'),
      name: h('p', { class: 'mono muted small' }),
      meta: h('p', { class: 'meta' }),
      pause: h('button', { type: 'button', class: 'btn small' }),
    };
    refs.viewersWrap = h('span', { class: 'viewers-chip' }, icon('users', 16), refs.viewers);
    refs.pause.addEventListener('click', async () => {
      const cur = store.broadcasts.get(b.name);
      const paused = cur && cur.status === 'paused';
      refs.pause.disabled = true;
      try {
        store.upsert(await api(`/broadcasts/${encodeURIComponent(b.name)}/${paused ? 'resume' : 'pause'}`, { method: 'POST' }));
        toast(paused ? t('toast.resumed') : t('toast.paused'), 'success');
      } catch (e) { toast(describeError(e), 'error'); }
      refs.pause.disabled = false;
    });
    const copyEmbed = iconButton('code', t('card.embed'), async () => {
      try {
        const info = await api(`/broadcasts/${encodeURIComponent(b.name)}/embed`);
        const ok = await copyText(info.iframe);
        toast(ok ? t('toast.iframeCopied') : t('toast.copyFailedShort'), ok ? 'success' : 'error');
      } catch (e) { toast(describeError(e), 'error'); }
    }, { cls: 'btn small', attrs: { title: t('card.embedTitle'), 'aria-label': t('card.embedLabel', { title: b.title }) } });
    const open = h('a', { class: 'btn small primary', href: `#/b/${encodeURIComponent(b.name)}`, 'aria-label': t('card.openLabel', { title: b.title }) }, h('span', null, t('card.open')));
    el.append(
      h('div', { class: 'bcard-top' }, h('div', { class: 'badges' }, refs.badge, refs.rec), refs.viewersWrap),
      h('h2', { class: 'bcard-title' }, refs.title),
      refs.name,
      refs.meta,
      h('div', { class: 'bcard-actions' }, refs.pause, copyEmbed, open));
    el.refs = refs;
    return el;
  }

  function updateCard(el, b) {
    const r = el.refs;
    el.dataset.status = b.status;
    r.badge.replaceChildren(statusBadge(b.status));
    r.rec.hidden = !(b.live && b.live.recording);
    setViewers(el, b.viewers);
    r.title.textContent = b.title;
    r.title.href = `#/b/${encodeURIComponent(b.name)}`;
    r.name.textContent = b.name;
    if (b.live) {
      const parts = [];
      if (b.live.width && b.live.height) parts.push(`${b.live.width}×${b.live.height}`);
      if (b.live.fps) parts.push(`${Math.round(b.live.fps)} fps`);
      parts.push(t('card.signalFor', { duration: fmtDuration((Date.now() - new Date(b.live.started_at).getTime()) / 1000) }));
      r.meta.textContent = parts.join(' · ');
    } else {
      r.meta.textContent = b.description ? b.description : t('card.waitingSignal');
    }
    const paused = b.status === 'paused';
    setChildren(r.pause, icon(paused ? 'play' : 'pause', 16), h('span', null, paused ? t('action.resume') : t('action.pause')));
    r.pause.setAttribute('aria-label', paused ? t('card.resumeLabel', { title: b.title }) : t('card.pauseLabel', { title: b.title }));
  }

  function setViewers(el, counts) {
    const n = totalViewers(counts);
    el.refs.viewers.textContent = fmtInt(n);
    el.refs.viewersWrap.setAttribute('aria-label', tn('card.viewersLabel', n, { n: fmtInt(n) }));
    el.refs.viewersWrap.title = PROTOCOLS.map((p) => `${p.label}: ${fmtInt((counts && counts[p.key]) || 0)}`).join(' · ');
  }

  function renderSummary() {
    const all = [...store.broadcasts.values()];
    const live = all.filter((b) => b.status === 'live').length;
    const viewers = all.reduce((acc, b) => acc + totalViewers(b.viewers), 0);
    summary.textContent = all.length
      ? [tn('list.broadcasts', all.length), t('list.onAir', { n: live }), tn('list.viewers', viewers, { n: fmtInt(viewers) })].join(' · ')
      : '';
  }

  function render() {
    const all = [...store.broadcasts.values()].sort((a, b) => (a.created_at < b.created_at ? 1 : a.created_at > b.created_at ? -1 : a.name.localeCompare(b.name)));
    const seen = new Set();
    all.forEach((b, i) => {
      let el = cards.get(b.name);
      if (!el) { el = card(b); cards.set(b.name, el); }
      updateCard(el, b);
      if (grid.children[i] !== el) grid.insertBefore(el, grid.children[i] || null);
      seen.add(b.name);
    });
    for (const [name, el] of cards) if (!seen.has(name)) { el.remove(); cards.delete(name); }
    empty.hidden = all.length > 0;
    grid.hidden = all.length === 0;
    renderSummary();
  }

  const offs = [
    bus.on('broadcast', render),
    bus.on('deleted', render),
    bus.on('list', render),
    bus.on('viewers', ({ name, counts }) => { const el = cards.get(name); if (el) setViewers(el, counts); renderSummary(); }),
  ];
  render();
  // Refresh "on air for N min" labels.
  const tick = setInterval(() => { if (!document.hidden) render(); }, 30000);
  return () => { offs.forEach((off) => off()); clearInterval(tick); };
}

function createDialog() {
  const dlg = modal({ title: t('create.title') });
  const title = textField({ label: t('create.titleLabel'), name: 'title', attrs: { required: true, maxlength: '200', placeholder: t('create.titlePlaceholder') } });
  const nameField = textField({
    label: t('create.nameLabel'), name: 'name',
    hint: t('create.nameHint'),
    attrs: { maxlength: '64', pattern: '[a-z0-9_\\-]{1,64}', spellcheck: 'false', autocapitalize: 'off', class: 'mono' },
  });
  const desc = textField({ label: t('create.descLabel'), name: 'description', textarea: true, rows: 2, attrs: { maxlength: '5000' } });
  const waiting = textField({
    label: t('create.waitingLabel'), name: 'waiting_text', attrs: { maxlength: '500', placeholder: t('create.defaultPlaceholder', { text: t('waiting.builtin') }) },
    hint: t('create.waitingHint'),
  });
  api('/settings').then((s) => { if (s && s.default_waiting_text) waiting.control.placeholder = t('create.defaultPlaceholder', { text: s.default_waiting_text }); }).catch(() => {});
  const err = h('p', { class: 'error', role: 'alert', hidden: true });
  const submit = h('button', { type: 'submit', class: 'btn primary' }, t('create.submit'));
  const form = h('form', { class: 'stack', novalidate: true }, title, nameField, desc, waiting, flagFields({}), err,
    h('div', { class: 'modal-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => dlg.close('cancel') }, t('common.cancel')), submit));

  let manual = false;
  const preview = h('span', { class: 'url-preview mono' });
  nameField.append(h('p', { class: 'hint' }, t('create.playerLink'), preview));
  const updatePreview = () => { preview.textContent = `${location.origin}/player/${nameField.control.value || '…'}`; };
  title.control.addEventListener('input', () => {
    if (!manual) nameField.control.value = slugify(title.control.value);
    updatePreview();
  });
  nameField.control.addEventListener('input', () => {
    manual = nameField.control.value !== '';
    nameField.control.value = nameField.control.value.toLowerCase();
    updatePreview();
  });
  updatePreview();

  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    err.hidden = true;
    const titleText = title.control.value.trim();
    const n = nameField.control.value.trim();
    if (!titleText) { err.textContent = t('error.titleRequired'); err.hidden = false; title.control.focus(); return; }
    if (n && !NAME_RE.test(n)) {
      err.textContent = t('create.invalidName');
      err.hidden = false; nameField.control.focus(); return;
    }
    submit.disabled = true;
    try {
      const b = await api('/broadcasts', {
        method: 'POST',
        body: { title: titleText, name: n || undefined, description: desc.control.value, waiting_text: waiting.control.value, ...readFlags(form) },
      });
      store.upsert(b);
      dlg.close('ok');
      toast(t('toast.created', { title: b.title }), 'success');
      navigate(`#/b/${encodeURIComponent(b.name)}`);
    } catch (ex) {
      err.textContent = describeError(ex);
      err.hidden = false;
    } finally { submit.disabled = false; }
  });
  dlg.body.append(form);
  dlg.showModal();
  title.control.focus();
}

// ---------------------------------------------------------------------------
// Settings page
// ---------------------------------------------------------------------------

function mountSettings(root) {
  document.title = t('settings.docTitle');
  let destroyed = false;
  let settings = null;
  const timers = [];

  // Interface language (dashboard + player texts).
  const langId = uid('lang');
  const langSelect = h('select', { id: langId, name: 'ui_language' },
    h('option', { value: 'it' }, t('lang.name.it')),
    h('option', { value: 'en' }, t('lang.name.en')));
  langSelect.value = getLanguage();
  const langNoteId = uid('hint');
  langSelect.setAttribute('aria-describedby', langNoteId);
  const langSave = h('button', { type: 'submit', class: 'btn primary' }, t('settings.languageSave'));
  const langForm = h('form', { class: 'stack' },
    h('div', { class: 'field' }, h('label', { for: langId }, t('settings.language')), langSelect,
      h('p', { id: langNoteId, class: 'hint' }, t('settings.languageNote'))),
    h('div', null, langSave));

  // Default theme (server setting); each browser can still override it with the toggle.
  const themeId = uid('theme');
  const themeSelect = h('select', { id: themeId, name: 'ui_theme' },
    ...['system', 'light', 'dark'].map((v) => h('option', { value: v }, themeName(v))));
  let shownServerTheme = getServerTheme();
  themeSelect.value = shownServerTheme;
  const themeNoteId = uid('hint');
  themeSelect.setAttribute('aria-describedby', themeNoteId);
  const themeSave = h('button', { type: 'submit', class: 'btn primary' }, t('settings.themeSave'));
  const overrideBox = h('div', { class: 'theme-override', role: 'status' });
  const themeForm = h('form', { class: 'stack' },
    h('div', { class: 'field' }, h('label', { for: themeId }, t('settings.theme')), themeSelect,
      h('p', { id: themeNoteId, class: 'hint' }, t('settings.themeNote'))),
    h('div', null, themeSave),
    overrideBox);
  themeForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    themeSave.disabled = true;
    try {
      const res = await api('/settings', { method: 'PATCH', body: { ui_theme: themeSelect.value } });
      shownServerTheme = res.ui_theme;
      setServerTheme(res.ui_theme);
      toast(t('settings.themeSaved'), 'success');
    } catch (ex) {
      toast(describeError(ex), 'error');
    } finally { themeSave.disabled = false; }
  });
  function renderOverride() {
    const own = getThemeOverride();
    if (!own) { setChildren(overrideBox); overrideBox.hidden = true; return; }
    overrideBox.hidden = false;
    setChildren(overrideBox,
      h('p', { class: 'hint' }, t('settings.themeOverride', { theme: themeName(own) })),
      h('button', { type: 'button', class: 'btn small', onclick: () => setThemeOverride(null) }, t('settings.themeOverrideReset')));
  }
  renderOverride();
  langForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    langSave.disabled = true;
    try {
      const res = await api('/settings', { method: 'PATCH', body: { ui_language: langSelect.value } });
      // Re-renders this page in the new language (see onLanguageChange below).
      setLanguage(res.ui_language);
      toast(t('settings.languageSaved'), 'success');
    } catch (ex) {
      toast(describeError(ex), 'error');
    } finally { langSave.disabled = false; }
  });

  const videoBox = h('div', { class: 'waiting-current' });
  const upload = uploadWidget({
    endpoint: '/settings/waiting-video',
    getLimits: () => ({ maxMb: settings ? settings.max_upload_mb : null, ffmpeg: settings ? settings.ffmpeg_available : true }),
    onDone: (s) => { settings = s; renderVideo(); },
    label: t('settings.uploadLabel'),
  });
  const text = textField({
    label: t('settings.textLabel'), name: 'default_waiting_text', textarea: true, rows: 2,
    hint: t('settings.textHint', { text: t('waiting.builtin') }),
    attrs: { maxlength: '500' },
  });
  const textForm = h('form', { class: 'stack' }, text, h('div', null, h('button', { type: 'submit', class: 'btn primary' }, t('common.saveText'))));
  textForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    try {
      settings = await api('/settings', { method: 'PATCH', body: { default_waiting_text: text.control.value } });
      text.control.value = settings.default_waiting_text;
      toast(t('settings.textSaved'), 'success');
    } catch (ex) { toast(describeError(ex), 'error'); }
  });

  const systemBox = h('div', null, h('p', { class: 'muted' }, t('common.loading')));
  const clusterBox = h('div', null, h('p', { class: 'muted' }, t('common.loading')));
  const overviewBox = h('div', null, h('p', { class: 'muted' }, t('common.loading')));

  function renderVideo() {
    if (!settings) return;
    const url = settings.default_waiting_video_url;
    if (!url) {
      setChildren(videoBox,
        h('div', { class: 'waiting-placeholder' }, icon('film', 28), h('p', null, t('settings.noVideo'))),
        h('p', { class: 'hint' }, t('settings.noVideoHint')));
      return;
    }
    setChildren(videoBox,
      h('video', { class: 'waiting-preview', src: url, poster: settings.default_poster_url || null, controls: true, muted: true, loop: true, playsinline: true, preload: 'metadata' }),
      h('div', { class: 'row-between' },
        h('p', { class: 'hint' }, t('settings.videoHint')),
        iconButton('trash', t('action.deleteVideo'), async () => {
          const ok = await confirmDialog({ title: t('settings.deleteVideoTitle'), message: t('settings.deleteVideoText'), confirmLabel: t('action.delete'), danger: true });
          if (!ok) return;
          try {
            settings = await api('/settings/waiting-video', { method: 'DELETE' });
            toast(t('settings.videoDeleted'), 'success');
            renderVideo();
          } catch (ex) { toast(describeError(ex), 'error'); }
        }, { cls: 'btn danger-ghost' })));
  }

  function kv(pairs) {
    return h('dl', { class: 'kv' }, ...pairs.map(([k, v]) => [h('dt', null, k), h('dd', null, v)]));
  }

  // Cluster panel: backend, this node and (cluster mode) the registered nodes, refreshed
  // while the page is open.
  let clusterTimer = null;
  function renderCluster(c) {
    if (!c) { setChildren(clusterBox); return; }
    if (c.enabled && !clusterTimer) {
      clusterTimer = setInterval(async () => {
        if (document.hidden || destroyed) return;
        try { const s = await api('/system'); if (!destroyed) renderCluster(s.cluster); } catch { /* next tick */ }
      }, POLL_MS);
      timers.push(clusterTimer);
    }
    const facts = kv([
      [t('cluster.mode'), c.enabled ? t('cluster.enabled') : t('cluster.single')],
      [t('cluster.backend'), c.backend === 'postgres' ? 'PostgreSQL' : 'SQLite'],
      [t('cluster.nodeId'), h('span', { class: 'mono' }, c.node_id || '—')],
    ]);
    if (!c.enabled) {
      setChildren(clusterBox, facts, h('p', { class: 'hint' }, t('cluster.singleHint')));
      return;
    }
    const nodes = c.nodes || [];
    const state = (n) => {
      if (!n.alive) return h('span', { class: 'badge status-error' }, t('cluster.dead'));
      if (n.draining) return h('span', { class: 'badge status-paused' }, t('cluster.draining'));
      return h('span', { class: 'badge status-ok' }, t('cluster.alive'));
    };
    const ago = (v) => {
      const ms = Date.now() - new Date(v).getTime();
      return Number.isFinite(ms) ? t('cluster.ago', { duration: fmtDuration(Math.max(0, ms) / 1000) }) : '—';
    };
    setChildren(clusterBox, facts, nodes.length
      ? h('div', { class: 'table-wrap' }, h('table', null,
        h('thead', null, h('tr', null,
          h('th', { scope: 'col' }, t('cluster.colNode')),
          h('th', { scope: 'col' }, t('cluster.colAddress')),
          h('th', { scope: 'col' }, t('cluster.colHeartbeat')),
          h('th', { scope: 'col', class: 'num' }, t('cluster.colLive')),
          h('th', { scope: 'col', class: 'num' }, t('common.viewers')),
          h('th', { scope: 'col' }, t('common.status')))),
        h('tbody', null, ...nodes.map((n) => h('tr', null,
          h('td', null, h('span', { class: 'mono' }, n.node_id),
            h('span', { class: 'muted small block' }, [n.node_id === c.node_id ? t('cluster.thisNode') : null, `v${n.version}`].filter(Boolean).join(' · '))),
          h('td', null, h('span', { class: 'mono small' }, n.advertise_url)),
          h('td', null, ago(n.heartbeat)),
          h('td', { class: 'num' }, fmtInt(n.live_streams)),
          h('td', { class: 'num' }, fmtInt(n.viewers)),
          h('td', null, state(n)))))))
      : h('p', { class: 'muted' }, t('cluster.noNodes')));
  }

  async function loadSystem() {
    try {
      const s = await api('/system');
      if (destroyed) return;
      const ff = s.ffmpeg || {};
      const rtc = s.webrtc || {};
      const rtcPublic = rtc.public_port && rtc.public_port !== rtc.udp_port ? t('system.rtcPublicPort', { port: rtc.public_port }) : null;
      const rtcLine = !rtc.enabled ? t('system.rtcDisabled')
        : !rtc.available ? (rtc.error ? t('system.rtcUnavailableReason', { error: rtc.error }) : t('system.unavailable'))
          : [t('system.rtcStatus', { port: rtc.udp_port, sessions: fmtInt(rtc.sessions) }), rtcPublic, rtc.audio ? null : t('system.rtcVideoOnly')].filter(Boolean).join(' · ');
      renderCluster(s.cluster);
      setChildren(systemBox,
        ff.available ? null : h('div', { class: 'banner warn', role: 'alert' }, icon('alert'),
          h('div', null, h('strong', null, t('system.ffmpegMissingTitle')), t('system.ffmpegMissingText'))),
        kv([
          [t('system.version'), `rms ${s.version}`],
          [t('system.uptime'), fmtDuration(s.uptime_secs)],
          [t('system.ffmpeg'), ff.available ? (ff.version || t('system.available')) : t('system.unavailable')],
          [t('system.ffmpegPath'), ff.path || '—'],
          [t('system.rtmpBind'), s.rtmp_bind],
          [t('system.httpBind'), s.http_bind],
          [t('system.publicUrl'), s.public_url || t('system.publicUrlDerived')],
          [t('common.liveNow'), fmtInt(s.hub && s.hub.live_streams)],
          [t('system.webrtc'), rtcLine],
          [t('system.iceAddresses'), (rtc.candidates && rtc.candidates.length) ? rtc.candidates.join(', ') : '—'],
          [t('system.vod'), vodSystemLine(s.vod)],
        ]));
    } catch (e) {
      if (!(e instanceof ApiError && e.status === 401)) setChildren(systemBox, h('p', { class: 'error' }, describeError(e)));
    }
  }

  async function loadOverview() {
    try {
      const o = await api('/metrics/overview');
      if (destroyed) return;
      const tot = o.total_viewers || {};
      setChildren(overviewBox,
        h('div', { class: 'tiles' },
          h('div', { class: 'tile' }, h('span', { class: 'tile-label' }, t('common.liveNow')), h('span', { class: 'tile-value' }, fmtInt(o.live_broadcasts))),
          h('div', { class: 'tile' }, h('span', { class: 'tile-label' }, t('settings.totalViewers')), h('span', { class: 'tile-value' }, fmtInt(totalViewers(tot)))),
          ...PROTOCOLS.map((p) => h('div', { class: 'tile' }, h('span', { class: 'tile-label' }, p.label), h('span', { class: 'tile-value' }, fmtInt(tot[p.key] || 0))))),
        (o.per_broadcast || []).length
          ? h('div', { class: 'table-wrap' }, h('table', null,
            h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('common.broadcast')), h('th', { scope: 'col' }, t('common.status')), h('th', { scope: 'col', class: 'num' }, t('common.viewers')))),
            h('tbody', null, ...o.per_broadcast.map((r) => h('tr', null,
              h('td', null, h('a', { href: `#/b/${encodeURIComponent(r.name)}` }, r.title), h('span', { class: 'mono muted small block' }, r.name)),
              h('td', null, statusBadge(r.status)),
              h('td', { class: 'num' }, fmtInt(totalViewers(r.viewers))))))))
          : h('p', { class: 'muted' }, t('settings.noBroadcasts')));
    } catch (e) {
      if (!(e instanceof ApiError && e.status === 401)) setChildren(overviewBox, h('p', { class: 'error' }, describeError(e)));
    }
  }

  const section = (id, iconName, title, ...children) => h('section', { class: 'panel', 'aria-labelledby': id },
    h('div', { class: 'panel-head' }, h('h2', { id }, icon(iconName), title)), ...children);

  setChildren(root, h('div', { class: 'page' },
    h('div', { class: 'page-head' }, h('div', null, h('h1', null, t('settings.title')), h('p', { class: 'muted' }, t('settings.lead')))),
    section('set-language', 'sliders', t('settings.appearanceSection'), h('div', { class: 'grid-2' }, langForm, themeForm)),
    section('set-waiting', 'film', t('settings.waitingSection'),
      h('p', { class: 'hint lead' }, t('settings.waitingLead')),
      h('div', { class: 'grid-2' }, videoBox, h('div', { class: 'stack' }, upload, textForm))),
    section('set-overview', 'users', t('settings.overview'), overviewBox),
    vodSettingsPanel(),
    section('set-system', 'server', t('settings.system'), systemBox),
    section('set-cluster', 'layers', t('settings.cluster'), clusterBox)));

  (async () => {
    try {
      settings = await api('/settings');
      if (destroyed) return;
      if (settings.ui_language) langSelect.value = settings.ui_language;
      if (settings.ui_theme) themeSelect.value = shownServerTheme = settings.ui_theme;
      text.control.value = settings.default_waiting_text || '';
      upload.refreshLimits();
      renderVideo();
    } catch (e) {
      if (!(e instanceof ApiError && e.status === 401)) setChildren(videoBox, h('p', { class: 'error' }, describeError(e)));
    }
  })();
  loadSystem();
  loadOverview();
  timers.push(setInterval(() => { if (!document.hidden) loadOverview(); }, POLL_MS));
  const offs = [
    bus.on('broadcast', () => loadOverview()),
    // Toggle pressed, override reset, or default changed elsewhere (SSE).
    onThemeChange(() => {
      renderOverride();
      if (getServerTheme() !== shownServerTheme) themeSelect.value = shownServerTheme = getServerTheme();
    }),
  ];
  return () => { destroyed = true; timers.forEach(clearInterval); offs.forEach((off) => off()); };
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

/** Language for pages shown before login (no auth). */
async function loadPublicLanguage() {
  try {
    const res = await fetch('/api/v1/public/ui', { cache: 'no-store' });
    if (res.ok) {
      const ui = await res.json();
      if (ui && ui.theme) setServerTheme(ui.theme);
      if (ui && ui.language) setLanguage(ui.language);
    }
  } catch (_) { /* keep <html lang> */ }
}

// A language change re-translates the static shell and re-renders the current view.
onLanguageChange(() => {
  translateDom(document);
  renderThemeToggle();
  if (!authed) return;
  applyRole();
  setConn(connEl.dataset.state);
  const y = window.scrollY;
  route();
  window.scrollTo(0, y);
});

(async function boot() {
  // <html lang> is set by the server from the same setting: no flash of the wrong language.
  setLanguage(document.documentElement.lang);
  translateDom(document);
  try {
    const [, check] = await Promise.allSettled([loadPublicLanguage(), api('/auth/check')]);
    if (check.status === 'rejected') throw check.reason;
    await start(check.value);
  } catch (e) {
    if (e instanceof ApiError && e.status === 401) showLogin();
    else if (e instanceof ApiError && e.status === 403) showLogin(t('error.userSuspended'));
    else {
      setChildren(app, h('div', { class: 'empty' }, icon('alert', 32), h('h1', null, t('boot.unreachable')),
        h('p', { class: 'muted' }, describeError(e)),
        h('button', { type: 'button', class: 'btn primary', onclick: () => location.reload() }, t('common.retry'))));
    }
  }
})();

