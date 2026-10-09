// rms public video catalogue (GET /vod): the ready videos on demand from
// GET /api/v1/public/vod, newest first, linking to /watch/{id}. Language and theme as the
// player pages: ?lang= / ?theme= win over the server settings (<html lang data-theme>).
import { t, tn, setLanguage, normalizeLanguage, translateDom, formatDate } from './i18n.js';
import { initTheme, normalizeTheme } from './theme.js';

const params = new URLSearchParams(location.search);
setLanguage(normalizeLanguage(params.get('lang')) || document.documentElement.lang);
translateDom(document);
initTheme({ override: normalizeTheme(params.get('theme')) });
document.title = t('catalog.docTitle');

/** Video length as a clock: 0:30, 12:05, 1:02:03. */
function clock(secs) {
  let s = Math.max(0, Math.round(Number(secs) || 0));
  const hh = Math.floor(s / 3600); s -= hh * 3600;
  const mm = Math.floor(s / 60); s -= mm * 60;
  const pad = (v) => String(v).padStart(2, '0');
  return hh ? `${hh}:${pad(mm)}:${pad(s)}` : `${mm}:${pad(s)}`;
}

const grid = document.getElementById('catalog-grid');
const empty = document.getElementById('catalog-empty');
const error = document.getElementById('catalog-error');
const count = document.getElementById('catalog-count');

// Keep ?lang= / ?theme= on the links to the watch pages.
const keep = new URLSearchParams();
for (const k of ['lang', 'theme']) if (params.get(k)) keep.set(k, params.get(k));
const suffix = keep.toString() ? '?' + keep : '';

function card(v) {
  const a = document.createElement('a');
  a.className = 'catalog-card';
  a.setAttribute('role', 'listitem');
  a.href = '/watch/' + encodeURIComponent(v.id) + suffix;
  a.setAttribute('aria-label', t('catalog.watch', { title: v.title }));
  const thumb = document.createElement('div');
  thumb.className = 'catalog-thumb';
  if (v.poster_url) {
    const img = document.createElement('img');
    img.src = v.poster_url;
    img.alt = '';
    img.loading = 'lazy';
    img.decoding = 'async';
    thumb.append(img);
  }
  if (v.duration_secs != null) {
    const d = document.createElement('span');
    d.className = 'catalog-duration';
    d.textContent = clock(v.duration_secs);
    thumb.append(d);
  }
  const title = document.createElement('h2');
  title.className = 'catalog-title';
  title.textContent = v.title;
  const meta = document.createElement('p');
  meta.className = 'catalog-meta';
  meta.textContent = v.ready_at ? formatDate(v.ready_at, { dateStyle: 'medium' }) : '';
  a.append(thumb, title, meta);
  return a;
}

fetch('/api/v1/public/vod', { cache: 'no-store' })
  .then((res) => { if (!res.ok) throw new Error('HTTP ' + res.status); return res.json(); })
  .then((list) => {
    grid.replaceChildren(...list.map(card));
    grid.setAttribute('aria-busy', 'false');
    empty.hidden = list.length > 0;
    grid.hidden = list.length === 0;
    count.textContent = list.length ? tn('catalog.count', list.length) : t('catalog.lead');
  })
  .catch((e) => {
    grid.setAttribute('aria-busy', 'false');
    grid.hidden = true;
    error.textContent = t('catalog.loadError') + ' (' + (e && e.message ? e.message : e) + ')';
    error.hidden = false;
  });
