// rms dashboard — building blocks shared by the admin "Utenti" pages (users.js) and the
// user portal (portal.js): the account API context (own account, or the admin acting on
// a user with ?user=), usage tiles and quota bars, API keys (create / edit / result /
// revoke dialogs and the key list), buckets (create, settings, rotate the signing key,
// delete with the force confirmation) and inline field errors for `400 {error, field}`.

import {
  api, ApiError, describeError, h, icon, iconButton, toast, uid, modal, confirmDialog,
  copyField, textField, fmtBytes, fmtDateTime, fmtInt, accessName,
} from './core.js';
import { t, tn, formatNumber } from './i18n.js';

export const enc = encodeURIComponent;

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/**
 * Account API context. Portal: `accountCtx()` (the key's own account). Admin acting on a
 * user: `accountCtx({ userId })`, every call gets `?user=<id>`.
 */
export function accountCtx({ userId = null, currentKeyId = null } = {}) {
  const admin = !!userId;
  const withUser = (path) => (admin ? `${path}${path.includes('?') ? '&' : '?'}user=${enc(userId)}` : path);
  return {
    admin,
    userId,
    currentKeyId,
    /** `/api/v1/account<path>` (+ `?user=` for the admin). */
    api: (path, opts) => api(withUser('/account' + path), opts),
  };
}

// ---------------------------------------------------------------------------
// Inline field errors
// ---------------------------------------------------------------------------

/** Show (or clear, with `null`) an error under a field wrapper built by textField & co. */
export function setFieldError(wrap, message) {
  if (!wrap) return;
  const control = wrap.control || wrap.querySelector('input, select, textarea');
  let p = wrap.querySelector(':scope > .field-error');
  if (!message) {
    if (p) p.remove();
    if (control) control.removeAttribute('aria-invalid');
    return;
  }
  if (!p) {
    p = h('p', { class: 'hint error field-error', id: uid('err'), role: 'alert' });
    wrap.append(p);
  }
  p.textContent = message;
  if (control) {
    control.setAttribute('aria-invalid', 'true');
    const ids = new Set((control.getAttribute('aria-describedby') || '').split(' ').filter(Boolean));
    ids.add(p.id);
    control.setAttribute('aria-describedby', [...ids].join(' '));
  }
}

/**
 * Error presenter of a form: `fields` maps API field names to field wrappers. An
 * ApiError naming one of them goes under that field (and focuses it), anything else in
 * the general error paragraph (`errors.general`, to be placed in the form).
 */
/** Conflicts (409, no `field`) that still belong to a form field. */
const CONFLICT_FIELDS = [
  [/username is already taken/i, 'username'],
  [/bucket name is already taken/i, 'name'],
];

export function formErrors(fields = {}) {
  const general = h('p', { class: 'error', role: 'alert', hidden: true });
  const clear = () => {
    general.hidden = true;
    for (const w of Object.values(fields)) setFieldError(w, null);
  };
  return {
    general,
    clear,
    /** Show `err` (an ApiError or a message string). */
    show(err, field = null) {
      clear();
      const msg = typeof err === 'string' ? err : describeError(err);
      const conflict = err instanceof ApiError && !err.field ? CONFLICT_FIELDS.find(([re]) => re.test(err.message)) : null;
      const name = field || (err instanceof ApiError ? err.field || (conflict && conflict[1]) : null);
      const w = name && fields[name];
      if (w) {
        setFieldError(w, msg);
        const c = w.control || w.querySelector('input, select, textarea');
        if (c) c.focus();
      } else {
        general.textContent = msg;
        general.hidden = false;
      }
    },
  };
}

// ---------------------------------------------------------------------------
// Usage, quota
// ---------------------------------------------------------------------------

/** Share of the quota used (0-100), null when unlimited. */
export function quotaPct(used, quota) {
  if (!quota) return null;
  return Math.min(100, Math.round((Number(used || 0) / quota) * 1000) / 10);
}

/** Quota bar: "1,2 GB di 10 GB" + bar coloured by level; "illimitata" when 0. */
export function quotaBar(used, quota, { compact = false } = {}) {
  const pct = quotaPct(used, quota);
  const label = quota
    ? t('usage.ofQuota', { used: fmtBytes(used || 0), quota: fmtBytes(quota) })
    : t('usage.unlimitedQuota', { used: fmtBytes(used || 0) });
  const level = pct == null ? 'none' : pct >= 95 ? 'full' : pct >= 80 ? 'high' : 'ok';
  const fill = h('span', { class: 'quota-fill' });
  fill.style.width = `${pct == null ? 0 : Math.max(pct, used ? 1 : 0)}%`;
  return h('div', { class: 'quota' + (compact ? ' compact' : ''), dataset: { level } },
    h('div', { class: 'quota-text' }, h('span', null, label), pct == null ? null : h('span', { class: 'quota-pct' }, `${formatNumber(pct, { maximumFractionDigits: 1 })}%`)),
    h('div', {
      class: 'quota-track', role: 'meter', 'aria-label': t('usage.quotaLabel'),
      'aria-valuemin': '0', 'aria-valuemax': '100', 'aria-valuenow': String(pct == null ? 0 : pct),
      'aria-valuetext': label,
    }, fill));
}

const tile = (label, value, sub) => h('div', { class: 'tile' },
  h('span', { class: 'tile-label' }, label), h('span', { class: 'tile-value' }, value), sub ? h('span', { class: 'tile-sub' }, sub) : null);

/** Usage tiles of an account: `usage` = Usage (+ quota_bytes), `limits` = {quota_bytes, max_buckets}. */
export function usageTiles(usage, { quota = 0, maxBuckets = null } = {}) {
  const u = usage || {};
  return h('div', { class: 'stack' },
    h('div', { class: 'usage-total' },
      h('div', { class: 'usage-total-head' }, h('span', { class: 'tile-label' }, t('usage.total')), h('strong', { class: 'usage-total-value' }, fmtBytes(u.bytes_used || 0))),
      quotaBar(u.bytes_used, quota)),
    h('div', { class: 'tiles' },
      tile(t('usage.originals'), fmtBytes(u.originals_bytes || 0), tn('usage.files', u.objects || 0, { n: fmtInt(u.objects || 0) })),
      tile(t('usage.encodingsBytes'), fmtBytes(u.encodings_bytes || 0), tn('usage.encodings', u.encodings || 0, { n: fmtInt(u.encodings || 0) })),
      tile(t('usage.buckets'), maxBuckets != null ? t('usage.ofMax', { n: fmtInt(u.buckets || 0), max: fmtInt(maxBuckets) }) : fmtInt(u.buckets || 0)),
      tile(t('usage.keys'), fmtInt(u.keys || 0), t('usage.keysSub'))));
}

/** "bucket-a: /videos/ (scrittura) · *: / (lettura)". */
export function permissionsSummary(perms, bucketNames) {
  if (!perms || !perms.length) return '';
  return perms.map((p) => `${p.bucket === '*' ? '*' : (bucketNames.get(p.bucket) || p.bucket)}: ${p.prefix} (${accessName(p.access)})`).join(' · ');
}

export function scopeLabel(scope) {
  if (scope === 'account') return t('keys.scopeAccount');
  if (scope === 'storage') return t('keys.scopeStorage');
  return scope;
}

export function scopeChips(scopes) {
  return h('span', { class: 'chips' }, ...(scopes || []).map((s) => h('span', { class: `chip-tag scope-${s}` }, scopeLabel(s))));
}

const KEY_STATE = {
  active: ['status-ok', () => t('keys.state.active')],
  expired: ['status-paused', () => t('keys.state.expired')],
  revoked: ['status-error', () => t('keys.state.revoked')],
};

export function keyStateBadge(state) {
  const [cls, label] = KEY_STATE[state] || ['status-offline', () => state];
  return h('span', { class: `badge ${cls}` }, label());
}

export function userStatusBadge(status) {
  return status === 'suspended'
    ? h('span', { class: 'badge status-error' }, t('users.status.suspended'))
    : h('span', { class: 'badge status-ok' }, t('users.status.active'));
}

/** Bucket names by id. */
export const bucketNameMap = (buckets) => new Map((buckets || []).map((b) => [b.id, b.name]));

// ---------------------------------------------------------------------------
// Small form controls
// ---------------------------------------------------------------------------

/** Labelled <select>: options = [[value, label], ...]. */
export function selectField({ label, name, options, value, hint }) {
  const id = uid('sel');
  const hintId = hint ? uid('hint') : null;
  const select = h('select', { id, name, 'aria-describedby': hintId }, ...options.map(([v, text]) => h('option', { value: v }, text)));
  if (value != null) select.value = value;
  const wrap = h('div', { class: 'field' }, h('label', { for: id }, label), select, hint ? h('p', { id: hintId, class: 'hint' }, hint) : null);
  wrap.control = select;
  return wrap;
}

/** Checkbox with label and hint. */
export function checkField({ label, name, checked, hint }) {
  const id = uid('cb');
  const hintId = hint ? uid('hint') : null;
  const input = h('input', { type: 'checkbox', id, name, checked: !!checked, 'aria-describedby': hintId });
  const wrap = h('div', { class: 'check' }, input, h('label', { for: id }, label, hint ? h('span', { id: hintId, class: 'hint' }, hint) : null));
  wrap.control = input;
  return wrap;
}

/** Ladder options: "" = the default (described by `defaultLabel`), then the ladders. */
export function ladderChoices(ladders, defaultLabel) {
  return [['', defaultLabel], ...(ladders || []).map((l) => [l, l])];
}

/** RFC 3339 end of the chosen local day, or null for an empty date input. */
export function dateToExpiry(value) {
  if (!value) return null;
  const d = new Date(`${value}T23:59:59`);
  return Number.isNaN(d.getTime()) ? undefined : d.toISOString();
}

/** `yyyy-mm-dd` (local) of a timestamp, for <input type="date">. */
export function expiryToDate(value) {
  if (!value) return '';
  const d = new Date(value);
  if (Number.isNaN(d.getTime())) return '';
  const pad = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

const tomorrow = () => expiryToDate(new Date(Date.now() + 86400000));

// ---------------------------------------------------------------------------
// API keys
// ---------------------------------------------------------------------------

/** Client-side check of a permission prefix: starts with '/', no empty or dot segments. */
export function prefixError(prefix) {
  if (!prefix.startsWith('/')) return t('keys.prefixSlash');
  const segs = prefix.split('/').slice(1, -1);
  if (segs.some((s) => s === '' || s === '.' || s === '..')) return t('keys.prefixInvalid');
  if (/[\\\u0000-\u001f]/.test(prefix)) return t('keys.prefixInvalid');
  return null;
}

/** Permission rows editor. `buckets` = [BucketView]; `value` = [{bucket, prefix, access}]. */
function permissionsEditor(buckets, value = []) {
  const rows = h('div', { class: 'perm-rows' });
  const head = h('div', { class: 'perm-row perm-head', 'aria-hidden': 'true' },
    h('span', null, t('keys.permBucket')), h('span', null, t('keys.permPrefix')), h('span', null, t('keys.permAccess')), h('span'));
  const empty = h('p', { class: 'hint perm-empty' }, t('keys.permNone'));
  const add = iconButton('plus', t('keys.permAdd'), () => { addRow({ bucket: '*', prefix: '/', access: 'read' }); sync(); rows.lastElementChild.querySelector('select').focus(); }, { cls: 'btn small' });
  const wrap = h('fieldset', { class: 'perm-editor' },
    h('legend', null, t('keys.permissions')),
    h('p', { class: 'hint' }, t('keys.permHint')),
    head, rows, empty, h('div', null, add));

  function addRow(p) {
    const n = rows.children.length + 1;
    const bucket = h('select', { 'aria-label': t('keys.permBucketN', { n }) },
      h('option', { value: '*' }, t('keys.allBuckets')),
      ...buckets.map((b) => h('option', { value: b.id }, b.name)));
    bucket.value = buckets.some((b) => b.id === p.bucket || b.name === p.bucket) ? (buckets.find((b) => b.id === p.bucket || b.name === p.bucket).id) : '*';
    const prefix = h('input', { type: 'text', class: 'mono', value: p.prefix || '/', spellcheck: 'false', autocapitalize: 'off', 'aria-label': t('keys.permPrefixN', { n }), placeholder: '/videos/' });
    const access = h('select', { 'aria-label': t('keys.permAccessN', { n }) },
      h('option', { value: 'read' }, accessName('read')),
      h('option', { value: 'write' }, accessName('write')),
      h('option', { value: 'admin' }, accessName('admin')));
    access.value = p.access || 'read';
    const err = h('p', { class: 'hint error perm-error', role: 'alert', hidden: true });
    const remove = iconButton('trash', t('keys.permRemoveN', { n }), () => { row.remove(); sync(); add.focus(); }, { cls: 'btn small ghost', showLabel: false });
    const row = h('div', { class: 'perm-row' }, bucket, prefix, access, remove, err);
    row.read = () => ({ bucket: bucket.value, prefix: prefix.value.trim() || '/', access: access.value });
    row.check = () => {
      const e = prefixError(prefix.value.trim() || '/');
      err.textContent = e || '';
      err.hidden = !e;
      if (e) prefix.setAttribute('aria-invalid', 'true'); else prefix.removeAttribute('aria-invalid');
      return e ? prefix : null;
    };
    prefix.addEventListener('input', () => { if (!err.hidden) row.check(); });
    rows.append(row);
  }
  function sync() {
    const n = rows.children.length;
    empty.hidden = n > 0;
    head.hidden = n === 0;
  }
  for (const p of value) addRow(p);
  sync();
  wrap.read = () => [...rows.children].map((r) => r.read());
  /** First invalid prefix input (and shows the errors), or null. */
  wrap.check = () => [...rows.children].map((r) => r.check()).find(Boolean) || null;
  wrap.count = () => rows.children.length;
  return wrap;
}

/** "Nuova chiave" / edit dialog. `key` = KeyView to edit (name, permissions, expiry). */
export function keyDialog({ ctx, buckets, key = null, onSaved }) {
  const editing = !!key;
  const dlg = modal({ title: editing ? t('keys.editTitle') : t('keys.newTitle'), wide: true });
  const name = textField({ label: t('keys.name'), name: 'name', value: key ? key.name : '', hint: t('keys.nameHint'), attrs: { required: true, maxlength: '100', placeholder: t('keys.namePlaceholder') } });
  const scopes = editing ? null : h('fieldset', { class: 'flags' },
    h('legend', null, t('keys.scopes')),
    checkField({ label: t('keys.scopeAccountLong'), name: 'scope_account', checked: false, hint: t('keys.scopeAccountHint') }),
    checkField({ label: t('keys.scopeStorageLong'), name: 'scope_storage', checked: true, hint: t('keys.scopeStorageHint') }));
  const perms = permissionsEditor(buckets, key ? key.permissions : [{ bucket: buckets[0] ? buckets[0].id : '*', prefix: '/', access: 'write' }]);
  const expId = uid('exp');
  const expiry = h('input', { id: expId, type: 'date', name: 'expires_at', min: tomorrow(), value: key ? expiryToDate(key.expires_at) : '' });
  const expiryWrap = h('div', { class: 'field' },
    h('label', { for: expId }, t('keys.expiry')),
    h('div', { class: 'input-group' }, expiry, h('button', { type: 'button', class: 'btn', onclick: () => { expiry.value = ''; } }, t('keys.noExpiry'))),
    h('p', { class: 'hint' }, t('keys.expiryHint')));
  expiryWrap.control = expiry;
  const errors = formErrors({ name, expires_at: expiryWrap, permissions: perms, scopes });
  const submit = h('button', { type: 'submit', class: 'btn primary' }, editing ? t('common.save') : t('keys.create'));
  const form = h('form', { class: 'stack', novalidate: true },
    name, scopes, perms, expiryWrap, errors.general,
    h('div', { class: 'modal-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => dlg.close('cancel') }, t('common.cancel')), submit));

  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    errors.clear();
    const body = { name: name.control.value.trim(), permissions: perms.read() };
    if (!body.name) { errors.show(t('keys.nameRequired'), 'name'); return; }
    if (scopes) {
      body.scopes = [];
      if (form.elements.scope_account.checked) body.scopes.push('account');
      if (form.elements.scope_storage.checked) body.scopes.push('storage');
      if (!body.scopes.length) { errors.show(t('keys.scopeRequired'), 'scopes'); return; }
      if (!body.scopes.includes('account') && !body.permissions.length) { errors.show(t('error.keyNeedsPermission'), 'permissions'); return; }
    }
    const bad = perms.check();
    if (bad) { bad.focus(); return; }
    const exp = dateToExpiry(expiry.value);
    if (exp === undefined) { errors.show(t('keys.expiryInvalid'), 'expires_at'); return; }
    if (editing) body.expires_at = exp; else if (exp) body.expires_at = exp;
    submit.disabled = true;
    try {
      const res = editing
        ? await ctx.api(`/keys/${enc(key.id)}`, { method: 'PATCH', body })
        : await ctx.api('/keys', { method: 'POST', body });
      dlg.close('ok');
      if (editing) toast(t('keys.saved'), 'success');
      else keyCreatedDialog(res);
      if (onSaved) onSaved(res);
    } catch (ex) {
      errors.show(ex);
    } finally { submit.disabled = false; }
  });
  dlg.body.append(form);
  dlg.showModal();
  name.control.focus();
}

/** "Chiave creata": the secret, shown once, with copy and usage examples. */
export function keyCreatedDialog(created) {
  const dlg = modal({ title: t('keys.createdTitle'), wide: true });
  const field = copyField({ label: t('keys.createdLabel'), value: created.key });
  const storage = (created.scopes || []).includes('storage') || (created.scopes || []).includes('account');
  const example = `curl -H "AccessKey: ${created.key}" -T video.mp4 \\\n  ${location.origin}/storage/<bucket>/videos/video.mp4`;
  const done = h('button', { type: 'button', class: 'btn primary', onclick: () => dlg.close('ok') }, t('keys.createdDone'));
  dlg.body.append(
    h('div', { class: 'banner warn', role: 'alert' }, icon('alert'), h('div', null, h('strong', null, t('keys.createdWarnTitle')), ' ', t('keys.createdWarn'))),
    field,
    h('dl', { class: 'kv compact' },
      h('dt', null, t('keys.name')), h('dd', null, created.name),
      h('dt', null, t('keys.prefix')), h('dd', null, h('code', null, created.key_prefix)),
      h('dt', null, t('keys.scopes')), h('dd', null, scopeChips(created.scopes)),
      h('dt', null, t('keys.expiry')), h('dd', null, created.expires_at ? fmtDateTime(created.expires_at) : t('keys.never'))),
    storage ? h('div', { class: 'field' }, h('span', { class: 'label' }, t('keys.createdExample')), h('pre', null, h('code', null, example))) : null,
    h('div', { class: 'modal-actions' }, done));
  dlg.showModal();
  field.input.focus();
  field.input.select();
}

/** Revoke after confirmation; `onDone(revokedSelf)`. */
export async function revokeKey(ctx, key, onDone) {
  const self = !ctx.admin && ctx.currentKeyId === key.id;
  const ok = await confirmDialog({
    title: t('keys.revokeTitle'),
    message: [t('keys.revokeText', { name: key.name, prefix: key.key_prefix }), self ? t('keys.revokeSelf') : null].filter(Boolean),
    confirmLabel: t('keys.revoke'), danger: true,
  });
  if (!ok) return;
  try {
    await ctx.api(`/keys/${enc(key.id)}`, { method: 'DELETE' });
    toast(t('keys.revoked'), 'success');
    if (onDone) onDone(self);
  } catch (e) { toast(describeError(e), 'error'); }
}

/** Key list (cards): name, prefix, scopes, permissions, expiry, last used, state, actions. */
export function keyList({ ctx, keys, buckets, onChange, onRevokedSelf }) {
  const names = bucketNameMap(buckets);
  if (!keys.length) {
    return h('div', { class: 'state-box' }, icon('key', 28), h('p', { class: 'state-title' }, t('keys.empty')), h('p', { class: 'hint' }, t('keys.emptyHint')));
  }
  const sorted = [...keys].sort((a, b) => (a.state === 'revoked') - (b.state === 'revoked') || (a.created_at < b.created_at ? 1 : -1));
  return h('ul', { class: 'key-list', 'aria-label': t('keys.listLabel') }, ...sorted.map((k) => {
    const current = !ctx.admin && ctx.currentKeyId === k.id;
    const perms = permissionsSummary(k.permissions, names);
    const active = k.state !== 'revoked';
    return h('li', { class: 'key-item', dataset: { state: k.state } },
      h('div', { class: 'key-main' },
        h('div', { class: 'key-title' },
          h('strong', null, k.name),
          keyStateBadge(k.state),
          current ? h('span', { class: 'tag' }, t('keys.current')) : null),
        h('div', { class: 'key-line' }, h('code', { title: t('keys.prefixTitle') }, `${k.key_prefix}…`), scopeChips(k.scopes)),
        h('p', { class: 'key-perms' }, perms ? [h('span', { class: 'muted' }, `${t('keys.permissions')}: `), perms]
          : h('span', { class: 'muted' }, (k.scopes || []).includes('account') ? t('keys.permAllAccount') : t('keys.permNoneShort'))),
        h('p', { class: 'key-meta' }, [
          k.expires_at ? t('keys.expiresAt', { when: fmtDateTime(k.expires_at) }) : t('keys.noExpiryShort'),
          k.last_used_at ? t('keys.lastUsed', { when: fmtDateTime(k.last_used_at) }) : t('keys.neverUsed'),
          k.revoked_at ? t('keys.revokedAt', { when: fmtDateTime(k.revoked_at) }) : t('keys.createdAt', { when: fmtDateTime(k.created_at) }),
        ].join(' · '))),
      h('div', { class: 'key-actions' },
        active ? iconButton('edit', t('action.edit'), () => keyDialog({ ctx, buckets, key: k, onSaved: onChange }), { cls: 'btn small', attrs: { 'aria-label': t('keys.editLabel', { name: k.name }) } }) : null,
        active ? iconButton('x', t('keys.revoke'), () => revokeKey(ctx, k, (self) => (self && onRevokedSelf ? onRevokedSelf() : onChange && onChange())), { cls: 'btn small danger-ghost', attrs: { 'aria-label': t('keys.revokeLabel', { name: k.name }) } }) : null));
  }));
}

// ---------------------------------------------------------------------------
// Buckets
// ---------------------------------------------------------------------------

export const BUCKET_RE = /^[a-z0-9_-]{2,63}$/;

/** "Nuovo bucket". `limits` = AccountView.limits (ladders, default_ladder). */
export function bucketCreateDialog({ ctx, limits, onCreated }) {
  const dlg = modal({ title: t('buckets.newTitle') });
  const name = textField({ label: t('buckets.name'), name: 'name', hint: t('buckets.nameHint'), attrs: { required: true, maxlength: '63', pattern: '[a-z0-9_\\-]{2,63}', spellcheck: 'false', autocapitalize: 'off', class: 'mono', placeholder: 'media' } });
  const desc = textField({ label: t('buckets.description'), name: 'description', textarea: true, rows: 2, attrs: { maxlength: '1000' } });
  const ladder = selectField({ label: t('buckets.defaultLadder'), name: 'default_ladder', options: ladderChoices(limits && limits.ladders, t('buckets.ladderInherit', { name: (limits && limits.default_ladder) || '—' })), hint: t('buckets.defaultLadderHint') });
  const auto = checkField({ label: t('buckets.autoEncode'), name: 'auto_encode', checked: true, hint: t('buckets.autoEncodeHint') });
  const pub = checkField({ label: t('buckets.publicPlayback'), name: 'public_playback', checked: true, hint: t('buckets.publicPlaybackHint') });
  const errors = formErrors({ name, description: desc, default_ladder: ladder });
  const submit = h('button', { type: 'submit', class: 'btn primary' }, t('buckets.create'));
  const form = h('form', { class: 'stack', novalidate: true }, name, desc, ladder, h('fieldset', { class: 'flags' }, h('legend', null, t('buckets.options')), auto, pub), errors.general,
    h('div', { class: 'modal-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => dlg.close('cancel') }, t('common.cancel')), submit));
  name.control.addEventListener('input', () => { name.control.value = name.control.value.toLowerCase(); });
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    errors.clear();
    const n = name.control.value.trim();
    if (!BUCKET_RE.test(n)) { errors.show(t('error.invalidBucketName'), 'name'); return; }
    submit.disabled = true;
    try {
      const b = await ctx.api('/buckets', {
        method: 'POST',
        body: { name: n, description: desc.control.value, default_ladder: ladder.control.value || null, auto_encode: auto.control.checked, public_playback: pub.control.checked },
      });
      dlg.close('ok');
      toast(t('buckets.created', { name: b.name }), 'success');
      if (onCreated) onCreated(b);
    } catch (ex) { errors.show(ex); } finally { submit.disabled = false; }
  });
  dlg.body.append(form);
  dlg.showModal();
  name.control.focus();
}

/** Delete a bucket: confirm, then (409 not empty) a second confirmation with ?force=1. */
export async function deleteBucket(ctx, bucket, onDeleted) {
  const ok = await confirmDialog({
    title: t('buckets.deleteTitle'),
    message: [t('buckets.deleteText', { name: bucket.name }), bucket.objects_count ? tn('buckets.deleteFiles', bucket.objects_count, { n: fmtInt(bucket.objects_count) }) : null].filter(Boolean),
    confirmLabel: t('action.delete'), danger: true,
  });
  if (!ok) return;
  const del = (force) => ctx.api(`/buckets/${enc(bucket.id)}${force ? '?force=1' : ''}`, { method: 'DELETE' });
  try {
    try {
      // Known not to be empty: straight to the second confirmation (the API's 409 also
      // leads there when files arrived meanwhile).
      if (bucket.objects_count) throw new ApiError(409, 'the bucket is not empty');
      await del(false);
    } catch (e) {
      if (!(e instanceof ApiError && e.status === 409)) throw e;
      const sure = await confirmDialog({
        title: t('buckets.forceTitle'),
        message: [t('buckets.forceText', { name: bucket.name }), t('buckets.forceWarn')],
        confirmLabel: t('buckets.forceConfirm'), danger: true,
      });
      if (!sure) return;
      await del(true);
    }
    toast(t('buckets.deleted', { name: bucket.name }), 'success');
    if (onDeleted) onDeleted(bucket);
  } catch (e) { toast(describeError(e), 'error'); }
}

/** Bucket settings: description, default ladder, auto-encode, public playback; rotate the signing key; delete. */
export function bucketSettingsDialog({ ctx, bucket, limits, onSaved, onDeleted }) {
  const dlg = modal({ title: t('buckets.settingsTitle', { name: bucket.name }), wide: true });
  const desc = textField({ label: t('buckets.description'), name: 'description', value: bucket.description, textarea: true, rows: 2, attrs: { maxlength: '1000' } });
  const ladder = selectField({ label: t('buckets.defaultLadder'), name: 'default_ladder', options: ladderChoices(limits && limits.ladders, t('buckets.ladderInherit', { name: (limits && limits.default_ladder) || '—' })), value: bucket.default_ladder || '', hint: t('buckets.defaultLadderHint') });
  const auto = checkField({ label: t('buckets.autoEncode'), name: 'auto_encode', checked: bucket.auto_encode, hint: t('buckets.autoEncodeHint') });
  const pub = checkField({ label: t('buckets.publicPlayback'), name: 'public_playback', checked: bucket.public_playback, hint: t('buckets.publicPlaybackHint') });
  const errors = formErrors({ description: desc, default_ladder: ladder });
  const save = h('button', { type: 'submit', class: 'btn primary' }, t('common.save'));
  const form = h('form', { class: 'stack', novalidate: true },
    copyField({ label: t('buckets.storageUrl'), value: bucket.storage_url, hint: t('buckets.storageUrlHint') }),
    desc, ladder, h('fieldset', { class: 'flags' }, h('legend', null, t('buckets.options')), auto, pub), errors.general,
    h('div', { class: 'modal-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => dlg.close('cancel') }, t('common.close')), save));
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    errors.clear();
    save.disabled = true;
    try {
      const b = await ctx.api(`/buckets/${enc(bucket.id)}`, {
        method: 'PATCH',
        body: { description: desc.control.value, default_ladder: ladder.control.value || null, auto_encode: auto.control.checked, public_playback: pub.control.checked },
      });
      Object.assign(bucket, b);
      toast(t('buckets.saved'), 'success');
      dlg.close('ok');
      if (onSaved) onSaved(b);
    } catch (ex) { errors.show(ex); } finally { save.disabled = false; }
  });

  const rotate = iconButton('refresh', t('buckets.rotateConfirm'), async () => {
    const ok = await confirmDialog({ title: t('buckets.rotateTitle'), message: [t('buckets.rotateText'), t('buckets.rotateWarn')], confirmLabel: t('buckets.rotateConfirm'), danger: true });
    if (!ok) return;
    try {
      const b = await ctx.api(`/buckets/${enc(bucket.id)}/rotate-signing-key`, { method: 'POST' });
      Object.assign(bucket, b);
      toast(t('buckets.rotated'), 'success');
      if (onSaved) onSaved(b);
    } catch (ex) { toast(describeError(ex), 'error'); }
  }, { cls: 'btn' });
  const del = iconButton('trash', t('action.delete'), () => {
    dlg.close('cancel');
    deleteBucket(ctx, bucket, onDeleted);
  }, { cls: 'btn danger-ghost' });

  dlg.body.append(form,
    h('section', { class: 'danger-zone', 'aria-labelledby': `${dlg.getAttribute('aria-labelledby')}-dz` },
      h('h3', { id: `${dlg.getAttribute('aria-labelledby')}-dz` }, t('buckets.dangerZone')),
      h('div', { class: 'dz-row' }, h('div', null, h('strong', null, t('buckets.rotate')), h('p', { class: 'hint' }, t('buckets.rotateHint'))), rotate),
      h('div', { class: 'dz-row' }, h('div', null, h('strong', null, t('buckets.delete')), h('p', { class: 'hint' }, t('buckets.deleteHint'))), del)));
  dlg.showModal();
  desc.control.focus();
}

/** Chips of a bucket: auto-encode, public/private playback. */
export function bucketChips(b) {
  return h('span', { class: 'chips' },
    h('span', { class: 'chip-tag', title: t('buckets.autoEncodeHint') }, b.auto_encode ? t('buckets.autoOn') : t('buckets.autoOff')),
    b.public_playback
      ? h('span', { class: 'chip-tag', title: t('buckets.publicPlaybackHint') }, icon('globe', 12), t('buckets.public'))
      : h('span', { class: 'chip-tag private', title: t('buckets.publicPlaybackHint') }, icon('lock', 12), t('buckets.private')));
}

/** "Nuovo bucket" disabled state: `n of max`. */
export function bucketsLeft(count, max) {
  return max != null && count >= max;
}

