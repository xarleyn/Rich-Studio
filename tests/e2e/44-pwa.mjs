// Installation UI, actual worker lifecycle and cache boundary in a fresh browser context.
import assert from 'node:assert/strict';
import { mkdirSync } from 'node:fs';

const SNOOZE_KEY = 'rs.pwa.snoozeUntil';
const SESSION_KEY = 'rs.pwa.offered';
const THIRTY_DAYS = 30 * 24 * 60 * 60 * 1000;
const offer = (page, outcome = 'dismissed') => page.evaluate((result) => {
  window.__pwaPromptCount ||= 0;
  const event = new Event('beforeinstallprompt', { cancelable: true });
  event.prompt = async () => { window.__pwaPromptCount++; };
  event.userChoice = Promise.resolve({ outcome: result });
  dispatchEvent(event);
}, outcome);
const engage = async (page) => {
  await page.evaluate(() => { document.activeElement?.blur(); });
  await page.keyboard.press('Shift');
};
const reopen = async (page, clear = false) => {
  if (clear) await page.evaluate(([snooze, session]) => {
    localStorage.removeItem(snooze); sessionStorage.removeItem(session);
  }, [SNOOZE_KEY, SESSION_KEY]);
  await page.reload();
  await page.waitForSelector('.block');
};

export default async function ({ browser, BASE, check }) {
  const context = await browser.newContext({ viewport: { width: 1400, height: 900 } });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  mkdirSync('test-results/ui', { recursive: true });
  try {
    await page.clock.install();
    await page.goto(BASE);
    await page.waitForSelector('.block');
    await page.evaluate(() => navigator.serviceWorker.ready);
    await page.reload();
    await page.waitForFunction(() => !!navigator.serviceWorker.controller);
    const manifest = await page.evaluate(async () => (await fetch(document.querySelector('link[rel=manifest]').href)).json());
    assert.equal(manifest.display, 'standalone');
    assert.equal(manifest.start_url, '/');

    assert.equal(await page.locator('.top rs-install, .login-card rs-install').count(), 0);
    assert.equal(await page.locator('#install-app section').count(), 0);
    await offer(page);
    await page.clock.fastForward(46000);
    assert.equal(await page.locator('#install-app section').count(), 0, 'No offer without interaction');
    await page.evaluate(() => document.body.classList.add('signed-out'));
    await engage(page); await page.clock.fastForward(1500);
    assert.equal(await page.locator('#install-app section').count(), 0, 'No offer over sign-in');
    await page.evaluate(() => {
      document.body.classList.remove('signed-out');
      Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => 'hidden' });
      document.dispatchEvent(new Event('visibilitychange'));
    });
    await page.clock.fastForward(1500);
    assert.equal(await page.locator('#install-app section').count(), 0, 'No offer in a hidden tab');
    await page.evaluate(() => { delete document.visibilityState; });
    await page.locator('#title').focus();
    await page.keyboard.press('Shift');
    await page.clock.fastForward(1500);
    assert.equal(await page.locator('#install-app section').count(), 0, 'Typing postpones the offer');
    await engage(page);
    await page.evaluate(async () => { (await import('/static/js/dialogs.js')).openDialog(['PWA test dialog']); });
    await page.clock.fastForward(1500);
    assert.equal(await page.locator('#install-app section').count(), 0, 'A modal postpones the offer');
    await page.getByRole('button', { name: 'Закрыть', exact: true }).click();
    const focused = await page.evaluate(() => document.activeElement?.id);
    await page.clock.fastForward(1500);
    await page.locator('#install-app section').waitFor();
    assert.equal(await page.evaluate(() => document.activeElement?.id), focused, 'Offer does not take focus');
    assert.equal(await page.locator('dialog[open]').count(), 0, 'Offer is nonmodal');
    assert.equal(await page.evaluate(() => window.__pwaPromptCount), 0, 'No native prompt without its own click');
    await page.screenshot({ path: 'test-results/ui/pwa-install-1400.png' });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.screenshot({ path: 'test-results/ui/pwa-install-390.png' });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
    check('PWA: delayed nonmodal offer waits for interaction and an idle editor; no header/login button', true);

    // Even an ignored offer is not repeated after reload in this browsing session.
    await reopen(page);
    await offer(page); await engage(page); await page.clock.fastForward(46000);
    assert.equal(await page.locator('#install-app section').count(), 0);
    await reopen(page, true);
    await offer(page); await engage(page); await page.clock.fastForward(46000);
    await page.locator('#install-app #later').click();
    const snoozed = await page.evaluate((key) => Number(localStorage.getItem(key)) - Date.now(), SNOOZE_KEY);
    assert.ok(snoozed <= THIRTY_DAYS && snoozed > THIRTY_DAYS - 10000);
    await page.evaluate((key) => sessionStorage.removeItem(key), SESSION_KEY); // a new session, same browser preference
    await reopen(page);
    await offer(page); await engage(page); await page.clock.fastForward(46000);
    assert.equal(await page.locator('#install-app section').count(), 0, 'Dismissal survives a new session');
    await page.evaluate((key) => localStorage.setItem(key, String(Date.now() - 1)), SNOOZE_KEY);
    await reopen(page);
    await offer(page); await engage(page); await page.clock.fastForward(46000);
    await page.locator('#install-app .close').click();
    assert.ok(await page.evaluate((key) => Number(localStorage.getItem(key)) > Date.now(), SNOOZE_KEY));
    check('PWA: ignored offers stay quiet for the session; Later/close persist a 30-day pause', true);

    // Native cancellation is also a dismissal; repeated readiness events do not nag again.
    await reopen(page, true);
    await offer(page); await engage(page); await page.clock.fastForward(46000);
    await page.locator('#install-app #install').click();
    await page.waitForFunction(() => window.__pwaPromptCount === 1 && !document.querySelector('#install-app').busy);
    assert.ok(await page.evaluate((key) => Number(localStorage.getItem(key)) > Date.now(), SNOOZE_KEY));
    await offer(page); await page.clock.fastForward(46000);
    assert.equal(await page.locator('#install-app section').count(), 0);
    assert.equal(await page.evaluate(() => window.__pwaPromptCount), 1);
    await reopen(page, true);
    await offer(page, 'accepted'); await engage(page); await page.clock.fastForward(46000);
    await page.locator('#install-app #install').click();
    await page.waitForFunction(() => !document.querySelector('#install-app').shown);
    await page.evaluate(() => dispatchEvent(new Event('appinstalled')));
    await offer(page); await page.clock.fastForward(46000);
    await page.waitForFunction(() => !document.querySelector('#install-app').shadowRoot.querySelector('button'));
    check('PWA: native prompt requires an explicit click; dismissal and installation suppress repeated offers', true);

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

  // iOS Safari gets instructions only after choosing its quiet offer; unavailable storage suppresses automation.
  for (const variant of ['ios', 'getItem', 'setItem', 'standalone']) {
    const storageBlocked = variant === 'getItem' || variant === 'setItem';
    const ios = await browser.newContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true,
      userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1' });
    const tab = await ios.newPage();
    const failures = [];
    tab.on('pageerror', (error) => failures.push(error.message));
    if (storageBlocked) await tab.addInitScript((method) => {
      Storage.prototype[method] = () => { throw new DOMException('Storage disabled', 'SecurityError'); };
    }, variant);
    if (variant === 'standalone') await tab.addInitScript(() => {
      Object.defineProperty(navigator, 'standalone', { get: () => true });
    });
    try {
      await tab.clock.install();
      await tab.goto(BASE); await tab.waitForSelector('.block');
      await engage(tab); await tab.clock.fastForward(46000);
      if (variant === 'standalone') {
        await offer(tab); await tab.clock.fastForward(46000);
        assert.equal(await tab.locator('#install-app section').count(), 0);
        assert.equal(await tab.evaluate(() => window.__pwaPromptCount), 0);
      } else if (storageBlocked) {
        assert.equal(await tab.locator('#install-app section').count(), 0);
        await offer(tab); await tab.clock.fastForward(46000);
        assert.equal(await tab.locator('#install-app section').count(), 0);
        await tab.evaluate(async () => { (await import('/static/js/pwa.js')).requestInstall(); });
        await tab.waitForFunction(() => window.__pwaPromptCount === 1); // manual intent still works
      } else {
        await tab.locator('#install-app #install').waitFor();
        assert.equal(await tab.locator('#install-app #install').textContent(), 'Как установить');
        assert.equal(await tab.locator('dialog[open]').count(), 0);
        await tab.locator('#install-app #install').click();
        await tab.getByRole('heading', { name: 'Установить RichPost', exact: true }).waitFor();
        assert.ok(await tab.locator('dialog').textContent().then((text) => text.includes('На экран Домой')));
        await tab.screenshot({ path: 'test-results/ui/pwa-ios-help-390.png' });
        await tab.getByRole('button', { name: 'Закрыть', exact: true }).click();
        await reopen(tab); await engage(tab); await tab.clock.fastForward(46000);
        assert.equal(await tab.locator('#install-app section').count(), 0);
      }
      assert.deepEqual(failures, []);
      check(variant === 'standalone' ? 'PWA: an installed standalone app never offers installation'
        : storageBlocked ? `PWA: ${variant} exceptions keep automatic offers off; manual installation remains usable`
          : 'PWA: Safari on iPhone offers instructions without an automatic modal or repeated nagging', true);
    } finally { await ios.close(); }
  }
}
