// Language changes must preserve authored content, pending JSON and the selected account/draft.
export default async function ({ browser, check, BASE }) {
  const context = await browser.newContext({ viewport: { width: 1400, height: 1000 } });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  page.on('dialog', (dialog) => dialog.accept());
  try {
    await page.goto(BASE + '/?lang=en');
    await page.waitForSelector('.block');
    check('HTML source placeholder is parsed as an attribute without stray markup', await page.locator('#src-html').getAttribute('placeholder') === '<h1>Heading</h1>\n<p>Post text</p>'
      && await page.locator('#pane-html > p, #pane-html > h1').count() === 0);
    check('English editor has translated static and generated labels', await page.locator('html').getAttribute('lang') === 'en'
      && await page.locator('#btn-publish').textContent() === 'Publish'
      && (await page.locator('.block-head').first().textContent()).includes('Heading'));
    const authored = '<h1>Мой текст — do not translate</h1><p>Сохранить</p>';
    await page.click('#mode-tabs [data-mode=html]');
    await page.locator('#src-html .cm-content').fill(authored);
    await page.fill('#title', 'Русское название — English content');
    await page.click('#mode-tabs [data-mode=json]');
    const pending = '{"rich_message": "неприменённый JSON';
    await page.locator('#src-json .cm-content').fill(pending);
    await page.selectOption('#locale-select', 'ru');
    await page.waitForFunction(() => document.documentElement.lang === 'ru' && document.querySelector('#title').value === 'Русское название — English content');
    await page.click('#mode-tabs [data-mode=json]');
    check('RU switch preserves invalid unapplied JSON exactly', await page.locator('#src-json').evaluate((el) => el.value) === pending);
    await page.click('#mode-tabs [data-mode=html]');
    check('RU switch preserves authored text and the active post format', await page.locator('#src-html').evaluate((el) => el.value) === authored
      && await page.locator('#btn-save').textContent() === 'Сохранить');
    await page.selectOption('#locale-select', 'en');
    await page.waitForFunction(() => document.documentElement.lang === 'en' && document.querySelector('#title').value === 'Русское название — English content');
    const stored = await page.evaluate(async () => (await (await fetch('/api/drafts/' + new URL(location.href).searchParams.get('draft'))).json()).payload);
    check('Language choice does not change post payload or pending JSON', stored.rich_message.html === authored && stored._studio.buffers.json === pending);
    await page.reload();
    await page.waitForFunction(() => document.querySelector('#title').value === 'Русское название — English content');
    check('Locale persists on reload', await page.locator('#locale-select').inputValue() === 'en');
    check('Relative dates and draft render links keep the English locale',
      (await page.evaluate(() => window.RichRender.formatTime(Math.floor(Date.now() / 1000) + 3600, 'r'))).includes('in ')
      && (await page.locator('#drafts a[href^="/render/draft/"]').first().getAttribute('href')).endsWith('?lang=en'));
    await page.click('#btn-check');
    check('Channel dialog is English', await page.getByRole('heading', { name: 'Channels', exact: true }).isVisible());
    await page.getByRole('button', { name: 'Close', exact: true }).click();
    await page.click('#mode-tabs [data-mode=markdown]');
    await page.locator('#src-markdown .cm-content').fill('# English editor\n\nA post with **formatting**.');
    await page.waitForFunction(() => document.querySelector('#preview').shadowRoot.textContent.includes('English editor'));
    await page.locator('#src-markdown').getByRole('button', { name: 'Find', exact: true }).click();
    check('Source editor search is English', await page.locator('#src-markdown .cm-search').textContent().then((s) => s.includes('Replace')));
    await page.locator('#src-markdown .cm-search button[name=close]').click();
    await page.screenshot({ path: 'test-results/ui/editor-en-1400.png', fullPage: true });
    await page.setViewportSize({ width: 390, height: 844 });
    check('English editor fits a phone and keeps language control accessible', await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)
      && await page.locator('#locale-select').isVisible());
    await page.screenshot({ path: 'test-results/ui/editor-en-390.png', fullPage: true });
    for (const width of [1400, 390]) {
      await page.setViewportSize({ width, height: width === 390 ? 844 : 1000 });
      await page.goto(BASE + '/en/');
      check(`English landing fits ${width}px and links to English app`, await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)
        && (await page.locator('a.button.primary').first().getAttribute('href')).includes('lang=en'));
      await page.screenshot({ path: `test-results/ui/landing-en-${width}.png`, fullPage: true });
      await page.goto(BASE + '/en/features/telegram-tables/');
      check(`English guide has an example and fits ${width}px`, await page.locator('table').count() === 1
        && await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
      await page.screenshot({ path: `test-results/ui/guide-en-${width}.png`, fullPage: true });
    }
    check('Locale flows have no JavaScript errors', errors.length === 0, errors.join(' | '));
  } finally { await context.close(); }
}
