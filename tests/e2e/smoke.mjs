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
  await page.click('[data-open-dialog="register-dialog"]');
  await page.fill('#name', 'Smoke API');
  await page.fill('#logs_url', 'https://smoke.example.com/admin/logs');
  await page.fill('#health_url', 'https://smoke.example.com/health');
  await Promise.all([page.waitForNavigation(), page.click('form.intake button[type=submit]')]);
  const entry = page.locator('#app-smoke-api');
  check(await entry.count() === 1, 'app registered');

  // Rejected registration keeps the typed values
  await page.click('[data-open-dialog="register-dialog"]');
  await page.fill('#name', 'Evil API');
  await page.fill('#logs_url', 'https://evil.test/logs');
  await page.fill('#health_url', 'https://evil.test/health');
  await Promise.all([page.waitForNavigation(), page.click('form.intake button[type=submit]')]);
  check(await page.locator('#register-dialog[open] form.intake .error-box').count() === 1, 'disallowed host is rejected, dialog reopened');
  check(await page.inputValue('#name') === 'Evil API', 'rejected form keeps what was typed');
  await page.click('#register-dialog .modal-foot [data-close-dialog]');
  check(await page.locator('#register-dialog[open]').count() === 0, 'cancel closes the dialog');

  // Edit
  await entry.locator('.entry-edit summary').click();
  await entry.locator('.edit-form [name=name]').fill('Smoke API v2');
  await Promise.all([page.waitForNavigation(), entry.locator('.edit-form button[type=submit]').click()]);
  check((await page.locator('#app-smoke-api .entry-name').innerText()) === 'Smoke API v2', 'edit saved, slug kept');

  // Per-app options: interval, expected status, own webhook
  await entry.locator('.entry-edit summary').click();
  await entry.locator('.edit-form [name=interval_secs]').fill('30');
  await entry.locator('.edit-form [name=expect_status]').fill('200, 401');
  await entry.locator('.edit-form [name=alert_webhooks]').fill('https://hooks.slack.com/services/T/B/smoke');
  await Promise.all([page.waitForNavigation(), entry.locator('.edit-form button[type=submit]').click()]);
  check((await entry.locator('.entry-urls').innerText()).includes('200, 401'), 'check options saved');

  // Scheduled pause (4 h), then resume and pause indefinitely + publish
  await entry.locator('.pause-menu summary').click();
  await Promise.all([page.waitForNavigation(), entry.locator('.pause-menu button[value="4"]').click()]);
  check(await entry.locator('.chip [data-until]').count() === 1, 'scheduled pause shows its end');
  await Promise.all([page.waitForNavigation(), entry.locator('form[action$="/pause"] button').click()]);
  await entry.locator('.pause-menu summary').click();
  await Promise.all([page.waitForNavigation(), entry.locator('.pause-menu button[value=""]').click()]);
  check(await page.locator('#app-smoke-api .chip').first().isVisible(), 'app paused');
  await entry.locator('.entry-share summary').click();
  await Promise.all([page.waitForNavigation(), page.locator('#app-smoke-api form[action$="/public"] button').click()]);
  await page.goto(`${BASE}/status/smoke-api`);
  check((await page.content()).includes('Smoke API v2'), 'public app has its own status page');

  // Incident notice on /status
  await page.goto(`${BASE}/notices`);
  await page.fill('#title', 'Smoke incident');
  await Promise.all([page.waitForNavigation(), page.click('form.notice-form button[type=submit]')]);
  // /status is cacheable for 30 s and was already visited: bypass the browser cache.
  await page.goto(`${BASE}/status/smoke-api?fresh=1`);
  check((await page.content()).includes('Smoke incident'), 'notice shown on /status');

  // Settings renders with no global webhooks
  await page.goto(`${BASE}/settings`);
  check(await page.locator('textarea#tpl-reminder').count() === 1, 'reminder template is editable');

  // Dashboard renders it
  await page.goto(`${BASE}/#smoke-api`);
  await page.waitForTimeout(800);
  check(await page.locator('.page-title').innerText() === 'Smoke API v2', 'dashboard detail renders');
  check(await page.locator('.incident-stats').isVisible(), 'incident stats render');

  // Logout
  await Promise.all([page.waitForNavigation(), page.click('.logout button')]);
  await page.goto(`${BASE}/apps`);
  check(new URL(page.url()).pathname === '/login', 'logout ends the session');

  check(consoleErrors.length === 0, `no console errors${consoleErrors.length ? ': ' + consoleErrors.join(' | ') : ''}`);
} finally {
  await browser.close();
}
process.exit(failures ? 1 : 0);
