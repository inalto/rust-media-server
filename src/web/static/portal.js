// rms dashboard — user portal (login with an API key holding the `account` scope):
// "I miei bucket" (#/buckets: cards, new bucket, settings, open the browser), "Chiavi API"
// (#/keys: list, create with scopes and path permissions, edit, revoke, how-to) and
// "Account" (#/account: profile, usage and quota, limits, the key in use, logout).
// The bucket browser itself is browser.js.

import {
  ApiError, describeError, h, icon, iconButton, setChildren, copyButton, fmtBytes, fmtDateTime, fmtInt,
} from './core.js';
import {
  enc, usageTiles, keyList, keyDialog, bucketCreateDialog, bucketSettingsDialog, bucketChips, scopeChips,
} from './accounts.js';
import { t, tn } from './i18n.js';

function errorBox(e) {
  return h('div', { class: 'state-box' }, icon('alert', 28), h('p', { class: 'state-title' }, t('portal.loadError')), h('p', { class: 'hint' }, describeError(e)));
}

const loading = () => h('div', { class: 'loading-page', role: 'status' }, h('span', { class: 'spinner' }), t('common.loading'));

// ---------------------------------------------------------------------------
// I miei bucket
// ---------------------------------------------------------------------------

export function mountBuckets(root, { ctx }) {
  document.title = t('buckets.docTitle');
  let destroyed = false;
  let account = null;
  let buckets = [];
  const summary = h('p', { class: 'muted', 'aria-live': 'polite' });
  const newBtn = h('button', { type: 'button', class: 'btn primary', disabled: true, onclick: () => bucketCreateDialog({ ctx, limits: account && account.limits, onCreated: (b) => { location.hash = `#/buckets/${enc(b.name)}/`; } }) }, icon('plus'), h('span', null, t('buckets.new')));
  const limitNote = h('p', { class: 'hint', hidden: true });
  const grid = h('div', { class: 'bucket-cards', role: 'list' });
  const body = h('div', null, loading());
  setChildren(root, h('div', { class: 'page' },
    h('div', { class: 'page-head' }, h('div', null, h('h1', null, t('buckets.myTitle')), summary), h('div', { class: 'stack head-new' }, newBtn, limitNote)),
    body));

  function card(b) {
    const href = `#/buckets/${enc(b.name)}/`;
    return h('article', { class: 'bucket-card', role: 'listitem' },
      h('div', { class: 'bucket-card-top' },
        h('span', { class: 'bucket-icon' }, icon('bucket', 20)),
        h('h2', { class: 'bucket-name' }, h('a', { href }, b.name)),
        iconButton('settings', t('buckets.settings'), () => bucketSettingsDialog({ ctx, bucket: b, limits: account && account.limits, onSaved: load, onDeleted: load }), { cls: 'btn small ghost', showLabel: false, attrs: { 'aria-label': t('buckets.settingsLabel', { name: b.name }) } })),
      h('p', { class: b.description ? 'bucket-desc' : 'bucket-desc muted' }, b.description || t('buckets.noDescription')),
      h('div', { class: 'bucket-stats' },
        h('span', null, h('strong', null, fmtBytes(b.bytes_used)), ' ', h('span', { class: 'muted' }, t('buckets.used'))),
        h('span', null, h('strong', null, fmtInt(b.objects_count)), ' ', h('span', { class: 'muted' }, tn('buckets.filesWord', b.objects_count)))),
      bucketChips(b),
      h('p', { class: 'hint' }, [b.default_ladder ? t('buckets.ladderIs', { name: b.default_ladder }) : null, t('buckets.createdOn', { when: fmtDateTime(b.created_at) })].filter(Boolean).join(' · ')),
      h('div', { class: 'bucket-actions' },
        h('a', { class: 'btn small primary', href, 'aria-label': t('buckets.openLabel', { name: b.name }) }, icon('folder', 16), h('span', null, t('buckets.open')))));
  }

  function render() {
    if (destroyed) return;
    const max = account ? account.limits.max_buckets : null;
    const full = max != null && buckets.length >= max;
    newBtn.disabled = !account || full;
    limitNote.hidden = !full;
    limitNote.textContent = full ? t('buckets.limitReached', { max: fmtInt(max) }) : '';
    summary.textContent = account
      ? [t('buckets.countOf', { n: fmtInt(buckets.length), max: fmtInt(max) }), t('buckets.totalUsed', { size: fmtBytes(account.usage.bytes_used) })].join(' · ')
      : '';
    if (!buckets.length) {
      setChildren(body, h('div', { class: 'empty' }, icon('bucket', 32), h('h2', null, t('buckets.emptyTitle')), h('p', { class: 'muted' }, t('buckets.emptyText')),
        h('button', { type: 'button', class: 'btn primary', disabled: newBtn.disabled, onclick: () => newBtn.click() }, icon('plus'), h('span', null, t('buckets.new')))));
      return;
    }
    setChildren(grid, ...buckets.map(card));
    setChildren(body, grid);
  }

  async function load() {
    try {
      [account, buckets] = await Promise.all([ctx.api(''), ctx.api('/buckets')]);
      render();
    } catch (e) {
      if (destroyed || (e instanceof ApiError && e.status === 401)) return;
      setChildren(body, errorBox(e));
    }
  }
  load();
  return () => { destroyed = true; };
}

// ---------------------------------------------------------------------------
// Chiavi API
// ---------------------------------------------------------------------------

/** How to use a key: headers, curl examples, reference. */
function keyHelp() {
  const origin = location.origin;
  const examples = [
    `# ${t('keys.helpUpload')}`,
    `curl -H "AccessKey: $KEY" -T video.mp4 ${origin}/storage/<bucket>/videos/video.mp4`,
    `# ${t('keys.helpList')}`,
    `curl -H "AccessKey: $KEY" ${origin}/storage/<bucket>/videos/`,
    `# ${t('keys.helpNoEncode')}`,
    `curl -H "AccessKey: $KEY" -H "X-Rms-Encode: none" -T notes.txt ${origin}/storage/<bucket>/docs/notes.txt`,
  ].join('\n');
  return h('section', { class: 'panel', 'aria-labelledby': 'keys-help' },
    h('div', { class: 'panel-head' }, h('h2', { id: 'keys-help' }, icon('info'), t('keys.helpTitle'))),
    h('div', { class: 'stack' },
      h('p', null, t('keys.helpLead')),
      h('ul', { class: 'help-list' },
        h('li', null, h('code', null, 'AccessKey: rms_…'), ' ', t('keys.helpHeader')),
        h('li', null, h('code', null, 'Authorization: Bearer rms_…'), ' ', t('keys.helpBearer')),
        h('li', null, h('code', null, 'X-Rms-Encode'), ' / ', h('code', null, 'X-Rms-Visibility'), ' ', t('keys.helpHeaders'))),
      h('div', { class: 'code-wrap' }, h('pre', null, h('code', null, examples)), copyButton(() => examples, t('links.copyCode'), { showLabel: true, cls: 'btn small' })),
      h('p', { class: 'hint' }, t('keys.helpScopes')),
      h('p', { class: 'hint' }, t('keys.helpDocs'), ' ', h('code', null, 'docs/API.md §9'), '.')));
}

export function mountKeys(root, { ctx, onRevokedSelf }) {
  document.title = t('keys.docTitle');
  let destroyed = false;
  let keys = [];
  let buckets = [];
  const summary = h('p', { class: 'muted', 'aria-live': 'polite' });
  const body = h('div', null, loading());
  const newBtn = h('button', { type: 'button', class: 'btn primary', onclick: () => keyDialog({ ctx, buckets, onSaved: load }) }, icon('plus'), h('span', null, t('keys.new')));
  setChildren(root, h('div', { class: 'page' },
    h('div', { class: 'page-head' }, h('div', null, h('h1', null, t('keys.title')), summary), newBtn),
    h('section', { class: 'panel', 'aria-labelledby': 'keys-list' },
      h('div', { class: 'panel-head' }, h('h2', { id: 'keys-list' }, icon('key'), t('keys.listTitle'))),
      h('p', { class: 'hint lead' }, t('keys.lead')),
      body),
    keyHelp()));

  function render() {
    if (destroyed) return;
    const active = keys.filter((k) => k.state === 'active').length;
    summary.textContent = [tn('keys.count', keys.length, { n: fmtInt(keys.length) }), t('keys.countActive', { n: fmtInt(active) })].join(' · ');
    setChildren(body, keyList({ ctx, keys, buckets, onChange: load, onRevokedSelf }));
  }
  async function load() {
    try {
      [keys, buckets] = await Promise.all([ctx.api('/keys'), ctx.api('/buckets')]);
      render();
    } catch (e) {
      if (destroyed || (e instanceof ApiError && e.status === 401)) return;
      setChildren(body, errorBox(e));
    }
  }
  load();
  return () => { destroyed = true; };
}

// ---------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------

export function mountAccount(root, { ctx, onLogout }) {
  document.title = t('account.docTitle');
  let destroyed = false;
  setChildren(root, loading());
  const kv = (pairs) => h('dl', { class: 'kv' }, ...pairs.filter(Boolean).map(([k, v]) => [h('dt', null, k), h('dd', null, v)]));
  const section = (id, iconName, title, ...children) => h('section', { class: 'panel', 'aria-labelledby': id },
    h('div', { class: 'panel-head' }, h('h2', { id }, icon(iconName), title)), ...children);

  (async () => {
    try {
      const [acct, keys] = await Promise.all([ctx.api(''), ctx.api('/keys').catch(() => [])]);
      if (destroyed) return;
      const u = acct.user;
      const l = acct.limits;
      const cur = acct.key ? keys.find((k) => k.id === acct.key.id) : null;
      setChildren(root, h('div', { class: 'page' },
        h('div', { class: 'page-head' }, h('div', null, h('h1', null, t('account.title')), h('p', { class: 'muted' }, t('account.lead')))),
        h('div', { class: 'grid-2' },
          section('acc-profile', 'user', t('account.profile'), kv([
            [t('users.username'), h('code', null, u.username)],
            [t('users.displayName'), u.display_name || '—'],
            [t('users.email'), u.email || '—'],
            [t('common.status'), u.status === 'suspended' ? t('users.status.suspended') : t('users.status.active')],
            [t('account.memberSince'), fmtDateTime(u.created_at)],
            [t('account.lastLogin'), u.last_login_at ? fmtDateTime(u.last_login_at) : '—'],
          ]), h('p', { class: 'hint' }, t('account.profileHint'))),
          section('acc-key', 'key', t('account.keyTitle'),
            acct.key ? kv([
              [t('keys.name'), cur ? cur.name : '—'],
              [t('keys.prefix'), h('code', null, cur ? `${cur.key_prefix}…` : '—')],
              [t('keys.scopes'), scopeChips(acct.key.scopes)],
              [t('keys.expiry'), cur && cur.expires_at ? fmtDateTime(cur.expires_at) : t('keys.never')],
            ]) : h('p', { class: 'muted' }, t('account.adminView')),
            h('div', { class: 'row-between' }, h('a', { href: '#/keys' }, t('account.manageKeys')),
              iconButton('logout', t('logout.label'), onLogout, { cls: 'btn' })))),
        section('acc-usage', 'chart', t('account.usage'),
          usageTiles(acct.usage, { quota: acct.usage.quota_bytes, maxBuckets: l.max_buckets })),
        section('acc-limits', 'sliders', t('account.limits'), kv([
          [t('users.quota'), l.quota_bytes ? fmtBytes(l.quota_bytes) : t('users.quotaUnlimited')],
          [t('users.maxBuckets'), fmtInt(l.max_buckets)],
          [t('account.maxObject'), fmtBytes(l.max_object_mb * 1024 * 1024)],
          [t('account.ladders'), h('span', { class: 'chips' }, ...l.ladders.map((n) => h('span', { class: 'chip-tag' }, n === (u.default_ladder || l.default_ladder) ? t('vod.ladderDefault', { name: n }) : n)))],
          [t('account.vod'), l.vod_available ? t('account.vodOn') : t('account.vodOff')],
        ]), h('p', { class: 'hint' }, t('account.limitsHint')))));
    } catch (e) {
      if (destroyed || (e instanceof ApiError && e.status === 401)) return;
      setChildren(root, errorBox(e));
    }
  })();
  return () => { destroyed = true; };
}
