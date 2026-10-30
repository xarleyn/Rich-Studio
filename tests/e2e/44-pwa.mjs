// Installation UI, actual worker lifecycle and cache boundary in a fresh browser context.
import assert from 'node:assert/strict';
import { mkdirSync } from 'node:fs';

export default async function ({ browser, BASE, check }) {
  const context = await browser.newContext({ viewport: { width: 1400, height: 900 } });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  mkdirSync('test-results/ui', { recursive: true });
  try {
    await page.goto(BASE);
    await page.waitForSelector('.block');
    await page.evaluate(() => navigator.serviceWorker.ready);
    await page.reload();
    await page.waitForFunction(() => !!navigator.serviceWorker.controller);
    const manifest = await page.evaluate(async () => (await fetch(document.querySelector('link[rel=manifest]').href)).json());
    assert.equal(manifest.display, 'standalone');
    assert.equal(manifest.start_url, '/');

    // No prompt available: expose accurate platform instructions, not a false success.
    await page.locator('#install-app button').click();
    await page.getByRole('heading', { name: 'Установить RichPost', exact: true }).waitFor();
    await page.screenshot({ path: 'test-results/ui/pwa-install-1400.png' });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.screenshot({ path: 'test-results/ui/pwa-install-390.png' });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
    await page.getByRole('button', { name: 'Закрыть', exact: true }).click();
    check('PWA: manifest, root worker and install help on desktop/phone', true);

    // One saved browser prompt can be used only once, even after a dismissal.
    await page.evaluate(() => {
      window.__pwaPromptCount = 0;
      const event = new Event('beforeinstallprompt', { cancelable: true });
      event.prompt = async () => { window.__pwaPromptCount++; };
      event.userChoice = Promise.resolve({ outcome: 'dismissed' });
      dispatchEvent(event);
    });
    await page.locator('#install-app button').click();
    await page.waitForFunction(() => window.__pwaPromptCount === 1 && !document.querySelector('#install-app').busy);
    await page.locator('#install-app button').click();
    await page.getByRole('heading', { name: 'Установить RichPost', exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.__pwaPromptCount), 1);
    await page.getByRole('button', { name: 'Закрыть', exact: true }).click();
    await page.evaluate(() => dispatchEvent(new Event('appinstalled')));
    await page.waitForFunction(() => !document.querySelector('#install-app').shadowRoot.querySelector('button'));
    check('PWA: native prompt consumed once and installed state hides the action', true);

    // Read private content and write a draft with an active SW; none belongs in Cache Storage.
    const saved = await page.evaluate(async () => {
      const response = await fetch('/api/drafts', { method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ title: 'PWA private content', post: { rich_message: { blocks: [{ type: 'paragraph', text: 'Private cached nowhere' }] } } }) });
      if (!response.ok) throw new Error('Could not create isolated PWA draft');
      const draft = await response.json();
      await fetch('/api/drafts/' + draft.id);
      return draft.id;
    });
    const cachedPaths = async () => page.evaluate(async () => {
      const urls = [];
      for (const name of await caches.keys()) for (const req of await (await caches.open(name)).keys()) urls.push(new URL(req.url).pathname);
      return urls.sort();
    });
    assert.deepEqual(await cachedPaths(), ['/static/offline.html']);
    await context.setOffline(true);
    const blocked = await page.evaluate(async () => {
      try { await fetch('/api/drafts'); return false; } catch { return true; }
    });
    assert.equal(blocked, true);
    await page.goto(BASE + '/?draft=' + saved);
    await page.getByRole('heading', { name: 'Нет подключения', exact: true }).waitFor();
    assert.equal(await page.locator('body').textContent().then((s) => s.includes('Private cached nowhere')), false);
    assert.deepEqual(await cachedPaths(), ['/static/offline.html']);
    await page.screenshot({ path: 'test-results/ui/pwa-offline-390.png' });
    await context.setOffline(false);
    await page.getByRole('link', { name: 'Открыть студию / Open studio', exact: true }).click();
    await page.waitForSelector('.block');
    await context.request.delete(BASE + '/api/drafts/' + saved);
    assert.deepEqual(errors, []);
    check('PWA: only public fallback cached; offline API fails; reconnect opens studio', true);
  } finally {
    await context.setOffline(false);
    await context.close();
  }
}
