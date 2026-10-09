// rms dashboard — admin "Utenti" pages: the users list (#/users: search, status, usage
// with quota bars, keys, last login; create, suspend/reactivate, delete with the force
// confirmation) and the user detail (#/users/<id>: edit, usage tiles + recount, API keys,
// buckets with the admin bucket browser, encodings). Keys and buckets reuse the portal's
// dialogs (accounts.js) through the account API with ?user=<id>.

import {
  api, ApiError, bus, describeError, h, icon, iconButton, setChildren, toast, uid, modal, confirmDialog,
  textField, slugify, fmtBytes, fmtDateTime, fmtInt,
} from './core.js';
import {
  enc, accountCtx, formErrors, selectField, checkField, quotaBar, usageTiles, keyList, keyDialog,
  bucketCreateDialog, bucketSettingsDialog, deleteBucket, bucketChips, userStatusBadge, bucketNameMap, ladderChoices,
} from './accounts.js';
import { encodingChip } from './browser.js';
import { loadLadders } from './vod.js';
import { t, tn } from './i18n.js';

const USERNAME_RE = /^[a-z0-9_-]{2,32}$/;
const GB = 1e9;

/** Username suggested from a display name (slug, max 32). */
function usernameFrom(name) {
  return slugify(name).slice(0, 32).replace(/^[-_]+|[-_]+$/g, '');
}

/** Quota input: GB number + "unlimited" checkbox. `wrap.read()` → bytes (0 = unlimited) or NaN. */
function quotaField(bytes) {
  const id = uid('quota');
  const hintId = uid('hint');
  const input = h('input', { id, type: 'number', name: 'quota_gb', min: '0.1', step: '0.1', class: 'num-input', 'aria-describedby': hintId });
  const unlimited = checkField({ label: t('users.quotaUnlimited'), name: 'quota_unlimited', checked: !bytes });
  input.value = bytes ? String(Math.round((bytes / GB) * 100) / 100) : '10';
  const sync = () => { input.disabled = unlimited.control.checked; };
  unlimited.control.addEventListener('change', () => { sync(); if (!input.disabled) input.focus(); });
  sync();
  const wrap = h('div', { class: 'field' },
    h('label', { for: id }, t('users.quota')),
    h('div', { class: 'quota-input' }, h('div', { class: 'input-group narrow' }, input, h('span', { class: 'unit' }, 'GB')), unlimited),
    h('p', { id: hintId, class: 'hint' }, t('users.quotaHint')));
  wrap.control = input;
  wrap.read = () => {
    if (unlimited.control.checked) return 0;
    const gb = Number(input.value);
    return Number.isFinite(gb) && gb > 0 ? Math.round(gb * GB) : NaN;
  };
  return wrap;
}

function maxBucketsField(value) {
  const f = textField({ label: t('users.maxBuckets'), name: 'max_buckets', type: 'number', value: String(value ?? 10), hint: t('users.maxBucketsHint'), attrs: { min: '0', max: '10000', step: '1', class: 'num-input' } });
  return f;
}

async function ladderSelect(value) {
  let ladders = null;
  try { ladders = await loadLadders(); } catch (_) { /* keep only the default */ }
  const names = ladders ? ladders.ladders.map((l) => l.name) : [];
  return selectField({
    label: t('users.defaultLadder'), name: 'default_ladder',
    options: ladderChoices(names, t('users.ladderServer', { name: ladders ? ladders.default_ladder : '—' })),
    value: value || '', hint: t('users.defaultLadderHint'),
  });
}

/** "Nuovo utente" / edit dialog. */
async function userDialog({ user = null, onSaved }) {
  const editing = !!user;
  const display = textField({ label: t('users.displayName'), name: 'display_name', value: user ? user.display_name : '', attrs: { maxlength: '100', placeholder: t('users.displayPlaceholder') } });
  const username = editing ? null : textField({ label: t('users.username'), name: 'username', hint: t('users.usernameHint'), attrs: { required: true, maxlength: '32', pattern: '[a-z0-9_\\-]{2,32}', spellcheck: 'false', autocapitalize: 'off', class: 'mono' } });
  const email = textField({ label: t('users.email'), name: 'email', type: 'email', value: user ? user.email || '' : '', attrs: { maxlength: '254', autocomplete: 'off', placeholder: 'nome@example.com' } });
  const quota = quotaField(user ? user.quota_bytes : 0);
  const maxB = maxBucketsField(user ? user.max_buckets : 10);
  const ladder = await ladderSelect(user ? user.default_ladder : '');
  const dlg = modal({ title: editing ? t('users.editTitle', { name: user.username }) : t('users.newTitle') });
  const errors = formErrors({ username, display_name: display, email, quota_bytes: quota, max_buckets: maxB, default_ladder: ladder });
  const submit = h('button', { type: 'submit', class: 'btn primary' }, editing ? t('common.save') : t('users.create'));
  const form = h('form', { class: 'stack', novalidate: true }, display, username, email,
    h('div', { class: 'grid-2 tight' }, quota, maxB), ladder, errors.general,
    h('div', { class: 'modal-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => dlg.close('cancel') }, t('common.cancel')), submit));
  if (username) {
    let manual = false;
    display.control.addEventListener('input', () => { if (!manual) username.control.value = usernameFrom(display.control.value); });
    username.control.addEventListener('input', () => { manual = username.control.value !== ''; username.control.value = username.control.value.toLowerCase(); });
  }
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    errors.clear();
    const body = { display_name: display.control.value.trim(), email: email.control.value.trim() || null, default_ladder: ladder.control.value || null };
    if (username) {
      body.username = username.control.value.trim();
      if (!USERNAME_RE.test(body.username)) { errors.show(t('error.invalidUsername'), 'username'); return; }
    }
    const q = quota.read();
    if (Number.isNaN(q)) { errors.show(t('users.quotaInvalid'), 'quota_bytes'); return; }
    body.quota_bytes = q;
    const mb = Number(maxB.control.value);
    if (!Number.isInteger(mb) || mb < 0 || mb > 10000) { errors.show(t('users.maxBucketsInvalid'), 'max_buckets'); return; }
    body.max_buckets = mb;
    if (!editing && !body.email) delete body.email;
    submit.disabled = true;
    try {
      const res = editing
        ? await api(`/users/${enc(user.id)}`, { method: 'PATCH', body })
        : await api('/users', { method: 'POST', body });
      dlg.close('ok');
      toast(editing ? t('users.saved') : t('users.created', { name: res.username }), 'success');
      if (onSaved) onSaved(res);
    } catch (ex) { errors.show(ex); } finally { submit.disabled = false; }
  });
  dlg.body.append(form);
  dlg.showModal();
  display.control.focus();
}

/** Suspend / reactivate (confirmation only when suspending). */
async function toggleStatus(user, onDone) {
  const suspend = user.status !== 'suspended';
  if (suspend) {
    const ok = await confirmDialog({ title: t('users.suspendTitle'), message: t('users.suspendText', { name: user.username }), confirmLabel: t('users.suspend'), danger: true });
    if (!ok) return;
  }
  try {
    const res = await api(`/users/${enc(user.id)}`, { method: 'PATCH', body: { status: suspend ? 'suspended' : 'active' } });
    toast(suspend ? t('users.suspended', { name: user.username }) : t('users.reactivated', { name: user.username }), 'success');
    if (onDone) onDone(res);
  } catch (e) { toast(describeError(e), 'error'); }
}

/** Delete: confirm, then (409: the user owns buckets) a second confirmation with ?force=1. */
async function deleteUser(user, onDone) {
  const ok = await confirmDialog({ title: t('users.deleteTitle'), message: t('users.deleteText', { name: user.username }), confirmLabel: t('action.delete'), danger: true });
  if (!ok) return;
  try {
    // Known to own buckets: straight to the second confirmation; otherwise the API's 409
    // (buckets created meanwhile) leads there.
    const owns = (user.usage && user.usage.buckets) || 0;
    try {
      if (owns) throw new ApiError(409, 'the user owns buckets');
      await api(`/users/${enc(user.id)}`, { method: 'DELETE' });
    } catch (e) {
      if (!(e instanceof ApiError && e.status === 409)) throw e;
      const n = owns || 1;
      const sure = await confirmDialog({
        title: t('users.forceTitle'),
        message: [tn('users.forceText', n, { n: fmtInt(n), name: user.username }), t('users.forceWarn')],
        confirmLabel: t('users.forceConfirm'), danger: true,
      });
      if (!sure) return;
      await api(`/users/${enc(user.id)}?force=1`, { method: 'DELETE' });
    }
    toast(t('users.deleted', { name: user.username }), 'success');
    if (onDone) onDone();
  } catch (e) { toast(describeError(e), 'error'); }
}

// ---------------------------------------------------------------------------
// Users list
// ---------------------------------------------------------------------------

export function mountUsers(root, { navigate }) {
  document.title = t('users.docTitle');
  let destroyed = false;
  let users = [];
  let query = '';
  const summary = h('p', { class: 'muted', 'aria-live': 'polite' });
  const searchId = uid('search');
  const search = h('input', { id: searchId, type: 'search', placeholder: t('users.searchPlaceholder'), autocomplete: 'off', spellcheck: 'false' });
  search.addEventListener('input', () => { query = search.value.trim().toLowerCase(); render(); });
  const body = h('div', null, h('p', { class: 'muted' }, t('common.loading')));
  const newBtn = h('button', { type: 'button', class: 'btn primary', onclick: () => userDialog({ onSaved: (u) => navigate(`#/users/${enc(u.username)}`) }) }, icon('plus'), h('span', null, t('users.new')));
  setChildren(root, h('div', { class: 'page' },
    h('div', { class: 'page-head' }, h('div', null, h('h1', null, t('users.title')), summary), newBtn),
    h('section', { class: 'panel', 'aria-labelledby': 'users-h' },
      h('div', { class: 'panel-head' }, h('h2', { id: 'users-h' }, icon('users'), t('users.listTitle')),
        h('div', { class: 'search-box' }, h('label', { for: searchId, class: 'sr-only' }, t('users.search')), icon('search', 16), search)),
      body)));

  function row(u) {
    const quota = u.quota_bytes;
    return h('tr', { dataset: { status: u.status } },
      h('td', null,
        h('a', { class: 'strong-link', href: `#/users/${enc(u.username)}` }, u.display_name || u.username),
        h('span', { class: 'mono muted small block' }, u.username),
        u.email ? h('span', { class: 'muted small block' }, u.email) : null),
      h('td', null, userStatusBadge(u.status)),
      h('td', { class: 'num' }, `${fmtInt(u.usage.buckets)} / ${fmtInt(u.max_buckets)}`),
      h('td', { class: 'usage-cell' }, quotaBar(u.usage.bytes_used, quota, { compact: true })),
      h('td', { class: 'num' }, fmtInt(u.usage.keys)),
      h('td', null, u.last_login_at ? fmtDateTime(u.last_login_at) : h('span', { class: 'muted' }, t('users.neverLogged'))),
      h('td', null, h('div', { class: 'actions-cell' },
        h('a', { class: 'btn small', href: `#/users/${enc(u.username)}`, 'aria-label': t('users.openLabel', { name: u.username }) }, t('users.open')),
        iconButton(u.status === 'suspended' ? 'play' : 'pause', u.status === 'suspended' ? t('users.reactivate') : t('users.suspend'),
          () => toggleStatus(u, load), { cls: 'btn small', showLabel: false, attrs: { 'aria-label': (u.status === 'suspended' ? t('users.reactivateLabel', { name: u.username }) : t('users.suspendLabel', { name: u.username })) } }),
        iconButton('trash', t('action.delete'), () => deleteUser(u, load), { cls: 'btn small danger-ghost', showLabel: false, attrs: { 'aria-label': t('users.deleteLabel', { name: u.username }) } }))));
  }

  function render() {
    if (destroyed) return;
    const suspended = users.filter((u) => u.status === 'suspended').length;
    summary.textContent = users.length
      ? [tn('users.count', users.length, { n: fmtInt(users.length) }), suspended ? t('users.countSuspended', { n: fmtInt(suspended) }) : null, t('users.countSpace', { size: fmtBytes(users.reduce((a, u) => a + u.usage.bytes_used, 0)) })].filter(Boolean).join(' · ')
      : t('users.lead');
    if (!users.length) {
      setChildren(body, h('div', { class: 'state-box' }, icon('users', 28), h('p', { class: 'state-title' }, t('users.empty')), h('p', { class: 'hint' }, t('users.emptyHint')),
        h('button', { type: 'button', class: 'btn primary', onclick: () => newBtn.click() }, icon('plus'), h('span', null, t('users.new')))));
      return;
    }
    const shown = users.filter((u) => !query || [u.username, u.display_name, u.email || ''].some((s) => s.toLowerCase().includes(query)));
    if (!shown.length) { setChildren(body, h('p', { class: 'muted' }, t('users.noMatch'))); return; }
    setChildren(body, h('div', { class: 'table-wrap' }, h('table', { class: 'users-table' },
      h('thead', null, h('tr', null,
        h('th', { scope: 'col' }, t('users.colUser')),
        h('th', { scope: 'col' }, t('common.status')),
        h('th', { scope: 'col', class: 'num' }, t('users.colBuckets')),
        h('th', { scope: 'col' }, t('users.colUsage')),
        h('th', { scope: 'col', class: 'num' }, t('users.colKeys')),
        h('th', { scope: 'col' }, t('users.colLastLogin')),
        h('th', { scope: 'col' }, h('span', { class: 'sr-only' }, t('browser.colActions'))))),
      h('tbody', null, ...shown.map(row)))));
  }

  async function load() {
    try {
      users = await api('/users');
      render();
    } catch (e) {
      if (destroyed || (e instanceof ApiError && e.status === 401)) return;
      setChildren(body, h('p', { class: 'error' }, describeError(e)));
    }
  }
  load();
  return () => { destroyed = true; };
}

// ---------------------------------------------------------------------------
// User detail
// ---------------------------------------------------------------------------

export function mountUserDetail(root, id, { navigate }) {
  let destroyed = false;
  let user = null;
  let account = null;
  let keys = [];
  let buckets = [];
  let encodings = [];
  const ctx = () => accountCtx({ userId: user ? user.id : id });

  setChildren(root, h('div', { class: 'loading-page', role: 'status' }, h('span', { class: 'spinner' }), t('common.loading')));

  const head = h('div', { class: 'detail-head' });
  const panel = (key, iconName, title, tools) => {
    const tid = `ud-${key}`;
    const el = h('section', { class: 'panel', 'aria-labelledby': tid },
      h('div', { class: 'panel-head' }, h('h2', { id: tid }, icon(iconName), title), h('div', { class: 'panel-tools' }, ...(tools || []))));
    el.body = h('div');
    el.append(el.body);
    return el;
  };
  const recountBtn = iconButton('refresh', t('users.recount'), recount, { cls: 'btn small', attrs: { title: t('users.recountTitle') } });
  const usagePanel = panel('usage', 'chart', t('users.usageTitle'), [recountBtn]);
  const keysPanel = panel('keys', 'key', t('keys.title'), [iconButton('plus', t('keys.new'), () => keyDialog({ ctx: ctx(), buckets, onSaved: loadKeys }), { cls: 'btn small primary' })]);
  const bucketsPanel = panel('buckets', 'bucket', t('buckets.title'), [iconButton('plus', t('buckets.new'), () => bucketCreateDialog({ ctx: ctx(), limits: account && account.limits, onCreated: () => { loadBuckets(); reloadUser(); } }), { cls: 'btn small primary' })]);
  const encPanel = panel('encodings', 'film', t('users.encodingsTitle'), []);

  function renderHead() {
    document.title = t('users.detailDocTitle', { name: user.username });
    setChildren(head,
      h('div', { class: 'detail-title' },
        h('a', { class: 'back', href: '#/users' }, icon('back', 16), t('users.backToList')),
        h('div', { class: 'title-row' }, h('h1', null, user.display_name || user.username), userStatusBadge(user.status)),
        h('p', { class: 'sub muted' }, [
          h('span', { class: 'mono' }, user.username),
          user.email ? ` · ${user.email}` : '',
          ` · ${t('users.createdAt', { when: fmtDateTime(user.created_at) })}`,
          ` · ${user.last_login_at ? t('users.lastLogin', { when: fmtDateTime(user.last_login_at) }) : t('users.neverLogged')}`,
        ])),
      h('div', { class: 'head-actions' },
        iconButton('edit', t('action.edit'), () => userDialog({ user, onSaved: (u) => { user = { ...user, ...u }; renderAll(); } }), { cls: 'btn' }),
        iconButton(user.status === 'suspended' ? 'play' : 'pause', user.status === 'suspended' ? t('users.reactivate') : t('users.suspend'), () => toggleStatus(user, (u) => { user = { ...user, ...u }; renderAll(); }), { cls: 'btn' }),
        iconButton('trash', t('action.delete'), () => deleteUser(user, () => navigate('#/users')), { cls: 'btn danger-ghost' })));
  }

  function renderUsage() {
    setChildren(usagePanel.body,
      user.status === 'suspended' ? h('div', { class: 'banner warn', role: 'status' }, icon('alert'), h('span', null, t('users.suspendedBanner'))) : null,
      usageTiles(user.usage, { quota: user.quota_bytes, maxBuckets: user.max_buckets }),
      h('dl', { class: 'kv compact' },
        h('dt', null, t('users.quota')), h('dd', null, user.quota_bytes ? fmtBytes(user.quota_bytes) : t('users.quotaUnlimited')),
        h('dt', null, t('users.maxBuckets')), h('dd', null, fmtInt(user.max_buckets)),
        h('dt', null, t('users.defaultLadder')), h('dd', null, user.default_ladder || t('users.ladderServer', { name: (account && account.limits.default_ladder) || '—' }))));
  }

  function renderKeys() {
    setChildren(keysPanel.body, keyList({ ctx: ctx(), keys, buckets, onChange: () => { loadKeys(); reloadUser(); } }));
  }

  function renderBuckets() {
    const base = `#/users/${enc(user.username)}/buckets`;
    if (!buckets.length) {
      setChildren(bucketsPanel.body, h('div', { class: 'state-box' }, icon('bucket', 28), h('p', { class: 'state-title' }, t('buckets.empty')), h('p', { class: 'hint' }, t('users.bucketsEmptyHint'))));
      return;
    }
    setChildren(bucketsPanel.body, h('div', { class: 'table-wrap' }, h('table', null,
      h('thead', null, h('tr', null,
        h('th', { scope: 'col' }, t('buckets.name')),
        h('th', { scope: 'col', class: 'num' }, t('buckets.usage')),
        h('th', { scope: 'col', class: 'num' }, t('buckets.files')),
        h('th', { scope: 'col' }, t('buckets.options')),
        h('th', { scope: 'col' }, t('buckets.createdCol')),
        h('th', { scope: 'col' }, h('span', { class: 'sr-only' }, t('browser.colActions'))))),
      h('tbody', null, ...buckets.map((b) => h('tr', null,
        h('td', null, h('a', { class: 'strong-link mono', href: `${base}/${enc(b.name)}/` }, b.name), b.description ? h('span', { class: 'muted small block' }, b.description) : null),
        h('td', { class: 'num' }, fmtBytes(b.bytes_used)),
        h('td', { class: 'num' }, fmtInt(b.objects_count)),
        h('td', null, bucketChips(b)),
        h('td', null, fmtDateTime(b.created_at)),
        h('td', null, h('div', { class: 'actions-cell' },
          h('a', { class: 'btn small primary', href: `${base}/${enc(b.name)}/`, 'aria-label': t('buckets.openLabel', { name: b.name }) }, icon('folder', 16), h('span', null, t('buckets.open'))),
          iconButton('settings', t('buckets.settings'), () => bucketSettingsDialog({ ctx: ctx(), bucket: b, limits: account && account.limits, onSaved: loadBuckets, onDeleted: () => { loadBuckets(); reloadUser(); } }), { cls: 'btn small', showLabel: false, attrs: { 'aria-label': t('buckets.settingsLabel', { name: b.name }) } }),
          iconButton('trash', t('action.delete'), () => deleteBucket(ctx(), b, () => { loadBuckets(); reloadUser(); }), { cls: 'btn small danger-ghost', showLabel: false, attrs: { 'aria-label': t('buckets.deleteLabel', { name: b.name }) } })))))))));
  }

  function renderEncodings() {
    const names = bucketNameMap(buckets);
    if (!encodings.length) { setChildren(encPanel.body, h('p', { class: 'muted' }, t('users.encodingsEmpty'))); return; }
    const sorted = [...encodings].sort((a, b) => (a.created_at < b.created_at ? 1 : -1));
    setChildren(encPanel.body, h('div', { class: 'table-wrap' }, h('table', null,
      h('thead', null, h('tr', null,
        h('th', { scope: 'col' }, t('users.colVideo')),
        h('th', { scope: 'col' }, t('users.colFile')),
        h('th', { scope: 'col' }, t('common.status')),
        h('th', { scope: 'col' }, t('vod.ladder')),
        h('th', { scope: 'col', class: 'num' }, t('browser.outputSize')),
        h('th', { scope: 'col' }, t('vod.added')))),
      h('tbody', null, ...sorted.map((v) => {
        const bname = names.get(v.bucket_id) || v.bucket_id || '—';
        const dirPath = v.object_path ? v.object_path.slice(0, v.object_path.lastIndexOf('/') + 1) : '/';
        const href = v.bucket_id && names.has(v.bucket_id) ? `#/users/${enc(user.username)}/buckets/${enc(bname)}${dirPath.split('/').map(enc).join('/')}` : null;
        return h('tr', null,
          h('td', null, h('strong', null, v.title), v.status === 'ready' && v.visibility === 'public' ? h('a', { class: 'small block', href: v.urls.watch, target: '_blank', rel: 'noopener' }, t('vod.watch')) : null),
          h('td', null, href ? h('a', { class: 'mono small', href }, `${bname}:${v.object_path || ''}`) : h('span', { class: 'mono small' }, `${bname}:${v.object_path || ''}`)),
          h('td', null, encodingChip({ status: v.status, progress: v.progress, error: v.error, visibility: v.visibility })),
          h('td', null, v.ladder),
          h('td', { class: 'num' }, v.output_bytes ? fmtBytes(v.output_bytes) : '—'),
          h('td', null, fmtDateTime(v.created_at)));
      })))));
  }

  function renderAll() {
    if (destroyed || !user) return;
    renderHead();
    renderUsage();
    renderKeys();
    renderBuckets();
    renderEncodings();
  }

  async function reloadUser() {
    try { user = await api(`/users/${enc(id)}`); if (!destroyed) { renderHead(); renderUsage(); } } catch (_) { /* keep */ }
  }
  async function loadKeys() {
    try { keys = await ctx().api('/keys'); if (!destroyed) renderKeys(); } catch (e) { toast(describeError(e), 'error'); }
  }
  async function loadBuckets() {
    try { buckets = await ctx().api('/buckets'); if (!destroyed) { renderBuckets(); renderKeys(); renderEncodings(); } } catch (e) { toast(describeError(e), 'error'); }
  }
  async function recount() {
    recountBtn.disabled = true;
    try {
      const r = await api(`/users/${enc(user.id)}/recount`, { method: 'POST' });
      user = { ...user, usage: r.usage };
      renderUsage();
      const rep = r.report || {};
      toast(t('users.recounted', { files: fmtInt(rep.files_checked || 0), fixed: fmtInt(rep.sizes_fixed || 0), missing: fmtInt(rep.missing_files || 0) }), 'success', 6000);
      loadBuckets();
    } catch (e) { toast(describeError(e), 'error'); } finally { recountBtn.disabled = false; }
  }

  (async () => {
    try {
      user = await api(`/users/${enc(id)}`);
      const c = ctx();
      [account, keys, buckets, encodings] = await Promise.all([
        c.api('').catch(() => null), c.api('/keys'), c.api('/buckets'), api(`/users/${enc(user.id)}/encodings`),
      ]);
      if (destroyed) return;
      setChildren(root, h('div', { class: 'detail' }, head, usagePanel, keysPanel, bucketsPanel, encPanel));
      renderAll();
    } catch (e) {
      if (destroyed || (e instanceof ApiError && e.status === 401)) return;
      document.title = t('users.docTitle');
      setChildren(root, h('div', { class: 'empty' }, icon('alert', 32),
        h('h1', null, e instanceof ApiError && e.status === 404 ? t('error.userNotFound') : t('users.loadError')),
        h('p', { class: 'muted' }, describeError(e)),
        h('a', { class: 'btn primary', href: '#/users' }, t('users.backToList'))));
    }
  })();

  let encTimer = null;
  const offs = [
    bus.on('vod', (v) => {
      if (!user || v.user_id !== user.id) return;
      const i = encodings.findIndex((x) => x.id === v.id);
      if (i >= 0) encodings[i] = v; else encodings.push(v);
      clearTimeout(encTimer);
      encTimer = setTimeout(() => { if (!destroyed) renderEncodings(); }, 250);
    }),
    bus.on('vod_deleted', (vid) => {
      const before = encodings.length;
      encodings = encodings.filter((x) => x.id !== vid);
      if (encodings.length !== before && !destroyed) renderEncodings();
    }),
  ];
  return () => { destroyed = true; clearTimeout(encTimer); offs.forEach((off) => off()); };
}

