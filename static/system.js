// Alpine component behind the system panels of templates/dashboard.html: Heartbeat's own
// host on the overview (`systemPanel('')`), an app's server in its detail
// (`systemPanel(slug)`). Polls /api/system or /api/system/{slug}: the latest reading
// (CPU, memory, swap, disks, the busiest processes, like `top`) and a chart of the
// stored samples.
const SYSTEM_RANGES = [
  { hours: 1, label: '1h' },
  { hours: 6, label: '6h' },
  { hours: 24, label: '24h' },
  { hours: 168, label: '7d' },
];
const SYSTEM_REFRESH_MS = 30000;
const SYSTEM_CHART = { height: 150, left: 34, right: 8, top: 8, bottom: 22 };
// What each line is drawn with: theme tokens, so a theme change restyles it by itself.
const SYSTEM_SERIES = [
  { key: 'cpu', color: 'var(--info)' },
  { key: 'memory', color: 'var(--warn)' },
  { key: 'disk', color: 'var(--ink-3)', dash: '4 3' },
];

function systemPanel(slug, title) {
  return {
    slug,
    title,
    hours: 24,
    reading: null,
    samples: [],
    error: '',
    loaded: false,
    sortBy: 'cpu',
    width: 0,
    timer: null,
    SYSTEM_RANGES,
    SYSTEM_SERIES,

    init() {
      this.load();
      this.timer = setInterval(() => this.load(), SYSTEM_REFRESH_MS);
    },

    destroy() {
      clearInterval(this.timer);
    },

    async load() {
      const base = this.slug ? `/api/system/${encodeURIComponent(this.slug)}` : '/api/system';
      try {
        const resp = await fetch(`${base}?hours=${this.hours}`, { credentials: 'same-origin' });
        if (resp.status === 401) {
          location.href = '/login';
          return;
        }
        const body = await resp.json();
        if (!resp.ok) throw new Error(body.error || `HTTP ${resp.status}`);
        this.reading = body.reading;
        this.samples = body.samples || [];
        this.error = '';
      } catch (e) {
        this.error = e.message;
      }
      this.loaded = true;
    },

    setHours(h) {
      if (this.hours === h) return;
      this.hours = h;
      this.load();
    },

    snap() {
      return (this.reading && this.reading.snapshot) || null;
    },

    pct(used, total) {
      return total ? (used * 100) / total : null;
    },

    cpuPct() {
      return this.snap() ? this.snap().cpu.usage_pct : null;
    },

    memoryPct() {
      const s = this.snap();
      return s ? this.pct(s.memory.used_bytes, s.memory.total_bytes) : null;
    },

    // The disk closest to full: what the disk alert is about.
    fullestDisk() {
      const disks = (this.snap() && this.snap().disks) || [];
      return disks.reduce((worst, d) => (!worst || this.pct(d.used_bytes, d.total_bytes) > this.pct(worst.used_bytes, worst.total_bytes) ? d : worst), null);
    },

    diskPct() {
      const d = this.fullestDisk();
      return d ? this.pct(d.used_bytes, d.total_bytes) : null;
    },

    // Same bands as the meters' colors: over 90 % red, over 75 % yellow.
    level(p) {
      if (p == null) return 'unknown';
      if (p >= 90) return 'down';
      if (p >= 75) return 'degraded';
      return 'up';
    },

    fmtPct(v) {
      return v == null ? '—' : `${v.toFixed(v < 10 ? 1 : 0)} %`;
    },

    fmtBytes(b) {
      if (b == null) return '—';
      const units = ['B', 'KB', 'MB', 'GB', 'TB'];
      let v = b;
      let i = 0;
      while (v >= 1024 && i < units.length - 1) {
        v /= 1024;
        i++;
      }
      return `${v.toFixed(v < 10 && i > 0 ? 1 : 0)} ${units[i]}`;
    },

    fmtLoad() {
      const load = this.snap() && this.snap().cpu.load;
      return load ? load.map((v) => v.toFixed(2)).join(' · ') : '—';
    },

    fmtUptime() {
      const secs = this.snap() && this.snap().uptime_secs;
      if (secs == null) return '—';
      const d = Math.floor(secs / 86400), h = Math.floor((secs % 86400) / 3600);
      return d ? T('js.system_uptime_days', { d, h }) : T('js.system_uptime_hours', { h, m: Math.floor((secs % 3600) / 60) });
    },

    // `top`'s list, sorted by the chosen column.
    processes() {
      const list = ((this.snap() && this.snap().processes) || []).slice();
      return this.sortBy === 'memory'
        ? list.sort((a, b) => b.memory_bytes - a.memory_bytes)
        : list.sort((a, b) => b.cpu_pct - a.cpu_pct);
    },

    readAt() {
      return this.reading ? this.reading.at : null;
    },

    observe(el) {
      this.width = el.clientWidth;
      new ResizeObserver(() => { this.width = el.clientWidth; }).observe(el);
    },

    // CPU, memory and the fullest disk, in %, over the window; a gap where samples are
    // missing (an app that didn't answer), not a line across it.
    chartSvg() {
      const C = SYSTEM_CHART;
      const w = Math.max(1, this.width);
      const now = Date.now() / 1000;
      const from = now - this.hours * 3600;
      const plotW = Math.max(1, w - C.left - C.right);
      const plotH = C.height - C.top - C.bottom;
      const x = (at) => C.left + ((at - from) / (now - from)) * plotW;
      const y = (p) => C.top + plotH - (Math.min(100, Math.max(0, p)) / 100) * plotH;
      const value = {
        cpu: (s) => s.cpu_pct,
        memory: (s) => this.pct(s.memory_used, s.memory_total),
        disk: (s) => (s.disk_total ? this.pct(s.disk_used, s.disk_total) : null),
      };
      // Twice the spacing of two samples (or 3 min) is a gap.
      const spacing = this.samples.length > 1 ? (this.samples[this.samples.length - 1].at - this.samples[0].at) / (this.samples.length - 1) : 60;
      const gap = Math.max(180, spacing * 2.5);
      let svg = `<svg width="${w}" height="${C.height}" role="img" aria-label="${T('js.system_chart')}">`;
      for (const p of [0, 50, 100]) {
        svg += `<line x1="${C.left}" x2="${w - C.right}" y1="${y(p)}" y2="${y(p)}" style="stroke: var(--grid-minor)"/>`;
        svg += `<text x="${C.left - 6}" y="${y(p) + 4}" text-anchor="end" style="fill: var(--ink-4); font: 10px var(--font-mono)">${p}%</text>`;
      }
      const ticks = this.hours <= 6 ? 3 : 4;
      for (let i = 0; i <= ticks; i++) {
        const at = from + ((now - from) * i) / ticks;
        const d = new Date(at * 1000);
        const label = this.hours > 24
          ? d.toLocaleDateString(LOCALE, { day: '2-digit', month: 'short' }).replace('.', '')
          : d.toLocaleTimeString(LOCALE, { hour: '2-digit', minute: '2-digit', hour12: false });
        const anchor = i === 0 ? 'start' : i === ticks ? 'end' : 'middle';
        svg += `<text x="${x(at)}" y="${C.height - 6}" text-anchor="${anchor}" style="fill: var(--ink-4); font: 10px var(--font-mono)">${label}</text>`;
      }
      for (const series of SYSTEM_SERIES) {
        let path = '';
        let prev = null;
        for (const s of this.samples) {
          const v = value[series.key](s);
          if (v == null) {
            prev = null;
            continue;
          }
          path += `${prev && s.at - prev <= gap ? 'L' : 'M'}${x(s.at).toFixed(1)},${y(v).toFixed(1)}`;
          prev = s.at;
        }
        if (path) {
          const dash = series.dash ? `stroke-dasharray: ${series.dash};` : '';
          svg += `<path d="${path}" style="fill: none; stroke: ${series.color}; stroke-width: 1.5; ${dash}"/>`;
        }
      }
      return `${svg}</svg>`;
    },
  };
}
