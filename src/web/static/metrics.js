// Broadcast detail — "Metrics" section: viewers over time (Chart.js) + summary tiles.
// Data: GET /broadcasts/{name}/metrics?from&to and /metrics/summary?from&to
// (ARCHITECTURE.md §8.1). A 501 renders a friendly "not available yet" state.

import {
  api, ApiError, describeError, h, icon, setChildren, uid, loadScript,
  fmtInt, fmtBytes, fmtDuration, fmtWhen, fmtDateTime, totalViewers,
} from './core.js';
import { t, formatDate } from './i18n.js';
import { onThemeChange, themeColor } from './theme.js';

const CHART_JS = 'https://cdnjs.cloudflare.com/ajax/libs/Chart.js/4.5.1/chart.umd.min.js';

// Categorical palette (fixed order, never cycled): CSS tokens `--series-*` of the theme
// (style.css; ≥ 3:1 against both chart surfaces, the light theme darkens the green and
// the amber). Labels are functions: resolved in the UI language when the section is built.
const SERIES = [
  { key: 'total', label: () => t('common.total'), token: '--series-total' },
  { key: 'flv', label: () => 'HTTP-FLV', token: '--series-flv' },
  { key: 'ws_flv', label: () => 'WS-FLV', token: '--series-ws-flv' },
  { key: 'hls', label: () => 'LL-HLS', token: '--series-hls' },
  { key: 'dash', label: () => 'LL-DASH', token: '--series-dash' },
  { key: 'webrtc', label: () => 'WebRTC', token: '--series-webrtc' },
];

/** Chart colours of the current theme (re-read on every theme change). */
function chartColors() {
  return {
    series: SERIES.map((s) => themeColor(s.token)),
    grid: themeColor('--chart-grid'),
    ink: themeColor('--text-2'),
    tooltipBg: themeColor('--bg'),
    tooltipBorder: themeColor('--border'),
    tooltipText: themeColor('--text'),
  };
}

/** Area fill of the total: its line colour (a `#rrggbb` token) at 18 % opacity. */
const fillOf = (hex) => (/^#[0-9a-f]{6}$/i.test(hex) ? hex + '2e' : 'transparent');

/** Apply the theme colours to an existing chart's datasets and options. */
function paintChart(chart, c) {
  chart.data.datasets.forEach((d, i) => {
    d.borderColor = c.series[i];
    d.backgroundColor = SERIES[i].key === 'total' ? fillOf(c.series[i]) : c.series[i];
  });
  const { x, y } = chart.options.scales;
  x.ticks.color = c.ink;
  x.grid.color = c.grid;
  x.border.color = c.grid;
  y.ticks.color = c.ink;
  y.grid.color = c.grid;
  y.title.color = c.ink;
  Object.assign(chart.options.plugins.tooltip, {
    backgroundColor: c.tooltipBg, borderColor: c.tooltipBorder, titleColor: c.tooltipText, bodyColor: c.tooltipText,
  });
}

const RANGES = [
  { key: '1h', label: () => t('metrics.range1h'), secs: 3600 },
  { key: '6h', label: () => t('metrics.range6h'), secs: 6 * 3600 },
  { key: '24h', label: () => t('metrics.range24h'), secs: 24 * 3600 },
  { key: '7d', label: () => t('metrics.range7d'), secs: 7 * 86400 },
  { key: 'custom', label: () => t('metrics.rangeCustom') },
];

const REFRESH_MS = 30000;

function loadVisibility() {
  try {
    const v = JSON.parse(localStorage.getItem('rmsChartSeries') || 'null');
    if (v && typeof v === 'object') return v;
  } catch (_) { /* ignore */ }
  return { total: true, flv: true, ws_flv: false, hls: true, dash: true, webrtc: false };
}

function saveVisibility(v) {
  try { localStorage.setItem('rmsChartSeries', JSON.stringify(v)); } catch (_) { /* ignore */ }
}

/** `<input type=datetime-local>` value for a Date (local time). */
function toLocalInput(d) {
  const p = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`;
}

/**
 * Create the metrics section for broadcast `name`.
 * Returns {el, setCurrent(counts), refresh(), destroy()}.
 */
export function metricsSection(name) {
  const titleId = uid('h');
  let range = '1h';
  let custom = null;
  let chart = null;
  let points = [];
  let timer = null;
  let destroyed = false;
  let inflight = null;
  let unavailable = false;
  const visible = loadVisibility();

  // --- controls
  const rangeButtons = RANGES.map((r) => h('button', {
    type: 'button', class: 'seg', 'aria-pressed': String(r.key === range),
    onclick: () => selectRange(r.key),
  }, r.label()));
  const fromId = uid('from');
  const toId = uid('to');
  const fromInput = h('input', { id: fromId, type: 'datetime-local', required: true });
  const toInput = h('input', { id: toId, type: 'datetime-local', required: true });
  const customError = h('p', { class: 'hint error', hidden: true, role: 'alert' });
  const customForm = h('form', { class: 'custom-range', hidden: true, onsubmit: (e) => { e.preventDefault(); applyCustom(); } },
    h('div', { class: 'field' }, h('label', { for: fromId }, t('metrics.from')), fromInput),
    h('div', { class: 'field' }, h('label', { for: toId }, t('metrics.to')), toInput),
    h('button', { type: 'submit', class: 'btn primary' }, t('metrics.apply')),
    customError);
  const refreshBtn = h('button', { type: 'button', class: 'btn icon-only', 'aria-label': t('metrics.refresh'), title: t('metrics.refresh'), onclick: () => refresh() }, icon('refresh'));

  // --- tiles
  const tile = (label) => {
    const value = h('span', { class: 'tile-value' }, '—');
    const sub = h('span', { class: 'tile-sub' });
    return { el: h('div', { class: 'tile' }, h('span', { class: 'tile-label' }, label), value, sub), value, sub };
  };
  const tiles = {
    now: tile(t('metrics.now')),
    peak: tile(t('metrics.peak')),
    sessions: tile(t('metrics.sessions')),
    avg: tile(t('metrics.avgWatch')),
    bytes: tile(t('common.bytesIn')),
  };

  // --- series toggles (HTML legend, keyboard accessible)
  const toggles = SERIES.map((s, i) => {
    const btn = h('button', { type: 'button', class: 'legend-item', 'aria-pressed': String(!!visible[s.key]) },
      h('span', { class: 'swatch', style: { background: `var(${s.token})` }, 'aria-hidden': 'true' }),
      s.label());
    btn.addEventListener('click', () => {
      visible[s.key] = !visible[s.key];
      btn.setAttribute('aria-pressed', String(visible[s.key]));
      saveVisibility(visible);
      if (chart) {
        chart.setDatasetVisibility(i, visible[s.key]);
        chart.update('none');
      }
    });
    return btn;
  });

  const canvas = h('canvas', { role: 'img', 'aria-label': t('metrics.chartLabel') });
  const chartBox = h('div', { class: 'chart-box' }, canvas);
  const chartEmpty = h('p', { class: 'chart-empty', hidden: true }, t('metrics.empty'));
  const tableBody = h('tbody');
  const tableDetails = h('details', { class: 'data-table' },
    h('summary', null, t('metrics.showTable')),
    h('div', { class: 'table-wrap tall' },
      h('table', null,
        h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('metrics.time')), ...SERIES.map((s) => h('th', { scope: 'col', class: 'num' }, s.label())))),
        tableBody)));
  tableDetails.addEventListener('toggle', () => { if (tableDetails.open) renderTable(); });

  const content = h('div', { class: 'metrics-content' },
    h('div', { class: 'tiles' }, ...Object.values(tiles).map((t) => t.el)),
    h('div', { class: 'legend', role: 'group', 'aria-label': t('metrics.legend') }, ...toggles),
    h('div', { class: 'chart-wrap' }, chartBox, chartEmpty),
    tableDetails);
  const state = h('div', { class: 'state-box', hidden: true });

  const el = h('section', { class: 'panel', id: 'sec-metrics', 'aria-labelledby': titleId },
    h('div', { class: 'panel-head' },
      h('h2', { id: titleId }, icon('chart'), t('detail.nav.metrics')),
      h('div', { class: 'panel-tools' },
        h('div', { class: 'segmented', role: 'group', 'aria-label': t('metrics.rangeLabel') }, ...rangeButtons),
        refreshBtn)),
    customForm,
    state,
    content);

  function selectRange(key) {
    range = key;
    rangeButtons.forEach((b, i) => b.setAttribute('aria-pressed', String(RANGES[i].key === key)));
    customForm.hidden = key !== 'custom';
    if (key === 'custom') {
      if (!custom) {
        const to = new Date();
        const from = new Date(to.getTime() - 3 * 3600e3);
        fromInput.value = toLocalInput(from);
        toInput.value = toLocalInput(to);
      }
      fromInput.focus();
      return;
    }
    refresh();
  }

  function applyCustom() {
    const from = new Date(fromInput.value);
    const to = new Date(toInput.value);
    customError.hidden = true;
    if (Number.isNaN(from.getTime()) || Number.isNaN(to.getTime()) || from >= to) {
      customError.textContent = t('metrics.rangeError');
      customError.hidden = false;
      return;
    }
    custom = { from, to };
    refresh();
  }

  function window_() {
    if (range === 'custom' && custom) return { from: custom.from, to: custom.to, relative: false };
    const r = RANGES.find((x) => x.key === range) || RANGES[0];
    const to = new Date();
    return { from: new Date(to.getTime() - r.secs * 1000), to, relative: true };
  }

  function showState(kind, title, text) {
    content.hidden = true;
    state.hidden = false;
    setChildren(state, icon(kind === 'error' ? 'alert' : 'chart', 28), h('p', { class: 'state-title' }, title), text ? h('p', { class: 'hint' }, text) : null);
  }

  async function refresh() {
    if (destroyed) return;
    clearTimeout(timer);
    const w = window_();
    const qs = `from=${encodeURIComponent(w.from.toISOString())}&to=${encodeURIComponent(w.to.toISOString())}`;
    const base = `/broadcasts/${encodeURIComponent(name)}`;
    if (inflight) inflight.abort();
    const ctrl = new AbortController();
    inflight = ctrl;
    el.setAttribute('aria-busy', 'true');
    try {
      const [series, summary] = await Promise.all([
        api(`${base}/metrics?${qs}`, { signal: ctrl.signal }),
        api(`${base}/metrics/summary?${qs}`, { signal: ctrl.signal }),
      ]);
      if (destroyed || ctrl.signal.aborted) return;
      content.hidden = false;
      state.hidden = true;
      renderSummary(summary || {});
      points = Array.isArray(series && series.points) ? series.points : [];
      const from = Date.parse(series && series.from) || w.from.getTime();
      const to = Date.parse(series && series.to) || w.to.getTime();
      await renderChart(from, to);
      if (tableDetails.open) renderTable();
    } catch (e) {
      if (e && e.name === 'AbortError') return;
      if (e instanceof ApiError && e.status === 501) {
        unavailable = true; // no point in polling an endpoint the server does not have
        showState('info', t('metrics.unavailable'), t('metrics.unavailableText'));
      } else if (e instanceof ApiError && e.status === 401) {
        return;
      } else {
        showState('error', t('metrics.loadError'), describeError(e));
      }
    } finally {
      if (inflight === ctrl) inflight = null;
      el.removeAttribute('aria-busy');
      if (!destroyed && w.relative && !unavailable) timer = setTimeout(refresh, REFRESH_MS);
    }
  }

  function renderSummary(s) {
    const current = s.current ? totalViewers(s.current) : null;
    tiles.now.value.textContent = fmtInt(current);
    const peak = s.peak || {};
    tiles.peak.value.textContent = fmtInt(peak.value);
    tiles.peak.sub.textContent = peak.at ? t('metrics.peakAt', { when: fmtWhen(peak.at) }) : '';
    tiles.sessions.value.textContent = fmtInt(s.sessions_total);
    tiles.avg.value.textContent = fmtDuration(s.avg_watch_secs);
    tiles.bytes.value.textContent = fmtBytes(s.bytes_in);
    tiles.bytes.sub.textContent = s.live_since ? t('metrics.liveSince', { when: fmtWhen(s.live_since) }) : '';
  }

  function tickFormatter(span) {
    return (v) => {
      const d = new Date(v);
      const hm = formatDate(d, { hour: '2-digit', minute: '2-digit' });
      if (span <= 26 * 3600e3) return hm;
      return formatDate(d, { day: '2-digit', month: '2-digit' }) + ' ' + hm;
    };
  }

  async function renderChart(from, to) {
    let Chart;
    try {
      Chart = await loadScript(CHART_JS, 'Chart');
    } catch (_) {
      showState('error', t('metrics.libError'), t('metrics.libErrorText'));
      return;
    }
    if (destroyed) return;
    const c = chartColors();
    const datasets = SERIES.map((s, i) => ({
      label: s.label(),
      data: points.map((p) => ({ x: Date.parse(p.t), y: Number(p[s.key]) || 0 })),
      borderColor: c.series[i],
      backgroundColor: s.key === 'total' ? fillOf(c.series[i]) : c.series[i],
      fill: s.key === 'total' ? 'origin' : false,
      stepped: 'before',
      borderWidth: 2,
      pointRadius: 0,
      pointHoverRadius: 4,
      pointHitRadius: 6,
      hidden: !visible[s.key],
    }));
    const any = points.some((p) => (Number(p.total) || 0) > 0);
    chartEmpty.hidden = any;
    const { grid, ink } = c;
    const span = to - from;
    if (chart) {
      chart.data.datasets = datasets;
      chart.options.scales.x.min = from;
      chart.options.scales.x.max = to;
      chart.options.scales.x.ticks.callback = tickFormatter(span);
      chart.update('none');
      return;
    }
    chart = new Chart(canvas, {
      type: 'line',
      data: { datasets },
      options: {
        responsive: true,
        maintainAspectRatio: false,
        animation: false,
        parsing: false,
        normalized: true,
        interaction: { mode: 'index', intersect: false },
        scales: {
          x: {
            type: 'linear', min: from, max: to,
            ticks: { color: ink, maxTicksLimit: 7, maxRotation: 0, callback: tickFormatter(span) },
            grid: { color: grid }, border: { color: grid },
          },
          y: {
            beginAtZero: true, suggestedMax: 5,
            ticks: { color: ink, precision: 0 },
            grid: { color: grid }, border: { display: false },
            title: { display: true, text: t('common.viewers'), color: ink },
          },
        },
        plugins: {
          legend: { display: false },
          tooltip: {
            backgroundColor: c.tooltipBg, borderColor: c.tooltipBorder, borderWidth: 1,
            titleColor: c.tooltipText, bodyColor: c.tooltipText, padding: 10,
            callbacks: { title: (items) => (items.length ? fmtDateTime(items[0].parsed.x) : '') },
          },
        },
      },
    });
  }

  function renderTable() {
    const rows = points.slice(-500).reverse().map((p) => h('tr', null,
      h('th', { scope: 'row' }, fmtDateTime(p.t)),
      ...SERIES.map((s) => h('td', { class: 'num' }, fmtInt(p[s.key] || 0)))));
    setChildren(tableBody, rows.length ? rows : h('tr', null, h('td', { colspan: String(SERIES.length + 1) }, t('metrics.noData'))));
  }

  // Theme switched (toggle, server default, or the OS with `system`): recolour in place.
  const offTheme = onThemeChange(() => {
    if (!chart) return;
    paintChart(chart, chartColors());
    chart.update('none');
  });

  return {
    el,
    /** Live counts from the admin SSE keep the "now" tile fresh between refreshes. */
    setCurrent(counts) {
      if (!content.hidden) tiles.now.value.textContent = fmtInt(totalViewers(counts));
    },
    refresh,
    destroy() {
      destroyed = true;
      offTheme();
      clearTimeout(timer);
      if (inflight) inflight.abort();
      if (chart) chart.destroy();
      chart = null;
    },
  };
}

