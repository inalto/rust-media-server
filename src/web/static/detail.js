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
import { t, tn, formatNumber, isBuiltinWaitingText } from './i18n.js';

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

  setChildren(root, h('div', { class: 'loading-page', role: 'status' }, h('span', { class: 'spinner' }), t('common.loading')));

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
      icon('alert', 32), h('h1', null, t('detail.notFound')),
      h('p', { class: 'muted' }, t('detail.notFoundText', { name })),
      h('a', { class: 'btn primary', href: '#/' }, icon('back'), h('span', null, t('detail.back')))));
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
    const rec = h('span', { class: 'rec-chip', hidden: true }, h('span', { class: 'dot', 'aria-hidden': 'true' }), t('common.rec'));
    const slug = h('span', { class: 'mono' });
    const desc = h('span', { class: 'desc' });
    const viewers = h('span', { class: 'viewers-chip', title: t('detail.viewersTitle') }, icon('users', 16), h('strong'));
    const playerLink = h('a', { class: 'btn', target: '_blank', rel: 'noopener' }, icon('external'), h('span', null, t('detail.openPlayer')));
    const el = h('header', { class: 'detail-head' },
      h('div', { class: 'detail-title' },
        h('a', { class: 'back', href: '#/' }, icon('back', 16), h('span', null, t('nav.broadcasts'))),
        h('div', { class: 'title-row' }, title, badge, rec, viewers),
        h('p', { class: 'muted sub' }, slug, desc)),
      h('div', { class: 'head-actions' },
        iconButton('edit', t('action.edit'), () => editDialog()),
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
        const n = totalViewers(b.viewers);
        viewers.setAttribute('aria-label', tn('detail.viewersLabel', n, { n: fmtInt(n) }));
        playerLink.href = b.urls.player;
      },
      viewers(counts) { viewers.querySelector('strong').textContent = fmtInt(totalViewers(counts)); },
    };
  }

  function subnav() {
    const items = [
      ['sec-status', t('common.status')], ['sec-preview', t('detail.nav.preview')], ['sec-obs', t('detail.nav.obs')],
      ['sec-links', t('detail.nav.links')], ['sec-metrics', t('detail.nav.metrics')],
      ['sec-recordings', t('detail.nav.recordings')], ['sec-waiting', t('detail.nav.waiting')],
    ];
    return h('nav', { class: 'subnav', 'aria-label': t('detail.sections') },
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
  // Status
  // -------------------------------------------------------------------------
  function statusSection() {
    const rows = {
      status: kvRow(t('common.status')), since: kvRow(t('state.since')), ip: kvRow(t('state.ip')), video: kvRow(t('state.video')),
      audio: kvRow(t('state.audio')), bitrate: kvRow(t('state.bitrate')), bytes: kvRow(t('common.bytesIn')), rec: kvRow(t('state.recording')),
    };
    const pauseBtn = h('button', { type: 'button', class: 'btn' });
    const discBtn = iconButton('power', t('action.disconnect'), async () => {
      const ok = await confirmDialog({
        title: t('state.disconnectTitle'),
        message: t('state.disconnectText'),
        confirmLabel: t('state.disconnectOk'), danger: true,
      });
      if (ok) act(() => api(`/broadcasts/${enc(name)}/disconnect`, { method: 'POST' }), t('state.disconnected'));
    });
    const recBtn = h('button', { type: 'button', class: 'btn' });
    const delBtn = iconButton('trash', t('action.delete'), async () => {
      const ok = await confirmDialog({
        title: t('state.deleteTitle'),
        message: [t('state.deleteText', { title: b.title })],
        confirmLabel: t('state.deleteOk'), danger: true,
      });
      if (!ok) return;
      try {
        await api(`/broadcasts/${enc(name)}`, { method: 'DELETE' });
        toast(t('state.deleted'), 'success');
        store.remove(name);
        navigate('#/');
      } catch (e) { toast(describeError(e), 'error'); }
    }, { cls: 'btn danger-ghost' });

    pauseBtn.addEventListener('click', () => {
      const paused = b.status === 'paused';
      act(() => api(`/broadcasts/${enc(name)}/${paused ? 'resume' : 'pause'}`, { method: 'POST' }),
        paused ? t('state.resumed') : t('toast.paused'));
    });
    recBtn.addEventListener('click', async () => {
      const recording = !!(b.live && b.live.recording);
      const res = await act(() => api(`/broadcasts/${enc(name)}/record/${recording ? 'stop' : 'start'}`, { method: 'POST' }),
        recording ? t('state.recStopped') : t('state.recStarted'));
      if (res !== undefined) { reload(); recordings.load(); }
    });

    const protoCells = {};
    const protoTable = h('table', { class: 'proto-table' },
      h('caption', { class: 'sr-only' }, t('state.byProtocol')),
      h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('common.protocol')), h('th', { scope: 'col', class: 'num' }, t('common.viewers')))),
      h('tbody', null,
        ...PROTOCOLS.map((p) => { protoCells[p.key] = h('td', { class: 'num' }, '0'); return h('tr', null, h('th', { scope: 'row' }, p.label), protoCells[p.key]); }),
        h('tr', { class: 'total' }, h('th', { scope: 'row' }, t('common.total')), (protoCells.total = h('td', { class: 'num' }, '0')))));

    const sessionsBody = h('tbody');
    const sessionsCount = h('span', { class: 'count' }, '0');
    const sessionsWrap = h('div', { class: 'table-wrap' },
      h('table', null,
        h('thead', null, h('tr', null, ...[t('common.protocol'), t('state.colIp'), t('state.colClient'), t('state.colConnected')].map((label) => h('th', { scope: 'col' }, label)))),
        sessionsBody));

    const el = panel('sec-status', 'activity', t('common.status'),
      h('div', { class: 'actions-row' }, pauseBtn, discBtn, recBtn, delBtn),
      h('dl', { class: 'kv' }, ...Object.values(rows).map((r) => r.row)),
      h('div', { class: 'split' },
        h('div', null, h('h3', null, t('state.byProtocol')), protoTable),
        h('div', { class: 'grow' }, h('h3', null, t('state.sessions'), sessionsCount), sessionsWrap)));

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
          : h('tr', null, h('td', { colspan: '4', class: 'muted' }, t('state.noSessions'))));
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
        rows.status.dd.replaceChildren(statusBadge(b.status), b.status === 'paused' && live ? h('span', { class: 'hint inline' }, t('state.pausedHint')) : '');
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
          rows.bitrate.dd.textContent = bitrate.kbps == null ? t('state.measuring') : t('state.measured', { rate: fmtBitrate(bitrate.kbps) });
          rows.bytes.dd.textContent = fmtBytes(live.bytes_in);
          rows.rec.dd.textContent = live.recording ? t('state.recSince', { time: fmtTime(live.recording.started_at) }) : t('common.no');
        } else {
          bitrate = { started: null, t: 0, bytes: 0, kbps: null };
          for (const k of ['video', 'audio', 'bitrate', 'bytes']) rows[k].dd.textContent = '—';
          rows.rec.dd.textContent = t('common.no');
        }
        setCounts(b.viewers);

        const paused = b.status === 'paused';
        setChildren(pauseBtn, icon(paused ? 'play' : 'pause'), h('span', null, paused ? t('action.resume') : t('action.pause')));
        pauseBtn.title = paused ? t('state.resumeTitle') : t('state.pauseTitle');
        discBtn.disabled = !live;
        const recording = !!(live && live.recording);
        setChildren(recBtn, icon(recording ? 'stop' : 'record'), h('span', null, recording ? t('action.stopRecording') : t('action.recordNow')));
        recBtn.classList.toggle('recording', recording);
        recBtn.disabled = !live;
        recBtn.title = live ? '' : t('state.recDisabledTitle');

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
  // Live preview
  // -------------------------------------------------------------------------
  function previewSection() {
    const key = 'rmsPreview';
    let enabled = true;
    try { enabled = localStorage.getItem(key) !== '0'; } catch (_) { /* ignore */ }
    const src = `/embed/${enc(name)}?muted=1&autoplay=1&latency=1`;
    const frameBox = h('div', { class: 'preview-box' });
    const toggle = h('button', { type: 'button', class: 'btn', 'aria-pressed': String(enabled) });
    const reloadBtn = iconButton('refresh', t('preview.reload'), () => { if (enabled) render(true); }, { showLabel: false });
    function render(force) {
      setChildren(toggle, icon(enabled ? 'eyeOff' : 'eye'), h('span', null, enabled ? t('preview.turnOff') : t('preview.turnOn')));
      toggle.setAttribute('aria-pressed', String(enabled));
      reloadBtn.disabled = !enabled;
      if (!enabled) {
        setChildren(frameBox, h('div', { class: 'preview-off' }, icon('video', 28), h('p', null, t('preview.off'))));
        return;
      }
      if (force || !frameBox.querySelector('iframe')) {
        setChildren(frameBox, h('iframe', { src, title: t('preview.frameTitle'), allow: 'autoplay; fullscreen; picture-in-picture', loading: 'lazy' }));
      }
    }
    toggle.addEventListener('click', () => {
      enabled = !enabled;
      try { localStorage.setItem(key, enabled ? '1' : '0'); } catch (_) { /* ignore */ }
      render(true);
    });
    const el = panel('sec-preview', 'video', t('preview.title'), frameBox,
      h('p', { class: 'hint' }, t('preview.hint')));
    el.tools.append(toggle, reloadBtn);
    render(false);
    return { el, update() {} };
  }

  // -------------------------------------------------------------------------
  // OBS settings
  // -------------------------------------------------------------------------
  function obsSection() {
    const server = copyField({ label: t('obs.server'), value: '', hint: t('obs.serverHint') });
    const regen = iconButton('refresh', t('action.regenerate'), async () => {
      const ok = await confirmDialog({
        title: t('obs.regenConfirmTitle'),
        message: t('obs.regenConfirmText'),
        confirmLabel: t('obs.regenConfirmOk'), danger: true,
      });
      if (ok) act(() => api(`/broadcasts/${enc(name)}/regenerate-key`, { method: 'POST' }), t('obs.regenerated'));
    }, { attrs: { title: t('obs.regenTitle') } });
    const key = copyField({ label: t('obs.key'), value: '', secret: true, extra: [regen], hint: t('obs.keyHint') });
    const gopWarn = h('div', { class: 'banner warn', role: 'alert', hidden: true });

    // Recommended OBS settings (labels follow OBS's own wording in each language).
    const reco = {
      x264: [
        [t('obs.outputMode'), t('obs.outputModeValue')],
        [t('obs.rateControl'), 'CBR'],
        [t('obs.bitrate'), t('obs.bitrateValue')],
        [t('obs.keyframe'), t('obs.keyframeValue')],
        [t('obs.cpuPreset'), t('obs.cpuPresetValue')],
        [t('obs.profile'), t('obs.profileValue')],
        [t('obs.tune'), 'zerolatency'],
        [t('obs.x264Options'), 'bframes=0'],
      ],
      nvenc: [
        [t('obs.outputMode'), t('obs.outputModeValue')],
        [t('obs.rateControl'), 'CBR'],
        [t('obs.keyframe'), t('obs.keyframeValue')],
        [t('obs.preset'), t('obs.presetValue')],
        [t('obs.tuning'), t('obs.tuningValue')],
        [t('obs.multipass'), t('obs.off')],
        [t('obs.maxBframes'), '0'],
        [t('obs.lookahead'), t('obs.off')],
      ],
      common: [
        [t('obs.audio'), t('obs.audioValue')],
        [t('obs.video'), t('obs.videoValue')],
        [t('obs.streamDelay'), t('obs.streamDelayValue')],
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

    const el = panel('sec-obs', 'monitor', t('obs.title'),
      gopWarn,
      h('div', { class: 'grid-2' },
        h('div', { class: 'stack' },
          server, key,
          h('p', { class: 'hint' }, t('obs.serverNote'))),
        h('div', { class: 'reco' },
          h('h3', null, t('obs.recoTitle')),
          h('div', { class: 'segmented', role: 'group', 'aria-label': t('obs.encoder') }, ...encButtons),
          recoList,
          h('p', { class: 'hint' }, t('obs.keyframeNote')))));

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
              h('strong', null, t('obs.gopWarnTitle', { secs: formatNumber(gop, { maximumFractionDigits: 1, useGrouping: false }) })),
              t('obs.gopWarnText')));
          gopWarn.hidden = false;
        } else {
          gopWarn.hidden = true;
        }
      },
    };
  }

  // -------------------------------------------------------------------------
  // Links and embed
  // -------------------------------------------------------------------------
  function linksSection() {
    const fields = {
      flv: copyField({ label: 'HTTP-FLV', value: '' }),
      ws_flv: copyField({ label: 'WebSocket-FLV', value: '' }),
      hls: copyField({ label: 'LL-HLS', value: '' }),
      dash: copyField({ label: 'LL-DASH', value: '' }),
      whep: copyField({ label: 'WebRTC (WHEP)', value: '' }),
      embed: copyField({ label: t('links.embedPage'), value: '' }),
      player: copyField({ label: t('links.playerPage'), value: '' }),
    };
    const disabledTag = {};
    for (const k of ['flv', 'ws_flv', 'hls', 'dash', 'whep']) {
      disabledTag[k] = h('span', { class: 'tag', hidden: true }, t('links.disabled'));
      fields[k].querySelector('label').append(' ', disabledTag[k]);
    }
    let embed = null;
    // lang: '' = the server setting (no ?lang= in the URL), else 'it' | 'en'.
    const opts = { autoplay: true, muted: true, controls: true, src: 'auto', lang: '' };
    const iframeCode = h('textarea', { class: 'code', rows: '4', readonly: true, spellcheck: 'false', id: uid('iframe') });
    const check = (key, label) => {
      const id = uid('opt');
      const input = h('input', { type: 'checkbox', id, checked: opts[key], onchange: (e) => { opts[key] = e.target.checked; renderIframe(); } });
      return h('div', { class: 'check inline' }, input, h('label', { for: id }, label));
    };
    const srcId = uid('src');
    const srcSelect = h('select', { id: srcId, onchange: (e) => { opts.src = e.target.value; renderIframe(); } },
      h('option', { value: 'auto' }, t('links.sourceAuto')),
      h('option', { value: 'webrtc' }, 'WebRTC (WHEP)'),
      h('option', { value: 'flv' }, 'HTTP-FLV'),
      h('option', { value: 'hls' }, 'LL-HLS'),
      h('option', { value: 'dash' }, 'LL-DASH'));
    const langId = uid('plang');
    const langSelect = h('select', { id: langId, onchange: (e) => { opts.lang = e.target.value; renderIframe(); } },
      h('option', { value: '' }, t('links.langDefault')),
      h('option', { value: 'it' }, t('links.langIt')),
      h('option', { value: 'en' }, t('links.langEn')));
    const optsForm = h('fieldset', { class: 'embed-opts' },
      h('legend', null, t('links.optsLegend')),
      check('autoplay', t('links.autoplay')),
      check('muted', t('links.muted')),
      check('controls', t('links.controls')),
      h('div', { class: 'field inline' }, h('label', { for: srcId }, t('links.source')), srcSelect),
      h('div', { class: 'field inline' }, h('label', { for: langId }, t('links.lang')), langSelect),
      h('p', { class: 'hint' }, t('links.autoplayHint')));
    const snippetBox = h('div', { class: 'snippets' });

    function embedUrl() {
      const u = new URL(embed ? embed.urls.embed : (b && b.urls.embed) || location.origin + '/embed/' + name);
      if (!opts.autoplay) u.searchParams.set('autoplay', '0');
      if (!opts.muted) u.searchParams.set('muted', '0');
      if (!opts.controls) u.searchParams.set('controls', '0');
      if (opts.src !== 'auto') u.searchParams.set('src', opts.src);
      if (opts.lang) u.searchParams.set('lang', opts.lang);
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
        ['hlsjs', t('links.snippetHlsjs')],
        ['mpegtsjs', t('links.snippetMpegtsjs')],
        ['dashjs', t('links.snippetDashjs')],
        ['whep', t('links.snippetWhep')],
      ];
      setChildren(snippetBox, ...items.map(([k, label]) => h('details', { class: 'snippet' },
        h('summary', null, icon('code', 16), label),
        h('div', { class: 'code-wrap' },
          h('pre', null, h('code', null, embed.snippets[k] || '')),
          copyButton(() => embed.snippets[k] || '', t('links.copyCode'), { showLabel: true })))));
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
    const el = panel('sec-links', 'link', t('detail.nav.links'),
      h('div', { class: 'grid-2' },
        h('div', { class: 'stack' }, ...Object.values(fields)),
        h('div', { class: 'stack' },
          optsForm,
          h('div', { class: 'field' },
            h('label', { for: iframeCode.id, id: iframeLabelId }, t('links.iframeLabel')),
            iframeCode,
            h('div', { class: 'row-end' }, copyButton(() => iframeCode.value, t('links.copyIframe'), { showLabel: true, cls: 'btn primary' }))),
          h('h3', null, t('links.jsTitle')),
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
  // Recordings
  // -------------------------------------------------------------------------
  const REC_STATUS = {
    recording: [() => t('rec.statusRecording'), 'status-live'],
    processing: [() => t('rec.statusProcessing'), 'status-paused'],
    ready: [() => t('rec.statusReady'), 'status-ok'],
    failed: [() => t('rec.statusFailed'), 'status-error'],
  };

  function recordingsSection() {
    const body = h('div');
    const el = panel('sec-recordings', 'film', t('detail.nav.recordings'), body);
    const refreshBtn = iconButton('refresh', t('rec.refresh'), () => load(), { showLabel: false });
    el.tools.append(refreshBtn);
    let playing = null;
    let lastRecordingId;
    let followTimer = null;

    function stateBox(iconName, title, text) {
      setChildren(body, h('div', { class: 'state-box' }, icon(iconName, 28), h('p', { class: 'state-title' }, title), text ? h('p', { class: 'hint' }, text) : null));
    }

    async function remove(r) {
      const ok = await confirmDialog({
        title: t('rec.deleteTitle'),
        message: t('rec.deleteText', { date: fmtDateTime(r.started_at), size: fmtBytes(r.size_bytes) }),
        confirmLabel: t('action.delete'), danger: true,
      });
      if (!ok) return;
      try {
        await api(`/recordings/${enc(r.id)}`, { method: 'DELETE' });
        toast(t('rec.deleted'), 'success');
        load();
      } catch (e) { toast(describeError(e), 'error'); }
    }

    function render(list) {
      if (!list.length) { stateBox('film', t('rec.none'), t('rec.noneHint')); return; }
      const tbody = h('tbody');
      for (const r of list) {
        const [label, cls] = REC_STATUS[r.status] || [() => r.status, 'status-offline'];
        const url = `/api/v1/recordings/${enc(r.id)}/download`;
        const playable = r.status === 'ready' && r.format === 'mp4';
        const playerRow = h('tr', { class: 'player-row', hidden: true }, h('td', { colspan: '6' }));
        const playBtn = playable ? iconButton('play', t('action.play'), () => {
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
          r.status === 'ready' ? h('a', { class: 'btn icon-only', href: `${url}?download=1`, download: '', 'aria-label': t('action.download'), title: t('action.download') }, icon('download')) : null,
          r.status !== 'recording' ? iconButton('trash', t('rec.delete'), () => remove(r), { showLabel: false, cls: 'btn danger-ghost' }) : null));
        tbody.append(
          h('tr', null,
            h('td', null, h('span', { class: `badge ${cls}`, title: r.error || '' }, label())),
            h('td', null, fmtDateTime(r.started_at)),
            h('td', { class: 'num' }, r.duration_secs != null ? fmtDuration(r.duration_secs) : (r.status === 'recording' ? fmtSince(r.started_at) : '—')),
            h('td', { class: 'num' }, fmtBytes(r.size_bytes)),
            h('td', null, (r.format || '').toUpperCase()),
            actions),
          playerRow);
      }
      setChildren(body,
        h('div', { class: 'table-wrap' }, h('table', null,
          h('thead', null, h('tr', null, ...[t('common.status'), t('rec.colStart'), t('rec.colDuration'), t('rec.colSize'), t('rec.colFormat'), t('rec.colActions')]
            .map((label) => h('th', { scope: 'col' }, label)))),
          tbody)),
        list.some((r) => r.format === 'flv' && r.status === 'ready')
          ? h('p', { class: 'hint' }, t('rec.flvHint')) : null);
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
        if (e instanceof ApiError && e.status === 501) stateBox('info', t('rec.unavailable'), t('rec.unavailableText'));
        else stateBox('alert', t('rec.loadError'), describeError(e));
      }
    }

    stateBox('film', t('common.loading'));
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
  // Waiting video
  // -------------------------------------------------------------------------
  function waitingSection() {
    const current = h('div', { class: 'waiting-current' });
    const upload = uploadWidget({
      endpoint: `/broadcasts/${enc(name)}/waiting-video`,
      getLimits: () => ({ maxMb: settings ? settings.max_upload_mb : null, ffmpeg: settings ? settings.ffmpeg_available : true }),
      onDone: (res) => { if (res && res.name) store.upsert(res); renderCurrent(true); },
      label: t('waiting.uploadLabel'),
    });
    const text = textField({
      label: t('waiting.textLabel'), name: 'waiting_text', textarea: true, rows: 2,
      hint: t('waiting.textHint'),
      attrs: { maxlength: '500', placeholder: t('create.defaultPlaceholder', { text: t('waiting.builtin') }) },
    });
    let dirty = false;
    text.control.addEventListener('input', () => { dirty = true; });
    const textForm = h('form', { class: 'stack', onsubmit: async (e) => {
      e.preventDefault();
      const res = await act(() => api(`/broadcasts/${enc(name)}`, { method: 'PATCH', body: { waiting_text: text.control.value } }), t('waiting.textSaved'));
      if (res) dirty = false;
    } }, text, h('div', null, h('button', { type: 'submit', class: 'btn primary' }, t('common.saveText'))));

    let shownUrl;
    async function renderCurrent(force) {
      if (!b) return;
      const url = b.waiting_video_url || null;
      if (!force && url === shownUrl) return;
      shownUrl = url;
      if (!url) {
        setChildren(current,
          h('div', { class: 'waiting-placeholder' }, icon('film', 28), h('p', null, t('waiting.none'))),
          h('p', { class: 'hint' }, t('waiting.noneHint')));
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
          h('p', { class: 'hint' }, own ? t('waiting.own') : t('waiting.usingDefault')),
          own ? iconButton('trash', t('action.deleteVideo'), async () => {
            const ok = await confirmDialog({ title: t('waiting.deleteTitle'), message: t('waiting.deleteText'), confirmLabel: t('action.delete'), danger: true });
            if (ok) { await act(() => api(`/broadcasts/${enc(name)}/waiting-video`, { method: 'DELETE' }), t('waiting.deleted')); }
          }, { cls: 'btn danger-ghost' }) : null));
    }

    const el = panel('sec-waiting', 'film', t('detail.nav.waiting'),
      h('p', { class: 'hint lead' }, t('waiting.lead')),
      h('div', { class: 'grid-2' }, current, h('div', { class: 'stack' }, upload, textForm)));
    return {
      el,
      refreshLimits: () => upload.refreshLimits(),
      setDefaultText: (value) => { if (value) text.control.placeholder = t('create.defaultPlaceholder', { text: value }); },
      update(b) {
        renderCurrent(false);
        // A stored built-in text (API default) is shown as "use the default" (placeholder).
        const own = isBuiltinWaitingText(b.waiting_text) ? '' : (b.waiting_text || '');
        if (!dirty && document.activeElement !== text.control) text.control.value = own;
      },
    };
  }

  // -------------------------------------------------------------------------
  // Edit dialog
  // -------------------------------------------------------------------------
  function editDialog() {
    if (!b) return;
    const dlg = modal({ title: t('edit.title') });
    const title = textField({ label: t('create.titleLabel'), name: 'title', value: b.title, attrs: { required: true, maxlength: '200' } });
    const desc = textField({ label: t('edit.description'), name: 'description', value: b.description, textarea: true, rows: 3, attrs: { maxlength: '5000' } });
    const err = h('p', { class: 'error', role: 'alert', hidden: true });
    const save = h('button', { type: 'submit', class: 'btn primary' }, t('common.save'));
    const form = h('form', { class: 'stack' }, title, desc,
      h('p', { class: 'hint' }, t('edit.nameNote'), h('span', { class: 'mono' }, b.name)),
      flagFields(b), err,
      h('div', { class: 'modal-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => dlg.close('cancel') }, t('common.cancel')), save));
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      err.hidden = true;
      save.disabled = true;
      try {
        const res = await api(`/broadcasts/${enc(name)}`, { method: 'PATCH', body: { title: title.control.value, description: desc.control.value, ...readFlags(form) } });
        store.upsert(res);
        toast(t('edit.saved'), 'success');
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
    toast(t('detail.deletedElsewhere'), 'info');
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

