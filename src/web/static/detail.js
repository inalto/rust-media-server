// Broadcast detail view (#/b/<name>): OBS settings, live status and actions, live
// preview, playback links and embed code, metrics, recordings, waiting video.
// Sections are built once and patched in place on SSE updates (no full re-render,
// so the preview iframe, the chart and form inputs are never reset).

import {
  api, ApiError, bus, store, describeError, h, icon, iconButton, setChildren, uid, toast,
  modal, confirmDialog, copyButton, copyField, uploadWidget, flagFields, readFlags, textField,
  fmtBytes, fmtBitrate, fmtDateTime, fmtDuration, fmtInt, fmtSince, fmtTime,
  PROTOCOLS, statusBadge, totalViewers,
} from './core.js';
import { metricsSection } from './metrics.js';

const LIVE_POLL_MS = 5000;
const SESSIONS_POLL_MS = 10000;

const enc = encodeURIComponent;

function shortUA(ua) {
  if (!ua) return '—';
  const browser = /Edg\//.test(ua) ? 'Edge' : /OPR\//.test(ua) ? 'Opera' : /Firefox\//.test(ua) ? 'Firefox'
    : /Chrome\//.test(ua) ? 'Chrome' : /Safari\//.test(ua) ? 'Safari' : /Lavf|ffmpeg|ffprobe/i.test(ua) ? 'ffmpeg'
      : /VLC/i.test(ua) ? 'VLC' : /curl/i.test(ua) ? 'curl' : null;
  const os = /Android/.test(ua) ? 'Android' : /iPhone|iPad|iPod/.test(ua) ? 'iOS' : /Mac OS X/.test(ua) ? 'macOS'
    : /Windows/.test(ua) ? 'Windows' : /Linux/.test(ua) ? 'Linux' : null;
  return [browser, os].filter(Boolean).join(' · ') || ua.slice(0, 40);
}

function panel(id, iconName, title, ...children) {
  const titleId = uid('h');
  const tools = h('div', { class: 'panel-tools' });
  const el = h('section', { class: 'panel', id, 'aria-labelledby': titleId },
    h('div', { class: 'panel-head' }, h('h2', { id: titleId }, icon(iconName), title), tools),
    ...children);
  el.tools = tools;
  return el;
}

function kvRow(label) {
  const dd = h('dd', null, '—');
  return { row: [h('dt', null, label), dd], dd };
}

/** Mount the detail view of broadcast `name` into `root`. Returns an unmount function. */
export function mountDetail(root, name, { navigate }) {
  let b = store.broadcasts.get(name) || null;
  let destroyed = false;
  let settings = null;
  const timers = [];
  const offs = [];
  const sections = [];

  setChildren(root, h('div', { class: 'loading-page', role: 'status' }, h('span', { class: 'spinner' }), 'Caricamento…'));

  const every = (fn, ms) => { const t = setInterval(() => { if (!document.hidden) fn(); }, ms); timers.push(t); };

  async function reload() {
    try {
      const fresh = await api(`/broadcasts/${enc(name)}`);
      if (destroyed) return null;
      store.upsert(fresh); // triggers update() through the bus
      return fresh;
    } catch (e) {
      if (destroyed) return null;
      if (e instanceof ApiError && e.status === 404) notFound();
      else if (!(e instanceof ApiError && e.status === 401)) toast(describeError(e), 'error');
      return null;
    }
  }

  function notFound() {
    setChildren(root, h('div', { class: 'empty' },
      icon('alert', 32), h('h1', null, 'Diretta non trovata'),
      h('p', { class: 'muted' }, `Non esiste una diretta chiamata «${name}» (forse è stata eliminata).`),
      h('a', { class: 'btn primary', href: '#/' }, icon('back'), h('span', null, 'Torna alle dirette'))));
  }

  async function act(fn, okMessage) {
    try {
      const res = await fn();
      if (res && res.name === name && res.status) store.upsert(res);
      if (okMessage) toast(okMessage, 'success');
      return res;
    } catch (e) {
      toast(describeError(e), 'error');
      return undefined;
    }
  }

  // -------------------------------------------------------------------------
  // Header
  // -------------------------------------------------------------------------
  function headerSection() {
    const title = h('h1', null);
    const badge = h('span');
    const rec = h('span', { class: 'rec-chip', hidden: true }, h('span', { class: 'dot', 'aria-hidden': 'true' }), 'REC');
    const slug = h('span', { class: 'mono' });
    const desc = h('span', { class: 'desc' });
    const viewers = h('span', { class: 'viewers-chip', title: 'Spettatori connessi' }, icon('users', 16), h('strong'));
    const playerLink = h('a', { class: 'btn', target: '_blank', rel: 'noopener' }, icon('external'), h('span', null, 'Apri player'));
    const el = h('header', { class: 'detail-head' },
      h('div', { class: 'detail-title' },
        h('a', { class: 'back', href: '#/' }, icon('back', 16), h('span', null, 'Dirette')),
        h('div', { class: 'title-row' }, title, badge, rec, viewers),
        h('p', { class: 'muted sub' }, slug, desc)),
      h('div', { class: 'head-actions' },
        iconButton('edit', 'Modifica', () => editDialog()),
        playerLink));
    return {
      el,
      update(b) {
        title.textContent = b.title;
        document.title = `${b.title} · rms`;
        badge.replaceChildren(statusBadge(b.status));
        rec.hidden = !(b.live && b.live.recording);
        slug.textContent = b.name;
        desc.textContent = b.description ? ` · ${b.description}` : '';
        viewers.querySelector('strong').textContent = fmtInt(totalViewers(b.viewers));
        viewers.setAttribute('aria-label', `${fmtInt(totalViewers(b.viewers))} spettatori connessi`);
        playerLink.href = b.urls.player;
      },
      viewers(counts) { viewers.querySelector('strong').textContent = fmtInt(totalViewers(counts)); },
    };
  }

  function subnav() {
    const items = [
      ['sec-status', 'Stato'], ['sec-preview', 'Anteprima'], ['sec-obs', 'OBS'], ['sec-links', 'Link e embed'],
      ['sec-metrics', 'Metriche'], ['sec-recordings', 'Registrazioni'], ['sec-waiting', 'Video di attesa'],
    ];
    return h('nav', { class: 'subnav', 'aria-label': 'Sezioni della diretta' },
      ...items.map(([id, label]) => h('button', {
        type: 'button', class: 'chip',
        onclick: () => {
          const target = document.getElementById(id);
          if (!target) return;
          target.scrollIntoView({ behavior: 'smooth', block: 'start' });
          target.setAttribute('tabindex', '-1');
          target.focus({ preventScroll: true });
        },
      }, label)));
  }

  // -------------------------------------------------------------------------
  // Stato
  // -------------------------------------------------------------------------
  function statusSection() {
    const rows = {
      status: kvRow('Stato'), since: kvRow('In diretta da'), ip: kvRow('IP encoder'), video: kvRow('Video'),
      audio: kvRow('Audio'), bitrate: kvRow('Bitrate in ingresso'), bytes: kvRow('Dati ricevuti'), rec: kvRow('Registrazione'),
    };
    const pauseBtn = h('button', { type: 'button', class: 'btn' });
    const discBtn = iconButton('power', 'Disconnetti encoder', async () => {
      const ok = await confirmDialog({
        title: 'Disconnettere l\'encoder?',
        message: 'OBS verrà scollegato dal server. Se è configurato per riconnettersi, ripartirà da solo dopo qualche secondo: per fermarlo davvero rigenera la chiave o metti in pausa.',
        confirmLabel: 'Disconnetti', danger: true,
      });
      if (ok) act(() => api(`/broadcasts/${enc(name)}/disconnect`, { method: 'POST' }), 'Encoder disconnesso.');
    });
    const recBtn = h('button', { type: 'button', class: 'btn' });
    const delBtn = iconButton('trash', 'Elimina', async () => {
      const ok = await confirmDialog({
        title: 'Eliminare la diretta?',
        message: [`La diretta «${b.title}» verrà eliminata definitivamente insieme al suo video di attesa. L'encoder connesso viene scollegato; le registrazioni restano disponibili.`],
        confirmLabel: 'Elimina diretta', danger: true,
      });
      if (!ok) return;
      try {
        await api(`/broadcasts/${enc(name)}`, { method: 'DELETE' });
        toast('Diretta eliminata.', 'success');
        store.remove(name);
        navigate('#/');
      } catch (e) { toast(describeError(e), 'error'); }
    }, { cls: 'btn danger-ghost' });

    pauseBtn.addEventListener('click', () => {
      const paused = b.status === 'paused';
      act(() => api(`/broadcasts/${enc(name)}/${paused ? 'resume' : 'pause'}`, { method: 'POST' }),
        paused ? 'Diretta ripresa: i player tornano al live.' : 'Diretta in pausa: i player mostrano il video di attesa.');
    });
    recBtn.addEventListener('click', async () => {
      const recording = !!(b.live && b.live.recording);
      const res = await act(() => api(`/broadcasts/${enc(name)}/record/${recording ? 'stop' : 'start'}`, { method: 'POST' }),
        recording ? 'Registrazione fermata.' : 'Registrazione avviata.');
      if (res !== undefined) { reload(); recordings.load(); }
    });

    const protoCells = {};
    const protoTable = h('table', { class: 'proto-table' },
      h('caption', { class: 'sr-only' }, 'Spettatori per protocollo'),
      h('thead', null, h('tr', null, h('th', { scope: 'col' }, 'Protocollo'), h('th', { scope: 'col', class: 'num' }, 'Spettatori'))),
      h('tbody', null,
        ...PROTOCOLS.map((p) => { protoCells[p.key] = h('td', { class: 'num' }, '0'); return h('tr', null, h('th', { scope: 'row' }, p.label), protoCells[p.key]); }),
        h('tr', { class: 'total' }, h('th', { scope: 'row' }, 'Totale'), (protoCells.total = h('td', { class: 'num' }, '0')))));

    const sessionsBody = h('tbody');
    const sessionsCount = h('span', { class: 'count' }, '0');
    const sessionsWrap = h('div', { class: 'table-wrap' },
      h('table', null,
        h('thead', null, h('tr', null, ...['Protocollo', 'IP', 'Client', 'Connesso'].map((t) => h('th', { scope: 'col' }, t)))),
        sessionsBody));

    const el = panel('sec-status', 'activity', 'Stato',
      h('div', { class: 'actions-row' }, pauseBtn, discBtn, recBtn, delBtn),
      h('dl', { class: 'kv' }, ...Object.values(rows).map((r) => r.row)),
      h('div', { class: 'split' },
        h('div', null, h('h3', null, 'Spettatori per protocollo'), protoTable),
        h('div', { class: 'grow' }, h('h3', null, 'Sessioni connesse ', sessionsCount), sessionsWrap)));

    let bitrate = { started: null, t: 0, bytes: 0, kbps: null };

    function setCounts(counts) {
      for (const p of PROTOCOLS) protoCells[p.key].textContent = fmtInt((counts && counts[p.key]) || 0);
      protoCells.total.textContent = fmtInt(totalViewers(counts));
    }

    async function loadSessions() {
      try {
        const list = await api(`/broadcasts/${enc(name)}/sessions`);
        if (destroyed) return;
        sessionsCount.textContent = `(${list.length})`;
        setChildren(sessionsBody, list.length
          ? list.map((s) => h('tr', null,
            h('td', null, (PROTOCOLS.find((p) => p.key === s.protocol) || { label: s.protocol }).label),
            h('td', { class: 'mono' }, s.ip || '—'),
            h('td', { title: s.user_agent || '' }, shortUA(s.user_agent)),
            h('td', { title: fmtDateTime(s.started_at) }, fmtSince(s.started_at))))
          : h('tr', null, h('td', { colspan: '4', class: 'muted' }, 'Nessuno spettatore connesso.')));
      } catch (e) {
        if (!(e instanceof ApiError && e.status === 401)) setChildren(sessionsBody, h('tr', null, h('td', { colspan: '4', class: 'error' }, describeError(e))));
      }
    }

    let sessionsTimer = null;
    return {
      el,
      loadSessions,
      update(b) {
        const live = b.live;
        rows.status.dd.replaceChildren(statusBadge(b.status), b.status === 'paused' && live ? h('span', { class: 'hint inline' }, ' Il segnale arriva ma i player mostrano il video di attesa.') : '');
        rows.since.dd.textContent = live ? `${fmtTime(live.started_at)} (${fmtSince(live.started_at)})` : '—';
        rows.ip.dd.textContent = (live && live.publisher_ip) || '—';
        if (live) {
          const res = live.width && live.height ? `${live.width}×${live.height}` : null;
          const fps = live.fps ? `${Math.round(live.fps * 100) / 100} fps` : null;
          rows.video.dd.textContent = [(live.video_codec || '').toUpperCase() || null, res, fps].filter(Boolean).join(' · ') || '—';
          rows.audio.dd.textContent = (live.audio_codec || '').toUpperCase() || '—';
          const now = Date.now();
          if (bitrate.started !== live.started_at) {
            bitrate = { started: live.started_at, t: now, bytes: live.bytes_in, kbps: null };
          } else if (now - bitrate.t >= 2000 && live.bytes_in >= bitrate.bytes) {
            bitrate.kbps = ((live.bytes_in - bitrate.bytes) * 8) / 1000 / ((now - bitrate.t) / 1000);
            bitrate.t = now;
            bitrate.bytes = live.bytes_in;
          }
          rows.bitrate.dd.textContent = bitrate.kbps == null ? 'misura in corso…' : `${fmtBitrate(bitrate.kbps)} (misurato)`;
          rows.bytes.dd.textContent = fmtBytes(live.bytes_in);
          rows.rec.dd.textContent = live.recording ? `In corso dalle ${fmtTime(live.recording.started_at)}` : 'No';
        } else {
          bitrate = { started: null, t: 0, bytes: 0, kbps: null };
          for (const k of ['video', 'audio', 'bitrate', 'bytes']) rows[k].dd.textContent = '—';
          rows.rec.dd.textContent = 'No';
        }
        setCounts(b.viewers);

        const paused = b.status === 'paused';
        setChildren(pauseBtn, icon(paused ? 'play' : 'pause'), h('span', null, paused ? 'Riprendi' : 'Pausa'));
        pauseBtn.title = paused ? 'Torna a mostrare il live ai player' : 'Mostra il video di attesa ai player senza fermare OBS';
        discBtn.disabled = !live;
        const recording = !!(live && live.recording);
        setChildren(recBtn, icon(recording ? 'stop' : 'record'), h('span', null, recording ? 'Ferma registrazione' : 'Registra ora'));
        recBtn.classList.toggle('recording', recording);
        recBtn.disabled = !live;
        recBtn.title = live ? '' : 'Disponibile quando la diretta riceve il segnale';

        if (live && !sessionsTimer) {
          loadSessions();
          sessionsTimer = setInterval(() => { if (!document.hidden) loadSessions(); }, SESSIONS_POLL_MS);
          timers.push(sessionsTimer);
        } else if (!live && sessionsTimer) {
          clearInterval(sessionsTimer);
          sessionsTimer = null;
          loadSessions();
        }
      },
      viewers(counts) { setCounts(counts); },
    };
  }

  // -------------------------------------------------------------------------
  // Anteprima live
  // -------------------------------------------------------------------------
  function previewSection() {
    const key = 'rmsPreview';
    let enabled = true;
    try { enabled = localStorage.getItem(key) !== '0'; } catch (_) { /* ignore */ }
    const src = `/embed/${enc(name)}?muted=1&autoplay=1&latency=1`;
    const frameBox = h('div', { class: 'preview-box' });
    const toggle = h('button', { type: 'button', class: 'btn', 'aria-pressed': String(enabled) });
    const reloadBtn = iconButton('refresh', 'Ricarica anteprima', () => { if (enabled) render(true); }, { showLabel: false });
    function render(force) {
      setChildren(toggle, icon(enabled ? 'eyeOff' : 'eye'), h('span', null, enabled ? 'Disattiva' : 'Attiva'));
      toggle.setAttribute('aria-pressed', String(enabled));
      reloadBtn.disabled = !enabled;
      if (!enabled) {
        setChildren(frameBox, h('div', { class: 'preview-off' }, icon('video', 28), h('p', null, 'Anteprima disattivata.')));
        return;
      }
      if (force || !frameBox.querySelector('iframe')) {
        setChildren(frameBox, h('iframe', { src, title: 'Anteprima della diretta', allow: 'autoplay; fullscreen; picture-in-picture', loading: 'lazy' }));
      }
    }
    toggle.addEventListener('click', () => {
      enabled = !enabled;
      try { localStorage.setItem(key, enabled ? '1' : '0'); } catch (_) { /* ignore */ }
      render(true);
    });
    const el = panel('sec-preview', 'video', 'Anteprima live', frameBox,
      h('p', { class: 'hint' }, 'È il player embed, come lo vedono gli spettatori (senza audio). L\'anteprima conta come uno spettatore.'));
    el.tools.append(toggle, reloadBtn);
    render(false);
    return { el, update() {} };
  }

  // -------------------------------------------------------------------------
  // Impostazioni OBS
  // -------------------------------------------------------------------------
  function obsSection() {
    const server = copyField({ label: 'Server', value: '', hint: 'In OBS: Impostazioni › Stream › Servizio «Personalizzato».' });
    const regen = iconButton('refresh', 'Rigenera', async () => {
      const ok = await confirmDialog({
        title: 'Rigenerare la chiave stream?',
        message: 'La chiave attuale non sarà più accettata: dovrai incollare quella nuova in OBS. Un encoder già collegato resta connesso finché non si disconnette.',
        confirmLabel: 'Rigenera chiave', danger: true,
      });
      if (ok) act(() => api(`/broadcasts/${enc(name)}/regenerate-key`, { method: 'POST' }), 'Nuova chiave generata: aggiornala in OBS.');
    }, { attrs: { title: 'Genera una nuova chiave stream' } });
    const key = copyField({ label: 'Chiave stream', value: '', secret: true, extra: [regen], hint: 'Segreta: chi la conosce può trasmettere su questa diretta.' });
    const gopWarn = h('div', { class: 'banner warn', role: 'alert', hidden: true });

    const reco = {
      x264: [
        ['Modalità di output', 'Avanzata (Impostazioni › Output)'],
        ['Controllo frequenza', 'CBR'],
        ['Bitrate', '2500–6000 kbps, secondo la banda in upload (lascia margine)'],
        ['Intervallo fotogrammi chiave', '2 s (1 s per la latenza minima)'],
        ['Preset utilizzo CPU', 'veryfast (o più veloce)'],
        ['Profilo', 'main o high'],
        ['Tune', 'zerolatency'],
        ['Opzioni x264', 'bframes=0'],
      ],
      nvenc: [
        ['Modalità di output', 'Avanzata (Impostazioni › Output)'],
        ['Controllo frequenza', 'CBR'],
        ['Intervallo fotogrammi chiave', '2 s (1 s per la latenza minima)'],
        ['Preset', 'P4 (equilibrato) o più veloce'],
        ['Tuning', 'Bassa latenza o Latenza ultra bassa'],
        ['Multipass', 'Disattivato'],
        ['B-frame massimi', '0'],
        ['Look-ahead', 'Disattivato'],
      ],
      common: [
        ['Audio', 'AAC 128–160 kbps, 48 kHz'],
        ['Video', '720p o 1080p, frame rate costante (30 o 60)'],
        ['Ritardo flusso', 'Disattivato (Impostazioni › Avanzate)'],
      ],
    };
    let encoder = 'x264';
    const recoList = h('dl', { class: 'kv compact' });
    const encButtons = ['x264', 'nvenc'].map((k) => h('button', {
      type: 'button', class: 'seg', 'aria-pressed': String(k === encoder),
      onclick: () => { encoder = k; renderReco(); },
    }, k === 'x264' ? 'x264 (CPU)' : 'NVENC (NVIDIA)'));
    function renderReco() {
      encButtons.forEach((btn, i) => btn.setAttribute('aria-pressed', String(['x264', 'nvenc'][i] === encoder)));
      setChildren(recoList, ...[...reco[encoder], ...reco.common].map(([k, v]) => [h('dt', null, k), h('dd', null, v)]));
    }
    renderReco();

    const el = panel('sec-obs', 'monitor', 'Impostazioni OBS',
      gopWarn,
      h('div', { class: 'grid-2' },
        h('div', { class: 'stack' },
          server, key,
          h('p', { class: 'hint' }, 'Il server accetta qualsiasi nome di applicazione: basta che la chiave sia corretta. Sono supportati H.264 + AAC (consigliato) e, solo per HTTP-FLV, HEVC/AV1 con Enhanced RTMP.')),
        h('div', { class: 'reco' },
          h('h3', null, 'Impostazioni consigliate per la bassa latenza'),
          h('div', { class: 'segmented', role: 'group', 'aria-label': 'Encoder' }, ...encButtons),
          recoList,
          h('p', { class: 'hint' }, 'In OBS l\'intervallo dei fotogrammi chiave predefinito è «0» (automatico, fino a 8 s): impostalo esplicitamente, altrimenti LL-HLS e LL-DASH avranno latenza alta.'))));

    return {
      el,
      update(b) {
        server.setValue(b.obs.server);
        key.setValue(b.obs.stream_key);
        const p = b.live && b.live.packager;
        const gop = p ? (p.measured_gop_secs ?? p.gop_secs) : undefined;
        if (typeof gop === 'number' && Number.isFinite(gop) && gop > 4) {
          setChildren(gopWarn, icon('alert'),
            h('div', null,
              h('strong', null, `Intervallo tra fotogrammi chiave troppo lungo: ${String(Math.round(gop * 10) / 10).replace('.', ',')} s. `),
              'In OBS imposta «Intervallo fotogrammi chiave» a 2 s (o 1 s): con GOP lunghi LL-HLS e LL-DASH hanno latenza alta e avvio lento.'));
          gopWarn.hidden = false;
        } else {
          gopWarn.hidden = true;
        }
      },
    };
  }

  // -------------------------------------------------------------------------
  // Link e embed
  // -------------------------------------------------------------------------
  function linksSection() {
    const fields = {
      flv: copyField({ label: 'HTTP-FLV', value: '' }),
      ws_flv: copyField({ label: 'WebSocket-FLV', value: '' }),
      hls: copyField({ label: 'LL-HLS', value: '' }),
      dash: copyField({ label: 'LL-DASH', value: '' }),
      whep: copyField({ label: 'WebRTC (WHEP)', value: '' }),
      embed: copyField({ label: 'Pagina embed', value: '' }),
      player: copyField({ label: 'Pagina player', value: '' }),
    };
    const disabledTag = {};
    for (const k of ['flv', 'ws_flv', 'hls', 'dash', 'whep']) {
      disabledTag[k] = h('span', { class: 'tag', hidden: true }, 'disattivato');
      fields[k].querySelector('label').append(' ', disabledTag[k]);
    }
    let embed = null;
    const opts = { autoplay: true, muted: true, controls: true, src: 'auto' };
    const iframeCode = h('textarea', { class: 'code', rows: '4', readonly: true, spellcheck: 'false', id: uid('iframe') });
    const check = (key, label) => {
      const id = uid('opt');
      const input = h('input', { type: 'checkbox', id, checked: opts[key], onchange: (e) => { opts[key] = e.target.checked; renderIframe(); } });
      return h('div', { class: 'check inline' }, input, h('label', { for: id }, label));
    };
    const srcId = uid('src');
    const srcSelect = h('select', { id: srcId, onchange: (e) => { opts.src = e.target.value; renderIframe(); } },
      h('option', { value: 'auto' }, 'Automatica (consigliata)'),
      h('option', { value: 'webrtc' }, 'WebRTC (WHEP)'),
      h('option', { value: 'flv' }, 'HTTP-FLV'),
      h('option', { value: 'hls' }, 'LL-HLS'),
      h('option', { value: 'dash' }, 'LL-DASH'));
    const optsForm = h('fieldset', { class: 'embed-opts' },
      h('legend', null, 'Opzioni embed'),
      check('autoplay', 'Avvio automatico'),
      check('muted', 'Senza audio all\'avvio'),
      check('controls', 'Controlli del player'),
      h('div', { class: 'field inline' }, h('label', { for: srcId }, 'Sorgente'), srcSelect),
      h('p', { class: 'hint' }, 'I browser bloccano l\'avvio automatico con l\'audio: lascia «Senza audio» attivo, lo spettatore può attivarlo con un tocco.'));
    const snippetBox = h('div', { class: 'snippets' });

    function embedUrl() {
      const u = new URL(embed ? embed.urls.embed : (b && b.urls.embed) || location.origin + '/embed/' + name);
      if (!opts.autoplay) u.searchParams.set('autoplay', '0');
      if (!opts.muted) u.searchParams.set('muted', '0');
      if (!opts.controls) u.searchParams.set('controls', '0');
      if (opts.src !== 'auto') u.searchParams.set('src', opts.src);
      return u.toString();
    }
    function renderIframe() {
      if (!embed) return;
      const src = embedUrl().replace(/&/g, '&amp;').replace(/"/g, '&quot;');
      iframeCode.value = embed.iframe.replace(/src="[^"]*"/, `src="${src}"`);
    }
    function renderSnippets() {
      if (!embed) return;
      const items = [
        ['hlsjs', 'Esempio con hls.js (LL-HLS)'],
        ['mpegtsjs', 'Esempio con mpegts.js (HTTP-FLV)'],
        ['dashjs', 'Esempio con dash.js (LL-DASH)'],
        ['whep', 'Esempio WebRTC (WHEP, senza librerie)'],
      ];
      setChildren(snippetBox, ...items.map(([k, label]) => h('details', { class: 'snippet' },
        h('summary', null, icon('code', 16), label),
        h('div', { class: 'code-wrap' },
          h('pre', null, h('code', null, embed.snippets[k] || '')),
          copyButton(() => embed.snippets[k] || '', 'Copia codice', { showLabel: true })))));
    }
    async function loadEmbed() {
      try {
        embed = await api(`/broadcasts/${enc(name)}/embed`);
        if (destroyed) return;
        renderIframe();
        renderSnippets();
      } catch (e) {
        if (!(e instanceof ApiError && e.status === 401)) iframeCode.value = describeError(e);
      }
    }

    const iframeLabelId = uid('lbl');
    const el = panel('sec-links', 'link', 'Link e embed',
      h('div', { class: 'grid-2' },
        h('div', { class: 'stack' }, ...Object.values(fields)),
        h('div', { class: 'stack' },
          optsForm,
          h('div', { class: 'field' },
            h('label', { for: iframeCode.id, id: iframeLabelId }, 'Codice iframe da incollare nel sito'),
            iframeCode,
            h('div', { class: 'row-end' }, copyButton(() => iframeCode.value, 'Copia iframe', { showLabel: true, cls: 'btn primary' }))),
          h('h3', null, 'Integrazione con un player JavaScript'),
          snippetBox)));
    loadEmbed();

    return {
      el,
      update(b) {
        for (const k of Object.keys(fields)) fields[k].setValue(b.urls[k]);
        const enabled = { flv: b.enable_flv, ws_flv: b.enable_flv, hls: b.enable_hls, dash: b.enable_dash, whep: b.enable_webrtc };
        for (const k of Object.keys(disabledTag)) disabledTag[k].hidden = !!enabled[k];
      },
    };
  }

  // -------------------------------------------------------------------------
  // Registrazioni
  // -------------------------------------------------------------------------
  const REC_STATUS = {
    recording: ['In registrazione', 'status-live'],
    processing: ['In elaborazione', 'status-paused'],
    ready: ['Pronta', 'status-ok'],
    failed: ['Errore', 'status-error'],
  };

  function recordingsSection() {
    const body = h('div');
    const el = panel('sec-recordings', 'film', 'Registrazioni', body);
    const refreshBtn = iconButton('refresh', 'Aggiorna registrazioni', () => load(), { showLabel: false });
    el.tools.append(refreshBtn);
    let playing = null;
    let lastRecordingId;
    let followTimer = null;

    function stateBox(iconName, title, text) {
      setChildren(body, h('div', { class: 'state-box' }, icon(iconName, 28), h('p', { class: 'state-title' }, title), text ? h('p', { class: 'hint' }, text) : null));
    }

    async function remove(r) {
      const ok = await confirmDialog({
        title: 'Eliminare la registrazione?',
        message: `La registrazione del ${fmtDateTime(r.started_at)} (${fmtBytes(r.size_bytes)}) verrà cancellata dal disco.`,
        confirmLabel: 'Elimina', danger: true,
      });
      if (!ok) return;
      try {
        await api(`/recordings/${enc(r.id)}`, { method: 'DELETE' });
        toast('Registrazione eliminata.', 'success');
        load();
      } catch (e) { toast(describeError(e), 'error'); }
    }

    function render(list) {
      if (!list.length) { stateBox('film', 'Nessuna registrazione', 'Avvia «Registra ora» durante la diretta, oppure attiva la registrazione automatica in «Modifica».'); return; }
      const tbody = h('tbody');
      for (const r of list) {
        const [label, cls] = REC_STATUS[r.status] || [r.status, 'status-offline'];
        const url = `/api/v1/recordings/${enc(r.id)}/download`;
        const playable = r.status === 'ready' && r.format === 'mp4';
        const playerRow = h('tr', { class: 'player-row', hidden: true }, h('td', { colspan: '6' }));
        const playBtn = playable ? iconButton('play', 'Riproduci', () => {
          const open = !playerRow.hidden;
          if (playing) { playing.hidden = true; playing.querySelector('td').replaceChildren(); }
          playing = null;
          if (open) return;
          playerRow.querySelector('td').append(h('video', { controls: true, autoplay: true, preload: 'metadata', src: url, class: 'rec-video' }));
          playerRow.hidden = false;
          playing = playerRow;
        }, { showLabel: false }) : null;
        const actions = h('td', null, h('div', { class: 'actions-cell' },
          playBtn,
          r.status === 'ready' ? h('a', { class: 'btn icon-only', href: `${url}?download=1`, download: '', 'aria-label': 'Scarica', title: 'Scarica' }, icon('download')) : null,
          r.status !== 'recording' ? iconButton('trash', 'Elimina registrazione', () => remove(r), { showLabel: false, cls: 'btn danger-ghost' }) : null));
        tbody.append(
          h('tr', null,
            h('td', null, h('span', { class: `badge ${cls}`, title: r.error || '' }, label)),
            h('td', null, fmtDateTime(r.started_at)),
            h('td', { class: 'num' }, r.duration_secs != null ? fmtDuration(r.duration_secs) : (r.status === 'recording' ? fmtSince(r.started_at) : '—')),
            h('td', { class: 'num' }, fmtBytes(r.size_bytes)),
            h('td', null, (r.format || '').toUpperCase()),
            actions),
          playerRow);
      }
      setChildren(body,
        h('div', { class: 'table-wrap' }, h('table', null,
          h('thead', null, h('tr', null, ...['Stato', 'Inizio', 'Durata', 'Dimensione', 'Formato', 'Azioni'].map((t) => h('th', { scope: 'col' }, t)))),
          tbody)),
        list.some((r) => r.format === 'flv' && r.status === 'ready')
          ? h('p', { class: 'hint' }, 'Le registrazioni FLV (senza ffmpeg sul server) non si riproducono nel browser: scaricale e aprile con VLC.') : null);
    }

    async function load() {
      clearTimeout(followTimer);
      followTimer = null;
      try {
        const list = await api(`/broadcasts/${enc(name)}/recordings`);
        if (destroyed) return;
        const rows = Array.isArray(list) ? list : [];
        // Keep the inline player if one is open; otherwise refresh the table.
        if (!playing) render(rows);
        // No event announces the end of the remux: follow in-progress recordings.
        if (rows.some((r) => r.status === 'recording' || r.status === 'processing')) {
          followTimer = setTimeout(() => { if (!destroyed) load(); }, 5000);
        }
      } catch (e) {
        if (destroyed || (e instanceof ApiError && e.status === 401)) return;
        if (e instanceof ApiError && e.status === 501) stateBox('info', 'Registrazioni non ancora disponibili', 'Questa versione del server non gestisce ancora l\'archivio delle registrazioni.');
        else stateBox('alert', 'Impossibile caricare le registrazioni', describeError(e));
      }
    }

    stateBox('film', 'Caricamento…');
    return {
      el,
      load,
      update(b) {
        const id = b.live && b.live.recording ? b.live.recording.id : null;
        if (lastRecordingId !== undefined && id !== lastRecordingId) load();
        lastRecordingId = id;
      },
    };
  }

  // -------------------------------------------------------------------------
  // Video di attesa
  // -------------------------------------------------------------------------
  function waitingSection() {
    const current = h('div', { class: 'waiting-current' });
    const upload = uploadWidget({
      endpoint: `/broadcasts/${enc(name)}/waiting-video`,
      getLimits: () => ({ maxMb: settings ? settings.max_upload_mb : null, ffmpeg: settings ? settings.ffmpeg_available : true }),
      onDone: (res) => { if (res && res.name) store.upsert(res); renderCurrent(true); },
      label: 'Carica un nuovo video di attesa',
    });
    const text = textField({
      label: 'Testo di attesa', name: 'waiting_text', textarea: true, rows: 2,
      hint: 'Mostrato quando non c\'è un video di attesa. Se lo lasci vuoto viene usato il testo predefinito globale.',
      attrs: { maxlength: '500' },
    });
    let dirty = false;
    text.control.addEventListener('input', () => { dirty = true; });
    const textForm = h('form', { class: 'stack', onsubmit: async (e) => {
      e.preventDefault();
      const res = await act(() => api(`/broadcasts/${enc(name)}`, { method: 'PATCH', body: { waiting_text: text.control.value } }), 'Testo di attesa salvato.');
      if (res) dirty = false;
    } }, text, h('div', null, h('button', { type: 'submit', class: 'btn primary' }, 'Salva testo')));

    let shownUrl;
    async function renderCurrent(force) {
      if (!b) return;
      const url = b.waiting_video_url || null;
      if (!force && url === shownUrl) return;
      shownUrl = url;
      if (!url) {
        setChildren(current,
          h('div', { class: 'waiting-placeholder' }, icon('film', 28), h('p', null, 'Nessun video di attesa')),
          h('p', { class: 'hint' }, 'Il player mostra il titolo e il testo di attesa su uno sfondo animato. Puoi caricare un video qui oppure impostarne uno predefinito per tutte le dirette in «Impostazioni».'));
        return;
      }
      let poster = null;
      try {
        const pub = await api(`/public/broadcasts/${enc(name)}`);
        poster = pub && pub.poster_url;
      } catch (_) { /* no poster */ }
      if (destroyed || shownUrl !== url) return;
      const own = !!b.waiting_video;
      setChildren(current,
        h('video', { class: 'waiting-preview', src: url, poster: poster || null, controls: true, muted: true, loop: true, playsinline: true, preload: 'metadata' }),
        h('div', { class: 'row-between' },
          h('p', { class: 'hint' }, own ? 'Video specifico di questa diretta.' : 'Nessun video specifico: è in uso il video predefinito globale (modificabile in «Impostazioni»).'),
          own ? iconButton('trash', 'Elimina video', async () => {
            const ok = await confirmDialog({ title: 'Eliminare il video di attesa?', message: 'Il file verrà cancellato. Se è impostato, i player useranno il video predefinito globale.', confirmLabel: 'Elimina', danger: true });
            if (ok) { await act(() => api(`/broadcasts/${enc(name)}/waiting-video`, { method: 'DELETE' }), 'Video di attesa eliminato.'); }
          }, { cls: 'btn danger-ghost' }) : null));
    }

    const el = panel('sec-waiting', 'film', 'Video di attesa',
      h('p', { class: 'hint lead' }, 'Il player lo mostra in loop, senza audio, quando la diretta è offline o in pausa, e passa da solo al live quando arriva il segnale.'),
      h('div', { class: 'grid-2' }, current, h('div', { class: 'stack' }, upload, textForm)));
    return {
      el,
      refreshLimits: () => upload.refreshLimits(),
      setDefaultText: (t) => { if (t) text.control.placeholder = `Predefinito: ${t}`; },
      update(b) {
        renderCurrent(false);
        if (!dirty && document.activeElement !== text.control) text.control.value = b.waiting_text || '';
      },
    };
  }

  // -------------------------------------------------------------------------
  // Edit dialog
  // -------------------------------------------------------------------------
  function editDialog() {
    if (!b) return;
    const dlg = modal({ title: 'Modifica diretta' });
    const title = textField({ label: 'Titolo', name: 'title', value: b.title, attrs: { required: true, maxlength: '200' } });
    const desc = textField({ label: 'Descrizione', name: 'description', value: b.description, textarea: true, rows: 3, attrs: { maxlength: '5000' } });
    const err = h('p', { class: 'error', role: 'alert', hidden: true });
    const save = h('button', { type: 'submit', class: 'btn primary' }, 'Salva');
    const form = h('form', { class: 'stack' }, title, desc,
      h('p', { class: 'hint' }, `Nome (non modificabile): `, h('span', { class: 'mono' }, b.name)),
      flagFields(b), err,
      h('div', { class: 'modal-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => dlg.close('cancel') }, 'Annulla'), save));
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      err.hidden = true;
      save.disabled = true;
      try {
        const res = await api(`/broadcasts/${enc(name)}`, { method: 'PATCH', body: { title: title.control.value, description: desc.control.value, ...readFlags(form) } });
        store.upsert(res);
        toast('Modifiche salvate.', 'success');
        dlg.close('ok');
      } catch (ex) {
        err.textContent = describeError(ex);
        err.hidden = false;
      } finally { save.disabled = false; }
    });
    dlg.body.append(form);
    dlg.showModal();
    title.control.focus();
  }

  // -------------------------------------------------------------------------
  // Assemble
  // -------------------------------------------------------------------------
  const header = headerSection();
  const status = statusSection();
  const preview = previewSection();
  const obs = obsSection();
  const links = linksSection();
  const metrics = metricsSection(name);
  const recordings = recordingsSection();
  const waiting = waitingSection();
  sections.push(header, status, preview, obs, links, recordings, waiting);

  function build() {
    setChildren(root, h('div', { class: 'detail' },
      header.el,
      subnav(),
      h('div', { class: 'detail-grid' }, status.el, preview.el),
      obs.el, links.el, metrics.el, recordings.el, waiting.el));
  }

  function update(fresh) {
    b = fresh;
    for (const s of sections) s.update(b);
  }

  offs.push(bus.on('broadcast', (nb) => { if (nb.name === name && !destroyed) update(nb); }));
  offs.push(bus.on('viewers', ({ name: n, counts }) => {
    if (n !== name || destroyed) return;
    header.viewers(counts);
    status.viewers(counts);
    metrics.setCurrent(counts);
  }));
  offs.push(bus.on('deleted', (n) => {
    if (n !== name || destroyed) return;
    toast('Questa diretta è stata eliminata.', 'info');
    navigate('#/');
  }));
  offs.push(bus.on('list', () => { const nb = store.broadcasts.get(name); if (nb && nb !== b && !destroyed) update(nb); }));

  (async () => {
    const fresh = await reload();
    if (!fresh || destroyed) return;
    build();
    update(fresh);
    metrics.refresh();
    recordings.load();
    status.loadSessions();
    try {
      settings = await api('/settings');
      if (!destroyed) { waiting.refreshLimits(); waiting.setDefaultText(settings.default_waiting_text); }
    } catch (_) { /* limits stay generic */ }
    // bytes_in / bitrate are not pushed by SSE: poll while live.
    every(() => { if (b && b.live) reload(); }, LIVE_POLL_MS);
  })();

  return () => {
    destroyed = true;
    timers.forEach(clearInterval);
    offs.forEach((off) => off());
    metrics.destroy();
  };
}

