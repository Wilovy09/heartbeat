// Alpine component backing templates/log_viewer.html -- one registered app's logs.
// Fetches this app's own /api/apps/{slug}/logs (same-origin, cookie-authenticated), which
// server-side proxies to the registered app's real /admin/logs endpoint. The session token
// never touches this script.
const ALL_LEVELS = ['ERROR', 'WARN', 'INFO', 'DEBUG', 'TRACE'];

// navigator.clipboard only exists in a secure context (https, or localhost) -- this app
// may well be reached over plain http on an internal server (see COOKIE_SECURE), where
// that API is undefined. Falls back to the classic hidden-textarea + execCommand trick.
async function copyToClipboard(text) {
  if (navigator.clipboard && window.isSecureContext) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // fall through to the legacy path
    }
  }
  try {
    const el = document.createElement('textarea');
    el.value = text;
    el.style.position = 'fixed';
    el.style.opacity = '0';
    document.body.appendChild(el);
    el.select();
    document.execCommand('copy');
    document.body.removeChild(el);
    return true;
  } catch {
    return false;
  }
}

function logViewer(slug) {
  return {
    slug,
    stream: 'both',
    lines: 500,
    search: '',
    autoRefresh: false,
    loading: false,
    fetchError: '',
    outRaw: [],
    errorRaw: [],
    ALL_LEVELS,
    excludedLevels: [],
    excludedTargets: [],
    // Key ('out-3' / 'err-5') of the line whose copy button last showed the checkmark, or
    // null. Shared per-component instead of per-line state, so filteredOut()/filteredError()
    // can keep recomputing plain arrays on every render without losing "just copied" state.
    copiedKey: null,
    _timer: null,

    init() {
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

    async load() {
      this.loading = true;
      this.fetchError = '';
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
          this.outRaw = [];
          this.errorRaw = [];
          return;
        }
        this.outRaw = (body.out && body.out.lines) || [];
        this.errorRaw = (body.error && body.error.lines) || [];
        const perStreamErrors = [body.out && body.out.error, body.error && body.error.error]
          .filter(Boolean);
        this.fetchError = perStreamErrors.join(' · ');
      } catch (e) {
        this.fetchError = String(e);
      } finally {
        this.loading = false;
      }
    },

    // tracing_subscriber's json formatter emits one object per line, roughly
    // {"timestamp":"...","level":"INFO","target":"...","fields":{"message":"..."}, ...}.
    // Falls back to showing the raw line untouched when a line isn't valid JSON (routine
    // for the error/stderr stream, which is raw panic/stderr text, not tracing output).
    parseLine(raw) {
      try {
        const j = JSON.parse(raw);
        return {
          raw,
          timestamp: j.timestamp || '',
          level: j.level || '',
          target: j.target || '',
          message: (j.fields && j.fields.message) || j.message || raw,
          _open: false,
        };
      } catch {
        return { raw, timestamp: '', level: '', target: '', message: raw, _open: false };
      }
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

    resetFilters() {
      this.excludedLevels = [];
      this.excludedTargets = [];
    },

    // How many stdout lines currently carry each level -- shown next to each chip so
    // it's obvious which levels are worth toggling before you click one.
    levelCounts() {
      const counts = {};
      for (const level of ALL_LEVELS) counts[level] = 0;
      for (const raw of this.outRaw) {
        const level = (this.parseLine(raw).level || '').toUpperCase();
        if (counts[level] !== undefined) counts[level]++;
      }
      return counts;
    },

    // Distinct stdout targets in the CURRENT (unfiltered-by-level) result set, with a count
    // each, sorted noisiest-first -- so a module worth excluding (e.g. sqlx::query) sorts
    // to the top instead of getting lost alphabetically. Independent of the level filter so
    // the module checklist doesn't shrink just because a level got excluded.
    distinctTargets() {
      const counts = new Map();
      for (const raw of this.outRaw) {
        const t = this.parseLine(raw).target;
        if (!t) continue;
        counts.set(t, (counts.get(t) || 0) + 1);
      }
      return Array.from(counts.entries())
        .map(([target, count]) => ({ target, count }))
        .sort((a, b) => b.count - a.count || a.target.localeCompare(b.target));
    },

    // outRaw/errorRaw arrive oldest-first (tail -n order, see pulso-backend's tail_lines) --
    // reversed here, at the one place every rendering path goes through, so the newest
    // line reads first without disturbing the underlying order distinctTargets()/
    // levelCounts() rely on.
    filteredOut() {
      const q = this.search.trim().toLowerCase();
      return this.outRaw
        .map((l) => this.parseLine(l))
        .filter((l) => !q || l.raw.toLowerCase().includes(q))
        .filter((l) => !l.level || !this.excludedLevels.includes(l.level.toUpperCase()))
        .filter((l) => !l.target || !this.excludedTargets.includes(l.target))
        .reverse();
    },

    filteredError() {
      const q = this.search.trim().toLowerCase();
      return this.errorRaw.filter((l) => !q || l.toLowerCase().includes(q)).reverse();
    },

    toggleAutoRefresh() {
      if (this._timer) {
        clearInterval(this._timer);
        this._timer = null;
      }
      if (this.autoRefresh) {
        this._timer = setInterval(() => this.load(), 5000);
      }
    },
  };
}
