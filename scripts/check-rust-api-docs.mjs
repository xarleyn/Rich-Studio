// Interactive documentation is an authenticated application surface, served entirely locally.
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { chromium } from 'playwright';

const base = process.env.BASE;
const key = process.env.DOCS_FIXTURE_KEY;
assert.ok(base && key, 'Run through scripts/check-rust-browser.mjs (isolated authenticated fixture).');
const errors = [];
for (const path of ['/docs', '/redoc', '/openapi.json']) {
  assert.equal((await fetch(base + path)).status, 401, `${path} must require the configured key`);
  assert.equal((await fetch(base + path, { headers: { Authorization: 'Bearer invalid-fixture-key' } })).status, 401);
}
const schema = await (await fetch(base + '/openapi.json', { headers: { Authorization: `Bearer ${key}` } })).json();
const operations = Object.values(schema.paths).flatMap((methods) => Object.values(methods).map((operation) => operation.operationId));
assert.equal(operations.length, 80);
const browser = await chromium.launch();
try {
  const context = await browser.newContext({ extraHTTPHeaders: { Authorization: `Bearer ${key}` }, viewport: { width: 1400, height: 900 } });
  await context.route('**/*', async (route) => {
    const url = new URL(route.request().url());
    if (['http:', 'https:'].includes(url.protocol) && url.origin !== new URL(base).origin) {
      errors.push('external documentation request: ' + url.origin + url.pathname);
      await route.abort();
    } else await route.continue();
  });
  const page = await context.newPage();
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('console', (message) => { if (message.type() === 'error' || /Content Security Policy/i.test(message.text())) errors.push(message.text()); });
  await mkdir('test-results/ui', { recursive: true });
  for (const [path, name] of [['/docs', 'swagger'], ['/redoc', 'redoc']]) {
    await page.setViewportSize({ width: 1400, height: 900 });
    const response = await page.goto(base + path);
    assert.equal(response.status(), 200);
    assert.ok(response.headers()['content-security-policy'].includes("script-src 'self'"));
    if (name === 'swagger') {
      await page.waitForFunction((count) => document.querySelectorAll('.opblock').length === count, operations.length);
      assert.equal(await page.locator('.opblock').count(), 80);
    } else {
      await page.waitForFunction((ids) => {
        const sections = [...document.querySelectorAll('[data-section-id]')].map((element) => element.getAttribute('data-section-id'));
        return ids.every((id) => sections.some((section) => section === 'operation/' + id || section.endsWith('/operation/' + id)));
      }, operations);
    }
    for (const width of [1400, 390]) {
      await page.setViewportSize({ width, height: width === 390 ? 844 : 900 });
      await page.screenshot({ path: `test-results/ui/api-${name}-${width}.png` });
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), `${name} must fit ${width}px`);
    }
    console.log(`PASS ${name}: all 80 operations render locally with the configured credential at 1400px and 390px.`);
  }
  assert.deepEqual(errors, [], 'API documentation must have no external requests, JavaScript or CSP errors');
  console.log('PASS API documentation: anonymous/invalid credentials rejected; no external requests, JavaScript or CSP errors.');
} finally {
  await browser.close();
}
