// Ordinary Telegram user -> UI token -> authenticated MCP -> revoke, against isolated mock data.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash, createHmac } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync } from 'node:fs';
import { createServer } from 'node:net';
import { resolve } from 'node:path';

export default async function ({ browser, check, MOCK }) {
  mkdirSync('work', { recursive: true });
  mkdirSync('test-results/ui', { recursive: true });
  const socket = createServer();
  await new Promise((done) => socket.listen(0, '127.0.0.1', done));
  const port = socket.address().port;
  await new Promise((done) => socket.close(done));
  const base = 'http://127.0.0.1:' + port;
  const python = ['.venv/bin/python', '.venv/Scripts/python.exe'].find(existsSync) || 'python3';
  const server = spawn(python, ['-m', 'uvicorn', 'main:app', '--host', '127.0.0.1', '--port', String(port), '--no-access-log'], {
    env: { ...process.env, PYTHONPATH: resolve('app'), DATA_DIR: resolve(mkdtempSync('work/token-e2e-')),
      STUDIO_PUBLIC: 'on', STUDIO_MODE: 'server', STUDIO_URL: 'https://public.example', TG_BOT_USERNAME: 'YourBot',
      TG_BOT_TOKEN: '1:x', TG_BOT_TOKEN_FILE: '', STUDIO_API_KEY: '', STUDIO_API_KEY_FILE: '',
      ALLOWED_HOSTS: '127.0.0.1', TG_API_BASE: MOCK, SPEC_PIN: 'on', STUDIO_BACKGROUND: 'off', TG_UPDATES: 'off',
      NO_PROXY: 'localhost,127.0.0.1' }, stdio: ['ignore', 'ignore', 'pipe'],
  });
  let log = '';
  server.stderr.on('data', (chunk) => { log = (log + chunk).slice(-6000); });
  const context = await browser.newContext({ viewport: { width: 1400, height: 1000 } });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  const openTokens = async (english = false) => {
    await page.locator('#account-menu').click();
    await page.getByRole('button', { name: english ? 'API and MCP tokens' : 'Токены API и MCP', exact: true }).click();
    await page.locator('rs-account-tokens #token-name').waitFor();
    await page.waitForFunction(() => !document.querySelector('rs-account-tokens').busy);
  };
  try {
    const deadline = Date.now() + 15000;
    while (true) {
      try { if ((await fetch(base + '/healthz')).ok) break; } catch { /* starting */ }
      if (Date.now() > deadline || server.exitCode !== null) throw new Error('Token studio did not start: ' + log);
      await new Promise((done) => setTimeout(done, 50));
    }
    const start = await (await context.request.post(base + '/api/auth/telegram/start', { data: {} })).json();
    const state = new URL(start.auth_url).searchParams.get('state');
    const fields = { id: '303', first_name: 'Тест токенов', auth_date: String(Math.floor(Date.now() / 1000)) };
    const signed = Object.entries(fields).sort(([a], [b]) => a.localeCompare(b)).map(([k, v]) => k + '=' + v).join('\n');
    const hash = createHmac('sha256', createHash('sha256').update('1:x').digest()).update(signed).digest('hex');
    await page.goto(base + '/auth/telegram/callback?' + new URLSearchParams({ ...fields, hash, state }));
    await page.getByRole('button', { name: 'Закрыть', exact: true }).click();
    await openTokens();
    const panel = page.locator('rs-account-tokens');
    assert.equal(await panel.locator('#token-role').inputValue(), 'draft');
    assert.equal(await panel.locator('#token-expiry').inputValue(), '90');
    assert.equal(await panel.locator('#token-role option').count(), 3);
    await page.screenshot({ path: 'test-results/ui/tokens-ru-1400.png', fullPage: true });
    await panel.locator('#token-name').fill('Claude <img src=x onerror=alert(1)>');
    await panel.getByRole('button', { name: 'Создать токен', exact: true }).click();
    const secretField = panel.locator('#secret');
    await secretField.waitFor();
    const secret = await secretField.inputValue();
    const config = JSON.parse(await panel.locator('#mcp-config').inputValue());
    assert.equal(config.mcpServers.richpost.headers.Authorization, 'Bearer ' + secret);
    assert.equal(config.mcpServers.richpost.url, 'https://public.example/mcp');
    assert.equal(await panel.locator('img').count(), 0);
    assert.ok(!JSON.stringify(await page.evaluate(() => ({ ...localStorage, ...sessionStorage }))).includes(secret));
    check('Account UI creates a draft token once, with MCP configuration and escaped connection names', true);
    await context.grantPermissions(['clipboard-read', 'clipboard-write']);
    await panel.getByRole('button', { name: 'Скопировать конфигурацию', exact: true }).click();
    await panel.getByText('Конфигурация скопирована', { exact: true }).waitFor();
    assert.equal(JSON.parse(await page.evaluate(() => navigator.clipboard.readText())).mcpServers.richpost.headers.Authorization, 'Bearer ' + secret);
    check('MCP configuration can be copied without shell access', true);
    // No Telegram requests: demonstrate actual Streamable HTTP draft tools using the UI-issued token.
    const rpc = async (name, args = {}) => {
      const response = await fetch(base + '/mcp', { method: 'POST', headers: {
        Authorization: 'Bearer ' + secret, 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream',
      }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name, arguments: args } }) });
      const raw = await response.text();
      const data = raw.startsWith('{') ? JSON.parse(raw) : JSON.parse(raw.split('\n').find((s) => s.startsWith('data: ')).slice(6));
      return data.result;
    };
    assert.ok(!(await rpc('save_draft', { title: 'MCP personal draft', post: { rich_message: { html: '<p>From account token</p>' } } })).isError);
    assert.ok((await rpc('list_drafts')).content.some((c) => c.text?.includes('MCP personal draft')));
    assert.ok((await rpc('publish_post', { post: { rich_message: { html: '<p>Blocked</p>' } } })).isError);
    check('UI-issued credential calls real MCP in its account and draft role cannot publish', true);
    await page.setViewportSize({ width: 390, height: 844 });
    await page.screenshot({ path: 'test-results/ui/tokens-secret-ru-390.png', fullPage: true, mask: [secretField, panel.locator('#mcp-config')] });
    await panel.getByRole('button', { name: 'Я сохранил токен', exact: true }).click();
    await page.getByRole('button', { name: 'Закрыть', exact: true }).click();
    await page.selectOption('#locale-select', 'en');
    await page.waitForFunction(() => document.documentElement.lang === 'en');
    await page.getByRole('button', { name: 'Close', exact: true }).click(); // channel onboarding on reload
    await openTokens(true);
    assert.equal(await panel.locator('#secret').count(), 0);
    await panel.getByText('Active', { exact: true }).waitFor();
    await page.screenshot({ path: 'test-results/ui/tokens-en-390.png', fullPage: true });
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
    await page.evaluate(() => { document.documentElement.dataset.theme = 'dark'; });
    await page.screenshot({ path: 'test-results/ui/tokens-en-dark-390.png', fullPage: true });
    await page.setViewportSize({ width: 1400, height: 1000 });
    await page.screenshot({ path: 'test-results/ui/tokens-en-dark-1400.png', fullPage: true });
    check('Token management translates RU/EN, fits mobile and never retrieves the secret again', true);
    await panel.locator('li button').first().click();
    await panel.getByRole('button', { name: 'Yes, revoke', exact: true }).click();
    await panel.getByText('Revoked', { exact: true }).waitFor();
    assert.equal((await fetch(base + '/api/drafts', { headers: { Authorization: 'Bearer ' + secret } })).status, 401);
    assert.equal((await context.request.get(base + '/api/session')).status(), 200);
    check('Revocation stops the external credential and preserves the Telegram browser session', true);
    check('Token screen has no JavaScript errors', errors.length === 0, errors.join(' | '));
  } finally {
    await context.close(); server.kill('SIGTERM');
    await new Promise((done) => server.exitCode !== null ? done() : server.once('exit', done));
  }
}
