// Explicit HTML export alongside the fork's automatic format synchronization: successful export keeps the post,
// while a field that HTML cannot represent leaves both the visual source and any preserved HTML buffer unchanged.
/** @param {import('./harness.mjs').Ctx} ctx */
export default async function ({ page, check, applyJson, showJson, state }) {
  await page.click('#btn-new');
  const post = { rich_message: { blocks: [
    { type: 'heading', size: 2, text: 'Экспорт' },
    { type: 'paragraph', text: ['Текст с ', { type: 'bold', text: 'жирным' }] },
    { type: 'photo', photo: { type: 'photo', media: 'AgACAgIAAxkBAAIBfile' }, caption: { text: 'Фото' } },
  ] } };
  await applyJson(post);
  const button = page.locator('#to-html');
  const hiddenInJson = await button.isHidden();
  await page.click('#mode-tabs [data-mode=blocks]');
  await button.click();
  await page.waitForSelector('#mode-tabs [data-mode=html].on');
  const html = await page.locator('#src-html').evaluate((el) => el.value);
  await showJson();
  const sent = (await state()).rich_message;
  check('Explicit HTML export opens the HTML source and keeps media attached', hiddenInJson
    && html.startsWith('<h2>Экспорт</h2>') && html.includes('<b>жирным</b>') && html.includes('tg://photo?id=photo1')
    && sent.html === html && sent.media?.[0]?.media?.media === 'AgACAgIAAxkBAAIBfile' && !sent.blocks,
  JSON.stringify({ hiddenInJson, html, sent }));

  const sentinel = '<p>Сохранённый исходник</p>';
  const unsupported = { rich_message: { blocks: [{ type: 'map', location: { latitude: 55.75, longitude: 37.62 }, width: 600, height: 400 }] },
    _studio: { format_sync: 1, buffers: { html: sentinel } } };
  await applyJson(unsupported);
  await page.click('#mode-tabs [data-mode=blocks]');
  await button.click();
  const dialog = page.locator('dialog.dlg');
  await dialog.waitFor();
  const refusal = await dialog.textContent();
  const kept = await page.locator('#src-html').evaluate((el) => el.value);
  check('HTML export refuses unrepresentable fields with a reason and preserves the source',
    /не переведён в HTML/.test(refusal) && /поле width/.test(refusal) && /поле height/.test(refusal)
      && kept === sentinel && await page.locator('#mode-tabs [data-mode=blocks].on').count() === 1,
  JSON.stringify({ refusal, kept }));
  await dialog.getByRole('button', { name: 'Закрыть' }).click();
}
