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
//   color-up, color-degraded, color-down, color-empty, color-bg, color-text, color-border
//                         any CSS color, e.g. color-up="#22c55e"
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
  };
  const DATA_ATTRS = ['app', 'token', 'refresh'];
  const LABELS = { up: 'Up', degraded: 'Degradado', down: 'Down' };
  // Fixed bar size: a wider card shows more history instead of stretching the bars.
  const BAR_W = 6;
  const BAR_GAP = 3;
  const PAD_X = 14;
  const MAX_BARS = 100;

  // Same instrument language as the Heartbeat dashboard: a vital dot that beats while the app
  // is alive (slower when degraded, still when down), mono readouts, a strip of checks.
  // Plex fonts are used only if the host page already loads them -- never fetched here.
  const STYLE = `
    :host { display: block; font-family: "IBM Plex Sans", -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; -webkit-font-smoothing: antialiased; }
    .card { box-sizing: border-box; padding: 12px ${PAD_X}px 14px; border-radius: 10px; border: 1px solid var(--_border); background: var(--_bg); color: var(--_text); }
    .head { display: flex; align-items: center; gap: 10px; margin-bottom: 12px; min-width: 0; }
    .vital { position: relative; flex-shrink: 0; width: 8px; height: 8px; border-radius: 50%; background: var(--_empty); }
    .vital.st-down { box-shadow: 0 0 0 3px color-mix(in srgb, var(--_down) 22%, transparent); }
    .vital.st-up::after, .vital.st-degraded::after { content: ""; position: absolute; inset: 0; border-radius: 50%; background: inherit; animation: beat 2.4s cubic-bezier(0.23, 1, 0.32, 1) infinite; }
    .vital.st-degraded::after { animation-duration: 3.6s; }
    @keyframes beat { 0% { transform: scale(1); opacity: 0.55; } 45%, 100% { transform: scale(2.6); opacity: 0; } }
    @media (prefers-reduced-motion: reduce) { .vital::after { display: none; } }
    .name { flex: 1; min-width: 0; font-size: 14px; font-weight: 500; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .reading { flex-shrink: 0; display: flex; align-items: baseline; gap: 8px; font-size: 12px; }
    .state { font-weight: 500; }
    .state.st-up { color: var(--_up); } .state.st-degraded { color: var(--_degraded); } .state.st-down { color: var(--_down); }
    .pct { font-family: "IBM Plex Mono", ui-monospace, SFMono-Regular, Menlo, monospace; font-variant-numeric: tabular-nums; opacity: 0.7; }
    .bars { display: flex; justify-content: space-between; gap: ${BAR_GAP}px; }
    .bar { flex: 0 0 ${BAR_W}px; height: 22px; border-radius: 2px; background: var(--_empty); }
    .st-up:not(.state) { background: var(--_up); }
    .st-degraded:not(.state) { background: var(--_degraded); }
    .st-down:not(.state) { background: var(--_down); }
    .error { font-size: 12px; margin-top: 10px; opacity: 0.6; }
    .dark {
      --_bg: var(--hb-bg, #111316); --_border: var(--hb-border, rgba(255, 255, 255, 0.075)); --_text: var(--hb-text, #e7e9ec); --_empty: var(--hb-empty, #262a30);
      --_up: var(--hb-up, #3fd68a); --_degraded: var(--hb-degraded, #f0b43c); --_down: var(--hb-down, #f0514e);
    }
    .light {
      --_bg: var(--hb-bg, #ffffff); --_border: var(--hb-border, rgba(0, 0, 0, 0.09)); --_text: var(--hb-text, #16181c); --_empty: var(--hb-empty, #e4e6ea);
      --_up: var(--hb-up, #1fa463); --_degraded: var(--hb-degraded, #d6921a); --_down: var(--hb-down, #dc3b38);
    }
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
      const d = this.data;
      const head = document.createElement('div');
      head.className = 'head';
      const vital = document.createElement('span');
      vital.className = d?.status ? `vital st-${d.status}` : 'vital';
      const name = document.createElement('span');
      name.className = 'name';
      // textContent, never innerHTML: the name/label is rendered on third-party pages.
      name.textContent = this.getAttribute('label') || d?.name || this.getAttribute('app') || '';
      const reading = document.createElement('span');
      reading.className = 'reading';
      const state = document.createElement('span');
      state.className = d?.status ? `state st-${d.status}` : 'state';
      state.textContent = d?.status ? LABELS[d.status] : 'Sin datos';
      const pct = document.createElement('span');
      pct.className = 'pct';
      pct.textContent = fmtPct(d?.uptime_24h);
      pct.title = 'Uptime últimas 24 h';
      reading.append(state, pct);
      head.append(vital, name, reading);
      card.append(head);

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
