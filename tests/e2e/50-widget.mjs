// HTML widget: the «Опубликовать ▾» dialog validates sizes and sends sendWidget; «Данные» edits the widget state
/** @param {import('./harness.mjs').Ctx} ctx */
export default async function ({ page, check, mockLog, until, setChat }) {
  const calls = async (method) => (await mockLog()).filter((c) => c.method === method);
  const source = `<div style="background:#3390ec;color:#fff;padding:12px;border-radius:10px">
  <b>Счётчик</b>
  <button>0</button>
</div><script>const b = document.querySelector('button'); let n = 0; b.addEventListener('click', () => { b.textContent = String(++n); });</script>`;
  await setChat('@widget_group');
  await page.click('#btn-more');
  await page.click('#btn-widget');
  const dialog = page.locator('dialog[open]');
  await dialog.locator('textarea[aria-label="Исходник виджета"]').fill(source);
  await dialog.locator('textarea[aria-label="Подпись"]').fill('клик-счётчик');
  // the server check lands after the debounce: wait for its verdict
  await page.waitForFunction(() => [...document.querySelectorAll('dialog[open] .row span')].some((s) => /проверка пройдена/.test(s.textContent || '')), null, { timeout: 8000 });
  await page.screenshot({ path: 'test-results/ui/widget-1400.png' });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: 'test-results/ui/widget-390.png' });
  const fits = await dialog.evaluate((node) => node.getBoundingClientRect().width <= innerWidth && document.documentElement.scrollWidth <= innerWidth);
  check('Autonomous widget dialog fits a phone and has no bot-query switch', fits && await dialog.locator('input[type=checkbox]').count() === 1);
  await page.setViewportSize({ width: 1400, height: 900 });
  // the preview opens in its own isolated window: the vendored WidgetSandbox runtime boots
  const popupPromise = page.waitForEvent('popup', { timeout: 15000 });
  await dialog.locator('button', { hasText: 'Предпросмотр' }).click();
  const popup = await popupPromise;
  const button = popup.frameLocator('#host iframe').locator('button');
  await button.waitFor({ state: 'visible', timeout: 20000 });
  await button.click();
  const incremented = await until(async () => await button.textContent() === '1');
  const status = await popup.locator('#status').textContent();
  check('Widget preview runs local JavaScript and responds to presses', incremented && /\d+ px/.test(status || ''), status);
  await popup.screenshot({ path: 'test-results/ui/widget-preview.png' });
  await popup.close();
  const before = (await calls('sendWidget')).length;
  await dialog.locator('button', { hasText: 'Отправить' }).click();
  const sent = await until(async () => (await calls('sendWidget')).slice(before).pop());
  // the widget row appears in «Посты» with its badge and a «Данные» action
  await page.waitForFunction(() => [...document.querySelectorAll('#posts li')].some((li) => (li.textContent || '').includes('виджет')), null, { timeout: 8000 }).catch(() => {});
  await page.click('#side-tabs [data-tab=posts]');
  const row = page.locator('#posts li', { hasText: 'виджет' }).first();
  await row.locator('button', { hasText: 'Данные' }).click();
  const dataDialog = page.locator('dialog[open]');
  await dataDialog.locator('textarea[aria-label="chat_data"]').fill('{"n":5}');
  const editsBefore = (await calls('editMessageWidgetData')).length;
  await dataDialog.locator('button', { hasText: 'Сохранить' }).click();
  const edited = await until(async () => (await calls('editMessageWidgetData')).slice(editsBefore).pop());
  await page.screenshot({ path: 'test-results/ui/widget-list-1400.png' }).catch(() => {});
  check('Widget goes out as sendWidget and its data edits via editMessageWidgetData',
    sent && sent.body.html === source && sent.body.caption === 'клик-счётчик' && sent.body.allow_queries !== true && sent.body.chat_id === '@widget_group'
      && edited && edited.body.chat_data === '{"n":5}'
      && incremented,
    JSON.stringify({ sent: sent && sent.body, edited: edited && edited.body, status }));
}
