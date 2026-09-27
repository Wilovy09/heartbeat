// Theme choice, per browser: system (the default: follows the OS light/dark setting,
// live), dark, light or custom (the CSS saved in Settings, served at /theme/custom.css on
// top of the dark tokens). Loaded in <head> right after the stylesheets, so the choice
// applies before the first paint and the page never flashes the wrong theme.
(function () {
  const KEY = 'hb-theme';
  const THEMES = ['dark', 'light', 'system', 'custom'];
  const prefersLight = window.matchMedia('(prefers-color-scheme: light)');

  function read() {
    try {
      const saved = localStorage.getItem(KEY);
      return THEMES.includes(saved) ? saved : 'system';
    } catch {
      return 'system';
    }
  }

  // The theme actually drawn: "system" becomes light or dark.
  function resolve(choice) {
    if (choice !== 'system') return choice;
    return prefersLight.matches ? 'light' : 'dark';
  }

  function apply(choice) {
    const root = document.documentElement;
    const theme = resolve(choice);
    // data-theme drives the tokens; data-theme-choice only the switch's icon.
    root.dataset.themeChoice = choice;
    if (theme === 'dark') delete root.dataset.theme;
    else root.dataset.theme = theme;
    const custom = document.getElementById('hb-custom-theme');
    if (custom) custom.media = theme === 'custom' ? 'all' : 'not all';
    for (const option of document.querySelectorAll('[data-theme-choice]')) {
      option.setAttribute('aria-checked', String(option.dataset.themeChoice === choice));
    }
    // Canvas-like widgets (the latency chart) read colors at draw time and redraw on this.
    document.dispatchEvent(new CustomEvent('hb:theme', { detail: theme }));
  }

  window.HBTheme = {
    current: read,
    set(theme) {
      if (!THEMES.includes(theme)) return;
      try {
        localStorage.setItem(KEY, theme);
      } catch {
        // Private mode: the choice lasts for this page only.
      }
      apply(theme);
    },
    apply,
    // The drawn theme ("light", "dark" or "custom"), with "system" resolved.
    resolved: () => resolve(read()),
    // A token's current value, e.g. HBTheme.token('--rhythm-up').
    token(name) {
      return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    },
  };

  apply(read());

  document.addEventListener('DOMContentLoaded', () => apply(read()));
  // With "system" chosen, follow the OS as it switches (e.g. at sunset).
  prefersLight.addEventListener('change', () => {
    if (read() === 'system') apply('system');
  });
  document.addEventListener('click', (e) => {
    const choice = e.target.closest('[data-theme-choice]');
    if (choice) {
      window.HBTheme.set(choice.dataset.themeChoice);
      const menu = choice.closest('details');
      if (menu) menu.open = false;
      return;
    }
    for (const menu of document.querySelectorAll('details.menu[open]')) {
      if (!menu.contains(e.target)) menu.open = false;
    }
  });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') for (const menu of document.querySelectorAll('details.menu[open]')) menu.open = false;
  });
})();
