// rms embeddable player (ARCHITECTURE.md §9).
//
// - Status from GET /api/v1/public/broadcasts/{name} + SSE .../events (`status`),
//   with polling every 5 s while the SSE is down.
// - offline/paused → waiting screen: looped muted waiting video (poster) or a styled
//   placeholder with the title, the waiting text and a pulsing badge.
// - live → source selection (query `src=auto|flv|hls|dash`, default auto):
//   auto = HTTP-FLV with mpegts.js when supported and not iOS, else native HLS
//   (Safari/iOS), else hls.js, else dash.js. On fatal errors (or no progress) the next
//   source in the order flv → hls → dash is tried, then the waiting screen with a retry
//   after 5 s. Player libraries are loaded from the CDN only when needed.
// - Query: autoplay=1|0 (1), muted=1|0 (1), controls=1|0 (1), src, latency=1|0.
// - Debug handle: window.rmsPlayer = { status, source, engine, latency, setSource(), reload() }.
(function () {
  'use strict';

  var LIBS = {
    mpegts: { url: 'https://cdn.jsdelivr.net/npm/mpegts.js@1.8.2/dist/mpegts.js', global: 'mpegts' },
    hls: { url: 'https://cdn.jsdelivr.net/npm/hls.js@1.7.3/dist/hls.min.js', global: 'Hls' },
    dash: { url: 'https://cdn.jsdelivr.net/npm/dashjs@5.2.1/dist/modern/umd/dash.all.min.js', global: 'dashjs' },
  };
  var ORDER = ['flv', 'hls', 'dash'];
  var RETRY_MS = 5000;
  var POLL_MS = 5000;
  var START_TIMEOUT_MS = 15000;
  var STALL_MS = 12000;
  var BEHIND_EDGE_S = 5;
  var WAITING_REFRESH_MS = 60000;

  var body = document.body;
  var name = body.dataset.name;
  var mode = body.dataset.mode === 'player' ? 'player' : 'embed';
  var params = new URLSearchParams(location.search);
  function flag(k, def) {
    if (!params.has(k)) return def;
    var v = (params.get(k) || '').toLowerCase();
    return !(v === '0' || v === 'false' || v === 'no' || v === 'off');
  }
  function normalizeSrc(s) {
    s = (s || 'auto').toLowerCase();
    return ORDER.indexOf(s) >= 0 ? s : 'auto';
  }
  var opts = {
    autoplay: flag('autoplay', true),
    muted: flag('muted', true),
    controls: flag('controls', true),
    src: normalizeSrc(params.get('src')),
    latency: flag('latency', mode === 'player'),
  };

  var $ = function (id) { return document.getElementById(id); };
  var stage = $('stage');
  var video = $('video');
  var waitingVideo = $('waiting-video');
  var waiting = $('waiting');
  var waitingBadge = $('waiting-badge');
  var waitingBadgeText = $('waiting-badge-text');
  var waitingTitle = $('waiting-title');
  var waitingText = $('waiting-text');
  var spinner = $('spinner');
  var liveBadge = $('live-badge');
  var latencyEl = $('latency');
  var goLiveBtn = $('go-live');
  var unmuteBtn = $('unmute');
  var playBtn = $('play');
  var srStatus = $('sr-status');

  video.controls = opts.controls;
  video.muted = opts.muted;
  video.defaultMuted = opts.muted;
  video.autoplay = false;

  var info = null;          // public broadcast info
  var status = 'loading';   // loading | offline | paused | live | notfound
  var engine = null;        // { source, kind, destroy(), latency(), goLive() }
  var queue = [];           // sources still to try for the current live session
  var generation = 0;       // invalidates callbacks of torn-down attempts
  var retryTimer = null;
  var startTimer = null;
  var playing = false;
  var lastProgress = { t: 0, time: 0 };
  var waitingVideoFailed = null; // URL that failed to load
  var lastWaitingRefresh = 0;
  var autoplayBlocked = false;   // even muted autoplay was refused (e.g. low-power mode)

  function log() {
    if (window.console && console.info) console.info.apply(console, ['[rms]'].concat([].slice.call(arguments)));
  }

  // ------------------------------------------------------------------ env

  function isIOS() {
    return /iPad|iPhone|iPod/.test(navigator.userAgent) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
  }
  function isSafari() {
    return /^((?!chrome|chromium|crios|fxios|edg|android).)*safari/i.test(navigator.userAgent);
  }
  function mseAvailable() {
    return !!(window.MediaSource || window.ManagedMediaSource || window.WebKitMediaSource);
  }
  function nativeHls() {
    return !!video.canPlayType('application/vnd.apple.mpegurl');
  }

  var libPromises = {};
  function loadLib(key) {
    var lib = LIBS[key];
    if (window[lib.global]) return Promise.resolve(window[lib.global]);
    if (libPromises[key]) return libPromises[key];
    libPromises[key] = new Promise(function (resolve, reject) {
      var s = document.createElement('script');
      s.src = lib.url;
      s.async = true;
      s.crossOrigin = 'anonymous';
      s.onload = function () { window[lib.global] ? resolve(window[lib.global]) : reject(new Error(key + ' not defined')); };
      s.onerror = function () { delete libPromises[key]; s.remove(); reject(new Error('cannot load ' + lib.url)); };
      document.head.appendChild(s);
    });
    return libPromises[key];
  }

  // ------------------------------------------------------------------ UI

  function announce(text) { srStatus.textContent = text; }

  function setState(s) { stage.dataset.state = s; }

  function showSpinner(on) { spinner.hidden = !on; }

  function updateUnmute() {
    // Also shown when autoplay was blocked: a tap starts playback with sound.
    unmuteBtn.hidden = !(playing && (video.muted || autoplayBlocked));
    // Without native controls a paused player needs its own play button.
    playBtn.hidden = !(playing && video.paused && !opts.controls);
  }

  function showWaiting() {
    playing = false;
    setState(status === 'notfound' ? 'notfound' : 'waiting');
    showSpinner(false);
    liveBadge.hidden = true;
    latencyEl.hidden = true;
    goLiveBtn.hidden = true;
    unmuteBtn.hidden = true;
    playBtn.hidden = true;
    var paused = status === 'paused';
    var title = info ? info.title : '';
    waitingTitle.textContent = title;
    if (status === 'notfound') {
      waitingBadge.hidden = true;
      waitingTitle.textContent = 'Diretta non trovata';
      waitingText.textContent = 'Controlla il link: questa diretta non esiste o è stata eliminata.';
    } else {
      waitingBadge.hidden = false;
      waitingBadge.classList.toggle('paused', paused);
      waitingBadgeText.textContent = paused ? 'Diretta in pausa' : 'In attesa della diretta';
      waitingText.textContent = (info && info.waiting_text) || 'La diretta inizierà a breve';
    }
    var url = status !== 'notfound' && info && info.waiting_video_url;
    if (url && url !== waitingVideoFailed) {
      if (waitingVideo.getAttribute('src') !== url) {
        if (info.poster_url) waitingVideo.poster = info.poster_url; else waitingVideo.removeAttribute('poster');
        waitingVideo.src = url;
      }
      waitingVideo.hidden = false;
      var p = waitingVideo.play();
      if (p && p.catch) p.catch(function () { /* poster stays visible */ });
      waiting.classList.add('over-video');
    } else {
      waitingVideo.hidden = true;
      waitingVideo.pause();
      waiting.classList.remove('over-video');
    }
    waiting.hidden = false;
    announce(status === 'notfound' ? 'Diretta non trovata' : paused ? 'Diretta in pausa' : 'In attesa della diretta');
  }

  function hideWaiting() {
    waiting.hidden = true;
    waitingVideo.hidden = true;
    try { waitingVideo.pause(); } catch (_) { /* ignore */ }
  }

  waitingVideo.addEventListener('error', function () {
    waitingVideoFailed = waitingVideo.getAttribute('src');
    log('waiting video failed to load', waitingVideoFailed);
    if (status !== 'live' || !playing) showWaiting();
  });

  function onPlaying() {
    if (!engine) return;
    clearTimeout(startTimer);
    if (!playing) {
      playing = true;
      lastProgress = { t: Date.now(), time: video.currentTime };
      setState('live');
      hideWaiting();
      showSpinner(false);
      liveBadge.hidden = false;
      announce('Diretta in corso');
      log('playing', engine.source, 'via', engine.kind);
    }
    updateUnmute();
  }

  video.addEventListener('playing', onPlaying);
  // First frame decoded: reveal the player even if autoplay is blocked or disabled, so
  // the viewer can start it with the controls (or the "Attiva audio" button).
  video.addEventListener('loadeddata', function () { if (engine) onPlaying(); });
  video.addEventListener('volumechange', updateUnmute);
  video.addEventListener('play', function () { autoplayBlocked = false; updateUnmute(); });
  video.addEventListener('pause', updateUnmute);
  video.addEventListener('waiting', function () { if (playing) showSpinner(true); });
  video.addEventListener('timeupdate', function () { if (playing) showSpinner(false); });

  unmuteBtn.addEventListener('click', function () {
    video.muted = false;
    if (video.volume === 0) video.volume = 1;
    var p = video.play();
    if (p && p.catch) p.catch(function () {});
    updateUnmute();
  });
  playBtn.addEventListener('click', function () {
    if (engine && engine.goLive) engine.goLive();
    var p = video.play();
    if (p && p.catch) p.catch(function () {});
  });
  goLiveBtn.addEventListener('click', function () {
    if (engine && engine.goLive) engine.goLive();
    var p = video.play();
    if (p && p.catch) p.catch(function () {});
  });

  function tryPlay() {
    if (!opts.autoplay) return;
    var p = video.play();
    if (p && p.catch) {
      p.catch(function (err) {
        // Autoplay with sound blocked: fall back to muted and offer "Attiva audio".
        if (err && err.name === 'NotAllowedError' && !video.muted) {
          video.muted = true;
          var p2 = video.play();
          if (p2 && p2.catch) p2.catch(function (e2) { if (e2 && e2.name === 'NotAllowedError') autoplayBlocked = true; updateUnmute(); });
        } else if (err && err.name === 'NotAllowedError') {
          autoplayBlocked = true;
        }
        updateUnmute();
      });
    }
  }

  // ------------------------------------------------------------------ engines

  function bufferedEnd() {
    var b = video.buffered;
    return b && b.length ? b.end(b.length - 1) : NaN;
  }
  function seekableEnd() {
    var s = video.seekable;
    return s && s.length ? s.end(s.length - 1) : NaN;
  }

  function createFlv(gen) {
    return loadLib('mpegts').then(function (mpegts) {
      if (!mpegts.isSupported() || !(mpegts.getFeatureList().mseLivePlayback)) throw new Error('mpegts.js: MSE live playback not supported');
      var player = mpegts.createPlayer({ type: 'flv', isLive: true, url: info.sources.flv }, {
        enableWorker: false,
        enableStashBuffer: false,
        lazyLoad: false,
        autoCleanupSourceBuffer: true,
        liveBufferLatencyChasing: true,
        liveBufferLatencyMaxLatency: 1.5,
        liveBufferLatencyMinRemain: 0.5,
      });
      player.on(mpegts.Events.ERROR, function (type, detail) { fatal(gen, 'flv: ' + type + ' / ' + detail); });
      player.on(mpegts.Events.LOADING_COMPLETE, function () { fatal(gen, 'flv: stream ended'); });
      player.attachMediaElement(video);
      player.load();
      tryPlay();
      return {
        source: 'flv', kind: 'mpegts.js',
        destroy: function () {
          try { player.pause(); } catch (_) { /* ignore */ }
          try { player.unload(); } catch (_) { /* ignore */ }
          try { player.detachMediaElement(); } catch (_) { /* ignore */ }
          try { player.destroy(); } catch (_) { /* ignore */ }
        },
        latency: function () { return bufferedEnd() - video.currentTime; },
        goLive: function () { var e = bufferedEnd(); if (isFinite(e)) video.currentTime = Math.max(0, e - 0.5); },
      };
    });
  }

  function createNativeHls(gen) {
    var onError = function () { fatal(gen, 'native hls: media error ' + (video.error ? video.error.code : '')); };
    video.addEventListener('error', onError);
    video.src = info.sources.hls;
    tryPlay();
    return Promise.resolve({
      source: 'hls', kind: 'native',
      destroy: function () {
        video.removeEventListener('error', onError);
        video.removeAttribute('src');
        try { video.load(); } catch (_) { /* ignore */ }
      },
      latency: function () { return seekableEnd() - video.currentTime; },
      goLive: function () { var e = seekableEnd(); if (isFinite(e)) video.currentTime = Math.max(0, e - 1.5); },
    });
  }

  function createHlsJs(gen) {
    return loadLib('hls').then(function (Hls) {
      if (!Hls.isSupported()) {
        if (nativeHls()) return createNativeHls(gen);
        throw new Error('hls.js not supported');
      }
      var hls = new Hls({ lowLatencyMode: true, liveSyncDuration: 1.5, liveMaxLatencyDuration: 4, backBufferLength: 30 });
      var recovered = false;
      hls.on(Hls.Events.ERROR, function (_evt, data) {
        if (!data || !data.fatal) return;
        if (data.type === Hls.ErrorTypes.MEDIA_ERROR && !recovered) {
          recovered = true;
          log('hls.js: recovering media error', data.details);
          hls.recoverMediaError();
          return;
        }
        fatal(gen, 'hls.js: ' + data.type + ' / ' + data.details);
      });
      hls.on(Hls.Events.MANIFEST_PARSED, function () { tryPlay(); });
      hls.loadSource(info.sources.hls);
      hls.attachMedia(video);
      return {
        source: 'hls', kind: 'hls.js',
        destroy: function () { try { hls.destroy(); } catch (_) { /* ignore */ } video.removeAttribute('src'); },
        latency: function () { return typeof hls.latency === 'number' ? hls.latency : NaN; },
        goLive: function () { var p = hls.liveSyncPosition; if (typeof p === 'number' && isFinite(p)) video.currentTime = p; },
      };
    });
  }

  function createDash(gen) {
    return loadLib('dash').then(function (dashjs) {
      if (typeof dashjs.supportsMediaSource === 'function' && !dashjs.supportsMediaSource()) throw new Error('dash.js: MSE not supported');
      var player = dashjs.MediaPlayer().create();
      // dash.js 5 enables its low-latency mode from the MPD (availabilityTimeOffset /
      // ServiceDescription); `streaming.lowLatencyEnabled` no longer exists.
      player.updateSettings({ streaming: { delay: { liveDelay: 2 }, liveCatchup: { enabled: true } } });
      player.on(dashjs.MediaPlayer.events.ERROR, function (e) {
        var err = e && e.error;
        fatal(gen, 'dash.js: ' + (err && (err.message || err.code) || 'error'));
      });
      player.initialize(video, info.sources.dash, opts.autoplay);
      return {
        source: 'dash', kind: 'dash.js',
        destroy: function () { try { player.reset(); } catch (_) { /* ignore */ } },
        latency: function () { try { return player.getCurrentLiveLatency(); } catch (_) { return NaN; } },
        goLive: function () {
          try {
            if (typeof player.seekToOriginalLive === 'function') player.seekToOriginalLive();
            else player.seek(player.duration());
          } catch (_) { /* ignore */ }
        },
      };
    });
  }

  function createEngine(source, gen) {
    if (source === 'flv') return createFlv(gen);
    if (source === 'hls') {
      // Native HLS on iOS / Safari; hls.js elsewhere (Chrome's native HLS is not LL-HLS aware).
      if (nativeHls() && (isIOS() || isSafari())) return createNativeHls(gen);
      if (mseAvailable()) return createHlsJs(gen);
      if (nativeHls()) return createNativeHls(gen);
      return Promise.reject(new Error('HLS not supported'));
    }
    if (source === 'dash') return createDash(gen);
    return Promise.reject(new Error('unknown source ' + source));
  }

  /** Sources to try, in order, for the current info and options. */
  function sourceOrder() {
    var s = (info && info.sources) || {};
    var available = {
      flv: !!s.flv && mseAvailable(),
      hls: !!s.hls && (mseAvailable() || nativeHls()),
      dash: !!s.dash && mseAvailable(),
    };
    var order;
    if (opts.src !== 'auto') order = ORDER.slice(ORDER.indexOf(opts.src));
    else order = isIOS() ? ['hls', 'dash'] : ORDER.slice();
    return order.filter(function (k) { return available[k]; });
  }

  // ------------------------------------------------------------------ lifecycle

  function destroyEngine() {
    clearTimeout(startTimer);
    if (engine) {
      var e = engine;
      engine = null;
      e.destroy();
    }
    playing = false;
    liveBadge.hidden = true;
    latencyEl.hidden = true;
    goLiveBtn.hidden = true;
    unmuteBtn.hidden = true;
    playBtn.hidden = true;
  }

  function clearRetry() {
    clearTimeout(retryTimer);
    retryTimer = null;
  }

  function stopLive() {
    generation++;
    clearRetry();
    queue = [];
    destroyEngine();
  }

  function startLive() {
    stopLive();
    queue = sourceOrder();
    if (!queue.length) log('no playable source for this browser', info && info.sources);
    tryNext();
  }

  function tryNext() {
    destroyEngine();
    var gen = ++generation;
    var source = queue.shift();
    if (!source) {
      // Every source failed: back to the waiting screen, retry in 5 s.
      showWaiting();
      scheduleRetry();
      return;
    }
    log('trying source', source);
    if (waiting.hidden) showWaiting();
    showSpinner(true);
    createEngine(source, gen).then(function (e) {
      if (gen !== generation) { e.destroy(); return; }
      engine = e;
      startTimer = setTimeout(function () { fatal(gen, source + ': no media within ' + START_TIMEOUT_MS / 1000 + ' s'); }, START_TIMEOUT_MS);
    }, function (err) {
      if (gen === generation) fatal(gen, source + ': ' + (err && err.message ? err.message : err));
    });
  }

  /** A fatal error of attempt `gen`: re-check the status, then try the next source. */
  function fatal(gen, reason) {
    if (gen !== generation) return;
    log('source failed:', reason);
    generation++;
    var myGen = generation;
    destroyEngine();
    refreshInfo().then(function (i) {
      if (myGen !== generation) return;
      if (i && i.status !== 'live') { applyStatus(i.status, true); return; }
      tryNext();
    });
  }

  function scheduleRetry() {
    clearRetry();
    retryTimer = setTimeout(function () {
      retryTimer = null;
      refreshInfo().then(function (i) {
        if (!i) { scheduleRetry(); return; }
        if (i.status === 'live') startLive(); else applyStatus(i.status, true);
      });
    }, RETRY_MS);
  }

  function applyStatus(next, force) {
    var prev = status;
    status = next;
    if (next === 'live') {
      if (prev !== 'live' || force) {
        if (!engine && !retryTimer) startLive();
        else if (prev !== 'live') startLive();
      }
    } else {
      if (prev === 'live' || engine || retryTimer) stopLive();
      showWaiting();
    }
  }

  // ------------------------------------------------------------------ status

  function refreshInfo() {
    return fetch('/api/v1/public/broadcasts/' + encodeURIComponent(name), { cache: 'no-store' }).then(function (res) {
      if (res.status === 404) {
        info = null;
        status = 'notfound';
        stopLive();
        showWaiting();
        return null;
      }
      if (!res.ok) return null;
      return res.json().then(function (data) {
        info = data;
        document.title = data.title ? data.title + ' · Diretta' : 'Diretta';
        $('title').textContent = data.title || 'Diretta';
        lastWaitingRefresh = Date.now();
        return data;
      });
    }).catch(function () { return null; });
  }

  var es = null;
  var pollTimer = null;
  var sseBackoff = 1000;

  function startPolling() {
    if (pollTimer) return;
    pollTimer = setInterval(function () {
      refreshInfo().then(function (i) { if (i) applyStatus(i.status); });
    }, POLL_MS);
  }
  function stopPolling() { clearInterval(pollTimer); pollTimer = null; }

  function connectEvents() {
    if (!window.EventSource) { startPolling(); return; }
    es = new EventSource('/api/v1/public/broadcasts/' + encodeURIComponent(name) + '/events');
    es.addEventListener('open', function () { sseBackoff = 1000; stopPolling(); });
    es.addEventListener('status', function (ev) {
      var next;
      try { next = JSON.parse(ev.data).status; } catch (_) { return; }
      if (next === status && info) { applyStatus(next); return; }
      refreshInfo().then(function () { applyStatus(next); });
    });
    es.addEventListener('error', function () {
      startPolling();
      if (es && es.readyState === 2) { // CLOSED: the browser gave up, reconnect ourselves
        es.close();
        es = null;
        setTimeout(connectEvents, sseBackoff);
        sseBackoff = Math.min(sseBackoff * 2, 30000);
      }
    });
  }

  // Latency indicator, "Vai al live", stall detection.
  setInterval(function () {
    if (!engine || !playing) return;
    var l = engine.latency();
    var finite = typeof l === 'number' && isFinite(l) && l >= 0;
    latencyEl.hidden = !(opts.latency && finite);
    if (finite) latencyEl.textContent = (Math.round(l * 10) / 10).toFixed(1).replace('.', ',') + ' s';
    goLiveBtn.hidden = !(finite && l > BEHIND_EDGE_S);
    if (video.paused) { lastProgress = { t: Date.now(), time: video.currentTime }; return; }
    if (video.currentTime !== lastProgress.time) {
      lastProgress = { t: Date.now(), time: video.currentTime };
    } else if (Date.now() - lastProgress.t > STALL_MS) {
      fatal(generation, (engine ? engine.source : '?') + ': playback stalled');
    }
  }, 1000);

  // While waiting, refresh the info now and then (new waiting video / text).
  setInterval(function () {
    if ((status === 'offline' || status === 'paused') && Date.now() - lastWaitingRefresh > WAITING_REFRESH_MS) {
      refreshInfo().then(function (i) { if (i) applyStatus(i.status); if (i && i.status !== 'live') showWaiting(); });
    }
  }, 10000);

  window.rmsPlayer = {
    get status() { return status; },
    get source() { return engine ? engine.source : null; },
    get engine() { return engine ? engine.kind : null; },
    get playing() { return playing; },
    get latency() { var l = engine && playing ? engine.latency() : NaN; return typeof l === 'number' && isFinite(l) ? l : null; },
    get info() { return info; },
    get options() { return Object.assign({}, opts); },
    setSource: function (src) {
      opts.src = normalizeSrc(src);
      if (status === 'live') startLive();
      return opts.src;
    },
    reload: function () { if (status === 'live') startLive(); },
  };

  showSpinner(true);
  refreshInfo().then(function (i) {
    showSpinner(false);
    if (i) applyStatus(i.status);
    else if (status !== 'notfound') { status = 'offline'; showWaiting(); startPolling(); }
    connectEvents();
  });
})();
