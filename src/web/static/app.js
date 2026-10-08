// PHASE 1 placeholder dashboard (Italian UI). Replaced by the feat/dashboard branch.
// Talks only to the documented /api/v1 endpoints; authentication via the rms_token cookie.
(function () {
  'use strict';

  const $ = (sel, root) => (root || document).querySelector(sel);
  const api = async (method, path, body) => {
    const opts = { method, credentials: 'same-origin', headers: {} };
    if (body !== undefined) {
      opts.headers['Content-Type'] = 'application/json';
      opts.body = JSON.stringify(body);
    }
    const res = await fetch('/api/v1' + path, opts);
    if (res.status === 401) {
      showLogin();
      throw new Error('Non autorizzato');
    }
    if (!res.ok) {
      let msg = res.status + ' ' + res.statusText;
      try { msg = (await res.json()).error || msg; } catch (_) { /* ignore */ }
      throw new Error(msg);
    }
    return res.status === 204 ? null : res.json();
  };

  const STATUS_LABEL = { live: 'In diretta', offline: 'Offline', paused: 'In pausa' };
  const embedCache = new Map();
  let events = null;
  let pollTimer = null;

  function showLogin() {
    $('#login-view').hidden = false;
    $('#app-view').hidden = true;
    $('#logout').hidden = true;
    if (events) { events.close(); events = null; }
    if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
  }

  async function showApp() {
    $('#login-view').hidden = true;
    $('#app-view').hidden = false;
    $('#logout').hidden = false;
    await refresh();
    loadSystem();
    connectEvents();
    if (!pollTimer) pollTimer = setInterval(refresh, 10000);
  }

  async function loadSystem() {
    try {
      const s = await api('GET', '/system');
      const ff = s.ffmpeg.available ? 'ffmpeg disponibile' : 'ffmpeg non trovato';
      $('#system-info').textContent = 'v' + s.version + ' · ' + s.hub.live_streams + ' dirette attive · ' + ff;
    } catch (_) { /* ignore */ }
  }

  function fmtBytes(n) {
    if (n > 1e9) return (n / 1e9).toFixed(2) + ' GB';
    if (n > 1e6) return (n / 1e6).toFixed(1) + ' MB';
    return Math.round(n / 1e3) + ' kB';
  }

  function render(list) {
    const root = $('#broadcasts');
    $('#empty').hidden = list.length > 0;
    const seen = new Set();
    for (const b of list) {
      seen.add(b.name);
      let el = root.querySelector('[data-name="' + b.name + '"]');
      if (!el) {
        el = $('#broadcast-tpl').content.firstElementChild.cloneNode(true);
        el.dataset.name = b.name;
        wire(el);
        root.appendChild(el);
      }
      fill(el, b);
    }
    for (const el of Array.from(root.children)) {
      if (!seen.has(el.dataset.name)) el.remove();
    }
  }

  function fill(el, b) {
    el._b = b;
    $('.b-title', el).textContent = b.title;
    $('.b-name', el).textContent = b.name + (b.description ? ' · ' + b.description : '');
    const st = $('.b-status', el);
    st.textContent = STATUS_LABEL[b.status] || b.status;
    st.className = 'badge b-status ' + b.status;
    const live = b.live;
    $('.b-live', el).textContent = live
      ? [live.video_codec, live.width && live.height ? live.width + '×' + live.height : null,
         live.fps ? Math.round(live.fps) + ' fps' : null, live.audio_codec,
         'da ' + new Date(live.started_at).toLocaleTimeString('it-IT'),
         'ricevuti ' + fmtBytes(live.bytes_in), live.publisher_ip ? 'encoder ' + live.publisher_ip : null]
          .filter(Boolean).join(' · ')
      : 'Nessun encoder collegato';
    const v = b.viewers;
    $('.b-viewers', el).textContent = 'Spettatori: ' + v.total +
      ' (FLV ' + v.flv + ', WS-FLV ' + v.ws_flv + ', HLS ' + v.hls + ', DASH ' + v.dash + ')';
    $('.b-obs-server', el).textContent = b.obs.server;
    const key = $('.b-obs-key', el);
    key.textContent = key.classList.contains('secret') ? '••••••••••••••••' : b.obs.stream_key;
    $('.b-url-flv', el).textContent = b.urls.flv;
    $('.b-url-wsflv', el).textContent = b.urls.ws_flv;
    $('.b-url-hls', el).textContent = b.urls.hls;
    $('.b-url-dash', el).textContent = b.urls.dash;
    const pl = $('.b-url-player', el);
    pl.textContent = b.urls.player;
    pl.href = b.urls.player;
    $('.act-pause', el).textContent = b.paused ? 'Riprendi' : 'Metti in pausa';
    $('.act-disconnect', el).disabled = !live;
  }

  function wire(el) {
    const name = () => el.dataset.name;
    const run = async (fn) => {
      try { await fn(); await refresh(); } catch (e) { alert(e.message); }
    };
    $('.act-pause', el).addEventListener('click', () =>
      run(() => api('POST', '/broadcasts/' + name() + (el._b.paused ? '/resume' : '/pause'))));
    $('.act-disconnect', el).addEventListener('click', () =>
      run(() => api('POST', '/broadcasts/' + name() + '/disconnect')));
    $('.act-regen', el).addEventListener('click', () => {
      if (confirm('Rigenerare la chiave? Dovrai aggiornarla in OBS.')) {
        run(() => api('POST', '/broadcasts/' + name() + '/regenerate-key'));
      }
    });
    $('.act-delete', el).addEventListener('click', () => {
      if (confirm('Eliminare definitivamente la diretta "' + el._b.title + '"?')) {
        run(() => api('DELETE', '/broadcasts/' + name()));
      }
    });
    $('.toggle-key', el).addEventListener('click', (ev) => {
      const key = $('.b-obs-key', el);
      key.classList.toggle('secret');
      ev.target.textContent = key.classList.contains('secret') ? 'Mostra' : 'Nascondi';
      fill(el, el._b);
    });
    el.querySelectorAll('.copy').forEach((btn) => btn.addEventListener('click', () => {
      const text = btn.dataset.copy === 'obs-key' ? el._b.obs.stream_key : el._b.obs.server;
      navigator.clipboard && navigator.clipboard.writeText(text);
    }));
    $('.b-embed', el).addEventListener('toggle', async (ev) => {
      if (!ev.target.open) return;
      try {
        let info = embedCache.get(name());
        if (!info) {
          info = await api('GET', '/broadcasts/' + name() + '/embed');
          embedCache.set(name(), info);
        }
        $('.b-iframe', el).value = info.iframe;
      } catch (e) { $('.b-iframe', el).value = e.message; }
    });
  }

  async function refresh() {
    try { render(await api('GET', '/broadcasts')); } catch (_) { /* shown elsewhere */ }
  }

  function connectEvents() {
    if (events || !window.EventSource) return;
    events = new EventSource('/api/v1/events');
    events.addEventListener('broadcast', (ev) => {
      try {
        const b = JSON.parse(ev.data);
        const el = $('#broadcasts').querySelector('[data-name="' + b.name + '"]');
        if (el) fill(el, b); else refresh();
      } catch (_) { /* ignore */ }
    });
    events.addEventListener('broadcast_deleted', refresh);
    events.addEventListener('viewers', (ev) => {
      try {
        const d = JSON.parse(ev.data);
        const el = $('#broadcasts').querySelector('[data-name="' + d.name + '"]');
        if (el && el._b) { el._b.viewers = d.counts; fill(el, el._b); }
      } catch (_) { /* ignore */ }
    });
    events.addEventListener('stream', () => loadSystem());
  }

  $('#login-form').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const err = $('#login-error');
    err.hidden = true;
    const res = await fetch('/api/v1/auth/login', {
      method: 'POST', credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ token: $('#token').value.trim() }),
    });
    if (res.status === 204) { $('#token').value = ''; showApp(); }
    else { err.textContent = 'Token non valido.'; err.hidden = false; }
  });

  $('#create-form').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const f = ev.target;
    const err = $('#create-error');
    err.hidden = true;
    const field = (n) => f.elements.namedItem(n).value.trim();
    const body = { title: field('title'), description: field('description') };
    if (field('name')) body.name = field('name');
    try {
      await api('POST', '/broadcasts', body);
      f.reset();
      await refresh();
    } catch (e) { err.textContent = e.message; err.hidden = false; }
  });

  $('#logout').addEventListener('click', async () => {
    await fetch('/api/v1/auth/logout', { method: 'POST', credentials: 'same-origin' });
    showLogin();
  });

  fetch('/api/v1/auth/check', { credentials: 'same-origin' })
    .then((r) => (r.status === 204 ? showApp() : showLogin()))
    .catch(showLogin);
})();
