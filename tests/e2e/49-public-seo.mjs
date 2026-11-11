// Public HTML must stay crawlable and readable on both runtimes and both screen sizes.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync } from 'node:fs';
import { createServer } from 'node:net';
import { resolve } from 'node:path';
import { setTimeout } from 'node:timers/promises';

export default async function ({ browser, check, MOCK }) {
  mkdirSync('work', { recursive: true });
  mkdirSync('test-results/ui', { recursive: true });
  const data = resolve(mkdtempSync('work/seo-e2e-'));
  const listener = createServer();
  await new Promise((done, reject) => listener.once('error', reject).listen(0, '127.0.0.1', done));
  const port = listener.address().port;
  await new Promise((done) => listener.close(done));
  const app = `http://127.0.0.1:${port}`;
  const site = `http://localhost:${port}`;
  const python = process.env.RICH_STUDIO_PYTHON || ['.venv/bin/python', '.venv/Scripts/python.exe'].find(existsSync) || 'python3';
  const binary = process.env.RICH_STUDIO_BINARY;
  const server = spawn(binary || python, binary ? ['serve'] : [
    '-m', 'uvicorn', 'main:app', '--app-dir', 'app', '--host', '127.0.0.1', '--port', String(port), '--no-access-log',
  ], { windowsHide: true, env: {
    ...process.env, PYTHONUTF8: '1', DATA_DIR: data, STUDIO_MODE: 'server', STUDIO_PUBLIC: 'on',
    STUDIO_URL: `https://127.0.0.1:${port}`, STUDIO_LANDING_URL: `https://localhost:${port}`,
    STUDIO_BIND: `127.0.0.1:${port}`, STUDIO_STATIC_DIR: resolve('app/static'),
    STUDIO_API_KEY: 'seo-fixture-key', STUDIO_API_KEY_FILE: '', TG_BOT_TOKEN: '1:x', TG_BOT_TOKEN_FILE: '',
    TG_BOT_USERNAME: 'fixture_bot', TG_API_BASE: MOCK, ALLOWED_HOSTS: `localhost:${port},127.0.0.1:${port},localhost,127.0.0.1`,
    STUDIO_BACKGROUND: 'off', TG_UPDATES: 'off', SPEC_PIN: 'on', NO_PROXY: 'localhost,127.0.0.1',
  }, stdio: ['ignore', 'ignore', 'pipe'] });
  let startError;
  server.once('error', (error) => { startError = error; });
  const closed = new Promise((done) => server.once('close', done));
  let log = '';
  server.stderr.on('data', (chunk) => { log = (log + chunk).slice(-6000); });
  const context = await browser.newContext({ reducedMotion: 'reduce' });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('console', (message) => { if (/Content Security Policy|Content-Security-Policy/i.test(message.text())) errors.push(message.text()); });
  try {
    const deadline = Date.now() + 20000;
    while (true) {
      if (startError) throw startError;
      try { if ((await fetch(app + '/healthz')).ok) break; } catch { /* starting */ }
      if (Date.now() > deadline || server.exitCode !== null) throw new Error('SEO fixture did not start: ' + log);
      await setTimeout(50);
    }
    const guides = ['telegram-tables', 'telegram-markdown', 'scheduled-posts', 'ai-agents'];
    for (const width of [1400, 390]) {
      await page.setViewportSize({ width, height: width === 390 ? 844 : 1000 });
      for (const prefix of ['', '/en']) {
        for (const suffix of ['/', '/features/', ...guides.map((slug) => `/features/${slug}/`)]) {
          const path = prefix + suffix;
          const response = await page.goto(site + path);
          assert.equal(response.status(), 200, path);
          assert.equal(await page.locator('h1').count(), 1, path);
          assert.equal(await page.locator('html').getAttribute('lang'), prefix ? 'en' : 'ru');
          assert.equal(await page.locator('link[rel=canonical]').getAttribute('href'), `https://localhost:${port}${path}`);
          assert.ok(!response.headers()['x-robots-tag'], path);
          assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `overflow: ${path} at ${width}`);
          const schema = await page.locator('script[type="application/ld+json"]').textContent();
          assert.ok(JSON.parse(schema)['@graph'].length > 0);
          assert.equal(await page.locator('meta[name="twitter:card"]').getAttribute('content'), 'summary_large_image');
          assert.ok(await page.locator(`a[href="${prefix}/features/ai-agents/"]`).count(), path);
          if (suffix === '/' || suffix === '/features/' || suffix === '/features/telegram-tables/') {
            await page.screenshot({ path: `test-results/ui/seo-${prefix ? 'en' : 'ru'}-${suffix === '/' ? 'home' : suffix === '/features/' ? 'features' : 'tables'}-${width}.png`, fullPage: true });
          }
        }
      }
      const missing = await page.goto(site + '/en/no-such-page');
      assert.equal(missing.status(), 404);
      assert.equal(await page.locator('html').getAttribute('lang'), 'en');
      assert.ok(await page.locator('a[href="/en/"]').count());
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
      await page.screenshot({ path: `test-results/ui/seo-404-${width}.png`, fullPage: true });
      check(`All public pages, metadata, navigation and HTML 404 fit ${width}px`, true);
    }
    check('Public JSON-LD, versioned resources and local fonts have no CSP or JavaScript errors', errors.length === 0, errors.join(' | '));
  } finally {
    await context.close();
    server.kill();
    await closed;
  }
}
