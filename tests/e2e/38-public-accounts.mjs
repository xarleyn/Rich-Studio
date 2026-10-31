// A separate public-mode studio: browser login, onboarding, isolation and logout against the real API.
import { spawn } from 'node:child_process';
import { createHash, createHmac } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync } from 'node:fs';
import { createServer } from 'node:net';
import { resolve } from 'node:path';

export default async function ({ browser, check, MOCK }) {
  mkdirSync('work', { recursive: true });
  mkdirSync('test-results/ui', { recursive: true });
  const data = resolve(mkdtempSync('work/public-e2e-'));
  const socket = createServer();
  await new Promise((done) => socket.listen(0, '127.0.0.1', done));
  const port = socket.address().port;
  await new Promise((done) => socket.close(done));
  const base = 'http://127.0.0.1:' + port;
  const server = spawn(['.venv/bin/python', '.venv/Scripts/python.exe'].find(existsSync) || 'python3', [
    '-m', 'uvicorn', 'main:app', '--host', '127.0.0.1', '--port', String(port), '--no-access-log',
  ], { env: { ...process.env, PYTHONPATH: resolve('app'), DATA_DIR: data, STUDIO_PUBLIC: 'on', STUDIO_MODE: 'server',
    STUDIO_URL: 'https://public.example', TG_BOT_USERNAME: 'YourBot', TG_BOT_TOKEN: '1:x', TG_BOT_TOKEN_FILE: '',
    TG_OWNER_ID: '555000111',
    STUDIO_API_KEY: '', STUDIO_API_KEY_FILE: '', ALLOWED_HOSTS: '127.0.0.1', TG_API_BASE: MOCK, SPEC_PIN: 'on',
    STUDIO_BACKGROUND: 'off', TG_UPDATES: 'off', NO_PROXY: 'localhost,127.0.0.1' }, stdio: ['ignore', 'ignore', 'pipe'] });
  let log = '';
  server.stderr.on('data', (chunk) => { log = (log + chunk).slice(-8000); });
  const contexts = [];
  try {
    const deadline = Date.now() + 15000;
    while (true) {
      try { if ((await fetch(base + '/healthz')).ok) break; } catch { /* not listening yet */ }
      if (Date.now() > deadline || server.exitCode !== null) throw new Error('Public studio did not start: ' + log);
      await new Promise((done) => setTimeout(done, 50));
    }
    const first = await browser.newContext({ viewport: { width: 1400, height: 900 } });
    contexts.push(first);
    // Telegram itself is external. Stub only its SDK, keeping all session and account API calls real.
    await first.route('https://telegram.org/js/telegram-widget.js?*', (route) => route.fulfill({
      contentType: 'application/javascript', body: `const s = document.currentScript;
        const b = document.createElement('button'); b.textContent = s.dataset.lang === 'en' ? 'Log in with Telegram' : 'Войти через Telegram'; b.id = 'telegram-widget-test'; s.after(b);`,
    }));
    const page = await first.newPage();
    const errors = [];
    page.on('pageerror', (e) => errors.push(e.message));
    page.on('console', (m) => { if (/Content Security Policy/i.test(m.text())) errors.push(m.text()); });
    await page.goto(base);
    await page.waitForSelector('#telegram-widget-test');
    await page.selectOption('#locale-select', 'en');
    await page.waitForFunction(() => document.querySelector('[data-telegram-login]')?.getAttribute('data-lang') === 'en');
    check('English sign-in translates onboarding and the official Telegram widget', await page.getByRole('heading', { name: 'Your channels. Your posts.' }).isVisible()
      && await page.locator('#locale-select').isVisible());
    await page.screenshot({ path: 'test-results/ui/public-login-en-1400.png', fullPage: true });
    await page.selectOption('#locale-select', 'ru');
    await page.waitForFunction(() => document.querySelector('[data-telegram-login]')?.getAttribute('data-lang') === 'ru');
    check('Public visitor sees Telegram entry with the editor hidden', await page.locator('#login-screen').isVisible() && !await page.locator('.layout').isVisible());
    check('With TG_OWNER_ID set the login form has no owner key button', await page.locator('#login-screen').getByRole('button', { name: 'Вход владельца сервиса' }).count() === 0
      && await page.evaluate(async () => (await (await fetch('/api/auth/config')).json()).owner_login === true));
    const widget = page.locator('[data-telegram-login]');
    check('Official widget receives the right bot, callback and write access',
      await widget.getAttribute('data-telegram-login') === 'YourBot'
      && (await widget.getAttribute('data-auth-url')).startsWith('https://public.example/auth/telegram/callback?state=')
      && await widget.getAttribute('data-request-access') === 'write');
    await page.screenshot({ path: 'test-results/ui/public-login-1400.png', fullPage: true });
    await page.setViewportSize({ width: 390, height: 844 });
    check('Public entry fits a phone and hides editor navigation', await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth) && !await page.locator('#view-tabs').isVisible());
    await page.screenshot({ path: 'test-results/ui/public-login-390.png', fullPage: true });

    const signIn = async (context, target, uid, name) => {
      const start = await (await context.request.post(base + '/api/auth/telegram/start', { data: {} })).json();
      const state = new URL(start.auth_url).searchParams.get('state');
      const fields = { id: String(uid), first_name: name, auth_date: String(Math.floor(Date.now() / 1000)) };
      const text = Object.entries(fields).sort(([a], [b]) => a.localeCompare(b)).map(([k, v]) => k + '=' + v).join('\n');
      const hash = createHmac('sha256', createHash('sha256').update('1:x').digest()).update(text).digest('hex');
      await target.goto(base + '/auth/telegram/callback?' + new URLSearchParams({ ...fields, hash, state }));
      await target.waitForSelector('.connect-channel');
    };
    await signIn(first, page, 101, 'Мария');
    check('Signed-in user sees their account and channel onboarding', await page.locator('#account-menu').textContent() === 'Мария');
    await page.screenshot({ path: 'test-results/ui/public-onboarding-390.png', fullPage: true });
    await page.getByPlaceholder('@канал или -100…').fill('@mine');
    await page.getByRole('button', { name: 'Добавить и проверить', exact: true }).click();
    await page.getByRole('button', { name: 'Сюда', exact: true }).waitFor();
    await page.getByRole('button', { name: 'Сюда', exact: true }).click();
    check('Selecting a connected channel immediately updates the target', (await page.locator('#btn-check').textContent()).includes('Тест'));
    await page.locator('#title').fill('Черновик Марии');
    await page.locator('#btn-save').click();
    await page.waitForFunction(() => document.querySelector('#save-state').textContent.startsWith('сохранено'));
    await page.setViewportSize({ width: 1400, height: 900 });
    await page.screenshot({ path: 'test-results/ui/public-editor-1400.png', fullPage: true });
    const drafts = await (await first.request.get(base + '/api/drafts')).json();

    const second = await browser.newContext();
    contexts.push(second);
    const other = await second.newPage();
    await signIn(second, other, 202, 'Алексей');
    check('A second Telegram account cannot read the first account draft', (await second.request.get(base + '/api/drafts/' + drafts[0].id)).status() === 404);
    await other.getByRole('button', { name: 'Закрыть', exact: true }).click();
    await other.locator('#account-menu').click();
    await other.getByRole('button', { name: 'Выйти', exact: true }).click();
    await other.waitForSelector('#login-screen');
    check('Logout ends the session on the server', (await second.request.get(base + '/api/drafts')).status() === 401);
    check('Public editor has no JavaScript or CSP errors', errors.length === 0, errors.join(' | '));
  } finally {
    for (const context of contexts) await context.close();
    server.kill('SIGTERM');
    await new Promise((done) => server.exitCode !== null ? done() : server.once('exit', done));
  }
}
