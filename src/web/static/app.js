// rms dashboard — entry point: login, hash router, admin SSE (live updates with
// reconnect/backoff and polling fallback), broadcast list, "Nuova diretta" dialog and
// the settings page. The broadcast detail lives in detail.js.

import {
  api, ApiError, bus, store, describeError, onUnauthorized, h, icon, iconButton, setChildren, toast,
  modal, copyText, uploadWidget, flagFields, readFlags, textField, slugify, NAME_RE,
  fmtInt, fmtDuration, fmtSince, statusBadge, totalViewers, PROTOCOLS, confirmDialog,
} from './core.js';
import { mountDetail } from './detail.js';

const POLL_MS = 10000;
const SSE_MAX_BACKOFF = 30000;

const app = document.getElementById('app');
const topbar = document.getElementById('topbar');
const connEl = document.getElementById('conn');
let unmount = null;
let authed = false;

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

function showLogin(message) {
  authed = false;
  stopLiveUpdates();
  if (unmount) { unmount(); unmount = null; }
  store.clear();
  topbar.hidden = true;
  document.title = 'Accesso · rms';
  const err = h('p', { class: 'error', role: 'alert', hidden: !message }, message || '');
  const token = textField({
    label: 'Token di amministrazione', name: 'token', type: 'password',
    hint: 'Lo trovi nel file data/admin_token del server (o nel log al primo avvio).',
    attrs: { required: true, autocomplete: 'current-password', spellcheck: 'false' },
  });
  const submit = h('button', { type: 'submit', class: 'btn primary block' }, 'Accedi');
  // Hidden username so password managers can store the token.
  const user = h('input', { type: 'text', name: 'username', autocomplete: 'username', value: 'admin', hidden: true, tabindex: '-1', 'aria-hidden': 'true' });
  const form = h('form', { class: 'stack', novalidate: true }, user, token, err, submit);
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const value = token.control.value.trim();
    err.hidden = true;
    if (!value) { err.textContent = 'Inserisci il token.'; err.hidden = false; token.control.focus(); return; }
    submit.disabled = true;
    try {
      await api('/auth/login', { method: 'POST', body: { token: value } });
      await start();
    } catch (ex) {
      err.textContent = ex instanceof ApiError && ex.status === 401 ? 'Token non valido.' : describeError(ex);
      err.hidden = false;
      token.control.select();
    } finally { submit.disabled = false; }
  });
  setChildren(app, h('section', { class: 'login' },
    h('div', { class: 'login-card' },
      h('div', { class: 'brand big' }, icon('radio', 28), h('span', null, 'rms')),
      h('h1', null, 'Pannello delle dirette'),
      h('p', { class: 'muted' }, 'Accedi per gestire le dirette, le impostazioni di OBS e il codice di embed.'),
      form)));
  token.control.focus();
}

onUnauthorized(() => { if (authed) showLogin('Sessione scaduta: accedi di nuovo.'); });

document.getElementById('logout').addEventListener('click', async () => {
  try { await api('/auth/logout', { method: 'POST' }); } catch (_) { /* ignore */ }
  showLogin();
});

async function start() {
  const list = await api('/broadcasts');
  authed = true;
  store.replaceAll(list);
  topbar.hidden = false;
  startLiveUpdates();
  route();
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
  connEl.querySelector('.conn-label').textContent = state === 'live' ? 'Tempo reale' : state === 'polling' ? 'Riconnessione…' : 'Connessione…';
  connEl.title = state === 'live' ? 'Aggiornamenti in tempo reale attivi' : 'Aggiornamenti in tempo reale non disponibili: aggiorno ogni 10 secondi';
}

async function refreshList() {
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
  es = new EventSource('/api/v1/events');
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
  es.addEventListener('stream', (ev) => {
    const d = parse(ev);
    if (!d || !d.name) return;
    const b = store.broadcasts.get(d.name);
    const title = b ? b.title : d.name;
    if (d.status === 'live') toast(`«${title}» riceve il segnale ed è in diretta.`, 'success');
    else if (d.status === 'offline') toast(`«${title}»: il segnale si è interrotto.`, 'info');
  });
  es.addEventListener('heartbeat', () => setConn('live'));
  es.addEventListener('error', async () => {
    if (!es) return;
    es.close();
    es = null;
    setConn('polling');
    startPolling();
    // A 401 closes the stream for good: check the session before reconnecting.
    try { await api('/auth/check'); } catch (e) { if (e instanceof ApiError && e.status === 401) { showLogin('Sessione scaduta: accedi di nuovo.'); return; } }
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

function route() {
  if (!authed) return;
  if (unmount) { unmount(); unmount = null; }
  const hash = location.hash || '#/';
  const m = hash.match(/^#\/b\/([^/?#]+)/);
  window.scrollTo(0, 0);
  if (m) {
    setNav('list');
    unmount = mountDetail(app, decodeURIComponent(m[1]), { navigate });
  } else if (hash.startsWith('#/settings')) {
    setNav('settings');
    unmount = mountSettings(app);
  } else {
    setNav('list');
    unmount = mountList(app);
  }
  app.focus({ preventScroll: true });
}

window.addEventListener('hashchange', route);

// ---------------------------------------------------------------------------
// Broadcast list
// ---------------------------------------------------------------------------

function mountList(root) {
  document.title = 'Dirette · rms';
  const cards = new Map();
  const grid = h('div', { class: 'cards', role: 'list' });
  const summary = h('p', { class: 'muted', 'aria-live': 'polite' });
  const empty = h('div', { class: 'empty', hidden: true },
    icon('radio', 32),
    h('h2', null, 'Nessuna diretta'),
    h('p', { class: 'muted' }, 'Crea la prima diretta: otterrai le impostazioni da inserire in OBS e il codice da incorporare nel sito.'),
    h('button', { type: 'button', class: 'btn primary', onclick: () => createDialog() }, icon('plus'), h('span', null, 'Nuova diretta')));
  setChildren(root, h('div', { class: 'page' },
    h('div', { class: 'page-head' },
      h('div', null, h('h1', null, 'Dirette'), summary),
      h('button', { type: 'button', class: 'btn primary', onclick: () => createDialog() }, icon('plus'), h('span', null, 'Nuova diretta'))),
    grid, empty));

  function card(b) {
    const el = h('article', { class: 'bcard', role: 'listitem' });
    const refs = {
      badge: h('span'),
      rec: h('span', { class: 'rec-chip', hidden: true, title: 'Registrazione in corso' }, h('span', { class: 'dot', 'aria-hidden': 'true' }), 'REC'),
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
        toast(paused ? 'Diretta ripresa.' : 'Diretta in pausa: i player mostrano il video di attesa.', 'success');
      } catch (e) { toast(describeError(e), 'error'); }
      refs.pause.disabled = false;
    });
    const copyEmbed = iconButton('code', 'Embed', async () => {
      try {
        const info = await api(`/broadcasts/${encodeURIComponent(b.name)}/embed`);
        const ok = await copyText(info.iframe);
        toast(ok ? 'Codice iframe copiato negli appunti.' : 'Copia non riuscita.', ok ? 'success' : 'error');
      } catch (e) { toast(describeError(e), 'error'); }
    }, { cls: 'btn small', attrs: { title: 'Copia il codice iframe da incollare nel sito', 'aria-label': `Copia il codice embed di ${b.title}` } });
    const open = h('a', { class: 'btn small primary', href: `#/b/${encodeURIComponent(b.name)}`, 'aria-label': `Apri ${b.title}` }, h('span', null, 'Apri'));
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
      parts.push(`segnale ${fmtSince(b.live.started_at)}`);
      r.meta.textContent = parts.join(' · ');
    } else {
      r.meta.textContent = b.description ? b.description : 'In attesa del segnale da OBS';
    }
    const paused = b.status === 'paused';
    setChildren(r.pause, icon(paused ? 'play' : 'pause', 16), h('span', null, paused ? 'Riprendi' : 'Pausa'));
    r.pause.setAttribute('aria-label', `${paused ? 'Riprendi' : 'Metti in pausa'} ${b.title}`);
  }

  function setViewers(el, counts) {
    const n = totalViewers(counts);
    el.refs.viewers.textContent = fmtInt(n);
    el.refs.viewersWrap.setAttribute('aria-label', `${fmtInt(n)} spettatori`);
    el.refs.viewersWrap.title = PROTOCOLS.map((p) => `${p.label}: ${fmtInt((counts && counts[p.key]) || 0)}`).join(' · ');
  }

  function renderSummary() {
    const all = [...store.broadcasts.values()];
    const live = all.filter((b) => b.status === 'live').length;
    const viewers = all.reduce((acc, b) => acc + totalViewers(b.viewers), 0);
    summary.textContent = all.length
      ? `${all.length} ${all.length === 1 ? 'diretta' : 'dirette'} · ${live} in onda · ${fmtInt(viewers)} ${viewers === 1 ? 'spettatore' : 'spettatori'}`
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
  // Refresh "segnale da N min" labels.
  const tick = setInterval(() => { if (!document.hidden) render(); }, 30000);
  return () => { offs.forEach((off) => off()); clearInterval(tick); };
}

function createDialog() {
  const dlg = modal({ title: 'Nuova diretta' });
  const title = textField({ label: 'Titolo', name: 'title', attrs: { required: true, maxlength: '200', placeholder: 'Es. Messa della domenica' } });
  const nameField = textField({
    label: 'Nome nell\'URL', name: 'name',
    hint: 'Generato dal titolo; usato in tutti i link (lettere minuscole, cifre, «-» e «_»). Non si può cambiare dopo.',
    attrs: { maxlength: '64', pattern: '[a-z0-9_\\-]{1,64}', spellcheck: 'false', autocapitalize: 'off', class: 'mono' },
  });
  const desc = textField({ label: 'Descrizione (facoltativa)', name: 'description', textarea: true, rows: 2, attrs: { maxlength: '5000' } });
  const waiting = textField({
    label: 'Testo di attesa (facoltativo)', name: 'waiting_text', attrs: { maxlength: '500', placeholder: 'Predefinito: La diretta inizierà a breve' },
    hint: 'Mostrato dal player prima dell\'inizio e durante le pause. Se lo lasci vuoto si usa il testo predefinito globale (Impostazioni).',
  });
  api('/settings').then((s) => { if (s && s.default_waiting_text) waiting.control.placeholder = `Predefinito: ${s.default_waiting_text}`; }).catch(() => {});
  const err = h('p', { class: 'error', role: 'alert', hidden: true });
  const submit = h('button', { type: 'submit', class: 'btn primary' }, 'Crea diretta');
  const form = h('form', { class: 'stack', novalidate: true }, title, nameField, desc, waiting, flagFields({}), err,
    h('div', { class: 'modal-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => dlg.close('cancel') }, 'Annulla'), submit));

  let manual = false;
  const preview = h('span', { class: 'url-preview mono' });
  nameField.append(h('p', { class: 'hint' }, 'Link del player: ', preview));
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
    const t = title.control.value.trim();
    const n = nameField.control.value.trim();
    if (!t) { err.textContent = 'Il titolo è obbligatorio.'; err.hidden = false; title.control.focus(); return; }
    if (n && !NAME_RE.test(n)) {
      err.textContent = 'Nome non valido: usa solo lettere minuscole, cifre, «-» e «_» (max 64 caratteri).';
      err.hidden = false; nameField.control.focus(); return;
    }
    submit.disabled = true;
    try {
      const b = await api('/broadcasts', {
        method: 'POST',
        body: { title: t, name: n || undefined, description: desc.control.value, waiting_text: waiting.control.value, ...readFlags(form) },
      });
      store.upsert(b);
      dlg.close('ok');
      toast(`Diretta «${b.title}» creata.`, 'success');
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
  document.title = 'Impostazioni · rms';
  let destroyed = false;
  let settings = null;
  const timers = [];

  const videoBox = h('div', { class: 'waiting-current' });
  const upload = uploadWidget({
    endpoint: '/settings/waiting-video',
    getLimits: () => ({ maxMb: settings ? settings.max_upload_mb : null, ffmpeg: settings ? settings.ffmpeg_available : true }),
    onDone: (s) => { settings = s; renderVideo(); },
    label: 'Carica il video predefinito',
  });
  const text = textField({
    label: 'Testo di attesa predefinito', name: 'default_waiting_text', textarea: true, rows: 2,
    hint: 'Usato dalle dirette che non hanno un proprio testo di attesa. Lascia vuoto per ripristinare «La diretta inizierà a breve».',
    attrs: { maxlength: '500' },
  });
  const textForm = h('form', { class: 'stack' }, text, h('div', null, h('button', { type: 'submit', class: 'btn primary' }, 'Salva testo')));
  textForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    try {
      settings = await api('/settings', { method: 'PATCH', body: { default_waiting_text: text.control.value } });
      text.control.value = settings.default_waiting_text;
      toast('Testo predefinito salvato.', 'success');
    } catch (ex) { toast(describeError(ex), 'error'); }
  });

  const systemBox = h('div', null, h('p', { class: 'muted' }, 'Caricamento…'));
  const overviewBox = h('div', null, h('p', { class: 'muted' }, 'Caricamento…'));

  function renderVideo() {
    if (!settings) return;
    const url = settings.default_waiting_video_url;
    if (!url) {
      setChildren(videoBox,
        h('div', { class: 'waiting-placeholder' }, icon('film', 28), h('p', null, 'Nessun video predefinito')),
        h('p', { class: 'hint' }, 'Le dirette senza un proprio video mostrano il titolo e il testo di attesa su sfondo animato.'));
      return;
    }
    setChildren(videoBox,
      h('video', { class: 'waiting-preview', src: url, poster: settings.default_poster_url || null, controls: true, muted: true, loop: true, playsinline: true, preload: 'metadata' }),
      h('div', { class: 'row-between' },
        h('p', { class: 'hint' }, 'Usato da tutte le dirette senza un video di attesa proprio.'),
        iconButton('trash', 'Elimina video', async () => {
          const ok = await confirmDialog({ title: 'Eliminare il video predefinito?', message: 'Le dirette senza un video proprio mostreranno il testo di attesa.', confirmLabel: 'Elimina', danger: true });
          if (!ok) return;
          try {
            settings = await api('/settings/waiting-video', { method: 'DELETE' });
            toast('Video predefinito eliminato.', 'success');
            renderVideo();
          } catch (ex) { toast(describeError(ex), 'error'); }
        }, { cls: 'btn danger-ghost' })));
  }

  function kv(pairs) {
    return h('dl', { class: 'kv' }, ...pairs.map(([k, v]) => [h('dt', null, k), h('dd', null, v)]));
  }

  async function loadSystem() {
    try {
      const s = await api('/system');
      if (destroyed) return;
      const ff = s.ffmpeg || {};
      setChildren(systemBox,
        ff.available ? null : h('div', { class: 'banner warn', role: 'alert' }, icon('alert'),
          h('div', null, h('strong', null, 'ffmpeg non è disponibile sul server. '),
            'Il video di attesa deve essere un file MP4 (H.264/AAC) e viene usato così com\'è (senza immagine di anteprima); le registrazioni restano in formato FLV. Installa ffmpeg o indica il percorso in [ffmpeg] path.')),
        kv([
          ['Versione', `rms ${s.version}`],
          ['In esecuzione da', fmtDuration(s.uptime_secs)],
          ['ffmpeg', ff.available ? (ff.version || 'disponibile') : 'non disponibile'],
          ['Percorso ffmpeg', ff.path || '—'],
          ['RTMP in ascolto su', s.rtmp_bind],
          ['HTTP in ascolto su', s.http_bind],
          ['URL pubblico', s.public_url || 'dedotto dalla richiesta (Host)'],
          ['Dirette in onda', fmtInt(s.hub && s.hub.live_streams)],
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
          h('div', { class: 'tile' }, h('span', { class: 'tile-label' }, 'Dirette in onda'), h('span', { class: 'tile-value' }, fmtInt(o.live_broadcasts))),
          h('div', { class: 'tile' }, h('span', { class: 'tile-label' }, 'Spettatori totali'), h('span', { class: 'tile-value' }, fmtInt(totalViewers(tot)))),
          ...PROTOCOLS.filter((p) => p.key !== 'webrtc').map((p) => h('div', { class: 'tile' }, h('span', { class: 'tile-label' }, p.label), h('span', { class: 'tile-value' }, fmtInt(tot[p.key] || 0))))),
        (o.per_broadcast || []).length
          ? h('div', { class: 'table-wrap' }, h('table', null,
            h('thead', null, h('tr', null, h('th', { scope: 'col' }, 'Diretta'), h('th', { scope: 'col' }, 'Stato'), h('th', { scope: 'col', class: 'num' }, 'Spettatori'))),
            h('tbody', null, ...o.per_broadcast.map((r) => h('tr', null,
              h('td', null, h('a', { href: `#/b/${encodeURIComponent(r.name)}` }, r.title), h('span', { class: 'mono muted small block' }, r.name)),
              h('td', null, statusBadge(r.status)),
              h('td', { class: 'num' }, fmtInt(totalViewers(r.viewers))))))))
          : h('p', { class: 'muted' }, 'Nessuna diretta.'));
    } catch (e) {
      if (!(e instanceof ApiError && e.status === 401)) setChildren(overviewBox, h('p', { class: 'error' }, describeError(e)));
    }
  }

  const section = (id, iconName, title, ...children) => h('section', { class: 'panel', 'aria-labelledby': id },
    h('div', { class: 'panel-head' }, h('h2', { id }, icon(iconName), title)), ...children);

  setChildren(root, h('div', { class: 'page' },
    h('div', { class: 'page-head' }, h('div', null, h('h1', null, 'Impostazioni'), h('p', { class: 'muted' }, 'Valori predefiniti per tutte le dirette e informazioni sul server.'))),
    section('set-waiting', 'film', 'Video di attesa predefinito',
      h('p', { class: 'hint lead' }, 'Mostrato in loop dai player delle dirette che non hanno un proprio video di attesa, quando sono offline o in pausa.'),
      h('div', { class: 'grid-2' }, videoBox, h('div', { class: 'stack' }, upload, textForm))),
    section('set-overview', 'users', 'Panoramica', overviewBox),
    section('set-system', 'server', 'Sistema', systemBox)));

  (async () => {
    try {
      settings = await api('/settings');
      if (destroyed) return;
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
  const offs = [bus.on('broadcast', () => loadOverview())];
  return () => { destroyed = true; timers.forEach(clearInterval); offs.forEach((off) => off()); };
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

(async function boot() {
  try {
    await api('/auth/check');
    await start();
  } catch (e) {
    if (e instanceof ApiError && e.status === 401) showLogin();
    else {
      setChildren(app, h('div', { class: 'empty' }, icon('alert', 32), h('h1', null, 'Server non raggiungibile'),
        h('p', { class: 'muted' }, describeError(e)),
        h('button', { type: 'button', class: 'btn primary', onclick: () => location.reload() }, 'Riprova')));
    }
  }
})();

