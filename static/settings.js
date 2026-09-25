// Alpine component backing templates/settings.html: edits the alert templates (live
// preview, save, restore), and sends pings and sample alerts through
// POST /settings/alerts/test, showing each webhook's answer ("pong").
const VARIABLES = ['app', 'message', 'latency', 'link', 'mentions'];

// Mirrors alert_templates::render in Rust, so the preview matches what gets sent.
function renderTemplate(template, vars) {
  let text = template;
  for (const name of VARIABLES) text = text.split(`{${name}}`).join(vars[name] ?? '');
  text = text.split('( )').join('').split('()').join('');
  const lines = text.split('\n').map((line) => line.split(/\s+/).filter(Boolean).join(' '));
  while (lines.length && !lines[lines.length - 1]) lines.pop();
  return lines.join('\n');
}
function escapeHtml(text) {
  return String(text).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}

function settingsPage(templates = []) {
  const byKind = Object.fromEntries(templates.map((t) => [t.kind, { ...t, saved: t.template }]));
  return {
    VARIABLES,
    target: '',
    busy: {},
    results: {},
    tpl: byKind,
    saving: false,
    saveError: '',

    preview(kind) {
      const t = this.tpl[kind];
      return t ? renderTemplate(t.template, t.sample) : '';
    },

    dirty(kind) {
      const t = this.tpl[kind];
      return !!t && t.template !== t.saved;
    },

    stateLabel(kind) {
      if (this.dirty(kind)) return T('settings.unsaved');
      return this.tpl[kind].template === this.tpl[kind].default ? T('settings.state_default') : T('settings.state_custom');
    },

    // Inserts {name} at the textarea's cursor.
    insertVar(kind, name) {
      const el = this.$refs[`tpl_${kind}`];
      const token = `{${name}}`;
      const t = this.tpl[kind];
      const start = el.selectionStart ?? t.template.length;
      const end = el.selectionEnd ?? start;
      t.template = t.template.slice(0, start) + token + t.template.slice(end);
      this.$nextTick(() => {
        el.focus();
        el.selectionStart = el.selectionEnd = start + token.length;
      });
    },

    restore(kind) {
      this.tpl[kind].template = this.tpl[kind].default;
    },

    // Saves all three; a template equal to its default is sent blank so it keeps
    // following the default (e.g. if APP_LANG changes).
    async save() {
      this.saving = true;
      this.saveError = '';
      const body = Object.fromEntries(
        Object.entries(this.tpl).map(([kind, t]) => [kind, t.template.trim() === t.default ? '' : t.template]),
      );
      try {
        const resp = await fetch('/settings/alerts/templates', {
          method: 'POST',
          credentials: 'same-origin',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(body),
        });
        if (resp.status === 401) {
          location.href = '/login';
          return false;
        }
        const data = await resp.json();
        if (!resp.ok) throw new Error(data.error || `HTTP ${resp.status}`);
        for (const t of data.templates) {
          this.tpl[t.kind] = { ...t, saved: t.template };
        }
        return true;
      } catch (e) {
        this.saveError = T('settings.save_failed', { error: e.message });
        return false;
      } finally {
        this.saving = false;
      }
    },

    // Tests always send the saved templates: save pending edits first.
    async sendSample(kind) {
      if (Object.keys(this.tpl).some((k) => this.dirty(k)) && !(await this.save())) return;
      await this.send(kind, this.target, kind);
    },

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
