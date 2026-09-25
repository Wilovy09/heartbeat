// Alpine component backing templates/dashboard.html (uptime view) and templates/logs.html
// (heartbeat strip per app card). Polls this app's own /api/uptime (every registered
// app's status + recent heartbeats) and, on the dashboard with one app selected (URL hash =
// its slug), /api/uptime/{slug} for the response-time chart.
const BAR_SLOTS = 40;
// The detail strip is wide: it draws the full history the server sends.
const STRIP_SLOTS = 100;
const EVENTS_COLLAPSED = 8;
const REFRESH_MS = 30000;
// Windows longer than this carry thousands of heartbeats -- refetched only when the range
// changes, not on every auto-refresh tick.
const LIVE_DETAIL_MAX_HOURS = 24;
const CHART = { height: 260, left: 48, right: 12, top: 12, bottom: 26 };
// Mirrors the rhythm/ink tokens in base.html (SVG attributes can't read CSS variables).
const COLOR = { up: '#3fd68a', degraded: '#f0b43c', down: '#f0514e', ink3: '#6e737c', ink4: '#454a52' };
const FONT_MONO = '"IBM Plex Mono", ui-monospace, monospace';
const RANGES = [
  { hours: 1, label: '1h' },
  { hours: 6, label: '6h' },
  { hours: 24, label: '24h' },
  { hours: 168, label: '7d' },
  { hours: 720, label: '30d' },
];
// Triage order: what needs a human first.
const SEVERITY = { down: 0, degraded: 1, up: 2, unknown: 3 };

function uptimeDashboard() {
  return {
    overview: { interval_secs: 60, degraded_after_ms: 1000, monitors: [], events: [] },
    detail: { beats: [], events: [] },
    selected: null,
    hours: 6,
    search: '',
    loadError: '',
    chartWidth: 0,
    hover: null,
    showAllEvents: false,
    RANGES,
    STRIP_SLOTS,

    async init() {
      this.selected = location.hash.slice(1) || null;
      window.addEventListener('hashchange', () => {
        this.selected = location.hash.slice(1) || null;
        this.hover = null;
        this.showAllEvents = false;
        this.detail = { beats: [], events: [] };
        this.loadDetail();
      });
      await this.refresh();
      setInterval(() => this.refresh(), REFRESH_MS);
    },

    async fetchJson(url) {
      const resp = await fetch(url, { credentials: 'same-origin' });
      if (resp.status === 401) {
        location.href = '/login';
        return null;
      }
      const body = await resp.json();
      if (!resp.ok) throw new Error(body.error || `HTTP ${resp.status}`);
      return body;
    },

    async refresh() {
      try {
        const overview = await this.fetchJson('/api/uptime');
        if (!overview) return;
        this.overview = overview;
        this.loadError = '';
      } catch (e) {
        this.loadError = `No se pudo cargar el estado: ${e.message}`;
      }
      if (this.hours <= LIVE_DETAIL_MAX_HOURS || this.detail.beats.length === 0) {
        await this.loadDetail();
      }
    },

    async loadDetail() {
      const m = this.current();
      if (!m || !m.health_url) return;
      try {
        const detail = await this.fetchJson(`/api/uptime/${encodeURIComponent(m.slug)}?hours=${this.hours}`);
        if (detail && this.selected === m.slug) this.detail = detail;
      } catch (e) {
        this.loadError = `No se pudo cargar el historial: ${e.message}`;
      }
    },

    current() {
      return this.overview.monitors.find((m) => m.slug === this.selected) || null;
    },

    filteredMonitors() {
      const q = this.search.trim().toLowerCase();
      return this.overview.monitors
        .filter((m) => !q || m.name.toLowerCase().includes(q) || m.slug.includes(q))
        .sort((a, b) => SEVERITY[this.statusKey(a.status)] - SEVERITY[this.statusKey(b.status)] || a.name.localeCompare(b.name));
    },

    // Worst state across all apps -- what the overview's verdict reports.
    overallStatus() {
      const keys = this.overview.monitors.map((m) => this.statusKey(m.status));
      return ['down', 'degraded', 'up'].find((k) => keys.includes(k)) || 'unknown';
    },

    overallMessage() {
      const down = this.count('down');
      const degraded = this.count('degraded');
      const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;
      if (down) return plural(down, 'app caída', 'apps caídas');
      if (degraded) return plural(degraded, 'app degradada', 'apps degradadas');
      if (this.count('up')) return 'Todo en orden';
      return 'Esperando la primera lectura';
    },

    attention() {
      return this.filteredMonitors().filter((m) => m.status === 'down' || m.status === 'degraded');
    },

    lastMessage(m) {
      const last = m.recent[m.recent.length - 1];
      if (!last) return '';
      return m.status === 'degraded' ? `${last.latency_ms} ms · ${last.message}` : last.message;
    },

    // Left label under the big strip: age of its oldest drawn bar.
    stripStart(m, slots = BAR_SLOTS) {
      // Must match the dashboard CSS that hides all but the latest 40 bars on phones.
      if (slots === STRIP_SLOTS && window.matchMedia('(max-width: 720px)').matches) slots = BAR_SLOTS;
      const drawn = m.recent.slice(-slots);
      return drawn.length ? this.fmtAgo(drawn[0].at) : '';
    },

    setHours(h) {
      if (this.hours === h) return;
      this.hours = h;
      this.hover = null;
      this.loadDetail();
    },

    count(key) {
      return this.overview.monitors.filter((m) => this.statusKey(m.status) === key).length;
    },

    monitorFor(slug) {
      return this.overview.monitors.find((m) => m.slug === slug) || null;
    },

    recentFor(slug) {
      return this.monitorFor(slug)?.recent || [];
    },

    // Right-aligned like a timeline: empty slots on the left until there's enough history.
    padBeats(recent, slots = BAR_SLOTS) {
      const beats = recent.slice(-slots);
      return Array(slots - beats.length).fill(null).concat(beats);
    },

    // Long event lists collapse to the latest few until asked.
    visibleEvents(events) {
      return this.showAllEvents ? events : events.slice(0, EVENTS_COLLAPSED);
    },

    statusKey(status) {
      return status || 'unknown';
    },
    statusLabel(status) {
      return { up: 'Up', degraded: 'Degradado', down: 'Down' }[status] || 'Sin datos';
    },
    beatTitle(b) {
      return `${this.fmtDateTime(b.at)} — ${this.statusLabel(b.status)} — ${this.fmtMs(b.latency_ms)}\n${b.message}`;
    },
    fmtPct(v) {
      return v == null ? '—' : `${v.toFixed(v === 100 ? 0 : 2)}%`;
    },
    fmtMs(v) {
      return v == null ? '—' : `${v} ms`;
    },
    fmtDateTime(at) {
      const d = new Date(at * 1000);
      const date = d.toLocaleDateString('es-MX', { day: '2-digit', month: 'short' }).replace('.', '');
      const time = d.toLocaleTimeString('es-MX', { hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false });
      return `${date} ${time}`;
    },
    fmtAgo(at) {
      const secs = Math.max(0, Math.round(Date.now() / 1000 - at));
      if (secs < 90) return `hace ${secs} s`;
      if (secs < 5400) return `hace ${Math.round(secs / 60)} min`;
      return `hace ${Math.round(secs / 3600)} h`;
    },

    observeChart(el) {
      this.chartWidth = el.clientWidth;
      new ResizeObserver(() => { this.chartWidth = el.clientWidth; }).observe(el);
    },

    // Shared x/y mapping for chartSvg() and onChartMove().
    chartScale() {
      const now = Date.now() / 1000;
      const from = now - this.hours * 3600;
      const plotW = Math.max(1, this.chartWidth - CHART.left - CHART.right);
      const plotH = CHART.height - CHART.top - CHART.bottom;
      // reduce, not Math.max(...beats): a 30-day window can exceed the engine's argument limit.
      const maxLatency = this.detail.beats.reduce((max, b) => (b.status !== 'down' ? Math.max(max, b.latency_ms || 0) : max), 0);
      const yMax = niceCeil(maxLatency || 100);
      return {
        from, now, plotW, plotH, yMax,
        x: (at) => CHART.left + ((at - from) / (now - from)) * plotW,
        y: (ms) => CHART.top + plotH - (ms / yMax) * plotH,
      };
    },

    chartSvg() {
      if (!this.chartWidth) return '';
      const s = this.chartScale();
      const w = this.chartWidth;
      const parts = [`<svg width="${w}" height="${CHART.height}" role="img" aria-label="Tiempo de respuesta">`];

      // ECG paper: a fine 8px grid with a heavier line every 5 squares, anchored to the
      // plot's top-left corner so it lines up with the axes.
      parts.push(`<defs>
        <pattern id="ecg-minor" width="8" height="8" patternUnits="userSpaceOnUse" x="${CHART.left}" y="${CHART.top}">
          <path d="M8 0H0V8" fill="none" stroke="rgba(255,255,255,0.028)" stroke-width="1"/>
        </pattern>
        <pattern id="ecg-major" width="40" height="40" patternUnits="userSpaceOnUse" x="${CHART.left}" y="${CHART.top}">
          <rect width="40" height="40" fill="url(#ecg-minor)"/>
          <path d="M40 0H0V40" fill="none" stroke="rgba(255,255,255,0.055)" stroke-width="1"/>
        </pattern>
      </defs>`);
      parts.push(`<rect x="${CHART.left}" y="${CHART.top}" width="${s.plotW}" height="${s.plotH}" fill="url(#ecg-major)" stroke="rgba(255,255,255,0.075)"/>`);

      for (let i = 0; i <= 4; i++) {
        const ms = (s.yMax / 4) * i;
        const y = s.y(ms);
        parts.push(`<text x="${CHART.left - 8}" y="${y + 4}" text-anchor="end" font-size="11" font-family='${FONT_MONO}' fill="${COLOR.ink3}">${Math.round(ms)}</text>`);
      }
      for (const at of timeTicks(s.from, s.now, s.plotW)) {
        parts.push(`<text x="${s.x(at)}" y="${CHART.height - 6}" text-anchor="middle" font-size="11" font-family='${FONT_MONO}' fill="${COLOR.ink3}">${tickLabel(at, this.hours)}</text>`);
      }

      // One bucket per 2px column so a 30-day window (tens of thousands of beats) still
      // draws a path the browser can handle: average latency of the Up/Degraded beats, and a red
      // band if any beat in the column was Down.
      const cols = Math.max(1, Math.floor(s.plotW / 2));
      const span = (s.now - s.from) / cols;
      const buckets = new Map();
      for (const b of this.detail.beats) {
        const col = Math.min(cols - 1, Math.floor((b.at - s.from) / span));
        if (col < 0) continue;
        const bucket = buckets.get(col) || { sum: 0, n: 0, down: false };
        if (b.status === 'down') bucket.down = true;
        else if (b.latency_ms != null) { bucket.sum += b.latency_ms; bucket.n += 1; }
        buckets.set(col, bucket);
      }
      const colX = (col) => CHART.left + (col + 0.5) * (s.plotW / cols);
      const bandW = Math.max(2, s.plotW / cols);
      const baseY = s.y(0);

      let line = '';
      let area = '';
      let segment = [];
      const flush = () => {
        if (segment.length === 0) return;
        line += 'M' + segment.map(([x, y]) => `${x},${y}`).join('L');
        area += `M${segment[0][0]},${baseY}L` + segment.map(([x, y]) => `${x},${y}`).join('L') + `L${segment[segment.length - 1][0]},${baseY}Z`;
        segment = [];
      };
      [...buckets.keys()].sort((a, b) => a - b).forEach((col, i, keys) => {
        const bucket = buckets.get(col);
        if (bucket.down) {
          parts.push(`<rect x="${colX(col) - bandW / 2}" y="${CHART.top}" width="${bandW}" height="${s.plotH}" fill="rgba(240,81,78,0.16)"/>`);
          parts.push(`<rect x="${colX(col) - bandW / 2}" y="${CHART.top}" width="${bandW}" height="3" fill="${COLOR.down}"/>`);
        }
        // A gap longer than a few check intervals (or a Down) breaks the line instead of
        // drawing a misleading straight run across missing data.
        const prev = keys[i - 1];
        // Adjacent columns never break: when one column spans more than a few intervals
        // (narrow chart, 7d/30d range) a 1-column step is continuous data, not a gap.
        const gapLimit = Math.max(this.overview.interval_secs * 3, span * 1.5);
        if (prev != null && (col - prev) * span > gapLimit) flush();
        if (bucket.n === 0) { flush(); return; }
        segment.push([colX(col), s.y(bucket.sum / bucket.n)]);
      });
      flush();

      // The line turns yellow above the "degraded" threshold: a vertical gradient with a
      // hard stop at the threshold's y, in user space so it lines up with the axis.
      const threshold = this.overview.degraded_after_ms;
      const cut = threshold < s.yMax ? (s.y(threshold) - CHART.top) / s.plotH : 0;
      parts.push(`<defs>
        <linearGradient id="lat-stroke" gradientUnits="userSpaceOnUse" x1="0" x2="0" y1="${CHART.top}" y2="${baseY}">
          <stop offset="0" stop-color="${COLOR.degraded}"/><stop offset="${cut}" stop-color="${COLOR.degraded}"/>
          <stop offset="${cut}" stop-color="${COLOR.up}"/><stop offset="1" stop-color="${COLOR.up}"/>
        </linearGradient>
      </defs>`);
      parts.push(`<path d="${area}" fill="url(#lat-stroke)" fill-opacity="0.08" stroke="none"/>`);
      parts.push(`<path d="${line}" fill="none" stroke="url(#lat-stroke)" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>`);
      if (threshold < s.yMax) {
        const ty = s.y(threshold);
        parts.push(`<line x1="${CHART.left}" x2="${w - CHART.right}" y1="${ty}" y2="${ty}" stroke="${COLOR.degraded}" stroke-width="1" stroke-dasharray="3 5" opacity="0.6"/>`);
        parts.push(`<text x="${w - CHART.right - 6}" y="${ty - 6}" text-anchor="end" font-size="11" font-family='${FONT_MONO}' fill="${COLOR.degraded}">umbral ${threshold} ms</text>`);
      }

      if (this.hover) {
        parts.push(`<line x1="${this.hover.x}" x2="${this.hover.x}" y1="${CHART.top}" y2="${baseY}" stroke="${COLOR.ink3}" stroke-width="1"/>`);
        if (this.hover.dotY != null) {
          parts.push(`<circle cx="${this.hover.x}" cy="${this.hover.dotY}" r="5" fill="${COLOR[this.hover.beat.status]}" stroke="#111316" stroke-width="2"/>`);
        }
      }
      if (this.detail.beats.length === 0) {
        parts.push(`<text x="${w / 2}" y="${CHART.height / 2}" text-anchor="middle" font-size="13" fill="${COLOR.ink3}">Sin lecturas en este rango</text>`);
      }
      parts.push('</svg>');
      return parts.join('');
    },

    onChartMove(ev) {
      const beats = this.detail.beats;
      if (!beats.length || !this.chartWidth) return;
      const rect = ev.currentTarget.getBoundingClientRect();
      const px = ev.clientX - rect.left;
      const s = this.chartScale();
      const at = s.from + ((px - CHART.left) / s.plotW) * (s.now - s.from);
      const beat = nearestBeat(beats, at);
      const x = s.x(beat.at);
      const dotY = beat.status !== 'down' && beat.latency_ms != null ? s.y(beat.latency_ms) : null;
      this.hover = {
        beat, x, dotY,
        left: Math.min(Math.max(x, 140), this.chartWidth - 140),
        top: dotY ?? CHART.top + s.plotH / 2,
      };
    },
  };
}

// Binary search on `at` (beats are oldest first).
function nearestBeat(beats, at) {
  let lo = 0;
  let hi = beats.length - 1;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (beats[mid].at < at) lo = mid + 1;
    else hi = mid;
  }
  if (lo > 0 && Math.abs(beats[lo - 1].at - at) < Math.abs(beats[lo].at - at)) return beats[lo - 1];
  return beats[lo];
}

// Rounds up to 1/2/5 × 10^n so the y-axis gridlines land on readable numbers.
function niceCeil(v) {
  const pow = 10 ** Math.floor(Math.log10(v));
  const step = [1, 2, 5, 10].find((m) => m * pow >= v);
  return step * pow;
}

function timeTicks(from, to, plotW) {
  const count = Math.max(2, Math.floor(plotW / 110));
  const step = (to - from) / count;
  // Interior ticks only -- labels centered on the plot edges would get clipped.
  return Array.from({ length: count - 1 }, (_, i) => from + (i + 1) * step);
}

function tickLabel(at, hours) {
  const d = new Date(at * 1000);
  if (hours > 24) return d.toLocaleDateString('es-MX', { day: '2-digit', month: 'short' }).replace('.', '');
  return d.toLocaleTimeString('es-MX', { hour: '2-digit', minute: '2-digit', hour12: false });
}
