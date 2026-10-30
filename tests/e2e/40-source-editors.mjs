// Source editing uses the same buffers, preview, draft persistence and safety boundary as the visual mode.
/** @param {import('./harness.mjs').Ctx} ctx */
export default async function ({ page, browser, check, BASE }) {
  await page.setViewportSize({ width: 1400, height: 900 });
  await page.click('#side-tabs [data-tab=drafts]');
  const editor = (kind) => page.locator('#src-' + kind);
  const value = (kind) => editor(kind).evaluate((el) => el.value);
  const fill = async (kind, marked) => {
    await page.click(`#mode-tabs [data-mode=${kind}]`);
    await editor(kind).locator('.cm-content').fill(marked.replace('|', ''));
    await page.waitForFunction(([kind, text]) => document.querySelector('#src-' + kind).value === text, [kind, marked.replace('|', '')]);
    if (marked.includes('|')) await editor(kind).evaluate((el, at) => { el.view.dispatch({ selection: { anchor: at } }); el.view.focus(); }, marked.indexOf('|'));
  };
  await page.click('#btn-new');
  await fill('html', '<h1>Текстовый редактор</h1>\n<p>Текст с <b>подсветкой</b>.</p>');
  await page.waitForFunction(() => document.querySelector('#preview').shadowRoot.textContent.includes('Текстовый редактор'));
  check('HTML editor highlights syntax, has line numbers and updates the preview',
    await editor('html').locator('.cm-line span').count() > 0 && await editor('html').locator('.cm-lineNumbers').count() === 1);

  await fill('html', '<tg-time uni|></tg-time>');
  await editor('html').getByRole('button', { name: /Подсказки/ }).click();
  const unix = editor('html').getByRole('option').filter({ hasText: 'unix' }).first();
  await unix.waitFor(); await unix.click();
  check('HTML completes Telegram-specific attributes', (await value('html')).includes('unix'), await value('html'));

  await fill('html', '<p>выделить</p>');
  await editor('html').evaluate((el) => el.view.dispatch({ selection: { anchor: 3, head: 11 } }));
  await page.locator('#snip-html').getByRole('button', { name: 'Ж', exact: true }).click();
  check('Existing snippet buttons wrap the selection in the code editor', await value('html') === '<p><b>выделить</b></p>');
  await editor('html').locator('.cm-content').press('Control+z');
  check('Code undo is local and does not switch the post mode', await value('html') === '<p>выделить</p>' && await page.locator('[data-mode=html].on').count() === 1);

  await fill('markdown', '/|');
  await editor('markdown').getByRole('button', { name: /Подсказки/ }).click();
  await editor('markdown').getByRole('option').first().waitFor();
  await editor('markdown').getByRole('option').filter({ hasText: 'Таблица' }).first().click();
  check('Markdown slash menu inserts a useful snippet', (await value('markdown')).startsWith('| Колонка 1'));
  await fill('markdown', '# План публикации\n\n- [ ] Подготовить текст\n- [ ] Проверить предпросмотр\n\n**Подсказки**, поиск и отмена работают прямо в тексте.');
  await editor('markdown').getByRole('button', { name: 'Найти', exact: true }).click();
  await editor('markdown').locator('.cm-search input[name=search]').fill('Подготовить');
  check('Source editor search and replacement are available', await editor('markdown').locator('.cm-search input[name=replace]').count() === 1);
  await editor('markdown').locator('.cm-search button[name=close]').click();
  await page.screenshot({ path: 'test-results/ui/source-markdown-1400.png' });

  await fill('json', '{"rich_message":{"blocks":[{"type": "|"}]}}');
  await editor('json').getByRole('button', { name: /Подсказки/ }).click();
  await editor('json').getByRole('option').filter({ hasText: 'paragraph' }).first().click();
  check('JSON block values are completed from the current Bot API specification', JSON.parse(await value('json')).rich_message.blocks[0].type === 'paragraph');
  await fill('json', '{"rich_message":{"markdown":"Сохранить"},"options":{"future_option":true}}');
  await editor('json').getByRole('button', { name: 'Форматировать', exact: true }).click();
  check('JSON formatting keeps unknown API fields', JSON.parse(await value('json')).options.future_option === true && (await value('json')).includes('\n'));
  await page.screenshot({ path: 'test-results/ui/source-json-1400.png' });
  await fill('json', '{"rich_message":');
  await editor('json').locator('.cm-lint-marker-error').first().waitFor();
  await editor('json').getByRole('button', { name: 'Форматировать', exact: true }).click();
  check('Invalid JSON is marked and formatting never destroys it', await value('json') === '{"rich_message":' && await editor('json').getByRole('status').isVisible());
  await page.click('#mode-tabs [data-mode=json]');
  await page.click('#mode-tabs [data-mode=markdown]'); await page.click('#mode-tabs [data-mode=json]');
  check('Unapplied JSON survives repeated selection and switching modes', await value('json') === '{"rich_message":');

  await fill('html', '<h1>Новости канала</h1>\n\n<details>\n  <summary>Что нового</summary>\n  <p>Подсветка, подсказки и удобная работа с текстом.</p>\n</details>\n\n<tg-|');
  await editor('html').getByRole('button', { name: /Подсказки/ }).click();
  await editor('html').getByRole('option').first().waitFor();
  await page.waitForFunction(() => document.querySelector('#preview').shadowRoot.textContent.includes('Новости канала'));
  await page.screenshot({ path: 'test-results/ui/source-html-1400.png' });
  await editor('html').locator('.cm-content').press('Escape');
  await fill('html', '<p>Текст после сохранения</p>');
  await page.fill('#title', 'Проверка текстового редактора');
  await page.waitForFunction(() => document.querySelector('#save-state').textContent.startsWith('сохранено'), null, { timeout: 12000 });
  const saved = await page.evaluate(async () => {
    const id = JSON.parse(localStorage.getItem('rs.lastDraft'));
    return (await (await fetch('/api/drafts/' + id)).json()).payload;
  });
  check('Code edits autosave their active text and inactive Markdown buffer', saved.rich_message.html === '<p>Текст после сохранения</p>' && saved._studio.buffers.markdown.includes('План публикации'));
  check('Even invalid unapplied JSON is saved without changing the published payload', saved._studio.buffers.json === '{"rich_message":' && !('json' in saved.rich_message));
  await page.click('#btn-new'); await page.click('#mode-tabs [data-mode=html]');
  const freshHtml = await value('html');
  await editor('html').locator('.cm-content').press('Control+z');
  check('A new draft cannot undo back into the previous draft text', freshHtml.includes('Заголовок') && await value('html') === freshHtml);
  await page.locator('#drafts li').filter({ hasText: 'Проверка текстового редактора' }).first().click();
  await page.waitForFunction(() => document.querySelector('#title').value === 'Проверка текстового редактора');
  await page.click('#mode-tabs [data-mode=json]');
  check('Reopening a saved draft restores invalid unapplied JSON exactly', await value('json') === '{"rich_message":');
  await fill('json', '{"rich_message":{"markdown":"Применённый текст"}}');
  await page.click('#json-apply');
  await page.waitForFunction(() => document.querySelector('#preview').shadowRoot.textContent.includes('Применённый текст'));
  await page.waitForFunction(() => document.querySelector('#save-state').textContent.startsWith('сохранено'), null, { timeout: 12000 });
  const applied = await page.evaluate(async () => (await (await fetch('/api/drafts/' + JSON.parse(localStorage.getItem('rs.lastDraft'))))).json());
  check('Applying JSON activates its payload and clears the pending source buffer', applied.payload.rich_message.markdown === 'Применённый текст' && !('json' in applied.payload._studio.buffers));

  await page.setViewportSize({ width: 390, height: 844 });
  await fill('markdown', '# Публикация с телефона\n\nТекст переносится по ширине. Подсказки доступны по кнопке.');
  await editor('markdown').getByRole('button', { name: /Подсказки/ }).click();
  await editor('markdown').getByRole('option').first().waitFor();
  check('Phone editor offers completion without a keyboard shortcut or horizontal page overflow', await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
  await page.screenshot({ path: 'test-results/ui/source-markdown-390.png', fullPage: true });
  await editor('markdown').locator('.cm-content').press('Escape');
  await page.emulateMedia({ colorScheme: 'dark' });
  await page.screenshot({ path: 'test-results/ui/source-markdown-dark-390.png', fullPage: true });
  await page.emulateMedia({ colorScheme: 'light' }); await page.setViewportSize({ width: 1400, height: 900 });
  check('No source editor renders a minimap', await page.locator('.minimap, .cm-minimap').count() === 0);

  const fallback = await browser.newPage();
  try {
    fallback.on('dialog', (dialog) => dialog.accept());
    await fallback.route('**/vendor/code-editor.js', (route) => route.abort());
    await fallback.goto(BASE);
    await fallback.waitForSelector('.block');
    await fallback.click('#mode-tabs [data-mode=markdown]');
    await fallback.locator('#src-markdown').getByRole('status').waitFor();
    await fallback.locator('#src-markdown textarea').fill('Текст доступен даже без библиотеки');
    await fallback.waitForFunction(() => document.querySelector('#preview').shadowRoot.textContent.includes('Текст доступен даже без библиотеки'));
    check('A failed editor download leaves a working text input and preview', await fallback.locator('#src-markdown textarea').isVisible());
    await fallback.click('#btn-new'); await fallback.click('#mode-tabs [data-mode=markdown]');
    const fresh = fallback.locator('#src-markdown textarea');
    const freshMarkdown = await fresh.inputValue();
    check('Fallback editor shows the new draft converted to Markdown', freshMarkdown === '# Заголовок\n\nТекст поста' && await fallback.locator('#src-markdown').evaluate(el => el.value) === freshMarkdown);
    await fresh.press('Control+End');
    await fresh.pressSequentially('Новый текст');
    check('Fallback typing cannot copy text from the previous draft', await fallback.locator('#src-markdown').evaluate((el) => el.value) === freshMarkdown + 'Новый текст');
    await fallback.locator('#src-markdown').evaluate(async (el) => { el.value = 'Новый текст'; el.resetHistory(); await el.updateComplete; });
    await fallback.locator('#src-markdown textarea').press('Control+z');
    check('Fallback undo history resets even for a different document with identical text', await fallback.locator('#src-markdown textarea').inputValue() === 'Новый текст');
  } finally { await fallback.close(); }
}
