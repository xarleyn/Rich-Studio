// A toolbar belongs to the visible rich-text field. Draft replacement or focus in another control removes it,
// even if the mouse happened to stay over its former location.
/** @param {import('./harness.mjs').Ctx} ctx */
export default async function ({ browser, BASE, check }) {
  const page = await browser.newPage({ viewport: { width: 1400, height: 900 } });
  page.on('dialog', (dialog) => dialog.accept());
  try {
    await page.goto(BASE);
    const field = page.locator('#pane-blocks .rte').first();
    await field.click();
    await page.locator('.toolbar.on').waitFor();
    await page.locator('#title').focus();
    check('Focusing another editor control hides the rich-text toolbar', await page.locator('.toolbar.on').count() === 0);
    await field.click();
    await page.locator('.toolbar.on').waitFor();
    await page.evaluate(async () => {
      const { loadPost } = await import('/static/js/editor.js');
      loadPost({ rich_message: { blocks: [{ type: 'paragraph', text: 'Другой черновик' }] } });
      dispatchEvent(new Event('scroll'));
    });
    check('Replacing the active draft leaves no orphan toolbar over the header', await page.locator('.toolbar.on').count() === 0);
    await page.click('#btn-new');
    await page.waitForSelector('.block');
  } finally { await page.close(); }
}
