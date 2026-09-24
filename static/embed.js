// <heartbeat-status> -- embeddable status card for one Heartbeat app, for use on other sites:
//
//   <script src="https://HEARTBEAT_HOST/static/embed.js" defer></script>
//   <heartbeat-status app="SLUG" token="EMBED_TOKEN"></heartbeat-status>
//
// Optional attributes:
//   label="Mi API"        text shown instead of the app's name
//   theme="light"         default dark
//   bars="N"              max bars (by default as many as fit the width, up to 100)
//   refresh="30"          seconds between updates (min 10)
//   color-up, color-degraded, color-down, color-empty, color-bg, color-text, color-border,
//   color-pill-text       any CSS color, e.g. color-up="#22c55e"
// Colors can also come from the host page's CSS, as custom properties (the attribute wins):
//   heartbeat-status { --hb-up: #22c55e; --hb-bg: transparent; }
// Talks only to Heartbeat's public /embed/{slug} endpoint (CORS-enabled, token-authorized).
// Renders into a Shadow DOM so the host page's CSS can't leak in or out.
(() => {
  // Resolved now: document.currentScript is only set while this script first executes.
  const ORIGIN = new URL(document.currentScript?.src || location.href).origin;
  // attribute -> CSS custom property it sets.
  const COLOR_ATTRS = {
    'color-up': '--hb-up',
    'color-degraded': '--hb-degraded',
    'color-down': '--hb-down',
    'color-empty': '--hb-empty',
    'color-bg': '--hb-bg',
    'color-text': '--hb-text',
    'color-border': '--hb-border',
    'color-pill-text': '--hb-pill-text',
  };
  const DATA_ATTRS = ['app', 'token', 'refresh'];
  const LABELS = { up: 'Up', degraded: 'Degradado', down: 'Down' };
  // Fixed bar size: a wider card shows more history instead of stretching the bars.
  const BAR_W = 8;
  const BAR_GAP = 4;
  const PAD_X = 16;
  const MAX_BARS = 100;

  const STYLE = `
    :host { display: block; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
    .card {
      --_up: var(--hb-up, #3ecf8e); --_degraded: var(--hb-degraded, #e0a940); --_down: var(--hb-down, #e05a5a);
      --_pill-text: var(--hb-pill-text, #0f1115);
      box-sizing: border-box; padding: 14px ${PAD_X}px; border-radius: 10px;
      border: 1px solid var(--_border); background: var(--_bg); color: var(--_text);
    }
    .head { display: flex; align-items: center; gap: 10px; margin-bottom: 10px; min-width: 0; }
    .pill { flex-shrink: 0; font-size: 12px; font-weight: 700; padding: 2px 10px; border-radius: 999px; color: var(--_pill-text); background: var(--_empty); }
    .pill.none { color: var(--_text); }
    .name { font-size: 15px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .bars { display: flex; justify-content: space-between; gap: ${BAR_GAP}px; }
    .bar { flex: 0 0 ${BAR_W}px; height: 24px; border-radius: 999px; background: var(--_empty); }
    .st-up { background: var(--_up); }
    .st-degraded { background: var(--_degraded); }
    .st-down { background: var(--_down); }
    .error { font-size: 12px; margin-top: 8px; color: var(--_text); opacity: 0.6; }
    .dark { --_bg: var(--hb-bg, #171a21); --_border: var(--hb-border, #262b36); --_text: var(--hb-text, #d6dae3); --_empty: var(--hb-empty, #3a4150); }
    .light { --_bg: var(--hb-bg, #ffffff); --_border: var(--hb-border, #e3e6ec); --_text: var(--hb-text, #1c2030); --_empty: var(--hb-empty, #d9dde5); }
  `;

  function fmtPct(v) {
    return v == null ? '—' : `${v.toFixed(v === 100 ? 0 : 2)}%`;
  }

  function beatTitle(b) {
    const when = new Date(b.at * 1000).toLocaleString();
    const ms = b.latency_ms == null ? '' : ` — ${b.latency_ms} ms`;
    return `${when} — ${LABELS[b.status]}${ms}`;
  }

  class HeartbeatStatus extends HTMLElement {
    static observedAttributes = [...DATA_ATTRS, 'label', 'theme', 'bars', ...Object.keys(COLOR_ATTRS)];

    constructor() {
      super();
      this.attachShadow({ mode: 'open' });
      this.data = null;
      this.error = '';
      this.timer = null;
      this.slots = 0;
      this.resizer = new ResizeObserver(() => {
        const slots = this.fitSlots();
        if (slots !== this.slots) this.render();
      });
    }

    connectedCallback() {
      this.resizer.observe(this);
      this.load();
    }

    disconnectedCallback() {
      this.resizer.disconnect();
      clearInterval(this.timer);
      this.timer = null;
    }

    // How many fixed-width bars fit the card's inner width (1px border each side).
    fitSlots() {
      const inner = this.clientWidth - 2 * PAD_X - 2;
      const fit = Math.floor((inner + BAR_GAP) / (BAR_W + BAR_GAP));
      const cap = Math.min(MAX_BARS, Number(this.getAttribute('bars')) || MAX_BARS);
      return Math.max(5, Math.min(cap, fit));
    }

    // Only app/token/refresh need a refetch; label, theme and colors are just a redraw.
    attributeChangedCallback(attr) {
      if (!this.isConnected) return;
      if (DATA_ATTRS.includes(attr)) this.load();
      else this.render();
    }

    async load() {
      clearInterval(this.timer);
      const secs = Math.max(10, Number(this.getAttribute('refresh')) || 30);
      this.timer = setInterval(() => this.fetchStatus(), secs * 1000);
      await this.fetchStatus();
    }

    async fetchStatus() {
      const app = this.getAttribute('app');
      const token = this.getAttribute('token');
      if (!app || !token) {
        this.error = 'Faltan los atributos app y token';
        this.render();
        return;
      }
      try {
        const url = `${ORIGIN}/embed/${encodeURIComponent(app)}?token=${encodeURIComponent(token)}`;
        const resp = await fetch(url);
        if (!resp.ok) throw new Error(resp.status === 404 ? 'App o token inválido' : `HTTP ${resp.status}`);
        this.data = await resp.json();
        this.error = '';
      } catch (e) {
        this.error = `Estado no disponible (${e.message})`;
      }
      this.render();
    }

    render() {
      const theme = this.getAttribute('theme') === 'light' ? 'light' : 'dark';
      const slots = this.fitSlots();
      this.slots = slots;
      const root = this.shadowRoot;
      root.innerHTML = `<style>${STYLE}</style>`;

      const card = document.createElement('div');
      card.className = `card ${theme}`;
      for (const [attr, prop] of Object.entries(COLOR_ATTRS)) {
        const value = this.getAttribute(attr);
        // CSS.supports keeps a typo from silently wiping the default.
        if (value && CSS.supports('color', value)) card.style.setProperty(prop, value);
      }
      const head = document.createElement('div');
      head.className = 'head';
      const pill = document.createElement('span');
      const name = document.createElement('span');
      name.className = 'name';
      head.append(pill, name);
      card.append(head);

      const d = this.data;
      pill.className = d?.status ? `pill st-${d.status}` : 'pill none';
      pill.textContent = fmtPct(d?.uptime_24h);
      // textContent, never innerHTML: the name/label is rendered on third-party pages.
      name.textContent = this.getAttribute('label') || d?.name || this.getAttribute('app') || '';

      const bars = document.createElement('div');
      bars.className = 'bars';
      const recent = (d?.recent || []).slice(-slots);
      for (let i = 0; i < slots; i++) {
        const b = recent[i - (slots - recent.length)];
        const bar = document.createElement('span');
        bar.className = b ? `bar st-${b.status}` : 'bar';
        if (b) bar.title = beatTitle(b);
        bars.append(bar);
      }
      card.append(bars);

      if (this.error) {
        const err = document.createElement('div');
        err.className = 'error';
        err.textContent = this.error;
        card.append(err);
      }
      root.append(card);
    }
  }

  if (!customElements.get('heartbeat-status')) {
    customElements.define('heartbeat-status', HeartbeatStatus);
  }
})();
