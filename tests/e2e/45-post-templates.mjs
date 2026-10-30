// Save a visual post as a template, define a form, fill it into a separate draft, then edit/delete the template.
import assert from 'node:assert/strict';
import { mkdirSync } from 'node:fs';

export default async function ({ browser, BASE, check, mockLog }) {
  const context = await browser.newContext({ viewport: { width: 1400, height: 1000 } });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  const before = (await mockLog()).filter((call) => call.method.startsWith('send')).length;
  try {
    await page.goto(BASE);
    await page.locator('.block').first().waitFor();
    await page.locator('#mode-tabs [data-mode=json]').click();
    const source = { rich_message: { blocks: [
      { type: 'heading', size: 1, text: 'Заголовок выпуска' },
      { type: 'paragraph', text: { type: 'bold', text: 'Постоянное оформление' } },
    ] }, options: { disable_notification: true } };
    await page.locator('#src-json .cm-content').fill(JSON.stringify(source));
    await page.locator('#json-apply').click();
    await page.locator('#title').fill('Исходный пост для шаблона');
    await page.locator('#btn-templates').click();
    const panel = page.locator('rs-templates');
    await panel.getByRole('button', { name: 'Сохранить текущий пост как шаблон', exact: true }).click();
    await panel.locator('#template-name').fill('Еженедельный анонс');
    await panel.locator('#template-source').selectOption('0');
    await panel.getByRole('button', { name: 'Добавить поле', exact: true }).click();
    await panel.locator('#label-0').fill('Тема выпуска');
    await panel.locator('#default-0').fill('');
    await panel.getByRole('button', { name: 'Сохранить шаблон', exact: true }).click();
    await panel.getByText('Еженедельный анонс', { exact: true }).waitFor();
    check('A visual post becomes a persistent template with a configurable form field', true);
    await panel.getByRole('button', { name: 'Создать по шаблону', exact: true }).click();
    await panel.locator('#value-0').waitFor();
    assert.equal(await panel.locator('#value-0').getAttribute('required'), '');
    await panel.getByRole('button', { name: 'Создать черновик', exact: true }).click();
    assert.equal(await panel.locator('#value-0').evaluate((el) => el.validity.valueMissing), true);
    await panel.locator('#value-0').fill('Запуск <нового> выпуска');
    await panel.locator('#new-draft-title').fill('Выпуск 42');
    mkdirSync('test-results/ui', { recursive: true });
    await page.screenshot({ path: 'test-results/ui/templates-form-1400.png', fullPage: true });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.screenshot({ path: 'test-results/ui/templates-form-390.png', fullPage: true });
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
    await panel.getByRole('button', { name: 'Создать черновик', exact: true }).click();
    await page.waitForFunction(() => !document.querySelector('rs-templates'));
    await page.waitForFunction(() => document.querySelector('#title').value === 'Выпуск 42');
    const draftId = new URL(page.url()).searchParams.get('draft');
    const draft = await (await context.request.get(BASE + '/api/drafts/' + draftId)).json();
    assert.equal(draft.payload.rich_message.blocks[0].text, 'Запуск <нового> выпуска');
    assert.deepEqual(draft.payload.rich_message.blocks[1], source.rich_message.blocks[1]);
    assert.equal(draft.payload.options.disable_notification, true);
    assert.equal(draft.post_id, null);
    const all = await (await context.request.get(BASE + '/api/drafts')).json();
    assert.ok(all.some((entry) => entry.title === 'Исходный пост для шаблона' && entry.id !== draftId));
    check('Required fields are enforced; creation keeps the source and opens a new filled draft', true);
    await page.setViewportSize({ width: 1400, height: 1000 });
    await page.locator('#btn-templates').click();
    await panel.getByRole('button', { name: 'Изменить', exact: true }).click();
    await panel.locator('#template-name').fill('Новый анонс');
    await panel.locator('#default-0').fill('Следующий выпуск');
    await panel.getByRole('button', { name: 'Сохранить шаблон', exact: true }).click();
    await panel.getByText('Новый анонс', { exact: true }).waitFor();
    await panel.getByRole('button', { name: 'Создать по шаблону', exact: true }).click();
    await panel.locator('#value-0').waitFor();
    assert.equal(await panel.locator('#value-0').inputValue(), 'Следующий выпуск');
    await panel.getByRole('button', { name: 'Назад к шаблонам', exact: true }).click();
    await panel.getByRole('button', { name: 'Удалить', exact: true }).click();
    await panel.getByRole('button', { name: 'Да, удалить', exact: true }).click();
    await panel.getByText('Шаблонов пока нет. Начните с поста в редакторе.', { exact: true }).waitFor();
    assert.equal((await context.request.get(BASE + '/api/drafts/' + draftId)).status(), 200);
    assert.equal((await mockLog()).filter((call) => call.method.startsWith('send')).length, before);
    check('Template edits update defaults; deleting a template preserves its drafts and never publishes', true);
    const literal = '1. News: a` **changed** `b & "quoted"';
    const destination = 'https://example.com/`**changed**`(x)?a=1&b=2';
    const field = { key: 'name', label: 'Name', type: 'text', required: true };
    for (const markdown of ['<h1>{{name}}</h1>', '{{name}}', '[site]({{url}})']) {
      const response = await context.request.post(BASE + '/api/templates', { data: { title: 'Literal rendering', post: { rich_message: { markdown } },
        fields: markdown.includes('{{url}}') ? [{ ...field, key: 'url', type: 'url' }] : [field] } });
      assert.equal(response.status(), 201);
      const template = await response.json();
      const filled = await (await context.request.post(BASE + '/api/templates/' + template.id + '/drafts', { data: {
        version: template.version, values: markdown.includes('{{url}}') ? { url: destination } : { name: literal },
      } })).json();
      const rendered = await page.evaluate((post) => {
        const root = window.RichRender.render(post, {});
        return { text: root.querySelector('.tg-body').textContent, formatting: root.querySelectorAll('strong,b,code').length,
          href: root.querySelector('a')?.getAttribute('href') };
      }, filled.payload);
      assert.equal(rendered.formatting, 0);
      if (markdown.includes('{{url}}')) assert.equal(decodeURI(rendered.href), destination);
      else assert.equal(rendered.text.trim(), literal);
      await context.request.delete(BASE + '/api/templates/' + template.id + '?version=' + template.version);
    }
    const rejected = await context.request.post(BASE + '/api/templates', { data: { title: 'Code', post: { rich_message: { markdown: '`{{name}}`' } }, fields: [field] } });
    assert.equal(rejected.status(), 422);
    check('Template values stay literal in Markdown text, raw HTML and link destinations; unsupported code fields are rejected', true);
    check('Template UI has no JavaScript errors', errors.length === 0, errors.join(' | '));
  } finally { await context.close(); }
}
