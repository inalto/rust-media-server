// rms colour theme (ARCHITECTURE.md §18) for the dashboard and the embedded player.
// Vanilla ES module, no build step; imported by app.js, detail.js, metrics.js and
// player.js.
//
// Model: a *choice* `system` | `light` | `dark`, always written to `<html data-theme>`.
// The CSS keeps the dark palette on `:root`, the light one on `[data-theme="light"]` and,
// for `system` (or no attribute), under `@media (prefers-color-scheme: light)`; so the
// attribute never holds a resolved value and `system` follows the OS live without JS.
//
// Resolution: override (dashboard: this browser's toggle, `localStorage` `rms.theme`;
// player: `?theme=`) → server default (`ui_theme`: `<html data-theme>` rendered by the
// server, then /public/ui, /settings, the SSE `settings` event or the public broadcast
// info) → `system`. The inline script in the <head> of index.html / embed.html applies
// the override before the first paint; this module takes over from there.
//
// - initTheme({override, storageKey})  start (storageKey: persist the override there)
// - getThemeChoice() / getThemeOverride() / getServerTheme() / resolvedTheme()
// - setServerTheme(theme) / setThemeOverride(theme | null) / nextTheme(theme)
// - onThemeChange(fn)  fn({choice, resolved}) after a change of the choice or, with
//                      `system`, of the OS preference (charts re-read their colours)
// - themeColor('--token')  current value of a CSS colour token

export const THEMES = ['system', 'light', 'dark'];

const root = document.documentElement;
const media = typeof window.matchMedia === 'function' ? window.matchMedia('(prefers-color-scheme: light)') : null;
const listeners = new Set();
let serverTheme = 'system';
let override = null;
let storageKey = null;
let applied = ''; // `${choice}|${resolved}` last announced

/** `"system"` / `"light"` / `"dark"` (case-insensitive), else null. */
export function normalizeTheme(value) {
  const v = String(value == null ? '' : value).trim().toLowerCase();
  return THEMES.includes(v) ? v : null;
}

/** Next value of the toggle cycle system → light → dark → system. */
export function nextTheme(theme) {
  return THEMES[(THEMES.indexOf(normalizeTheme(theme) || 'system') + 1) % THEMES.length];
}

/** Choice in effect (override, else server default). */
export function getThemeChoice() { return override || serverTheme; }
export function getThemeOverride() { return override; }
export function getServerTheme() { return serverTheme; }

/** `light` or `dark`: the choice, with `system` resolved through prefers-color-scheme. */
export function resolvedTheme() {
  const choice = getThemeChoice();
  if (choice !== 'system') return choice;
  return media && media.matches ? 'light' : 'dark';
}

/** Current value of a CSS custom property (colour token) of the page. */
export function themeColor(name) {
  return getComputedStyle(root).getPropertyValue(name).trim();
}

function updateMeta() {
  const meta = document.querySelector('meta[name="theme-color"]');
  const bg = themeColor('--bg');
  if (meta && bg) meta.setAttribute('content', bg);
}

const state = () => `${getThemeChoice()}|${resolvedTheme()}`;

function notify() {
  applied = state();
  updateMeta();
  const detail = { choice: getThemeChoice(), resolved: resolvedTheme() };
  for (const fn of [...listeners]) {
    try { fn(detail); } catch (e) { console.error(e); }
  }
}

function apply() {
  root.setAttribute('data-theme', getThemeChoice());
  if (state() !== applied) notify();
}

function readStored() {
  if (!storageKey) return null;
  try { return normalizeTheme(window.localStorage.getItem(storageKey)); } catch (_) { return null; }
}

function writeStored(theme) {
  if (!storageKey) return;
  try {
    if (theme) window.localStorage.setItem(storageKey, theme);
    else window.localStorage.removeItem(storageKey);
  } catch (_) { /* private mode, blocked storage: the choice lasts until reload */ }
}

/**
 * Start: `override` (player: `?theme=`) or the value stored under `storageKey`
 * (dashboard). Without an override, `<html data-theme>` is the server default.
 */
export function initTheme({ override: initial = null, storageKey: key = null } = {}) {
  storageKey = key;
  override = normalizeTheme(initial) || readStored();
  // Without an override the attribute is the server's; with one, the server default
  // arrives later (setServerTheme) and only matters once the override is cleared.
  if (!override) serverTheme = normalizeTheme(root.getAttribute('data-theme')) || 'system';
  root.setAttribute('data-theme', getThemeChoice());
  applied = state();
  updateMeta();
}

/** Server default (`ui_theme`); applied unless an override is active. */
export function setServerTheme(theme) {
  const t = normalizeTheme(theme);
  if (!t) return;
  serverTheme = t;
  apply();
}

/** Per-browser / per-page override; `null` returns to the server default. */
export function setThemeOverride(theme) {
  override = normalizeTheme(theme);
  writeStored(override);
  apply();
}

/** Call `fn({choice, resolved})` after every theme change. Returns an unsubscribe function. */
export function onThemeChange(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

// `system`: the CSS follows the OS by itself; JS consumers (charts, theme-color) are told.
if (media) {
  const onMedia = () => { if (state() !== applied) notify(); };
  if (typeof media.addEventListener === 'function') media.addEventListener('change', onMedia);
  else if (typeof media.addListener === 'function') media.addListener(onMedia);
}

// Another tab of the dashboard changed this browser's override.
window.addEventListener('storage', (e) => {
  if (!storageKey || e.key !== storageKey) return;
  override = normalizeTheme(e.newValue);
  apply();
});
