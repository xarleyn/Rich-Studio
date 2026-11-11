/** @param {import('./harness.mjs').Ctx} ctx */
export default async function ({ page, check, mockLog, setChat, until }) {
  await setChat('@widget_group');
  await page.click('#btn-more');
  await page.click('#btn-widget');
  const dialog = page.locator('dialog[open]');
  await dialog.locator('textarea[aria-label="chat_data"]').fill('x'.repeat(1025));
  await dialog.locator('textarea[aria-label="Исходник виджета"]').fill('<div>ok</div>');
  await dialog.locator('.row .e').filter({ hasText: 'chat_data' }).waitFor();
  await dialog.locator('textarea[aria-label="chat_data"]').fill('fixed');
  const before = (await mockLog()).filter((call) => call.method === 'sendWidget').length;
  await dialog.locator('button', { hasText: 'Отправить' }).click();
  const sent = await until(async () => (await mockLog()).filter((call) => call.method === 'sendWidget').slice(before).pop());
  check('Correcting chat_data clears the previous validation error and allows sending',
    sent?.body.chat_data === 'fixed', JSON.stringify(sent?.body));
  await dialog.waitFor({ state: 'detached' });
  const id = await page.evaluate(async () => {
    const { api } = await import('/static/js/api.js');
    const sent = await api('POST', '/api/widget', {
      widget: { html: '<div>state</div>', chat_data: 'current-chat', public_data: 'current-public' },
      chat_id: '@widget_group',
    });
    const { loadPosts } = await import('/static/js/posts.js');
    await loadPosts();
    return sent.results[0].post.id;
  });
  await page.click('#side-tabs [data-tab=posts]');
  await page.locator('#posts li', { hasText: 'виджет' }).first().locator('button', { hasText: 'Данные' }).click();
  const dataDialog = page.locator('dialog[open]');
  const values = {
    chat_data: await dataDialog.locator('textarea[aria-label="chat_data"]').inputValue(),
    public_data: await dataDialog.locator('textarea[aria-label="public_data"]').inputValue(),
  };
  check('Widget data dialog shows the current saved state',
    values.chat_data === 'current-chat' && values.public_data === 'current-public', JSON.stringify(values));
  const editsBefore = (await mockLog()).filter((call) => call.method === 'editMessageWidgetData').length;
  await dataDialog.locator('textarea[aria-label="chat_data"]').fill('');
  await dataDialog.locator('button', { hasText: 'Сохранить' }).click();
  const edit = await until(async () => (await mockLog()).filter((call) => call.method === 'editMessageWidgetData').slice(editsBefore).pop());
  check('Clearing one field never sends or clears the untouched field', edit?.body.chat_data === '' && !Object.hasOwn(edit.body, 'public_data'), JSON.stringify(edit?.body));
  await dataDialog.waitFor({ state: 'detached' });
  await page.locator('#posts li', { hasText: 'виджет' }).first().locator('button', { hasText: 'Данные' }).click();
  const conflictDialog = page.locator('dialog[open]');
  await conflictDialog.locator('textarea[aria-label="public_data"]').fill('my change');
  await page.evaluate(async (id) => {
    const { api } = await import('/static/js/api.js');
    await api('POST', `/api/posts/${id}/widget-data`, { public_data: 'another client' });
  }, id);
  const conflict = page.waitForResponse((reply) => reply.url().includes('/widget-data?post_version=') && reply.status() === 409);
  await conflictDialog.locator('button', { hasText: 'Сохранить' }).click();
  await conflict;
  const saved = await page.evaluate(async (id) => (await (await import('/static/js/api.js')).api('GET', `/api/posts/${id}`)).payload.widget.public_data, id);
  check('A stale widget dialog cannot overwrite another client', saved === 'another client', saved);
  await conflictDialog.locator('button', { hasText: 'Отмена' }).click();

  // Deliver an older invalid verdict after a newer valid one, without timing guesses.
  let releaseOld;
  let oldStarted;
  const oldGate = new Promise((resolve) => { releaseOld = resolve; });
  const started = new Promise((resolve) => { oldStarted = resolve; });
  const intercept = async (route) => {
    if (route.request().postDataJSON().widget.html !== '<div>old</div>') { await route.continue(); return; }
    oldStarted();
    await oldGate;
    await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ ok: false, errors: ['stale verdict'], warnings: [], size_raw: 14, size_compressed: 22 }) });
  };
  await page.route('**/api/widget/validate', intercept);
  await page.click('#btn-more');
  await page.click('#btn-widget');
  const raceDialog = page.locator('dialog[open]');
  await raceDialog.locator('textarea[aria-label="Исходник виджета"]').fill('<div>old</div>');
  await started;
  const fresh = page.waitForResponse((reply) => reply.url().endsWith('/api/widget/validate') && reply.request().postDataJSON().widget.html === '<div>new</div>');
  await raceDialog.locator('textarea[aria-label="Исходник виджета"]').fill('<div>new</div>');
  await fresh;
  await raceDialog.locator('.ok', { hasText: 'проверка пройдена' }).waitFor();
  const old = page.waitForResponse((reply) => reply.url().endsWith('/api/widget/validate') && reply.request().postDataJSON().widget.html === '<div>old</div>');
  releaseOld();
  await old;
  const sendsBefore = (await mockLog()).filter((call) => call.method === 'sendWidget').length;
  await raceDialog.locator('button', { hasText: 'Отправить' }).click();
  const latest = await until(async () => (await mockLog()).filter((call) => call.method === 'sendWidget').slice(sendsBefore).pop());
  check('An out-of-order validation response cannot block or replace newer input', latest?.body.html === '<div>new</div>', JSON.stringify(latest?.body));
  await page.unroute('**/api/widget/validate', intercept);
}
