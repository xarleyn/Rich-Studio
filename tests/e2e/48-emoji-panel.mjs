// Паки кастомных эмодзи: настройка из панели редактора, вставка в тексте, картинка в предпросмотре
/** @param {import('./harness.mjs').Ctx} ctx */
export default async function ({ page, check, state, showJson }) {
  await page.setViewportSize({ width: 1400, height: 900 });
  // 1. Пустая панель предлагает настроить паки; диалог добавляет набор
  await page.click('#mode-tabs [data-mode=blocks]');
  await page.locator('#pane-blocks .rte').first().click();
  await page.click('.toolbar button:has-text("🙂")');
  await page.waitForSelector('.panel-emoji');
  const hint = await page.locator('.panel-emoji button:has-text("Настроить паки")').count();
  check('Пустая панель предлагает настроить паки', hint === 1, String(hint));
  await page.click('.panel-emoji button:has-text("Настроить паки")');
  await page.waitForSelector('dialog[open]');
  await page.locator('dialog[open] input[type=text]').fill('richpost_test_emoji');
  await page.click('dialog[open] button:has-text("Добавить")');
  await page.waitForTimeout(500);
  const row = await page.locator('dialog[open] .packs-row').count();
  const title = await page.locator('dialog[open] .packs-title').first().textContent().catch(() => '');
  check('Пак добавляется в диалоге настройки', row === 1 && title === 'RichPost Test', `${row}/${title}`);
  await page.click('dialog[open] button:has-text("Готово")');
  await page.waitForTimeout(200);

  // 2. Панель с паком: сетка из трёх эмодзи, поиск фильтрует по символу
  await page.locator('#pane-blocks .rte').first().click();
  await page.click('.toolbar button:has-text("🙂")');
  await page.waitForSelector('.panel-emoji .emoji-cell');
  const cells = await page.locator('.panel-emoji .emoji-cell').count();
  check('Панель показывает эмодзи пака', cells === 3, String(cells));
  await page.locator('.panel-emoji input[type=search]').fill('🚀');
  await page.waitForTimeout(200);
  const filtered = await page.locator('.panel-emoji .emoji-cell').count();
  check('Поиск в панели фильтрует по эмодзи', filtered === 1, String(filtered));
  await page.locator('.panel-emoji input[type=search]').fill('');

  // 3. Клик по эмодзи вставляет сущность
  await page.locator('.panel-emoji .emoji-cell').first().click();
  await page.waitForTimeout(300);
  await showJson();
  const para = JSON.stringify((await state()).rich_message.blocks[0].text);
  check('Клик по эмодзи вставляет сущность custom_emoji', /"type":"custom_emoji"/.test(para) && /"custom_emoji_id":"1111111111111111111"/.test(para), para.slice(0, 120));

  // 4. Предпросмотр: картинка из кэша учётки (мок отдаёт webp)
  await page.waitForTimeout(600);
  const img = await page.evaluate(() => {
    const root = document.querySelector('#preview')?.shadowRoot;
    const node = root && root.querySelector('.tg-emoji img');
    return node ? { src: node.getAttribute('src'), w: node.clientWidth } : null;
  });
  check('Предпросмотр рисует картинку эмодзи', Boolean(img) && img.src.includes('/api/emoji/img/1111111111111111111') && img.w > 0, JSON.stringify(img));
}
