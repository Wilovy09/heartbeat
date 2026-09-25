// End-to-end smoke test: drives a running Heartbeat (AUTH_MODE=password) through the main
// admin flows in a real browser. `just e2e` starts a throwaway server and runs this.
//
//   HB_BASE_URL=http://localhost:8199 HB_EMAIL=... HB_PASSWORD=... node smoke.mjs
import { chromium } from 'playwright';

const BASE = process.env.HB_BASE_URL ?? 'http://localhost:8199';
const EMAIL = process.env.HB_EMAIL ?? 'admin@example.com';
const PASSWORD = process.env.HB_PASSWORD ?? 'smoke-test-password';

let failures = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failures += 1;
};

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
const consoleErrors = [];
page.on('console', (m) => m.type() === 'error' && consoleErrors.push(m.text()));
page.on('pageerror', (e) => consoleErrors.push(e.message));

try {
  // Login
  await page.goto(`${BASE}/`);
  check(new URL(page.url()).pathname === '/login', 'anonymous visit redirects to /login');
  await page.fill('input[name=email]', EMAIL);
  await page.fill('input[name=password]', PASSWORD);
  await Promise.all([page.waitForNavigation(), page.click('button[type=submit]')]);
  check(new URL(page.url()).pathname === '/', 'login lands on the dashboard');

  // Register
  await page.goto(`${BASE}/apps`);
  await page.fill('#name', 'Smoke API');
  await page.fill('#logs_url', 'https://smoke.example.com/admin/logs');
  await page.fill('#health_url', 'https://smoke.example.com/health');
  await Promise.all([page.waitForNavigation(), page.click('form.intake button[type=submit]')]);
  const entry = page.locator('#app-smoke-api');
  check(await entry.count() === 1, 'app registered');

  // Rejected registration keeps the typed values
  await page.fill('#name', 'Evil API');
  await page.fill('#logs_url', 'https://evil.test/logs');
  await page.fill('#health_url', 'https://evil.test/health');
  await Promise.all([page.waitForNavigation(), page.click('form.intake button[type=submit]')]);
  check(await page.locator('form.intake .error-box').count() === 1, 'disallowed host is rejected');
  check(await page.inputValue('#name') === 'Evil API', 'rejected form keeps what was typed');

  // Edit
  await entry.locator('.entry-edit summary').click();
  await entry.locator('.edit-form [name=name]').fill('Smoke API v2');
  await Promise.all([page.waitForNavigation(), entry.locator('.edit-form button[type=submit]').click()]);
  check((await page.locator('#app-smoke-api .entry-name').innerText()) === 'Smoke API v2', 'edit saved, slug kept');

  // Pause + publish
  await Promise.all([page.waitForNavigation(), page.locator('#app-smoke-api form[action$="/pause"] button').click()]);
  check(await page.locator('#app-smoke-api .chip').first().isVisible(), 'app paused');
  await Promise.all([page.waitForNavigation(), page.locator('#app-smoke-api form[action$="/public"] button').click()]);
  await page.goto(`${BASE}/status`);
  check((await page.content()).includes('Smoke API v2'), 'public app listed on /status');

  // Dashboard renders it
  await page.goto(`${BASE}/#smoke-api`);
  await page.waitForTimeout(800);
  check(await page.locator('.page-title').innerText() === 'Smoke API v2', 'dashboard detail renders');

  // Logout
  await Promise.all([page.waitForNavigation(), page.click('.logout button')]);
  await page.goto(`${BASE}/apps`);
  check(new URL(page.url()).pathname === '/login', 'logout ends the session');

  check(consoleErrors.length === 0, `no console errors${consoleErrors.length ? ': ' + consoleErrors.join(' | ') : ''}`);
} finally {
  await browser.close();
}
process.exit(failures ? 1 : 0);
