// rms embeddable player (ARCHITECTURE.md §9).
//
// - Status from GET /api/v1/public/broadcasts/{name} + SSE .../events (`status`),
//   with polling every 5 s while the SSE is down.
// - offline/paused → waiting screen: looped muted waiting video (poster) or a styled
//   placeholder with the title, the waiting text and a pulsing badge.
// - live → source selection (query `src=auto|webrtc|flv|hls|dash`, default auto):
//   auto = WebRTC (WHEP, sub-second) when the server offers it, then HTTP-FLV with
//   mpegts.js when supported and not iOS, else native HLS (Safari/iOS), else hls.js,
//   else dash.js. On fatal errors (or no progress) the next source in the order
//   webrtc → flv → hls → dash is tried, then the waiting screen with a retry after 5 s.
//   WebRTC fails over when ICE does not connect within 6 s, the connection fails or
//   closes, the WHEP POST is refused, or no video frame arrives within 8 s; its
//   teardown closes the peer connection and DELETEs the WHEP session.
//   Player libraries are loaded from the CDN only when needed.
// - Query: autoplay=1|0 (1), muted=1|0 (1), controls=1|0 (1), src, latency=1|0,
//   lang=it|en (texts; default: `ui_language` from the public broadcast info, before
//   that the <html lang> set by the server), theme=system|light|dark (colours; default:
//   `ui_theme` from the public broadcast info, before that the <html data-theme> set by
//   the server; `system` follows prefers-color-scheme). The dashboard's preview iframe
//   also sends {type: 'rms-theme', theme} messages (same origin only).
// - Debug handle: window.rmsPlayer = { status, source, engine, latency, peerConnection, language, theme, setSource(), reload() }.
// - VOD mode: pages with <body data-kind="vod"> (/embed/vod/{id}, /watch/{id}, vod.html)
//   run the video on demand player of vod-player.js instead (ARCHITECTURE.md §19); it
//   forwards the page's token=&expires= (signed URLs of private videos, docs/ACCOUNTS.md
//   §5) to the info, manifest, segment and poster requests. Live streams have no tokens.
// ES module (loaded with type="module") only to import the shared translations and theme.
import { t, setLanguage, getLanguage, normalizeLanguage, translateDom, formatNumber, isBuiltinWaitingText } from './i18n.js';
import { initTheme, normalizeTheme, setServerTheme, setThemeOverride, resolvedTheme } from './theme.js';
import { startVodPlayer } from './vod-player.js';

(function () {
  'use strict';

  if (document.body.dataset.kind === 'vod') {
    startVodPlayer();
    return;
  }

  var LIBS = {
    mpegts: { url: 'https://cdn.jsdelivr.net/npm/mpegts.js@1.8.2/dist/mpegts.js', global: 'mpegts' },
    hls: { url: 'https://cdn.jsdelivr.net/npm/hls.js@1.7.3/dist/hls.min.js', global: 'Hls' },
    dash: { url: 'https://cdn.jsdelivr.net/npm/dashjs@5.2.1/dist/modern/umd/dash.all.min.js', global: 'dashjs' },
  };
  var ORDER = ['webrtc', 'flv', 'hls', 'dash'];
  var ICE_TIMEOUT_MS = 6000;
  var FIRST_FRAME_TIMEOUT_MS = 8000;
  var WEBRTC_NO_MEDIA_S = 4;
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
    if (s === 'whep') s = 'webrtc';
    return ORDER.indexOf(s) >= 0 ? s : 'auto';
  }
  var opts = {
    autoplay: flag('autoplay', true),
    muted: flag('muted', true),
    controls: flag('controls', true),
    src: normalizeSrc(params.get('src')),
    latency: flag('latency', mode === 'player'),
  };

  // Language: ?lang= wins; otherwise the server setting (<html lang> now, then
  // `ui_language` from every info refresh).
  var langOverride = normalizeLanguage(params.get('lang'));
  setLanguage(langOverride || document.documentElement.lang);
  translateDom(document);

  // Theme: ?theme= wins; otherwise the server setting (<html data-theme> now, then
  // `ui_theme` from every info refresh); `system` follows the OS (CSS media query).
  initTheme({ override: normalizeTheme(params.get('theme')) });
  window.addEventListener('message', function (ev) {
    var d = ev.data;
    if (ev.origin !== location.origin || !d || d.type !== 'rms-theme') return;
    if (normalizeTheme(d.theme)) setThemeOverride(d.theme);
  });

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
  var titleEl = $('title');

  function renderTitle() {
    var title = info && info.title;
    document.title = title ? t('player.docTitle', { title: title }) : t('player.defaultTitle');
    titleEl.textContent = title || t('player.defaultTitle');
  }
  renderTitle();

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
      waitingTitle.textContent = t('player.notFound');
      waitingText.textContent = t('player.notFoundText');
    } else {
      waitingBadge.hidden = false;
      waitingBadge.classList.toggle('paused', paused);
      waitingBadgeText.textContent = paused ? t('player.paused') : t('player.waiting');
      // The server's built-in text follows its language: show ours when ?lang= differs.
      var text = info && info.waiting_text;
      waitingText.textContent = !text || isBuiltinWaitingText(text) ? t('waiting.builtin') : text;
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
    announce(status === 'notfound' ? t('player.notFound') : paused ? t('player.paused') : t('player.waiting'));
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
      announce(t('player.playing'));
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

  /**
   * WebRTC via WHEP: recvonly video + audio transceivers, POST the offer (no need to
   * wait for local ICE candidates: the server is ICE-lite with public host candidates,
   * the browser's checks reach it), apply the answer, play the MediaStream.
   */
  function createWebRtc(gen) {
    if (!window.RTCPeerConnection || !window.MediaStream) return Promise.reject(new Error('WebRTC not supported'));
    var endpoint = info.sources.whep;
    var pc = new RTCPeerConnection();
    var stream = new MediaStream();
    var sessionUrl = null;
    var closed = false;
    var latency = NaN;
    var lastStats = null;
    var statsTimer = null;
    var lastPackets = -1;
    var silentSeconds = 0;
    pc.addTransceiver('video', { direction: 'recvonly' });
    pc.addTransceiver('audio', { direction: 'recvonly' });
    pc.ontrack = function (e) {
      if (stream.getTracks().indexOf(e.track) < 0) stream.addTrack(e.track);
      if (video.srcObject !== stream) {
        video.srcObject = stream;
        tryPlay();
      }
    };
    var iceTimer = setTimeout(function () {
      if (pc.iceConnectionState !== 'connected' && pc.iceConnectionState !== 'completed') fatal(gen, 'webrtc: ICE not connected within ' + ICE_TIMEOUT_MS / 1000 + ' s');
    }, ICE_TIMEOUT_MS);
    var frameTimer = setTimeout(function () {
      if (video.readyState < 2) fatal(gen, 'webrtc: no video frames within ' + FIRST_FRAME_TIMEOUT_MS / 1000 + ' s');
    }, FIRST_FRAME_TIMEOUT_MS);
    pc.oniceconnectionstatechange = function () {
      var st = pc.iceConnectionState;
      if (st === 'connected' || st === 'completed') clearTimeout(iceTimer);
      if (st === 'failed' || st === 'closed') fatal(gen, 'webrtc: ICE ' + st);
    };
    pc.onconnectionstatechange = function () {
      var st = pc.connectionState;
      if (st === 'failed' || st === 'closed') fatal(gen, 'webrtc: connection ' + st);
    };
    // Estimated delay: jitter buffer (last second) + half the round-trip time. Also
    // detects a session the server closed (stream ended, WebRTC disabled): a live
    // stream never stops sending video packets for seconds.
    function pollStats() {
      pc.getStats().then(function (report) {
        var jb = null, rtt = 0, packets = 0;
        report.forEach(function (r) {
          if (r.type === 'inbound-rtp' && r.kind === 'video') {
            jb = { delay: r.jitterBufferDelay || 0, count: r.jitterBufferEmittedCount || 0 };
            packets = r.packetsReceived || 0;
          }
          if (r.type === 'candidate-pair' && r.nominated && typeof r.currentRoundTripTime === 'number') rtt = r.currentRoundTripTime;
        });
        if (jb && lastStats && jb.count > lastStats.count) latency = (jb.delay - lastStats.delay) / (jb.count - lastStats.count) + rtt / 2;
        if (jb) lastStats = jb;
        if (packets > 0 && packets === lastPackets) {
          if (++silentSeconds >= WEBRTC_NO_MEDIA_S) fatal(gen, 'webrtc: no media for ' + WEBRTC_NO_MEDIA_S + ' s');
        } else {
          silentSeconds = 0;
        }
        lastPackets = packets;
      }, function () {});
    }
    function teardown() {
      if (closed) return;
      closed = true;
      clearTimeout(iceTimer);
      clearTimeout(frameTimer);
      clearInterval(statsTimer);
      try { pc.close(); } catch (_) { /* ignore */ }
      if (sessionUrl) {
        try { fetch(sessionUrl, { method: 'DELETE', keepalive: true }).catch(function () {}); } catch (_) { /* ignore */ }
      }
      if (video.srcObject === stream) video.srcObject = null;
    }
    var eng = {
      source: 'webrtc', kind: 'webrtc', pc: pc,
      destroy: teardown,
      latency: function () { return latency; },
      goLive: function () { /* always at the live edge */ },
    };
    return pc.createOffer().then(function (offer) {
      return pc.setLocalDescription(offer);
    }).then(function () {
      return fetch(endpoint, { method: 'POST', headers: { 'Content-Type': 'application/sdp' }, body: pc.localDescription.sdp });
    }).then(function (res) {
      var loc = res.headers.get('Location');
      // After a 307 (cluster: another node took the offer) the session lives there.
      if (loc) sessionUrl = new URL(loc, res.url || endpoint).href;
      if (res.status !== 201) {
        return res.text().then(function (t) { throw new Error('WHEP ' + res.status + ' ' + t.slice(0, 200)); });
      }
      return res.text();
    }).then(function (answer) {
      if (closed) throw new Error('cancelled');
      return pc.setRemoteDescription({ type: 'answer', sdp: answer });
    }).then(function () {
      statsTimer = setInterval(pollStats, 1000);
      return eng;
    }, function (err) {
      teardown();
      throw err;
    });
  }

  function createEngine(source, gen) {
    if (source === 'webrtc') return createWebRtc(gen);
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
      webrtc: !!s.whep && !!window.RTCPeerConnection,
      flv: !!s.flv && mseAvailable(),
      hls: !!s.hls && (mseAvailable() || nativeHls()),
      dash: !!s.dash && mseAvailable(),
    };
    var order;
    if (opts.src !== 'auto') order = ORDER.slice(ORDER.indexOf(opts.src));
    else order = isIOS() ? ['webrtc', 'hls', 'dash'] : ORDER.slice();
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
        if (!langOverride && data.ui_language && setLanguage(data.ui_language)) translateDom(document);
        if (data.ui_theme) setServerTheme(data.ui_theme); // ignored while ?theme= is set
        renderTitle();
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
    if (finite) latencyEl.textContent = formatNumber(l, { minimumFractionDigits: 1, maximumFractionDigits: 1, useGrouping: false }) + ' s';
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

  // Leaving the page: close a WebRTC session right away (DELETE with keepalive) instead
  // of letting the server notice the missing ICE consent; resume if restored (bfcache).
  window.addEventListener('pagehide', function () {
    if (engine && engine.source === 'webrtc') stopLive();
  });
  window.addEventListener('pageshow', function (ev) {
    if (ev.persisted && status === 'live' && !engine) startLive();
  });

  window.rmsPlayer = {
    get status() { return status; },
    get source() { return engine ? engine.source : null; },
    get engine() { return engine ? engine.kind : null; },
    get peerConnection() { return engine && engine.pc ? engine.pc : null; },
    get playing() { return playing; },
    get latency() { var l = engine && playing ? engine.latency() : NaN; return typeof l === 'number' && isFinite(l) ? l : null; },
    get info() { return info; },
    get language() { return getLanguage(); },
    get theme() { return resolvedTheme(); },
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
