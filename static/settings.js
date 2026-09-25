// Alpine component backing templates/settings.html: sends pings and sample alerts through
// POST /settings/alerts/test and shows each webhook's answer ("pong").
function escapeHtml(text) {
  return String(text).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}

function settingsPage() {
  return {
    target: '',
    busy: {},
    results: {},

    async send(kind, target, key) {
      this.busy = { ...this.busy, [key]: true };
      try {
        const resp = await fetch('/settings/alerts/test', {
          method: 'POST',
          credentials: 'same-origin',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ kind, target: target === '' || target == null ? null : Number(target) }),
        });
        if (resp.status === 401) {
          location.href = '/login';
          return;
        }
        const body = await resp.json();
        this.results = { ...this.results, [key]: resp.ok ? body.outcomes : [{ ok: false, detail: body.error || `HTTP ${resp.status}` }] };
      } catch (e) {
        this.results = { ...this.results, [key]: [{ ok: false, detail: e.message }] };
      } finally {
        this.busy = { ...this.busy, [key]: false };
      }
    },

    // One line per webhook answer; everything that came from the network is escaped.
    resultHtml(key) {
      const outcomes = this.results[key] || [];
      return outcomes
        .map((o) => {
          const state = o.ok ? 'up' : 'down';
          const summary = o.ok
            ? T('settings.pong', { status: o.status, ms: o.latency_ms })
            : T('settings.failed', { status: o.status ?? '—' });
          const detail = !o.ok && o.detail ? `<span class="detail" title="${escapeHtml(o.detail)}">${escapeHtml(o.detail)}</span>` : '';
          return `<span class="vital st-${state}"></span><span>${escapeHtml(summary)}</span>${detail}`;
        })
        .join('<br>');
    },
  };
}
