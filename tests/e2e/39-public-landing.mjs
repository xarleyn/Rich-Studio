// The public product page introduces the real workflow and links into the separate app.
export default async function ({ browser, check, BASE }) {
  const page = await browser.newPage({ viewport: { width: 1400, height: 1000 } });
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  page.on('console', (m) => { if (/Content Security Policy/i.test(m.text())) errors.push(m.text()); });
  try {
    await page.goto(BASE + '/landing');
    await page.locator('#hero-title').waitFor();
    check('Landing explains RichPost and has working app destinations', (await page.title()).startsWith('RichPost')
      && (await page.locator('a.button.primary').first().getAttribute('href')).startsWith('http')
      && await page.locator('.steps li').count() === 3);
    await page.screenshot({ path: 'test-results/ui/landing-hero-1400.png' });
    await page.screenshot({ path: 'test-results/ui/landing-1400.png', fullPage: true });
    await page.locator('nav a[href="#features"]').click();
    await page.waitForFunction(() => scrollY > 200);
    await page.locator('.questions summary').first().click();
    check('Landing navigation and FAQ work without JavaScript', await page.locator('.questions details').first().getAttribute('open') !== null);
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(BASE + '/landing');
    await page.waitForFunction(() => scrollY === 0);
    check('Landing fits a phone without horizontal scrolling', await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
    await page.screenshot({ path: 'test-results/ui/landing-hero-390.png' });
    await page.screenshot({ path: 'test-results/ui/landing-390.png', fullPage: true });
    check('Landing has no JavaScript or CSP errors', errors.length === 0, errors.join(' | '));
  } finally { await page.close(); }
}
