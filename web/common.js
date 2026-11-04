/* Shared helpers (ES2017): request queue, fetch helpers, formatting, top bar, polling.
   Theme: pure CSS (prefers-color-scheme), see common.css. */
(function () {
'use strict';
const $ = id => document.getElementById(id);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const noop = () => {};

/* Request queue: the board serves one connection at a time. */
let tail = Promise.resolve();
function enqueue(task) {
  const run = tail.then(task);
  tail = run.then(noop, noop); // a failed task must not block the queue
  return run;
}

// fetch + body with a timeout (a hung request would block the queue).
async function req(url, opt, ms) {
  const o = Object.assign({cache: 'no-store'}, opt);
  let t = 0;
  if (typeof AbortController === 'function') {
    const c = new AbortController();
    o.signal = c.signal;
    t = setTimeout(() => c.abort(), ms);
  }
  try {
    const r = await fetch(url, o);
    return {ok: r.ok, status: r.status, text: await r.text()};
  } finally { clearTimeout(t); }
}

// GET -> JSON; network errors/5xx retried after 300 ms, 4xx not (err.status set).
function getJSON(url, tries) {
  tries = tries || 3;
  return enqueue(async () => {
    for (let i = 1; ; i++) {
      try {
        const r = await req(url, null, 8000);
        if (!r.ok) { const e = new Error(r.text || 'HTTP ' + r.status); e.status = r.status; throw e; }
        return JSON.parse(r.text);
      } catch (e) {
        if (i >= tries || (e.status && e.status < 500)) throw e;
        await sleep(300);
      }
    }
  });
}

// POST urlencoded + X-Requested-With, never retried. Resolves {ok,status,text}; rejects if no answer.
function postForm(url, fields, ms) {
  const body = new URLSearchParams();
  Object.keys(fields || {}).forEach(k => { if (fields[k] != null) body.append(k, fields[k]); });
  return enqueue(() => req(url, {
    method: 'POST', body: body.toString(),
    headers: {'X-Requested-With': 'fetch', 'Content-Type': 'application/x-www-form-urlencoded'}
  }, ms || 15000));
}

// Polling: chained timeouts, >= ms between starts, paused while hidden, slower after failures.
function poll(fn, ms) {
  let timer = 0, running = false, last = 0, fails = 0;
  const wait = () => Math.max(ms - (Date.now() - last), 0);
  function schedule(d) {
    clearTimeout(timer);
    if (!document.hidden) timer = setTimeout(tick, d);
  }
  async function tick() {
    if (document.hidden || running) return;
    running = true; last = Date.now();
    try { await fn(); fails = 0; setConn(true); } catch (e) { fails++; setConn(false); }
    running = false;
    schedule(Math.max(wait(), 300) + Math.min(fails, 5) * 1000);
  }
  document.addEventListener('visibilitychange', () => { if (!running) schedule(wait()); });
  schedule(0);
  return {kick() { if (!running) schedule(wait()); }};
}

/* Formatting */
const pad = n => (n < 10 ? '0' : '') + n;
function fmtUptime(s) {
  s = Math.max(0, Math.floor(s));
  const d = Math.floor(s / 86400);
  return (d ? d + 'd ' : '') + pad(Math.floor(s % 86400 / 3600)) + ':' + pad(Math.floor(s % 3600 / 60)) + ':' + pad(s % 60);
}
const fmtPort = u => (u >> 8) + ':' + ((u >> 4) & 15) + ':' + (u & 15); // Net:Sub:Uni
function fmtBytes(n) {
  return n >= 1048576 ? (n / 1048576).toFixed(2) + ' MiB' : n >= 1024 ? (n / 1024).toFixed(1) + ' KiB' : n + ' B';
}
const netName = n => n === 'eth' ? 'Ethernet' : n === 'wifi' ? 'Setup Wi-Fi' : '—';

/* DOM helper and inline SVG icons (24x24 stroke paths) */
function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}
const ICONS = {
  fader: 'M5 20V9M12 20V4M19 20V13',
  pulse: 'M22 12h-4l-3 9L9 3l-3 9H2',
  grid: 'M3 3h7v7H3zM14 3h7v7h-7zM14 14h7v7h-7zM3 14h7v7H3z',
  sliders: 'M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6',
  warn: 'M10.3 3.9L1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0zM12 9v4M12 17h.01',
  cpu: 'M5 5h14v14H5zM9 9h6v6H9zM9 1v4M15 1v4M9 19v4M15 19v4M19 9h4M19 15h4M1 9h4M1 15h4',
  wifi: 'M5 12.5a11 11 0 0 1 14 0M1.4 9a16 16 0 0 1 21.2 0M8.5 16.1a6 6 0 0 1 7 0M12 20h.01',
  zap: 'M13 2L3 14h9l-1 8 10-12h-9z',
  eye: 'M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8zM12 9a3 3 0 1 0 0 6 3 3 0 0 0 0-6z',
  upload: 'M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M17 8l-5-5-5 5M12 3v12',
  power: 'M18.4 6.6a9 9 0 1 1-12.8 0M12 2v10',
  sun: 'M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8zM12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4',
  moon: 'M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z',
  auto: 'M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM12 3v18'
};

/* Theme: auto (system) -> light -> dark. The page head applies the stored
   choice before first paint; this keeps the button in sync. */
const THEMES = [['', 'auto', 'Theme: automatic'], ['light', 'sun', 'Theme: light'], ['dark', 'moon', 'Theme: dark']];
function themeIndex() {
  const t = document.documentElement.dataset.theme || '';
  return Math.max(0, THEMES.findIndex(x => x[0] === t));
}
function showTheme() {
  const b = $('theme'), t = THEMES[themeIndex()];
  b.innerHTML = icon(t[1], 18);
  b.title = b.ariaLabel = t[2];
}
function cycleTheme() {
  const t = THEMES[(themeIndex() + 1) % THEMES.length][0];
  if (t) document.documentElement.dataset.theme = t; else delete document.documentElement.dataset.theme;
  try { if (t) localStorage.setItem('an-theme', t); else localStorage.removeItem('an-theme'); } catch (e) { /* ignore */ }
  showTheme();
}
function icon(n, size) {
  size = size || 16;
  return '<svg viewBox="0 0 24 24" width="' + size + '" height="' + size + '" fill="none" stroke="currentColor" ' +
    'stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="' + ICONS[n] + '"/></svg>';
}

/* Top bar: device name, nav (current page from <body data-page>), live indicator */
const root = document.documentElement;
const PAGES = [['status', '/', 'Status', 'pulse'], ['artnet', '/artnet', 'Live Art-Net', 'zap'], ['config', '/config', 'Settings', 'sliders']];
let pageTitle = '';
function setName(n) {
  $('dev').textContent = n;
  document.title = n + ' · ' + pageTitle;
  try { sessionStorage.setItem('an-name', n); } catch (e) { /* ignore */ }
}
function ensureName() { // cached name, else one /api/status call
  let n = '';
  try { n = sessionStorage.getItem('an-name') || ''; } catch (e) { /* ignore */ }
  if (n) setName(n); else getJSON('/api/status', 2).then(s => setName(s.name), noop);
}
function setConn(ok) {
  $('conn').hidden = false;
  root.classList.toggle('offline', !ok);
  $('conn').lastChild.textContent = ok ? 'Live' : 'Offline';
}
function init() {
  const page = document.body.dataset.page;
  $('top').className = 'top';
  $('top').innerHTML = '<div class="top-in"><div class="brand">' + icon('fader', 26) + '<b id="dev">Art-Net node</b></div>' +
    '<nav class="nav">' + PAGES.map(p => '<a href="' + p[1] + '"' + (p[0] === page ? ' aria-current="page"' : '') + '><i class="ic">' +
      icon(p[3]) + '</i>' + p[2] + '</a>').join('') + '</nav>' +
    '<button type="button" class="theme" id="theme"></button>' +
    '<span class="conn" id="conn" hidden><i></i><span>Live</span></span></div>';
  showTheme();
  $('theme').onclick = cycleTheme;
  PAGES.forEach(p => { if (p[0] === page) pageTitle = p[2]; });
  document.title = pageTitle;
  document.querySelectorAll('[data-i]').forEach(e => { e.innerHTML = icon(e.dataset.i); });
}

window.AN = {$, el, sleep, enqueue, getJSON, postForm, poll, fmtUptime, fmtPort, fmtBytes, netName,
  init, setName, ensureName, setConn};
})();
