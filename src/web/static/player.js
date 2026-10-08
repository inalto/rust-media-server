// Embed player — PHASE 1 minimal version: HTTP-FLV via mpegts.js, waiting screen when
// offline/paused, status from the public SSE (with polling fallback every 3 s).
// The feat/dashboard branch extends it (LL-HLS / LL-DASH sources and fallbacks, §9).
(function () {
  'use strict';

  const body = document.body;
  const name = body.dataset.name;
  const params = new URLSearchParams(location.search);
  const flag = (k, def) => (params.has(k) ? params.get(k) !== '0' : def);
  const opts = {
    autoplay: flag('autoplay', true),
    muted: flag('muted', false),
    controls: flag('controls', true),
    src: params.get('src') || 'auto',
  };

  const video = document.getElementById('video');
  const waitingVideo = document.getElementById('waiting-video');
  const waiting = document.getElementById('waiting');
  const waitingText = document.getElementById('waiting-text');
  const liveBadge = document.getElementById('live-badge');
  const unmuteBtn = document.getElementById('unmute');
  video.controls = opts.controls;
  video.muted = opts.muted;

  let info = null;
  let status = null;
  let player = null;
  let retryTimer = null;
  let pollTimer = null;

  function showWaiting() {
    liveBadge.hidden = true;
    unmuteBtn.hidden = true;
    const text = (info && info.waiting_text) || 'La diretta inizierà a breve';
    waitingText.textContent = text;
    const url = info && info.waiting_video_url;
    if (url) {
      if (waitingVideo.getAttribute('src') !== url) {
        waitingVideo.src = url;
        if (info.poster_url) waitingVideo.poster = info.poster_url;
      }
      waitingVideo.hidden = false;
      waitingVideo.play().catch(() => {});
      waiting.classList.add('over-video');
    } else {
      waitingVideo.hidden = true;
      waiting.classList.remove('over-video');
    }
    waiting.hidden = false;
  }

  function hideWaiting() {
    waiting.hidden = true;
    waitingVideo.hidden = true;
    waitingVideo.pause();
  }

  function destroyPlayer() {
    if (retryTimer) { clearTimeout(retryTimer); retryTimer = null; }
    if (player) {
      try { player.pause(); player.unload(); player.detachMediaElement(); player.destroy(); } catch (_) { /* ignore */ }
      player = null;
    }
    video.removeAttribute('src');
    try { video.load(); } catch (_) { /* ignore */ }
  }

  function tryPlay() {
    if (!opts.autoplay) return;
    const p = video.play();
    if (p && p.catch) {
      p.catch(() => {
        // Autoplay with sound blocked: retry muted and offer an unmute button.
        video.muted = true;
        unmuteBtn.hidden = false;
        video.play().catch(() => {});
      });
    }
  }

  function startFlv() {
    if (!(window.mpegts && mpegts.getFeatureList().mseLivePlayback) || !info.sources.flv) return false;
    player = mpegts.createPlayer(
      { type: 'flv', isLive: true, url: info.sources.flv },
      { enableStashBuffer: false, liveBufferLatencyChasing: true, lazyLoad: false }
    );
    player.attachMediaElement(video);
    player.on(mpegts.Events.ERROR, () => scheduleRetry());
    player.load();
    tryPlay();
    return true;
  }

  function startNativeHls() {
    if (!info.sources.hls || !video.canPlayType('application/vnd.apple.mpegurl')) return false;
    video.src = info.sources.hls;
    tryPlay();
    return true;
  }

  function startPlayback() {
    destroyPlayer();
    let ok = false;
    if (opts.src === 'hls') ok = startNativeHls();
    if (!ok) ok = startFlv() || startNativeHls();
    if (!ok) {
      waitingText.textContent = 'Il browser non supporta la riproduzione della diretta.';
      waiting.hidden = false;
      return;
    }
    hideWaiting();
    liveBadge.hidden = false;
  }

  function scheduleRetry() {
    if (retryTimer) return;
    retryTimer = setTimeout(async () => {
      retryTimer = null;
      await refreshInfo();
      if (status === 'live') startPlayback();
    }, 2000);
  }

  function applyStatus(next) {
    if (next === status && (next !== 'live' || player || video.src)) return;
    status = next;
    if (status === 'live') {
      startPlayback();
    } else {
      destroyPlayer();
      showWaiting();
    }
  }

  async function refreshInfo() {
    try {
      const res = await fetch('/api/v1/public/broadcasts/' + encodeURIComponent(name));
      if (res.status === 404) {
        info = null;
        destroyPlayer();
        waitingText.textContent = 'Diretta non trovata';
        waiting.hidden = false;
        return null;
      }
      if (!res.ok) return null;
      info = await res.json();
      document.title = info.title;
      document.getElementById('title').textContent = info.title;
      return info;
    } catch (_) {
      return null;
    }
  }

  function startPolling() {
    if (pollTimer) return;
    pollTimer = setInterval(async () => {
      const i = await refreshInfo();
      if (i) applyStatus(i.status);
    }, 3000);
  }

  function connectEvents() {
    if (!window.EventSource) { startPolling(); return; }
    const es = new EventSource('/api/v1/public/broadcasts/' + encodeURIComponent(name) + '/events');
    es.addEventListener('status', async (ev) => {
      try {
        const s = JSON.parse(ev.data).status;
        if (s !== status) await refreshInfo();
        applyStatus(s);
      } catch (_) { /* ignore */ }
    });
    es.onerror = () => {
      // The browser reconnects by itself; poll meanwhile so state stays fresh.
      startPolling();
    };
    es.onopen = () => {
      if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
    };
  }

  unmuteBtn.addEventListener('click', () => {
    video.muted = false;
    unmuteBtn.hidden = true;
  });

  window.rmsPlayer = {
    get status() { return status; },
    get info() { return info; },
    setSource(src) { opts.src = src; if (status === 'live') startPlayback(); },
    reload() { if (status === 'live') startPlayback(); },
  };

  (async function init() {
    const i = await refreshInfo();
    if (i) applyStatus(i.status);
    connectEvents();
  })();
})();
