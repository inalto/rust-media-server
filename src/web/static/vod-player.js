// rms video on demand player (ARCHITECTURE.md §19): the VOD mode of player.js, used by
// /embed/vod/{id} (iframe, data-mode="embed") and /watch/{id} (full page with title and
// description, data-mode="watch"), both rendered from vod.html.
//
// - Info from GET /api/v1/public/vod/{id}: a video that is not ready yet shows a
//   "processing" card (with its progress) and is polled every 5 s until it is ready.
// - Engines: native HLS on iOS/Safari, else hls.js with adaptive bitrate (startLevel -1,
//   capLevelToPlayerSize), else dash.js; on a fatal error the next one is tried
//   (src=hls: hls → dash, src=dash: dash only). Libraries are loaded from the CDN on demand.
// - Quality menu (overlay button): Auto + each rendition (hls.js levels, dash.js video
//   representations); native HLS has no quality API, so the menu is hidden there.
// - Query: autoplay=1|0 (0), muted=1|0 (= autoplay), controls=1|0 (1), t=<seconds> start
//   position, src=auto|hls|dash, lang=it|en, theme=system|light|dark.
// - Debug handle: window.rmsPlayer = { kind: 'vod', status, source, engine, hls, dash,
//   levels, currentLevel, setLevel(i), info, language, theme, options, reload() }.
import { t, setLanguage, getLanguage, normalizeLanguage, translateDom, formatNumber } from './i18n.js';
import { initTheme, normalizeTheme, setServerTheme, setThemeOverride, resolvedTheme } from './theme.js';

const LIBS = {
  hls: { url: 'https://cdn.jsdelivr.net/npm/hls.js@1.7.3/dist/hls.min.js', global: 'Hls' },
  dash: { url: 'https://cdn.jsdelivr.net/npm/dashjs@5.2.1/dist/modern/umd/dash.all.min.js', global: 'dashjs' },
};
const ORDER = ['hls', 'dash'];
const POLL_MS = 5000;
const START_TIMEOUT_MS = 20000;

export function startVodPlayer() {
  const body = document.body;
  const id = body.dataset.id;
  const mode = body.dataset.mode === 'watch' ? 'watch' : 'embed';
  const params = new URLSearchParams(location.search);
  const flag = (k, def) => {
    if (!params.has(k)) return def;
    const v = (params.get(k) || '').toLowerCase();
    return !(v === '0' || v === 'false' || v === 'no' || v === 'off');
  };
  const normalizeSrc = (s) => { s = (s || 'auto').toLowerCase(); return ORDER.includes(s) ? s : 'auto'; };
  const autoplay = flag('autoplay', false);
  const start = Math.max(0, parseFloat(params.get('t') || '0') || 0);
  const opts = { autoplay, muted: flag('muted', autoplay), controls: flag('controls', true), src: normalizeSrc(params.get('src')), t: start };

  const langOverride = normalizeLanguage(params.get('lang'));
  setLanguage(langOverride || document.documentElement.lang);
  translateDom(document);
  initTheme({ override: normalizeTheme(params.get('theme')) });
  window.addEventListener('message', (ev) => {
    const d = ev.data;
    if (ev.origin !== location.origin || !d || d.type !== 'rms-theme') return;
    if (normalizeTheme(d.theme)) setThemeOverride(d.theme);
  });

  const $ = (x) => document.getElementById(x);
  const stage = $('stage');
  const video = $('video');
  const notice = $('notice');
  const noticeTitle = $('notice-title');
  const noticeText = $('notice-text');
  const noticeProgress = $('notice-progress');
  const noticeRetry = $('notice-retry');
  const spinner = $('spinner');
  const qualityBtn = $('quality-btn');
  const qualityLabel = $('quality-label');
  const qualityMenu = $('quality-menu');
  const unmuteBtn = $('unmute');
  const playBtn = $('play');
  const srStatus = $('sr-status');
  const titleEl = $('title');
  const descEl = $('description');
  const catalogLink = $('catalog-link');

  video.controls = opts.controls;
  video.muted = opts.muted;
  video.defaultMuted = opts.muted;
  video.preload = 'metadata';

  let info = null;
  let status = 'loading'; // loading | queued | processing | ready | failed | notfound | error
  let engine = null;      // { source, kind, destroy(), levels(), current(), auto(), setLevel(i) }
  let queue = [];
  let generation = 0;
  let pollTimer = null;
  let startTimer = null;
  let started = false;
  let menuOpen = false;

  const log = (...a) => { if (window.console && console.info) console.info('[rms vod]', ...a); };
  const announce = (text) => { srStatus.textContent = text; };
  const showSpinner = (on) => { spinner.hidden = !on; };
  const isIOS = () => /iPad|iPhone|iPod/.test(navigator.userAgent) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
  const isSafari = () => /^((?!chrome|chromium|crios|fxios|edg|android).)*safari/i.test(navigator.userAgent);
  const mseAvailable = () => !!(window.MediaSource || window.ManagedMediaSource || window.WebKitMediaSource);
  const nativeHls = () => !!video.canPlayType('application/vnd.apple.mpegurl');

  if (catalogLink) {
    const q = new URLSearchParams();
    for (const k of ['lang', 'theme']) if (params.get(k)) q.set(k, params.get(k));
    catalogLink.href = '/vod' + (q.toString() ? '?' + q : '');
  }

  const libPromises = {};
  function loadLib(key) {
    const lib = LIBS[key];
    if (window[lib.global]) return Promise.resolve(window[lib.global]);
    if (libPromises[key]) return libPromises[key];
    libPromises[key] = new Promise((resolve, reject) => {
      const s = document.createElement('script');
      s.src = lib.url;
      s.async = true;
      s.crossOrigin = 'anonymous';
      s.onload = () => (window[lib.global] ? resolve(window[lib.global]) : reject(new Error(key + ' not defined')));
      s.onerror = () => { delete libPromises[key]; s.remove(); reject(new Error('cannot load ' + lib.url)); };
      document.head.appendChild(s);
    });
    return libPromises[key];
  }

  // ---------------------------------------------------------------- texts

  function renderTexts() {
    const title = info && info.title;
    document.title = title ? t('vodplayer.docTitle', { title }) : t('vodplayer.defaultTitle');
    if (titleEl) titleEl.textContent = title || t('vodplayer.defaultTitle');
    if (descEl) {
      descEl.textContent = (info && info.description) || '';
      descEl.hidden = !(info && info.description);
    }
    if (status !== 'ready' && status !== 'loading') showNotice();
    renderQuality();
  }

  function showNotice() {
    stage.dataset.state = status;
    showSpinner(false);
    let title; let text; let progress = null; let retry = false;
    if (status === 'notfound') { title = t('vodplayer.notFound'); text = t('vodplayer.notFoundText'); }
    else if (status === 'failed') { title = t('vodplayer.unavailable'); text = t('vodplayer.unavailableText'); }
    else if (status === 'error') { title = t('vodplayer.error'); text = t('vodplayer.errorText'); retry = true; }
    else {
      title = status === 'queued' ? t('vodplayer.queued') : t('vodplayer.processing');
      text = t('vodplayer.processingText');
      progress = info ? info.progress || 0 : 0;
    }
    noticeTitle.textContent = title;
    noticeText.textContent = text;
    noticeProgress.hidden = progress == null;
    if (progress != null) {
      noticeProgress.querySelector('progress').value = progress;
      noticeProgress.querySelector('span').textContent = t('vodplayer.progress', { pct: formatNumber(progress) });
    }
    noticeRetry.hidden = !retry;
    notice.hidden = false;
    video.hidden = true;
    qualityBtn.hidden = true;
    closeMenu();
    announce(title);
  }

  function hideNotice() {
    notice.hidden = true;
    video.hidden = false;
  }

  // ---------------------------------------------------------------- engines

  function bindStart(apply) {
    if (!(opts.t > 0)) return;
    const once = () => { video.removeEventListener('loadedmetadata', once); apply(); };
    video.addEventListener('loadedmetadata', once);
  }

  function tryPlay() {
    if (!opts.autoplay) return;
    const p = video.play();
    if (p && p.catch) {
      p.catch((err) => {
        if (err && err.name === 'NotAllowedError' && !video.muted) {
          video.muted = true;
          const p2 = video.play();
          if (p2 && p2.catch) p2.catch(() => {});
        }
        updateButtons();
      });
    }
  }

  const shortSide = (w, h) => Math.min(w || 0, h || 0) || h || w || 0;
  const levelLabel = (w, h) => (shortSide(w, h) ? `${shortSide(w, h)}p` : '?');

  function createNative(gen) {
    const onError = () => fatal(gen, 'native: media error ' + (video.error ? video.error.code : ''));
    video.addEventListener('error', onError);
    bindStart(() => { video.currentTime = Math.min(opts.t, (video.duration || opts.t) - 0.5); });
    video.src = info.sources.hls;
    tryPlay();
    return Promise.resolve({
      source: 'hls', kind: 'native',
      destroy() { video.removeEventListener('error', onError); video.removeAttribute('src'); try { video.load(); } catch (_) { /* ignore */ } },
      levels: () => [], current: () => null, auto: () => true, setLevel() {},
    });
  }

  function createHlsJs(gen) {
    return loadLib('hls').then((Hls) => {
      if (!Hls.isSupported()) {
        if (nativeHls()) return createNative(gen);
        throw new Error('hls.js not supported');
      }
      const hls = new Hls({ startLevel: -1, capLevelToPlayerSize: true, startPosition: opts.t > 0 ? opts.t : -1 });
      let recovered = false;
      hls.on(Hls.Events.ERROR, (_e, data) => {
        if (!data || !data.fatal) return;
        if (data.type === Hls.ErrorTypes.MEDIA_ERROR && !recovered) { recovered = true; hls.recoverMediaError(); return; }
        fatal(gen, 'hls.js: ' + data.type + ' / ' + data.details);
      });
      hls.on(Hls.Events.MANIFEST_PARSED, () => { renderQuality(); tryPlay(); });
      hls.on(Hls.Events.LEVEL_SWITCHED, renderQuality);
      hls.loadSource(info.sources.hls);
      hls.attachMedia(video);
      return {
        source: 'hls', kind: 'hls.js', hls,
        destroy() { try { hls.destroy(); } catch (_) { /* ignore */ } video.removeAttribute('src'); },
        levels: () => hls.levels.map((l, i) => ({ index: i, width: l.width, height: l.height, bitrate: l.bitrate })),
        current: () => (hls.currentLevel >= 0 ? hls.currentLevel : null),
        auto: () => hls.autoLevelEnabled,
        setLevel(i) { hls.currentLevel = i; },
      };
    });
  }

  function createDash(gen) {
    return loadLib('dash').then((dashjs) => {
      if (typeof dashjs.supportsMediaSource === 'function' && !dashjs.supportsMediaSource()) throw new Error('dash.js: MSE not supported');
      const player = dashjs.MediaPlayer().create();
      player.updateSettings({ streaming: { abr: { autoSwitchBitrate: { video: true } } } });
      const ev = dashjs.MediaPlayer.events;
      player.on(ev.ERROR, (e) => { const err = e && e.error; fatal(gen, 'dash.js: ' + ((err && (err.message || err.code)) || 'error')); });
      player.on(ev.STREAM_INITIALIZED, () => {
        if (opts.t > 0) { try { player.seek(opts.t); } catch (_) { /* ignore */ } }
        renderQuality();
      });
      player.on(ev.QUALITY_CHANGE_RENDERED, renderQuality);
      player.initialize(video, info.sources.dash, opts.autoplay);
      if (opts.autoplay) tryPlay();
      const reps = () => { try { return player.getRepresentationsByType('video') || []; } catch (_) { return []; } };
      let manual = false;
      return {
        source: 'dash', kind: 'dash.js', dash: player,
        destroy() { try { player.reset(); } catch (_) { /* ignore */ } },
        levels: () => reps().map((r, i) => ({ index: i, width: r.width, height: r.height, bitrate: r.bandwidth, id: r.id })),
        current: () => {
          let cur = null;
          try { cur = player.getCurrentRepresentationForType('video'); } catch (_) { /* ignore */ }
          if (!cur) return null;
          const i = reps().findIndex((r) => r.id === cur.id);
          return i >= 0 ? i : null;
        },
        auto: () => !manual,
        setLevel(i) {
          const list = reps();
          if (i < 0 || !list[i]) {
            manual = false;
            player.updateSettings({ streaming: { abr: { autoSwitchBitrate: { video: true } } } });
          } else {
            manual = true;
            player.updateSettings({ streaming: { abr: { autoSwitchBitrate: { video: false } } } });
            player.setRepresentationForTypeById('video', list[i].id, true);
          }
          renderQuality();
        },
      };
    });
  }

  function createEngine(source, gen) {
    if (source === 'hls') {
      if (nativeHls() && (isIOS() || isSafari())) return createNative(gen);
      if (mseAvailable()) return createHlsJs(gen);
      if (nativeHls()) return createNative(gen);
      return Promise.reject(new Error('HLS not supported'));
    }
    if (source === 'dash') return mseAvailable() ? createDash(gen) : Promise.reject(new Error('MSE not supported'));
    return Promise.reject(new Error('unknown source ' + source));
  }

  function sourceOrder() {
    const s = (info && info.sources) || {};
    const available = { hls: !!s.hls, dash: !!s.dash };
    const order = opts.src === 'auto' ? ORDER.slice() : ORDER.slice(ORDER.indexOf(opts.src));
    return order.filter((k) => available[k]);
  }

  function destroyEngine() {
    clearTimeout(startTimer);
    if (engine) { const e = engine; engine = null; e.destroy(); }
    started = false;
    qualityBtn.hidden = true;
    closeMenu();
  }

  function startPlayback() {
    generation++;
    destroyEngine();
    hideNotice();
    stage.dataset.state = 'ready';
    if (info.poster_url) video.poster = info.poster_url; else video.removeAttribute('poster');
    queue = sourceOrder();
    tryNext();
  }

  function tryNext() {
    destroyEngine();
    const gen = ++generation;
    const source = queue.shift();
    if (!source) { status = 'error'; showNotice(); return; }
    log('trying', source);
    if (opts.autoplay) showSpinner(true);
    createEngine(source, gen).then((e) => {
      if (gen !== generation) { e.destroy(); return; }
      engine = e;
      renderQuality();
      updateButtons();
      if (opts.autoplay) startTimer = setTimeout(() => { if (!started) fatal(gen, source + ': no media within ' + START_TIMEOUT_MS / 1000 + ' s'); }, START_TIMEOUT_MS);
    }, (err) => { if (gen === generation) fatal(gen, source + ': ' + (err && err.message ? err.message : err)); });
  }

  function fatal(gen, reason) {
    if (gen !== generation) return;
    log('source failed:', reason);
    tryNext();
  }

  video.addEventListener('playing', () => {
    clearTimeout(startTimer);
    showSpinner(false);
    if (!started) { started = true; announce(t('vodplayer.playing')); }
    updateButtons();
  });
  video.addEventListener('loadeddata', () => { showSpinner(false); renderQuality(); });
  video.addEventListener('waiting', () => { if (started) showSpinner(true); });
  video.addEventListener('timeupdate', () => showSpinner(false));
  video.addEventListener('volumechange', updateButtons);
  video.addEventListener('pause', updateButtons);
  video.addEventListener('play', updateButtons);
  window.addEventListener('resize', () => { if (menuOpen) closeMenu(); });

  function updateButtons() {
    unmuteBtn.hidden = !(engine && started && video.muted);
    playBtn.hidden = !(engine && !opts.controls && video.paused);
  }
  unmuteBtn.addEventListener('click', () => {
    video.muted = false;
    if (video.volume === 0) video.volume = 1;
    const p = video.play(); if (p && p.catch) p.catch(() => {});
    updateButtons();
  });
  playBtn.addEventListener('click', () => { const p = video.play(); if (p && p.catch) p.catch(() => {}); });
  noticeRetry.addEventListener('click', () => { if (info && info.status === 'ready') { status = 'ready'; startPlayback(); } else refresh(); });

  // ---------------------------------------------------------------- quality menu

  function levelsSorted() {
    if (!engine) return [];
    return engine.levels().slice().sort((a, b) => shortSide(b.width, b.height) - shortSide(a.width, a.height) || b.bitrate - a.bitrate);
  }

  function renderQuality() {
    const levels = levelsSorted();
    qualityBtn.hidden = !(engine && levels.length > 1);
    if (qualityBtn.hidden) { closeMenu(); return; }
    const cur = engine.current();
    const curLevel = cur == null ? null : engine.levels()[cur];
    const curText = curLevel ? levelLabel(curLevel.width, curLevel.height) : null;
    const label = engine.auto() ? (curText ? t('vodplayer.qualityAutoCurrent', { quality: curText }) : t('vodplayer.qualityAuto')) : (curText || t('vodplayer.qualityAuto'));
    qualityLabel.textContent = label;
    qualityBtn.setAttribute('aria-label', t('vodplayer.qualityLabel', { quality: label }));
    qualityBtn.title = t('vodplayer.quality');
    if (menuOpen) buildMenu();
  }

  function buildMenu() {
    const levels = levelsSorted();
    const auto = engine.auto();
    const cur = engine.current();
    const item = (text, sub, checked, onPick) => {
      const b = document.createElement('button');
      b.type = 'button';
      b.className = 'menu-item';
      b.setAttribute('role', 'menuitemradio');
      b.setAttribute('aria-checked', String(checked));
      const main = document.createElement('span');
      main.textContent = text;
      b.append(main);
      if (sub) { const s = document.createElement('span'); s.className = 'menu-sub'; s.textContent = sub; b.append(s); }
      b.addEventListener('click', () => { onPick(); closeMenu(); qualityBtn.focus(); });
      return b;
    };
    const items = [item(t('vodplayer.qualityAuto'), null, auto, () => engine.setLevel(-1))];
    for (const l of levels) {
      const mbps = l.bitrate ? formatNumber(l.bitrate / 1e6, { maximumFractionDigits: 1, minimumFractionDigits: 1 }) + ' Mbit/s' : null;
      items.push(item(levelLabel(l.width, l.height), mbps, !auto && cur === l.index, () => engine.setLevel(l.index)));
    }
    qualityMenu.replaceChildren(...items);
  }

  function openMenu() {
    if (!engine) return;
    menuOpen = true;
    buildMenu();
    qualityMenu.hidden = false;
    qualityBtn.setAttribute('aria-expanded', 'true');
    const checked = qualityMenu.querySelector('[aria-checked="true"]') || qualityMenu.querySelector('button');
    if (checked) checked.focus();
  }
  function closeMenu() {
    menuOpen = false;
    qualityMenu.hidden = true;
    qualityBtn.setAttribute('aria-expanded', 'false');
  }
  qualityBtn.addEventListener('click', (e) => { e.stopPropagation(); if (menuOpen) closeMenu(); else openMenu(); });
  qualityMenu.addEventListener('keydown', (e) => {
    const items = [...qualityMenu.querySelectorAll('button')];
    const i = items.indexOf(document.activeElement);
    if (e.key === 'Escape') { e.preventDefault(); closeMenu(); qualityBtn.focus(); }
    else if (e.key === 'ArrowDown') { e.preventDefault(); items[(i + 1) % items.length].focus(); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); items[(i - 1 + items.length) % items.length].focus(); }
  });
  document.addEventListener('click', (e) => { if (menuOpen && !qualityMenu.contains(e.target)) closeMenu(); });

  // ---------------------------------------------------------------- info

  function refresh() {
    return fetch('/api/v1/public/vod/' + encodeURIComponent(id), { cache: 'no-store' }).then((res) => {
      if (res.status === 404) return { notfound: true };
      if (!res.ok) throw new Error('HTTP ' + res.status);
      return res.json();
    }).then((data) => {
      if (data.notfound) {
        info = null;
        status = 'notfound';
        destroyEngine();
        renderTexts();
        showNotice();
        return;
      }
      const prev = status;
      info = data;
      if (!langOverride && data.ui_language && setLanguage(data.ui_language)) translateDom(document);
      if (data.ui_theme) setServerTheme(data.ui_theme);
      status = data.status;
      renderTexts();
      if (status === 'ready') {
        clearInterval(pollTimer);
        pollTimer = null;
        if (prev !== 'ready') startPlayback();
      } else {
        destroyEngine();
        showNotice();
        if (status !== 'failed' && !pollTimer) pollTimer = setInterval(refresh, POLL_MS);
      }
    }).catch((e) => {
      log('info failed', e);
      if (!info) { status = 'error'; showNotice(); }
    });
  }

  window.rmsPlayer = {
    kind: 'vod',
    get status() { return status; },
    get source() { return engine ? engine.source : null; },
    get engine() { return engine ? engine.kind : null; },
    get hls() { return engine && engine.hls ? engine.hls : null; },
    get dash() { return engine && engine.dash ? engine.dash : null; },
    get levels() { return engine ? engine.levels() : []; },
    get currentLevel() { return engine ? engine.current() : null; },
    get autoLevel() { return engine ? engine.auto() : true; },
    setLevel(i) { if (engine) engine.setLevel(i); },
    get info() { return info; },
    get language() { return getLanguage(); },
    get theme() { return resolvedTheme(); },
    get options() { return Object.assign({}, opts); },
    get mode() { return mode; },
    reload() { if (status === 'ready') startPlayback(); },
  };

  showSpinner(true);
  refresh().then(() => { if (status !== 'ready' || !opts.autoplay) showSpinner(false); });
}
