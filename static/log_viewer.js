// Alpine component backing templates/log_viewer.html -- one registered app's logs.
// Fetches this app's own /api/apps/{slug}/logs (same-origin, cookie-authenticated), which
// server-side proxies to the registered app's real /admin/logs endpoint. The session token
// never touches this script.
const ALL_LEVELS = ['ERROR', 'WARN', 'INFO', 'DEBUG', 'TRACE'];
const LINE_OPTIONS = [100, 500, 1000, 2000, 5000];
const MODULES_COLLAPSED = 8;
const AUTO_REFRESH_MS = 5000;
const VIEWER_LOCALE = document.documentElement.lang === 'en' ? 'en-US' : 'es-MX';
// tracing's own bookkeeping, not the event's data: left out of the inline fields.
const HIDDEN_FIELDS = new Set(['message', 'log.target', 'log.module_path', 'log.file', 'log.line']);

function escapeHtml(text) {
  return String(text).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}

// 14:02:07.312 today, "26 sep 14:02:07" on another day -- the full timestamp is the title.
function shortTime(timestamp) {
  const d = new Date(timestamp);
  if (Number.isNaN(d.getTime())) return timestamp || '';
  const time = d.toLocaleTimeString(VIEWER_LOCALE, { hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false });
  if (d.toDateString() === new Date().toDateString()) return `${time}.${String(d.getMilliseconds()).padStart(3, '0')}`;
  return `${d.toLocaleDateString(VIEWER_LOCALE, { day: '2-digit', month: 'short' }).replace('.', '')} ${time}`;
}

// tracing_subscriber's json formatter emits one object per line, roughly
// {"timestamp":"...","level":"INFO","target":"...","fields":{"message":"...", ...}}.
// A line that isn't valid JSON is shown untouched as the message.
function parseLine(raw, key) {
  const line = { key, raw, timestamp: '', time: '', level: '', levelKey: 'none', target: '', message: raw, fields: [], pretty: raw, open: false };
  let j;
  try {
    j = JSON.parse(raw);
  } catch {
    return line;
  }
  if (!j || typeof j !== 'object') return line;
  const fields = j.fields && typeof j.fields === 'object' ? j.fields : {};
  line.timestamp = j.timestamp || '';
  line.time = shortTime(line.timestamp);
  line.level = String(j.level || '').toUpperCase();
  line.levelKey = line.level ? line.level.toLowerCase() : 'none';
  line.target = j.target || '';
  line.message = String(fields.message ?? j.message ?? '');
  line.fields = Object.entries(fields)
    .filter(([k]) => !HIDDEN_FIELDS.has(k))
    .map(([k, v]) => ({ key: k, value: typeof v === 'string' ? v : JSON.stringify(v) }));
  line.pretty = JSON.stringify(j, null, 2);
  return line;
}

// Stable keys across refreshes (the same line keeps its key, and its open/closed state),
// with a suffix for repeated identical lines.
function keyed(rawLines, prefix) {
  const seen = new Map();
  return rawLines.map((raw) => {
    const n = (seen.get(raw) || 0) + 1;
    seen.set(raw, n);
    return `${prefix}${n}:${raw}`;
  });
}

function logViewer(slug) {
  return {
    slug,
    ALL_LEVELS,
    LINE_OPTIONS,
    MODULES_COLLAPSED,
    stream: 'both',
    lines: 500,
    search: '',
    autoRefresh: false,
    loading: false,
    loaded: false,
    loadedAt: null,
    now: Date.now(),
    fetchError: '',
    outRaw: [],
    errorRaw: [],
    outLines: [],
    errorLines: [],
    excludedLevels: [],
    excludedTargets: [],
    showAllTargets: false,
    health: null,
    // Key of the line whose copy button last showed the checkmark, or null.
    copiedKey: null,
    _timer: null,

    init() {
      this.load();
      // Drives the "updated N s ago" label.
      setInterval(() => { this.now = Date.now(); }, 1000);
    },

    onKey(e) {
      const typing = ['INPUT', 'SELECT', 'TEXTAREA'].includes(e.target.tagName);
      if (e.key === 'Escape' && e.target === this.$refs.search) {
        this.search = '';
        this.$refs.search.blur();
        return;
      }
      if (typing || e.metaKey || e.ctrlKey || e.altKey) return;
      if (e.key === '/') {
        e.preventDefault();
        this.$refs.search.focus();
      } else if (e.key === 'r' && !this.loading) {
        this.load();
      }
    },

    setStream(stream) {
      if (this.stream === stream) return;
      this.stream = stream;
      this.load();
    },

    async copyLine(raw, key) {
      const ok = await copyToClipboard(raw);
      if (!ok) return;
      this.copiedKey = key;
      setTimeout(() => {
        if (this.copiedKey === key) this.copiedKey = null;
      }, 1200);
    },

    async loadHealth() {
      try {
        const resp = await fetch('/api/uptime', { credentials: 'same-origin' });
        if (!resp.ok) return;
        const overview = await resp.json();
        this.health = overview.monitors.find((m) => m.slug === this.slug) || null;
      } catch {
        // The header just keeps its last state; the logs are what matter here.
      }
    },

    async load() {
      this.loading = true;
      this.loadHealth();
      try {
        const params = new URLSearchParams({ stream: this.stream, lines: String(this.lines) });
        const res = await fetch(`/api/apps/${encodeURIComponent(this.slug)}/logs?${params}`);
        if (res.status === 401) {
          // Session cookie missing or the upstream token expired -- back to /login.
          window.location.href = '/login';
          return;
        }
        const body = await res.json().catch(() => null);
        if (!res.ok) {
          this.fetchError = (body && body.error) || `Error ${res.status}`;
          this.setLines([], []);
          return;
        }
        this.setLines((body.out && body.out.lines) || [], (body.error && body.error.lines) || []);
        this.fetchError = [body.out && body.out.error, body.error && body.error.error].filter(Boolean).join(' · ');
        this.loadedAt = Date.now();
      } catch (e) {
        this.fetchError = String(e);
      } finally {
        this.loading = false;
        this.loaded = true;
      }
    },

    // Keeps stderr scrolled to its newest (last) line, unless the user scrolled up to read.
    followStderr() {
      const el = this.$refs.errBody;
      if (!el) return;
      const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
      if (atBottom || !this.loadedAt) this.$nextTick(() => { el.scrollTop = el.scrollHeight; });
    },

    // Parsed once per load, not per render. Lines arrive oldest-first (tail -n order, per
    // the endpoint contract); stdout is reversed so the newest event reads first.
    setLines(outRaw, errorRaw) {
      const open = new Set(this.outLines.filter((l) => l.open).map((l) => l.key));
      this.outRaw = outRaw;
      this.errorRaw = errorRaw;
      this.outLines = keyed(outRaw, 'o')
        .map((key, i) => {
          const line = parseLine(outRaw[i], key);
          line.open = open.has(key);
          return line;
        })
        .reverse();
      // stderr stays in terminal order: a multi-line panic only reads right top to bottom.
      this.errorLines = keyed(errorRaw, 'e').map((key, i) => ({ key, text: errorRaw[i] }));
      this.followStderr();
    },

    healthState() {
      if (!this.health) return 'unknown';
      if (this.health.paused) return 'paused';
      return this.health.status || 'unknown';
    },

    healthLabel() {
      const state = this.healthState();
      return T(['up', 'degraded', 'down', 'paused'].includes(state) ? `js.status_${state}` : 'js.status_unknown');
    },

    freshness() {
      if (this.loading && !this.loadedAt) return T('viewer.loading');
      if (!this.loadedAt) return '';
      const secs = Math.max(0, Math.round((this.now - this.loadedAt) / 1000));
      const ago = secs < 90 ? T('js.ago_s', { n: secs }) : T('js.ago_min', { n: Math.round(secs / 60) });
      return T('viewer.updated', { ago });
    },

    toggleAutoRefresh() {
      this.autoRefresh = !this.autoRefresh;
      clearInterval(this._timer);
      this._timer = this.autoRefresh ? setInterval(() => !this.loading && this.load(), AUTO_REFRESH_MS) : null;
    },

    toggleLevel(level) {
      const i = this.excludedLevels.indexOf(level);
      if (i === -1) this.excludedLevels.push(level);
      else this.excludedLevels.splice(i, 1);
    },

    toggleTarget(target) {
      const i = this.excludedTargets.indexOf(target);
      if (i === -1) this.excludedTargets.push(target);
      else this.excludedTargets.splice(i, 1);
    },

    filtersActive() {
      return this.excludedLevels.length > 0 || this.excludedTargets.length > 0 || this.search.trim() !== '';
    },

    resetFilters() {
      this.excludedLevels = [];
      this.excludedTargets = [];
      this.search = '';
    },

    // How many stdout lines carry each level -- shown on each chip, so it's obvious which
    // levels are worth toggling before clicking one.
    levelCounts() {
      const counts = Object.fromEntries(ALL_LEVELS.map((l) => [l, 0]));
      for (const line of this.outLines) if (counts[line.level] !== undefined) counts[line.level]++;
      return counts;
    },

    // Distinct stdout modules, noisiest first, so the one worth excluding (e.g.
    // sqlx::query) is at the front. Independent of the other filters.
    distinctTargets() {
      const counts = new Map();
      for (const line of this.outLines) if (line.target) counts.set(line.target, (counts.get(line.target) || 0) + 1);
      return Array.from(counts, ([target, count]) => ({ target, count }))
        .sort((a, b) => b.count - a.count || a.target.localeCompare(b.target));
    },

    // Collapsed to the noisiest few, but an excluded module always stays visible so it can
    // be turned back on.
    visibleTargets() {
      const all = this.distinctTargets();
      if (this.showAllTargets) return all;
      return all.filter((t, i) => i < MODULES_COLLAPSED || this.excludedTargets.includes(t.target));
    },

    filteredOut() {
      const q = this.search.trim().toLowerCase();
      return this.outLines.filter(
        (l) =>
          (!q || l.raw.toLowerCase().includes(q)) &&
          (!l.level || !this.excludedLevels.includes(l.level)) &&
          (!l.target || !this.excludedTargets.includes(l.target)),
      );
    },

    filteredError() {
      const q = this.search.trim().toLowerCase();
      return this.errorLines.filter((l) => !q || l.text.toLowerCase().includes(q));
    },

    countLabel(shown, total) {
      return shown === total ? String(total) : `${shown} / ${total}`;
    },

    // Escaped text with every match of the search wrapped in <mark>.
    highlight(text) {
      const safe = escapeHtml(text);
      const q = this.search.trim();
      if (!q) return safe;
      const pattern = escapeHtml(q).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
      return safe.replace(new RegExp(pattern, 'gi'), (m) => `<mark>${m}</mark>`);
    },

    // Why a pane is empty: still loading, the request failed, the endpoint sent nothing,
    // or the filters hide everything.
    emptyKind(stream) {
      const raw = stream === 'out' ? this.outRaw : this.errorRaw;
      if (!this.loaded) return 'loading';
      if (this.fetchError && raw.length === 0) return 'error';
      if (raw.length === 0) return 'endpoint';
      return 'filtered';
    },

    emptyTitle(stream) {
      const kind = this.emptyKind(stream);
      if (kind === 'loading') return T('viewer.loading');
      if (kind === 'error') return T('viewer.empty_error');
      if (kind === 'filtered') return T('viewer.empty_filtered');
      return stream === 'out' ? T('viewer.empty_out') : T('viewer.empty_err');
    },

    emptyHint(stream) {
      const kind = this.emptyKind(stream);
      if (kind === 'endpoint') return stream === 'out' ? T('viewer.empty_out_hint') : T('viewer.empty_err_hint');
      return '';
    },
  };
}
