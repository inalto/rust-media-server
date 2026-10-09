// rms dashboard — bucket browser (#/buckets/<bucket>/<path…> in the user portal,
// #/users/<id>/buckets/<bucket>/<path…> for the admin): breadcrumb, listing of
// GET /storage/{bucket}/{dir}/ (folders first), multi-select delete, new folder, uploads
// (drag & drop or file picker; one streamed PUT per file with XMLHttpRequest progress,
// optional SHA-256 `Checksum` for files ≤ 256 MB, X-Rms-Encode / X-Rms-Visibility from
// the upload options, overwrite confirmation, a queue that survives navigation), live
// encoding status (bus `vod` / `vod_deleted`: account SSE in the portal, admin SSE for the
// admin) and the object dialog (?info=1: metadata, encoding, links, signed links with an
// expiry picker, play in a dialog, visibility, title/description, encode, delete).

import {
  request, ApiError, bus, describeError, h, icon, iconButton, setChildren, toast, uid, modal, confirmDialog,
  copyButton, copyField, textField, fmtBytes, fmtDateTime, fmtDuration, fmtInt, fmtBitrate, signalUnauthorized,
} from './core.js';
import {
  enc, formErrors, selectField, bucketSettingsDialog, bucketChips, quotaBar,
} from './accounts.js';
import { t, tn, formatNumber } from './i18n.js';
import { getThemeChoice } from './theme.js';

const VIDEO_EXT = ['mp4', 'mov', 'm4v', 'mkv', 'webm', 'avi', 'mts', 'm2ts', 'ts', 'mxf', 'flv', 'mpg', 'mpeg', 'wmv'];
const extOf = (name) => (name.includes('.') ? name.split('.').pop().toLowerCase() : '');
export const isVideoName = (name) => VIDEO_EXT.includes(extOf(name));
/** Files up to this size get a client-side SHA-256 `Checksum` (crypto.subtle). */
const CHECKSUM_MAX = 256 * 1024 * 1024;

/** Percent-encode every segment of a storage path ("/a b/c.mp4" → "/a%20b/c.mp4"). */
export const encPath = (path) => path.split('/').map(enc).join('/');
export const storageUrl = (bucket, path) => `/storage/${enc(bucket)}${encPath(path)}`;
const parentDir = (path) => path.slice(0, path.lastIndexOf('/') + 1) || '/';

function storage(bucket, path, { method = 'GET', query, body, headers } = {}) {
  const url = storageUrl(bucket, path) + (query ? `?${new URLSearchParams(query)}` : '');
  return request(url, { method, body, headers });
}

/** Name of a new file or folder: one path segment the server accepts. */
export function segmentError(name) {
  if (!name) return t('browser.nameRequired');
  if (name.includes('/') || name.includes('\\')) return t('browser.nameSlash');
  if (name === '.' || name === '..' || name.toLowerCase().startsWith('.rms')) return t('browser.nameReserved');
  if (/[\u0000-\u001f\u007f]/.test(name)) return t('browser.nameInvalid');
  if (new TextEncoder().encode(name).length > 255) return t('browser.nameTooLong');
  return null;
}

// ---------------------------------------------------------------------------
// Encoding status
// ---------------------------------------------------------------------------

const ENC_STATUS = {
  queued: ['status-offline', () => t('vod.status.queued')],
  processing: ['status-paused', () => t('vod.status.processing')],
  ready: ['status-ok', () => t('vod.status.ready')],
  failed: ['status-error', () => t('vod.status.failed')],
};

/** The `encoding` of an ObjectView rebuilt from a VodView (SSE `vod`). */
function encodingFromVod(v) {
  return {
    vod_id: v.id, status: v.status, progress: v.progress, ladder: v.ladder, visibility: v.visibility,
    renditions: v.renditions || [], error: v.error, duration_secs: v.source_duration_secs, output_bytes: v.output_bytes,
    hls_url: v.urls.hls, dash_url: v.urls.dash, poster_url: v.poster_url, embed_url: v.urls.embed, watch_url: v.urls.watch,
    created_at: v.created_at, ready_at: v.ready_at,
  };
}

/** Status chip of an encoding: queued / processing N% / ready / failed (error in the tooltip). */
export function encodingChip(e) {
  if (!e) return null;
  const [cls, label] = ENC_STATUS[e.status] || ['status-offline', () => e.status];
  const text = e.status === 'processing' ? t('browser.encProcessing', { pct: formatNumber(e.progress || 0) }) : label();
  const title = e.status === 'failed' && e.error ? e.error.split('\n').filter(Boolean).slice(-1)[0] : null;
  const chip = h('span', { class: `badge enc-chip ${cls}`, title, dataset: { status: e.status } }, text);
  if (e.status === 'processing') {
    const bar = h('span', { class: 'enc-bar', 'aria-hidden': 'true' }, h('span'));
    bar.firstChild.style.width = `${Math.max(2, Math.min(100, e.progress || 0))}%`;
    chip.append(bar);
  }
  return h('span', { class: 'enc-cell' }, chip,
    e.visibility === 'private' ? h('span', { class: 'enc-lock', title: t('browser.privateTitle'), 'aria-label': t('browser.private') }, icon('lock', 14)) : null);
}

function renditionsLine(e) {
  return (e.renditions || []).map((r) => `${Math.min(r.width, r.height) || r.height}p`).join(' · ');
}

const fileIcon = (item) => {
  if (item.is_dir) return 'folder';
  const ct = item.content_type || '';
  if (ct.startsWith('video/') || isVideoName(item.name)) return 'video';
  if (ct.startsWith('image/')) return 'image';
  if (ct.startsWith('text/')) return 'text';
  return 'file';
};

// ---------------------------------------------------------------------------
// Upload queue (module level: survives navigation between folders)
// ---------------------------------------------------------------------------

const uploads = {
  jobs: [],
  running: null,
  list: null,
  ensureList() {
    if (!this.list) this.list = h('ul', { class: 'upload-queue', 'aria-label': t('browser.queueLabel') });
    return this.list;
  },
};

/** PUT a file with progress (XMLHttpRequest streams the File from disk). */
function putFile(url, file, { headers, onProgress, onSent }) {
  const xhr = new XMLHttpRequest();
  const promise = new Promise((resolve, reject) => {
    xhr.open('PUT', url);
    xhr.responseType = 'json';
    for (const [k, v] of Object.entries(headers || {})) xhr.setRequestHeader(k, v);
    xhr.upload.addEventListener('progress', (e) => onProgress && onProgress(e.loaded, e.lengthComputable ? e.total : file.size));
    xhr.upload.addEventListener('load', () => onSent && onSent());
    xhr.addEventListener('load', () => {
      const data = xhr.response;
      if (xhr.status === 401) { signalUnauthorized(); reject(new ApiError(401, 'unauthorized')); } else if (xhr.status >= 200 && xhr.status < 300) resolve(data);
      else reject(new ApiError(xhr.status, (data && data.error) || xhr.statusText || 'upload failed', (data && data.field) || null));
    });
    xhr.addEventListener('error', () => reject(new ApiError(0, 'network error')));
    xhr.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
    xhr.send(file);
  });
  return { promise, abort: () => xhr.abort() };
}

async function sha256Hex(file) {
  const buf = await file.arrayBuffer();
  const digest = await crypto.subtle.digest('SHA-256', buf);
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

function enqueueUpload(job) {
  const { file } = job;
  job.state = 'pending';
  job.progress = h('progress', { max: '100', value: '0', 'aria-label': t('vod.upload.fileProgress', { name: file.name }) });
  job.status = h('span', { class: 'upload-status hint', 'aria-live': 'polite' }, t('vod.upload.pending'));
  job.button = h('button', { type: 'button', class: 'btn small ghost icon-only', 'aria-label': t('vod.upload.cancel', { name: file.name }), title: t('common.cancel') }, icon('x', 16));
  job.button.addEventListener('click', () => {
    if (job.state === 'uploading' && job.handle) job.handle.abort();
    else if (job.state === 'hashing') job.cancelled = true;
    else { if (job.state === 'pending') job.state = 'cancelled'; job.row.remove(); }
  });
  const opts = [job.encodeLabel, job.visibilityLabel].filter(Boolean).join(' · ');
  job.row = h('li', { class: 'upload-item' },
    h('div', { class: 'upload-head' },
      h('span', { class: 'upload-name', title: `${job.bucket}${job.path}` }, file.name),
      h('span', { class: 'hint' }, [fmtBytes(file.size), opts].filter(Boolean).join(' · ')),
      job.button),
    job.progress, job.status);
  uploads.jobs.push(job);
  uploads.ensureList().append(job.row);
  nextUpload();
}

async function nextUpload() {
  if (uploads.running) return;
  const job = uploads.jobs.find((j) => j.state === 'pending');
  if (!job) return;
  uploads.running = job;
  const fail = (e) => {
    job.state = e && e.name === 'AbortError' ? 'cancelled' : 'error';
    job.progress.hidden = true;
    job.status.textContent = job.state === 'cancelled' ? t('error.aborted') : describeError(e);
    job.status.classList.add('error');
    if (job.state === 'error') toast(`${job.file.name}: ${describeError(e)}`, 'error');
  };
  try {
    const headers = { ...job.headers };
    if (job.file.size <= CHECKSUM_MAX && window.crypto && crypto.subtle) {
      job.state = 'hashing';
      job.progress.removeAttribute('value');
      job.status.textContent = t('browser.hashing');
      try { headers.Checksum = await sha256Hex(job.file); } catch (_) { /* optional: upload without it */ }
      if (job.cancelled) throw new DOMException('aborted', 'AbortError');
    }
    job.state = 'uploading';
    job.progress.value = 0;
    job.progress.setAttribute('value', '0');
    job.status.textContent = t('upload.progressPct', { pct: 0 });
    const started = performance.now();
    job.handle = putFile(storageUrl(job.bucket, job.path), job.file, {
      headers,
      onProgress(loaded, total) {
        const pct = total ? Math.min(100, Math.floor((loaded / total) * 100)) : 0;
        job.progress.value = pct;
        const secs = (performance.now() - started) / 1000;
        const rate = secs > 0.5 ? fmtBytes(loaded / secs) : null;
        job.status.textContent = rate
          ? t('browser.uploadRate', { pct, loaded: fmtBytes(loaded), total: fmtBytes(total), rate })
          : t('upload.progress', { pct, loaded: fmtBytes(loaded), total: fmtBytes(total) });
      },
      onSent() {
        job.progress.removeAttribute('value');
        job.status.textContent = t('upload.saving');
      },
    });
    const obj = await job.handle.promise;
    job.state = 'done';
    job.progress.value = 100;
    job.progress.setAttribute('value', '100');
    job.status.textContent = obj && obj.encoding ? t('browser.uploadedEncoding') : t('browser.uploaded');
    job.status.classList.add('ok');
    job.button.setAttribute('aria-label', t('vod.upload.dismiss', { name: job.file.name }));
    job.button.title = t('common.close');
    toast(t('browser.uploadedToast', { name: job.file.name }), 'success');
    bus.emit('storage-uploaded', { bucket: job.bucket, path: job.path, object: obj });
    setTimeout(() => { if (job.state === 'done') job.row.remove(); }, 15000);
  } catch (e) {
    fail(e);
  } finally {
    uploads.running = null;
    uploads.jobs = uploads.jobs.filter((j) => j !== job);
    nextUpload();
  }
}

// ---------------------------------------------------------------------------
// Browser view
// ---------------------------------------------------------------------------

/**
 * Mount the browser. `ctx` = accountCtx (portal or admin ?user=), `bucketName`, `dir`
 * (directory path with leading and trailing '/'), `baseHash` (`#/buckets/<name>` or
 * `#/users/<id>/buckets/<name>`), `back` = {href, label}.
 */
export function mountBrowser(root, { ctx, bucketName, dir, baseHash, back }) {
  document.title = t('browser.docTitle', { bucket: bucketName, path: dir });
  let destroyed = false;
  let bucket = null;
  let account = null;
  const items = new Map(); // path → ObjectView
  const rows = new Map(); // path → <tr>
  const selection = new Set();
  let openInfo = null; // { path, update(object) }
  const dirHash = (p) => `${baseHash}${encPath(p)}`;

  // ---------------------------------------------------------------- header
  const headSub = h('div', { class: 'browser-sub' });
  const crumbs = h('ol', { class: 'crumbs' });
  const segs = dir.split('/').filter(Boolean);
  crumbs.append(h('li', null, segs.length ? h('a', { href: dirHash('/') }, icon('bucket', 14), h('span', null, bucketName)) : h('span', { 'aria-current': 'page' }, icon('bucket', 14), h('span', null, bucketName))));
  segs.forEach((s, i) => {
    const p = `/${segs.slice(0, i + 1).join('/')}/`;
    crumbs.append(h('li', null, i === segs.length - 1 ? h('span', { 'aria-current': 'page' }, s) : h('a', { href: dirHash(p) }, s)));
  });
  const settingsBtn = iconButton('settings', t('browser.bucketSettings'), () => {
    if (bucket) bucketSettingsDialog({ ctx, bucket, limits: account && account.limits, onSaved: (b) => { bucket = b; renderHead(); renderOptions(); }, onDeleted: () => { location.hash = back.href; } });
  }, { cls: 'btn', attrs: { disabled: true } });

  // ---------------------------------------------------------------- toolbar
  const fileId = uid('files');
  const fileInput = h('input', { id: fileId, type: 'file', class: 'file-input', multiple: true });
  const uploadBtn = h('label', { for: fileId, class: 'btn primary' }, icon('upload'), h('span', null, t('browser.upload')));
  const encodeSel = selectField({ label: t('browser.encodeAfter'), name: 'encode', options: [['auto', t('browser.encodeAutoShort')]] });
  const visSel = selectField({ label: t('browser.visibility'), name: 'visibility', options: [['auto', t('browser.visDefaultShort')]] });
  const toolbar = h('div', { class: 'fb-toolbar' },
    h('div', { class: 'fb-tools' },
      fileInput, uploadBtn,
      iconButton('folderPlus', t('browser.newFolder'), () => newFolderDialog(), { cls: 'btn' }),
      iconButton('refresh', t('browser.refresh'), () => load(true), { cls: 'btn', showLabel: false })),
    h('div', { class: 'fb-options' }, encodeSel, visSel));
  const selBar = h('div', { class: 'fb-selbar', hidden: true, role: 'region', 'aria-label': t('browser.selectionLabel') });
  const summary = h('p', { class: 'hint fb-summary', 'aria-live': 'polite' });

  // ---------------------------------------------------------------- table
  const selectAll = h('input', { type: 'checkbox', 'aria-label': t('browser.selectAll') });
  const tbody = h('tbody');
  const table = h('table', { class: 'fb-table' },
    h('caption', { class: 'sr-only' }, t('browser.tableCaption', { path: `${bucketName}${dir}` })),
    h('thead', null, h('tr', null,
      h('th', { scope: 'col', class: 'fb-sel' }, selectAll),
      h('th', { scope: 'col', class: 'fb-name' }, t('browser.colName')),
      h('th', { scope: 'col', class: 'fb-size num' }, t('browser.colSize')),
      h('th', { scope: 'col', class: 'fb-type' }, t('browser.colType')),
      h('th', { scope: 'col', class: 'fb-date' }, t('browser.colModified')),
      h('th', { scope: 'col', class: 'fb-enc' }, t('browser.colEncoding')),
      h('th', { scope: 'col', class: 'fb-actions' }, h('span', { class: 'sr-only' }, t('browser.colActions'))))),
    tbody);
  const tableWrap = h('div', { class: 'fb-table-wrap' }, table);
  const empty = h('div', { class: 'state-box fb-empty', hidden: true });
  const dropOverlay = h('div', { class: 'fb-drop', 'aria-hidden': 'true' }, icon('upload', 32), h('p', null, t('browser.dropHere', { path: `${bucketName}${dir}` })));
  const panel = h('section', { class: 'panel fb-panel', 'aria-label': t('browser.panelLabel') },
    toolbar, h('p', { class: 'hint fb-drophint' }, icon('upload', 14), ' ', t('browser.dropHint')),
    uploads.ensureList(), selBar, tableWrap, empty, summary, dropOverlay);

  setChildren(root, h('div', { class: 'page browser' },
    h('div', { class: 'detail-head' },
      h('div', { class: 'detail-title' },
        h('a', { class: 'back', href: back.href }, icon('back', 16), back.label),
        h('div', { class: 'title-row' }, h('h1', { class: 'mono-title' }, bucketName)),
        headSub),
      h('div', { class: 'head-actions' }, settingsBtn)),
    h('nav', { class: 'crumbs-nav', 'aria-label': t('browser.breadcrumb') }, crumbs),
    panel));

  function renderHead() {
    if (!bucket) return;
    const quota = account && account.usage ? account.usage.quota_bytes : 0;
    setChildren(headSub,
      bucket.description ? h('p', { class: 'muted' }, bucket.description) : null,
      h('div', { class: 'browser-meta' },
        bucketChips(bucket),
        h('span', { class: 'hint' }, [t('browser.bucketUsage', { size: fmtBytes(bucket.bytes_used) }), tn('usage.files', bucket.objects_count, { n: fmtInt(bucket.objects_count) })].join(' · '))),
      account && quota ? h('div', { class: 'browser-quota' }, quotaBar(account.usage.bytes_used, quota, { compact: true })) : null);
    settingsBtn.disabled = false;
  }

  /** Default ladder of a new encoding: bucket → user → server. */
  const autoLadder = () => (bucket && bucket.default_ladder) || (account && account.user && account.user.default_ladder) || (account && account.limits.default_ladder) || '—';

  function renderOptions() {
    if (!bucket) return;
    const ladders = (account && account.limits.ladders) || [];
    const vodOk = !account || account.limits.vod_available;
    const cur = encodeSel.control.value;
    setChildren(encodeSel.control,
      h('option', { value: 'auto' }, bucket.auto_encode ? t('browser.encodeAuto', { ladder: autoLadder() }) : t('browser.encodeAutoOff')),
      ...(vodOk ? ladders.map((l) => h('option', { value: `ladder:${l}` }, t('browser.encodeLadder', { name: l }))) : []),
      h('option', { value: 'none' }, t('browser.encodeNone')));
    encodeSel.control.value = [...encodeSel.control.options].some((o) => o.value === cur) ? cur : 'auto';
    const vcur = visSel.control.value;
    setChildren(visSel.control,
      h('option', { value: 'auto' }, bucket.public_playback ? t('browser.visDefaultPublic') : t('browser.visDefaultPrivate')),
      h('option', { value: 'public' }, t('browser.public')),
      h('option', { value: 'private' }, t('browser.private')));
    visSel.control.value = vcur || 'auto';
  }

  // ---------------------------------------------------------------- rows
  function rowFor(item) {
    const tr = h('tr', { class: item.is_dir ? 'fb-row is-dir' : 'fb-row' });
    tr.update = (it) => {
      const cb = h('input', { type: 'checkbox', checked: selection.has(it.path), 'aria-label': t('browser.selectItem', { name: it.name }) });
      cb.addEventListener('change', () => { if (cb.checked) selection.add(it.path); else selection.delete(it.path); renderSelection(); });
      const label = h('span', { class: 'fb-label' }, it.name);
      const nameEl = it.is_dir
        ? h('a', { class: 'fb-link', href: dirHash(`${it.path}/`) }, icon('folder', 18), label)
        : h('button', { type: 'button', class: 'fb-link', onclick: () => objectDialog(it.path), title: t('browser.infoTitle', { name: it.name }) }, icon(fileIcon(it), 18), label);
      const chip = encodingChip(it.encoding);
      const sub = h('span', { class: 'fb-sub' }, it.is_dir ? t('browser.folder') : [fmtBytes(it.size), fmtDateTime(it.updated_at)].join(' · '), chip ? encodingChip(it.encoding) : null);
      const actions = it.is_dir
        ? [iconButton('trash', t('action.delete'), () => deleteItems([it.path]), { cls: 'btn small ghost danger-ghost', showLabel: false, attrs: { 'aria-label': t('browser.deleteLabel', { name: it.name }), title: t('browser.deleteFolderTitle') } })]
        : [
          h('a', { class: 'btn small ghost icon-only fb-wide', href: `${storageUrl(bucketName, it.path)}?download=1`, download: it.name, title: t('action.download'), 'aria-label': t('browser.downloadLabel', { name: it.name }) }, icon('download', 16)),
          iconButton('info', t('browser.info'), () => objectDialog(it.path), { cls: 'btn small ghost', showLabel: false, attrs: { 'aria-label': t('browser.infoLabel', { name: it.name }) } }),
          isVideoName(it.name) || it.encoding
            ? iconButton('film', it.encoding ? t('browser.reencode') : t('browser.encode'), () => encodeDialog(it.path), { cls: 'btn small ghost fb-wide', showLabel: false, attrs: { 'aria-label': it.encoding ? t('browser.reencodeLabel', { name: it.name }) : t('browser.encodeLabel', { name: it.name }), disabled: !(account ? account.limits.vod_available : true) } })
            : null,
          iconButton('trash', t('action.delete'), () => deleteItems([it.path]), { cls: 'btn small ghost danger-ghost', showLabel: false, attrs: { 'aria-label': t('browser.deleteLabel', { name: it.name }) } }),
        ];
      setChildren(tr,
        h('td', { class: 'fb-sel' }, cb),
        h('td', { class: 'fb-name' }, nameEl, sub),
        h('td', { class: 'fb-size num' }, it.is_dir ? '—' : fmtBytes(it.size)),
        h('td', { class: 'fb-type' }, it.is_dir ? t('browser.folder') : (it.content_type || '—')),
        h('td', { class: 'fb-date' }, fmtDateTime(it.updated_at)),
        h('td', { class: 'fb-enc' }, chip || h('span', { class: 'muted' }, it.is_dir ? '' : (isVideoName(it.name) ? t('browser.notEncoded') : '—'))),
        h('td', { class: 'fb-actions' }, h('div', { class: 'actions-cell' }, ...actions)));
      tr.dataset.path = it.path;
      if (it.encoding) tr.dataset.enc = it.encoding.status; else delete tr.dataset.enc;
    };
    return tr;
  }

  const sorted = () => [...items.values()].sort((a, b) => (b.is_dir - a.is_dir) || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));

  function render() {
    if (destroyed) return;
    const all = sorted();
    const seen = new Set();
    all.forEach((it, i) => {
      let tr = rows.get(it.path);
      if (!tr) { tr = rowFor(it); rows.set(it.path, tr); }
      tr.update(it);
      if (tbody.children[i] !== tr) tbody.insertBefore(tr, tbody.children[i] || null);
      seen.add(it.path);
    });
    for (const [p, tr] of rows) if (!seen.has(p)) { tr.remove(); rows.delete(p); }
    for (const p of [...selection]) if (!items.has(p)) selection.delete(p);
    tableWrap.hidden = all.length === 0;
    empty.hidden = all.length > 0;
    if (!all.length) {
      setChildren(empty, icon(dir === '/' ? 'bucket' : 'folder', 28),
        h('p', { class: 'state-title' }, dir === '/' ? t('browser.emptyBucket') : t('browser.emptyFolder')),
        h('p', { class: 'hint' }, t('browser.emptyHint')));
    }
    const files = all.filter((x) => !x.is_dir);
    const folders = all.length - files.length;
    summary.textContent = all.length
      ? [folders ? tn('browser.countFolders', folders, { n: fmtInt(folders) }) : null, tn('usage.files', files.length, { n: fmtInt(files.length) }), fmtBytes(files.reduce((a, x) => a + x.size, 0))].filter(Boolean).join(' · ')
      : '';
    renderSelection();
  }

  function updateItem(it) {
    items.set(it.path, it);
    const tr = rows.get(it.path);
    if (tr) tr.update(it); else render();
    if (openInfo && openInfo.path === it.path) openInfo.update(it);
  }

  function renderSelection() {
    const n = selection.size;
    const total = items.size;
    selectAll.checked = n > 0 && n === total;
    selectAll.indeterminate = n > 0 && n < total;
    selBar.hidden = n === 0;
    if (!n) { setChildren(selBar); return; }
    setChildren(selBar,
      h('span', { class: 'fb-selcount' }, tn('browser.selected', n, { n: fmtInt(n) })),
      iconButton('trash', t('browser.deleteSelected'), () => deleteItems([...selection]), { cls: 'btn small danger-ghost' }),
      h('button', { type: 'button', class: 'btn small ghost', onclick: () => { selection.clear(); render(); } }, t('browser.clearSelection')));
  }
  selectAll.addEventListener('change', () => {
    if (selectAll.checked) for (const p of items.keys()) selection.add(p); else selection.clear();
    render();
  });

  // ---------------------------------------------------------------- data
  let loading = null;
  async function load(manual = false) {
    if (loading) return loading;
    loading = (async () => {
      try {
        const [b, acct, list] = await Promise.all([
          ctx.api(`/buckets/${enc(bucketName)}`),
          account && !manual ? Promise.resolve(account) : ctx.api('').catch(() => account),
          storage(bucketName, dir),
        ]);
        if (destroyed) return;
        bucket = b;
        account = acct;
        items.clear();
        for (const it of list) items.set(it.path, it);
        renderHead();
        renderOptions();
        render();
        if (manual) toast(t('browser.refreshed'), 'info', 1800);
      } catch (e) {
        if (destroyed || (e instanceof ApiError && e.status === 401)) return;
        tableWrap.hidden = true;
        empty.hidden = false;
        setChildren(empty, icon('alert', 28), h('p', { class: 'state-title' }, t('browser.loadError')), h('p', { class: 'hint' }, describeError(e)),
          h('a', { class: 'btn', href: back.href }, t('browser.backToList')));
      } finally { loading = null; }
    })();
    return loading;
  }

  let refreshTimer = null;
  /** Refresh the bucket counters (usage) shortly after a change. */
  function refreshBucketSoon() {
    clearTimeout(refreshTimer);
    refreshTimer = setTimeout(async () => {
      try {
        const [b, acct] = await Promise.all([ctx.api(`/buckets/${enc(bucketName)}`), ctx.api('').catch(() => account)]);
        if (destroyed) return;
        bucket = b; account = acct; renderHead();
      } catch (_) { /* next change */ }
    }, 800);
  }

  // ---------------------------------------------------------------- uploads
  function uploadHeaders(file) {
    const headers = {};
    const encode = encodeSel.control.value;
    if (encode === 'none') headers['X-Rms-Encode'] = 'none';
    else if (encode.startsWith('ladder:') && isVideoName(file.name)) headers['X-Rms-Encode'] = encode.slice(7);
    const vis = visSel.control.value;
    if (vis === 'public' || vis === 'private') headers['X-Rms-Visibility'] = vis;
    return headers;
  }

  async function addFiles(fileList) {
    let files = [...fileList].filter((f) => f && f.name);
    if (!files.length || !bucket) return;
    const max = account ? account.limits.max_object_mb * 1024 * 1024 : 0;
    const bad = [];
    files = files.filter((f) => {
      const err = segmentError(f.name) || (max && f.size > max ? t('upload.tooLarge', { size: fmtBytes(f.size), max: fmtInt(account.limits.max_object_mb) }) : null)
        || (items.get(dir + f.name) && items.get(dir + f.name).is_dir ? t('error.isDirectory') : null);
      if (err) bad.push(`${f.name}: ${err}`);
      return !err;
    });
    for (const msg of bad) toast(msg, 'error', 8000);
    const clash = files.filter((f) => items.has(dir + f.name));
    if (clash.length) {
      const ok = await confirmDialog({
        title: t('browser.overwriteTitle'),
        message: [tn('browser.overwriteText', clash.length, { n: fmtInt(clash.length) }), h('ul', { class: 'name-list' }, ...clash.slice(0, 10).map((f) => h('li', null, f.name))), clash.length > 10 ? t('browser.andMore', { n: fmtInt(clash.length - 10) }) : null].filter(Boolean),
        confirmLabel: t('browser.overwrite'),
      });
      if (!ok) files = files.filter((f) => !clash.includes(f));
    }
    const encodeOpt = encodeSel.control.selectedOptions[0];
    const visOpt = visSel.control.selectedOptions[0];
    for (const file of files) {
      const headers = uploadHeaders(file);
      enqueueUpload({
        file, bucket: bucketName, path: dir + file.name, headers,
        encodeLabel: headers['X-Rms-Encode'] === 'none' ? t('browser.encodeNone') : headers['X-Rms-Encode'] ? t('browser.encodeLadder', { name: headers['X-Rms-Encode'] }) : (isVideoName(file.name) && encodeOpt && encodeOpt.value === 'auto' && bucket.auto_encode ? t('browser.encodeAutoShort') : null),
        visibilityLabel: headers['X-Rms-Visibility'] ? (visOpt ? visOpt.textContent : null) : null,
      });
    }
  }

  fileInput.addEventListener('change', () => { if (fileInput.files && fileInput.files.length) addFiles(fileInput.files); fileInput.value = ''; });
  let dragDepth = 0;
  const hasFiles = (e) => e.dataTransfer && [...(e.dataTransfer.types || [])].includes('Files');
  panel.addEventListener('dragenter', (e) => { if (!hasFiles(e)) return; e.preventDefault(); dragDepth++; panel.classList.add('drag'); });
  panel.addEventListener('dragover', (e) => { if (!hasFiles(e)) return; e.preventDefault(); e.dataTransfer.dropEffect = 'copy'; });
  panel.addEventListener('dragleave', () => { dragDepth = Math.max(0, dragDepth - 1); if (!dragDepth) panel.classList.remove('drag'); });
  panel.addEventListener('drop', (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    dragDepth = 0;
    panel.classList.remove('drag');
    addFiles(e.dataTransfer.files);
  });

  // ---------------------------------------------------------------- folder, delete, encode
  function newFolderDialog() {
    const dlg = modal({ title: t('browser.newFolderTitle') });
    const name = textField({ label: t('browser.folderName'), name: 'name', hint: t('browser.folderHint', { path: `${bucketName}${dir}` }), attrs: { required: true, maxlength: '255', spellcheck: 'false', autocapitalize: 'off' } });
    const errors = formErrors({ path: name });
    const ok = h('button', { type: 'submit', class: 'btn primary' }, t('browser.createFolder'));
    const form = h('form', { class: 'stack', novalidate: true }, name, errors.general,
      h('div', { class: 'modal-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => dlg.close('cancel') }, t('common.cancel')), ok));
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      const n = name.control.value.trim();
      const err = segmentError(n) || (items.has(dir + n) ? t('browser.nameExists') : null);
      if (err) { errors.show(err, 'path'); return; }
      ok.disabled = true;
      try {
        const obj = await storage(bucketName, `${dir}${n}/`, { method: 'PUT' });
        dlg.close('ok');
        toast(t('browser.folderCreated', { name: n }), 'success');
        if (obj && obj.path) updateItem(obj); else load();
      } catch (ex) { errors.show(ex, ex.field === 'path' ? 'path' : null); } finally { ok.disabled = false; }
    });
    dlg.body.append(form);
    dlg.showModal();
    name.control.focus();
  }

  async function deleteItems(paths) {
    const list = paths.map((p) => items.get(p)).filter(Boolean);
    if (!list.length) return;
    const dirs = list.filter((x) => x.is_dir);
    const encoded = list.filter((x) => x.encoding);
    const one = list.length === 1 ? list[0] : null;
    const message = [
      one ? (one.is_dir ? t('browser.deleteFolderText', { name: one.name }) : t('browser.deleteFileText', { name: one.name })) : tn('browser.deleteManyText', list.length, { n: fmtInt(list.length) }),
      one ? null : h('ul', { class: 'name-list' }, ...list.slice(0, 10).map((x) => h('li', null, x.is_dir ? `${x.name}/` : x.name))),
      encoded.length ? t('browser.deleteEncodings') : null,
      dirs.length ? t('browser.deleteFolderAdmin') : null,
    ].filter(Boolean);
    const ok = await confirmDialog({ title: one ? (one.is_dir ? t('browser.deleteFolderTitle') : t('browser.deleteFileTitle')) : t('browser.deleteManyTitle'), message, confirmLabel: t('action.delete'), danger: true });
    if (!ok) return;
    let done = 0;
    for (const it of list) {
      try {
        await storage(bucketName, it.is_dir ? `${it.path}/` : it.path, { method: 'DELETE' });
        items.delete(it.path);
        selection.delete(it.path);
        done++;
        if (openInfo && openInfo.path === it.path) openInfo.close();
      } catch (e) {
        toast(`${it.name}: ${e instanceof ApiError && e.status === 403 && it.is_dir ? t('browser.folderNeedsAdmin') : describeError(e)}`, 'error', 8000);
      }
    }
    render();
    if (done) {
      toast(tn('browser.deleted', done, { n: fmtInt(done) }), 'success');
      refreshBucketSoon();
    }
  }

  function encodeDialog(path) {
    const it = items.get(path);
    if (!it) return;
    const e = it.encoding;
    const dlg = modal({ title: e ? t('browser.reencodeTitle') : t('browser.encodeTitle') });
    const ladders = (account && account.limits.ladders) || [];
    const ladder = selectField({ label: t('vod.ladder'), name: 'ladder', options: ladders.map((l) => [l, l === autoLadder() ? t('vod.ladderDefault', { name: l }) : l]), value: e ? e.ladder : autoLadder() });
    const vis = selectField({
      label: t('browser.visibility'), name: 'visibility',
      options: [['public', t('browser.public')], ['private', t('browser.private')]],
      value: e ? e.visibility : (bucket && bucket.public_playback ? 'public' : 'private'),
      hint: t('browser.visibilityHint'),
    });
    const errors = formErrors({ ladder, visibility: vis });
    const go = h('button', { type: 'submit', class: 'btn primary' }, icon('film'), h('span', null, e ? t('browser.reencode') : t('browser.encode')));
    const form = h('form', { class: 'stack' },
      h('p', null, e ? t('browser.reencodeText', { name: it.name }) : t('browser.encodeText', { name: it.name })),
      ladder, vis, errors.general,
      h('div', { class: 'modal-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => dlg.close('cancel') }, t('common.cancel')), go));
    form.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      go.disabled = true;
      try {
        const obj = await storage(bucketName, path, { method: 'POST', query: { action: 'encode' }, body: { ladder: ladder.control.value, visibility: vis.control.value } });
        updateItem(obj);
        toast(t('vod.retranscodeQueued'), 'success');
        dlg.close('ok');
      } catch (ex) { errors.show(ex); } finally { go.disabled = false; }
    });
    dlg.body.append(form);
    dlg.showModal();
    ladder.control.focus();
  }

  // ---------------------------------------------------------------- object dialog
  function objectDialog(path) {
    const it0 = items.get(path);
    if (!it0) return;
    let info = null;
    let signed = null;
    let vodMeta = null; // VodView (title, description) of the encoding, loaded lazily
    const dlg = modal({ title: it0.name, wide: true, onClose: () => { if (openInfo && openInfo.path === path) openInfo = null; } });
    dlg.classList.add('object-dialog');
    const body = h('div', { class: 'stack' }, h('p', { class: 'muted' }, t('common.loading')));
    dlg.body.append(body);

    const kv = (pairs) => h('dl', { class: 'kv compact' }, ...pairs.filter(Boolean).map(([k, v]) => [h('dt', null, k), h('dd', null, v)]));
    const iframeCode = (src) => `<iframe src="${src.replace(/&/g, '&amp;').replace(/"/g, '&quot;')}" width="640" height="360" style="border:0;max-width:100%;aspect-ratio:16/9;height:auto" allow="autoplay; fullscreen; picture-in-picture" allowfullscreen></iframe>`;
    const codeField = (label, value) => {
      const id = uid('code');
      const ta = h('textarea', { id, class: 'code', rows: '3', readonly: true, spellcheck: 'false' });
      ta.value = value;
      return h('div', { class: 'field' }, h('label', { for: id }, label), ta,
        h('div', { class: 'row-end' }, copyButton(() => ta.value, t('links.copyIframe'), { showLabel: true, cls: 'btn small' })));
    };

    async function fetchInfo() {
      info = await storage(bucketName, path, { query: { info: '1' } });
      if (info.signed && !signed) signed = info.signed;
      if (!info.encoding || info.encoding.visibility !== 'private') signed = null;
      return info;
    }

    function playDialog() {
      const e = info && info.encoding;
      if (!e || e.status !== 'ready') return;
      const src = new URL(e.visibility === 'private' && signed ? signed.embed : e.embed_url, location.origin);
      src.searchParams.set('autoplay', '1');
      src.searchParams.set('theme', getThemeChoice());
      const pd = modal({ title: t('browser.playTitle', { name: info.name }), wide: true });
      pd.body.append(h('div', { class: 'preview-box' }, h('iframe', { src: src.toString(), title: t('vod.previewTitle', { title: info.name }), allow: 'autoplay; fullscreen; picture-in-picture' })),
        e.visibility === 'private' && signed ? h('p', { class: 'hint' }, icon('lock', 14), ' ', t('browser.playSigned', { when: fmtDateTime(signed.expires_at) })) : null);
      pd.showModal();
    }

    async function setVisibility(v) {
      const e = info.encoding;
      try {
        await ctx.api(`/encodings/${enc(e.vod_id)}`, { method: 'PATCH', body: { visibility: v } });
        signed = null;
        await fetchInfo();
        const { bucket: _b, url: _u, download_url: _d, signed: _s, ...obj } = info;
        updateItem(obj);
        renderAll();
        toast(v === 'private' ? t('browser.madePrivate') : t('browser.madePublic'), 'success');
      } catch (ex) { toast(describeError(ex), 'error'); renderAll(); }
    }

    function editMetaDialog() {
      const e = info.encoding;
      const dlg2 = modal({ title: t('browser.editMetaTitle') });
      const title = textField({ label: t('create.titleLabel'), name: 'title', value: vodMeta ? vodMeta.title : info.name, attrs: { required: true, maxlength: '200' } });
      const desc = textField({ label: t('edit.description'), name: 'description', value: vodMeta ? vodMeta.description : '', textarea: true, rows: 4, attrs: { maxlength: '5000' } });
      const errors = formErrors({ title, description: desc });
      const save = h('button', { type: 'submit', class: 'btn primary' }, t('common.save'));
      const form = h('form', { class: 'stack' }, h('p', { class: 'hint' }, t('browser.editMetaHint')), title, desc, errors.general,
        h('div', { class: 'modal-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => dlg2.close('cancel') }, t('common.cancel')), save));
      form.addEventListener('submit', async (ev) => {
        ev.preventDefault();
        if (!title.control.value.trim()) { errors.show(t('error.titleRequired'), 'title'); return; }
        save.disabled = true;
        try {
          vodMeta = await ctx.api(`/encodings/${enc(e.vod_id)}`, { method: 'PATCH', body: { title: title.control.value, description: desc.control.value } });
          toast(t('edit.saved'), 'success');
          dlg2.close('ok');
          renderAll();
        } catch (ex) { errors.show(ex); } finally { save.disabled = false; }
      });
      dlg2.body.append(form);
      dlg2.showModal();
      title.control.focus();
    }

    function signedPanel() {
      const e = info.encoding;
      const box = h('div', { class: 'stack' });
      const presets = [['3600', t('browser.exp1h')], ['86400', t('browser.exp24h')], ['604800', t('browser.exp7d')], ['custom', t('browser.expCustom')]];
      const expSel = selectField({ label: t('browser.expiry'), name: 'expiry', options: presets, value: '3600' });
      const amountId = uid('amt');
      const amount = h('input', { id: amountId, type: 'number', min: '1', max: '365', step: '1', value: '30', class: 'num-input' });
      const unit = h('select', { 'aria-label': t('browser.expUnit') }, h('option', { value: '86400' }, t('browser.expDays')), h('option', { value: '3600' }, t('browser.expHours')));
      const custom = h('div', { class: 'field', hidden: true }, h('label', { for: amountId }, t('browser.expCustomLabel')), h('div', { class: 'input-group narrow' }, amount, unit));
      expSel.control.addEventListener('change', () => { custom.hidden = expSel.control.value !== 'custom'; if (!custom.hidden) amount.focus(); });
      const err = h('p', { class: 'error', role: 'alert', hidden: true });
      const gen = iconButton('key', t('browser.signGenerate'), async () => {
        err.hidden = true;
        let secs = expSel.control.value === 'custom' ? Math.round(Number(amount.value) * Number(unit.value)) : Number(expSel.control.value);
        if (!Number.isFinite(secs) || secs < 3600 || secs > 31536000) { err.textContent = t('browser.expInvalid'); err.hidden = false; amount.focus(); return; }
        gen.disabled = true;
        try {
          signed = await storage(bucketName, path, { method: 'POST', query: { action: 'sign' }, body: { expires_secs: secs } });
          renderSigned();
          toast(t('browser.signedToast', { when: fmtDateTime(signed.expires_at) }), 'success');
        } catch (ex) { err.textContent = describeError(ex); err.hidden = false; } finally { gen.disabled = false; }
      }, { cls: 'btn primary' });
      const out = h('div', { class: 'stack' });
      function renderSigned() {
        if (!signed) { setChildren(out); return; }
        setChildren(out,
          h('p', { class: 'hint' }, icon('clock', 14), ' ', t('browser.signedExpires', { when: fmtDateTime(signed.expires_at) })),
          copyField({ label: t('browser.signedEmbed'), value: signed.embed }),
          copyField({ label: t('browser.signedWatch'), value: signed.watch }),
          copyField({ label: t('browser.signedHls'), value: signed.hls }),
          copyField({ label: t('browser.signedDash'), value: signed.dash }),
          codeField(t('browser.signedIframe'), iframeCode(signed.embed)));
      }
      renderSigned();
      setChildren(box,
        h('p', { class: 'hint' }, t('browser.signedLead')),
        h('div', { class: 'sign-row' }, expSel, custom, h('div', { class: 'sign-go' }, gen)),
        err, out);
      return h('section', { class: 'signed-panel', 'aria-label': t('browser.signedTitle') },
        h('h3', null, icon('lock', 16), ' ', t('browser.signedTitle')), box, e.status !== 'ready' ? h('p', { class: 'hint' }, t('browser.signedNotReady')) : null);
    }

    function renderAll() {
      if (!info) return;
      const e = info.encoding;
      const ready = e && e.status === 'ready';
      // Preview: poster (play button) / image / icon.
      let preview;
      if (ready) {
        const posterUrl = e.poster_url ? (e.visibility === 'private' && signed ? signed.poster : e.poster_url) : null;
        preview = h('button', { type: 'button', class: 'obj-preview playable', onclick: playDialog, 'aria-label': t('browser.playLabel', { name: info.name }) },
          posterUrl ? h('img', { src: posterUrl, alt: '' }) : h('span', { class: 'obj-preview-icon' }, icon('film', 32)),
          h('span', { class: 'obj-play' }, icon('play', 28)));
      } else if ((info.content_type || '').startsWith('image/')) {
        preview = h('div', { class: 'obj-preview' }, h('img', { src: storageUrl(bucketName, info.path), alt: info.name }));
      } else {
        preview = h('div', { class: 'obj-preview' }, h('span', { class: 'obj-preview-icon' }, icon(fileIcon(info), 36)),
          e && !ready ? h('span', { class: 'obj-preview-state' }, encodingChip(e)) : null);
      }
      const meta = kv([
        [t('browser.path'), h('code', null, info.path)],
        [t('browser.colSize'), `${fmtBytes(info.size)} (${fmtInt(info.size)} B)`],
        [t('browser.colType'), info.content_type || '—'],
        [t('browser.created'), fmtDateTime(info.created_at)],
        [t('browser.colModified'), fmtDateTime(info.updated_at)],
        info.sha256 ? ['SHA-256', h('code', { class: 'hash' }, info.sha256)] : null,
      ]);
      let encBox;
      if (e) {
        const visId = uid('vis');
        const visToggle = h('div', { class: 'segmented', role: 'group', 'aria-labelledby': visId },
          h('button', { type: 'button', class: 'seg', 'aria-pressed': String(e.visibility === 'public'), onclick: () => e.visibility !== 'public' && setVisibility('public') }, icon('globe', 14), ' ', t('browser.public')),
          h('button', { type: 'button', class: 'seg', 'aria-pressed': String(e.visibility === 'private'), onclick: () => e.visibility !== 'private' && setVisibility('private') }, icon('lock', 14), ' ', t('browser.private')));
        encBox = h('div', { class: 'stack' },
          kv([
            [t('common.status'), encodingChip(e)],
            [t('vod.ladder'), e.ladder],
            [t('vod.renditions'), renditionsLine(e) || (ready ? '—' : t('vod.noRenditionsYet'))],
            [t('vod.duration'), e.duration_secs != null ? fmtDuration(e.duration_secs) : '—'],
            [t('browser.outputSize'), e.output_bytes ? fmtBytes(e.output_bytes) : '—'],
            [t('vod.readyAt'), e.ready_at ? fmtDateTime(e.ready_at) : '—'],
            vodMeta ? [t('create.titleLabel'), vodMeta.title] : null,
            e.status === 'failed' && e.error ? [t('vod.error'), h('pre', { class: 'vod-error-text' }, e.error)] : null,
          ]),
          h('div', { class: 'field' }, h('span', { class: 'label', id: visId }, t('browser.visibility')), visToggle, h('p', { class: 'hint' }, e.visibility === 'private' ? t('browser.privateHint') : t('browser.publicHint'))),
          (e.renditions || []).length ? h('details', { class: 'snippet' }, h('summary', null, t('vod.renditions')),
            h('div', { class: 'table-wrap' }, h('table', null,
              h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('vod.colRendition')), h('th', { scope: 'col' }, t('vod.colResolution')), h('th', { scope: 'col', class: 'num' }, t('vod.colVideo')), h('th', { scope: 'col', class: 'num' }, t('vod.colBandwidth')))),
              h('tbody', null, ...e.renditions.map((r) => h('tr', null, h('td', null, r.name), h('td', null, `${r.width}×${r.height}`),
                h('td', { class: 'num' }, r.video_kbps ? fmtBitrate(r.video_kbps) : t('vod.crf')), h('td', { class: 'num' }, r.bandwidth ? fmtBitrate(r.bandwidth / 1000) : '—'))))))) : null);
      } else {
        encBox = h('p', { class: 'hint' }, isVideoName(info.name) ? t('browser.noEncodingVideo') : t('browser.noEncoding'));
      }
      const links = [
        copyField({ label: t('browser.linkDirect'), value: info.url, hint: t('browser.linkDirectHint') }),
        copyField({ label: t('browser.linkDownload'), value: info.download_url }),
      ];
      if (e) {
        links.push(
          e.visibility === 'private' ? h('p', { class: 'banner warn' }, icon('lock'), h('span', null, t('browser.privateLinks'))) : null,
          copyField({ label: t('vod.linkEmbed'), value: e.embed_url }),
          copyField({ label: t('vod.linkWatch'), value: e.watch_url }),
          copyField({ label: t('vod.linkHls'), value: e.hls_url }),
          copyField({ label: t('vod.linkDash'), value: e.dash_url }),
          e.poster_url ? copyField({ label: t('vod.linkPoster'), value: e.poster_url }) : null,
          codeField(t('links.iframeLabel'), iframeCode(e.embed_url)));
      }
      const actions = h('div', { class: 'modal-actions obj-actions' },
        h('a', { class: 'btn', href: info.download_url, download: info.name }, icon('download'), h('span', null, t('action.download'))),
        ready ? iconButton('play', t('action.play'), playDialog, { cls: 'btn' }) : null,
        e ? iconButton('edit', t('browser.editMeta'), editMetaDialog, { cls: 'btn' }) : null,
        isVideoName(info.name) || e ? iconButton('film', e ? t('browser.reencode') : t('browser.encode'), () => encodeDialog(path), { cls: 'btn' }) : null,
        iconButton('trash', t('action.delete'), () => deleteItems([path]), { cls: 'btn danger-ghost' }));
      setChildren(body,
        h('div', { class: 'grid-2' },
          h('div', { class: 'stack' }, preview, meta),
          h('div', { class: 'stack' }, h('h3', null, t('browser.encodingTitle')), encBox)),
        e && e.visibility === 'private' ? signedPanel() : null,
        h('h3', null, t('browser.linksTitle')),
        h('div', { class: 'stack' }, ...links),
        actions);
    }

    // What the dialog shows of an encoding: re-render only when it changes (a `vod`
    // event also comes for every play counted, which must not reset the open forms).
    const shown = (e) => (e ? [e.vod_id, e.status, e.progress, e.visibility, e.ladder, e.poster_url, e.error, e.output_bytes, (e.renditions || []).length].join('|') : '');
    openInfo = {
      path,
      update(obj) {
        if (!info) return;
        const prev = info.encoding;
        const changed = shown(prev) !== shown(obj.encoding) || info.size !== obj.size || info.updated_at !== obj.updated_at;
        info = { ...info, ...obj };
        if (!obj.encoding) delete info.encoding;
        if (!changed) return;
        const nowReady = obj.encoding && obj.encoding.status === 'ready';
        if (nowReady && !(prev && prev.status === 'ready')) fetchInfo().then(renderAll).catch(() => renderAll());
        else renderAll();
      },
      close() { dlg.close('cancel'); },
    };
    dlg.showModal();
    fetchInfo().then(() => {
      if (info.encoding) ctx.api(`/encodings/${enc(info.encoding.vod_id)}`).then((v) => { vodMeta = v; renderAll(); }).catch(() => {});
      renderAll();
    }).catch((e) => setChildren(body, h('p', { class: 'error' }, describeError(e))));
  }

  // ---------------------------------------------------------------- live updates
  const offs = [
    bus.on('vod', (v) => {
      if (!bucket || v.bucket_id !== bucket.id) return;
      for (const it of items.values()) {
        if (it.id === v.object_id) {
          updateItem({ ...it, encoding: encodingFromVod(v) });
          if (v.status === 'ready' || v.status === 'failed') refreshBucketSoon();
        }
      }
    }),
    bus.on('vod_deleted', (id) => {
      for (const it of items.values()) {
        if (it.encoding && it.encoding.vod_id === id) {
          const { encoding: _e, ...rest } = it;
          updateItem(rest);
        }
      }
    }),
    bus.on('storage-uploaded', ({ bucket: b, path, object }) => {
      if (b !== bucketName) return;
      if (parentDir(path) === dir && object && object.path) updateItem(object);
      else if (path.startsWith(dir)) load();
      refreshBucketSoon();
    }),
    // SSE reconnected / polling: resync.
    bus.on('resync', () => load()),
    bus.on('list', () => load()),
  ];
  load();
  return () => {
    destroyed = true;
    clearTimeout(refreshTimer);
    offs.forEach((off) => off());
    if (uploads.list) uploads.list.remove();
  };
}
