// Theme builder: pick the 14 base colors, see them on a copy of the dashboard, get the
// CSS for Settings -> Custom theme. The app applies that CSS on top of its dark tokens,
// so the output sets every token, deriving borders, tints and overlays from the base
// colors the same way static/tokens.css does for dark and light. Nothing leaves the
// browser; the draft is kept in localStorage.
(function () {
  'use strict';

  const DRAFT_KEY = 'hb-site-theme-draft';
  const TOKENS = [
    'glass', 'bay', 'bay-raised', 'well',
    'ink', 'ink-2', 'ink-3', 'ink-4',
    'rhythm-up', 'rhythm-degraded', 'rhythm-down', 'rhythm-flat',
    'info', 'pulse',
  ];

  const PRESETS = {
    dark: {
      glass: '#0b0c0e', bay: '#111316', 'bay-raised': '#171a1e', well: '#08090a',
      ink: '#e7e9ec', 'ink-2': '#a4a9b1', 'ink-3': '#6e737c', 'ink-4': '#454a52',
      'rhythm-up': '#3fd68a', 'rhythm-degraded': '#f0b43c', 'rhythm-down': '#f0514e', 'rhythm-flat': '#262a30',
      info: '#8aa4e8', pulse: '#dd1818',
    },
    light: {
      glass: '#f3f4f6', bay: '#ffffff', 'bay-raised': '#eceef1', well: '#f7f8fa',
      ink: '#15171b', 'ink-2': '#474c55', 'ink-3': '#6a707a', 'ink-4': '#9aa0a9',
      'rhythm-up': '#12a05f', 'rhythm-degraded': '#c4860a', 'rhythm-down': '#d8392f', 'rhythm-flat': '#dde0e5',
      info: '#3a5fc4', pulse: '#dd1818',
    },
    midnight: {
      glass: '#0b1020', bay: '#111831', 'bay-raised': '#18213f', well: '#080c19',
      ink: '#e4e8f7', 'ink-2': '#a3acc9', 'ink-3': '#6f79a0', 'ink-4': '#444d6b',
      'rhythm-up': '#4ade9a', 'rhythm-degraded': '#f5c451', 'rhythm-down': '#ff6b6b', 'rhythm-flat': '#232b48',
      info: '#8ab4ff', pulse: '#dd1818',
    },
    forest: {
      glass: '#0c120f', bay: '#121a16', 'bay-raised': '#18231d', well: '#080d0a',
      ink: '#e6ede8', 'ink-2': '#a3b3a9', 'ink-3': '#6f8176', 'ink-4': '#46544b',
      'rhythm-up': '#5be39b', 'rhythm-degraded': '#e9c46a', 'rhythm-down': '#f07167', 'rhythm-flat': '#223029',
      info: '#7fc8c2', pulse: '#dd1818',
    },
    contrast: {
      glass: '#000000', bay: '#0a0a0a', 'bay-raised': '#161616', well: '#000000',
      ink: '#ffffff', 'ink-2': '#e0e0e0', 'ink-3': '#b0b0b0', 'ink-4': '#7a7a7a',
      'rhythm-up': '#00e676', 'rhythm-degraded': '#ffd000', 'rhythm-down': '#ff4d4d', 'rhythm-flat': '#333333',
      info: '#66b3ff', pulse: '#ff2020',
    },
  };

  // ---------- Color math ----------

  function rgb(hex) {
    const n = parseInt(hex.slice(1), 16);
    return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
  }
  function toHex([r, g, b]) {
    return '#' + [r, g, b].map((v) => Math.round(v).toString(16).padStart(2, '0')).join('');
  }
  function alpha(hex, a) {
    const [r, g, b] = rgb(hex);
    return `rgba(${r}, ${g}, ${b}, ${a})`;
  }
  function mix(hex, other, amount) {
    const a = rgb(hex);
    const b = rgb(other);
    return toHex(a.map((v, i) => v + (b[i] - v) * amount));
  }
  function luminance(hex) {
    const [r, g, b] = rgb(hex).map((v) => {
      v /= 255;
      return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4;
    });
    return 0.2126 * r + 0.7152 * g + 0.0722 * b;
  }
  function contrast(a, b) {
    const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
    return (hi + 0.05) / (lo + 0.05);
  }
  const isHex = (v) => /^#[0-9a-f]{6}$/i.test(v);

  // ---------- Theme -> every token ----------

  function derive(c) {
    const light = luminance(c.glass) > 0.4;
    // Same steps as tokens.css: borders and overlays are the text color at low alpha.
    const a = light
      ? { soft: 0.06, grat: 0.1, strong: 0.17, ov: 0.03, ovs: 0.16, hover: 0.3, hl: 0.11, gmin: 0.035, gmaj: 0.075 }
      : { soft: 0.045, grat: 0.075, strong: 0.12, ov: 0.025, ovs: 0.12, hover: 0.18, hl: 0.18, gmin: 0.028, gmaj: 0.055 };
    const t = light
      ? { dt: 0.07, dts: 0.11, dl: 0.32, dh: 0.16, deg: 0.13, up: 0.4, info: 0.1, dbg: 0.12 }
      : { dt: 0.08, dts: 0.12, dl: 0.26, dh: 0.18, deg: 0.12, up: 0.35, info: 0.12, dbg: 0.14 };
    return {
      'color-scheme': light ? 'light' : 'dark',
      ...c,
      'graticule-soft': alpha(c.ink, a.soft),
      graticule: alpha(c.ink, a.grat),
      'graticule-strong': alpha(c.ink, a.strong),
      overlay: alpha(c.ink, a.ov),
      'overlay-strong': alpha(c.ink, a.ovs),
      'input-border-hover': alpha(c.ink, a.hover),
      'focus-ring': alpha(c.ink, 0.35),
      highlight: alpha(c.ink, a.hl),
      topbar: alpha(c.glass, 0.86),
      shadow: light ? alpha(c.ink, 0.14) : 'rgba(0, 0, 0, 0.45)',
      backdrop: light ? alpha(c.ink, 0.34) : alpha(mix(c.glass, '#000000', 0.5), 0.72),
      'btn-hover': light ? mix(c.ink, '#ffffff', 0.12) : mix(c.ink, '#ffffff', 1),
      'down-tint': alpha(c['rhythm-down'], t.dt),
      'down-tint-strong': alpha(c['rhythm-down'], t.dts),
      'down-line': alpha(c['rhythm-down'], t.dl),
      'down-halo': alpha(c['rhythm-down'], t.dh),
      'down-ink': light ? mix(c['rhythm-down'], '#000000', 0.25) : mix(c['rhythm-down'], '#ffffff', 0.45),
      'degraded-tint': alpha(c['rhythm-degraded'], t.deg),
      'up-line': alpha(c['rhythm-up'], t.up),
      'info-tint': alpha(c.info, t.info),
      'debug-tint': alpha(c['ink-3'], t.dbg),
      'grid-minor': alpha(c.ink, a.gmin),
      'grid-major': alpha(c.ink, a.gmaj),
    };
  }

  function css(all) {
    const lines = Object.entries(all).map(([k, v]) => (k === 'color-scheme' ? `  color-scheme: ${v};` : `  --${k}: ${v};`));
    return `/* Heartbeat custom theme */\n:root {\n${lines.join('\n')}\n}\n`;
  }

  // ---------- Page ----------

  document.addEventListener('DOMContentLoaded', () => {
    const preview = document.getElementById('preview');
    const out = document.getElementById('css-out');
    if (!preview || !out) return;
    const pickers = new Map([...document.querySelectorAll('[data-token]')].map((el) => [el.dataset.token, el]));
    const hexes = new Map([...document.querySelectorAll('[data-hex]')].map((el) => [el.dataset.hex, el]));
    const presetButtons = [...document.querySelectorAll('[data-preset]')];
    let colors = { ...PRESETS.dark };

    try {
      const draft = JSON.parse(localStorage.getItem(DRAFT_KEY) || 'null');
      if (draft && TOKENS.every((t) => isHex(draft[t]))) colors = draft;
    } catch {
      // No storage or a broken draft: start from dark.
    }

    function render() {
      for (const t of TOKENS) {
        pickers.get(t).value = colors[t];
        if (document.activeElement !== hexes.get(t)) hexes.get(t).value = colors[t];
      }
      const all = derive(colors);
      for (const [k, v] of Object.entries(all)) {
        if (k === 'color-scheme') preview.style.colorScheme = v;
        else preview.style.setProperty('--' + k, v);
      }
      out.textContent = css(all);
      for (const el of document.querySelectorAll('[data-ratio]')) {
        const ratio = contrast(colors[el.dataset.ratio], colors.glass);
        const ok = ratio >= parseFloat(el.dataset.min);
        el.querySelector('strong').textContent = ratio.toFixed(1) + ':1';
        el.querySelector('em').textContent = ok ? el.dataset.ok : el.dataset.low;
        el.classList.toggle('low', !ok);
      }
      const match = Object.keys(PRESETS).find((p) => TOKENS.every((t) => PRESETS[p][t] === colors[t]));
      for (const b of presetButtons) b.setAttribute('aria-pressed', String(b.dataset.preset === match));
      try {
        localStorage.setItem(DRAFT_KEY, JSON.stringify(colors));
      } catch {
        // Private mode: the draft lasts for this page only.
      }
    }

    function set(token, value) {
      if (!isHex(value)) return;
      colors = { ...colors, [token]: value.toLowerCase() };
      render();
    }

    for (const [t, el] of pickers) el.addEventListener('input', () => set(t, el.value));
    for (const [t, el] of hexes) {
      el.addEventListener('input', () => {
        const v = el.value.startsWith('#') ? el.value : '#' + el.value;
        if (isHex(v)) set(t, v);
      });
      el.addEventListener('blur', () => { el.value = colors[t]; });
    }
    for (const b of presetButtons) {
      b.addEventListener('click', () => {
        colors = { ...PRESETS[b.dataset.preset] };
        render();
      });
    }
    document.querySelector('[data-reset]').addEventListener('click', () => {
      colors = { ...PRESETS.dark };
      render();
    });

    // Load an existing theme (e.g. the one downloaded from Settings): takes the hex
    // values of the base tokens it sets, keeps the rest.
    document.querySelector('[data-import]').addEventListener('change', (e) => {
      const file = e.target.files && e.target.files[0];
      if (!file) return;
      file.text().then((text) => {
        const next = { ...colors };
        for (const [, name, value] of text.matchAll(/--([\w-]+)\s*:\s*(#[0-9a-f]{3,6})\b/gi)) {
          if (!TOKENS.includes(name)) continue;
          const v = value.length === 4 ? '#' + [...value.slice(1)].map((ch) => ch + ch).join('') : value;
          if (isHex(v)) next[name] = v.toLowerCase();
        }
        colors = next;
        render();
      });
      e.target.value = '';
    });

    document.querySelector('[data-download]').addEventListener('click', () => {
      const url = URL.createObjectURL(new Blob([out.textContent], { type: 'text/css' }));
      const a = Object.assign(document.createElement('a'), { href: url, download: 'heartbeat-theme.css' });
      a.click();
      URL.revokeObjectURL(url);
    });

    const copy = document.querySelector('[data-copy-css]');
    copy.addEventListener('click', async () => {
      try {
        await navigator.clipboard.writeText(out.textContent);
      } catch {
        const range = document.createRange();
        range.selectNodeContents(out);
        getSelection().removeAllRanges();
        getSelection().addRange(range);
        return;
      }
      copy.textContent = copy.dataset.done;
      clearTimeout(copy._reset);
      copy._reset = setTimeout(() => { copy.textContent = copy.dataset.label; }, 1600);
    });

    render();
  });
})();
