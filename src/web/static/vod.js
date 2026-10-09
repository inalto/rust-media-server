// rms dashboard — video on demand section (#/vod): multi-file upload with per-file
// progress and ladder choice, the library (poster, title, duration, status with live
// progress from the admin SSE `vod` / `vod_deleted` events, renditions, plays, viewers),
// actions (watch, links and embed, edit, re-transcode, download the original, delete)
// and the read-only "VOD ladders" panel of the Settings page (vodSettingsPanel).
// Encodings of users' bucket files (origin `storage`) show owner and bucket chips linking
// to the user (#/users/<id>) and to the file's folder in the admin bucket browser; their
// preview is signed when the video is private.

import {
  api, ApiError, bus, describeError, h, icon, iconButton, setChildren, toast, uid, modal, confirmDialog,
  copyButton, copyField, textField, uploadFile, fmtBytes, fmtDateTime, fmtDuration, fmtInt, fmtBitrate,
} from './core.js';
import { t, tn, formatNumber } from './i18n.js';
import { getThemeChoice, onThemeChange } from './theme.js';

const enc = encodeURIComponent;

/** Owners of bucket encodings: users by id and buckets by id (admin API), loaded lazily. */
const owners = { users: new Map(), buckets: new Map(), usersLoaded: false, bucketsOf: new Set() };
async function loadOwners(list) {
  const ids = [...new Set(list.filter((v) => v.user_id).map((v) => v.user_id))];
  if (!ids.length) return false;
  let changed = false;
  if (!owners.usersLoaded || ids.some((id) => !owners.users.has(id))) {
    for (const u of await api('/users')) owners.users.set(u.id, u);
    owners.usersLoaded = true;
    changed = true;
  }
  await Promise.all(ids.filter((id) => !owners.bucketsOf.has(id) && owners.users.has(id)).map(async (id) => {
    owners.bucketsOf.add(id);
    for (const b of await api(`/users/${enc(id)}/buckets`)) owners.buckets.set(b.id, b);
    changed = true;
  }));
  return changed;
}

/** Owner / bucket / visibility chips of a bucket encoding (null for the admin library). */
function ownerChips(v) {
  if (!v.user_id) return null;
  const u = owners.users.get(v.user_id);
  const b = owners.buckets.get(v.bucket_id);
  const uname = u ? u.username : v.user_id.slice(0, 8);
  const path = v.object_path || '';
  const dir = path.slice(0, path.lastIndexOf('/') + 1) || '/';
  return h('p', { class: 'chips vod-owner' },
    h('a', { class: 'chip-tag', href: `#/users/${enc(u ? u.username : v.user_id)}`, title: t('vod.ownerTitle') }, icon('user', 12), uname),
    h(b ? 'a' : 'span', { class: 'chip-tag mono', href: b ? `#/users/${enc(u ? u.username : v.user_id)}/buckets/${enc(b.name)}${dir.split('/').map(enc).join('/')}` : null, title: t('vod.bucketTitle') },
      icon('bucket', 12), `${b ? b.name : '…'}:${path}`),
    v.visibility === 'private' ? h('span', { class: 'chip-tag private', title: t('browser.privateTitle') }, icon('lock', 12), t('vod.privateChip')) : null);
}

const STATUS = {
  queued: ['status-offline', () => t('vod.status.queued')],
  processing: ['status-paused', () => t('vod.status.processing')],
  ready: ['status-ok', () => t('vod.status.ready')],
  failed: ['status-error', () => t('vod.status.failed')],
};
const FILTERS = [
  ['all', () => t('vod.filter.all'), () => true],
  ['working', () => t('vod.filter.working'), (v) => v.status === 'queued' || v.status === 'processing'],
  ['ready', () => t('vod.filter.ready'), (v) => v.status === 'ready'],
  ['failed', () => t('vod.filter.failed'), (v) => v.status === 'failed'],
];

/** Ladders and limits (GET /vod/ladders), cached for the session. */
let laddersPromise = null;
export function loadLadders(force = false) {
  if (force || !laddersPromise) laddersPromise = api('/vod/ladders').catch((e) => { laddersPromise = null; throw e; });
  return laddersPromise;
}

function statusBadge(status) {
  const [cls, label] = STATUS[status] || ['status-offline', () => status];
  return h('span', { class: `badge ${cls}` }, label());
}

/** "1080p · 720p · 480p" (short side of each rendition). */
function renditionsSummary(v) {
  return (v.renditions || []).map((r) => `${Math.min(r.width, r.height) || r.height}p`).join(' · ');
}

function sourceSummary(v) {
  const parts = [];
  if (v.source_width && v.source_height) parts.push(`${v.source_width}×${v.source_height}`);
  if (v.source_codec) parts.push(v.source_codec.toUpperCase());
  if (v.source_size) parts.push(fmtBytes(v.source_size));
  return parts.join(' · ');
}

/** Video length as a clock: 0:30, 12:05, 1:02:03. */
export function clock(secs) {
  let s = Math.max(0, Math.round(Number(secs) || 0));
  const hh = Math.floor(s / 3600); s -= hh * 3600;
  const mm = Math.floor(s / 60); s -= mm * 60;
  const pad = (v) => String(v).padStart(2, '0');
  return hh ? `${hh}:${pad(mm)}:${pad(s)}` : `${mm}:${pad(s)}`;
}

/** "90", "1:30", "1:02:03" → seconds (null when invalid or empty). */
export function parseStartTime(text) {
  const s = String(text || '').trim();
  if (!s) return null;
  if (!/^\d+(:\d{1,2}){0,2}(\.\d+)?$/.test(s)) return NaN;
  return s.split(':').reduce((acc, p) => acc * 60 + parseFloat(p), 0);
}

function ladderOptions(ladders, selected) {
  return (ladders ? ladders.ladders : []).map((l) => h('option', { value: l.name, selected: l.name === selected },
    l.name === (ladders && ladders.default_ladder) ? t('vod.ladderDefault', { name: l.name }) : l.name));
}

function ladderHintText(ladders, name) {
  const l = ladders && ladders.ladders.find((x) => x.name === name);
  if (!l) return '';
  const r = l.renditions.map((x) => (x.height ? `${x.height}p` : t('vod.sourceSize'))).join(' · ');
  return l.description ? `${l.description} — ${r}` : r;
}

// ---------------------------------------------------------------------------
// Section
// ---------------------------------------------------------------------------

export function mountVod(root) {
  document.title = t('vod.docTitle');
  let destroyed = false;
  let ladders = null;
  const items = new Map();
  const rows = new Map();
  let filter = 'all';
  let openDetail = null; // { id, update(v) } of the open detail dialog

  const summary = h('p', { class: 'muted', 'aria-live': 'polite' });
  const list = h('div', { class: 'vod-list', role: 'list' });
  const empty = h('div', { class: 'state-box', hidden: true });
  const filterBar = h('div', { class: 'segmented', role: 'group', 'aria-label': t('vod.filter.label') });
  const filterButtons = FILTERS.map(([key, label]) => {
    const b = h('button', { type: 'button', class: 'seg', 'aria-pressed': String(key === filter), onclick: () => { filter = key; render(); } }, label());
    filterBar.append(b);
    return [key, b, label];
  });
  const disabledBanner = h('div', { class: 'banner warn', role: 'alert', hidden: true });
  const upload = uploadPanel({
    getLadders: () => ladders,
    onUploaded: (v) => { upsert(v); },
  });

  setChildren(root, h('div', { class: 'page' },
    h('div', { class: 'page-head' },
      h('div', null, h('h1', null, t('vod.title')), summary),
      h('a', { class: 'btn', href: '/vod', target: '_blank', rel: 'noopener' }, icon('external'), h('span', null, t('vod.catalog')))),
    disabledBanner,
    upload.el,
    h('section', { class: 'panel', 'aria-labelledby': 'vod-lib' },
      h('div', { class: 'panel-head' }, h('h2', { id: 'vod-lib' }, icon('film'), t('vod.library')), filterBar),
      list, empty)));

  function upsert(v) {
    if (!v || !v.id) return;
    const isNew = !items.has(v.id);
    items.set(v.id, v);
    render();
    if (isNew && v.user_id && (!owners.users.has(v.user_id) || !owners.buckets.has(v.bucket_id))) {
      owners.bucketsOf.delete(v.user_id);
      loadOwners([v]).then(() => { const el = rows.get(v.id); if (el && items.has(v.id)) el.update(items.get(v.id)); }).catch(() => {});
    }
    if (openDetail && openDetail.id === v.id) openDetail.update(v);
  }

  function remove(id) {
    items.delete(id);
    render();
    if (openDetail && openDetail.id === id) openDetail.deleted();
  }

  function renderSummary() {
    const all = [...items.values()];
    const working = all.filter((v) => v.status === 'queued' || v.status === 'processing').length;
    const ready = all.filter((v) => v.status === 'ready').length;
    summary.textContent = all.length
      ? [tn('vod.count', all.length), t('vod.countReady', { n: fmtInt(ready) }), working ? t('vod.countWorking', { n: fmtInt(working) }) : null].filter(Boolean).join(' · ')
      : t('vod.lead');
    for (const [key, b, label] of filterButtons) {
      const n = all.filter(FILTERS.find((f) => f[0] === key)[2]).length;
      b.textContent = `${label()} (${fmtInt(n)})`;
      b.setAttribute('aria-pressed', String(key === filter));
    }
  }

  function render() {
    if (destroyed) return;
    const pred = FILTERS.find((f) => f[0] === filter)[2];
    const all = [...items.values()].sort((a, b) => (a.created_at < b.created_at ? 1 : a.created_at > b.created_at ? -1 : a.id.localeCompare(b.id)));
    const shown = all.filter(pred);
    const seen = new Set();
    shown.forEach((v, i) => {
      let el = rows.get(v.id);
      if (!el) { el = row(v.id); rows.set(v.id, el); }
      el.update(v);
      if (list.children[i] !== el) list.insertBefore(el, list.children[i] || null);
      seen.add(v.id);
    });
    for (const [id, el] of rows) if (!seen.has(id)) { el.remove(); if (!items.has(id)) rows.delete(id); }
    list.hidden = shown.length === 0;
    empty.hidden = shown.length > 0;
    if (!shown.length) {
      setChildren(empty, icon('film', 28),
        h('p', { class: 'state-title' }, all.length ? t('vod.emptyFilter') : t('vod.empty')),
        all.length ? null : h('p', { class: 'hint' }, t('vod.emptyHint')));
    }
    renderSummary();
  }

  // ------------------------------------------------------------------ row

  function row(id) {
    const el = h('article', { class: 'vod-item', role: 'listitem' });
    const thumb = h('div', { class: 'vod-thumb' });
    const titleBtn = h('button', { type: 'button', class: 'vod-title-btn', onclick: () => detailDialog(id) });
    const desc = h('p', { class: 'vod-desc' });
    const meta = h('p', { class: 'vod-meta' });
    const owner = h('div', { class: 'vod-owner-box' });
    const statusBox = h('div', { class: 'vod-status' });
    const rend = h('p', { class: 'vod-renditions' });
    const plays = h('span', { class: 'vod-stat' });
    const viewers = h('span', { class: 'vod-stat' });
    const actions = h('div', { class: 'vod-actions' });
    el.append(thumb,
      h('div', { class: 'vod-main' }, h('h3', { class: 'vod-title' }, titleBtn), desc, meta, owner, statusBox, rend),
      h('div', { class: 'vod-side' }, h('div', { class: 'vod-stats' }, plays, viewers), actions));
    let lastPoster;
    el.update = (v) => {
      el.dataset.status = v.status;
      // A private bucket encoding's poster needs a signed token: show a placeholder.
      const poster = v.visibility === 'private' ? null : v.poster_url;
      const posterKey = v.visibility === 'private' ? 'private' : v.poster_url;
      if (posterKey !== lastPoster) {
        lastPoster = posterKey;
        setChildren(thumb, poster
          ? h('img', { src: `${poster}?t=${enc(v.updated_at || '')}`, alt: '', loading: 'lazy', decoding: 'async' })
          : h('div', { class: 'vod-thumb-empty', title: v.visibility === 'private' ? t('browser.privateTitle') : null }, icon(v.visibility === 'private' ? 'lock' : 'film', 24)));
        thumb.append(h('span', { class: 'vod-duration', hidden: v.source_duration_secs == null }, clock(v.source_duration_secs)));
      }
      const dur = thumb.querySelector('.vod-duration');
      if (dur) { dur.hidden = v.source_duration_secs == null; dur.textContent = clock(v.source_duration_secs); }
      titleBtn.textContent = v.title;
      titleBtn.setAttribute('aria-label', t('vod.openDetail', { title: v.title }));
      desc.textContent = v.description || '';
      desc.hidden = !v.description;
      meta.textContent = [sourceSummary(v), t('vod.addedAt', { when: fmtDateTime(v.created_at) }), v.origin === 'inbox' ? t('vod.fromInbox') : null].filter(Boolean).join(' · ');
      setChildren(owner, ownerChips(v));
      owner.hidden = !v.user_id;
      const parts = [statusBadge(v.status)];
      if (v.status === 'processing' || v.status === 'queued') {
        const pct = v.status === 'processing' ? v.progress || 0 : 0;
        parts.push(h('progress', { max: '100', value: String(pct), 'aria-label': t('vod.progressLabel', { title: v.title }) }),
          h('span', { class: 'vod-pct' }, v.status === 'processing' ? `${formatNumber(pct)}%` : t('vod.waitingSlot')));
      }
      if (v.status === 'failed' && v.error) parts.push(h('span', { class: 'vod-error', title: v.error }, v.error.split('\n').filter(Boolean).slice(-1)[0] || v.error));
      setChildren(statusBox, ...parts);
      const summaryText = renditionsSummary(v);
      setChildren(rend, h('span', { class: 'tag', title: t('vod.ladderTitle') }, v.ladder), ' ', summaryText || (v.status === 'ready' ? '' : t('vod.noRenditionsYet')));
      plays.replaceChildren(icon('play', 14), h('span', null, tn('vod.plays', v.plays || 0, { n: fmtInt(v.plays || 0) })));
      plays.title = t('vod.playsTitle');
      viewers.replaceChildren(icon('users', 14), h('span', null, fmtInt(v.viewers_now || 0)));
      viewers.title = t('vod.viewersTitle');
      viewers.setAttribute('aria-label', tn('vod.viewersLabel', v.viewers_now || 0, { n: fmtInt(v.viewers_now || 0) }));
      const ready = v.status === 'ready';
      const busy = v.status === 'processing';
      setChildren(actions,
        ready && v.visibility !== 'private' ? h('a', { class: 'btn small', href: v.urls.watch, target: '_blank', rel: 'noopener', title: t('vod.watchTitle') }, icon('external', 16), h('span', null, t('vod.watch'))) : null,
        iconButton('code', t('vod.embed'), () => detailDialog(id), { cls: 'btn small', attrs: { title: t('vod.embedTitle') } }),
        iconButton('edit', t('action.edit'), () => editDialog(id), { cls: 'btn small', showLabel: false }),
        iconButton('refresh', t('vod.retranscode'), () => retranscodeDialog(id), { cls: 'btn small', showLabel: false, attrs: { disabled: busy || !v.source_available } }),
        v.source_available ? h('a', { class: 'btn small icon-only', href: `/api/v1/vod/${enc(id)}/source`, download: '', title: t('vod.downloadSource'), 'aria-label': t('vod.downloadSource') }, icon('download', 16)) : null,
        iconButton('trash', t('action.delete'), () => deleteItem(id), { cls: 'btn small danger-ghost', showLabel: false }));
    };
    return el;
  }

  // ------------------------------------------------------------------ actions

  async function deleteItem(id) {
    const v = items.get(id);
    if (!v) return;
    const busy = v.status === 'processing';
    const ok = await confirmDialog({
      title: t('vod.deleteTitle'),
      message: [t('vod.deleteText', { title: v.title }), busy ? t('vod.deleteBusy') : null].filter(Boolean),
      confirmLabel: t('action.delete'), danger: true,
    });
    if (!ok) return;
    try {
      await api(`/vod/${enc(id)}${busy ? '?force=1' : ''}`, { method: 'DELETE' });
      remove(id);
      toast(t('vod.deleted'), 'success');
    } catch (e) { toast(describeError(e), 'error'); }
  }

  function editDialog(id) {
    const v = items.get(id);
    if (!v) return;
    const dlg = modal({ title: t('vod.editTitle') });
    const title = textField({ label: t('create.titleLabel'), name: 'title', value: v.title, attrs: { required: true, maxlength: '200' } });
    const desc = textField({ label: t('edit.description'), name: 'description', value: v.description, textarea: true, rows: 4, attrs: { maxlength: '5000' } });
    const err = h('p', { class: 'error', role: 'alert', hidden: true });
    const save = h('button', { type: 'submit', class: 'btn primary' }, t('common.save'));
    const form = h('form', { class: 'stack' }, title, desc, err,
      h('div', { class: 'modal-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => dlg.close('cancel') }, t('common.cancel')), save));
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      err.hidden = true;
      if (!title.control.value.trim()) { err.textContent = t('error.titleRequired'); err.hidden = false; return; }
      save.disabled = true;
      try {
        upsert(await api(`/vod/${enc(id)}`, { method: 'PATCH', body: { title: title.control.value, description: desc.control.value } }));
        toast(t('edit.saved'), 'success');
        dlg.close('ok');
      } catch (ex) { err.textContent = describeError(ex); err.hidden = false; } finally { save.disabled = false; }
    });
    dlg.body.append(form);
    dlg.showModal();
    title.control.focus();
  }

  function retranscodeDialog(id) {
    const v = items.get(id);
    if (!v) return;
    const dlg = modal({ title: t('vod.retranscodeTitle') });
    const selId = uid('ladder');
    const hintId = uid('hint');
    const select = h('select', { id: selId, 'aria-describedby': hintId }, ...ladderOptions(ladders, v.ladder));
    const hint = h('p', { id: hintId, class: 'hint' }, ladderHintText(ladders, v.ladder));
    select.addEventListener('change', () => { hint.textContent = ladderHintText(ladders, select.value); });
    const err = h('p', { class: 'error', role: 'alert', hidden: true });
    const go = h('button', { type: 'submit', class: 'btn primary' }, icon('refresh'), h('span', null, t('vod.retranscodeOk')));
    const form = h('form', { class: 'stack' },
      h('p', null, t('vod.retranscodeText', { title: v.title })),
      h('div', { class: 'field' }, h('label', { for: selId }, t('vod.ladder')), select, hint),
      err,
      h('div', { class: 'modal-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => dlg.close('cancel') }, t('common.cancel')), go));
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      go.disabled = true;
      try {
        upsert(await api(`/vod/${enc(id)}/retranscode`, { method: 'POST', body: { ladder: select.value } }));
        toast(t('vod.retranscodeQueued'), 'success');
        dlg.close('ok');
      } catch (ex) { err.textContent = describeError(ex); err.hidden = false; } finally { go.disabled = false; }
    });
    dlg.body.append(form);
    dlg.showModal();
    select.focus();
  }

  // ------------------------------------------------------------------ detail / embed

  function detailDialog(id) {
    const v0 = items.get(id);
    if (!v0) return;
    let v = v0;
    const dlg = modal({ title: v.title, wide: true, onClose: () => { if (openDetail && openDetail.id === id) openDetail = null; offTheme(); } });
    const preview = h('div', { class: 'preview-box' });
    const links = {
      hls: copyField({ label: t('vod.linkHls'), value: v.urls.hls }),
      dash: copyField({ label: t('vod.linkDash'), value: v.urls.dash }),
      watch: copyField({ label: t('vod.linkWatch'), value: v.urls.watch }),
      embed: copyField({ label: t('vod.linkEmbed'), value: v.urls.embed }),
      poster: copyField({ label: t('vod.linkPoster'), value: v.urls.poster }),
    };
    const opts = { autoplay: false, muted: false, controls: true, src: 'auto', lang: '', theme: '', start: '' };
    const code = h('textarea', { class: 'code', rows: '4', readonly: true, spellcheck: 'false', id: uid('iframe') });
    const check = (key, label) => {
      const cid = uid('opt');
      return h('div', { class: 'check inline' },
        h('input', { type: 'checkbox', id: cid, checked: opts[key], onchange: (e) => { opts[key] = e.target.checked; renderCode(); } }),
        h('label', { for: cid }, label));
    };
    const select = (label, key, options) => {
      const sid = uid('sel');
      return h('div', { class: 'field inline' }, h('label', { for: sid }, label),
        h('select', { id: sid, onchange: (e) => { opts[key] = e.target.value; renderCode(); } }, ...options.map(([val, text]) => h('option', { value: val }, text))));
    };
    const startId = uid('start');
    const startErr = h('span', { class: 'hint error', hidden: true }, t('vod.startInvalid'));
    const startInput = h('input', { id: startId, type: 'text', inputmode: 'numeric', placeholder: '0:00', class: 'mono start-input', oninput: (e) => { opts.start = e.target.value; renderCode(); } });
    const optsForm = h('fieldset', { class: 'embed-opts' },
      h('legend', null, t('links.optsLegend')),
      check('autoplay', t('links.autoplay')),
      check('muted', t('links.muted')),
      check('controls', t('links.controls')),
      select(t('links.source'), 'src', [['auto', t('links.sourceAuto')], ['hls', 'HLS'], ['dash', 'DASH']]),
      select(t('links.lang'), 'lang', [['', t('links.langDefault')], ['it', t('links.langIt')], ['en', t('links.langEn')]]),
      select(t('links.theme'), 'theme', [['', t('links.themeDefault')], ['system', t('theme.system')], ['light', t('theme.light')], ['dark', t('theme.dark')]]),
      h('div', { class: 'field inline' }, h('label', { for: startId }, t('vod.startAt')), startInput, startErr),
      h('p', { class: 'hint' }, t('vod.optsHint')));
    let embed = null;
    const snippets = h('div', { class: 'snippets' });

    function embedUrl() {
      const u = new URL(v.urls.embed);
      if (opts.autoplay) u.searchParams.set('autoplay', '1');
      if (opts.autoplay && !opts.muted) u.searchParams.set('muted', '0');
      if (!opts.autoplay && opts.muted) u.searchParams.set('muted', '1');
      if (!opts.controls) u.searchParams.set('controls', '0');
      if (opts.src !== 'auto') u.searchParams.set('src', opts.src);
      if (opts.lang) u.searchParams.set('lang', opts.lang);
      if (opts.theme) u.searchParams.set('theme', opts.theme);
      const start = parseStartTime(opts.start);
      startErr.hidden = !Number.isNaN(start);
      if (start > 0) u.searchParams.set('t', String(Math.round(start * 10) / 10));
      return u.toString();
    }
    function renderCode() {
      if (!embed) return;
      const src = embedUrl().replace(/&/g, '&amp;').replace(/"/g, '&quot;');
      code.value = embed.iframe.replace(/src="[^"]*"/, `src="${src}"`);
    }
    function renderSnippets() {
      if (!embed) return;
      setChildren(snippets, ...[['hlsjs', t('vod.snippetHls')], ['dashjs', t('vod.snippetDash')]].map(([k, label]) => h('details', { class: 'snippet' },
        h('summary', null, icon('code', 16), label),
        h('div', { class: 'code-wrap' }, h('pre', null, h('code', null, embed.snippets[k])),
          copyButton(() => embed.snippets[k], t('links.copyCode'), { showLabel: true })))));
    }
    api(`/vod/${enc(id)}/embed`).then((e) => { embed = e; renderCode(); renderSnippets(); }).catch((e) => { code.value = describeError(e); });

    const infoBox = h('dl', { class: 'kv' });
    const rendBox = h('div');
    let previewFor = null;
    function renderPreview() {
      const key = v.status === 'ready' ? `ready|${v.updated_at}|${v.visibility}` : v.status;
      if (key === previewFor) return;
      previewFor = key;
      if (v.status === 'ready' && v.visibility === 'private' && v.user_id && v.bucket_id) {
        // Private bucket encoding: sign a 1 h link as the owner (admin ?user=).
        setChildren(preview, h('div', { class: 'preview-off' }, icon('lock', 28), h('p', null, t('common.loading'))));
        api(`/account/buckets/${enc(v.bucket_id)}/sign?user=${enc(v.user_id)}`, { method: 'POST', body: { path: v.object_path, expires_secs: 3600 } })
          .then((s) => {
            const u = new URL(s.embed);
            u.searchParams.set('theme', getThemeChoice());
            setChildren(preview, h('iframe', { src: u.toString(), title: t('vod.previewTitle', { title: v.title }), allow: 'autoplay; fullscreen; picture-in-picture' }));
          })
          .catch((e) => setChildren(preview, h('div', { class: 'preview-off' }, icon('alert', 28), h('p', null, describeError(e)))));
      } else if (v.status === 'ready') {
        setChildren(preview, h('iframe', { src: `/embed/vod/${enc(id)}?theme=${getThemeChoice()}`, title: t('vod.previewTitle', { title: v.title }), allow: 'autoplay; fullscreen; picture-in-picture' }));
      } else {
        setChildren(preview, h('div', { class: 'preview-off' }, icon('film', 28), h('p', null, (STATUS[v.status] || STATUS.queued)[1]())));
      }
    }
    const offTheme = onThemeChange(({ choice }) => {
      const frame = preview.querySelector('iframe');
      if (frame && frame.contentWindow) frame.contentWindow.postMessage({ type: 'rms-theme', theme: choice }, location.origin);
    });
    function renderInfo() {
      const kv = [
        [t('common.status'), h('span', null, statusBadge(v.status), v.status === 'processing' ? ` ${formatNumber(v.progress || 0)}%` : '')],
        [t('vod.ladder'), v.ladder],
        [t('vod.source'), sourceSummary(v) || '—'],
        [t('vod.duration'), v.source_duration_secs != null ? fmtDuration(v.source_duration_secs) : '—'],
        [t('vod.originalFile'), v.source_available ? (v.source_name || '—') : t('vod.sourceRemoved')],
        [t('vod.origin'), v.origin === 'inbox' ? t('vod.originInbox') : v.origin === 'storage' ? t('vod.originStorage') : t('vod.originUpload')],
        v.user_id ? [t('vod.owner'), ownerChips(v)] : null,
        [t('vod.added'), fmtDateTime(v.created_at)],
        [t('vod.readyAt'), v.ready_at ? fmtDateTime(v.ready_at) : '—'],
        [t('vod.playsLabel'), fmtInt(v.plays || 0)],
        [t('vod.viewersNow'), fmtInt(v.viewers_now || 0)],
      ];
      if (v.status === 'failed' && v.error) kv.push([t('vod.error'), h('pre', { class: 'vod-error-text' }, v.error)]);
      setChildren(infoBox, ...kv.filter(Boolean).map(([k, val]) => [h('dt', null, k), h('dd', null, val)]));
      const rs = v.renditions || [];
      setChildren(rendBox, rs.length
        ? h('div', { class: 'table-wrap' }, h('table', null,
          h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('vod.colRendition')), h('th', { scope: 'col' }, t('vod.colResolution')),
            ...[t('vod.colVideo'), t('vod.colAudio'), t('vod.colBandwidth')].map((x) => h('th', { scope: 'col', class: 'num' }, x)))),
          h('tbody', null, ...rs.map((r) => h('tr', null,
            h('td', null, r.name),
            h('td', null, `${r.width}×${r.height}`),
            h('td', { class: 'num' }, r.video_kbps ? fmtBitrate(r.video_kbps) : t('vod.crf')),
            h('td', { class: 'num' }, r.audio_kbps ? fmtBitrate(r.audio_kbps) : '—'),
            h('td', { class: 'num' }, r.bandwidth ? fmtBitrate(r.bandwidth / 1000) : '—'))))))
        : h('p', { class: 'hint' }, v.status === 'ready' ? '—' : t('vod.noRenditionsYet')));
      for (const k of Object.keys(links)) links[k].setValue(v.urls[k]);
    }

    dlg.body.append(
      h('div', { class: 'grid-2' },
        h('div', { class: 'stack' }, preview, h('h3', null, t('vod.info')), infoBox),
        h('div', { class: 'stack' }, ...Object.values(links))),
      h('h3', null, t('vod.renditions')), rendBox,
      h('h3', null, t('links.iframeLabel')),
      optsForm,
      h('div', { class: 'field' }, h('label', { for: code.id, class: 'sr-only' }, t('links.iframeLabel')), code,
        h('div', { class: 'row-end' }, copyButton(() => code.value, t('links.copyIframe'), { showLabel: true, cls: 'btn primary' }))),
      h('h3', null, t('links.jsTitle')), snippets);
    renderPreview();
    renderInfo();
    openDetail = {
      id,
      update(nv) { v = nv; dlg.querySelector('.modal-head h2').textContent = v.title; renderPreview(); renderInfo(); },
      deleted() { toast(t('vod.deletedElsewhere'), 'info'); dlg.close('cancel'); },
    };
    dlg.showModal();
  }

  // ------------------------------------------------------------------ data

  async function load() {
    try {
      const [listRes, lad] = await Promise.all([api('/vod'), loadLadders().catch(() => null)]);
      if (destroyed) return;
      ladders = lad;
      upload.setLadders(lad);
      if (lad && (!lad.enabled || !lad.available)) {
        setChildren(disabledBanner, icon('alert'), h('div', null, h('strong', null, t('vod.unavailableTitle')), lad.enabled ? t('vod.noFfmpegText') : t('vod.disabledText')));
        disabledBanner.hidden = false;
      }
      items.clear();
      for (const v of listRes) items.set(v.id, v);
      render();
      loadOwners(listRes).then((changed) => { if (changed && !destroyed) { rows.forEach((el, rid) => { if (items.has(rid)) el.update(items.get(rid)); }); } }).catch(() => {});
    } catch (e) {
      if (destroyed || (e instanceof ApiError && e.status === 401)) return;
      setChildren(empty, icon('alert', 28), h('p', { class: 'state-title' }, t('vod.loadError')), h('p', { class: 'hint' }, describeError(e)));
      empty.hidden = false;
      list.hidden = true;
    }
  }

  const offs = [
    bus.on('vod', upsert),
    bus.on('vod_deleted', remove),
    bus.on('viewers', ({ name, counts }) => {
      if (!name || !name.startsWith('vod:')) return;
      const v = items.get(name.slice(4));
      if (v) upsert({ ...v, viewers_now: (counts && counts.total) || 0 });
    }),
    // SSE reconnected (the broadcast list was refreshed): resync.
    bus.on('list', () => load()),
  ];
  load();
  return () => { destroyed = true; offs.forEach((off) => off()); upload.destroy(); };
}

// ---------------------------------------------------------------------------
// Upload panel: multi-file queue (one upload at a time), per-file progress
// ---------------------------------------------------------------------------

function uploadPanel({ getLadders, onUploaded }) {
  const inputId = uid('vodfiles');
  const ladderId = uid('ladder');
  const ladderHintId = uid('hint');
  const input = h('input', { id: inputId, type: 'file', class: 'file-input', multiple: true, accept: 'video/*,.mp4,.mov,.m4v,.mkv,.webm,.avi,.mts,.m2ts,.ts,.mxf,.flv,.mpg,.mpeg,.wmv' });
  const pick = h('label', { for: inputId, class: 'btn primary' }, icon('folder'), h('span', null, t('vod.upload.choose')));
  const ladderSelect = h('select', { id: ladderId, 'aria-describedby': ladderHintId });
  const ladderHint = h('p', { id: ladderHintId, class: 'hint' });
  const limits = h('p', { class: 'hint' });
  const queueList = h('ul', { class: 'upload-queue', 'aria-label': t('vod.upload.queueLabel') });
  const drop = h('div', { class: 'dropzone vod-drop' },
    h('div', { class: 'vod-drop-text' }, icon('upload', 26), h('p', null, h('strong', null, t('vod.upload.dropTitle')), h('span', { class: 'hint block' }, t('vod.upload.dropText')))),
    h('div', { class: 'dropzone-row' }, input, pick));
  const inboxNote = h('p', { class: 'hint' });
  const el = h('section', { class: 'panel', 'aria-labelledby': 'vod-up' },
    h('div', { class: 'panel-head' }, h('h2', { id: 'vod-up' }, icon('upload'), t('vod.upload.title'))),
    h('div', { class: 'grid-2' },
      h('div', { class: 'stack' }, drop, limits),
      h('div', { class: 'stack' },
        h('div', { class: 'field' }, h('label', { for: ladderId }, t('vod.ladder')), ladderSelect, ladderHint),
        inboxNote)),
    queueList);
  const jobs = [];
  let running = null;
  let destroyed = false;

  ladderSelect.addEventListener('change', () => { ladderHint.textContent = ladderHintText(getLadders(), ladderSelect.value); });

  function setLadders(l) {
    if (!l) return;
    const cur = ladderSelect.value || l.default_ladder;
    setChildren(ladderSelect, ...ladderOptions(l, cur));
    ladderSelect.value = cur;
    ladderHint.textContent = ladderHintText(l, ladderSelect.value);
    limits.textContent = t('vod.upload.limits', { formats: l.extensions.map((x) => x.toUpperCase()).join(', '), mb: fmtInt(l.max_upload_mb) });
    setChildren(inboxNote, icon('inbox', 16), ' ', t('vod.upload.inboxNote'), ' ', h('code', null, l.inbox_dir));
    const disabled = !l.enabled || !l.available;
    input.disabled = disabled;
    pick.classList.toggle('disabled', disabled);
  }

  function validate(file) {
    const l = getLadders();
    const ext = (file.name.split('.').pop() || '').toLowerCase();
    if (l && !l.extensions.includes(ext) && !(file.type || '').startsWith('video/')) return t('vod.upload.unsupported');
    if (l && l.max_upload_mb && file.size > l.max_upload_mb * 1024 * 1024) return t('upload.tooLarge', { size: fmtBytes(file.size), max: fmtInt(l.max_upload_mb) });
    if (!file.size) return t('error.emptyFile');
    return null;
  }

  function addFiles(files) {
    for (const file of files) {
      const job = { file, ladder: ladderSelect.value, state: 'pending', handle: null };
      job.progress = h('progress', { max: '100', value: '0', 'aria-label': t('vod.upload.fileProgress', { name: file.name }) });
      job.status = h('span', { class: 'upload-status hint' }, t('vod.upload.pending'));
      job.button = h('button', { type: 'button', class: 'btn small ghost icon-only', 'aria-label': t('vod.upload.cancel', { name: file.name }), title: t('common.cancel') }, icon('x', 16));
      job.button.addEventListener('click', () => {
        if (job.state === 'uploading' && job.handle) job.handle.abort();
        else if (job.state === 'pending') { job.state = 'cancelled'; job.row.remove(); }
        else job.row.remove();
      });
      job.row = h('li', { class: 'upload-item' },
        h('div', { class: 'upload-head' },
          h('span', { class: 'upload-name' }, file.name),
          h('span', { class: 'hint' }, `${fmtBytes(file.size)} · ${job.ladder}`),
          job.button),
        job.progress, job.status);
      const err = validate(file);
      if (err) {
        job.state = 'error';
        job.status.textContent = err;
        job.status.classList.add('error');
        job.progress.hidden = true;
      } else {
        jobs.push(job);
      }
      queueList.append(job.row);
    }
    next();
  }

  async function next() {
    if (running || destroyed) return;
    const job = jobs.find((j) => j.state === 'pending');
    if (!job) return;
    running = job;
    job.state = 'uploading';
    job.status.textContent = t('upload.progressPct', { pct: 0 });
    job.handle = uploadFile('/vod', job.file, {
      fields: { ladder: job.ladder },
      onProgress(loaded, total) {
        const pct = total ? Math.min(100, Math.round((loaded / total) * 100)) : 0;
        job.progress.value = pct;
        job.status.textContent = t('upload.progress', { pct, loaded: fmtBytes(loaded), total: fmtBytes(total) });
      },
      onSent() {
        job.progress.removeAttribute('value');
        job.status.textContent = t('vod.upload.analyzing');
      },
    });
    try {
      const v = await job.handle.promise;
      job.state = 'done';
      job.progress.value = 100;
      job.progress.setAttribute('value', '100');
      job.status.textContent = t('vod.upload.queued');
      job.status.classList.add('ok');
      job.button.setAttribute('aria-label', t('vod.upload.dismiss', { name: job.file.name }));
      job.button.title = t('common.close');
      toast(t('vod.upload.toastDone', { title: v.title }), 'success');
      onUploaded(v);
    } catch (e) {
      job.state = e && e.name === 'AbortError' ? 'cancelled' : 'error';
      job.progress.hidden = true;
      job.status.textContent = job.state === 'cancelled' ? t('error.aborted') : describeError(e);
      job.status.classList.add('error');
      if (job.state === 'error') toast(`${job.file.name}: ${describeError(e)}`, 'error');
    } finally {
      running = null;
      next();
    }
  }

  input.addEventListener('change', () => { if (input.files && input.files.length) addFiles([...input.files]); input.value = ''; });
  for (const ev of ['dragenter', 'dragover']) drop.addEventListener(ev, (e) => { e.preventDefault(); if (!input.disabled) drop.classList.add('drag'); });
  for (const ev of ['dragleave', 'drop']) drop.addEventListener(ev, () => drop.classList.remove('drag'));
  drop.addEventListener('drop', (e) => {
    e.preventDefault();
    if (input.disabled || !e.dataTransfer || !e.dataTransfer.files.length) return;
    addFiles([...e.dataTransfer.files]);
  });

  return {
    el,
    setLadders,
    // Uploads continue in the background when the view changes (the row elements are
    // detached); only new ones stop being started.
    destroy() { destroyed = true; },
  };
}

// ---------------------------------------------------------------------------
// Settings page: read-only "VOD ladders" panel
// ---------------------------------------------------------------------------

export function vodSettingsPanel() {
  const body = h('div', { class: 'stack' }, h('p', { class: 'muted' }, t('common.loading')));
  const el = h('section', { class: 'panel', 'aria-labelledby': 'set-vod' },
    h('div', { class: 'panel-head' }, h('h2', { id: 'set-vod' }, icon('layers'), t('vod.settings.title'))),
    h('p', { class: 'hint lead' }, t('vod.settings.lead')),
    body);
  loadLadders(true).then((l) => {
    const ladderCard = (ld) => h('div', { class: 'ladder-card' },
      h('h3', null, ld.name, ld.name === l.default_ladder ? h('span', { class: 'tag' }, t('vod.settings.default')) : null),
      ld.description ? h('p', { class: 'hint' }, ld.description) : null,
      h('div', { class: 'table-wrap' }, h('table', null,
        h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('vod.colRendition')),
          ...[t('vod.settings.height'), t('vod.colVideo'), t('vod.colAudio')].map((x) => h('th', { scope: 'col', class: 'num' }, x)))),
        h('tbody', null, ...ld.renditions.map((r) => h('tr', null,
          h('td', null, r.name),
          h('td', { class: 'num' }, r.height ? `${r.height}p` : t('vod.sourceSize')),
          h('td', { class: 'num' }, r.video_kbps ? fmtBitrate(r.video_kbps) : t('vod.crf')),
          h('td', { class: 'num' }, fmtBitrate(r.audio_kbps))))))));
    setChildren(body,
      l.enabled && l.available ? null : h('div', { class: 'banner warn', role: 'alert' }, icon('alert'),
        h('div', null, h('strong', null, t('vod.unavailableTitle')), l.enabled ? t('vod.noFfmpegText') : t('vod.disabledText'))),
      h('div', { class: 'ladders' }, ...l.ladders.map(ladderCard)),
      copyField({ label: t('vod.settings.inbox'), value: l.inbox_dir, hint: t('vod.settings.inboxHint') }),
      h('dl', { class: 'kv' },
        h('dt', null, t('vod.settings.encoder')), h('dd', null, h('code', null, l.encoder)),
        h('dt', null, t('vod.settings.segment')), h('dd', null, t('vod.settings.segmentValue', { secs: formatNumber(l.segment_duration, { maximumFractionDigits: 1 }) })),
        h('dt', null, t('vod.settings.keepSource')), h('dd', null, l.keep_source ? t('vod.settings.yes') : t('common.no'))),
      h('p', { class: 'hint' }, t('vod.settings.configNote')));
  }).catch((e) => {
    if (!(e instanceof ApiError && e.status === 401)) setChildren(body, h('p', { class: 'error' }, describeError(e)));
  });
  return el;
}

/** One line for the System panel (`/system` → vod). */
export function vodSystemLine(vod) {
  if (!vod) return '—';
  if (!vod.enabled) return t('vod.system.disabled');
  if (!vod.available) return t('vod.system.noFfmpeg');
  return t('vod.system.status', { running: fmtInt(vod.jobs_running), queued: fmtInt(vod.queued), encoder: vod.encoder });
}
