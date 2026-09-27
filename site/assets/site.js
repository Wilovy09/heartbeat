// Heartbeat site: theme switch, the hero monitor, the screenshot tabs, copy buttons and
// the docs table of contents. No dependencies.
(function () {
  'use strict';

  const reduceMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  // ---------- Theme: system (default), light or dark; same tokens as the app ----------

  const THEME_KEY = 'hb-site-theme';
  const prefersLight = window.matchMedia('(prefers-color-scheme: light)');

  function savedTheme() {
    try {
      const saved = localStorage.getItem(THEME_KEY);
      return saved === 'light' || saved === 'dark' ? saved : 'system';
    } catch {
      return 'system';
    }
  }

  function applyTheme(choice) {
    const root = document.documentElement;
    const light = choice === 'light' || (choice === 'system' && prefersLight.matches);
    root.dataset.themeChoice = choice;
    if (light) root.dataset.theme = 'light';
    else delete root.dataset.theme;
    for (const button of document.querySelectorAll('.theme-switch [data-theme-choice]')) {
      button.setAttribute('aria-checked', String(button.dataset.themeChoice === choice));
    }
  }

  prefersLight.addEventListener('change', () => {
    if (savedTheme() === 'system') applyTheme('system');
  });

  function initTheme() {
    applyTheme(savedTheme());
    // Two copies of the switch: the bar's and, on phones, the menu's.
    for (const group of document.querySelectorAll('.theme-switch')) initSwitch(group);
  }

  function initSwitch(group) {
    group.addEventListener('click', (e) => {
      const button = e.target.closest('[data-theme-choice]');
      if (!button) return;
      const choice = button.dataset.themeChoice;
      try {
        if (choice === 'system') localStorage.removeItem(THEME_KEY);
        else localStorage.setItem(THEME_KEY, choice);
      } catch {
        // Private mode: the choice lasts for this page only.
      }
      applyTheme(choice);
    });
    // Radio group keys: arrows move and select.
    group.addEventListener('keydown', (e) => {
      if (!['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(e.key)) return;
      const buttons = [...group.querySelectorAll('[data-theme-choice]')];
      const at = buttons.indexOf(document.activeElement);
      const step = e.key === 'ArrowLeft' || e.key === 'ArrowUp' ? -1 : 1;
      const next = buttons[(at + step + buttons.length) % buttons.length];
      next.focus();
      next.click();
      e.preventDefault();
    });
  }

  // ---------- Mobile menu ----------

  function initMenu() {
    const nav = document.querySelector('.nav');
    const button = nav && nav.querySelector('.nav-menu-btn');
    if (!button) return;
    const setOpen = (open) => {
      nav.classList.toggle('open', open);
      button.setAttribute('aria-expanded', String(open));
    };
    button.addEventListener('click', () => setOpen(!nav.classList.contains('open')));
    // Following a link (often an anchor on this same page) closes it.
    nav.querySelector('.nav-links').addEventListener('click', (e) => {
      if (e.target.closest('a')) setOpen(false);
    });
    document.addEventListener('click', (e) => {
      if (!nav.contains(e.target)) setOpen(false);
    });
    document.addEventListener('keydown', (e) => {
      if (e.key === 'Escape' && nav.classList.contains('open')) {
        setOpen(false);
        button.focus();
      }
    });
    window.matchMedia('(min-width: 1021px)').addEventListener('change', () => setOpen(false));
  }

  // ---------- Hero monitor: four apps beating, one goes down and recovers ----------

  const BEATS = 40;
  const TICK_MS = 1100;
  // The outage loop: ticks 14 to 21 of every 34 are down, the two around them degraded.
  const CYCLE = 34;

  const PROFILES = {
    steady: (base, t) => ({ state: 'up', ms: jitter(base, 0.12, t) }),
    slow: (base, t) => {
      const spike = t % 23 === 7 || t % 23 === 8;
      return spike
        ? { state: 'degraded', ms: jitter(base * 3.4, 0.08, t) }
        : { state: 'up', ms: jitter(base, 0.18, t) };
    },
    outage: (base, t) => {
      const phase = t % CYCLE;
      if (phase >= 14 && phase <= 21) return { state: 'down', ms: null };
      if (phase === 13 || phase === 22) return { state: 'degraded', ms: jitter(base * 9, 0.1, t) };
      return { state: 'up', ms: jitter(base, 0.15, t) };
    },
  };

  // Deterministic noise, so every visitor sees the same believable numbers.
  function jitter(base, spread, t) {
    const n = Math.sin(t * 12.9898 + base * 78.233) * 43758.5453;
    return Math.round(base * (1 + (n - Math.floor(n) - 0.5) * 2 * spread));
  }

  function initMonitor() {
    const monitor = document.querySelector('.monitor');
    if (!monitor) return;
    const labels = { up: monitor.dataset.up, degraded: monitor.dataset.degraded, down: monitor.dataset.down };
    const verdict = monitor.querySelector('[data-verdict]');
    const rows = [...monitor.querySelectorAll('.monitor-row')].map((el) => ({
      el,
      profile: PROFILES[el.dataset.profile] || PROFILES.steady,
      base: parseInt(el.querySelector('.row-lat').textContent, 10) || 100,
      beats: el.querySelector('.beats'),
      vital: el.querySelector('.vital'),
      label: el.querySelector('.row-state'),
      lat: el.querySelector('.row-lat'),
    }));

    // Start mid-history so the strips are full; with reduced motion, freeze on a moment
    // that shows a recent outage.
    let t = reduceMotion ? 26 : 8;

    function beat(state, fresh) {
      const b = document.createElement('span');
      b.className = 'beat' + (state === 'up' ? '' : ' st-' + state) + (fresh ? ' fresh' : '');
      return b;
    }

    function show(row, reading) {
      row.el.classList.toggle('is-down', reading.state === 'down');
      row.el.classList.toggle('is-degraded', reading.state === 'degraded');
      row.vital.className = 'vital' + (reading.state === 'up' ? '' : ' st-' + reading.state);
      row.label.textContent = labels[reading.state];
      row.lat.textContent = reading.ms === null ? '—' : reading.ms + ' ms';
    }

    function showVerdict() {
      const states = rows.map((r) => (r.el.classList.contains('is-down') ? 'down' : r.el.classList.contains('is-degraded') ? 'degraded' : 'up'));
      const worst = states.includes('down') ? 'down' : states.includes('degraded') ? 'degraded' : 'up';
      verdict.className = 'vital' + (worst === 'up' ? '' : ' st-' + worst);
    }

    for (const row of rows) {
      const frag = document.createDocumentFragment();
      let last;
      for (let i = BEATS - 1; i >= 0; i--) {
        last = row.profile(row.base, t + CYCLE - i);
        frag.appendChild(beat(last.state, false));
      }
      row.beats.replaceChildren(frag);
      show(row, last);
    }
    showVerdict();
    if (reduceMotion) return;

    function tick() {
      t += 1;
      for (const row of rows) {
        const reading = row.profile(row.base, t + CYCLE);
        row.beats.firstElementChild.remove();
        row.beats.appendChild(beat(reading.state, true));
        show(row, reading);
      }
      showVerdict();
    }

    // Only beat while the monitor is on screen and the tab is visible.
    let timer = null;
    let onScreen = true;
    const run = () => {
      const should = onScreen && !document.hidden;
      if (should && !timer) timer = setInterval(tick, TICK_MS);
      if (!should && timer) {
        clearInterval(timer);
        timer = null;
      }
    };
    new IntersectionObserver((entries) => {
      onScreen = entries[0].isIntersecting;
      run();
    }).observe(monitor);
    document.addEventListener('visibilitychange', run);
    run();
  }

  // ---------- Screenshot tabs ----------

  function initTabs() {
    const list = document.querySelector('.tabs[role="tablist"]');
    const img = document.querySelector('.screen img');
    if (!list || !img) return;
    const tabs = [...list.querySelectorAll('[role="tab"]')];
    // Warm the cache once the gallery is near, so switching is instant.
    new IntersectionObserver((entries, observer) => {
      if (!entries[0].isIntersecting) return;
      for (const tab of tabs) new Image().src = tab.dataset.src;
      observer.disconnect();
    }, { rootMargin: '400px' }).observe(list);

    function select(tab) {
      for (const other of tabs) {
        const on = other === tab;
        other.setAttribute('aria-selected', String(on));
        other.tabIndex = on ? 0 : -1;
      }
      if (img.getAttribute('src') === tab.dataset.src) return;
      img.classList.add('swapping');
      setTimeout(() => {
        img.src = tab.dataset.src;
        img.alt = tab.dataset.alt;
        const done = () => img.classList.remove('swapping');
        if (img.complete) done();
        else img.addEventListener('load', done, { once: true });
      }, reduceMotion ? 0 : 180);
    }

    tabs.forEach((tab, i) => {
      tab.tabIndex = i === 0 ? 0 : -1;
      tab.addEventListener('click', () => select(tab));
      tab.addEventListener('keydown', (e) => {
        const step = e.key === 'ArrowRight' ? 1 : e.key === 'ArrowLeft' ? -1 : 0;
        if (!step) return;
        const next = tabs[(i + step + tabs.length) % tabs.length];
        next.focus();
        select(next);
        e.preventDefault();
      });
    });
  }

  // ---------- Copy buttons ----------

  function initCopy() {
    document.addEventListener('click', async (e) => {
      const button = e.target.closest('[data-copy]');
      if (!button) return;
      const code = button.parentElement.querySelector('code');
      try {
        await navigator.clipboard.writeText(code.textContent.trim());
      } catch {
        // No clipboard access (http, old browser): select the text so it can be copied.
        const range = document.createRange();
        range.selectNodeContents(code);
        const sel = window.getSelection();
        sel.removeAllRanges();
        sel.addRange(range);
        return;
      }
      button.textContent = button.dataset.done;
      button.classList.add('done');
      clearTimeout(button._reset);
      button._reset = setTimeout(() => {
        button.textContent = button.dataset.label;
        button.classList.remove('done');
      }, 1600);
    });
  }

  // ---------- Docs: highlight the section being read ----------

  function initToc() {
    const links = [...document.querySelectorAll('.docs-toc a[href^="#"]')];
    if (!links.length) return;
    const byId = new Map(links.map((a) => [decodeURIComponent(a.hash.slice(1)), a]));
    const headings = [...document.querySelectorAll('.prose h2[id], .prose h3[id]')].filter((h) => byId.has(h.id));
    let current = null;
    function update() {
      let active = headings[0];
      for (const h of headings) {
        if (h.getBoundingClientRect().top < 140) active = h;
        else break;
      }
      const link = active && byId.get(active.id);
      if (link === current) return;
      if (current) current.classList.remove('active');
      if (link) link.classList.add('active');
      current = link;
    }
    let queued = false;
    window.addEventListener('scroll', () => {
      if (queued) return;
      queued = true;
      requestAnimationFrame(() => {
        queued = false;
        update();
      });
    }, { passive: true });
    update();
  }

  document.addEventListener('DOMContentLoaded', () => {
    initTheme();
    initMenu();
    initMonitor();
    initTabs();
    initCopy();
    initToc();
  });
})();
