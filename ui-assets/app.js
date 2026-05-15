// The sim's local zone (returned by /api/clock — physics is
// calibrated in it). SIM_TZ is what we *currently format with*;
// the user can flip between LOCAL_TZ and 'UTC' via the toggle in
// the header. Browser zone is intentionally ignored — a remote
// operator looking at a Berlin-anchored sim should never see
// dropdowns shifted by 6 h because they're sitting in EDT.
let LOCAL_TZ = 'UTC';
let LOCAL_TZ_LABEL = 'UTC';
let SIM_TZ = LOCAL_TZ;
let SIM_TZ_LABEL = LOCAL_TZ_LABEL;
const TZ_PREF_KEY = 'tradingsim-tz';

async function loadClock() {
  try {
    const r = await fetch('/api/clock');
    if (!r.ok) return;
    const j = await r.json();
    if (j.tz) LOCAL_TZ = j.tz;
    // Probe the short zone abbreviation for the *current* instant.
    // CEST in summer, CET in winter — Intl picks based on DST.
    const parts = new Intl.DateTimeFormat('en-US', {
      timeZone: LOCAL_TZ, timeZoneName: 'short',
    }).formatToParts(new Date());
    const tag = parts.find(p => p.type === 'timeZoneName');
    if (tag) LOCAL_TZ_LABEL = tag.value;
  } catch (_) { /* keep UTC fallback */ }
  // Apply persisted preference (default: local). When the user
  // last picked UTC, restore that; otherwise sit on local.
  const saved = localStorage.getItem(TZ_PREF_KEY);
  applyTz(saved === 'utc' ? 'utc' : 'local');
}

function applyTz(mode) {
  if (mode === 'utc') {
    SIM_TZ = 'UTC';
    SIM_TZ_LABEL = 'UTC';
  } else {
    SIM_TZ = LOCAL_TZ;
    SIM_TZ_LABEL = LOCAL_TZ_LABEL;
  }
  const chip = document.getElementById('tz-toggle');
  if (chip) chip.textContent = mode === 'utc' ? 'UTC' : 'local';
}

function toggleTz() {
  const next = SIM_TZ === 'UTC' ? 'local' : 'utc';
  localStorage.setItem(TZ_PREF_KEY, next);
  applyTz(next);
  // Re-render every panel that displays a time. The clock + the
  // scenario panel both reflect the new zone on the next poll,
  // but we force an immediate refresh so the toggle feels
  // instantaneous.
  tickClock();
  rerenderBook();
  rerenderTrades();
  rerenderWeather();
  loadScenarios();
}

function shortTime(iso) {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '--:--';
  return d.toLocaleTimeString('en-GB', {
    hour: '2-digit', minute: '2-digit', hour12: false, timeZone: SIM_TZ,
  });
}

// hh:mm:ss variant for the gridpool drill-down's order create/upd
// columns + the per-order trade exec column — seconds matter when
// reading the local history of one order at the resolution the
// matcher actually fires at. The book / trades-tape / period
// dropdowns stay on shortTime since their context is "which 15-min
// contract" and seconds would just add noise.
function shortTimeSec(iso) {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '--:--:--';
  return d.toLocaleTimeString('en-GB', {
    hour: '2-digit', minute: '2-digit', second: '2-digit',
    hour12: false, timeZone: SIM_TZ,
  });
}

async function loadInfo() {
  const r = await fetch('/api/info');
  if (!r.ok) return;
  const j = await r.json();
  document.getElementById('info').textContent =
    `v${j.version} · ${j.gridpools} gridpool${j.gridpools === 1 ? '' : 's'} · ${j.markets} markets · ${j.couplings} couplings`;
}

// Pulse-bar state: per-area trade counts in 12 × 5s rolling buckets.
// DE TSO zones only at this step; the +neighbours chip in step 5
// will surface the international areas.
const SPARK_BUCKET_MS = 5000;
const SPARK_BUCKETS = 12;

// Single source of truth for all configured delivery zones. Each
// entry carries every field any subsystem cares about; the spark
// bars / chart / filter chips all derive their views via .filter().
// Keep the four control zones first so the home market sits on the
// left in every list.
const ALL_AREAS = [
  { code: '10YDE-EON------1', tag: 'TN', group: 'de',   color: '#58a6ff' },
  { code: '10YDE-RWENET---I', tag: 'AM', group: 'de',   color: '#a371f7' },
  { code: '10YDE-VE-------2', tag: 'HZ', group: 'de',   color: '#9ee493' },
  { code: '10YDE-ENBW-----N', tag: 'BW', group: 'de',   color: '#e1af6c' },
  { code: '10YFR-RTE------C', tag: 'FR', group: 'intl', color: null },
  { code: '10YNL----------L', tag: 'NL', group: 'intl', color: null },
  { code: '10YBE----------2', tag: 'BE', group: 'intl', color: null },
  { code: '10YAT-APG------L', tag: 'AT', group: 'intl', color: null },
];
const DE_AREAS = ALL_AREAS.filter(a => a.group === 'de');
const sparkState = new Map();
for (const a of DE_AREAS) sparkState.set(a.code, new Array(SPARK_BUCKETS).fill(0));

function rotateSparkBuckets() {
  for (const buckets of sparkState.values()) {
    buckets.shift();
    buckets.push(0);
  }
}

function recordTrade(t) {
  // Credit the buy-side area; counts both intra and cross-area
  // prints from the perspective of the bidding zone receiving energy.
  const buckets = sparkState.get(t.buy_area);
  if (buckets) buckets[buckets.length - 1] += 1;
}

function renderSparkbars() {
  const el = document.getElementById('spark-row');
  if (!el) return;
  el.innerHTML = DE_AREAS.map(a => {
    const buckets = sparkState.get(a.code) || [];
    const max = Math.max(1, ...buckets);
    const bars = buckets.map(n => {
      const h = Math.max(1, Math.round((n / max) * 14));
      return `<span class="spark-bar" style="height:${h}px"></span>`;
    }).join('');
    return `<span class="spark-item"><span class="area-badge">${a.tag}</span><span class="spark">${bars}</span></span>`;
  }).join('');
}

function setPill(id, state, label) {
  const el = document.getElementById(id);
  if (!el) return;
  el.classList.remove('ok', 'down', 'warn');
  if (state) el.classList.add(state);
  if (label) el.textContent = label;
}

function tickClock() {
  const hhmmss = new Date().toLocaleTimeString('en-GB', {
    hour: '2-digit', minute: '2-digit', second: '2-digit',
    hour12: false, timeZone: SIM_TZ,
  });
  // Show the sim's local time + zone tag so the user can tell at
  // a glance whether they're looking at e.g. 14:00 CEST (= UTC 12)
  // vs 14:00 UTC.
  document.getElementById('clock').textContent = `${hhmmss} ${SIM_TZ_LABEL}`;
}

const DENSITY_KEY = 'tradingsim-density';

function updateDensityChip() {
  const el = document.getElementById('density-toggle');
  if (!el) return;
  el.textContent = document.body.classList.contains('comfortable') ? 'comfortable' : 'compact';
}

function toggleDensity() {
  const isComf = document.body.classList.toggle('comfortable');
  localStorage.setItem(DENSITY_KEY, isComf ? 'comfortable' : 'compact');
  updateDensityChip();
  drawChart();
}

function initDensity() {
  let saved = localStorage.getItem(DENSITY_KEY);
  if (!saved) saved = window.innerWidth >= 1800 ? 'comfortable' : 'compact';
  if (saved === 'comfortable') document.body.classList.add('comfortable');
  updateDensityChip();
}

// Tier 2 — price chart. Per-area circular buffers of recent
// trade prints, redrawn on a 1s tick. Canvas is sized to its
// parent's CSS width each draw so window resizes don't blur it.
//
// Each entry stores periodMs so the chart can isolate a single
// delivery period — without that filter, prints for delivery
// 15:00 and 18:00 would land on the same line at the same
// wallclock minute and the line would jump nonsensically.
const PERIOD_STEP_MS = 15 * 60 * 1000;
const MAX_WINDOW_MS = 4 * 60 * 60 * 1000;
let chartWindowMs = 30 * 60 * 1000;
// Chart plots only the colored areas (the four control zones).
// Intl areas don't get a chart line — they show up in trades and
// the order book but not the per-area price tape.
const CHART_AREAS = ALL_AREAS.filter(a => a.color);
const priceSeries = new Map();
for (const a of CHART_AREAS) priceSeries.set(a.code, []);

function recordTradePrice(t) {
  const arr = priceSeries.get(t.buy_area);
  if (!arr) return;
  const ms = Date.parse(t.execution_time);
  const pms = Date.parse(t.period);
  if (!isFinite(ms) || !isFinite(pms)) return;
  arr.push({ t: ms, price: parseFloat(t.price), periodMs: pms });
  const cutoff = Date.now() - MAX_WINDOW_MS;
  while (arr.length && arr[0].t < cutoff) arr.shift();
}

// Persisted-select state — localStorage entries keep the chart
// window, book delivery period, and trades filter alive across
// page reloads without depending on the browser's form-restoration
// timing (which is unreliable for dropdowns whose options are
// populated by JS after data arrives).
const SELECT_PREFS = {
  chart:  'tradingsim-chart-window-min',
  book:   'tradingsim-book-period',
  trades: 'tradingsim-trades-filter',
};

function rememberSelectChoice(which, value) {
  if (value == null || value === '') localStorage.removeItem(SELECT_PREFS[which]);
  else localStorage.setItem(SELECT_PREFS[which], value);
}

function rememberedSelectChoice(which) {
  return localStorage.getItem(SELECT_PREFS[which]);
}

function setChartWindow(mins) {
  chartWindowMs = parseInt(mins) * 60 * 1000;
  rememberSelectChoice('chart', mins);
  drawChart();
}

function initChartWindow() {
  // Prefer the saved choice; fall back to whatever the <select>
  // is showing right now (defaults to its `selected` option).
  const sel = document.getElementById('chart-window');
  if (!sel) return;
  const saved = rememberedSelectChoice('chart');
  if (saved) sel.value = saved;
  setChartWindow(sel.value);
}

/** Effective delivery period (epoch ms) the chart is plotting:
 *  pinned period if the user clicked into one, otherwise the
 *  soonest upcoming 15-min boundary (the contract that's about
 *  to close + currently most-actively-traded). Rotates as
 *  wallclock advances. */
function effectivePeriodMs() {
  if (periodFilter) return Date.parse(periodFilter);
  const now = Date.now();
  return Math.ceil(now / PERIOD_STEP_MS) * PERIOD_STEP_MS;
}

function pickXStep(windowMs) {
  const candidates = [1, 2, 5, 10, 15, 30, 60, 120].map(m => m * 60 * 1000);
  for (const c of candidates) if (windowMs / c <= 8) return c;
  return windowMs;
}

function formatBack(mins) {
  if (mins === 0) return 'now';
  if (mins < 60) return `-${mins}m`;
  const h = Math.floor(mins / 60);
  const m = mins % 60;
  return m ? `-${h}h${m}m` : `-${h}h`;
}

function updateChartTitle() {
  const el = document.getElementById('chart-title');
  if (!el) return;
  const eff = effectivePeriodMs();
  const hhmm = shortTime(new Date(eff).toISOString());
  const tag = periodFilter ? 'pinned' : 'auto';
  el.textContent = `Price tape — delivery ${hhmm} (${tag})`;
}

function renderChartLegend() {
  const el = document.getElementById('chart-legend');
  if (!el) return;
  el.innerHTML = CHART_AREAS
    .map(a => `<span><span class="dot" style="background:${a.color}"></span>${a.tag}</span>`)
    .join('');
}

function drawChart() {
  const canvas = document.getElementById('price-chart');
  if (!canvas) return;
  const dpr = window.devicePixelRatio || 1;
  const cssW = canvas.parentElement.clientWidth - 20;
  const cssH = 280;
  canvas.style.width = cssW + 'px';
  canvas.style.height = cssH + 'px';
  canvas.width = Math.max(1, Math.floor(cssW * dpr));
  canvas.height = Math.max(1, Math.floor(cssH * dpr));
  const ctx = canvas.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, cssW, cssH);

  const padL = 38, padR = 8, padT = 6, padB = 18;
  const innerW = cssW - padL - padR;
  const innerH = cssH - padT - padB;

  updateChartTitle();
  const effMs = effectivePeriodMs();
  const now = Date.now();
  const tmin = now - chartWindowMs;

  let ymin = Infinity, ymax = -Infinity;
  for (const [code, arr] of priceSeries) {
    if (!activeAreas.has(code)) continue;
    for (const p of arr) {
      if (p.t < tmin) continue;
      if (p.periodMs !== effMs) continue;
      if (p.price < ymin) ymin = p.price;
      if (p.price > ymax) ymax = p.price;
    }
  }
  if (!isFinite(ymin)) { ymin = 70; ymax = 100; }
  if (ymax - ymin < 1) { ymax = ymin + 1; }
  const ypad = (ymax - ymin) * 0.1;
  ymin -= ypad; ymax += ypad;

  const xs = t => padL + ((t - tmin) / chartWindowMs) * innerW;
  const ys = p => padT + (1 - (p - ymin) / (ymax - ymin)) * innerH;

  const styles = getComputedStyle(document.documentElement);
  const borderC = styles.getPropertyValue('--border').trim() || '#30363d';
  const mutedC = styles.getPropertyValue('--muted').trim() || '#8b949e';

  ctx.strokeStyle = borderC;
  ctx.fillStyle = mutedC;
  ctx.font = '10px ui-monospace, monospace';
  ctx.lineWidth = 1;

  const ySteps = 5;
  for (let i = 0; i <= ySteps; i++) {
    const y = padT + (i / ySteps) * innerH;
    ctx.beginPath();
    ctx.moveTo(padL, y);
    ctx.lineTo(cssW - padR, y);
    ctx.stroke();
    const v = ymax - (i / ySteps) * (ymax - ymin);
    ctx.textAlign = 'right';
    ctx.fillText(v.toFixed(0), padL - 4, y + 3);
  }

  ctx.textAlign = 'center';
  const xStep = pickXStep(chartWindowMs);
  for (let offset = 0; offset <= chartWindowMs; offset += xStep) {
    const t = now - offset;
    const x = xs(t);
    ctx.fillText(formatBack(Math.round(offset / 60000)), x, cssH - 4);
  }

  ctx.lineWidth = 1.5;
  ctx.lineJoin = 'round';
  for (const a of CHART_AREAS) {
    if (!activeAreas.has(a.code)) continue;
    const raw = priceSeries.get(a.code) || [];
    const arr = raw.filter(p => p.t >= tmin && p.periodMs === effMs);
    if (arr.length < 2) continue;
    ctx.strokeStyle = a.color;
    ctx.beginPath();
    for (let i = 0; i < arr.length; i++) {
      const x = xs(arr[i].t);
      const y = ys(arr[i].price);
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.stroke();
  }
}

// Gridpool drill-down (3-pane master-detail): the pool list cache
// + the currently-selected pool / order drive the middle and right
// panes. Server endpoints used:
//   /api/gridpools                      — list + counts
//   /api/gridpools/{id}/orders          — order list (newest first)
//   /api/gridpools/{id}/orders/{oid}/trades — fills for that order
// Period filter is applied client-side: at our volumes the full
// order list is cheap, and the dropdown's distinct-period set has
// to be derived from the unfiltered data anyway.
let gridpoolList = [];
let gridpoolSelectedId = null;
let gridpoolOrders = [];
let gridpoolDeliveryFilter = null;
let gridpoolSelectedOrderId = null;
let gridpoolTrades = [];

// Drop the noisy SCREAMING_SNAKE proto prefix for display. Falls
// back to the raw name on unknown enum variants so a future server
// addition shows up legibly rather than blank.
const ORDER_STATE_SHORT = {
  ORDER_STATE_PENDING:   'pending',
  ORDER_STATE_ACTIVE:    'active',
  ORDER_STATE_HIBERNATE: 'hibernate',
  ORDER_STATE_FILLED:    'filled',
  ORDER_STATE_CANCELED:  'canceled',
  ORDER_STATE_EXPIRED:   'expired',
  ORDER_STATE_FAILED:    'failed',
};
const SIDE_SHORT = { MARKET_SIDE_BUY: 'buy', MARKET_SIDE_SELL: 'sell' };
const TRADE_STATE_SHORT = {
  TRADE_STATE_ACTIVE:            'active',
  TRADE_STATE_CANCEL_REQUESTED:  'cancel?',
  TRADE_STATE_CANCEL_REJECTED:   'cancel✗',
  TRADE_STATE_CANCELED:          'canceled',
  TRADE_STATE_RECALL_REQUESTED:  'recall?',
  TRADE_STATE_RECALL_REJECTED:   'recall✗',
  TRADE_STATE_RECALLED:          'recalled',
  TRADE_STATE_APPROVAL_REQUESTED:'approval?',
};
function shortOrderState(s) { return ORDER_STATE_SHORT[s] || s; }
function shortSide(s)       { return SIDE_SHORT[s] || s; }
function shortTradeState(s) { return TRADE_STATE_SHORT[s] || s; }

async function loadGridpools() {
  const r = await fetch('/api/gridpools');
  if (!r.ok) return;
  gridpoolList = await r.json();
  // Auto-select on first paint: pool with the most trades wins —
  // it's the one most likely to have a non-empty drill-down state,
  // which avoids opening the page onto two empty panes. Falls back
  // to the first pool if every pool's trade count is zero.
  if (gridpoolSelectedId == null && gridpoolList.length) {
    const best = [...gridpoolList].sort((a, b) => b.trades - a.trades)[0];
    await selectGridpool(best.id);
    return;
  }
  renderGridpoolList();
}

function renderGridpoolList() {
  const el = document.getElementById('gridpool-list');
  if (!el) return;
  if (!gridpoolList.length) {
    el.innerHTML = '<i>no gridpools registered</i>';
    return;
  }
  el.innerHTML = gridpoolList.map(g => {
    const sel = g.id === gridpoolSelectedId ? ' selected' : '';
    const badges = g.areas.map(a => `<span class="area-badge">${areaTag(a)}</span>`).join(' ');
    return `<div class="row-item${sel}" onclick="selectGridpool(${g.id})">
      <div class="row-head">
        <span class="area-badge">${g.id}</span>
        <span>${escapeHtml(g.name)}</span>
      </div>
      <div class="row-meta muted">
        ${badges}
        <span class="row-sep">·</span>
        <span>${g.orders} orders</span>
        <span class="row-sep">·</span>
        <span>${g.trades} trades</span>
      </div>
    </div>`;
  }).join('');
}

async function selectGridpool(id) {
  if (gridpoolSelectedId !== id) {
    gridpoolSelectedId = id;
    gridpoolSelectedOrderId = null;
    gridpoolDeliveryFilter = null;
    gridpoolTrades = [];
  }
  renderGridpoolList();
  renderGridpoolTrades();
  await loadGridpoolOrders();
}

async function loadGridpoolOrders() {
  if (gridpoolSelectedId == null) return;
  const r = await fetch(`/api/gridpools/${gridpoolSelectedId}/orders`);
  if (!r.ok) return;
  gridpoolOrders = await r.json();
  // If the previously-selected order vanished (fully filled +
  // pruned, or cancelled), drop the selection so the trades pane
  // doesn't keep showing stale fills.
  if (
    gridpoolSelectedOrderId != null &&
    !gridpoolOrders.some(o => o.id === gridpoolSelectedOrderId)
  ) {
    gridpoolSelectedOrderId = null;
    gridpoolTrades = [];
    renderGridpoolTrades();
  }
  renderGridpoolPeriodSelect();
  renderGridpoolOrders();
}

function gridpoolDistinctPeriods() {
  const set = new Set(gridpoolOrders.map(o => o.period));
  return [...set].sort();
}

function renderGridpoolPeriodSelect() {
  const sel = document.getElementById('gridpool-period-select');
  if (!sel) return;
  const periods = gridpoolDistinctPeriods();
  const opts = ['<option value="">all</option>'];
  for (const p of periods) opts.push(`<option value="${p}">${shortTime(p)}</option>`);
  sel.innerHTML = opts.join('');
  // Preserve the user's choice across the periodic refresh; drop
  // it silently if the period no longer matches any open order.
  if (gridpoolDeliveryFilter && periods.includes(gridpoolDeliveryFilter)) {
    sel.value = gridpoolDeliveryFilter;
  } else {
    gridpoolDeliveryFilter = null;
    sel.value = '';
  }
}

function selectGridpoolPeriod(value) {
  gridpoolDeliveryFilter = value || null;
  renderGridpoolOrders();
}

function gridpoolVisibleOrders() {
  if (!gridpoolDeliveryFilter) return gridpoolOrders;
  return gridpoolOrders.filter(o => o.period === gridpoolDeliveryFilter);
}

function renderGridpoolOrders() {
  const el = document.getElementById('gridpool-orders');
  if (!el) return;
  if (gridpoolSelectedId == null) {
    el.innerHTML = '<i>select a gridpool</i>';
    return;
  }
  const orders = gridpoolVisibleOrders();
  if (!orders.length) {
    el.innerHTML = gridpoolDeliveryFilter
      ? '<i>no orders for this delivery</i>'
      : '<i>no orders for this gridpool</i>';
    return;
  }
  const rows = orders.map(o => {
    const sel = o.id === gridpoolSelectedOrderId ? ' selected' : '';
    const sideCls = o.side === 'MARKET_SIDE_BUY' ? 'buy' : 'sell';
    return `<tr class="gp-order-row${sel}" onclick="selectGridpoolOrder(${o.id})">
      <td>${o.id}</td>
      <td class="${sideCls}">${shortSide(o.side)}</td>
      <td><span class="area-badge">${areaTag(o.area)}</span></td>
      <td>${shortTime(o.period)}</td>
      <td>${o.price}</td>
      <td>${o.filled_quantity}/${o.quantity}</td>
      <td>${shortOrderState(o.state)}</td>
      <td>${shortTimeSec(o.create_time)}</td>
      <td>${shortTimeSec(o.modification_time)}</td>
    </tr>`;
  }).join('');
  el.innerHTML = `<div class="scroll"><table>
    <thead><tr>
      <th>id</th><th>side</th><th>area</th><th>delivery</th>
      <th>price</th><th>filled/qty</th><th>state</th>
      <th>created</th><th>upd</th>
    </tr></thead>
    <tbody>${rows}</tbody>
  </table></div>`;
}

async function selectGridpoolOrder(oid) {
  gridpoolSelectedOrderId = oid;
  renderGridpoolOrders();
  await loadGridpoolOrderTrades();
}

async function loadGridpoolOrderTrades() {
  if (gridpoolSelectedId == null || gridpoolSelectedOrderId == null) return;
  const r = await fetch(
    `/api/gridpools/${gridpoolSelectedId}/orders/${gridpoolSelectedOrderId}/trades`,
  );
  if (!r.ok) return;
  gridpoolTrades = await r.json();
  renderGridpoolTrades();
}

function renderGridpoolTrades() {
  const el = document.getElementById('gridpool-trades');
  if (!el) return;
  if (gridpoolSelectedId == null) {
    el.innerHTML = '<i>select a gridpool</i>';
    return;
  }
  if (gridpoolSelectedOrderId == null) {
    el.innerHTML = '<i>select an order</i>';
    return;
  }
  if (!gridpoolTrades.length) {
    el.innerHTML = '<i>no trades yet for this order</i>';
    return;
  }
  const rows = gridpoolTrades.map(t => `<tr>
    <td>${t.id}</td>
    <td><span class="area-badge">${areaTag(t.area)}</span></td>
    <td>${shortTimeSec(t.execution_time)}</td>
    <td>${t.price}</td>
    <td>${t.quantity}</td>
    <td>${shortTradeState(t.state)}</td>
  </tr>`).join('');
  el.innerHTML = `<div class="scroll"><table>
    <thead><tr>
      <th>id</th><th>area</th><th>exec</th><th>price</th><th>qty</th><th>state</th>
    </tr></thead>
    <tbody>${rows}</tbody>
  </table></div>`;
}

async function refreshGridpoolDrilldown() {
  await loadGridpools();
  if (gridpoolSelectedId != null) await loadGridpoolOrders();
  if (gridpoolSelectedOrderId != null) await loadGridpoolOrderTrades();
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, c => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
  })[c]);
}

function wallclockNowHour() {
  // Stage hour_from / hour_to are *always* in the sim's local zone
  // (that's how the lisp config writes them and how the bias tick
  // matches them server-side). The now-marker on the timeline
  // tracks LOCAL_TZ regardless of whether the user toggled the
  // display chip to UTC — toggling shouldn't visually misalign
  // the marker against the stage blocks.
  const parts = new Intl.DateTimeFormat('en-GB', {
    timeZone: LOCAL_TZ,
    hour: '2-digit', minute: '2-digit', second: '2-digit',
    hour12: false,
  }).formatToParts(new Date());
  const get = t => parseInt(parts.find(p => p.type === t).value, 10);
  return get('hour') + get('minute') / 60 + get('second') / 3600;
}

function fmtStageHour(h) {
  const hh = Math.floor(h);
  const mm = Math.round((h - hh) * 60);
  return `${String(hh).padStart(2, '0')}:${String(mm).padStart(2, '0')}`;
}

function renderScenarioActive(s, safe) {
  const cur = s.current_stage;
  const last = s.stages.length - 1;
  const manualBadge = s.manual_override
    ? '<span class="badge-manual">manual</span>'
    : '';

  // Timeline strip: blocks carry just the stage number now —
  // names live in the list below where there's actually room
  // for them. The full name + bias still surface on hover.
  const stageBlocks = s.stages.map((st, i) => {
    const left = (st.hour_from / 24) * 100;
    const width = ((st.hour_to - st.hour_from) / 24) * 100;
    let cls = 'timeline-stage';
    if (i === cur) cls += ' current';
    else if (i < cur) cls += ' done';
    return `<div class="${cls}" style="left:${left}%;width:${width}%"
                 onclick="scenarioJump('${safe}', ${i})"
                 title="${escapeHtml(st.name)} — bias ${st.bias_from.toFixed(2)} → ${st.bias_to.toFixed(2)}">${i + 1}</div>`;
  }).join('');
  const nowPct = (wallclockNowHour() / 24) * 100;

  // List of all stages with full detail — name, time window,
  // bias trajectory. State markers: ✓ for done, ▶ for current.
  const stageRows = s.stages.map((st, i) => {
    let cls = 'stage-list-row';
    let state = '';
    if (i === cur) { cls += ' current'; state = '▶'; }
    else if (i < cur) { cls += ' done'; state = '✓'; }
    const time = `${fmtStageHour(st.hour_from)}–${fmtStageHour(st.hour_to)}`;
    const bias = `${st.bias_from.toFixed(2)} → ${st.bias_to.toFixed(2)}`;
    const weatherParts = [];
    if (st.cloud_cover != null) weatherParts.push(`☁ ${st.cloud_cover.toFixed(2)}`);
    if (st.mean_wind != null) weatherParts.push(`${st.mean_wind.toFixed(1)} m/s`);
    if (st.temperature_base != null) weatherParts.push(`${Math.round(st.temperature_base - 273.15)} °C`);
    const weather = weatherParts.join(' · ');
    return `<div class="${cls}" onclick="scenarioJump('${safe}', ${i})" title="${escapeHtml(st.name)}">
      <span class="stage-num">${i + 1}</span>
      <span class="stage-state">${state}</span>
      <span class="stage-name">${escapeHtml(st.name)}</span>
      <span class="stage-time">${time}</span>
      <span class="stage-bias">${bias}</span>
      <span class="stage-weather">${weather}</span>
    </div>`;
  }).join('');

  const prevDis = cur <= 0 ? 'disabled' : '';
  const nextDis = cur >= last ? 'disabled' : '';

  return `
    <div class="scenario active">
      <div class="scenario-head">
        <strong>${escapeHtml(s.name)}</strong>
        ${manualBadge}
        <span class="scenario-controls" style="margin-left:auto">
          <button onclick="scenarioAction('${safe}', 'prev')" ${prevDis}>Prev</button>
          <button onclick="scenarioAction('${safe}', 'next')" ${nextDis}>Next</button>
          <button onclick="scenarioAction('${safe}', 'stop')">Stop</button>
        </span>
      </div>
      <div class="timeline">
        ${stageBlocks}
        <div class="timeline-now" style="left:${nowPct}%"></div>
      </div>
      <div class="timeline-axis"><span>00:00</span><span>06:00</span><span>12:00</span><span>18:00</span><span>24:00</span></div>
      <div class="stage-list">
        <div class="stage-list-header">
          <span></span>
          <span></span>
          <span class="stage-name">stage</span>
          <span class="stage-time">time</span>
          <span class="stage-bias">bias</span>
          <span class="stage-weather">weather</span>
        </div>
        ${stageRows}
      </div>
    </div>`;
}

async function scenarioJump(name, idx) {
  await fetch(`/api/scenarios/${name}/jump/${idx}`, { method: 'POST' });
  loadScenarios();
}

async function loadWeather() {
  let list;
  try {
    const r = await fetch('/api/weather');
    if (!r.ok) {
      setPill('pill-weather', 'down', 'weather');
      return;
    }
    list = await r.json();
    setPill('pill-weather', list.length ? 'ok' : 'warn', 'weather');
  } catch (_e) {
    setPill('pill-weather', 'down', 'weather');
    return;
  }
  weatherList = list;
  rerenderWeather();
}

// Cache the most recent /api/weather response so chip toggles
// can refilter without waiting for the next poll.
let weatherList = [];

function rerenderWeather() {
  const el = document.getElementById('weather');
  if (!el) return;
  // Filter to locations whose area is in the active chips. The
  // default (unlinked) location is hidden — every TSO zone has
  // its own location in config.lisp. Order respects ALL_FILTER_AREAS
  // so DE shows up before the international zones.
  const order = new Map(ALL_FILTER_AREAS.map((a, i) => [a.code, i]));
  const filtered = weatherList
    .filter(l => l.area_code && activeAreas.has(l.area_code))
    .sort((a, b) => (order.get(a.area_code) ?? 999) - (order.get(b.area_code) ?? 999));
  if (!filtered.length) {
    el.innerHTML = '<i>no weather for the active areas</i>';
    return;
  }
  el.innerHTML = filtered.map(l => {
    const tag = areaTag(l.area_code);
    return `<div class="weather-cell" onclick="toggleWeatherCell(event)">
      <div class="weather-head">
        <span class="area-badge">${escapeHtml(tag)}</span>
        <span class="muted">☁ ${l.cloud_cover.toFixed(2)}</span>
      </div>
      <div class="weather-metric">solar <span class="muted">${Math.round(l.solar_now)} W/m²</span></div>
      <div class="weather-metric">wind <span class="muted">${l.wind_now.toFixed(1)} m/s</span></div>
      <div class="weather-metric">temp <span class="muted">${l.temp_c_now.toFixed(1)} °C</span></div>
      <div class="weather-detail">
        lat ${l.lat.toFixed(1)} · lon ${l.lon.toFixed(1)}<br>
        wind direction ${Math.round(l.wind_direction)}°<br>
        mean wind ${l.mean_wind.toFixed(1)} m/s
      </div>
    </div>`;
  }).join('');
}

function toggleWeatherCell(ev) {
  ev.currentTarget.classList.toggle('open');
}

async function loadScenarios() {
  const r = await fetch('/api/scenarios');
  if (!r.ok) return;
  const list = await r.json();
  const el = document.getElementById('scenarios');
  // If the user is mid-interaction with a control inside this
  // panel — typically the open <select> picker — skip the
  // innerHTML rewrite that would destroy the dropdown and snap
  // it shut. Next poll picks up where we left off.
  if (el && el.contains(document.activeElement)) return;
  const active = list.find(s => s.current_stage !== null && s.current_stage !== undefined);

  if (!list.length) {
    el.innerHTML = '<i>no scenarios registered</i>';
  } else if (active) {
    // Active one gets the whole panel; the rest collapse into a
    // switch-to dropdown so the timeline + controls breathe.
    const inactive = list.filter(s => s !== active);
    el.innerHTML = renderScenarioActive(active, encodeURIComponent(active.name))
      + renderScenarioSwitcher(inactive);
  } else {
    el.innerHTML = renderScenarioIdlePicker(list);
  }

  const indicator = document.getElementById('scenario-indicator');
  if (active) {
    const stage = active.stages[active.current_stage];
    const stageName = (stage && stage.name) || '?';
    indicator.textContent = `${active.name} · ${stageName} (${active.current_stage + 1}/${active.stages.length})`;
    indicator.classList.remove('muted');
  } else {
    indicator.textContent = '—';
    indicator.classList.add('muted');
  }
}

function renderScenarioIdlePicker(list) {
  const opts = list
    .map(s => `<option value="${encodeURIComponent(s.name)}">${escapeHtml(s.name)} — ${escapeHtml(s.description)}</option>`)
    .join('');
  return `<div class="scenario-picker">
    <span class="muted">Start a scenario:</span>
    <select onchange="if (this.value) scenarioAction(this.value, 'start')">
      <option value="">— pick one —</option>
      ${opts}
    </select>
  </div>`;
}

function renderScenarioSwitcher(others) {
  if (!others.length) return '';
  const opts = others
    .map(s => `<option value="${encodeURIComponent(s.name)}">${escapeHtml(s.name)}</option>`)
    .join('');
  return `<div class="scenario-picker">
    <span class="muted">Switch to:</span>
    <select onchange="if (this.value) switchScenario(this.value)">
      <option value="">—</option>
      ${opts}
    </select>
  </div>`;
}

// After firing a scenario action, blur whatever was focused
// (typically the picker <select>) so loadScenarios's
// focus-guard doesn't skip the immediate re-render. Without
// this the user clicks an item, the select keeps focus, the
// next poll is skipped, and the panel doesn't update until the
// poll after that — visible as a ~2 s lag between picking and
// the active panel showing up.
function blurActive() {
  const a = document.activeElement;
  if (a && typeof a.blur === 'function') a.blur();
}

async function switchScenario(name) {
  // Stop whichever scenario is currently active (if any), then
  // start the picked one. Best-effort: errors are swallowed because
  // the next loadScenarios() poll will reflect the truth either way.
  const r = await fetch('/api/scenarios');
  if (r.ok) {
    const list = await r.json();
    const active = list.find(s => s.current_stage !== null && s.current_stage !== undefined);
    if (active) {
      await fetch(`/api/scenarios/${encodeURIComponent(active.name)}/stop`, { method: 'POST' });
    }
  }
  await fetch(`/api/scenarios/${name}/start`, { method: 'POST' });
  blurActive();
  loadScenarios();
}

async function scenarioAction(name, action) {
  await fetch(`/api/scenarios/${name}/${action}`, { method: 'POST' });
  blurActive();
  loadScenarios();
}

// WebSocket reconnect backoff — 1s, 2s, 4s, 8s, capped at 30s.
// Reset to 1s on a successful connect (`onopen`). Stops the client
// from hammering the server every second when it's down for a
// while; once the server is back the next attempt fires within
// 1-30s instead of 1s.
const WS_BACKOFF_MIN_MS = 1000;
const WS_BACKOFF_MAX_MS = 30000;
let tradesWsBackoff = WS_BACKOFF_MIN_MS;
let bookWsBackoff = WS_BACKOFF_MIN_MS;

function openTradesWs() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const ws = new WebSocket(`${proto}//${location.host}/ws/public-trades`);
  ws.onopen = () => {
    setPill('pill-trades', 'ok', 'trades');
    tradesWsBackoff = WS_BACKOFF_MIN_MS;
  };
  ws.onmessage = (e) => {
    const t = JSON.parse(e.data);
    recordTrade(t);
    recordTradePrice(t);
    pushTrade(t);
  };
  ws.onclose = () => {
    setPill('pill-trades', 'down', 'trades');
    setTimeout(openTradesWs, tradesWsBackoff);
    tradesWsBackoff = Math.min(tradesWsBackoff * 2, WS_BACKOFF_MAX_MS);
  };
}

// Client-side book state: Map<orderId, {area, period, side, price, qty}>.
// Each WS message updates one entry; qty=0 removes it. Then we
// re-render aggregated by (area, period, side, price).
const bookState = new Map();
let bookDirty = false;

// Same array as ALL_AREAS — kept as an alias for the places that
// already use the longer name to talk about "every chip the filter
// row knows about".
const ALL_FILTER_AREAS = ALL_AREAS;
const AREA_TAGS = Object.fromEntries(ALL_FILTER_AREAS.map(a => [a.code, a.tag]));
function areaTag(code) {
  return AREA_TAGS[code] || code.slice(0, 4);
}

const activeAreas = new Set(ALL_FILTER_AREAS.filter(a => a.group === 'de').map(a => a.code));
let showNeighbours = false;
/** Delivery-start string (RFC-3339) when the user has drilled into one
 *  contract period across all areas; null otherwise. Area filtering
 *  via chips remains orthogonal — both compose. */
let periodFilter = null;

function setPeriodFilter(period) {
  if (!period) return;
  periodFilter = period;
  // Sync the single-contract book so a click on a trade row in TN
  // lands on the same delivery the user is looking at.
  bookContract.period = period;
  renderPeriodPill();
  rerenderBook();
  rerenderTrades();
}

function clearPeriodFilter() {
  periodFilter = null;
  renderPeriodPill();
  rerenderBook();
  rerenderTrades();
}

function renderPeriodPill() {
  const bar = document.getElementById('contract-filter-bar');
  const label = document.getElementById('contract-pill-label');
  if (!bar || !label) return;
  if (periodFilter) {
    bar.style.display = '';
    label.textContent = `delivery ${shortTime(periodFilter)}`;
  } else {
    bar.style.display = 'none';
  }
}

function toggleArea(code) {
  if (activeAreas.has(code)) activeAreas.delete(code);
  else activeAreas.add(code);
  renderFilterChips();
  rerenderBook();
  rerenderTrades();
  rerenderWeather();
}

function toggleNeighbours() {
  showNeighbours = !showNeighbours;
  for (const a of ALL_FILTER_AREAS) {
    if (a.group !== 'intl') continue;
    if (showNeighbours) activeAreas.add(a.code);
    else activeAreas.delete(a.code);
  }
  renderFilterChips();
  rerenderBook();
  rerenderTrades();
  rerenderWeather();
}

function renderFilterChips() {
  const el = document.getElementById('filter-chips');
  if (!el) return;
  const visible = ALL_FILTER_AREAS.filter(a => a.group === 'de' || showNeighbours);
  const chips = visible
    .map(a => `<span class="chip${activeAreas.has(a.code) ? ' active' : ''}" onclick="toggleArea('${a.code}')">${a.tag}</span>`)
    .join('');
  const nb = `<span class="chip${showNeighbours ? ' active' : ''}" onclick="toggleNeighbours()">${showNeighbours ? '−' : '+'} neighbours</span>`;
  el.innerHTML = chips + nb;
}

// Public-trade tape — JS-side ring of the last 500 prints. Each
// rerender takes the 50 latest matching the current filters
// (area chips, focused-period pill, trades-delivery dropdown) so
// "last 50" stays true regardless of which combination is on.
const TRADES_BUFFER_CAP = 500;
const TRADES_DISPLAY_CAP = 50;
const tradesBuffer = [];
let tradesDirty = false;
let tradesPeriodFilter = null; // null = all deliveries

function pushTrade(t) {
  tradesBuffer.unshift(t);
  if (tradesBuffer.length > TRADES_BUFFER_CAP) {
    tradesBuffer.length = TRADES_BUFFER_CAP;
  }
  tradesDirty = true;
}

function selectTradesPeriod(value) {
  tradesPeriodFilter = value === 'all' ? null : value;
  rememberSelectChoice('trades', value);
  blurActive();
  rerenderTrades();
}

function tradeMatchesActiveFilters(t) {
  if (periodFilter && t.period !== periodFilter) return false;
  if (tradesPeriodFilter && t.period !== tradesPeriodFilter) return false;
  return activeAreas.has(t.buy_area) || activeAreas.has(t.sell_area);
}

function renderTradeRow(t) {
  const area = t.buy_area === t.sell_area
    ? `<span class="area-badge">${areaTag(t.buy_area)}</span>`
    : `<span class="area-badge">${areaTag(t.buy_area)}</span><span class="area-cross">→</span><span class="area-badge">${areaTag(t.sell_area)}</span>`;
  return `<tr onclick="setPeriodFilter('${String(t.period).replace(/'/g, '%27')}')">` +
    `<td>#${t.id}</td><td>${t.quantity}</td><td>${t.price}</td>` +
    `<td>${area}</td>` +
    `<td class="muted">${shortTime(t.period)}</td>` +
    `<td class="muted">${shortTime(t.execution_time)}</td>` +
    `</tr>`;
}

function rerenderTrades() {
  const tbody = document.getElementById('trades');
  if (!tbody) return;
  // Keep the delivery dropdown's options in sync with whatever
  // periods we've seen in the buffer; skip while the user is
  // focused on the <select> so it doesn't snap shut.
  const sel = document.getElementById('trades-period-select');
  if (sel && document.activeElement !== sel) {
    const periods = new Set();
    for (const t of tradesBuffer) periods.add(t.period);
    const sorted = Array.from(periods).sort();
    // Drop a stale persisted filter — the contract may have
    // expired between page loads.
    if (tradesPeriodFilter && !sorted.includes(tradesPeriodFilter)) {
      tradesPeriodFilter = null;
      rememberSelectChoice('trades', null);
    }
    const opts = ['<option value="all">All delivery periods</option>']
      .concat(sorted.map(p => {
        const sel = p === tradesPeriodFilter ? ' selected' : '';
        return `<option value="${p}"${sel}>${shortTime(p)}</option>`;
      }))
      .join('');
    sel.innerHTML = opts;
  }
  const visible = [];
  for (const t of tradesBuffer) {
    if (!tradeMatchesActiveFilters(t)) continue;
    visible.push(t);
    if (visible.length === TRADES_DISPLAY_CAP) break;
  }
  tbody.innerHTML = visible.map(renderTradeRow).join('');
}

const TOP_LEVELS = 3;
const ladderExpanded = new Set();

function toggleLadder(key) {
  if (ladderExpanded.has(key)) ladderExpanded.delete(key);
  else ladderExpanded.add(key);
  rerenderBook();
}

function ladderRow(side, price, qty, maxQ) {
  const pct = Math.max(2, Math.round((qty / maxQ) * 100));
  return `<div class="ladder-row ${side}">
    <span class="price">${parseFloat(price).toFixed(2)}</span>
    <span class="qty">${qty.toFixed(1)}</span>
    <span class="bar" style="width:${pct}%"></span>
  </div>`;
}

function renderLadder(g) {
  const key = `${g.area}|${g.period}`;
  const expanded = ladderExpanded.has(key);
  const asks = Array.from(g.asks.entries())
    .map(([p, q]) => [parseFloat(p), q])
    .sort((a, b) => a[0] - b[0]);
  const bids = Array.from(g.bids.entries())
    .map(([p, q]) => [parseFloat(p), q])
    .sort((a, b) => b[0] - a[0]);
  const maxQ = Math.max(0.01, ...asks.map(x => x[1]), ...bids.map(x => x[1]));

  const visAsks = expanded ? asks : asks.slice(0, TOP_LEVELS);
  const visBids = expanded ? bids : bids.slice(0, TOP_LEVELS);

  // asks rendered worst-to-best so the lowest ask sits next to the
  // spread row at the centre of the ladder.
  const askRows = visAsks
    .slice()
    .reverse()
    .map(([p, q]) => ladderRow('ask', p, q, maxQ))
    .join('');
  const bidRows = visBids
    .map(([p, q]) => ladderRow('bid', p, q, maxQ))
    .join('');

  const bestBid = bids[0]?.[0];
  const bestAsk = asks[0]?.[0];
  const mid = (bestBid != null && bestAsk != null)
    ? ((bestBid + bestAsk) / 2).toFixed(2)
    : '—';
  const spread = (bestBid != null && bestAsk != null)
    ? (bestAsk - bestBid).toFixed(2)
    : '—';

  const safeKey = key.replace(/'/g, '%27');
  const askMore = (!expanded && asks.length > TOP_LEVELS)
    ? `<div class="ladder-more" onclick="toggleLadder('${safeKey}')">+ ${asks.length - TOP_LEVELS} more asks</div>`
    : '';
  const bidMore = (!expanded && bids.length > TOP_LEVELS)
    ? `<div class="ladder-more" onclick="toggleLadder('${safeKey}')">+ ${bids.length - TOP_LEVELS} more bids</div>`
    : '';
  const collapse = expanded
    ? `<div class="ladder-more" onclick="toggleLadder('${safeKey}')">collapse</div>`
    : '';

  return `
    <div class="ladder">
      <div class="ladder-head">
        <span><span class="area-badge">${areaTag(g.area)}</span></span>
        <span class="midprice">mid ${mid}</span>
      </div>
      ${askMore}
      ${askRows || '<div class="ladder-empty">no asks</div>'}
      <div class="ladder-spread">spread ${spread}</div>
      ${bidRows || '<div class="ladder-empty">no bids</div>'}
      ${bidMore}
      ${collapse}
    </div>`;
}

/** Delivery period the book panel is currently showing. Area
 *  selection comes from the global `activeAreas` set (the
 *  filter chips), so the book always reflects the user's
 *  current scope. `period` is null until the first render
 *  picks a default. */
const bookContract = { period: null };
const BOOK_VISIBLE_CAP = 4;
let bookShowAll = false;

function selectBookPeriod(period) {
  bookContract.period = period;
  rememberSelectChoice('book', period);
  rerenderBook();
}

function toggleBookShowAll() {
  bookShowAll = !bookShowAll;
  rerenderBook();
}

function rerenderBook() {
  // Group rows by (area, period). Aggregate bids/asks per level.
  // Closed-gate contracts (period.start in the past) get dropped
  // belt-and-suspenders for missed qty=0 events.
  const groups = new Map();
  const cutoff = Date.now();
  for (const r of bookState.values()) {
    if (r.period) {
      const ts = Date.parse(r.period);
      if (!Number.isNaN(ts) && ts <= cutoff) continue;
    }
    const key = `${r.area}|${r.period}`;
    let g = groups.get(key);
    if (!g) {
      g = { area: r.area, period: r.period, bids: new Map(), asks: new Map() };
      groups.set(key, g);
    }
    const target = r.side === 1 ? g.bids : (r.side === 2 ? g.asks : null);
    if (target == null) continue;
    const prev = target.get(r.price) || 0;
    const next = prev + parseFloat(r.qty || '0');
    target.set(r.price, next);
  }

  // Available delivery periods across ALL areas — the period
  // dropdown lets the user pick any contract anyone is quoting,
  // independent of which areas they currently have selected.
  const allPeriods = new Set();
  for (const g of groups.values()) allPeriods.add(g.period);
  const periods = Array.from(allPeriods).sort();
  if (!bookContract.period || !periods.includes(bookContract.period)) {
    // Drop a stale persisted period (contract expired between
    // reloads) before falling back to the soonest available.
    if (bookContract.period && !periods.includes(bookContract.period)) {
      rememberSelectChoice('book', null);
    }
    bookContract.period = periods[0] || null;
  }
  const periodSel = document.getElementById('book-period-select');
  // Skip rewriting the <select> while the user is interacting
  // with it — replacing innerHTML on every 200 ms tick would
  // close the open dropdown before they can pick.
  if (periodSel && document.activeElement !== periodSel) {
    periodSel.innerHTML = periods
      .map(
        p => `<option value="${p}"${p === bookContract.period ? ' selected' : ''}>${shortTime(p)}</option>`
      )
      .join('');
  }

  const container = document.getElementById('book');
  if (!container) return;
  if (!bookContract.period) {
    container.innerHTML = '<div class="ladder-empty"><i>no contracts have resting orders yet</i></div>';
    return;
  }

  // One ladder per active area for the selected period. DE TSO
  // zones come first (tn → bw), then international (fr → at), so
  // the home market is always on the left. Empty groups are kept
  // (and rendered with "no asks / no bids" placeholders) so the
  // card itself doesn't flicker every couple of seconds when the
  // MM rotates quotes — the cancel arrives a hair before the
  // repost, leaving a brief no-orders window the renderer used to
  // collapse into "no resting orders in active areas".
  const matches = ALL_FILTER_AREAS
    .filter(a => activeAreas.has(a.code))
    .map(a => groups.get(`${a.code}|${bookContract.period}`) ?? {
      area: a.code,
      period: bookContract.period,
      bids: new Map(),
      asks: new Map(),
    });

  if (matches.length === 0) {
    container.innerHTML = '<div class="ladder-empty"><i>no areas selected</i></div>';
    return;
  }

  const visible = bookShowAll ? matches : matches.slice(0, BOOK_VISIBLE_CAP);
  const hidden = matches.length - visible.length;
  const row = `<div class="book-row">${visible.map(renderLadder).join('')}</div>`;
  let footer = '';
  if (hidden > 0) {
    footer = `<div class="book-show-more" onclick="toggleBookShowAll()">+ ${hidden} more area${hidden === 1 ? '' : 's'}</div>`;
  } else if (bookShowAll && matches.length > BOOK_VISIBLE_CAP) {
    footer = '<div class="book-show-more" onclick="toggleBookShowAll()">collapse</div>';
  }
  container.innerHTML = row + footer;
}

function openBookWs() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const ws = new WebSocket(`${proto}//${location.host}/ws/public-book`);
  ws.onopen = () => {
    // Wipe any state from the previous connection — the server
    // re-emits a full snapshot on every connect, so anything we
    // already had is either reflected in that snapshot or has
    // since been cancelled. Without this clear, ghost orders left
    // over from a Lagged disconnect would accumulate.
    bookState.clear();
    bookDirty = true;
    setPill('pill-book', 'ok', 'book');
    bookWsBackoff = WS_BACKOFF_MIN_MS;
  };
  ws.onmessage = (e) => {
    const r = JSON.parse(e.data);
    const qty = parseFloat(r.quantity || '0');
    if (qty <= 0) {
      bookState.delete(r.id);
    } else {
      bookState.set(r.id, {
        area: r.area,
        period: r.period || '',
        side: r.side,
        price: r.price,
        qty: r.quantity,
      });
    }
    bookDirty = true;
  };
  ws.onclose = () => {
    setPill('pill-book', 'down', 'book');
    setTimeout(openBookWs, bookWsBackoff);
    bookWsBackoff = Math.min(bookWsBackoff * 2, WS_BACKOFF_MAX_MS);
  };
}

// Re-render at most ~5 times/sec; the MM refreshes generate enough
// churn that per-message rerenders would overdraw needlessly.
setInterval(() => {
  if (bookDirty) {
    bookDirty = false;
    rerenderBook();
  }
  if (tradesDirty) {
    tradesDirty = false;
    rerenderTrades();
  }
}, 200);

(async () => {
  initDensity();
  initChartWindow();
  // Restore book + trades dropdown choices from localStorage so
  // the first render uses the user's last picks instead of the
  // defaults. Validation against the live period set happens
  // inside rerenderBook / rerenderTrades — stale contracts that
  // already expired get cleared there.
  bookContract.period = rememberedSelectChoice('book') || null;
  const tradesSaved = rememberedSelectChoice('trades');
  tradesPeriodFilter = tradesSaved && tradesSaved !== 'all' ? tradesSaved : null;
  // Pull the sim's timezone first so the very first tickClock /
  // renderTrades / renderBook formats in the right zone — without
  // this they'd render in UTC for a frame, then snap.
  await loadClock();
  tickClock();
  renderSparkbars();
  renderChartLegend();
  renderFilterChips();
  drawChart();
  setPill('pill-trades', 'down', 'trades');
  setPill('pill-book', 'down', 'book');
  setPill('pill-weather', 'down', 'weather');
  await Promise.all([loadInfo(), loadGridpools(), loadScenarios(), loadWeather()]);
  setInterval(refreshGridpoolDrilldown, 3000);
  setInterval(loadScenarios, 2000);
  setInterval(loadWeather, 10000);
  setInterval(tickClock, 1000);
  setInterval(rotateSparkBuckets, SPARK_BUCKET_MS);
  setInterval(renderSparkbars, 1000);
  setInterval(drawChart, 1000);
  window.addEventListener('resize', drawChart);
  openTradesWs();
  openBookWs();
})();
