import { readFileSync, mkdirSync } from 'node:fs';

/** @param {import('./harness.mjs').Ctx} ctx */
export default async function ({ page, check, BASE, applyJson, push }) {
  const cases = JSON.parse(readFileSync(new URL('../import_links.json', import.meta.url), 'utf8'));
  const parity = await page.evaluate(async (inputs) => {
    const { parseTelegramLink } = await import('/static/js/import-links.js');
    return inputs.map((c) => {
      try { const parsed = parseTelegramLink(c.url); return c.valid && parsed.message_id === c.message_id && parsed.topic_id === c.topic_id; }
      catch { return !c.valid; }
    });
  }, cases);
  check('Frontend and backend share the Telegram URL contract', parity.every(Boolean), JSON.stringify(parity));
  await applyJson({ rich_message: { blocks: [{ type: 'paragraph', text: 'Сохранить текущую работу' }] } });
  await page.click('#btn-import');
  await page.waitForSelector('rs-import input');
  await page.locator('rs-import input').fill('https://t.me.evil.org/simple_agi/203');
  check('Hostile URL disables import in UI', await page.locator('rs-import button').first().isDisabled());
  await page.locator('rs-import input').fill('https://t.me/mcoadvanced/2/3450');
  await page.waitForFunction(() => !document.querySelector('rs-import').shadowRoot.querySelector('button').disabled);
  mkdirSync('test-results/ui', { recursive: true });
  await page.screenshot({ path: 'test-results/ui/import-1400.png' });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: 'test-results/ui/import-390.png', fullPage: true });
  check('Import dialog fits mobile', await page.evaluate(() => {
    const dialog = document.querySelector('rs-import').closest('dialog');
    const box = dialog.getBoundingClientRect();
    return box.left >= 0 && box.right <= innerWidth && dialog.scrollWidth <= dialog.clientWidth;
  }));
  await page.locator('rs-import').screenshot({ path: 'test-results/ui/import-form-390.png' });
  await page.setViewportSize({ width: 1400, height: 900 });
  // Stub only upstream extraction, create the returned draft through the real API.
  const blocks = [{ type: 'heading', size: 2, text: 'Импортированный пост' }, { type: 'paragraph', text: 'Содержимое' }];
  const created = await page.request.post(BASE + '/api/drafts', { data: {
    title: 'Импорт SimpleAGI', post: { rich_message: { blocks }, _studio: {
      buffers: { blocks, html: '<h2>Импортированный пост</h2><p>Содержимое</p>', markdown: '## Импортированный пост\n\nСодержимое' },
      import_source: { url: 'https://t.me/simple_agi/203' }, import_warnings: ['Проверить исходную кнопку'],
    } },
  } });
  const draft = await created.json();
  await page.route('**/api/imports/telegram', async (route) => {
    if (route.request().method() === 'POST') await route.fulfill({ json: { draft, block_count: 2, media_count: 0, warnings: ['Проверить исходную кнопку'] } });
    else await route.continue();
  });
  await page.locator('rs-import button').first().click();
  await page.waitForFunction(() => document.querySelector('#title').value === 'Импорт SimpleAGI');
  check('Import opens a new draft and displays source and losses', (await page.locator('#import-info').innerText()).includes('Проверить исходную кнопку'));
  await page.click('#mode-tabs [data-mode=html]');
  check('Imported HTML buffer is available', (await page.inputValue('#src-html')).includes('Импортированный пост'));
  await page.click('#mode-tabs [data-mode=markdown]');
  check('Imported Markdown buffer is available', (await page.inputValue('#src-markdown')).includes('## Импортированный пост'));
  await page.click('#btn-save');
  await page.waitForFunction(() => document.querySelector('#save-state').textContent.startsWith('сохранено'));
  const saved = await (await page.request.get(BASE + '/api/drafts/' + draft.id)).json();
  check('Saving edits preserves import provenance', saved.payload._studio.import_source.url === 'https://t.me/simple_agi/203');
  await page.unroute('**/api/imports/telegram');
  await page.click('#btn-import');
  await page.waitForSelector('rs-import input');
  await page.locator('rs-import input').fill('https://t.me/simple_agi/203');
  await page.getByRole('button', { name: 'Через пересылку боту', exact: true }).click();
  const link = page.locator('rs-import a');
  await link.waitFor();
  const start = new URL(await link.getAttribute('href')).searchParams.get('start');
  const base = { from: { id: 777, is_bot: false }, chat: { id: 777, type: 'private' } };
  await push({ message: { ...base, message_id: 901, text: '/start ' + start } });
  await push({ message: { ...base, message_id: 902, text: 'Получено через пересылку',
    forward_origin: { type: 'channel', chat: { id: -100123, username: 'simple_agi' }, message_id: 203 } } });
  await page.waitForFunction(() => document.querySelector('#title').value === 'Импорт simple_agi/203', undefined, { timeout: 15000 });
  check('Bot forward travels through updates, scoped inbox, draft and editor', (await page.locator('#import-info').innerText()).includes('Содержимое получено'));
  await page.screenshot({ path: 'test-results/ui/import-result-1400.png' });
}
